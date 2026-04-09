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

fn prepend_system_date_section(
    mut sections: Vec<String>,
    system_date: Option<&str>,
) -> Vec<String> {
    if let Some(system_date) = system_date.map(str::trim).filter(|value| !value.is_empty()) {
        sections.insert(0, system_date.to_string());
    }
    sections
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

pub(super) fn should_emit_runtime_change_prompt(text: Option<&str>) -> bool {
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

    let mut manager = ConversationManager::new(workdir).ok()?;
    let settings = manager
        .get_snapshot(&message.address)
        .map(|snapshot| snapshot.settings)
        .unwrap_or_default();
    let inferred_backend = settings
        .main_model
        .as_deref()
        .and_then(|model_key| infer_single_agent_backend(agent, model_key));
    let effective_backend = settings.agent_backend.or(inferred_backend);
    if settings.agent_backend.is_none()
        && settings.main_model.is_some()
        && let Some(backend) = inferred_backend
    {
        let _ = manager.set_agent_backend(&message.address, Some(backend));
    }
    if effective_backend.is_some() && settings.main_model.is_some() {
        return None;
    }

    if let Some(backend) = effective_backend {
        let mut options = agent
            .available_models(backend)
            .iter()
            .filter(|model_key| models.contains_key(model_key.as_str()))
            .cloned()
            .map(|model_key| ShowOption {
                label: model_key.clone(),
                value: format!(
                    "/agent {} {}",
                    render_agent_backend_value(backend),
                    model_key
                ),
            })
            .collect::<Vec<_>>();
        options.sort_by(|left, right| left.label.cmp(&right.label));
        return Some(OutgoingMessage::with_options(
            format!(
                "This conversation has no model yet.\nCurrent agent backend: `{}`\nCurrent conversation model: `<not selected>`\nChoose a model below or send `/agent {} <model>`.",
                render_agent_backend_value(backend),
                render_agent_backend_value(backend),
            ),
            "Choose a model",
            options,
        ));
    }

    let mut options = [AgentBackendKind::AgentFrame, AgentBackendKind::Zgent]
        .into_iter()
        .filter(|backend| !agent.available_models(*backend).is_empty())
        .map(|backend| ShowOption {
            label: render_agent_backend_value(backend).to_string(),
            value: format!("/agent {}", render_agent_backend_value(backend)),
        })
        .collect::<Vec<_>>();
    options.sort_by(|left, right| left.label.cmp(&right.label));
    Some(OutgoingMessage::with_options(
        "This conversation has no agent selection yet.\nCurrent agent backend: `<not selected>`\nCurrent conversation model: `<not selected>`\nChoose a backend below or send `/agent <agent_frame|zgent>`.",
        "Choose a backend",
        options,
    ))
}

fn render_agent_backend_value(backend: AgentBackendKind) -> &'static str {
    match backend {
        AgentBackendKind::AgentFrame => "agent_frame",
        AgentBackendKind::Zgent => "zgent",
    }
}

pub(super) fn build_user_turn_message(
    text: Option<&str>,
    attachments: &[StoredAttachment],
    model: &ModelConfig,
    backend_supports_native_multimodal: bool,
    system_date: Option<&str>,
) -> Result<ChatMessage> {
    let image_attachments = attachments
        .iter()
        .filter(|attachment| attachment.kind == AttachmentKind::Image)
        .collect::<Vec<_>>();
    if !backend_supports_native_multimodal
        || !model.supports_image_input()
        || image_attachments.is_empty()
    {
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
    let text_sections = prepend_system_date_section(text_sections, system_date);

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
    user_time_tip: Option<&str>,
    model_catalog_change_notice: Option<&str>,
    skill_updates_prefix: Option<&str>,
    profile_change_notices: &[SharedProfileChangeNotice],
) -> Vec<ChatMessage> {
    let mut messages = Vec::new();
    if let Some(user_time_tip) = user_time_tip
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        messages.push(ChatMessage::text("system", user_time_tip));
    }
    for notice in profile_change_notices {
        let text = match notice {
            SharedProfileChangeNotice::UserUpdated => {
                "[System Message: USER.md changed. It stores user info. If you need refreshed user info in this run, use file_read on ./USER.md.]"
            }
            SharedProfileChangeNotice::IdentityUpdated => {
                "[System Message: IDENTITY.md changed. It defines your persona. Use file_read on ./IDENTITY.md now so your current behavior follows the updated persona.]"
            }
        };
        messages.push(ChatMessage::text("system", text));
    }
    if let Some(model_catalog_change_notice) = model_catalog_change_notice
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        messages.push(ChatMessage::text("system", model_catalog_change_notice));
    }
    if let Some(skill_updates_prefix) = skill_updates_prefix
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        messages.push(ChatMessage::text("system", skill_updates_prefix));
    }
    messages
}

pub(super) fn render_last_user_message_time_tip(
    session: &SessionSnapshot,
    now: chrono::DateTime<chrono::Utc>,
) -> Option<String> {
    let last_user_message_at = session.last_user_message_at?;
    let last_agent_returned_at = session.last_agent_returned_at?;
    let idle_seconds = (now - last_agent_returned_at).num_seconds().max(0);
    if idle_seconds < 5 * 60 {
        return None;
    }
    let elapsed_seconds = (now - last_user_message_at).num_seconds().max(0);
    let elapsed_hours = elapsed_seconds as f64 / 3600.0;
    Some(format!(
        "[System Tip: {:.1} hours since the last user message.]",
        elapsed_hours
    ))
}

pub(super) fn render_system_date_on_user_message(now: chrono::DateTime<chrono::Utc>) -> String {
    let local_now = now.with_timezone(&chrono::Local);
    format!(
        "[System Date: {}]",
        local_now.format("%Y-%m-%d %H:%M:%S %:z")
    )
}

pub(super) fn render_model_catalog_change_notice(
    notices: &[ModelCatalogChangeNotice],
    model_catalog: &str,
) -> Option<String> {
    if notices.is_empty() {
        return None;
    }
    let model_catalog = model_catalog.trim();
    if model_catalog.is_empty() {
        return Some(
            "[System Message: Available models changed since earlier in this session. Treat the current system prompt as authoritative for the latest model list.]"
                .to_string(),
        );
    }
    Some(format!(
        "[System Message: Available models changed since earlier in this session. The current model catalog for this run is authoritative.\nAvailable models:\n{}]",
        model_catalog
    ))
}

pub(super) fn build_previous_messages_for_turn_with_prompt(
    session_agent_messages: &[ChatMessage],
    pending_continue: Option<&PendingContinueState>,
    injected_messages: &[ChatMessage],
    next_user_message: Option<ChatMessage>,
    canonical_system_prompt: Option<&str>,
) -> (Vec<ChatMessage>, bool) {
    let base_messages = pending_continue
        .map(|pending| pending.resume_messages.clone())
        .unwrap_or_else(|| session_agent_messages.to_vec());
    let (mut previous_messages, rebuilt_system_prompt) = canonical_system_prompt
        .map(|prompt| rebuild_canonical_system_prompt(&base_messages, prompt))
        .unwrap_or_else(|| (base_messages, false));
    previous_messages.extend(injected_messages.iter().cloned());
    if let Some(next_user_message) = next_user_message {
        previous_messages.push(next_user_message);
    }
    (previous_messages, rebuilt_system_prompt)
}

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
