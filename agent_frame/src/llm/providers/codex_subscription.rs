use super::{UpstreamProvider, openrouter_responses::responses_reasoning_payload};
use crate::config::{ReasoningConfig, UpstreamApiKind, UpstreamConfig};
use crate::llm::{
    ChatCompletionOutcome, ChatCompletionSession, account_id_from_access_token,
    build_chat_completions_url, build_responses_input, build_responses_tools_payload,
    load_codex_auth, log_upstream_api_request_completed, log_upstream_api_request_failed,
    log_upstream_api_request_started, next_api_request_id, refresh_codex_auth,
    request_cache_log_fields, response_id_from_value, responses_value_to_chat_message,
};
use crate::message::ChatMessage;
use crate::tooling::Tool;
use anyhow::{Context, Result, anyhow};
use serde_json::{Map, Value};
use std::net::TcpStream;
use std::thread::sleep;
use std::time::{Duration, Instant};
use tungstenite::WebSocket;
use tungstenite::client::IntoClientRequest;
use tungstenite::http::{HeaderName, HeaderValue};
use tungstenite::stream::MaybeTlsStream;
use tungstenite::{Message, connect};
use url::Url;
use uuid::Uuid;

const OPENAI_BETA_RESPONSES_WEBSOCKETS: &str = "responses_websockets=2026-02-06";
const DEFAULT_STREAM_RECONNECT_ATTEMPTS: u32 = 5;
const INITIAL_STREAM_RETRY_DELAY_MS: u64 = 200;

pub(super) struct CodexSubscriptionProvider;

pub(crate) struct CodexSubscriptionSession {
    socket: WebSocket<MaybeTlsStream<TcpStream>>,
}

#[derive(Default)]
struct WebsocketResponseAccumulator {
    output_items: Vec<Value>,
    text_deltas: String,
}

impl UpstreamProvider for CodexSubscriptionProvider {
    fn start_session(&self, upstream: &UpstreamConfig) -> Result<Option<ChatCompletionSession>> {
        if upstream.api_kind != UpstreamApiKind::Responses {
            return Ok(None);
        }
        let socket = establish_codex_websocket(upstream)?;
        Ok(Some(ChatCompletionSession::CodexSubscription(
            CodexSubscriptionSession { socket },
        )))
    }

    fn create_completion(
        &self,
        upstream: &UpstreamConfig,
        messages: &[ChatMessage],
        tools: &[Tool],
        extra_payload: Option<Map<String, Value>>,
        session: Option<&mut ChatCompletionSession>,
    ) -> Result<ChatCompletionOutcome> {
        if upstream.api_kind != UpstreamApiKind::Responses {
            return Err(anyhow!(
                "codex-subscription currently only supports the responses api"
            ));
        }

        let payload =
            build_responses_request_payload(upstream, messages, tools, extra_payload, true)?;
        let request_cache = request_cache_log_fields(&Value::Object(payload.clone()));

        let (response, api_request_id) = match session {
            Some(ChatCompletionSession::CodexSubscription(session)) => {
                match send_response_create(upstream, &mut session.socket, payload.clone()) {
                    Ok(response) => response,
                    Err(error) if should_recover_codex_websocket_error(&error) => {
                        let (response, api_request_id, socket) =
                            send_responses_websocket_request_with_retries(upstream, payload)
                                .map_err(|recovery_error| {
                                    if let Ok(socket) = establish_codex_websocket(upstream) {
                                        session.socket = socket;
                                    }
                                    recovery_error.context(format!(
                                "existing codex websocket session failed and recovery request also failed; original stale-session error: {error:#}"
                            ))
                                })?;
                        session.socket = socket;
                        (response, api_request_id)
                    }
                    Err(error) => return Err(error),
                }
            }
            None => {
                let (response, api_request_id, _) =
                    send_responses_websocket_request_with_retries(upstream, payload)?;
                (response, api_request_id)
            }
        };

        let usage = crate::llm::parse_usage(&response);
        let message = responses_value_to_chat_message(&response)?;
        Ok(ChatCompletionOutcome {
            message,
            usage,
            response_id: response_id_from_value(&response),
            api_request_id: Some(api_request_id),
            request_cache,
        })
    }
}

fn build_responses_request_payload(
    upstream: &UpstreamConfig,
    messages: &[ChatMessage],
    tools: &[Tool],
    extra_payload: Option<Map<String, Value>>,
    include_tools: bool,
) -> Result<Map<String, Value>> {
    let (instructions, input) = build_responses_input(messages)?;
    let mut payload = Map::new();
    payload.insert("model".to_string(), Value::String(upstream.model.clone()));
    payload.insert("input".to_string(), Value::Array(input));
    payload.insert(
        "instructions".to_string(),
        Value::String(instructions.unwrap_or_default()),
    );
    payload.insert("store".to_string(), Value::Bool(false));
    payload.insert("stream".to_string(), Value::Bool(true));
    if let Some(reasoning) = codex_reasoning_payload(upstream.reasoning.as_ref())? {
        payload.insert("reasoning".to_string(), reasoning);
    }
    let response_tools = build_responses_tools_payload(upstream, tools);
    if include_tools && !response_tools.is_empty() {
        payload.insert("tools".to_string(), Value::Array(response_tools));
        payload.insert("parallel_tool_calls".to_string(), Value::Bool(true));
        payload.insert("tool_choice".to_string(), Value::String("auto".to_string()));
    }
    if let Some(extra_payload) = extra_payload {
        for (key, value) in extra_payload {
            payload.insert(key, value);
        }
    }
    payload.remove("max_completion_tokens");
    Ok(payload)
}

fn retry_after_auth_failure(
    upstream: &UpstreamConfig,
    payload: Map<String, Value>,
    auth: Option<&crate::config::CodexAuthConfig>,
    original_error: anyhow::Error,
) -> Result<(Value, String, WebSocket<MaybeTlsStream<TcpStream>>)> {
    if !is_probable_auth_error(&original_error) {
        return Err(original_error);
    }

    if upstream.codex_auth.is_none()
        && let Some(reloaded) = load_codex_auth(upstream)?
        && auth
            .map(|current| current.access_token != reloaded.access_token)
            .unwrap_or(true)
    {
        match send_responses_websocket_request(upstream, payload.clone(), Some(&reloaded)) {
            Ok(response) => return Ok(response),
            Err(reloaded_error) if !is_probable_auth_error(&reloaded_error) => {
                return Err(reloaded_error.context(format!(
                    "previous codex websocket auth error: {original_error:#}"
                )));
            }
            Err(_) => {}
        }
    }

    if let Some(auth) = auth
        && let Some(refreshed) = refresh_codex_auth(upstream, auth)?
    {
        return send_responses_websocket_request(upstream, payload, Some(&refreshed))
            .with_context(|| format!("previous codex websocket auth error: {original_error:#}"));
    }

    Err(original_error)
}

fn is_probable_auth_error(error: &anyhow::Error) -> bool {
    let text = format!("{error:#}").to_ascii_lowercase();
    text.contains("401")
        || text.contains("unauthorized")
        || text.contains("invalid api key")
        || text.contains("invalid_token")
        || text.contains("expired")
        || text.contains("authentication")
}

fn should_recover_codex_websocket_error(error: &anyhow::Error) -> bool {
    is_probable_auth_error(error) || is_probable_retryable_websocket_error(error)
}

fn is_probable_retryable_websocket_error(error: &anyhow::Error) -> bool {
    let text = format!("{error:#}").to_ascii_lowercase();
    if text.contains("timed out") || text.contains("would block") {
        return false;
    }
    text.contains("websocket_connection_limit_reached")
        || text.contains("responses websocket connection limit reached")
        || text.contains("failed to establish codex websocket")
        || text.contains("failed to send codex websocket request")
        || text.contains("failed to read codex websocket event")
        || text.contains("failed to send codex websocket pong")
        || text.contains("codex websocket closed before response.completed")
        || text.contains("broken pipe")
        || text.contains("connection reset")
        || text.contains("unexpected eof")
        || text.contains("tls handshake eof")
        || text.contains("already closed")
        || text.contains("connection closed")
}

fn stream_retry_backoff(attempt: u32) -> Duration {
    let exponent = attempt.saturating_sub(1).min(20);
    Duration::from_millis(INITIAL_STREAM_RETRY_DELAY_MS.saturating_mul(1_u64 << exponent))
}

fn send_responses_websocket_request(
    upstream: &UpstreamConfig,
    payload: Map<String, Value>,
    auth: Option<&crate::config::CodexAuthConfig>,
) -> Result<(Value, String, WebSocket<MaybeTlsStream<TcpStream>>)> {
    let mut socket = connect_codex_websocket(upstream, auth)?;
    let (response, api_request_id) = send_response_create(upstream, &mut socket, payload)?;
    Ok((response, api_request_id, socket))
}

fn send_responses_websocket_request_once(
    upstream: &UpstreamConfig,
    payload: Map<String, Value>,
) -> Result<(Value, String, WebSocket<MaybeTlsStream<TcpStream>>)> {
    let auth = load_codex_auth(upstream)?;
    match send_responses_websocket_request(upstream, payload.clone(), auth.as_ref()) {
        Ok(response) => Ok(response),
        Err(error) => retry_after_auth_failure(upstream, payload, auth.as_ref(), error),
    }
}

fn send_responses_websocket_request_with_retries(
    upstream: &UpstreamConfig,
    payload: Map<String, Value>,
) -> Result<(Value, String, WebSocket<MaybeTlsStream<TcpStream>>)> {
    let mut last_error = None;

    for attempt in 0..=DEFAULT_STREAM_RECONNECT_ATTEMPTS {
        if attempt > 0 {
            sleep(stream_retry_backoff(attempt));
        }

        match send_responses_websocket_request_once(upstream, payload.clone()) {
            Ok(response) => return Ok(response),
            Err(error) if is_probable_retryable_websocket_error(&error) => {
                last_error = Some(error);
            }
            Err(error) => return Err(error),
        }
    }

    Err(last_error.unwrap_or_else(|| anyhow!("codex websocket retries exhausted")))
}

fn establish_codex_websocket(
    upstream: &UpstreamConfig,
) -> Result<WebSocket<MaybeTlsStream<TcpStream>>> {
    let auth = load_codex_auth(upstream)?;
    match connect_codex_websocket(upstream, auth.as_ref()) {
        Ok(socket) => Ok(socket),
        Err(error) => retry_after_auth_failure_connect(upstream, auth.as_ref(), error),
    }
}

fn retry_after_auth_failure_connect(
    upstream: &UpstreamConfig,
    auth: Option<&crate::config::CodexAuthConfig>,
    original_error: anyhow::Error,
) -> Result<WebSocket<MaybeTlsStream<TcpStream>>> {
    if !is_probable_auth_error(&original_error) {
        return Err(original_error);
    }

    if upstream.codex_auth.is_none()
        && let Some(reloaded) = load_codex_auth(upstream)?
        && auth
            .map(|current| current.access_token != reloaded.access_token)
            .unwrap_or(true)
    {
        match connect_codex_websocket(upstream, Some(&reloaded)) {
            Ok(socket) => return Ok(socket),
            Err(reloaded_error) if !is_probable_auth_error(&reloaded_error) => {
                return Err(reloaded_error.context(format!(
                    "previous codex websocket auth error: {original_error:#}"
                )));
            }
            Err(_) => {}
        }
    }

    if let Some(auth) = auth
        && let Some(refreshed) = refresh_codex_auth(upstream, auth)?
    {
        return connect_codex_websocket(upstream, Some(&refreshed))
            .with_context(|| format!("previous codex websocket auth error: {original_error:#}"));
    }

    Err(original_error)
}

fn connect_codex_websocket(
    upstream: &UpstreamConfig,
    auth: Option<&crate::config::CodexAuthConfig>,
) -> Result<WebSocket<MaybeTlsStream<TcpStream>>> {
    let websocket_url = build_websocket_url(&build_chat_completions_url(upstream))?;
    let mut request = websocket_url
        .as_str()
        .into_client_request()
        .context("failed to build websocket request")?;
    let auth = auth.ok_or_else(|| anyhow!("codex auth is unavailable"))?;
    let account_id = auth
        .account_id
        .clone()
        .or_else(|| account_id_from_access_token(&auth.access_token))
        .ok_or_else(|| anyhow!("codex auth token is missing chatgpt account id"))?;
    request.headers_mut().insert(
        "authorization",
        HeaderValue::from_str(&format!("Bearer {}", auth.access_token))
            .context("failed to encode authorization header")?,
    );
    request.headers_mut().insert(
        "chatgpt-account-id",
        HeaderValue::from_str(&account_id).context("failed to encode chatgpt-account-id")?,
    );
    request.headers_mut().insert(
        "openai-beta",
        HeaderValue::from_static(OPENAI_BETA_RESPONSES_WEBSOCKETS),
    );
    request
        .headers_mut()
        .insert("user-agent", HeaderValue::from_static("codex-cli"));
    request.headers_mut().insert(
        "x-client-request-id",
        HeaderValue::from_str(&Uuid::new_v4().to_string())
            .context("failed to encode x-client-request-id")?,
    );
    request.headers_mut().insert(
        "session_id",
        HeaderValue::from_str(&Uuid::new_v4().to_string())
            .context("failed to encode session_id")?,
    );
    for (key, value) in &upstream.headers {
        if let Some(value) = value.as_str() {
            let header_name = HeaderName::from_bytes(key.as_bytes())
                .context("failed to parse upstream websocket header name")?;
            request.headers_mut().insert(
                header_name,
                HeaderValue::from_str(value)
                    .context("failed to encode upstream websocket header value")?,
            );
        }
    }

    let (mut socket, _) = connect(request).context("failed to establish codex websocket")?;
    set_codex_socket_timeout(&mut socket, codex_socket_timeout(upstream))?;
    Ok(socket)
}

fn codex_socket_timeout(upstream: &UpstreamConfig) -> Option<Duration> {
    (upstream.timeout_seconds > 0.0).then(|| Duration::from_secs_f64(upstream.timeout_seconds))
}

fn set_codex_socket_timeout(
    socket: &mut WebSocket<MaybeTlsStream<TcpStream>>,
    timeout: Option<Duration>,
) -> Result<()> {
    match socket.get_mut() {
        MaybeTlsStream::Plain(stream) => set_tcp_stream_timeout(stream, timeout),
        MaybeTlsStream::Rustls(stream) => set_tcp_stream_timeout(&stream.sock, timeout),
        _ => Ok(()),
    }
}

fn set_tcp_stream_timeout(stream: &TcpStream, timeout: Option<Duration>) -> Result<()> {
    stream
        .set_read_timeout(timeout)
        .context("failed to set codex websocket read timeout")?;
    stream
        .set_write_timeout(timeout)
        .context("failed to set codex websocket write timeout")?;
    Ok(())
}

fn send_response_create(
    upstream: &UpstreamConfig,
    socket: &mut WebSocket<MaybeTlsStream<TcpStream>>,
    payload: Map<String, Value>,
) -> Result<(Value, String)> {
    let create_request = Value::Object(Map::from_iter([(
        "type".to_string(),
        Value::String("response.create".to_string()),
    )]))
    .as_object()
    .cloned()
    .unwrap()
    .into_iter()
    .chain(payload)
    .collect::<Map<_, _>>();
    let request_body = Value::Object(create_request);
    let request_cache = request_cache_log_fields(&request_body);
    let api_request_id = next_api_request_id();
    let websocket_url = build_websocket_url(&build_chat_completions_url(upstream))
        .map(|url| url.to_string())
        .unwrap_or_else(|_| build_chat_completions_url(upstream));
    log_upstream_api_request_started(
        &api_request_id,
        upstream,
        "codex_subscription_responses_websocket",
        "WEBSOCKET",
        &websocket_url,
        "{}",
        &request_body,
        &request_cache,
    );

    let started = Instant::now();
    if let Err(error) = socket.send(Message::Text(request_body.to_string().into())) {
        log_upstream_api_request_failed(
            &api_request_id,
            upstream,
            "codex_subscription_responses_websocket",
            None,
            started.elapsed().as_millis() as u64,
            "{}",
            None,
            &format!("{error:#}"),
            &request_cache,
        );
        return Err(error).context("failed to send codex websocket request");
    }

    let mut accumulator = WebsocketResponseAccumulator::default();

    loop {
        let message = match socket.read() {
            Ok(message) => message,
            Err(error) => {
                log_upstream_api_request_failed(
                    &api_request_id,
                    upstream,
                    "codex_subscription_responses_websocket",
                    None,
                    started.elapsed().as_millis() as u64,
                    "{}",
                    None,
                    &format!("{error:#}"),
                    &request_cache,
                );
                return Err(error).context("failed to read codex websocket event");
            }
        };
        match message {
            Message::Text(text) => {
                let value: Value = match serde_json::from_str(&text) {
                    Ok(value) => value,
                    Err(error) => {
                        let response_body = Value::String(text.to_string());
                        log_upstream_api_request_failed(
                            &api_request_id,
                            upstream,
                            "codex_subscription_responses_websocket",
                            None,
                            started.elapsed().as_millis() as u64,
                            "{}",
                            Some(&response_body),
                            &format!("{error:#}"),
                            &request_cache,
                        );
                        return Err(error).context("failed to parse codex websocket event");
                    }
                };
                match value.get("type").and_then(Value::as_str) {
                    Some("response.completed") => {
                        let mut response = value
                            .get("response")
                            .cloned()
                            .ok_or_else(|| anyhow!("codex websocket completed without response"))?;
                        merge_streamed_response_output(&mut response, accumulator);
                        let usage = crate::llm::parse_usage(&response);
                        let response_id = response_id_from_value(&response);
                        log_upstream_api_request_completed(
                            &api_request_id,
                            upstream,
                            "codex_subscription_responses_websocket",
                            200,
                            started.elapsed().as_millis() as u64,
                            "{}",
                            &response,
                            &usage,
                            response_id.as_deref(),
                            &request_cache,
                        );
                        return Ok((response, api_request_id));
                    }
                    Some("response.failed") | Some("error") => {
                        let error_message = crate::llm::upstream_error_from_value(&value)
                            .unwrap_or_else(|| value.to_string());
                        log_upstream_api_request_failed(
                            &api_request_id,
                            upstream,
                            "codex_subscription_responses_websocket",
                            None,
                            started.elapsed().as_millis() as u64,
                            "{}",
                            Some(&value),
                            &error_message,
                            &request_cache,
                        );
                        return Err(anyhow!("codex websocket request failed: {}", error_message));
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
            Message::Ping(payload) => {
                socket
                    .send(Message::Pong(payload))
                    .context("failed to send codex websocket pong")?;
            }
            Message::Close(frame) => {
                let error = format!(
                    "codex websocket closed before response.completed: {}",
                    frame
                        .map(|value| value.reason.to_string())
                        .unwrap_or_else(|| "connection closed".to_string())
                );
                log_upstream_api_request_failed(
                    &api_request_id,
                    upstream,
                    "codex_subscription_responses_websocket",
                    None,
                    started.elapsed().as_millis() as u64,
                    "{}",
                    None,
                    &error,
                    &request_cache,
                );
                return Err(anyhow!("{error}"));
            }
            _ => {}
        }
    }
}

impl WebsocketResponseAccumulator {
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

fn merge_streamed_response_output(response: &mut Value, accumulator: WebsocketResponseAccumulator) {
    let Some(response_object) = response.as_object_mut() else {
        return;
    };

    let existing_output = response_object
        .get("output")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let has_existing_output = !existing_output.is_empty();

    let mut merged_output = existing_output;
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

    if !has_existing_output || !merged_output.is_empty() {
        response_object.insert("output".to_string(), Value::Array(merged_output));
    }
}

fn build_websocket_url(http_url: &str) -> Result<Url> {
    let mut url = Url::parse(http_url).context("failed to parse codex websocket url")?;
    match url.scheme() {
        "https" => {
            url.set_scheme("wss")
                .map_err(|_| anyhow!("failed to convert https base url to wss"))?;
        }
        "http" => {
            url.set_scheme("ws")
                .map_err(|_| anyhow!("failed to convert http base url to ws"))?;
        }
        "wss" | "ws" => {}
        other => return Err(anyhow!("unsupported codex websocket scheme '{}'", other)),
    }
    Ok(url)
}

pub(crate) fn codex_reasoning_payload(
    reasoning: Option<&ReasoningConfig>,
) -> Result<Option<Value>> {
    let Some(mut payload) = responses_reasoning_payload(reasoning)? else {
        return Ok(None);
    };
    if let Some(object) = payload.as_object_mut() {
        object.remove("max_tokens");
        object.remove("exclude");
        object.remove("enabled");
    }
    Ok(Some(payload))
}

#[cfg(test)]
mod tests {
    use super::{
        WebsocketResponseAccumulator, build_responses_request_payload,
        is_probable_retryable_websocket_error, merge_streamed_response_output,
        stream_retry_backoff,
    };
    use crate::config::{
        AuthCredentialsStoreMode, UpstreamApiKind, UpstreamAuthKind, UpstreamConfig,
    };
    use crate::tooling::Tool;
    use anyhow::anyhow;
    use serde_json::{Value, json};
    use std::time::Duration;

    fn test_upstream() -> UpstreamConfig {
        UpstreamConfig {
            base_url: "https://chatgpt.com/backend-api/codex".to_string(),
            model: "gpt-5.4-mini".to_string(),
            api_kind: UpstreamApiKind::Responses,
            auth_kind: UpstreamAuthKind::CodexSubscription,
            supports_vision_input: true,
            supports_pdf_input: false,
            supports_audio_input: false,
            api_key: None,
            api_key_env: "OPENAI_API_KEY".to_string(),
            chat_completions_path: "/responses".to_string(),
            codex_home: None,
            codex_auth: None,
            auth_credentials_store_mode: AuthCredentialsStoreMode::Auto,
            timeout_seconds: 30.0,
            retry_mode: Default::default(),
            context_window_tokens: 128_000,
            cache_control: None,
            prompt_cache_retention: None,
            prompt_cache_key: None,
            reasoning: None,
            headers: Default::default(),
            native_web_search: None,
            external_web_search: None,
            native_image_input: false,
            native_pdf_input: false,
            native_audio_input: false,
            native_image_generation: false,
            token_estimation: None,
        }
    }

    #[test]
    fn merge_streamed_response_output_backfills_empty_completed_output() {
        let mut response = json!({
            "id": "resp_1",
            "output": []
        });
        let mut accumulator = WebsocketResponseAccumulator::default();
        accumulator.record_output_item_done(&json!({
            "type": "response.output_item.done",
            "item": {
                "id": "msg_1",
                "type": "message",
                "role": "assistant",
                "content": [{
                    "type": "output_text",
                    "text": "你好"
                }]
            }
        }));

        merge_streamed_response_output(&mut response, accumulator);

        assert_eq!(response["output"][0]["id"], "msg_1");
        assert_eq!(response["output"][0]["content"][0]["text"], "你好");
    }

    #[test]
    fn merge_streamed_response_output_falls_back_to_text_deltas() {
        let mut response = json!({
            "id": "resp_1",
            "output": []
        });
        let mut accumulator = WebsocketResponseAccumulator::default();
        accumulator.record_output_text_delta(&json!({
            "type": "response.output_text.delta",
            "delta": "你"
        }));
        accumulator.record_output_text_delta(&json!({
            "type": "response.output_text.delta",
            "delta": "好"
        }));

        merge_streamed_response_output(&mut response, accumulator);

        assert_eq!(response["output"][0]["type"], "message");
        assert_eq!(response["output"][0]["content"][0]["text"], "你好");
    }

    #[test]
    fn merge_streamed_response_output_appends_missing_items_without_dropping_existing() {
        let mut response = json!({
            "id": "resp_1",
            "output": [{
                "id": "fc_1",
                "type": "function_call",
                "call_id": "call_1",
                "name": "web_search",
                "arguments": "{}"
            }]
        });
        let mut accumulator = WebsocketResponseAccumulator::default();
        accumulator.record_output_item_done(&json!({
            "type": "response.output_item.done",
            "item": {
                "id": "msg_1",
                "type": "message",
                "role": "assistant",
                "content": [{
                    "type": "output_text",
                    "text": "done"
                }]
            }
        }));

        merge_streamed_response_output(&mut response, accumulator);

        assert_eq!(response["output"].as_array().unwrap().len(), 2);
        assert_eq!(response["output"][0]["id"], "fc_1");
        assert_eq!(response["output"][1]["id"], "msg_1");
    }

    #[test]
    fn request_payload_includes_tools_when_requested() {
        let payload = build_responses_request_payload(
            &test_upstream(),
            &[crate::message::ChatMessage::text("user", "hello")],
            &[Tool::new(
                "web_search",
                "search docs",
                json!({
                    "type": "object",
                    "properties": {
                        "query": { "type": "string" }
                    },
                    "required": ["query"]
                }),
                |_| Ok(json!({"ok": true})),
            )],
            None,
            true,
        )
        .expect("payload should build");

        assert!(payload.get("tools").is_some());
        assert_eq!(payload["parallel_tool_calls"], Value::Bool(true));
        assert_eq!(payload["tool_choice"], Value::String("auto".to_string()));
    }

    #[test]
    fn request_payload_omits_unsupported_prompt_cache_fields() {
        let mut upstream = test_upstream();
        upstream.prompt_cache_key = Some("session-key".to_string());
        upstream.prompt_cache_retention = Some("24h".to_string());

        let payload = build_responses_request_payload(
            &upstream,
            &[crate::message::ChatMessage::text("user", "hello")],
            &[],
            None,
            true,
        )
        .expect("payload should build");

        assert!(payload.get("prompt_cache_key").is_none());
        assert!(payload.get("prompt_cache_retention").is_none());
    }

    #[test]
    fn retryable_websocket_error_detection_matches_disconnect_signals() {
        assert!(is_probable_retryable_websocket_error(&anyhow!(
            "failed to send codex websocket pong: Broken pipe (os error 32)"
        )));
        assert!(is_probable_retryable_websocket_error(&anyhow!(
            "codex websocket closed before response.completed: connection closed"
        )));
        assert!(is_probable_retryable_websocket_error(&anyhow!(
            "codex websocket request failed: websocket_connection_limit_reached"
        )));
        assert!(!is_probable_retryable_websocket_error(&anyhow!(
            "codex websocket request failed: invalid schema"
        )));
        assert!(!is_probable_retryable_websocket_error(&anyhow!(
            "failed to read codex websocket event: IO error: timed out"
        )));
    }

    #[test]
    fn stream_retry_backoff_doubles_from_two_hundred_milliseconds() {
        assert_eq!(stream_retry_backoff(1), Duration::from_millis(200));
        assert_eq!(stream_retry_backoff(2), Duration::from_millis(400));
        assert_eq!(stream_retry_backoff(3), Duration::from_millis(800));
        assert_eq!(stream_retry_backoff(4), Duration::from_millis(1600));
    }
}
