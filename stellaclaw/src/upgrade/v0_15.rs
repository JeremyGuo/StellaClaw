use std::{fs, path::Path};

use anyhow::{Context, Result};
use chrono::Utc;
use serde::Serialize;

use super::{WorkdirUpgrader, WORKDIR_VERSION_0_15, WORKDIR_VERSION_0_16};
use crate::config::StellaclawConfig;

pub struct MemoryV1DirectoryUpgrade;

impl WorkdirUpgrader for MemoryV1DirectoryUpgrade {
    fn from_version(&self) -> &'static str {
        WORKDIR_VERSION_0_15
    }

    fn to_version(&self) -> &'static str {
        WORKDIR_VERSION_0_16
    }

    fn upgrade(&self, workdir: &Path, _config: &StellaclawConfig) -> Result<()> {
        let user_memory = workdir.join("rundir").join("memory_v1").join("user");
        create_dir(&user_memory)?;
        ensure_user_compaction_status(&user_memory)?;
        create_dir(workdir.join("rundir").join("memory_v1").join("public"))?;

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
            create_dir(
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

fn create_dir(path: impl AsRef<Path>) -> Result<()> {
    let path = path.as_ref();
    fs::create_dir_all(path).with_context(|| format!("failed to create {}", path.display()))
}

fn ensure_user_compaction_status(user_memory: &Path) -> Result<()> {
    let path = user_memory.join("compaction.json");
    if path.exists() {
        return Ok(());
    }
    let status = UserMemoryCompactionStatus {
        state: "idle",
        attempts: 0,
        last_error: None,
        next_retry_at: None,
        last_input_hash: "",
        last_output_hash: "",
        threshold_override_bytes: None,
        last_soft_compaction_at: None,
        updated_at: Some(Utc::now().to_rfc3339()),
    };
    fs::write(&path, serde_json::to_string_pretty(&status)?)
        .with_context(|| format!("failed to write {}", path.display()))
}

#[derive(Serialize)]
struct UserMemoryCompactionStatus<'a> {
    state: &'a str,
    attempts: u32,
    last_error: Option<&'a str>,
    next_retry_at: Option<&'a str>,
    last_input_hash: &'a str,
    last_output_hash: &'a str,
    threshold_override_bytes: Option<u64>,
    last_soft_compaction_at: Option<&'a str>,
    updated_at: Option<String>,
}
