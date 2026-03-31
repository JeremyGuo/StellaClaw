use crate::compaction::{
    COMPACTION_MARKER, ContextCompactionReport, maybe_compact_messages_with_report,
};
use crate::config::AgentConfig;
use crate::llm::{TokenUsage, create_chat_completion};
use crate::message::ChatMessage;
use crate::skills::{SkillMetadata, build_skills_meta_prompt, discover_skills};
use crate::tooling::{Tool, build_tool_registry, build_tool_registry_with_cancel, execute_tool_call};
use anyhow::{Result, anyhow};
use humantime::parse_duration;
use serde_json::Value;
use std::sync::Mutex;
use std::sync::Arc;
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
        "The model is responsible for choosing timeout_seconds for any built-in tool call."
            .to_string(),
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

#[derive(Clone, Debug, Default)]
pub struct SessionRunReport {
    pub messages: Vec<ChatMessage>,
    pub usage: TokenUsage,
}

#[derive(Clone)]
pub struct SessionExecutionControl {
    cancel_flag: Arc<AtomicBool>,
    checkpoint_callback: Option<Arc<dyn Fn(SessionRunReport) + Send + Sync>>,
    stable_prefix_messages: Arc<Mutex<Vec<ChatMessage>>>,
    pending_prefix_rewrite: Arc<Mutex<Option<PendingPrefixRewrite>>>,
}

#[derive(Clone)]
struct PendingPrefixRewrite {
    expected_prefix: Vec<ChatMessage>,
    replacement_prefix: Vec<ChatMessage>,
    usage: TokenUsage,
}

struct PendingToolWaitCompaction {
    cancel_sender: mpsc::Sender<()>,
    join_handle: thread::JoinHandle<Result<Option<PendingPrefixRewrite>>>,
}

impl SessionExecutionControl {
    pub fn new() -> Self {
        Self {
            cancel_flag: Arc::new(AtomicBool::new(false)),
            checkpoint_callback: None,
            stable_prefix_messages: Arc::new(Mutex::new(Vec::new())),
            pending_prefix_rewrite: Arc::new(Mutex::new(None)),
        }
    }

    pub fn with_checkpoint_callback(
        callback: impl Fn(SessionRunReport) + Send + Sync + 'static,
    ) -> Self {
        Self {
            cancel_flag: Arc::new(AtomicBool::new(false)),
            checkpoint_callback: Some(Arc::new(callback)),
            stable_prefix_messages: Arc::new(Mutex::new(Vec::new())),
            pending_prefix_rewrite: Arc::new(Mutex::new(None)),
        }
    }

    pub fn request_cancel(&self) {
        self.cancel_flag.store(true, Ordering::SeqCst);
    }

    pub fn is_cancelled(&self) -> bool {
        self.cancel_flag.load(Ordering::SeqCst)
    }

    pub fn cancel_flag(&self) -> Arc<AtomicBool> {
        Arc::clone(&self.cancel_flag)
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
    ) {
        if let Ok(mut pending) = self.pending_prefix_rewrite.lock() {
            *pending = Some(PendingPrefixRewrite {
                expected_prefix,
                replacement_prefix,
                usage,
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
            });
        }
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

    if let Some(control) = &control {
        control.ensure_not_cancelled()?;
    }

    let registry = build_tool_registry_with_cancel(
        &config.enabled_tools,
        &config.workspace_root,
        &config.upstream,
        config.image_tool_upstream.as_ref(),
        &discovered_skills,
        &extra_tools,
        control.as_ref().map(SessionExecutionControl::cancel_flag),
    )?;
    let tool_definitions = registry.values().cloned().collect::<Vec<_>>();

    let initial_compaction =
        maybe_compact_messages_with_report(&config, &messages, &tool_definitions, &prompt)?;
    usage.add_assign(&initial_compaction.usage);
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
            apply_pending_prefix_rewrite(control, &mut messages, &mut usage);
        }
        if round_index > 0 {
            let compaction =
                maybe_compact_messages_with_report(&config, &messages, &tool_definitions, "")?;
            usage.add_assign(&compaction.usage);
            messages = compaction.messages;
        }
        if let Some(control) = &control {
            control.ensure_not_cancelled()?;
        }
        let outcome = create_chat_completion(&config.upstream, &messages, &tool_definitions, None)?;
        usage.add_assign(&outcome.usage);
        let last_model_response_at = Instant::now();
        let tool_calls = outcome.message.tool_calls.clone().unwrap_or_default();
        messages.push(outcome.message);
        if let Some(control) = &control
            && !extract_assistant_text(&messages).trim().is_empty()
        {
            control.emit_checkpoint(&messages, &usage);
        }
        if tool_calls.is_empty() {
            return Ok(SessionRunReport { messages, usage });
        }

        for tool_call in tool_calls {
            if let Some(control) = &control {
                control.ensure_not_cancelled()?;
                apply_pending_prefix_rewrite(control, &mut messages, &mut usage);
                control.set_stable_prefix_messages(&messages);
            }
            let pending_compaction = start_pending_tool_wait_compaction(
                &config,
                &messages,
                &extra_tools,
                control.as_ref(),
                Some(last_model_response_at),
            )?;
            let result = execute_tool_call(
                &registry,
                &tool_call.function.name,
                tool_call.function.arguments.as_deref(),
            );
            if let Some(control) = &control
                && let Some(rewrite) = finish_pending_tool_wait_compaction(pending_compaction)?
            {
                control.request_prefix_rewrite(
                    rewrite.expected_prefix,
                    rewrite.replacement_prefix,
                    rewrite.usage,
                );
                apply_pending_prefix_rewrite(control, &mut messages, &mut usage);
            }
            messages.push(ChatMessage::tool_output(
                tool_call.id,
                tool_call.function.name,
                result,
            ));
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
    extra_tools: &[Tool],
    control: Option<&SessionExecutionControl>,
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

    let config = config.clone();
    let stable_prefix = stable_prefix.to_vec();
    let extra_tools = extra_tools.to_vec();
    let (cancel_sender, cancel_receiver) = mpsc::channel();
    let join_handle = thread::spawn(move || -> Result<Option<PendingPrefixRewrite>> {
        if !delay.is_zero() {
            match cancel_receiver.recv_timeout(delay) {
                Ok(_) | Err(mpsc::RecvTimeoutError::Disconnected) => return Ok(None),
                Err(mpsc::RecvTimeoutError::Timeout) => {}
            }
        }
        let report =
            compact_session_messages_with_report(stable_prefix.clone(), config, extra_tools)?;
        if !report.compacted {
            return Ok(None);
        }
        Ok(Some(PendingPrefixRewrite {
            expected_prefix: stable_prefix,
            replacement_prefix: report.messages,
            usage: report.usage,
        }))
    });

    Ok(Some(PendingToolWaitCompaction {
        cancel_sender,
        join_handle,
    }))
}

fn finish_pending_tool_wait_compaction(
    pending: Option<PendingToolWaitCompaction>,
) -> Result<Option<PendingPrefixRewrite>> {
    let Some(pending) = pending else {
        return Ok(None);
    };
    let _ = pending.cancel_sender.send(());
    pending
        .join_handle
        .join()
        .map_err(|_| anyhow!("tool wait compaction worker thread panicked"))?
}

fn apply_pending_prefix_rewrite(
    control: &SessionExecutionControl,
    messages: &mut Vec<ChatMessage>,
    usage: &mut TokenUsage,
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
    control.set_stable_prefix_messages(&rewrite.replacement_prefix);
}

#[cfg(test)]
mod tests {
    use super::{
        COMPACTION_MARKER, SessionExecutionControl, finish_pending_tool_wait_compaction,
        start_pending_tool_wait_compaction,
    };
    use crate::config::{AgentConfig, CacheControlConfig, UpstreamConfig};
    use crate::message::ChatMessage;
    use crate::tooling::Tool;
    use serde_json::{Value, json};
    use std::collections::VecDeque;
    use std::io::{Read, Write};
    use std::net::{TcpListener, TcpStream};
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, Mutex};
    use std::thread;
    use std::time::{Duration, Instant};

    struct TestServer {
        address: String,
        responses: Arc<Mutex<VecDeque<Value>>>,
        shutdown: Arc<AtomicBool>,
        handle: Option<thread::JoinHandle<()>>,
    }

    impl TestServer {
        fn start() -> Self {
            let listener = TcpListener::bind("127.0.0.1:0").expect("bind test server");
            listener
                .set_nonblocking(true)
                .expect("set_nonblocking on test server");
            let address = format!("http://{}", listener.local_addr().expect("local addr"));
            let responses = Arc::new(Mutex::new(VecDeque::new()));
            let shutdown = Arc::new(AtomicBool::new(false));
            let handle = {
                let responses = Arc::clone(&responses);
                let shutdown = Arc::clone(&shutdown);
                thread::spawn(move || {
                    loop {
                        if shutdown.load(Ordering::Relaxed) {
                            break;
                        }
                        match listener.accept() {
                            Ok((stream, _)) => handle_stream(stream, &responses),
                            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                                thread::sleep(Duration::from_millis(10));
                            }
                            Err(error) => panic!("accept failed: {}", error),
                        }
                    }
                })
            };
            Self {
                address,
                responses,
                shutdown,
                handle: Some(handle),
            }
        }

        fn push_response(&self, response: Value) {
            self.responses.lock().unwrap().push_back(response);
        }
    }

    impl Drop for TestServer {
        fn drop(&mut self) {
            self.shutdown.store(true, Ordering::Relaxed);
            let _ = TcpStream::connect(self.address.trim_start_matches("http://"));
            if let Some(handle) = self.handle.take() {
                let _ = handle.join();
            }
        }
    }

    fn handle_stream(mut stream: TcpStream, responses: &Arc<Mutex<VecDeque<Value>>>) {
        stream
            .set_nonblocking(false)
            .expect("set blocking mode on accepted stream");
        let mut buffer = vec![0_u8; 64 * 1024];
        let bytes_read = stream.read(&mut buffer).expect("read request");
        let request_text = String::from_utf8_lossy(&buffer[..bytes_read]).to_string();
        let header_end = request_text.find("\r\n\r\n").expect("header end");
        let headers = &request_text[..header_end];
        let body = &request_text[header_end + 4..];
        let request_line = headers.lines().next().expect("request line");
        let mut parts = request_line.split_whitespace();
        let method = parts.next().expect("method");
        let response = responses
            .lock()
            .unwrap()
            .pop_front()
            .expect("queued response");
        if method != "GET" {
            let _: Value = serde_json::from_str(body).expect("parse request body");
        }
        let response_body = response.to_string();
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            response_body.len(),
            response_body
        );
        stream.write_all(response.as_bytes()).expect("write response");
    }

    #[test]
    fn pending_tool_wait_compaction_returns_prefix_rewrite_after_deadline() {
        let server = TestServer::start();
        server.push_response(json!({
            "choices": [{
                "message": {
                    "role": "assistant",
                    "content": "Goals\n- Preserve context"
                }
            }],
            "usage": {
                "prompt_tokens": 120,
                "completion_tokens": 12,
                "total_tokens": 132
            }
        }));

        let config = AgentConfig {
            enabled_tools: Vec::new(),
            upstream: UpstreamConfig {
                base_url: server.address.clone(),
                model: "fake-model".to_string(),
                supports_vision_input: false,
                api_key: None,
                api_key_env: "OPENAI_API_KEY".to_string(),
                chat_completions_path: "/chat/completions".to_string(),
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

        let pending = start_pending_tool_wait_compaction(
            &config,
            &stable_prefix,
            &Vec::<Tool>::new(),
            Some(&SessionExecutionControl::new()),
            Some(Instant::now() - Duration::from_secs(2)),
        )
        .unwrap();

        let rewrite = finish_pending_tool_wait_compaction(pending)
            .unwrap()
            .expect("expected compaction rewrite");
        assert!(rewrite.usage.llm_calls >= 1);
        assert!(
            rewrite.replacement_prefix.iter().any(|message| {
                message
                    .content
                    .as_ref()
                    .and_then(Value::as_str)
                    .is_some_and(|text| text.contains(COMPACTION_MARKER))
            })
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
        &config.upstream,
        config.image_tool_upstream.as_ref(),
        &discovered_skills,
        &extra_tools,
    )?;
    let tool_definitions = registry.values().cloned().collect::<Vec<_>>();
    maybe_compact_messages_with_report(&config, &messages, &tool_definitions, "")
}
