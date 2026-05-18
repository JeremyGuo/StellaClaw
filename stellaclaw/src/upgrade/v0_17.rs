use std::{
    fs,
    io::{BufRead, BufReader, Write},
    path::Path,
};

use anyhow::{Context, Result};
use serde_json::{json, Value};

use super::{WorkdirUpgrader, WORKDIR_VERSION_0_17, WORKDIR_VERSION_0_18};
use crate::config::StellaclawConfig;

pub struct ToolResultStructuredContentUpgrade;

impl WorkdirUpgrader for ToolResultStructuredContentUpgrade {
    fn from_version(&self) -> &'static str {
        WORKDIR_VERSION_0_17
    }

    fn to_version(&self) -> &'static str {
        WORKDIR_VERSION_0_18
    }

    fn upgrade(&self, workdir: &Path, _config: &StellaclawConfig) -> Result<()> {
        let conversations_root = workdir.join("conversations");
        if !conversations_root.exists() {
            return Ok(());
        }

        for entry in fs::read_dir(&conversations_root)
            .with_context(|| format!("failed to read {}", conversations_root.display()))?
        {
            let entry = entry
                .with_context(|| format!("failed to enumerate {}", conversations_root.display()))?;
            let conversation_root = entry.path();
            if !conversation_root.is_dir() {
                continue;
            }
            migrate_conversation_logs(&conversation_root)?;
        }
        Ok(())
    }
}

fn migrate_conversation_logs(conversation_root: &Path) -> Result<()> {
    let log_root = conversation_root.join(".stellaclaw").join("log");
    if !log_root.exists() {
        return Ok(());
    }
    for entry in
        fs::read_dir(&log_root).with_context(|| format!("failed to read {}", log_root.display()))?
    {
        let entry = entry.with_context(|| format!("failed to enumerate {}", log_root.display()))?;
        let session_root = entry.path();
        if !session_root.is_dir() {
            continue;
        }
        migrate_session_log(&session_root)?;
    }
    Ok(())
}

fn migrate_session_log(session_root: &Path) -> Result<()> {
    migrate_session_json(&session_root.join("session.json"))?;
    migrate_messages_jsonl(&session_root.join("all_messages.jsonl"))?;
    migrate_messages_jsonl(&session_root.join("current_messages.jsonl"))?;
    Ok(())
}

fn migrate_session_json(path: &Path) -> Result<()> {
    if !path.exists() {
        return Ok(());
    }
    let raw =
        fs::read_to_string(path).with_context(|| format!("failed to read {}", path.display()))?;
    let mut value: Value = serde_json::from_str(&raw)
        .with_context(|| format!("failed to parse {}", path.display()))?;
    let changed = migrate_message_array(value.get_mut("all_messages"))
        | migrate_message_array(value.get_mut("current_messages"));
    if changed {
        fs::write(
            path,
            serde_json::to_string_pretty(&value)
                .context("failed to serialize migrated session state")?,
        )
        .with_context(|| format!("failed to write {}", path.display()))?;
    }
    Ok(())
}

fn migrate_messages_jsonl(path: &Path) -> Result<()> {
    if !path.exists() {
        return Ok(());
    }
    let file =
        fs::File::open(path).with_context(|| format!("failed to open {}", path.display()))?;
    let reader = BufReader::new(file);
    let mut changed = false;
    let mut lines = Vec::new();
    for line in reader.lines() {
        let line = line.with_context(|| format!("failed to read line from {}", path.display()))?;
        if line.trim().is_empty() {
            lines.push(line);
            continue;
        }
        let mut message: Value = serde_json::from_str(&line)
            .with_context(|| format!("failed to parse JSONL message in {}", path.display()))?;
        changed |= migrate_message(&mut message);
        lines
            .push(serde_json::to_string(&message).context("failed to serialize migrated message")?);
    }
    if !changed {
        return Ok(());
    }
    let mut file =
        fs::File::create(path).with_context(|| format!("failed to create {}", path.display()))?;
    for line in lines {
        writeln!(file, "{line}").with_context(|| format!("failed to write {}", path.display()))?;
    }
    file.flush()
        .with_context(|| format!("failed to flush {}", path.display()))?;
    Ok(())
}

fn migrate_message_array(value: Option<&mut Value>) -> bool {
    let Some(Value::Array(messages)) = value else {
        return false;
    };
    let mut changed = false;
    for message in messages {
        changed |= migrate_message(message);
    }
    changed
}

fn migrate_message(message: &mut Value) -> bool {
    let Some(Value::Array(items)) = message.get_mut("data") else {
        return false;
    };
    let mut changed = false;
    for item in items {
        if item.get("type").and_then(Value::as_str) != Some("tool_result") {
            continue;
        }
        changed |= migrate_tool_result_item(item);
    }
    changed
}

fn migrate_tool_result_item(item: &mut Value) -> bool {
    let Some(result) = item
        .get_mut("payload")
        .and_then(|payload| payload.get_mut("result"))
        .and_then(Value::as_object_mut)
    else {
        return false;
    };

    let mut changed = false;
    changed |= migrate_tool_result_file_fields(result);

    let has_structured = result
        .get("structured")
        .is_some_and(|value| !value.is_null());
    if has_structured {
        return result.remove("context").is_some() || changed;
    }

    let text = result.remove("context").and_then(|context| {
        context
            .get("text")
            .and_then(Value::as_str)
            .map(str::to_string)
    });
    if let Some(text) = text {
        result.insert(
            "structured".to_string(),
            json!({
                "kind": "text_result",
                "text": text,
            }),
        );
        return true;
    }
    changed
}

fn migrate_tool_result_file_fields(result: &mut serde_json::Map<String, Value>) -> bool {
    let mut changed = false;
    let mut files = match result.remove("files") {
        Some(Value::Array(files)) => files,
        Some(other) => {
            changed = true;
            vec![other]
        }
        None => Vec::new(),
    };
    if let Some(file) = result.remove("file") {
        if !file.is_null() {
            files.push(file);
        }
        changed = true;
    }
    if !files.is_empty() {
        result.insert("files".to_string(), Value::Array(files));
        changed = true;
    }
    changed
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{
        AgentServerConfig, SandboxConfig, SessionDefaults, StellaclawConfig, LATEST_CONFIG_VERSION,
    };
    use std::collections::BTreeMap;

    #[test]
    fn migrates_session_tool_result_context_to_structured() {
        let root = std::env::temp_dir().join(format!(
            "stellaclaw-tool-result-structured-upgrade-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&root);
        let session = root
            .join("conversations")
            .join("web-main-000001")
            .join(".stellaclaw")
            .join("log")
            .join("session");
        fs::create_dir_all(&session).unwrap();
        let message = json!({
            "role": "assistant",
            "data": [{
                "type": "tool_result",
                "payload": {
                    "tool_call_id": "call_1",
                    "tool_name": "file_read",
                    "result": {
                        "context": {"text": "loaded"},
                        "file": {
                            "uri": "file:///tmp/loaded.png",
                            "name": "loaded.png",
                            "media_type": "image/png"
                        }
                    }
                }
            }]
        });
        let state = json!({
            "version": 1,
            "initial": {"session_id": "session", "session_type": "foreground"},
            "all_messages": [message.clone()],
            "current_messages": [message],
            "next_turn_id": 2,
            "next_batch_id": 1
        });
        fs::write(
            session.join("session.json"),
            serde_json::to_string_pretty(&state).unwrap(),
        )
        .unwrap();
        fs::write(
            session.join("all_messages.jsonl"),
            format!(
                "{}\n",
                serde_json::to_string(&state["all_messages"][0]).unwrap()
            ),
        )
        .unwrap();

        ToolResultStructuredContentUpgrade
            .upgrade(&root, &test_config())
            .unwrap();

        let migrated: Value =
            serde_json::from_str(&fs::read_to_string(session.join("session.json")).unwrap())
                .unwrap();
        let result = &migrated["all_messages"][0]["data"][0]["payload"]["result"];
        assert!(result.get("context").is_none());
        assert!(result.get("file").is_none());
        assert_eq!(result["structured"]["kind"], "text_result");
        assert_eq!(result["structured"]["text"], "loaded");
        assert_eq!(result["files"][0]["uri"], "file:///tmp/loaded.png");

        let line = fs::read_to_string(session.join("all_messages.jsonl")).unwrap();
        let migrated_line: Value = serde_json::from_str(line.trim()).unwrap();
        let result = &migrated_line["data"][0]["payload"]["result"];
        assert!(result.get("context").is_none());
        assert!(result.get("file").is_none());
        assert_eq!(result["structured"]["text"], "loaded");
        assert_eq!(result["files"][0]["name"], "loaded.png");

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
            memory: crate::config::MemoryConfig::default(),
            sandbox: SandboxConfig::default(),
            skill_sync: Vec::new(),
            available_agent_models: Vec::new(),
        }
    }
}
