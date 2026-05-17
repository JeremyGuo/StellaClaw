use std::{fs, path::Path};

use anyhow::{Context, Result};
use serde_json::Value;

use super::{WorkdirUpgrader, LATEST_WORKDIR_VERSION, WORKDIR_VERSION_0_21};
use crate::config::StellaclawConfig;

pub struct RemoveStatusServiceUpgrade;

impl WorkdirUpgrader for RemoveStatusServiceUpgrade {
    fn from_version(&self) -> &'static str {
        WORKDIR_VERSION_0_21
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
            remove_status_from_manifest(&conversation_dir.join("manifest.json"))?;
            remove_status_storage(&conversation_dir)?;
        }
        Ok(())
    }
}

fn remove_status_from_manifest(path: &Path) -> Result<()> {
    if !path.is_file() {
        return Ok(());
    }
    let content =
        fs::read_to_string(path).with_context(|| format!("failed to read {}", path.display()))?;
    let mut manifest: Value = serde_json::from_str(&content)
        .with_context(|| format!("failed to parse {}", path.display()))?;
    let Some(services) = manifest.get_mut("services").and_then(Value::as_array_mut) else {
        return Ok(());
    };
    let before = services.len();
    services.retain(|entry| {
        entry
            .get("kind")
            .and_then(|kind| kind.get("type"))
            .and_then(Value::as_str)
            != Some("status")
            && entry
                .get("addr")
                .and_then(|addr| addr.get("path"))
                .and_then(Value::as_array)
                .is_none_or(|path| {
                    path.iter()
                        .map(Value::as_str)
                        .collect::<Option<Vec<_>>>()
                        .is_none_or(|segments| segments != ["status"])
                })
    });
    if services.len() == before {
        return Ok(());
    }
    let encoded =
        serde_json::to_string_pretty(&manifest).context("failed to encode service manifest")?;
    fs::write(path, encoded).with_context(|| format!("failed to write {}", path.display()))
}

fn remove_status_storage(conversation_dir: &Path) -> Result<()> {
    let status_dir = conversation_dir.join("local__status");
    if status_dir.exists() {
        fs::remove_dir_all(&status_dir)
            .with_context(|| format!("failed to remove {}", status_dir.display()))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn removes_status_service_from_manifest() {
        let root = std::env::temp_dir().join(format!(
            "stellaclaw-remove-status-service-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&root);
        let conversation_dir = root.join("services").join("web-main-000001");
        fs::create_dir_all(conversation_dir.join("local__status")).unwrap();
        fs::write(
            conversation_dir
                .join("local__status")
                .join("service_state.json"),
            "{}",
        )
        .unwrap();
        fs::write(
            conversation_dir.join("manifest.json"),
            r#"{
  "version": 1,
  "services": [
    {
      "addr": {"scope": "local", "path": ["status"]},
      "kind": {"type": "status"},
      "storage": "local__status"
    },
    {
      "addr": {"scope": "local", "path": ["channel", "main"]},
      "kind": {"type": "channel"},
      "storage": "local__channel__main"
    }
  ],
  "next_background_id": 1,
  "next_subagent_id": 1
}"#,
        )
        .unwrap();

        RemoveStatusServiceUpgrade
            .upgrade(&root, &test_config())
            .expect("upgrade removes status");

        let manifest = fs::read_to_string(conversation_dir.join("manifest.json")).unwrap();
        assert!(!manifest.contains(r#""type": "status""#));
        assert!(manifest.contains(r#""type": "channel""#));
        assert!(!conversation_dir.join("local__status").exists());
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
