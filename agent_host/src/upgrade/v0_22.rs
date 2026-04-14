use super::WorkdirUpgrader;
use anyhow::{Context, Result};
use serde_json::{Value, json};
use std::fs;
use std::path::Path;

pub(super) struct Upgrade;

impl WorkdirUpgrader for Upgrade {
    fn from_version(&self) -> &'static str {
        "0.21"
    }

    fn to_version(&self) -> &'static str {
        "0.22"
    }

    fn upgrade(&self, workdir: &Path) -> Result<()> {
        backfill_remote_workpaths(&workdir.join("conversations"), "conversation.json")?;
        backfill_remote_workpaths(&workdir.join("snapshots"), "snapshot.json")?;
        Ok(())
    }
}

fn backfill_remote_workpaths(root: &Path, file_name: &str) -> Result<()> {
    if !root.is_dir() {
        return Ok(());
    }

    for entry in fs::read_dir(root).with_context(|| format!("failed to read {}", root.display()))? {
        let path = entry?.path().join(file_name);
        if !path.is_file() {
            continue;
        }

        let raw = fs::read_to_string(&path)
            .with_context(|| format!("failed to read {}", path.display()))?;
        let mut value: Value = serde_json::from_str(&raw)
            .with_context(|| format!("failed to parse {}", path.display()))?;
        let Some(object) = value.as_object_mut() else {
            continue;
        };
        let settings = object.entry("settings").or_insert_with(|| json!({}));
        let Some(settings) = settings.as_object_mut() else {
            continue;
        };
        if settings.contains_key("remote_workpaths") {
            continue;
        }

        settings.insert("remote_workpaths".to_string(), json!([]));
        fs::write(&path, serde_json::to_string_pretty(&value)?)
            .with_context(|| format!("failed to write {}", path.display()))?;
    }

    Ok(())
}
