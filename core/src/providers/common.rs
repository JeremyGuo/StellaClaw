use base64::{engine::general_purpose, Engine as _};
use serde_json::{Map, Value};

use crate::session_actor::{FileItem, TokenUsage};

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

pub(crate) fn token_usage_from_value(value: &Value) -> Option<TokenUsage> {
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

    Some(TokenUsage {
        cache_read,
        cache_write,
        uncache_input: input.saturating_sub(cache_read.saturating_add(cache_write)),
        output,
    })
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
}
