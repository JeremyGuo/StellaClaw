use super::*;

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
struct PersistedMemoryGroup {
    group: String,
    #[serde(default)]
    conclusions: Vec<String>,
    #[serde(default)]
    keywords: Vec<String>,
    #[serde(default)]
    rollouts: Vec<String>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
struct PersistedMemoryIndex {
    #[serde(default)]
    groups: Vec<PersistedMemoryGroup>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
struct PersistedMemorySummary {
    #[serde(default)]
    recent_groups: Vec<String>,
    #[serde(default)]
    recent_rollouts: Vec<String>,
    #[serde(default)]
    updated_at: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct PersistedRolloutSummary {
    rollout_id: String,
    conversation_id: String,
    created_at: String,
    source_message_count: usize,
    summary: String,
    #[serde(default)]
    old_summary: String,
    #[serde(default)]
    keywords: Vec<String>,
    important_refs: agent_frame::StructuredCompactionRefs,
    #[serde(default)]
    next_step: String,
}

pub(super) fn conversation_memory_root(session: &SessionSnapshot) -> PathBuf {
    session.root_dir.join("conversation_memory")
}

fn ensure_parent_dir(path: &Path) -> Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| anyhow!("path {} has no parent", path.display()))?;
    fs::create_dir_all(parent).with_context(|| format!("failed to create {}", parent.display()))
}

fn write_json_pretty<T: Serialize>(path: &Path, value: &T) -> Result<()> {
    ensure_parent_dir(path)?;
    let raw = serde_json::to_string_pretty(value)
        .with_context(|| format!("failed to serialize {}", path.display()))?;
    fs::write(path, raw).with_context(|| format!("failed to write {}", path.display()))
}

fn read_json_or_default<T>(path: &Path) -> Result<T>
where
    T: for<'de> Deserialize<'de> + Default,
{
    if !path.is_file() {
        return Ok(T::default());
    }
    let raw =
        fs::read_to_string(path).with_context(|| format!("failed to read {}", path.display()))?;
    serde_json::from_str(&raw).with_context(|| format!("failed to parse {}", path.display()))
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

fn persist_model_switch_artifact(
    workspace_root: &Path,
    batch_dir: &mut Option<PathBuf>,
    artifact_index: usize,
    item_type: &str,
    item: &Value,
) -> Result<String> {
    let kind_label = match item_type {
        "image_url" | "input_image" => "image",
        "file" | "input_file" => "file",
        "input_audio" => "audio",
        _ => "item",
    };
    let directory = if let Some(existing) = batch_dir {
        existing.clone()
    } else {
        let created = workspace_root
            .join(crate::workspace::CONTEXT_ATTACHMENT_STORE_DIR_NAME)
            .join(Uuid::new_v4().to_string());
        fs::create_dir_all(&created)
            .with_context(|| format!("failed to create {}", created.display()))?;
        *batch_dir = Some(created.clone());
        created
    };

    let binary_target = match item_type {
        "image_url" | "input_image" => {
            extract_model_switch_image_bytes(item_type, item).map(|(bytes, ext)| {
                (
                    bytes,
                    format!("history-{kind_label}-{artifact_index}.{ext}"),
                )
            })
        }
        "file" | "input_file" => {
            extract_model_switch_file_bytes(item_type, item).map(|(bytes, ext)| {
                (
                    bytes,
                    format!("history-{kind_label}-{artifact_index}.{ext}"),
                )
            })
        }
        "input_audio" => extract_model_switch_audio_bytes(item).map(|(bytes, ext)| {
            (
                bytes,
                format!("history-{kind_label}-{artifact_index}.{ext}"),
            )
        }),
        _ => None,
    };

    let target_path = if let Some((bytes, file_name)) = binary_target {
        let path = directory.join(file_name);
        fs::write(&path, bytes).with_context(|| format!("failed to write {}", path.display()))?;
        path
    } else {
        let path = directory.join(format!("history-{kind_label}-{artifact_index}.json"));
        write_json_pretty(&path, item)?;
        path
    };

    relative_attachment_path(workspace_root, &target_path)
}

fn extract_model_switch_image_bytes(item_type: &str, item: &Value) -> Option<(Vec<u8>, String)> {
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
    let extension = preferred_extension(None, media_type.as_deref(), "bin");
    Some((bytes, extension))
}

fn extract_model_switch_file_bytes(item_type: &str, item: &Value) -> Option<(Vec<u8>, String)> {
    let file_value = if item_type == "file" {
        item.get("file")
    } else {
        Some(item)
    }?;
    let filename_hint = file_value.get("filename").and_then(Value::as_str);
    let file_data = file_value.get("file_data").and_then(Value::as_str)?;
    if let Some((bytes, media_type)) = decode_data_url_payload(file_data) {
        let extension = preferred_extension(filename_hint, media_type.as_deref(), "bin");
        return Some((bytes, extension));
    }
    let bytes = decode_base64_payload(file_data)?;
    let extension = preferred_extension(filename_hint, None, "bin");
    Some((bytes, extension))
}

fn extract_model_switch_audio_bytes(item: &Value) -> Option<(Vec<u8>, String)> {
    let audio = item.get("input_audio").and_then(Value::as_object)?;
    let data = audio.get("data").and_then(Value::as_str)?;
    let format = audio.get("format").and_then(Value::as_str);
    let bytes = decode_base64_payload(data)?;
    let extension = format
        .and_then(sanitize_extension)
        .unwrap_or("bin")
        .to_string();
    Some((bytes, extension))
}

fn model_switch_placeholder_text(
    item_type: &str,
    capability_text: &str,
    relative_path: &str,
) -> String {
    match item_type {
        "image_url" | "input_image" => format!(
            "[Earlier image downgraded during model switch because the {capability_text} does not accept image input. Reference saved at {relative_path}. Inspect it with tools if needed.]"
        ),
        "file" | "input_file" => format!(
            "[Earlier file downgraded during model switch because the {capability_text} does not accept file input. Reference saved at {relative_path}. Inspect it with tools if needed.]"
        ),
        "input_audio" => format!(
            "[Earlier audio downgraded during model switch because the {capability_text} does not accept audio input. Reference saved at {relative_path}. Inspect it with tools if needed.]"
        ),
        _ => format!(
            "[Earlier multimodal content downgraded during model switch. Reference saved at {relative_path}. Inspect it with tools if needed.]"
        ),
    }
}

pub(super) fn downgrade_messages_for_model_switch(
    messages: &[ChatMessage],
    workspace_root: &Path,
    model: &ModelConfig,
    backend_supports_native_multimodal: bool,
) -> Result<Vec<ChatMessage>> {
    let allow_images = backend_supports_native_multimodal && model.supports_image_input();
    let allow_files =
        backend_supports_native_multimodal && model.has_capability(ModelCapability::Pdf);
    let allow_audio =
        backend_supports_native_multimodal && model.has_capability(ModelCapability::AudioIn);
    let capability_text = if backend_supports_native_multimodal {
        "selected model"
    } else {
        "selected backend/model combination"
    };

    let mut batch_dir = None;
    let mut artifact_index = 0usize;
    let mut rewritten_messages = Vec::with_capacity(messages.len());
    for message in messages {
        let Some(Value::Array(items)) = &message.content else {
            rewritten_messages.push(message.clone());
            continue;
        };
        let mut rewritten = message.clone();
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
            artifact_index = artifact_index.saturating_add(1);
            let relative_path = persist_model_switch_artifact(
                workspace_root,
                &mut batch_dir,
                artifact_index,
                item_type,
                item,
            )?;
            rewritten_items.push(json!({
                "type": "text",
                "text": model_switch_placeholder_text(item_type, capability_text, &relative_path),
            }));
            changed = true;
        }
        if changed {
            rewritten.content = Some(Value::Array(rewritten_items));
        }
        rewritten_messages.push(rewritten);
    }
    Ok(rewritten_messages)
}

fn merge_unique_strings(existing: &mut Vec<String>, incoming: impl IntoIterator<Item = String>) {
    for value in incoming {
        let trimmed = value.trim();
        if trimmed.is_empty() {
            continue;
        }
        if !existing.iter().any(|current| current == trimmed) {
            existing.push(trimmed.to_string());
        }
    }
}

fn classify_chat_message_kind(message: &ChatMessage) -> &'static str {
    if message.role == "tool" {
        "tool_result"
    } else if message.role == "assistant"
        && message
            .tool_calls
            .as_ref()
            .is_some_and(|calls| !calls.is_empty())
    {
        "tool_call"
    } else if message.role == "user" {
        "user_message"
    } else if message.role == "system" {
        "system_message"
    } else {
        "assistant_message"
    }
}

fn next_rollout_id() -> String {
    let suffix = Uuid::new_v4().simple().to_string();
    format!(
        "{}-{}",
        Utc::now().format("%Y-%m-%dT%H-%M-%S"),
        &suffix[..8]
    )
}

pub(super) fn persist_compaction_artifacts(
    session: &SessionSnapshot,
    report: &ContextCompactionReport,
) -> Result<Option<String>> {
    let Some(structured) = report.structured_output.as_ref() else {
        return Ok(None);
    };
    if report.compacted_messages.is_empty() {
        return Ok(None);
    }

    let memory_root = conversation_memory_root(session);
    let rollout_id = next_rollout_id();
    let rollout_dir = memory_root.join("rollouts").join(&rollout_id);
    fs::create_dir_all(&rollout_dir)
        .with_context(|| format!("failed to create {}", rollout_dir.display()))?;

    let created_at = Utc::now().to_rfc3339();
    let rollout_summary = PersistedRolloutSummary {
        rollout_id: rollout_id.clone(),
        conversation_id: session.address.conversation_id.clone(),
        created_at: created_at.clone(),
        source_message_count: report.compacted_messages.len(),
        summary: structured.new_summary.clone(),
        old_summary: structured.old_summary.clone(),
        keywords: structured.keywords.clone(),
        important_refs: structured.important_refs.clone(),
        next_step: structured.next_step.clone(),
    };
    write_json_pretty(&rollout_dir.join("rollout_summary.json"), &rollout_summary)?;

    let transcript_path = rollout_dir.join("rollout_transcript.jsonl");
    let mut transcript_lines = Vec::with_capacity(report.compacted_messages.len());
    for (index, message) in report.compacted_messages.iter().enumerate() {
        transcript_lines.push(serde_json::to_string(&json!({
            "event_id": index,
            "timestamp": created_at,
            "kind": classify_chat_message_kind(message),
            "role": message.role,
            "name": message.name,
            "tool_call_id": message.tool_call_id,
            "tool_calls": message.tool_calls,
            "content": message.content,
        }))?);
    }
    fs::write(&transcript_path, transcript_lines.join("\n"))
        .with_context(|| format!("failed to write {}", transcript_path.display()))?;

    let memory_path = memory_root.join("MEMORY.json");
    let mut memory_index: PersistedMemoryIndex = read_json_or_default(&memory_path)?;
    let rollout_summary_path = format!("rollouts/{}/rollout_summary.json", rollout_id);
    for hint in &structured.memory_hints {
        if hint.group.trim().is_empty() {
            continue;
        }
        let group = if let Some(existing) = memory_index
            .groups
            .iter_mut()
            .find(|group| group.group == hint.group)
        {
            existing
        } else {
            memory_index.groups.push(PersistedMemoryGroup {
                group: hint.group.clone(),
                ..PersistedMemoryGroup::default()
            });
            memory_index
                .groups
                .last_mut()
                .expect("group inserted for memory index")
        };
        merge_unique_strings(&mut group.conclusions, hint.conclusions.clone());
        merge_unique_strings(&mut group.keywords, structured.keywords.clone());
        merge_unique_strings(&mut group.rollouts, [rollout_summary_path.clone()]);
    }
    write_json_pretty(&memory_path, &memory_index)?;

    let memory_summary_path = memory_root.join("memory_summary.json");
    let mut memory_summary: PersistedMemorySummary = read_json_or_default(&memory_summary_path)?;
    memory_summary.updated_at = created_at;
    merge_unique_strings(
        &mut memory_summary.recent_groups,
        structured
            .memory_hints
            .iter()
            .map(|hint| hint.group.clone()),
    );
    merge_unique_strings(
        &mut memory_summary.recent_rollouts,
        [rollout_summary_path.clone()],
    );
    if memory_summary.recent_groups.len() > 12 {
        let drain_count = memory_summary.recent_groups.len() - 12;
        memory_summary.recent_groups.drain(0..drain_count);
    }
    if memory_summary.recent_rollouts.len() > 12 {
        let drain_count = memory_summary.recent_rollouts.len() - 12;
        memory_summary.recent_rollouts.drain(0..drain_count);
    }
    write_json_pretty(&memory_summary_path, &memory_summary)?;

    Ok(Some(rollout_id))
}

pub(super) fn persist_compaction_artifacts_from_event(
    session: &SessionSnapshot,
    structured_output: &StructuredCompactionOutput,
    compacted_messages: &[ChatMessage],
) -> Result<Option<String>> {
    let report = ContextCompactionReport {
        messages: Vec::new(),
        compacted_messages: compacted_messages.to_vec(),
        usage: TokenUsage::default(),
        compacted: true,
        estimated_tokens_before: 0,
        estimated_tokens_after: 0,
        token_limit: 0,
        structured_output: Some(structured_output.clone()),
    };
    persist_compaction_artifacts(session, &report)
}

fn lower_contains(haystack: &str, needle: &str) -> bool {
    haystack.to_lowercase().contains(&needle.to_lowercase())
}

pub(super) fn stable_content_version(content: &str) -> String {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    content.hash(&mut hasher);
    format!("{:016x}", hasher.finish())
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) struct SharedProfileUploadReport {
    pub(super) user_changed: bool,
    pub(super) identity_changed: bool,
}

impl SharedProfileUploadReport {
    pub(super) fn changed_any(self) -> bool {
        self.user_changed || self.identity_changed
    }
}

pub(super) fn sync_workspace_shared_profile_files(
    agent_workspace: &AgentWorkspace,
    workspace_root: &Path,
) -> Result<Vec<SharedProfileChangeNotice>> {
    let mut notices = Vec::new();
    if sync_shared_profile_file(
        &agent_workspace.user_md_path,
        &workspace_root.join("USER.md"),
    )? {
        notices.push(SharedProfileChangeNotice::UserUpdated);
    }
    if sync_shared_profile_file(
        &agent_workspace.identity_md_path,
        &workspace_root.join("IDENTITY.md"),
    )? {
        notices.push(SharedProfileChangeNotice::IdentityUpdated);
    }
    Ok(notices)
}

pub(super) fn ensure_workspace_partclaw_file(
    agent_workspace: &AgentWorkspace,
    workspace_root: &Path,
) -> Result<()> {
    let target_path = workspace_root.join("PARTCLAW.md");
    if target_path.exists() {
        return Ok(());
    }
    let source_path = agent_workspace.rundir.join("PARTCLAW.md");
    if source_path.is_file() {
        fs::copy(&source_path, &target_path).with_context(|| {
            format!(
                "failed to copy {} to {}",
                source_path.display(),
                target_path.display()
            )
        })?;
    } else {
        fs::write(&target_path, crate::bootstrap::default_partclaw_template())
            .with_context(|| format!("failed to write {}", target_path.display()))?;
    }
    Ok(())
}

pub(super) fn upload_workspace_shared_profile_files(
    agent_workspace: &AgentWorkspace,
    workspace_root: &Path,
) -> Result<SharedProfileUploadReport> {
    Ok(SharedProfileUploadReport {
        user_changed: sync_shared_profile_file(
            &workspace_root.join("USER.md"),
            &agent_workspace.user_md_path,
        )?,
        identity_changed: sync_shared_profile_file(
            &workspace_root.join("IDENTITY.md"),
            &agent_workspace.identity_md_path,
        )?,
    })
}

fn sync_shared_profile_file(source_path: &Path, target_path: &Path) -> Result<bool> {
    let source_bytes = fs::read(source_path)
        .with_context(|| format!("failed to read {}", source_path.display()))?;
    let changed = match fs::read(target_path) {
        Ok(existing) => existing != source_bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => true,
        Err(error) => {
            return Err(error).with_context(|| format!("failed to read {}", target_path.display()));
        }
    };
    if !changed {
        return Ok(false);
    }
    if let Some(parent) = target_path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }
    fs::write(target_path, &source_bytes)
        .with_context(|| format!("failed to write {}", target_path.display()))?;
    Ok(true)
}

pub(super) fn memory_search_files(
    session: &SessionSnapshot,
    query: &str,
    limit: usize,
) -> Result<Value> {
    let query = query.trim();
    if query.is_empty() {
        return Err(anyhow!("query must be a non-empty string"));
    }
    let memory_root = conversation_memory_root(session);
    let memory_summary: PersistedMemorySummary =
        read_json_or_default(&memory_root.join("memory_summary.json"))?;
    let memory_index: PersistedMemoryIndex =
        read_json_or_default(&memory_root.join("MEMORY.json"))?;

    let mut matches = Vec::new();
    let summary_blob = serde_json::to_string(&memory_summary).unwrap_or_default();
    if lower_contains(&summary_blob, query) {
        matches.push(json!({
            "layer": "memory_summary",
            "preview": summary_blob.chars().take(200).collect::<String>(),
            "recent_groups": memory_summary.recent_groups,
            "recent_rollouts": memory_summary.recent_rollouts,
        }));
    }
    for group in memory_index.groups {
        let group_blob = serde_json::to_string(&group).unwrap_or_default();
        if !lower_contains(&group_blob, query) {
            continue;
        }
        matches.push(json!({
            "layer": "memory",
            "group": group.group,
            "preview": group_blob.chars().take(200).collect::<String>(),
            "keywords": group.keywords,
            "rollouts": group.rollouts,
        }));
        if matches.len() >= limit {
            break;
        }
    }

    Ok(json!({
        "query": query,
        "matches": matches.into_iter().take(limit).collect::<Vec<_>>(),
    }))
}

pub(super) fn rollout_search_files(
    session: &SessionSnapshot,
    query: &str,
    rollout_id: Option<&str>,
    kinds: &[String],
    limit: usize,
) -> Result<Value> {
    let query = query.trim();
    if query.is_empty() {
        return Err(anyhow!("query must be a non-empty string"));
    }
    let allowed_kinds = kinds
        .iter()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .collect::<Vec<_>>();
    let memory_root = conversation_memory_root(session);
    let rollouts_root = memory_root.join("rollouts");
    if !rollouts_root.is_dir() {
        return Ok(json!({ "query": query, "matches": [] }));
    }

    let mut rollout_ids = if let Some(rollout_id) = rollout_id {
        vec![rollout_id.to_string()]
    } else {
        fs::read_dir(&rollouts_root)
            .with_context(|| format!("failed to read {}", rollouts_root.display()))?
            .filter_map(|entry| entry.ok())
            .filter_map(|entry| {
                entry
                    .file_type()
                    .ok()
                    .filter(|file_type| file_type.is_dir())
                    .map(|_| entry.file_name().to_string_lossy().to_string())
            })
            .collect::<Vec<_>>()
    };
    rollout_ids.sort();
    rollout_ids.reverse();

    let mut matches = Vec::new();
    for current_rollout_id in rollout_ids {
        let transcript_path = rollouts_root
            .join(&current_rollout_id)
            .join("rollout_transcript.jsonl");
        let events = parse_transcript_events(&transcript_path)?;
        for event in events {
            let kind = transcript_event_kind(&event);
            if !allowed_kinds.is_empty()
                && !allowed_kinds.iter().any(|candidate| candidate == &kind)
            {
                continue;
            }
            let event_blob = serde_json::to_string(&event).unwrap_or_default();
            if !lower_contains(&event_blob, query) {
                continue;
            }
            matches.push(json!({
                "rollout_id": current_rollout_id,
                "event_id": event.get("event_id").and_then(Value::as_u64).unwrap_or(0),
                "timestamp": event.get("timestamp").and_then(Value::as_str).unwrap_or(""),
                "kind": kind,
                "preview": transcript_event_preview(&event),
            }));
            if matches.len() >= limit {
                return Ok(json!({
                    "query": query,
                    "matches": matches,
                    "truncated": true,
                }));
            }
        }
    }

    Ok(json!({
        "query": query,
        "matches": matches,
        "truncated": false,
    }))
}

pub(super) fn rollout_read_file(
    session: &SessionSnapshot,
    rollout_id: &str,
    anchor_event_id: usize,
    mode: Option<&str>,
    before: usize,
    after: usize,
) -> Result<Value> {
    let transcript_path = conversation_memory_root(session)
        .join("rollouts")
        .join(rollout_id)
        .join("rollout_transcript.jsonl");
    let events = parse_transcript_events(&transcript_path)?;
    let anchor_index = events
        .iter()
        .position(|event| {
            event.get("event_id").and_then(Value::as_u64) == Some(anchor_event_id as u64)
        })
        .ok_or_else(|| {
            anyhow!(
                "anchor_event_id {} not found in rollout {}",
                anchor_event_id,
                rollout_id
            )
        })?;

    let mode = mode.unwrap_or("turn_segment").to_string();
    let (start, end) = if mode == "window" {
        (
            anchor_index.saturating_sub(before),
            (anchor_index + after + 1).min(events.len()),
        )
    } else {
        let mut start = anchor_index;
        while start > 0 {
            let previous_kind = transcript_event_kind(&events[start - 1]);
            if previous_kind == "user_message" {
                start -= 1;
                break;
            }
            start -= 1;
        }
        let mut end = anchor_index + 1;
        while end < events.len() {
            if end > anchor_index && transcript_event_kind(&events[end]) == "user_message" {
                break;
            }
            end += 1;
        }
        (start, end)
    };

    Ok(json!({
        "rollout_id": rollout_id,
        "anchor_event_id": anchor_event_id,
        "mode": mode,
        "events": events[start..end].to_vec(),
        "has_more_before": start > 0,
        "has_more_after": end < events.len(),
    }))
}

fn parse_transcript_events(transcript_path: &Path) -> Result<Vec<Value>> {
    if !transcript_path.is_file() {
        return Ok(Vec::new());
    }
    let raw = fs::read_to_string(transcript_path)
        .with_context(|| format!("failed to read {}", transcript_path.display()))?;
    raw.lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| {
            serde_json::from_str::<Value>(line).with_context(|| {
                format!(
                    "failed to parse transcript line in {}",
                    transcript_path.display()
                )
            })
        })
        .collect()
}

fn transcript_event_kind(event: &Value) -> String {
    event
        .get("kind")
        .and_then(Value::as_str)
        .map(ToOwned::to_owned)
        .or_else(|| {
            let role = event.get("role").and_then(Value::as_str)?;
            Some(match role {
                "tool" => "tool_result".to_string(),
                "user" => "user_message".to_string(),
                "system" => "system_message".to_string(),
                _ => "assistant_message".to_string(),
            })
        })
        .unwrap_or_else(|| "assistant_message".to_string())
}

fn transcript_event_preview(event: &Value) -> String {
    if let Some(content) = event.get("content") {
        if let Some(text) = content.as_str() {
            let trimmed = text.trim();
            if !trimmed.is_empty() {
                return trimmed.chars().take(180).collect();
            }
        }
        if !content.is_null() {
            return serde_json::to_string(content)
                .unwrap_or_default()
                .chars()
                .take(180)
                .collect();
        }
    }
    format!("[{}]", transcript_event_kind(event))
}
