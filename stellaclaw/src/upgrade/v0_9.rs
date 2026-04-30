use std::{fs, path::Path};

use anyhow::{Context, Result};
use serde_json::Value;

use super::{WorkdirUpgrader, LATEST_WORKDIR_VERSION, WORKDIR_VERSION_0_9};
use crate::{config::StellaclawConfig, workspace::is_sshfs_workspace_entry_name};

pub struct ConversationNicknameUpgrade;

impl WorkdirUpgrader for ConversationNicknameUpgrade {
    fn from_version(&self) -> &'static str {
        WORKDIR_VERSION_0_9
    }

    fn to_version(&self) -> &'static str {
        LATEST_WORKDIR_VERSION
    }

    fn upgrade(&self, workdir: &Path, _config: &StellaclawConfig) -> Result<()> {
        let conversations_root = workdir.join("conversations");
        if !conversations_root.is_dir() {
            return Ok(());
        }

        for entry in fs::read_dir(&conversations_root)
            .with_context(|| format!("failed to read {}", conversations_root.display()))?
        {
            let entry = entry
                .with_context(|| format!("failed to enumerate {}", conversations_root.display()))?;
            if entry
                .file_name()
                .to_str()
                .is_some_and(is_sshfs_workspace_entry_name)
            {
                continue;
            }
            let path = entry.path().join("conversation.json");
            if path.is_file() {
                upgrade_conversation_state(&path)?;
            }
        }
        Ok(())
    }
}

fn upgrade_conversation_state(path: &Path) -> Result<()> {
    let raw =
        fs::read_to_string(path).with_context(|| format!("failed to read {}", path.display()))?;
    let mut value: Value = serde_json::from_str(&raw)
        .with_context(|| format!("failed to parse {}", path.display()))?;
    let Some(object) = value.as_object_mut() else {
        return Ok(());
    };
    let nickname_missing_or_empty = object
        .get("nickname")
        .and_then(Value::as_str)
        .map_or(true, |nickname| nickname.trim().is_empty());
    if !nickname_missing_or_empty {
        return Ok(());
    }
    let nickname = object
        .get("conversation_id")
        .and_then(Value::as_str)
        .map(str::to_string)
        .or_else(|| {
            path.parent()
                .and_then(Path::file_name)
                .and_then(|value| value.to_str())
                .map(str::to_string)
        });
    let Some(nickname) = nickname else {
        return Ok(());
    };
    object.insert("nickname".to_string(), Value::String(nickname));
    fs::write(
        path,
        serde_json::to_string_pretty(&value)
            .context("failed to serialize upgraded conversation state")?,
    )
    .with_context(|| format!("failed to write {}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    use serde_json::json;

    use crate::config::{
        AgentServerConfig, SandboxConfig, SessionDefaults, StellaclawConfig, LATEST_CONFIG_VERSION,
    };

    #[test]
    fn adds_conversation_nickname_from_conversation_id() {
        let root = std::env::temp_dir().join(format!(
            "stellaclaw-conversation-nickname-upgrade-{}",
            std::process::id()
        ));
        let conversation_root = root.join("conversations").join("web-main-000042");
        fs::create_dir_all(&conversation_root).expect("create conversation root");
        fs::write(
            conversation_root.join("conversation.json"),
            serde_json::to_string_pretty(&json!({
                "version": 1,
                "conversation_id": "web-main-000042",
                "channel_id": "web-main",
                "platform_chat_id": "test-chat"
            }))
            .unwrap(),
        )
        .unwrap();

        ConversationNicknameUpgrade
            .upgrade(&root, &test_config())
            .expect("upgrade should add nickname");

        let upgraded: Value = serde_json::from_str(
            &fs::read_to_string(conversation_root.join("conversation.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(upgraded["nickname"].as_str(), Some("web-main-000042"));

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn preserves_existing_conversation_nickname() {
        let root = std::env::temp_dir().join(format!(
            "stellaclaw-conversation-nickname-preserve-upgrade-{}",
            std::process::id()
        ));
        let conversation_root = root.join("conversations").join("web-main-000043");
        fs::create_dir_all(&conversation_root).expect("create conversation root");
        fs::write(
            conversation_root.join("conversation.json"),
            serde_json::to_string_pretty(&json!({
                "version": 1,
                "conversation_id": "web-main-000043",
                "nickname": "Demo",
                "channel_id": "web-main",
                "platform_chat_id": "test-chat"
            }))
            .unwrap(),
        )
        .unwrap();

        ConversationNicknameUpgrade
            .upgrade(&root, &test_config())
            .expect("upgrade should preserve nickname");

        let upgraded: Value = serde_json::from_str(
            &fs::read_to_string(conversation_root.join("conversation.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(upgraded["nickname"].as_str(), Some("Demo"));

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
