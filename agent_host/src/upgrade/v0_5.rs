use super::WorkdirUpgrader;
use crate::domain::ChannelAddress;
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs;
use std::path::Path;
use uuid::Uuid;

pub(super) struct Upgrade;

#[derive(Clone, Debug, Deserialize)]
struct PersistedSession {
    address: ChannelAddress,
    #[serde(default)]
    workspace_id: Option<String>,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
struct ConversationSettings {
    #[serde(default)]
    main_model: Option<String>,
    #[serde(default)]
    sandbox_mode: Option<String>,
    #[serde(default)]
    reasoning_effort: Option<String>,
    #[serde(default)]
    context_compaction_enabled: Option<bool>,
    #[serde(default)]
    workspace_id: Option<String>,
    #[serde(default = "Uuid::new_v4")]
    chat_version_id: Uuid,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct PersistedConversation {
    id: Uuid,
    address: ChannelAddress,
    #[serde(default)]
    settings: ConversationSettings,
}

impl WorkdirUpgrader for Upgrade {
    fn from_version(&self) -> &'static str {
        "0.4"
    }

    fn to_version(&self) -> &'static str {
        "0.5"
    }

    fn upgrade(&self, workdir: &Path) -> Result<()> {
        let sessions_root = workdir.join("sessions");
        let mut workspace_by_conversation = HashMap::new();
        if sessions_root.exists() {
            for entry in fs::read_dir(&sessions_root)
                .with_context(|| format!("failed to read {}", sessions_root.display()))?
            {
                let path = entry?.path().join("session.json");
                if !path.is_file() {
                    continue;
                }
                let raw = fs::read_to_string(&path)
                    .with_context(|| format!("failed to read {}", path.display()))?;
                let session: PersistedSession =
                    serde_json::from_str(&raw).context("failed to parse persisted session")?;
                if let Some(workspace_id) = session.workspace_id {
                    workspace_by_conversation
                        .entry(session.address.session_key())
                        .or_insert(workspace_id);
                }
            }
        }

        let conversations_root = workdir.join("conversations");
        if !conversations_root.exists() {
            return Ok(());
        }
        for entry in fs::read_dir(&conversations_root)
            .with_context(|| format!("failed to read {}", conversations_root.display()))?
        {
            let path = entry?.path().join("conversation.json");
            if !path.is_file() {
                continue;
            }
            let raw = fs::read_to_string(&path)
                .with_context(|| format!("failed to read {}", path.display()))?;
            let mut conversation: PersistedConversation =
                serde_json::from_str(&raw).context("failed to parse persisted conversation")?;
            if conversation.settings.workspace_id.is_none() {
                conversation.settings.workspace_id = workspace_by_conversation
                    .get(&conversation.address.session_key())
                    .cloned();
                let updated = serde_json::to_string_pretty(&conversation)
                    .context("failed to serialize upgraded conversation")?;
                fs::write(&path, updated)
                    .with_context(|| format!("failed to write {}", path.display()))?;
            }
        }
        Ok(())
    }
}
