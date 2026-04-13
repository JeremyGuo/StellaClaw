use super::WorkdirUpgrader;
use anyhow::{Context, Result};
use serde_json::Value;
use std::fs;
use std::path::Path;

pub(super) struct Upgrade;

impl WorkdirUpgrader for Upgrade {
    fn from_version(&self) -> &'static str {
        "0.20"
    }

    fn to_version(&self) -> &'static str {
        "0.21"
    }

    fn upgrade(&self, workdir: &Path) -> Result<()> {
        let sessions_root = workdir.join("sessions");
        if !sessions_root.is_dir() {
            return Ok(());
        }

        for session_entry in fs::read_dir(&sessions_root)
            .with_context(|| format!("failed to read {}", sessions_root.display()))?
        {
            let path = session_entry?.path().join("session.json");
            if !path.is_file() {
                continue;
            }

            let raw = fs::read_to_string(&path)
                .with_context(|| format!("failed to read {}", path.display()))?;
            let mut value: Value = serde_json::from_str(&raw)
                .with_context(|| format!("failed to parse {}", path.display()))?;
            let Some(session_object) = value.as_object_mut() else {
                continue;
            };
            let Some(session_state) = session_object
                .get_mut("session_state")
                .and_then(Value::as_object_mut)
            else {
                continue;
            };
            if session_state.contains_key("progress_message") {
                continue;
            }

            session_state.insert("progress_message".to_string(), Value::Null);
            fs::write(&path, serde_json::to_string_pretty(&value)?)
                .with_context(|| format!("failed to write {}", path.display()))?;
        }

        Ok(())
    }
}
