use super::WorkdirUpgrader;
use anyhow::{Context, Result};
use serde_json::Value;
use std::fs;
use std::path::Path;

pub(super) struct Upgrade;

impl WorkdirUpgrader for Upgrade {
    fn from_version(&self) -> &'static str {
        "0.19"
    }

    fn to_version(&self) -> &'static str {
        "0.20"
    }

    fn upgrade(&self, workdir: &Path) -> Result<()> {
        let runtime_root = workdir.join("agent").join("runtime");
        if !runtime_root.is_dir() {
            return Ok(());
        }

        for workspace_entry in fs::read_dir(&runtime_root)
            .with_context(|| format!("failed to read {}", runtime_root.display()))?
        {
            let agent_frame_dir = workspace_entry?.path().join("agent_frame");
            let processes_dir = agent_frame_dir.join("processes");
            if !processes_dir.is_dir() {
                backfill_exec_worker_jobs(&agent_frame_dir)?;
                continue;
            }

            for process_entry in fs::read_dir(&processes_dir)
                .with_context(|| format!("failed to read {}", processes_dir.display()))?
            {
                let path = process_entry?.path();
                if !path.is_file()
                    || path.extension().and_then(|value| value.to_str()) != Some("json")
                {
                    continue;
                }

                let raw = fs::read_to_string(&path)
                    .with_context(|| format!("failed to read {}", path.display()))?;
                let mut value: Value = serde_json::from_str(&raw)
                    .with_context(|| format!("failed to parse {}", path.display()))?;
                let Some(object) = value.as_object_mut() else {
                    continue;
                };
                if object.contains_key("remote") {
                    continue;
                }

                object.insert("remote".to_string(), Value::String("local".to_string()));
                fs::write(&path, serde_json::to_string_pretty(&value)?)
                    .with_context(|| format!("failed to write {}", path.display()))?;
            }

            backfill_exec_worker_jobs(&agent_frame_dir)?;
        }

        Ok(())
    }
}

fn backfill_exec_worker_jobs(agent_frame_dir: &Path) -> Result<()> {
    let tool_workers_dir = agent_frame_dir.join("tool_workers");
    if !tool_workers_dir.is_dir() {
        return Ok(());
    }

    for worker_entry in fs::read_dir(&tool_workers_dir)
        .with_context(|| format!("failed to read {}", tool_workers_dir.display()))?
    {
        let path = worker_entry?.path();
        if !path.is_file() || path.extension().and_then(|value| value.to_str()) != Some("json") {
            continue;
        }

        let raw = fs::read_to_string(&path)
            .with_context(|| format!("failed to read {}", path.display()))?;
        let mut value: Value = serde_json::from_str(&raw)
            .with_context(|| format!("failed to parse {}", path.display()))?;
        let Some(object) = value.as_object_mut() else {
            continue;
        };
        if object.get("kind").and_then(Value::as_str) != Some("exec")
            || object.contains_key("remote")
        {
            continue;
        }

        object.insert("remote".to_string(), Value::Null);
        fs::write(&path, serde_json::to_string_pretty(&value)?)
            .with_context(|| format!("failed to write {}", path.display()))?;
    }

    Ok(())
}
