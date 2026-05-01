use std::{fs, path::Path};

use anyhow::{Context, Result};

use super::{WorkdirUpgrader, LATEST_WORKDIR_VERSION, WORKDIR_VERSION_0_11};
use crate::config::StellaclawConfig;

/// Migrate workdir-level files from .log/stellaclaw/ to .stellaclaw/.
pub struct StellaclawWorkdirDirectoryUpgrade;

impl WorkdirUpgrader for StellaclawWorkdirDirectoryUpgrade {
    fn from_version(&self) -> &'static str {
        WORKDIR_VERSION_0_11
    }

    fn to_version(&self) -> &'static str {
        LATEST_WORKDIR_VERSION
    }

    fn upgrade(&self, workdir: &Path, _config: &StellaclawConfig) -> Result<()> {
        let old_root = workdir.join(".log").join("stellaclaw");
        let new_root = workdir.join(".stellaclaw");
        fs::create_dir_all(&new_root).with_context(|| {
            format!("failed to create {}", new_root.display())
        })?;

        // Move individual files and directories from .log/stellaclaw/ → .stellaclaw/
        for entry_name in ["host.log", "conversation_ids.json", "cron_tasks.json", "channels"] {
            let old_path = old_root.join(entry_name);
            let new_path = new_root.join(entry_name);
            if !old_path.exists() {
                continue;
            }
            if !new_path.exists() {
                if let Err(error) = fs::rename(&old_path, &new_path) {
                    eprintln!(
                        "stellaclaw: warning: failed to migrate {}: {error}",
                        old_path.display()
                    );
                }
            } else if old_path.is_file() {
                // New file was already created (e.g. host.log opened before upgrade).
                // Prepend old content before the new data, then remove the old file.
                if let Ok(old_data) = fs::read(&old_path) {
                    if !old_data.is_empty() {
                        if let Ok(new_data) = fs::read(&new_path) {
                            let mut merged = old_data;
                            merged.extend_from_slice(&new_data);
                            let _ = fs::write(&new_path, merged);
                        }
                    }
                    let _ = fs::remove_file(&old_path);
                }
            }
        }

        // Clean up empty .log/stellaclaw/ and .log/ parents.
        let _ = fs::remove_dir(&old_root);
        let _ = fs::remove_dir(workdir.join(".log"));
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;
    use crate::config::StellaclawConfig;

    #[test]
    fn migrates_workdir_log_stellaclaw_to_stellaclaw() {
        let root = std::env::temp_dir().join(format!(
            "stellaclaw-workdir-upgrade-v0_11-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&root);

        let old_dir = root.join(".log").join("stellaclaw");
        fs::create_dir_all(old_dir.join("channels").join("web-main"))
            .expect("old channels dir should exist");
        fs::write(old_dir.join("host.log"), "log data")
            .expect("host.log should exist");
        fs::write(old_dir.join("conversation_ids.json"), "{}")
            .expect("conversation_ids.json should exist");
        fs::write(old_dir.join("cron_tasks.json"), "{}")
            .expect("cron_tasks.json should exist");
        fs::write(
            old_dir.join("channels").join("web-main").join("web_state.json"),
            "{}",
        )
        .expect("web_state.json should exist");

        let upgrade = StellaclawWorkdirDirectoryUpgrade;
        upgrade
            .upgrade(&root, &test_config())
            .expect("upgrade should succeed");

        let new_dir = root.join(".stellaclaw");
        assert!(new_dir.join("host.log").is_file());
        assert!(new_dir.join("conversation_ids.json").is_file());
        assert!(new_dir.join("cron_tasks.json").is_file());
        assert!(new_dir.join("channels").join("web-main").join("web_state.json").is_file());

        // Old directories should be cleaned up.
        assert!(!root.join(".log").exists());

        let _ = fs::remove_dir_all(root);
    }

    fn test_config() -> StellaclawConfig {
        use crate::config::*;
        use std::collections::BTreeMap;
        StellaclawConfig {
            version: LATEST_CONFIG_VERSION.to_string(),
            agent_server: AgentServerConfig::default(),
            default_profile: None,
            channels: Vec::new(),
            models: BTreeMap::new(),
            session_defaults: SessionDefaults::default(),
            sandbox: SandboxConfig::default(),
            skill_sync: Vec::new(),
            available_agent_models: Vec::new(),
        }
    }
}
