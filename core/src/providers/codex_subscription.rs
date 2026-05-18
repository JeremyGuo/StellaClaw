use std::{
    fs,
    net::{TcpStream, ToSocketAddrs},
    path::PathBuf,
    sync::{Arc, Mutex},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use base64::Engine as _;
use reqwest::{blocking::Client, StatusCode};
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};
use tungstenite::{
    client::IntoClientRequest, http::HeaderValue, stream::MaybeTlsStream, Message, WebSocket,
};
use url::Url;

use crate::{
    model_config::{ModelConfig, ProviderType},
    session_actor::{
        media_tool_definitions, normalize_messages_for_model, BuiltinBaseTool, ChatMessage,
        ChatMessageItem, ChatRole, CompactionItem, ContextItem, ExtTool, FileItem, LocalToolError,
        ReasoningItem, ToolBackend, ToolCallContext, ToolCallItem, ToolCatalog, ToolCatalogError,
        ToolConcurrency, ToolDefinition, ToolEnablementEnv, ToolEntry, ToolExecutionMode,
        ToolResultContent, ToolSet,
    },
};

use super::{
    common::{
        account_id_from_access_token, ensure_request_payload_size, is_image_file, nonce,
        provider_error_kind, provider_error_message, token_usage_from_value,
    },
    OutputPersistor, ProviderBackend, ProviderCompactionMode, ProviderError, ProviderRequest,
    ProviderStreamEvent,
};

const OPENAI_BETA_RESPONSES_WEBSOCKETS: &str = "responses_websockets=2026-02-06";
const CHATGPT_REFRESH_TOKEN_URL: &str = "https://auth.openai.com/oauth/token";
const CHATGPT_REFRESH_TOKEN_URL_OVERRIDE_ENV: &str = "CODEX_REFRESH_TOKEN_URL_OVERRIDE";
const CODEX_MODELS_URL_OVERRIDE_ENV: &str = "STELLACLAW_CODEX_MODELS_URL";
const CODEX_CLIENT_ID: &str = "app_EMoamEEZ73f0CkXaXp7hrann";
const CODEX_PERSONALITY_PLACEHOLDER: &str = "{{ personality }}";
const CODEX_PRAGMATIC_PERSONALITY_PROMPT: &str = r#"# Personality

You are a deeply pragmatic, effective software engineer. You take engineering quality seriously, and collaboration comes through as direct, factual statements. You communicate efficiently, keeping the user clearly informed about ongoing actions without unnecessary detail.

## Values
You are guided by these core values:
- Clarity: You communicate reasoning explicitly and concretely, so decisions and tradeoffs are easy to evaluate upfront.
- Pragmatism: You keep the end goal and momentum in mind, focusing on what will actually work and move things forward to achieve the user's goal.
- Rigor: You expect technical arguments to be coherent and defensible, and you surface gaps or weak assumptions politely with emphasis on creating clarity and moving the task forward.

## Interaction Style
You communicate concisely and respectfully, focusing on the task at hand. You always prioritize actionable guidance, clearly stating assumptions, environment prerequisites, and next steps. Unless explicitly asked, you avoid excessively verbose explanations about your work.

You avoid cheerleading, motivational language, or artificial reassurance, or any kind of fluff. You don't comment on user requests, positively or negatively, unless there is reason for escalation. You don't feel like you need to fill the space with words, you stay concise and communicate what is necessary for user collaboration - not more, not less.

## Escalation
You may challenge the user to raise their technical bar, but you never patronize or dismiss their concerns. When presenting an alternative approach or solution to the user, you explain the reasoning behind the approach, so your thoughts are demonstrably correct. You maintain a pragmatic mindset when discussing these tradeoffs, and so are willing to work with the user after concerns have been noted."#;

pub struct CodexSubscriptionProvider {
    output_persistor: OutputPersistor,
    auth_manager: CodexSubscriptionAuthManager,
    socket: Mutex<Option<WebSocket<MaybeTlsStream<TcpStream>>>>,
    models_cache: Mutex<Option<CachedCodexModels>>,
    session_id: String,
    installation_id: String,
}

#[derive(Debug, Default)]
struct StreamAccumulator {
    output_items: Vec<Value>,
    active_output_item_id: Option<String>,
    active_reasoning_item_id: Option<String>,
    streamed_tool_inputs: Vec<String>,
}

#[derive(Debug, Default)]
struct CodexSubscriptionAuthManager {
    cached: Mutex<Option<CodexAuthMaterial>>,
}

#[derive(Debug, Clone)]
struct CodexAuthMaterial {
    access_token: String,
    refresh_token: Option<String>,
    account_id: String,
    is_fedramp_account: bool,
    expires_at: Option<i64>,
    source: CodexAuthSource,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct CachedCodexModels {
    version: u32,
    fetched_at_ms: u128,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    etag: Option<String>,
    models: Vec<CodexModelInfo>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
struct CodexModelsResponse {
    models: Vec<CodexModelInfo>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
struct CodexModelInfo {
    #[serde(alias = "id", alias = "model")]
    slug: String,
    #[serde(default)]
    model_messages: Option<CodexModelMessages>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
struct CodexModelMessages {
    #[serde(default)]
    instructions_template: Option<String>,
    #[serde(default)]
    instructions_variables: Option<CodexModelInstructionsVariables>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
struct CodexModelInstructionsVariables {
    #[serde(default)]
    personality_pragmatic: Option<String>,
}

#[derive(Debug, Clone)]
enum CodexAuthSource {
    AuthJson(PathBuf),
    Env,
}

#[derive(Debug, Deserialize, Serialize)]
struct RefreshTokenRequest {
    client_id: &'static str,
    grant_type: &'static str,
    refresh_token: String,
}

#[derive(Debug, Deserialize)]
struct RefreshTokenResponse {
    access_token: Option<String>,
    refresh_token: Option<String>,
    id_token: Option<String>,
}

impl CodexSubscriptionProvider {
    pub fn new() -> Self {
        Self::default()
    }

    fn send_once(
        &self,
        model_config: &ModelConfig,
        request: &ProviderRequest<'_>,
        on_stream: &mut dyn FnMut(ProviderStreamEvent),
    ) -> Result<ChatMessage, ProviderError> {
        let auth = self.auth_manager.resolve(model_config)?;

        let payload = self.build_payload(model_config, request)?;

        match self.send_with_auth(model_config, payload.clone(), &auth, on_stream) {
            Ok(message) => Ok(message),
            Err(error) if is_unauthorized(&error) => {
                self.clear_socket();
                let refreshed = self.auth_manager.refresh(model_config, &auth)?;
                self.send_with_auth(model_config, payload, &refreshed, on_stream)
            }
            Err(error) => Err(error),
        }
    }

    fn build_payload(
        &self,
        model_config: &ModelConfig,
        request: &ProviderRequest<'_>,
    ) -> Result<Map<String, Value>, ProviderError> {
        let mut payload = Map::new();
        payload.insert(
            "model".to_string(),
            Value::String(model_config.model_name.clone()),
        );
        payload.insert(
            "input".to_string(),
            Value::Array(build_responses_input(request.messages, model_config)?),
        );
        if let Some(system_prompt) = request.system_prompt {
            if !system_prompt.trim().is_empty() {
                payload.insert(
                    "instructions".to_string(),
                    Value::String(system_prompt.to_string()),
                );
            }
        }
        if !request.tools.is_empty() {
            payload.insert(
                "tools".to_string(),
                Value::Array(
                    request
                        .tools
                        .iter()
                        .map(|tool| tool.responses_tool_schema())
                        .collect(),
                ),
            );
        }
        if let Some(reasoning) = codex_reasoning_payload(model_config) {
            payload.insert(
                "include".to_string(),
                Value::Array(vec![Value::String(
                    "reasoning.encrypted_content".to_string(),
                )]),
            );
            payload.insert("reasoning".to_string(), reasoning);
        } else {
            payload.insert("include".to_string(), Value::Array(Vec::new()));
        }
        payload.insert("store".to_string(), Value::Bool(false));
        payload.insert("stream".to_string(), Value::Bool(true));
        payload.insert("tool_choice".to_string(), Value::String("auto".to_string()));
        payload.insert("parallel_tool_calls".to_string(), Value::Bool(true));
        if let Some(service_tier) = codex_service_tier_payload(model_config) {
            payload.insert("service_tier".to_string(), Value::String(service_tier));
        }
        payload.insert(
            "prompt_cache_key".to_string(),
            Value::String(self.session_id.clone()),
        );
        payload.insert(
            "client_metadata".to_string(),
            json!({
                "x-codex-installation-id": self.installation_id,
                "x-codex-window-id": format!("{}:0", self.session_id),
            }),
        );

        Ok(payload)
    }

    fn send_with_auth(
        &self,
        model_config: &ModelConfig,
        payload: Map<String, Value>,
        auth: &CodexAuthMaterial,
        on_stream: &mut dyn FnMut(ProviderStreamEvent),
    ) -> Result<ChatMessage, ProviderError> {
        let socket = {
            let mut cached = self.socket.lock().expect("mutex poisoned");
            cached.take()
        };

        let response = self.send_response_create_with_transport_reconnect(
            model_config,
            payload,
            auth,
            socket,
            on_stream,
        )?;
        responses_value_to_chat_message(&response, model_config, &self.output_persistor)
    }

    fn send_response_create_with_transport_reconnect(
        &self,
        model_config: &ModelConfig,
        payload: Map<String, Value>,
        auth: &CodexAuthMaterial,
        initial_socket: Option<WebSocket<MaybeTlsStream<TcpStream>>>,
        on_stream: &mut dyn FnMut(ProviderStreamEvent),
    ) -> Result<Value, ProviderError> {
        let mut socket = initial_socket;
        let mut retried_transport_error = false;

        loop {
            let mut active_socket = match socket.take() {
                Some(socket) => socket,
                None => connect_codex_websocket(
                    model_config,
                    auth,
                    &self.session_id,
                    &self.installation_id,
                )?,
            };
            let response =
                send_response_create(&mut active_socket, payload.clone(), model_config, on_stream);
            if response.is_ok() {
                let mut cached = self.socket.lock().expect("mutex poisoned");
                *cached = Some(active_socket);
            }

            if is_websocket_transport_error(&response) && !retried_transport_error {
                retried_transport_error = true;
                self.clear_socket();
                socket = None;
                continue;
            }

            return response;
        }
    }

    fn clear_socket(&self) {
        let mut cached = self.socket.lock().expect("mutex poisoned");
        *cached = None;
    }

    fn compact_history_once(
        &self,
        model_config: &ModelConfig,
        request: &ProviderRequest<'_>,
    ) -> Result<Vec<ChatMessage>, ProviderError> {
        let auth = self.auth_manager.resolve(model_config)?;
        let payload = self.build_compact_payload(model_config, request)?;
        match self.send_compact_with_auth(model_config, payload.clone(), &auth) {
            Ok(messages) => Ok(messages),
            Err(error) if is_unauthorized(&error) => {
                let refreshed = self.auth_manager.refresh(model_config, &auth)?;
                self.send_compact_with_auth(model_config, payload, &refreshed)
            }
            Err(error) => Err(error),
        }
    }

    fn build_compact_payload(
        &self,
        model_config: &ModelConfig,
        request: &ProviderRequest<'_>,
    ) -> Result<Map<String, Value>, ProviderError> {
        let mut payload = Map::new();
        payload.insert(
            "model".to_string(),
            Value::String(model_config.model_name.clone()),
        );
        payload.insert(
            "input".to_string(),
            Value::Array(build_responses_input(request.messages, model_config)?),
        );
        if let Some(system_prompt) = request.system_prompt {
            if !system_prompt.trim().is_empty() {
                payload.insert(
                    "instructions".to_string(),
                    Value::String(system_prompt.to_string()),
                );
            }
        }
        payload.insert(
            "tools".to_string(),
            Value::Array(
                request
                    .tools
                    .iter()
                    .map(|tool| tool.responses_tool_schema())
                    .collect(),
            ),
        );
        payload.insert("parallel_tool_calls".to_string(), Value::Bool(true));
        if let Some(reasoning) = codex_reasoning_payload(model_config) {
            payload.insert("reasoning".to_string(), reasoning);
        }
        if let Some(service_tier) = codex_service_tier_payload(model_config) {
            payload.insert("service_tier".to_string(), Value::String(service_tier));
        }
        payload.insert(
            "prompt_cache_key".to_string(),
            Value::String(self.session_id.clone()),
        );
        Ok(payload)
    }

    fn send_compact_with_auth(
        &self,
        model_config: &ModelConfig,
        payload: Map<String, Value>,
        auth: &CodexAuthMaterial,
    ) -> Result<Vec<ChatMessage>, ProviderError> {
        let endpoint = build_codex_compact_url(model_config)?;
        let client = Client::builder()
            .connect_timeout(Duration::from_secs(model_config.conn_timeout_secs()))
            .timeout(Duration::from_secs(model_config.request_timeout_secs()))
            .build()
            .map_err(ProviderError::BuildHttpClient)?;
        let mut request = client
            .post(endpoint.clone())
            .header("authorization", format!("Bearer {}", auth.access_token))
            .header("chatgpt-account-id", auth.account_id.clone())
            .header("openai-beta", OPENAI_BETA_RESPONSES_WEBSOCKETS)
            .header("user-agent", "stellaclaw")
            .header("x-client-request-id", nonce("compact"))
            .header("x-codex-installation-id", self.installation_id.clone())
            .header("session_id", self.session_id.clone())
            .header("x-codex-window-id", format!("{}:0", self.session_id))
            .json(&Value::Object(payload));
        if auth.is_fedramp_account {
            request = request.header("x-openai-fedramp", "true");
        }

        let response = request.send().map_err(ProviderError::request)?;
        let status = response.status();
        let body = response.text().map_err(ProviderError::DecodeResponse)?;
        if !status.is_success() {
            return Err(ProviderError::HttpStatus {
                url: endpoint.to_string(),
                status: status.as_u16(),
                body,
            });
        }
        let value: Value = serde_json::from_str(&body).map_err(ProviderError::DecodeJson)?;
        compact_response_value_to_chat_messages(&value)
    }

    fn codex_models(&self, model_config: &ModelConfig) -> Result<CachedCodexModels, ProviderError> {
        if let Some(cached) = self.models_cache.lock().expect("mutex poisoned").clone() {
            return Ok(cached);
        }

        if let Some(cached) = read_cached_codex_models()? {
            *self.models_cache.lock().expect("mutex poisoned") = Some(cached.clone());
            return Ok(cached);
        }

        let auth = self.auth_manager.resolve(model_config)?;
        let fetched = match fetch_codex_models(model_config, &auth, &self.installation_id) {
            Ok(models) => models,
            Err(error) if is_unauthorized(&error) => {
                let refreshed = self.auth_manager.refresh(model_config, &auth)?;
                fetch_codex_models(model_config, &refreshed, &self.installation_id)?
            }
            Err(error) => return Err(error),
        };
        write_cached_codex_models(&fetched)?;
        *self.models_cache.lock().expect("mutex poisoned") = Some(fetched.clone());
        Ok(fetched)
    }
}

fn render_codex_model_instructions(messages: &CodexModelMessages) -> Option<String> {
    let template = messages.instructions_template.as_deref()?.trim();
    if template.is_empty() {
        return None;
    }

    let pragmatic = messages
        .instructions_variables
        .as_ref()
        .and_then(|variables| variables.personality_pragmatic.as_deref())
        .map(str::trim)
        .filter(|prompt| !prompt.is_empty())
        .unwrap_or(CODEX_PRAGMATIC_PERSONALITY_PROMPT);

    Some(template.replace(CODEX_PERSONALITY_PLACEHOLDER, pragmatic))
}

impl Default for CodexSubscriptionProvider {
    fn default() -> Self {
        let session_id = std::env::var("STELLACLAW_SESSION_ID")
            .or_else(|_| std::env::var("CODEX_SESSION_ID"))
            .unwrap_or_else(|_| nonce("session"));
        let installation_id = std::env::var("CODEX_INSTALLATION_ID")
            .or_else(|_| std::env::var("STELLACLAW_INSTALLATION_ID"))
            .unwrap_or_else(|_| session_id.clone());
        Self {
            output_persistor: OutputPersistor,
            auth_manager: CodexSubscriptionAuthManager::default(),
            socket: Mutex::new(None),
            models_cache: Mutex::new(None),
            session_id,
            installation_id,
        }
    }
}

impl ProviderBackend for CodexSubscriptionProvider {
    fn compaction_mode(&self, _model_config: &ModelConfig) -> ProviderCompactionMode {
        ProviderCompactionMode::Builtin
    }

    fn compact_history(
        &self,
        model_config: &ModelConfig,
        request: ProviderRequest<'_>,
    ) -> Result<Option<Vec<ChatMessage>>, ProviderError> {
        self.compact_history_once(model_config, &request).map(Some)
    }

    fn tool_set(&self, _model_config: &ModelConfig) -> Option<Arc<dyn ToolSet>> {
        Some(Arc::new(CodexSubscriptionToolSet))
    }

    fn system_prompt_for_model(
        &self,
        model_config: &ModelConfig,
    ) -> Result<Option<String>, ProviderError> {
        let models = self.codex_models(model_config)?;
        let model = models
            .models
            .iter()
            .find(|model| model.slug == model_config.model_name)
            .ok_or_else(|| {
                ProviderError::InvalidResponse(format!(
                    "codex models endpoint did not return model {}",
                    model_config.model_name
                ))
            })?;
        Ok(model
            .model_messages
            .as_ref()
            .and_then(render_codex_model_instructions))
    }

    fn normalize_messages_for_provider(
        &self,
        _model_config: &ModelConfig,
        messages: &[ChatMessage],
    ) -> Vec<ChatMessage> {
        messages
            .iter()
            .filter_map(normalize_message_for_codex_provider)
            .collect()
    }

    fn send(
        &self,
        model_config: &ModelConfig,
        request: ProviderRequest<'_>,
    ) -> Result<ChatMessage, ProviderError> {
        self.send_once(model_config, &request, &mut |_| {})
    }

    fn send_with_stream(
        &self,
        model_config: &ModelConfig,
        request: ProviderRequest<'_>,
        on_stream: &mut dyn FnMut(ProviderStreamEvent),
    ) -> Result<ChatMessage, ProviderError> {
        self.send_once(model_config, &request, on_stream)
    }

    fn before_retry(&self, _model_config: &ModelConfig, error: &ProviderError) {
        if matches!(error, ProviderError::WebSocket(_)) {
            self.clear_socket();
        }
    }
}

struct CodexSubscriptionToolSet;

impl ToolSet for CodexSubscriptionToolSet {
    fn register(
        &self,
        catalog: &mut ToolCatalog,
        env: &ToolEnablementEnv<'_>,
    ) -> Result<(), ToolCatalogError> {
        for tool in CODEX_BUILTIN_BASE_TOOLS {
            add_builtin_base_tool(catalog, env, tool)?;
        }
        add_native_image_generation_tool(catalog, env)?;
        catalog.add_enabled_tool_entry(ToolEntry::Ext(Arc::new(CodexExecCommandTool)), env)?;
        catalog.add_enabled_tool_entry(ToolEntry::Ext(Arc::new(CodexWriteStdinTool)), env)?;
        catalog.add_enabled_tool_entry(ToolEntry::Ext(Arc::new(CodexExecStopTool)), env)?;
        catalog.add_enabled_tool_entry(ToolEntry::Ext(Arc::new(CodexExecMakeVisibleTool)), env)?;
        catalog.add_enabled_tool_entry(ToolEntry::Ext(Arc::new(CodexApplyPatchTool)), env)?;
        Ok(())
    }
}

const CODEX_BUILTIN_BASE_TOOLS: &[&str] = &[
    "attachment_make_visible",
    "web_fetch",
    "web_search",
    "image_view",
    "pdf_view",
    "audio_view",
    "image_generation",
    "skill_load",
    "skill_create",
    "skill_update",
    "skill_delete",
    "update_plan",
    "cron_tasks_list",
    "cron_task_get",
    "cron_task_create",
    "cron_task_update",
    "cron_task_remove",
    "background_agents_list",
    "background_agent_start",
    "terminate",
    "subagent_start",
    "subagent_kill",
    "subagent_join",
    "memory_search",
    "memory_write",
    "memory_update",
    "memory_delete",
];

fn add_builtin_base_tool(
    catalog: &mut ToolCatalog,
    env: &ToolEnablementEnv<'_>,
    tool_name: &str,
) -> Result<(), ToolCatalogError> {
    let Some(tool) = BuiltinBaseTool::definition(tool_name, env.options) else {
        return Ok(());
    };
    if tool.is_enabled_for_model(env.model_config) {
        catalog.add(tool)?;
    }
    Ok(())
}

fn add_native_image_generation_tool(
    catalog: &mut ToolCatalog,
    env: &ToolEnablementEnv<'_>,
) -> Result<(), ToolCatalogError> {
    for tool in media_tool_definitions(env.options) {
        if tool.name == "image_generation"
            && matches!(
                tool.backend,
                ToolBackend::ProviderNative {
                    kind: crate::session_actor::ProviderNativeToolKind::ImageGeneration
                }
            )
            && tool.is_enabled_for_model(env.model_config)
        {
            catalog.add(tool)?;
        }
    }
    Ok(())
}

struct CodexExecCommandTool;

impl ExtTool for CodexExecCommandTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition::new(
            "exec_command",
            "Runs a command in a fresh shell process, returning output or a session id for ongoing interaction.",
            json!({
                "type": "object",
                "properties": {
                    "cmd": {"type": "string", "description": "Shell command to execute."},
                    "workdir": {"type": "string", "description": "Optional working directory to run the command in; defaults to the turn cwd."},
                    "shell": {"type": "string", "description": "Shell binary to launch. Defaults to the user's default shell."},
                    "login": {"type": "boolean", "description": "Whether to run the shell with -l/-i semantics. Defaults to false."},
                    "tty": {"type": "boolean", "description": "Whether to allocate a TTY for the command. Defaults to false; set to true to keep stdin writable."},
                    "yield_time_ms": {"type": "integer", "minimum": 250, "maximum": 30000, "description": "How long to wait in milliseconds for output before yielding."},
                    "timeout_ms": {"type": "integer", "minimum": 0, "maximum": 86400000},
                    "max_output_tokens": {"type": "integer", "minimum": 0, "maximum": 50000, "description": "Maximum output tokens to return. Excess output will be truncated."}
                },
                "required": ["cmd"],
                "additionalProperties": false
            }),
            ToolExecutionMode::Interruptible,
            ToolBackend::Local,
        )
        .with_concurrency(ToolConcurrency::Serial)
    }

    fn base_tool_id(&self) -> &'static str {
        "shell_exec"
    }

    fn call(
        &self,
        ctx: &ToolCallContext<'_>,
        args: Value,
    ) -> Result<ToolResultContent, LocalToolError> {
        let Value::Object(mut arguments) = args else {
            return Err(LocalToolError::InvalidArguments(
                "tool arguments must be a JSON object".to_string(),
            ));
        };
        let command = arguments
            .remove("cmd")
            .ok_or_else(|| LocalToolError::InvalidArguments("missing required cmd".to_string()))?;
        arguments.insert("command".to_string(), command);
        BuiltinBaseTool::call_local(self.base_tool_id(), ctx, Value::Object(arguments))
    }
}

struct CodexWriteStdinTool;

impl ExtTool for CodexWriteStdinTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition::new(
            "write_stdin",
            "Writes characters to an existing shell session and returns recent output.",
            json!({
                "type": "object",
                "properties": {
                    "session_id": {"type": "string", "description": "Identifier of the running shell session."},
                    "chars": {"type": "string", "description": "Bytes to write to stdin; pass an empty string to poll recent output."},
                    "yield_time_ms": {"type": "integer", "minimum": 250, "maximum": 30000, "description": "How long to wait in milliseconds for output before yielding."},
                    "max_output_tokens": {"type": "integer", "minimum": 0, "maximum": 50000, "description": "Maximum output tokens to return. Excess output will be truncated."}
                },
                "required": ["session_id"],
                "additionalProperties": false
            }),
            ToolExecutionMode::Interruptible,
            ToolBackend::Local,
        )
        .with_concurrency(ToolConcurrency::Serial)
    }

    fn base_tool_id(&self) -> &'static str {
        "shell_write_stdin"
    }

    fn call(
        &self,
        ctx: &ToolCallContext<'_>,
        args: Value,
    ) -> Result<ToolResultContent, LocalToolError> {
        let Value::Object(mut arguments) = args else {
            return Err(LocalToolError::InvalidArguments(
                "tool arguments must be a JSON object".to_string(),
            ));
        };
        let session_id = arguments.remove("session_id").ok_or_else(|| {
            LocalToolError::InvalidArguments("missing required session_id".to_string())
        })?;
        arguments.insert("process_id".to_string(), session_id);
        BuiltinBaseTool::call_local(self.base_tool_id(), ctx, Value::Object(arguments))
    }
}

struct CodexExecStopTool;

impl ExtTool for CodexExecStopTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition::new(
            "exec_stop",
            "Stop a running shell session by session_id. signal defaults to terminate.",
            json!({
                "type": "object",
                "properties": {
                    "session_id": {"type": "string", "description": "Identifier of the running shell session."},
                    "signal": {"type": "string", "enum": ["interrupt", "terminate", "kill"]}
                },
                "required": ["session_id"],
                "additionalProperties": false
            }),
            ToolExecutionMode::Immediate,
            ToolBackend::Local,
        )
        .with_concurrency(ToolConcurrency::Serial)
    }

    fn base_tool_id(&self) -> &'static str {
        "shell_stop"
    }

    fn call(
        &self,
        ctx: &ToolCallContext<'_>,
        args: Value,
    ) -> Result<ToolResultContent, LocalToolError> {
        let Value::Object(mut arguments) = args else {
            return Err(LocalToolError::InvalidArguments(
                "tool arguments must be a JSON object".to_string(),
            ));
        };
        let session_id = arguments.remove("session_id").ok_or_else(|| {
            LocalToolError::InvalidArguments("missing required session_id".to_string())
        })?;
        arguments.insert("process_id".to_string(), session_id);
        BuiltinBaseTool::call_local(self.base_tool_id(), ctx, Value::Object(arguments))
    }
}

struct CodexExecMakeVisibleTool;

impl ExtTool for CodexExecMakeVisibleTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition::new(
            "exec_make_visible",
            "Copy a local workspace-relative file or directory to the fixed remote workspace so remote exec_command can read it.",
            json!({
                "type": "object",
                "properties": {
                    "path": {"type": "string"},
                    "timeout_seconds": {"type": "number"}
                },
                "required": ["path"],
                "additionalProperties": false
            }),
            ToolExecutionMode::Interruptible,
            ToolBackend::Local,
        )
        .with_concurrency(ToolConcurrency::Serial)
    }

    fn base_tool_id(&self) -> &'static str {
        "shell_make_visible"
    }

    fn call(
        &self,
        ctx: &ToolCallContext<'_>,
        args: Value,
    ) -> Result<ToolResultContent, LocalToolError> {
        BuiltinBaseTool::call_local(self.base_tool_id(), ctx, args)
    }
}

struct CodexApplyPatchTool;

impl ExtTool for CodexApplyPatchTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition::new(
            "apply_patch",
            "Apply a Codex-style patch inside the workspace. The patch must use *** Begin Patch / *** End Patch sections.",
            json!({
                "type": "object",
                "properties": {
                    "patch": {"type": "string"},
                    "max_output_chars": {"type": "integer", "minimum": 0, "maximum": 1000}
                },
                "required": ["patch"],
                "additionalProperties": false
            }),
            ToolExecutionMode::Immediate,
            ToolBackend::Local,
        )
        .with_concurrency(ToolConcurrency::Serial)
    }

    fn base_tool_id(&self) -> &'static str {
        "apply_patch"
    }

    fn call(
        &self,
        ctx: &ToolCallContext<'_>,
        args: Value,
    ) -> Result<ToolResultContent, LocalToolError> {
        let Value::Object(mut arguments) = args else {
            return Err(LocalToolError::InvalidArguments(
                "tool arguments must be a JSON object".to_string(),
            ));
        };
        arguments
            .entry("format".to_string())
            .or_insert_with(|| Value::String("freeform".to_string()));
        BuiltinBaseTool::call_local(self.base_tool_id(), ctx, Value::Object(arguments))
    }
}

fn normalize_message_for_codex_provider(message: &ChatMessage) -> Option<ChatMessage> {
    let data = message
        .data
        .iter()
        .filter_map(|item| match item {
            ChatMessageItem::Reasoning(reasoning) => reasoning
                .codex_encrypted_content
                .as_ref()
                .filter(|content| !content.is_empty())
                .map(|encrypted_content| {
                    ChatMessageItem::Reasoning(ReasoningItem::codex(
                        reasoning.codex_summary.clone(),
                        Some(encrypted_content.clone()),
                        None,
                    ))
                }),
            ChatMessageItem::Compaction(compaction) => {
                if compaction.generic_summary_text().is_some()
                    || compaction.codex_encrypted_content().is_some()
                {
                    Some(item.clone())
                } else {
                    None
                }
            }
            _ => Some(item.clone()),
        })
        .collect::<Vec<_>>();

    (!data.is_empty()).then(|| ChatMessage {
        message_id: message.message_id.clone(),
        role: message.role.clone(),
        user_name: message.user_name.clone(),
        message_time: message.message_time.clone(),
        token_usage: message.token_usage.clone(),
        data,
    })
}

fn connect_codex_websocket(
    model_config: &ModelConfig,
    auth: &CodexAuthMaterial,
    session_id: &str,
    installation_id: &str,
) -> Result<WebSocket<MaybeTlsStream<TcpStream>>, ProviderError> {
    let websocket_url = build_websocket_url(&model_config.url)?;
    let mut request = websocket_url
        .as_str()
        .into_client_request()
        .map_err(|error| ProviderError::WebSocket(error.to_string()))?;

    request.headers_mut().insert(
        "authorization",
        HeaderValue::from_str(&format!("Bearer {}", auth.access_token))
            .map_err(|error| ProviderError::WebSocket(error.to_string()))?,
    );
    request.headers_mut().insert(
        "chatgpt-account-id",
        HeaderValue::from_str(&auth.account_id)
            .map_err(|error| ProviderError::WebSocket(error.to_string()))?,
    );
    if auth.is_fedramp_account {
        request
            .headers_mut()
            .insert("x-openai-fedramp", HeaderValue::from_static("true"));
    }
    request.headers_mut().insert(
        "openai-beta",
        HeaderValue::from_static(OPENAI_BETA_RESPONSES_WEBSOCKETS),
    );
    request
        .headers_mut()
        .insert("user-agent", HeaderValue::from_static("stellaclaw"));
    request.headers_mut().insert(
        "x-client-request-id",
        HeaderValue::from_str(session_id)
            .map_err(|error| ProviderError::WebSocket(error.to_string()))?,
    );
    request.headers_mut().insert(
        "session_id",
        HeaderValue::from_str(session_id)
            .map_err(|error| ProviderError::WebSocket(error.to_string()))?,
    );
    request.headers_mut().insert(
        "x-codex-window-id",
        HeaderValue::from_str(&format!("{session_id}:0"))
            .map_err(|error| ProviderError::WebSocket(error.to_string()))?,
    );
    request.headers_mut().insert(
        "x-codex-installation-id",
        HeaderValue::from_str(installation_id)
            .map_err(|error| ProviderError::WebSocket(error.to_string()))?,
    );

    let connect_timeout = Duration::from_secs(model_config.conn_timeout_secs());
    let (mut socket, _) =
        if let Some(proxy_stream) = connect_via_https_proxy(&websocket_url, model_config)? {
            set_tcp_timeout(&proxy_stream, connect_timeout)?;
            tungstenite::client_tls_with_config(request, proxy_stream, None, None).map_err(
                |error| match error {
                    tungstenite::HandshakeError::Failure(f) => map_websocket_connect_error(f),
                    tungstenite::HandshakeError::Interrupted(_) => {
                        ProviderError::WebSocket("websocket handshake interrupted".to_string())
                    }
                },
            )?
        } else {
            connect_direct_websocket(request, &websocket_url, connect_timeout)?
        };
    set_socket_timeout(
        &mut socket,
        Duration::from_secs(model_config.request_timeout_secs()),
    )?;
    Ok(socket)
}

fn connect_direct_websocket(
    request: tungstenite::handshake::client::Request,
    websocket_url: &Url,
    timeout: Duration,
) -> Result<
    (
        WebSocket<MaybeTlsStream<TcpStream>>,
        tungstenite::handshake::client::Response,
    ),
    ProviderError,
> {
    let host = websocket_url
        .host_str()
        .ok_or_else(|| ProviderError::WebSocket("websocket url has no host".to_string()))?;
    let port = websocket_url.port_or_known_default().ok_or_else(|| {
        ProviderError::WebSocket(format!(
            "websocket url has no known port for scheme {}",
            websocket_url.scheme()
        ))
    })?;
    let addr = format!("{host}:{port}");
    let addrs = addr
        .to_socket_addrs()
        .map_err(|error| ProviderError::WebSocket(format!("failed to resolve {addr}: {error}")))?;

    let mut last_error = None;
    for socket_addr in addrs {
        match TcpStream::connect_timeout(&socket_addr, timeout) {
            Ok(stream) => {
                set_tcp_timeout(&stream, timeout)?;
                return tungstenite::client_tls_with_config(request, stream, None, None).map_err(
                    |error| match error {
                        tungstenite::HandshakeError::Failure(f) => map_websocket_connect_error(f),
                        tungstenite::HandshakeError::Interrupted(_) => {
                            ProviderError::WebSocket("websocket handshake interrupted".to_string())
                        }
                    },
                );
            }
            Err(error) => {
                last_error = Some(error);
            }
        }
    }

    Err(ProviderError::WebSocket(format!(
        "failed to connect to websocket {addr}: {}",
        last_error
            .map(|error| error.to_string())
            .unwrap_or_else(|| "no socket addresses resolved".to_string())
    )))
}

fn send_response_create(
    socket: &mut WebSocket<MaybeTlsStream<TcpStream>>,
    payload: Map<String, Value>,
    model_config: &ModelConfig,
    on_stream: &mut dyn FnMut(ProviderStreamEvent),
) -> Result<Value, ProviderError> {
    let mut request = Map::new();
    request.insert(
        "type".to_string(),
        Value::String("response.create".to_string()),
    );
    request.extend(payload);

    let body = Value::Object(request).to_string();
    ensure_request_payload_size(model_config, "codex_subscription websocket", body.len())?;

    socket
        .send(Message::Text(body.into()))
        .map_err(|error| ProviderError::WebSocket(error.to_string()))?;

    let mut accumulator = StreamAccumulator::default();
    let progress_timeout = Duration::from_secs(model_config.request_timeout_secs());
    let mut last_progress = Instant::now();

    loop {
        let message = socket
            .read()
            .map_err(|error| ProviderError::WebSocket(error.to_string()))?;

        match message {
            Message::Text(text) => {
                let value =
                    serde_json::from_str::<Value>(&text).map_err(ProviderError::DecodeJson)?;
                let event_type = value.get("type").and_then(Value::as_str);
                if event_type.is_some_and(is_codex_response_progress_event) {
                    last_progress = Instant::now();
                }
                match event_type {
                    Some("response.completed") => {
                        let mut response = value.get("response").cloned().ok_or_else(|| {
                            ProviderError::InvalidResponse(
                                "codex websocket completed without response".to_string(),
                            )
                        })?;
                        let missing_tool_call_events =
                            accumulator.missing_tool_call_stream_events(&response);
                        for event in missing_tool_call_events {
                            on_stream(event);
                        }
                        merge_streamed_response_output(&mut response, accumulator);
                        if let Some(error) = provider_error_message(&response) {
                            if let Some(kind) = provider_error_kind(&response) {
                                return Err(ProviderError::ProviderFailure {
                                    kind,
                                    message: error,
                                    body: response.to_string(),
                                });
                            }
                            return Err(ProviderError::InvalidResponse(error));
                        }
                        return Ok(response);
                    }
                    Some("response.failed") | Some("error") => {
                        if let Some(kind) = provider_error_kind(&value) {
                            return Err(ProviderError::ProviderFailure {
                                kind,
                                message: provider_error_message(&value)
                                    .unwrap_or_else(|| "provider returned an error".to_string()),
                                body: value.to_string(),
                            });
                        }
                        if let Some(status) = value
                            .get("status")
                            .or_else(|| value.get("status_code"))
                            .and_then(Value::as_u64)
                            .and_then(|status| u16::try_from(status).ok())
                        {
                            let error =
                                provider_error_message(&value).unwrap_or_else(|| value.to_string());
                            return Err(ProviderError::HttpStatus {
                                url: "codex websocket stream".to_string(),
                                status,
                                body: error,
                            });
                        }
                        let error =
                            provider_error_message(&value).unwrap_or_else(|| value.to_string());
                        return Err(ProviderError::WebSocket(error));
                    }
                    Some("response.output_item.done") => {
                        let completed_tool_call_event =
                            accumulator.completed_tool_call_stream_event(&value);
                        accumulator.record_output_item_done(&value);
                        if let Some(event) = completed_tool_call_event {
                            on_stream(event);
                        }
                    }
                    Some("response.output_item.added") => {
                        accumulator.record_output_item_added(&value);
                    }
                    Some("response.output_text.delta") => {
                        if let Some(delta) = value.get("delta").and_then(Value::as_str) {
                            on_stream(ProviderStreamEvent::OutputTextDelta {
                                item_id: accumulator.active_output_item_id.clone(),
                                delta: delta.to_string(),
                            });
                        }
                    }
                    Some(
                        "response.custom_tool_call_input.delta"
                        | "response.function_call_arguments.delta",
                    ) => {
                        if let Some(event) =
                            provider_tool_call_input_delta_event(&value, &accumulator)
                        {
                            accumulator.record_streamed_tool_input(&event);
                            on_stream(event);
                        }
                    }
                    Some("response.reasoning_summary_text.delta") => {
                        if let (Some(delta), Some(summary_index)) = (
                            value.get("delta").and_then(Value::as_str),
                            value.get("summary_index").and_then(Value::as_i64),
                        ) {
                            on_stream(ProviderStreamEvent::ReasoningSummaryDelta {
                                item_id: accumulator.active_reasoning_item_id.clone(),
                                delta: delta.to_string(),
                                summary_index,
                            });
                        }
                    }
                    Some("response.reasoning_summary_part.added") => {
                        if let Some(summary_index) =
                            value.get("summary_index").and_then(Value::as_i64)
                        {
                            on_stream(ProviderStreamEvent::ReasoningSummaryPartAdded {
                                item_id: accumulator.active_reasoning_item_id.clone(),
                                summary_index,
                            });
                        }
                    }
                    _ => {}
                }
            }
            Message::Ping(payload) => socket
                .send(Message::Pong(payload))
                .map_err(|error| ProviderError::WebSocket(error.to_string()))?,
            Message::Close(frame) => {
                return Err(ProviderError::WebSocket(format!(
                    "codex websocket closed before response.completed: {}",
                    frame
                        .map(|value| value.reason.to_string())
                        .unwrap_or_else(|| "connection closed".to_string())
                )));
            }
            _ => {}
        }
        if last_progress.elapsed() >= progress_timeout {
            return Err(ProviderError::WebSocket(format!(
                "codex websocket response made no progress for {}s",
                progress_timeout.as_secs()
            )));
        }
    }
}

fn is_codex_response_progress_event(event_type: &str) -> bool {
    event_type.starts_with("response.")
        && (event_type.contains(".delta")
            || event_type.contains(".added")
            || event_type.contains(".done"))
}

fn provider_tool_call_input_delta_event(
    event: &Value,
    accumulator: &StreamAccumulator,
) -> Option<ProviderStreamEvent> {
    let delta = event.get("delta").and_then(Value::as_str)?.to_string();
    let call_id = event
        .get("call_id")
        .and_then(Value::as_str)
        .map(str::to_string);
    let item_id = event
        .get("item_id")
        .and_then(Value::as_str)
        .map(str::to_string)
        .or_else(|| call_id.clone())?;
    let tool_name = event
        .get("name")
        .and_then(Value::as_str)
        .map(str::to_string)
        .or_else(|| accumulator.tool_name_for_item(&item_id, call_id.as_deref()));
    Some(ProviderStreamEvent::ToolCallInputDelta {
        item_id,
        call_id,
        tool_name,
        delta,
    })
}

fn codex_reasoning_payload(model_config: &ModelConfig) -> Option<Value> {
    let mut payload = model_config.reasoning.clone()?;
    let object = payload.as_object_mut()?;
    object.remove("max_tokens");
    object.remove("exclude");
    object.remove("enabled");
    object.remove("fast");
    object.remove("fast_mode");
    object.remove("service_tier");
    if object.is_empty() {
        return None;
    }
    Some(payload)
}

fn codex_service_tier_payload(model_config: &ModelConfig) -> Option<String> {
    if env_flag_enabled("STELLACLAW_CODEX_FAST_MODE")
        .or_else(|| env_flag_enabled("CODEX_FAST_MODE"))
        == Some(true)
    {
        return Some("priority".to_string());
    }

    let object = model_config.reasoning.as_ref()?.as_object()?;
    if value_truthy(object.get("fast")).unwrap_or(false)
        || value_truthy(object.get("fast_mode")).unwrap_or(false)
    {
        return Some("priority".to_string());
    }

    let service_tier = object.get("service_tier")?.as_str()?.trim();
    match service_tier {
        "" | "auto" | "default" | "standard" => None,
        "fast" | "priority" => Some("priority".to_string()),
        other => Some(other.to_string()),
    }
}

fn env_flag_enabled(name: &str) -> Option<bool> {
    let value = std::env::var(name).ok()?;
    value_truthy_str(&value)
}

fn value_truthy(value: Option<&Value>) -> Option<bool> {
    match value? {
        Value::Bool(value) => Some(*value),
        Value::Number(value) => value.as_u64().map(|value| value != 0),
        Value::String(value) => value_truthy_str(value),
        _ => None,
    }
}

fn value_truthy_str(value: &str) -> Option<bool> {
    match value.trim().to_ascii_lowercase().as_str() {
        "1" | "true" | "yes" | "y" | "on" | "fast" | "priority" => Some(true),
        "0" | "false" | "no" | "n" | "off" | "default" | "standard" => Some(false),
        _ => None,
    }
}

fn build_websocket_url(http_url: &str) -> Result<Url, ProviderError> {
    let mut url = Url::parse(http_url)
        .map_err(|error| ProviderError::WebSocket(format!("invalid websocket url: {error}")))?;
    match url.scheme() {
        "https" => url
            .set_scheme("wss")
            .map_err(|_| ProviderError::WebSocket("failed to convert https to wss".to_string()))?,
        "http" => url
            .set_scheme("ws")
            .map_err(|_| ProviderError::WebSocket("failed to convert http to ws".to_string()))?,
        "wss" | "ws" => {}
        other => {
            return Err(ProviderError::WebSocket(format!(
                "unsupported codex websocket scheme {other}"
            )));
        }
    }
    Ok(url)
}

fn fetch_codex_models(
    model_config: &ModelConfig,
    auth: &CodexAuthMaterial,
    installation_id: &str,
) -> Result<CachedCodexModels, ProviderError> {
    let endpoint = build_codex_models_url(model_config)?;
    let client = Client::builder()
        .connect_timeout(Duration::from_secs(model_config.conn_timeout_secs()))
        .timeout(Duration::from_secs(model_config.request_timeout_secs()))
        .build()
        .map_err(ProviderError::BuildHttpClient)?;
    let mut request = client
        .get(endpoint.clone())
        .header("authorization", format!("Bearer {}", auth.access_token))
        .header("chatgpt-account-id", auth.account_id.clone())
        .header("openai-beta", OPENAI_BETA_RESPONSES_WEBSOCKETS)
        .header("user-agent", "stellaclaw")
        .header("x-client-request-id", nonce("models"))
        .header("x-codex-installation-id", installation_id)
        .header("session_id", nonce("models-session"));
    if auth.is_fedramp_account {
        request = request.header("x-openai-fedramp", "true");
    }

    let response = request.send().map_err(ProviderError::request)?;
    let status = response.status();
    let etag = response
        .headers()
        .get(reqwest::header::ETAG)
        .and_then(|value| value.to_str().ok())
        .map(str::to_string);
    let body = response.text().map_err(ProviderError::DecodeResponse)?;
    if !status.is_success() {
        return Err(ProviderError::HttpStatus {
            url: endpoint.to_string(),
            status: status.as_u16(),
            body,
        });
    }

    let response =
        serde_json::from_str::<CodexModelsResponse>(&body).map_err(ProviderError::DecodeJson)?;
    Ok(CachedCodexModels {
        version: 1,
        fetched_at_ms: now_millis(),
        etag,
        models: response.models,
    })
}

fn build_codex_models_url(model_config: &ModelConfig) -> Result<Url, ProviderError> {
    let mut url = if let Ok(override_url) = std::env::var(CODEX_MODELS_URL_OVERRIDE_ENV) {
        Url::parse(&override_url).map_err(|error| {
            ProviderError::InvalidResponse(format!(
                "invalid {CODEX_MODELS_URL_OVERRIDE_ENV}: {error}"
            ))
        })?
    } else {
        let mut url = Url::parse(&model_config.url).map_err(|error| {
            ProviderError::InvalidResponse(format!("invalid codex provider url: {error}"))
        })?;
        let path = url.path().trim_end_matches('/').to_string();
        if let Some(prefix) = path.strip_suffix("/responses") {
            url.set_path(&format!("{prefix}/models"));
        } else if path.ends_with("/models") {
            url.set_path(&path);
        } else {
            url.set_path("/backend-api/codex/models");
        }
        url
    };
    url.query_pairs_mut()
        .append_pair("client_version", env!("CARGO_PKG_VERSION"));
    Ok(url)
}

fn build_codex_compact_url(model_config: &ModelConfig) -> Result<Url, ProviderError> {
    let mut url = Url::parse(&model_config.url).map_err(|error| {
        ProviderError::InvalidResponse(format!("invalid codex provider url: {error}"))
    })?;
    let path = url.path().trim_end_matches('/').to_string();
    if path.ends_with("/responses") {
        url.set_path(&format!("{path}/compact"));
    } else if path.ends_with("/responses/compact") {
        url.set_path(&path);
    } else {
        url.set_path("/backend-api/codex/responses/compact");
    }
    Ok(url)
}

fn compact_response_value_to_chat_messages(
    value: &Value,
) -> Result<Vec<ChatMessage>, ProviderError> {
    let output = value
        .get("output")
        .and_then(Value::as_array)
        .ok_or_else(|| {
            ProviderError::InvalidResponse("compact response missing output array".to_string())
        })?;
    let mut messages = Vec::new();
    for item in output {
        match item.get("type").and_then(Value::as_str) {
            Some("message") => {
                if let Some(message) = compact_message_item_to_chat_message(item) {
                    messages.push(message);
                }
            }
            Some("compaction") | Some("compaction_summary") => {
                if let Some(encrypted_content) =
                    item.get("encrypted_content").and_then(Value::as_str)
                {
                    messages.push(ChatMessage::new(
                        ChatRole::Compaction,
                        vec![ChatMessageItem::Compaction(
                            CompactionItem::provider_builtin(
                                ProviderType::CodexSubscription,
                                json!({ "encrypted_content": encrypted_content }),
                            ),
                        )],
                    ));
                }
            }
            Some("context_compaction") => {
                if let Some(encrypted_content) =
                    item.get("encrypted_content").and_then(Value::as_str)
                {
                    messages.push(ChatMessage::new(
                        ChatRole::Compaction,
                        vec![ChatMessageItem::Compaction(
                            CompactionItem::provider_builtin(
                                ProviderType::CodexSubscription,
                                json!({ "encrypted_content": encrypted_content }),
                            ),
                        )],
                    ));
                }
            }
            _ => {}
        }
    }
    Ok(messages)
}

fn compact_message_item_to_chat_message(item: &Value) -> Option<ChatMessage> {
    let role = match item.get("role").and_then(Value::as_str)? {
        "user" => ChatRole::User,
        "assistant" => ChatRole::Assistant,
        _ => return None,
    };
    let mut data = Vec::new();
    for content in item.get("content").and_then(Value::as_array)? {
        match content.get("type").and_then(Value::as_str) {
            Some("input_text") | Some("output_text") | Some("text") => {
                if let Some(text) = content.get("text").and_then(Value::as_str) {
                    data.push(ChatMessageItem::Context(ContextItem {
                        text: text.to_string(),
                    }));
                }
            }
            _ => {}
        }
    }
    (!data.is_empty()).then(|| ChatMessage::new(role, data))
}

fn read_cached_codex_models() -> Result<Option<CachedCodexModels>, ProviderError> {
    let cache_file = codex_models_cache_file();
    if !cache_file.exists() {
        return Ok(None);
    }
    let bytes = fs::read(&cache_file).map_err(|error| {
        ProviderError::InvalidResponse(format!("failed to read codex models cache: {error}"))
    })?;
    let cached =
        serde_json::from_slice::<CachedCodexModels>(&bytes).map_err(ProviderError::DecodeJson)?;
    Ok(Some(cached))
}

fn write_cached_codex_models(cached: &CachedCodexModels) -> Result<(), ProviderError> {
    let cache_file = codex_models_cache_file();
    let parent = cache_file.parent().ok_or_else(|| {
        ProviderError::InvalidResponse("codex models cache path has no parent".to_string())
    })?;
    fs::create_dir_all(parent).map_err(|error| {
        ProviderError::InvalidResponse(format!("failed to create codex cache dir: {error}"))
    })?;
    let bytes = serde_json::to_vec_pretty(cached).map_err(|error| {
        ProviderError::InvalidResponse(format!("failed to serialize codex models cache: {error}"))
    })?;
    fs::write(&cache_file, bytes).map_err(|error| {
        ProviderError::InvalidResponse(format!("failed to write codex models cache: {error}"))
    })?;
    Ok(())
}

fn codex_models_cache_file() -> PathBuf {
    codex_cache_root().join("models.json")
}

fn codex_cache_root() -> PathBuf {
    std::env::var_os("STELLACLAW_CODEX_CACHE_ROOT")
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var_os("STELLACLAW_DATA_ROOT")
                .map(|root| PathBuf::from(root).join(".stellaclaw").join("codex"))
        })
        .unwrap_or_else(|| {
            std::env::current_dir()
                .unwrap_or_else(|_| PathBuf::from("."))
                .join(".stellaclaw")
                .join("codex")
        })
}

fn now_millis() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
}

/// Detect `HTTPS_PROXY` / `https_proxy` and establish an HTTP CONNECT tunnel to the
/// WebSocket target host.  Returns `Ok(None)` when no proxy is configured so the
/// caller can fall back to a direct connection.
fn connect_via_https_proxy(
    target_url: &Url,
    model_config: &ModelConfig,
) -> Result<Option<TcpStream>, ProviderError> {
    let proxy_url = std::env::var("HTTPS_PROXY")
        .or_else(|_| std::env::var("https_proxy"))
        .or_else(|_| std::env::var("ALL_PROXY"))
        .or_else(|_| std::env::var("all_proxy"));
    let proxy_url = match proxy_url {
        Ok(url) if !url.is_empty() => url,
        _ => return Ok(None),
    };

    let target_host = target_url
        .host_str()
        .ok_or_else(|| ProviderError::WebSocket("websocket url has no host".to_string()))?;
    let target_port = target_url.port_or_known_default().unwrap_or(443);

    // Check NO_PROXY
    if is_no_proxy(target_host) {
        return Ok(None);
    }

    // Parse proxy URL — supports http://host:port and socks5://host:port (treat as HTTP CONNECT)
    let proxy_parsed = Url::parse(&proxy_url).map_err(|error| {
        ProviderError::WebSocket(format!("invalid proxy URL {proxy_url}: {error}"))
    })?;
    let proxy_host = proxy_parsed
        .host_str()
        .ok_or_else(|| ProviderError::WebSocket(format!("proxy URL has no host: {proxy_url}")))?;
    let proxy_port = proxy_parsed.port().unwrap_or(match proxy_parsed.scheme() {
        "socks5" | "socks5h" => 1080,
        _ => 8080,
    });

    let conn_timeout = Duration::from_secs(model_config.conn_timeout_secs());
    let proxy_addr = format!("{proxy_host}:{proxy_port}");
    let addrs: Vec<std::net::SocketAddr> = proxy_addr
        .to_socket_addrs()
        .map_err(|error| {
            ProviderError::WebSocket(format!("failed to resolve proxy {proxy_addr}: {error}"))
        })?
        .collect();

    let mut stream = None;
    for addr in &addrs {
        match TcpStream::connect_timeout(addr, conn_timeout) {
            Ok(s) => {
                stream = Some(s);
                break;
            }
            Err(_) => continue,
        }
    }
    let mut stream = stream.ok_or_else(|| {
        ProviderError::WebSocket(format!("failed to connect to proxy {proxy_addr}"))
    })?;
    stream
        .set_nodelay(true)
        .map_err(|error| ProviderError::WebSocket(error.to_string()))?;

    // Send HTTP CONNECT request
    let connect_request = format!(
        "CONNECT {target_host}:{target_port} HTTP/1.1\r\nHost: {target_host}:{target_port}\r\n\r\n"
    );
    use std::io::Write;
    stream
        .write_all(connect_request.as_bytes())
        .map_err(|error| {
            ProviderError::WebSocket(format!("failed to send CONNECT to proxy: {error}"))
        })?;

    // Read the HTTP response status line
    use std::io::{BufRead, BufReader};
    stream
        .set_read_timeout(Some(conn_timeout))
        .map_err(|error| ProviderError::WebSocket(error.to_string()))?;
    let mut reader = BufReader::new(stream);
    let mut status_line = String::new();
    reader.read_line(&mut status_line).map_err(|error| {
        ProviderError::WebSocket(format!("failed to read proxy CONNECT response: {error}"))
    })?;
    if !status_line.contains("200") {
        return Err(ProviderError::WebSocket(format!(
            "proxy CONNECT failed: {status_line}"
        )));
    }
    // Consume remaining headers
    loop {
        let mut line = String::new();
        reader.read_line(&mut line).map_err(|error| {
            ProviderError::WebSocket(format!(
                "failed to read proxy CONNECT response headers: {error}"
            ))
        })?;
        if line.trim().is_empty() {
            break;
        }
    }

    let stream = reader.into_inner();
    // Clear read timeout — the caller will set appropriate timeouts
    stream
        .set_read_timeout(None)
        .map_err(|error| ProviderError::WebSocket(error.to_string()))?;
    Ok(Some(stream))
}

fn is_no_proxy(host: &str) -> bool {
    let no_proxy = std::env::var("NO_PROXY")
        .or_else(|_| std::env::var("no_proxy"))
        .unwrap_or_default();
    if no_proxy.trim() == "*" {
        return true;
    }
    for entry in no_proxy.split(',') {
        let entry = entry.trim();
        if entry.is_empty() {
            continue;
        }
        if host == entry || host.ends_with(&format!(".{entry}")) {
            return true;
        }
    }
    false
}

fn set_socket_timeout(
    socket: &mut WebSocket<MaybeTlsStream<TcpStream>>,
    timeout: Duration,
) -> Result<(), ProviderError> {
    match socket.get_mut() {
        MaybeTlsStream::Plain(stream) => set_tcp_timeout(stream, timeout),
        MaybeTlsStream::Rustls(stream) => set_tcp_timeout(&stream.sock, timeout),
        _ => Ok(()),
    }
}

fn set_tcp_timeout(stream: &TcpStream, timeout: Duration) -> Result<(), ProviderError> {
    stream
        .set_read_timeout(Some(timeout))
        .map_err(|error| ProviderError::WebSocket(error.to_string()))?;
    stream
        .set_write_timeout(Some(timeout))
        .map_err(|error| ProviderError::WebSocket(error.to_string()))?;
    Ok(())
}

impl CodexSubscriptionAuthManager {
    fn resolve(&self, model_config: &ModelConfig) -> Result<CodexAuthMaterial, ProviderError> {
        if let Some(cached) = self.cached.lock().expect("mutex poisoned").clone() {
            if !cached.should_refresh() {
                return Ok(cached);
            }
        }

        let loaded = load_codex_auth_material(model_config)?;
        let material = if loaded.should_refresh() && loaded.refresh_token.is_some() {
            self.refresh_material(model_config, &loaded)?
        } else {
            loaded
        };

        *self.cached.lock().expect("mutex poisoned") = Some(material.clone());
        Ok(material)
    }

    fn refresh(
        &self,
        model_config: &ModelConfig,
        previous: &CodexAuthMaterial,
    ) -> Result<CodexAuthMaterial, ProviderError> {
        let refreshed = self.refresh_material(model_config, previous)?;
        *self.cached.lock().expect("mutex poisoned") = Some(refreshed.clone());
        Ok(refreshed)
    }

    fn refresh_material(
        &self,
        model_config: &ModelConfig,
        previous: &CodexAuthMaterial,
    ) -> Result<CodexAuthMaterial, ProviderError> {
        let refresh_token = previous.refresh_token.clone().ok_or_else(|| {
            ProviderError::InvalidResponse(
                "codex subscription token refresh requested but no refresh token is available"
                    .to_string(),
            )
        })?;
        let refreshed = request_chatgpt_token_refresh(model_config, refresh_token)?;
        let access_token = refreshed.access_token.clone().ok_or_else(|| {
            ProviderError::InvalidResponse(
                "codex subscription token refresh response did not include access_token"
                    .to_string(),
            )
        })?;
        let account_id = chatgpt_account_id_from_tokens(
            &access_token,
            refreshed.id_token.as_deref(),
            Some(&previous.account_id),
        )
        .ok_or_else(|| {
            ProviderError::InvalidResponse(
                "codex subscription refreshed token does not include chatgpt_account_id"
                    .to_string(),
            )
        })?;

        if account_id != previous.account_id {
            return Err(ProviderError::InvalidResponse(format!(
                "codex subscription refreshed token account mismatch: expected {}, got {}",
                previous.account_id, account_id
            )));
        }

        let mut material = previous.clone();
        material.access_token = access_token;
        material.refresh_token = refreshed.refresh_token.or(material.refresh_token);
        material.expires_at = jwt_expiration(&material.access_token);
        material.is_fedramp_account =
            chatgpt_fedramp_from_tokens(&material.access_token, refreshed.id_token.as_deref())
                .unwrap_or(material.is_fedramp_account);

        if let CodexAuthSource::AuthJson(path) = &material.source {
            persist_refreshed_auth_json(path, &material, refreshed.id_token.as_deref())?;
        }

        Ok(material)
    }
}

impl CodexAuthMaterial {
    fn should_refresh(&self) -> bool {
        const REFRESH_LEAD_SECS: i64 = 60;
        self.expires_at.is_some_and(|expires_at| {
            expires_at <= now_unix_secs().saturating_add(REFRESH_LEAD_SECS)
        })
    }
}

fn load_codex_auth_material(
    model_config: &ModelConfig,
) -> Result<CodexAuthMaterial, ProviderError> {
    if let Some(material) = load_auth_json_material()? {
        return Ok(material);
    }

    load_env_auth_material(model_config)
}

fn load_env_auth_material(model_config: &ModelConfig) -> Result<CodexAuthMaterial, ProviderError> {
    let access_token = std::env::var(&model_config.api_key_env)
        .or_else(|_| std::env::var("CHATGPT_ACCESS_TOKEN"))
        .map_err(|_| ProviderError::MissingApiKeyEnv(model_config.api_key_env.clone()))?;
    let refresh_token = std::env::var("CHATGPT_REFRESH_TOKEN").ok();
    let account_id = chatgpt_account_id_from_tokens(&access_token, None, None)
        .or_else(|| std::env::var("CHATGPT_ACCOUNT_ID").ok())
        .ok_or_else(|| {
            ProviderError::InvalidResponse(
                "codex subscription account id is unavailable; set CHATGPT_ACCOUNT_ID, use a ChatGPT token containing chatgpt_account_id, or provide Codex auth.json".to_string(),
            )
        })?;

    Ok(CodexAuthMaterial {
        expires_at: jwt_expiration(&access_token),
        is_fedramp_account: chatgpt_fedramp_from_tokens(&access_token, None).unwrap_or(false),
        access_token,
        refresh_token,
        account_id,
        source: CodexAuthSource::Env,
    })
}

fn load_auth_json_material() -> Result<Option<CodexAuthMaterial>, ProviderError> {
    for path in auth_json_candidate_paths() {
        let Ok(text) = fs::read_to_string(&path) else {
            continue;
        };
        let value = serde_json::from_str::<Value>(&text).map_err(ProviderError::DecodeJson)?;
        let Some(tokens) = value.get("tokens").and_then(Value::as_object) else {
            continue;
        };
        let Some(access_token) = tokens.get("access_token").and_then(Value::as_str) else {
            continue;
        };
        if access_token.trim().is_empty() {
            continue;
        }
        let id_token = tokens.get("id_token").and_then(Value::as_str);
        let account_id = tokens
            .get("account_id")
            .and_then(Value::as_str)
            .map(str::to_string)
            .or_else(|| chatgpt_account_id_from_tokens(access_token, id_token, None));
        let Some(account_id) = account_id else {
            continue;
        };

        let refresh_token = tokens
            .get("refresh_token")
            .and_then(Value::as_str)
            .filter(|token| !token.trim().is_empty())
            .map(str::to_string);
        return Ok(Some(CodexAuthMaterial {
            access_token: access_token.to_string(),
            refresh_token,
            account_id,
            is_fedramp_account: chatgpt_fedramp_from_tokens(access_token, id_token)
                .unwrap_or(false),
            expires_at: jwt_expiration(access_token),
            source: CodexAuthSource::AuthJson(path),
        }));
    }

    Ok(None)
}

fn auth_json_candidate_paths() -> Vec<PathBuf> {
    let mut paths = Vec::new();
    for env in ["CODEX_AUTH_JSON", "CHATGPT_AUTH_JSON"] {
        if let Ok(path) = std::env::var(env) {
            paths.push(PathBuf::from(path));
        }
    }
    if let Ok(home) = std::env::var("CODEX_HOME") {
        paths.push(PathBuf::from(home).join("auth.json"));
    }
    if let Ok(home) = std::env::var("HOME") {
        paths.push(PathBuf::from(home).join(".codex").join("auth.json"));
    }
    paths
}

fn request_chatgpt_token_refresh(
    model_config: &ModelConfig,
    refresh_token: String,
) -> Result<RefreshTokenResponse, ProviderError> {
    let endpoint = std::env::var(CHATGPT_REFRESH_TOKEN_URL_OVERRIDE_ENV)
        .unwrap_or_else(|_| CHATGPT_REFRESH_TOKEN_URL.to_string());
    let client = Client::builder()
        .connect_timeout(Duration::from_secs(model_config.conn_timeout_secs()))
        .timeout(Duration::from_secs(model_config.request_timeout_secs()))
        .build()
        .map_err(ProviderError::BuildHttpClient)?;
    let response = client
        .post(&endpoint)
        .header("Content-Type", "application/json")
        .json(&RefreshTokenRequest {
            client_id: CODEX_CLIENT_ID,
            grant_type: "refresh_token",
            refresh_token,
        })
        .send()
        .map_err(ProviderError::request)?;
    let status = response.status();
    let body = response.text().map_err(ProviderError::DecodeResponse)?;
    if !status.is_success() {
        return Err(ProviderError::HttpStatus {
            url: endpoint,
            status: status.as_u16(),
            body,
        });
    }

    serde_json::from_str::<RefreshTokenResponse>(&body).map_err(ProviderError::DecodeJson)
}

fn persist_refreshed_auth_json(
    path: &PathBuf,
    material: &CodexAuthMaterial,
    id_token: Option<&str>,
) -> Result<(), ProviderError> {
    let text = fs::read_to_string(path).map_err(|error| {
        ProviderError::InvalidResponse(format!("failed to read auth.json: {error}"))
    })?;
    let mut value = serde_json::from_str::<Value>(&text).map_err(ProviderError::DecodeJson)?;
    let object = value.as_object_mut().ok_or_else(|| {
        ProviderError::InvalidResponse("codex auth.json root is not an object".to_string())
    })?;
    let tokens = object
        .entry("tokens")
        .or_insert_with(|| Value::Object(Map::new()))
        .as_object_mut()
        .ok_or_else(|| {
            ProviderError::InvalidResponse("codex auth.json tokens is not an object".to_string())
        })?;

    tokens.insert(
        "access_token".to_string(),
        Value::String(material.access_token.clone()),
    );
    if let Some(refresh_token) = &material.refresh_token {
        tokens.insert(
            "refresh_token".to_string(),
            Value::String(refresh_token.clone()),
        );
    }
    tokens.insert(
        "account_id".to_string(),
        Value::String(material.account_id.clone()),
    );
    if let Some(id_token) = id_token {
        tokens.insert("id_token".to_string(), Value::String(id_token.to_string()));
    }
    object.insert("last_refresh".to_string(), Value::String(rfc3339_now_utc()));

    let rendered = serde_json::to_string_pretty(&value)
        .map_err(|error| ProviderError::InvalidResponse(error.to_string()))?;
    fs::write(path, rendered).map_err(|error| {
        ProviderError::InvalidResponse(format!("failed to write auth.json: {error}"))
    })?;
    Ok(())
}

fn chatgpt_account_id_from_tokens(
    access_token: &str,
    id_token: Option<&str>,
    fallback: Option<&str>,
) -> Option<String> {
    account_id_from_access_token(access_token)
        .or_else(|| id_token.and_then(account_id_from_access_token))
        .or_else(|| fallback.map(str::to_string))
}

fn chatgpt_fedramp_from_tokens(access_token: &str, id_token: Option<&str>) -> Option<bool> {
    bool_claim_from_jwt(
        access_token,
        &["https://api.openai.com/auth", "chatgpt_account_is_fedramp"],
    )
    .or_else(|| {
        id_token.and_then(|token| {
            bool_claim_from_jwt(
                token,
                &["https://api.openai.com/auth", "chatgpt_account_is_fedramp"],
            )
        })
    })
}

fn jwt_expiration(token: &str) -> Option<i64> {
    i64_claim_from_jwt(token, &["exp"])
}

fn bool_claim_from_jwt(token: &str, path: &[&str]) -> Option<bool> {
    jwt_payload_value(token)
        .ok()
        .and_then(|value| value_at_path(&value, path).and_then(Value::as_bool))
}

fn i64_claim_from_jwt(token: &str, path: &[&str]) -> Option<i64> {
    jwt_payload_value(token).ok().and_then(|value| {
        value_at_path(&value, path).and_then(|value| {
            value
                .as_i64()
                .or_else(|| value.as_u64().and_then(|value| i64::try_from(value).ok()))
        })
    })
}

fn jwt_payload_value(token: &str) -> Result<Value, ProviderError> {
    let mut parts = token.split('.');
    let (_, payload, _) = (parts.next(), parts.next(), parts.next());
    let payload = payload
        .ok_or_else(|| ProviderError::InvalidResponse("invalid JWT token format".to_string()))?;
    let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(payload)
        .map_err(|error| ProviderError::InvalidResponse(error.to_string()))?;
    serde_json::from_slice::<Value>(&bytes).map_err(ProviderError::DecodeJson)
}

fn value_at_path<'a>(value: &'a Value, path: &[&str]) -> Option<&'a Value> {
    let mut current = value;
    for key in path {
        current = current.get(*key)?;
    }
    Some(current)
}

fn now_unix_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| i64::try_from(duration.as_secs()).unwrap_or(i64::MAX))
        .unwrap_or_default()
}

fn rfc3339_now_utc() -> String {
    time::OffsetDateTime::now_utc()
        .format(&time::format_description::well_known::Rfc3339)
        .unwrap_or_else(|_| now_unix_secs().to_string())
}

fn map_websocket_connect_error(error: tungstenite::Error) -> ProviderError {
    match error {
        tungstenite::Error::Http(response) => {
            let status = response.status().as_u16();
            let body = response
                .body()
                .as_ref()
                .and_then(|body| String::from_utf8(body.clone()).ok())
                .unwrap_or_default();
            ProviderError::HttpStatus {
                url: "websocket handshake".to_string(),
                status,
                body,
            }
        }
        error => ProviderError::WebSocket(error.to_string()),
    }
}

fn is_websocket_transport_error(response: &Result<Value, ProviderError>) -> bool {
    matches!(response, Err(ProviderError::WebSocket(message)) if websocket_message_is_transport_error(message))
}

fn websocket_message_is_transport_error(message: &str) -> bool {
    let message = message.to_ascii_lowercase();
    message.contains("broken pipe")
        || message.contains("connection reset")
        || message.contains("connection aborted")
        || message.contains("connection refused")
        || message.contains("connection closed")
        || message.contains("closed before response.completed")
        || message.contains("closing handshake")
        || message.contains("reset without closing handshake")
        || message.contains("io error")
        || message.contains("transport error")
}

fn is_unauthorized(error: &ProviderError) -> bool {
    matches!(
        error,
        ProviderError::HttpStatus { status, .. }
            if *status == StatusCode::UNAUTHORIZED.as_u16()
    )
}

impl StreamAccumulator {
    fn record_output_item_done(&mut self, event: &Value) {
        if let Some(item) = event.get("item") {
            let item = item.clone();
            let completed_id = item.get("id").and_then(Value::as_str).map(str::to_string);
            if item.get("type").and_then(Value::as_str) == Some("reasoning") {
                if let Some(item_id) = item.get("id").and_then(Value::as_str) {
                    self.activate_reasoning_item(item_id);
                }
                self.active_reasoning_item_id = None;
            }
            self.push_unique_item(item);
            if self.active_output_item_id == completed_id {
                self.active_output_item_id = None;
            }
        }
    }

    fn record_output_item_added(&mut self, event: &Value) {
        let Some(item) = event.get("item") else {
            return;
        };
        if let Some(item_id) = item.get("id").and_then(Value::as_str) {
            self.active_output_item_id = Some(item_id.to_string());
            if item.get("type").and_then(Value::as_str) == Some("reasoning") {
                self.activate_reasoning_item(item_id);
            }
        }
        self.push_unique_item(item.clone());
    }

    fn tool_name_for_item(&self, item_id: &str, call_id: Option<&str>) -> Option<String> {
        self.output_items.iter().find_map(|item| {
            let item_matches = item
                .get("id")
                .and_then(Value::as_str)
                .is_some_and(|id| id == item_id)
                || call_id.is_some_and(|call_id| {
                    item.get("call_id")
                        .and_then(Value::as_str)
                        .is_some_and(|id| id == call_id)
                });
            if item_matches {
                item.get("name").and_then(Value::as_str).map(str::to_string)
            } else {
                None
            }
        })
    }

    fn record_streamed_tool_input(&mut self, event: &ProviderStreamEvent) {
        let ProviderStreamEvent::ToolCallInputDelta {
            item_id, call_id, ..
        } = event
        else {
            return;
        };
        for key in tool_input_stream_keys(item_id, call_id.as_deref()) {
            if !self
                .streamed_tool_inputs
                .iter()
                .any(|existing| existing == &key)
            {
                self.streamed_tool_inputs.push(key);
            }
        }
    }

    fn completed_tool_call_stream_event(&mut self, event: &Value) -> Option<ProviderStreamEvent> {
        let item = event.get("item")?;
        self.tool_call_stream_event_from_item(item)
    }

    fn missing_tool_call_stream_events(&mut self, response: &Value) -> Vec<ProviderStreamEvent> {
        let response_items = response
            .get("output")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        let accumulated_items = self.output_items.clone();
        response_items
            .iter()
            .chain(accumulated_items.iter())
            .filter_map(|item| self.tool_call_stream_event_from_item(item))
            .collect()
    }

    fn tool_call_stream_event_from_item(&mut self, item: &Value) -> Option<ProviderStreamEvent> {
        if item.get("type").and_then(Value::as_str) != Some("function_call") {
            return None;
        }
        let call_id = item
            .get("call_id")
            .and_then(Value::as_str)
            .map(str::to_string);
        let item_id = item
            .get("id")
            .and_then(Value::as_str)
            .map(str::to_string)
            .or_else(|| call_id.clone())?;
        let keys = tool_input_stream_keys(&item_id, call_id.as_deref());
        if keys.iter().any(|key| {
            self.streamed_tool_inputs
                .iter()
                .any(|existing| existing == key)
        }) {
            return None;
        }
        self.streamed_tool_inputs.extend(keys);
        let tool_name = item.get("name").and_then(Value::as_str).map(str::to_string);
        let delta = item
            .get("arguments")
            .map(value_to_arguments_string)
            .unwrap_or_else(|| "{}".to_string());
        Some(ProviderStreamEvent::ToolCallInputDelta {
            item_id,
            call_id,
            tool_name,
            delta,
        })
    }

    fn push_unique_item(&mut self, item: Value) {
        let new_id = item.get("id").and_then(Value::as_str);
        if let Some(new_id) = new_id {
            if let Some(existing) = self
                .output_items
                .iter_mut()
                .find(|existing| existing.get("id").and_then(Value::as_str) == Some(new_id))
            {
                *existing = item;
                return;
            }
        }

        self.output_items.push(item);
    }

    fn activate_reasoning_item(&mut self, item_id: &str) {
        self.active_reasoning_item_id = Some(item_id.to_string());
    }
}

fn tool_input_stream_keys(item_id: &str, call_id: Option<&str>) -> Vec<String> {
    let mut keys = Vec::new();
    let trimmed_item_id = item_id.trim();
    if !trimmed_item_id.is_empty() {
        keys.push(trimmed_item_id.to_string());
    }
    if let Some(call_id) = call_id.map(str::trim).filter(|value| !value.is_empty()) {
        if !keys.iter().any(|existing| existing == call_id) {
            keys.push(call_id.to_string());
        }
    }
    keys
}

fn merge_streamed_response_output(response: &mut Value, mut accumulator: StreamAccumulator) {
    let Some(response_object) = response.as_object_mut() else {
        return;
    };

    let mut merged_output = response_object
        .get("output")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();

    for item in std::mem::take(&mut accumulator.output_items) {
        let item_id = item.get("id").and_then(Value::as_str);
        let already_present = item_id.is_some_and(|item_id| {
            merged_output
                .iter()
                .any(|existing| existing.get("id").and_then(Value::as_str) == Some(item_id))
        });
        if !already_present {
            merged_output.push(item);
        }
    }

    response_object.insert("output".to_string(), Value::Array(merged_output));
}

fn build_responses_input(
    messages: &[ChatMessage],
    model_config: &ModelConfig,
) -> Result<Vec<Value>, ProviderError> {
    let mut input = Vec::new();

    for message in messages {
        match message.role {
            ChatRole::User => {
                let content = user_responses_content(message)?;
                if !content.is_empty() {
                    input.push(json!({
                        "type": "message",
                        "role": "user",
                        "content": content,
                    }));
                }
                append_responses_tool_outputs(&mut input, message);
                append_tool_result_image_messages(&mut input, message, model_config)?;
            }
            ChatRole::Compaction => {
                for item in &message.data {
                    let ChatMessageItem::Compaction(compaction) = item else {
                        continue;
                    };
                    if let Some(encrypted_content) = compaction.codex_encrypted_content() {
                        if !encrypted_content.trim().is_empty() {
                            input.push(json!({
                                "type": "compaction",
                                "encrypted_content": encrypted_content,
                            }));
                        }
                    } else if let Some(summary) = compaction.generic_summary_text() {
                        if !summary.trim().is_empty() {
                            input.push(json!({
                                "type": "message",
                                "role": "user",
                                "content": [{
                                    "type": "input_text",
                                    "text": summary,
                                }],
                            }));
                        }
                    }
                }
            }
            ChatRole::Assistant => {
                append_codex_reasoning_items(&mut input, message);
                append_assistant_response_items(&mut input, message, model_config)?;
                append_responses_tool_outputs(&mut input, message);
                append_tool_result_image_messages(&mut input, message, model_config)?;
            }
        }
    }

    Ok(input)
}

fn responses_value_to_chat_message(
    value: &Value,
    model_config: &ModelConfig,
    output_persistor: &OutputPersistor,
) -> Result<ChatMessage, ProviderError> {
    if let Some(error) = provider_error_message(value) {
        if let Some(kind) = provider_error_kind(value) {
            return Err(ProviderError::ProviderFailure {
                kind,
                message: error,
                body: value.to_string(),
            });
        }
        return Err(ProviderError::InvalidResponse(error));
    }

    let output = value
        .get("output")
        .and_then(Value::as_array)
        .ok_or_else(|| ProviderError::InvalidResponse("missing output array".to_string()))?;

    let mut data = Vec::new();

    for item in output {
        match item.get("type").and_then(Value::as_str) {
            Some("message") if item.get("role").and_then(Value::as_str) == Some("assistant") => {
                if let Some(content) = item.get("content").and_then(Value::as_array) {
                    append_responses_content_items(&mut data, content, output_persistor)?;
                }
            }
            Some("reasoning") => {
                if let Some(reasoning) = extract_codex_reasoning(item) {
                    data.push(ChatMessageItem::Reasoning(reasoning));
                }
            }
            Some("function_call") => {
                let call_id = item.get("call_id").and_then(Value::as_str).ok_or_else(|| {
                    ProviderError::InvalidResponse(
                        "responses function_call missing call_id".to_string(),
                    )
                })?;
                let name = item.get("name").and_then(Value::as_str).ok_or_else(|| {
                    ProviderError::InvalidResponse(
                        "responses function_call missing name".to_string(),
                    )
                })?;
                let arguments = item
                    .get("arguments")
                    .map(value_to_arguments_string)
                    .unwrap_or_else(|| "{}".to_string());
                data.push(ChatMessageItem::ToolCall(ToolCallItem {
                    tool_call_id: call_id.to_string(),
                    tool_name: name.to_string(),
                    arguments: ContextItem { text: arguments },
                }));
            }
            Some("image_generation_call") => {
                if let Some(reference) = item.get("result").and_then(Value::as_str) {
                    append_image_reference(&mut data, reference, output_persistor)?;
                }
            }
            _ => {}
        }
    }

    Ok(ChatMessage {
        message_id: ChatMessage::new_message_id(),
        role: ChatRole::Assistant,
        user_name: None,
        message_time: None,
        token_usage: token_usage_from_value(value, model_config),
        data,
    })
}

fn append_codex_reasoning_items(target: &mut Vec<Value>, message: &ChatMessage) {
    for item in &message.data {
        let ChatMessageItem::Reasoning(reasoning) = item else {
            continue;
        };
        let Some(encrypted_content) = reasoning
            .codex_encrypted_content
            .as_deref()
            .filter(|content| !content.is_empty())
        else {
            continue;
        };

        let mut payload = Map::new();
        payload.insert("type".to_string(), Value::String("reasoning".to_string()));
        payload.insert("summary".to_string(), reasoning_summary_payload(reasoning));
        payload.insert(
            "encrypted_content".to_string(),
            Value::String(encrypted_content.to_string()),
        );
        target.push(Value::Object(payload));
    }
}

fn reasoning_summary_payload(reasoning: &ReasoningItem) -> Value {
    Value::Array(
        reasoning
            .codex_summary
            .iter()
            .filter(|part| !part.text.is_empty())
            .map(|part| {
                json!({
                    "type": "summary_text",
                    "text": part.text,
                })
            })
            .collect(),
    )
}

fn user_responses_content(message: &ChatMessage) -> Result<Vec<Value>, ProviderError> {
    let mut content = Vec::new();

    for item in &message.data {
        match item {
            ChatMessageItem::Reasoning(_) | ChatMessageItem::ToolResult(_) => {}
            ChatMessageItem::Compaction(compaction) => {
                if let Some(text) = compaction.generic_summary_text() {
                    content.push(json!({
                        "type": "input_text",
                        "text": text,
                    }));
                }
            }
            ChatMessageItem::Context(context) => {
                content.push(json!({
                    "type": "input_text",
                    "text": context.text,
                }));
            }
            ChatMessageItem::SelectionReference(selection) => {
                content.push(json!({
                    "type": "input_text",
                    "text": selection.to_prompt_text(),
                }));
            }
            ChatMessageItem::File(file) => content.push(responses_file_item(file)?),
            ChatMessageItem::ToolCall(tool_call) => {
                content.push(json!({
                    "type": "input_text",
                    "text": format!(
                        "<tool_call name=\"{}\">{}</tool_call>",
                        tool_call.tool_name, tool_call.arguments.text
                    ),
                }));
            }
        }
    }

    Ok(content)
}

fn append_assistant_response_items(
    target: &mut Vec<Value>,
    message: &ChatMessage,
    model_config: &ModelConfig,
) -> Result<(), ProviderError> {
    let mut content = Vec::new();

    for item in &message.data {
        match item {
            ChatMessageItem::Context(context) => {
                content.push(json!({
                    "type": "output_text",
                    "text": context.text,
                }));
            }
            ChatMessageItem::Compaction(compaction) => {
                if let Some(text) = compaction.generic_summary_text() {
                    content.push(json!({
                        "type": "output_text",
                        "text": text,
                    }));
                }
            }
            ChatMessageItem::SelectionReference(selection) => {
                content.push(json!({
                    "type": "output_text",
                    "text": selection.to_prompt_text(),
                }));
            }
            ChatMessageItem::File(file) if is_image_file(file) && file.state.is_none() => {
                if model_config.supports(crate::model_config::ModelCapability::ImageIn) {
                    append_assistant_message_if_needed(target, &mut content);
                    append_assistant_image_visual_context(target, file, model_config)?;
                } else {
                    content.push(json!({
                        "type": "output_text",
                        "text": file_reference_text(file),
                    }));
                }
            }
            ChatMessageItem::File(file) => {
                content.push(json!({
                    "type": "output_text",
                    "text": file.uri,
                }));
            }
            ChatMessageItem::ToolCall(tool_call) => {
                append_assistant_message_if_needed(target, &mut content);
                target.push(json!({
                    "type": "function_call",
                    "name": tool_call.tool_name,
                    "arguments": tool_call.arguments.text,
                    "call_id": tool_call.tool_call_id,
                }));
            }
            ChatMessageItem::Reasoning(_) | ChatMessageItem::ToolResult(_) => {}
        }
    }

    append_assistant_message_if_needed(target, &mut content);
    Ok(())
}

fn append_assistant_message_if_needed(target: &mut Vec<Value>, content: &mut Vec<Value>) {
    if content.is_empty() {
        return;
    }
    target.push(json!({
        "type": "message",
        "role": "assistant",
        "content": std::mem::take(content),
    }));
}

fn append_assistant_image_visual_context(
    target: &mut Vec<Value>,
    file: &FileItem,
    model_config: &ModelConfig,
) -> Result<(), ProviderError> {
    let fake = ChatMessage::new(
        ChatRole::User,
        vec![
            ChatMessageItem::Context(ContextItem {
                text: "[Generated by Assistant]".to_string(),
            }),
            ChatMessageItem::File(file.clone()),
        ],
    );
    for normalized in normalize_messages_for_model(&[fake], model_config) {
        let content = user_responses_content(&normalized)?;
        if !content.is_empty() {
            target.push(json!({
                "type": "message",
                "role": "user",
                "content": content,
            }));
        }
    }
    Ok(())
}

fn file_reference_text(file: &FileItem) -> String {
    format!(
        "[Image output omitted]\nuri: {}\nname: {}\nmedia_type: {}",
        file.uri,
        file.name.as_deref().unwrap_or("<unknown>"),
        file.media_type.as_deref().unwrap_or("<unknown>")
    )
}

fn append_responses_tool_outputs(target: &mut Vec<Value>, message: &ChatMessage) {
    for item in &message.data {
        if let ChatMessageItem::ToolResult(tool_result) = item {
            target.push(json!({
                "type": "function_call_output",
                "call_id": tool_result.tool_call_id,
                "output": tool_result_text(tool_result),
            }));
        }
    }
}

fn append_tool_result_image_messages(
    target: &mut Vec<Value>,
    message: &ChatMessage,
    model_config: &ModelConfig,
) -> Result<(), ProviderError> {
    for item in &message.data {
        let ChatMessageItem::ToolResult(tool_result) = item else {
            continue;
        };
        for file in &tool_result.result.files {
            if !is_image_file(file) || file.state.is_some() {
                continue;
            }
            let fake = ChatMessage::new(ChatRole::User, vec![ChatMessageItem::File(file.clone())]);
            for normalized in normalize_messages_for_model(&[fake], model_config) {
                let content = user_responses_content(&normalized)?;
                if !content.is_empty() {
                    target.push(json!({
                        "type": "message",
                        "role": "user",
                        "content": content,
                    }));
                }
            }
        }
    }
    Ok(())
}

fn responses_file_item(file: &FileItem) -> Result<Value, ProviderError> {
    if is_image_file(file) {
        return Ok(json!({
            "type": "input_image",
            "image_url": file.uri,
        }));
    }

    let mut payload = Map::new();
    payload.insert("type".to_string(), Value::String("input_file".to_string()));
    if file.uri.starts_with("data:") {
        payload.insert(
            "filename".to_string(),
            Value::String(input_file_filename(file)),
        );
        payload.insert("file_data".to_string(), Value::String(file.uri.clone()));
    } else if let Some(file_id) = openai_file_id(&file.uri) {
        payload.insert("file_id".to_string(), Value::String(file_id.to_string()));
    } else if let Some(path) = local_file_path(&file.uri) {
        payload.insert(
            "filename".to_string(),
            Value::String(input_file_filename(file)),
        );
        payload.insert(
            "file_data".to_string(),
            Value::String(local_file_data_url(file, &path)?),
        );
    } else {
        payload.insert("file_url".to_string(), Value::String(file.uri.clone()));
    }
    Ok(Value::Object(payload))
}

fn input_file_filename(file: &FileItem) -> String {
    file.name
        .as_deref()
        .filter(|name| !name.trim().is_empty())
        .unwrap_or("file")
        .to_string()
}

fn openai_file_id(uri: &str) -> Option<&str> {
    let file_id = uri.strip_prefix("sediment://").unwrap_or(uri);
    if file_id.starts_with("file-") || file_id.starts_with("file_") {
        Some(file_id)
    } else {
        None
    }
}

fn local_file_path(uri: &str) -> Option<PathBuf> {
    if !uri.starts_with("file://") {
        return None;
    }
    Url::parse(uri)
        .ok()
        .and_then(|url| url.to_file_path().ok())
        .or_else(|| uri.strip_prefix("file://").map(PathBuf::from))
}

fn local_file_data_url(file: &FileItem, path: &PathBuf) -> Result<String, ProviderError> {
    let bytes = fs::read(path).map_err(|error| {
        ProviderError::InvalidResponse(format!(
            "failed to read local file input {}: {error}",
            path.display()
        ))
    })?;
    let media_type = input_file_media_type(file, path);
    Ok(format!(
        "data:{media_type};base64,{}",
        base64::engine::general_purpose::STANDARD.encode(bytes)
    ))
}

fn input_file_media_type(file: &FileItem, path: &PathBuf) -> String {
    if let Some(media_type) = file
        .media_type
        .as_deref()
        .filter(|media_type| !media_type.trim().is_empty())
    {
        return media_type.to_string();
    }

    match path
        .extension()
        .and_then(|extension| extension.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase()
        .as_str()
    {
        "csv" => "text/csv",
        "html" | "htm" => "text/html",
        "json" => "application/json",
        "md" | "markdown" => "text/markdown",
        "pdf" => "application/pdf",
        "txt" | "log" | "rs" | "toml" | "yaml" | "yml" | "xml" => "text/plain",
        _ => "application/octet-stream",
    }
    .to_string()
}

fn append_responses_content_items(
    data: &mut Vec<ChatMessageItem>,
    content: &[Value],
    output_persistor: &OutputPersistor,
) -> Result<(), ProviderError> {
    for item in content {
        match item.get("type").and_then(Value::as_str) {
            Some("output_text") | Some("text") | Some("refusal") => {
                if let Some(text) = item.get("text").and_then(Value::as_str) {
                    if !text.is_empty() {
                        data.push(ChatMessageItem::Context(ContextItem {
                            text: text.to_string(),
                        }));
                    }
                }
            }
            Some("image_url") | Some("output_image") | Some("input_image") => {
                if let Some(reference) = image_reference_from_item(item) {
                    append_image_reference(data, &reference, output_persistor)?;
                }
            }
            _ => {}
        }
    }

    Ok(())
}

fn append_image_reference(
    data: &mut Vec<ChatMessageItem>,
    reference: &str,
    output_persistor: &OutputPersistor,
) -> Result<(), ProviderError> {
    if reference.starts_with("data:") {
        data.push(ChatMessageItem::File(
            output_persistor.persist_image_data_url(reference)?,
        ));
    } else if is_probable_base64_image(reference) {
        let data_url = format!("data:image/png;base64,{reference}");
        data.push(ChatMessageItem::File(
            output_persistor.persist_image_data_url(&data_url)?,
        ));
    } else {
        data.push(ChatMessageItem::File(FileItem {
            uri: reference.to_string(),
            name: None,
            media_type: Some("image/*".to_string()),
            width: None,
            height: None,
            state: None,
        }));
    }

    Ok(())
}

fn is_probable_base64_image(reference: &str) -> bool {
    let trimmed = reference.trim();
    !trimmed.is_empty()
        && !trimmed.contains("://")
        && trimmed.len() % 4 == 0
        && trimmed.chars().all(|ch| {
            ch.is_ascii_alphanumeric()
                || ch == '+'
                || ch == '/'
                || ch == '='
                || ch == '\n'
                || ch == '\r'
        })
}

fn image_reference_from_item(item: &Value) -> Option<String> {
    value_string_or_url(item.get("image_url"))
        .or_else(|| value_string_or_url(item.get("imageUrl")))
        .or_else(|| value_string_or_url(item.get("url")))
        .or_else(|| {
            item.get("result")
                .and_then(Value::as_str)
                .map(str::to_string)
        })
}

fn value_string_or_url(value: Option<&Value>) -> Option<String> {
    match value {
        Some(Value::String(text)) => Some(text.clone()),
        Some(Value::Object(object)) => object
            .get("url")
            .and_then(Value::as_str)
            .or_else(|| object.get("image_url").and_then(Value::as_str))
            .or_else(|| object.get("imageUrl").and_then(Value::as_str))
            .map(str::to_string),
        _ => None,
    }
}

fn extract_codex_reasoning(item: &Value) -> Option<ReasoningItem> {
    let summary = extract_reasoning_summary(item);
    let encrypted_content = item
        .get("encrypted_content")
        .and_then(Value::as_str)
        .filter(|content| !content.is_empty())
        .map(str::to_string);

    if !summary.is_empty() || encrypted_content.is_some() {
        return Some(ReasoningItem::codex(summary, encrypted_content, None));
    }

    item.get("text")
        .and_then(Value::as_str)
        .filter(|text| !text.is_empty())
        .map(str::to_string)
        .map(ReasoningItem::from_text)
}

fn extract_reasoning_summary(item: &Value) -> Vec<crate::session_actor::ReasoningSummaryPart> {
    item.get("summary")
        .and_then(Value::as_array)
        .map(|summary| {
            summary
                .iter()
                .filter_map(|part| {
                    part.get("text")
                        .and_then(Value::as_str)
                        .or_else(|| part.as_str())
                })
                .filter(|text| !text.is_empty())
                .map(|text| crate::session_actor::ReasoningSummaryPart {
                    text: text.to_string(),
                })
                .collect::<Vec<_>>()
        })
        .unwrap_or_default()
}

fn tool_result_text(tool_result: &crate::session_actor::ToolResultItem) -> String {
    crate::session_actor::tool_result_text(tool_result)
}

fn value_to_arguments_string(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        other => other.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model_config::{
        MediaInputConfig, MediaInputTransport, ModelCapability, MultimodalInputConfig,
        ProviderType, RetryMode, TokenEstimatorType,
    };
    use crate::session_actor::{
        SessionInitial, SessionType, ToolCatalog, ToolRemoteMode, ToolResultContent, ToolResultItem,
    };
    use std::path::PathBuf;

    #[test]
    fn converts_http_url_to_websocket_url() {
        let url = build_websocket_url("https://chatgpt.com/backend-api/codex/responses")
            .expect("url should convert");

        assert_eq!(
            url.as_str(),
            "wss://chatgpt.com/backend-api/codex/responses"
        );
    }

    #[test]
    fn websocket_transport_error_is_reconnectable_for_cached_socket() {
        let response = Err(ProviderError::WebSocket(
            "WebSocket protocol error: Connection reset without closing handshake".to_string(),
        ));

        assert!(is_websocket_transport_error(&response));
    }

    #[test]
    fn websocket_broken_pipe_is_reconnectable_transport_error() {
        let response = Err(ProviderError::WebSocket(
            "IO error: Broken pipe (os error 32)".to_string(),
        ));

        assert!(is_websocket_transport_error(&response));
    }

    #[test]
    fn provider_payload_error_is_not_websocket_transport_error() {
        let response = Err(ProviderError::InvalidResponse(
            "provider returned response.failed".to_string(),
        ));

        assert!(!is_websocket_transport_error(&response));
    }

    #[test]
    fn websocket_provider_payload_error_is_not_reconnectable_transport_error() {
        let response = Err(ProviderError::WebSocket(
            "model rejected this request".to_string(),
        ));

        assert!(!is_websocket_transport_error(&response));
    }

    #[test]
    fn codex_response_progress_events_exclude_keepalive_noise() {
        assert!(is_codex_response_progress_event(
            "response.output_text.delta"
        ));
        assert!(is_codex_response_progress_event(
            "response.output_item.added"
        ));
        assert!(is_codex_response_progress_event(
            "response.function_call_arguments.done"
        ));

        assert!(!is_codex_response_progress_event("response.created"));
        assert!(!is_codex_response_progress_event("response.in_progress"));
        assert!(!is_codex_response_progress_event("rate_limits.updated"));
        assert!(!is_codex_response_progress_event("ping"));
    }

    #[test]
    fn function_call_argument_delta_uses_accumulated_tool_metadata() {
        let mut accumulator = StreamAccumulator::default();
        accumulator.record_output_item_added(&serde_json::json!({
            "type": "response.output_item.added",
            "item": {
                "type": "function_call",
                "id": "fc_1",
                "call_id": "call_1",
                "name": "exec_command"
            }
        }));

        let event = provider_tool_call_input_delta_event(
            &serde_json::json!({
                "type": "response.function_call_arguments.delta",
                "item_id": "fc_1",
                "delta": "{\"cmd\""
            }),
            &accumulator,
        )
        .expect("function call argument delta should be renderable");

        let ProviderStreamEvent::ToolCallInputDelta {
            item_id,
            call_id,
            tool_name,
            delta,
        } = event
        else {
            panic!("expected tool call input delta event");
        };
        assert_eq!(item_id, "fc_1");
        assert_eq!(call_id, None);
        assert_eq!(tool_name.as_deref(), Some("exec_command"));
        assert_eq!(delta, "{\"cmd\"");
    }

    #[test]
    fn completed_function_call_emits_tool_input_when_argument_deltas_were_absent() {
        let mut accumulator = StreamAccumulator::default();

        let event = accumulator
            .completed_tool_call_stream_event(&serde_json::json!({
                "type": "response.output_item.done",
                "item": {
                    "type": "function_call",
                    "id": "fc_1",
                    "call_id": "call_1",
                    "name": "apply_patch",
                    "arguments": {"patch": "*** Begin Patch\n*** End Patch\n"}
                }
            }))
            .expect("completed function call should be renderable");

        let ProviderStreamEvent::ToolCallInputDelta {
            item_id,
            call_id,
            tool_name,
            delta,
        } = event
        else {
            panic!("expected tool call input delta event");
        };
        assert_eq!(item_id, "fc_1");
        assert_eq!(call_id.as_deref(), Some("call_1"));
        assert_eq!(tool_name.as_deref(), Some("apply_patch"));
        assert!(delta.contains("Begin Patch"));
    }

    #[test]
    fn completed_function_call_does_not_duplicate_streamed_argument_delta() {
        let mut accumulator = StreamAccumulator::default();
        let streamed = ProviderStreamEvent::ToolCallInputDelta {
            item_id: "fc_1".to_string(),
            call_id: Some("call_1".to_string()),
            tool_name: Some("apply_patch".to_string()),
            delta: "{\"patch\"".to_string(),
        };
        accumulator.record_streamed_tool_input(&streamed);

        let event = accumulator.completed_tool_call_stream_event(&serde_json::json!({
            "type": "response.output_item.done",
            "item": {
                "type": "function_call",
                "id": "fc_1",
                "call_id": "call_1",
                "name": "apply_patch",
                "arguments": {"patch": "*** Begin Patch\n*** End Patch\n"}
            }
        }));

        assert!(event.is_none());
    }

    #[test]
    fn completed_function_call_matches_streamed_item_id_without_call_id() {
        let mut accumulator = StreamAccumulator::default();
        let streamed = ProviderStreamEvent::ToolCallInputDelta {
            item_id: "fc_1".to_string(),
            call_id: None,
            tool_name: Some("apply_patch".to_string()),
            delta: "{\"patch\"".to_string(),
        };
        accumulator.record_streamed_tool_input(&streamed);

        let events = accumulator.missing_tool_call_stream_events(&serde_json::json!({
            "id": "resp_1",
            "output": [{
                "type": "function_call",
                "id": "fc_1",
                "call_id": "call_1",
                "name": "apply_patch",
                "arguments": {"patch": "*** Begin Patch\n*** End Patch\n"}
            }]
        }));

        assert!(events.is_empty());
    }

    #[test]
    fn completed_response_emits_unstreamed_tool_inputs() {
        let mut accumulator = StreamAccumulator::default();

        let events = accumulator.missing_tool_call_stream_events(&serde_json::json!({
            "id": "resp_1",
            "output": [{
                "type": "function_call",
                "id": "fc_1",
                "call_id": "call_1",
                "name": "apply_patch",
                "arguments": "{\"patch\":\"*** Begin Patch\\n*** End Patch\\n\"}"
            }]
        }));

        assert_eq!(events.len(), 1);
        let ProviderStreamEvent::ToolCallInputDelta {
            item_id,
            call_id,
            tool_name,
            delta,
        } = &events[0]
        else {
            panic!("expected tool call input delta event");
        };
        assert_eq!(item_id, "fc_1");
        assert_eq!(call_id.as_deref(), Some("call_1"));
        assert_eq!(tool_name.as_deref(), Some("apply_patch"));
        assert!(delta.contains("Begin Patch"));
    }

    #[test]
    fn completed_response_tool_input_can_use_call_id_as_item_id() {
        let mut accumulator = StreamAccumulator::default();

        let events = accumulator.missing_tool_call_stream_events(&serde_json::json!({
            "id": "resp_1",
            "output": [{
                "type": "function_call",
                "call_id": "call_1",
                "name": "apply_patch",
                "arguments": "{\"patch\":\"*** Begin Patch\\n*** End Patch\\n\"}"
            }]
        }));

        assert_eq!(events.len(), 1);
        let ProviderStreamEvent::ToolCallInputDelta {
            item_id, call_id, ..
        } = &events[0]
        else {
            panic!("expected tool call input delta event");
        };
        assert_eq!(item_id, "call_1");
        assert_eq!(call_id.as_deref(), Some("call_1"));
    }

    #[test]
    fn merge_streamed_response_output_ignores_delta_only_state() {
        let mut response = serde_json::json!({"id": "resp_1", "output": []});
        let accumulator = StreamAccumulator::default();

        merge_streamed_response_output(&mut response, accumulator);

        assert!(response["output"].as_array().unwrap().is_empty());
    }

    #[test]
    fn merge_streamed_response_output_appends_completed_stream_items() {
        let mut response = serde_json::json!({
            "id": "resp_1",
            "output": [{
                "type": "reasoning",
                "id": "rs_1",
                "summary": [],
                "encrypted_content": "opaque"
            }]
        });
        let mut accumulator = StreamAccumulator::default();
        accumulator.record_output_item_done(&serde_json::json!({
            "type": "response.output_item.done",
            "item": {
                "type": "message",
                "id": "msg_1",
                "role": "assistant",
                "content": [{
                    "type": "output_text",
                    "text": "completed stream item"
                }]
            }
        }));

        merge_streamed_response_output(&mut response, accumulator);

        assert_eq!(response["output"].as_array().unwrap().len(), 2);
        assert_eq!(
            response["output"][1]["content"][0]["text"],
            "completed stream item"
        );
    }

    #[test]
    fn merge_streamed_response_output_does_not_duplicate_completed_items() {
        let mut response = serde_json::json!({
            "id": "resp_1",
            "output": [{
                "type": "message",
                "id": "msg_1",
                "role": "assistant",
                "content": [{
                    "type": "output_text",
                    "text": "completed text"
                }]
            }]
        });
        let mut accumulator = StreamAccumulator::default();
        accumulator.record_output_item_done(&serde_json::json!({
            "type": "response.output_item.done",
            "item": {
                "type": "message",
                "id": "msg_1",
                "role": "assistant",
                "content": [{
                    "type": "output_text",
                    "text": "stream item duplicate"
                }]
            }
        }));

        merge_streamed_response_output(&mut response, accumulator);

        assert_eq!(response["output"].as_array().unwrap().len(), 1);
        assert_eq!(
            response["output"][0]["content"][0]["text"],
            "completed text"
        );
    }

    #[test]
    fn parses_chatgpt_account_and_expiration_from_jwt() {
        let token = fake_jwt(
            r#"{"exp":4102444800,"https://api.openai.com/auth":{"chatgpt_account_id":"acc_123","chatgpt_account_is_fedramp":true}}"#,
        );

        assert_eq!(
            chatgpt_account_id_from_tokens(&token, None, None),
            Some("acc_123".to_string())
        );
        assert_eq!(jwt_expiration(&token), Some(4_102_444_800));
        assert_eq!(chatgpt_fedramp_from_tokens(&token, None), Some(true));
    }

    #[test]
    fn persists_refreshed_auth_json_without_dropping_unknown_fields() {
        let path = temp_auth_json_path();
        fs::write(
            &path,
            serde_json::json!({
                "tokens": {
                    "access_token": "old-access",
                    "refresh_token": "old-refresh"
                },
                "custom": "keep-me"
            })
            .to_string(),
        )
        .expect("auth json should write");

        let material = CodexAuthMaterial {
            access_token: "new-access".to_string(),
            refresh_token: Some("new-refresh".to_string()),
            account_id: "acc_123".to_string(),
            is_fedramp_account: false,
            expires_at: None,
            source: CodexAuthSource::AuthJson(path.clone()),
        };

        persist_refreshed_auth_json(&path, &material, Some("new-id")).expect("persist succeeds");

        let value = serde_json::from_str::<Value>(
            &fs::read_to_string(&path).expect("auth json should read"),
        )
        .expect("auth json should parse");
        assert_eq!(value["tokens"]["access_token"], "new-access");
        assert_eq!(value["tokens"]["refresh_token"], "new-refresh");
        assert_eq!(value["tokens"]["account_id"], "acc_123");
        assert_eq!(value["tokens"]["id_token"], "new-id");
        assert_eq!(value["custom"], "keep-me");
        assert!(value.get("last_refresh").is_some());

        let _ = fs::remove_file(path);
    }

    #[test]
    fn refresh_material_posts_to_oauth_endpoint_and_updates_tokens() {
        let _env_guard = test_env_lock();
        let mut server = mockito::Server::new();
        let new_access = fake_jwt(
            r#"{"exp":4102444800,"https://api.openai.com/auth":{"chatgpt_account_id":"acc_123"}}"#,
        );
        let mock = server
            .mock("POST", "/oauth/token")
            .match_header("content-type", "application/json")
            .match_body(mockito::Matcher::PartialJson(serde_json::json!({
                "client_id": CODEX_CLIENT_ID,
                "grant_type": "refresh_token",
                "refresh_token": "old-refresh"
            })))
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(
                serde_json::json!({
                    "access_token": new_access,
                    "refresh_token": "new-refresh"
                })
                .to_string(),
            )
            .create();
        std::env::set_var(
            CHATGPT_REFRESH_TOKEN_URL_OVERRIDE_ENV,
            format!("{}/oauth/token", server.url()),
        );

        let manager = CodexSubscriptionAuthManager::default();
        let previous = CodexAuthMaterial {
            access_token: fake_jwt(
                r#"{"exp":1,"https://api.openai.com/auth":{"chatgpt_account_id":"acc_123"}}"#,
            ),
            refresh_token: Some("old-refresh".to_string()),
            account_id: "acc_123".to_string(),
            is_fedramp_account: false,
            expires_at: Some(1),
            source: CodexAuthSource::Env,
        };
        let refreshed = manager
            .refresh_material(&test_model_config(), &previous)
            .expect("refresh should succeed");

        mock.assert();
        assert_eq!(refreshed.refresh_token.as_deref(), Some("new-refresh"));
        assert_eq!(refreshed.account_id, "acc_123");
        assert_eq!(refreshed.expires_at, Some(4_102_444_800));
        std::env::remove_var(CHATGPT_REFRESH_TOKEN_URL_OVERRIDE_ENV);
    }

    #[test]
    fn codex_fast_mode_maps_to_priority_service_tier_and_not_reasoning() {
        let mut config = test_model_config();
        config.reasoning = Some(serde_json::json!({
            "effort": "medium",
            "fast_mode": true,
            "service_tier": "fast",
            "max_tokens": 1024
        }));

        assert_eq!(
            codex_service_tier_payload(&config),
            Some("priority".to_string())
        );
        assert_eq!(
            codex_reasoning_payload(&config),
            Some(serde_json::json!({"effort": "medium"}))
        );
    }

    #[test]
    fn codex_fast_mode_can_be_disabled_with_default_service_tier() {
        let mut config = test_model_config();
        config.reasoning = Some(serde_json::json!({
            "service_tier": "default"
        }));

        assert_eq!(codex_service_tier_payload(&config), None);
        assert_eq!(codex_reasoning_payload(&config), None);
    }

    #[test]
    fn input_file_payload_uses_filename_only_with_file_data() {
        let file = FileItem {
            uri: "data:application/pdf;base64,abc".to_string(),
            name: Some("demo.pdf".to_string()),
            media_type: Some("application/pdf".to_string()),
            width: None,
            height: None,
            state: None,
        };

        assert_eq!(
            responses_file_item(&file).expect("file item should serialize"),
            serde_json::json!({
                "type": "input_file",
                "filename": "demo.pdf",
                "file_data": "data:application/pdf;base64,abc"
            })
        );
    }

    #[test]
    fn input_file_payload_does_not_mix_filename_with_url_or_file_id() {
        let url_file = FileItem {
            uri: "https://example.com/demo.pdf".to_string(),
            name: Some("demo.pdf".to_string()),
            media_type: Some("application/pdf".to_string()),
            width: None,
            height: None,
            state: None,
        };
        let uploaded_file = FileItem {
            uri: "sediment://file-abc123".to_string(),
            name: Some("demo.pdf".to_string()),
            media_type: Some("application/pdf".to_string()),
            width: None,
            height: None,
            state: None,
        };

        assert_eq!(
            responses_file_item(&url_file).expect("file item should serialize"),
            serde_json::json!({
                "type": "input_file",
                "file_url": "https://example.com/demo.pdf"
            })
        );
        assert_eq!(
            responses_file_item(&uploaded_file).expect("file item should serialize"),
            serde_json::json!({
                "type": "input_file",
                "file_id": "file-abc123"
            })
        );
    }

    #[test]
    fn input_file_payload_inlines_local_file_uri() {
        let path = std::env::temp_dir().join(format!("stellaclaw-file-{}.txt", nonce("test")));
        fs::write(&path, b"hello from local file").expect("temp file should write");
        let file = FileItem {
            uri: format!("file://{}", path.display()),
            name: Some("local.txt".to_string()),
            media_type: None,
            width: None,
            height: None,
            state: None,
        };

        let payload = responses_file_item(&file).expect("local file should serialize");

        assert_eq!(payload["type"], "input_file");
        assert_eq!(payload["filename"], "local.txt");
        assert_eq!(
            payload["file_data"],
            format!(
                "data:text/plain;base64,{}",
                base64::engine::general_purpose::STANDARD.encode(b"hello from local file")
            )
        );
        assert!(payload.get("file_url").is_none());

        let _ = fs::remove_file(path);
    }

    #[test]
    fn compaction_message_replays_as_responses_compaction_item() {
        let messages = vec![ChatMessage::new(
            ChatRole::Compaction,
            vec![ChatMessageItem::Compaction(
                CompactionItem::provider_builtin(
                    ProviderType::CodexSubscription,
                    json!({ "encrypted_content": "encrypted-compact-state" }),
                ),
            )],
        )];

        let input =
            build_responses_input(&messages, &test_model_config()).expect("input should build");

        assert_eq!(input.len(), 1);
        assert_eq!(input[0]["type"], "compaction");
        assert_eq!(input[0]["encrypted_content"], "encrypted-compact-state");
    }

    #[test]
    fn compact_response_maps_opaque_item_to_compaction_message() {
        let response = serde_json::json!({
            "output": [
                {
                    "type": "compaction",
                    "encrypted_content": "encrypted-compact-state"
                },
                {
                    "type": "message",
                    "role": "user",
                    "content": [
                        { "type": "input_text", "text": "recent user request" }
                    ]
                }
            ]
        });

        let messages = compact_response_value_to_chat_messages(&response)
            .expect("compact response should parse");

        assert_eq!(messages.len(), 2);
        assert_eq!(messages[0].role, ChatRole::Compaction);
        assert_eq!(messages[1].role, ChatRole::User);
        assert!(matches!(
            messages[0].data.first(),
            Some(ChatMessageItem::Compaction(compaction))
                if compaction.codex_encrypted_content() == Some("encrypted-compact-state")
        ));
    }

    #[test]
    fn assistant_image_output_replays_as_user_image_context_when_images_supported() {
        let config = image_in_model_config();
        let messages = vec![ChatMessage::new(
            ChatRole::Assistant,
            vec![ChatMessageItem::File(FileItem {
                uri: "data:image/png;base64,QUJD".to_string(),
                name: Some("generated.png".to_string()),
                media_type: Some("image/png".to_string()),
                width: None,
                height: None,
                state: None,
            })],
        )];

        let input = build_responses_input(&messages, &config).expect("input should build");

        assert_eq!(input.len(), 1);
        assert_eq!(input[0]["type"], "message");
        assert_eq!(input[0]["role"], "user");
        assert_eq!(input[0]["content"][0]["type"], "input_text");
        assert_eq!(input[0]["content"][0]["text"], "[Generated by Assistant]");
        assert_eq!(input[0]["content"][1]["type"], "input_image");
    }

    #[test]
    fn assistant_image_output_becomes_text_when_images_unsupported() {
        let messages = vec![ChatMessage::new(
            ChatRole::Assistant,
            vec![ChatMessageItem::File(FileItem {
                uri: "data:image/png;base64,QUJD".to_string(),
                name: Some("generated.png".to_string()),
                media_type: Some("image/png".to_string()),
                width: None,
                height: None,
                state: None,
            })],
        )];

        let input =
            build_responses_input(&messages, &test_model_config()).expect("input should build");

        assert_eq!(input.len(), 1);
        assert_eq!(input[0]["type"], "message");
        assert_eq!(input[0]["role"], "assistant");
        assert!(input[0]["content"][0]["text"]
            .as_str()
            .is_some_and(|text| text.contains("Image output omitted")));
    }

    #[test]
    fn tool_result_image_is_replayed_as_user_image_after_tool_output() {
        let config = image_in_model_config();
        let messages = vec![ChatMessage::new(
            ChatRole::Assistant,
            vec![ChatMessageItem::ToolResult(ToolResultItem {
                tool_call_id: "call_1".to_string(),
                tool_name: "image_view".to_string(),
                result: ToolResultContent::from_text("loaded image".to_string()).with_file(
                    FileItem {
                        uri: "data:image/png;base64,QUJD".to_string(),
                        name: Some("loaded.png".to_string()),
                        media_type: Some("image/png".to_string()),
                        width: None,
                        height: None,
                        state: None,
                    },
                ),
            })],
        )];

        let input = build_responses_input(&messages, &config).expect("input should build");

        assert_eq!(input.len(), 2);
        assert_eq!(input[0]["type"], "function_call_output");
        assert_eq!(input[0]["call_id"], "call_1");
        assert_eq!(input[1]["type"], "message");
        assert_eq!(input[1]["role"], "user");
        assert_eq!(input[1]["content"][0]["type"], "input_image");
        assert_eq!(
            input[1]["content"][0]["image_url"],
            "data:image/png;base64,QUJD"
        );
    }

    #[test]
    fn encrypted_reasoning_without_summary_is_not_exposed_as_text() {
        let item = serde_json::json!({
            "type": "reasoning",
            "encrypted_content": "opaque",
            "summary": []
        });

        let reasoning = extract_codex_reasoning(&item).expect("encrypted reasoning is retained");
        assert!(reasoning.text.is_empty());
        assert!(reasoning.codex_summary.is_empty());
        assert_eq!(reasoning.codex_encrypted_content.as_deref(), Some("opaque"));
    }

    #[test]
    fn streamed_reasoning_summary_is_stored_as_codex_summary() {
        let item = serde_json::json!({
            "type": "reasoning",
            "summary": [{
                "type": "summary_text",
                "text": "Inspected files."
            }]
        });

        let reasoning = extract_codex_reasoning(&item).expect("summary reasoning is retained");

        assert!(reasoning.text.is_empty());
        assert_eq!(
            reasoning.codex_summary_text().as_deref(),
            Some("Inspected files.")
        );
        assert_eq!(reasoning.codex_encrypted_content, None);
    }

    #[test]
    fn codex_reasoning_round_trips_as_responses_reasoning_item() {
        let messages = vec![ChatMessage::new(
            ChatRole::Assistant,
            vec![
                ChatMessageItem::Reasoning(ReasoningItem::codex(
                    vec![crate::session_actor::ReasoningSummaryPart {
                        text: "checked repository state".to_string(),
                    }],
                    Some("encrypted-state".to_string()),
                    Some("raw text should not be sent".to_string()),
                )),
                ChatMessageItem::Context(ContextItem {
                    text: "visible answer".to_string(),
                }),
            ],
        )];

        let provider = CodexSubscriptionProvider::new();
        let normalized = provider.normalize_messages_for_provider(&test_model_config(), &messages);
        let input =
            build_responses_input(&normalized, &test_model_config()).expect("input should build");

        assert_eq!(input[0]["type"], "reasoning");
        assert_eq!(input[0]["encrypted_content"], "encrypted-state");
        assert_eq!(input[0]["summary"][0]["text"], "checked repository state");
        assert!(input[0].get("text").is_none());
        assert_eq!(input[1]["type"], "message");
        assert_eq!(input[1]["content"][0]["text"], "visible answer");
    }

    #[test]
    fn codex_provider_normalization_drops_plain_reasoning_and_sanitizes_text() {
        let messages = vec![ChatMessage::new(
            ChatRole::Assistant,
            vec![
                ChatMessageItem::Reasoning(ReasoningItem::from_text("plain reasoning")),
                ChatMessageItem::Reasoning(ReasoningItem::codex(
                    vec![crate::session_actor::ReasoningSummaryPart {
                        text: "summary".to_string(),
                    }],
                    Some("encrypted".to_string()),
                    Some("raw text".to_string()),
                )),
            ],
        )];

        let provider = CodexSubscriptionProvider::new();
        let normalized = provider.normalize_messages_for_provider(&test_model_config(), &messages);

        assert_eq!(normalized.len(), 1);
        assert_eq!(normalized[0].data.len(), 1);
        let ChatMessageItem::Reasoning(reasoning) = &normalized[0].data[0] else {
            panic!("expected reasoning item");
        };
        assert!(reasoning.text.is_empty());
        assert_eq!(reasoning.codex_summary_text().as_deref(), Some("summary"));
        assert_eq!(
            reasoning.codex_encrypted_content.as_deref(),
            Some("encrypted")
        );
    }

    #[test]
    fn codex_provider_system_prompt_injects_pragmatic_personality() {
        let prompt = render_codex_model_instructions(&CodexModelMessages {
            instructions_template: Some(
                "header\n\n{{ personality }}\n\nbase instructions".to_string(),
            ),
            instructions_variables: Some(CodexModelInstructionsVariables {
                personality_pragmatic: Some("remote pragmatic prompt".to_string()),
            }),
        })
        .expect("prompt should render");

        assert_eq!(
            prompt,
            "header\n\nremote pragmatic prompt\n\nbase instructions"
        );
    }

    #[test]
    fn codex_provider_system_prompt_uses_local_pragmatic_fallback() {
        let prompt = render_codex_model_instructions(&CodexModelMessages {
            instructions_template: Some("header\n\n{{ personality }}".to_string()),
            instructions_variables: None,
        })
        .expect("prompt should render");

        assert!(prompt.contains(CODEX_PRAGMATIC_PERSONALITY_PROMPT));
        assert!(!prompt.contains(CODEX_PERSONALITY_PLACEHOLDER));
    }

    #[test]
    fn codex_provider_system_prompt_fetches_models_and_caches_to_disk() {
        let _env_guard = test_env_lock();
        let mut server = mockito::Server::new();
        let cache_root =
            std::env::temp_dir().join(format!("stellaclaw-codex-cache-{}", nonce("test")));
        let _env = EnvRestore::set_many(&[
            (
                "STELLACLAW_CODEX_CACHE_ROOT",
                Some(cache_root.to_string_lossy().to_string()),
            ),
            (
                CODEX_MODELS_URL_OVERRIDE_ENV,
                Some(format!("{}/backend-api/codex/models", server.url())),
            ),
            (
                "CHATGPT_ACCESS_TOKEN",
                Some(fake_jwt(
                    r#"{"exp":4102444800,"https://api.openai.com/auth":{"chatgpt_account_id":"acc_123"}}"#,
                )),
            ),
            ("CHATGPT_ACCOUNT_ID", Some("acc_123".to_string())),
            ("CODEX_AUTH_JSON", None),
            ("CHATGPT_AUTH_JSON", None),
            (
                "CODEX_HOME",
                Some(cache_root.join("codex-home").to_string_lossy().to_string()),
            ),
            (
                "HOME",
                Some(cache_root.join("home").to_string_lossy().to_string()),
            ),
        ]);
        let config = test_model_config();
        let mock = server
            .mock("GET", "/backend-api/codex/models")
            .match_query(mockito::Matcher::Any)
            .match_header(
                "authorization",
                mockito::Matcher::Regex("^Bearer ".to_string()),
            )
            .match_header("chatgpt-account-id", "acc_123")
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_header("etag", "models-etag-1")
            .with_body(
                json!({
                    "models": [{
                        "slug": config.model_name,
                        "model_messages": {
                            "instructions_template": "provider native prompt"
                        }
                    }]
                })
                .to_string(),
            )
            .create();

        let provider = CodexSubscriptionProvider::new();
        assert_eq!(
            provider
                .system_prompt_for_model(&config)
                .expect("models fetch should succeed")
                .as_deref(),
            Some("provider native prompt")
        );
        mock.assert();

        let cache_value = serde_json::from_str::<Value>(
            &fs::read_to_string(cache_root.join("models.json")).expect("models cache should exist"),
        )
        .expect("models cache should parse");
        assert_eq!(cache_value["etag"], "models-etag-1");
        assert_eq!(
            cache_value["models"][0]["model_messages"]["instructions_template"],
            "provider native prompt"
        );

        let cached_provider = CodexSubscriptionProvider::new();
        std::env::set_var(
            CODEX_MODELS_URL_OVERRIDE_ENV,
            "http://127.0.0.1:9/unreachable-models",
        );
        assert_eq!(
            cached_provider
                .system_prompt_for_model(&config)
                .expect("disk cache should avoid network")
                .as_deref(),
            Some("provider native prompt")
        );
        let _ = fs::remove_dir_all(cache_root);
    }

    #[test]
    fn codex_provider_system_prompt_returns_error_when_models_fetch_fails() {
        let _env_guard = test_env_lock();
        let mut server = mockito::Server::new();
        let cache_root =
            std::env::temp_dir().join(format!("stellaclaw-codex-cache-{}", nonce("test")));
        let _env = EnvRestore::set_many(&[
            (
                "STELLACLAW_CODEX_CACHE_ROOT",
                Some(cache_root.to_string_lossy().to_string()),
            ),
            (
                CODEX_MODELS_URL_OVERRIDE_ENV,
                Some(format!("{}/backend-api/codex/models", server.url())),
            ),
            (
                "CHATGPT_ACCESS_TOKEN",
                Some(fake_jwt(
                    r#"{"exp":4102444800,"https://api.openai.com/auth":{"chatgpt_account_id":"acc_123"}}"#,
                )),
            ),
            ("CHATGPT_ACCOUNT_ID", Some("acc_123".to_string())),
            ("CODEX_AUTH_JSON", None),
            ("CHATGPT_AUTH_JSON", None),
            (
                "CODEX_HOME",
                Some(cache_root.join("codex-home").to_string_lossy().to_string()),
            ),
            (
                "HOME",
                Some(cache_root.join("home").to_string_lossy().to_string()),
            ),
        ]);
        let config = test_model_config();
        let mock = server
            .mock("GET", "/backend-api/codex/models")
            .match_query(mockito::Matcher::Any)
            .with_status(500)
            .with_body("models unavailable")
            .create();

        let provider = CodexSubscriptionProvider::new();
        let error = provider
            .system_prompt_for_model(&config)
            .expect_err("models fetch failure should surface");

        mock.assert();
        assert!(matches!(
            error,
            ProviderError::HttpStatus { status: 500, .. }
        ));
        let _ = fs::remove_dir_all(cache_root);
    }

    #[test]
    fn codex_tool_set_uses_codex_shell_and_patch_facades() {
        let config = test_model_config();
        let initial = SessionInitial::new("session_1", SessionType::Foreground);
        let provider = CodexSubscriptionProvider::new();
        let tool_set = provider
            .tool_set(&config)
            .expect("codex provider should expose a tool set");
        let catalog = ToolCatalog::from_model_config_and_initial_with_tool_set(
            &config,
            &initial,
            Some(tool_set.as_ref()),
        )
        .expect("catalog should build");

        let apply_patch = catalog.get("apply_patch").expect("apply_patch exists");
        assert!(apply_patch.parameters["properties"].get("patch").is_some());
        assert!(apply_patch.parameters["properties"].get("format").is_none());
        assert!(catalog.contains("exec_command"));
        assert!(catalog.contains("write_stdin"));
        assert!(catalog.contains("exec_stop"));
        assert!(catalog.contains("update_plan"));
        assert!(!catalog.contains("shell_exec"));
        assert!(!catalog.contains("shell_write_stdin"));
        assert!(!catalog.contains("shell_stop"));
    }

    #[test]
    fn codex_tool_set_keeps_fixed_ssh_visibility_tools() {
        let config = test_model_config();
        let mut initial = SessionInitial::new("session_1", SessionType::Foreground);
        initial.tool_remote_mode = ToolRemoteMode::FixedSsh {
            host: "gpu-box".to_string(),
            cwd: None,
        };
        let provider = CodexSubscriptionProvider::new();
        let tool_set = provider
            .tool_set(&config)
            .expect("codex provider should expose a tool set");
        let catalog = ToolCatalog::from_model_config_and_initial_with_tool_set(
            &config,
            &initial,
            Some(tool_set.as_ref()),
        )
        .expect("catalog should build");

        assert!(catalog.contains("exec_make_visible"));
        assert!(!catalog.contains("shell_make_visible"));
        assert!(catalog.contains("attachment_make_visible"));
    }

    fn fake_jwt(payload: &str) -> String {
        let header = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(br#"{"alg":"none"}"#);
        let payload = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(payload);
        format!("{header}.{payload}.sig")
    }

    fn temp_auth_json_path() -> PathBuf {
        std::env::temp_dir().join(format!("stellaclaw-auth-{}.json", nonce("test")))
    }

    static TEST_ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn test_env_lock() -> std::sync::MutexGuard<'static, ()> {
        TEST_ENV_LOCK.lock().expect("test env mutex poisoned")
    }

    struct EnvRestore {
        previous: Vec<(&'static str, Option<std::ffi::OsString>)>,
    }

    impl EnvRestore {
        fn set_many(values: &[(&'static str, Option<String>)]) -> Self {
            let previous = values
                .iter()
                .map(|(name, _)| (*name, std::env::var_os(name)))
                .collect::<Vec<_>>();
            for (name, value) in values {
                match value {
                    Some(value) => std::env::set_var(name, value),
                    None => std::env::remove_var(name),
                }
            }
            Self { previous }
        }
    }

    impl Drop for EnvRestore {
        fn drop(&mut self) {
            for (name, value) in self.previous.drain(..) {
                match value {
                    Some(value) => std::env::set_var(name, value),
                    None => std::env::remove_var(name),
                }
            }
        }
    }

    fn test_model_config() -> ModelConfig {
        ModelConfig {
            provider_type: ProviderType::CodexSubscription,
            model_name: "gpt-5.5".to_string(),
            url: "https://chatgpt.com/backend-api/codex/responses".to_string(),
            api_key_env: "CHATGPT_ACCESS_TOKEN_TEST".to_string(),
            capabilities: vec![ModelCapability::Chat],
            token_max_context: 128_000,
            max_tokens: 0,
            cache_timeout: 300,
            idle_timeout_compact_enabled: true,
            conn_timeout: 5,
            request_timeout: 600,
            max_request_size: 30 * 1024 * 1024,
            retry_mode: RetryMode::Once,
            reasoning: None,
            token_estimator_type: TokenEstimatorType::Local,
            multimodal_estimator: None,
            multimodal_input: None,
            token_estimator_url: None,
        }
    }

    fn image_in_model_config() -> ModelConfig {
        let mut config = test_model_config();
        config.capabilities.push(ModelCapability::ImageIn);
        config.multimodal_input = Some(MultimodalInputConfig {
            image: Some(MediaInputConfig {
                transport: MediaInputTransport::FileReference,
                supported_media_types: vec!["image/png".to_string()],
                max_width: None,
                max_height: None,
            }),
            pdf: None,
            audio: None,
        });
        config
    }
}
