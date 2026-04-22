use super::UpstreamProvider;
use crate::config::UpstreamConfig;
use crate::llm::{
    ChatCompletionOutcome, ChatCompletionSession, build_chat_completions_url,
    build_claude_messages_input, claude_messages_value_to_chat_message,
    log_upstream_api_request_completed, log_upstream_api_request_failed,
    log_upstream_api_request_started, next_api_request_id, parse_usage,
    redacted_response_headers_json, redacted_upstream_request_headers_json_with_auth,
    request_cache_log_fields, should_bypass_proxy, upstream_error_from_value,
};
use crate::message::ChatMessage;
use crate::tooling::Tool;
use anyhow::{Context, Result, anyhow};
use serde_json::{Map, Value, json};
use std::time::{Duration, Instant};

pub(super) struct ClaudeCodeProvider;

impl UpstreamProvider for ClaudeCodeProvider {
    fn create_completion(
        &self,
        upstream: &UpstreamConfig,
        messages: &[ChatMessage],
        tools: &[Tool],
        extra_payload: Option<Map<String, Value>>,
        _session: Option<&mut ChatCompletionSession>,
    ) -> Result<ChatCompletionOutcome> {
        let messages_url = build_chat_completions_url(upstream);
        let mut client_builder = reqwest::blocking::Client::builder()
            .timeout(Duration::from_secs_f64(upstream.timeout_seconds));
        if should_bypass_proxy(&messages_url) {
            client_builder = client_builder.no_proxy();
        }
        let client = client_builder
            .build()
            .context("failed to construct upstream client")?;

        let (system, input_messages) =
            build_claude_messages_input(messages, upstream.cache_control.as_ref())?;
        let mut payload = Map::new();
        payload.insert("model".to_string(), Value::String(upstream.model.clone()));
        payload.insert("messages".to_string(), Value::Array(input_messages));
        payload.insert("max_tokens".to_string(), json!(4096));
        if !system.is_empty() {
            payload.insert("system".to_string(), Value::Array(system));
        }
        if !tools.is_empty() {
            payload.insert(
                "tools".to_string(),
                Value::Array(tools.iter().map(Tool::as_claude_tool).collect()),
            );
            payload.insert("tool_choice".to_string(), json!({ "type": "auto" }));
        }
        if let Some(extra_payload) = extra_payload {
            for (key, value) in extra_payload {
                payload.insert(key, value);
            }
        }

        let payload = Value::Object(payload);
        let request_cache = request_cache_log_fields(&payload);
        let api_request_id = next_api_request_id();
        let api_key = upstream
            .api_key
            .clone()
            .or_else(|| std::env::var(&upstream.api_key_env).ok());
        let request_headers_json = redacted_upstream_request_headers_json_with_auth(
            upstream,
            api_key.as_ref().map(|_| "x-api-key"),
        );
        log_upstream_api_request_started(
            &api_request_id,
            upstream,
            "claude_code_messages",
            "POST",
            &messages_url,
            &request_headers_json,
            &payload,
            &request_cache,
        );

        let mut request = client.post(&messages_url).json(&payload);
        if let Some(api_key) = api_key {
            request = request.header("x-api-key", api_key);
        }
        if !upstream
            .headers
            .keys()
            .any(|key| key.eq_ignore_ascii_case("anthropic-version"))
        {
            request = request.header("anthropic-version", "2023-06-01");
        }
        for (key, value) in &upstream.headers {
            if let Some(value) = value.as_str() {
                request = request.header(key, value);
            }
        }

        let started = Instant::now();
        let response = match request.send() {
            Ok(response) => response,
            Err(error) => {
                log_upstream_api_request_failed(
                    &api_request_id,
                    upstream,
                    "claude_code_messages",
                    None,
                    started.elapsed().as_millis() as u64,
                    "{}",
                    None,
                    &format!("{error:#}"),
                    &request_cache,
                );
                return Err(error).context("upstream claude messages request failed");
            }
        };
        let status = response.status();
        let response_headers_json = redacted_response_headers_json(response.headers());
        let body = match response.text() {
            Ok(body) => body,
            Err(error) => {
                log_upstream_api_request_failed(
                    &api_request_id,
                    upstream,
                    "claude_code_messages",
                    Some(status.as_u16()),
                    started.elapsed().as_millis() as u64,
                    &response_headers_json,
                    None,
                    &format!("{error:#}"),
                    &request_cache,
                );
                return Err(error).context("failed to read upstream claude messages body");
            }
        };
        if !status.is_success() {
            let response_body = serde_json::from_str::<Value>(&body)
                .unwrap_or_else(|_| Value::String(body.clone()));
            log_upstream_api_request_failed(
                &api_request_id,
                upstream,
                "claude_code_messages",
                Some(status.as_u16()),
                started.elapsed().as_millis() as u64,
                &response_headers_json,
                Some(&response_body),
                &format!("upstream claude messages failed with {}", status),
                &request_cache,
            );
            return Err(anyhow!(
                "upstream claude messages failed with {}: {}",
                status,
                body
            ));
        }

        let value: Value = match serde_json::from_str(&body) {
            Ok(value) => value,
            Err(error) => {
                let response_body = Value::String(body.clone());
                log_upstream_api_request_failed(
                    &api_request_id,
                    upstream,
                    "claude_code_messages",
                    Some(status.as_u16()),
                    started.elapsed().as_millis() as u64,
                    &response_headers_json,
                    Some(&response_body),
                    &format!("{error:#}"),
                    &request_cache,
                );
                return Err(error).context("failed to parse claude messages response");
            }
        };
        if let Some(error_message) = upstream_error_from_value(&value) {
            log_upstream_api_request_failed(
                &api_request_id,
                upstream,
                "claude_code_messages",
                Some(status.as_u16()),
                started.elapsed().as_millis() as u64,
                &response_headers_json,
                Some(&value),
                &error_message,
                &request_cache,
            );
            return Err(anyhow!(
                "upstream claude messages returned an error payload: {}",
                error_message
            ));
        }

        let usage = parse_usage(&value);
        log_upstream_api_request_completed(
            &api_request_id,
            upstream,
            "claude_code_messages",
            status.as_u16(),
            started.elapsed().as_millis() as u64,
            &response_headers_json,
            &value,
            &usage,
            None,
            &request_cache,
        );
        let message = claude_messages_value_to_chat_message(&value)?;
        Ok(ChatCompletionOutcome {
            message,
            usage,
            response_id: None,
            api_request_id: Some(api_request_id),
            request_cache,
        })
    }
}
