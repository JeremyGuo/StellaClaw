use crate::backend::AgentBackendKind;
use crate::config::SandboxMode;
use crate::domain::ChannelAddress;
use crate::workpath::{
    RemoteWorkpath, replace_workpath_description, validate_remote_workpath,
    validate_remote_workpath_host,
};
use anyhow::{Context, Result, anyhow};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use uuid::Uuid;

fn default_chat_version_id() -> Uuid {
    Uuid::new_v4()
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct ConversationSettings {
    #[serde(default)]
    pub agent_backend: Option<AgentBackendKind>,
    #[serde(default)]
    pub main_model: Option<String>,
    #[serde(default)]
    pub sandbox_mode: Option<SandboxMode>,
    #[serde(default)]
    pub reasoning_effort: Option<String>,
    #[serde(default)]
    pub context_compaction_enabled: Option<bool>,
    #[serde(default)]
    pub workspace_id: Option<String>,
    #[serde(default)]
    pub remote_workpaths: Vec<RemoteWorkpath>,
    #[serde(default = "default_chat_version_id")]
    pub chat_version_id: Uuid,
}

impl Default for ConversationSettings {
    fn default() -> Self {
        Self {
            agent_backend: None,
            main_model: None,
            sandbox_mode: None,
            reasoning_effort: None,
            context_compaction_enabled: None,
            workspace_id: None,
            remote_workpaths: Vec::new(),
            chat_version_id: default_chat_version_id(),
        }
    }
}

#[derive(Clone, Debug)]
pub struct ConversationSnapshot {
    pub id: Uuid,
    pub address: ChannelAddress,
    pub root_dir: PathBuf,
    pub settings: ConversationSettings,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct PersistedConversation {
    pub id: Uuid,
    pub address: ChannelAddress,
    #[serde(default)]
    pub settings: ConversationSettings,
}

#[derive(Clone, Debug)]
struct ConversationState {
    id: Uuid,
    address: ChannelAddress,
    root_dir: PathBuf,
    settings: ConversationSettings,
}

impl ConversationState {
    fn snapshot(&self) -> ConversationSnapshot {
        ConversationSnapshot {
            id: self.id,
            address: self.address.clone(),
            root_dir: self.root_dir.clone(),
            settings: self.settings.clone(),
        }
    }

    fn persisted(&self) -> PersistedConversation {
        PersistedConversation {
            id: self.id,
            address: self.address.clone(),
            settings: self.settings.clone(),
        }
    }

    fn state_path(&self) -> PathBuf {
        self.root_dir.join("conversation.json")
    }

    fn persist(&self) -> Result<()> {
        let raw = serde_json::to_string_pretty(&self.persisted())
            .context("failed to serialize conversation state")?;
        fs::write(self.state_path(), raw)
            .with_context(|| format!("failed to write {}", self.state_path().display()))
    }
}

pub struct ConversationManager {
    conversations_root: PathBuf,
    conversations: HashMap<String, ConversationState>,
}

impl ConversationManager {
    pub fn new(workdir: impl AsRef<Path>) -> Result<Self> {
        let conversations_root = workdir.as_ref().join("conversations");
        fs::create_dir_all(&conversations_root)
            .with_context(|| format!("failed to create {}", conversations_root.display()))?;
        let conversations = load_persisted_conversations(&conversations_root)?;
        Ok(Self {
            conversations_root,
            conversations,
        })
    }

    pub fn ensure_conversation(
        &mut self,
        address: &ChannelAddress,
    ) -> Result<ConversationSnapshot> {
        let key = address.session_key();
        if !self.conversations.contains_key(&key) {
            let id = Uuid::new_v4();
            let root_dir = self.conversations_root.join(id.to_string());
            fs::create_dir_all(&root_dir)
                .with_context(|| format!("failed to create {}", root_dir.display()))?;
            let state = ConversationState {
                id,
                address: address.clone(),
                root_dir,
                settings: ConversationSettings::default(),
            };
            state.persist()?;
            self.conversations.insert(key.clone(), state);
        }
        Ok(self
            .conversations
            .get(&key)
            .expect("conversation inserted")
            .snapshot())
    }

    pub fn get_snapshot(&self, address: &ChannelAddress) -> Option<ConversationSnapshot> {
        self.conversations
            .get(&address.session_key())
            .map(ConversationState::snapshot)
    }

    pub fn remove_conversation(
        &mut self,
        address: &ChannelAddress,
    ) -> Result<Option<ConversationSnapshot>> {
        let Some(state) = self.conversations.remove(&address.session_key()) else {
            return Ok(None);
        };
        let snapshot = state.snapshot();
        if state.root_dir.exists() {
            fs::remove_dir_all(&state.root_dir)
                .with_context(|| format!("failed to remove {}", state.root_dir.display()))?;
        }
        Ok(Some(snapshot))
    }

    pub fn set_main_model(
        &mut self,
        address: &ChannelAddress,
        model_key: Option<String>,
    ) -> Result<ConversationSnapshot> {
        let key = address.session_key();
        if !self.conversations.contains_key(&key) {
            self.ensure_conversation(address)?;
        }
        let state = self
            .conversations
            .get_mut(&key)
            .ok_or_else(|| anyhow!("missing conversation {}", key))?;
        state.settings.main_model = model_key;
        state.persist()?;
        Ok(state.snapshot())
    }

    pub fn set_agent_selection(
        &mut self,
        address: &ChannelAddress,
        backend: Option<AgentBackendKind>,
        model_key: Option<String>,
    ) -> Result<ConversationSnapshot> {
        let key = address.session_key();
        if !self.conversations.contains_key(&key) {
            self.ensure_conversation(address)?;
        }
        let state = self
            .conversations
            .get_mut(&key)
            .ok_or_else(|| anyhow!("missing conversation {}", key))?;
        state.settings.agent_backend = backend;
        state.settings.main_model = model_key;
        state.persist()?;
        Ok(state.snapshot())
    }

    pub fn set_sandbox_mode(
        &mut self,
        address: &ChannelAddress,
        sandbox_mode: Option<SandboxMode>,
    ) -> Result<ConversationSnapshot> {
        let key = address.session_key();
        if !self.conversations.contains_key(&key) {
            self.ensure_conversation(address)?;
        }
        let state = self
            .conversations
            .get_mut(&key)
            .ok_or_else(|| anyhow!("missing conversation {}", key))?;
        state.settings.sandbox_mode = sandbox_mode;
        state.persist()?;
        Ok(state.snapshot())
    }

    pub fn set_reasoning_effort(
        &mut self,
        address: &ChannelAddress,
        reasoning_effort: Option<String>,
    ) -> Result<ConversationSnapshot> {
        let key = address.session_key();
        if !self.conversations.contains_key(&key) {
            self.ensure_conversation(address)?;
        }
        let state = self
            .conversations
            .get_mut(&key)
            .ok_or_else(|| anyhow!("missing conversation {}", key))?;
        state.settings.reasoning_effort = reasoning_effort;
        state.persist()?;
        Ok(state.snapshot())
    }

    pub fn set_context_compaction_enabled(
        &mut self,
        address: &ChannelAddress,
        enabled: Option<bool>,
    ) -> Result<ConversationSnapshot> {
        let key = address.session_key();
        if !self.conversations.contains_key(&key) {
            self.ensure_conversation(address)?;
        }
        let state = self
            .conversations
            .get_mut(&key)
            .ok_or_else(|| anyhow!("missing conversation {}", key))?;
        state.settings.context_compaction_enabled = enabled;
        state.persist()?;
        Ok(state.snapshot())
    }

    pub fn set_workspace_id(
        &mut self,
        address: &ChannelAddress,
        workspace_id: Option<String>,
    ) -> Result<ConversationSnapshot> {
        let key = address.session_key();
        if !self.conversations.contains_key(&key) {
            self.ensure_conversation(address)?;
        }
        let state = self
            .conversations
            .get_mut(&key)
            .ok_or_else(|| anyhow!("missing conversation {}", key))?;
        state.settings.workspace_id = workspace_id;
        state.persist()?;
        Ok(state.snapshot())
    }

    pub fn rotate_chat_version_id(
        &mut self,
        address: &ChannelAddress,
    ) -> Result<ConversationSnapshot> {
        let key = address.session_key();
        if !self.conversations.contains_key(&key) {
            self.ensure_conversation(address)?;
        }
        let state = self
            .conversations
            .get_mut(&key)
            .ok_or_else(|| anyhow!("missing conversation {}", key))?;
        state.settings.chat_version_id = Uuid::new_v4();
        state.persist()?;
        Ok(state.snapshot())
    }

    pub fn add_remote_workpath(
        &mut self,
        address: &ChannelAddress,
        host: &str,
        path: &str,
        description: &str,
    ) -> Result<ConversationSnapshot> {
        let workpath = validate_remote_workpath(host, path, description)?;
        let key = address.session_key();
        if !self.conversations.contains_key(&key) {
            self.ensure_conversation(address)?;
        }
        let state = self
            .conversations
            .get_mut(&key)
            .ok_or_else(|| anyhow!("missing conversation {}", key))?;
        let host_key = workpath.host.clone();
        state
            .settings
            .remote_workpaths
            .retain(|item| item.host != host_key);
        state.settings.remote_workpaths.push(workpath);
        state.settings.chat_version_id = Uuid::new_v4();
        state.persist()?;
        Ok(state.snapshot())
    }

    pub fn modify_remote_workpath(
        &mut self,
        address: &ChannelAddress,
        host: &str,
        _path: &str,
        description: &str,
    ) -> Result<ConversationSnapshot> {
        let host = validate_remote_workpath_host(host)?;
        let key = address.session_key();
        if !self.conversations.contains_key(&key) {
            self.ensure_conversation(address)?;
        }
        let state = self
            .conversations
            .get_mut(&key)
            .ok_or_else(|| anyhow!("missing conversation {}", key))?;
        replace_workpath_description(&mut state.settings.remote_workpaths, &host, description)?;
        state.settings.chat_version_id = Uuid::new_v4();
        state.persist()?;
        Ok(state.snapshot())
    }

    pub fn remove_remote_workpath(
        &mut self,
        address: &ChannelAddress,
        host: &str,
        _path: &str,
    ) -> Result<ConversationSnapshot> {
        let host = validate_remote_workpath_host(host)?;
        let key = address.session_key();
        if !self.conversations.contains_key(&key) {
            self.ensure_conversation(address)?;
        }
        let state = self
            .conversations
            .get_mut(&key)
            .ok_or_else(|| anyhow!("missing conversation {}", key))?;
        let before = state.settings.remote_workpaths.len();
        state
            .settings
            .remote_workpaths
            .retain(|item| item.host != host);
        if state.settings.remote_workpaths.len() == before {
            return Err(anyhow!("remote workpath not found for {}", host));
        }
        state.settings.chat_version_id = Uuid::new_v4();
        state.persist()?;
        Ok(state.snapshot())
    }
}

fn load_persisted_conversations(root: &Path) -> Result<HashMap<String, ConversationState>> {
    let mut conversations = HashMap::new();
    for entry in fs::read_dir(root).with_context(|| format!("failed to read {}", root.display()))? {
        let entry = entry?;
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let state_path = path.join("conversation.json");
        if !state_path.is_file() {
            continue;
        }
        let raw = fs::read_to_string(&state_path)
            .with_context(|| format!("failed to read {}", state_path.display()))?;
        let persisted: PersistedConversation =
            serde_json::from_str(&raw).context("failed to parse conversation state")?;
        let key = persisted.address.session_key();
        conversations.insert(
            key,
            ConversationState {
                id: persisted.id,
                address: persisted.address,
                root_dir: path,
                settings: persisted.settings,
            },
        );
    }
    Ok(conversations)
}

#[cfg(test)]
mod tests {
    use super::ConversationManager;
    use crate::config::SandboxMode;
    use crate::domain::ChannelAddress;
    use tempfile::TempDir;
    use uuid::Uuid;

    fn test_address() -> ChannelAddress {
        ChannelAddress {
            channel_id: "telegram-main".to_string(),
            conversation_id: "conversation-1".to_string(),
            user_id: Some("user-1".to_string()),
            display_name: Some("Test User".to_string()),
        }
    }

    #[test]
    fn persists_conversation_settings() {
        let temp_dir = TempDir::new().unwrap();
        let address = test_address();
        let mut manager = ConversationManager::new(temp_dir.path()).unwrap();

        manager
            .set_main_model(&address, Some("secondary".to_string()))
            .unwrap();
        manager
            .set_sandbox_mode(&address, Some(SandboxMode::Subprocess))
            .unwrap();
        manager
            .set_reasoning_effort(&address, Some("high".to_string()))
            .unwrap();
        manager
            .set_context_compaction_enabled(&address, Some(false))
            .unwrap();
        manager
            .set_workspace_id(&address, Some("workspace-1".to_string()))
            .unwrap();
        manager
            .add_remote_workpath(
                &address,
                "wuwen-dev6",
                "~/project",
                "remote development checkout",
            )
            .unwrap();
        manager
            .modify_remote_workpath(
                &address,
                "wuwen-dev6",
                "~/project",
                "remote build and test checkout",
            )
            .unwrap();

        let reloaded = ConversationManager::new(temp_dir.path()).unwrap();
        let snapshot = reloaded.get_snapshot(&address).unwrap();
        assert_eq!(snapshot.settings.main_model.as_deref(), Some("secondary"));
        assert_eq!(
            snapshot.settings.sandbox_mode,
            Some(SandboxMode::Subprocess)
        );
        assert_eq!(snapshot.settings.reasoning_effort.as_deref(), Some("high"));
        assert_eq!(snapshot.settings.context_compaction_enabled, Some(false));
        assert_eq!(
            snapshot.settings.workspace_id.as_deref(),
            Some("workspace-1")
        );
        assert_eq!(snapshot.settings.remote_workpaths.len(), 1);
        assert_eq!(snapshot.settings.remote_workpaths[0].host, "wuwen-dev6");
        assert_eq!(snapshot.settings.remote_workpaths[0].path, "~/project");
        assert_eq!(
            snapshot.settings.remote_workpaths[0].description,
            "remote build and test checkout"
        );
        assert_ne!(snapshot.settings.chat_version_id, Uuid::nil());
    }

    #[test]
    fn removes_remote_workpath() {
        let temp_dir = TempDir::new().unwrap();
        let address = test_address();
        let mut manager = ConversationManager::new(temp_dir.path()).unwrap();

        manager
            .add_remote_workpath(&address, "wuwen-dev6", "/srv/app", "remote app")
            .unwrap();
        let snapshot = manager
            .remove_remote_workpath(&address, "wuwen-dev6", "/srv/app")
            .unwrap();

        assert!(snapshot.settings.remote_workpaths.is_empty());
    }

    #[test]
    fn add_remote_workpath_replaces_existing_host() {
        let temp_dir = TempDir::new().unwrap();
        let address = test_address();
        let mut manager = ConversationManager::new(temp_dir.path()).unwrap();

        manager
            .add_remote_workpath(&address, "wuwen-dev6", "/srv/old", "old checkout")
            .unwrap();
        let snapshot = manager
            .add_remote_workpath(&address, "wuwen-dev6", "/srv/new", "new checkout")
            .unwrap();

        assert_eq!(snapshot.settings.remote_workpaths.len(), 1);
        assert_eq!(snapshot.settings.remote_workpaths[0].host, "wuwen-dev6");
        assert_eq!(snapshot.settings.remote_workpaths[0].path, "/srv/new");
        assert_eq!(
            snapshot.settings.remote_workpaths[0].description,
            "new checkout"
        );
    }

    #[test]
    fn rotates_chat_version_id() {
        let temp_dir = TempDir::new().unwrap();
        let address = test_address();
        let mut manager = ConversationManager::new(temp_dir.path()).unwrap();

        let original = manager.ensure_conversation(&address).unwrap();
        let rotated = manager.rotate_chat_version_id(&address).unwrap();

        assert_ne!(
            original.settings.chat_version_id,
            rotated.settings.chat_version_id
        );
    }

    #[test]
    fn remove_conversation_deletes_persisted_state() {
        let temp_dir = TempDir::new().unwrap();
        let address = test_address();
        let mut manager = ConversationManager::new(temp_dir.path()).unwrap();

        let snapshot = manager.ensure_conversation(&address).unwrap();
        assert!(snapshot.root_dir.exists());
        let removed = manager.remove_conversation(&address).unwrap();

        assert!(removed.is_some());
        assert!(!snapshot.root_dir.exists());
        let reloaded = ConversationManager::new(temp_dir.path()).unwrap();
        assert!(reloaded.get_snapshot(&address).is_none());
    }
}
