use std::{fs, path::PathBuf};

use anyhow::{Context, Result};

use crate::config::StellaclawConfig;

use super::{default_index, ConversationSessionBinding, ConversationState};
use stellaclaw_core::session_actor::ToolRemoteMode;

#[derive(Debug, Clone)]
pub(crate) struct WorkdirLayout {
    workdir: PathBuf,
}

impl WorkdirLayout {
    pub(crate) fn new(workdir: impl Into<PathBuf>) -> Self {
        Self {
            workdir: workdir.into(),
        }
    }

    pub(crate) fn conversations_root(&self) -> PathBuf {
        self.workdir.join("conversations")
    }

    pub(crate) fn conversation_root(&self, conversation_id: &str) -> PathBuf {
        self.conversations_root().join(conversation_id)
    }

    pub(crate) fn conversation_state_path(&self, conversation_id: &str) -> PathBuf {
        self.conversation_root(conversation_id)
            .join("conversation.json")
    }

    pub(crate) fn runtime_root(&self) -> PathBuf {
        self.workdir.join("rundir")
    }

    pub(crate) fn runtime_shared_root(&self) -> PathBuf {
        self.runtime_root().join("shared")
    }

    pub(crate) fn runtime_skill_root(&self) -> PathBuf {
        self.runtime_root().join(".stellaclaw").join("skill")
    }
}

#[derive(Debug, Clone)]
pub(crate) struct ConversationStore {
    layout: WorkdirLayout,
}

impl ConversationStore {
    pub(crate) fn new(workdir: impl Into<PathBuf>) -> Self {
        Self {
            layout: WorkdirLayout::new(workdir),
        }
    }

    pub(crate) fn layout(&self) -> &WorkdirLayout {
        &self.layout
    }

    pub(crate) fn ensure_conversation_root(&self, conversation_id: &str) -> Result<PathBuf> {
        let root = self.layout.conversation_root(conversation_id);
        fs::create_dir_all(&root)
            .with_context(|| format!("failed to create {}", root.display()))?;
        Ok(root)
    }

    pub(crate) fn load(&self, conversation_id: &str) -> Result<ConversationState> {
        let path = self.layout.conversation_state_path(conversation_id);
        let raw = fs::read_to_string(&path)
            .with_context(|| format!("failed to read {}", path.display()))?;
        serde_json::from_str(&raw).with_context(|| format!("failed to parse {}", path.display()))
    }

    pub(crate) fn load_or_create(
        &self,
        conversation_id: &str,
        channel_id: &str,
        platform_chat_id: &str,
        config: &StellaclawConfig,
    ) -> Result<ConversationState> {
        self.ensure_conversation_root(conversation_id)?;
        let path = self.layout.conversation_state_path(conversation_id);
        if path.exists() {
            let raw = fs::read_to_string(&path)
                .with_context(|| format!("failed to read {}", path.display()))?;
            let mut state: ConversationState = serde_json::from_str(&raw)
                .with_context(|| format!("failed to parse {}", path.display()))?;
            if state.nickname.trim().is_empty() {
                state.nickname = state.conversation_id.clone();
            }
            return Ok(state);
        }

        Ok(ConversationState {
            version: 1,
            conversation_id: conversation_id.to_string(),
            nickname: conversation_id.to_string(),
            channel_id: channel_id.to_string(),
            platform_chat_id: platform_chat_id.to_string(),
            session_profile: config
                .initial_session_profile()
                .map_err(anyhow::Error::msg)?,
            model_selection_pending: true,
            tool_remote_mode: ToolRemoteMode::Selectable,
            sandbox: None,
            reasoning_effort: None,
            session_binding: ConversationSessionBinding {
                foreground_session_id: format!("{conversation_id}.foreground"),
                next_background_index: default_index(),
                next_subagent_index: default_index(),
                background_sessions: Default::default(),
                subagent_sessions: Default::default(),
            },
        })
    }

    pub(crate) fn persist(&self, state: &ConversationState) -> Result<()> {
        let root = self.ensure_conversation_root(&state.conversation_id)?;
        let path = root.join("conversation.json");
        let raw = serde_json::to_string_pretty(state)
            .context("failed to serialize conversation state")?;
        fs::write(&path, raw).with_context(|| format!("failed to write {}", path.display()))
    }

    pub(crate) fn list_state_paths(&self) -> Result<Vec<PathBuf>> {
        let root = self.layout.conversations_root();
        if !root.exists() {
            return Ok(Vec::new());
        }
        let mut paths = Vec::new();
        for entry in
            fs::read_dir(&root).with_context(|| format!("failed to read {}", root.display()))?
        {
            let entry = entry?;
            let path = entry.path().join("conversation.json");
            if path.exists() {
                paths.push(path);
            }
        }
        Ok(paths)
    }
}
