use std::{collections::HashMap, fs, path::PathBuf, sync::Mutex, time::Duration};

use base64::{engine::general_purpose::STANDARD, Engine};
use rand::Rng;
use reqwest::{
    blocking::{multipart, Client},
    StatusCode,
};
use serde::Deserialize;

use crate::{
    model_config::{ModelConfig, RetryMode},
    session_actor::{ChatMessage, ChatMessageItem, ChatRole, ContextItem, FileItem},
};

use super::{
    common::{data_url_parts, is_image_file},
    OutputPersistor, Provider, ProviderError, ProviderRequest,
};

#[derive(Debug, Default)]
pub struct OpenAiImageEditProvider {
    clients_by_timeout: Mutex<HashMap<u64, Client>>,
    output_persistor: OutputPersistor,
}

impl OpenAiImageEditProvider {
    pub fn new() -> Self {
        Self::default()
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
        let (prompt, image) = prompt_and_image_from_request(request)?;
        let image_part = image_file_part(&image)?;

        let form = multipart::Form::new()
            .text("model", model_config.model_name.clone())
            .text("prompt", prompt)
            .text("n", "1")
            .text("response_format", "b64_json")
            .part("image", image_part);

        let client = self.client_for_timeout(model_config.conn_timeout)?;
        let response = client
            .post(&model_config.url)
            .bearer_auth(api_key)
            .multipart(form)
            .send()
            .map_err(ProviderError::Request)?;
        let status = response.status();
        let body = response.text().map_err(ProviderError::DecodeResponse)?;

        if !status.is_success() {
            return Err(ProviderError::HttpStatus {
                url: model_config.url.clone(),
                status: status.as_u16(),
                body,
            });
        }

        let response = serde_json::from_str::<OpenAiImageResponse>(&body)
            .map_err(ProviderError::DecodeJson)?;
        convert_image_response(response, &self.output_persistor)
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

impl Provider for OpenAiImageEditProvider {
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

#[derive(Debug, Deserialize)]
struct OpenAiImageResponse {
    #[serde(default)]
    data: Vec<OpenAiImageData>,
    #[serde(default)]
    error: Option<serde_json::Value>,
}

#[derive(Debug, Deserialize)]
struct OpenAiImageData {
    #[serde(default)]
    b64_json: Option<String>,
    #[serde(default)]
    url: Option<String>,
    #[serde(default)]
    revised_prompt: Option<String>,
}

fn prompt_and_image_from_request(
    request: &ProviderRequest<'_>,
) -> Result<(String, FileItem), ProviderError> {
    let mut prompt_parts = Vec::new();
    let mut first_image = None;

    if let Some(system_prompt) = request.system_prompt {
        if !system_prompt.trim().is_empty() {
            prompt_parts.push(system_prompt.trim().to_string());
        }
    }

    for message in request.messages {
        if !matches!(message.role, ChatRole::User) {
            continue;
        }
        for item in &message.data {
            match item {
                ChatMessageItem::Context(context) if !context.text.trim().is_empty() => {
                    prompt_parts.push(context.text.trim().to_string());
                }
                ChatMessageItem::File(file) if first_image.is_none() && is_image_file(file) => {
                    first_image = Some(file.clone());
                }
                _ => {}
            }
        }
    }

    let prompt = prompt_parts.join("\n\n");
    if prompt.is_empty() {
        return Err(ProviderError::InvalidResponse(
            "openai_image_edit requires a prompt".to_string(),
        ));
    }
    let image = first_image.ok_or_else(|| {
        ProviderError::InvalidResponse(
            "openai_image_edit requires at least one user image".to_string(),
        )
    })?;
    Ok((prompt, image))
}

fn image_file_part(file: &FileItem) -> Result<multipart::Part, ProviderError> {
    let (bytes, filename, media_type) = image_file_bytes(file)?;
    multipart::Part::bytes(bytes)
        .file_name(filename)
        .mime_str(&media_type)
        .map_err(|error| {
            ProviderError::InvalidResponse(format!("invalid image mime type: {error}"))
        })
}

fn image_file_bytes(file: &FileItem) -> Result<(Vec<u8>, String, String), ProviderError> {
    if let Some((media_type, data)) = data_url_parts(&file.uri) {
        let bytes = STANDARD.decode(data).map_err(|error| {
            ProviderError::InvalidResponse(format!("failed to decode image data url: {error}"))
        })?;
        let filename = file
            .name
            .clone()
            .unwrap_or_else(|| format!("image.{}", image_extension(&media_type)));
        return Ok((bytes, filename, media_type));
    }

    let path = file_path_from_uri(&file.uri)?;
    let bytes = fs::read(&path).map_err(|error| {
        ProviderError::InvalidResponse(format!(
            "failed to read image file {}: {error}",
            path.display()
        ))
    })?;
    let filename = file.name.clone().unwrap_or_else(|| {
        path.file_name()
            .map(|name| name.to_string_lossy().to_string())
            .unwrap_or_else(|| "image.png".to_string())
    });
    let media_type = file
        .media_type
        .clone()
        .unwrap_or_else(|| "image/png".to_string());
    Ok((bytes, filename, media_type))
}

fn file_path_from_uri(uri: &str) -> Result<PathBuf, ProviderError> {
    if let Some(path) = uri.strip_prefix("file://") {
        return Ok(PathBuf::from(path));
    }
    Err(ProviderError::InvalidResponse(format!(
        "openai_image_edit only supports local file:// or data: image inputs, got {uri}"
    )))
}

fn image_extension(media_type: &str) -> &'static str {
    match media_type {
        "image/jpeg" => "jpg",
        "image/webp" => "webp",
        "image/gif" => "gif",
        _ => "png",
    }
}

fn convert_image_response(
    response: OpenAiImageResponse,
    output_persistor: &OutputPersistor,
) -> Result<ChatMessage, ProviderError> {
    if let Some(error) = response.error.as_ref().and_then(image_error_message) {
        return Err(ProviderError::InvalidResponse(error));
    }

    let mut data = Vec::new();
    for item in response.data {
        if let Some(prompt) = item
            .revised_prompt
            .filter(|prompt| !prompt.trim().is_empty())
        {
            data.push(ChatMessageItem::Context(ContextItem { text: prompt }));
        }
        if let Some(b64_json) = item.b64_json {
            let data_url = format!("data:image/png;base64,{b64_json}");
            data.push(ChatMessageItem::File(
                output_persistor.persist_image_data_url(&data_url)?,
            ));
        } else if let Some(url) = item.url {
            data.push(ChatMessageItem::File(FileItem {
                uri: url,
                name: None,
                media_type: Some("image/*".to_string()),
                width: None,
                height: None,
                state: None,
            }));
        }
    }

    if data.is_empty() {
        return Err(ProviderError::InvalidResponse(
            "image edit response did not include image data".to_string(),
        ));
    }

    Ok(ChatMessage {
        role: ChatRole::Assistant,
        user_name: None,
        message_time: None,
        token_usage: None,
        data,
    })
}

fn image_error_message(error: &serde_json::Value) -> Option<String> {
    error
        .get("message")
        .and_then(serde_json::Value::as_str)
        .or_else(|| error.as_str())
        .map(str::to_string)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        model_config::{ModelCapability, ProviderType, TokenEstimatorType},
        session_actor::ChatMessage,
        test_support::temp_cwd,
    };

    fn test_model_config(url: String) -> ModelConfig {
        ModelConfig {
            provider_type: ProviderType::OpenAiImageEdit,
            model_name: "gpt-image-2".to_string(),
            url,
            api_key_env: "OPENAI_IMAGE_EDIT_API_KEY_TEST".to_string(),
            capabilities: vec![
                ModelCapability::Chat,
                ModelCapability::ImageIn,
                ModelCapability::ImageOut,
            ],
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
    fn rejects_requests_without_user_image() {
        std::env::set_var("OPENAI_IMAGE_EDIT_API_KEY_TEST", "test-key");
        let provider = OpenAiImageEditProvider::new();
        let messages = vec![ChatMessage::new(
            ChatRole::User,
            vec![ChatMessageItem::Context(ContextItem {
                text: "draw a cat".to_string(),
            })],
        )];
        let error = provider
            .send(
                &test_model_config("http://127.0.0.1:9/v1/images/edits".to_string()),
                ProviderRequest::new(&messages),
            )
            .expect_err("missing image should fail");

        assert!(error
            .to_string()
            .contains("requires at least one user image"));
    }

    #[test]
    fn sends_multipart_image_edit_and_persists_b64_output() {
        let _cwd = temp_cwd("openai-image-edit-provider");
        let mut server = mockito::Server::new();
        let mock = server
            .mock("POST", "/v1/images/edits")
            .match_header("authorization", "Bearer test-key")
            .match_header("content-type", mockito::Matcher::Regex("multipart/form-data; boundary=.*".to_string()))
            .match_body(mockito::Matcher::Regex(
                "(?s).*name=\"model\".*gpt-image-2.*name=\"prompt\".*draw a moon.*name=\"response_format\".*b64_json.*name=\"image\".*".to_string(),
            ))
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(r#"{"data":[{"b64_json":"aGVsbG8=","revised_prompt":"draw a moon"}]}"#)
            .create();

        std::env::set_var("OPENAI_IMAGE_EDIT_API_KEY_TEST", "test-key");
        let provider = OpenAiImageEditProvider::new();
        let messages = vec![ChatMessage::new(
            ChatRole::User,
            vec![
                ChatMessageItem::Context(ContextItem {
                    text: "draw a moon".to_string(),
                }),
                ChatMessageItem::File(FileItem {
                    uri: "data:image/png;base64,aW1hZ2U=".to_string(),
                    name: Some("input.png".to_string()),
                    media_type: Some("image/png".to_string()),
                    width: Some(1),
                    height: Some(1),
                    state: None,
                }),
            ],
        )];

        let response = provider
            .send(
                &test_model_config(format!("{}/v1/images/edits", server.url())),
                ProviderRequest::new(&messages),
            )
            .expect("image edit request should succeed");

        mock.assert();
        assert_eq!(response.role, ChatRole::Assistant);
        assert!(response.data.iter().any(
            |item| matches!(item, ChatMessageItem::File(file) if file.uri.starts_with("file://"))
        ));
    }
}
