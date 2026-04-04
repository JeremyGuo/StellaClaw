use super::{UpstreamProvider, openrouter_responses::responses_reasoning_payload};
use crate::config::{ReasoningConfig, UpstreamApiKind, UpstreamAuthKind, UpstreamConfig};
use crate::llm::{
    ChatCompletionOutcome, apply_auth_headers, build_chat_completions_url, build_responses_input,
    load_codex_auth, parse_streamed_responses_body, parse_usage, refresh_codex_auth,
    responses_value_to_chat_message, should_bypass_proxy, upstream_error_from_value,
};
use crate::message::ChatMessage;
use crate::tooling::Tool;
use anyhow::{Context, Result, anyhow};
use serde_json::{Map, Value};
use std::time::Duration;

pub(super) struct CodexSubscriptionProvider;

impl UpstreamProvider for CodexSubscriptionProvider {
    fn create_completion(
        &self,
        upstream: &UpstreamConfig,
        messages: &[ChatMessage],
        tools: &[Tool],
        extra_payload: Option<Map<String, Value>>,
    ) -> Result<ChatCompletionOutcome> {
        if upstream.api_kind != UpstreamApiKind::Responses {
            return Err(anyhow!(
                "codex-subscription currently only supports the responses api"
            ));
        }

        let responses_url = build_chat_completions_url(upstream);
        let mut client_builder = reqwest::blocking::Client::builder()
            .timeout(Duration::from_secs_f64(upstream.timeout_seconds));
        if should_bypass_proxy(&responses_url) {
            client_builder = client_builder.no_proxy();
        }
        let client = client_builder
            .build()
            .context("failed to construct upstream client")?;

        let (instructions, input) = build_responses_input(messages)?;
        let mut payload = Map::new();
        payload.insert("model".to_string(), Value::String(upstream.model.clone()));
        payload.insert("input".to_string(), Value::Array(input));
        payload.insert("store".to_string(), Value::Bool(false));
        payload.insert("stream".to_string(), Value::Bool(true));
        if let Some(instructions) = instructions {
            payload.insert("instructions".to_string(), Value::String(instructions));
        }
        if let Some(reasoning) = codex_reasoning_payload(upstream.reasoning.as_ref())? {
            payload.insert("reasoning".to_string(), reasoning);
        }
        if let Some(native_web_search) = &upstream.native_web_search
            && native_web_search.enabled
        {
            for (key, value) in &native_web_search.payload {
                payload.insert(key.clone(), value.clone());
            }
        }
        if !tools.is_empty() {
            payload.insert(
                "tools".to_string(),
                Value::Array(tools.iter().map(Tool::as_responses_tool).collect()),
            );
            payload.insert("parallel_tool_calls".to_string(), Value::Bool(true));
        }
        if let Some(extra_payload) = extra_payload {
            for (key, value) in extra_payload {
                payload.insert(key, value);
            }
        }
        payload.remove("max_completion_tokens");

        let auth = load_codex_auth(upstream)?;
        let mut request = client
            .post(&responses_url)
            .json(&Value::Object(payload.clone()));
        request = apply_auth_headers(request, upstream, auth.as_ref())?;
        request = request.header(reqwest::header::ACCEPT, "text/event-stream");
        for (key, value) in &upstream.headers {
            if let Some(value) = value.as_str() {
                request = request.header(key, value);
            }
        }

        let mut response = request
            .send()
            .context("upstream responses request failed")?;
        if response.status() == reqwest::StatusCode::UNAUTHORIZED
            && upstream.auth_kind == UpstreamAuthKind::CodexSubscription
        {
            if let Some(ref auth) = auth
                && let Some(refreshed) = refresh_codex_auth(upstream, auth)?
            {
                let mut retry = client.post(&responses_url).json(&Value::Object(payload));
                retry = apply_auth_headers(retry, upstream, Some(&refreshed))?;
                retry = retry.header(reqwest::header::ACCEPT, "text/event-stream");
                for (key, value) in &upstream.headers {
                    if let Some(value) = value.as_str() {
                        retry = retry.header(key, value);
                    }
                }
                response = retry
                    .send()
                    .context("upstream responses request failed after token refresh")?;
            }
        }

        let status = response.status();
        let body = response
            .text()
            .context("failed to read upstream responses body")?;
        if !status.is_success() {
            return Err(anyhow!(
                "upstream responses failed with {}: {}",
                status,
                body
            ));
        }

        let value = parse_streamed_responses_body(&body)?;
        if let Some(error_message) = upstream_error_from_value(&value) {
            return Err(anyhow!(
                "upstream responses returned an error payload: {}",
                error_message
            ));
        }
        let usage = parse_usage(&value);
        let message = responses_value_to_chat_message(&value)?;
        Ok(ChatCompletionOutcome { message, usage })
    }
}

pub(crate) fn codex_reasoning_payload(
    reasoning: Option<&ReasoningConfig>,
) -> Result<Option<Value>> {
    let Some(mut payload) = responses_reasoning_payload(reasoning)? else {
        return Ok(None);
    };
    if let Some(object) = payload.as_object_mut() {
        object.remove("max_tokens");
        object.remove("exclude");
        object.remove("enabled");
    }
    Ok(Some(payload))
}
