use super::UpstreamProvider;
use crate::config::UpstreamConfig;
use crate::llm::{
    ChatCompletionOutcome, ChatCompletionResponse, ChatCompletionSession,
    build_chat_completions_url, parse_usage, should_bypass_proxy, upstream_error_from_value,
};
use crate::message::ChatMessage;
use crate::tooling::Tool;
use anyhow::{Context, Result, anyhow};
use serde_json::{Map, Value};
use std::time::Duration;

pub(super) struct OpenRouterProvider;

impl UpstreamProvider for OpenRouterProvider {
    fn create_completion(
        &self,
        upstream: &UpstreamConfig,
        messages: &[ChatMessage],
        tools: &[Tool],
        extra_payload: Option<Map<String, Value>>,
        _session: Option<&mut ChatCompletionSession>,
    ) -> Result<ChatCompletionOutcome> {
        let chat_completions_url = build_chat_completions_url(upstream);
        let mut client_builder = reqwest::blocking::Client::builder()
            .timeout(Duration::from_secs_f64(upstream.timeout_seconds));
        if should_bypass_proxy(&chat_completions_url) {
            client_builder = client_builder.no_proxy();
        }
        let client = client_builder
            .build()
            .context("failed to construct upstream client")?;

        let mut payload = Map::new();
        payload.insert("model".to_string(), Value::String(upstream.model.clone()));
        payload.insert(
            "messages".to_string(),
            serde_json::to_value(messages).context("failed to serialize messages")?,
        );
        if let Some(cache_control) = &upstream.cache_control {
            payload.insert(
                "cache_control".to_string(),
                serde_json::to_value(cache_control).context("failed to serialize cache_control")?,
            );
        }
        if let Some(reasoning) = &upstream.reasoning {
            payload.insert(
                "reasoning".to_string(),
                serde_json::to_value(reasoning).context("failed to serialize reasoning config")?,
            );
        }
        if !tools.is_empty() {
            payload.insert(
                "tools".to_string(),
                Value::Array(tools.iter().map(Tool::as_openai_tool).collect()),
            );
            payload.insert("tool_choice".to_string(), Value::String("auto".to_string()));
        }
        if let Some(extra_payload) = extra_payload {
            for (key, value) in extra_payload {
                payload.insert(key, value);
            }
        }

        let mut request = client
            .post(chat_completions_url)
            .json(&Value::Object(payload));

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
            .context("upstream chat completion request failed")?;
        let status = response.status();
        let body = response
            .text()
            .context("failed to read upstream response body")?;
        if !status.is_success() {
            return Err(anyhow!(
                "upstream chat completion failed with {}: {}",
                status,
                body
            ));
        }

        let value: Value =
            serde_json::from_str(&body).context("failed to parse chat completion response")?;
        if let Some(error_message) = upstream_error_from_value(&value) {
            return Err(anyhow!(
                "upstream chat completion returned an error payload: {}",
                error_message
            ));
        }
        let usage = parse_usage(&value);
        let parsed: ChatCompletionResponse =
            serde_json::from_value(value).context("failed to decode chat completion response")?;
        let message = parsed
            .choices
            .into_iter()
            .next()
            .map(|choice| choice.message)
            .ok_or_else(|| {
                anyhow!("invalid chat completion response: missing choices[0].message")
            })?;
        Ok(ChatCompletionOutcome {
            message,
            usage,
            response_id: None,
        })
    }
}
