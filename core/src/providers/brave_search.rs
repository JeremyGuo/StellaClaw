use std::time::Duration;

use reqwest::blocking::Client;
use serde_json::{json, Map, Value};

use crate::{
    model_config::ModelConfig,
    session_actor::{ChatMessage, ChatMessageItem, ChatRole, ContextItem},
};

use super::{Provider, ProviderError, ProviderRequest};

#[derive(Debug, Default)]
pub struct BraveSearchProvider;

impl BraveSearchProvider {
    pub fn new() -> Self {
        Self
    }

    pub fn search(
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
                ("count", max_results.clamp(1, 20).to_string()),
                ("result_filter", "web".to_string()),
                ("text_decorations", "false".to_string()),
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
        Ok(parse_brave_web_search_response(
            &value,
            query,
            max_results.clamp(1, 20),
        ))
    }
}

impl Provider for BraveSearchProvider {
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
                    "Brave search provider request did not include a query message".to_string(),
                )
            })?;
        let result = self.search(model_config, &query, 5)?;
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

pub fn parse_brave_web_search_response(value: &Value, query: &str, max_results: usize) -> Value {
    let results = value
        .get("web")
        .and_then(|web| web.get("results"))
        .and_then(Value::as_array)
        .map(|results| {
            results
                .iter()
                .filter_map(brave_web_result_summary)
                .take(max_results)
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    let citations = results
        .iter()
        .filter_map(|result| result.get("url").cloned())
        .collect::<Vec<_>>();
    let answer = if results.is_empty() {
        "No web results returned.".to_string()
    } else {
        results
            .iter()
            .enumerate()
            .map(|(index, result)| {
                let title = result
                    .get("title")
                    .and_then(Value::as_str)
                    .unwrap_or("Untitled result");
                let url = result.get("url").and_then(Value::as_str).unwrap_or("");
                let description = result
                    .get("description")
                    .and_then(Value::as_str)
                    .unwrap_or("");
                let extra = result
                    .get("extra_snippets")
                    .and_then(Value::as_array)
                    .map(|items| {
                        items
                            .iter()
                            .filter_map(Value::as_str)
                            .collect::<Vec<_>>()
                            .join(" | ")
                    })
                    .filter(|value| !value.is_empty());

                let mut lines = vec![format!("{}. {}", index + 1, title)];
                if !url.is_empty() {
                    lines.push(format!("URL: {url}"));
                }
                if !description.is_empty() {
                    lines.push(format!("Snippet: {description}"));
                }
                if let Some(extra) = extra {
                    lines.push(format!("Extra snippets: {extra}"));
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

fn brave_web_result_summary(result: &Value) -> Option<Value> {
    let result = result.as_object()?;
    let title = string_field(result, "title");
    let url = string_field(result, "url");
    let description = string_field(result, "description");
    if title.is_empty() && url.is_empty() && description.is_empty() {
        return None;
    }

    let mut summary = Map::new();
    if !title.is_empty() {
        summary.insert("title".to_string(), Value::String(title));
    }
    if !url.is_empty() {
        summary.insert("url".to_string(), Value::String(url));
    }
    if !description.is_empty() {
        summary.insert("description".to_string(), Value::String(description));
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
    fn sends_brave_search_request_and_compacts_response() {
        let mut server = mockito::Server::new();
        let _mock = server
            .mock("GET", "/res/v1/web/search")
            .match_header("x-subscription-token", "brave-secret")
            .match_query(mockito::Matcher::AllOf(vec![
                mockito::Matcher::UrlEncoded("q".to_string(), "rust async actors".to_string()),
                mockito::Matcher::UrlEncoded("count".to_string(), "20".to_string()),
                mockito::Matcher::UrlEncoded("result_filter".to_string(), "web".to_string()),
                mockito::Matcher::UrlEncoded("text_decorations".to_string(), "false".to_string()),
                mockito::Matcher::UrlEncoded("extra_snippets".to_string(), "true".to_string()),
            ]))
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(
                r#"{
                    "web": {
                        "results": [
                            {
                                "title": "Tokio Tutorial",
                                "url": "https://tokio.rs/tutorial",
                                "description": "Learn async Rust with Tokio.",
                                "extra_snippets": ["Covers tasks and channels."]
                            }
                        ]
                    }
                }"#,
            )
            .create();
        std::env::set_var("BRAVE_SEARCH_API_KEY_TEST", "brave-secret");

        let result = BraveSearchProvider::new()
            .search(
                &test_model_config(format!("{}/res/v1/web/search", server.url())),
                "rust async actors",
                50,
            )
            .expect("search should succeed");

        std::env::remove_var("BRAVE_SEARCH_API_KEY_TEST");
        assert_eq!(result["citations"][0], "https://tokio.rs/tutorial");
        assert_eq!(result["results"][0]["title"], "Tokio Tutorial");
        assert!(result["answer"]
            .as_str()
            .unwrap()
            .contains("Snippet: Learn async Rust with Tokio."));
    }

    fn test_model_config(url: String) -> ModelConfig {
        ModelConfig {
            provider_type: ProviderType::BraveSearch,
            model_name: "brave-web-search".to_string(),
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
