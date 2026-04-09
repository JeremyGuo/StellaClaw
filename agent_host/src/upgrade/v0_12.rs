use super::WorkdirUpgrader;
use anyhow::{Context, Result};
use std::fs;
use std::path::Path;

pub(super) struct Upgrade;

impl WorkdirUpgrader for Upgrade {
    fn from_version(&self) -> &'static str {
        "0.11"
    }

    fn to_version(&self) -> &'static str {
        "0.12"
    }

    fn upgrade(&self, workdir: &Path) -> Result<()> {
        let template_path = workdir.join("rundir").join("PARTCLAW.md");
        if let Some(parent) = template_path.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("failed to create {}", parent.display()))?;
        }
        if !template_path.exists() {
            fs::write(&template_path, crate::bootstrap::default_partclaw_template())
                .with_context(|| format!("failed to write {}", template_path.display()))?;
        }

        let workspaces_root = workdir.join("workspaces");
        if !workspaces_root.is_dir() {
            return Ok(());
        }

        for entry in fs::read_dir(&workspaces_root)
            .with_context(|| format!("failed to read {}", workspaces_root.display()))?
        {
            let entry = entry?;
            let files_dir = entry.path().join("files");
            if !files_dir.is_dir() {
                continue;
            }
            let target_path = files_dir.join("PARTCLAW.md");
            if target_path.exists() {
                continue;
            }
            fs::write(&target_path, crate::bootstrap::default_partclaw_template())
                .with_context(|| format!("failed to write {}", target_path.display()))?;
        }

        Ok(())
    }
}
