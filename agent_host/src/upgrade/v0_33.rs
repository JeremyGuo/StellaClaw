use super::WorkdirUpgrader;
use crate::session::{SessionKind, session_conversation_dir_name, session_kind_dir_name};
use anyhow::{Context, Result, anyhow};
use serde_json::Value;
use std::fs;
use std::path::{Path, PathBuf};

pub(super) struct Upgrade;

impl WorkdirUpgrader for Upgrade {
    fn from_version(&self) -> &'static str {
        "0.32"
    }

    fn to_version(&self) -> &'static str {
        "0.33"
    }

    fn upgrade(&self, workdir: &Path) -> Result<()> {
        move_flat_sessions(&workdir.join("sessions"))
    }
}

fn move_flat_sessions(sessions_root: &Path) -> Result<()> {
    if !sessions_root.is_dir() {
        return Ok(());
    }

    let mut moves = Vec::new();
    for entry in fs::read_dir(sessions_root)
        .with_context(|| format!("failed to read {}", sessions_root.display()))?
    {
        let source_root = entry?.path();
        if !source_root.is_dir() {
            continue;
        }
        let session_path = source_root.join("session.json");
        if !session_path.is_file() {
            continue;
        }
        let target_root = target_root_for_session(sessions_root, &session_path)?;
        if target_root != source_root {
            moves.push((source_root, target_root));
        }
    }

    for (source_root, target_root) in moves {
        if target_root.exists() {
            if same_session_id(&source_root, &target_root)? {
                fs::remove_dir_all(&source_root)
                    .with_context(|| format!("failed to remove {}", source_root.display()))?;
                continue;
            }
            return Err(anyhow!(
                "session target {} already exists for source {}",
                target_root.display(),
                source_root.display()
            ));
        }
        let parent = target_root
            .parent()
            .ok_or_else(|| anyhow!("session target {} has no parent", target_root.display()))?;
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
        fs::rename(&source_root, &target_root).with_context(|| {
            format!(
                "failed to move session {} to {}",
                source_root.display(),
                target_root.display()
            )
        })?;
    }

    Ok(())
}

fn target_root_for_session(sessions_root: &Path, session_path: &Path) -> Result<PathBuf> {
    let raw = fs::read_to_string(session_path)
        .with_context(|| format!("failed to read {}", session_path.display()))?;
    let value: Value = serde_json::from_str(&raw)
        .with_context(|| format!("failed to parse {}", session_path.display()))?;
    let session_id = value
        .get("id")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("session {} is missing id", session_path.display()))?;
    let conversation_id = value
        .get("address")
        .and_then(|address| address.get("conversation_id"))
        .and_then(Value::as_str)
        .unwrap_or("_unknown");
    let kind = match value.get("kind").and_then(Value::as_str) {
        Some("background") => SessionKind::Background,
        _ => SessionKind::Foreground,
    };
    Ok(sessions_root
        .join(session_conversation_dir_name(conversation_id))
        .join(session_kind_dir_name(kind))
        .join(session_id))
}

fn same_session_id(source_root: &Path, target_root: &Path) -> Result<bool> {
    let source_id = read_session_id(&source_root.join("session.json"))?;
    let target_id = read_session_id(&target_root.join("session.json"))?;
    Ok(source_id == target_id)
}

fn read_session_id(path: &Path) -> Result<Option<String>> {
    if !path.is_file() {
        return Ok(None);
    }
    let raw =
        fs::read_to_string(path).with_context(|| format!("failed to read {}", path.display()))?;
    let value: Value = serde_json::from_str(&raw)
        .with_context(|| format!("failed to parse {}", path.display()))?;
    Ok(value
        .get("id")
        .and_then(Value::as_str)
        .map(ToOwned::to_owned))
}
