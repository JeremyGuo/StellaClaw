use super::*;

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

pub(super) fn coalesce_buffered_conversation_messages(
    initial: IncomingMessage,
    pending_messages: &mut VecDeque<IncomingMessage>,
) -> IncomingMessage {
    if initial.control.is_some() {
        return initial;
    }

    let mut grouped = vec![initial];
    let mut remaining = VecDeque::new();
    while let Some(candidate) = pending_messages.pop_front() {
        if candidate.control.is_none() && candidate.address == grouped[0].address {
            grouped.push(candidate);
        } else {
            remaining.push_back(candidate);
        }
    }
    *pending_messages = remaining;
    merge_buffered_messages(grouped)
}

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
        control: None,
    }
}

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

pub(super) fn should_emit_profile_change_prompt(text: Option<&str>) -> bool {
    let trimmed = text.map(str::trim_start).unwrap_or("");
    !trimmed.starts_with(INTERRUPTED_FOLLOWUP_MARKER)
        && !trimmed.starts_with(QUEUED_USER_UPDATES_MARKER)
}

pub(super) struct IncomingYieldDisposition {
    pub(super) interrupted: bool,
    pub(super) compaction_in_progress: bool,
}

pub(super) fn request_yield_for_incoming(
    active_controls: &Arc<Mutex<HashMap<String, SessionExecutionControl>>>,
    active_phases: &Arc<Mutex<HashMap<String, ForegroundRuntimePhase>>>,
    message: &IncomingMessage,
) -> IncomingYieldDisposition {
    if message.control.is_some() {
        return IncomingYieldDisposition {
            interrupted: false,
            compaction_in_progress: false,
        };
    }
    let session_key = message.address.session_key();
    let control = active_controls
        .lock()
        .ok()
        .and_then(|controls| controls.get(&session_key).cloned());
    let compaction_in_progress = active_phases
        .lock()
        .ok()
        .and_then(|phases| phases.get(&session_key).copied())
        .is_some_and(|phase| phase == ForegroundRuntimePhase::Compacting);
    if let Some(control) = control {
        control.request_yield();
        IncomingYieldDisposition {
            interrupted: true,
            compaction_in_progress,
        }
    } else {
        IncomingYieldDisposition {
            interrupted: false,
            compaction_in_progress: false,
        }
    }
}

pub(super) fn update_active_foreground_phase(
    active_phases: &Arc<Mutex<HashMap<String, ForegroundRuntimePhase>>>,
    session_key: &str,
    event: &SessionEvent,
) {
    let phase = match event {
        SessionEvent::CompactionStarted { .. } | SessionEvent::ToolWaitCompactionStarted { .. } => {
            Some(ForegroundRuntimePhase::Compacting)
        }
        SessionEvent::CompactionCompleted { .. }
        | SessionEvent::ToolWaitCompactionCompleted { .. }
        | SessionEvent::SessionStarted { .. }
        | SessionEvent::RoundStarted { .. }
        | SessionEvent::ModelCallStarted { .. }
        | SessionEvent::ModelCallCompleted { .. }
        | SessionEvent::CheckpointEmitted { .. }
        | SessionEvent::ToolWaitCompactionScheduled { .. }
        | SessionEvent::ToolCallStarted { .. }
        | SessionEvent::ToolCallCompleted { .. }
        | SessionEvent::SessionYielded { .. }
        | SessionEvent::PrefixRewriteApplied { .. }
        | SessionEvent::SessionCompleted { .. } => Some(ForegroundRuntimePhase::Running),
    };
    if let Some(phase) = phase
        && let Ok(mut phases) = active_phases.lock()
    {
        phases.insert(session_key.to_string(), phase);
    }
}

pub(super) fn tag_interrupted_followup_text(text: Option<String>) -> Option<String> {
    match text {
        Some(text) if !text.trim().is_empty() => {
            Some(format!("{INTERRUPTED_FOLLOWUP_MARKER}\n{text}"))
        }
        _ => Some(INTERRUPTED_FOLLOWUP_MARKER.to_string()),
    }
}

pub(super) fn fast_path_model_selection_message(
    workdir: &Path,
    models: &BTreeMap<String, ModelConfig>,
    chat_model_keys: &[String],
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

    let settings = ConversationManager::new(workdir)
        .ok()
        .and_then(|manager| manager.get_snapshot(&message.address))
        .map(|snapshot| snapshot.settings)
        .unwrap_or_default();
    if settings.main_model.is_some() {
        return None;
    }

    let mut options = chat_model_keys
        .iter()
        .filter(|model_key| models.contains_key(model_key.as_str()))
        .cloned()
        .map(|model_key| ShowOption {
            label: model_key.clone(),
            value: format!("/model {}", model_key),
        })
        .collect::<Vec<_>>();
    options.sort_by(|left, right| left.label.cmp(&right.label));
    Some(OutgoingMessage::with_options(
        "This conversation has no model yet. Choose one to start a new session.\nCurrent conversation model: `<not selected>`\nChoose a model below or send `/model <name>`.",
        "Choose a model",
        options,
    ))
}

pub(super) fn build_user_turn_message(
    text: Option<&str>,
    attachments: &[StoredAttachment],
    model: &ModelConfig,
    backend_supports_native_multimodal: bool,
) -> Result<ChatMessage> {
    let image_attachments = attachments
        .iter()
        .filter(|attachment| attachment.kind == AttachmentKind::Image)
        .collect::<Vec<_>>();
    if !backend_supports_native_multimodal
        || !model.supports_vision_input
        || image_attachments.is_empty()
    {
        return Ok(ChatMessage::text(
            "user",
            compose_user_prompt(text, attachments),
        ));
    }

    let mut text_sections = Vec::new();
    if let Some(text) = text.map(str::trim).filter(|value| !value.is_empty()) {
        text_sections.push(text.to_string());
    }

    let file_attachments = attachments
        .iter()
        .filter(|attachment| attachment.kind != AttachmentKind::Image)
        .collect::<Vec<_>>();
    if !file_attachments.is_empty() {
        let mut attachment_lines =
            vec!["Non-image attachments available for this turn:".to_string()];
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
            "Use tools if you need to inspect any non-image attachment or related files."
                .to_string(),
        );
        text_sections.push(attachment_lines.join("\n"));
    }

    if text_sections.is_empty() {
        text_sections.push(format!(
            "The user attached {} image(s). Inspect the images directly.",
            image_attachments.len()
        ));
    } else {
        text_sections.push(format!(
            "The user attached {} image(s), and those images are already directly visible in this request. Inspect them directly here instead of calling the image tool again for the same current-turn attachments.",
            image_attachments.len()
        ));
    }

    let mut content = vec![json!({
        "type": "text",
        "text": text_sections.join("\n\n")
    })];
    for image in image_attachments {
        content.push(json!({
            "type": "image_url",
            "image_url": {
                "url": build_image_data_url(image)?,
            }
        }));
    }

    Ok(ChatMessage {
        role: "user".to_string(),
        content: Some(Value::Array(content)),
        name: None,
        tool_call_id: None,
        tool_calls: None,
    })
}

pub(super) fn build_synthetic_system_messages(
    skill_updates_prefix: Option<&str>,
    profile_change_notices: &[SharedProfileChangeNotice],
) -> Vec<ChatMessage> {
    let mut messages = Vec::new();
    for notice in profile_change_notices {
        let text = match notice {
            SharedProfileChangeNotice::UserUpdated => {
                "[System Message: USER.md changed. It stores user info. If you need refreshed user info in this run, use read_file on ./USER.md.]"
            }
            SharedProfileChangeNotice::IdentityUpdated => {
                "[System Message: IDENTITY.md changed. It defines your persona. Use read_file on ./IDENTITY.md now so your current behavior follows the updated persona.]"
            }
        };
        messages.push(ChatMessage::text("system", text));
    }
    if let Some(skill_updates_prefix) = skill_updates_prefix
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        messages.push(ChatMessage::text("system", skill_updates_prefix));
    }
    messages
}

pub(super) fn build_previous_messages_for_turn(
    session_agent_messages: &[ChatMessage],
    pending_continue: Option<&PendingContinueState>,
    injected_messages: &[ChatMessage],
    next_user_message: Option<ChatMessage>,
) -> Vec<ChatMessage> {
    let mut previous_messages = pending_continue
        .map(|pending| pending.resume_messages.clone())
        .unwrap_or_else(|| session_agent_messages.to_vec());
    previous_messages.extend(injected_messages.iter().cloned());
    if let Some(next_user_message) = next_user_message {
        previous_messages.push(next_user_message);
    }
    previous_messages
}

fn system_message_text(message: &ChatMessage) -> Option<&str> {
    if message.role != "system" {
        return None;
    }
    message.content.as_ref().and_then(Value::as_str)
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
    let mime_type = attachment
        .media_type
        .as_deref()
        .filter(|value| value.starts_with("image/"))
        .map(ToOwned::to_owned)
        .unwrap_or_else(|| infer_image_media_type(&attachment.path));
    let encoded = base64::engine::general_purpose::STANDARD.encode(bytes);
    Ok(format!("data:{};base64,{}", mime_type, encoded))
}

fn infer_image_media_type(path: &Path) -> String {
    match path
        .extension()
        .and_then(|value| value.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase()
        .as_str()
    {
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "bmp" => "image/bmp",
        _ => "image/png",
    }
    .to_string()
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
    relative_path: &str,
) -> Result<OutgoingAttachment> {
    let candidate = PathBuf::from(relative_path);
    if candidate.is_absolute() {
        return Err(anyhow!(
            "attachment path must be relative to workspace root, got absolute path {}",
            candidate.display()
        ));
    }

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

    let extension = canonical_file
        .extension()
        .and_then(|value| value.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase();
    let kind = match extension.as_str() {
        "png" | "jpg" | "jpeg" | "gif" | "webp" | "bmp" => AttachmentKind::Image,
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
    };
    for attachment in attachments {
        let attachment = persist_outgoing_attachment(session, attachment)?;
        match attachment.kind {
            AttachmentKind::Image => outgoing.images.push(attachment),
            AttachmentKind::File => outgoing.attachments.push(attachment),
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
