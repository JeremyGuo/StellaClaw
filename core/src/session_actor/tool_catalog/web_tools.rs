use std::{io::Read, str::FromStr, thread, time::Duration};

use crossbeam_channel::select;
use regex::Regex;
use reqwest::header::{HeaderMap, HeaderName, HeaderValue};
use serde_json::{json, Map, Value};
use url::Url;

use super::{
    schema::{add_images_property, object_schema, properties},
    ToolBackend, ToolDefinition, ToolExecutionMode,
};
use crate::{
    model_config::{ModelCapability, ModelConfig, ProviderType},
    providers::{global_provider_fork_server, ProviderError, ProviderRequestOwned},
    session_actor::tool_runtime::{
        bool_arg_with_default, f64_arg_with_default, string_arg, usize_arg_with_default,
        LocalToolError, ToolExecutionContext,
    },
    session_actor::{ChatMessage, ChatMessageItem, ChatRole, ContextItem, SearchToolModels},
};

#[cfg(test)]
use crate::providers::{
    BraveSearchImageProvider, BraveSearchNewsProvider, BraveSearchProvider,
    BraveSearchVideoProvider,
};

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct WebSearchOptions {
    pub enabled: bool,
    pub image: bool,
    pub video: bool,
    pub news: bool,
}

pub fn web_tool_definitions(search_options: WebSearchOptions) -> Vec<ToolDefinition> {
    let mut tools = vec![ToolDefinition::new(
        "web_fetch",
        "Fetch an HTTP/HTTPS URL and return a structured response. Defaults are timeout_seconds=30, max_chars=20000, method=GET, format=auto. In auto format, HTML is converted to readable markdown-like text with page metadata and links; binary content returns metadata without a lossy body.",
        object_schema(
            properties([
                ("url", json!({"type": "string", "description": "HTTP or HTTPS URL to fetch."})),
                ("method", json!({"type": "string", "enum": ["GET", "HEAD"], "description": "HTTP method. Defaults to GET."})),
                ("timeout_seconds", json!({"type": "number", "minimum": 1, "maximum": 120, "description": "Request timeout in seconds. Defaults to 30."})),
                ("max_chars", json!({"type": "integer", "minimum": 0, "maximum": 100000, "description": "Maximum response body characters to return. Defaults to 20000."})),
                ("max_bytes", json!({"type": "integer", "minimum": 0, "maximum": 8388608, "description": "Maximum response body bytes to read before text extraction. Defaults to a bounded value derived from max_chars, capped at 8 MiB."})),
                ("format", json!({"type": "string", "enum": ["auto", "text", "raw"], "description": "auto strips HTML to readable text, text always strips HTML-like content, raw returns response text unchanged. Defaults to auto."})),
                ("user_agent", json!({"type": "string", "description": "Optional User-Agent override."})),
                ("headers", json!({"type": "object", "additionalProperties": {"type": "string"}})),
            ]),
            &["url"],
        ),
        ToolExecutionMode::Interruptible,
        ToolBackend::Local,
    )];

    if search_options.enabled {
        let mut schema_properties = properties([
            ("query", json!({"type": "string"})),
            ("timeout_seconds", json!({"type": "number"})),
            ("max_results", json!({"type": "integer"})),
            ("image", json!({"type": "boolean"})),
            ("video", json!({"type": "boolean"})),
            ("news", json!({"type": "boolean"})),
        ]);
        add_images_property(&mut schema_properties, false);
        let description = web_search_description(search_options);
        tools.push(ToolDefinition::new(
            "web_search",
            &description,
            object_schema(schema_properties, &["query", "timeout_seconds"]),
            ToolExecutionMode::Interruptible,
            ToolBackend::Local,
        ));
    }

    tools
}

fn web_search_description(options: WebSearchOptions) -> String {
    let mut supported = vec!["web"];
    if options.image {
        supported.push("image");
    }
    if options.video {
        supported.push("video");
    }
    if options.news {
        supported.push("news");
    }
    let unsupported = [
        (!options.image).then_some("image=true"),
        (!options.video).then_some("video=true"),
        (!options.news).then_some("news=true"),
    ]
    .into_iter()
    .flatten()
    .collect::<Vec<_>>()
    .join(", ");
    let unsupported = if unsupported.is_empty() {
        String::new()
    } else {
        format!(" This session does not support: {unsupported}.")
    };
    format!(
        "Search using the configured provider and return structured results plus citations. Supported result types: {}. Set at most one of image=true, video=true, or news=true; omit them for normal web results.{} If interrupted by a newer user message or timeout observation, this tool cancels the in-flight search result and returns immediately.",
        supported.join(", "),
        unsupported
    )
}

pub(crate) fn execute_web_tool(
    tool_name: &str,
    arguments: &Map<String, Value>,
    context: Option<&ToolExecutionContext<'_>>,
    search_tool_models: Option<&SearchToolModels>,
) -> Result<Option<Value>, LocalToolError> {
    let result = match tool_name {
        "web_fetch" => web_fetch(arguments)?,
        "web_search" => web_search(arguments, context, search_tool_models)?,
        _ => return Ok(None),
    };
    Ok(Some(result))
}

const MIN_FETCH_BODY_BYTES: usize = 1_048_576;
const MAX_FETCH_BODY_BYTES: usize = 8 * 1_048_576;
const MAX_FETCH_LINKS: usize = 50;

fn web_fetch(arguments: &Map<String, Value>) -> Result<Value, LocalToolError> {
    let url = string_arg(arguments, "url")?;
    let parsed_url = Url::parse(&url)
        .map_err(|error| LocalToolError::InvalidArguments(format!("invalid url: {error}")))?;
    if !matches!(parsed_url.scheme(), "http" | "https") {
        return Err(LocalToolError::InvalidArguments(
            "url must use http or https".to_string(),
        ));
    }
    let timeout_seconds = f64_arg_with_default(arguments, "timeout_seconds", 30.0)?;
    if !timeout_seconds.is_finite() || timeout_seconds <= 0.0 {
        return Err(LocalToolError::InvalidArguments(
            "timeout_seconds must be a positive finite number".to_string(),
        ));
    }
    let timeout_seconds = timeout_seconds.min(120.0);
    let max_chars = usize_arg_with_default(arguments, "max_chars", 20_000)?.min(100_000);
    let default_max_bytes = default_fetch_max_bytes(max_chars);
    let max_bytes = usize_arg_with_default(arguments, "max_bytes", default_max_bytes)?
        .min(MAX_FETCH_BODY_BYTES);
    let format = fetch_format(arguments.get("format"))?;
    let method = fetch_method(arguments.get("method"))?;
    let user_agent = arguments
        .get("user_agent")
        .and_then(Value::as_str)
        .filter(|value| !value.trim().is_empty())
        .unwrap_or("stellaclaw-core/0.1");
    let headers = request_headers(arguments.get("headers"))?;

    let client = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs_f64(timeout_seconds))
        .user_agent(user_agent)
        .build()
        .map_err(|error| LocalToolError::Io(format!("failed to build web client: {error}")))?;
    let response = client
        .request(method.clone(), parsed_url)
        .headers(headers)
        .send()
        .map_err(|error| LocalToolError::Io(format!("web_fetch request failed: {error}")))?;

    let final_url = response.url().to_string();
    let status = response.status().as_u16();
    let response_headers = response.headers().clone();
    let content_length = response.content_length();
    let content_type = response
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .map(ToOwned::to_owned);
    let (raw_body_bytes, body_truncated_by_bytes) =
        read_limited_response_body(response, method, max_bytes, content_length)?;
    let raw_body = String::from_utf8_lossy(&raw_body_bytes).into_owned();
    let body_format =
        resolved_fetch_body_format(format, content_type.as_deref(), &raw_body, &raw_body_bytes);
    let page_metadata = if body_format == FetchBodyFormat::Text
        && is_html_content(content_type.as_deref(), &raw_body)
    {
        PageMetadata::from_html(&raw_body, &final_url)
    } else {
        PageMetadata::default()
    };
    let body = match body_format {
        FetchBodyFormat::Binary => String::new(),
        FetchBodyFormat::Raw => raw_body,
        FetchBodyFormat::Text => html_to_readable_text(&raw_body, content_type.as_deref()),
    };
    let (body, body_truncated_by_chars) = truncate_chars(&body, max_chars);
    let truncated = body_truncated_by_bytes || body_truncated_by_chars;

    Ok(json!({
        "kind": "web_fetch_result",
        "url": url,
        "final_url": final_url,
        "redirected": final_url != url,
        "status": status,
        "ok": (200..300).contains(&status),
        "content_type": content_type,
        "content_length": content_length,
        "body_format": body_format.name(),
        "truncated": truncated,
        "body_truncated_by_bytes": body_truncated_by_bytes,
        "body_truncated_by_chars": body_truncated_by_chars,
        "max_chars": max_chars,
        "max_bytes": max_bytes,
        "bytes_read": raw_body_bytes.len(),
        "title": page_metadata.title,
        "description": page_metadata.description,
        "links": page_metadata.links,
        "headers": selected_response_headers(&response_headers),
        "body": body,
    }))
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum RequestedFetchFormat {
    Auto,
    Text,
    Raw,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum FetchBodyFormat {
    Binary,
    Text,
    Raw,
}

impl FetchBodyFormat {
    fn name(self) -> &'static str {
        match self {
            Self::Binary => "binary",
            Self::Text => "text",
            Self::Raw => "raw",
        }
    }
}

fn fetch_format(value: Option<&Value>) -> Result<RequestedFetchFormat, LocalToolError> {
    match value.and_then(Value::as_str).unwrap_or("auto") {
        "auto" => Ok(RequestedFetchFormat::Auto),
        "text" => Ok(RequestedFetchFormat::Text),
        "raw" => Ok(RequestedFetchFormat::Raw),
        other => Err(LocalToolError::InvalidArguments(format!(
            "unsupported web_fetch format {other}"
        ))),
    }
}

fn fetch_method(value: Option<&Value>) -> Result<reqwest::Method, LocalToolError> {
    let method = value.and_then(Value::as_str).unwrap_or("GET");
    match method {
        "GET" | "HEAD" => reqwest::Method::from_str(method).map_err(|error| {
            LocalToolError::InvalidArguments(format!("invalid web_fetch method {method}: {error}"))
        }),
        other => Err(LocalToolError::InvalidArguments(format!(
            "unsupported web_fetch method {other}"
        ))),
    }
}

fn resolved_fetch_body_format(
    requested: RequestedFetchFormat,
    content_type: Option<&str>,
    body: &str,
    body_bytes: &[u8],
) -> FetchBodyFormat {
    match requested {
        RequestedFetchFormat::Raw => FetchBodyFormat::Raw,
        RequestedFetchFormat::Text => FetchBodyFormat::Text,
        RequestedFetchFormat::Auto => {
            if is_html_content(content_type, body) {
                FetchBodyFormat::Text
            } else if is_textual_content_type(content_type) || looks_like_text(body_bytes) {
                FetchBodyFormat::Raw
            } else {
                FetchBodyFormat::Binary
            }
        }
    }
}

fn default_fetch_max_bytes(max_chars: usize) -> usize {
    if max_chars == 0 {
        return 0;
    }
    max_chars
        .saturating_mul(8)
        .max(MIN_FETCH_BODY_BYTES)
        .min(MAX_FETCH_BODY_BYTES)
}

fn read_limited_response_body(
    response: reqwest::blocking::Response,
    method: reqwest::Method,
    max_bytes: usize,
    content_length: Option<u64>,
) -> Result<(Vec<u8>, bool), LocalToolError> {
    if method == reqwest::Method::HEAD || max_bytes == 0 {
        return Ok((Vec::new(), content_length.is_some_and(|length| length > 0)));
    }
    let mut body = Vec::new();
    let limit = max_bytes.saturating_add(1) as u64;
    response
        .take(limit)
        .read_to_end(&mut body)
        .map_err(|error| LocalToolError::Io(format!("failed to read web_fetch body: {error}")))?;
    let truncated = body.len() > max_bytes;
    if truncated {
        body.truncate(max_bytes);
    }
    Ok((body, truncated))
}

#[derive(Default)]
struct PageMetadata {
    title: Option<String>,
    description: Option<String>,
    links: Vec<Value>,
}

impl PageMetadata {
    fn from_html(html: &str, base_url: &str) -> Self {
        Self {
            title: extract_html_title(html),
            description: extract_meta_description(html),
            links: extract_links(html, base_url, MAX_FETCH_LINKS),
        }
    }
}

fn html_to_readable_text(input: &str, content_type: Option<&str>) -> String {
    if !is_html_content(content_type, input) {
        return normalize_text_lines(&html_unescape(input));
    }
    let without_scripts = Regex::new(
        r"(?is)<script[^>]*>.*?</script>|<style[^>]*>.*?</style>|<noscript[^>]*>.*?</noscript>",
    )
    .map(|regex| regex.replace_all(input, " ").to_string())
    .unwrap_or_else(|_| input.to_string());
    let without_head = Regex::new(r"(?is)<head[^>]*>.*?</head>")
        .map(|regex| regex.replace_all(&without_scripts, " ").to_string())
        .unwrap_or(without_scripts);
    let with_blocks = Regex::new(
        r"(?is)</?(article|section|main|header|footer|nav|aside|div|p|br|hr|blockquote|pre|table|thead|tbody|tr|h[1-6])[^>]*>",
    )
    .map(|regex| regex.replace_all(&without_head, "\n\n").to_string())
    .unwrap_or(without_head);
    let with_list_items = Regex::new(r"(?is)<li[^>]*>")
        .map(|regex| regex.replace_all(&with_blocks, "\n- ").to_string())
        .unwrap_or(with_blocks);
    let with_link_text = replace_anchors_with_text(&with_list_items);
    let without_tags = Regex::new(r"(?is)<[^>]+>")
        .map(|regex| regex.replace_all(&with_link_text, " ").to_string())
        .unwrap_or(with_link_text);
    normalize_text_lines(&html_unescape(&without_tags))
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
    context: Option<&ToolExecutionContext<'_>>,
    search_tool_models: Option<&SearchToolModels>,
) -> Result<Value, LocalToolError> {
    let query = string_arg(arguments, "query")?;
    let timeout_seconds = f64_arg_with_default(arguments, "timeout_seconds", 30.0)?;
    if !timeout_seconds.is_finite() || timeout_seconds <= 0.0 {
        return Err(LocalToolError::InvalidArguments(
            "timeout_seconds must be a positive finite number".to_string(),
        ));
    }
    let max_results = usize_arg_with_default(arguments, "max_results", 5)?;
    let vertical = requested_search_vertical(arguments)?;
    if vertical != SearchVertical::Web {
        let Some(search_tool_models) = search_tool_models else {
            return Err(LocalToolError::InvalidArguments(format!(
                "web_search {} results require a configured provider",
                vertical.name()
            )));
        };
        return search_with_vertical_provider(
            search_tool_models,
            vertical,
            &query,
            context,
            timeout_seconds,
            max_results,
        );
    }
    if let Some(search_tool_model) = search_tool_models.and_then(|models| models.web.as_ref()) {
        return search_with_provider(
            search_tool_model,
            arguments,
            &query,
            context,
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
    context: Option<&ToolExecutionContext<'_>>,
    timeout_seconds: f64,
    max_results: usize,
) -> Result<Value, LocalToolError> {
    if !model_config.supports(ModelCapability::WebSearch) {
        return Err(LocalToolError::InvalidArguments(
            "the configured search provider does not have web_search capability".to_string(),
        ));
    }
    if arguments
        .get("images")
        .and_then(Value::as_array)
        .is_some_and(|images| !images.is_empty())
    {
        return Err(LocalToolError::InvalidArguments(
            "the configured web search provider does not support image inputs".to_string(),
        ));
    }

    if model_config.provider_type != ProviderType::BraveSearch {
        return Err(LocalToolError::InvalidArguments(format!(
            "unsupported web_search provider {:?}",
            model_config.provider_type
        )));
    }
    let mut model_config = model_config.clone();
    model_config.request_timeout = timeout_seconds.ceil().max(1.0) as u64;
    search_with_provider_worker(&model_config, query, max_results, context)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SearchVertical {
    Web,
    Image,
    Video,
    News,
}

impl SearchVertical {
    fn name(self) -> &'static str {
        match self {
            SearchVertical::Web => "web",
            SearchVertical::Image => "image",
            SearchVertical::Video => "video",
            SearchVertical::News => "news",
        }
    }
}

fn requested_search_vertical(
    arguments: &Map<String, Value>,
) -> Result<SearchVertical, LocalToolError> {
    let image = bool_arg_with_default(arguments, "image", false)?;
    let video = bool_arg_with_default(arguments, "video", false)?;
    let news = bool_arg_with_default(arguments, "news", false)?;
    let requested = [image, video, news]
        .into_iter()
        .filter(|value| *value)
        .count();
    if requested > 1 {
        return Err(LocalToolError::InvalidArguments(
            "set at most one of image, video, or news to true".to_string(),
        ));
    }
    Ok(if image {
        SearchVertical::Image
    } else if video {
        SearchVertical::Video
    } else if news {
        SearchVertical::News
    } else {
        SearchVertical::Web
    })
}

fn search_with_vertical_provider(
    models: &SearchToolModels,
    vertical: SearchVertical,
    query: &str,
    context: Option<&ToolExecutionContext<'_>>,
    timeout_seconds: f64,
    max_results: usize,
) -> Result<Value, LocalToolError> {
    let model_config = match vertical {
        SearchVertical::Web => models.web.as_ref(),
        SearchVertical::Image => models.image.as_ref(),
        SearchVertical::Video => models.video.as_ref(),
        SearchVertical::News => models.news.as_ref(),
    }
    .ok_or_else(|| {
        LocalToolError::InvalidArguments(format!(
            "web_search {} results are not configured in this session",
            vertical.name()
        ))
    })?;
    if !model_config.supports(ModelCapability::WebSearch) {
        return Err(LocalToolError::InvalidArguments(format!(
            "the configured {} search provider does not have web_search capability",
            vertical.name()
        )));
    }
    let max_results = match (vertical, &model_config.provider_type) {
        (SearchVertical::Image, ProviderType::BraveSearchImage) => max_results.clamp(1, 200),
        (SearchVertical::Video, ProviderType::BraveSearchVideo) => max_results.clamp(1, 50),
        (SearchVertical::News, ProviderType::BraveSearchNews) => max_results.clamp(1, 50),
        _ => {
            return Err(LocalToolError::InvalidArguments(format!(
                "unsupported web_search {} provider {:?}",
                vertical.name(),
                model_config.provider_type,
            )))
        }
    };
    let mut model_config = model_config.clone();
    model_config.request_timeout = timeout_seconds.ceil().max(1.0) as u64;
    search_with_provider_worker(&model_config, query, max_results, context)
}

fn search_with_provider_worker(
    model_config: &ModelConfig,
    query: &str,
    max_results: usize,
    context: Option<&ToolExecutionContext<'_>>,
) -> Result<Value, LocalToolError> {
    #[cfg(test)]
    if context.is_none() {
        return search_with_provider_direct(model_config, query, max_results);
    }

    let fork_server = match global_provider_fork_server() {
        Ok(fork_server) => fork_server,
        Err(error) => {
            #[cfg(test)]
            {
                let _ = error;
                return search_with_provider_direct(model_config, query, max_results);
            }
            #[cfg(not(test))]
            {
                return Err(provider_error_to_local_tool_error(error));
            }
        }
    };

    let messages = vec![ChatMessage::new(
        ChatRole::User,
        vec![ChatMessageItem::Context(ContextItem {
            text: json!({
                "query": query,
                "max_results": max_results,
            })
            .to_string(),
        })],
    )];
    let handle = fork_server
        .start(model_config.clone(), ProviderRequestOwned::new(messages))
        .map_err(provider_error_to_local_tool_error)?;
    let abort_handle = handle.abort_handle();
    let cancel_rx = context
        .map(|context| context.cancel_token.cancel_rx())
        .unwrap_or_else(crossbeam_channel::never);
    let (result_tx, result_rx) = crossbeam_channel::bounded(1);
    thread::spawn(move || {
        let _ = result_tx.send(handle.wait());
    });

    select! {
        recv(result_rx) -> result => provider_worker_result_to_value(result),
        recv(cancel_rx) -> _ => {
            if let Ok(result) = result_rx.try_recv() {
                return provider_worker_result_to_value(Ok(result));
            }
            let _ = abort_handle.abort();
            match result_rx.recv() {
                Ok(Ok(message)) => provider_message_to_json_value(message),
                Ok(Err(_)) | Err(_) => Ok(json!({
                    "status": "interrupted",
                    "reason": "tool_interrupted",
                })),
            }
        }
    }
}

fn provider_worker_result_to_value(
    result: Result<Result<ChatMessage, ProviderError>, crossbeam_channel::RecvError>,
) -> Result<Value, LocalToolError> {
    let message = result
        .map_err(|_| LocalToolError::Io("web_search provider worker stopped".to_string()))?
        .map_err(provider_error_to_local_tool_error)?;
    provider_message_to_json_value(message)
}

fn provider_message_to_json_value(message: ChatMessage) -> Result<Value, LocalToolError> {
    let mut text = Vec::new();
    for item in message.data {
        match item {
            ChatMessageItem::Context(context) => text.push(context.text),
            ChatMessageItem::ToolResult(result) => {
                if let Some(structured) = result.result.structured {
                    return Ok(structured);
                }
                let rendered = crate::session_actor::tool_result_text(&result);
                if !rendered.trim().is_empty() {
                    text.push(rendered);
                }
            }
            _ => {}
        }
    }
    let text = text.join("\n");
    serde_json::from_str::<Value>(&text).map_err(|error| {
        LocalToolError::Io(format!(
            "web_search provider returned non-JSON result: {error}"
        ))
    })
}

#[cfg(test)]
fn search_with_provider_direct(
    model_config: &ModelConfig,
    query: &str,
    max_results: usize,
) -> Result<Value, LocalToolError> {
    match model_config.provider_type {
        ProviderType::BraveSearch => BraveSearchProvider::new()
            .search(model_config, query, max_results.clamp(1, 20))
            .map_err(provider_error_to_local_tool_error),
        ProviderType::BraveSearchImage => BraveSearchImageProvider::new()
            .search_images(model_config, query, max_results.clamp(1, 200))
            .map_err(provider_error_to_local_tool_error),
        ProviderType::BraveSearchVideo => BraveSearchVideoProvider::new()
            .search_videos(model_config, query, max_results.clamp(1, 50))
            .map_err(provider_error_to_local_tool_error),
        ProviderType::BraveSearchNews => BraveSearchNewsProvider::new()
            .search_news(model_config, query, max_results.clamp(1, 50))
            .map_err(provider_error_to_local_tool_error),
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

fn is_html_content(content_type: Option<&str>, body: &str) -> bool {
    content_type.is_some_and(|value| value.to_ascii_lowercase().contains("html"))
        || body
            .trim_start()
            .to_ascii_lowercase()
            .starts_with("<!doctype")
        || body.trim_start().to_ascii_lowercase().starts_with("<html")
}

fn is_textual_content_type(content_type: Option<&str>) -> bool {
    let Some(content_type) = content_type else {
        return false;
    };
    let content_type = content_type.to_ascii_lowercase();
    content_type.starts_with("text/")
        || content_type.contains("json")
        || content_type.contains("xml")
        || content_type.contains("javascript")
        || content_type.contains("ecmascript")
        || content_type.contains("x-www-form-urlencoded")
        || content_type.contains("csv")
        || content_type.contains("yaml")
        || content_type.contains("svg")
}

fn looks_like_text(bytes: &[u8]) -> bool {
    if bytes.is_empty() {
        return true;
    }
    if std::str::from_utf8(bytes).is_ok() {
        return true;
    }
    if bytes.iter().any(|byte| *byte == 0) {
        return false;
    }
    let control = bytes
        .iter()
        .filter(|byte| matches!(byte, 0..=8 | 11 | 12 | 14..=31))
        .count();
    control.saturating_mul(100) / bytes.len() < 2
}

fn selected_response_headers(headers: &HeaderMap) -> Map<String, Value> {
    let mut selected = Map::new();
    for name in [
        reqwest::header::CACHE_CONTROL,
        reqwest::header::CONTENT_DISPOSITION,
        reqwest::header::ETAG,
        reqwest::header::LAST_MODIFIED,
        reqwest::header::LOCATION,
    ] {
        if let Some(value) = headers.get(&name).and_then(|value| value.to_str().ok()) {
            selected.insert(name.as_str().to_string(), Value::String(value.to_string()));
        }
    }
    selected
}

fn extract_html_title(html: &str) -> Option<String> {
    Regex::new(r"(?is)<title[^>]*>(.*?)</title>")
        .ok()
        .and_then(|regex| regex.captures(html))
        .and_then(|captures| captures.get(1).map(|value| strip_html(value.as_str())))
        .filter(|value| !value.trim().is_empty())
}

fn extract_meta_description(html: &str) -> Option<String> {
    for pattern in [
        r#"(?is)<meta[^>]*(?:name|property)\s*=\s*["'](?:description|og:description|twitter:description)["'][^>]*content\s*=\s*["']([^"']*)["'][^>]*>"#,
        r#"(?is)<meta[^>]*content\s*=\s*["']([^"']*)["'][^>]*(?:name|property)\s*=\s*["'](?:description|og:description|twitter:description)["'][^>]*>"#,
    ] {
        if let Some(description) = Regex::new(pattern)
            .ok()
            .and_then(|regex| regex.captures(html))
            .and_then(|captures| captures.get(1).map(|value| html_unescape(value.as_str())))
            .map(|value| normalize_text_lines(&value))
            .filter(|value| !value.is_empty())
        {
            return Some(description);
        }
    }
    None
}

fn extract_links(html: &str, base_url: &str, max_links: usize) -> Vec<Value> {
    let Some(anchor_regex) =
        Regex::new(r#"(?is)<a\b[^>]*href\s*=\s*(?:"([^"]*)"|'([^']*)'|([^\s>]+))[^>]*>(.*?)</a>"#)
            .ok()
    else {
        return Vec::new();
    };
    let base = Url::parse(base_url).ok();
    anchor_regex
        .captures_iter(html)
        .filter_map(|captures| {
            let href = captures
                .get(1)
                .or_else(|| captures.get(2))
                .or_else(|| captures.get(3))?
                .as_str()
                .trim();
            let url = normalize_page_link(href, base.as_ref())?;
            let text = captures
                .get(4)
                .map(|value| strip_html(value.as_str()))
                .unwrap_or_default();
            (!text.is_empty() || !url.is_empty()).then(|| {
                json!({
                    "text": text,
                    "url": url,
                })
            })
        })
        .take(max_links)
        .collect()
}

fn normalize_page_link(href: &str, base_url: Option<&Url>) -> Option<String> {
    let href = html_unescape(href);
    let href = href.trim();
    if href.is_empty() || href.starts_with('#') {
        return None;
    }
    let parsed = Url::parse(href)
        .ok()
        .or_else(|| base_url.and_then(|base| base.join(href).ok()))?;
    matches!(parsed.scheme(), "http" | "https").then(|| parsed.to_string())
}

fn replace_anchors_with_text(html: &str) -> String {
    let Some(anchor_regex) =
        Regex::new(r#"(?is)<a\b[^>]*href\s*=\s*(?:"([^"]*)"|'([^']*)'|([^\s>]+))[^>]*>(.*?)</a>"#)
            .ok()
    else {
        return html.to_string();
    };
    anchor_regex
        .replace_all(html, |captures: &regex::Captures<'_>| {
            let text = captures
                .get(4)
                .map(|value| strip_html(value.as_str()))
                .unwrap_or_default();
            if text.is_empty() {
                captures
                    .get(0)
                    .map(|value| value.as_str())
                    .unwrap_or("")
                    .to_string()
            } else {
                text
            }
        })
        .to_string()
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
    let named = input
        .replace("&amp;", "&")
        .replace("&quot;", "\"")
        .replace("&apos;", "'")
        .replace("&#x27;", "'")
        .replace("&#39;", "'")
        .replace("&nbsp;", " ")
        .replace("&lt;", "<")
        .replace("&gt;", ">");
    let decoded = Regex::new(r"&#(x[0-9a-fA-F]+|[0-9]+);")
        .map(|regex| {
            regex
                .replace_all(&named, |captures: &regex::Captures<'_>| {
                    let raw = captures.get(1).map(|value| value.as_str()).unwrap_or("");
                    let parsed = raw
                        .strip_prefix('x')
                        .or_else(|| raw.strip_prefix('X'))
                        .map(|hex| u32::from_str_radix(hex, 16))
                        .unwrap_or_else(|| raw.parse::<u32>());
                    parsed
                        .ok()
                        .and_then(char::from_u32)
                        .map(|value| value.to_string())
                        .unwrap_or_else(|| {
                            captures
                                .get(0)
                                .map(|value| value.as_str())
                                .unwrap_or("")
                                .to_string()
                        })
                })
                .to_string()
        })
        .unwrap_or(named);
    decoded
}

fn normalize_text_lines(input: &str) -> String {
    let mut lines = Vec::new();
    let mut previous_blank = true;
    for raw_line in input.lines() {
        let line = raw_line.split_whitespace().collect::<Vec<_>>().join(" ");
        if line.is_empty() {
            if !previous_blank {
                lines.push(String::new());
            }
            previous_blank = true;
            continue;
        }
        lines.push(line);
        previous_blank = false;
    }
    while lines.last().is_some_and(|line| line.is_empty()) {
        lines.pop();
    }
    lines.join("\n")
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
    fn web_fetch_defaults_and_strips_html() {
        let mut server = mockito::Server::new();
        let _mock = server
            .mock("GET", "/doc")
            .match_header("user-agent", "stellaclaw-core/0.1")
            .with_status(200)
            .with_header("content-type", "text/html")
            .with_body("<html><head><title>Example Doc</title><meta name=\"description\" content=\"A &amp; B\"><style>.x{}</style></head><body><h1>Hello</h1><script>ignore()</script><p>World &amp; docs</p><a href=\"/next\">Next page</a></body></html>")
            .create();
        let mut arguments = Map::new();
        arguments.insert(
            "url".to_string(),
            Value::String(format!("{}/doc", server.url())),
        );

        let result = web_fetch(&arguments).expect("fetch should succeed");

        assert_eq!(result["kind"], "web_fetch_result");
        assert_eq!(result["status"], 200);
        assert_eq!(result["ok"], true);
        assert_eq!(result["body_format"], "text");
        assert_eq!(result["title"], "Example Doc");
        assert_eq!(result["description"], "A & B");
        assert_eq!(result["links"][0]["text"], "Next page");
        assert_eq!(result["links"][0]["url"], format!("{}/next", server.url()));
        assert_eq!(result["body"], "Hello\n\nWorld & docs\n\nNext page");
    }

    #[test]
    fn web_fetch_raw_format_preserves_html() {
        let mut server = mockito::Server::new();
        let _mock = server
            .mock("GET", "/raw")
            .with_status(200)
            .with_header("content-type", "text/html")
            .with_body("<h1>Raw</h1>")
            .create();
        let mut arguments = Map::new();
        arguments.insert(
            "url".to_string(),
            Value::String(format!("{}/raw", server.url())),
        );
        arguments.insert("format".to_string(), Value::String("raw".to_string()));

        let result = web_fetch(&arguments).expect("fetch should succeed");

        assert_eq!(result["body_format"], "raw");
        assert_eq!(result["body"], "<h1>Raw</h1>");
    }

    #[test]
    fn web_fetch_caps_body_bytes_before_text_processing() {
        let mut server = mockito::Server::new();
        let _mock = server
            .mock("GET", "/large")
            .with_status(200)
            .with_header("content-type", "text/plain")
            .with_body("abcdef")
            .create();
        let mut arguments = Map::new();
        arguments.insert(
            "url".to_string(),
            Value::String(format!("{}/large", server.url())),
        );
        arguments.insert("max_bytes".to_string(), json!(3));

        let result = web_fetch(&arguments).expect("fetch should succeed");

        assert_eq!(result["body_format"], "raw");
        assert_eq!(result["body"], "abc");
        assert_eq!(result["body_truncated_by_bytes"], true);
        assert_eq!(result["truncated"], true);
        assert_eq!(result["bytes_read"], 3);
    }

    #[test]
    fn web_fetch_binary_auto_returns_metadata_without_lossy_body() {
        let mut server = mockito::Server::new();
        let _mock = server
            .mock("GET", "/image.png")
            .with_status(200)
            .with_header("content-type", "image/png")
            .with_header("etag", "\"abc\"")
            .with_body(vec![0x89, b'P', b'N', b'G', 0, 1, 2, 3])
            .create();
        let mut arguments = Map::new();
        arguments.insert(
            "url".to_string(),
            Value::String(format!("{}/image.png", server.url())),
        );

        let result = web_fetch(&arguments).expect("fetch should succeed");

        assert_eq!(result["body_format"], "binary");
        assert_eq!(result["body"], "");
        assert_eq!(result["bytes_read"], 8);
        assert_eq!(result["headers"]["etag"], "\"abc\"");
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

        let models = SearchToolModels {
            web: Some(model),
            ..SearchToolModels::default()
        };
        let result = execute_web_tool("web_search", &arguments, None, Some(&models))
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

        let models = SearchToolModels {
            web: Some(model),
            ..SearchToolModels::default()
        };
        let error = execute_web_tool("web_search", &arguments, None, Some(&models))
            .expect_err("brave search should reject image inputs");

        assert!(error.to_string().contains("does not support image inputs"));
    }

    #[test]
    fn brave_image_search_uses_subscription_header_and_compacts_results() {
        let mut server = mockito::Server::new();
        let _mock = server
            .mock("GET", "/res/v1/images/search")
            .match_header("x-subscription-token", "brave-secret")
            .match_query(mockito::Matcher::AllOf(vec![
                mockito::Matcher::UrlEncoded("q".to_string(), "architecture".to_string()),
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
                            "title": "Modern Building",
                            "url": "https://example.com/building",
                            "thumbnail": {
                                "src": "https://imgs.search.brave.com/thumb",
                                "width": 500,
                                "height": 300
                            },
                            "properties": {
                                "url": "https://example.com/building.jpg",
                                "width": 1200,
                                "height": 720
                            }
                        }
                    ],
                    "extra": {}
                }"#,
            )
            .create();
        let _env = EnvVarGuard::set("BRAVE_SEARCH_API_KEY_TEST", "brave-secret");
        let model = test_brave_image_model_config(format!("{}/res/v1/images/search", server.url()));
        let mut arguments = Map::new();
        arguments.insert(
            "query".to_string(),
            Value::String("architecture".to_string()),
        );
        arguments.insert("timeout_seconds".to_string(), json!(2.0));
        arguments.insert("max_results".to_string(), json!(250));
        arguments.insert("image".to_string(), json!(true));

        let models = SearchToolModels {
            image: Some(model),
            ..SearchToolModels::default()
        };
        let result = execute_web_tool("web_search", &arguments, None, Some(&models))
            .expect("image search should run")
            .expect("image search should return a value");

        assert_eq!(result["citations"][0], "https://example.com/building");
        assert_eq!(
            result["results"][0]["thumbnail_url"],
            "https://imgs.search.brave.com/thumb"
        );
        assert_eq!(
            result["results"][0]["image_url"],
            "https://example.com/building.jpg"
        );
    }

    #[test]
    fn web_search_image_mode_requires_image_provider() {
        let mut arguments = Map::new();
        arguments.insert("query".to_string(), Value::String("diagram".to_string()));
        arguments.insert("timeout_seconds".to_string(), json!(2.0));
        arguments.insert("image".to_string(), json!(true));
        let models = SearchToolModels::default();

        let error = execute_web_tool("web_search", &arguments, None, Some(&models))
            .expect_err("image search should reject missing image provider");

        assert!(error
            .to_string()
            .contains("image results are not configured"));
    }

    fn test_brave_model_config(url: String) -> ModelConfig {
        ModelConfig {
            provider_type: ProviderType::BraveSearch,
            model_name: "brave-web-search".to_string(),
            url,
            api_key_env: "BRAVE_SEARCH_API_KEY_TEST".to_string(),
            capabilities: vec![ModelCapability::WebSearch],
            token_max_context: 0,
            max_tokens: 0,
            cache_timeout: 0,
            idle_timeout_compact_enabled: true,
            conn_timeout: 30,
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

    fn test_brave_image_model_config(url: String) -> ModelConfig {
        ModelConfig {
            provider_type: ProviderType::BraveSearchImage,
            model_name: "brave-image-search".to_string(),
            url,
            api_key_env: "BRAVE_SEARCH_API_KEY_TEST".to_string(),
            capabilities: vec![ModelCapability::WebSearch],
            token_max_context: 0,
            max_tokens: 0,
            cache_timeout: 0,
            idle_timeout_compact_enabled: true,
            conn_timeout: 30,
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
