use std::{fs, path::Path};

use anyhow::{Context, Result};

use super::{WorkdirUpgrader, WORKDIR_VERSION_0_10, WORKDIR_VERSION_0_11};
use crate::config::StellaclawConfig;

pub struct ChannelStateDirectoryUpgrade;

impl WorkdirUpgrader for ChannelStateDirectoryUpgrade {
    fn from_version(&self) -> &'static str {
        WORKDIR_VERSION_0_10
    }

    fn to_version(&self) -> &'static str {
        WORKDIR_VERSION_0_11
    }

    fn upgrade(&self, workdir: &Path, _config: &StellaclawConfig) -> Result<()> {
        let channels_root = workdir.join(".log").join("stellaclaw").join("channels");
        fs::create_dir_all(&channels_root)
            .with_context(|| format!("failed to create {}", channels_root.display()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    use crate::config::{
        AgentServerConfig, SandboxConfig, SessionDefaults, StellaclawConfig, LATEST_CONFIG_VERSION,
    };

    #[test]
    fn creates_channel_state_directory() {
        let root = std::env::temp_dir().join(format!(
            "stellaclaw-channel-state-upgrade-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).expect("temp workdir should exist");

        ChannelStateDirectoryUpgrade
            .upgrade(&root, &test_config())
            .expect("upgrade should create channel state root");

        assert!(root
            .join(".log")
            .join("stellaclaw")
            .join("channels")
            .is_dir());
        let _ = fs::remove_dir_all(root);
    }

    fn test_config() -> StellaclawConfig {
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
