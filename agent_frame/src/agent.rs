use crate::compaction::{
    COMPACTION_MARKER, ContextCompactionReport, maybe_compact_messages_with_report,
};
use crate::config::AgentConfig;
use crate::llm::{TokenUsage, create_chat_completion};
use crate::message::ChatMessage;
use crate::skills::{SkillMetadata, build_skills_meta_prompt, discover_skills};
use crate::tooling::{
    InterruptSignal, Tool, build_tool_registry, build_tool_registry_with_cancel, execute_tool,
};
use anyhow::{Result, anyhow};
use crossbeam_channel::{Receiver, Sender, unbounded};
use humantime::parse_duration;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
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

pub fn run_session(
    previous_messages: Vec<ChatMessage>,
    prompt: impl Into<String>,
    config: AgentConfig,
    extra_tools: Vec<Tool>,
) -> Result<Vec<ChatMessage>> {
    Ok(run_session_with_report(previous_messages, prompt, config, extra_tools)?.messages)
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct SessionRunReport {
    pub messages: Vec<ChatMessage>,
    pub usage: TokenUsage,
    pub compaction: SessionCompactionStats,
    #[serde(default)]
    pub yielded: bool,
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
    CheckpointEmitted {
        message_count: usize,
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
    },
    ToolCallStarted {
        round_index: usize,
        tool_name: String,
        tool_call_id: String,
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
    checkpoint_callback: Option<Arc<dyn Fn(SessionRunReport) + Send + Sync>>,
    event_callback: Option<Arc<dyn Fn(SessionEvent) + Send + Sync>>,
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
            checkpoint_callback: None,
            event_callback: None,
            stable_prefix_messages: Arc::new(Mutex::new(Vec::new())),
            pending_prefix_rewrite: Arc::new(Mutex::new(None)),
        }
    }

    pub fn with_checkpoint_callback(
        callback: impl Fn(SessionRunReport) + Send + Sync + 'static,
    ) -> Self {
        let (signal_sender, signal_receiver) = unbounded();
        Self {
            cancel_flag: Arc::new(AtomicBool::new(false)),
            tool_interrupt_flag: Arc::new(InterruptSignal::new()),
            timeout_observation_requested: Arc::new(AtomicBool::new(false)),
            yield_requested: Arc::new(AtomicBool::new(false)),
            signal_sender,
            signal_receiver,
            checkpoint_callback: Some(Arc::new(callback)),
            event_callback: None,
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

    pub fn emit_checkpoint_report(&self, report: SessionRunReport) {
        if let Some(callback) = &self.checkpoint_callback {
            callback(report);
        }
    }

    pub fn emit_event_external(&self, event: SessionEvent) {
        self.emit_event(event);
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

    fn emit_checkpoint(&self, messages: &[ChatMessage], usage: &TokenUsage) {
        if let Some(callback) = &self.checkpoint_callback {
            callback(SessionRunReport {
                messages: messages.to_vec(),
                usage: usage.clone(),
                compaction: SessionCompactionStats::default(),
                yielded: false,
            });
        }
        self.emit_event(SessionEvent::CheckpointEmitted {
            message_count: messages.len(),
            total_tokens: usage.total_tokens,
        });
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
}

impl Default for SessionExecutionControl {
    fn default() -> Self {
        Self::new()
    }
}

pub fn run_session_with_report(
    previous_messages: Vec<ChatMessage>,
    prompt: impl Into<String>,
    config: AgentConfig,
    extra_tools: Vec<Tool>,
) -> Result<SessionRunReport> {
    run_session_with_report_controlled(previous_messages, prompt, config, extra_tools, None)
}

pub fn run_session_with_report_controlled(
    previous_messages: Vec<ChatMessage>,
    prompt: impl Into<String>,
    config: AgentConfig,
    extra_tools: Vec<Tool>,
    control: Option<SessionExecutionControl>,
) -> Result<SessionRunReport> {
    let prompt = prompt.into();
    let discovered_skills = discover_skills(&config.skills_dirs)?;
    let system_prompt = compose_system_prompt(&config, &discovered_skills);
    let mut messages = ensure_system_message(&previous_messages, &system_prompt);
    let mut usage = TokenUsage::default();
    let mut compaction_stats = SessionCompactionStats::default();

    if let Some(control) = &control {
        control.ensure_not_cancelled()?;
    }

    let registry = build_tool_registry_with_cancel(
        &config.enabled_tools,
        &config.workspace_root,
        &config.runtime_state_root,
        &config.upstream,
        config.image_tool_upstream.as_ref(),
        &config.skills_dirs,
        &discovered_skills,
        &extra_tools,
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

    let initial_compaction =
        maybe_compact_messages_with_report(&config, &messages, &tool_definitions, &prompt)?;
    if let Some(control) = &control {
        control.emit_event(SessionEvent::CompactionCompleted {
            phase: "initial".to_string(),
            compacted: initial_compaction.compacted,
            estimated_tokens_before: initial_compaction.estimated_tokens_before,
            estimated_tokens_after: initial_compaction.estimated_tokens_after,
            token_limit: initial_compaction.token_limit,
        });
    }
    usage.add_assign(&initial_compaction.usage);
    compaction_stats.record_report(&initial_compaction);
    messages = initial_compaction.messages;
    if let Some(control) = &control {
        control.set_stable_prefix_messages(&messages);
    }
    if !prompt.is_empty() {
        messages.push(ChatMessage::text("user", prompt));
    }

    for round_index in 0..config.max_tool_roundtrips {
        if let Some(control) = &control {
            control.ensure_not_cancelled()?;
            apply_pending_prefix_rewrite(control, &mut messages, &mut usage, &mut compaction_stats);
            if control.take_yield_requested() {
                control.emit_event(SessionEvent::SessionYielded {
                    phase: "round_start".to_string(),
                    message_count: messages.len(),
                    total_tokens: usage.total_tokens,
                });
                return Ok(SessionRunReport {
                    messages,
                    usage,
                    compaction: compaction_stats,
                    yielded: true,
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
            let round_compaction =
                maybe_compact_messages_with_report(&config, &messages, &tool_definitions, "")?;
            if let Some(control) = &control {
                control.emit_event(SessionEvent::CompactionCompleted {
                    phase: "round".to_string(),
                    compacted: round_compaction.compacted,
                    estimated_tokens_before: round_compaction.estimated_tokens_before,
                    estimated_tokens_after: round_compaction.estimated_tokens_after,
                    token_limit: round_compaction.token_limit,
                });
            }
            usage.add_assign(&round_compaction.usage);
            compaction_stats.record_report(&round_compaction);
            messages = round_compaction.messages;
        }
        if let Some(control) = &control {
            control.ensure_not_cancelled()?;
            control.emit_event(SessionEvent::ModelCallStarted {
                round_index,
                message_count: messages.len(),
            });
        }
        let outcome = create_chat_completion(&config.upstream, &messages, &tool_definitions, None)?;
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
        messages.push(outcome.message);
        if let Some(control) = &control
            && tool_calls.is_empty()
            && !extract_assistant_text(&messages).trim().is_empty()
        {
            control.emit_checkpoint(&messages, &usage);
        }
        if tool_calls.is_empty() {
            if let Some(control) = &control {
                control.emit_event(SessionEvent::SessionCompleted {
                    message_count: messages.len(),
                    total_tokens: usage.total_tokens,
                });
            }
            return Ok(SessionRunReport {
                messages,
                usage,
                compaction: compaction_stats,
                yielded: false,
            });
        }

        if let Some(control) = &control {
            control.ensure_not_cancelled()?;
            apply_pending_prefix_rewrite(control, &mut messages, &mut usage, &mut compaction_stats);
            control.set_stable_prefix_messages(&messages);
            for tool_call in &tool_calls {
                control.emit_event(SessionEvent::ToolCallStarted {
                    round_index,
                    tool_name: tool_call.function.name.clone(),
                    tool_call_id: tool_call.id.clone(),
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
        for tool_call in &tool_calls {
            let tool_name = tool_call.function.name.clone();
            let tool_call_id = tool_call.id.clone();
            let raw_arguments = tool_call.function.arguments.clone();
            let maybe_tool = registry.get(&tool_name).cloned();
            handles.push(thread::spawn(move || -> CompletedToolCall {
                let result = match maybe_tool {
                    Some(tool) => execute_tool(&tool, raw_arguments.as_deref()),
                    None => {
                        json!({"error": format!("unknown tool: {}", tool_name), "tool": tool_name})
                            .to_string()
                    }
                };
                CompletedToolCall {
                    tool_call_id,
                    tool_name,
                    result,
                }
            }));
        }

        let mut completed = Vec::with_capacity(handles.len());
        for handle in handles {
            completed.push(
                handle
                    .join()
                    .map_err(|_| anyhow!("tool worker thread panicked"))?,
            );
        }
        finish_pending_tool_wait_compaction(pending_compaction)?;

        let timeout_observation_requested = control
            .as_ref()
            .is_some_and(SessionExecutionControl::take_timeout_observation_requested);
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
        if timeout_observation_requested {
            if let Some(control) = &control {
                control.emit_event(SessionEvent::ToolWaitCompactionStarted {
                    tool_name: "tool_batch".to_string(),
                    stable_prefix_message_count: messages.len(),
                });
            }
            let post_tool_compaction =
                maybe_compact_messages_with_report(&config, &messages, &tool_definitions, "")?;
            if let Some(control) = &control {
                control.emit_event(SessionEvent::ToolWaitCompactionCompleted {
                    tool_name: "tool_batch".to_string(),
                    compacted: post_tool_compaction.compacted,
                    estimated_tokens_before: post_tool_compaction.estimated_tokens_before,
                    estimated_tokens_after: post_tool_compaction.estimated_tokens_after,
                    token_limit: post_tool_compaction.token_limit,
                });
            }
            usage.add_assign(&post_tool_compaction.usage);
            compaction_stats.record_report(&post_tool_compaction);
            messages = post_tool_compaction.messages;
            if let Some(control) = &control {
                control.set_stable_prefix_messages(&messages);
            }
        }
        if let Some(control) = &control
            && control.take_yield_requested()
        {
            control.emit_event(SessionEvent::SessionYielded {
                phase: "after_tool_batch".to_string(),
                message_count: messages.len(),
                total_tokens: usage.total_tokens,
            });
            return Ok(SessionRunReport {
                messages,
                usage,
                compaction: compaction_stats,
                yielded: true,
            });
        }
    }

    Err(anyhow!(
        "Agent stopped after exceeding max_tool_roundtrips={}",
        config.max_tool_roundtrips
    ))
}

fn start_pending_tool_wait_compaction(
    config: &AgentConfig,
    stable_prefix: &[ChatMessage],
    control: Option<&SessionExecutionControl>,
    tool_name: &str,
    last_model_response_at: Option<Instant>,
) -> Result<Option<PendingToolWaitCompaction>> {
    if !config.enable_context_compression {
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
    serde_json::from_str::<Value>(result)
        .ok()
        .and_then(|value| value.get("error").cloned())
        .is_some()
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
        ExecutionSignal, SessionExecutionControl, finish_pending_tool_wait_compaction,
        start_pending_tool_wait_compaction,
    };
    use crate::config::{AgentConfig, CacheControlConfig, UpstreamConfig};
    use crate::message::ChatMessage;
    use std::path::PathBuf;
    use std::thread;
    use std::time::{Duration, Instant};

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
                api_key: None,
                api_key_env: "OPENAI_API_KEY".to_string(),
                chat_completions_path: "/chat/completions".to_string(),
                codex_home: None,
                codex_auth: None,
                auth_credentials_store_mode: crate::config::AuthCredentialsStoreMode::Auto,
                timeout_seconds: 30.0,
                context_window_tokens: 1000,
                cache_control: Some(CacheControlConfig {
                    cache_type: "ephemeral".to_string(),
                    ttl: Some("31s".to_string()),
                }),
                reasoning: None,
                headers: serde_json::Map::new(),
                native_web_search: None,
                external_web_search: None,
            },
            image_tool_upstream: None,
            skills_dirs: Vec::new(),
            system_prompt: "Test system prompt.".to_string(),
            max_tool_roundtrips: 4,
            workspace_root: PathBuf::from("."),
            runtime_state_root: std::env::temp_dir().join("agent_frame_tests"),
            enable_context_compression: true,
            effective_context_window_percent: 0.9,
            auto_compact_token_limit: Some(40),
            retain_recent_messages: 1,
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
        config.image_tool_upstream.as_ref(),
        &config.skills_dirs,
        &discovered_skills,
        &extra_tools,
    )?;
    let tool_definitions = registry.values().cloned().collect::<Vec<_>>();
    maybe_compact_messages_with_report(&config, &messages, &tool_definitions, "")
}
