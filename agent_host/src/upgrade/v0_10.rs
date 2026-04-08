use super::WorkdirUpgrader;
use anyhow::{Context, Result};
use std::fs;
use std::path::Path;

pub(super) struct Upgrade;

impl WorkdirUpgrader for Upgrade {
    fn from_version(&self) -> &'static str {
        "0.9"
    }

    fn to_version(&self) -> &'static str {
        "0.10"
    }

    fn upgrade(&self, workdir: &Path) -> Result<()> {
        let runtime_root = workdir.join("agent").join("runtime");
        if !runtime_root.exists() {
            return Ok(());
        }

        for entry in fs::read_dir(&runtime_root)
            .with_context(|| format!("failed to read {}", runtime_root.display()))?
        {
            let workspace_runtime = entry?.path();
            let agent_frame_root = workspace_runtime.join("agent_frame");
            let processes_dir = agent_frame_root.join("processes");
            if processes_dir.exists() {
                fs::remove_dir_all(&processes_dir)
                    .with_context(|| format!("failed to remove {}", processes_dir.display()))?;
            }

            let tool_workers_dir = agent_frame_root.join("tool_workers");
            if !tool_workers_dir.is_dir() {
                continue;
            }
            for worker_entry in fs::read_dir(&tool_workers_dir)
                .with_context(|| format!("failed to read {}", tool_workers_dir.display()))?
            {
                let path = worker_entry?.path();
                let Some(name) = path.file_name().and_then(|value| value.to_str()) else {
                    continue;
                };
                if !name.starts_with("exec-") {
                    continue;
                }
                if path.is_dir() {
                    fs::remove_dir_all(&path)
                        .with_context(|| format!("failed to remove {}", path.display()))?;
                } else {
                    fs::remove_file(&path)
                        .with_context(|| format!("failed to remove {}", path.display()))?;
                }
            }
        }
        Ok(())
    }
}
