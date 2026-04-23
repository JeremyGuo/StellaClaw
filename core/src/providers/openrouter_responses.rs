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
        is_image_file, openrouter_cache_control_payload, provider_error_message,
        token_usage_from_value,
    },
    OutputPersistor, Provider, ProviderError, ProviderRequest,
};

#[derive(Debug, Default)]
pub struct OpenRouterResponsesProvider {
    clients_by_timeout: Mutex<HashMap<u64, Client>>,
    http_referer: Option<String>,
    title: Option<String>,
    output_persistor: OutputPersistor,
}

impl OpenRouterResponsesProvider {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_app_metadata(http_referer: Option<String>, title: Option<String>) -> Self {
        Self {
            clients_by_timeout: Mutex::new(HashMap::new()),
            http_referer,
            title,
            output_persistor: OutputPersistor,
        }
    }

    fn client_for_timeout(&self, timeout_secs: u64) -> Result<Client, ProviderError> {
        let mut clients = self.clients_by_timeout.lock().expect("mutex poisoned");

        if let Some(client) = clients.get(&timeout_secs) {
            return Ok(client.clone());
        }

        let client = Client::builder()
            .connect_timeout(Duration::from_secs(timeout_secs))
            .timeout(Duration::from_secs(timeout_secs))
            .build()
            .map_err(ProviderError::BuildHttpClient)?;

        clients.insert(timeout_secs, client.clone());
        Ok(client)
    }

    fn send_once(
        &self,
        model_config: &ModelConfig,
        request: &ProviderRequest<'_>,
    ) -> Result<ChatMessage, ProviderError> {
        let api_key = std::env::var(&model_config.api_key_env)
            .map_err(|_| ProviderError::MissingApiKeyEnv(model_config.api_key_env.clone()))?;

        let mut payload = Map::new();
        payload.insert(
            "model".to_string(),
            Value::String(model_config.model_name.clone()),
        );
        payload.insert(
            "input".to_string(),
            Value::Array(build_responses_input(request.messages)),
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
        if let Some(reasoning) = reasoning_payload(model_config) {
            payload.insert("reasoning".to_string(), reasoning);
        }
        if let Some(cache_control) = openrouter_cache_control_payload(model_config) {
            payload.insert("cache_control".to_string(), cache_control);
        }
        payload.insert("store".to_string(), Value::Bool(false));
        let payload = Value::Object(payload);

        let client = self.client_for_timeout(model_config.conn_timeout)?;
        let mut request_builder = client
            .post(&model_config.url)
            .bearer_auth(api_key)
            .header("Content-Type", "application/json")
            .json(&payload);

        if let Some(http_referer) = &self.http_referer {
            request_builder = request_builder.header("HTTP-Referer", http_referer);
        }
        if let Some(title) = &self.title {
            request_builder = request_builder.header("X-OpenRouter-Title", title);
        }

        let response = request_builder.send().map_err(ProviderError::Request)?;
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

        responses_value_to_chat_message(&value, &self.output_persistor)
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

impl Provider for OpenRouterResponsesProvider {
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

fn reasoning_payload(model_config: &ModelConfig) -> Option<Value> {
    model_config
        .reasoning
        .as_ref()
        .and_then(|reasoning| match reasoning {
            Value::Null => None,
            Value::Object(_) => Some(reasoning.clone()),
            _ => Some(reasoning.clone()),
        })
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
        user_name: None,
        message_time: None,
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
    use crate::{
        model_config::{ModelCapability, ProviderType, TokenEstimatorType},
        session_actor::{ChatMessageItem, ChatRole, ContextItem},
    };

    fn test_model_config(url: String) -> ModelConfig {
        ModelConfig {
            provider_type: ProviderType::OpenRouterResponses,
            model_name: "openai/gpt-4o-mini".to_string(),
            url,
            api_key_env: "OPENROUTER_RESPONSES_API_KEY_TEST".to_string(),
            capabilities: vec![ModelCapability::Chat],
            token_max_context: 128_000,
            cache_timeout: 300,
            conn_timeout: 5,
            retry_mode: RetryMode::Once,
            reasoning: None,
            token_estimator_type: TokenEstimatorType::Local,
            multimodal_estimator: None,
            multimodal_input: None,
            token_estimator_url: None,
        }
    }

    #[test]
    fn sends_responses_request_and_parses_assistant_message() {
        let mut server = mockito::Server::new();
        let mock = server
            .mock("POST", "/api/v1/responses")
            .match_header("authorization", "Bearer test-key")
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(
                r#"{
                    "id": "resp_123",
                    "output": [
                        {
                            "type": "message",
                            "role": "assistant",
                            "content": [
                                {"type": "output_text", "text": "hello from responses"}
                            ]
                        },
                        {
                            "type": "function_call",
                            "call_id": "call_1",
                            "name": "file_read",
                            "arguments": "{\"path\":\"README.md\"}"
                        }
                    ],
                    "usage": {
                        "input_tokens": 9,
                        "input_tokens_details": {"cached_tokens": 2},
                        "output_tokens": 4
                    }
                }"#,
            )
            .create();

        std::env::set_var("OPENROUTER_RESPONSES_API_KEY_TEST", "test-key");

        let provider = OpenRouterResponsesProvider::new();
        let messages = vec![ChatMessage::new(
            ChatRole::User,
            vec![ChatMessageItem::Context(ContextItem {
                text: "hello".to_string(),
            })],
        )];
        let response = provider
            .send(
                &test_model_config(format!("{}/api/v1/responses", server.url())),
                ProviderRequest::new(&messages),
            )
            .expect("provider should return message");

        mock.assert();
        assert_eq!(response.role, ChatRole::Assistant);
        assert_eq!(
            response.data[0],
            ChatMessageItem::Context(ContextItem {
                text: "hello from responses".to_string()
            })
        );
        assert_eq!(response.token_usage.unwrap().cache_read, 2);
        assert!(matches!(response.data[1], ChatMessageItem::ToolCall(_)));
    }

    #[test]
    fn anthropic_openrouter_responses_request_adds_one_hour_cache_control() {
        let mut server = mockito::Server::new();
        let mock = server
            .mock("POST", "/api/v1/responses")
            .match_header("authorization", "Bearer test-key")
            .match_body(mockito::Matcher::PartialJson(serde_json::json!({
                "model": "anthropic/claude-sonnet-4.5",
                "cache_control": {
                    "type": "ephemeral",
                    "ttl": "1h"
                }
            })))
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(
                r#"{
                    "id": "resp_cache",
                    "output": [
                        {
                            "type": "message",
                            "role": "assistant",
                            "content": [
                                {"type": "output_text", "text": "cached"}
                            ]
                        }
                    ],
                    "usage": {
                        "input_tokens": 9,
                        "output_tokens": 4
                    }
                }"#,
            )
            .create();

        std::env::set_var("OPENROUTER_RESPONSES_API_KEY_TEST", "test-key");

        let provider = OpenRouterResponsesProvider::new();
        let messages = vec![ChatMessage::new(
            ChatRole::User,
            vec![ChatMessageItem::Context(ContextItem {
                text: "hello".to_string(),
            })],
        )];
        let mut model_config = test_model_config(format!("{}/api/v1/responses", server.url()));
        model_config.model_name = "anthropic/claude-sonnet-4.5".to_string();
        model_config.cache_timeout = 3600;

        provider
            .send(&model_config, ProviderRequest::new(&messages))
            .expect("provider should return message");

        mock.assert();
    }

    #[test]
    fn responses_input_converts_tool_results_without_extra_user_message() {
        use crate::session_actor::{ToolResultContent, ToolResultItem};

        let messages = vec![ChatMessage::new(
            ChatRole::User,
            vec![ChatMessageItem::ToolResult(ToolResultItem {
                tool_call_id: "call_1".to_string(),
                tool_name: "file_read".to_string(),
                result: ToolResultContent {
                    context: Some(ContextItem {
                        text: "loaded".to_string(),
                    }),
                    file: None,
                },
            })],
        )];

        let input = build_responses_input(&messages);

        assert_eq!(input.len(), 1);
        assert_eq!(input[0]["type"], "function_call_output");
        assert_eq!(input[0]["call_id"], "call_1");
    }
}
