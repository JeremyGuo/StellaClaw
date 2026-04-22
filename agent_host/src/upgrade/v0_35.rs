use super::WorkdirUpgrader;
use crate::attachment_prep::normalize_inline_image_content_for_persistence;
use anyhow::{Context, Result};
use serde_json::Value;
use std::fs;
use std::path::Path;

pub(super) struct Upgrade;

impl WorkdirUpgrader for Upgrade {
    fn from_version(&self) -> &'static str {
        "0.34"
    }

    fn to_version(&self) -> &'static str {
        "0.35"
    }

    fn upgrade(&self, workdir: &Path) -> Result<()> {
        rewrite_json_files_recursive(
            &workdir.join("sessions"),
            "session.json",
            rewrite_session_file,
        )?;
        rewrite_json_files_recursive(
            &workdir.join("snapshots"),
            "snapshot.json",
            rewrite_snapshot_file,
        )?;
        Ok(())
    }
}

fn rewrite_json_files_recursive(
    root: &Path,
    file_name: &str,
    rewrite: fn(&mut Value) -> Result<bool>,
) -> Result<()> {
    if !root.is_dir() {
        return Ok(());
    }

    for entry in fs::read_dir(root).with_context(|| format!("failed to read {}", root.display()))? {
        let path = entry?.path();
        if path.is_dir() {
            rewrite_json_files_recursive(&path, file_name, rewrite)?;
            continue;
        }
        if path.file_name().and_then(|value| value.to_str()) != Some(file_name) {
            continue;
        }

        let raw = fs::read_to_string(&path)
            .with_context(|| format!("failed to read {}", path.display()))?;
        let mut value: Value = serde_json::from_str(&raw)
            .with_context(|| format!("failed to parse {}", path.display()))?;
        if !rewrite(&mut value)? {
            continue;
        }
        fs::write(&path, serde_json::to_string_pretty(&value)?)
            .with_context(|| format!("failed to write {}", path.display()))?;
    }

    Ok(())
}

fn rewrite_session_file(value: &mut Value) -> Result<bool> {
    let Some(object) = value.as_object_mut() else {
        return Ok(false);
    };
    let Some(session_state) = object
        .get_mut("session_state")
        .and_then(Value::as_object_mut)
    else {
        return Ok(false);
    };

    let mut changed = false;
    changed |= rewrite_chat_message_array_field(session_state.get_mut("messages"))?;
    changed |= rewrite_chat_message_array_field(session_state.get_mut("pending_messages"))?;
    changed |= rewrite_user_mailbox_pending_messages(session_state.get_mut("user_mailbox"))?;
    Ok(changed)
}

fn rewrite_snapshot_file(value: &mut Value) -> Result<bool> {
    let Some(object) = value.as_object_mut() else {
        return Ok(false);
    };
    let Some(session) = object.get_mut("session").and_then(Value::as_object_mut) else {
        return Ok(false);
    };

    let mut changed = false;
    changed |= rewrite_chat_message_array_field(session.get_mut("messages"))?;
    changed |= rewrite_chat_message_array_field(session.get_mut("pending_messages"))?;
    changed |= rewrite_user_mailbox_pending_messages(session.get_mut("user_mailbox"))?;
    Ok(changed)
}

fn rewrite_user_mailbox_pending_messages(value: Option<&mut Value>) -> Result<bool> {
    let Some(entries) = value.and_then(Value::as_array_mut) else {
        return Ok(false);
    };

    let mut changed = false;
    for entry in entries {
        let Some(object) = entry.as_object_mut() else {
            continue;
        };
        let Some(pending_message) = object.get_mut("pending_message") else {
            continue;
        };
        changed |= rewrite_chat_message_value(pending_message)?;
    }
    Ok(changed)
}

fn rewrite_chat_message_array_field(value: Option<&mut Value>) -> Result<bool> {
    let Some(messages) = value.and_then(Value::as_array_mut) else {
        return Ok(false);
    };

    let mut changed = false;
    for message in messages {
        changed |= rewrite_chat_message_value(message)?;
    }
    Ok(changed)
}

fn rewrite_chat_message_value(value: &mut Value) -> Result<bool> {
    let Some(object) = value.as_object_mut() else {
        return Ok(false);
    };
    let existing_content = object.get("content").cloned();
    let rewritten_content = normalize_inline_image_content_for_persistence(&existing_content)?;
    if rewritten_content == existing_content {
        return Ok(false);
    }

    match rewritten_content {
        Some(content) => {
            object.insert("content".to_string(), content);
        }
        None => {
            object.remove("content");
        }
    }
    Ok(true)
}
