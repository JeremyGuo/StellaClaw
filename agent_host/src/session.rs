use crate::domain::{ChannelAddress, MessageRole, SessionMessage, StoredAttachment};
use crate::workspace::WorkspaceManager;
use agent_frame::{ChatMessage, SessionCompactionStats, TokenUsage};
use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use tracing::{info, warn};
use uuid::Uuid;

#[derive(Clone, Debug)]
pub struct SessionSnapshot {
    pub id: Uuid,
    pub agent_id: Uuid,
    pub address: ChannelAddress,
    pub root_dir: PathBuf,
    pub attachments_dir: PathBuf,
    pub workspace_id: String,
    pub workspace_root: PathBuf,
    pub message_count: usize,
    pub agent_message_count: usize,
    pub agent_messages: Vec<ChatMessage>,
    pub last_agent_returned_at: Option<DateTime<Utc>>,
    pub last_compacted_at: Option<DateTime<Utc>>,
    pub turn_count: u64,
    pub last_compacted_turn_count: u64,
    pub cumulative_usage: TokenUsage,
    pub cumulative_compaction: SessionCompactionStats,
    pub api_timeout_override_seconds: Option<f64>,
    pub pending_workspace_summary: bool,
    pub close_after_summary: bool,
}

#[derive(Debug)]
struct Session {
    id: Uuid,
    agent_id: Uuid,
    address: ChannelAddress,
    root_dir: PathBuf,
    attachments_dir: PathBuf,
    workspace_id: String,
    workspace_root: PathBuf,
    history: Vec<SessionMessage>,
    agent_messages: Vec<ChatMessage>,
    last_agent_returned_at: Option<DateTime<Utc>>,
    last_compacted_at: Option<DateTime<Utc>>,
    turn_count: u64,
    last_compacted_turn_count: u64,
    cumulative_usage: TokenUsage,
    cumulative_compaction: SessionCompactionStats,
    api_timeout_override_seconds: Option<f64>,
    pending_workspace_summary: bool,
    close_after_summary: bool,
    closed_at: Option<DateTime<Utc>>,
}

impl Session {
    fn state_path(&self) -> PathBuf {
        self.root_dir.join("session.json")
    }

    fn snapshot(&self) -> SessionSnapshot {
        SessionSnapshot {
            id: self.id,
            agent_id: self.agent_id,
            address: self.address.clone(),
            root_dir: self.root_dir.clone(),
            attachments_dir: self.attachments_dir.clone(),
            workspace_id: self.workspace_id.clone(),
            workspace_root: self.workspace_root.clone(),
            message_count: self.history.len(),
            agent_message_count: self.agent_messages.len(),
            agent_messages: self.agent_messages.clone(),
            last_agent_returned_at: self.last_agent_returned_at,
            last_compacted_at: self.last_compacted_at,
            turn_count: self.turn_count,
            last_compacted_turn_count: self.last_compacted_turn_count,
            cumulative_usage: self.cumulative_usage.clone(),
            cumulative_compaction: self.cumulative_compaction.clone(),
            api_timeout_override_seconds: self.api_timeout_override_seconds,
            pending_workspace_summary: self.pending_workspace_summary,
            close_after_summary: self.close_after_summary,
        }
    }

    fn push_message(
        &mut self,
        role: MessageRole,
        text: Option<String>,
        attachments: Vec<StoredAttachment>,
    ) {
        self.history.push(SessionMessage {
            role,
            text,
            attachments,
        });
    }

    fn persist(&self) -> Result<()> {
        let state = PersistedSession {
            id: self.id,
            agent_id: self.agent_id,
            address: self.address.clone(),
            workspace_id: Some(self.workspace_id.clone()),
            history: self.history.clone(),
            agent_messages: self.agent_messages.clone(),
            last_agent_returned_at: self.last_agent_returned_at,
            last_compacted_at: self.last_compacted_at,
            turn_count: self.turn_count,
            last_compacted_turn_count: self.last_compacted_turn_count,
            cumulative_usage: self.cumulative_usage.clone(),
            cumulative_compaction: self.cumulative_compaction.clone(),
            api_timeout_override_seconds: self.api_timeout_override_seconds,
            pending_workspace_summary: self.pending_workspace_summary,
            close_after_summary: self.close_after_summary,
            closed_at: self.closed_at,
        };
        let raw =
            serde_json::to_string_pretty(&state).context("failed to serialize session state")?;
        fs::write(self.state_path(), raw)
            .with_context(|| format!("failed to write {}", self.state_path().display()))
    }

    fn from_persisted(
        root_dir: PathBuf,
        persisted: PersistedSession,
        workspace_id: String,
        workspace_root: PathBuf,
    ) -> Result<Self> {
        fs::create_dir_all(&root_dir)
            .with_context(|| format!("failed to create {}", root_dir.display()))?;
        let attachments_dir = workspace_root.join("upload");
        fs::create_dir_all(&attachments_dir)
            .with_context(|| format!("failed to create {}", attachments_dir.display()))?;
        Ok(Self {
            id: persisted.id,
            agent_id: persisted.agent_id,
            address: persisted.address,
            root_dir,
            attachments_dir,
            workspace_id,
            workspace_root,
            history: persisted.history,
            agent_messages: persisted.agent_messages,
            last_agent_returned_at: persisted.last_agent_returned_at,
            last_compacted_at: persisted.last_compacted_at,
            turn_count: persisted.turn_count,
            last_compacted_turn_count: persisted.last_compacted_turn_count,
            cumulative_usage: persisted.cumulative_usage,
            cumulative_compaction: persisted.cumulative_compaction,
            api_timeout_override_seconds: persisted.api_timeout_override_seconds,
            pending_workspace_summary: persisted.pending_workspace_summary,
            close_after_summary: persisted.close_after_summary,
            closed_at: persisted.closed_at,
        })
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct PersistedSession {
    id: Uuid,
    agent_id: Uuid,
    address: ChannelAddress,
    #[serde(default)]
    workspace_id: Option<String>,
    #[serde(default)]
    history: Vec<SessionMessage>,
    #[serde(default)]
    agent_messages: Vec<ChatMessage>,
    #[serde(default)]
    last_agent_returned_at: Option<DateTime<Utc>>,
    #[serde(default)]
    last_compacted_at: Option<DateTime<Utc>>,
    #[serde(default)]
    turn_count: u64,
    #[serde(default)]
    last_compacted_turn_count: u64,
    #[serde(default)]
    cumulative_usage: TokenUsage,
    #[serde(default)]
    cumulative_compaction: SessionCompactionStats,
    #[serde(default)]
    api_timeout_override_seconds: Option<f64>,
    #[serde(default)]
    pending_workspace_summary: bool,
    #[serde(default)]
    close_after_summary: bool,
    #[serde(default)]
    closed_at: Option<DateTime<Utc>>,
}

pub struct SessionManager {
    sessions_root: PathBuf,
    workspace_manager: WorkspaceManager,
    foreground_sessions: HashMap<String, Session>,
}

impl SessionManager {
    pub fn new(workdir: impl AsRef<Path>, workspace_manager: WorkspaceManager) -> Result<Self> {
        let sessions_root = workdir.as_ref().join("sessions");
        fs::create_dir_all(&sessions_root)
            .with_context(|| format!("failed to create {}", sessions_root.display()))?;
        let foreground_sessions = load_persisted_sessions(&sessions_root, &workspace_manager)?;
        Ok(Self {
            sessions_root,
            workspace_manager,
            foreground_sessions,
        })
    }

    pub fn ensure_foreground(&mut self, address: &ChannelAddress) -> Result<SessionSnapshot> {
        let key = address.session_key();
        if !self.foreground_sessions.contains_key(&key) {
            let session = self.create_session(address)?;
            info!(
                log_stream = "session",
                log_key = %session.id,
                kind = "session_created",
                channel_id = %address.channel_id,
                conversation_id = %address.conversation_id,
                root_dir = %session.root_dir.display(),
                "created foreground session"
            );
            self.foreground_sessions.insert(key.clone(), session);
        }
        Ok(self
            .foreground_sessions
            .get(&key)
            .expect("foreground session inserted")
            .snapshot())
    }

    pub fn reset_foreground(&mut self, address: &ChannelAddress) -> Result<SessionSnapshot> {
        self.destroy_foreground(address)?;
        self.ensure_foreground(address)
    }

    pub fn reset_foreground_to_workspace(
        &mut self,
        address: &ChannelAddress,
        workspace_id: &str,
    ) -> Result<SessionSnapshot> {
        self.destroy_foreground(address)?;
        self.ensure_foreground_in_workspace(address, workspace_id)
    }

    pub fn ensure_foreground_in_workspace(
        &mut self,
        address: &ChannelAddress,
        workspace_id: &str,
    ) -> Result<SessionSnapshot> {
        let key = address.session_key();
        if !self.foreground_sessions.contains_key(&key) {
            let session = self.create_session_with_workspace(address, workspace_id)?;
            info!(
                log_stream = "session",
                log_key = %session.id,
                kind = "session_created",
                channel_id = %address.channel_id,
                conversation_id = %address.conversation_id,
                root_dir = %session.root_dir.display(),
                workspace_id = %session.workspace_id,
                "created foreground session in existing workspace"
            );
            self.foreground_sessions.insert(key.clone(), session);
        }
        Ok(self
            .foreground_sessions
            .get(&key)
            .expect("foreground session inserted")
            .snapshot())
    }

    pub fn destroy_foreground(&mut self, address: &ChannelAddress) -> Result<()> {
        let key = address.session_key();
        if let Some(mut session) = self.foreground_sessions.remove(&key) {
            info!(
                log_stream = "session",
                log_key = %session.id,
                kind = "session_destroying",
                root_dir = %session.root_dir.display(),
                "destroying foreground session"
            );
            session.closed_at = Some(Utc::now());
            session.persist()?;
            info!(
                log_stream = "session",
                log_key = %session.id,
                kind = "session_destroyed",
                root_dir = %session.root_dir.display(),
                "foreground session closed and retained on disk"
            );
        }
        Ok(())
    }

    pub fn append_user_message(
        &mut self,
        address: &ChannelAddress,
        text: Option<String>,
        attachments: Vec<StoredAttachment>,
    ) -> Result<()> {
        self.append_message(address, MessageRole::User, text, attachments)
    }

    pub fn append_assistant_message(
        &mut self,
        address: &ChannelAddress,
        text: Option<String>,
        attachments: Vec<StoredAttachment>,
    ) -> Result<()> {
        self.append_message(address, MessageRole::Assistant, text, attachments)
    }

    pub fn get_snapshot(&self, address: &ChannelAddress) -> Option<SessionSnapshot> {
        self.foreground_sessions
            .get(&address.session_key())
            .map(Session::snapshot)
    }

    pub fn list_foreground_snapshots(&self) -> Vec<SessionSnapshot> {
        self.foreground_sessions
            .values()
            .map(Session::snapshot)
            .collect()
    }

    pub fn has_active_workspace(&self, workspace_id: &str) -> bool {
        self.foreground_sessions
            .values()
            .any(|session| session.workspace_id == workspace_id)
    }

    pub fn pending_workspace_summary_snapshots(&self) -> Vec<SessionSnapshot> {
        self.foreground_sessions
            .values()
            .filter(|session| session.pending_workspace_summary)
            .map(Session::snapshot)
            .collect()
    }

    pub fn mark_workspace_summary_state(
        &mut self,
        address: &ChannelAddress,
        pending: bool,
        close_after_summary: bool,
    ) -> Result<()> {
        let key = address.session_key();
        let session = self
            .foreground_sessions
            .get_mut(&key)
            .with_context(|| format!("no active session for {}", key))?;
        session.pending_workspace_summary = pending;
        session.close_after_summary = close_after_summary;
        session.persist()?;
        Ok(())
    }

    pub fn update_agent_messages(
        &mut self,
        address: &ChannelAddress,
        messages: Vec<ChatMessage>,
    ) -> Result<()> {
        let key = address.session_key();
        let session = self
            .foreground_sessions
            .get_mut(&key)
            .with_context(|| format!("no active session for {}", key))?;
        session.agent_messages = messages;
        info!(
            log_stream = "session",
            log_key = %session.id,
            kind = "agent_messages_updated",
            agent_message_count = session.agent_messages.len() as u64,
            "updated agent_frame message history"
        );
        session.persist()?;
        Ok(())
    }

    pub fn set_api_timeout_override(
        &mut self,
        address: &ChannelAddress,
        timeout_seconds: Option<f64>,
    ) -> Result<()> {
        let key = address.session_key();
        let session = self
            .foreground_sessions
            .get_mut(&key)
            .with_context(|| format!("no active session for {}", key))?;
        session.api_timeout_override_seconds = timeout_seconds;
        session.persist()?;
        Ok(())
    }

    pub fn record_agent_turn(
        &mut self,
        address: &ChannelAddress,
        messages: Vec<ChatMessage>,
        usage: &TokenUsage,
        compaction: &SessionCompactionStats,
    ) -> Result<()> {
        let key = address.session_key();
        let session = self
            .foreground_sessions
            .get_mut(&key)
            .with_context(|| format!("no active session for {}", key))?;
        session.agent_messages = messages;
        session.last_agent_returned_at = Some(Utc::now());
        session.turn_count = session.turn_count.saturating_add(1);
        session.cumulative_usage.add_assign(usage);
        session.cumulative_compaction.run_count = session
            .cumulative_compaction
            .run_count
            .saturating_add(compaction.run_count);
        session.cumulative_compaction.compacted_run_count = session
            .cumulative_compaction
            .compacted_run_count
            .saturating_add(compaction.compacted_run_count);
        session.cumulative_compaction.estimated_tokens_before = session
            .cumulative_compaction
            .estimated_tokens_before
            .saturating_add(compaction.estimated_tokens_before);
        session.cumulative_compaction.estimated_tokens_after = session
            .cumulative_compaction
            .estimated_tokens_after
            .saturating_add(compaction.estimated_tokens_after);
        session
            .cumulative_compaction
            .usage
            .add_assign(&compaction.usage);
        info!(
            log_stream = "session",
            log_key = %session.id,
            kind = "agent_turn_recorded",
            agent_message_count = session.agent_messages.len() as u64,
            turn_count = session.turn_count,
            "recorded successful agent turn"
        );
        session.persist()?;
        Ok(())
    }

    pub fn record_idle_compaction(
        &mut self,
        address: &ChannelAddress,
        messages: Vec<ChatMessage>,
        compaction: &SessionCompactionStats,
    ) -> Result<()> {
        let key = address.session_key();
        let session = self
            .foreground_sessions
            .get_mut(&key)
            .with_context(|| format!("no active session for {}", key))?;
        session.agent_messages = messages;
        session.last_compacted_at = Some(Utc::now());
        session.last_compacted_turn_count = session.turn_count;
        session.cumulative_compaction.run_count = session
            .cumulative_compaction
            .run_count
            .saturating_add(compaction.run_count);
        session.cumulative_compaction.compacted_run_count = session
            .cumulative_compaction
            .compacted_run_count
            .saturating_add(compaction.compacted_run_count);
        session.cumulative_compaction.estimated_tokens_before = session
            .cumulative_compaction
            .estimated_tokens_before
            .saturating_add(compaction.estimated_tokens_before);
        session.cumulative_compaction.estimated_tokens_after = session
            .cumulative_compaction
            .estimated_tokens_after
            .saturating_add(compaction.estimated_tokens_after);
        session
            .cumulative_compaction
            .usage
            .add_assign(&compaction.usage);
        info!(
            log_stream = "session",
            log_key = %session.id,
            kind = "idle_context_compacted",
            agent_message_count = session.agent_messages.len() as u64,
            turn_count = session.turn_count,
            "persisted idle context compaction"
        );
        session.persist()?;
        Ok(())
    }

    fn append_message(
        &mut self,
        address: &ChannelAddress,
        role: MessageRole,
        text: Option<String>,
        attachments: Vec<StoredAttachment>,
    ) -> Result<()> {
        let key = address.session_key();
        let session = self
            .foreground_sessions
            .get_mut(&key)
            .with_context(|| format!("no active session for {}", key))?;
        let attachment_count = attachments.len();
        session.push_message(role, text, attachments);
        info!(
            log_stream = "session",
            log_key = %session.id,
            kind = "message_appended",
            role = ?role,
            message_count = session.history.len() as u64,
            attachment_count = attachment_count as u64,
            "appended message to session history"
        );
        session.persist()?;
        Ok(())
    }

    fn create_session(&self, address: &ChannelAddress) -> Result<Session> {
        let session_id = Uuid::new_v4();
        let agent_id = Uuid::new_v4();
        let workspace = self
            .workspace_manager
            .create_workspace(agent_id, session_id, None)?;
        let root_dir = self.sessions_root.join(session_id.to_string());
        fs::create_dir_all(&root_dir)
            .with_context(|| format!("failed to create session root {}", root_dir.display()))?;
        let attachments_dir = workspace.files_dir.join("upload");
        fs::create_dir_all(&attachments_dir).with_context(|| {
            format!(
                "failed to create workspace upload directory {}",
                attachments_dir.display()
            )
        })?;
        let session = Session {
            id: session_id,
            agent_id,
            address: address.clone(),
            root_dir,
            attachments_dir,
            workspace_id: workspace.id,
            workspace_root: workspace.files_dir,
            history: Vec::new(),
            agent_messages: Vec::new(),
            last_agent_returned_at: None,
            last_compacted_at: None,
            turn_count: 0,
            last_compacted_turn_count: 0,
            cumulative_usage: TokenUsage::default(),
            cumulative_compaction: SessionCompactionStats::default(),
            api_timeout_override_seconds: None,
            pending_workspace_summary: false,
            close_after_summary: false,
            closed_at: None,
        };
        session.persist()?;
        Ok(session)
    }

    fn create_session_with_workspace(
        &self,
        address: &ChannelAddress,
        workspace_id: &str,
    ) -> Result<Session> {
        let session_id = Uuid::new_v4();
        let agent_id = Uuid::new_v4();
        let workspace = self
            .workspace_manager
            .ensure_workspace_exists(workspace_id)?;
        let root_dir = self.sessions_root.join(session_id.to_string());
        fs::create_dir_all(&root_dir)
            .with_context(|| format!("failed to create session root {}", root_dir.display()))?;
        let attachments_dir = workspace.files_dir.join("upload");
        fs::create_dir_all(&attachments_dir).with_context(|| {
            format!(
                "failed to create workspace upload directory {}",
                attachments_dir.display()
            )
        })?;
        let session = Session {
            id: session_id,
            agent_id,
            address: address.clone(),
            root_dir,
            attachments_dir,
            workspace_id: workspace.id,
            workspace_root: workspace.files_dir,
            history: Vec::new(),
            agent_messages: Vec::new(),
            last_agent_returned_at: None,
            last_compacted_at: None,
            turn_count: 0,
            last_compacted_turn_count: 0,
            cumulative_usage: TokenUsage::default(),
            cumulative_compaction: SessionCompactionStats::default(),
            api_timeout_override_seconds: None,
            pending_workspace_summary: false,
            close_after_summary: false,
            closed_at: None,
        };
        session.persist()?;
        Ok(session)
    }
}

fn load_persisted_sessions(
    sessions_root: &Path,
    workspace_manager: &WorkspaceManager,
) -> Result<HashMap<String, Session>> {
    let mut sessions = HashMap::new();
    for entry in fs::read_dir(sessions_root)
        .with_context(|| format!("failed to read {}", sessions_root.display()))?
    {
        let entry = entry?;
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let state_path = path.join("session.json");
        if !state_path.exists() {
            continue;
        }
        match load_single_session(&path, &state_path, workspace_manager) {
            Ok(Some(session)) => {
                let key = session.address.session_key();
                info!(
                    log_stream = "session",
                    log_key = %session.id,
                    kind = "session_restored",
                    channel_id = %session.address.channel_id,
                    conversation_id = %session.address.conversation_id,
                    root_dir = %session.root_dir.display(),
                    "restored persisted foreground session"
                );
                sessions.insert(key, session);
            }
            Ok(None) => {
                info!(
                    log_stream = "session",
                    kind = "session_restore_skipped",
                    root_dir = %path.display(),
                    "skipping closed persisted session"
                );
            }
            Err(error) => {
                warn!(
                    log_stream = "session",
                    kind = "session_restore_failed",
                    root_dir = %path.display(),
                    error = %format!("{error:#}"),
                    "failed to restore persisted session; skipping"
                );
            }
        }
    }
    Ok(sessions)
}

fn load_single_session(
    root_dir: &Path,
    state_path: &Path,
    workspace_manager: &WorkspaceManager,
) -> Result<Option<Session>> {
    let raw = fs::read_to_string(state_path)
        .with_context(|| format!("failed to read {}", state_path.display()))?;
    let persisted: PersistedSession =
        serde_json::from_str(&raw).context("failed to parse session state")?;
    if persisted.closed_at.is_some() {
        return Ok(None);
    }
    let (workspace_id, workspace_root) = match persisted.workspace_id.as_deref() {
        Some(workspace_id) => (
            workspace_id.to_string(),
            workspace_manager
                .ensure_workspace_exists(workspace_id)?
                .files_dir,
        ),
        None => {
            let workspace = workspace_manager.create_workspace(
                persisted.agent_id,
                persisted.id,
                Some(&format!("migrated-{}", &persisted.id.to_string()[..8])),
            )?;
            info!(
                log_stream = "session",
                log_key = %persisted.id,
                kind = "session_workspace_migrated",
                workspace_id = %workspace.id,
                root_dir = %root_dir.display(),
                "migrated legacy session to a dedicated workspace"
            );
            (workspace.id, workspace.files_dir)
        }
    };
    let session = Session::from_persisted(
        root_dir.to_path_buf(),
        persisted,
        workspace_id,
        workspace_root,
    )?;
    session.persist()?;
    Ok(Some(session))
}
