use std::time::Duration;

use reqwest::blocking::Client;
use serde_json::{json, Map, Value};

use crate::{
    model_config::ModelConfig,
    session_actor::{ChatMessage, ChatMessageItem, ChatRole, ContextItem},
};

use super::{ProviderBackend, ProviderError, ProviderRequest};

#[derive(Debug, Default)]
pub struct BraveSearchImageProvider;

impl BraveSearchImageProvider {
    pub fn new() -> Self {
        Self
    }

    pub fn search_images(
        &self,
        model_config: &ModelConfig,
        query: &str,
        max_results: usize,
    ) -> Result<Value, ProviderError> {
        let api_key = std::env::var(&model_config.api_key_env)
            .map_err(|_| ProviderError::MissingApiKeyEnv(model_config.api_key_env.clone()))?;
        let client = Client::builder()
            .connect_timeout(Duration::from_secs(model_config.conn_timeout_secs()))
            .timeout(Duration::from_secs(model_config.request_timeout_secs()))
            .build()
            .map_err(ProviderError::BuildHttpClient)?;
        let response = client
            .get(&model_config.url)
            .header("x-subscription-token", api_key)
            .header("accept", "application/json")
            .query(&image_search_query(query, max_results))
            .send()
            .map_err(ProviderError::request)?;
        let status = response.status();
        if !status.is_success() {
            let body = response.text().unwrap_or_default();
            return Err(ProviderError::HttpStatus {
                url: model_config.url.clone(),
                status: status.as_u16(),
                body,
            });
        }
        let value = response
            .json::<Value>()
            .map_err(ProviderError::DecodeResponse)?;
        Ok(parse_brave_image_search_response(
            &value,
            query,
            max_results.clamp(1, 200),
        ))
    }
}

impl ProviderBackend for BraveSearchImageProvider {
    fn send(
        &self,
        model_config: &ModelConfig,
        request: ProviderRequest<'_>,
    ) -> Result<ChatMessage, ProviderError> {
        let query = request
            .messages
            .iter()
            .rev()
            .find_map(message_text)
            .ok_or_else(|| {
                ProviderError::InvalidResponse(
                    "Brave image search provider request did not include a query message"
                        .to_string(),
                )
            })?;
        let result = self.search_images(model_config, &query, 5)?;
        Ok(ChatMessage::new(
            ChatRole::Assistant,
            vec![ChatMessageItem::Context(ContextItem {
                text: result.to_string(),
            })],
        ))
    }
}

fn message_text(message: &ChatMessage) -> Option<String> {
    let text = message
        .data
        .iter()
        .filter_map(|item| match item {
            ChatMessageItem::Context(context) => Some(context.text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n");
    (!text.trim().is_empty()).then_some(text)
}

pub fn parse_brave_image_search_response(value: &Value, query: &str, max_results: usize) -> Value {
    let results = value
        .get("results")
        .and_then(Value::as_array)
        .map(|results| {
            results
                .iter()
                .filter_map(brave_image_result_summary)
                .take(max_results)
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    let citations = results
        .iter()
        .filter_map(|result| result.get("page_url").cloned())
        .collect::<Vec<_>>();
    json!({
        "query": query,
        "citations": citations,
        "results": results,
    })
}

fn brave_image_result_summary(result: &Value) -> Option<Value> {
    let result = result.as_object()?;
    let title = string_field(result, "title");
    let page_url = string_field(result, "url");
    let source = string_field(result, "source");
    let confidence = string_field(result, "confidence");
    let thumbnail = result.get("thumbnail").and_then(Value::as_object);
    let properties = result.get("properties").and_then(Value::as_object);
    let thumbnail_url = thumbnail.map(|thumbnail| string_field(thumbnail, "src"));
    let image_url = properties.map(|properties| string_field(properties, "url"));

    if title.is_empty()
        && page_url.is_empty()
        && thumbnail_url.as_deref().unwrap_or("").is_empty()
        && image_url.as_deref().unwrap_or("").is_empty()
    {
        return None;
    }

    let mut summary = Map::new();
    if !title.is_empty() {
        summary.insert("title".to_string(), Value::String(title));
    }
    if !page_url.is_empty() {
        summary.insert("page_url".to_string(), Value::String(page_url));
    }
    if !source.is_empty() {
        summary.insert("source".to_string(), Value::String(source));
    }
    if let Some(image_url) = image_url.filter(|value| !value.is_empty()) {
        summary.insert("image_url".to_string(), Value::String(image_url));
    }
    if let Some(thumbnail_url) = thumbnail_url.filter(|value| !value.is_empty()) {
        summary.insert("thumbnail_url".to_string(), Value::String(thumbnail_url));
    }
    if let Some(placeholder_url) = properties
        .map(|properties| string_field(properties, "placeholder"))
        .filter(|value| !value.is_empty())
    {
        summary.insert(
            "placeholder_url".to_string(),
            Value::String(placeholder_url),
        );
    }
    if let Some(width) = properties
        .and_then(|properties| properties.get("width"))
        .cloned()
    {
        if width.is_number() {
            summary.insert("width".to_string(), width);
        }
    }
    if let Some(height) = properties
        .and_then(|properties| properties.get("height"))
        .cloned()
    {
        if height.is_number() {
            summary.insert("height".to_string(), height);
        }
    }
    if let Some(width) = thumbnail
        .and_then(|thumbnail| thumbnail.get("width"))
        .cloned()
    {
        if width.is_number() {
            summary.insert("thumbnail_width".to_string(), width);
        }
    }
    if let Some(height) = thumbnail
        .and_then(|thumbnail| thumbnail.get("height"))
        .cloned()
    {
        if height.is_number() {
            summary.insert("thumbnail_height".to_string(), height);
        }
    }
    if !confidence.is_empty() {
        summary.insert("confidence".to_string(), Value::String(confidence));
    }
    Some(Value::Object(summary))
}

fn string_field(result: &Map<String, Value>, key: &str) -> String {
    result
        .get(key)
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim()
        .to_string()
}

fn image_search_query(query: &str, max_results: usize) -> Vec<(&'static str, String)> {
    vec![
        ("q", query.to_string()),
        ("count", max_results.clamp(1, 200).to_string()),
        ("safesearch", "strict".to_string()),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model_config::{ModelCapability, ProviderType, RetryMode, TokenEstimatorType};

    #[test]
    fn sends_brave_image_search_request_and_compacts_response() {
        let mut server = mockito::Server::new();
        let _mock = server
            .mock("GET", "/res/v1/images/search")
            .match_header("x-subscription-token", "brave-secret")
            .match_query(mockito::Matcher::AllOf(vec![
                mockito::Matcher::UrlEncoded("q".to_string(), "rust logos".to_string()),
                mockito::Matcher::UrlEncoded("count".to_string(), "200".to_string()),
                mockito::Matcher::UrlEncoded("safesearch".to_string(), "strict".to_string()),
            ]))
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(
                r#"{
                    "type": "images",
                    "results": [
                        {
                            "type": "image_result",
                            "title": "Rust Logo",
                            "url": "https://example.com/rust-logo",
                            "source": "example.com",
                            "thumbnail": {
                                "src": "https://imgs.search.brave.com/thumb",
                                "width": 500,
                                "height": 320
                            },
                            "properties": {
                                "url": "https://example.com/rust-logo.png",
                                "placeholder": "https://imgs.search.brave.com/placeholder",
                                "width": 1600,
                                "height": 1024
                            },
                            "confidence": "high"
                        }
                    ],
                    "extra": {
                        "might_be_offensive": false
                    }
                }"#,
            )
            .create();
        std::env::set_var("BRAVE_SEARCH_API_KEY_TEST", "brave-secret");

        let result = BraveSearchImageProvider::new()
            .search_images(
                &test_model_config(format!("{}/res/v1/images/search", server.url())),
                "rust logos",
                250,
            )
            .expect("image search should succeed");

        std::env::remove_var("BRAVE_SEARCH_API_KEY_TEST");
        assert_eq!(result["citations"][0], "https://example.com/rust-logo");
        assert_eq!(
            result["results"][0]["image_url"],
            "https://example.com/rust-logo.png"
        );
        assert_eq!(
            result["results"][0]["thumbnail_url"],
            "https://imgs.search.brave.com/thumb"
        );
        assert!(result.get("answer").is_none());
    }

    fn test_model_config(url: String) -> ModelConfig {
        ModelConfig {
            provider_type: ProviderType::BraveSearchImage,
            model_name: "brave-image-search".to_string(),
            url,
            api_key_env: "BRAVE_SEARCH_API_KEY_TEST".to_string(),
            capabilities: vec![ModelCapability::WebSearch],
            token_max_context: 0,
            max_tokens: 0,
            cache_timeout: 0,
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
