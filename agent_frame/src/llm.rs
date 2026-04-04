mod providers;

use crate::config::{AuthCredentialsStoreMode, CodexAuthConfig, UpstreamAuthKind, UpstreamConfig};
use crate::message::ChatMessage;
use crate::tooling::Tool;
use anyhow::{Context, Result, anyhow};
use base64::Engine;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use std::fs;
use std::net::IpAddr;
use std::path::{Path, PathBuf};
use std::time::Duration;

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
}

#[derive(Clone, Debug)]
pub struct ChatCompletionOutcome {
    pub message: ChatMessage,
    pub usage: TokenUsage,
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

pub fn create_chat_completion(
    upstream: &UpstreamConfig,
    messages: &[ChatMessage],
    tools: &[Tool],
    extra_payload: Option<Map<String, Value>>,
) -> Result<ChatCompletionOutcome> {
    providers::provider_for(upstream).create_completion(upstream, messages, tools, extra_payload)
}

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
                if let Some(text) = message_text_content(message.content.as_ref())
                    && !text.is_empty()
                {
                    input.push(json!({
                        "type": "message",
                        "role": "assistant",
                        "content": [{
                            "type": "output_text",
                            "text": text,
                        }],
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
            }
            "tool" => {
                input.push(json!({
                    "type": "function_call_output",
                    "call_id": message.tool_call_id.clone().unwrap_or_default(),
                    "output": message_text_content(message.content.as_ref()).unwrap_or_default(),
                }));
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
                        converted.push(json!({
                            "type": "input_image",
                            "image_url": image_url,
                        }));
                    }
                    "input_image" => {
                        if let Some(image_url) = item.get("image_url").and_then(Value::as_str) {
                            converted.push(json!({
                                "type": "input_image",
                                "image_url": image_url,
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

fn message_text_content(content: Option<&Value>) -> Option<String> {
    match content {
        None | Some(Value::Null) => None,
        Some(Value::String(text)) => Some(text.clone()),
        Some(Value::Array(items)) => {
            let texts = items
                .iter()
                .filter_map(|item| {
                    let kind = item.get("type").and_then(Value::as_str)?;
                    match kind {
                        "text" | "input_text" | "output_text" => {
                            item.get("text").and_then(Value::as_str).map(str::to_string)
                        }
                        _ => None,
                    }
                })
                .collect::<Vec<_>>();
            (!texts.is_empty()).then(|| texts.join("\n\n"))
        }
        Some(other) => Some(other.to_string()),
    }
}

pub(super) fn responses_value_to_chat_message(value: &Value) -> Result<ChatMessage> {
    let output = value
        .get("output")
        .and_then(Value::as_array)
        .ok_or_else(|| anyhow!("invalid responses response: missing output array"))?;
    let mut text_parts = Vec::new();
    let mut tool_calls = Vec::new();
    for item in output {
        match item.get("type").and_then(Value::as_str) {
            Some("message") if item.get("role").and_then(Value::as_str) == Some("assistant") => {
                if let Some(content) = item.get("content").and_then(Value::as_array) {
                    for entry in content {
                        if entry.get("type").and_then(Value::as_str) == Some("output_text")
                            && let Some(text) = entry.get("text").and_then(Value::as_str)
                        {
                            text_parts.push(text.to_string());
                        }
                    }
                }
            }
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
        content: (!text_parts.is_empty()).then(|| Value::String(text_parts.join("\n\n"))),
        name: None,
        tool_call_id: None,
        tool_calls: (!tool_calls.is_empty()).then_some(tool_calls),
    })
}

pub(super) fn apply_auth_headers(
    mut request: reqwest::blocking::RequestBuilder,
    upstream: &UpstreamConfig,
    codex_auth: Option<&CodexAuthConfig>,
) -> Result<reqwest::blocking::RequestBuilder> {
    match upstream.auth_kind {
        UpstreamAuthKind::ApiKey => {
            if let Some(api_key) = upstream
                .api_key
                .clone()
                .or_else(|| std::env::var(&upstream.api_key_env).ok())
            {
                request = request.bearer_auth(api_key);
            }
        }
        UpstreamAuthKind::CodexSubscription => {
            let auth = codex_auth.ok_or_else(|| anyhow!("codex auth is unavailable"))?;
            let account_id = auth
                .account_id
                .clone()
                .or_else(|| account_id_from_access_token(&auth.access_token))
                .ok_or_else(|| {
                    anyhow!("codex auth token is missing chatgpt account id; please log in again")
                })?;
            request = request
                .bearer_auth(&auth.access_token)
                .header("chatgpt-account-id", account_id);
        }
    }
    Ok(request)
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

fn account_id_from_access_token(access_token: &str) -> Option<String> {
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
        UpstreamAuthKind, build_responses_input, parse_streamed_responses_body,
        responses_value_to_chat_message, upstream_error_from_value,
    };
    use crate::config::{
        AuthCredentialsStoreMode, ReasoningConfig, UpstreamApiKind, UpstreamConfig,
    };
    use crate::message::{ChatMessage, FunctionCall, ToolCall};
    use serde_json::json;

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
            api_key: None,
            api_key_env: "OPENAI_API_KEY".to_string(),
            chat_completions_path: "/responses".to_string(),
            codex_home: None,
            codex_auth: None,
            auth_credentials_store_mode: AuthCredentialsStoreMode::Auto,
            timeout_seconds: 30.0,
            context_window_tokens: 200_000,
            cache_control: None,
            reasoning: None,
            headers: serde_json::Map::new(),
            native_web_search: None,
            external_web_search: None,
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
    fn build_responses_input_converts_mixed_chat_history() {
        let messages = vec![
            ChatMessage::text("system", "System rules"),
            ChatMessage {
                role: "user".to_string(),
                content: Some(json!([
                    { "type": "text", "text": "Look at this" },
                    { "type": "image_url", "image_url": { "url": "https://example.com/a.png" } }
                ])),
                name: None,
                tool_call_id: None,
                tool_calls: None,
            },
            ChatMessage {
                role: "assistant".to_string(),
                content: Some(json!("Working on it")),
                name: None,
                tool_call_id: None,
                tool_calls: Some(vec![ToolCall {
                    id: "call_1".to_string(),
                    kind: "function".to_string(),
                    function: FunctionCall {
                        name: "read_file".to_string(),
                        arguments: Some("{\"path\":\"README.md\"}".to_string()),
                    },
                }]),
            },
            ChatMessage {
                role: "tool".to_string(),
                content: Some(json!("file contents")),
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
        assert_eq!(input[2]["name"], "read_file");
        assert_eq!(input[3]["type"], "function_call_output");
        assert_eq!(input[3]["call_id"], "call_1");
    }

    #[test]
    fn responses_output_converts_back_into_assistant_message() {
        let response = json!({
            "output": [
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
        assert_eq!(message.content, Some(json!("First part\n\nSecond part")));
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
}
