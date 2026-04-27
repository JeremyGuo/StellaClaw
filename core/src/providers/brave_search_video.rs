use std::time::Duration;

use reqwest::blocking::Client;
use serde_json::{json, Map, Value};

use crate::{
    model_config::ModelConfig,
    session_actor::{ChatMessage, ChatMessageItem, ChatRole, ContextItem},
};

use super::{ProviderBackend, ProviderError, ProviderRequest};

#[derive(Debug, Default)]
pub struct BraveSearchVideoProvider;

impl BraveSearchVideoProvider {
    pub fn new() -> Self {
        Self
    }

    pub fn search_videos(
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
            .query(&[
                ("q", query.to_string()),
                ("count", max_results.clamp(1, 50).to_string()),
                ("safesearch", "moderate".to_string()),
            ])
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
        Ok(parse_brave_video_search_response(
            &value,
            query,
            max_results.clamp(1, 50),
        ))
    }
}

impl ProviderBackend for BraveSearchVideoProvider {
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
                    "Brave video search provider request did not include a query message"
                        .to_string(),
                )
            })?;
        let result = self.search_videos(model_config, &query, 5)?;
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

pub fn parse_brave_video_search_response(value: &Value, query: &str, max_results: usize) -> Value {
    let results = value
        .get("results")
        .and_then(Value::as_array)
        .map(|results| {
            results
                .iter()
                .filter_map(brave_video_result_summary)
                .take(max_results)
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    let citations = results
        .iter()
        .filter_map(|result| result.get("url").cloned())
        .collect::<Vec<_>>();
    let answer = if results.is_empty() {
        "No video results returned.".to_string()
    } else {
        results
            .iter()
            .enumerate()
            .map(|(index, result)| {
                let title = result
                    .get("title")
                    .and_then(Value::as_str)
                    .unwrap_or("Untitled video");
                let url = result.get("url").and_then(Value::as_str).unwrap_or("");
                let description = result
                    .get("description")
                    .and_then(Value::as_str)
                    .unwrap_or("");
                let duration = result.get("duration").and_then(Value::as_str).unwrap_or("");
                let creator = result.get("creator").and_then(Value::as_str).unwrap_or("");

                let mut lines = vec![format!("{}. {}", index + 1, title)];
                if !url.is_empty() {
                    lines.push(format!("URL: {url}"));
                }
                if !creator.is_empty() {
                    lines.push(format!("Creator: {creator}"));
                }
                if !duration.is_empty() {
                    lines.push(format!("Duration: {duration}"));
                }
                if !description.is_empty() {
                    lines.push(format!("Snippet: {description}"));
                }
                lines.join("\n")
            })
            .collect::<Vec<_>>()
            .join("\n\n")
    };
    json!({
        "query": query,
        "answer": answer,
        "citations": citations,
        "results": results,
    })
}

fn brave_video_result_summary(result: &Value) -> Option<Value> {
    let result = result.as_object()?;
    let title = string_field(result, "title");
    let url = string_field(result, "url");
    let description = string_field(result, "description");
    if title.is_empty() && url.is_empty() && description.is_empty() {
        return None;
    }

    let mut summary = Map::new();
    insert_string(&mut summary, "title", title);
    insert_string(&mut summary, "url", url);
    insert_string(&mut summary, "description", description);
    insert_string(&mut summary, "age", string_field(result, "age"));
    insert_string(&mut summary, "page_age", string_field(result, "page_age"));
    if let Some(thumbnail) = result.get("thumbnail").and_then(Value::as_object) {
        insert_string(
            &mut summary,
            "thumbnail_url",
            string_field(thumbnail, "src"),
        );
        insert_string(
            &mut summary,
            "thumbnail_original_url",
            string_field(thumbnail, "original"),
        );
    }
    if let Some(video) = result.get("video").and_then(Value::as_object) {
        insert_string(&mut summary, "duration", string_field(video, "duration"));
        insert_string(&mut summary, "creator", string_field(video, "creator"));
        insert_string(&mut summary, "publisher", string_field(video, "publisher"));
        if let Some(views) = video.get("views").cloned() {
            if views.is_number() {
                summary.insert("views".to_string(), views);
            }
        }
        if let Some(requires_subscription) = video.get("requires_subscription").cloned() {
            if requires_subscription.is_boolean() {
                summary.insert("requires_subscription".to_string(), requires_subscription);
            }
        }
        if let Some(author) = video.get("author").and_then(Value::as_object) {
            insert_string(&mut summary, "author_name", string_field(author, "name"));
            insert_string(&mut summary, "author_url", string_field(author, "url"));
        }
    }
    Some(Value::Object(summary))
}

fn insert_string(summary: &mut Map<String, Value>, key: &str, value: String) {
    if !value.is_empty() {
        summary.insert(key.to_string(), Value::String(value));
    }
}

fn string_field(result: &Map<String, Value>, key: &str) -> String {
    result
        .get(key)
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim()
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model_config::{ModelCapability, ProviderType, RetryMode, TokenEstimatorType};

    #[test]
    fn sends_brave_video_search_request_and_compacts_response() {
        let mut server = mockito::Server::new();
        let _mock = server
            .mock("GET", "/res/v1/videos/search")
            .match_header("x-subscription-token", "brave-secret")
            .match_query(mockito::Matcher::AllOf(vec![
                mockito::Matcher::UrlEncoded("q".to_string(), "python tutorial".to_string()),
                mockito::Matcher::UrlEncoded("count".to_string(), "50".to_string()),
                mockito::Matcher::UrlEncoded("safesearch".to_string(), "moderate".to_string()),
            ]))
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(
                r#"{
                    "type": "videos",
                    "results": [
                        {
                            "type": "video_result",
                            "title": "Python Tutorial",
                            "url": "https://www.youtube.com/watch?v=abc",
                            "description": "Learn Python.",
                            "thumbnail": {
                                "src": "https://imgs.search.brave.com/video-thumb"
                            },
                            "video": {
                                "duration": "03:45:00",
                                "views": 1523000,
                                "creator": "freeCodeCamp",
                                "publisher": "YouTube",
                                "requires_subscription": false
                            }
                        }
                    ]
                }"#,
            )
            .create();
        std::env::set_var("BRAVE_SEARCH_API_KEY_TEST", "brave-secret");

        let result = BraveSearchVideoProvider::new()
            .search_videos(
                &test_model_config(format!("{}/res/v1/videos/search", server.url())),
                "python tutorial",
                60,
            )
            .expect("video search should succeed");

        std::env::remove_var("BRAVE_SEARCH_API_KEY_TEST");
        assert_eq!(
            result["citations"][0],
            "https://www.youtube.com/watch?v=abc"
        );
        assert_eq!(result["results"][0]["creator"], "freeCodeCamp");
        assert_eq!(result["results"][0]["views"], 1523000);
    }

    fn test_model_config(url: String) -> ModelConfig {
        ModelConfig {
            provider_type: ProviderType::BraveSearchVideo,
            model_name: "brave-video-search".to_string(),
            url,
            api_key_env: "BRAVE_SEARCH_API_KEY_TEST".to_string(),
            capabilities: vec![ModelCapability::WebSearch],
            token_max_context: 0,
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
