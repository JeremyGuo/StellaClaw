use std::{fs, path::Path};

use anyhow::{Context, Result};
use serde_json::Value;

use super::{WorkdirUpgrader, LATEST_WORKDIR_VERSION, WORKDIR_VERSION_0_7};
use crate::config::StellaclawConfig;

pub struct CronScriptFieldRenameUpgrade;

impl WorkdirUpgrader for CronScriptFieldRenameUpgrade {
    fn from_version(&self) -> &'static str {
        WORKDIR_VERSION_0_7
    }

    fn to_version(&self) -> &'static str {
        LATEST_WORKDIR_VERSION
    }

    fn upgrade(&self, workdir: &Path, _config: &StellaclawConfig) -> Result<()> {
        let path = workdir
            .join(".log")
            .join("stellaclaw")
            .join("cron_tasks.json");
        if !path.exists() {
            return Ok(());
        }

        let raw = fs::read_to_string(&path)
            .with_context(|| format!("failed to read {}", path.display()))?;
        let mut value: Value = serde_json::from_str(&raw)
            .with_context(|| format!("failed to parse {}", path.display()))?;
        if let Some(tasks) = value.get_mut("tasks").and_then(Value::as_object_mut) {
            for task in tasks.values_mut() {
                rename_field(task, "checker_command", "script_command");
                rename_field(task, "checker_timeout_seconds", "script_timeout_seconds");
                rename_field(task, "checker_cwd", "script_cwd");
            }
        }
        let raw = serde_json::to_string_pretty(&value)
            .context("failed to serialize upgraded cron task store")?;
        fs::write(&path, raw).with_context(|| format!("failed to write {}", path.display()))
    }
}

fn rename_field(value: &mut Value, old: &str, new: &str) {
    let Some(object) = value.as_object_mut() else {
        return;
    };
    let old_value = object.remove(old);
    if !object.contains_key(new) {
        if let Some(old_value) = old_value {
            object.insert(new.to_string(), old_value);
        }
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
    fn renames_legacy_cron_script_fields() {
        let root = std::env::temp_dir().join(format!(
            "stellaclaw-cron-script-field-upgrade-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&root);
        let dir = root.join(".log").join("stellaclaw");
        fs::create_dir_all(&dir).expect("cron store dir should exist");
        let path = dir.join("cron_tasks.json");
        fs::write(
            &path,
            r#"{
  "next_index": 2,
  "tasks": {
    "cron_0001": {
      "id": "cron_0001",
      "checker_command": "python3 check.py",
      "checker_timeout_seconds": 2,
      "checker_cwd": "checks"
    }
  }
}"#,
        )
        .expect("cron store should be written");

        CronScriptFieldRenameUpgrade
            .upgrade(&root, &empty_config())
            .expect("upgrade should succeed");

        let upgraded: Value = serde_json::from_str(
            &fs::read_to_string(&path).expect("cron store should be readable"),
        )
        .expect("cron store should parse");
        let task = upgraded
            .pointer("/tasks/cron_0001")
            .and_then(Value::as_object)
            .expect("task should exist");
        assert_eq!(
            task.get("script_command").and_then(Value::as_str),
            Some("python3 check.py")
        );
        assert_eq!(
            task.get("script_timeout_seconds").and_then(Value::as_i64),
            Some(2)
        );
        assert_eq!(
            task.get("script_cwd").and_then(Value::as_str),
            Some("checks")
        );
        assert!(!task.contains_key("checker_command"));
        assert!(!task.contains_key("checker_timeout_seconds"));
        assert!(!task.contains_key("checker_cwd"));

        fs::remove_dir_all(&root).expect("temp root should be removed");
    }

    fn empty_config() -> StellaclawConfig {
        StellaclawConfig {
            version: LATEST_CONFIG_VERSION.to_string(),
            agent_server: AgentServerConfig::default(),
            default_profile: None,
            models: BTreeMap::new(),
            session_defaults: SessionDefaults::default(),
            skill_sync: Vec::new(),
            sandbox: SandboxConfig::default(),
            channels: Vec::new(),
        }
    }
}
