use crate::config::AgentConfig;
use crate::message::ChatMessage;
use anyhow::{Context, Result, anyhow};
use base64::Engine;
use image::{ImageFormat, ImageReader};
use serde_json::{Map, Value, json};
use std::collections::hash_map::DefaultHasher;
use std::fs;
use std::hash::{Hash, Hasher};
use std::io::Cursor;
use std::path::{Path, PathBuf};

const CONTEXT_ATTACHMENT_STORE_DIR_NAME: &str = ".context_attachments";
const CONTEXT_ATTACHMENT_HASH_DIR_NAME: &str = "by-hash";
const CANONICAL_MESSAGE_MEDIA_DIR_NAME: &str = "media";
const CANONICAL_MESSAGE_MEDIA_HASH_DIR_NAME: &str = "by-hash";

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UpstreamModalityPolicy {
    pub allow_images: bool,
    pub allow_files: bool,
    pub allow_audio: bool,
    pub capability_text: String,
}

impl UpstreamModalityPolicy {
    pub fn for_agent_config(config: &AgentConfig) -> Self {
        Self {
            allow_images: config.upstream.supports_vision_input,
            allow_files: config.upstream.supports_pdf_input,
            allow_audio: config.upstream.supports_audio_input,
            capability_text: "current model/backend combination".to_string(),
        }
    }

    fn should_downgrade(&self, item_type: &str) -> bool {
        match item_type {
            "image_url" | "input_image" | "output_image" => !self.allow_images,
            "file" | "input_file" | "output_file" => !self.allow_files,
            "input_audio" | "output_audio" => !self.allow_audio,
            _ => false,
        }
    }
}

#[derive(Clone, Debug)]
pub struct ModalityItemContext<'a> {
    pub item_type: &'a str,
    pub item: &'a Value,
    pub policy: UpstreamModalityPolicy,
    pub is_downgraded: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub enum ModalityItemRewrite {
    KeepOriginal,
    Replace(Value),
    Drop,
}

#[derive(Clone, Copy, Debug)]
pub enum CanonicalMessageScope {
    Assistant,
    Tool,
    Legacy,
}

impl CanonicalMessageScope {
    fn dir_name(self) -> &'static str {
        match self {
            Self::Assistant => "assistant",
            Self::Tool => "tool",
            Self::Legacy => "legacy",
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct RequestViewArtifactStore<'a> {
    workspace_root: &'a Path,
}

impl<'a> RequestViewArtifactStore<'a> {
    pub fn new(workspace_root: &'a Path) -> Self {
        Self { workspace_root }
    }

    fn persist_item(self, item_type: &str, item: &Value) -> Result<String> {
        persist_request_view_artifact(self.workspace_root, item_type, item)
    }
}

#[derive(Clone, Debug)]
pub(crate) struct UpstreamMaterializationRequest<'a> {
    pub messages: &'a [ChatMessage],
    pub policy: UpstreamModalityPolicy,
    pub artifact_store: RequestViewArtifactStore<'a>,
}

pub(crate) fn materialize_messages_for_upstream(
    messages: &[ChatMessage],
    config: &AgentConfig,
) -> Result<Vec<ChatMessage>> {
    materialize_messages_for_request(UpstreamMaterializationRequest {
        messages,
        policy: UpstreamModalityPolicy::for_agent_config(config),
        artifact_store: RequestViewArtifactStore::new(&config.workspace_root),
    })
}

pub(crate) fn materialize_messages_for_request(
    request: UpstreamMaterializationRequest<'_>,
) -> Result<Vec<ChatMessage>> {
    let mut rewritten_messages = Vec::with_capacity(request.messages.len());
    for message in request.messages {
        let policy = request.policy.clone();
        let rewritten_content = rewrite_message_content_with_modality_policy(
            &message.content,
            policy.clone(),
            |ctx| {
                if !is_multimodal_item_type(ctx.item_type) {
                    return Ok(ModalityItemRewrite::KeepOriginal);
                }
                if message.role != "user" {
                    let relative_path = request_view_reference_path(
                        ctx.item_type,
                        ctx.item,
                        request.artifact_store,
                    )?;
                    return Ok(ModalityItemRewrite::Replace(placeholder_text_item(
                        request_view_non_user_placeholder_text(ctx.item_type, &relative_path),
                    )));
                }
                if !ctx.is_downgraded {
                    if let Some(inline_item) = materialize_user_item_from_path(
                        request.artifact_store.workspace_root,
                        ctx.item_type,
                        ctx.item,
                    )? {
                        return Ok(ModalityItemRewrite::Replace(inline_item));
                    }
                    return Ok(ModalityItemRewrite::KeepOriginal);
                }
                let relative_path =
                    request_view_reference_path(ctx.item_type, ctx.item, request.artifact_store)?;
                Ok(ModalityItemRewrite::Replace(placeholder_text_item(
                    request_view_placeholder_text(
                        ctx.item_type,
                        policy.capability_text.as_str(),
                        &relative_path,
                    ),
                )))
            },
        )?;
        if rewritten_content != message.content {
            let mut rewritten = message.clone();
            rewritten.content = rewritten_content;
            rewritten_messages.push(rewritten);
        } else {
            rewritten_messages.push(message.clone());
        }
    }

    Ok(rewritten_messages)
}

pub fn canonicalize_message_multimodal_for_storage(
    workspace_root: &Path,
    message: &ChatMessage,
    scope: CanonicalMessageScope,
) -> Result<ChatMessage> {
    let rewritten_content = canonicalize_message_content_for_storage(
        workspace_root,
        &message.role,
        &message.content,
        scope,
    )?;
    if rewritten_content == message.content {
        return Ok(message.clone());
    }
    let mut rewritten = message.clone();
    rewritten.content = rewritten_content;
    Ok(rewritten)
}

fn canonicalize_message_content_for_storage(
    workspace_root: &Path,
    role: &str,
    content: &Option<Value>,
    scope: CanonicalMessageScope,
) -> Result<Option<Value>> {
    let Some(Value::Array(items)) = content else {
        return Ok(content.clone());
    };

    let mut rewritten = Vec::with_capacity(items.len());
    let mut changed = false;
    for item in items {
        let next =
            canonicalize_message_content_item_for_storage(workspace_root, role, item, scope)?;
        changed |= next != *item;
        rewritten.push(next);
    }
    if changed {
        Ok(Some(Value::Array(rewritten)))
    } else {
        Ok(content.clone())
    }
}

fn canonicalize_message_content_item_for_storage(
    workspace_root: &Path,
    role: &str,
    item: &Value,
    scope: CanonicalMessageScope,
) -> Result<Value> {
    let Some(item_type) = item.get("type").and_then(Value::as_str) else {
        return Ok(item.clone());
    };
    let Some(canonical_type) = canonical_item_type_for_storage(role, item_type) else {
        return Ok(item.clone());
    };
    if let Some(path) = path_from_item(item) {
        return canonicalize_path_item_for_storage(
            workspace_root,
            canonical_type,
            item,
            Path::new(path),
            scope,
        );
    }
    canonicalize_inline_item_for_storage(workspace_root, canonical_type, item_type, item, scope)
}

fn canonical_item_type_for_storage(role: &str, item_type: &str) -> Option<&'static str> {
    match item_type {
        "image_url" | "input_image" | "output_image" => Some(if role == "assistant" {
            "output_image"
        } else {
            "input_image"
        }),
        "file" | "input_file" | "output_file" => Some(if role == "assistant" {
            "output_file"
        } else {
            "input_file"
        }),
        "input_audio" | "output_audio" => Some(if role == "assistant" {
            "output_audio"
        } else {
            "input_audio"
        }),
        _ => None,
    }
}

fn canonicalize_path_item_for_storage(
    workspace_root: &Path,
    canonical_type: &str,
    item: &Value,
    source_path: &Path,
    scope: CanonicalMessageScope,
) -> Result<Value> {
    let resolved = resolve_message_path(workspace_root, source_path);
    match canonical_type {
        "input_image" | "output_image" => canonicalize_image_path_item_for_storage(
            workspace_root,
            canonical_type,
            item,
            &resolved,
            scope,
        ),
        "input_file" | "output_file" => canonicalize_file_path_item_for_storage(
            workspace_root,
            canonical_type,
            item,
            &resolved,
            scope,
        ),
        "input_audio" | "output_audio" => canonicalize_audio_path_item_for_storage(
            workspace_root,
            canonical_type,
            item,
            &resolved,
            scope,
        ),
        _ => Ok(item.clone()),
    }
}

fn canonicalize_inline_item_for_storage(
    workspace_root: &Path,
    canonical_type: &str,
    item_type: &str,
    item: &Value,
    scope: CanonicalMessageScope,
) -> Result<Value> {
    match canonical_type {
        "input_image" | "output_image" => {
            let Some((bytes, extension)) = load_image_item_bytes(item_type, item)? else {
                return Ok(item.clone());
            };
            let relative_path =
                persist_canonical_image_bytes(workspace_root, scope, &bytes, &extension)?;
            Ok(build_path_media_item(
                canonical_type,
                &relative_path,
                None,
                None,
                None,
            ))
        }
        "input_file" | "output_file" => {
            let Some((bytes, extension, filename, media_type)) =
                load_file_item_bytes(item_type, item)?
            else {
                return Ok(item.clone());
            };
            let relative_path = persist_canonical_media_bytes(
                workspace_root,
                scope,
                canonical_type,
                &bytes,
                &extension,
            )?;
            Ok(build_path_media_item(
                canonical_type,
                &relative_path,
                filename.as_deref(),
                media_type.as_deref(),
                None,
            ))
        }
        "input_audio" | "output_audio" => {
            let Some((bytes, extension, format, media_type)) = load_audio_item_bytes(item)? else {
                return Ok(item.clone());
            };
            let relative_path = persist_canonical_media_bytes(
                workspace_root,
                scope,
                canonical_type,
                &bytes,
                &extension,
            )?;
            Ok(build_path_media_item(
                canonical_type,
                &relative_path,
                None,
                media_type.as_deref(),
                format.as_deref(),
            ))
        }
        _ => Ok(item.clone()),
    }
}

fn canonicalize_image_path_item_for_storage(
    workspace_root: &Path,
    canonical_type: &str,
    _item: &Value,
    resolved_path: &Path,
    scope: CanonicalMessageScope,
) -> Result<Value> {
    let bytes = fs::read(resolved_path)
        .with_context(|| format!("failed to read {}", resolved_path.display()))?;
    let reader = ImageReader::new(Cursor::new(&bytes))
        .with_guessed_format()
        .with_context(|| {
            format!(
                "failed to guess image format for {}",
                resolved_path.display()
            )
        })?;
    let format = reader
        .format()
        .ok_or_else(|| anyhow!("unsupported image format: {}", resolved_path.display()))?;
    let canonical_extension = canonical_image_extension(format);
    let relative_path =
        if is_path_under_root(workspace_root, resolved_path) && canonical_extension.is_some() {
            relative_path(workspace_root, resolved_path)?
        } else {
            let extension = canonical_extension.unwrap_or("png");
            let output_bytes = canonicalize_image_bytes(&bytes, format)?;
            persist_canonical_media_bytes(
                workspace_root,
                scope,
                canonical_type,
                &output_bytes,
                extension,
            )?
        };
    Ok(build_path_media_item(
        canonical_type,
        &relative_path,
        None,
        None,
        None,
    ))
}

fn canonicalize_file_path_item_for_storage(
    workspace_root: &Path,
    canonical_type: &str,
    item: &Value,
    resolved_path: &Path,
    scope: CanonicalMessageScope,
) -> Result<Value> {
    let filename = item_filename(item).or_else(|| {
        resolved_path
            .file_name()
            .and_then(|value| value.to_str())
            .map(ToOwned::to_owned)
    });
    let media_type = item
        .get("media_type")
        .and_then(Value::as_str)
        .map(ToOwned::to_owned);
    let relative_path = if is_path_under_root(workspace_root, resolved_path) {
        relative_path(workspace_root, resolved_path)?
    } else {
        let bytes = fs::read(resolved_path)
            .with_context(|| format!("failed to read {}", resolved_path.display()))?;
        let extension = preferred_extension(
            filename.as_deref(),
            media_type.as_deref(),
            resolved_path
                .extension()
                .and_then(|value| value.to_str())
                .unwrap_or("bin"),
        );
        persist_canonical_media_bytes(workspace_root, scope, canonical_type, &bytes, &extension)?
    };
    Ok(build_path_media_item(
        canonical_type,
        &relative_path,
        filename.as_deref(),
        media_type.as_deref(),
        None,
    ))
}

fn canonicalize_audio_path_item_for_storage(
    workspace_root: &Path,
    canonical_type: &str,
    item: &Value,
    resolved_path: &Path,
    scope: CanonicalMessageScope,
) -> Result<Value> {
    let format = item
        .get("format")
        .and_then(Value::as_str)
        .map(ToOwned::to_owned)
        .or_else(|| infer_audio_format_from_path(resolved_path).map(ToOwned::to_owned));
    let media_type = item
        .get("media_type")
        .and_then(Value::as_str)
        .map(ToOwned::to_owned);
    let relative_path = if is_path_under_root(workspace_root, resolved_path) {
        relative_path(workspace_root, resolved_path)?
    } else {
        let bytes = fs::read(resolved_path)
            .with_context(|| format!("failed to read {}", resolved_path.display()))?;
        let extension = sanitize_extension(
            format
                .as_deref()
                .or_else(|| resolved_path.extension().and_then(|value| value.to_str()))
                .unwrap_or("bin"),
        )
        .unwrap_or("bin");
        persist_canonical_media_bytes(workspace_root, scope, canonical_type, &bytes, extension)?
    };
    Ok(build_path_media_item(
        canonical_type,
        &relative_path,
        None,
        media_type.as_deref(),
        format.as_deref(),
    ))
}

fn build_path_media_item(
    item_type: &str,
    path: &str,
    filename: Option<&str>,
    media_type: Option<&str>,
    format: Option<&str>,
) -> Value {
    let mut object = Map::from_iter([
        ("type".to_string(), Value::String(item_type.to_string())),
        ("path".to_string(), Value::String(path.to_string())),
    ]);
    if let Some(filename) = filename.filter(|value| !value.trim().is_empty()) {
        object.insert("filename".to_string(), Value::String(filename.to_string()));
    }
    if let Some(media_type) = media_type.filter(|value| !value.trim().is_empty()) {
        object.insert(
            "media_type".to_string(),
            Value::String(media_type.to_string()),
        );
    }
    if let Some(format) = format.filter(|value| !value.trim().is_empty()) {
        object.insert("format".to_string(), Value::String(format.to_string()));
    }
    Value::Object(object)
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

fn request_view_non_user_placeholder_text(item_type: &str, relative_path: &str) -> String {
    match item_type {
        "image_url" | "input_image" | "output_image" => {
            format!("[Earlier assistant image kept at {relative_path}.]")
        }
        "file" | "input_file" | "output_file" => {
            format!("[Earlier assistant file kept at {relative_path}.]")
        }
        "input_audio" | "output_audio" => {
            format!("[Earlier assistant audio kept at {relative_path}.]")
        }
        _ => format!("[Earlier multimodal content kept at {relative_path}.]"),
    }
}

fn request_view_reference_path(
    item_type: &str,
    item: &Value,
    artifact_store: RequestViewArtifactStore<'_>,
) -> Result<String> {
    if let Some(path) = path_from_item(item) {
        let resolved = resolve_message_path(artifact_store.workspace_root, Path::new(path));
        if is_path_under_root(artifact_store.workspace_root, &resolved) {
            return relative_path(artifact_store.workspace_root, &resolved);
        }
    }
    artifact_store.persist_item(item_type, item)
}

pub fn placeholder_text_item(text: String) -> Value {
    json!({
        "type": "text",
        "text": text,
    })
}

pub fn downgraded_multimodal_placeholder_text(
    item_type: &str,
    item: &Value,
    capability_text: &str,
) -> Option<String> {
    match item_type {
        "image_url" | "input_image" | "output_image" => Some(format!(
            "[Earlier image omitted because the current {capability_text} does not accept image input.]"
        )),
        "file" | "input_file" | "output_file" => {
            let file_value = if item_type == "file" {
                item.get("file")
            } else {
                Some(item)
            }?;
            let filename = file_value
                .get("filename")
                .and_then(Value::as_str)
                .unwrap_or("document");
            Some(format!(
                "[Earlier file omitted because the current {capability_text} does not accept file input: {filename}]"
            ))
        }
        "input_audio" | "output_audio" => Some(format!(
            "[Earlier audio omitted because the current {capability_text} does not accept audio input.]"
        )),
        _ => None,
    }
}

pub fn rewrite_message_content_with_modality_policy<F>(
    content: &Option<Value>,
    policy: UpstreamModalityPolicy,
    mut rewrite_item: F,
) -> Result<Option<Value>>
where
    F: FnMut(ModalityItemContext<'_>) -> Result<ModalityItemRewrite>,
{
    let Some(Value::Array(items)) = content else {
        return Ok(content.clone());
    };

    let mut rewritten_items = Vec::with_capacity(items.len());
    let mut changed = false;
    for item in items {
        let Some(item_type) = item.get("type").and_then(Value::as_str) else {
            rewritten_items.push(item.clone());
            continue;
        };
        let rewrite = rewrite_item(ModalityItemContext {
            item_type,
            item,
            policy: policy.clone(),
            is_downgraded: policy.should_downgrade(item_type),
        })?;
        match rewrite {
            ModalityItemRewrite::KeepOriginal => rewritten_items.push(item.clone()),
            ModalityItemRewrite::Replace(value) => {
                changed = true;
                rewritten_items.push(value);
            }
            ModalityItemRewrite::Drop => changed = true,
        }
    }

    if changed {
        Ok(Some(Value::Array(rewritten_items)))
    } else {
        Ok(content.clone())
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
        "image" | "image_url" | "input_image" | "output_image" => "image",
        "file" | "input_file" | "output_file" => "file",
        "audio" | "input_audio" | "output_audio" => "audio",
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
        "image_url" | "input_image" | "output_image" => {
            if let Some((bytes, extension)) = extract_image_bytes(item_type, item) {
                return (bytes, extension);
            }
        }
        "file" | "input_file" | "output_file" => {
            if let Some((bytes, extension)) = extract_file_bytes(item_type, item) {
                return (bytes, extension);
            }
        }
        "input_audio" | "output_audio" => {
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
    if let Some(path) = path_from_item(item) {
        let bytes = fs::read(path).ok()?;
        let extension = Path::new(path)
            .extension()
            .and_then(|value| value.to_str())
            .and_then(sanitize_extension)
            .unwrap_or("bin")
            .to_string();
        return Some((bytes, extension));
    }
    let image_url = if item_type == "image_url" {
        item.get("image_url").and_then(|value| {
            value
                .get("url")
                .and_then(Value::as_str)
                .or_else(|| value.as_str())
        })
    } else if item_type == "input_image" || item_type == "output_image" {
        item.get("image_url").and_then(Value::as_str)
    } else {
        None
    }?;
    let (bytes, media_type) = decode_data_url_payload(image_url)?;
    Some((
        bytes,
        preferred_extension(None, media_type.as_deref(), "bin"),
    ))
}

fn extract_file_bytes(item_type: &str, item: &Value) -> Option<(Vec<u8>, String)> {
    if let Some(path) = path_from_item(item) {
        let bytes = fs::read(path).ok()?;
        let filename_hint = item_filename(item);
        let extension = preferred_extension(
            filename_hint.as_deref(),
            item.get("media_type").and_then(Value::as_str),
            Path::new(path)
                .extension()
                .and_then(|value| value.to_str())
                .unwrap_or("bin"),
        );
        return Some((bytes, extension));
    }
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
    if let Some(path) = path_from_item(item) {
        let bytes = fs::read(path).ok()?;
        let extension = sanitize_extension(
            item.get("format")
                .and_then(Value::as_str)
                .or_else(|| Path::new(path).extension().and_then(|value| value.to_str()))
                .unwrap_or("bin"),
        )
        .unwrap_or("bin")
        .to_string();
        return Some((bytes, extension));
    }
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

fn is_multimodal_item_type(item_type: &str) -> bool {
    matches!(
        item_type,
        "image_url"
            | "input_image"
            | "output_image"
            | "file"
            | "input_file"
            | "output_file"
            | "input_audio"
            | "output_audio"
    )
}

fn path_from_item(item: &Value) -> Option<&str> {
    item.get("path")
        .and_then(Value::as_str)
        .filter(|value| !value.trim().is_empty())
}

fn item_filename(item: &Value) -> Option<String> {
    item.get("filename")
        .and_then(Value::as_str)
        .filter(|value| !value.trim().is_empty())
        .map(ToOwned::to_owned)
        .or_else(|| {
            item.get("file")
                .and_then(Value::as_object)
                .and_then(|file| file.get("filename"))
                .and_then(Value::as_str)
                .filter(|value| !value.trim().is_empty())
                .map(ToOwned::to_owned)
        })
}

fn resolve_message_path(workspace_root: &Path, path: &Path) -> PathBuf {
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        workspace_root.join(path)
    }
}

fn is_path_under_root(workspace_root: &Path, path: &Path) -> bool {
    path.strip_prefix(workspace_root).is_ok()
}

fn materialize_user_item_from_path(
    workspace_root: &Path,
    item_type: &str,
    item: &Value,
) -> Result<Option<Value>> {
    let Some(path) = path_from_item(item) else {
        return Ok(None);
    };
    let resolved = resolve_message_path(workspace_root, Path::new(path));
    match item_type {
        "input_image" => Ok(Some(json!({
            "type": "input_image",
            "image_url": image_path_to_data_url(&resolved)?,
        }))),
        "input_file" => {
            if !path_looks_like_pdf(&resolved, item.get("media_type").and_then(Value::as_str)) {
                return Ok(None);
            }
            let filename = item_filename(item)
                .or_else(|| {
                    resolved
                        .file_name()
                        .and_then(|value| value.to_str())
                        .map(ToOwned::to_owned)
                })
                .unwrap_or_else(|| "document.pdf".to_string());
            Ok(Some(json!({
                "type": "file",
                "file": {
                    "file_data": file_path_to_base64(&resolved)?,
                    "filename": filename,
                }
            })))
        }
        "input_audio" => {
            let format = item
                .get("format")
                .and_then(Value::as_str)
                .or_else(|| infer_audio_format_from_path(&resolved))
                .ok_or_else(|| anyhow!("unsupported audio format for {}", resolved.display()))?;
            Ok(Some(json!({
                "type": "input_audio",
                "input_audio": {
                    "data": file_path_to_base64(&resolved)?,
                    "format": format,
                }
            })))
        }
        _ => Ok(None),
    }
}

fn image_path_to_data_url(path: &Path) -> Result<String> {
    let bytes = fs::read(path).with_context(|| format!("failed to read {}", path.display()))?;
    let reader = ImageReader::new(Cursor::new(&bytes))
        .with_guessed_format()
        .with_context(|| format!("failed to guess image format for {}", path.display()))?;
    let format = reader
        .format()
        .ok_or_else(|| anyhow!("unsupported image format: {}", path.display()))?;
    let output_bytes = canonicalize_image_bytes(&bytes, format)?;
    let media_type = canonical_image_extension(format)
        .and_then(media_type_from_extension)
        .unwrap_or("image/png");
    let encoded = base64::engine::general_purpose::STANDARD.encode(output_bytes);
    Ok(format!("data:{media_type};base64,{encoded}"))
}

fn file_path_to_base64(path: &Path) -> Result<String> {
    let bytes = fs::read(path).with_context(|| format!("failed to read {}", path.display()))?;
    Ok(base64::engine::general_purpose::STANDARD.encode(bytes))
}

fn canonicalize_image_bytes(bytes: &[u8], format: ImageFormat) -> Result<Vec<u8>> {
    match canonical_image_extension(format) {
        Some(_) => Ok(bytes.to_vec()),
        None => {
            let image = ImageReader::new(Cursor::new(bytes))
                .with_guessed_format()
                .context("failed to guess inline image format")?
                .decode()
                .context("failed to decode image bytes")?;
            let mut output = Vec::new();
            image
                .write_to(&mut Cursor::new(&mut output), ImageFormat::Png)
                .context("failed to transcode image bytes to PNG")?;
            Ok(output)
        }
    }
}

fn canonical_image_extension(format: ImageFormat) -> Option<&'static str> {
    match format {
        ImageFormat::Png => Some("png"),
        ImageFormat::Jpeg => Some("jpg"),
        ImageFormat::Gif => Some("gif"),
        ImageFormat::WebP => Some("webp"),
        _ => None,
    }
}

fn media_type_from_extension(extension: &str) -> Option<&'static str> {
    match extension {
        "png" => Some("image/png"),
        "jpg" | "jpeg" => Some("image/jpeg"),
        "gif" => Some("image/gif"),
        "webp" => Some("image/webp"),
        _ => None,
    }
}

fn infer_audio_format_from_path(path: &Path) -> Option<&'static str> {
    match path
        .extension()
        .and_then(|value| value.to_str())
        .map(|value| value.to_ascii_lowercase())
        .as_deref()
    {
        Some("wav") => Some("wav"),
        Some("mp3") | Some("mpeg") | Some("mpga") => Some("mp3"),
        Some("ogg") | Some("opus") => Some("ogg"),
        Some("webm") => Some("webm"),
        Some("m4a") | Some("mp4") | Some("aac") => Some("m4a"),
        Some("flac") => Some("flac"),
        _ => None,
    }
}

fn path_looks_like_pdf(path: &Path, media_type: Option<&str>) -> bool {
    media_type.is_some_and(|value| value.eq_ignore_ascii_case("application/pdf"))
        || path
            .extension()
            .and_then(|value| value.to_str())
            .is_some_and(|value| value.eq_ignore_ascii_case("pdf"))
}

fn persist_canonical_image_bytes(
    workspace_root: &Path,
    scope: CanonicalMessageScope,
    bytes: &[u8],
    extension_hint: &str,
) -> Result<String> {
    let reader = ImageReader::new(Cursor::new(bytes))
        .with_guessed_format()
        .context("failed to guess image format for canonical persistence")?;
    let format = reader
        .format()
        .ok_or_else(|| anyhow!("unsupported image format for canonical persistence"))?;
    let extension = canonical_image_extension(format).unwrap_or("png");
    let output_bytes = canonicalize_image_bytes(bytes, format)?;
    let extension = sanitize_extension(extension_hint)
        .filter(|_| canonical_image_extension(format).is_some())
        .unwrap_or(extension);
    persist_canonical_media_bytes(workspace_root, scope, "image", &output_bytes, extension)
}

fn persist_canonical_media_bytes(
    workspace_root: &Path,
    scope: CanonicalMessageScope,
    item_type: &str,
    bytes: &[u8],
    extension: &str,
) -> Result<String> {
    let directory = workspace_root
        .join(CANONICAL_MESSAGE_MEDIA_DIR_NAME)
        .join(scope.dir_name())
        .join(CANONICAL_MESSAGE_MEDIA_HASH_DIR_NAME);
    fs::create_dir_all(&directory)
        .with_context(|| format!("failed to create {}", directory.display()))?;
    let file_name = format!(
        "{}-{}.{}",
        kind_label(item_type),
        stable_hash_hex(item_type, bytes),
        sanitize_extension(extension).unwrap_or("bin")
    );
    let target_path = directory.join(file_name);
    if !target_path.is_file() {
        fs::write(&target_path, bytes)
            .with_context(|| format!("failed to write {}", target_path.display()))?;
    }
    relative_path(workspace_root, &target_path)
}

fn load_image_item_bytes(item_type: &str, item: &Value) -> Result<Option<(Vec<u8>, String)>> {
    if let Some((bytes, extension)) = extract_image_bytes(item_type, item) {
        return Ok(Some((bytes, extension)));
    }
    let Some(url) = remote_url_from_image_item(item_type, item) else {
        return Ok(None);
    };
    let (bytes, media_type, file_name) = download_url_bytes(url)?;
    Ok(Some((
        bytes,
        preferred_extension(file_name.as_deref(), media_type.as_deref(), "bin"),
    )))
}

fn load_file_item_bytes(
    item_type: &str,
    item: &Value,
) -> Result<Option<(Vec<u8>, String, Option<String>, Option<String>)>> {
    if let Some(path) = path_from_item(item) {
        let resolved = Path::new(path);
        let bytes =
            fs::read(resolved).with_context(|| format!("failed to read {}", resolved.display()))?;
        return Ok(Some((
            bytes,
            preferred_extension(
                item_filename(item).as_deref(),
                item.get("media_type").and_then(Value::as_str),
                resolved
                    .extension()
                    .and_then(|value| value.to_str())
                    .unwrap_or("bin"),
            ),
            item_filename(item),
            item.get("media_type")
                .and_then(Value::as_str)
                .map(ToOwned::to_owned),
        )));
    }
    let file_value = if item_type == "file" {
        item.get("file")
    } else {
        Some(item)
    };
    let Some(file_value) = file_value else {
        return Ok(None);
    };
    let filename = file_value
        .get("filename")
        .and_then(Value::as_str)
        .map(ToOwned::to_owned);
    if let Some(file_data) = file_value.get("file_data").and_then(Value::as_str) {
        if let Some((bytes, media_type)) = decode_data_url_payload(file_data) {
            let extension = preferred_extension(filename.as_deref(), media_type.as_deref(), "bin");
            return Ok(Some((bytes, extension, filename, media_type)));
        }
        let bytes = decode_base64_payload(file_data)
            .ok_or_else(|| anyhow!("failed to decode inline file payload"))?;
        let extension = preferred_extension(filename.as_deref(), None, "bin");
        return Ok(Some((bytes, extension, filename, None)));
    }
    let Some(file_url) = file_value.get("file_url").and_then(Value::as_str) else {
        return Ok(None);
    };
    let (bytes, media_type, downloaded_name) = download_url_bytes(file_url)?;
    let filename = filename.or(downloaded_name);
    let extension = preferred_extension(filename.as_deref(), media_type.as_deref(), "bin");
    Ok(Some((bytes, extension, filename, media_type)))
}

fn load_audio_item_bytes(
    item: &Value,
) -> Result<Option<(Vec<u8>, String, Option<String>, Option<String>)>> {
    if let Some(path) = path_from_item(item) {
        let resolved = Path::new(path);
        let bytes =
            fs::read(resolved).with_context(|| format!("failed to read {}", resolved.display()))?;
        let format = item
            .get("format")
            .and_then(Value::as_str)
            .map(ToOwned::to_owned)
            .or_else(|| infer_audio_format_from_path(resolved).map(ToOwned::to_owned));
        let extension = sanitize_extension(
            format
                .as_deref()
                .or_else(|| resolved.extension().and_then(|value| value.to_str()))
                .unwrap_or("bin"),
        )
        .unwrap_or("bin")
        .to_string();
        return Ok(Some((
            bytes,
            extension,
            format,
            item.get("media_type")
                .and_then(Value::as_str)
                .map(ToOwned::to_owned),
        )));
    }
    let audio = item.get("input_audio").and_then(Value::as_object);
    let Some(audio) = audio else {
        return Ok(None);
    };
    let data = audio.get("data").and_then(Value::as_str);
    let format = audio
        .get("format")
        .and_then(Value::as_str)
        .map(ToOwned::to_owned);
    let Some(data) = data else {
        return Ok(None);
    };
    let bytes = decode_base64_payload(data)
        .ok_or_else(|| anyhow!("failed to decode inline audio payload"))?;
    let extension = sanitize_extension(format.as_deref().unwrap_or("bin"))
        .unwrap_or("bin")
        .to_string();
    Ok(Some((bytes, extension, format, None)))
}

fn remote_url_from_image_item<'a>(item_type: &str, item: &'a Value) -> Option<&'a str> {
    let url = if item_type == "image_url" {
        item.get("image_url").and_then(|value| {
            value
                .get("url")
                .and_then(Value::as_str)
                .or_else(|| value.as_str())
        })
    } else {
        item.get("image_url").and_then(Value::as_str)
    }?;
    url.starts_with("http://")
        .then_some(url)
        .or_else(|| url.starts_with("https://").then_some(url))
}

fn download_url_bytes(url: &str) -> Result<(Vec<u8>, Option<String>, Option<String>)> {
    let client = reqwest::blocking::Client::builder()
        .timeout(std::time::Duration::from_secs(30))
        .build()
        .context("failed to build download client")?;
    let response = client
        .get(url)
        .send()
        .with_context(|| format!("failed to download {}", url))?;
    let status = response.status();
    if !status.is_success() {
        return Err(anyhow!("download failed with {} for {}", status, url));
    }
    let media_type = response
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .map(|value| value.split(';').next().unwrap_or(value).trim().to_string())
        .filter(|value| !value.is_empty());
    let final_url = response.url().clone();
    let bytes = response
        .bytes()
        .context("failed to read downloaded body")?
        .to_vec();
    let file_name = final_url
        .path_segments()
        .and_then(|mut segments| segments.next_back())
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned);
    Ok((bytes, media_type, file_name))
}

#[cfg(test)]
mod tests {
    use super::{
        CanonicalMessageScope, ModalityItemRewrite, RequestViewArtifactStore,
        UpstreamMaterializationRequest, UpstreamModalityPolicy,
        canonicalize_message_multimodal_for_storage, materialize_messages_for_request,
        materialize_messages_for_upstream, placeholder_text_item,
        rewrite_message_content_with_modality_policy,
    };
    use crate::config::{
        AgentConfig, ContextCompactionConfig, MemorySystem, TimeoutObservationCompactionConfig,
        UpstreamApiKind, UpstreamAuthKind, UpstreamConfig,
    };
    use crate::message::ChatMessage;
    use base64::Engine;
    use serde_json::json;
    use std::path::Path;
    use tempfile::TempDir;

    fn tiny_png_bytes() -> Vec<u8> {
        base64::engine::general_purpose::STANDARD
            .decode("iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVQIHWP4//8/AwAI/AL+XxYl3wAAAABJRU5ErkJggg==")
            .unwrap()
    }

    fn test_config(workspace_root: &Path, supports_vision_input: bool) -> AgentConfig {
        AgentConfig {
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
                token_estimation: None,
            },
            available_upstreams: Default::default(),
            image_tool_upstream: None,
            pdf_tool_upstream: None,
            audio_tool_upstream: None,
            image_generation_tool_upstream: None,
            skills_dirs: Vec::new(),
            skills_metadata_prompt: None,
            system_prompt: String::new(),
            remote_workpaths: Vec::new(),
            enable_remote_tools: true,
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
            reasoning: None,
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
            reasoning: None,
            name: None,
            tool_call_id: None,
            tool_calls: None,
        }];

        let rewritten = materialize_messages_for_upstream(&messages, &config).unwrap();
        assert_eq!(rewritten, messages);
    }

    #[test]
    fn materializes_path_based_user_image_into_inline_request_view() {
        let temp_dir = TempDir::new().unwrap();
        let config = test_config(temp_dir.path(), true);
        let image_path = temp_dir.path().join("photo.png");
        std::fs::write(&image_path, tiny_png_bytes()).unwrap();
        let messages = vec![ChatMessage {
            role: "user".to_string(),
            content: Some(json!([{
                "type": "input_image",
                "path": "photo.png"
            }])),
            reasoning: None,
            name: None,
            tool_call_id: None,
            tool_calls: None,
        }];

        let rewritten = materialize_messages_for_upstream(&messages, &config).unwrap();
        let image_url = rewritten[0].content.as_ref().unwrap()[0]["image_url"]
            .as_str()
            .unwrap();
        assert!(image_url.starts_with("data:image/png;base64,"));
    }

    #[test]
    fn rewrites_assistant_path_media_to_placeholder_text_for_request_view() {
        let temp_dir = TempDir::new().unwrap();
        let config = test_config(temp_dir.path(), true);
        let image_path = temp_dir.path().join("generated.png");
        std::fs::write(&image_path, tiny_png_bytes()).unwrap();
        let messages = vec![ChatMessage {
            role: "assistant".to_string(),
            content: Some(json!([{
                "type": "output_image",
                "path": "generated.png"
            }])),
            reasoning: None,
            name: None,
            tool_call_id: None,
            tool_calls: None,
        }];

        let rewritten = materialize_messages_for_upstream(&messages, &config).unwrap();
        let text = rewritten[0].content.as_ref().unwrap()[0]["text"]
            .as_str()
            .unwrap();
        assert!(text.contains("assistant image"));
        assert!(text.contains("generated.png"));
    }

    #[test]
    fn canonicalizes_inline_assistant_image_output_to_workspace_path() {
        let temp_dir = TempDir::new().unwrap();
        let payload = base64::engine::general_purpose::STANDARD.encode(tiny_png_bytes());
        let message = ChatMessage {
            role: "assistant".to_string(),
            content: Some(json!([{
                "type": "image_url",
                "image_url": { "url": format!("data:image/png;base64,{payload}") }
            }])),
            reasoning: None,
            name: None,
            tool_call_id: None,
            tool_calls: None,
        };

        let rewritten = canonicalize_message_multimodal_for_storage(
            temp_dir.path(),
            &message,
            CanonicalMessageScope::Assistant,
        )
        .unwrap();
        let item = &rewritten.content.as_ref().unwrap()[0];
        assert_eq!(item["type"], "output_image");
        let path = item["path"].as_str().unwrap();
        assert!(path.starts_with("media/assistant/by-hash/image-"));
        assert!(temp_dir.path().join(path).is_file());
    }

    #[test]
    fn explicit_policy_materialization_matches_legacy_wrapper() {
        let temp_dir = TempDir::new().unwrap();
        let config = test_config(temp_dir.path(), false);
        let payload = base64::engine::general_purpose::STANDARD.encode([0_u8, 1, 2, 3]);
        let messages = vec![ChatMessage {
            role: "user".to_string(),
            content: Some(json!([{
                "type": "input_image",
                "image_url": format!("data:image/png;base64,{payload}")
            }])),
            reasoning: None,
            name: None,
            tool_call_id: None,
            tool_calls: None,
        }];

        let legacy = materialize_messages_for_upstream(&messages, &config).unwrap();
        let explicit = materialize_messages_for_request(UpstreamMaterializationRequest {
            messages: &messages,
            policy: UpstreamModalityPolicy {
                allow_images: false,
                allow_files: false,
                allow_audio: false,
                capability_text: "current model/backend combination".to_string(),
            },
            artifact_store: RequestViewArtifactStore::new(temp_dir.path()),
        })
        .unwrap();

        assert_eq!(explicit, legacy);
    }

    #[test]
    fn shared_modality_rewriter_supports_replace_and_drop() {
        let content = Some(json!([
            {"type": "text", "text": "hello"},
            {"type": "input_image", "image_url": "data:image/png;base64,AA=="},
            {"type": "input_audio", "input_audio": {"data": "AA==", "format": "wav"}}
        ]));

        let rewritten = rewrite_message_content_with_modality_policy(
            &content,
            UpstreamModalityPolicy {
                allow_images: false,
                allow_files: true,
                allow_audio: false,
                capability_text: "test model".to_string(),
            },
            |ctx| {
                if ctx.is_downgraded && ctx.item_type == "input_image" {
                    return Ok(ModalityItemRewrite::Replace(placeholder_text_item(
                        "image downgraded".to_string(),
                    )));
                }
                if ctx.is_downgraded && ctx.item_type == "input_audio" {
                    return Ok(ModalityItemRewrite::Drop);
                }
                Ok(ModalityItemRewrite::KeepOriginal)
            },
        )
        .unwrap();

        assert_eq!(
            rewritten,
            Some(json!([
                {"type": "text", "text": "hello"},
                {"type": "text", "text": "image downgraded"}
            ]))
        );
    }
}
