use super::WorkdirUpgrader;
use anyhow::{Context, Result};
use std::fs;
use std::path::Path;

pub(super) struct Upgrade;

impl WorkdirUpgrader for Upgrade {
    fn from_version(&self) -> &'static str {
        "0.13"
    }

    fn to_version(&self) -> &'static str {
        "0.14"
    }

    fn upgrade(&self, workdir: &Path) -> Result<()> {
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
            let target_path = files_dir.join(crate::workspace::CONTEXT_ATTACHMENT_STORE_DIR_NAME);
            fs::create_dir_all(&target_path)
                .with_context(|| format!("failed to create {}", target_path.display()))?;
        }

        Ok(())
    }
}
