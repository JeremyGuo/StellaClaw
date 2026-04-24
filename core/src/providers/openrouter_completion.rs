use std::{collections::HashMap, sync::Mutex, time::Duration};

use rand::Rng;
use reqwest::{blocking::Client, header::ACCEPT_ENCODING, StatusCode};
use serde::{Deserialize, Serialize};

use crate::{
    model_config::{ModelConfig, RetryMode},
    session_actor::{
        ChatMessage, ChatMessageItem, ChatRole, ContextItem, FileItem, TokenUsage, ToolCallItem,
    },
};

use super::{
    common::openrouter_cache_control_payload, OutputPersistor, Provider, ProviderError,
    ProviderRequest,
};

const RESPONSE_PREVIEW_CHARS: usize = 2000;

#[derive(Debug, Default)]
pub struct OpenRouterCompletionProvider {
    clients_by_timeout: Mutex<HashMap<u64, Client>>,
    http_referer: Option<String>,
    title: Option<String>,
    output_persistor: OutputPersistor,
}

impl OpenRouterCompletionProvider {
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

        let request = OpenRouterChatCompletionRequest {
            model: model_config.model_name.clone(),
            messages: build_openrouter_messages(request),
            tools: request
                .tools
                .iter()
                .map(|tool| tool.openai_tool_schema())
                .collect(),
            reasoning: model_config.reasoning.clone(),
            cache_control: openrouter_cache_control_payload(model_config),
        };

        let client = self.client_for_timeout(model_config.conn_timeout)?;
        let mut request_builder = client
            .post(&model_config.url)
            .bearer_auth(api_key)
            .header(ACCEPT_ENCODING, "identity")
            .header("Content-Type", "application/json")
            .json(&request);

        if let Some(http_referer) = &self.http_referer {
            request_builder = request_builder.header("HTTP-Referer", http_referer);
        }
        if let Some(title) = &self.title {
            request_builder = request_builder.header("X-OpenRouter-Title", title);
        }

        let response = request_builder.send().map_err(ProviderError::request)?;
        let status = response.status();
        let body = response.bytes().map_err(|error| {
            ProviderError::InvalidResponse(format!(
                "failed to read response body from {} with status {status}: {error}",
                model_config.url
            ))
        })?;
        let body = String::from_utf8_lossy(&body).to_string();

        if !status.is_success() {
            return Err(ProviderError::HttpStatus {
                url: model_config.url.clone(),
                status: status.as_u16(),
                body,
            });
        }

        let response_body = serde_json::from_str::<OpenRouterChatCompletionResponse>(&body)
            .map_err(|error| {
                ProviderError::InvalidResponse(format!(
                    "OpenRouter chat completion response from {} was not valid JSON: {error}; body preview: {}",
                    model_config.url,
                    preview_body(&body)
                ))
            })?;

        convert_openrouter_response(response_body, &self.output_persistor)
    }

    fn should_retry(error: &ProviderError) -> bool {
        match error {
            ProviderError::Request(_) => true,
            ProviderError::HttpStatus { status, .. } => {
                *status == StatusCode::TOO_MANY_REQUESTS.as_u16() || *status >= 500
            }
            ProviderError::MissingApiKeyEnv(_)
            | ProviderError::BuildHttpClient(_)
            | ProviderError::DecodeResponse(_)
            | ProviderError::DecodeJson(_)
            | ProviderError::InvalidResponse(_)
            | ProviderError::WebSocket(_)
            | ProviderError::PersistOutput(_)
            | ProviderError::EmptyChoices
            | ProviderError::Subprocess(_) => false,
        }
    }
}

fn preview_body(body: &str) -> String {
    let mut preview = body
        .chars()
        .take(RESPONSE_PREVIEW_CHARS)
        .collect::<String>();
    if body.chars().count() > RESPONSE_PREVIEW_CHARS {
        preview.push_str("...");
    }
    preview
}

impl Provider for OpenRouterCompletionProvider {
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

#[derive(Debug, Clone, Serialize)]
struct OpenRouterChatCompletionRequest {
    model: String,
    messages: Vec<OpenRouterRequestMessage>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    tools: Vec<serde_json::Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    reasoning: Option<serde_json::Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    cache_control: Option<serde_json::Value>,
}

#[derive(Debug, Clone, Serialize)]
struct OpenRouterRequestMessage {
    role: String,
    content: OpenRouterMessageContent,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    tool_calls: Vec<OpenRouterRequestToolCall>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_call_id: Option<String>,
}

impl OpenRouterRequestMessage {
    fn system(content: String) -> Self {
        Self {
            role: "system".to_string(),
            content: OpenRouterMessageContent::Text(content),
            tool_calls: Vec::new(),
            tool_call_id: None,
        }
    }

    fn from_chat_message(message: &ChatMessage) -> Vec<Self> {
        let text_segments = collect_visible_text_segments(message);
        let image_files = collect_image_files(message);
        let tool_calls = collect_request_tool_calls(message);
        let mut messages = Vec::new();

        let content =
            if image_files.is_empty() {
                OpenRouterMessageContent::Text(text_segments.join("\n"))
            } else {
                let mut parts = Vec::new();

                if !text_segments.is_empty() {
                    parts.push(OpenRouterContentPart::Text {
                        text: text_segments.join("\n"),
                    });
                }

                if matches!(message.role, ChatRole::User) {
                    parts.extend(image_files.into_iter().map(|file| {
                        OpenRouterContentPart::ImageUrl {
                            image_url: OpenRouterImageUrl {
                                url: file.uri,
                                detail: None,
                            },
                        }
                    }));
                }

                if parts.is_empty() {
                    OpenRouterMessageContent::Text(String::new())
                } else {
                    OpenRouterMessageContent::Parts(parts)
                }
            };

        if !matches!(content, OpenRouterMessageContent::Text(ref text) if text.is_empty())
            || !tool_calls.is_empty()
        {
            messages.push(Self {
                role: openrouter_role(&message.role).to_string(),
                content,
                tool_calls,
                tool_call_id: None,
            });
        }

        for item in &message.data {
            if let ChatMessageItem::ToolResult(tool_result) = item {
                messages.push(Self {
                    role: "tool".to_string(),
                    content: OpenRouterMessageContent::Text(tool_result_content_text(tool_result)),
                    tool_calls: Vec::new(),
                    tool_call_id: Some(tool_result.tool_call_id.clone()),
                });
            }
        }

        messages
    }
}

#[derive(Debug, Clone, Serialize)]
struct OpenRouterRequestToolCall {
    id: String,
    #[serde(rename = "type")]
    kind: String,
    function: OpenRouterRequestFunctionCall,
}

#[derive(Debug, Clone, Serialize)]
struct OpenRouterRequestFunctionCall {
    name: String,
    arguments: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(untagged)]
enum OpenRouterMessageContent {
    Text(String),
    Parts(Vec<OpenRouterContentPart>),
}

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum OpenRouterContentPart {
    Text { text: String },
    ImageUrl { image_url: OpenRouterImageUrl },
}

#[derive(Debug, Clone, Serialize)]
struct OpenRouterImageUrl {
    url: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    detail: Option<String>,
}

#[derive(Debug, Deserialize)]
struct OpenRouterChatCompletionResponse {
    id: String,
    model: String,
    usage: Option<OpenRouterUsage>,
    choices: Vec<OpenRouterChoice>,
}

#[derive(Debug, Deserialize)]
struct OpenRouterChoice {
    finish_reason: Option<String>,
    message: OpenRouterAssistantMessage,
}

#[derive(Debug, Deserialize)]
struct OpenRouterAssistantMessage {
    content: Option<String>,
    #[serde(default)]
    tool_calls: Vec<OpenRouterToolCall>,
    #[serde(default)]
    images: Vec<OpenRouterImage>,
}

#[derive(Debug, Deserialize)]
struct OpenRouterToolCall {
    id: String,
    function: OpenRouterFunctionCall,
}

#[derive(Debug, Deserialize)]
struct OpenRouterFunctionCall {
    name: String,
    arguments: String,
}

#[derive(Debug, Deserialize)]
struct OpenRouterUsage {
    #[serde(default)]
    prompt_tokens: u64,
    #[serde(default)]
    completion_tokens: u64,
}

#[derive(Debug, Deserialize)]
struct OpenRouterImage {
    image_url: OpenRouterReturnedImageUrl,
}

#[derive(Debug, Deserialize)]
struct OpenRouterReturnedImageUrl {
    url: String,
}

fn build_openrouter_messages(request: &ProviderRequest<'_>) -> Vec<OpenRouterRequestMessage> {
    let mut messages = Vec::new();
    if let Some(system_prompt) = request.system_prompt {
        if !system_prompt.trim().is_empty() {
            messages.push(OpenRouterRequestMessage::system(system_prompt.to_string()));
        }
    }

    for message in request.messages {
        messages.extend(OpenRouterRequestMessage::from_chat_message(message));
    }

    messages
}

fn collect_visible_text_segments(message: &ChatMessage) -> Vec<String> {
    let mut segments = Vec::new();

    for item in &message.data {
        match item {
            ChatMessageItem::Reasoning(_) => {}
            ChatMessageItem::Context(context) => segments.push(context.text.clone()),
            ChatMessageItem::File(file) => {
                if !is_image_file(file) || !matches!(message.role, ChatRole::User) {
                    segments.push(file.uri.clone());
                }
            }
            ChatMessageItem::ToolCall(_) | ChatMessageItem::ToolResult(_) => {}
        }
    }

    segments
}

fn collect_request_tool_calls(message: &ChatMessage) -> Vec<OpenRouterRequestToolCall> {
    if !matches!(message.role, ChatRole::Assistant) {
        return Vec::new();
    }

    message
        .data
        .iter()
        .filter_map(|item| match item {
            ChatMessageItem::ToolCall(tool_call) => Some(OpenRouterRequestToolCall {
                id: tool_call.tool_call_id.clone(),
                kind: "function".to_string(),
                function: OpenRouterRequestFunctionCall {
                    name: tool_call.tool_name.clone(),
                    arguments: tool_call.arguments.text.clone(),
                },
            }),
            _ => None,
        })
        .collect()
}

fn tool_result_content_text(tool_result: &crate::session_actor::ToolResultItem) -> String {
    let mut parts = Vec::new();
    if let Some(context) = &tool_result.result.context {
        if !context.text.trim().is_empty() {
            parts.push(context.text.clone());
        }
    }
    if let Some(file) = &tool_result.result.file {
        parts.push(file.uri.clone());
    }
    parts.join("\n")
}

fn collect_image_files(message: &ChatMessage) -> Vec<FileItem> {
    if !matches!(message.role, ChatRole::User) {
        return Vec::new();
    }

    let mut files = Vec::new();

    for item in &message.data {
        match item {
            ChatMessageItem::File(file) if is_image_file(file) => files.push(file.clone()),
            ChatMessageItem::ToolResult(tool_result) => {
                if let Some(file) = &tool_result.result.file {
                    if is_image_file(file) {
                        files.push(file.clone());
                    }
                }
            }
            _ => {}
        }
    }

    files
}

fn is_image_file(file: &FileItem) -> bool {
    matches!(file.media_type.as_deref(), Some(media_type) if media_type.starts_with("image/"))
}

fn openrouter_role(role: &ChatRole) -> &'static str {
    match role {
        ChatRole::User => "user",
        ChatRole::Assistant => "assistant",
    }
}

fn convert_openrouter_response(
    response: OpenRouterChatCompletionResponse,
    output_persistor: &OutputPersistor,
) -> Result<ChatMessage, ProviderError> {
    let choice = response
        .choices
        .into_iter()
        .next()
        .ok_or(ProviderError::EmptyChoices)?;

    let mut data = Vec::new();

    if let Some(content) = choice.message.content {
        if !content.is_empty() {
            data.push(ChatMessageItem::Context(ContextItem { text: content }));
        }
    }

    for tool_call in choice.message.tool_calls {
        data.push(ChatMessageItem::ToolCall(ToolCallItem {
            tool_call_id: tool_call.id,
            tool_name: tool_call.function.name,
            arguments: ContextItem {
                text: tool_call.function.arguments,
            },
        }));
    }

    for image in choice.message.images {
        let file = output_persistor.persist_image_data_url(&image.image_url.url)?;
        data.push(ChatMessageItem::File(file));
    }

    let token_usage = response.usage.map(|usage| TokenUsage {
        cache_read: 0,
        cache_write: 0,
        uncache_input: usage.prompt_tokens,
        output: usage.completion_tokens,
    });

    let _ = response.id;
    let _ = response.model;
    let _ = choice.finish_reason;

    Ok(ChatMessage {
        role: ChatRole::Assistant,
        user_name: None,
        message_time: None,
        token_usage,
        data,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        model_config::{ModelCapability, ProviderType, TokenEstimatorType},
        session_actor::ChatMessageItem,
        test_support::temp_cwd,
    };

    fn test_model_config(url: String) -> ModelConfig {
        ModelConfig {
            provider_type: ProviderType::OpenRouterCompletion,
            model_name: "openai/gpt-4o-mini".to_string(),
            url,
            api_key_env: "OPENROUTER_API_KEY_TEST".to_string(),
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
    fn sends_openrouter_chat_completion_request() {
        let mut server = mockito::Server::new();
        let mock = server
            .mock("POST", "/api/v1/chat/completions")
            .match_header("authorization", "Bearer test-key")
            .match_header("content-type", "application/json")
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(
                r#"{
                    "id": "gen_123",
                    "model": "openai/gpt-4o-mini",
                    "choices": [
                        {
                            "finish_reason": "stop",
                            "message": {
                                "content": "hello from openrouter",
                                "tool_calls": []
                            }
                        }
                    ],
                    "usage": {
                        "prompt_tokens": 12,
                        "completion_tokens": 7
                    }
                }"#,
            )
            .create();

        std::env::set_var("OPENROUTER_API_KEY_TEST", "test-key");

        let provider = OpenRouterCompletionProvider::new();
        let messages = vec![ChatMessage::new(
            ChatRole::User,
            vec![ChatMessageItem::Context(ContextItem {
                text: "hello".to_string(),
            })],
        )];
        let response = provider
            .send(
                &test_model_config(format!("{}/api/v1/chat/completions", server.url())),
                ProviderRequest::new(&messages),
            )
            .expect("request should succeed");

        mock.assert();
        assert_eq!(response.token_usage.as_ref().unwrap().uncache_input, 12);
        assert_eq!(response.token_usage.as_ref().unwrap().output, 7);
        assert_eq!(
            response.data,
            vec![ChatMessageItem::Context(ContextItem {
                text: "hello from openrouter".to_string(),
            })]
        );
    }

    #[test]
    fn invalid_openrouter_success_body_reports_preview() {
        let mut server = mockito::Server::new();
        let mock = server
            .mock("POST", "/api/v1/chat/completions")
            .with_status(200)
            .with_header("content-type", "text/plain")
            .with_body("not json")
            .create();

        std::env::set_var("OPENROUTER_API_KEY_TEST", "test-key");

        let provider = OpenRouterCompletionProvider::new();
        let messages = vec![ChatMessage::new(
            ChatRole::User,
            vec![ChatMessageItem::Context(ContextItem {
                text: "hello".to_string(),
            })],
        )];

        let error = provider
            .send(
                &test_model_config(format!("{}/api/v1/chat/completions", server.url())),
                ProviderRequest::new(&messages),
            )
            .expect_err("invalid JSON should fail with preview");

        mock.assert();
        let error = error.to_string();
        assert!(error.contains("OpenRouter chat completion response"));
        assert!(error.contains("body preview: not json"));
    }

    #[test]
    fn encodes_user_images_as_openrouter_content_parts() {
        let request_messages = OpenRouterRequestMessage::from_chat_message(&ChatMessage::new(
            ChatRole::User,
            vec![
                ChatMessageItem::Context(ContextItem {
                    text: "describe this".to_string(),
                }),
                ChatMessageItem::File(FileItem {
                    uri: "https://example.com/cat.png".to_string(),
                    name: Some("cat.png".to_string()),
                    media_type: Some("image/png".to_string()),
                    width: Some(640),
                    height: Some(480),
                    state: None,
                }),
            ],
        ));
        let request_message = request_messages
            .first()
            .expect("user message should convert");

        let value = serde_json::to_value(request_message).expect("message should serialize");
        assert_eq!(value["role"], "user");
        assert_eq!(value["content"][0]["type"], "text");
        assert_eq!(value["content"][0]["text"], "describe this");
        assert_eq!(value["content"][1]["type"], "image_url");
        assert_eq!(
            value["content"][1]["image_url"]["url"],
            "https://example.com/cat.png"
        );
    }

    #[test]
    fn encodes_assistant_images_as_text_references() {
        let request_messages = OpenRouterRequestMessage::from_chat_message(&ChatMessage::new(
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
        let request_message = request_messages
            .first()
            .expect("assistant image reference should convert");

        let value = serde_json::to_value(request_message).expect("message should serialize");
        assert_eq!(value["role"], "assistant");
        assert_eq!(value["content"], "file:///tmp/generated.png");
    }

    #[test]
    fn anthropic_openrouter_request_adds_cache_control() {
        let mut server = mockito::Server::new();
        let mock = server
            .mock("POST", "/api/v1/chat/completions")
            .match_header("authorization", "Bearer test-key")
            .match_body(mockito::Matcher::PartialJson(serde_json::json!({
                "model": "anthropic/claude-sonnet-4.5",
                "cache_control": {
                    "type": "ephemeral"
                }
            })))
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(
                r#"{
                    "id": "gen_cache",
                    "model": "anthropic/claude-sonnet-4.5",
                    "choices": [
                        {
                            "finish_reason": "stop",
                            "message": {
                                "content": "cached",
                                "tool_calls": []
                            }
                        }
                    ],
                    "usage": {
                        "prompt_tokens": 12,
                        "completion_tokens": 7
                    }
                }"#,
            )
            .create();

        std::env::set_var("OPENROUTER_API_KEY_TEST", "test-key");

        let provider = OpenRouterCompletionProvider::new();
        let messages = vec![ChatMessage::new(
            ChatRole::User,
            vec![ChatMessageItem::Context(ContextItem {
                text: "hello".to_string(),
            })],
        )];
        let mut model_config =
            test_model_config(format!("{}/api/v1/chat/completions", server.url()));
        model_config.model_name = "anthropic/claude-sonnet-4.5".to_string();
        model_config.cache_timeout = 300;

        provider
            .send(&model_config, ProviderRequest::new(&messages))
            .expect("request should succeed");

        mock.assert();
    }

    #[test]
    fn persists_openrouter_output_images_into_output_directory() {
        let _cwd = temp_cwd("openrouter-completion-output-images");
        let message = convert_openrouter_response(
            OpenRouterChatCompletionResponse {
                id: "gen_456".to_string(),
                model: "google/gemini-2.5-flash-image".to_string(),
                usage: None,
                choices: vec![OpenRouterChoice {
                    finish_reason: Some("stop".to_string()),
                    message: OpenRouterAssistantMessage {
                        content: Some("image ready".to_string()),
                        tool_calls: Vec::new(),
                        images: vec![OpenRouterImage {
                            image_url: OpenRouterReturnedImageUrl {
                                url: "data:image/png;base64,aGVsbG8=".to_string(),
                            },
                        }],
                    },
                }],
            },
            &OutputPersistor,
        )
        .expect("image output should persist");

        assert_eq!(message.role, ChatRole::Assistant);
        assert_eq!(message.data.len(), 2);
        match &message.data[1] {
            ChatMessageItem::File(file) => {
                assert!(file.uri.starts_with("file://"));
                assert_eq!(file.media_type.as_deref(), Some("image/png"));
            }
            other => panic!("expected file item, got {other:?}"),
        }
    }
}
