use agent_frame::SessionCompactionStats;
use agent_frame::compaction::ContextCompactionReport;
use agent_frame::config::AgentConfig as FrameAgentConfig;
use agent_frame::message::ChatMessage;
use agent_frame::message::{FunctionCall as FrameFunctionCall, ToolCall as FrameToolCall};
use agent_frame::skills::{build_skills_meta_prompt, discover_skills};
use agent_frame::tooling::build_tool_registry_with_cancel;
use agent_frame::{
    SessionExecutionControl, SessionRunReport, TokenUsage, Tool,
    compact_session_messages_with_report as frame_compact_session_messages_with_report,
    run_session_with_report_controlled as frame_run_session_with_report_controlled,
};
use anyhow::Context;
use anyhow::{Result, anyhow};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use serde_json::json;
use std::collections::BTreeMap;
use std::path::Path;

const ZGENT_COMPAT_MARKER: &str = "[AgentHost ZGent Compatibility Runtime]";

fn zgent_checkout_available() -> bool {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../zgent/crates/zgent-core/Cargo.toml")
        .is_file()
}

fn upstream_error_from_value(value: &Value) -> Option<String> {
    let error = value.get("error")?;
    match error {
        Value::String(text) => Some(text.clone()),
        Value::Object(object) => {
            let message = object
                .get("message")
                .and_then(Value::as_str)
                .map(ToOwned::to_owned);
            let code = object.get("code").map(|value| match value {
                Value::String(text) => text.clone(),
                Value::Number(number) => number.to_string(),
                other => other.to_string(),
            });
            match (message, code) {
                (Some(message), Some(code)) => Some(format!("{message} (code: {code})")),
                (Some(message), None) => Some(message),
                (None, Some(code)) => Some(format!("upstream error code: {code}")),
                (None, None) => Some(error.to_string()),
            }
        }
        other => Some(other.to_string()),
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct ZgentChatCompletionRequest {
    model: String,
    messages: Vec<ZgentChatMessage>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tools: Option<Vec<ZgentToolDefinition>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    temperature: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    max_tokens: Option<u32>,
    stream: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct ZgentChatCompletionResponse {
    choices: Vec<ZgentChoice>,
    #[serde(default)]
    usage: Option<ZgentUsage>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct ZgentChoice {
    message: ZgentChatMessage,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "role")]
enum ZgentChatMessage {
    #[serde(rename = "system")]
    System {
        content: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        timestamp: Option<String>,
    },
    #[serde(rename = "user")]
    User {
        content: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        timestamp: Option<String>,
    },
    #[serde(rename = "assistant")]
    Assistant {
        #[serde(skip_serializing_if = "Option::is_none")]
        content: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        tool_calls: Option<Vec<ZgentToolCall>>,
        #[serde(skip_serializing_if = "Option::is_none")]
        timestamp: Option<String>,
    },
    #[serde(rename = "tool")]
    Tool {
        content: String,
        tool_call_id: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        timestamp: Option<String>,
    },
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct ZgentToolCall {
    id: String,
    #[serde(rename = "type")]
    call_type: String,
    function: ZgentFunctionCall,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct ZgentFunctionCall {
    name: String,
    arguments: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct ZgentToolDefinition {
    #[serde(rename = "type")]
    tool_type: String,
    function: ZgentFunctionDefinition,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct ZgentFunctionDefinition {
    name: String,
    description: String,
    parameters: Value,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct ZgentUsage {
    prompt_tokens: u64,
    completion_tokens: u64,
    total_tokens: u64,
}

#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum AgentBackendKind {
    #[default]
    AgentFrame,
    Zgent,
}

pub fn backend_supports_native_multimodal_input(kind: AgentBackendKind) -> bool {
    matches!(kind, AgentBackendKind::AgentFrame)
}

pub fn run_session_with_report_controlled(
    backend: AgentBackendKind,
    previous_messages: Vec<ChatMessage>,
    prompt: impl Into<String>,
    config: FrameAgentConfig,
    extra_tools: Vec<Tool>,
    control: Option<SessionExecutionControl>,
) -> Result<SessionRunReport> {
    match backend {
        AgentBackendKind::AgentFrame => frame_run_session_with_report_controlled(
            previous_messages,
            prompt,
            config,
            extra_tools,
            control,
        ),
        AgentBackendKind::Zgent => {
            if !zgent_checkout_available() {
                return Err(anyhow!(
                    "the zgent backend is unavailable because the local zgent checkout is missing"
                ));
            }
            run_zgent_session_with_report_controlled(
                previous_messages,
                prompt.into(),
                config,
                extra_tools,
                control,
            )
        }
    }
}

pub fn compact_session_messages_with_report(
    backend: AgentBackendKind,
    previous_messages: Vec<ChatMessage>,
    config: FrameAgentConfig,
    extra_tools: Vec<Tool>,
) -> Result<ContextCompactionReport> {
    match backend {
        AgentBackendKind::AgentFrame => {
            frame_compact_session_messages_with_report(previous_messages, config, extra_tools)
        }
        AgentBackendKind::Zgent => {
            if !zgent_checkout_available() {
                return Err(anyhow!(
                    "the zgent backend is unavailable because the local zgent checkout is missing"
                ));
            }
            Ok(ContextCompactionReport {
                messages: previous_messages,
                usage: TokenUsage::default(),
                compacted: false,
                token_limit: config.auto_compact_token_limit.unwrap_or_default(),
                estimated_tokens_before: 0,
                estimated_tokens_after: 0,
            })
        }
    }
}

fn run_zgent_session_with_report_controlled(
    previous_messages: Vec<ChatMessage>,
    prompt: String,
    config: FrameAgentConfig,
    extra_tools: Vec<Tool>,
    control: Option<SessionExecutionControl>,
) -> Result<SessionRunReport> {
    let discovered_skills = discover_skills(&config.skills_dirs)?;
    let system_prompt = compose_zgent_system_prompt(&config, &discovered_skills);
    let mut messages = ensure_system_message(&previous_messages, &system_prompt);
    if let Some(control) = &control {
        ensure_not_cancelled(control)?;
    }

    let mut tool_config = config.clone();
    tool_config.upstream.native_web_search = None;
    let registry = build_tool_registry_with_cancel(
        &tool_config.enabled_tools,
        &tool_config.workspace_root,
        &tool_config.runtime_state_root,
        &tool_config.upstream,
        tool_config.image_tool_upstream.as_ref(),
        &tool_config.skills_dirs,
        &discovered_skills,
        &extra_tools,
        control
            .as_ref()
            .map(SessionExecutionControl::tool_interrupt_flag),
    )?;
    let tool_definitions = build_zgent_tool_definitions(&registry);
    let client = reqwest::blocking::Client::builder()
        .timeout(std::time::Duration::from_secs_f64(
            tool_config.upstream.timeout_seconds,
        ))
        .build()
        .context("failed to construct zgent compatibility HTTP client")?;

    if !prompt.trim().is_empty() {
        messages.push(ChatMessage::text("user", prompt));
    }

    let mut usage = TokenUsage::default();
    for _round_index in 0..tool_config.max_tool_roundtrips {
        if let Some(control) = &control {
            ensure_not_cancelled(control)?;
        }

        let request = ZgentChatCompletionRequest {
            model: tool_config.upstream.model.clone(),
            messages: messages
                .iter()
                .map(host_message_to_zgent)
                .collect::<Vec<_>>(),
            tools: if tool_definitions.is_empty() {
                None
            } else {
                Some(tool_definitions.clone())
            },
            temperature: Some(0.0),
            max_tokens: Some(4096),
            stream: false,
        };
        let response = send_zgent_chat_completion(&client, &tool_config, &request)
            .context("zgent chat completion failed")?;
        usage.add_assign(&token_usage_from_zgent(response.usage.as_ref()));

        let assistant = response
            .choices
            .into_iter()
            .next()
            .map(|choice| choice.message)
            .ok_or_else(|| anyhow!("zgent chat completion response missing choices[0].message"))?;
        let assistant_host = zgent_message_to_host(&assistant);
        messages.push(assistant_host);

        let tool_calls = match assistant {
            ZgentChatMessage::Assistant { tool_calls, .. } => tool_calls.unwrap_or_default(),
            _ => Vec::new(),
        };
        if tool_calls.is_empty() {
            return Ok(SessionRunReport {
                messages,
                usage,
                compaction: SessionCompactionStats::default(),
            });
        }

        for tool_call in tool_calls {
            if let Some(control) = &control {
                ensure_not_cancelled(control)?;
            }
            let arguments =
                serde_json::from_str::<Value>(&tool_call.function.arguments).unwrap_or(Value::Null);
            let result = match registry.get(&tool_call.function.name) {
                Some(tool) => match tool.invoke(arguments) {
                    Ok(value) => value,
                    Err(error) => json!({ "error": format!("{error:#}") }),
                },
                None => json!({ "error": format!("Unknown tool: {}", tool_call.function.name) }),
            };
            messages.push(ChatMessage::tool_output(
                tool_call.id,
                tool_call.function.name,
                normalize_tool_result(result),
            ));
        }
    }

    Err(anyhow!(
        "Agent stopped after exceeding max_tool_roundtrips={}",
        tool_config.max_tool_roundtrips
    ))
}

fn send_zgent_chat_completion(
    client: &reqwest::blocking::Client,
    config: &FrameAgentConfig,
    request: &ZgentChatCompletionRequest,
) -> Result<ZgentChatCompletionResponse> {
    let url = build_zgent_chat_completions_url(config);
    let mut payload = serde_json::to_value(request)
        .context("failed to serialize zgent chat completion request")?;
    if let Some(object) = payload.as_object_mut() {
        object.insert(
            "model".to_string(),
            Value::String(config.upstream.model.clone()),
        );
    }

    let mut builder = client.post(url).json(&payload);
    if let Some(api_key) = config
        .upstream
        .api_key
        .clone()
        .or_else(|| std::env::var(&config.upstream.api_key_env).ok())
    {
        builder = builder.bearer_auth(api_key);
    }
    for (key, value) in &config.upstream.headers {
        if let Some(value) = value.as_str() {
            builder = builder.header(key, value);
        }
    }

    let response = builder
        .send()
        .context("failed to send zgent chat completion request")?;
    let status = response.status();
    let body = response
        .text()
        .context("failed to read zgent chat completion response body")?;
    if !status.is_success() {
        return Err(anyhow!("Chat completion failed (HTTP {status}): {body}"));
    }

    let value: Value =
        serde_json::from_str(&body).context("failed to parse zgent chat completion response")?;
    if let Some(error_message) = upstream_error_from_value(&value) {
        return Err(anyhow!(
            "zgent chat completion returned an error payload: {}",
            error_message
        ));
    }
    serde_json::from_value(value).context("failed to parse zgent chat completion response")
}

fn build_zgent_chat_completions_url(config: &FrameAgentConfig) -> String {
    let base = config.upstream.base_url.trim_end_matches('/');
    let path = if config.upstream.chat_completions_path.starts_with('/') {
        config.upstream.chat_completions_path.clone()
    } else {
        format!("/{}", config.upstream.chat_completions_path)
    };
    format!("{base}{path}")
}

fn compose_zgent_system_prompt(
    config: &FrameAgentConfig,
    skills: &[agent_frame::skills::SkillMetadata],
) -> String {
    let mut parts = vec![
        ZGENT_COMPAT_MARKER.to_string(),
        "You are running inside ZGent through AgentHost's compatibility layer. Use tools when they materially help.".to_string(),
        "The model is responsible for choosing timeout_seconds for any built-in tool call.".to_string(),
    ];
    if !config.system_prompt.is_empty() {
        parts.push(config.system_prompt.clone());
    }
    let skills_prompt = build_skills_meta_prompt(skills);
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
        first.content = Some(Value::String(system_prompt.to_string()));
        return cloned;
    }
    let mut with_system = vec![ChatMessage::text("system", system_prompt)];
    with_system.extend(cloned);
    with_system
}

fn ensure_not_cancelled(control: &SessionExecutionControl) -> Result<()> {
    if control.is_cancelled() {
        return Err(anyhow!("session execution cancelled"));
    }
    Ok(())
}

fn normalize_tool_result(result: Value) -> String {
    match result {
        Value::String(text) => text,
        other => serde_json::to_string_pretty(&other).unwrap_or_else(|_| "{}".to_string()),
    }
}

fn host_message_to_zgent(message: &ChatMessage) -> ZgentChatMessage {
    match message.role.as_str() {
        "system" => ZgentChatMessage::System {
            content: content_to_text(&message.content),
            timestamp: None,
        },
        "user" => ZgentChatMessage::User {
            content: content_to_text(&message.content),
            timestamp: None,
        },
        "assistant" => ZgentChatMessage::Assistant {
            content: message
                .content
                .as_ref()
                .map(|value| content_to_text(&Some(value.clone()))),
            tool_calls: message.tool_calls.as_ref().map(|calls| {
                calls
                    .iter()
                    .map(|call| ZgentToolCall {
                        id: call.id.clone(),
                        call_type: call.kind.clone(),
                        function: ZgentFunctionCall {
                            name: call.function.name.clone(),
                            arguments: call.function.arguments.clone().unwrap_or_default(),
                        },
                    })
                    .collect()
            }),
            timestamp: None,
        },
        "tool" => ZgentChatMessage::Tool {
            content: content_to_text(&message.content),
            tool_call_id: message.tool_call_id.clone().unwrap_or_default(),
            timestamp: None,
        },
        other => ZgentChatMessage::User {
            content: format!(
                "[unsupported role {}]\n{}",
                other,
                content_to_text(&message.content)
            ),
            timestamp: None,
        },
    }
}

fn zgent_message_to_host(message: &ZgentChatMessage) -> ChatMessage {
    match message {
        ZgentChatMessage::System { content, .. } => ChatMessage::text("system", content),
        ZgentChatMessage::User { content, .. } => ChatMessage::text("user", content),
        ZgentChatMessage::Assistant {
            content,
            tool_calls,
            ..
        } => ChatMessage {
            role: "assistant".to_string(),
            content: content.clone().map(Value::String),
            name: None,
            tool_call_id: None,
            tool_calls: tool_calls.as_ref().map(|calls| {
                calls
                    .iter()
                    .map(|call| FrameToolCall {
                        id: call.id.clone(),
                        kind: call.call_type.clone(),
                        function: FrameFunctionCall {
                            name: call.function.name.clone(),
                            arguments: Some(call.function.arguments.clone()),
                        },
                    })
                    .collect()
            }),
        },
        ZgentChatMessage::Tool {
            content,
            tool_call_id,
            ..
        } => ChatMessage {
            role: "tool".to_string(),
            content: Some(Value::String(content.clone())),
            name: None,
            tool_call_id: Some(tool_call_id.clone()),
            tool_calls: None,
        },
    }
}

fn content_to_text(content: &Option<Value>) -> String {
    match content {
        Some(Value::String(text)) => text.clone(),
        Some(Value::Array(items)) => {
            let mut parts = Vec::new();
            let mut omitted_images = 0_u64;
            for item in items {
                let Some(object) = item.as_object() else {
                    continue;
                };
                let Some(kind) = object.get("type").and_then(Value::as_str) else {
                    continue;
                };
                match kind {
                    "text" | "input_text" | "output_text" => {
                        if let Some(text) = object.get("text").and_then(Value::as_str) {
                            parts.push(text.to_string());
                        }
                    }
                    "image_url" | "input_image" => omitted_images += 1,
                    _ => {}
                }
            }
            if omitted_images > 0 {
                parts.push(format!(
                    "[{} image item(s) omitted because the zgent backend currently uses text-only chat messages for model input.]",
                    omitted_images
                ));
            }
            parts.join("\n")
        }
        Some(other) => other.to_string(),
        None => String::new(),
    }
}

fn token_usage_from_zgent(usage: Option<&ZgentUsage>) -> TokenUsage {
    let Some(usage) = usage else {
        return TokenUsage::default();
    };
    TokenUsage {
        llm_calls: 1,
        prompt_tokens: usage.prompt_tokens as u64,
        completion_tokens: usage.completion_tokens as u64,
        total_tokens: usage.total_tokens as u64,
        cache_hit_tokens: 0,
        cache_miss_tokens: 0,
        cache_read_tokens: 0,
        cache_write_tokens: 0,
    }
}

fn build_zgent_tool_definitions(registry: &BTreeMap<String, Tool>) -> Vec<ZgentToolDefinition> {
    registry
        .values()
        .map(|tool| ZgentToolDefinition {
            tool_type: "function".to_string(),
            function: ZgentFunctionDefinition {
                name: tool.name.clone(),
                description: tool.description.clone(),
                parameters: tool.parameters.clone(),
            },
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::{AgentBackendKind, backend_supports_native_multimodal_input};

    #[test]
    fn only_agent_frame_backend_supports_native_multimodal_input() {
        assert!(backend_supports_native_multimodal_input(
            AgentBackendKind::AgentFrame
        ));
        assert!(!backend_supports_native_multimodal_input(
            AgentBackendKind::Zgent
        ));
    }

    mod zgent_tests {
        use super::AgentBackendKind;
        use crate::backend::run_session_with_report_controlled;
        use crate::backend::zgent_checkout_available;
        use agent_frame::config::UpstreamConfig;
        use agent_frame::{Tool, extract_assistant_text};
        use serde_json::{Value, json};
        use std::collections::VecDeque;
        use std::fs;
        use std::io::{Read, Write};
        use std::net::{TcpListener, TcpStream};
        use std::path::PathBuf;
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::sync::{Arc, Mutex};
        use std::thread;
        use tempfile::TempDir;

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
                    .expect("nonblocking listener");
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
                                    thread::sleep(std::time::Duration::from_millis(10));
                                }
                                Err(error) => panic!("accept failed: {error}"),
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

            fn push_response(&self, value: Value) {
                self.responses.lock().unwrap().push_back(value);
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
            stream.set_nonblocking(false).expect("blocking stream");
            let mut buffer = vec![0_u8; 64 * 1024];
            let bytes_read = stream.read(&mut buffer).expect("read request");
            let request_text = String::from_utf8_lossy(&buffer[..bytes_read]).to_string();
            let response_json = responses
                .lock()
                .unwrap()
                .pop_front()
                .expect("queued response");
            let body = response_json.to_string();
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            );
            if request_text.starts_with("POST ") {
                stream
                    .write_all(response.as_bytes())
                    .expect("write response");
            }
        }

        fn test_config(base_url: &str, workspace_root: PathBuf) -> agent_frame::AgentConfig {
            agent_frame::AgentConfig {
                enabled_tools: vec!["read_file".to_string()],
                upstream: UpstreamConfig {
                    base_url: base_url.to_string(),
                    model: "test-model".to_string(),
                    supports_vision_input: false,
                    api_key: None,
                    api_key_env: "TEST_API_KEY".to_string(),
                    chat_completions_path: "/chat/completions".to_string(),
                    timeout_seconds: 10.0,
                    context_window_tokens: 128_000,
                    cache_control: None,
                    reasoning: None,
                    headers: serde_json::Map::new(),
                    native_web_search: None,
                    external_web_search: None,
                },
                image_tool_upstream: None,
                skills_dirs: Vec::new(),
                system_prompt: "You are a test backend.".to_string(),
                max_tool_roundtrips: 4,
                runtime_state_root: workspace_root.clone(),
                workspace_root,
                enable_context_compression: false,
                effective_context_window_percent: 0.9,
                auto_compact_token_limit: None,
                retain_recent_messages: 8,
            }
        }

        #[test]
        fn zgent_backend_returns_text_response() {
            if !zgent_checkout_available() {
                return;
            }
            let server = TestServer::start();
            server.push_response(json!({
                "id": "resp-1",
                "choices": [{
                    "index": 0,
                    "message": {
                        "role": "assistant",
                        "content": "zgent-ok"
                    },
                    "finish_reason": "stop"
                }],
                "usage": {
                    "prompt_tokens": 12,
                    "completion_tokens": 3,
                    "total_tokens": 15
                }
            }));

            let temp_dir = TempDir::new().unwrap();
            let report = run_session_with_report_controlled(
                AgentBackendKind::Zgent,
                vec![agent_frame::ChatMessage::text("user", "hello")],
                "",
                test_config(&server.address, temp_dir.path().to_path_buf()),
                Vec::new(),
                None,
            )
            .unwrap();

            assert_eq!(extract_assistant_text(&report.messages), "zgent-ok");
            assert_eq!(report.usage.llm_calls, 1);
            assert_eq!(report.usage.total_tokens, 15);
        }

        #[test]
        fn zgent_backend_executes_wrapped_tools() {
            if !zgent_checkout_available() {
                return;
            }
            let server = TestServer::start();
            server.push_response(json!({
                "id": "resp-1",
                "choices": [{
                    "index": 0,
                    "message": {
                        "role": "assistant",
                        "tool_calls": [{
                            "id": "call-1",
                            "type": "function",
                            "function": {
                                "name": "echo",
                                "arguments": "{\"value\":\"ok\"}"
                            }
                        }]
                    },
                    "finish_reason": "tool_calls"
                }],
                "usage": {
                    "prompt_tokens": 10,
                    "completion_tokens": 4,
                    "total_tokens": 14
                }
            }));
            server.push_response(json!({
                "id": "resp-2",
                "choices": [{
                    "index": 0,
                    "message": {
                        "role": "assistant",
                        "content": "done"
                    },
                    "finish_reason": "stop"
                }],
                "usage": {
                    "prompt_tokens": 20,
                    "completion_tokens": 2,
                    "total_tokens": 22
                }
            }));

            let temp_dir = TempDir::new().unwrap();
            fs::write(temp_dir.path().join("dummy.txt"), "hello").unwrap();
            let echo = Tool::new(
                "echo",
                "Echo a value.",
                json!({
                    "type": "object",
                    "properties": {
                        "value": {"type": "string"}
                    },
                    "required": ["value"],
                    "additionalProperties": false
                }),
                |arguments| Ok(json!({ "echo": arguments["value"].clone() })),
            );

            let report = run_session_with_report_controlled(
                AgentBackendKind::Zgent,
                vec![agent_frame::ChatMessage::text("user", "hello")],
                "",
                test_config(&server.address, temp_dir.path().to_path_buf()),
                vec![echo],
                None,
            )
            .unwrap();

            assert_eq!(extract_assistant_text(&report.messages), "done");
            assert_eq!(report.usage.llm_calls, 2);
            assert_eq!(report.usage.total_tokens, 36);
            assert!(report.messages.iter().any(|message| message.role == "tool"));
        }

        #[tokio::test(flavor = "current_thread")]
        async fn zgent_backend_is_safe_inside_tokio_context() {
            if !zgent_checkout_available() {
                return;
            }
            let server = TestServer::start();
            server.push_response(json!({
                "id": "resp-1",
                "choices": [{
                    "index": 0,
                    "message": {
                        "role": "assistant",
                        "content": "async-ok"
                    },
                    "finish_reason": "stop"
                }],
                "usage": {
                    "prompt_tokens": 8,
                    "completion_tokens": 2,
                    "total_tokens": 10
                }
            }));

            let temp_dir = TempDir::new().unwrap();
            let report = tokio::task::spawn_blocking(move || {
                run_session_with_report_controlled(
                    AgentBackendKind::Zgent,
                    vec![agent_frame::ChatMessage::text("user", "hello")],
                    "",
                    test_config(&server.address, temp_dir.path().to_path_buf()),
                    Vec::new(),
                    None,
                )
            })
            .await
            .unwrap()
            .unwrap();

            assert_eq!(extract_assistant_text(&report.messages), "async-ok");
            assert_eq!(report.usage.total_tokens, 10);
        }

        #[test]
        fn zgent_backend_reports_error_payloads_in_success_responses() {
            if !zgent_checkout_available() {
                return;
            }
            let server = TestServer::start();
            server.push_response(json!({
                "error": {
                    "message": "Insufficient credits",
                    "code": 402
                }
            }));

            let temp_dir = TempDir::new().unwrap();
            let error = run_session_with_report_controlled(
                AgentBackendKind::Zgent,
                vec![agent_frame::ChatMessage::text("user", "hello")],
                "",
                test_config(&server.address, temp_dir.path().to_path_buf()),
                Vec::new(),
                None,
            )
            .unwrap_err();

            let rendered = format!("{error:#}");
            assert!(rendered.contains("zgent chat completion returned an error payload"));
            assert!(rendered.contains("Insufficient credits"));
        }
    }
}
