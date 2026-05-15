use std::{fs, path::PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

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

    pub(crate) fn services_root(&self) -> PathBuf {
        self.workdir.join("services")
    }

    pub(crate) fn conversation_service_root(&self, conversation_id: &str) -> PathBuf {
        self.services_root().join(conversation_id)
    }

    pub(crate) fn conversation_metadata_path(&self, conversation_id: &str) -> PathBuf {
        self.conversation_service_root(conversation_id)
            .join("conversation_metadata.json")
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

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct ConversationMetadata {
    pub version: u32,
    pub conversation_id: String,
    #[serde(default)]
    pub nickname: String,
    pub channel_id: String,
    pub platform_chat_id: String,
    #[serde(default = "default_foreground_session_id")]
    pub foreground_session_id: String,
    #[serde(default)]
    pub model_selection_pending: bool,
    #[serde(default)]
    pub session_nicknames: BTreeMap<String, String>,
}

impl ConversationMetadata {
    pub(crate) fn new(conversation_id: &str, channel_id: &str, platform_chat_id: &str) -> Self {
        Self {
            version: 1,
            conversation_id: conversation_id.to_string(),
            nickname: conversation_id.to_string(),
            channel_id: channel_id.to_string(),
            platform_chat_id: platform_chat_id.to_string(),
            foreground_session_id: default_foreground_session_id(),
            model_selection_pending: true,
            session_nicknames: BTreeMap::from([(
                default_foreground_session_id(),
                "Main".to_string(),
            )]),
        }
    }
}

#[derive(Debug, Clone)]
pub(crate) struct ConversationMetadataStore {
    layout: WorkdirLayout,
}

impl ConversationMetadataStore {
    pub(crate) fn new(workdir: impl Into<PathBuf>) -> Self {
        Self {
            layout: WorkdirLayout::new(workdir),
        }
    }

    pub(crate) fn layout(&self) -> &WorkdirLayout {
        &self.layout
    }

    pub(crate) fn ensure_conversation_roots(&self, conversation_id: &str) -> Result<()> {
        let conversation_root = self.layout.conversation_root(conversation_id);
        fs::create_dir_all(&conversation_root)
            .with_context(|| format!("failed to create {}", conversation_root.display()))?;
        let service_root = self.layout.conversation_service_root(conversation_id);
        fs::create_dir_all(&service_root)
            .with_context(|| format!("failed to create {}", service_root.display()))?;
        Ok(())
    }

    pub(crate) fn load(&self, conversation_id: &str) -> Result<ConversationMetadata> {
        let path = self.layout.conversation_metadata_path(conversation_id);
        let raw = fs::read_to_string(&path)
            .with_context(|| format!("failed to read {}", path.display()))?;
        serde_json::from_str(&raw).with_context(|| format!("failed to parse {}", path.display()))
    }

    pub(crate) fn load_or_create(
        &self,
        conversation_id: &str,
        channel_id: &str,
        platform_chat_id: &str,
    ) -> Result<ConversationMetadata> {
        self.ensure_conversation_roots(conversation_id)?;
        let path = self.layout.conversation_metadata_path(conversation_id);
        if path.is_file() {
            let mut metadata = self.load(conversation_id)?;
            if metadata.nickname.trim().is_empty() {
                metadata.nickname = metadata.conversation_id.clone();
            }
            return Ok(metadata);
        }
        Ok(ConversationMetadata::new(
            conversation_id,
            channel_id,
            platform_chat_id,
        ))
    }

    pub(crate) fn persist(&self, metadata: &ConversationMetadata) -> Result<()> {
        self.ensure_conversation_roots(&metadata.conversation_id)?;
        let path = self
            .layout
            .conversation_metadata_path(&metadata.conversation_id);
        let raw = serde_json::to_string_pretty(metadata)
            .context("failed to serialize conversation metadata")?;
        fs::write(&path, raw).with_context(|| format!("failed to write {}", path.display()))
    }

    pub(crate) fn list_metadata_paths(&self) -> Result<Vec<PathBuf>> {
        let root = self.layout.services_root();
        if !root.exists() {
            return Ok(Vec::new());
        }
        let mut paths = Vec::new();
        for entry in
            fs::read_dir(&root).with_context(|| format!("failed to read {}", root.display()))?
        {
            let entry = entry?;
            let path = entry.path().join("conversation_metadata.json");
            if path.is_file() {
                paths.push(path);
            }
        }
        Ok(paths)
    }

    pub(crate) fn remove(&self, conversation_id: &str) -> Result<()> {
        let conversation_root = self.layout.conversation_root(conversation_id);
        if conversation_root.exists() {
            fs::remove_dir_all(&conversation_root)
                .with_context(|| format!("failed to remove {}", conversation_root.display()))?;
        }
        let service_root = self.layout.conversation_service_root(conversation_id);
        if service_root.exists() {
            fs::remove_dir_all(&service_root)
                .with_context(|| format!("failed to remove {}", service_root.display()))?;
        }
        Ok(())
    }
}

pub(crate) fn default_foreground_session_id() -> String {
    "local__agent__foreground__main".to_string()
}
