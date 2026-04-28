use std::time::Duration;

use reqwest::blocking::Client;
use serde_json::{json, Map, Value};

use crate::{
    model_config::ModelConfig,
    session_actor::{ChatMessage, ChatMessageItem, ChatRole, ContextItem},
};

use super::{ProviderBackend, ProviderError, ProviderRequest};

#[derive(Debug, Default)]
pub struct BraveSearchNewsProvider;

impl BraveSearchNewsProvider {
    pub fn new() -> Self {
        Self
    }

    pub fn search_news(
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
                ("safesearch", "strict".to_string()),
                ("extra_snippets", "true".to_string()),
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
        Ok(parse_brave_news_search_response(
            &value,
            query,
            max_results.clamp(1, 50),
        ))
    }
}

impl ProviderBackend for BraveSearchNewsProvider {
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
                    "Brave news search provider request did not include a query message"
                        .to_string(),
                )
            })?;
        let result = self.search_news(model_config, &query, 5)?;
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

pub fn parse_brave_news_search_response(value: &Value, query: &str, max_results: usize) -> Value {
    let results = value
        .get("results")
        .and_then(Value::as_array)
        .map(|results| {
            results
                .iter()
                .filter_map(brave_news_result_summary)
                .take(max_results)
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    let citations = results
        .iter()
        .filter_map(|result| result.get("url").cloned())
        .collect::<Vec<_>>();
    let answer = if results.is_empty() {
        "No news results returned.".to_string()
    } else {
        results
            .iter()
            .enumerate()
            .map(|(index, result)| {
                let title = result
                    .get("title")
                    .and_then(Value::as_str)
                    .unwrap_or("Untitled article");
                let url = result.get("url").and_then(Value::as_str).unwrap_or("");
                let description = result
                    .get("description")
                    .and_then(Value::as_str)
                    .unwrap_or("");
                let age = result.get("age").and_then(Value::as_str).unwrap_or("");

                let mut lines = vec![format!("{}. {}", index + 1, title)];
                if !url.is_empty() {
                    lines.push(format!("URL: {url}"));
                }
                if !age.is_empty() {
                    lines.push(format!("Age: {age}"));
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

fn brave_news_result_summary(result: &Value) -> Option<Value> {
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
    insert_string(
        &mut summary,
        "page_fetched",
        string_field(result, "page_fetched"),
    );
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
    if let Some(extra_snippets) = result.get("extra_snippets").and_then(Value::as_array) {
        let extra_snippets = extra_snippets
            .iter()
            .filter_map(Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(|value| Value::String(value.to_string()))
            .collect::<Vec<_>>();
        if !extra_snippets.is_empty() {
            summary.insert("extra_snippets".to_string(), Value::Array(extra_snippets));
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
    fn sends_brave_news_search_request_and_compacts_response() {
        let mut server = mockito::Server::new();
        let _mock = server
            .mock("GET", "/res/v1/news/search")
            .match_header("x-subscription-token", "brave-secret")
            .match_query(mockito::Matcher::AllOf(vec![
                mockito::Matcher::UrlEncoded("q".to_string(), "space exploration".to_string()),
                mockito::Matcher::UrlEncoded("count".to_string(), "50".to_string()),
                mockito::Matcher::UrlEncoded("safesearch".to_string(), "strict".to_string()),
                mockito::Matcher::UrlEncoded("extra_snippets".to_string(), "true".to_string()),
            ]))
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(
                r#"{
                    "type": "news",
                    "results": [
                        {
                            "type": "news_result",
                            "title": "New Space Mission",
                            "url": "https://news.example.com/space",
                            "description": "A new mission launched.",
                            "age": "2 hours ago",
                            "page_age": "2026-01-15T14:30:00",
                            "thumbnail": {
                                "src": "https://imgs.search.brave.com/news-thumb"
                            }
                        }
                    ]
                }"#,
            )
            .create();
        std::env::set_var("BRAVE_SEARCH_API_KEY_TEST", "brave-secret");

        let result = BraveSearchNewsProvider::new()
            .search_news(
                &test_model_config(format!("{}/res/v1/news/search", server.url())),
                "space exploration",
                60,
            )
            .expect("news search should succeed");

        std::env::remove_var("BRAVE_SEARCH_API_KEY_TEST");
        assert_eq!(result["citations"][0], "https://news.example.com/space");
        assert_eq!(result["results"][0]["age"], "2 hours ago");
        assert_eq!(
            result["results"][0]["thumbnail_url"],
            "https://imgs.search.brave.com/news-thumb"
        );
    }

    fn test_model_config(url: String) -> ModelConfig {
        ModelConfig {
            provider_type: ProviderType::BraveSearchNews,
            model_name: "brave-news-search".to_string(),
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
