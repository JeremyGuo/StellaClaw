use std::{
    fs::{self, OpenOptions},
    path::Path,
};

use anyhow::{Context, Result};

use super::{WorkdirUpgrader, WORKDIR_VERSION_0_16, WORKDIR_VERSION_0_17};
use crate::config::StellaclawConfig;

pub struct MemoryV1UsageLogUpgrade;

impl WorkdirUpgrader for MemoryV1UsageLogUpgrade {
    fn from_version(&self) -> &'static str {
        WORKDIR_VERSION_0_16
    }

    fn to_version(&self) -> &'static str {
        WORKDIR_VERSION_0_17
    }

    fn upgrade(&self, workdir: &Path, _config: &StellaclawConfig) -> Result<()> {
        ensure_usage_log(workdir.join("rundir").join("memory_v1").join("user"))?;
        ensure_usage_log(workdir.join("rundir").join("memory_v1").join("public"))?;

        let conversations_root = workdir.join("conversations");
        if !conversations_root.exists() {
            return Ok(());
        }
        for entry in fs::read_dir(&conversations_root)
            .with_context(|| format!("failed to read {}", conversations_root.display()))?
        {
            let entry = entry
                .with_context(|| format!("failed to enumerate {}", conversations_root.display()))?;
            if !entry.path().is_dir() {
                continue;
            }
            ensure_usage_log(
                entry
                    .path()
                    .join(".stellaclaw")
                    .join("memory_v1")
                    .join("conversation"),
            )?;
        }
        Ok(())
    }
}

fn ensure_usage_log(scope_dir: impl AsRef<Path>) -> Result<()> {
    let scope_dir = scope_dir.as_ref();
    fs::create_dir_all(scope_dir)
        .with_context(|| format!("failed to create {}", scope_dir.display()))?;
    let path = scope_dir.join("usage.jsonl");
    OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .with_context(|| format!("failed to create {}", path.display()))?;
    Ok(())
}
