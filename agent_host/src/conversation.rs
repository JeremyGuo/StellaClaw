use crate::attachment_prep::normalize_stored_attachment_for_persistence;
use crate::backend::AgentBackendKind;
use crate::channel::{IncomingMessage, PendingAttachment};
use crate::config::SandboxMode;
use crate::domain::{ChannelAddress, StoredAttachment, validate_conversation_id};
use crate::remote_execution::RemoteExecutionBinding;
use crate::session::{SessionActorRef, SessionManager};
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
pub struct LocalMount {
    pub path: PathBuf,
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
    #[serde(default)]
    pub local_mounts: Vec<LocalMount>,
    #[serde(default)]
    pub remote_execution: Option<RemoteExecutionBinding>,
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
            local_mounts: Vec::new(),
            remote_execution: None,
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

#[derive(Clone)]
struct ConversationState {
    id: Uuid,
    address: ChannelAddress,
    root_dir: PathBuf,
    settings: ConversationSettings,
    foreground_actor: Option<SessionActorRef>,
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
    fn workdir_root(&self) -> &Path {
        self.conversations_root
            .parent()
            .unwrap_or(self.conversations_root.as_path())
    }

    fn workspace_files_root_for_id(&self, workspace_id: &str) -> PathBuf {
        self.workdir_root()
            .join("workspaces")
            .join(workspace_id)
            .join("files")
    }

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
        validate_conversation_id(&address.conversation_id)?;
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
                foreground_actor: None,
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

    pub fn list_snapshots(&self) -> Vec<ConversationSnapshot> {
        self.conversations
            .values()
            .map(ConversationState::snapshot)
            .collect()
    }

    fn ensure_state_mut(&mut self, address: &ChannelAddress) -> Result<&mut ConversationState> {
        let key = address.session_key();
        if !self.conversations.contains_key(&key) {
            self.ensure_conversation(address)?;
        }
        self.conversations
            .get_mut(&key)
            .ok_or_else(|| anyhow!("missing conversation {}", key))
    }

    pub fn ensure_foreground_actor(
        &mut self,
        address: &ChannelAddress,
        sessions: &mut SessionManager,
    ) -> Result<SessionActorRef> {
        let state = self.ensure_state_mut(address)?;
        if let Some(actor) = state.foreground_actor.clone() {
            return Ok(actor);
        }
        let actor = match state.settings.workspace_id.as_deref() {
            Some(workspace_id) => {
                sessions.ensure_foreground_in_workspace_actor(address, workspace_id)?
            }
            None => sessions.ensure_foreground_actor(address)?,
        };
        let session = actor.snapshot()?;
        if state.settings.workspace_id.as_deref() != Some(session.workspace_id.as_str()) {
            state.settings.workspace_id = Some(session.workspace_id.clone());
            state.persist()?;
        }
        state.foreground_actor = Some(actor.clone());
        Ok(actor)
    }

    pub fn resolve_attachment_storage_dir(
        &mut self,
        address: &ChannelAddress,
        sessions: &mut SessionManager,
    ) -> Result<PathBuf> {
        let workspace_id = self
            .ensure_state_mut(address)?
            .settings
            .workspace_id
            .clone();
        if let Some(workspace_id) = workspace_id.as_deref() {
            let attachments_dir = self
                .workspace_files_root_for_id(workspace_id)
                .join("upload");
            fs::create_dir_all(&attachments_dir)
                .with_context(|| format!("failed to create {}", attachments_dir.display()))?;
            return Ok(attachments_dir);
        }
        let actor = self.ensure_foreground_actor(address, sessions)?;
        Ok(actor.snapshot()?.attachments_dir)
    }

    pub fn resolve_foreground_actor(
        &mut self,
        address: &ChannelAddress,
        sessions: &mut SessionManager,
    ) -> Result<Option<SessionActorRef>> {
        let key = address.session_key();
        let Some(state) = self.conversations.get_mut(&key) else {
            return Ok(None);
        };
        if let Some(actor) = state.foreground_actor.clone() {
            return Ok(Some(actor));
        }
        let Ok(actor) = sessions.resolve_foreground_by_address(address) else {
            return Ok(None);
        };
        state.foreground_actor = Some(actor.clone());
        Ok(Some(actor))
    }

    pub fn clear_foreground_actor(&mut self, address: &ChannelAddress) {
        if let Some(state) = self.conversations.get_mut(&address.session_key()) {
            state.foreground_actor = None;
        }
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
        let state = self.ensure_state_mut(address)?;
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
        let state = self.ensure_state_mut(address)?;
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
        let state = self.ensure_state_mut(address)?;
        state.settings.sandbox_mode = sandbox_mode;
        state.persist()?;
        Ok(state.snapshot())
    }

    pub fn set_reasoning_effort(
        &mut self,
        address: &ChannelAddress,
        reasoning_effort: Option<String>,
    ) -> Result<ConversationSnapshot> {
        let state = self.ensure_state_mut(address)?;
        state.settings.reasoning_effort = reasoning_effort;
        state.persist()?;
        Ok(state.snapshot())
    }

    pub fn set_context_compaction_enabled(
        &mut self,
        address: &ChannelAddress,
        enabled: Option<bool>,
    ) -> Result<ConversationSnapshot> {
        let state = self.ensure_state_mut(address)?;
        state.settings.context_compaction_enabled = enabled;
        state.persist()?;
        Ok(state.snapshot())
    }

    pub fn set_workspace_id(
        &mut self,
        address: &ChannelAddress,
        workspace_id: Option<String>,
    ) -> Result<ConversationSnapshot> {
        let state = self.ensure_state_mut(address)?;
        if state.settings.workspace_id != workspace_id {
            state.foreground_actor = None;
        }
        state.settings.workspace_id = workspace_id;
        state.persist()?;
        Ok(state.snapshot())
    }

    pub fn rotate_chat_version_id(
        &mut self,
        address: &ChannelAddress,
    ) -> Result<ConversationSnapshot> {
        let state = self.ensure_state_mut(address)?;
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
        let state = self.ensure_state_mut(address)?;
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
        let state = self.ensure_state_mut(address)?;
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
        let state = self.ensure_state_mut(address)?;
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

    pub fn add_local_mount(
        &mut self,
        address: &ChannelAddress,
        path: PathBuf,
    ) -> Result<ConversationSnapshot> {
        let state = self.ensure_state_mut(address)?;
        state.settings.local_mounts.retain(|item| item.path != path);
        state.settings.local_mounts.push(LocalMount { path });
        state.settings.chat_version_id = Uuid::new_v4();
        state.persist()?;
        Ok(state.snapshot())
    }

    pub fn set_remote_execution(
        &mut self,
        address: &ChannelAddress,
        binding: Option<RemoteExecutionBinding>,
    ) -> Result<ConversationSnapshot> {
        let state = self.ensure_state_mut(address)?;
        state.settings.remote_execution = binding;
        state.settings.remote_workpaths.clear();
        state.settings.chat_version_id = Uuid::new_v4();
        state.persist()?;
        Ok(state.snapshot())
    }
}

pub async fn prepare_incoming_conversation_message(
    attachments_dir: Option<&Path>,
    mut incoming: IncomingMessage,
) -> Result<IncomingMessage> {
    if let Some(attachments_dir) = attachments_dir
        && !incoming.attachments.is_empty()
    {
        let mut stored_attachments = materialize_conversation_attachments(
            attachments_dir,
            std::mem::take(&mut incoming.attachments),
        )
        .await?;
        incoming.stored_attachments.append(&mut stored_attachments);
    }
    Ok(incoming)
}

pub fn resolve_local_mount_path(raw: &str, base_dir: &Path) -> Result<PathBuf> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Err(anyhow!("mount path must be a non-empty local directory"));
    }
    if trimmed.chars().any(char::is_control) {
        return Err(anyhow!("mount path must not contain control characters"));
    }
    let expanded = expand_home_path(trimmed);
    let candidate = if expanded.is_absolute() {
        expanded
    } else {
        base_dir.join(expanded)
    };
    let canonical = fs::canonicalize(&candidate)
        .with_context(|| format!("failed to resolve mount path {}", candidate.display()))?;
    if !canonical.is_dir() {
        return Err(anyhow!(
            "mount path must be an existing local directory: {}",
            canonical.display()
        ));
    }
    Ok(canonical)
}

fn expand_home_path(path: &str) -> PathBuf {
    if path == "~" {
        return std::env::var_os("HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from(path));
    }
    if let Some(rest) = path.strip_prefix("~/")
        && let Some(home) = std::env::var_os("HOME")
    {
        return PathBuf::from(home).join(rest);
    }
    PathBuf::from(path)
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
                foreground_actor: None,
            },
        );
    }
    Ok(conversations)
}

pub async fn materialize_conversation_attachments(
    attachments_dir: &Path,
    attachments: Vec<PendingAttachment>,
) -> Result<Vec<StoredAttachment>> {
    let mut stored = Vec::with_capacity(attachments.len());
    for attachment in attachments {
        let item = attachment.materialize(attachments_dir).await?;
        let normalized = normalize_stored_attachment_for_persistence(item)?;
        tracing::info!(
            log_stream = "conversation",
            kind = "attachment_materialized",
            attachment_id = %normalized.id,
            path = %normalized.path.display(),
            size_bytes = normalized.size_bytes,
            "attachment persisted to conversation session storage"
        );
        stored.push(normalized);
    }
    Ok(stored)
}

#[cfg(test)]
mod tests {
    use super::ConversationManager;
    use crate::channel::{LocalFileAttachmentSource, PendingAttachment};
    use crate::config::SandboxMode;
    use crate::domain::{AttachmentKind, ChannelAddress};
    use crate::session::SessionManager;
    use crate::workspace::WorkspaceManager;
    use std::sync::Arc;
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
    fn conversation_owns_foreground_actor_reference() {
        let temp_dir = TempDir::new().unwrap();
        let address = test_address();
        let workspace_manager = WorkspaceManager::load_or_create(temp_dir.path()).unwrap();
        let mut sessions = SessionManager::new(temp_dir.path(), workspace_manager).unwrap();
        let mut conversations = ConversationManager::new(temp_dir.path()).unwrap();

        let first = conversations
            .ensure_foreground_actor(&address, &mut sessions)
            .unwrap();
        let first_snapshot = first.snapshot().unwrap();
        let second = conversations
            .ensure_foreground_actor(&address, &mut sessions)
            .unwrap();
        let conversation = conversations.get_snapshot(&address).unwrap();

        assert!(first.ptr_eq(&second));
        assert_eq!(
            conversation.settings.workspace_id.as_deref(),
            Some(first_snapshot.workspace_id.as_str())
        );

        conversations.clear_foreground_actor(&address);
        let resolved = conversations
            .resolve_foreground_actor(&address, &mut sessions)
            .unwrap()
            .expect("foreground actor should resolve from session registry");
        assert!(first.ptr_eq(&resolved));
    }

    #[test]
    fn resolves_attachment_storage_dir_from_conversation_workspace_without_session() {
        let temp_dir = TempDir::new().unwrap();
        let address = test_address();
        let workspace_manager = WorkspaceManager::load_or_create(temp_dir.path()).unwrap();
        let mut sessions = SessionManager::new(temp_dir.path(), workspace_manager).unwrap();
        let mut conversations = ConversationManager::new(temp_dir.path()).unwrap();

        conversations
            .set_workspace_id(&address, Some("workspace-1".to_string()))
            .unwrap();

        let attachments_dir = conversations
            .resolve_attachment_storage_dir(&address, &mut sessions)
            .unwrap();

        assert_eq!(
            attachments_dir,
            temp_dir
                .path()
                .join("workspaces")
                .join("workspace-1")
                .join("files")
                .join("upload")
        );
        assert!(attachments_dir.is_dir());
        assert!(sessions.get_snapshot(&address).is_none());
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
        let mount_dir = temp_dir.path().join("mounted");
        std::fs::create_dir_all(&mount_dir).unwrap();
        manager
            .add_local_mount(&address, mount_dir.clone())
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
        assert_eq!(snapshot.settings.local_mounts.len(), 1);
        assert_eq!(snapshot.settings.local_mounts[0].path, mount_dir);
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
    fn resolves_local_mount_paths_against_workspace_root() {
        let temp_dir = TempDir::new().unwrap();
        let workspace = temp_dir.path().join("workspace");
        let mount = workspace.join("data");
        std::fs::create_dir_all(&mount).unwrap();

        let resolved = super::resolve_local_mount_path("data", &workspace).unwrap();

        assert_eq!(resolved, mount.canonicalize().unwrap());
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

    #[test]
    fn rejects_invalid_conversation_id() {
        let temp_dir = TempDir::new().unwrap();
        let mut manager = ConversationManager::new(temp_dir.path()).unwrap();
        let address = ChannelAddress {
            channel_id: "web".to_string(),
            conversation_id: "..".to_string(),
            user_id: Some("web-user".to_string()),
            display_name: Some("Web User".to_string()),
        };

        let error = manager.ensure_conversation(&address).unwrap_err();
        assert!(
            error
                .to_string()
                .contains("conversation_id must contain only ASCII letters")
        );
    }

    fn tiny_tiff_bytes() -> Vec<u8> {
        let image = image::ImageBuffer::from_pixel(1, 1, image::Rgba([12, 34, 56, 255]));
        let mut bytes = Vec::new();
        image::DynamicImage::ImageRgba8(image)
            .write_to(
                &mut std::io::Cursor::new(&mut bytes),
                image::ImageFormat::Tiff,
            )
            .unwrap();
        bytes
    }

    #[tokio::test]
    async fn materialized_attachments_normalize_unsupported_images_for_persistence() {
        let temp_dir = TempDir::new().unwrap();
        let source = temp_dir.path().join("photo.tiff");
        std::fs::write(&source, tiny_tiff_bytes()).unwrap();
        let attachments_dir = temp_dir.path().join("upload");

        let stored = super::materialize_conversation_attachments(
            &attachments_dir,
            vec![PendingAttachment::new(
                AttachmentKind::Image,
                Some("photo.tiff".to_string()),
                Some("image/tiff".to_string()),
                None,
                Arc::new(LocalFileAttachmentSource::new(&source)),
            )],
        )
        .await
        .unwrap();

        assert_eq!(stored.len(), 1);
        assert_eq!(stored[0].media_type.as_deref(), Some("image/png"));
        assert_eq!(
            stored[0].path.extension().and_then(|value| value.to_str()),
            Some("png")
        );
        assert!(stored[0].path.is_file());
    }
}
