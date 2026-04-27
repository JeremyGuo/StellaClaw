use std::{collections::HashMap, sync::Mutex, time::Duration};

use rand::Rng;
use reqwest::{blocking::Client, StatusCode};
use serde_json::{json, Map, Value};

use crate::{
    model_config::{ModelConfig, RetryMode},
    session_actor::{
        ChatMessage, ChatMessageItem, ChatRole, ContextItem, FileItem, ReasoningItem, ToolCallItem,
    },
};

use super::{
    common::{
        claude_cache_control_payload, data_url_parts, is_image_file, provider_error_message,
        serialize_json_request_body, token_usage_from_value,
    },
    ProviderBackend, ProviderError, ProviderRequest,
};

#[derive(Debug, Default)]
pub struct ClaudeCodeProvider {
    clients_by_timeout: Mutex<HashMap<(u64, u64), Client>>,
}

impl ClaudeCodeProvider {
    pub fn new() -> Self {
        Self::default()
    }

    fn client_for_timeout(
        &self,
        connect_timeout_secs: u64,
        request_timeout_secs: u64,
    ) -> Result<Client, ProviderError> {
        let mut clients = self.clients_by_timeout.lock().expect("mutex poisoned");
        let key = (connect_timeout_secs, request_timeout_secs);

        if let Some(client) = clients.get(&key) {
            return Ok(client.clone());
        }

        let client = Client::builder()
            .connect_timeout(Duration::from_secs(connect_timeout_secs))
            .timeout(Duration::from_secs(request_timeout_secs))
            .build()
            .map_err(ProviderError::BuildHttpClient)?;

        clients.insert(key, client.clone());
        Ok(client)
    }

    fn send_once(
        &self,
        model_config: &ModelConfig,
        request: &ProviderRequest<'_>,
    ) -> Result<ChatMessage, ProviderError> {
        let api_key = std::env::var(&model_config.api_key_env)
            .map_err(|_| ProviderError::MissingApiKeyEnv(model_config.api_key_env.clone()))?;
        let cache_control = claude_cache_control_payload(model_config);

        let mut payload = Map::new();
        payload.insert(
            "model".to_string(),
            Value::String(model_config.model_name.clone()),
        );
        let (messages, message_cache_control_applied) =
            build_claude_messages(request.messages, cache_control.as_ref());
        payload.insert("messages".to_string(), Value::Array(messages));
        if let Some(system_prompt) = request.system_prompt {
            if !system_prompt.trim().is_empty() {
                let system_cache_control = if message_cache_control_applied {
                    None
                } else {
                    cache_control.as_ref()
                };
                let system_value =
                    claude_system_with_optional_cache_control(system_prompt, system_cache_control);
                payload.insert("system".to_string(), system_value);
            }
        }
        if !request.tools.is_empty() {
            payload.insert(
                "tools".to_string(),
                Value::Array(
                    request
                        .tools
                        .iter()
                        .map(|tool| tool.claude_tool_schema())
                        .collect(),
                ),
            );
        }
        payload.insert("max_tokens".to_string(), json!(4096));
        let payload = Value::Object(payload);

        let body = serialize_json_request_body(model_config, "claude_code", &payload)?;
        let client = self.client_for_timeout(
            model_config.conn_timeout_secs(),
            model_config.request_timeout_secs(),
        )?;
        let response = client
            .post(&model_config.url)
            .header("Content-Type", "application/json")
            .header("x-api-key", api_key)
            .header("anthropic-version", "2023-06-01")
            .body(body)
            .send()
            .map_err(ProviderError::request)?;

        let status = response.status();
        let body = response.text().map_err(ProviderError::DecodeResponse)?;

        if !status.is_success() {
            return Err(ProviderError::HttpStatus {
                url: model_config.url.clone(),
                status: status.as_u16(),
                body,
            });
        }

        let value = serde_json::from_str::<Value>(&body).map_err(ProviderError::DecodeJson)?;
        if let Some(error) = provider_error_message(&value) {
            return Err(ProviderError::InvalidResponse(error));
        }

        claude_value_to_chat_message(&value, model_config)
    }

    fn should_retry(error: &ProviderError) -> bool {
        match error {
            ProviderError::Request(_) => true,
            ProviderError::HttpStatus { status, .. } => {
                *status == StatusCode::TOO_MANY_REQUESTS.as_u16() || *status >= 500
            }
            _ => false,
        }
    }
}

impl ProviderBackend for ClaudeCodeProvider {
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
                        std::thread::sleep(Duration::from_secs(sleep_secs));
                    }
                },
                Err(error) => return Err(error),
            }
        }
    }
}

fn build_claude_messages(
    messages: &[ChatMessage],
    cache_control: Option<&Value>,
) -> (Vec<Value>, bool) {
    let mut converted = Vec::new();

    for message in messages {
        let mut content = claude_content_blocks(message);

        if matches!(message.role, ChatRole::Assistant) {
            for item in &message.data {
                if let ChatMessageItem::ToolCall(tool_call) = item {
                    content.push(json!({
                        "type": "tool_use",
                        "id": tool_call.tool_call_id,
                        "name": tool_call.tool_name,
                        "input": parse_tool_arguments(&tool_call.arguments.text),
                    }));
                }
            }
        }

        if !content.is_empty() {
            converted.push(json!({
                "role": role_as_str(&message.role),
                "content": content,
            }));
        }

        let tool_results = claude_tool_result_blocks(message);
        if !tool_results.is_empty() {
            converted.push(json!({
                "role": "user",
                "content": tool_results,
            }));
        }
    }

    let cache_control_applied = cache_control
        .map(|cache_control| add_cache_control_to_last_claude_block(&mut converted, cache_control))
        .unwrap_or(false);

    (converted, cache_control_applied)
}

fn add_cache_control_to_last_claude_block(messages: &mut [Value], cache_control: &Value) -> bool {
    for message in messages.iter_mut().rev() {
        let Some(content) = message.get_mut("content").and_then(Value::as_array_mut) else {
            continue;
        };
        let Some(last_block) = content.last_mut().and_then(Value::as_object_mut) else {
            continue;
        };
        last_block.insert("cache_control".to_string(), cache_control.clone());
        return true;
    }
    false
}

fn claude_system_with_optional_cache_control(
    system_prompt: &str,
    cache_control: Option<&Value>,
) -> Value {
    let mut block = Map::new();
    block.insert("type".to_string(), Value::String("text".to_string()));
    block.insert("text".to_string(), Value::String(system_prompt.to_string()));
    if let Some(cache_control) = cache_control {
        block.insert("cache_control".to_string(), cache_control.clone());
    }
    Value::Array(vec![Value::Object(block)])
}

fn claude_value_to_chat_message(
    value: &Value,
    model_config: &ModelConfig,
) -> Result<ChatMessage, ProviderError> {
    if let Some(error) = provider_error_message(value) {
        return Err(ProviderError::InvalidResponse(error));
    }

    let content = value
        .get("content")
        .and_then(Value::as_array)
        .ok_or_else(|| ProviderError::InvalidResponse("missing content array".to_string()))?;

    let mut data = Vec::new();

    for item in content {
        match item.get("type").and_then(Value::as_str) {
            Some("text") => {
                if let Some(text) = item.get("text").and_then(Value::as_str) {
                    if !text.is_empty() {
                        data.push(ChatMessageItem::Context(ContextItem {
                            text: text.to_string(),
                        }));
                    }
                }
            }
            Some("thinking") | Some("reasoning") => {
                if let Some(text) = item
                    .get("thinking")
                    .or_else(|| item.get("text"))
                    .and_then(Value::as_str)
                {
                    if !text.is_empty() {
                        data.push(ChatMessageItem::Reasoning(ReasoningItem::from_text(text)));
                    }
                }
            }
            Some("tool_use") => {
                let call_id = item.get("id").and_then(Value::as_str).ok_or_else(|| {
                    ProviderError::InvalidResponse("claude tool_use missing id".to_string())
                })?;
                let name = item.get("name").and_then(Value::as_str).ok_or_else(|| {
                    ProviderError::InvalidResponse("claude tool_use missing name".to_string())
                })?;
                let arguments = item
                    .get("input")
                    .map(value_to_arguments_string)
                    .unwrap_or_else(|| "{}".to_string());
                data.push(ChatMessageItem::ToolCall(ToolCallItem {
                    tool_call_id: call_id.to_string(),
                    tool_name: name.to_string(),
                    arguments: ContextItem { text: arguments },
                }));
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

fn claude_content_blocks(message: &ChatMessage) -> Vec<Value> {
    let mut blocks = Vec::new();

    for item in &message.data {
        match item {
            ChatMessageItem::Reasoning(_)
            | ChatMessageItem::ToolCall(_)
            | ChatMessageItem::ToolResult(_) => {}
            ChatMessageItem::Context(context) => {
                blocks.push(json!({
                    "type": "text",
                    "text": context.text,
                }));
            }
            ChatMessageItem::File(file)
                if matches!(message.role, ChatRole::User) && is_image_file(file) =>
            {
                blocks.push(claude_image_block(file));
            }
            ChatMessageItem::File(file) => {
                blocks.push(json!({
                    "type": "text",
                    "text": file.uri,
                }));
            }
        }
    }

    blocks
}

fn claude_image_block(file: &FileItem) -> Value {
    if let Some((media_type, data)) = data_url_parts(&file.uri) {
        json!({
            "type": "image",
            "source": {
                "type": "base64",
                "media_type": media_type,
                "data": data,
            }
        })
    } else {
        json!({
            "type": "image",
            "source": {
                "type": "url",
                "url": file.uri,
            }
        })
    }
}

fn claude_tool_result_blocks(message: &ChatMessage) -> Vec<Value> {
    message
        .data
        .iter()
        .filter_map(|item| {
            if let ChatMessageItem::ToolResult(tool_result) = item {
                Some(json!({
                    "type": "tool_result",
                    "tool_use_id": tool_result.tool_call_id,
                    "content": tool_result_text(tool_result),
                }))
            } else {
                None
            }
        })
        .collect()
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

fn parse_tool_arguments(arguments: &str) -> Value {
    serde_json::from_str(arguments).unwrap_or_else(|_| Value::String(arguments.to_string()))
}

fn value_to_arguments_string(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        other => other.to_string(),
    }
}

fn role_as_str(role: &ChatRole) -> &'static str {
    match role {
        ChatRole::User => "user",
        ChatRole::Assistant => "assistant",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        model_config::{ModelCapability, ProviderType, TokenEstimatorType},
        session_actor::{ChatMessageItem, ChatRole, ContextItem},
    };

    fn test_model_config(url: String) -> ModelConfig {
        ModelConfig {
            provider_type: ProviderType::ClaudeCode,
            model_name: "claude-sonnet-4-5".to_string(),
            url,
            api_key_env: "CLAUDE_CODE_API_KEY_TEST".to_string(),
            capabilities: vec![ModelCapability::Chat],
            token_max_context: 200_000,
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

    #[test]
    fn sends_claude_messages_request_and_parses_tool_use() {
        let mut server = mockito::Server::new();
        let mock = server
            .mock("POST", "/v1/messages")
            .match_header("x-api-key", "test-key")
            .match_header("anthropic-version", "2023-06-01")
            .match_body(mockito::Matcher::PartialJson(serde_json::json!({
                "messages": [
                    {
                        "role": "user",
                        "content": [
                            {
                                "type": "text",
                                "text": "hello",
                                "cache_control": {
                                    "type": "ephemeral",
                                    "ttl": "5m"
                                }
                            }
                        ]
                    }
                ]
            })))
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(
                r#"{
                    "id": "msg_123",
                    "type": "message",
                    "role": "assistant",
                    "content": [
                        {"type": "text", "text": "need the file"},
                        {"type": "tool_use", "id": "toolu_1", "name": "file_read", "input": {"path": "README.md"}}
                    ],
                    "usage": {
                        "input_tokens": 10,
                        "cache_read_input_tokens": 3,
                        "output_tokens": 5
                    }
                }"#,
            )
            .create();

        std::env::set_var("CLAUDE_CODE_API_KEY_TEST", "test-key");

        let provider = ClaudeCodeProvider::new();
        let messages = vec![ChatMessage::new(
            ChatRole::User,
            vec![ChatMessageItem::Context(ContextItem {
                text: "hello".to_string(),
            })],
        )];
        let response = provider
            .send(
                &test_model_config(format!("{}/v1/messages", server.url())),
                ProviderRequest::new(&messages),
            )
            .expect("provider should return message");

        mock.assert();
        assert_eq!(response.role, ChatRole::Assistant);
        assert_eq!(
            response.data[0],
            ChatMessageItem::Context(ContextItem {
                text: "need the file".to_string()
            })
        );
        assert_eq!(response.token_usage.unwrap().cache_read, 3);
        assert!(matches!(response.data[1], ChatMessageItem::ToolCall(_)));
    }

    #[test]
    fn sends_claude_system_prompt_with_message_cache_breakpoint() {
        let mut server = mockito::Server::new();
        let mock = server
            .mock("POST", "/v1/messages")
            .match_body(mockito::Matcher::PartialJson(serde_json::json!({
                "system": [
                    {
                        "type": "text",
                        "text": "stable system prompt"
                    }
                ],
                "messages": [
                    {
                        "role": "user",
                        "content": [
                            {
                                "type": "text",
                                "text": "hello",
                                "cache_control": {
                                    "type": "ephemeral",
                                    "ttl": "5m"
                                }
                            }
                        ]
                    }
                ]
            })))
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(
                r#"{
                    "id": "msg_123",
                    "type": "message",
                    "role": "assistant",
                    "content": [
                        {"type": "text", "text": "ok"}
                    ],
                    "usage": {
                        "input_tokens": 10,
                        "cache_creation_input_tokens": 8,
                        "output_tokens": 2
                    }
                }"#,
            )
            .create();

        std::env::set_var("CLAUDE_CODE_API_KEY_TEST", "test-key");

        let provider = ClaudeCodeProvider::new();
        let messages = vec![ChatMessage::new(
            ChatRole::User,
            vec![ChatMessageItem::Context(ContextItem {
                text: "hello".to_string(),
            })],
        )];
        let response = provider
            .send(
                &test_model_config(format!("{}/v1/messages", server.url())),
                ProviderRequest::new(&messages).with_system_prompt(Some("stable system prompt")),
            )
            .expect("provider should return message");

        mock.assert();
        assert_eq!(response.token_usage.unwrap().cache_write, 8);
    }

    #[test]
    fn system_only_request_gets_system_cache_breakpoint() {
        let mut server = mockito::Server::new();
        let mock = server
            .mock("POST", "/v1/messages")
            .match_body(mockito::Matcher::PartialJson(serde_json::json!({
                "system": [
                    {
                        "type": "text",
                        "text": "stable system prompt",
                        "cache_control": {
                            "type": "ephemeral",
                            "ttl": "5m"
                        }
                    }
                ],
                "messages": []
            })))
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(
                r#"{
                    "id": "msg_123",
                    "type": "message",
                    "role": "assistant",
                    "content": [
                        {"type": "text", "text": "ok"}
                    ],
                    "usage": {
                        "input_tokens": 10,
                        "cache_creation_input_tokens": 8,
                        "output_tokens": 2
                    }
                }"#,
            )
            .create();

        std::env::set_var("CLAUDE_CODE_API_KEY_TEST", "test-key");

        let provider = ClaudeCodeProvider::new();
        let messages = Vec::new();
        let response = provider
            .send(
                &test_model_config(format!("{}/v1/messages", server.url())),
                ProviderRequest::new(&messages).with_system_prompt(Some("stable system prompt")),
            )
            .expect("provider should return message");

        mock.assert();
        assert_eq!(response.token_usage.unwrap().cache_write, 8);
    }

    #[test]
    fn assistant_images_are_encoded_as_text_references() {
        let blocks = claude_content_blocks(&ChatMessage::new(
            ChatRole::Assistant,
            vec![ChatMessageItem::File(FileItem {
                uri: "file:///tmp/generated.png".to_string(),
                name: Some("generated.png".to_string()),
                media_type: Some("image/png".to_string()),
                width: Some(640),
                height: Some(480),
                state: None,
            })],
        ));

        assert_eq!(blocks[0]["type"], "text");
        assert_eq!(blocks[0]["text"], "file:///tmp/generated.png");
        assert!(blocks[0].get("source").is_none());
    }

    #[test]
    fn claude_system_only_request_gets_cache_control_block() {
        let cache_control = serde_json::json!({
            "type": "ephemeral",
            "ttl": "1h"
        });

        let system =
            claude_system_with_optional_cache_control("system prompt", Some(&cache_control));

        assert_eq!(system[0]["type"], "text");
        assert_eq!(system[0]["text"], "system prompt");
        assert_eq!(system[0]["cache_control"]["type"], "ephemeral");
        assert_eq!(system[0]["cache_control"]["ttl"], "1h");
    }
}
