use std::{
    collections::hash_map::DefaultHasher,
    fs,
    hash::{Hash, Hasher},
    io::{BufRead, BufReader, Write},
    path::Path,
};

use anyhow::{Context, Result};
use serde_json::{json, Value};

use super::{WorkdirUpgrader, LATEST_WORKDIR_VERSION, WORKDIR_VERSION_0_19};
use crate::config::StellaclawConfig;

pub struct ReasoningSummaryPartsUpgrade;

impl WorkdirUpgrader for ReasoningSummaryPartsUpgrade {
    fn from_version(&self) -> &'static str {
        WORKDIR_VERSION_0_19
    }

    fn to_version(&self) -> &'static str {
        LATEST_WORKDIR_VERSION
    }

    fn upgrade(&self, workdir: &Path, _config: &StellaclawConfig) -> Result<()> {
        let conversations_root = workdir.join("conversations");
        if !conversations_root.exists() {
            return Ok(());
        }

        for entry in fs::read_dir(&conversations_root)
            .with_context(|| format!("failed to read {}", conversations_root.display()))?
        {
            let entry = entry
                .with_context(|| format!("failed to enumerate {}", conversations_root.display()))?;
            let conversation_root = entry.path();
            if conversation_root.is_dir() {
                migrate_conversation_logs(&conversation_root)?;
            }
        }
        Ok(())
    }
}

fn migrate_conversation_logs(conversation_root: &Path) -> Result<()> {
    let log_root = conversation_root.join(".stellaclaw").join("log");
    if !log_root.exists() {
        return Ok(());
    }
    for entry in
        fs::read_dir(&log_root).with_context(|| format!("failed to read {}", log_root.display()))?
    {
        let entry = entry.with_context(|| format!("failed to enumerate {}", log_root.display()))?;
        let session_root = entry.path();
        if session_root.is_dir() {
            migrate_session_log(&session_root)?;
        }
    }
    Ok(())
}

fn migrate_session_log(session_root: &Path) -> Result<()> {
    migrate_session_json(session_root, &session_root.join("session.json"))?;
    let all_messages_path = session_root.join("all_messages.jsonl");
    migrate_messages_jsonl(session_root, "all_messages", &all_messages_path)?;
    write_messages_index(
        &all_messages_path,
        &session_root.join("messages_index.json"),
    )?;
    migrate_messages_jsonl(
        session_root,
        "current_messages",
        &session_root.join("current_messages.jsonl"),
    )?;
    Ok(())
}

fn migrate_session_json(session_root: &Path, path: &Path) -> Result<()> {
    if !path.exists() {
        return Ok(());
    }
    let raw =
        fs::read_to_string(path).with_context(|| format!("failed to read {}", path.display()))?;
    let mut value: Value = serde_json::from_str(&raw)
        .with_context(|| format!("failed to parse {}", path.display()))?;
    let changed =
        migrate_message_array(session_root, "all_messages", value.get_mut("all_messages"))
            | migrate_message_array(
                session_root,
                "current_messages",
                value.get_mut("current_messages"),
            );
    if changed {
        fs::write(
            path,
            serde_json::to_string_pretty(&value)
                .context("failed to serialize migrated session state")?,
        )
        .with_context(|| format!("failed to write {}", path.display()))?;
    }
    Ok(())
}

fn migrate_messages_jsonl(session_root: &Path, scope: &str, path: &Path) -> Result<()> {
    if !path.exists() {
        return Ok(());
    }
    let file =
        fs::File::open(path).with_context(|| format!("failed to open {}", path.display()))?;
    let reader = BufReader::new(file);
    let mut changed = false;
    let mut lines = Vec::new();
    for (index, line) in reader.lines().enumerate() {
        let line = line.with_context(|| format!("failed to read line from {}", path.display()))?;
        if line.trim().is_empty() {
            lines.push(line);
            continue;
        }
        let mut message: Value = serde_json::from_str(&line)
            .with_context(|| format!("failed to parse JSONL message in {}", path.display()))?;
        changed |= migrate_message(&mut message, stable_message_id(session_root, scope, index));
        lines
            .push(serde_json::to_string(&message).context("failed to serialize migrated message")?);
    }
    if !changed {
        return Ok(());
    }
    let mut file =
        fs::File::create(path).with_context(|| format!("failed to create {}", path.display()))?;
    for line in lines {
        writeln!(file, "{line}").with_context(|| format!("failed to write {}", path.display()))?;
    }
    file.flush()
        .with_context(|| format!("failed to flush {}", path.display()))?;
    Ok(())
}

fn write_messages_index(path: &Path, index_path: &Path) -> Result<()> {
    if !path.exists() {
        return Ok(());
    }
    let file =
        fs::File::open(path).with_context(|| format!("failed to open {}", path.display()))?;
    let mut reader = BufReader::new(file);
    let mut byte_offset = 0u64;
    let mut index = serde_json::Map::new();
    let mut message_count = 0usize;
    let mut last_message_id = None;
    loop {
        let mut line = String::new();
        let bytes_read = reader
            .read_line(&mut line)
            .with_context(|| format!("failed to read {}", path.display()))?;
        if bytes_read == 0 {
            break;
        }
        let current_offset = byte_offset;
        byte_offset = byte_offset.saturating_add(bytes_read as u64);
        if line.trim().is_empty() {
            continue;
        }
        let value: Value = serde_json::from_str(&line)
            .with_context(|| format!("failed to parse JSONL message in {}", path.display()))?;
        if let Some(message_id) = value.get("message_id").and_then(Value::as_str) {
            if !message_id.is_empty() {
                index.insert(
                    message_id.to_string(),
                    json!({
                        "index": message_count,
                        "byte_offset": current_offset,
                    }),
                );
                last_message_id = Some(message_id.to_string());
            }
        }
        message_count = message_count.saturating_add(1);
    }
    fs::write(
        index_path,
        serde_json::to_string_pretty(&json!({
            "version": 1,
            "message_count": message_count,
            "last_message_id": last_message_id,
            "messages": index,
        }))
        .context("failed to serialize messages index")?,
    )
    .with_context(|| format!("failed to write {}", index_path.display()))?;
    Ok(())
}

fn migrate_message_array(session_root: &Path, array_name: &str, value: Option<&mut Value>) -> bool {
    let Some(Value::Array(messages)) = value else {
        return false;
    };
    let mut changed = false;
    for (index, message) in messages.iter_mut().enumerate() {
        changed |= migrate_message(message, stable_message_id(session_root, array_name, index));
    }
    changed
}

fn migrate_message(message: &mut Value, fallback_id: String) -> bool {
    let mut changed = ensure_message_id(message, fallback_id);
    let Some(Value::Array(items)) = message.get_mut("data") else {
        return changed;
    };
    for item in items {
        if item.get("type").and_then(Value::as_str) == Some("reasoning") {
            changed |= migrate_reasoning_item(item);
        }
    }
    changed
}

fn ensure_message_id(message: &mut Value, fallback_id: String) -> bool {
    match message.get("message_id").and_then(Value::as_str) {
        Some(id) if !id.is_empty() => false,
        _ => {
            if let Some(object) = message.as_object_mut() {
                if let Some(Value::String(id)) = object
                    .remove("id")
                    .filter(|id| id.as_str().map(|id| !id.is_empty()).unwrap_or(false))
                {
                    object.insert("message_id".to_string(), Value::String(id));
                } else {
                    object.insert("message_id".to_string(), Value::String(fallback_id));
                }
                true
            } else {
                false
            }
        }
    }
}

fn stable_message_id(path: &Path, scope: &str, index: usize) -> String {
    let mut hasher = DefaultHasher::new();
    path.display().to_string().hash(&mut hasher);
    scope.hash(&mut hasher);
    index.hash(&mut hasher);
    format!("msg_{index:020}_upgrade_{:016x}", hasher.finish())
}

fn migrate_reasoning_item(item: &mut Value) -> bool {
    let Some(payload) = item.get_mut("payload").and_then(Value::as_object_mut) else {
        return false;
    };
    match payload.get("codex_summary") {
        Some(Value::String(text)) if !text.is_empty() => {
            let text = text.clone();
            payload.insert("codex_summary".to_string(), json!([{ "text": text }]));
            true
        }
        Some(Value::String(_)) | Some(Value::Null) => {
            payload.remove("codex_summary");
            true
        }
        _ => false,
    }
}
