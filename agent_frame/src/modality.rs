use crate::config::AgentConfig;
use crate::message::ChatMessage;
use anyhow::{Context, Result};
use base64::Engine;
use serde_json::{Value, json};
use std::collections::hash_map::DefaultHasher;
use std::fs;
use std::hash::{Hash, Hasher};
use std::path::Path;

const CONTEXT_ATTACHMENT_STORE_DIR_NAME: &str = ".context_attachments";
const CONTEXT_ATTACHMENT_HASH_DIR_NAME: &str = "by-hash";

pub(crate) fn materialize_messages_for_upstream(
    messages: &[ChatMessage],
    config: &AgentConfig,
) -> Result<Vec<ChatMessage>> {
    let allow_images = config.upstream.supports_vision_input;
    let allow_files = config.upstream.supports_pdf_input;
    let allow_audio = config.upstream.supports_audio_input;
    let capability_text = "current model/backend combination";

    let mut rewritten_messages = Vec::with_capacity(messages.len());
    for message in messages {
        let Some(Value::Array(items)) = &message.content else {
            rewritten_messages.push(message.clone());
            continue;
        };
        let mut rewritten_items = Vec::with_capacity(items.len());
        let mut changed = false;
        for item in items {
            let Some(item_type) = item.get("type").and_then(Value::as_str) else {
                rewritten_items.push(item.clone());
                continue;
            };
            let should_downgrade = match item_type {
                "image_url" | "input_image" => !allow_images,
                "file" | "input_file" => !allow_files,
                "input_audio" => !allow_audio,
                _ => false,
            };
            if !should_downgrade {
                rewritten_items.push(item.clone());
                continue;
            }
            let relative_path =
                persist_request_view_artifact(&config.workspace_root, item_type, item)?;
            rewritten_items.push(json!({
                "type": "text",
                "text": request_view_placeholder_text(item_type, capability_text, &relative_path),
            }));
            changed = true;
        }
        if changed {
            let mut rewritten = message.clone();
            rewritten.content = Some(Value::Array(rewritten_items));
            rewritten_messages.push(rewritten);
        } else {
            rewritten_messages.push(message.clone());
        }
    }

    Ok(rewritten_messages)
}

fn request_view_placeholder_text(
    item_type: &str,
    capability_text: &str,
    relative_path: &str,
) -> String {
    match item_type {
        "image_url" | "input_image" => format!(
            "[Earlier image is referenced at {relative_path} because the {capability_text} does not accept image input. Inspect it with tools if needed.]"
        ),
        "file" | "input_file" => format!(
            "[Earlier file is referenced at {relative_path} because the {capability_text} does not accept file input. Inspect it with tools if needed.]"
        ),
        "input_audio" => format!(
            "[Earlier audio is referenced at {relative_path} because the {capability_text} does not accept audio input. Inspect it with tools if needed.]"
        ),
        _ => format!(
            "[Earlier multimodal content is referenced at {relative_path}. Inspect it with tools if needed.]"
        ),
    }
}

fn persist_request_view_artifact(
    workspace_root: &Path,
    item_type: &str,
    item: &Value,
) -> Result<String> {
    let directory = workspace_root
        .join(CONTEXT_ATTACHMENT_STORE_DIR_NAME)
        .join(CONTEXT_ATTACHMENT_HASH_DIR_NAME);
    fs::create_dir_all(&directory)
        .with_context(|| format!("failed to create {}", directory.display()))?;

    let (bytes, extension) = request_view_artifact_bytes(item_type, item);
    let file_name = format!(
        "{}-{}.{}",
        kind_label(item_type),
        stable_hash_hex(item_type, &bytes),
        extension
    );
    let target_path = directory.join(file_name);
    if !target_path.is_file() {
        fs::write(&target_path, &bytes)
            .with_context(|| format!("failed to write {}", target_path.display()))?;
    }
    relative_path(workspace_root, &target_path)
}

fn kind_label(item_type: &str) -> &'static str {
    match item_type {
        "image_url" | "input_image" => "image",
        "file" | "input_file" => "file",
        "input_audio" => "audio",
        _ => "item",
    }
}

fn stable_hash_hex(item_type: &str, bytes: &[u8]) -> String {
    let mut hasher = DefaultHasher::new();
    item_type.hash(&mut hasher);
    bytes.hash(&mut hasher);
    format!("{:016x}", hasher.finish())
}

fn relative_path(workspace_root: &Path, path: &Path) -> Result<String> {
    let relative = path.strip_prefix(workspace_root).with_context(|| {
        format!(
            "path {} is not under workspace root {}",
            path.display(),
            workspace_root.display()
        )
    })?;
    Ok(relative.to_string_lossy().replace('\\', "/"))
}

fn request_view_artifact_bytes(item_type: &str, item: &Value) -> (Vec<u8>, String) {
    match item_type {
        "image_url" | "input_image" => {
            if let Some((bytes, extension)) = extract_image_bytes(item_type, item) {
                return (bytes, extension);
            }
        }
        "file" | "input_file" => {
            if let Some((bytes, extension)) = extract_file_bytes(item_type, item) {
                return (bytes, extension);
            }
        }
        "input_audio" => {
            if let Some((bytes, extension)) = extract_audio_bytes(item) {
                return (bytes, extension);
            }
        }
        _ => {}
    }

    (
        serde_json::to_vec_pretty(item).unwrap_or_else(|_| b"{}".to_vec()),
        "json".to_string(),
    )
}

fn extract_image_bytes(item_type: &str, item: &Value) -> Option<(Vec<u8>, String)> {
    let image_url = if item_type == "image_url" {
        item.get("image_url").and_then(|value| {
            value
                .get("url")
                .and_then(Value::as_str)
                .or_else(|| value.as_str())
        })
    } else {
        item.get("image_url").and_then(Value::as_str)
    }?;
    let (bytes, media_type) = decode_data_url_payload(image_url)?;
    Some((
        bytes,
        preferred_extension(None, media_type.as_deref(), "bin"),
    ))
}

fn extract_file_bytes(item_type: &str, item: &Value) -> Option<(Vec<u8>, String)> {
    let file_value = if item_type == "file" {
        item.get("file")
    } else {
        Some(item)
    }?;
    let filename_hint = file_value.get("filename").and_then(Value::as_str);
    let file_data = file_value.get("file_data").and_then(Value::as_str)?;
    if let Some((bytes, media_type)) = decode_data_url_payload(file_data) {
        return Some((
            bytes,
            preferred_extension(filename_hint, media_type.as_deref(), "bin"),
        ));
    }
    let bytes = decode_base64_payload(file_data)?;
    Some((bytes, preferred_extension(filename_hint, None, "bin")))
}

fn extract_audio_bytes(item: &Value) -> Option<(Vec<u8>, String)> {
    let audio = item.get("input_audio").and_then(Value::as_object)?;
    let data = audio.get("data").and_then(Value::as_str)?;
    let format = audio.get("format").and_then(Value::as_str);
    let bytes = decode_base64_payload(data)?;
    Some((
        bytes,
        sanitize_extension(format.unwrap_or("bin"))
            .unwrap_or("bin")
            .to_string(),
    ))
}

fn decode_base64_payload(value: &str) -> Option<Vec<u8>> {
    base64::engine::general_purpose::STANDARD.decode(value).ok()
}

fn decode_data_url_payload(value: &str) -> Option<(Vec<u8>, Option<String>)> {
    let payload = value.strip_prefix("data:")?;
    let (metadata, encoded) = payload.split_once(',')?;
    let media_type = metadata
        .strip_suffix(";base64")
        .filter(|value| !value.is_empty())
        .map(|value| value.to_string())?;
    let bytes = decode_base64_payload(encoded)?;
    Some((bytes, Some(media_type)))
}

fn preferred_extension(
    filename_hint: Option<&str>,
    media_type_hint: Option<&str>,
    fallback: &str,
) -> String {
    filename_hint
        .and_then(|name| {
            Path::new(name)
                .extension()
                .and_then(|ext| ext.to_str())
                .and_then(sanitize_extension)
        })
        .or_else(|| media_type_hint.and_then(extension_from_media_type))
        .unwrap_or(fallback)
        .to_string()
}

fn sanitize_extension(value: &str) -> Option<&str> {
    let trimmed = value.trim().trim_start_matches('.');
    if trimmed.is_empty() || !trimmed.chars().all(|ch| ch.is_ascii_alphanumeric()) {
        return None;
    }
    Some(trimmed)
}

fn extension_from_media_type(media_type: &str) -> Option<&'static str> {
    match media_type {
        "image/png" => Some("png"),
        "image/jpeg" => Some("jpg"),
        "image/jpg" => Some("jpg"),
        "image/webp" => Some("webp"),
        "image/gif" => Some("gif"),
        "image/bmp" => Some("bmp"),
        "image/svg+xml" => Some("svg"),
        "application/pdf" => Some("pdf"),
        "text/plain" => Some("txt"),
        "application/json" => Some("json"),
        "audio/mpeg" => Some("mp3"),
        "audio/mp3" => Some("mp3"),
        "audio/wav" => Some("wav"),
        "audio/x-wav" => Some("wav"),
        "audio/ogg" => Some("ogg"),
        "audio/flac" => Some("flac"),
        "audio/mp4" => Some("m4a"),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::materialize_messages_for_upstream;
    use crate::config::{
        AgentConfig, ContextCompactionConfig, MemorySystem, TimeoutObservationCompactionConfig,
        UpstreamApiKind, UpstreamAuthKind, UpstreamConfig,
    };
    use crate::message::ChatMessage;
    use base64::Engine;
    use serde_json::json;
    use std::path::Path;
    use tempfile::TempDir;

    fn test_config(workspace_root: &Path, supports_vision_input: bool) -> AgentConfig {
        AgentConfig {
            enabled_tools: Vec::new(),
            upstream: UpstreamConfig {
                base_url: "https://example.com/v1".to_string(),
                model: "demo".to_string(),
                api_kind: UpstreamApiKind::Responses,
                auth_kind: UpstreamAuthKind::ApiKey,
                supports_vision_input,
                supports_pdf_input: false,
                supports_audio_input: false,
                api_key: None,
                api_key_env: "TEST".to_string(),
                chat_completions_path: "/responses".to_string(),
                codex_home: None,
                codex_auth: None,
                auth_credentials_store_mode: Default::default(),
                timeout_seconds: 30.0,
                retry_mode: Default::default(),
                context_window_tokens: 100000,
                cache_control: None,
                prompt_cache_retention: None,
                prompt_cache_key: None,
                reasoning: None,
                headers: Default::default(),
                native_web_search: None,
                external_web_search: None,
                native_image_input: false,
                native_pdf_input: false,
                native_audio_input: false,
                native_image_generation: false,
            },
            image_tool_upstream: None,
            pdf_tool_upstream: None,
            audio_tool_upstream: None,
            image_generation_tool_upstream: None,
            skills_dirs: Vec::new(),
            system_prompt: String::new(),
            max_tool_roundtrips: 4,
            workspace_root: workspace_root.to_path_buf(),
            runtime_state_root: workspace_root.join(".runtime"),
            enable_context_compression: true,
            context_compaction: ContextCompactionConfig::default(),
            timeout_observation_compaction: TimeoutObservationCompactionConfig::default(),
            memory_system: MemorySystem::Layered,
        }
    }

    #[test]
    fn materializes_unsupported_image_into_workspace_reference() {
        let temp_dir = TempDir::new().unwrap();
        let config = test_config(temp_dir.path(), false);
        let payload = base64::engine::general_purpose::STANDARD.encode([0_u8, 1, 2, 3]);
        let messages = vec![ChatMessage {
            role: "user".to_string(),
            content: Some(json!([{
                "type": "input_image",
                "image_url": format!("data:image/png;base64,{payload}")
            }])),
            name: None,
            tool_call_id: None,
            tool_calls: None,
        }];

        let rewritten = materialize_messages_for_upstream(&messages, &config).unwrap();
        let text = rewritten[0].content.as_ref().unwrap()[0]["text"]
            .as_str()
            .unwrap();
        assert!(text.contains(".context_attachments/by-hash/"));
    }

    #[test]
    fn preserves_supported_image_input_in_request_view() {
        let temp_dir = TempDir::new().unwrap();
        let config = test_config(temp_dir.path(), true);
        let payload = base64::engine::general_purpose::STANDARD.encode([0_u8, 1, 2, 3]);
        let messages = vec![ChatMessage {
            role: "user".to_string(),
            content: Some(json!([{
                "type": "input_image",
                "image_url": format!("data:image/png;base64,{payload}")
            }])),
            name: None,
            tool_call_id: None,
            tool_calls: None,
        }];

        let rewritten = materialize_messages_for_upstream(&messages, &config).unwrap();
        assert_eq!(rewritten, messages);
    }
}
