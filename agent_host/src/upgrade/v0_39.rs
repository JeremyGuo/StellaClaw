use super::WorkdirUpgrader;
use anyhow::{Context, Result};
use serde_json::{Value, json};
use std::fs;
use std::path::Path;

pub(super) struct Upgrade;

impl WorkdirUpgrader for Upgrade {
    fn from_version(&self) -> &'static str {
        "0.38"
    }

    fn to_version(&self) -> &'static str {
        "0.39"
    }

    fn upgrade(&self, workdir: &Path) -> Result<()> {
        backfill_remote_execution(&workdir.join("conversations"), "conversation.json")?;
        backfill_remote_execution(&workdir.join("snapshots"), "snapshot.json")?;
        Ok(())
    }
}

fn backfill_remote_execution(root: &Path, file_name: &str) -> Result<()> {
    if !root.is_dir() {
        return Ok(());
    }
    for entry in fs::read_dir(root).with_context(|| format!("failed to read {}", root.display()))? {
        let path = entry?.path();
        if path.is_dir() {
            backfill_remote_execution(&path, file_name)?;
            continue;
        }
        if path.file_name().and_then(|value| value.to_str()) != Some(file_name) {
            continue;
        }
        let raw = fs::read_to_string(&path)
            .with_context(|| format!("failed to read {}", path.display()))?;
        let mut value: Value = serde_json::from_str(&raw)
            .with_context(|| format!("failed to parse {}", path.display()))?;
        let Some(settings) = value.get_mut("settings").and_then(Value::as_object_mut) else {
            continue;
        };
        if settings.contains_key("remote_execution") {
            continue;
        }
        settings.insert("remote_execution".to_string(), json!(null));
        fs::write(&path, serde_json::to_string_pretty(&value)?)
            .with_context(|| format!("failed to write {}", path.display()))?;
    }
    Ok(())
}
