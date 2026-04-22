use super::WorkdirUpgrader;
use agent_frame::{
    CanonicalMessageScope, ChatMessage, canonicalize_message_multimodal_for_storage,
};
use anyhow::{Context, Result};
use serde_json::Value;
use std::fs;
use std::path::{Path, PathBuf};
use tracing::warn;

pub(super) struct Upgrade;

impl WorkdirUpgrader for Upgrade {
    fn from_version(&self) -> &'static str {
        "0.37"
    }

    fn to_version(&self) -> &'static str {
        "0.38"
    }

    fn upgrade(&self, workdir: &Path) -> Result<()> {
        rewrite_json_files_recursive(&workdir.join("sessions"), "session.json", |path, value| {
            rewrite_session_file(workdir, path, value)
        })?;
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
    rewrite: impl Fn(&Path, &mut Value) -> Result<bool> + Copy,
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
        let mut value: Value = match serde_json::from_str(&raw) {
            Ok(value) => value,
            Err(error) => {
                warn!(
                    log_stream = "upgrade",
                    kind = "workdir_upgrade_json_skipped",
                    path = %path.display(),
                    error = %error,
                    "skipping malformed JSON file during v0.38 workdir upgrade"
                );
                continue;
            }
        };
        if !rewrite(&path, &mut value)? {
            continue;
        }
        fs::write(&path, serde_json::to_string_pretty(&value)?)
            .with_context(|| format!("failed to write {}", path.display()))?;
    }

    Ok(())
}

fn rewrite_session_file(workdir: &Path, _path: &Path, value: &mut Value) -> Result<bool> {
    let Some(object) = value.as_object_mut() else {
        return Ok(false);
    };
    let Some(workspace_id) = object.get("workspace_id").and_then(Value::as_str) else {
        return Ok(false);
    };
    let workspace_root = workdir.join("workspaces").join(workspace_id).join("files");
    if !workspace_root.is_dir() {
        return Ok(false);
    }
    let Some(session_state) = object
        .get_mut("session_state")
        .and_then(Value::as_object_mut)
    else {
        return Ok(false);
    };

    let mut changed = false;
    changed |=
        rewrite_chat_message_array_field(session_state.get_mut("messages"), &workspace_root)?;
    changed |= rewrite_chat_message_array_field(
        session_state.get_mut("pending_messages"),
        &workspace_root,
    )?;
    changed |= rewrite_user_mailbox_pending_messages(
        session_state.get_mut("user_mailbox"),
        &workspace_root,
    )?;
    Ok(changed)
}

fn rewrite_snapshot_file(path: &Path, value: &mut Value) -> Result<bool> {
    let Some(object) = value.as_object_mut() else {
        return Ok(false);
    };
    let Some(session) = object.get_mut("session").and_then(Value::as_object_mut) else {
        return Ok(false);
    };
    let workspace_root = path
        .parent()
        .map(|parent| parent.join("workspace"))
        .unwrap_or_else(|| PathBuf::from("workspace"));
    if !workspace_root.is_dir() {
        return Ok(false);
    }

    let mut changed = false;
    changed |= rewrite_chat_message_array_field(session.get_mut("messages"), &workspace_root)?;
    changed |=
        rewrite_chat_message_array_field(session.get_mut("pending_messages"), &workspace_root)?;
    changed |=
        rewrite_user_mailbox_pending_messages(session.get_mut("user_mailbox"), &workspace_root)?;
    Ok(changed)
}

fn rewrite_user_mailbox_pending_messages(
    value: Option<&mut Value>,
    workspace_root: &Path,
) -> Result<bool> {
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
        changed |= rewrite_chat_message_value(pending_message, workspace_root)?;
    }
    Ok(changed)
}

fn rewrite_chat_message_array_field(
    value: Option<&mut Value>,
    workspace_root: &Path,
) -> Result<bool> {
    let Some(messages) = value.and_then(Value::as_array_mut) else {
        return Ok(false);
    };

    let mut changed = false;
    for message in messages {
        changed |= rewrite_chat_message_value(message, workspace_root)?;
    }
    Ok(changed)
}

fn rewrite_chat_message_value(value: &mut Value, workspace_root: &Path) -> Result<bool> {
    let message: ChatMessage = match serde_json::from_value(value.clone()) {
        Ok(message) => message,
        Err(_) => return Ok(false),
    };
    let rewritten = canonicalize_message_multimodal_for_storage(
        workspace_root,
        &message,
        CanonicalMessageScope::Legacy,
    )?;
    if rewritten == message {
        return Ok(false);
    }
    *value = serde_json::to_value(rewritten).context("failed to serialize upgraded message")?;
    Ok(true)
}
