use base64::{engine::general_purpose, Engine as _};
use serde_json::{Map, Value};

use crate::{
    model_config::{ModelConfig, ProviderType},
    session_actor::{FileItem, TokenUsage},
};

use super::pricing::PriceManager;

pub(crate) fn is_image_file(file: &FileItem) -> bool {
    matches!(file.media_type.as_deref(), Some(media_type) if media_type.starts_with("image/"))
}

pub(crate) fn provider_error_message(value: &Value) -> Option<String> {
    value
        .get("error")
        .and_then(|error| {
            error
                .get("message")
                .and_then(Value::as_str)
                .or_else(|| error.as_str())
        })
        .map(str::to_string)
        .or_else(|| {
            value
                .get("response")
                .and_then(|response| response.get("error"))
                .and_then(|error| {
                    error
                        .get("message")
                        .and_then(Value::as_str)
                        .or_else(|| error.as_str())
                })
                .map(str::to_string)
        })
}

pub(crate) fn token_usage_from_value(
    value: &Value,
    model_config: &ModelConfig,
) -> Option<TokenUsage> {
    let usage = value.get("usage").and_then(Value::as_object)?;

    let input = first_u64(
        usage,
        &[
            &["prompt_tokens"],
            &["input_tokens"],
            &["input_tokens_details", "total_tokens"],
        ],
    )
    .unwrap_or(0);
    let output = first_u64(usage, &[&["completion_tokens"], &["output_tokens"]]).unwrap_or(0);
    let cache_read = first_u64(
        usage,
        &[
            &["prompt_tokens_details", "cached_tokens"],
            &["input_tokens_details", "cached_tokens"],
            &["cache_read_input_tokens"],
        ],
    )
    .unwrap_or(0);
    let cache_write = first_u64(
        usage,
        &[
            &["input_tokens_details", "cache_creation_tokens"],
            &["cache_creation_input_tokens"],
            &["cache_creation", "ephemeral_5m_input_tokens"],
            &["cache_creation", "ephemeral_1h_input_tokens"],
        ],
    )
    .unwrap_or(0);

    let mut token_usage = TokenUsage {
        cache_read,
        cache_write,
        uncache_input: input.saturating_sub(cache_read.saturating_add(cache_write)),
        output,
        cost_usd: None,
    };
    PriceManager::attach_cost(model_config, &mut token_usage);
    Some(token_usage)
}

pub(crate) fn account_id_from_access_token(access_token: &str) -> Option<String> {
    let mut parts = access_token.split('.');
    let (_, payload, _) = (parts.next()?, parts.next()?, parts.next()?);
    let payload = general_purpose::URL_SAFE_NO_PAD.decode(payload).ok()?;
    let value = serde_json::from_slice::<Value>(&payload).ok()?;
    value
        .get("https://api.openai.com/auth")
        .and_then(Value::as_object)
        .and_then(|auth| auth.get("chatgpt_account_id"))
        .and_then(Value::as_str)
        .map(str::to_string)
}

pub(crate) fn data_url_parts(url: &str) -> Option<(String, String)> {
    let (metadata, data) = url.strip_prefix("data:")?.split_once(',')?;
    let mut parts = metadata.split(';');
    let media_type = parts.next()?.to_string();
    if !parts.any(|part| part.eq_ignore_ascii_case("base64")) {
        return None;
    }
    Some((media_type, data.to_string()))
}

pub(crate) fn openrouter_cache_control_payload(model_config: &ModelConfig) -> Option<Value> {
    let ttl = automatic_anthropic_cache_ttl(model_config)?;
    let mut object = Map::new();
    object.insert("type".to_string(), Value::String("ephemeral".to_string()));
    if ttl != "5m" {
        object.insert("ttl".to_string(), Value::String(ttl));
    }
    Some(Value::Object(object))
}

pub(crate) fn claude_cache_control_payload(model_config: &ModelConfig) -> Option<Value> {
    let ttl = automatic_anthropic_cache_ttl(model_config)?;
    let mut object = Map::new();
    object.insert("type".to_string(), Value::String("ephemeral".to_string()));
    object.insert("ttl".to_string(), Value::String(ttl));
    Some(Value::Object(object))
}

fn automatic_anthropic_cache_ttl(model_config: &ModelConfig) -> Option<String> {
    if !supports_anthropic_prompt_cache(model_config) {
        return None;
    }
    match model_config.cache_timeout {
        300 => Some("5m".to_string()),
        3600 => Some("1h".to_string()),
        _ => None,
    }
}

fn supports_anthropic_prompt_cache(model_config: &ModelConfig) -> bool {
    match model_config.provider_type {
        ProviderType::OpenRouterCompletion | ProviderType::OpenRouterResponses => {
            model_config.model_name.starts_with("anthropic/claude-")
        }
        ProviderType::ClaudeCode => model_config.model_name.starts_with("claude-"),
        ProviderType::OpenAiImageEdit
        | ProviderType::CodexSubscription
        | ProviderType::BraveSearch => false,
    }
}

pub(crate) fn nonce(prefix: &str) -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or_default();
    format!("{prefix}-{nanos}")
}

fn first_u64(object: &Map<String, Value>, paths: &[&[&str]]) -> Option<u64> {
    'paths: for path in paths {
        let mut current = None;
        for (index, key) in path.iter().enumerate() {
            current = if index == 0 {
                object.get(*key)
            } else {
                current.and_then(|value: &Value| value.get(*key))
            };

            if current.is_none() {
                continue 'paths;
            }
        }
        if let Some(value) = current.and_then(Value::as_u64) {
            return Some(value);
        }
    }

    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model_config::{ModelCapability, RetryMode, TokenEstimatorType};

    fn test_model_config(
        provider_type: ProviderType,
        model_name: &str,
        cache_timeout: u64,
    ) -> ModelConfig {
        ModelConfig {
            provider_type,
            model_name: model_name.to_string(),
            url: "https://example.invalid".to_string(),
            api_key_env: "TEST_API_KEY".to_string(),
            capabilities: vec![ModelCapability::Chat],
            token_max_context: 128_000,
            cache_timeout,
            conn_timeout: 30,
            retry_mode: RetryMode::Once,
            reasoning: None,
            token_estimator_type: TokenEstimatorType::Local,
            multimodal_estimator: None,
            multimodal_input: None,
            token_estimator_url: None,
        }
    }

    #[test]
    fn parses_codex_account_id_from_jwt() {
        let payload = general_purpose::URL_SAFE_NO_PAD
            .encode(br#"{"https://api.openai.com/auth":{"chatgpt_account_id":"acc_123"}}"#);
        let token = format!("header.{payload}.sig");

        assert_eq!(
            account_id_from_access_token(&token),
            Some("acc_123".to_string())
        );
    }

    #[test]
    fn cache_control_only_targets_anthropic_models_with_supported_ttl() {
        let openrouter = test_model_config(
            ProviderType::OpenRouterCompletion,
            "anthropic/claude-sonnet-4.5",
            300,
        );
        assert_eq!(
            openrouter_cache_control_payload(&openrouter),
            Some(serde_json::json!({"type": "ephemeral"}))
        );

        let claude = test_model_config(ProviderType::ClaudeCode, "claude-sonnet-4-5", 3600);
        assert_eq!(
            claude_cache_control_payload(&claude),
            Some(serde_json::json!({"type": "ephemeral", "ttl": "1h"}))
        );

        let non_anthropic =
            test_model_config(ProviderType::OpenRouterCompletion, "openai/gpt-4.1", 300);
        assert!(openrouter_cache_control_payload(&non_anthropic).is_none());

        let unsupported_ttl = test_model_config(
            ProviderType::OpenRouterCompletion,
            "anthropic/claude-sonnet-4.5",
            600,
        );
        assert!(openrouter_cache_control_payload(&unsupported_ttl).is_none());
    }
}
