use std::{
    net::TcpStream,
    sync::Mutex,
    thread::sleep,
    time::{Duration, Instant},
};

use rand::Rng;
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
    },
};

use super::{
    common::{
        account_id_from_access_token, is_image_file, nonce, provider_error_message,
        token_usage_from_value,
    },
    OutputPersistor, Provider, ProviderError, ProviderRequest,
};

const OPENAI_BETA_RESPONSES_WEBSOCKETS: &str = "responses_websockets=2026-02-06";

pub struct CodexSubscriptionProvider {
    output_persistor: OutputPersistor,
    socket: Mutex<Option<WebSocket<MaybeTlsStream<TcpStream>>>>,
}

#[derive(Debug, Default)]
struct StreamAccumulator {
    output_items: Vec<Value>,
    text_deltas: String,
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
        let access_token = std::env::var(&model_config.api_key_env)
            .map_err(|_| ProviderError::MissingApiKeyEnv(model_config.api_key_env.clone()))?;
        let account_id = account_id_from_access_token(&access_token)
            .or_else(|| std::env::var("CHATGPT_ACCOUNT_ID").ok())
            .ok_or_else(|| {
                ProviderError::InvalidResponse(
                    "codex subscription account id is unavailable; set CHATGPT_ACCOUNT_ID or use a token containing chatgpt_account_id".to_string(),
                )
            })?;

        let mut payload = Map::new();
        payload.insert(
            "model".to_string(),
            Value::String(model_config.model_name.clone()),
        );
        payload.insert(
            "input".to_string(),
            Value::Array(build_responses_input(request.messages)),
        );
        payload.insert(
            "instructions".to_string(),
            Value::String(request.system_prompt.unwrap_or_default().to_string()),
        );
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
        payload.insert("store".to_string(), Value::Bool(false));
        payload.insert("stream".to_string(), Value::Bool(true));

        let mut socket = {
            let mut cached = self.socket.lock().expect("mutex poisoned");
            cached.take()
        };
        if socket.is_none() {
            socket = Some(connect_codex_websocket(
                model_config,
                &access_token,
                &account_id,
            )?);
        }

        let mut socket = socket.expect("socket should be initialized");
        let response = send_response_create(&mut socket, payload);
        if response.is_ok() {
            let mut cached = self.socket.lock().expect("mutex poisoned");
            *cached = Some(socket);
        }
        let response = response?;
        responses_value_to_chat_message(&response, &self.output_persistor)
    }

    fn should_retry(error: &ProviderError) -> bool {
        matches!(error, ProviderError::WebSocket(_))
    }
}

impl Default for CodexSubscriptionProvider {
    fn default() -> Self {
        Self {
            output_persistor: OutputPersistor,
            socket: Mutex::new(None),
        }
    }
}

impl Provider for CodexSubscriptionProvider {
    fn send(
        &self,
        model_config: &ModelConfig,
        request: ProviderRequest<'_>,
    ) -> Result<ChatMessage, ProviderError> {
        let started_at = Instant::now();

        loop {
            match self.send_once(model_config, &request) {
                Ok(response) => return Ok(response),
                Err(error) if Self::should_retry(&error) => match &model_config.retry_mode {
                    RetryMode::Once => return Err(error),
                    RetryMode::RandomInterval {
                        max_interval_secs,
                        max_time_secs,
                    } => {
                        if started_at.elapsed() >= Duration::from_secs(*max_time_secs) {
                            return Err(error);
                        }

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

fn connect_codex_websocket(
    model_config: &ModelConfig,
    access_token: &str,
    account_id: &str,
) -> Result<WebSocket<MaybeTlsStream<TcpStream>>, ProviderError> {
    let websocket_url = build_websocket_url(&model_config.url)?;
    let mut request = websocket_url
        .as_str()
        .into_client_request()
        .map_err(|error| ProviderError::WebSocket(error.to_string()))?;

    request.headers_mut().insert(
        "authorization",
        HeaderValue::from_str(&format!("Bearer {access_token}"))
            .map_err(|error| ProviderError::WebSocket(error.to_string()))?,
    );
    request.headers_mut().insert(
        "chatgpt-account-id",
        HeaderValue::from_str(account_id)
            .map_err(|error| ProviderError::WebSocket(error.to_string()))?,
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
        HeaderValue::from_str(&nonce("req"))
            .map_err(|error| ProviderError::WebSocket(error.to_string()))?,
    );
    request.headers_mut().insert(
        "session_id",
        HeaderValue::from_str(&nonce("session"))
            .map_err(|error| ProviderError::WebSocket(error.to_string()))?,
    );

    let (mut socket, _) =
        connect(request).map_err(|error| ProviderError::WebSocket(error.to_string()))?;
    set_socket_timeout(&mut socket, Duration::from_secs(model_config.conn_timeout))?;
    Ok(socket)
}

fn send_response_create(
    socket: &mut WebSocket<MaybeTlsStream<TcpStream>>,
    payload: Map<String, Value>,
) -> Result<Value, ProviderError> {
    let mut request = Map::new();
    request.insert(
        "type".to_string(),
        Value::String("response.create".to_string()),
    );
    request.extend(payload);

    socket
        .send(Message::Text(Value::Object(request).to_string().into()))
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

fn build_responses_input(messages: &[ChatMessage]) -> Vec<Value> {
    let mut input = Vec::new();

    for message in messages {
        match message.role {
            ChatRole::User => {
                let content = user_responses_content(message);
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

    input
}

fn responses_value_to_chat_message(
    value: &Value,
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
                if let Some(text) = extract_reasoning_text(item) {
                    data.push(ChatMessageItem::Reasoning(ReasoningItem { text }));
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
        token_usage: token_usage_from_value(value),
        data,
    })
}

fn user_responses_content(message: &ChatMessage) -> Vec<Value> {
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
            ChatMessageItem::File(file) => content.push(responses_file_item(file)),
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

    content
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

fn responses_file_item(file: &FileItem) -> Value {
    if is_image_file(file) {
        return json!({
            "type": "input_image",
            "image_url": file.uri,
        });
    }

    let mut payload = Map::new();
    payload.insert("type".to_string(), Value::String("input_file".to_string()));
    if let Some(name) = &file.name {
        payload.insert("filename".to_string(), Value::String(name.clone()));
    }
    if file.uri.starts_with("data:") {
        payload.insert("file_data".to_string(), Value::String(file.uri.clone()));
    } else {
        payload.insert("file_url".to_string(), Value::String(file.uri.clone()));
    }
    Value::Object(payload)
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

fn extract_reasoning_text(item: &Value) -> Option<String> {
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
        .or_else(|| {
            item.get("text")
                .and_then(Value::as_str)
                .filter(|text| !text.is_empty())
                .map(str::to_string)
        })
        .or_else(|| Some(item.to_string()))
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
    fn merges_stream_delta_when_completed_response_has_no_output() {
        let mut response = serde_json::json!({"id": "resp_1", "output": []});
        let accumulator = StreamAccumulator {
            output_items: Vec::new(),
            text_deltas: "streamed text".to_string(),
        };

        merge_streamed_response_output(&mut response, accumulator);

        assert_eq!(response["output"][0]["content"][0]["text"], "streamed text");
    }
}
