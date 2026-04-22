mod providers;

use crate::config::{
    AuthCredentialsStoreMode, CacheControlConfig, CodexAuthConfig, RetryModeConfig,
    UpstreamApiKind, UpstreamAuthKind, UpstreamConfig,
};
use crate::message::{
    ChatMessage, ToolResultBlock, collect_tool_result_blocks, content_item_text,
    content_without_tool_result_blocks, parse_tool_result_block, value_text,
};
use crate::token_estimation::observe_prompt_tokens_for_upstream;
use crate::tooling::Tool;
use anyhow::{Context, Result, anyhow};
use base64::Engine;
use image::{GenericImageView, ImageFormat, ImageReader};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use std::fs;
use std::io::Cursor;
use std::net::IpAddr;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use uuid::Uuid;

static RETRY_RANDOM_SEED: AtomicU64 = AtomicU64::new(0);
const MAX_INLINE_IMAGE_DIMENSION: u32 = 2000;

#[derive(Deserialize)]
pub(super) struct ChatCompletionChoice {
    message: ChatMessage,
}

#[derive(Deserialize)]
pub(super) struct ChatCompletionResponse {
    choices: Vec<ChatCompletionChoice>,
}

#[derive(Deserialize)]
struct CodexRefreshResponse {
    #[serde(default)]
    access_token: Option<String>,
    #[serde(default)]
    refresh_token: Option<String>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct TokenUsage {
    pub llm_calls: u64,
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
    pub total_tokens: u64,
    pub cache_hit_tokens: u64,
    pub cache_miss_tokens: u64,
    pub cache_read_tokens: u64,
    pub cache_write_tokens: u64,
}

impl TokenUsage {
    pub fn add_assign(&mut self, other: &TokenUsage) {
        self.llm_calls += other.llm_calls;
        self.prompt_tokens += other.prompt_tokens;
        self.completion_tokens += other.completion_tokens;
        self.total_tokens += other.total_tokens;
        self.cache_hit_tokens += other.cache_hit_tokens;
        self.cache_miss_tokens += other.cache_miss_tokens;
        self.cache_read_tokens += other.cache_read_tokens;
        self.cache_write_tokens += other.cache_write_tokens;
    }

    pub fn input_total_tokens(&self) -> u64 {
        self.prompt_tokens
    }

    pub fn output_total_tokens(&self) -> u64 {
        self.completion_tokens
    }

    pub fn context_total_tokens(&self) -> u64 {
        self.total_tokens
    }

    pub fn cache_hit_input_tokens(&self) -> u64 {
        self.cache_hit_tokens
    }

    pub fn cache_read_input_tokens(&self) -> u64 {
        self.cache_read_tokens
    }

    pub fn cache_write_input_tokens(&self) -> u64 {
        self.cache_write_tokens
    }

    pub fn cache_uncached_input_tokens(&self) -> u64 {
        self.cache_miss_tokens
    }

    pub fn normal_billed_input_tokens(&self) -> u64 {
        self.cache_miss_tokens
            .saturating_sub(self.cache_write_tokens)
    }
}

#[derive(Clone, Debug)]
pub struct ChatCompletionOutcome {
    pub message: ChatMessage,
    pub usage: TokenUsage,
    pub response_id: Option<String>,
    pub api_request_id: Option<String>,
    pub request_cache: RequestCacheLogFields,
}

#[derive(Clone, Debug)]
pub struct ImageGenerationOutcome {
    pub image_reference: String,
    pub usage: TokenUsage,
    pub response_id: Option<String>,
    pub api_request_id: Option<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RequestCacheLogFields {
    pub request_cache_control_type: Option<String>,
    pub request_cache_control_ttl: Option<String>,
    pub request_has_cache_breakpoint: bool,
    pub request_cache_breakpoint_count: u64,
}

pub(crate) enum ChatCompletionSession {
    CodexSubscription(providers::codex_subscription::CodexSubscriptionSession),
}

pub(super) fn upstream_error_from_value(value: &Value) -> Option<String> {
    let error = value.get("error")?;
    match error {
        Value::Null => None,
        Value::String(text) => Some(text.clone()),
        Value::Object(object) => {
            let message = object
                .get("message")
                .and_then(Value::as_str)
                .map(ToOwned::to_owned);
            let code = object.get("code").map(|value| match value {
                Value::String(text) => text.clone(),
                Value::Number(number) => number.to_string(),
                other => other.to_string(),
            });
            match (message, code) {
                (Some(message), Some(code)) => Some(format!("{message} (code: {code})")),
                (Some(message), None) => Some(message),
                (None, Some(code)) => Some(format!("upstream error code: {code}")),
                (None, None) => Some(error.to_string()),
            }
        }
        other => Some(other.to_string()),
    }
}

pub(super) fn build_chat_completions_url(config: &UpstreamConfig) -> String {
    let base = config.base_url.trim_end_matches('/');
    let path = if config.chat_completions_path.starts_with('/') {
        config.chat_completions_path.clone()
    } else {
        format!("/{}", config.chat_completions_path)
    };
    format!("{}{}", base, path)
}

fn auth_file_path(codex_home: &Path) -> PathBuf {
    codex_home.join("auth.json")
}

pub(super) fn should_bypass_proxy(url: &str) -> bool {
    let Ok(parsed) = reqwest::Url::parse(url) else {
        return false;
    };
    match parsed.host_str() {
        Some("localhost") => true,
        Some(host) => host
            .parse::<IpAddr>()
            .map(|ip| ip.is_loopback())
            .unwrap_or(false),
        None => false,
    }
}

pub(crate) fn create_chat_completion(
    upstream: &UpstreamConfig,
    messages: &[ChatMessage],
    tools: &[Tool],
    extra_payload: Option<Map<String, Value>>,
    mut session: Option<&mut ChatCompletionSession>,
) -> Result<ChatCompletionOutcome> {
    let max_retries = match upstream.retry_mode {
        RetryModeConfig::No => 0,
        RetryModeConfig::Random { max_retries, .. } => max_retries,
    };
    let mut attempt = 0;

    loop {
        let result = providers::provider_for(upstream).create_completion(
            upstream,
            messages,
            tools,
            extra_payload.clone(),
            session.as_deref_mut(),
        );

        match result {
            Ok(outcome) => {
                observe_prompt_tokens_for_upstream(
                    upstream,
                    messages,
                    tools,
                    "",
                    outcome.usage.prompt_tokens,
                );
                return Ok(outcome);
            }
            Err(error) if attempt < max_retries && should_retry_completion_error(&error) => {
                attempt += 1;
                if let Some(delay) = retry_delay_duration(&upstream.retry_mode) {
                    thread::sleep(delay);
                }
            }
            Err(error) => return Err(error),
        }
    }
}

pub(crate) fn create_image_generation(
    upstream: &UpstreamConfig,
    messages: &[ChatMessage],
) -> Result<ImageGenerationOutcome> {
    let max_retries = match upstream.retry_mode {
        RetryModeConfig::No => 0,
        RetryModeConfig::Random { max_retries, .. } => max_retries,
    };
    let mut attempt = 0;

    loop {
        let result = providers::provider_for(upstream).create_image_generation(upstream, messages);
        match result {
            Ok(outcome) => {
                observe_prompt_tokens_for_upstream(
                    upstream,
                    messages,
                    &[],
                    "",
                    outcome.usage.prompt_tokens,
                );
                return Ok(outcome);
            }
            Err(error) if attempt < max_retries && should_retry_completion_error(&error) => {
                attempt += 1;
                if let Some(delay) = retry_delay_duration(&upstream.retry_mode) {
                    thread::sleep(delay);
                }
            }
            Err(error) => return Err(error),
        }
    }
}

pub(crate) fn start_chat_completion_session(
    upstream: &UpstreamConfig,
) -> Result<Option<ChatCompletionSession>> {
    providers::provider_for(upstream).start_session(upstream)
}

fn should_retry_completion_error(error: &anyhow::Error) -> bool {
    let text = error.to_string().to_lowercase();
    !(text.contains("previous_response_id") || text.contains("previous response id"))
}

fn retry_delay_duration(retry_mode: &RetryModeConfig) -> Option<Duration> {
    match retry_mode {
        RetryModeConfig::No => None,
        RetryModeConfig::Random {
            retry_random_mean, ..
        } => Some(Duration::from_secs_f64(retry_random_delay_seconds(
            *retry_random_mean,
        ))),
    }
}

fn retry_random_delay_seconds(mean_seconds: f64) -> f64 {
    if !mean_seconds.is_finite() || mean_seconds <= 0.0 {
        return 0.0;
    }

    let mut normalish = 0.0;
    for _ in 0..12 {
        normalish += next_retry_uniform();
    }
    normalish -= 6.0;

    let stddev = mean_seconds / 3.0;
    let delay = mean_seconds + normalish * stddev;
    delay.clamp(0.0, mean_seconds * 2.0)
}

fn next_retry_uniform() -> f64 {
    let mut seed = RETRY_RANDOM_SEED.load(Ordering::Relaxed);
    if seed == 0 {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|value| value.as_nanos() as u64)
            .unwrap_or(0);
        seed = now ^ 0x9E37_79B9_7F4A_7C15;
    }

    seed ^= seed << 7;
    seed ^= seed >> 9;
    seed ^= seed << 8;
    RETRY_RANDOM_SEED.store(seed, Ordering::Relaxed);

    (seed as f64) / (u64::MAX as f64)
}

pub(super) fn build_responses_tools_payload(
    upstream: &UpstreamConfig,
    tools: &[Tool],
) -> Vec<Value> {
    let mut payload_tools = Vec::new();

    if upstream.api_kind == UpstreamApiKind::Responses
        && let Some(native_web_search) = &upstream.native_web_search
        && native_web_search.enabled
    {
        let mut native_tool = native_web_search.payload.clone();
        native_tool.insert("type".to_string(), Value::String("web_search".to_string()));
        payload_tools.push(Value::Object(native_tool));
    }

    if upstream.api_kind == UpstreamApiKind::Responses && upstream.native_image_generation {
        payload_tools.push(json!({
            "type": "image_generation"
        }));
    }

    payload_tools.extend(tools.iter().map(Tool::as_responses_tool));
    payload_tools
}

#[cfg(test)]
pub(super) fn parse_streamed_responses_body(body: &str) -> Result<Value> {
    let mut current_event: Option<String> = None;
    let mut current_data = Vec::new();
    let mut completed_response: Option<Value> = None;
    let mut failed_event: Option<String> = None;

    fn flush_sse_event(
        current_event: &mut Option<String>,
        current_data: &mut Vec<String>,
        completed_response: &mut Option<Value>,
        failed_event: &mut Option<String>,
    ) -> Result<()> {
        if current_data.is_empty() {
            *current_event = None;
            return Ok(());
        }
        let data = current_data.join("\n");
        current_data.clear();

        if data.trim() == "[DONE]" {
            *current_event = None;
            return Ok(());
        }

        let value: Value =
            serde_json::from_str(&data).context("failed to parse streamed responses event")?;
        let event_type = current_event
            .as_deref()
            .or_else(|| value.get("type").and_then(Value::as_str));

        match event_type {
            Some("response.completed") => {
                if let Some(response) = value.get("response") {
                    *completed_response = Some(response.clone());
                } else {
                    *completed_response = Some(value.clone());
                }
            }
            Some("response.failed") | Some("error") => {
                *failed_event = Some(
                    upstream_error_from_value(&value)
                        .or_else(|| {
                            value
                                .get("message")
                                .and_then(Value::as_str)
                                .map(str::to_string)
                        })
                        .unwrap_or_else(|| value.to_string()),
                );
            }
            _ => {
                if completed_response.is_none() && value.get("output").is_some() {
                    *completed_response = Some(value.clone());
                }
            }
        }

        *current_event = None;
        Ok(())
    }

    for raw_line in body.lines() {
        let line = raw_line.trim_end_matches('\r');
        if line.is_empty() {
            flush_sse_event(
                &mut current_event,
                &mut current_data,
                &mut completed_response,
                &mut failed_event,
            )?;
            continue;
        }
        if let Some(event_name) = line.strip_prefix("event:") {
            current_event = Some(event_name.trim().to_string());
            continue;
        }
        if let Some(data) = line.strip_prefix("data:") {
            current_data.push(data.trim_start().to_string());
        }
    }

    flush_sse_event(
        &mut current_event,
        &mut current_data,
        &mut completed_response,
        &mut failed_event,
    )?;

    if let Some(response) = completed_response {
        return Ok(response);
    }
    if let Some(error) = failed_event {
        return Err(anyhow!("upstream streamed responses failed: {}", error));
    }

    serde_json::from_str(body)
        .context("failed to parse streamed responses fallback body as a response object")
}

pub(super) fn build_responses_input(
    messages: &[ChatMessage],
) -> Result<(Option<String>, Vec<Value>)> {
    let mut instructions = Vec::new();
    let mut input = Vec::new();
    for message in messages {
        if message.role == "system" {
            if let Some(text) = message_text_content(message.content.as_ref()) {
                instructions.push(text);
            }
            continue;
        }

        match message.role.as_str() {
            "user" => {
                input.push(json!({
                    "type": "message",
                    "role": "user",
                    "content": user_content_to_responses_items(message.content.as_ref())?,
                }));
            }
            "assistant" => {
                let assistant_content =
                    assistant_content_to_responses_items(message.content.as_ref());
                if !assistant_content.is_empty() {
                    input.push(json!({
                        "type": "message",
                        "role": "assistant",
                        "content": assistant_content,
                    }));
                }
                if let Some(tool_calls) = &message.tool_calls {
                    for tool_call in tool_calls {
                        input.push(json!({
                            "type": "function_call",
                            "name": tool_call.function.name,
                            "arguments": tool_call.function.arguments.clone().unwrap_or_default(),
                            "call_id": tool_call.id,
                        }));
                    }
                }
                append_responses_tool_result_items(
                    &mut input,
                    collect_tool_result_blocks(message.content.as_ref()),
                );
            }
            "tool" => {
                append_responses_tool_result_items(
                    &mut input,
                    vec![ToolResultBlock {
                        tool_call_id: message.tool_call_id.clone().unwrap_or_default(),
                        name: message.name.clone(),
                        content: message.content.clone(),
                    }],
                );
            }
            other => {
                input.push(json!({
                    "type": "message",
                    "role": other,
                    "content": [{
                        "type": "input_text",
                        "text": message_text_content(message.content.as_ref()).unwrap_or_default(),
                    }],
                }));
            }
        }
    }

    let instructions = (!instructions.is_empty()).then(|| instructions.join("\n\n"));
    Ok((instructions, input))
}

pub(super) fn chat_completions_messages_payload(messages: &[ChatMessage]) -> Result<Value> {
    let mut payload_messages = Vec::with_capacity(messages.len());
    for message in messages {
        match message.role.as_str() {
            "tool" => {
                append_chat_completions_tool_message(
                    &mut payload_messages,
                    ToolResultBlock {
                        tool_call_id: message.tool_call_id.clone().unwrap_or_default(),
                        name: message.name.clone(),
                        content: message.content.clone(),
                    },
                );
            }
            _ => {
                let mut payload = serde_json::to_value(message)
                    .context("failed to serialize chat completion message")?;
                if let Some(object) = payload.as_object_mut() {
                    object.remove("reasoning");
                    if let Some(content) =
                        content_without_tool_result_blocks(message.content.as_ref())
                    {
                        object.insert("content".to_string(), content);
                    } else {
                        object.remove("content");
                    }
                }
                if let Some(content) = payload.get_mut("content") {
                    normalize_chat_completions_content(content);
                }
                payload_messages.push(payload);
                for tool_result in collect_tool_result_blocks(message.content.as_ref()) {
                    append_chat_completions_tool_message(&mut payload_messages, tool_result);
                }
            }
        }
    }
    Ok(Value::Array(payload_messages))
}

fn assistant_content_to_responses_items(content: Option<&Value>) -> Vec<Value> {
    match content {
        None | Some(Value::Null) => Vec::new(),
        Some(Value::String(text)) if text.is_empty() => Vec::new(),
        Some(Value::String(text)) => vec![json!({
            "type": "output_text",
            "text": text,
        })],
        Some(Value::Array(items)) => {
            let mut converted = Vec::new();
            for item in items {
                if parse_tool_result_block(item).is_some() {
                    continue;
                }
                let Some(kind) = item.get("type").and_then(Value::as_str) else {
                    continue;
                };
                match kind {
                    "text" | "input_text" | "output_text" => {
                        if let Some(text) = item.get("text").and_then(Value::as_str)
                            && !text.is_empty()
                        {
                            converted.push(json!({
                                "type": "output_text",
                                "text": text,
                            }));
                        }
                    }
                    "context" => {
                        if let Some(text) = content_item_text(item)
                            && !text.is_empty()
                        {
                            converted.push(json!({
                                "type": "output_text",
                                "text": text,
                            }));
                        }
                    }
                    _ => {}
                }
            }
            converted
        }
        Some(other) => vec![json!({
            "type": "output_text",
            "text": other.to_string(),
        })],
    }
}

fn normalize_chat_completions_content(content: &mut Value) {
    let Value::Array(items) = content else {
        return;
    };
    for item in items {
        let Some(object) = item.as_object_mut() else {
            continue;
        };
        let item_type = object.get("type").and_then(Value::as_str);
        match item_type {
            Some("input_text") | Some("output_text") => {
                object.insert("type".to_string(), Value::String("text".to_string()));
            }
            Some("input_image") | Some("image_url") => {
                object.insert("type".to_string(), Value::String("image_url".to_string()));
                if let Some(image_url) = object.get_mut("image_url")
                    && let Some(url) = image_url.as_str().map(ToOwned::to_owned)
                {
                    *image_url = json!({ "url": normalize_inline_image_url(&url) });
                } else if let Some(image_url) = object.get_mut("image_url")
                    && let Some(url) = image_url.get_mut("url")
                    && let Some(raw_url) = url.as_str().map(ToOwned::to_owned)
                {
                    *url = Value::String(normalize_inline_image_url(&raw_url));
                }
            }
            Some("context") => {
                let rendered = content_item_text(item).unwrap_or_default();
                *item = json!({
                    "type": "text",
                    "text": rendered,
                });
            }
            _ => {}
        }
    }
}

fn user_content_to_responses_items(content: Option<&Value>) -> Result<Vec<Value>> {
    match content {
        None | Some(Value::Null) => Ok(vec![json!({
            "type": "input_text",
            "text": "",
        })]),
        Some(Value::String(text)) => Ok(vec![json!({
            "type": "input_text",
            "text": text,
        })]),
        Some(Value::Array(items)) => {
            let mut converted = Vec::new();
            for item in items {
                if parse_tool_result_block(item).is_some() {
                    continue;
                }
                let Some(kind) = item.get("type").and_then(Value::as_str) else {
                    continue;
                };
                match kind {
                    "text" | "input_text" => {
                        if let Some(text) = item.get("text").and_then(Value::as_str) {
                            converted.push(json!({
                                "type": "input_text",
                                "text": text,
                            }));
                        }
                    }
                    "image_url" => {
                        let image_url = item
                            .get("image_url")
                            .and_then(|value| {
                                value
                                    .get("url")
                                    .and_then(Value::as_str)
                                    .or_else(|| value.as_str())
                            })
                            .ok_or_else(|| anyhow!("image_url content item is missing a url"))?;
                        let image_url = normalize_inline_image_url(image_url);
                        converted.push(json!({
                            "type": "input_image",
                            "image_url": image_url,
                        }));
                    }
                    "input_image" => {
                        if let Some(image_url) = item.get("image_url").and_then(Value::as_str) {
                            let image_url = normalize_inline_image_url(image_url);
                            converted.push(json!({
                                "type": "input_image",
                                "image_url": image_url,
                            }));
                        }
                    }
                    "file" | "input_file" => {
                        let file_value = if kind == "file" {
                            item.get("file")
                        } else {
                            Some(item)
                        };
                        if let Some(file_value) = file_value {
                            let file_id = file_value.get("file_id").and_then(Value::as_str);
                            let file_data = file_value.get("file_data").and_then(Value::as_str);
                            let file_url = file_value.get("file_url").and_then(Value::as_str);
                            let filename = file_value.get("filename").and_then(Value::as_str);
                            let mut payload = Map::new();
                            payload.insert(
                                "type".to_string(),
                                Value::String("input_file".to_string()),
                            );
                            if let Some(file_id) = file_id {
                                payload.insert(
                                    "file_id".to_string(),
                                    Value::String(file_id.to_string()),
                                );
                            }
                            if let Some(file_data) = file_data {
                                payload.insert(
                                    "file_data".to_string(),
                                    Value::String(file_data.to_string()),
                                );
                            }
                            if let Some(file_url) = file_url {
                                payload.insert(
                                    "file_url".to_string(),
                                    Value::String(file_url.to_string()),
                                );
                            }
                            if let Some(filename) = filename {
                                payload.insert(
                                    "filename".to_string(),
                                    Value::String(filename.to_string()),
                                );
                            }
                            if payload.len() > 1 {
                                converted.push(Value::Object(payload));
                            }
                        }
                    }
                    "input_audio" => {
                        if let Some(audio) = item.get("input_audio").and_then(Value::as_object) {
                            let data = audio.get("data").and_then(Value::as_str);
                            let format = audio.get("format").and_then(Value::as_str);
                            if let (Some(data), Some(format)) = (data, format) {
                                converted.push(json!({
                                    "type": "input_audio",
                                    "input_audio": {
                                        "data": data,
                                        "format": format,
                                    }
                                }));
                            }
                        }
                    }
                    "context" => {
                        if let Some(text) = content_item_text(item) {
                            converted.push(json!({
                                "type": "input_text",
                                "text": text,
                            }));
                        }
                    }
                    _ => {}
                }
            }
            if converted.is_empty() {
                Ok(vec![json!({
                    "type": "input_text",
                    "text": Value::Array(items.clone()).to_string(),
                })])
            } else {
                Ok(converted)
            }
        }
        Some(other) => Ok(vec![json!({
            "type": "input_text",
            "text": other.to_string(),
        })]),
    }
}

fn normalize_inline_image_url(url: &str) -> String {
    normalize_inline_image_data_url(url).unwrap_or_else(|| url.to_string())
}

fn normalize_inline_image_data_url(url: &str) -> Option<String> {
    let encoded = parse_inline_image_data_url(url)?;
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(encoded)
        .ok()?;
    let image = ImageReader::new(Cursor::new(&bytes))
        .with_guessed_format()
        .ok()?
        .decode()
        .ok()?;
    let (width, height) = image.dimensions();
    if width <= MAX_INLINE_IMAGE_DIMENSION && height <= MAX_INLINE_IMAGE_DIMENSION {
        return None;
    }

    let resized = image.resize(
        MAX_INLINE_IMAGE_DIMENSION,
        MAX_INLINE_IMAGE_DIMENSION,
        image::imageops::FilterType::Lanczos3,
    );
    let mut output = Vec::new();
    resized
        .write_to(&mut Cursor::new(&mut output), ImageFormat::Png)
        .ok()?;
    let encoded = base64::engine::general_purpose::STANDARD.encode(output);
    Some(format!("data:image/png;base64,{encoded}"))
}

fn parse_inline_image_data_url(url: &str) -> Option<&str> {
    let (metadata, encoded) = url.strip_prefix("data:")?.split_once(',')?;
    let mut parts = metadata.split(';');
    let media_type = parts.next()?.to_ascii_lowercase();
    if !media_type.starts_with("image/") {
        return None;
    }
    if !parts.any(|part| part.eq_ignore_ascii_case("base64")) {
        return None;
    }
    Some(encoded)
}

fn message_text_content(content: Option<&Value>) -> Option<String> {
    content.and_then(value_text)
}

pub(super) fn responses_value_to_chat_message(value: &Value) -> Result<ChatMessage> {
    let output = value
        .get("output")
        .and_then(Value::as_array)
        .ok_or_else(|| anyhow!("invalid responses response: missing output array"))?;
    let mut content_parts = Vec::new();
    let mut reasoning_parts = Vec::new();
    let mut tool_calls = Vec::new();
    for item in output {
        match item.get("type").and_then(Value::as_str) {
            Some("message") if item.get("role").and_then(Value::as_str) == Some("assistant") => {
                if let Some(content) = item.get("content").and_then(Value::as_array) {
                    content_parts.extend(content.iter().cloned());
                }
            }
            Some("reasoning") => reasoning_parts.push(item.clone()),
            Some("function_call") => {
                let call_id = item
                    .get("call_id")
                    .and_then(Value::as_str)
                    .ok_or_else(|| anyhow!("invalid responses function_call: missing call_id"))?;
                let name = item
                    .get("name")
                    .and_then(Value::as_str)
                    .ok_or_else(|| anyhow!("invalid responses function_call: missing name"))?;
                let arguments = item
                    .get("arguments")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string();
                tool_calls.push(crate::message::ToolCall {
                    id: call_id.to_string(),
                    kind: "function".to_string(),
                    function: crate::message::FunctionCall {
                        name: name.to_string(),
                        arguments: Some(arguments),
                    },
                });
            }
            _ => {}
        }
    }

    Ok(ChatMessage {
        role: "assistant".to_string(),
        content: (!content_parts.is_empty()).then(|| Value::Array(content_parts)),
        reasoning: (!reasoning_parts.is_empty()).then(|| Value::Array(reasoning_parts)),
        name: None,
        tool_call_id: None,
        tool_calls: (!tool_calls.is_empty()).then_some(tool_calls),
    })
}

pub(super) fn generated_image_reference_from_value(value: &Value) -> Option<String> {
    if let Some(choices) = value.get("choices").and_then(Value::as_array) {
        for choice in choices {
            if let Some(reference) =
                generated_image_reference_from_message(choice.get("message").unwrap_or(choice))
            {
                return Some(reference);
            }
        }
    }

    if let Some(output) = value.get("output").and_then(Value::as_array) {
        for item in output {
            if item.get("type").and_then(Value::as_str) == Some("image_generation_call")
                && let Some(reference) = item.get("result").and_then(Value::as_str)
            {
                return Some(reference.to_string());
            }
            if item.get("type").and_then(Value::as_str) == Some("message")
                && let Some(reference) = generated_image_reference_from_message(item)
            {
                return Some(reference);
            }
            if let Some(reference) = generated_image_reference_from_item(item) {
                return Some(reference);
            }
        }
    }

    value
        .get("images")
        .and_then(Value::as_array)
        .and_then(|images| images.iter().find_map(generated_image_reference_from_item))
}

fn generated_image_reference_from_message(message: &Value) -> Option<String> {
    if let Some(images) = message.get("images").and_then(Value::as_array)
        && let Some(reference) = images.iter().find_map(generated_image_reference_from_item)
    {
        return Some(reference);
    }

    message
        .get("content")
        .and_then(Value::as_array)
        .and_then(|items| items.iter().find_map(generated_image_reference_from_item))
}

fn generated_image_reference_from_item(item: &Value) -> Option<String> {
    if let Some(reference) = generated_image_reference_from_field(item.get("image_url")) {
        return Some(reference);
    }
    if let Some(reference) = generated_image_reference_from_field(item.get("imageUrl")) {
        return Some(reference);
    }
    if let Some(reference) = generated_image_reference_from_field(item.get("url")) {
        return Some(reference);
    }
    if let Some(reference) = item.get("result").and_then(Value::as_str)
        && (reference.starts_with("data:")
            || reference.starts_with("http://")
            || reference.starts_with("https://"))
    {
        return Some(reference.to_string());
    }
    None
}

fn generated_image_reference_from_field(value: Option<&Value>) -> Option<String> {
    match value {
        Some(Value::String(reference)) => Some(reference.clone()),
        Some(Value::Object(object)) => object
            .get("url")
            .and_then(Value::as_str)
            .map(ToOwned::to_owned)
            .or_else(|| {
                object
                    .get("image_url")
                    .and_then(Value::as_str)
                    .map(ToOwned::to_owned)
            })
            .or_else(|| {
                object
                    .get("imageUrl")
                    .and_then(Value::as_str)
                    .map(ToOwned::to_owned)
            }),
        _ => None,
    }
}

pub(super) fn response_id_from_value(value: &Value) -> Option<String> {
    value
        .get("id")
        .and_then(Value::as_str)
        .map(str::to_string)
        .or_else(|| {
            value
                .get("response")
                .and_then(|response| response.get("id"))
                .and_then(Value::as_str)
                .map(str::to_string)
        })
}

pub(super) fn load_codex_auth(upstream: &UpstreamConfig) -> Result<Option<CodexAuthConfig>> {
    if upstream.auth_kind != UpstreamAuthKind::CodexSubscription {
        return Ok(None);
    }
    if let Some(auth) = upstream.codex_auth.clone() {
        return Ok(Some(auth));
    }
    if matches!(
        upstream.auth_credentials_store_mode,
        AuthCredentialsStoreMode::Keyring
    ) {
        return Err(anyhow!(
            "codex subscription auth_credentials_store_mode=keyring is not supported here yet"
        ));
    }
    let codex_home = upstream
        .codex_home
        .as_ref()
        .ok_or_else(|| anyhow!("codex subscription config must include codex_home"))?;
    Ok(Some(crate::config::load_codex_auth_tokens(codex_home)?))
}

pub(super) fn refresh_codex_auth(
    upstream: &UpstreamConfig,
    auth: &CodexAuthConfig,
) -> Result<Option<CodexAuthConfig>> {
    if auth.refresh_token.trim().is_empty() {
        return Ok(None);
    }
    let client = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs_f64(upstream.timeout_seconds.min(30.0)))
        .build()
        .context("failed to construct codex auth refresh client")?;
    let response = client
        .post("https://auth.openai.com/oauth/token")
        .json(&serde_json::json!({
            "client_id": "app_EMoamEEZ73f0CkXaXp7hrann",
            "grant_type": "refresh_token",
            "refresh_token": auth.refresh_token,
        }))
        .send()
        .context("codex refresh token request failed")?;
    let status = response.status();
    let body = response
        .text()
        .context("failed to read codex refresh response body")?;
    if !status.is_success() {
        return Err(anyhow!(
            "codex refresh token failed with {}: {}",
            status,
            body
        ));
    }
    let parsed: CodexRefreshResponse =
        serde_json::from_str(&body).context("failed to parse codex refresh response")?;
    let refreshed = CodexAuthConfig {
        access_token: parsed
            .access_token
            .ok_or_else(|| anyhow!("codex refresh response missing access_token"))?,
        refresh_token: parsed
            .refresh_token
            .unwrap_or_else(|| auth.refresh_token.clone()),
        account_id: auth
            .account_id
            .clone()
            .or_else(|| account_id_from_access_token(&auth.access_token)),
    };
    if upstream.codex_auth.is_none()
        && let Some(codex_home) = upstream.codex_home.as_ref()
    {
        let auth_path = auth_file_path(codex_home);
        let raw = fs::read_to_string(&auth_path)
            .with_context(|| format!("failed to read {}", auth_path.display()))?;
        let mut auth_json: Value =
            serde_json::from_str(&raw).context("failed to parse codex auth.json for refresh")?;
        auth_json["tokens"]["access_token"] = Value::String(refreshed.access_token.clone());
        auth_json["tokens"]["refresh_token"] = Value::String(refreshed.refresh_token.clone());
        fs::write(
            &auth_path,
            serde_json::to_string_pretty(&auth_json)
                .context("failed to serialize refreshed auth")?,
        )
        .with_context(|| format!("failed to write {}", auth_path.display()))?;
    }
    Ok(Some(refreshed))
}

pub(super) fn account_id_from_access_token(access_token: &str) -> Option<String> {
    let payload = decode_jwt_payload(access_token).ok()?;
    payload
        .get("https://api.openai.com/auth")
        .and_then(Value::as_object)
        .and_then(|auth| auth.get("chatgpt_account_id"))
        .and_then(Value::as_str)
        .map(str::to_string)
}

fn decode_jwt_payload(jwt: &str) -> Result<Value> {
    let mut parts = jwt.split('.');
    let (_header, payload, _sig) = match (parts.next(), parts.next(), parts.next()) {
        (Some(header), Some(payload), Some(sig))
            if !header.is_empty() && !payload.is_empty() && !sig.is_empty() =>
        {
            (header, payload, sig)
        }
        _ => return Err(anyhow!("invalid JWT format")),
    };
    let payload_bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(payload)
        .context("failed to decode JWT payload")?;
    serde_json::from_slice(&payload_bytes).context("failed to parse JWT payload")
}

pub(super) fn parse_usage(response: &Value) -> TokenUsage {
    let usage = response.get("usage").and_then(Value::as_object);
    let prompt_tokens = usage
        .and_then(|value| {
            first_u64(
                value,
                &[
                    &["prompt_tokens"],
                    &["input_tokens"],
                    &["input_tokens_details", "total_tokens"],
                ],
            )
        })
        .unwrap_or(0);
    let completion_tokens = usage
        .and_then(|value| {
            first_u64(
                value,
                &[
                    &["completion_tokens"],
                    &["output_tokens"],
                    &["output_tokens_details", "total_tokens"],
                ],
            )
        })
        .unwrap_or(0);
    let total_tokens = usage
        .and_then(|value| first_u64(value, &[&["total_tokens"]]))
        .unwrap_or_else(|| prompt_tokens + completion_tokens);
    let cache_read_tokens = usage
        .and_then(|value| {
            first_u64(
                value,
                &[
                    &["cache_read_input_tokens"],
                    &["prompt_tokens_details", "cached_tokens"],
                    &["input_tokens_details", "cached_tokens"],
                    &["input_tokens_details", "cache_read_input_tokens"],
                ],
            )
        })
        .unwrap_or(0);
    let cache_write_tokens = usage
        .and_then(|value| {
            first_u64(
                value,
                &[
                    &["cache_creation_input_tokens"],
                    &["cache_write_input_tokens"],
                    &["prompt_tokens_details", "cache_write_tokens"],
                    &["input_tokens_details", "cache_write_tokens"],
                    &["input_tokens_details", "cache_creation_input_tokens"],
                ],
            )
        })
        .unwrap_or(0);
    let cache_hit_tokens = usage
        .and_then(|value| first_u64(value, &[&["cache_hit_tokens"]]))
        .unwrap_or(cache_read_tokens);
    let cache_miss_tokens = prompt_tokens.saturating_sub(cache_hit_tokens);

    TokenUsage {
        llm_calls: 1,
        prompt_tokens,
        completion_tokens,
        total_tokens,
        cache_hit_tokens,
        cache_miss_tokens,
        cache_read_tokens,
        cache_write_tokens,
    }
}

pub(super) fn next_api_request_id() -> String {
    Uuid::new_v4().to_string()
}

#[derive(Clone, Copy)]
enum ApiBodyLogMode {
    Off,
    Preview,
    Full,
}

impl ApiBodyLogMode {
    fn current() -> Self {
        match std::env::var("AGENT_FRAME_LOG_API_BODIES")
            .unwrap_or_else(|_| "preview".to_string())
            .trim()
            .to_ascii_lowercase()
            .as_str()
        {
            "0" | "false" | "off" | "none" => Self::Off,
            "1" | "true" | "full" => Self::Full,
            _ => Self::Preview,
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::Preview => "preview",
            Self::Full => "full",
        }
    }
}

struct ApiBodyLogFields {
    mode: &'static str,
    bytes: u64,
    preview: String,
    full_json: String,
    truncated: bool,
    included: bool,
}

const API_BODY_PREVIEW_CHARS: usize = 12_000;

fn api_body_log_fields(value: &Value) -> ApiBodyLogFields {
    let mode = ApiBodyLogMode::current();
    let redacted = redact_sensitive_json(value);
    let json = serde_json::to_string(&redacted).unwrap_or_else(|_| "<unserializable>".to_string());
    let bytes = json.len() as u64;
    match mode {
        ApiBodyLogMode::Off => ApiBodyLogFields {
            mode: mode.label(),
            bytes,
            preview: String::new(),
            full_json: String::new(),
            truncated: false,
            included: false,
        },
        ApiBodyLogMode::Preview => {
            let (preview, truncated) = preview_chars(&json, API_BODY_PREVIEW_CHARS);
            ApiBodyLogFields {
                mode: mode.label(),
                bytes,
                preview,
                full_json: String::new(),
                truncated,
                included: true,
            }
        }
        ApiBodyLogMode::Full => ApiBodyLogFields {
            mode: mode.label(),
            bytes,
            preview: String::new(),
            full_json: json,
            truncated: false,
            included: true,
        },
    }
}

fn preview_chars(value: &str, max_chars: usize) -> (String, bool) {
    let mut count = 0usize;
    for (idx, _) in value.char_indices() {
        if count == max_chars {
            return (value[..idx].to_string(), true);
        }
        count += 1;
    }
    (value.to_string(), false)
}

fn redact_sensitive_json(value: &Value) -> Value {
    match value {
        Value::Object(object) => Value::Object(
            object
                .iter()
                .map(|(key, value)| {
                    if is_sensitive_name(key) {
                        (key.clone(), Value::String("[REDACTED]".to_string()))
                    } else {
                        (key.clone(), redact_sensitive_json(value))
                    }
                })
                .collect(),
        ),
        Value::Array(items) => Value::Array(items.iter().map(redact_sensitive_json).collect()),
        _ => value.clone(),
    }
}

fn is_sensitive_name(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    lower.contains("authorization")
        || lower.contains("api-key")
        || lower.contains("apikey")
        || lower.contains("api_key")
        || lower.contains("token")
        || lower.contains("secret")
        || lower.contains("password")
        || lower.contains("cookie")
        || lower.contains("credential")
}

pub(super) fn redacted_upstream_request_headers_json_with_auth(
    upstream: &UpstreamConfig,
    auth_header_name: Option<&str>,
) -> String {
    let mut object = Map::new();
    object.insert(
        "content-type".to_string(),
        Value::String("application/json".to_string()),
    );
    if let Some(header_name) = auth_header_name {
        object.insert(
            header_name.to_string(),
            if header_name.eq_ignore_ascii_case("authorization") {
                Value::String("Bearer [REDACTED]".to_string())
            } else {
                Value::String("[REDACTED]".to_string())
            },
        );
    }
    for (key, value) in &upstream.headers {
        let rendered = value
            .as_str()
            .map(ToOwned::to_owned)
            .unwrap_or_else(|| value.to_string());
        object.insert(
            key.clone(),
            if is_sensitive_name(key) {
                Value::String("[REDACTED]".to_string())
            } else {
                Value::String(rendered)
            },
        );
    }
    serde_json::to_string(&Value::Object(object)).unwrap_or_else(|_| "{}".to_string())
}

pub(super) fn redacted_upstream_request_headers_json(
    upstream: &UpstreamConfig,
    has_authorization: bool,
) -> String {
    redacted_upstream_request_headers_json_with_auth(
        upstream,
        has_authorization.then_some("authorization"),
    )
}

fn parse_data_url(value: &str) -> Option<(&str, &str)> {
    let (metadata, encoded) = value.strip_prefix("data:")?.split_once(',')?;
    let mut parts = metadata.split(';');
    let media_type = parts.next()?;
    parts
        .any(|part| part.eq_ignore_ascii_case("base64"))
        .then_some((media_type, encoded))
}

fn content_item_to_claude_block(item: &Value) -> Result<Option<Value>> {
    let Some(kind) = item.get("type").and_then(Value::as_str) else {
        return Ok(None);
    };
    Ok(match kind {
        "text" | "input_text" | "output_text" => item
            .get("text")
            .and_then(Value::as_str)
            .filter(|text| !text.is_empty())
            .map(|text| json!({ "type": "text", "text": text })),
        "image_url" => {
            let image_url = item
                .get("image_url")
                .and_then(|value| {
                    value
                        .get("url")
                        .and_then(Value::as_str)
                        .or_else(|| value.as_str())
                })
                .ok_or_else(|| anyhow!("image_url content item is missing a url"))?;
            Some(claude_image_block_from_url(image_url))
        }
        "input_image" => item
            .get("image_url")
            .and_then(Value::as_str)
            .map(claude_image_block_from_url),
        "file" | "input_file" => claude_document_block_from_item(kind, item),
        "context" => content_item_text(item).map(|text| json!({ "type": "text", "text": text })),
        _ => None,
    })
}

fn claude_image_block_from_url(image_url: &str) -> Value {
    if let Some((media_type, data)) = parse_data_url(image_url) {
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
                "url": image_url,
            }
        })
    }
}

fn claude_document_block_from_item(kind: &str, item: &Value) -> Option<Value> {
    let file_value = if kind == "file" {
        item.get("file")
    } else {
        Some(item)
    }?;
    let file_id = file_value.get("file_id").and_then(Value::as_str);
    let file_data = file_value.get("file_data").and_then(Value::as_str);
    let file_url = file_value.get("file_url").and_then(Value::as_str);
    let filename = file_value.get("filename").and_then(Value::as_str);

    if let Some(file_id) = file_id {
        return Some(json!({
            "type": "document",
            "source": {
                "type": "file",
                "file_id": file_id,
            }
        }));
    }

    if let Some(file_url) = file_url
        && filename_looks_like_pdf(filename.or_else(|| file_url.rsplit('/').next()))
    {
        return Some(json!({
            "type": "document",
            "source": {
                "type": "url",
                "url": file_url,
            }
        }));
    }

    if let Some(file_data) = file_data {
        if let Some((media_type, data)) = parse_data_url(file_data) {
            if media_type.eq_ignore_ascii_case("application/pdf") {
                return Some(json!({
                    "type": "document",
                    "source": {
                        "type": "base64",
                        "media_type": "application/pdf",
                        "data": data,
                    }
                }));
            }
        } else if filename_looks_like_pdf(filename) {
            return Some(json!({
                "type": "document",
                "source": {
                    "type": "base64",
                    "media_type": "application/pdf",
                    "data": file_data,
                }
            }));
        }
    }

    None
}

fn filename_looks_like_pdf(filename: Option<&str>) -> bool {
    filename
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .is_some_and(|value| value.to_ascii_lowercase().ends_with(".pdf"))
}

fn message_content_to_claude_blocks(content: Option<&Value>) -> Result<Vec<Value>> {
    match content {
        None | Some(Value::Null) => Ok(Vec::new()),
        Some(Value::String(text)) if text.is_empty() => Ok(Vec::new()),
        Some(Value::String(text)) => Ok(vec![json!({ "type": "text", "text": text })]),
        Some(Value::Array(items)) => {
            let mut converted = Vec::new();
            for item in items {
                if let Some(block) = content_item_to_claude_block(item)? {
                    converted.push(block);
                }
            }
            if converted.is_empty() {
                Ok(vec![json!({
                    "type": "text",
                    "text": Value::Array(items.clone()).to_string(),
                })])
            } else {
                Ok(converted)
            }
        }
        Some(other) => Ok(vec![json!({
            "type": "text",
            "text": other.to_string(),
        })]),
    }
}

fn parse_tool_arguments_for_claude(arguments: Option<&String>) -> Value {
    let Some(arguments) = arguments else {
        return json!({});
    };
    serde_json::from_str::<Value>(arguments).unwrap_or_else(|_| json!({}))
}

fn append_claude_message(messages: &mut Vec<Value>, role: &str, content: Vec<Value>) {
    if content.is_empty() {
        return;
    }
    if let Some(last) = messages.last_mut()
        && last.get("role").and_then(Value::as_str) == Some(role)
        && let Some(last_content) = last.get_mut("content").and_then(Value::as_array_mut)
    {
        last_content.extend(content);
        return;
    }
    messages.push(json!({
        "role": role,
        "content": content,
    }));
}

fn add_cache_control_to_last_claude_block(
    system: &mut [Value],
    messages: &mut [Value],
    cache_control: &CacheControlConfig,
) {
    for message in messages.iter_mut().rev() {
        if let Some(content) = message.get_mut("content").and_then(Value::as_array_mut)
            && let Some(last_block) = content.last_mut()
            && let Some(object) = last_block.as_object_mut()
        {
            object.insert(
                "cache_control".to_string(),
                serde_json::to_value(cache_control)
                    .unwrap_or_else(|_| json!({ "type": "ephemeral" })),
            );
            return;
        }
    }

    if let Some(last_block) = system.last_mut()
        && let Some(object) = last_block.as_object_mut()
    {
        object.insert(
            "cache_control".to_string(),
            serde_json::to_value(cache_control).unwrap_or_else(|_| json!({ "type": "ephemeral" })),
        );
    }
}

pub(super) fn build_claude_messages_input(
    messages: &[ChatMessage],
    cache_control: Option<&CacheControlConfig>,
) -> Result<(Vec<Value>, Vec<Value>)> {
    let mut system = Vec::new();
    let mut converted_messages = Vec::new();

    for message in messages {
        match message.role.as_str() {
            "system" => {
                system.extend(message_content_to_claude_blocks(message.content.as_ref())?);
            }
            "user" => append_claude_message(
                &mut converted_messages,
                "user",
                message_content_to_claude_blocks(message.content.as_ref())?,
            ),
            "assistant" => {
                let mut content = message_content_to_claude_blocks(
                    content_without_tool_result_blocks(message.content.as_ref()).as_ref(),
                )?;
                if let Some(tool_calls) = &message.tool_calls {
                    content.extend(tool_calls.iter().map(|tool_call| {
                        json!({
                            "type": "tool_use",
                            "id": tool_call.id,
                            "name": tool_call.function.name,
                            "input": parse_tool_arguments_for_claude(
                                tool_call.function.arguments.as_ref()
                            ),
                        })
                    }));
                }
                append_claude_message(&mut converted_messages, "assistant", content);
                append_claude_tool_results(
                    &mut converted_messages,
                    collect_tool_result_blocks(message.content.as_ref()),
                );
            }
            "tool" => append_claude_tool_results(
                &mut converted_messages,
                vec![ToolResultBlock {
                    tool_call_id: message.tool_call_id.clone().unwrap_or_default(),
                    name: message.name.clone(),
                    content: message.content.clone(),
                }],
            ),
            other => append_claude_message(
                &mut converted_messages,
                other,
                message_content_to_claude_blocks(message.content.as_ref())?,
            ),
        }
    }

    if let Some(cache_control) = cache_control {
        add_cache_control_to_last_claude_block(&mut system, &mut converted_messages, cache_control);
    }

    Ok((system, converted_messages))
}

pub(super) fn claude_messages_value_to_chat_message(value: &Value) -> Result<ChatMessage> {
    let content = value
        .get("content")
        .and_then(Value::as_array)
        .ok_or_else(|| anyhow!("invalid claude messages response: missing content array"))?;
    let mut text_parts = Vec::new();
    let mut tool_calls = Vec::new();

    for item in content {
        match item.get("type").and_then(Value::as_str) {
            Some("text") => {
                if let Some(text) = item.get("text").and_then(Value::as_str) {
                    text_parts.push(text.to_string());
                }
            }
            Some("tool_use") => {
                let call_id = item
                    .get("id")
                    .and_then(Value::as_str)
                    .ok_or_else(|| anyhow!("invalid claude tool_use block: missing id"))?;
                let name = item
                    .get("name")
                    .and_then(Value::as_str)
                    .ok_or_else(|| anyhow!("invalid claude tool_use block: missing name"))?;
                let arguments = item
                    .get("input")
                    .map(Value::to_string)
                    .unwrap_or_else(|| "{}".to_string());
                tool_calls.push(crate::message::ToolCall {
                    id: call_id.to_string(),
                    kind: "function".to_string(),
                    function: crate::message::FunctionCall {
                        name: name.to_string(),
                        arguments: Some(arguments),
                    },
                });
            }
            _ => {}
        }
    }

    Ok(ChatMessage {
        role: "assistant".to_string(),
        content: (!text_parts.is_empty()).then(|| Value::String(text_parts.join("\n\n"))),
        reasoning: None,
        name: None,
        tool_call_id: None,
        tool_calls: (!tool_calls.is_empty()).then_some(tool_calls),
    })
}

fn append_responses_tool_result_items(target: &mut Vec<Value>, tool_results: Vec<ToolResultBlock>) {
    target.extend(tool_results.into_iter().map(|tool_result| {
        json!({
            "type": "function_call_output",
            "call_id": tool_result.tool_call_id,
            "output": tool_result
                .content
                .as_ref()
                .and_then(value_text)
                .unwrap_or_default(),
        })
    }));
}

fn append_chat_completions_tool_message(target: &mut Vec<Value>, tool_result: ToolResultBlock) {
    target.push(json!({
        "role": "tool",
        "tool_call_id": tool_result.tool_call_id,
        "name": tool_result.name,
        "content": tool_result
            .content
            .as_ref()
            .and_then(value_text)
            .unwrap_or_default(),
    }));
}

fn append_claude_tool_results(messages: &mut Vec<Value>, tool_results: Vec<ToolResultBlock>) {
    let blocks = tool_results
        .into_iter()
        .map(|tool_result| {
            json!({
                "type": "tool_result",
                "tool_use_id": tool_result.tool_call_id,
                "content": tool_result
                    .content
                    .as_ref()
                    .and_then(value_text)
                    .unwrap_or_default(),
            })
        })
        .collect::<Vec<_>>();
    if !blocks.is_empty() {
        append_claude_message(messages, "user", blocks);
    }
}

pub(super) fn redacted_response_headers_json(headers: &reqwest::header::HeaderMap) -> String {
    let mut object = Map::new();
    for (key, value) in headers {
        let key = key.as_str().to_string();
        let rendered = value
            .to_str()
            .map(ToOwned::to_owned)
            .unwrap_or_else(|_| "<non-utf8>".to_string());
        object.insert(
            key.clone(),
            if is_sensitive_name(&key) {
                Value::String("[REDACTED]".to_string())
            } else {
                Value::String(rendered)
            },
        );
    }
    serde_json::to_string(&Value::Object(object)).unwrap_or_else(|_| "{}".to_string())
}

pub(super) fn log_upstream_api_request_started(
    api_request_id: &str,
    upstream: &UpstreamConfig,
    provider: &str,
    method: &str,
    url: &str,
    request_headers_json: &str,
    request_body: &Value,
    request_cache: &RequestCacheLogFields,
) {
    let body = api_body_log_fields(request_body);
    tracing::debug!(
        log_stream = "api",
        log_key = %upstream.model,
        kind = "upstream_api_request_started",
        api_request_id,
        provider,
        api_kind = ?upstream.api_kind,
        auth_kind = ?upstream.auth_kind,
        model = %upstream.model,
        method,
        url,
        timeout_seconds = upstream.timeout_seconds,
        request_cache_control_type = request_cache.request_cache_control_type.as_deref().unwrap_or(""),
        request_cache_control_ttl = request_cache.request_cache_control_ttl.as_deref().unwrap_or(""),
        request_has_cache_breakpoint = request_cache.request_has_cache_breakpoint,
        request_cache_breakpoint_count = request_cache.request_cache_breakpoint_count,
        request_headers_json,
        request_body_log_mode = body.mode,
        request_body_bytes = body.bytes,
        request_body_included = body.included,
        request_body_truncated = body.truncated,
        request_body_preview = %body.preview,
        request_body_json = %body.full_json,
        "upstream API request started"
    );
}

pub(super) fn log_upstream_api_request_completed(
    api_request_id: &str,
    upstream: &UpstreamConfig,
    provider: &str,
    status_code: u16,
    elapsed_ms: u64,
    response_headers_json: &str,
    response_body: &Value,
    usage: &TokenUsage,
    response_id: Option<&str>,
    request_cache: &RequestCacheLogFields,
) {
    let body = api_body_log_fields(response_body);
    tracing::info!(
        log_stream = "api",
        log_key = %upstream.model,
        kind = "upstream_api_request_completed",
        api_request_id,
        provider,
        api_kind = ?upstream.api_kind,
        auth_kind = ?upstream.auth_kind,
        model = %upstream.model,
        status_code,
        elapsed_ms,
        request_cache_control_type = request_cache.request_cache_control_type.as_deref().unwrap_or(""),
        request_cache_control_ttl = request_cache.request_cache_control_ttl.as_deref().unwrap_or(""),
        request_has_cache_breakpoint = request_cache.request_has_cache_breakpoint,
        request_cache_breakpoint_count = request_cache.request_cache_breakpoint_count,
        response_headers_json,
        response_id = response_id.unwrap_or(""),
        llm_calls = usage.llm_calls,
        input_total_tokens = usage.input_total_tokens(),
        output_total_tokens = usage.output_total_tokens(),
        context_total_tokens = usage.context_total_tokens(),
        cache_read_input_tokens = usage.cache_read_input_tokens(),
        cache_write_input_tokens = usage.cache_write_input_tokens(),
        cache_uncached_input_tokens = usage.cache_uncached_input_tokens(),
        normal_billed_input_tokens = usage.normal_billed_input_tokens(),
        response_body_log_mode = body.mode,
        response_body_bytes = body.bytes,
        response_body_included = body.included,
        response_body_truncated = body.truncated,
        response_body_preview = %body.preview,
        response_body_json = %body.full_json,
        "upstream API request completed"
    );
}

pub(super) fn log_upstream_api_request_failed(
    api_request_id: &str,
    upstream: &UpstreamConfig,
    provider: &str,
    status_code: Option<u16>,
    elapsed_ms: u64,
    response_headers_json: &str,
    response_body: Option<&Value>,
    error: &str,
    request_cache: &RequestCacheLogFields,
) {
    let body = response_body.map(api_body_log_fields);
    tracing::warn!(
        log_stream = "api",
        log_key = %upstream.model,
        kind = "upstream_api_request_failed",
        api_request_id,
        provider,
        api_kind = ?upstream.api_kind,
        auth_kind = ?upstream.auth_kind,
        model = %upstream.model,
        status_code = status_code.unwrap_or(0),
        elapsed_ms,
        request_cache_control_type = request_cache.request_cache_control_type.as_deref().unwrap_or(""),
        request_cache_control_ttl = request_cache.request_cache_control_ttl.as_deref().unwrap_or(""),
        request_has_cache_breakpoint = request_cache.request_has_cache_breakpoint,
        request_cache_breakpoint_count = request_cache.request_cache_breakpoint_count,
        response_headers_json,
        response_body_log_mode = body.as_ref().map(|body| body.mode).unwrap_or("none"),
        response_body_bytes = body.as_ref().map(|body| body.bytes).unwrap_or(0),
        response_body_included = body.as_ref().map(|body| body.included).unwrap_or(false),
        response_body_truncated = body.as_ref().map(|body| body.truncated).unwrap_or(false),
        response_body_preview = %body.as_ref().map(|body| body.preview.as_str()).unwrap_or(""),
        response_body_json = %body.as_ref().map(|body| body.full_json.as_str()).unwrap_or(""),
        error,
        "upstream API request failed"
    );
}

pub(crate) fn request_cache_log_fields(request_body: &Value) -> RequestCacheLogFields {
    let mut markers = Vec::new();
    collect_cache_control_markers(request_body, &mut markers);
    let top_level = request_body
        .as_object()
        .and_then(|object| object.get("cache_control"))
        .and_then(parse_cache_control_marker);
    let fallback = markers.iter().find(|marker| {
        marker.request_cache_control_type.is_some() || marker.request_cache_control_ttl.is_some()
    });

    RequestCacheLogFields {
        request_cache_control_type: top_level
            .as_ref()
            .and_then(|marker| marker.request_cache_control_type.clone())
            .or_else(|| fallback.and_then(|marker| marker.request_cache_control_type.clone())),
        request_cache_control_ttl: top_level
            .as_ref()
            .and_then(|marker| marker.request_cache_control_ttl.clone())
            .or_else(|| fallback.and_then(|marker| marker.request_cache_control_ttl.clone())),
        request_has_cache_breakpoint: !markers.is_empty(),
        request_cache_breakpoint_count: markers.len() as u64,
    }
}

fn collect_cache_control_markers(value: &Value, markers: &mut Vec<RequestCacheLogFields>) {
    match value {
        Value::Object(object) => {
            if let Some(cache_control) = object.get("cache_control")
                && let Some(marker) = parse_cache_control_marker(cache_control)
            {
                markers.push(marker);
            }
            for nested in object.values() {
                collect_cache_control_markers(nested, markers);
            }
        }
        Value::Array(items) => {
            for item in items {
                collect_cache_control_markers(item, markers);
            }
        }
        _ => {}
    }
}

fn parse_cache_control_marker(value: &Value) -> Option<RequestCacheLogFields> {
    let object = value.as_object()?;
    let cache_type = object
        .get("type")
        .and_then(Value::as_str)
        .map(ToOwned::to_owned)
        .filter(|value| !value.trim().is_empty());
    let ttl = object
        .get("ttl")
        .and_then(Value::as_str)
        .map(ToOwned::to_owned)
        .filter(|value| !value.trim().is_empty());
    if cache_type.is_none() && ttl.is_none() {
        return None;
    }
    Some(RequestCacheLogFields {
        request_cache_control_type: cache_type,
        request_cache_control_ttl: ttl,
        request_has_cache_breakpoint: true,
        request_cache_breakpoint_count: 1,
    })
}

fn first_u64(object: &Map<String, Value>, paths: &[&[&str]]) -> Option<u64> {
    paths.iter().find_map(|path| nested_u64(object, path))
}

fn nested_u64(object: &Map<String, Value>, path: &[&str]) -> Option<u64> {
    let mut current = object.get(*path.first()?)?;
    for segment in &path[1..] {
        current = current.as_object()?.get(*segment)?;
    }
    current.as_u64()
}

#[cfg(test)]
mod tests {
    use super::{
        TokenUsage, UpstreamAuthKind, build_claude_messages_input, build_responses_input,
        build_responses_tools_payload, chat_completions_messages_payload,
        claude_messages_value_to_chat_message, generated_image_reference_from_value,
        normalize_inline_image_url, parse_streamed_responses_body, parse_usage,
        redact_sensitive_json, redacted_upstream_request_headers_json,
        redacted_upstream_request_headers_json_with_auth, request_cache_log_fields,
        responses_value_to_chat_message, upstream_error_from_value,
    };
    use crate::config::{
        AuthCredentialsStoreMode, CacheControlConfig, NativeWebSearchConfig, ReasoningConfig,
        UpstreamApiKind, UpstreamConfig,
    };
    use crate::message::{
        ChatMessage, FunctionCall, ToolCall, context_content_block, tool_result_content_block,
    };
    use crate::tooling::Tool;
    use base64::Engine as _;
    use image::{GenericImageView, ImageBuffer, ImageFormat, Rgba};
    use serde_json::json;
    use std::io::Cursor;

    fn png_data_url(width: u32, height: u32) -> String {
        let image = ImageBuffer::from_pixel(width, height, Rgba([12, 34, 56, 255]));
        let mut bytes = Vec::new();
        image::DynamicImage::ImageRgba8(image)
            .write_to(&mut Cursor::new(&mut bytes), ImageFormat::Png)
            .unwrap();
        let encoded = base64::engine::general_purpose::STANDARD.encode(bytes);
        format!("data:image/png;base64,{encoded}")
    }

    fn data_url_dimensions(url: &str) -> (u32, u32) {
        let encoded = url.split_once(',').unwrap().1;
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(encoded)
            .unwrap();
        image::load_from_memory(&bytes).unwrap().dimensions()
    }

    #[test]
    fn token_usage_exposes_clear_accounting_names() {
        let usage = TokenUsage {
            llm_calls: 1,
            prompt_tokens: 100,
            completion_tokens: 20,
            total_tokens: 120,
            cache_hit_tokens: 60,
            cache_miss_tokens: 40,
            cache_read_tokens: 60,
            cache_write_tokens: 15,
        };

        assert_eq!(usage.input_total_tokens(), 100);
        assert_eq!(usage.output_total_tokens(), 20);
        assert_eq!(usage.context_total_tokens(), 120);
        assert_eq!(usage.cache_hit_input_tokens(), 60);
        assert_eq!(usage.cache_read_input_tokens(), 60);
        assert_eq!(usage.cache_write_input_tokens(), 15);
        assert_eq!(usage.cache_uncached_input_tokens(), 40);
        assert_eq!(usage.normal_billed_input_tokens(), 25);
    }

    #[test]
    fn api_log_redacts_sensitive_headers() {
        let mut upstream = test_upstream_config();
        upstream.base_url = "https://example.test".to_string();
        upstream.model = "demo-model".to_string();
        upstream
            .headers
            .insert("X-Api-Key".to_string(), json!("secret-value"));
        upstream
            .headers
            .insert("X-Trace".to_string(), json!("trace-value"));

        let headers = redacted_upstream_request_headers_json(&upstream, true);
        let value: serde_json::Value = serde_json::from_str(&headers).unwrap();
        assert_eq!(value["authorization"], "Bearer [REDACTED]");
        assert_eq!(value["X-Api-Key"], "[REDACTED]");
        assert_eq!(value["X-Trace"], "trace-value");
        assert!(!headers.contains("secret-value"));
    }

    #[test]
    fn api_log_can_redact_x_api_key_headers() {
        let upstream = test_upstream_config();
        let headers =
            redacted_upstream_request_headers_json_with_auth(&upstream, Some("x-api-key"));
        let value: serde_json::Value = serde_json::from_str(&headers).unwrap();
        assert_eq!(value["x-api-key"], "[REDACTED]");
        assert!(value.get("authorization").is_none());
    }

    #[test]
    fn api_log_redacts_sensitive_json_keys() {
        let value = json!({
            "model": "demo",
            "messages": [{"role": "user", "content": "keep this visible"}],
            "metadata": {
                "api_key": "secret",
                "nested_token": "secret-token",
                "ordinary": "value"
            }
        });

        let redacted = redact_sensitive_json(&value);
        assert_eq!(redacted["metadata"]["api_key"], "[REDACTED]");
        assert_eq!(redacted["metadata"]["nested_token"], "[REDACTED]");
        assert_eq!(redacted["metadata"]["ordinary"], "value");
        assert_eq!(redacted["messages"][0]["content"], "keep this visible");
    }

    #[test]
    fn extracts_error_payloads_before_choices_decoding() {
        let body = json!({
            "error": {
                "message": "Insufficient credits",
                "code": 402
            }
        });
        assert_eq!(
            upstream_error_from_value(&body).as_deref(),
            Some("Insufficient credits (code: 402)")
        );
    }

    #[test]
    fn ignores_null_error_payload() {
        let body = json!({
            "error": null,
            "output": []
        });
        assert_eq!(upstream_error_from_value(&body), None);
    }

    #[test]
    fn codex_responses_drop_unsupported_max_completion_tokens() {
        let mut payload = serde_json::Map::new();
        payload.insert("model".to_string(), json!("gpt-5.4"));
        payload.insert("input".to_string(), json!([]));
        payload.insert("max_completion_tokens".to_string(), json!(1200));

        if UpstreamAuthKind::CodexSubscription == UpstreamAuthKind::CodexSubscription {
            payload.remove("max_completion_tokens");
        }

        assert!(payload.get("max_completion_tokens").is_none());
    }

    fn test_upstream_config() -> UpstreamConfig {
        UpstreamConfig {
            base_url: "https://example.com".to_string(),
            model: "gpt-5.4".to_string(),
            api_kind: UpstreamApiKind::Responses,
            auth_kind: UpstreamAuthKind::ApiKey,
            supports_vision_input: false,
            supports_pdf_input: false,
            supports_audio_input: false,
            api_key: None,
            api_key_env: "OPENAI_API_KEY".to_string(),
            chat_completions_path: "/responses".to_string(),
            codex_home: None,
            codex_auth: None,
            auth_credentials_store_mode: AuthCredentialsStoreMode::Auto,
            timeout_seconds: 30.0,
            retry_mode: Default::default(),
            context_window_tokens: 200_000,
            cache_control: None,
            prompt_cache_retention: None,
            prompt_cache_key: None,
            reasoning: None,
            headers: serde_json::Map::new(),
            native_web_search: None,
            external_web_search: None,
            native_image_input: false,
            native_pdf_input: false,
            native_audio_input: false,
            native_image_generation: false,
            token_estimation: None,
        }
    }

    #[test]
    fn responses_reasoning_payload_omits_none_fields() {
        let mut upstream = test_upstream_config();
        upstream.reasoning = Some(ReasoningConfig {
            effort: Some("medium".to_string()),
            max_tokens: None,
            exclude: None,
            enabled: None,
        });

        let payload = super::providers::openrouter_responses::responses_reasoning_payload(
            upstream.reasoning.as_ref(),
        )
        .unwrap()
        .unwrap();
        assert_eq!(payload, json!({ "effort": "medium" }));
    }

    #[test]
    fn codex_responses_reasoning_payload_only_keeps_effort() {
        let mut upstream = test_upstream_config();
        upstream.auth_kind = UpstreamAuthKind::CodexSubscription;
        upstream.reasoning = Some(ReasoningConfig {
            effort: Some("medium".to_string()),
            max_tokens: Some(2048),
            exclude: Some(true),
            enabled: Some(true),
        });

        let payload = super::providers::codex_subscription::codex_reasoning_payload(
            upstream.reasoning.as_ref(),
        )
        .unwrap()
        .unwrap();
        assert_eq!(payload, json!({ "effort": "medium" }));
    }

    #[test]
    fn parse_usage_reads_cached_tokens_from_input_token_details() {
        let usage = parse_usage(&json!({
            "usage": {
                "input_tokens": 1200,
                "output_tokens": 80,
                "input_tokens_details": {
                    "cached_tokens": 900,
                    "cache_creation_input_tokens": 120
                }
            }
        }));

        assert_eq!(usage.prompt_tokens, 1200);
        assert_eq!(usage.completion_tokens, 80);
        assert_eq!(usage.cache_hit_tokens, 900);
        assert_eq!(usage.cache_read_tokens, 900);
        assert_eq!(usage.cache_write_tokens, 120);
        assert_eq!(usage.cache_miss_tokens, 300);
    }

    #[test]
    fn request_cache_log_fields_reads_top_level_cache_control() {
        let fields = request_cache_log_fields(&json!({
            "model": "demo",
            "cache_control": {
                "type": "ephemeral",
                "ttl": "5m"
            }
        }));

        assert_eq!(
            fields.request_cache_control_type.as_deref(),
            Some("ephemeral")
        );
        assert_eq!(fields.request_cache_control_ttl.as_deref(), Some("5m"));
        assert!(fields.request_has_cache_breakpoint);
        assert_eq!(fields.request_cache_breakpoint_count, 1);
    }

    #[test]
    fn request_cache_log_fields_reads_nested_claude_cache_breakpoint() {
        let fields = request_cache_log_fields(&json!({
            "messages": [{
                "role": "user",
                "content": [{
                    "type": "text",
                    "text": "hello",
                    "cache_control": {
                        "type": "ephemeral",
                        "ttl": "5m"
                    }
                }]
            }]
        }));

        assert_eq!(
            fields.request_cache_control_type.as_deref(),
            Some("ephemeral")
        );
        assert_eq!(fields.request_cache_control_ttl.as_deref(), Some("5m"));
        assert!(fields.request_has_cache_breakpoint);
        assert_eq!(fields.request_cache_breakpoint_count, 1);
    }

    #[test]
    fn build_responses_input_converts_mixed_chat_history() {
        let messages = vec![
            ChatMessage::text("system", "System rules"),
            ChatMessage {
                role: "user".to_string(),
                content: Some(json!([
                    { "type": "text", "text": "Look at this" },
                    { "type": "image_url", "image_url": { "url": "https://example.com/a.png" } }
                ])),
                reasoning: None,
                name: None,
                tool_call_id: None,
                tool_calls: None,
            },
            ChatMessage {
                role: "assistant".to_string(),
                content: Some(json!("Working on it")),
                reasoning: None,
                name: None,
                tool_call_id: None,
                tool_calls: Some(vec![ToolCall {
                    id: "call_1".to_string(),
                    kind: "function".to_string(),
                    function: FunctionCall {
                        name: "file_read".to_string(),
                        arguments: Some("{\"file_path\":\"README.md\"}".to_string()),
                    },
                }]),
            },
            ChatMessage {
                role: "tool".to_string(),
                content: Some(json!("file contents")),
                reasoning: None,
                name: None,
                tool_call_id: Some("call_1".to_string()),
                tool_calls: None,
            },
        ];

        let (instructions, input) = build_responses_input(&messages).unwrap();
        assert_eq!(instructions.as_deref(), Some("System rules"));
        assert_eq!(input[0]["type"], "message");
        assert_eq!(input[0]["role"], "user");
        assert_eq!(input[0]["content"][0]["type"], "input_text");
        assert_eq!(input[0]["content"][1]["type"], "input_image");
        assert_eq!(input[1]["type"], "message");
        assert_eq!(input[1]["role"], "assistant");
        assert_eq!(input[2]["type"], "function_call");
        assert_eq!(input[2]["name"], "file_read");
        assert_eq!(input[3]["type"], "function_call_output");
        assert_eq!(input[3]["call_id"], "call_1");
    }

    #[test]
    fn build_claude_messages_input_converts_history_and_applies_cache_breakpoint() {
        let messages = vec![
            ChatMessage::text("system", "System prefix"),
            ChatMessage::text("user", "First question"),
            ChatMessage {
                role: "assistant".to_string(),
                content: Some(json!("Calling tool")),
                reasoning: None,
                name: None,
                tool_call_id: None,
                tool_calls: Some(vec![ToolCall {
                    id: "tool-1".to_string(),
                    kind: "function".to_string(),
                    function: FunctionCall {
                        name: "lookup".to_string(),
                        arguments: Some("{\"topic\":\"cache\"}".to_string()),
                    },
                }]),
            },
            ChatMessage::tool_output("tool-1", "lookup", "tool result"),
            ChatMessage::text("user", "Final question"),
        ];

        let (system, input) = build_claude_messages_input(
            &messages,
            Some(&CacheControlConfig {
                cache_type: "ephemeral".to_string(),
                ttl: Some("5m".to_string()),
            }),
        )
        .unwrap();

        assert_eq!(system[0]["type"], "text");
        assert_eq!(system[0]["text"], "System prefix");
        assert_eq!(input[0]["role"], "user");
        assert_eq!(input[1]["role"], "assistant");
        assert_eq!(input[1]["content"][1]["type"], "tool_use");
        assert_eq!(input[2]["role"], "user");
        assert_eq!(input[2]["content"][0]["type"], "tool_result");
        assert_eq!(input[2]["content"][1]["type"], "text");
        assert_eq!(input[2]["content"][1]["cache_control"]["type"], "ephemeral");
        assert_eq!(input[2]["content"][1]["cache_control"]["ttl"], "5m");
    }

    #[test]
    fn build_claude_messages_input_omits_empty_text_blocks() {
        let messages = vec![
            ChatMessage {
                role: "assistant".to_string(),
                content: Some(json!("")),
                reasoning: None,
                name: None,
                tool_call_id: None,
                tool_calls: Some(vec![ToolCall {
                    id: "tool-1".to_string(),
                    kind: "function".to_string(),
                    function: FunctionCall {
                        name: "lookup".to_string(),
                        arguments: Some("{\"topic\":\"cache\"}".to_string()),
                    },
                }]),
            },
            ChatMessage {
                role: "user".to_string(),
                content: Some(json!("")),
                reasoning: None,
                name: None,
                tool_call_id: None,
                tool_calls: None,
            },
        ];

        let (_system, input) = build_claude_messages_input(&messages, None).unwrap();

        assert_eq!(input.len(), 1);
        assert_eq!(input[0]["role"], "assistant");
        assert_eq!(input[0]["content"].as_array().unwrap().len(), 1);
        assert_eq!(input[0]["content"][0]["type"], "tool_use");
    }

    #[test]
    fn build_claude_messages_input_converts_pdf_files_into_document_blocks() {
        let messages = vec![ChatMessage {
            role: "user".to_string(),
            content: Some(json!([
                {
                    "type": "file",
                    "file": {
                        "file_data": "JVBERi0xLjQK",
                        "filename": "report.pdf",
                    }
                },
                {
                    "type": "text",
                    "text": "Summarize it"
                }
            ])),
            reasoning: None,
            name: None,
            tool_call_id: None,
            tool_calls: None,
        }];

        let (_system, input) = build_claude_messages_input(&messages, None).unwrap();

        assert_eq!(input.len(), 1);
        assert_eq!(input[0]["role"], "user");
        assert_eq!(input[0]["content"][0]["type"], "document");
        assert_eq!(input[0]["content"][0]["source"]["type"], "base64");
        assert_eq!(
            input[0]["content"][0]["source"]["media_type"],
            "application/pdf"
        );
        assert_eq!(input[0]["content"][0]["source"]["data"], "JVBERi0xLjQK");
        assert_eq!(input[0]["content"][1]["type"], "text");
        assert_eq!(input[0]["content"][1]["text"], "Summarize it");
    }

    #[test]
    fn claude_messages_value_to_chat_message_reads_text_and_tool_use() {
        let response = json!({
            "content": [
                { "type": "text", "text": "Need a tool" },
                {
                    "type": "tool_use",
                    "id": "call_1",
                    "name": "lookup",
                    "input": { "topic": "cache" }
                }
            ]
        });

        let message = claude_messages_value_to_chat_message(&response).unwrap();
        assert_eq!(message.role, "assistant");
        assert_eq!(message.content, Some(json!("Need a tool")));
        assert_eq!(message.tool_calls.as_ref().unwrap()[0].id, "call_1");
        assert_eq!(
            message.tool_calls.as_ref().unwrap()[0]
                .function
                .arguments
                .as_deref(),
            Some("{\"topic\":\"cache\"}")
        );
    }

    #[test]
    fn chat_completions_payload_converts_internal_input_image_items() {
        let messages = vec![ChatMessage {
            role: "user".to_string(),
            content: Some(json!([
                { "type": "input_text", "text": "Look again" },
                { "type": "input_image", "image_url": "data:image/png;base64,AAAA" }
            ])),
            reasoning: None,
            name: None,
            tool_call_id: None,
            tool_calls: None,
        }];

        let payload = chat_completions_messages_payload(&messages).unwrap();
        assert_eq!(payload[0]["content"][0]["type"], "text");
        assert_eq!(payload[0]["content"][1]["type"], "image_url");
        assert_eq!(
            payload[0]["content"][1]["image_url"]["url"],
            "data:image/png;base64,AAAA"
        );
    }

    #[test]
    fn inline_image_data_urls_are_resized_under_provider_limit() {
        let original = png_data_url(2100, 100);
        let normalized = normalize_inline_image_url(&original);

        assert_ne!(normalized, original);
        assert!(normalized.starts_with("data:image/png;base64,"));
        let (width, height) = data_url_dimensions(&normalized);
        assert!(width <= 2000);
        assert!(height <= 2000);
    }

    #[test]
    fn chat_completions_payload_resizes_object_image_urls() {
        let messages = vec![ChatMessage {
            role: "user".to_string(),
            content: Some(json!([
                { "type": "text", "text": "Look" },
                { "type": "image_url", "image_url": { "url": png_data_url(100, 2100) } }
            ])),
            reasoning: None,
            name: None,
            tool_call_id: None,
            tool_calls: None,
        }];

        let payload = chat_completions_messages_payload(&messages).unwrap();
        let url = payload[0]["content"][1]["image_url"]["url"]
            .as_str()
            .unwrap();
        let (width, height) = data_url_dimensions(url);
        assert!(width <= 2000);
        assert!(height <= 2000);
    }

    #[test]
    fn responses_input_resizes_inline_images() {
        let messages = vec![ChatMessage {
            role: "user".to_string(),
            content: Some(json!([
                { "type": "text", "text": "Look" },
                { "type": "input_image", "image_url": png_data_url(2400, 1200) }
            ])),
            reasoning: None,
            name: None,
            tool_call_id: None,
            tool_calls: None,
        }];

        let (_, input) = build_responses_input(&messages).unwrap();
        let url = input[0]["content"][1]["image_url"].as_str().unwrap();
        let (width, height) = data_url_dimensions(url);
        assert!(width <= 2000);
        assert!(height <= 2000);
    }

    #[test]
    fn responses_output_converts_back_into_assistant_message() {
        let response = json!({
            "output": [
                {
                    "type": "reasoning",
                    "id": "rs_1",
                    "summary": [
                        { "type": "summary_text", "text": "Need to inspect the page first." }
                    ]
                },
                {
                    "type": "message",
                    "role": "assistant",
                    "content": [
                        { "type": "output_text", "text": "First part" },
                        { "type": "output_text", "text": "Second part" }
                    ]
                },
                {
                    "type": "function_call",
                    "call_id": "call_1",
                    "name": "web_fetch",
                    "arguments": "{\"url\":\"https://example.com\"}"
                }
            ]
        });

        let message = responses_value_to_chat_message(&response).unwrap();
        assert_eq!(message.role, "assistant");
        assert_eq!(
            message.content,
            Some(json!([
                { "type": "output_text", "text": "First part" },
                { "type": "output_text", "text": "Second part" }
            ]))
        );
        assert_eq!(
            message.reasoning,
            Some(json!([
                {
                    "type": "reasoning",
                    "id": "rs_1",
                    "summary": [
                        { "type": "summary_text", "text": "Need to inspect the page first." }
                    ]
                }
            ]))
        );
        assert_eq!(message.tool_calls.as_ref().map(Vec::len), Some(1));
        let tool_call = &message.tool_calls.unwrap()[0];
        assert_eq!(tool_call.id, "call_1");
        assert_eq!(tool_call.function.name, "web_fetch");
        assert_eq!(
            tool_call.function.arguments.as_deref(),
            Some("{\"url\":\"https://example.com\"}")
        );
    }

    #[test]
    fn chat_completions_payload_omits_internal_reasoning_field() {
        let messages = vec![ChatMessage {
            role: "assistant".to_string(),
            content: Some(json!("Done")),
            reasoning: Some(json!([{ "type": "reasoning", "text": "hidden" }])),
            name: None,
            tool_call_id: None,
            tool_calls: None,
        }];

        let payload = chat_completions_messages_payload(&messages).unwrap();
        assert!(payload[0].get("reasoning").is_none());
    }

    #[test]
    fn responses_input_splits_tool_result_blocks_and_preserves_context_text() {
        let messages = vec![ChatMessage {
            role: "assistant".to_string(),
            content: Some(json!([
                { "type": "output_text", "text": "Drafting answer" },
                context_content_block(
                    Some("retrieval"),
                    Some("top-k passages were refreshed"),
                    Some(json!({"count": 3}))
                ),
                tool_result_content_block("call_1", "fetch", json!("first result")),
                tool_result_content_block("call_2", "grep", json!({"matches": 2}))
            ])),
            reasoning: None,
            name: None,
            tool_call_id: None,
            tool_calls: None,
        }];

        let (_instructions, input) = build_responses_input(&messages).unwrap();
        assert_eq!(input.len(), 3);
        assert_eq!(input[0]["type"], "message");
        assert_eq!(input[0]["role"], "assistant");
        let assistant_items = input[0]["content"].as_array().unwrap();
        assert_eq!(assistant_items.len(), 2);
        assert_eq!(assistant_items[0]["text"], "Drafting answer");
        assert!(
            assistant_items[1]["text"]
                .as_str()
                .unwrap()
                .contains("[context: retrieval]")
        );
        assert_eq!(input[1]["type"], "function_call_output");
        assert_eq!(input[1]["call_id"], "call_1");
        assert_eq!(input[1]["output"], "first result");
        assert_eq!(input[2]["type"], "function_call_output");
        assert_eq!(input[2]["call_id"], "call_2");
        assert!(
            input[2]["output"]
                .as_str()
                .unwrap()
                .contains("\"matches\":2")
        );
    }

    #[test]
    fn chat_completions_payload_splits_tool_result_blocks_into_tool_messages() {
        let messages = vec![ChatMessage {
            role: "assistant".to_string(),
            content: Some(json!([
                { "type": "output_text", "text": "Need two observations" },
                context_content_block(
                    Some("runtime"),
                    Some("restored from stable prefix"),
                    Some(json!({"cache_window": "5m"}))
                ),
                tool_result_content_block("call_1", "fetch", json!("first result")),
                tool_result_content_block("call_2", "grep", json!("second result"))
            ])),
            reasoning: Some(json!([{ "type": "reasoning", "text": "hidden" }])),
            name: None,
            tool_call_id: None,
            tool_calls: None,
        }];

        let payload = chat_completions_messages_payload(&messages).unwrap();
        let payload = payload.as_array().unwrap();
        assert_eq!(payload.len(), 3);
        assert_eq!(payload[0]["role"], "assistant");
        assert!(payload[0].get("reasoning").is_none());
        let content = payload[0]["content"].as_array().unwrap();
        assert_eq!(content.len(), 2);
        assert_eq!(content[0]["type"], "text");
        assert_eq!(content[0]["text"], "Need two observations");
        assert!(
            content[1]["text"]
                .as_str()
                .unwrap()
                .contains("[context: runtime]")
        );
        assert_eq!(payload[1]["role"], "tool");
        assert_eq!(payload[1]["tool_call_id"], "call_1");
        assert_eq!(payload[1]["content"], "first result");
        assert_eq!(payload[2]["role"], "tool");
        assert_eq!(payload[2]["tool_call_id"], "call_2");
        assert_eq!(payload[2]["content"], "second result");
    }

    #[test]
    fn streamed_responses_body_extracts_completed_response() {
        let body = concat!(
            "event: response.in_progress\n",
            "data: {\"type\":\"response.in_progress\",\"response\":{\"id\":\"resp_1\"}}\n\n",
            "event: response.completed\n",
            "data: {\"type\":\"response.completed\",\"response\":{\"output\":[{\"type\":\"message\",\"role\":\"assistant\",\"content\":[{\"type\":\"output_text\",\"text\":\"hello\"}]}],\"usage\":{\"input_tokens\":1,\"output_tokens\":2}}}\n\n",
            "data: [DONE]\n",
        );

        let parsed = parse_streamed_responses_body(body).unwrap();
        assert_eq!(parsed["output"][0]["type"], "message");
        assert_eq!(parsed["output"][0]["content"][0]["text"], "hello");
        assert_eq!(parsed["usage"]["input_tokens"], 1);
    }

    #[test]
    fn streamed_responses_body_surfaces_failed_event() {
        let body = concat!(
            "event: response.failed\n",
            "data: {\"type\":\"response.failed\",\"error\":{\"message\":\"boom\",\"code\":\"bad_request\"}}\n\n"
        );

        let error = parse_streamed_responses_body(body).unwrap_err().to_string();
        assert!(error.contains("boom"));
    }

    #[test]
    fn responses_tools_payload_includes_native_web_search_tool() {
        let mut upstream = test_upstream_config();
        upstream.native_web_search = Some(NativeWebSearchConfig {
            enabled: true,
            payload: serde_json::Map::from_iter([(
                "user_location".to_string(),
                json!({
                    "type": "approximate",
                    "approximate": { "country": "US" }
                }),
            )]),
        });
        let local_tool = Tool::new("demo", "demo tool", json!({"type":"object"}), |_| {
            Ok(json!({"ok": true}))
        });

        let tools = build_responses_tools_payload(&upstream, &[local_tool]);

        assert_eq!(tools.len(), 2);
        assert_eq!(tools[0]["type"], "web_search");
        assert_eq!(tools[0]["user_location"]["approximate"]["country"], "US");
        assert_eq!(tools[1]["type"], "function");
        assert_eq!(tools[1]["name"], "demo");
    }

    #[test]
    fn responses_tools_payload_skips_native_web_search_for_chat_completions() {
        let mut upstream = test_upstream_config();
        upstream.api_kind = UpstreamApiKind::ChatCompletions;
        upstream.native_web_search = Some(NativeWebSearchConfig {
            enabled: true,
            payload: serde_json::Map::new(),
        });

        let tools = build_responses_tools_payload(&upstream, &[]);

        assert!(tools.is_empty());
    }

    #[test]
    fn responses_tools_payload_includes_native_image_generation_tool() {
        let mut upstream = test_upstream_config();
        upstream.native_image_generation = true;

        let tools = build_responses_tools_payload(&upstream, &[]);

        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0]["type"], "image_generation");
    }

    #[test]
    fn generated_image_reference_reads_chat_completions_images() {
        let value = json!({
            "choices": [
                {
                    "message": {
                        "role": "assistant",
                        "images": [
                            {
                                "type": "image_url",
                                "image_url": {
                                    "url": "data:image/png;base64,AAAA"
                                }
                            }
                        ]
                    }
                }
            ]
        });

        assert_eq!(
            generated_image_reference_from_value(&value).as_deref(),
            Some("data:image/png;base64,AAAA")
        );
    }

    #[test]
    fn generated_image_reference_reads_responses_message_images() {
        let value = json!({
            "output": [
                {
                    "type": "message",
                    "role": "assistant",
                    "images": [
                        {
                            "imageUrl": {
                                "url": "https://example.com/generated.png"
                            }
                        }
                    ]
                }
            ]
        });

        assert_eq!(
            generated_image_reference_from_value(&value).as_deref(),
            Some("https://example.com/generated.png")
        );
    }
}
