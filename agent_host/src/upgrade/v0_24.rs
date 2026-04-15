use super::WorkdirUpgrader;
use anyhow::{Context, Result};
use serde_json::Value;
use std::fs;
use std::path::Path;

pub(super) struct Upgrade;

impl WorkdirUpgrader for Upgrade {
    fn from_version(&self) -> &'static str {
        "0.23"
    }

    fn to_version(&self) -> &'static str {
        "0.24"
    }

    fn upgrade(&self, workdir: &Path) -> Result<()> {
        rewrite_json_files(&workdir.join("conversations"), "conversation.json")?;
        rewrite_json_files(&workdir.join("snapshots"), "metadata.json")?;
        rewrite_json_files(&workdir.join("snapshots"), "snapshot.json")?;
        rewrite_json_files(&workdir.join("sessions"), "session.json")?;
        rewrite_json_file_if_exists(&workdir.join("cron").join("tasks.json"))?;
        rewrite_subagent_states(&workdir.join("agent").join("runtime"))?;
        Ok(())
    }
}

fn rewrite_json_files(root: &Path, file_name: &str) -> Result<()> {
    if !root.is_dir() {
        return Ok(());
    }

    for entry in fs::read_dir(root).with_context(|| format!("failed to read {}", root.display()))? {
        let path = entry?.path().join(file_name);
        rewrite_json_file_if_exists(&path)?;
    }

    Ok(())
}

fn rewrite_subagent_states(runtime_root: &Path) -> Result<()> {
    if !runtime_root.is_dir() {
        return Ok(());
    }

    for workspace_entry in fs::read_dir(runtime_root)
        .with_context(|| format!("failed to read {}", runtime_root.display()))?
    {
        let subagents_dir = workspace_entry?
            .path()
            .join("agent_frame")
            .join("subagents");
        if !subagents_dir.is_dir() {
            continue;
        }
        for subagent_entry in fs::read_dir(&subagents_dir)
            .with_context(|| format!("failed to read {}", subagents_dir.display()))?
        {
            let path = subagent_entry?.path();
            if path.extension().and_then(|value| value.to_str()) == Some("json") {
                rewrite_json_file_if_exists(&path)?;
            }
        }
    }

    Ok(())
}

fn rewrite_json_file_if_exists(path: &Path) -> Result<()> {
    if !path.is_file() {
        return Ok(());
    }

    let raw =
        fs::read_to_string(path).with_context(|| format!("failed to read {}", path.display()))?;
    let mut value: Value = serde_json::from_str(&raw)
        .with_context(|| format!("failed to parse {}", path.display()))?;
    normalize_zgent_state(&mut value);
    let updated = serde_json::to_string_pretty(&value)
        .with_context(|| format!("failed to serialize {}", path.display()))?;
    fs::write(path, updated).with_context(|| format!("failed to write {}", path.display()))
}

fn normalize_zgent_state(value: &mut Value) {
    match value {
        Value::Object(object) => {
            if object
                .get("agent_backend")
                .and_then(Value::as_str)
                .is_some_and(|backend| backend == "zgent")
            {
                object.insert(
                    "agent_backend".to_string(),
                    Value::String("agent_frame".to_string()),
                );
            }
            object.remove("zgent_native");
            for value in object.values_mut() {
                normalize_zgent_state(value);
            }
        }
        Value::Array(items) => {
            for value in items {
                normalize_zgent_state(value);
            }
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::{normalize_zgent_state, rewrite_json_file_if_exists};
    use serde_json::json;
    use tempfile::TempDir;

    #[test]
    fn normalizes_legacy_zgent_backend_and_removes_native_state() {
        let mut value = json!({
            "settings": {
                "agent_backend": "zgent"
            },
            "zgent_native": {
                "remote_session_id": "old"
            },
            "nested": [{
                "agent_backend": "zgent",
                "zgent_native": {}
            }]
        });

        normalize_zgent_state(&mut value);

        assert_eq!(value["settings"]["agent_backend"], "agent_frame");
        assert!(value.get("zgent_native").is_none());
        assert_eq!(value["nested"][0]["agent_backend"], "agent_frame");
        assert!(value["nested"][0].get("zgent_native").is_none());
    }

    #[test]
    fn rewrites_json_file_in_place() {
        let temp_dir = TempDir::new().unwrap();
        let path = temp_dir.path().join("session.json");
        std::fs::write(
            &path,
            serde_json::to_string_pretty(&json!({
                "agent_backend": "zgent",
                "zgent_native": {"remote_session_id": "old"}
            }))
            .unwrap(),
        )
        .unwrap();

        rewrite_json_file_if_exists(&path).unwrap();
        let value: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
        assert_eq!(value["agent_backend"], "agent_frame");
        assert!(value.get("zgent_native").is_none());
    }
}
