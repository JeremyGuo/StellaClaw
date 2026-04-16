use crate::compaction::{
    COMPACTION_MARKER, ContextCompactionReport, StructuredCompactionOutput,
    maybe_compact_messages_with_report, maybe_compact_messages_with_report_with_session,
};
use crate::config::AgentConfig;
use crate::llm::{
    ChatCompletionSession, TokenUsage, create_chat_completion, start_chat_completion_session,
};
use crate::message::ChatMessage;
use crate::modality::materialize_messages_for_upstream;
use crate::skills::{SkillMetadata, build_skills_meta_prompt, discover_skills};
use crate::token_estimation::estimate_session_tokens_for_upstream;
use crate::tooling::{
    InterruptSignal, Tool, build_tool_registry, build_tool_registry_with_cancel, execute_tool,
};
use anyhow::{Context, Result, anyhow};
use crossbeam_channel::{Receiver, Sender, unbounded};
use humantime::parse_duration;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::path::Path;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

const AGENT_FRAME_MARKER: &str = "[AgentFrame Runtime]";

fn compose_system_prompt(config: &AgentConfig, skills: &[SkillMetadata]) -> String {
    let skills_prompt = build_skills_meta_prompt(skills);
    let mut parts = vec![
        AGENT_FRAME_MARKER.to_string(),
        "You are running inside AgentFrame. Use tools when they materially help.".to_string(),
        "When a supported tool has remote=\"<host>|local\" and the task is on an SSH host, you MUST set remote to the actual SSH alias instead of manually running ssh <host> inside a shell command; this avoids brittle quoting and escaping retries. Omit remote for local work.".to_string(),
        "DSL tool guidance: use dsl_start/dsl_wait/dsl_kill only for finite multi-step orchestration that reduces multiple dependent LLM/tool rounds. DSL code runs in a restricted CPython worker, so normal Python expressions, assignments, if statements, f-strings, list/dict operations, slices, string methods, and type() work. Available globals are emit(text), quit(), quit(value), handle = LLM(), handle.system(text), handle.config(key=value), handle.fork(), await handle.gen(prompt, **format_vars), await handle.json(prompt, **format_vars), await handle.select(prompt, [\"A\", \"B\", \"C\"]), and await tool({\"name\": \"tool_name\", \"args\": {\"arg\": value}}). tool(...) only accepts that single dict request shape; do not call tool(\"tool_name\", arg=value). DSL LLM calls always use the same model as the dsl_start caller; call LLM() without arguments and do not use LLM(model=...) or handle.config(model=...). LLM is only a callable handle factory; do not call LLM.llm(), LLM.tool(), LLM.gen(), LLM.json(), or LLM.select(). Use emit(text) for DSL output; emitted text is joined into the default result, while quit(value) returns an explicit result. Assign tool(...) results to variables and access returned JSON with normal Python dict/list syntax. DSL jobs are exec-like long-running jobs: interrupting dsl_start or dsl_wait only interrupts the outer wait, while the DSL job continues in the background regardless of what it is doing internally. User interrupts do not cancel DSL code, DSL LLM calls, DSL tool calls, or child long-running tools; use dsl_kill to stop the DSL job and kill child tools explicitly when needed. DSL code must not use loops, comprehensions, generators, imports, functions, classes, lambdas, private '_' names/attributes, or recursive DSL calls. DSL cannot directly mutate canonical system prompts; dynamic prompt changes still arrive through system notifications and are folded into the canonical system prompt after compaction.".to_string(),
    ];
    if config
        .upstream
        .native_web_search
        .as_ref()
        .is_some_and(|settings| settings.enabled)
    {
        parts.push(
            "Native provider web search is enabled for this session. Prefer that built-in capability instead of expecting a separate external web_search tool."
                .to_string(),
        );
    }
    if config.upstream.native_image_generation {
        parts.push(
            "Native provider image generation is enabled for this session. When the user asks to create or edit images, use that built-in capability instead of expecting a separate local image generation tool."
                .to_string(),
        );
    }
    if !config.system_prompt.is_empty() {
        parts.push(config.system_prompt.clone());
    }
    if !skills_prompt.is_empty() {
        parts.push(skills_prompt);
    }
    parts.join("\n\n")
}

fn ensure_system_message(messages: &[ChatMessage], system_prompt: &str) -> Vec<ChatMessage> {
    let mut cloned = messages.to_vec();
    if let Some(first) = cloned.first_mut()
        && first.role == "system"
    {
        let first_content = match &first.content {
            Some(Value::String(text)) => text.clone(),
            _ => String::new(),
        };
        if first_content == system_prompt {
            return cloned;
        }
        if first_content.starts_with(AGENT_FRAME_MARKER)
            || first_content.starts_with(COMPACTION_MARKER)
        {
            first.content = Some(Value::String(system_prompt.to_string()));
            return cloned;
        }
    }
    let mut with_system = vec![ChatMessage::text("system", system_prompt)];
    with_system.extend(cloned);
    with_system
}

pub fn extract_assistant_text(messages: &[ChatMessage]) -> String {
    messages
        .iter()
        .rev()
        .find(|message| message.role == "assistant")
        .map(|message| match &message.content {
            Some(Value::String(text)) => text.clone(),
            Some(Value::Array(items)) => items
                .iter()
                .filter_map(|item| {
                    let object = item.as_object()?;
                    let item_type = object.get("type")?.as_str()?;
                    match item_type {
                        "text" | "input_text" | "output_text" => {
                            object.get("text")?.as_str().map(ToOwned::to_owned)
                        }
                        _ => None,
                    }
                })
                .collect::<Vec<_>>()
                .join("\n"),
            Some(other) => other.to_string(),
            None => String::new(),
        })
        .unwrap_or_default()
}

fn assistant_message_has_content_or_tool_calls(message: &ChatMessage) -> bool {
    if message
        .tool_calls
        .as_ref()
        .is_some_and(|tool_calls| !tool_calls.is_empty())
    {
        return true;
    }
    match &message.content {
        None | Some(Value::Null) => false,
        Some(Value::String(text)) => !text.trim().is_empty(),
        Some(Value::Array(items)) => items.iter().any(|item| match item {
            Value::String(text) => !text.trim().is_empty(),
            Value::Object(object) => match object.get("type").and_then(Value::as_str) {
                Some("text" | "input_text" | "output_text") => object
                    .get("text")
                    .and_then(Value::as_str)
                    .is_some_and(|text| !text.trim().is_empty()),
                _ => true,
            },
            Value::Null => false,
            _ => true,
        }),
        Some(_) => true,
    }
}

pub fn run_session(
    previous_messages: Vec<ChatMessage>,
    prompt: impl Into<String>,
    config: AgentConfig,
    extra_tools: Vec<Tool>,
) -> Result<Vec<ChatMessage>> {
    Ok(run_session_state(previous_messages, prompt, config, extra_tools)?.messages)
}

#[derive(Clone, Debug, PartialEq)]
struct ResponseContinuation {
    response_id: String,
    anchor_messages: Vec<ChatMessage>,
}

impl ResponseContinuation {
    fn for_messages(response_id: impl Into<String>, messages: &[ChatMessage]) -> Self {
        Self {
            response_id: response_id.into(),
            anchor_messages: messages.to_vec(),
        }
    }

    fn delta_request_messages(&self, messages: &[ChatMessage]) -> Option<Vec<ChatMessage>> {
        if !messages.starts_with(&self.anchor_messages)
            || messages.len() <= self.anchor_messages.len()
        {
            return None;
        }

        let mut request_messages = messages
            .iter()
            .filter(|message| message.role == "system")
            .cloned()
            .collect::<Vec<_>>();
        request_messages.extend(
            messages[self.anchor_messages.len()..]
                .iter()
                .filter(|message| message.role != "system")
                .cloned(),
        );
        (!request_messages.is_empty()).then_some(request_messages)
    }

    fn previous_response_payload(&self) -> serde_json::Map<String, Value> {
        serde_json::Map::from_iter([(
            "previous_response_id".to_string(),
            Value::String(self.response_id.clone()),
        )])
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct UpstreamRuntimeFingerprint {
    base_url: String,
    model: String,
    api_kind: crate::config::UpstreamApiKind,
    auth_kind: crate::config::UpstreamAuthKind,
    chat_completions_path: String,
}

impl UpstreamRuntimeFingerprint {
    fn from_config(config: &AgentConfig) -> Self {
        Self {
            base_url: config.upstream.base_url.clone(),
            model: config.upstream.model.clone(),
            api_kind: config.upstream.api_kind,
            auth_kind: config.upstream.auth_kind,
            chat_completions_path: config.upstream.chat_completions_path.clone(),
        }
    }
}

#[derive(Default)]
pub struct PersistentSessionRuntime {
    fingerprint: Option<UpstreamRuntimeFingerprint>,
    continuation: Option<ResponseContinuation>,
    llm_session: Option<ChatCompletionSession>,
}

impl PersistentSessionRuntime {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn reset(&mut self) {
        self.fingerprint = None;
        self.continuation = None;
        self.llm_session = None;
    }

    fn prepare_for_config(&mut self, config: &AgentConfig) -> Result<()> {
        let fingerprint = UpstreamRuntimeFingerprint::from_config(config);
        if self.fingerprint.as_ref() != Some(&fingerprint) {
            self.fingerprint = Some(fingerprint);
            self.continuation = None;
            self.llm_session = start_chat_completion_session(&config.upstream)?;
        } else if self.llm_session.is_none() {
            self.llm_session = start_chat_completion_session(&config.upstream)?;
        }
        Ok(())
    }
}

fn is_previous_response_id_error(error: &anyhow::Error) -> bool {
    let message = format!("{error:#}").to_ascii_lowercase();
    message.contains("previous_response_id")
        || message.contains("previous response")
        || (message.contains("response") && message.contains("not found"))
        || (message.contains("response") && message.contains("invalid"))
}

fn image_path_to_data_url(path: &Path) -> Result<String> {
    let bytes =
        std::fs::read(path).with_context(|| format!("failed to read {}", path.display()))?;
    let media_type = match path
        .extension()
        .and_then(|value| value.to_str())
        .map(|value| value.to_ascii_lowercase())
        .as_deref()
    {
        Some("png") => "image/png",
        Some("jpg") | Some("jpeg") => "image/jpeg",
        Some("gif") => "image/gif",
        Some("webp") => "image/webp",
        Some("bmp") => "image/bmp",
        Some("svg") => "image/svg+xml",
        _ => "application/octet-stream",
    };
    use base64::Engine as _;
    let encoded = base64::engine::general_purpose::STANDARD.encode(bytes);
    Ok(format!("data:{};base64,{}", media_type, encoded))
}

fn file_path_to_base64(path: &Path) -> Result<String> {
    let bytes =
        std::fs::read(path).with_context(|| format!("failed to read {}", path.display()))?;
    use base64::Engine as _;
    Ok(base64::engine::general_purpose::STANDARD.encode(bytes))
}

fn infer_audio_format(path: &Path) -> Option<&'static str> {
    match path
        .extension()
        .and_then(|value| value.to_str())
        .map(|value| value.to_ascii_lowercase())
        .as_deref()
    {
        Some("wav") => Some("wav"),
        Some("mp3") | Some("mpeg") | Some("mpga") => Some("mp3"),
        Some("ogg") | Some("opus") => Some("ogg"),
        Some("webm") => Some("webm"),
        Some("m4a") | Some("mp4") | Some("aac") => Some("m4a"),
        Some("flac") => Some("flac"),
        _ => None,
    }
}

fn synthetic_user_message_from_tool_result(result: &str) -> Option<ChatMessage> {
    let value: Value = serde_json::from_str(result).ok()?;
    let object = value.as_object()?;
    if object.get("kind").and_then(Value::as_str) != Some("synthetic_user_multimodal") {
        return None;
    }
    let mut content = Vec::new();
    for item in object.get("media")?.as_array()? {
        let kind = item.get("type").and_then(Value::as_str)?;
        match kind {
            "input_image" => {
                let path = item.get("path").and_then(Value::as_str)?;
                let image_url = image_path_to_data_url(Path::new(path)).ok()?;
                content.push(json!({
                    "type": "input_image",
                    "image_url": image_url,
                }));
            }
            "input_file" => {
                let path = item.get("path").and_then(Value::as_str)?;
                let filename = item
                    .get("filename")
                    .and_then(Value::as_str)
                    .or_else(|| Path::new(path).file_name().and_then(|value| value.to_str()))
                    .unwrap_or("document.pdf");
                let file_data = file_path_to_base64(Path::new(path)).ok()?;
                content.push(json!({
                    "type": "file",
                    "file": {
                        "file_data": file_data,
                        "filename": filename,
                    }
                }));
            }
            "input_audio" => {
                let path = item.get("path").and_then(Value::as_str)?;
                let format = item
                    .get("format")
                    .and_then(Value::as_str)
                    .map(ToOwned::to_owned)
                    .or_else(|| infer_audio_format(Path::new(path)).map(ToOwned::to_owned))?;
                let data = file_path_to_base64(Path::new(path)).ok()?;
                content.push(json!({
                    "type": "input_audio",
                    "input_audio": {
                        "data": data,
                        "format": format,
                    }
                }));
            }
            _ => {}
        }
    }
    if content.is_empty() {
        return None;
    }
    Some(ChatMessage {
        role: "user".to_string(),
        content: Some(Value::Array(content)),
        name: None,
        tool_call_id: None,
        tool_calls: None,
    })
}

#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SessionPhase {
    #[default]
    End,
    Yielded,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SessionErrno {
    ApiFailure,
    ThresholdCompactionFailure,
    ToolWaitTimeout,
    IdleCompactionFailure,
    RuntimeFailure,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct SessionState {
    pub messages: Vec<ChatMessage>,
    #[serde(default)]
    pub pending_messages: Vec<ChatMessage>,
    #[serde(default)]
    pub phase: SessionPhase,
    #[serde(default)]
    pub errno: Option<SessionErrno>,
    #[serde(default)]
    pub errinfo: Option<String>,
    pub usage: TokenUsage,
    pub compaction: SessionCompactionStats,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct SessionCompactionStats {
    pub run_count: u64,
    pub compacted_run_count: u64,
    pub estimated_tokens_before: u64,
    pub estimated_tokens_after: u64,
    pub usage: TokenUsage,
}

impl SessionCompactionStats {
    fn record_report(&mut self, report: &ContextCompactionReport) {
        self.run_count = self.run_count.saturating_add(1);
        self.estimated_tokens_before = self
            .estimated_tokens_before
            .saturating_add(report.estimated_tokens_before as u64);
        self.estimated_tokens_after = self
            .estimated_tokens_after
            .saturating_add(report.estimated_tokens_after as u64);
        if report.compacted {
            self.compacted_run_count = self.compacted_run_count.saturating_add(1);
        }
        self.usage.add_assign(&report.usage);
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum SessionEvent {
    SessionStarted {
        previous_message_count: usize,
        prompt_len: usize,
        tool_definition_count: usize,
        skill_count: usize,
    },
    CompactionStarted {
        phase: String,
        message_count: usize,
    },
    CompactionCompleted {
        phase: String,
        compacted: bool,
        estimated_tokens_before: usize,
        estimated_tokens_after: usize,
        token_limit: usize,
        structured_output: Option<StructuredCompactionOutput>,
        compacted_messages: Vec<ChatMessage>,
    },
    RoundStarted {
        round_index: usize,
        message_count: usize,
    },
    ModelCallStarted {
        round_index: usize,
        message_count: usize,
    },
    ModelCallCompleted {
        round_index: usize,
        tool_call_count: usize,
        prompt_tokens: u64,
        completion_tokens: u64,
        total_tokens: u64,
    },
    ToolWaitCompactionScheduled {
        tool_name: String,
        stable_prefix_message_count: usize,
        delay_ms: u64,
    },
    ToolWaitCompactionStarted {
        tool_name: String,
        stable_prefix_message_count: usize,
    },
    ToolWaitCompactionCompleted {
        tool_name: String,
        compacted: bool,
        estimated_tokens_before: usize,
        estimated_tokens_after: usize,
        token_limit: usize,
        structured_output: Option<StructuredCompactionOutput>,
        compacted_messages: Vec<ChatMessage>,
    },
    ToolCallStarted {
        round_index: usize,
        tool_name: String,
        tool_call_id: String,
        arguments: Option<String>,
    },
    ToolCallCompleted {
        round_index: usize,
        tool_name: String,
        tool_call_id: String,
        output_len: usize,
        errored: bool,
    },
    SessionYielded {
        phase: String,
        message_count: usize,
        total_tokens: u64,
    },
    PrefixRewriteApplied {
        previous_prefix_message_count: usize,
        replacement_prefix_message_count: usize,
    },
    SessionCompleted {
        message_count: usize,
        total_tokens: u64,
    },
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ExecutionProgressPhase {
    Thinking,
    Tools,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ToolExecutionStatus {
    Running,
    Completed,
    Failed,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct ToolExecutionProgress {
    pub tool_call_id: String,
    pub tool_name: String,
    pub arguments: Option<String>,
    pub status: ToolExecutionStatus,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct ExecutionProgress {
    pub round_index: usize,
    pub phase: ExecutionProgressPhase,
    #[serde(default)]
    pub tools: Vec<ToolExecutionProgress>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum ExecutionSignal {
    Cancel,
    TimeoutObservation,
    Yield,
}

#[derive(Clone)]
pub struct SessionExecutionControl {
    cancel_flag: Arc<AtomicBool>,
    tool_interrupt_flag: Arc<InterruptSignal>,
    timeout_observation_requested: Arc<AtomicBool>,
    yield_requested: Arc<AtomicBool>,
    signal_sender: Sender<ExecutionSignal>,
    signal_receiver: Receiver<ExecutionSignal>,
    event_callback: Option<Arc<dyn Fn(SessionEvent) + Send + Sync>>,
    progress_callback: Option<Arc<dyn Fn(ExecutionProgress) + Send + Sync>>,
    stable_prefix_messages: Arc<Mutex<Vec<ChatMessage>>>,
    pending_prefix_rewrite: Arc<Mutex<Option<PendingPrefixRewrite>>>,
}

#[derive(Clone)]
struct PendingPrefixRewrite {
    expected_prefix: Vec<ChatMessage>,
    replacement_prefix: Vec<ChatMessage>,
    usage: TokenUsage,
    compaction: SessionCompactionStats,
}

struct PendingToolWaitCompaction {
    cancel_sender: mpsc::Sender<()>,
    join_handle: thread::JoinHandle<()>,
}

struct CompletedToolCall {
    tool_call_id: String,
    tool_name: String,
    result: String,
}

const MAX_IMAGE_LOADS_PER_TOOL_BATCH: usize = 3;

fn enforce_image_load_batch_limit(completed: &mut [CompletedToolCall]) {
    let mut seen = 0usize;
    for completed_tool in completed {
        if completed_tool.tool_name != "image_load" {
            continue;
        }
        seen = seen.saturating_add(1);
        if seen <= MAX_IMAGE_LOADS_PER_TOOL_BATCH {
            continue;
        }
        completed_tool.result = json!({
            "error": format!(
                "too many image_load calls in one tool batch: maximum is {MAX_IMAGE_LOADS_PER_TOOL_BATCH}; load additional images in a later turn or after inspecting the first batch"
            ),
            "tool": "image_load",
            "failed": true,
            "max_per_tool_batch": MAX_IMAGE_LOADS_PER_TOOL_BATCH,
            "position_in_batch": seen,
        })
        .to_string();
    }
}

impl SessionExecutionControl {
    pub fn new() -> Self {
        let (signal_sender, signal_receiver) = unbounded();
        Self {
            cancel_flag: Arc::new(AtomicBool::new(false)),
            tool_interrupt_flag: Arc::new(InterruptSignal::new()),
            timeout_observation_requested: Arc::new(AtomicBool::new(false)),
            yield_requested: Arc::new(AtomicBool::new(false)),
            signal_sender,
            signal_receiver,
            event_callback: None,
            progress_callback: None,
            stable_prefix_messages: Arc::new(Mutex::new(Vec::new())),
            pending_prefix_rewrite: Arc::new(Mutex::new(None)),
        }
    }

    pub fn with_event_callback(
        mut self,
        callback: impl Fn(SessionEvent) + Send + Sync + 'static,
    ) -> Self {
        self.event_callback = Some(Arc::new(callback));
        self
    }

    pub fn with_progress_callback(
        mut self,
        callback: impl Fn(ExecutionProgress) + Send + Sync + 'static,
    ) -> Self {
        self.progress_callback = Some(Arc::new(callback));
        self
    }

    pub fn request_cancel(&self) {
        self.cancel_flag.store(true, Ordering::SeqCst);
        self.tool_interrupt_flag.request();
        let _ = self.signal_sender.send(ExecutionSignal::Cancel);
    }

    pub fn is_cancelled(&self) -> bool {
        self.cancel_flag.load(Ordering::SeqCst)
    }

    pub fn request_timeout_observation(&self) {
        self.timeout_observation_requested
            .store(true, Ordering::SeqCst);
        self.tool_interrupt_flag.request();
        let _ = self.signal_sender.send(ExecutionSignal::TimeoutObservation);
    }

    pub fn request_yield(&self) {
        self.yield_requested.store(true, Ordering::SeqCst);
        self.tool_interrupt_flag.request();
        let _ = self.signal_sender.send(ExecutionSignal::Yield);
    }

    pub fn signal_receiver(&self) -> Receiver<ExecutionSignal> {
        self.signal_receiver.clone()
    }

    pub fn tool_interrupt_flag(&self) -> Arc<InterruptSignal> {
        Arc::clone(&self.tool_interrupt_flag)
    }

    pub fn emit_event_external(&self, event: SessionEvent) {
        self.emit_event(event);
    }

    pub fn emit_progress_external(&self, progress: ExecutionProgress) {
        self.emit_progress(progress);
    }

    fn take_timeout_observation_requested(&self) -> bool {
        let requested = self
            .timeout_observation_requested
            .swap(false, Ordering::SeqCst);
        if requested {
            self.tool_interrupt_flag.clear();
        }
        requested
    }

    pub fn take_yield_requested(&self) -> bool {
        let requested = self.yield_requested.swap(false, Ordering::SeqCst);
        if requested {
            self.tool_interrupt_flag.clear();
        }
        requested
    }

    pub fn stable_prefix_snapshot(&self) -> Vec<ChatMessage> {
        self.stable_prefix_messages
            .lock()
            .map(|messages| messages.clone())
            .unwrap_or_default()
    }

    pub fn request_prefix_rewrite(
        &self,
        expected_prefix: Vec<ChatMessage>,
        replacement_prefix: Vec<ChatMessage>,
        usage: TokenUsage,
        compaction: SessionCompactionStats,
    ) {
        if let Ok(mut pending) = self.pending_prefix_rewrite.lock() {
            *pending = Some(PendingPrefixRewrite {
                expected_prefix,
                replacement_prefix,
                usage,
                compaction,
            });
        }
    }

    fn ensure_not_cancelled(&self) -> Result<()> {
        if self.is_cancelled() {
            return Err(anyhow!("session execution cancelled"));
        }
        Ok(())
    }

    fn set_stable_prefix_messages(&self, messages: &[ChatMessage]) {
        if let Ok(mut stable_prefix) = self.stable_prefix_messages.lock() {
            *stable_prefix = messages.to_vec();
        }
    }

    fn take_pending_prefix_rewrite(&self) -> Option<PendingPrefixRewrite> {
        self.pending_prefix_rewrite
            .lock()
            .ok()
            .and_then(|mut pending| pending.take())
    }

    fn emit_event(&self, event: SessionEvent) {
        if let Some(callback) = &self.event_callback {
            callback(event);
        }
    }

    fn emit_progress(&self, progress: ExecutionProgress) {
        if let Some(callback) = &self.progress_callback {
            callback(progress);
        }
    }
}

impl Default for SessionExecutionControl {
    fn default() -> Self {
        Self::new()
    }
}

pub fn run_session_state(
    previous_messages: Vec<ChatMessage>,
    prompt: impl Into<String>,
    config: AgentConfig,
    extra_tools: Vec<Tool>,
) -> Result<SessionState> {
    run_session_state_controlled(previous_messages, prompt, config, extra_tools, None)
}

pub fn run_session_state_controlled_persistent(
    previous_messages: Vec<ChatMessage>,
    prompt: impl Into<String>,
    config: AgentConfig,
    extra_tools: Vec<Tool>,
    control: Option<SessionExecutionControl>,
    runtime: &mut PersistentSessionRuntime,
) -> Result<SessionState> {
    runtime.prepare_for_config(&config)?;
    run_session_state_controlled_internal(
        previous_messages,
        prompt.into(),
        config,
        extra_tools,
        control,
        runtime,
    )
}

pub fn run_session_state_controlled(
    previous_messages: Vec<ChatMessage>,
    prompt: impl Into<String>,
    config: AgentConfig,
    extra_tools: Vec<Tool>,
    control: Option<SessionExecutionControl>,
) -> Result<SessionState> {
    let mut runtime = PersistentSessionRuntime::new();
    runtime.prepare_for_config(&config)?;
    run_session_state_controlled_internal(
        previous_messages,
        prompt.into(),
        config,
        extra_tools,
        control,
        &mut runtime,
    )
}

fn run_session_state_controlled_internal(
    previous_messages: Vec<ChatMessage>,
    prompt: String,
    config: AgentConfig,
    extra_tools: Vec<Tool>,
    control: Option<SessionExecutionControl>,
    runtime: &mut PersistentSessionRuntime,
) -> Result<SessionState> {
    let discovered_skills = discover_skills(&config.skills_dirs)?;
    let system_prompt = compose_system_prompt(&config, &discovered_skills);
    let mut messages = ensure_system_message(&previous_messages, &system_prompt);
    let mut usage = TokenUsage::default();
    let mut compaction_stats = SessionCompactionStats::default();
    let llm_session = &mut runtime.llm_session;

    if let Some(control) = &control {
        control.ensure_not_cancelled()?;
    }

    let registry = build_tool_registry_with_cancel(
        &config.enabled_tools,
        &config.workspace_root,
        &config.runtime_state_root,
        &config.upstream,
        &config.available_upstreams,
        config.image_tool_upstream.as_ref(),
        config.pdf_tool_upstream.as_ref(),
        config.audio_tool_upstream.as_ref(),
        config.image_generation_tool_upstream.as_ref(),
        &config.skills_dirs,
        &discovered_skills,
        &extra_tools,
        &config.remote_workpaths,
        control
            .as_ref()
            .map(SessionExecutionControl::tool_interrupt_flag),
    )?;
    let tool_definitions = registry.values().cloned().collect::<Vec<_>>();
    if let Some(control) = &control {
        control.emit_event(SessionEvent::SessionStarted {
            previous_message_count: previous_messages.len(),
            prompt_len: prompt.len(),
            tool_definition_count: tool_definitions.len(),
            skill_count: discovered_skills.len(),
        });
        control.emit_event(SessionEvent::CompactionStarted {
            phase: "initial".to_string(),
            message_count: messages.len(),
        });
    }

    let initial_compaction_source = materialize_messages_for_upstream(&messages, &config)?;
    let initial_compaction = maybe_compact_messages_with_report_with_session(
        &config,
        &initial_compaction_source,
        &tool_definitions,
        &prompt,
        llm_session.as_mut(),
    )
    .context("threshold context compaction failed during initial phase")?;
    if let Some(control) = &control {
        control.emit_event(SessionEvent::CompactionCompleted {
            phase: "initial".to_string(),
            compacted: initial_compaction.compacted,
            estimated_tokens_before: initial_compaction.estimated_tokens_before,
            estimated_tokens_after: initial_compaction.estimated_tokens_after,
            token_limit: initial_compaction.token_limit,
            structured_output: initial_compaction.structured_output.clone(),
            compacted_messages: initial_compaction.compacted_messages.clone(),
        });
    }
    usage.add_assign(&initial_compaction.usage);
    compaction_stats.record_report(&initial_compaction);
    if initial_compaction.compacted {
        messages = initial_compaction.messages;
    }
    if let Some(control) = &control {
        control.set_stable_prefix_messages(&messages);
    }
    if !prompt.is_empty() {
        messages.push(ChatMessage::text("user", prompt));
    }

    let round_index = 0usize;
    loop {
        if let Some(control) = &control {
            control.ensure_not_cancelled()?;
            apply_pending_prefix_rewrite(control, &mut messages, &mut usage, &mut compaction_stats);
            if control.take_yield_requested() {
                control.emit_event(SessionEvent::SessionYielded {
                    phase: "round_start".to_string(),
                    message_count: messages.len(),
                    total_tokens: usage.total_tokens,
                });
                return Ok(SessionState {
                    messages,
                    pending_messages: Vec::new(),
                    phase: SessionPhase::Yielded,
                    errno: None,
                    errinfo: None,
                    usage,
                    compaction: compaction_stats,
                });
            }
            control.emit_event(SessionEvent::RoundStarted {
                round_index,
                message_count: messages.len(),
            });
        }
        if round_index > 0 {
            if let Some(control) = &control {
                control.emit_event(SessionEvent::CompactionStarted {
                    phase: "round".to_string(),
                    message_count: messages.len(),
                });
            }
            let round_compaction_source = materialize_messages_for_upstream(&messages, &config)?;
            let round_compaction = maybe_compact_messages_with_report_with_session(
                &config,
                &round_compaction_source,
                &tool_definitions,
                "",
                llm_session.as_mut(),
            )
            .context("threshold context compaction failed during round phase")?;
            if let Some(control) = &control {
                control.emit_event(SessionEvent::CompactionCompleted {
                    phase: "round".to_string(),
                    compacted: round_compaction.compacted,
                    estimated_tokens_before: round_compaction.estimated_tokens_before,
                    estimated_tokens_after: round_compaction.estimated_tokens_after,
                    token_limit: round_compaction.token_limit,
                    structured_output: round_compaction.structured_output.clone(),
                    compacted_messages: round_compaction.compacted_messages.clone(),
                });
            }
            usage.add_assign(&round_compaction.usage);
            compaction_stats.record_report(&round_compaction);
            if round_compaction.compacted {
                messages = round_compaction.messages;
            }
        }
        if let Some(control) = &control {
            control.ensure_not_cancelled()?;
            control.emit_event(SessionEvent::ModelCallStarted {
                round_index,
                message_count: messages.len(),
            });
            control.emit_progress(ExecutionProgress {
                round_index,
                phase: ExecutionProgressPhase::Thinking,
                tools: Vec::new(),
            });
        }
        let request_messages = materialize_messages_for_upstream(&messages, &config)?;
        let continuation_messages = runtime
            .continuation
            .as_ref()
            .and_then(|continuation| continuation.delta_request_messages(&request_messages));
        let continuation_payload = runtime.continuation.as_ref().and_then(|continuation| {
            continuation_messages
                .as_ref()
                .map(|_| continuation.previous_response_payload())
        });
        let outcome = match create_chat_completion(
            &config.upstream,
            continuation_messages
                .as_deref()
                .unwrap_or(&request_messages),
            &tool_definitions,
            continuation_payload.clone(),
            llm_session.as_mut(),
        ) {
            Ok(outcome) => outcome,
            Err(error)
                if continuation_payload.is_some() && is_previous_response_id_error(&error) =>
            {
                runtime.continuation = None;
                create_chat_completion(
                    &config.upstream,
                    &request_messages,
                    &tool_definitions,
                    None,
                    llm_session.as_mut(),
                )?
            }
            Err(error) => return Err(error),
        };
        usage.add_assign(&outcome.usage);
        let last_model_response_at = Instant::now();
        let tool_calls = outcome.message.tool_calls.clone().unwrap_or_default();
        if let Some(control) = &control {
            control.emit_event(SessionEvent::ModelCallCompleted {
                round_index,
                tool_call_count: tool_calls.len(),
                prompt_tokens: outcome.usage.prompt_tokens,
                completion_tokens: outcome.usage.completion_tokens,
                total_tokens: outcome.usage.total_tokens,
            });
        }
        if tool_calls.is_empty() && !assistant_message_has_content_or_tool_calls(&outcome.message) {
            runtime.continuation = None;
            return Ok(SessionState {
                messages,
                pending_messages: Vec::new(),
                phase: SessionPhase::Yielded,
                errno: Some(SessionErrno::ApiFailure),
                errinfo: Some("upstream returned an empty final assistant message".to_string()),
                usage,
                compaction: compaction_stats,
            });
        }

        messages.push(outcome.message);
        if let Some(response_id) = outcome.response_id {
            let mut anchor_messages = request_messages;
            anchor_messages.push(
                messages
                    .last()
                    .cloned()
                    .expect("assistant response just appended"),
            );
            runtime.continuation = Some(ResponseContinuation::for_messages(
                response_id,
                &anchor_messages,
            ));
        } else {
            runtime.continuation = None;
        }
        if tool_calls.is_empty() {
            if let Some(control) = &control {
                control.emit_event(SessionEvent::SessionCompleted {
                    message_count: messages.len(),
                    total_tokens: usage.total_tokens,
                });
            }
            return Ok(SessionState {
                messages,
                pending_messages: Vec::new(),
                phase: SessionPhase::End,
                errno: None,
                errinfo: None,
                usage,
                compaction: compaction_stats,
            });
        }

        if let Some(control) = &control {
            control.ensure_not_cancelled()?;
            apply_pending_prefix_rewrite(control, &mut messages, &mut usage, &mut compaction_stats);
            control.set_stable_prefix_messages(&messages);
            let tool_progress = tool_calls
                .iter()
                .map(|tool_call| ToolExecutionProgress {
                    tool_call_id: tool_call.id.clone(),
                    tool_name: tool_call.function.name.clone(),
                    arguments: tool_call.function.arguments.clone(),
                    status: ToolExecutionStatus::Running,
                })
                .collect::<Vec<_>>();
            control.emit_progress(ExecutionProgress {
                round_index,
                phase: ExecutionProgressPhase::Tools,
                tools: tool_progress,
            });
            for tool_call in &tool_calls {
                control.emit_event(SessionEvent::ToolCallStarted {
                    round_index,
                    tool_name: tool_call.function.name.clone(),
                    tool_call_id: tool_call.id.clone(),
                    arguments: tool_call.function.arguments.clone(),
                });
            }
        }
        let pending_compaction = start_pending_tool_wait_compaction(
            &config,
            &messages,
            control.as_ref(),
            "tool_batch",
            Some(last_model_response_at),
        )?;
        let mut handles = Vec::with_capacity(tool_calls.len());
        let (completed_sender, completed_receiver) = mpsc::channel();
        for tool_call in &tool_calls {
            let tool_name = tool_call.function.name.clone();
            let tool_call_id = tool_call.id.clone();
            let raw_arguments = tool_call.function.arguments.clone();
            let maybe_tool = registry.get(&tool_name).cloned();
            let completed_sender = completed_sender.clone();
            handles.push(thread::spawn(move || {
                let result = match maybe_tool {
                    Some(tool) => execute_tool(&tool, raw_arguments.as_deref()),
                    None => {
                        json!({"error": format!("unknown tool: {}", tool_name), "tool": tool_name})
                            .to_string()
                    }
                };
                let completed = CompletedToolCall {
                    tool_call_id,
                    tool_name,
                    result,
                };
                let _ = completed_sender.send(completed);
            }));
        }
        drop(completed_sender);

        let mut completed = Vec::with_capacity(handles.len());
        while completed.len() < handles.len() {
            let completed_tool = completed_receiver
                .recv()
                .map_err(|_| anyhow!("tool worker channel closed unexpectedly"))?;
            completed.push(completed_tool);
        }
        for handle in handles {
            handle
                .join()
                .map_err(|_| anyhow!("tool worker thread panicked"))?;
        }
        completed.sort_by_key(|completed_tool| {
            tool_calls
                .iter()
                .position(|tool_call| tool_call.id == completed_tool.tool_call_id)
                .unwrap_or(usize::MAX)
        });
        enforce_image_load_batch_limit(&mut completed);
        finish_pending_tool_wait_compaction(pending_compaction)?;

        let timeout_observation_requested = config.timeout_observation_compaction.enabled
            && control
                .as_ref()
                .is_some_and(SessionExecutionControl::take_timeout_observation_requested);
        let mut synthetic_user_content = Vec::new();
        for completed_tool in completed {
            let result = if timeout_observation_requested {
                synthesize_tool_timeout_observation(
                    &completed_tool.tool_name,
                    &completed_tool.result,
                    round_index,
                )
            } else {
                completed_tool.result
            };
            messages.push(ChatMessage::tool_output(
                completed_tool.tool_call_id.clone(),
                completed_tool.tool_name.clone(),
                result.clone(),
            ));
            if let Some(synthetic_user_message) = synthetic_user_message_from_tool_result(&result)
                && let Some(Value::Array(items)) = synthetic_user_message.content
            {
                synthetic_user_content.extend(items);
            }
            if let Some(control) = &control {
                control.emit_event(SessionEvent::ToolCallCompleted {
                    round_index,
                    tool_name: completed_tool.tool_name,
                    tool_call_id: completed_tool.tool_call_id,
                    output_len: result.len(),
                    errored: tool_result_looks_like_error(&result),
                });
            }
        }
        if !synthetic_user_content.is_empty() {
            messages.push(ChatMessage {
                role: "user".to_string(),
                content: Some(Value::Array(synthetic_user_content)),
                name: None,
                tool_call_id: None,
                tool_calls: None,
            });
        }
        if timeout_observation_requested {
            if let Some(control) = &control {
                control.emit_event(SessionEvent::ToolWaitCompactionStarted {
                    tool_name: "tool_batch".to_string(),
                    stable_prefix_message_count: messages.len(),
                });
            }
            let post_tool_compaction_source =
                materialize_messages_for_upstream(&messages, &config)?;
            let post_tool_compaction = maybe_compact_messages_with_report_with_session(
                &config,
                &post_tool_compaction_source,
                &tool_definitions,
                "",
                llm_session.as_mut(),
            )
            .context("tool-wait context compaction failed")?;
            if let Some(control) = &control {
                control.emit_event(SessionEvent::ToolWaitCompactionCompleted {
                    tool_name: "tool_batch".to_string(),
                    compacted: post_tool_compaction.compacted,
                    estimated_tokens_before: post_tool_compaction.estimated_tokens_before,
                    estimated_tokens_after: post_tool_compaction.estimated_tokens_after,
                    token_limit: post_tool_compaction.token_limit,
                    structured_output: post_tool_compaction.structured_output.clone(),
                    compacted_messages: post_tool_compaction.compacted_messages.clone(),
                });
            }
            usage.add_assign(&post_tool_compaction.usage);
            compaction_stats.record_report(&post_tool_compaction);
            if post_tool_compaction.compacted {
                messages = post_tool_compaction.messages;
            }
            if let Some(control) = &control {
                control.set_stable_prefix_messages(&messages);
            }
        }
        if let Some(control) = &control {
            control.emit_event(SessionEvent::SessionYielded {
                phase: "after_tool_batch".to_string(),
                message_count: messages.len(),
                total_tokens: usage.total_tokens,
            });
        }
        return Ok(SessionState {
            messages,
            pending_messages: Vec::new(),
            phase: SessionPhase::Yielded,
            errno: None,
            errinfo: None,
            usage,
            compaction: compaction_stats,
        });
    }
}

fn start_pending_tool_wait_compaction(
    config: &AgentConfig,
    stable_prefix: &[ChatMessage],
    control: Option<&SessionExecutionControl>,
    tool_name: &str,
    last_model_response_at: Option<Instant>,
) -> Result<Option<PendingToolWaitCompaction>> {
    if !config.enable_context_compression || !config.timeout_observation_compaction.enabled {
        return Ok(None);
    }
    if control.is_none() || stable_prefix.is_empty() {
        return Ok(None);
    }
    let Some(cache_control) = &config.upstream.cache_control else {
        return Ok(None);
    };
    let Some(ttl) = cache_control.ttl.as_deref() else {
        return Ok(None);
    };
    let Some(last_model_response_at) = last_model_response_at else {
        return Ok(None);
    };
    let ttl = parse_duration(ttl)
        .map_err(|error| anyhow!("failed to parse cache ttl '{}': {}", ttl, error))?;
    let Some(idle_threshold) = ttl.checked_sub(Duration::from_secs(30)) else {
        return Ok(None);
    };
    let delay = idle_threshold.saturating_sub(last_model_response_at.elapsed());
    if let Some(control) = control {
        control.emit_event(SessionEvent::ToolWaitCompactionScheduled {
            tool_name: tool_name.to_string(),
            stable_prefix_message_count: stable_prefix.len(),
            delay_ms: delay.as_millis().min(u128::from(u64::MAX)) as u64,
        });
    }

    let control = control.cloned().expect("checked above");
    let (cancel_sender, cancel_receiver) = mpsc::channel();
    let join_handle = thread::spawn(move || {
        if !delay.is_zero() {
            match cancel_receiver.recv_timeout(delay) {
                Ok(_) | Err(mpsc::RecvTimeoutError::Disconnected) => return,
                Err(mpsc::RecvTimeoutError::Timeout) => {}
            }
        }
        control.request_timeout_observation();
    });

    Ok(Some(PendingToolWaitCompaction {
        cancel_sender,
        join_handle,
    }))
}

fn finish_pending_tool_wait_compaction(pending: Option<PendingToolWaitCompaction>) -> Result<()> {
    let Some(pending) = pending else {
        return Ok(());
    };
    let _ = pending.cancel_sender.send(());
    pending
        .join_handle
        .join()
        .map_err(|_| anyhow!("tool wait compaction worker thread panicked"))?;
    Ok(())
}

fn apply_pending_prefix_rewrite(
    control: &SessionExecutionControl,
    messages: &mut Vec<ChatMessage>,
    usage: &mut TokenUsage,
    compaction_stats: &mut SessionCompactionStats,
) {
    let Some(rewrite) = control.take_pending_prefix_rewrite() else {
        return;
    };
    if !messages.starts_with(&rewrite.expected_prefix) {
        return;
    }
    let tail = messages[rewrite.expected_prefix.len()..].to_vec();
    let mut next_messages = rewrite.replacement_prefix.clone();
    next_messages.extend(tail);
    *messages = next_messages;
    usage.add_assign(&rewrite.usage);
    compaction_stats.run_count = compaction_stats
        .run_count
        .saturating_add(rewrite.compaction.run_count);
    compaction_stats.compacted_run_count = compaction_stats
        .compacted_run_count
        .saturating_add(rewrite.compaction.compacted_run_count);
    compaction_stats.estimated_tokens_before = compaction_stats
        .estimated_tokens_before
        .saturating_add(rewrite.compaction.estimated_tokens_before);
    compaction_stats.estimated_tokens_after = compaction_stats
        .estimated_tokens_after
        .saturating_add(rewrite.compaction.estimated_tokens_after);
    compaction_stats.usage.add_assign(&rewrite.compaction.usage);
    control.emit_event(SessionEvent::PrefixRewriteApplied {
        previous_prefix_message_count: rewrite.expected_prefix.len(),
        replacement_prefix_message_count: rewrite.replacement_prefix.len(),
    });
    control.set_stable_prefix_messages(&rewrite.replacement_prefix);
}

fn tool_result_looks_like_error(result: &str) -> bool {
    let Ok(value) = serde_json::from_str::<Value>(result) else {
        return false;
    };
    value
        .get("error")
        .is_some_and(|error| !matches!(error, Value::Null))
        || value
            .get("failed")
            .and_then(Value::as_bool)
            .unwrap_or(false)
}

fn synthesize_tool_timeout_observation(
    tool_name: &str,
    observed_result: &str,
    round_index: usize,
) -> String {
    let observed = serde_json::from_str::<Value>(observed_result)
        .unwrap_or_else(|_| Value::String(observed_result.to_string()));
    json!({
        "error": format!("tool execution was interrupted because the overall agent turn hit its timeout budget while waiting for {}", tool_name),
        "tool": tool_name,
        "timed_out": true,
        "round_index": round_index,
        "observed_result": observed,
        "next_step_options": [
            "retry the tool with a longer timeout_seconds",
            "inspect the partial observation and continue with a different tool",
            "explain the timeout to the user"
        ]
    })
    .to_string()
}

#[cfg(test)]
mod tests {
    use super::{
        CompletedToolCall, ExecutionSignal, ResponseContinuation, SessionExecutionControl,
        enforce_image_load_batch_limit, finish_pending_tool_wait_compaction,
        start_pending_tool_wait_compaction, synthetic_user_message_from_tool_result,
        tool_result_looks_like_error,
    };
    use crate::config::{AgentConfig, CacheControlConfig, MemorySystem, UpstreamConfig};
    use crate::message::ChatMessage;
    use serde_json::Value;
    use std::path::PathBuf;
    use std::thread;
    use std::time::{Duration, Instant};
    use tempfile::TempDir;

    #[test]
    fn tool_result_error_detection_ignores_null_error_fields() {
        assert!(!tool_result_looks_like_error(
            r#"{"error": null, "failed": false, "stdout": "ok"}"#
        ));
        assert!(!tool_result_looks_like_error(r#"{"stdout": "ok"}"#));
        assert!(tool_result_looks_like_error(
            r#"{"error": "boom", "failed": false}"#
        ));
        assert!(tool_result_looks_like_error(
            r#"{"error": null, "failed": true}"#
        ));
    }

    #[test]
    fn image_load_batch_limit_fails_excess_calls() {
        let mut completed = (0..5)
            .map(|index| CompletedToolCall {
                tool_call_id: format!("call_{index}"),
                tool_name: "image_load".to_string(),
                result: serde_json::json!({
                    "loaded": true,
                    "kind": "synthetic_user_multimodal",
                    "media": [{
                        "type": "input_image",
                        "path": format!("/tmp/image-{index}.png"),
                    }],
                })
                .to_string(),
            })
            .collect::<Vec<_>>();

        enforce_image_load_batch_limit(&mut completed);

        for result in completed.iter().take(3).map(|tool| &tool.result) {
            assert!(result.contains("synthetic_user_multimodal"));
            assert!(!tool_result_looks_like_error(result));
        }
        for result in completed.iter().skip(3).map(|tool| &tool.result) {
            assert!(tool_result_looks_like_error(result));
            let value: Value = serde_json::from_str(result).unwrap();
            assert_eq!(value["tool"], "image_load");
            assert_eq!(value["max_per_tool_batch"], 3);
        }
    }

    #[test]
    fn pending_tool_wait_compaction_requests_timeout_observation_after_deadline() {
        let config = AgentConfig {
            enabled_tools: Vec::new(),
            upstream: UpstreamConfig {
                base_url: "http://127.0.0.1:1".to_string(),
                model: "fake-model".to_string(),
                api_kind: crate::config::UpstreamApiKind::ChatCompletions,
                auth_kind: crate::config::UpstreamAuthKind::ApiKey,
                supports_vision_input: false,
                supports_pdf_input: false,
                supports_audio_input: false,
                api_key: None,
                api_key_env: "OPENAI_API_KEY".to_string(),
                chat_completions_path: "/chat/completions".to_string(),
                codex_home: None,
                codex_auth: None,
                auth_credentials_store_mode: crate::config::AuthCredentialsStoreMode::Auto,
                timeout_seconds: 30.0,
                retry_mode: Default::default(),
                context_window_tokens: 1000,
                cache_control: Some(CacheControlConfig {
                    cache_type: "ephemeral".to_string(),
                    ttl: Some("31s".to_string()),
                }),
                prompt_cache_retention: None,
                prompt_cache_key: None,
                reasoning: None,
                headers: serde_json::Map::new(),
                native_web_search: None,
                external_web_search: None,
                native_image_input: false,
                native_pdf_input: false,
                native_audio_input: false,
                native_image_generation: false,
                token_estimation: None,
            },
            available_upstreams: Default::default(),
            image_tool_upstream: None,
            pdf_tool_upstream: None,
            audio_tool_upstream: None,
            image_generation_tool_upstream: None,
            skills_dirs: Vec::new(),
            system_prompt: "Test system prompt.".to_string(),
            remote_workpaths: Vec::new(),
            max_tool_roundtrips: 4,
            workspace_root: PathBuf::from("."),
            runtime_state_root: std::env::temp_dir().join("agent_frame_tests"),
            enable_context_compression: true,
            context_compaction: crate::config::ContextCompactionConfig {
                trigger_ratio: 0.9,
                token_limit_override: Some(40),
                recent_fidelity_target_ratio: 0.18,
            },
            timeout_observation_compaction: crate::config::TimeoutObservationCompactionConfig {
                enabled: true,
            },
            memory_system: MemorySystem::Layered,
        };
        let stable_prefix = vec![
            ChatMessage::text("system", "[AgentFrame Runtime]\n\nTest system prompt."),
            ChatMessage::text("user", "A".repeat(400)),
            ChatMessage::text("assistant", "B".repeat(400)),
            ChatMessage::text("user", "go"),
            ChatMessage {
                role: "assistant".to_string(),
                content: None,
                name: None,
                tool_call_id: None,
                tool_calls: Some(Vec::new()),
            },
        ];
        let control = SessionExecutionControl::new();
        let signal_receiver = control.signal_receiver();

        let pending = start_pending_tool_wait_compaction(
            &config,
            &stable_prefix,
            Some(&control),
            "test_tool",
            Some(Instant::now() - Duration::from_secs(2)),
        )
        .unwrap();

        thread::sleep(Duration::from_millis(50));
        finish_pending_tool_wait_compaction(pending).unwrap();
        assert!(matches!(
            signal_receiver.recv_timeout(Duration::from_millis(50)),
            Ok(ExecutionSignal::TimeoutObservation)
        ));
    }

    #[test]
    fn synthetic_multimodal_tool_result_becomes_user_image_message() {
        let temp_dir = TempDir::new().unwrap();
        let image_path = temp_dir.path().join("sample.png");
        std::fs::write(&image_path, b"png-bytes").unwrap();

        let message = synthetic_user_message_from_tool_result(
            &serde_json::json!({
                "kind": "synthetic_user_multimodal",
                "media": [{
                    "type": "input_image",
                    "path": image_path.display().to_string(),
                }]
            })
            .to_string(),
        )
        .unwrap();

        assert_eq!(message.role, "user");
        let content = message.content.unwrap();
        let items = content.as_array().unwrap();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0]["type"], "input_image");
        assert!(
            items[0]["image_url"]
                .as_str()
                .unwrap_or_default()
                .starts_with("data:image/png;base64,")
        );
        assert!(matches!(content, Value::Array(_)));
    }

    #[test]
    fn synthetic_multimodal_tool_result_becomes_user_file_message() {
        let temp_dir = TempDir::new().unwrap();
        let file_path = temp_dir.path().join("sample.pdf");
        std::fs::write(&file_path, b"%PDF-demo").unwrap();

        let message = synthetic_user_message_from_tool_result(
            &serde_json::json!({
                "kind": "synthetic_user_multimodal",
                "media": [{
                    "type": "input_file",
                    "path": file_path.display().to_string(),
                    "filename": "sample.pdf",
                }]
            })
            .to_string(),
        )
        .unwrap();

        let content = message.content.unwrap();
        let items = content.as_array().unwrap();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0]["type"], "file");
        assert_eq!(items[0]["file"]["filename"], "sample.pdf");
        assert!(
            items[0]["file"]["file_data"]
                .as_str()
                .unwrap_or_default()
                .starts_with("JVBER")
        );
    }

    #[test]
    fn synthetic_multimodal_tool_result_becomes_user_audio_message() {
        let temp_dir = TempDir::new().unwrap();
        let audio_path = temp_dir.path().join("sample.wav");
        std::fs::write(&audio_path, b"RIFFdemoWAVE").unwrap();

        let message = synthetic_user_message_from_tool_result(
            &serde_json::json!({
                "kind": "synthetic_user_multimodal",
                "media": [{
                    "type": "input_audio",
                    "path": audio_path.display().to_string(),
                    "format": "wav",
                }]
            })
            .to_string(),
        )
        .unwrap();

        let content = message.content.unwrap();
        let items = content.as_array().unwrap();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0]["type"], "input_audio");
        assert_eq!(items[0]["input_audio"]["format"], "wav");
        assert!(
            !items[0]["input_audio"]["data"]
                .as_str()
                .unwrap_or_default()
                .is_empty()
        );
    }

    #[test]
    fn response_continuation_sends_only_delta_messages() {
        let anchor = vec![
            ChatMessage::text("system", "system prompt"),
            ChatMessage::text("user", "first request"),
            ChatMessage::text("assistant", "first answer"),
        ];
        let continuation = ResponseContinuation::for_messages("resp_123", &anchor);
        let mut next_request = anchor.clone();
        next_request.push(ChatMessage::text("user", "follow-up"));

        let delta = continuation.delta_request_messages(&next_request).unwrap();
        assert_eq!(
            delta,
            vec![
                ChatMessage::text("system", "system prompt"),
                ChatMessage::text("user", "follow-up")
            ]
        );
        assert_eq!(
            continuation.previous_response_payload()["previous_response_id"],
            serde_json::json!("resp_123")
        );
    }
}

pub fn compact_session_messages(
    previous_messages: Vec<ChatMessage>,
    config: AgentConfig,
    extra_tools: Vec<Tool>,
) -> Result<Vec<ChatMessage>> {
    Ok(compact_session_messages_with_report(previous_messages, config, extra_tools)?.messages)
}

pub fn compact_session_messages_with_report(
    previous_messages: Vec<ChatMessage>,
    config: AgentConfig,
    extra_tools: Vec<Tool>,
) -> Result<ContextCompactionReport> {
    let discovered_skills = discover_skills(&config.skills_dirs)?;
    let system_prompt = compose_system_prompt(&config, &discovered_skills);
    let messages = ensure_system_message(&previous_messages, &system_prompt);
    let registry = build_tool_registry(
        &config.enabled_tools,
        &config.workspace_root,
        &config.runtime_state_root,
        &config.upstream,
        &config.available_upstreams,
        config.image_tool_upstream.as_ref(),
        config.pdf_tool_upstream.as_ref(),
        config.audio_tool_upstream.as_ref(),
        config.image_generation_tool_upstream.as_ref(),
        &config.skills_dirs,
        &discovered_skills,
        &extra_tools,
        &config.remote_workpaths,
    )?;
    let tool_definitions = registry.values().cloned().collect::<Vec<_>>();
    let compaction_source = materialize_messages_for_upstream(&messages, &config)?;
    let mut report =
        maybe_compact_messages_with_report(&config, &compaction_source, &tool_definitions, "")?;
    if !report.compacted {
        report.messages = messages;
    }
    Ok(report)
}

pub fn estimate_configured_session_tokens(
    previous_messages: Vec<ChatMessage>,
    prompt: impl Into<String>,
    config: AgentConfig,
    extra_tools: Vec<Tool>,
) -> Result<usize> {
    let discovered_skills = discover_skills(&config.skills_dirs)?;
    let system_prompt = compose_system_prompt(&config, &discovered_skills);
    let messages = ensure_system_message(&previous_messages, &system_prompt);
    let registry = build_tool_registry(
        &config.enabled_tools,
        &config.workspace_root,
        &config.runtime_state_root,
        &config.upstream,
        &config.available_upstreams,
        config.image_tool_upstream.as_ref(),
        config.pdf_tool_upstream.as_ref(),
        config.audio_tool_upstream.as_ref(),
        config.image_generation_tool_upstream.as_ref(),
        &config.skills_dirs,
        &discovered_skills,
        &extra_tools,
        &config.remote_workpaths,
    )?;
    let tool_definitions = registry.values().cloned().collect::<Vec<_>>();
    let request_messages = materialize_messages_for_upstream(&messages, &config)?;
    Ok(estimate_session_tokens_for_upstream(
        &request_messages,
        &tool_definitions,
        &prompt.into(),
        &config.upstream,
    ))
}
