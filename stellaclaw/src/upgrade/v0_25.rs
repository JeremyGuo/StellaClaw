use std::{fs, path::Path};

use anyhow::{Context, Result};
use serde_json::Value;

use super::{WorkdirUpgrader, LATEST_WORKDIR_VERSION, WORKDIR_VERSION_0_25};
use crate::config::StellaclawConfig;

pub struct RemoveIdleTimeoutCompactUpgrade;

impl WorkdirUpgrader for RemoveIdleTimeoutCompactUpgrade {
    fn from_version(&self) -> &'static str {
        WORKDIR_VERSION_0_25
    }

    fn to_version(&self) -> &'static str {
        LATEST_WORKDIR_VERSION
    }

    fn upgrade(&self, workdir: &Path, _config: &StellaclawConfig) -> Result<()> {
        let services_root = workdir.join("services");
        if !services_root.is_dir() {
            return Ok(());
        }

        for entry in fs::read_dir(&services_root)
            .with_context(|| format!("failed to read {}", services_root.display()))?
        {
            let entry = entry
                .with_context(|| format!("failed to enumerate {}", services_root.display()))?;
            let conversation_dir = entry.path();
            if !conversation_dir.is_dir() {
                continue;
            }
            remove_runtime_config_idle_compact(&conversation_dir.join("runtime_config.json"))?;
        }
        Ok(())
    }
}

fn remove_runtime_config_idle_compact(path: &Path) -> Result<()> {
    if !path.is_file() {
        return Ok(());
    }
    let content =
        fs::read_to_string(path).with_context(|| format!("failed to read {}", path.display()))?;
    let mut value: Value = serde_json::from_str(&content)
        .with_context(|| format!("failed to parse {}", path.display()))?;
    let Some(object) = value.as_object_mut() else {
        return Ok(());
    };
    if object.remove("idle_timeout_compact_enabled").is_none() {
        return Ok(());
    }
    let encoded =
        serde_json::to_string_pretty(&value).context("failed to encode runtime config")?;
    fs::write(path, encoded).with_context(|| format!("failed to write {}", path.display()))
}
