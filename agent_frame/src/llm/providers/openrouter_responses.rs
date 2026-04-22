use super::{UpstreamProvider, openrouter::openrouter_cache_control_payload};
use crate::config::{ReasoningConfig, UpstreamConfig};
use crate::llm::{
    ChatCompletionOutcome, ChatCompletionSession, ImageGenerationOutcome,
    build_chat_completions_url, build_responses_input, build_responses_tools_payload,
    generated_image_reference_from_value, log_upstream_api_request_completed,
    log_upstream_api_request_failed, log_upstream_api_request_started, next_api_request_id,
    parse_usage, redacted_response_headers_json, redacted_upstream_request_headers_json,
    response_id_from_value, responses_value_to_chat_message, should_bypass_proxy,
    upstream_error_from_value,
};
use crate::message::ChatMessage;
use crate::tooling::Tool;
use anyhow::{Context, Result, anyhow};
use serde_json::{Map, Value};
use std::time::{Duration, Instant};

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
        insert_responses_cache_payload(&mut payload, upstream)?;
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

        let payload = Value::Object(payload);
        let api_request_id = next_api_request_id();
        let api_key = upstream
            .api_key
            .clone()
            .or_else(|| std::env::var(&upstream.api_key_env).ok());
        let request_headers_json =
            redacted_upstream_request_headers_json(upstream, api_key.is_some());
        log_upstream_api_request_started(
            &api_request_id,
            upstream,
            "openrouter_responses",
            "POST",
            &responses_url,
            &request_headers_json,
            &payload,
        );

        let mut request = client.post(&responses_url).json(&payload);
        if let Some(api_key) = api_key {
            request = request.bearer_auth(api_key);
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
                    "openrouter_responses",
                    None,
                    started.elapsed().as_millis() as u64,
                    "{}",
                    None,
                    &format!("{error:#}"),
                );
                return Err(error).context("upstream responses request failed");
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
                    "openrouter_responses",
                    Some(status.as_u16()),
                    started.elapsed().as_millis() as u64,
                    &response_headers_json,
                    None,
                    &format!("{error:#}"),
                );
                return Err(error).context("failed to read upstream responses body");
            }
        };
        if !status.is_success() {
            let response_body = serde_json::from_str::<Value>(&body)
                .unwrap_or_else(|_| Value::String(body.clone()));
            log_upstream_api_request_failed(
                &api_request_id,
                upstream,
                "openrouter_responses",
                Some(status.as_u16()),
                started.elapsed().as_millis() as u64,
                &response_headers_json,
                Some(&response_body),
                &format!("upstream responses failed with {}", status),
            );
            return Err(anyhow!(
                "upstream responses failed with {}: {}",
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
                    "openrouter_responses",
                    Some(status.as_u16()),
                    started.elapsed().as_millis() as u64,
                    &response_headers_json,
                    Some(&response_body),
                    &format!("{error:#}"),
                );
                return Err(error).context("failed to parse responses response");
            }
        };
        if let Some(error_message) = upstream_error_from_value(&value) {
            log_upstream_api_request_failed(
                &api_request_id,
                upstream,
                "openrouter_responses",
                Some(status.as_u16()),
                started.elapsed().as_millis() as u64,
                &response_headers_json,
                Some(&value),
                &error_message,
            );
            return Err(anyhow!(
                "upstream responses returned an error payload: {}",
                error_message
            ));
        }
        let usage = parse_usage(&value);
        let response_id = response_id_from_value(&value);
        log_upstream_api_request_completed(
            &api_request_id,
            upstream,
            "openrouter_responses",
            status.as_u16(),
            started.elapsed().as_millis() as u64,
            &response_headers_json,
            &value,
            &usage,
            response_id.as_deref(),
        );
        let message = responses_value_to_chat_message(&value)?;
        Ok(ChatCompletionOutcome {
            message,
            usage,
            response_id,
            api_request_id: Some(api_request_id),
        })
    }

    fn create_image_generation(
        &self,
        upstream: &UpstreamConfig,
        messages: &[ChatMessage],
    ) -> Result<ImageGenerationOutcome> {
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
        payload.insert("modalities".to_string(), serde_json::json!(["image"]));
        if let Some(instructions) = instructions {
            payload.insert("instructions".to_string(), Value::String(instructions));
        }
        insert_responses_cache_payload(&mut payload, upstream)?;

        let payload = Value::Object(payload);
        let api_request_id = next_api_request_id();
        let api_key = upstream
            .api_key
            .clone()
            .or_else(|| std::env::var(&upstream.api_key_env).ok());
        let request_headers_json =
            redacted_upstream_request_headers_json(upstream, api_key.is_some());
        log_upstream_api_request_started(
            &api_request_id,
            upstream,
            "openrouter_responses_image_generation",
            "POST",
            &responses_url,
            &request_headers_json,
            &payload,
        );

        let mut request = client.post(&responses_url).json(&payload);
        if let Some(api_key) = api_key {
            request = request.bearer_auth(api_key);
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
                    "openrouter_responses_image_generation",
                    None,
                    started.elapsed().as_millis() as u64,
                    "{}",
                    None,
                    &format!("{error:#}"),
                );
                return Err(error).context("upstream responses image generation request failed");
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
                    "openrouter_responses_image_generation",
                    Some(status.as_u16()),
                    started.elapsed().as_millis() as u64,
                    &response_headers_json,
                    None,
                    &format!("{error:#}"),
                );
                return Err(error).context("failed to read image generation responses body");
            }
        };
        if !status.is_success() {
            let response_body = serde_json::from_str::<Value>(&body)
                .unwrap_or_else(|_| Value::String(body.clone()));
            log_upstream_api_request_failed(
                &api_request_id,
                upstream,
                "openrouter_responses_image_generation",
                Some(status.as_u16()),
                started.elapsed().as_millis() as u64,
                &response_headers_json,
                Some(&response_body),
                &format!("upstream responses image generation failed with {}", status),
            );
            return Err(anyhow!(
                "upstream responses image generation failed with {}: {}",
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
                    "openrouter_responses_image_generation",
                    Some(status.as_u16()),
                    started.elapsed().as_millis() as u64,
                    &response_headers_json,
                    Some(&response_body),
                    &format!("{error:#}"),
                );
                return Err(error).context("failed to parse image generation responses body");
            }
        };
        if let Some(error_message) = upstream_error_from_value(&value) {
            log_upstream_api_request_failed(
                &api_request_id,
                upstream,
                "openrouter_responses_image_generation",
                Some(status.as_u16()),
                started.elapsed().as_millis() as u64,
                &response_headers_json,
                Some(&value),
                &error_message,
            );
            return Err(anyhow!(
                "upstream responses image generation returned an error payload: {}",
                error_message
            ));
        }

        let usage = parse_usage(&value);
        let response_id = response_id_from_value(&value);
        log_upstream_api_request_completed(
            &api_request_id,
            upstream,
            "openrouter_responses_image_generation",
            status.as_u16(),
            started.elapsed().as_millis() as u64,
            &response_headers_json,
            &value,
            &usage,
            response_id.as_deref(),
        );
        let image_reference = generated_image_reference_from_value(&value)
            .ok_or_else(|| anyhow!("image generation response did not contain image data"))?;
        Ok(ImageGenerationOutcome {
            image_reference,
            usage,
            response_id,
            api_request_id: Some(api_request_id),
        })
    }
}

pub(super) fn insert_responses_cache_payload(
    payload: &mut Map<String, Value>,
    upstream: &UpstreamConfig,
) -> Result<()> {
    if let Some(cache_control) = &upstream.cache_control {
        payload.insert(
            "cache_control".to_string(),
            openrouter_cache_control_payload(cache_control)
                .context("failed to serialize cache_control")?,
        );
    }
    if let Some(prompt_cache_key) = upstream.prompt_cache_key.as_ref() {
        payload.insert(
            "prompt_cache_key".to_string(),
            Value::String(prompt_cache_key.clone()),
        );
    }
    if let Some(prompt_cache_retention) = upstream.prompt_cache_retention.as_ref() {
        payload.insert(
            "prompt_cache_retention".to_string(),
            Value::String(prompt_cache_retention.clone()),
        );
    }
    Ok(())
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

#[cfg(test)]
mod tests {
    use super::insert_responses_cache_payload;
    use crate::config::{
        AuthCredentialsStoreMode, CacheControlConfig, UpstreamApiKind, UpstreamAuthKind,
        UpstreamConfig,
    };
    use serde_json::{Map, Value, json};

    fn test_upstream() -> UpstreamConfig {
        UpstreamConfig {
            base_url: "https://openrouter.ai/api/v1".to_string(),
            model: "anthropic/claude-opus-4.6".to_string(),
            api_kind: UpstreamApiKind::Responses,
            auth_kind: UpstreamAuthKind::ApiKey,
            supports_vision_input: true,
            supports_pdf_input: false,
            supports_audio_input: false,
            api_key: None,
            api_key_env: "OPENROUTER_API_KEY".to_string(),
            chat_completions_path: "/responses".to_string(),
            codex_home: None,
            codex_auth: None,
            auth_credentials_store_mode: AuthCredentialsStoreMode::Auto,
            timeout_seconds: 30.0,
            retry_mode: Default::default(),
            context_window_tokens: 128_000,
            cache_control: Some(CacheControlConfig {
                cache_type: "ephemeral".to_string(),
                ttl: Some("5m".to_string()),
            }),
            prompt_cache_retention: Some("5m".to_string()),
            prompt_cache_key: Some("session-version".to_string()),
            reasoning: None,
            headers: Default::default(),
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
    fn responses_cache_payload_forwards_cache_fields() {
        let mut payload = Map::new();
        insert_responses_cache_payload(&mut payload, &test_upstream()).unwrap();

        assert_eq!(payload["cache_control"], json!({ "type": "ephemeral" }));
        assert_eq!(
            payload["prompt_cache_key"],
            Value::String("session-version".to_string())
        );
        assert_eq!(
            payload["prompt_cache_retention"],
            Value::String("5m".to_string())
        );
    }
}
