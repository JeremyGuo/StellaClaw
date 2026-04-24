use std::{
    fs,
    net::TcpStream,
    path::PathBuf,
    sync::Mutex,
    thread::sleep,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use base64::Engine as _;
use rand::Rng;
use reqwest::{blocking::Client, StatusCode};
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};
use tungstenite::{
    client::IntoClientRequest, connect, http::HeaderValue, stream::MaybeTlsStream, Message,
    WebSocket,
};
use url::Url;

use crate::{
    model_config::{ModelConfig, RetryMode},
    session_actor::{
        ChatMessage, ChatMessageItem, ChatRole, ContextItem, FileItem, ReasoningItem, ToolCallItem,
        ToolDefinition,
    },
};

use super::{
    common::{
        account_id_from_access_token, ensure_request_payload_size, is_image_file, nonce,
        provider_error_message, token_usage_from_value,
    },
    OutputPersistor, Provider, ProviderError, ProviderRequest,
};

const OPENAI_BETA_RESPONSES_WEBSOCKETS: &str = "responses_websockets=2026-02-06";
const CHATGPT_REFRESH_TOKEN_URL: &str = "https://auth.openai.com/oauth/token";
const CHATGPT_REFRESH_TOKEN_URL_OVERRIDE_ENV: &str = "CODEX_REFRESH_TOKEN_URL_OVERRIDE";
const CODEX_CLIENT_ID: &str = "app_EMoamEEZ73f0CkXaXp7hrann";

pub struct CodexSubscriptionProvider {
    output_persistor: OutputPersistor,
    auth_manager: CodexSubscriptionAuthManager,
    socket: Mutex<Option<WebSocket<MaybeTlsStream<TcpStream>>>>,
    session_id: String,
    installation_id: String,
}

#[derive(Debug, Default)]
struct StreamAccumulator {
    output_items: Vec<Value>,
    text_deltas: String,
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
    ) -> Result<ChatMessage, ProviderError> {
        let auth = self.auth_manager.resolve(model_config)?;

        let payload = self.build_payload(model_config, request)?;

        match self.send_with_auth(model_config, payload.clone(), &auth) {
            Ok(message) => Ok(message),
            Err(error) if is_unauthorized(&error) => {
                self.clear_socket();
                let refreshed = self.auth_manager.refresh(model_config, &auth)?;
                self.send_with_auth(model_config, payload, &refreshed)
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
            Value::Array(build_responses_input(request.messages)?),
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
        )?;
        responses_value_to_chat_message(&response, model_config, &self.output_persistor)
    }

    fn send_response_create_with_transport_reconnect(
        &self,
        model_config: &ModelConfig,
        payload: Map<String, Value>,
        auth: &CodexAuthMaterial,
        initial_socket: Option<WebSocket<MaybeTlsStream<TcpStream>>>,
    ) -> Result<Value, ProviderError> {
        let mut socket = initial_socket;
        let mut retried_transport_error = false;

        loop {
            let mut active_socket = match socket.take() {
                Some(socket) => socket,
                None => connect_codex_websocket(model_config, auth, &self.session_id)?,
            };
            let response = send_response_create(&mut active_socket, payload.clone(), model_config);
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

    fn should_retry(error: &ProviderError) -> bool {
        matches!(error, ProviderError::WebSocket(_))
    }
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
            session_id,
            installation_id,
        }
    }
}

impl Provider for CodexSubscriptionProvider {
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

    fn filter_tools_for_provider<'a>(
        &self,
        _model_config: &ModelConfig,
        tools: Vec<&'a ToolDefinition>,
    ) -> Vec<&'a ToolDefinition> {
        tools
            .into_iter()
            .filter(|tool| tool.name != "user_tell")
            .collect()
    }

    fn send(
        &self,
        model_config: &ModelConfig,
        request: ProviderRequest<'_>,
    ) -> Result<ChatMessage, ProviderError> {
        let mut retries_used = 0_u64;

        loop {
            match self.send_once(model_config, &request) {
                Ok(response) => return Ok(response),
                Err(error) if Self::should_retry(&error) => match &model_config.retry_mode {
                    RetryMode::Once => return Err(error),
                    RetryMode::RandomInterval {
                        max_interval_secs,
                        max_retries,
                    } => {
                        if retries_used >= *max_retries {
                            return Err(error);
                        }
                        retries_used = retries_used.saturating_add(1);

                        let sleep_secs = if *max_interval_secs == 0 {
                            0
                        } else {
                            rand::rng().random_range(0..=*max_interval_secs)
                        };
                        sleep(Duration::from_secs(sleep_secs));
                    }
                },
                Err(error) => return Err(error),
            }
        }
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
            _ => Some(item.clone()),
        })
        .collect::<Vec<_>>();

    (!data.is_empty()).then(|| ChatMessage {
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
        .insert("user-agent", HeaderValue::from_static("codex-cli"));
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

    let (mut socket, _) = connect(request).map_err(map_websocket_connect_error)?;
    set_socket_timeout(
        &mut socket,
        Duration::from_secs(model_config.request_timeout_secs()),
    )?;
    Ok(socket)
}

fn send_response_create(
    socket: &mut WebSocket<MaybeTlsStream<TcpStream>>,
    payload: Map<String, Value>,
    model_config: &ModelConfig,
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

    loop {
        let message = socket
            .read()
            .map_err(|error| ProviderError::WebSocket(error.to_string()))?;

        match message {
            Message::Text(text) => {
                let value =
                    serde_json::from_str::<Value>(&text).map_err(ProviderError::DecodeJson)?;
                match value.get("type").and_then(Value::as_str) {
                    Some("response.completed") => {
                        let mut response = value.get("response").cloned().ok_or_else(|| {
                            ProviderError::InvalidResponse(
                                "codex websocket completed without response".to_string(),
                            )
                        })?;
                        merge_streamed_response_output(&mut response, accumulator);
                        if let Some(error) = provider_error_message(&response) {
                            return Err(ProviderError::InvalidResponse(error));
                        }
                        return Ok(response);
                    }
                    Some("response.failed") | Some("error") => {
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
                        accumulator.record_output_item_done(&value);
                    }
                    Some("response.output_text.delta") => {
                        accumulator.record_output_text_delta(&value);
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
    }
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
            self.push_unique_item(item.clone());
        }
    }

    fn record_output_text_delta(&mut self, event: &Value) {
        if let Some(delta) = event.get("delta").and_then(Value::as_str) {
            self.text_deltas.push_str(delta);
        }
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
}

fn merge_streamed_response_output(response: &mut Value, accumulator: StreamAccumulator) {
    let Some(response_object) = response.as_object_mut() else {
        return;
    };

    let mut merged_output = response_object
        .get("output")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();

    for item in accumulator.output_items {
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

    if merged_output.is_empty() && !accumulator.text_deltas.is_empty() {
        merged_output.push(serde_json::json!({
            "type": "message",
            "role": "assistant",
            "content": [{
                "type": "output_text",
                "text": accumulator.text_deltas,
            }],
        }));
    }

    response_object.insert("output".to_string(), Value::Array(merged_output));
}

fn build_responses_input(messages: &[ChatMessage]) -> Result<Vec<Value>, ProviderError> {
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
            }
            ChatRole::Assistant => {
                append_codex_reasoning_items(&mut input, message);
                let content = assistant_responses_content(message);
                if !content.is_empty() {
                    input.push(json!({
                        "type": "message",
                        "role": "assistant",
                        "content": content,
                    }));
                }
                for item in &message.data {
                    if let ChatMessageItem::ToolCall(tool_call) = item {
                        input.push(json!({
                            "type": "function_call",
                            "name": tool_call.tool_name,
                            "arguments": tool_call.arguments.text,
                            "call_id": tool_call.tool_call_id,
                        }));
                    }
                }
                append_responses_tool_outputs(&mut input, message);
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
    match reasoning
        .codex_summary
        .as_deref()
        .filter(|summary| !summary.is_empty())
    {
        Some(summary) => Value::Array(vec![json!({
            "type": "summary_text",
            "text": summary,
        })]),
        None => Value::Array(Vec::new()),
    }
}

fn user_responses_content(message: &ChatMessage) -> Result<Vec<Value>, ProviderError> {
    let mut content = Vec::new();

    for item in &message.data {
        match item {
            ChatMessageItem::Reasoning(_) | ChatMessageItem::ToolResult(_) => {}
            ChatMessageItem::Context(context) => {
                content.push(json!({
                    "type": "input_text",
                    "text": context.text,
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

fn assistant_responses_content(message: &ChatMessage) -> Vec<Value> {
    let mut content = Vec::new();

    for item in &message.data {
        match item {
            ChatMessageItem::Context(context) => {
                content.push(json!({
                    "type": "output_text",
                    "text": context.text,
                }));
            }
            ChatMessageItem::File(file) => {
                content.push(json!({
                    "type": "output_text",
                    "text": file.uri,
                }));
            }
            ChatMessageItem::Reasoning(_)
            | ChatMessageItem::ToolCall(_)
            | ChatMessageItem::ToolResult(_) => {}
        }
    }

    content
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

    if encrypted_content.is_some() {
        return Some(ReasoningItem::codex(summary, encrypted_content, None));
    }

    summary
        .or_else(|| {
            item.get("text")
                .and_then(Value::as_str)
                .filter(|text| !text.is_empty())
                .map(str::to_string)
        })
        .map(ReasoningItem::from_text)
}

fn extract_reasoning_summary(item: &Value) -> Option<String> {
    item.get("summary")
        .and_then(Value::as_array)
        .and_then(|summary| {
            let parts = summary
                .iter()
                .filter_map(|part| {
                    part.get("text")
                        .and_then(Value::as_str)
                        .or_else(|| part.as_str())
                })
                .filter(|text| !text.is_empty())
                .map(str::to_string)
                .collect::<Vec<_>>();
            (!parts.is_empty()).then(|| parts.join("\n"))
        })
}

fn tool_result_text(tool_result: &crate::session_actor::ToolResultItem) -> String {
    let mut parts = Vec::new();

    if let Some(context) = &tool_result.result.context {
        parts.push(context.text.clone());
    }
    if let Some(file) = &tool_result.result.file {
        parts.push(file.uri.clone());
    }

    parts.join("\n")
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
    use crate::model_config::{ModelCapability, ProviderType, RetryMode, TokenEstimatorType};
    use crate::session_actor::{ToolBackend, ToolExecutionMode};
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
    fn merges_stream_delta_when_completed_response_has_no_output() {
        let mut response = serde_json::json!({"id": "resp_1", "output": []});
        let accumulator = StreamAccumulator {
            output_items: Vec::new(),
            text_deltas: "streamed text".to_string(),
        };

        merge_streamed_response_output(&mut response, accumulator);

        assert_eq!(response["output"][0]["content"][0]["text"], "streamed text");
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
    fn encrypted_reasoning_without_summary_is_not_exposed_as_text() {
        let item = serde_json::json!({
            "type": "reasoning",
            "encrypted_content": "opaque",
            "summary": []
        });

        let reasoning = extract_codex_reasoning(&item).expect("encrypted reasoning is retained");
        assert!(reasoning.text.is_empty());
        assert_eq!(reasoning.codex_summary, None);
        assert_eq!(reasoning.codex_encrypted_content.as_deref(), Some("opaque"));
    }

    #[test]
    fn codex_reasoning_round_trips_as_responses_reasoning_item() {
        let messages = vec![ChatMessage::new(
            ChatRole::Assistant,
            vec![
                ChatMessageItem::Reasoning(ReasoningItem::codex(
                    Some("checked repository state".to_string()),
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
        let input = build_responses_input(&normalized).expect("input should build");

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
                    Some("summary".to_string()),
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
        assert_eq!(reasoning.codex_summary.as_deref(), Some("summary"));
        assert_eq!(
            reasoning.codex_encrypted_content.as_deref(),
            Some("encrypted")
        );
    }

    #[test]
    fn codex_provider_filters_user_tell_tool() {
        let user_tell = ToolDefinition::new(
            "user_tell",
            "send progress",
            json!({"type": "object"}),
            ToolExecutionMode::Immediate,
            ToolBackend::ConversationBridge {
                action: "user_tell".to_string(),
            },
        );
        let file_read = ToolDefinition::new(
            "file_read",
            "read file",
            json!({"type": "object"}),
            ToolExecutionMode::Immediate,
            ToolBackend::Local,
        );
        let tools = vec![&user_tell, &file_read];

        let provider = CodexSubscriptionProvider::new();
        let filtered = provider.filter_tools_for_provider(&test_model_config(), tools);

        assert_eq!(
            filtered
                .iter()
                .map(|tool| tool.name.as_str())
                .collect::<Vec<_>>(),
            vec!["file_read"]
        );
    }

    fn fake_jwt(payload: &str) -> String {
        let header = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(br#"{"alg":"none"}"#);
        let payload = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(payload);
        format!("{header}.{payload}.sig")
    }

    fn temp_auth_json_path() -> PathBuf {
        std::env::temp_dir().join(format!("stellaclaw-auth-{}.json", nonce("test")))
    }

    fn test_model_config() -> ModelConfig {
        ModelConfig {
            provider_type: ProviderType::CodexSubscription,
            model_name: "gpt-5.5".to_string(),
            url: "https://chatgpt.com/backend-api/codex/responses".to_string(),
            api_key_env: "CHATGPT_ACCESS_TOKEN_TEST".to_string(),
            capabilities: vec![ModelCapability::Chat],
            token_max_context: 128_000,
            cache_timeout: 300,
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
}
