use std::{fs, path::Path};

use anyhow::{Context, Result};

use super::{WorkdirUpgrader, WORKDIR_VERSION_0_20, WORKDIR_VERSION_0_21};
use crate::config::StellaclawConfig;

pub struct WebSeenStateUpgrade;

impl WorkdirUpgrader for WebSeenStateUpgrade {
    fn from_version(&self) -> &'static str {
        WORKDIR_VERSION_0_20
    }

    fn to_version(&self) -> &'static str {
        WORKDIR_VERSION_0_21
    }

    fn upgrade(&self, workdir: &Path, _config: &StellaclawConfig) -> Result<()> {
        let old_root = workdir.join(".stellaclaw").join("channels");
        if !old_root.exists() {
            return Ok(());
        }

        for entry in fs::read_dir(&old_root)
            .with_context(|| format!("failed to read {}", old_root.display()))?
        {
            let entry =
                entry.with_context(|| format!("failed to enumerate {}", old_root.display()))?;
            let channel_dir = entry.path();
            if !channel_dir.is_dir() {
                continue;
            }
            let old_path = channel_dir.join("web_state.json");
            if !old_path.is_file() {
                continue;
            }
            let Some(channel_id) = channel_dir.file_name().and_then(|name| name.to_str()) else {
                continue;
            };
            let new_path = workdir
                .join(".stellaclaw")
                .join("web")
                .join(channel_id)
                .join("seen_state.json");
            if !new_path.exists() {
                if let Some(parent) = new_path.parent() {
                    fs::create_dir_all(parent)
                        .with_context(|| format!("failed to create {}", parent.display()))?;
                }
                fs::copy(&old_path, &new_path).with_context(|| {
                    format!(
                        "failed to migrate {} to {}",
                        old_path.display(),
                        new_path.display()
                    )
                })?;
            }
            fs::remove_file(&old_path)
                .with_context(|| format!("failed to remove {}", old_path.display()))?;
        }

        let _ = fs::remove_dir(&old_root);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;

    #[test]
    fn migrates_web_seen_state_out_of_legacy_channel_dir() {
        let root = std::env::temp_dir().join(format!(
            "stellaclaw-web-seen-upgrade-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&root);
        let old = root
            .join(".stellaclaw")
            .join("channels")
            .join("web-main")
            .join("web_state.json");
        fs::create_dir_all(old.parent().unwrap()).unwrap();
        fs::write(&old, r#"{"seen":{"conversation:local__agent__foreground__main":{"last_seen_message_id":"msg_1","updated_at":"2026-05-17T00:00:00Z"}}}"#).unwrap();

        WebSeenStateUpgrade
            .upgrade(&root, &test_config())
            .expect("upgrade should migrate state");

        let new = root
            .join(".stellaclaw")
            .join("web")
            .join("web-main")
            .join("seen_state.json");
        assert!(new.is_file());
        assert!(!old.exists());
        assert!(fs::read_to_string(new).unwrap().contains("msg_1"));
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
            memory: crate::config::MemoryConfig::default(),
            sandbox: SandboxConfig::default(),
            skill_sync: Vec::new(),
            available_agent_models: Vec::new(),
        }
    }
}
