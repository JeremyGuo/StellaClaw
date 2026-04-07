use super::UpstreamProvider;
use crate::config::{ReasoningConfig, UpstreamConfig};
use crate::llm::{
    ChatCompletionOutcome, ChatCompletionSession, build_chat_completions_url,
    build_responses_input, build_responses_tools_payload, parse_usage, response_id_from_value,
    responses_value_to_chat_message, should_bypass_proxy, upstream_error_from_value,
};
use crate::message::ChatMessage;
use crate::tooling::Tool;
use anyhow::{Context, Result, anyhow};
use serde_json::{Map, Value};
use std::time::Duration;

pub(super) struct OpenRouterResponsesProvider;

impl UpstreamProvider for OpenRouterResponsesProvider {
    fn create_completion(
        &self,
        upstream: &UpstreamConfig,
        messages: &[ChatMessage],
        tools: &[Tool],
        extra_payload: Option<Map<String, Value>>,
        _session: Option<&mut ChatCompletionSession>,
    ) -> Result<ChatCompletionOutcome> {
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
        if let Some(instructions) = instructions {
            payload.insert("instructions".to_string(), Value::String(instructions));
        }
        if let Some(reasoning) = responses_reasoning_payload(upstream.reasoning.as_ref())? {
            payload.insert("reasoning".to_string(), reasoning);
        }
        let response_tools = build_responses_tools_payload(upstream, tools);
        if !response_tools.is_empty() {
            payload.insert("tools".to_string(), Value::Array(response_tools));
            payload.insert("parallel_tool_calls".to_string(), Value::Bool(true));
        }
        if let Some(extra_payload) = extra_payload {
            for (key, value) in extra_payload {
                payload.insert(key, value);
            }
        }

        let mut request = client.post(&responses_url).json(&Value::Object(payload));
        if let Some(api_key) = upstream
            .api_key
            .clone()
            .or_else(|| std::env::var(&upstream.api_key_env).ok())
        {
            request = request.bearer_auth(api_key);
        }
        for (key, value) in &upstream.headers {
            if let Some(value) = value.as_str() {
                request = request.header(key, value);
            }
        }

        let response = request
            .send()
            .context("upstream responses request failed")?;
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

        let value: Value =
            serde_json::from_str(&body).context("failed to parse responses response")?;
        if let Some(error_message) = upstream_error_from_value(&value) {
            return Err(anyhow!(
                "upstream responses returned an error payload: {}",
                error_message
            ));
        }
        let usage = parse_usage(&value);
        let message = responses_value_to_chat_message(&value)?;
        Ok(ChatCompletionOutcome {
            message,
            usage,
            response_id: response_id_from_value(&value),
        })
    }
}

pub(crate) fn responses_reasoning_payload(
    reasoning: Option<&ReasoningConfig>,
) -> Result<Option<Value>> {
    let Some(reasoning) = reasoning else {
        return Ok(None);
    };
    let mut payload = Map::new();
    if let Some(effort) = reasoning.effort.as_ref() {
        payload.insert("effort".to_string(), Value::String(effort.clone()));
    }
    if let Some(max_tokens) = reasoning.max_tokens {
        payload.insert("max_tokens".to_string(), Value::Number(max_tokens.into()));
    }
    if let Some(exclude) = reasoning.exclude {
        payload.insert("exclude".to_string(), Value::Bool(exclude));
    }
    if let Some(enabled) = reasoning.enabled {
        payload.insert("enabled".to_string(), Value::Bool(enabled));
    }
    Ok((!payload.is_empty()).then_some(Value::Object(payload)))
}
