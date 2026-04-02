use crate::config::SandboxMode;
use crate::domain::ChannelAddress;
use crate::session::SessionCheckpointData;
use anyhow::{Context, Result, anyhow};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use uuid::Uuid;

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct ConversationSettings {
    #[serde(default)]
    pub main_model: Option<String>,
    #[serde(default)]
    pub sandbox_mode: Option<SandboxMode>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ConversationCheckpointRecord {
    pub name: String,
    pub saved_at: DateTime<Utc>,
    #[serde(default)]
    pub main_model: Option<String>,
    #[serde(default)]
    pub sandbox_mode: Option<SandboxMode>,
}

#[derive(Clone, Debug)]
pub struct ConversationSnapshot {
    pub id: Uuid,
    pub address: ChannelAddress,
    pub root_dir: PathBuf,
    pub settings: ConversationSettings,
    pub checkpoints: Vec<ConversationCheckpointRecord>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ConversationCheckpointBundle {
    pub saved_at: DateTime<Utc>,
    pub settings: ConversationSettings,
    pub session: SessionCheckpointData,
}

#[derive(Clone, Debug)]
pub struct LoadedConversationCheckpoint {
    pub name: String,
    pub workspace_dir: PathBuf,
    pub bundle: ConversationCheckpointBundle,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct PersistedConversation {
    pub id: Uuid,
    pub address: ChannelAddress,
    #[serde(default)]
    pub settings: ConversationSettings,
    #[serde(default)]
    pub checkpoints: Vec<ConversationCheckpointRecord>,
}

#[derive(Clone, Debug)]
struct ConversationState {
    id: Uuid,
    address: ChannelAddress,
    root_dir: PathBuf,
    settings: ConversationSettings,
    checkpoints: Vec<ConversationCheckpointRecord>,
}

impl ConversationState {
    fn snapshot(&self) -> ConversationSnapshot {
        ConversationSnapshot {
            id: self.id,
            address: self.address.clone(),
            root_dir: self.root_dir.clone(),
            settings: self.settings.clone(),
            checkpoints: self.checkpoints.clone(),
        }
    }

    fn persisted(&self) -> PersistedConversation {
        PersistedConversation {
            id: self.id,
            address: self.address.clone(),
            settings: self.settings.clone(),
            checkpoints: self.checkpoints.clone(),
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
                checkpoints: Vec::new(),
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

    pub fn save_checkpoint(
        &mut self,
        address: &ChannelAddress,
        checkpoint_name: &str,
        bundle: ConversationCheckpointBundle,
        workspace_root: &Path,
    ) -> Result<ConversationCheckpointRecord> {
        let sanitized_name = sanitize_checkpoint_name(checkpoint_name)?;
        let key = address.session_key();
        if !self.conversations.contains_key(&key) {
            self.ensure_conversation(address)?;
        }
        let state = self
            .conversations
            .get_mut(&key)
            .ok_or_else(|| anyhow!("missing conversation {}", key))?;
        let checkpoint_dir = state.root_dir.join("checkpoints").join(&sanitized_name);
        if checkpoint_dir.exists() {
            fs::remove_dir_all(&checkpoint_dir).with_context(|| {
                format!(
                    "failed to replace existing checkpoint {}",
                    checkpoint_dir.display()
                )
            })?;
        }
        fs::create_dir_all(&checkpoint_dir)
            .with_context(|| format!("failed to create {}", checkpoint_dir.display()))?;
        let workspace_dir = checkpoint_dir.join("workspace");
        copy_dir_recursive(workspace_root, &workspace_dir)?;
        let bundle_path = checkpoint_dir.join("checkpoint.json");
        let raw =
            serde_json::to_string_pretty(&bundle).context("failed to serialize checkpoint")?;
        fs::write(&bundle_path, raw)
            .with_context(|| format!("failed to write {}", bundle_path.display()))?;
        let record = ConversationCheckpointRecord {
            name: sanitized_name.clone(),
            saved_at: bundle.saved_at,
            main_model: bundle.settings.main_model.clone(),
            sandbox_mode: bundle.settings.sandbox_mode,
        };
        if let Some(existing) = state
            .checkpoints
            .iter_mut()
            .find(|existing| existing.name == sanitized_name)
        {
            *existing = record.clone();
        } else {
            state.checkpoints.push(record.clone());
            state
                .checkpoints
                .sort_by(|left, right| left.name.cmp(&right.name));
        }
        state.persist()?;
        Ok(record)
    }

    pub fn load_checkpoint(
        &mut self,
        address: &ChannelAddress,
        checkpoint_name: &str,
    ) -> Result<LoadedConversationCheckpoint> {
        let sanitized_name = sanitize_checkpoint_name(checkpoint_name)?;
        let snapshot = self.ensure_conversation(address)?;
        let checkpoint_dir = snapshot.root_dir.join("checkpoints").join(&sanitized_name);
        let bundle_path = checkpoint_dir.join("checkpoint.json");
        let workspace_dir = checkpoint_dir.join("workspace");
        let raw = fs::read_to_string(&bundle_path)
            .with_context(|| format!("failed to read {}", bundle_path.display()))?;
        let bundle: ConversationCheckpointBundle =
            serde_json::from_str(&raw).context("failed to parse checkpoint bundle")?;
        if !workspace_dir.is_dir() {
            return Err(anyhow!(
                "checkpoint workspace directory is missing: {}",
                workspace_dir.display()
            ));
        }
        Ok(LoadedConversationCheckpoint {
            name: sanitized_name,
            workspace_dir,
            bundle,
        })
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
                checkpoints: persisted.checkpoints,
            },
        );
    }
    Ok(conversations)
}

fn sanitize_checkpoint_name(name: &str) -> Result<String> {
    let trimmed = name.trim();
    if trimmed.is_empty() {
        return Err(anyhow!("checkpoint name must not be empty"));
    }
    let sanitized: String = trimmed
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '.') {
                ch
            } else {
                '_'
            }
        })
        .collect();
    let sanitized = sanitized.trim_matches('_').to_string();
    if sanitized.is_empty() {
        return Err(anyhow!(
            "checkpoint name must contain at least one safe character"
        ));
    }
    Ok(sanitized)
}

fn copy_dir_recursive(source: &Path, target: &Path) -> Result<()> {
    fs::create_dir_all(target).with_context(|| format!("failed to create {}", target.display()))?;
    for entry in
        fs::read_dir(source).with_context(|| format!("failed to read {}", source.display()))?
    {
        let entry = entry?;
        let source_path = entry.path();
        let target_path = target.join(entry.file_name());
        let file_type = entry
            .file_type()
            .with_context(|| format!("failed to inspect {}", source_path.display()))?;
        if file_type.is_dir() {
            copy_dir_recursive(&source_path, &target_path)?;
        } else if file_type.is_symlink() {
            let target_link = fs::read_link(&source_path)
                .with_context(|| format!("failed to read link {}", source_path.display()))?;
            create_symlink(&target_link, &target_path)?;
        } else {
            fs::copy(&source_path, &target_path).with_context(|| {
                format!(
                    "failed to copy {} to {}",
                    source_path.display(),
                    target_path.display()
                )
            })?;
        }
    }
    Ok(())
}

fn create_symlink(source: &Path, target: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(source, target)
            .with_context(|| format!("failed to create symlink {}", target.display()))
    }
    #[cfg(windows)]
    {
        let metadata = fs::metadata(source)
            .with_context(|| format!("failed to stat symlink target {}", source.display()))?;
        if metadata.is_dir() {
            std::os::windows::fs::symlink_dir(source, target)
                .with_context(|| format!("failed to create symlink {}", target.display()))
        } else {
            std::os::windows::fs::symlink_file(source, target)
                .with_context(|| format!("failed to create symlink {}", target.display()))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{ConversationCheckpointBundle, ConversationManager};
    use crate::config::SandboxMode;
    use crate::domain::ChannelAddress;
    use crate::session::SessionCheckpointData;
    use chrono::Utc;
    use std::fs;
    use tempfile::TempDir;

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

        let reloaded = ConversationManager::new(temp_dir.path()).unwrap();
        let snapshot = reloaded.get_snapshot(&address).unwrap();
        assert_eq!(snapshot.settings.main_model.as_deref(), Some("secondary"));
        assert_eq!(
            snapshot.settings.sandbox_mode,
            Some(SandboxMode::Subprocess)
        );
    }

    #[test]
    fn saves_and_loads_checkpoint_bundle() {
        let temp_dir = TempDir::new().unwrap();
        let address = test_address();
        let mut manager = ConversationManager::new(temp_dir.path()).unwrap();
        manager.ensure_conversation(&address).unwrap();

        let workspace_root = temp_dir.path().join("workspace");
        fs::create_dir_all(&workspace_root).unwrap();
        fs::write(workspace_root.join("note.txt"), "hello").unwrap();

        let bundle = ConversationCheckpointBundle {
            saved_at: Utc::now(),
            settings: Default::default(),
            session: SessionCheckpointData {
                turn_count: 3,
                ..Default::default()
            },
        };
        manager
            .save_checkpoint(&address, "demo", bundle, &workspace_root)
            .unwrap();

        let loaded = manager.load_checkpoint(&address, "demo").unwrap();
        assert_eq!(loaded.name, "demo");
        assert_eq!(loaded.bundle.session.turn_count, 3);
        assert_eq!(
            fs::read_to_string(loaded.workspace_dir.join("note.txt")).unwrap(),
            "hello"
        );
    }
}
