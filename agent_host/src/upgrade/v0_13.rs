use super::WorkdirUpgrader;
use anyhow::{Context, Result};
use serde_json::Value;
use std::fs;
use std::path::Path;

pub(super) struct Upgrade;

impl WorkdirUpgrader for Upgrade {
    fn from_version(&self) -> &'static str {
        "0.12"
    }

    fn to_version(&self) -> &'static str {
        "0.13"
    }

    fn upgrade(&self, workdir: &Path) -> Result<()> {
        let sessions_root = workdir.join("sessions");
        if !sessions_root.is_dir() {
            return Ok(());
        }

        for entry in fs::read_dir(&sessions_root)
            .with_context(|| format!("failed to read {}", sessions_root.display()))?
        {
            let entry = entry?;
            let session_path = entry.path().join("session.json");
            if !session_path.is_file() {
                continue;
            }

            let raw = fs::read_to_string(&session_path)
                .with_context(|| format!("failed to read {}", session_path.display()))?;
            let mut value: Value = serde_json::from_str(&raw)
                .with_context(|| format!("failed to parse {}", session_path.display()))?;
            let Some(object) = value.as_object_mut() else {
                continue;
            };
            if object.contains_key("last_user_message_at") {
                continue;
            }

            let has_user_message =
                object
                    .get("history")
                    .and_then(Value::as_array)
                    .is_some_and(|history| {
                        history.iter().any(|message| {
                            message.get("role").and_then(Value::as_str) == Some("user")
                        })
                    });
            let inferred_last_user_message_at = if has_user_message {
                object
                    .get("last_agent_returned_at")
                    .cloned()
                    .unwrap_or(Value::Null)
            } else {
                Value::Null
            };
            object.insert(
                "last_user_message_at".to_string(),
                inferred_last_user_message_at,
            );

            fs::write(&session_path, serde_json::to_string_pretty(&value)?)
                .with_context(|| format!("failed to write {}", session_path.display()))?;
        }

        Ok(())
    }
}
