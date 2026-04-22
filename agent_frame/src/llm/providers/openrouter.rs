use super::UpstreamProvider;
use crate::config::{CacheControlConfig, UpstreamConfig};
use crate::llm::{
    ChatCompletionOutcome, ChatCompletionResponse, ChatCompletionSession, ImageGenerationOutcome,
    build_chat_completions_url, chat_completions_messages_payload,
    generated_image_reference_from_value, log_upstream_api_request_completed,
    log_upstream_api_request_failed, log_upstream_api_request_started, next_api_request_id,
    parse_usage, redacted_response_headers_json, redacted_upstream_request_headers_json,
    should_bypass_proxy, upstream_error_from_value,
};
use crate::message::ChatMessage;
use crate::tooling::Tool;
use anyhow::{Context, Result, anyhow};
use serde_json::{Map, Value};
use std::time::{Duration, Instant};

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
            chat_completions_messages_payload(messages)
                .context("failed to serialize chat completion messages")?,
        );
        if let Some(cache_control) = &upstream.cache_control {
            payload.insert(
                "cache_control".to_string(),
                openrouter_cache_control_payload(cache_control)
                    .context("failed to serialize cache_control")?,
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
            "openrouter_chat_completions",
            "POST",
            &chat_completions_url,
            &request_headers_json,
            &payload,
        );

        let mut request = client.post(&chat_completions_url).json(&payload);

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
                    "openrouter_chat_completions",
                    None,
                    started.elapsed().as_millis() as u64,
                    "{}",
                    None,
                    &format!("{error:#}"),
                );
                return Err(error).context("upstream chat completion request failed");
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
                    "openrouter_chat_completions",
                    Some(status.as_u16()),
                    started.elapsed().as_millis() as u64,
                    &response_headers_json,
                    None,
                    &format!("{error:#}"),
                );
                return Err(error).context("failed to read upstream response body");
            }
        };
        if !status.is_success() {
            let response_body = serde_json::from_str::<Value>(&body)
                .unwrap_or_else(|_| Value::String(body.clone()));
            log_upstream_api_request_failed(
                &api_request_id,
                upstream,
                "openrouter_chat_completions",
                Some(status.as_u16()),
                started.elapsed().as_millis() as u64,
                &response_headers_json,
                Some(&response_body),
                &format!("upstream chat completion failed with {}", status),
            );
            return Err(anyhow!(
                "upstream chat completion failed with {}: {}",
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
                    "openrouter_chat_completions",
                    Some(status.as_u16()),
                    started.elapsed().as_millis() as u64,
                    &response_headers_json,
                    Some(&response_body),
                    &format!("{error:#}"),
                );
                return Err(error).context("failed to parse chat completion response");
            }
        };
        if let Some(error_message) = upstream_error_from_value(&value) {
            log_upstream_api_request_failed(
                &api_request_id,
                upstream,
                "openrouter_chat_completions",
                Some(status.as_u16()),
                started.elapsed().as_millis() as u64,
                &response_headers_json,
                Some(&value),
                &error_message,
            );
            return Err(anyhow!(
                "upstream chat completion returned an error payload: {}",
                error_message
            ));
        }
        let usage = parse_usage(&value);
        log_upstream_api_request_completed(
            &api_request_id,
            upstream,
            "openrouter_chat_completions",
            status.as_u16(),
            started.elapsed().as_millis() as u64,
            &response_headers_json,
            &value,
            &usage,
            None,
        );
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
            api_request_id: Some(api_request_id),
        })
    }

    fn create_image_generation(
        &self,
        upstream: &UpstreamConfig,
        messages: &[ChatMessage],
    ) -> Result<ImageGenerationOutcome> {
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
            chat_completions_messages_payload(messages)
                .context("failed to serialize image generation messages")?,
        );
        payload.insert("modalities".to_string(), serde_json::json!(["image"]));
        payload.insert("stream".to_string(), Value::Bool(false));

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
            "openrouter_chat_image_generation",
            "POST",
            &chat_completions_url,
            &request_headers_json,
            &payload,
        );

        let mut request = client.post(&chat_completions_url).json(&payload);
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
                    "openrouter_chat_image_generation",
                    None,
                    started.elapsed().as_millis() as u64,
                    "{}",
                    None,
                    &format!("{error:#}"),
                );
                return Err(error).context("upstream image generation request failed");
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
                    "openrouter_chat_image_generation",
                    Some(status.as_u16()),
                    started.elapsed().as_millis() as u64,
                    &response_headers_json,
                    None,
                    &format!("{error:#}"),
                );
                return Err(error).context("failed to read image generation response body");
            }
        };
        if !status.is_success() {
            let response_body = serde_json::from_str::<Value>(&body)
                .unwrap_or_else(|_| Value::String(body.clone()));
            log_upstream_api_request_failed(
                &api_request_id,
                upstream,
                "openrouter_chat_image_generation",
                Some(status.as_u16()),
                started.elapsed().as_millis() as u64,
                &response_headers_json,
                Some(&response_body),
                &format!("upstream image generation failed with {}", status),
            );
            return Err(anyhow!(
                "upstream image generation failed with {}: {}",
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
                    "openrouter_chat_image_generation",
                    Some(status.as_u16()),
                    started.elapsed().as_millis() as u64,
                    &response_headers_json,
                    Some(&response_body),
                    &format!("{error:#}"),
                );
                return Err(error).context("failed to parse image generation response");
            }
        };
        if let Some(error_message) = upstream_error_from_value(&value) {
            log_upstream_api_request_failed(
                &api_request_id,
                upstream,
                "openrouter_chat_image_generation",
                Some(status.as_u16()),
                started.elapsed().as_millis() as u64,
                &response_headers_json,
                Some(&value),
                &error_message,
            );
            return Err(anyhow!(
                "upstream image generation returned an error payload: {}",
                error_message
            ));
        }

        let usage = parse_usage(&value);
        log_upstream_api_request_completed(
            &api_request_id,
            upstream,
            "openrouter_chat_image_generation",
            status.as_u16(),
            started.elapsed().as_millis() as u64,
            &response_headers_json,
            &value,
            &usage,
            None,
        );
        let image_reference = generated_image_reference_from_value(&value)
            .ok_or_else(|| anyhow!("image generation response did not contain image data"))?;
        Ok(ImageGenerationOutcome {
            image_reference,
            usage,
            response_id: None,
            api_request_id: Some(api_request_id),
        })
    }
}

pub(super) fn openrouter_cache_control_payload(
    cache_control: &CacheControlConfig,
) -> Result<Value> {
    let mut cache_control = cache_control.clone();
    if cache_control
        .ttl
        .as_deref()
        .is_some_and(is_default_anthropic_cache_ttl)
    {
        cache_control.ttl = None;
    }
    serde_json::to_value(cache_control).context("failed to serialize cache_control")
}

fn is_default_anthropic_cache_ttl(ttl: &str) -> bool {
    ttl.trim().eq_ignore_ascii_case("5m")
}

#[cfg(test)]
mod tests {
    use super::openrouter_cache_control_payload;
    use crate::config::CacheControlConfig;
    use serde_json::json;

    #[test]
    fn cache_control_omits_five_minute_default_ttl() {
        let value = openrouter_cache_control_payload(&CacheControlConfig {
            cache_type: "ephemeral".to_string(),
            ttl: Some("5m".to_string()),
        })
        .unwrap();

        assert_eq!(value, json!({ "type": "ephemeral" }));
    }

    #[test]
    fn cache_control_preserves_one_hour_ttl() {
        let value = openrouter_cache_control_payload(&CacheControlConfig {
            cache_type: "ephemeral".to_string(),
            ttl: Some("1h".to_string()),
        })
        .unwrap();

        assert_eq!(value, json!({ "type": "ephemeral", "ttl": "1h" }));
    }
}
