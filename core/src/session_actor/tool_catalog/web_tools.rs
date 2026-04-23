use std::time::Duration;

use regex::Regex;
use reqwest::header::{HeaderMap, HeaderName, HeaderValue};
use serde_json::{json, Map, Value};
use url::Url;

use super::{
    schema::{add_images_property, object_schema, properties},
    ToolBackend, ToolDefinition, ToolExecutionMode,
};
use crate::{
    model_config::{ModelConfig, ProviderType},
    providers::{BraveSearchProvider, ProviderError},
    session_actor::tool_runtime::{
        f64_arg_with_default, string_arg, usize_arg_with_default, LocalToolError,
    },
};

pub fn web_tool_definitions(enable_web_search: bool) -> Vec<ToolDefinition> {
    let mut tools = vec![ToolDefinition::new(
        "web_fetch",
        "Fetch a web page or HTTP resource and return a readable text body. If interrupted by a newer user message or timeout observation, cancel the in-flight fetch. The model must choose timeout_seconds.",
        object_schema(
            properties([
                ("url", json!({"type": "string"})),
                ("timeout_seconds", json!({"type": "number"})),
                ("max_chars", json!({"type": "integer"})),
                ("headers", json!({"type": "object"})),
            ]),
            &["url", "timeout_seconds"],
        ),
        ToolExecutionMode::Interruptible,
        ToolBackend::Local,
    )];

    if enable_web_search {
        let mut schema_properties = properties([
            ("query", json!({"type": "string"})),
            ("timeout_seconds", json!({"type": "number"})),
            ("max_results", json!({"type": "integer"})),
        ]);
        add_images_property(&mut schema_properties, false);
        tools.push(ToolDefinition::new(
            "web_search",
            "Search the web using the configured search provider and return an answer plus citations. If interrupted by a newer user message or timeout observation, this tool cancels the in-flight search result and returns immediately.",
            object_schema(schema_properties, &["query", "timeout_seconds"]),
            ToolExecutionMode::Interruptible,
            ToolBackend::Local,
        ));
    }

    tools
}

pub(crate) fn execute_web_tool(
    tool_name: &str,
    arguments: &Map<String, Value>,
    search_tool_model: Option<&ModelConfig>,
) -> Result<Option<Value>, LocalToolError> {
    let result = match tool_name {
        "web_fetch" => web_fetch(arguments)?,
        "web_search" => web_search(arguments, search_tool_model)?,
        _ => return Ok(None),
    };
    Ok(Some(result))
}

fn web_fetch(arguments: &Map<String, Value>) -> Result<Value, LocalToolError> {
    let url = string_arg(arguments, "url")?;
    let timeout_seconds = f64_arg_with_default(arguments, "timeout_seconds", 30.0)?;
    if !timeout_seconds.is_finite() || timeout_seconds <= 0.0 {
        return Err(LocalToolError::InvalidArguments(
            "timeout_seconds must be a positive finite number".to_string(),
        ));
    }
    let max_chars = usize_arg_with_default(arguments, "max_chars", 20_000)?;
    let headers = request_headers(arguments.get("headers"))?;

    let client = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs_f64(timeout_seconds))
        .build()
        .map_err(|error| LocalToolError::Io(format!("failed to build web client: {error}")))?;
    let response = client
        .get(&url)
        .headers(headers)
        .send()
        .map_err(|error| LocalToolError::Io(format!("web_fetch request failed: {error}")))?;

    let final_url = response.url().to_string();
    let status = response.status().as_u16();
    let content_type = response
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .map(ToOwned::to_owned);
    let body = response
        .text()
        .map_err(|error| LocalToolError::Io(format!("failed to read web_fetch body: {error}")))?;
    let (body, truncated) = truncate_chars(&body, max_chars);

    Ok(json!({
        "url": url,
        "final_url": final_url,
        "status": status,
        "content_type": content_type,
        "truncated": truncated,
        "body": body,
    }))
}

fn request_headers(value: Option<&Value>) -> Result<HeaderMap, LocalToolError> {
    let mut headers = HeaderMap::new();
    let Some(value) = value else {
        return Ok(headers);
    };
    let object = value
        .as_object()
        .ok_or_else(|| LocalToolError::InvalidArguments("headers must be an object".to_string()))?;

    for (name, value) in object {
        let value = value.as_str().ok_or_else(|| {
            LocalToolError::InvalidArguments(format!("header {name} must be a string"))
        })?;
        let name = HeaderName::from_bytes(name.as_bytes()).map_err(|error| {
            LocalToolError::InvalidArguments(format!("invalid header name {name}: {error}"))
        })?;
        let value = HeaderValue::from_str(value).map_err(|error| {
            LocalToolError::InvalidArguments(format!("invalid header value: {error}"))
        })?;
        headers.insert(name, value);
    }
    Ok(headers)
}

fn truncate_chars(text: &str, max_chars: usize) -> (String, bool) {
    let mut chars = text.chars();
    let truncated = text.chars().count() > max_chars;
    let body = chars.by_ref().take(max_chars).collect::<String>();
    (body, truncated)
}

fn web_search(
    arguments: &Map<String, Value>,
    search_tool_model: Option<&ModelConfig>,
) -> Result<Value, LocalToolError> {
    let query = string_arg(arguments, "query")?;
    let timeout_seconds = f64_arg_with_default(arguments, "timeout_seconds", 30.0)?;
    if !timeout_seconds.is_finite() || timeout_seconds <= 0.0 {
        return Err(LocalToolError::InvalidArguments(
            "timeout_seconds must be a positive finite number".to_string(),
        ));
    }
    let max_results = usize_arg_with_default(arguments, "max_results", 5)?;
    if let Some(search_tool_model) = search_tool_model {
        return search_with_provider(
            search_tool_model,
            arguments,
            &query,
            timeout_seconds,
            max_results.clamp(1, 20),
        );
    }
    let max_results = max_results.clamp(1, 10);

    let client = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs_f64(timeout_seconds))
        .user_agent("stellaclaw-core/0.1")
        .build()
        .map_err(|error| LocalToolError::Io(format!("failed to build web client: {error}")))?;

    if let Ok(base_url) = std::env::var("STELLACLAW_WEB_SEARCH_URL") {
        return web_search_json_endpoint(&client, &base_url, &query, max_results);
    }

    let body = client
        .get("https://duckduckgo.com/html/")
        .query(&[("q", query.as_str())])
        .send()
        .map_err(|error| LocalToolError::Io(format!("web_search request failed: {error}")))?
        .text()
        .map_err(|error| LocalToolError::Io(format!("failed to read web_search body: {error}")))?;
    Ok(json!({
        "query": query,
        "results": parse_duckduckgo_html_results(&body, max_results),
    }))
}

fn search_with_provider(
    model_config: &ModelConfig,
    arguments: &Map<String, Value>,
    query: &str,
    timeout_seconds: f64,
    max_results: usize,
) -> Result<Value, LocalToolError> {
    if arguments
        .get("images")
        .and_then(Value::as_array)
        .is_some_and(|images| !images.is_empty())
    {
        return Err(LocalToolError::InvalidArguments(
            "the configured web search provider does not support image inputs".to_string(),
        ));
    }

    match model_config.provider_type {
        ProviderType::BraveSearch => {
            let mut model_config = model_config.clone();
            model_config.conn_timeout = timeout_seconds.ceil().max(1.0) as u64;
            BraveSearchProvider::new()
                .search(&model_config, query, max_results)
                .map_err(provider_error_to_local_tool_error)
        }
        _ => Err(LocalToolError::InvalidArguments(format!(
            "unsupported web_search provider {:?}",
            model_config.provider_type
        ))),
    }
}

fn provider_error_to_local_tool_error(error: ProviderError) -> LocalToolError {
    match error {
        ProviderError::MissingApiKeyEnv(env) => LocalToolError::InvalidArguments(format!(
            "missing web search API key in environment variable {env}"
        )),
        error => LocalToolError::Io(format!("web_search provider request failed: {error}")),
    }
}

fn web_search_json_endpoint(
    client: &reqwest::blocking::Client,
    base_url: &str,
    query: &str,
    max_results: usize,
) -> Result<Value, LocalToolError> {
    let mut url = Url::parse(base_url).map_err(|error| {
        LocalToolError::InvalidArguments(format!("invalid web search URL: {error}"))
    })?;
    url.query_pairs_mut()
        .append_pair("q", query)
        .append_pair("query", query)
        .append_pair("max_results", &max_results.to_string());
    let value = client
        .get(url)
        .send()
        .map_err(|error| LocalToolError::Io(format!("web_search request failed: {error}")))?
        .json::<Value>()
        .map_err(|error| LocalToolError::Io(format!("failed to parse web_search JSON: {error}")))?;
    Ok(value)
}

fn parse_duckduckgo_html_results(body: &str, max_results: usize) -> Vec<Value> {
    let Ok(anchor_regex) =
        Regex::new(r#"(?s)<a[^>]*class="[^"]*result__a[^"]*"[^>]*href="([^"]+)"[^>]*>(.*?)</a>"#)
    else {
        return Vec::new();
    };
    let snippet_regex =
        Regex::new(r#"(?s)<a[^>]*class="[^"]*result__snippet[^"]*"[^>]*>(.*?)</a>"#).ok();
    let snippets = snippet_regex
        .as_ref()
        .map(|regex| {
            regex
                .captures_iter(body)
                .filter_map(|cap| cap.get(1).map(|value| strip_html(value.as_str())))
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();

    anchor_regex
        .captures_iter(body)
        .take(max_results)
        .enumerate()
        .map(|(index, cap)| {
            let url = cap
                .get(1)
                .map(|value| html_unescape(value.as_str()))
                .unwrap_or_default();
            let title = cap
                .get(2)
                .map(|value| strip_html(value.as_str()))
                .unwrap_or_default();
            json!({
                "title": title,
                "url": normalize_duckduckgo_url(&url),
                "snippet": snippets.get(index).cloned().unwrap_or_default(),
            })
        })
        .collect()
}

fn normalize_duckduckgo_url(url: &str) -> String {
    if let Ok(parsed) = Url::parse(url) {
        if parsed.domain() == Some("duckduckgo.com") {
            if let Some(target) = parsed
                .query_pairs()
                .find_map(|(key, value)| (key == "uddg").then(|| value.to_string()))
            {
                return target;
            }
        }
    }
    url.to_string()
}

fn strip_html(input: &str) -> String {
    let without_tags = Regex::new(r"<[^>]+>")
        .map(|regex| regex.replace_all(input, "").to_string())
        .unwrap_or_else(|_| input.to_string());
    html_unescape(&without_tags)
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

fn html_unescape(input: &str) -> String {
    input
        .replace("&amp;", "&")
        .replace("&quot;", "\"")
        .replace("&#x27;", "'")
        .replace("&#39;", "'")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model_config::{
        ModelCapability, ModelConfig, ProviderType, RetryMode, TokenEstimatorType,
    };

    struct EnvVarGuard {
        key: &'static str,
        previous: Option<std::ffi::OsString>,
    }

    impl EnvVarGuard {
        fn set(key: &'static str, value: &str) -> Self {
            let previous = std::env::var_os(key);
            std::env::set_var(key, value);
            Self { key, previous }
        }
    }

    impl Drop for EnvVarGuard {
        fn drop(&mut self) {
            match &self.previous {
                Some(value) => std::env::set_var(self.key, value),
                None => std::env::remove_var(self.key),
            }
        }
    }

    #[test]
    fn brave_web_search_uses_subscription_header_and_compacts_results() {
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
        let _env = EnvVarGuard::set("BRAVE_SEARCH_API_KEY_TEST", "brave-secret");
        let model = test_brave_model_config(format!("{}/res/v1/web/search", server.url()));
        let mut arguments = Map::new();
        arguments.insert(
            "query".to_string(),
            Value::String("rust async actors".to_string()),
        );
        arguments.insert("timeout_seconds".to_string(), json!(2.0));
        arguments.insert("max_results".to_string(), json!(50));

        let result = execute_web_tool("web_search", &arguments, Some(&model))
            .expect("web search should run")
            .expect("web search should return a value");

        assert_eq!(result["citations"][0], "https://tokio.rs/tutorial");
        assert_eq!(result["results"][0]["title"], "Tokio Tutorial");
        assert!(result["answer"]
            .as_str()
            .unwrap()
            .contains("Snippet: Learn async Rust with Tokio."));
    }

    #[test]
    fn brave_web_search_rejects_images() {
        let model =
            test_brave_model_config("https://api.search.brave.com/res/v1/web/search".to_string());
        let mut arguments = Map::new();
        arguments.insert("query".to_string(), Value::String("diagram".to_string()));
        arguments.insert("timeout_seconds".to_string(), json!(2.0));
        arguments.insert("images".to_string(), json!(["diagram.png"]));

        let error = execute_web_tool("web_search", &arguments, Some(&model))
            .expect_err("brave search should reject image inputs");

        assert!(error.to_string().contains("does not support image inputs"));
    }

    fn test_brave_model_config(url: String) -> ModelConfig {
        ModelConfig {
            provider_type: ProviderType::BraveSearch,
            model_name: "brave-web-search".to_string(),
            url,
            api_key_env: "BRAVE_SEARCH_API_KEY_TEST".to_string(),
            capabilities: vec![ModelCapability::WebSearch],
            token_max_context: 0,
            cache_timeout: 0,
            conn_timeout: 30,
            retry_mode: RetryMode::Once,
            reasoning: None,
            token_estimator_type: TokenEstimatorType::Local,
            multimodal_estimator: None,
            multimodal_input: None,
            token_estimator_url: None,
        }
    }
}
