use std::{fs, path::Path};

use anyhow::{Context, Result};

use super::{WorkdirUpgrader, LATEST_WORKDIR_VERSION, WORKDIR_VERSION_0_8};
use crate::config::StellaclawConfig;

pub struct RuntimeCacheDirectoryUpgrade;

impl WorkdirUpgrader for RuntimeCacheDirectoryUpgrade {
    fn from_version(&self) -> &'static str {
        WORKDIR_VERSION_0_8
    }

    fn to_version(&self) -> &'static str {
        LATEST_WORKDIR_VERSION
    }

    fn upgrade(&self, workdir: &Path, _config: &StellaclawConfig) -> Result<()> {
        let cache_root = workdir.join("rundir").join("cache").join("conversations");
        fs::create_dir_all(&cache_root)
            .with_context(|| format!("failed to create {}", cache_root.display()))
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
    fn creates_runtime_conversation_cache_directory() {
        let root = std::env::temp_dir().join(format!(
            "stellaclaw-runtime-cache-upgrade-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).expect("temp workdir should exist");

        RuntimeCacheDirectoryUpgrade
            .upgrade(&root, &test_config())
            .expect("upgrade should create cache directory");

        assert!(root
            .join("rundir")
            .join("cache")
            .join("conversations")
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
