use super::*;
#[cfg(test)]
use crate::session::QUEUED_USER_UPDATES_MARKER;
use image::{ImageFormat, ImageReader};
#[cfg(test)]
use std::collections::VecDeque;
use std::io::Cursor;

pub(super) fn send_outgoing_message_now(
    channel: Arc<dyn Channel>,
    address: ChannelAddress,
    message: OutgoingMessage,
) -> Result<()> {
    let (sender, receiver) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let result = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .context("failed to build temporary Tokio runtime for immediate channel send")
            .and_then(|runtime| {
                runtime
                    .block_on(async move { channel.send(&address, message).await })
                    .context("failed to send immediate channel message")
            })
            .map_err(|error| format!("{error:#}"));
        let _ = sender.send(result);
    });
    match receiver.recv() {
        Ok(Ok(())) => Ok(()),
        Ok(Err(error)) => Err(anyhow!(error)),
        Err(_) => Err(anyhow!("immediate channel send thread closed unexpectedly")),
    }
}

pub(super) fn compose_user_prompt(text: Option<&str>, attachments: &[StoredAttachment]) -> String {
    let mut sections = Vec::new();
    if let Some(text) = text.map(str::trim).filter(|value| !value.is_empty()) {
        sections.push(text.to_string());
    }
    if !attachments.is_empty() {
        let mut attachment_lines = vec!["Attachments available for this turn:".to_string()];
        for attachment in attachments {
            attachment_lines.push(format!(
                "- kind={:?}, path={}, original_name={}, media_type={}",
                attachment.kind,
                attachment.path.display(),
                attachment.original_name.as_deref().unwrap_or("unknown"),
                attachment.media_type.as_deref().unwrap_or("unknown")
            ));
        }
        attachment_lines.push(
            "Use tools if you need to inspect any text attachment or related files.".to_string(),
        );
        sections.push(attachment_lines.join("\n"));
    }
    if sections.is_empty() {
        "(No text content; inspect attachments if needed.)".to_string()
    } else {
        sections.join("\n\n")
    }
}

fn prepend_system_date_section(
    mut sections: Vec<String>,
    system_date: Option<&str>,
) -> Vec<String> {
    if let Some(system_date) = system_date.map(str::trim).filter(|value| !value.is_empty()) {
        sections.insert(0, system_date.to_string());
    }
    sections
}

#[cfg(test)]
pub(super) fn coalesce_buffered_conversation_messages(
    initial: IncomingMessage,
    pending_messages: &mut VecDeque<IncomingMessage>,
) -> IncomingMessage {
    if initial.control.is_some()
        || !initial.stored_attachments.is_empty()
        || is_command_like_text(initial.text.as_deref())
    {
        return initial;
    }

    let mut grouped = vec![initial];
    let mut remaining = VecDeque::new();
    while let Some(candidate) = pending_messages.pop_front() {
        if candidate.control.is_none()
            && candidate.stored_attachments.is_empty()
            && candidate.address == grouped[0].address
            && !is_command_like_text(candidate.text.as_deref())
        {
            grouped.push(candidate);
        } else {
            remaining.push_back(candidate);
            remaining.extend(pending_messages.drain(..));
            break;
        }
    }
    *pending_messages = remaining;
    merge_buffered_messages(grouped)
}

#[cfg(test)]
fn merge_buffered_messages(mut grouped: Vec<IncomingMessage>) -> IncomingMessage {
    if grouped.len() == 1 {
        return grouped.remove(0);
    }

    let remote_message_id = grouped
        .last()
        .map(|message| message.remote_message_id.clone())
        .expect("grouped messages should not be empty");
    let address = grouped
        .last()
        .map(|message| message.address.clone())
        .expect("grouped messages should not be empty");
    let mut flattened = Vec::new();
    let mut attachments = Vec::new();
    for message in grouped.drain(..) {
        flattened.push((message.text, message.attachments.len()));
        attachments.extend(message.attachments);
    }

    IncomingMessage {
        remote_message_id,
        address,
        text: Some(render_buffered_followup_messages(
            &flattened
                .iter()
                .map(|(text, attachment_count)| (text.as_deref(), *attachment_count))
                .collect::<Vec<_>>(),
        )),
        attachments,
        stored_attachments: Vec::new(),
        control: None,
    }
}

#[cfg(test)]
fn render_buffered_followup_messages(messages: &[(Option<&str>, usize)]) -> String {
    let mut sections = vec![
        QUEUED_USER_UPDATES_MARKER.to_string(),
        "While you were still working on the previous turn, the user sent multiple follow-up messages. Treat later items as newer steering updates when they conflict.".to_string(),
    ];
    for (index, (text, attachment_count)) in messages.iter().enumerate() {
        let trimmed = text.map(str::trim).unwrap_or("");
        let body = match (trimmed, *attachment_count) {
            ("", 0) => "[empty message]".to_string(),
            ("", count) => format!("[attachments only: {count}]"),
            (value, 0) => value.to_string(),
            (value, count) => format!("{value}\n[attachments: {count}]"),
        };
        sections.push(format!("Follow-up {}:\n{}", index + 1, body));
    }
    sections.join("\n\n")
}

pub(super) fn fast_path_agent_selection_message(
    workdir: &Path,
    models: &BTreeMap<String, ModelConfig>,
    agent: &AgentConfig,
    message: &IncomingMessage,
) -> Option<OutgoingMessage> {
    if message.control.is_some() {
        return None;
    }
    let text = message.text.as_deref()?.trim();
    if text.is_empty() {
        return None;
    }
    if text.starts_with('/') {
        return None;
    }

    let manager = ConversationManager::new(workdir).ok()?;
    let settings = manager
        .get_snapshot(&message.address)
        .map(|snapshot| snapshot.settings)
        .unwrap_or_default();
    if settings.main_model.is_some() {
        return None;
    }

    let mut options = agent
        .available_models(AgentBackendKind::AgentFrame)
        .iter()
        .filter(|model_key| models.contains_key(model_key.as_str()))
        .cloned()
        .map(|model_key| ShowOption {
            label: model_key.clone(),
            value: format!("/agent {}", model_key),
        })
        .collect::<Vec<_>>();
    options.sort_by(|left, right| left.label.cmp(&right.label));
    Some(OutgoingMessage::with_options(
        "This conversation has no model yet.\nCurrent conversation model: `<not selected>`\nChoose a model below or send `/agent <model>`.",
        "Choose a model",
        options,
    ))
}

pub(super) fn build_user_turn_message(
    text: Option<&str>,
    attachments: &[StoredAttachment],
    model: &ModelConfig,
    backend_supports_native_multimodal: bool,
    system_date: Option<&str>,
) -> Result<ChatMessage> {
    let allow_images = backend_supports_native_multimodal && model.supports_image_input();
    let allow_pdfs =
        backend_supports_native_multimodal && model.has_capability(ModelCapability::Pdf);
    let allow_audio =
        backend_supports_native_multimodal && model.has_capability(ModelCapability::AudioIn);
    let direct_images = attachments
        .iter()
        .filter(|attachment| attachment.kind.is_image() && allow_images)
        .filter_map(|attachment| {
            build_image_data_url(attachment)
                .ok()
                .map(|url| (attachment, url))
        })
        .collect::<Vec<_>>();
    let pdf_attachments = attachments
        .iter()
        .filter(|attachment| attachment.kind.is_pdf() && allow_pdfs)
        .collect::<Vec<_>>();
    let audio_attachments = attachments
        .iter()
        .filter(|attachment| attachment.kind.is_audio() && allow_audio)
        .filter(|attachment| infer_audio_format_for_attachment(attachment).is_some())
        .collect::<Vec<_>>();
    if direct_images.is_empty() && pdf_attachments.is_empty() && audio_attachments.is_empty() {
        return Ok(ChatMessage::text(
            "user",
            prepend_system_date_section(vec![compose_user_prompt(text, attachments)], system_date)
                .join("\n\n"),
        ));
    }

    let mut text_sections = Vec::new();
    if let Some(text) = text.map(str::trim).filter(|value| !value.is_empty()) {
        text_sections.push(text.to_string());
    }

    let file_attachments = attachments
        .iter()
        .filter(|attachment| {
            !direct_images
                .iter()
                .any(|(direct, _)| direct.id == attachment.id)
                && !pdf_attachments
                    .iter()
                    .any(|direct| direct.id == attachment.id)
                && !audio_attachments
                    .iter()
                    .any(|direct| direct.id == attachment.id)
        })
        .collect::<Vec<_>>();
    if !file_attachments.is_empty() {
        let mut attachment_lines =
            vec!["Additional attachments available for this turn:".to_string()];
        for attachment in file_attachments {
            attachment_lines.push(format!(
                "- kind={:?}, path={}, original_name={}, media_type={}",
                attachment.kind,
                attachment.path.display(),
                attachment.original_name.as_deref().unwrap_or("unknown"),
                attachment.media_type.as_deref().unwrap_or("unknown")
            ));
        }
        attachment_lines.push(
            "Use tools if you need to inspect any attachment that is not already directly visible in this request."
                .to_string(),
        );
        text_sections.push(attachment_lines.join("\n"));
    }

    if text_sections.is_empty() {
        text_sections.push(direct_multimodal_summary(
            direct_images.len(),
            pdf_attachments.len(),
            audio_attachments.len(),
        ));
    } else {
        text_sections.push(format!(
            "{} Inspect directly visible current-turn attachments here instead of calling load/query tools for the same files.",
            direct_multimodal_summary(
                direct_images.len(),
                pdf_attachments.len(),
                audio_attachments.len()
            )
        ));
    }
    let text_sections = prepend_system_date_section(text_sections, system_date);

    let mut content = vec![json!({
        "type": "text",
        "text": text_sections.join("\n\n")
    })];
    for (_, image_url) in direct_images {
        content.push(json!({
            "type": "image_url",
            "image_url": {
                "url": image_url,
            }
        }));
    }
    for pdf in pdf_attachments {
        content.push(build_pdf_content_item(pdf)?);
    }
    for audio in audio_attachments {
        content.push(build_audio_content_item(audio)?);
    }

    Ok(ChatMessage {
        role: "user".to_string(),
        content: Some(Value::Array(content)),
        name: None,
        tool_call_id: None,
        tool_calls: None,
    })
}

pub(super) fn build_synthetic_runtime_messages(
    prompt_updates_prefix: Option<&str>,
    skill_updates_prefix: Option<&str>,
) -> Vec<ChatMessage> {
    let mut messages = Vec::new();
    if let Some(prompt_updates_prefix) = prompt_updates_prefix
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        messages.push(ChatMessage::text("user", prompt_updates_prefix));
    }
    if let Some(skill_updates_prefix) = skill_updates_prefix
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        messages.push(ChatMessage::text("user", skill_updates_prefix));
    }
    messages
}

#[cfg(test)]
pub(super) fn render_system_date_on_user_message(now: chrono::DateTime<chrono::Utc>) -> String {
    let local_now = now.with_timezone(&chrono::Local);
    format!(
        "[System Date: {}]",
        local_now.format("%Y-%m-%d %H:%M:%S %:z")
    )
}

const SUPPORTED_INLINE_IMAGE_FORMATS_TEXT: &str = "JPEG, PNG, GIF, or WebP";

enum InlineImageUrlNormalization {
    Unchanged,
    Rewritten(String),
}

#[cfg_attr(not(test), allow(dead_code))]
fn placeholder_text_item(text: String) -> Value {
    json!({
        "type": "text",
        "text": text,
    })
}

fn downgraded_multimodal_placeholder(
    item_type: &str,
    item: &Value,
    capability_text: &str,
) -> Option<String> {
    match item_type {
        "image_url" | "input_image" => Some(format!(
            "[Earlier image omitted because the current {capability_text} does not accept image input.]"
        )),
        "file" | "input_file" => {
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
        "input_audio" => Some(format!(
            "[Earlier audio omitted because the current {capability_text} does not accept audio input.]"
        )),
        _ => None,
    }
}

fn sanitize_message_content_for_model_capabilities(
    content: &Option<Value>,
    allow_images: bool,
    allow_files: bool,
    allow_audio: bool,
    capability_text: &str,
) -> Option<Value> {
    let Some(Value::Array(items)) = content else {
        return content.clone();
    };

    let mut sanitized = Vec::with_capacity(items.len());
    for item in items {
        let Some(item_type) = item.get("type").and_then(Value::as_str) else {
            sanitized.push(item.clone());
            continue;
        };
        match item_type {
            "image_url" | "input_image" if !allow_images => {
                if let Some(text) =
                    downgraded_multimodal_placeholder(item_type, item, capability_text)
                {
                    sanitized.push(placeholder_text_item(text));
                }
            }
            "image_url" | "input_image" => match sanitize_inline_image_item(item_type, item) {
                Ok(value) => sanitized.push(value),
                Err(_) => {
                    sanitized.push(placeholder_text_item(unsupported_inline_image_placeholder()))
                }
            },
            "file" | "input_file" if !allow_files => {
                if let Some(text) =
                    downgraded_multimodal_placeholder(item_type, item, capability_text)
                {
                    sanitized.push(placeholder_text_item(text));
                }
            }
            "input_audio" if !allow_audio => {
                if let Some(text) =
                    downgraded_multimodal_placeholder(item_type, item, capability_text)
                {
                    sanitized.push(placeholder_text_item(text));
                }
            }
            _ => sanitized.push(item.clone()),
        }
    }

    Some(Value::Array(sanitized))
}

pub(super) fn sanitize_messages_for_model_capabilities(
    messages: &[ChatMessage],
    model: &ModelConfig,
    backend_supports_native_multimodal: bool,
) -> Vec<ChatMessage> {
    let allow_images = backend_supports_native_multimodal && model.supports_image_input();
    let allow_files =
        backend_supports_native_multimodal && model.has_capability(ModelCapability::Pdf);
    let allow_audio =
        backend_supports_native_multimodal && model.has_capability(ModelCapability::AudioIn);
    let capability_text = if backend_supports_native_multimodal {
        "model"
    } else {
        "backend/model combination"
    };

    messages
        .iter()
        .map(|message| {
            let mut sanitized = message.clone();
            sanitized.content = sanitize_message_content_for_model_capabilities(
                &message.content,
                allow_images,
                allow_files,
                allow_audio,
                capability_text,
            );
            sanitized
        })
        .collect()
}

fn system_message_text(message: &ChatMessage) -> Option<&str> {
    if message.role != "system" {
        return None;
    }
    message.content.as_ref().and_then(Value::as_str)
}

pub(super) fn rebuild_canonical_system_prompt(
    messages: &[ChatMessage],
    canonical_system_prompt: &str,
) -> (Vec<ChatMessage>, bool) {
    let mut leading_system_count = 0usize;
    while leading_system_count < messages.len()
        && system_message_text(&messages[leading_system_count]).is_some()
    {
        leading_system_count += 1;
    }

    if leading_system_count == 1
        && system_message_text(&messages[0]) == Some(canonical_system_prompt)
    {
        return (messages.to_vec(), false);
    }

    if leading_system_count == 0 {
        if messages.is_empty() {
            return (Vec::new(), false);
        }
        let mut rebuilt = Vec::with_capacity(messages.len() + 1);
        rebuilt.push(ChatMessage::text("system", canonical_system_prompt));
        rebuilt.extend_from_slice(messages);
        return (rebuilt, true);
    }

    let mut rebuilt = Vec::with_capacity(messages.len() - leading_system_count + 1);
    rebuilt.push(ChatMessage::text("system", canonical_system_prompt));
    rebuilt.extend(messages.iter().skip(leading_system_count).cloned());
    (rebuilt, true)
}

pub(super) fn prepare_system_prompt_for_turn(
    messages: &[ChatMessage],
    current_full_system_prompt: &str,
    force_rebuild: bool,
) -> (Vec<ChatMessage>, String, bool) {
    let mut leading_system_count = 0usize;
    while leading_system_count < messages.len()
        && system_message_text(&messages[leading_system_count]).is_some()
    {
        leading_system_count += 1;
    }

    if force_rebuild || leading_system_count == 0 || leading_system_count > 1 {
        let (rebuilt, changed) =
            rebuild_canonical_system_prompt(messages, current_full_system_prompt);
        return (rebuilt, current_full_system_prompt.to_string(), changed);
    }

    let cached = system_message_text(&messages[0])
        .unwrap_or(current_full_system_prompt)
        .to_string();
    (messages.to_vec(), cached, false)
}

pub(super) fn normalize_messages_for_persistence(
    messages: Vec<ChatMessage>,
    canonical_system_prompt: &str,
    ephemeral_system_messages: &[ChatMessage],
) -> Vec<ChatMessage> {
    let ephemeral_texts = ephemeral_system_messages
        .iter()
        .filter_map(system_message_text)
        .collect::<HashSet<_>>();

    let mut normalized = Vec::new();
    let mut index = 0usize;
    while index < messages.len() && system_message_text(&messages[index]).is_some() {
        index += 1;
    }
    if index > 0 {
        normalized.push(ChatMessage::text("system", canonical_system_prompt));
    }
    for message in messages.into_iter().skip(index) {
        if let Some(text) = system_message_text(&message)
            && ephemeral_texts.contains(text)
        {
            continue;
        }
        normalized.push(message);
    }
    normalized
}

pub(super) fn render_skill_change_notices(notices: &[SkillChangeNotice]) -> String {
    if notices.is_empty() {
        return String::new();
    }
    let mut sections = vec![
        "[Runtime Skill Updates]".to_string(),
        "The global skill registry changed since earlier in this session. Apply these updates before handling the user's new request.".to_string(),
    ];
    for notice in notices {
        match notice {
            SkillChangeNotice::MetadataChanged { metadata_prompt } => {
                sections.push(format!(
                    "The available skill metadata changed. Treat this refreshed metadata as authoritative for this turn:\n{}",
                    metadata_prompt.trim()
                ));
            }
            SkillChangeNotice::DescriptionChanged { name, description } => {
                sections.push(format!(
                    "Skill \"{name}\" has an updated description:\n{description}"
                ));
            }
            SkillChangeNotice::ContentChanged {
                name,
                description,
                content,
            } => {
                sections.push(format!(
                    "Skill \"{name}\" changed after it was loaded earlier in this session and before that load was compacted away. Use the refreshed skill immediately.\nUpdated description: {description}\nRefreshed SKILL.md content:\n{content}"
                ));
            }
        }
    }
    sections.join("\n\n")
}

pub(super) fn render_prompt_component_change_notices(
    notices: &[PromptComponentChangeNotice],
) -> String {
    if notices.is_empty() {
        return String::new();
    }
    let mut sections = vec![
        "[Runtime Prompt Updates]".to_string(),
        "Some durable profile context changed since the current canonical system prompt snapshot. Apply these updates for this user turn; they will be folded into the canonical system prompt after compaction.".to_string(),
    ];
    for notice in notices {
        match notice.key.as_str() {
            IDENTITY_PROMPT_COMPONENT => {
                if notice.value.trim().is_empty() {
                    sections.push(
                        "Identity is now empty. Ignore earlier Identity prompt content."
                            .to_string(),
                    );
                } else {
                    sections.push(format!(
                        "Identity changed. Treat this refreshed identity as authoritative for this turn:\n{}",
                        notice.value.trim()
                    ));
                }
            }
            USER_META_PROMPT_COMPONENT => {
                if notice.value.trim().is_empty() {
                    sections.push(
                        "User meta is now empty. Ignore earlier User meta prompt content."
                            .to_string(),
                    );
                } else {
                    sections.push(format!(
                        "User meta changed. Treat this refreshed user metadata as authoritative for this turn:\n{}",
                        notice.value.trim()
                    ));
                }
            }
            REMOTE_ALIASES_PROMPT_COMPONENT => {
                sections.push(format!(
                    "The available SSH remote alias list changed. Treat this refreshed list as authoritative for remote tool calls in this turn:\n{}",
                    notice.value.trim()
                ));
            }
            key => {
                sections.push(format!(
                    "Prompt component `{key}` changed. Treat this refreshed value as authoritative for this turn:\n{}",
                    notice.value.trim()
                ));
            }
        }
    }
    sections.join("\n\n")
}

pub(super) fn extract_loaded_skill_names(
    messages: &[ChatMessage],
    previous_message_count: usize,
) -> Vec<String> {
    let mut skill_names = Vec::new();
    for message in messages.iter().skip(previous_message_count) {
        if message.role != "tool" {
            continue;
        }
        let Some(tool_name) = message.name.as_deref() else {
            continue;
        };
        if tool_name != "skill_load" {
            continue;
        }
        let Some(content) = message.content.as_ref().and_then(|value| value.as_str()) else {
            continue;
        };
        let Ok(parsed) = serde_json::from_str::<Value>(content) else {
            continue;
        };
        let Some(skill_name) = parsed.get("name").and_then(|value| value.as_str()) else {
            continue;
        };
        if !skill_names.iter().any(|existing| existing == skill_name) {
            skill_names.push(skill_name.to_string());
        }
    }
    skill_names
}

fn build_image_data_url(attachment: &StoredAttachment) -> Result<String> {
    let bytes = std::fs::read(&attachment.path).with_context(|| {
        format!(
            "failed to read image attachment {}",
            attachment.path.display()
        )
    })?;
    if let Some(media_type) = image_media_type_hint(attachment) {
        let encoded = base64::engine::general_purpose::STANDARD.encode(&bytes);
        let original_url = format!("data:{media_type};base64,{encoded}");
        return match normalize_inline_image_url(&original_url)? {
            InlineImageUrlNormalization::Unchanged => Ok(original_url),
            InlineImageUrlNormalization::Rewritten(url) => Ok(url),
        };
    }

    transcode_inline_image_bytes_to_png_data_url(&bytes, &attachment.path.display().to_string())
}

fn direct_multimodal_summary(image_count: usize, pdf_count: usize, audio_count: usize) -> String {
    let mut parts = Vec::new();
    if image_count > 0 {
        parts.push(format!("{image_count} image(s)"));
    }
    if pdf_count > 0 {
        parts.push(format!("{pdf_count} PDF document(s)"));
    }
    if audio_count > 0 {
        parts.push(format!("{audio_count} audio clip(s)"));
    }
    format!(
        "The user attached {}, and they are already directly visible in this request.",
        parts.join(", ")
    )
}

fn build_pdf_content_item(attachment: &StoredAttachment) -> Result<Value> {
    let encoded = file_to_base64(attachment)?;
    Ok(json!({
        "type": "file",
        "file": {
            "file_data": encoded,
            "filename": attachment_filename(attachment, "document.pdf"),
        }
    }))
}

fn build_audio_content_item(attachment: &StoredAttachment) -> Result<Value> {
    let encoded = file_to_base64(attachment)?;
    let format = infer_audio_format_for_attachment(attachment).ok_or_else(|| {
        anyhow!(
            "unsupported audio attachment format for {}",
            attachment.path.display()
        )
    })?;
    Ok(json!({
        "type": "input_audio",
        "input_audio": {
            "data": encoded,
            "format": format,
        }
    }))
}

fn file_to_base64(attachment: &StoredAttachment) -> Result<String> {
    let bytes = std::fs::read(&attachment.path)
        .with_context(|| format!("failed to read attachment {}", attachment.path.display()))?;
    Ok(base64::engine::general_purpose::STANDARD.encode(bytes))
}

fn attachment_filename(attachment: &StoredAttachment, fallback: &str) -> String {
    attachment
        .original_name
        .as_deref()
        .filter(|value| !value.trim().is_empty())
        .map(ToOwned::to_owned)
        .or_else(|| {
            attachment
                .path
                .file_name()
                .map(|value| value.to_string_lossy().to_string())
        })
        .unwrap_or_else(|| fallback.to_string())
}

fn infer_audio_format_for_attachment(attachment: &StoredAttachment) -> Option<&'static str> {
    if let Some(media_type) = attachment.media_type.as_deref() {
        match media_type.to_ascii_lowercase().as_str() {
            "audio/wav" | "audio/wave" | "audio/x-wav" => return Some("wav"),
            "audio/mpeg" | "audio/mp3" | "audio/mpga" => return Some("mp3"),
            "audio/ogg" | "audio/opus" => return Some("ogg"),
            "audio/webm" => return Some("webm"),
            "audio/mp4" | "audio/aac" | "audio/m4a" => return Some("m4a"),
            "audio/flac" => return Some("flac"),
            _ => {}
        }
    }
    match attachment
        .path
        .extension()
        .and_then(|value| value.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase()
        .as_str()
    {
        "wav" => Some("wav"),
        "mp3" | "mpeg" | "mpga" => Some("mp3"),
        "ogg" | "opus" => Some("ogg"),
        "webm" => Some("webm"),
        "m4a" | "mp4" | "aac" => Some("m4a"),
        "flac" => Some("flac"),
        _ => None,
    }
}

fn unsupported_inline_image_placeholder() -> String {
    format!(
        "[Earlier image omitted because it could not be converted into a supported inline image format ({SUPPORTED_INLINE_IMAGE_FORMATS_TEXT}).]"
    )
}

fn sanitize_inline_image_item(item_type: &str, item: &Value) -> Result<Value> {
    let Some(url) = inline_image_url_from_item(item_type, item) else {
        return Ok(item.clone());
    };
    match normalize_inline_image_url(url)? {
        InlineImageUrlNormalization::Unchanged => Ok(item.clone()),
        InlineImageUrlNormalization::Rewritten(url) => {
            Ok(rebuild_inline_image_item(item_type, item, url))
        }
    }
}

fn normalize_inline_image_url(url: &str) -> Result<InlineImageUrlNormalization> {
    let Some((media_type, encoded)) = parse_inline_image_data_url(url) else {
        return Ok(InlineImageUrlNormalization::Unchanged);
    };
    if let Some(canonical) = canonical_inline_image_media_type(media_type) {
        if media_type.eq_ignore_ascii_case(canonical) {
            return Ok(InlineImageUrlNormalization::Unchanged);
        }
        return Ok(InlineImageUrlNormalization::Rewritten(format!(
            "data:{canonical};base64,{encoded}"
        )));
    }

    let bytes = base64::engine::general_purpose::STANDARD
        .decode(encoded)
        .context("failed to decode unsupported inline image data")?;
    let rewritten = transcode_inline_image_bytes_to_png_data_url(&bytes, media_type)?;
    Ok(InlineImageUrlNormalization::Rewritten(rewritten))
}

fn transcode_inline_image_bytes_to_png_data_url(bytes: &[u8], label: &str) -> Result<String> {
    let image = ImageReader::new(Cursor::new(bytes))
        .with_guessed_format()
        .with_context(|| format!("failed to guess image format for {label}"))?
        .decode()
        .with_context(|| format!("failed to decode image data for {label}"))?;
    let mut output = Vec::new();
    image
        .write_to(&mut Cursor::new(&mut output), ImageFormat::Png)
        .with_context(|| format!("failed to transcode image data for {label} to PNG"))?;
    let encoded = base64::engine::general_purpose::STANDARD.encode(output);
    Ok(format!("data:image/png;base64,{encoded}"))
}

fn inline_image_url_from_item<'a>(item_type: &str, item: &'a Value) -> Option<&'a str> {
    if item_type == "image_url" {
        return item.get("image_url").and_then(|value| match value {
            Value::String(url) => Some(url.as_str()),
            Value::Object(object) => object.get("url").and_then(Value::as_str),
            _ => None,
        });
    }
    item.get("image_url").and_then(Value::as_str)
}

fn rebuild_inline_image_item(item_type: &str, item: &Value, url: String) -> Value {
    let mut rebuilt = item.clone();
    let Some(object) = rebuilt.as_object_mut() else {
        return item.clone();
    };
    if item_type == "image_url" {
        object.insert("image_url".to_string(), json!({ "url": url }));
    } else {
        object.insert("image_url".to_string(), Value::String(url));
    }
    rebuilt
}

fn parse_inline_image_data_url(url: &str) -> Option<(&str, &str)> {
    let (metadata, encoded) = url.strip_prefix("data:")?.split_once(',')?;
    let mut parts = metadata.split(';');
    let media_type = parts.next()?.trim();
    if !media_type.starts_with("image/") || !parts.any(|part| part.eq_ignore_ascii_case("base64")) {
        return None;
    }
    Some((media_type, encoded))
}

fn canonical_inline_image_media_type(media_type: &str) -> Option<&'static str> {
    match media_type.to_ascii_lowercase().as_str() {
        "image/png" => Some("image/png"),
        "image/jpeg" | "image/jpg" => Some("image/jpeg"),
        "image/gif" => Some("image/gif"),
        "image/webp" => Some("image/webp"),
        _ => None,
    }
}

fn image_media_type_hint(attachment: &StoredAttachment) -> Option<String> {
    attachment
        .media_type
        .as_deref()
        .filter(|value| value.starts_with("image/"))
        .map(ToOwned::to_owned)
        .or_else(|| infer_image_media_type(&attachment.path).map(ToOwned::to_owned))
}

fn infer_image_media_type(path: &Path) -> Option<&'static str> {
    match path
        .extension()
        .and_then(|value| value.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase()
        .as_str()
    {
        "png" => Some("image/png"),
        "jpg" | "jpeg" => Some("image/jpeg"),
        "gif" => Some("image/gif"),
        "webp" => Some("image/webp"),
        "bmp" => Some("image/bmp"),
        "tif" | "tiff" => Some("image/tiff"),
        "svg" => Some("image/svg+xml"),
        _ => None,
    }
}

pub(super) fn spawn_processing_keepalive(
    channel: Arc<dyn Channel>,
    address: ChannelAddress,
    state: ProcessingState,
) -> Option<oneshot::Sender<()>> {
    let keepalive_interval = channel.processing_keepalive_interval(state)?;
    let (stop_sender, mut stop_receiver) = oneshot::channel();
    tokio::spawn(async move {
        let mut ticker = interval(keepalive_interval);
        ticker.set_missed_tick_behavior(MissedTickBehavior::Skip);
        loop {
            tokio::select! {
                _ = &mut stop_receiver => break,
                _ = ticker.tick() => {
                    if let Err(error) = channel.set_processing(&address, state).await {
                        warn!(
                            log_stream = "channel",
                            log_key = %channel.id(),
                            kind = "processing_keepalive_failed",
                            conversation_id = %address.conversation_id,
                            error = %format!("{error:#}"),
                            "processing keepalive failed"
                        );
                        break;
                    }
                }
            }
        }
    });
    Some(stop_sender)
}

pub(super) fn extract_attachment_references(
    assistant_text: &str,
    workspace_root: &Path,
) -> Result<(String, Vec<OutgoingAttachment>)> {
    let mut clean = String::new();
    let mut remainder = assistant_text;
    let mut found_paths = Vec::new();

    loop {
        let Some(open_index) = remainder.find(ATTACHMENT_OPEN_TAG) else {
            clean.push_str(remainder);
            break;
        };
        clean.push_str(&remainder[..open_index]);
        let after_open = &remainder[open_index + ATTACHMENT_OPEN_TAG.len()..];
        let Some(close_index) = after_open.find(ATTACHMENT_CLOSE_TAG) else {
            clean.push_str(&remainder[open_index..]);
            break;
        };
        let path_text = after_open[..close_index].trim();
        if !path_text.is_empty() {
            found_paths.push(path_text.to_string());
        }
        remainder = &after_open[close_index + ATTACHMENT_CLOSE_TAG.len()..];
    }

    let attachments = found_paths
        .into_iter()
        .map(|path_text| resolve_outgoing_attachment(workspace_root, &path_text))
        .collect::<Result<Vec<_>>>()?;

    Ok((clean.trim().to_string(), attachments))
}

fn resolve_outgoing_attachment(
    workspace_root: &Path,
    path_text: &str,
) -> Result<OutgoingAttachment> {
    let candidate = PathBuf::from(path_text);
    let canonical_file = if candidate.is_absolute() {
        std::fs::canonicalize(&candidate)
            .with_context(|| format!("attachment path does not exist: {}", candidate.display()))?
    } else {
        let joined = workspace_root.join(&candidate);
        let canonical_root = std::fs::canonicalize(workspace_root)
            .with_context(|| format!("failed to canonicalize {}", workspace_root.display()))?;
        let canonical_file = std::fs::canonicalize(&joined)
            .with_context(|| format!("attachment path does not exist: {}", joined.display()))?;
        if !canonical_file.starts_with(&canonical_root) {
            return Err(anyhow!(
                "attachment path escapes workspace root: {}",
                canonical_file.display()
            ));
        }
        canonical_file
    };

    let extension = canonical_file
        .extension()
        .and_then(|value| value.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase();
    let kind = match extension.as_str() {
        "png" | "jpg" | "jpeg" | "gif" | "webp" | "bmp" => AttachmentKind::Image,
        "pdf" => AttachmentKind::Pdf,
        "wav" | "mp3" | "mpeg" | "mpga" | "ogg" | "opus" | "m4a" | "aac" | "flac" => {
            AttachmentKind::Audio
        }
        "mp4" | "mov" | "mkv" | "avi" | "webm" => AttachmentKind::Video,
        "tgs" => AttachmentKind::Sticker,
        _ => AttachmentKind::File,
    };

    Ok(OutgoingAttachment {
        kind,
        path: canonical_file,
        caption: None,
    })
}

pub(super) fn build_outgoing_message_for_session(
    session: &SessionSnapshot,
    assistant_text: &str,
    workspace_root: &Path,
) -> Result<OutgoingMessage> {
    let (clean_text, attachments) = extract_attachment_references(assistant_text, workspace_root)?;
    let mut outgoing = OutgoingMessage {
        text: if clean_text.trim().is_empty() {
            None
        } else {
            Some(clean_text)
        },
        images: Vec::new(),
        attachments: Vec::new(),
        options: None,
        usage_chart: None,
    };
    for attachment in attachments {
        let attachment = persist_outgoing_attachment(session, attachment)?;
        match attachment.kind {
            AttachmentKind::Image => outgoing.images.push(attachment),
            AttachmentKind::Pdf
            | AttachmentKind::Audio
            | AttachmentKind::Voice
            | AttachmentKind::Video
            | AttachmentKind::Animation
            | AttachmentKind::Sticker
            | AttachmentKind::File => outgoing.attachments.push(attachment),
        }
    }
    Ok(outgoing)
}

fn persist_outgoing_attachment(
    session: &SessionSnapshot,
    attachment: OutgoingAttachment,
) -> Result<OutgoingAttachment> {
    let outgoing_dir = session.root_dir.join("outgoing");
    std::fs::create_dir_all(&outgoing_dir)
        .with_context(|| format!("failed to create {}", outgoing_dir.display()))?;
    let file_name = attachment
        .path
        .file_name()
        .map(|value| value.to_os_string())
        .unwrap_or_else(|| format!("attachment-{}", uuid::Uuid::new_v4()).into());
    let persisted_path = outgoing_dir.join(file_name);
    std::fs::copy(&attachment.path, &persisted_path).with_context(|| {
        format!(
            "failed to copy outgoing attachment {} to {}",
            attachment.path.display(),
            persisted_path.display()
        )
    })?;
    Ok(OutgoingAttachment {
        kind: attachment.kind,
        path: persisted_path,
        caption: attachment.caption,
    })
}

pub(super) fn relative_attachment_path(workspace_root: &Path, path: &Path) -> Result<String> {
    let relative = path.strip_prefix(workspace_root).with_context(|| {
        format!(
            "path {} is not under {}",
            path.display(),
            workspace_root.display()
        )
    })?;
    Ok(relative.to_string_lossy().to_string())
}
