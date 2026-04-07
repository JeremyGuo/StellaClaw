use crate::domain::{ChannelAddress, MessageRole, SessionMessage, StoredAttachment};
use crate::workspace::WorkspaceManager;
use agent_frame::{ChatMessage, ResponseCheckpoint, SessionCompactionStats, TokenUsage};
use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use tracing::{info, warn};
use uuid::Uuid;

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct SessionSkillState {
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub content: String,
    #[serde(default)]
    pub last_loaded_turn: Option<u64>,
}

#[derive(Clone, Debug)]
pub struct SessionSkillObservation {
    pub name: String,
    pub description: String,
    pub content: String,
}

#[derive(Clone, Debug)]
pub enum SkillChangeNotice {
    DescriptionChanged {
        name: String,
        description: String,
    },
    ContentChanged {
        name: String,
        description: String,
        content: String,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SharedProfileChangeNotice {
    UserUpdated,
    IdentityUpdated,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct SessionCheckpointData {
    #[serde(default)]
    pub history: Vec<SessionMessage>,
    #[serde(default)]
    pub agent_messages: Vec<ChatMessage>,
    #[serde(default)]
    pub last_agent_returned_at: Option<DateTime<Utc>>,
    #[serde(default)]
    pub last_compacted_at: Option<DateTime<Utc>>,
    #[serde(default)]
    pub turn_count: u64,
    #[serde(default)]
    pub last_compacted_turn_count: u64,
    #[serde(default)]
    pub cumulative_usage: TokenUsage,
    #[serde(default)]
    pub cumulative_compaction: SessionCompactionStats,
    #[serde(default)]
    pub api_timeout_override_seconds: Option<f64>,
    #[serde(default)]
    pub skill_states: HashMap<String, SessionSkillState>,
    #[serde(default)]
    pub seen_user_profile_version: Option<String>,
    #[serde(default)]
    pub seen_identity_profile_version: Option<String>,
    #[serde(default)]
    pub pending_user_profile_notice: bool,
    #[serde(default)]
    pub pending_identity_profile_notice: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PendingContinueState {
    pub model_key: String,
    #[serde(default)]
    pub resume_messages: Vec<ChatMessage>,
    #[serde(default)]
    pub original_user_text: Option<String>,
    #[serde(default)]
    pub original_attachments: Vec<StoredAttachment>,
    #[serde(default)]
    pub error_summary: String,
    #[serde(default)]
    pub progress_summary: String,
    #[serde(skip, default)]
    pub response_checkpoint: Option<ResponseCheckpoint>,
    pub failed_at: DateTime<Utc>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct IdleCompactionRetryState {
    #[serde(default)]
    pub error_summary: String,
    #[serde(default)]
    pub failed_at: Option<DateTime<Utc>>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct ZgentNativeSessionState {
    #[serde(default)]
    pub remote_session_id: Option<String>,
    #[serde(default)]
    pub model_key: Option<String>,
    #[serde(default)]
    pub context_window_current: Option<u32>,
    #[serde(default)]
    pub context_window_size: Option<u32>,
}

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
    pub skill_states: HashMap<String, SessionSkillState>,
    pub seen_user_profile_version: Option<String>,
    pub seen_identity_profile_version: Option<String>,
    pub idle_compaction_retry: Option<IdleCompactionRetryState>,
    pub zgent_native: Option<ZgentNativeSessionState>,
    pub pending_continue: Option<PendingContinueState>,
    pub response_checkpoint: Option<ResponseCheckpoint>,
    pub pending_workspace_summary: bool,
    pub close_after_summary: bool,
}

#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum SessionKind {
    #[default]
    Foreground,
    Background,
}

#[derive(Debug)]
struct Session {
    kind: SessionKind,
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
    skill_states: HashMap<String, SessionSkillState>,
    seen_user_profile_version: Option<String>,
    seen_identity_profile_version: Option<String>,
    pending_user_profile_notice: bool,
    pending_identity_profile_notice: bool,
    idle_compaction_retry: Option<IdleCompactionRetryState>,
    zgent_native: Option<ZgentNativeSessionState>,
    pending_continue: Option<PendingContinueState>,
    response_checkpoint: Option<ResponseCheckpoint>,
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
            skill_states: self.skill_states.clone(),
            seen_user_profile_version: self.seen_user_profile_version.clone(),
            seen_identity_profile_version: self.seen_identity_profile_version.clone(),
            idle_compaction_retry: self.idle_compaction_retry.clone(),
            zgent_native: self.zgent_native.clone(),
            pending_continue: self.pending_continue.clone(),
            response_checkpoint: self.response_checkpoint.clone(),
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
            kind: self.kind,
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
            skill_states: self.skill_states.clone(),
            seen_user_profile_version: self.seen_user_profile_version.clone(),
            seen_identity_profile_version: self.seen_identity_profile_version.clone(),
            pending_user_profile_notice: self.pending_user_profile_notice,
            pending_identity_profile_notice: self.pending_identity_profile_notice,
            idle_compaction_retry: self.idle_compaction_retry.clone(),
            zgent_native: self.zgent_native.clone(),
            pending_continue: self.pending_continue.clone(),
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
        let agent_messages = sanitize_persisted_agent_messages(persisted.agent_messages);
        let pending_continue = persisted.pending_continue.map(|mut pending| {
            pending.resume_messages = sanitize_persisted_agent_messages(pending.resume_messages);
            pending
        });
        Ok(Self {
            kind: persisted.kind,
            id: persisted.id,
            agent_id: persisted.agent_id,
            address: persisted.address,
            root_dir,
            attachments_dir,
            workspace_id,
            workspace_root,
            history: persisted.history,
            agent_messages,
            last_agent_returned_at: persisted.last_agent_returned_at,
            last_compacted_at: persisted.last_compacted_at,
            turn_count: persisted.turn_count,
            last_compacted_turn_count: persisted.last_compacted_turn_count,
            cumulative_usage: persisted.cumulative_usage,
            cumulative_compaction: persisted.cumulative_compaction,
            api_timeout_override_seconds: persisted.api_timeout_override_seconds,
            skill_states: persisted.skill_states,
            seen_user_profile_version: persisted.seen_user_profile_version,
            seen_identity_profile_version: persisted.seen_identity_profile_version,
            pending_user_profile_notice: persisted.pending_user_profile_notice,
            pending_identity_profile_notice: persisted.pending_identity_profile_notice,
            idle_compaction_retry: persisted.idle_compaction_retry,
            zgent_native: persisted.zgent_native,
            pending_continue,
            response_checkpoint: None,
            pending_workspace_summary: persisted.pending_workspace_summary,
            close_after_summary: persisted.close_after_summary,
            closed_at: persisted.closed_at,
        })
    }
}

fn sanitize_persisted_agent_messages(messages: Vec<ChatMessage>) -> Vec<ChatMessage> {
    let mut sanitized = Vec::new();
    let mut seen_leading_system = false;
    let mut leading = true;
    for message in messages {
        let is_system = message.role == "system";
        if leading {
            if is_system {
                if seen_leading_system {
                    continue;
                }
                seen_leading_system = true;
                sanitized.push(message);
                continue;
            }
            leading = false;
        }
        sanitized.push(message);
    }
    sanitized
}

fn record_turn(
    session: &mut Session,
    messages: Vec<ChatMessage>,
    usage: &TokenUsage,
    compaction: &SessionCompactionStats,
    response_checkpoint: Option<ResponseCheckpoint>,
    clear_pending_continue: bool,
    log_kind: &str,
) -> Result<()> {
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
    if clear_pending_continue {
        session.pending_continue = None;
    }
    session.response_checkpoint = response_checkpoint;
    session.idle_compaction_retry = None;
    info!(
        log_stream = "session",
        log_key = %session.id,
        kind = log_kind,
        agent_message_count = session.agent_messages.len() as u64,
        turn_count = session.turn_count,
        "recorded agent turn"
    );
    session.persist()?;
    Ok(())
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct PersistedSession {
    #[serde(default)]
    kind: SessionKind,
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
    skill_states: HashMap<String, SessionSkillState>,
    #[serde(default)]
    seen_user_profile_version: Option<String>,
    #[serde(default)]
    seen_identity_profile_version: Option<String>,
    #[serde(default)]
    pending_user_profile_notice: bool,
    #[serde(default)]
    pending_identity_profile_notice: bool,
    #[serde(default)]
    idle_compaction_retry: Option<IdleCompactionRetryState>,
    #[serde(default)]
    zgent_native: Option<ZgentNativeSessionState>,
    #[serde(default)]
    pending_continue: Option<PendingContinueState>,
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
    background_sessions: HashMap<Uuid, Session>,
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
            background_sessions: HashMap::new(),
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
            .chain(self.background_sessions.values())
            .any(|session| session.workspace_id == workspace_id)
    }

    pub fn create_background(
        &mut self,
        address: &ChannelAddress,
        agent_id: Uuid,
    ) -> Result<SessionSnapshot> {
        let session =
            self.create_session_with_kind(address, agent_id, None, SessionKind::Background)?;
        let snapshot = session.snapshot();
        self.background_sessions.insert(session.id, session);
        Ok(snapshot)
    }

    pub fn create_background_in_workspace(
        &mut self,
        address: &ChannelAddress,
        agent_id: Uuid,
        workspace_id: &str,
    ) -> Result<SessionSnapshot> {
        let session = self.create_session_with_kind(
            address,
            agent_id,
            Some(workspace_id),
            SessionKind::Background,
        )?;
        let snapshot = session.snapshot();
        self.background_sessions.insert(session.id, session);
        Ok(snapshot)
    }

    pub fn background_snapshot(&self, session_id: Uuid) -> Result<SessionSnapshot> {
        self.background_sessions
            .get(&session_id)
            .map(Session::snapshot)
            .with_context(|| format!("no active background session for {}", session_id))
    }

    pub fn close_background(&mut self, session_id: Uuid) -> Result<()> {
        if let Some(mut session) = self.background_sessions.remove(&session_id) {
            session.closed_at = Some(Utc::now());
            session.persist()?;
        }
        Ok(())
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

    pub fn export_checkpoint(&self, address: &ChannelAddress) -> Result<SessionCheckpointData> {
        let key = address.session_key();
        let session = self
            .foreground_sessions
            .get(&key)
            .with_context(|| format!("no active session for {}", key))?;
        Ok(SessionCheckpointData {
            history: session.history.clone(),
            agent_messages: session.agent_messages.clone(),
            last_agent_returned_at: session.last_agent_returned_at,
            last_compacted_at: session.last_compacted_at,
            turn_count: session.turn_count,
            last_compacted_turn_count: session.last_compacted_turn_count,
            cumulative_usage: session.cumulative_usage.clone(),
            cumulative_compaction: session.cumulative_compaction.clone(),
            api_timeout_override_seconds: session.api_timeout_override_seconds,
            skill_states: session.skill_states.clone(),
            seen_user_profile_version: session.seen_user_profile_version.clone(),
            seen_identity_profile_version: session.seen_identity_profile_version.clone(),
            pending_user_profile_notice: session.pending_user_profile_notice,
            pending_identity_profile_notice: session.pending_identity_profile_notice,
        })
    }

    pub fn restore_foreground_from_checkpoint(
        &mut self,
        address: &ChannelAddress,
        checkpoint: SessionCheckpointData,
        workspace_id: String,
        workspace_root: PathBuf,
    ) -> Result<SessionSnapshot> {
        self.destroy_foreground(address)?;
        let session_id = Uuid::new_v4();
        let agent_id = Uuid::new_v4();
        let root_dir = self.sessions_root.join(session_id.to_string());
        fs::create_dir_all(&root_dir)
            .with_context(|| format!("failed to create session root {}", root_dir.display()))?;
        let attachments_dir = workspace_root.join("upload");
        fs::create_dir_all(&attachments_dir)
            .with_context(|| format!("failed to create {}", attachments_dir.display()))?;
        let session = Session {
            kind: SessionKind::Foreground,
            id: session_id,
            agent_id,
            address: address.clone(),
            root_dir,
            attachments_dir,
            workspace_id,
            workspace_root,
            history: checkpoint.history,
            agent_messages: checkpoint.agent_messages,
            last_agent_returned_at: checkpoint.last_agent_returned_at,
            last_compacted_at: checkpoint.last_compacted_at,
            turn_count: checkpoint.turn_count,
            last_compacted_turn_count: checkpoint.last_compacted_turn_count,
            cumulative_usage: checkpoint.cumulative_usage,
            cumulative_compaction: checkpoint.cumulative_compaction,
            api_timeout_override_seconds: checkpoint.api_timeout_override_seconds,
            skill_states: checkpoint.skill_states,
            seen_user_profile_version: checkpoint.seen_user_profile_version,
            seen_identity_profile_version: checkpoint.seen_identity_profile_version,
            pending_user_profile_notice: checkpoint.pending_user_profile_notice,
            pending_identity_profile_notice: checkpoint.pending_identity_profile_notice,
            idle_compaction_retry: None,
            zgent_native: None,
            pending_continue: None,
            response_checkpoint: None,
            pending_workspace_summary: false,
            close_after_summary: false,
            closed_at: None,
        };
        session.persist()?;
        let key = address.session_key();
        self.foreground_sessions.insert(key.clone(), session);
        Ok(self
            .foreground_sessions
            .get(&key)
            .expect("foreground session inserted")
            .snapshot())
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
        session.response_checkpoint = None;
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

    pub fn pending_continue(
        &self,
        address: &ChannelAddress,
    ) -> Result<Option<PendingContinueState>> {
        let key = address.session_key();
        let session = self
            .foreground_sessions
            .get(&key)
            .with_context(|| format!("no active session for {}", key))?;
        Ok(session.pending_continue.clone())
    }

    pub fn zgent_native_state(
        &self,
        address: &ChannelAddress,
    ) -> Result<Option<ZgentNativeSessionState>> {
        let key = address.session_key();
        let session = self
            .foreground_sessions
            .get(&key)
            .with_context(|| format!("no active session for {}", key))?;
        Ok(session.zgent_native.clone())
    }

    pub fn set_zgent_native_state(
        &mut self,
        address: &ChannelAddress,
        zgent_native: Option<ZgentNativeSessionState>,
    ) -> Result<()> {
        let key = address.session_key();
        let session = self
            .foreground_sessions
            .get_mut(&key)
            .with_context(|| format!("no active session for {}", key))?;
        session.zgent_native = zgent_native;
        session.persist()?;
        Ok(())
    }

    pub fn set_pending_continue(
        &mut self,
        address: &ChannelAddress,
        pending_continue: Option<PendingContinueState>,
    ) -> Result<()> {
        let key = address.session_key();
        let session = self
            .foreground_sessions
            .get_mut(&key)
            .with_context(|| format!("no active session for {}", key))?;
        session.pending_continue = pending_continue;
        session.persist()?;
        Ok(())
    }

    pub fn observe_skill_changes(
        &mut self,
        address: &ChannelAddress,
        observed_skills: &[SessionSkillObservation],
    ) -> Result<Vec<SkillChangeNotice>> {
        let key = address.session_key();
        let session = self
            .foreground_sessions
            .get_mut(&key)
            .with_context(|| format!("no active session for {}", key))?;
        let mut notices = Vec::new();
        for observed in observed_skills {
            match session.skill_states.get_mut(&observed.name) {
                Some(state) => {
                    let description_changed = state.description != observed.description;
                    let content_changed = state.content != observed.content;
                    if description_changed {
                        notices.push(SkillChangeNotice::DescriptionChanged {
                            name: observed.name.clone(),
                            description: observed.description.clone(),
                        });
                    }
                    if content_changed
                        && state
                            .last_loaded_turn
                            .is_some_and(|turn| turn > session.last_compacted_turn_count)
                    {
                        notices.push(SkillChangeNotice::ContentChanged {
                            name: observed.name.clone(),
                            description: observed.description.clone(),
                            content: observed.content.clone(),
                        });
                    }
                    state.description = observed.description.clone();
                    state.content = observed.content.clone();
                }
                None => {
                    session.skill_states.insert(
                        observed.name.clone(),
                        SessionSkillState {
                            description: observed.description.clone(),
                            content: observed.content.clone(),
                            last_loaded_turn: None,
                        },
                    );
                }
            }
        }
        session.persist()?;
        Ok(notices)
    }

    pub fn observe_shared_profile_changes(
        &mut self,
        address: &ChannelAddress,
        user_profile_version: String,
        identity_profile_version: String,
    ) -> Result<Vec<SharedProfileChangeNotice>> {
        let key = address.session_key();
        let session = self
            .foreground_sessions
            .get_mut(&key)
            .with_context(|| format!("no active session for {}", key))?;
        let mut notices = Vec::new();

        match session.seen_user_profile_version.as_deref() {
            None => {
                session.seen_user_profile_version = Some(user_profile_version);
            }
            Some(previous) if previous != user_profile_version => {
                session.seen_user_profile_version = Some(user_profile_version);
                session.pending_user_profile_notice = true;
                notices.push(SharedProfileChangeNotice::UserUpdated);
            }
            Some(_) => {}
        }

        match session.seen_identity_profile_version.as_deref() {
            None => {
                session.seen_identity_profile_version = Some(identity_profile_version);
            }
            Some(previous) if previous != identity_profile_version => {
                session.seen_identity_profile_version = Some(identity_profile_version);
                session.pending_identity_profile_notice = true;
                notices.push(SharedProfileChangeNotice::IdentityUpdated);
            }
            Some(_) => {}
        }

        session.persist()?;
        Ok(notices)
    }

    pub fn stage_shared_profile_change_notices(
        &mut self,
        address: &ChannelAddress,
        notices: &[SharedProfileChangeNotice],
    ) -> Result<()> {
        if notices.is_empty() {
            return Ok(());
        }
        let key = address.session_key();
        let session = self
            .foreground_sessions
            .get_mut(&key)
            .with_context(|| format!("no active session for {}", key))?;
        for notice in notices {
            match notice {
                SharedProfileChangeNotice::UserUpdated => {
                    session.pending_user_profile_notice = true;
                }
                SharedProfileChangeNotice::IdentityUpdated => {
                    session.pending_identity_profile_notice = true;
                }
            }
        }
        session.persist()?;
        Ok(())
    }

    pub fn take_shared_profile_change_notices(
        &mut self,
        address: &ChannelAddress,
    ) -> Result<Vec<SharedProfileChangeNotice>> {
        let key = address.session_key();
        let session = self
            .foreground_sessions
            .get_mut(&key)
            .with_context(|| format!("no active session for {}", key))?;
        let mut notices = Vec::new();
        if session.pending_user_profile_notice {
            notices.push(SharedProfileChangeNotice::UserUpdated);
            session.pending_user_profile_notice = false;
        }
        if session.pending_identity_profile_notice {
            notices.push(SharedProfileChangeNotice::IdentityUpdated);
            session.pending_identity_profile_notice = false;
        }
        session.persist()?;
        Ok(notices)
    }

    pub fn mark_skills_loaded_current_turn(
        &mut self,
        address: &ChannelAddress,
        skill_names: &[String],
    ) -> Result<()> {
        if skill_names.is_empty() {
            return Ok(());
        }
        let key = address.session_key();
        let session = self
            .foreground_sessions
            .get_mut(&key)
            .with_context(|| format!("no active session for {}", key))?;
        for skill_name in skill_names {
            session
                .skill_states
                .entry(skill_name.clone())
                .or_insert_with(SessionSkillState::default)
                .last_loaded_turn = Some(session.turn_count);
        }
        session.persist()?;
        Ok(())
    }

    pub fn record_agent_turn(
        &mut self,
        address: &ChannelAddress,
        messages: Vec<ChatMessage>,
        usage: &TokenUsage,
        compaction: &SessionCompactionStats,
        response_checkpoint: Option<ResponseCheckpoint>,
    ) -> Result<()> {
        let key = address.session_key();
        let session = self
            .foreground_sessions
            .get_mut(&key)
            .with_context(|| format!("no active session for {}", key))?;
        record_turn(
            session,
            messages,
            usage,
            compaction,
            response_checkpoint,
            true,
            "agent_turn_recorded",
        )
    }

    pub fn record_yielded_turn(
        &mut self,
        address: &ChannelAddress,
        messages: Vec<ChatMessage>,
        usage: &TokenUsage,
        compaction: &SessionCompactionStats,
        response_checkpoint: Option<ResponseCheckpoint>,
    ) -> Result<()> {
        let key = address.session_key();
        let session = self
            .foreground_sessions
            .get_mut(&key)
            .with_context(|| format!("no active session for {}", key))?;
        record_turn(
            session,
            messages,
            usage,
            compaction,
            response_checkpoint,
            false,
            "agent_turn_yielded",
        )
    }

    pub fn record_background_turn(
        &mut self,
        session_id: Uuid,
        messages: Vec<ChatMessage>,
        usage: &TokenUsage,
        compaction: &SessionCompactionStats,
        response_checkpoint: Option<ResponseCheckpoint>,
    ) -> Result<()> {
        let session = self.background_session_mut(session_id)?;
        record_turn(
            session,
            messages,
            usage,
            compaction,
            response_checkpoint,
            true,
            "agent_turn_recorded",
        )?;
        Ok(())
    }

    pub fn record_background_yielded_turn(
        &mut self,
        session_id: Uuid,
        messages: Vec<ChatMessage>,
        usage: &TokenUsage,
        compaction: &SessionCompactionStats,
        response_checkpoint: Option<ResponseCheckpoint>,
    ) -> Result<()> {
        let session = self.background_session_mut(session_id)?;
        record_turn(
            session,
            messages,
            usage,
            compaction,
            response_checkpoint,
            false,
            "agent_turn_yielded",
        )?;
        Ok(())
    }

    pub fn update_background_checkpoint(
        &mut self,
        session_id: Uuid,
        messages: Vec<ChatMessage>,
        usage: &TokenUsage,
        compaction: &SessionCompactionStats,
        response_checkpoint: Option<ResponseCheckpoint>,
    ) -> Result<()> {
        let session = self.background_session_mut(session_id)?;
        session.agent_messages = messages;
        session.last_agent_returned_at = Some(Utc::now());
        session.response_checkpoint = response_checkpoint;
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
        session.agent_messages = messages.clone();
        session.response_checkpoint = None;
        if let Some(pending_continue) = &mut session.pending_continue {
            pending_continue.resume_messages = messages;
            pending_continue.response_checkpoint = None;
        }
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
        session.idle_compaction_retry = None;
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

    pub fn mark_idle_compaction_retry_needed(
        &mut self,
        address: &ChannelAddress,
        error_summary: String,
    ) -> Result<()> {
        let key = address.session_key();
        let session = self
            .foreground_sessions
            .get_mut(&key)
            .with_context(|| format!("no active session for {}", key))?;
        session.idle_compaction_retry = Some(IdleCompactionRetryState {
            error_summary,
            failed_at: Some(Utc::now()),
        });
        session.persist()?;
        Ok(())
    }

    pub fn clear_idle_compaction_retry(&mut self, address: &ChannelAddress) -> Result<()> {
        let key = address.session_key();
        let session = self
            .foreground_sessions
            .get_mut(&key)
            .with_context(|| format!("no active session for {}", key))?;
        session.idle_compaction_retry = None;
        session.persist()?;
        Ok(())
    }

    pub fn append_background_user_message(
        &mut self,
        session_id: Uuid,
        text: Option<String>,
        attachments: Vec<StoredAttachment>,
    ) -> Result<()> {
        self.append_background_message(session_id, MessageRole::User, text, attachments)
    }

    pub fn append_background_assistant_message(
        &mut self,
        session_id: Uuid,
        text: Option<String>,
        attachments: Vec<StoredAttachment>,
    ) -> Result<()> {
        self.append_background_message(session_id, MessageRole::Assistant, text, attachments)
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

    fn append_background_message(
        &mut self,
        session_id: Uuid,
        role: MessageRole,
        text: Option<String>,
        attachments: Vec<StoredAttachment>,
    ) -> Result<()> {
        let session = self.background_session_mut(session_id)?;
        let attachment_count = attachments.len();
        session.push_message(role.clone(), text, attachments);
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
        self.create_session_with_kind(address, Uuid::new_v4(), None, SessionKind::Foreground)
    }

    fn create_session_with_workspace(
        &self,
        address: &ChannelAddress,
        workspace_id: &str,
    ) -> Result<Session> {
        self.create_session_with_kind(
            address,
            Uuid::new_v4(),
            Some(workspace_id),
            SessionKind::Foreground,
        )
    }

    fn create_session_with_kind(
        &self,
        address: &ChannelAddress,
        agent_id: Uuid,
        workspace_id: Option<&str>,
        kind: SessionKind,
    ) -> Result<Session> {
        let session_id = Uuid::new_v4();
        let workspace = match workspace_id {
            Some(workspace_id) => self
                .workspace_manager
                .ensure_workspace_exists(workspace_id)?,
            None => self
                .workspace_manager
                .create_workspace(agent_id, session_id, None)?,
        };
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
            kind,
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
            skill_states: HashMap::new(),
            seen_user_profile_version: None,
            seen_identity_profile_version: None,
            pending_user_profile_notice: false,
            pending_identity_profile_notice: false,
            idle_compaction_retry: None,
            zgent_native: None,
            pending_continue: None,
            response_checkpoint: None,
            pending_workspace_summary: false,
            close_after_summary: false,
            closed_at: None,
        };
        session.persist()?;
        Ok(session)
    }

    fn background_session_mut(&mut self, session_id: Uuid) -> Result<&mut Session> {
        self.background_sessions
            .get_mut(&session_id)
            .with_context(|| format!("no active background session for {}", session_id))
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
                if session.kind != SessionKind::Foreground {
                    info!(
                        log_stream = "session",
                        kind = "session_restore_skipped",
                        root_dir = %path.display(),
                        "skipping persisted background session on startup"
                    );
                    continue;
                }
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

#[cfg(test)]
mod tests {
    use super::{
        SessionManager, SessionSkillObservation, SharedProfileChangeNotice, SkillChangeNotice,
        sanitize_persisted_agent_messages,
    };
    use crate::domain::{ChannelAddress, StoredAttachment};
    use crate::workspace::WorkspaceManager;
    use agent_frame::{ChatMessage, SessionCompactionStats, TokenUsage};
    use tempfile::TempDir;
    use uuid::Uuid;

    fn test_address() -> ChannelAddress {
        ChannelAddress {
            channel_id: "telegram-main".to_string(),
            conversation_id: "123".to_string(),
            user_id: Some("user-1".to_string()),
            display_name: Some("Test User".to_string()),
        }
    }

    #[test]
    fn sanitize_persisted_agent_messages_keeps_only_first_leading_system() {
        let messages = vec![
            ChatMessage::text("system", "system a"),
            ChatMessage::text("system", "system b"),
            ChatMessage::text("assistant", "summary"),
            ChatMessage::text("system", "runtime state"),
        ];

        let sanitized = sanitize_persisted_agent_messages(messages);

        assert_eq!(sanitized.len(), 3);
        assert_eq!(sanitized[0].role, "system");
        assert_eq!(
            sanitized[0].content.as_ref().and_then(|v| v.as_str()),
            Some("system a")
        );
        assert_eq!(sanitized[1].role, "assistant");
        assert_eq!(sanitized[2].role, "system");
        assert_eq!(
            sanitized[2].content.as_ref().and_then(|v| v.as_str()),
            Some("runtime state")
        );
    }

    #[test]
    fn emits_content_change_notice_for_loaded_skill_after_baseline() {
        let temp_dir = TempDir::new().unwrap();
        let workspace_manager = WorkspaceManager::load_or_create(temp_dir.path()).unwrap();
        let mut sessions = SessionManager::new(temp_dir.path(), workspace_manager).unwrap();
        let address = test_address();
        let session = sessions.ensure_foreground(&address).unwrap();

        sessions
            .observe_skill_changes(
                &address,
                &[SessionSkillObservation {
                    name: "skill-a".to_string(),
                    description: "old desc".to_string(),
                    content: "old content".to_string(),
                }],
            )
            .unwrap();

        sessions
            .record_agent_turn(
                &address,
                session.agent_messages.clone(),
                &TokenUsage::default(),
                &SessionCompactionStats::default(),
                None,
            )
            .unwrap();
        sessions
            .mark_skills_loaded_current_turn(&address, &["skill-a".to_string()])
            .unwrap();

        let notices = sessions
            .observe_skill_changes(
                &address,
                &[SessionSkillObservation {
                    name: "skill-a".to_string(),
                    description: "new desc".to_string(),
                    content: "new content".to_string(),
                }],
            )
            .unwrap();

        assert!(matches!(
            notices.as_slice(),
            [
                SkillChangeNotice::DescriptionChanged { name, description },
                SkillChangeNotice::ContentChanged {
                    name: content_name,
                    description: content_description,
                    content,
                }
            ] if name == "skill-a"
                && description == "new desc"
                && content_name == "skill-a"
                && content_description == "new desc"
                && content == "new content"
        ));
    }

    #[test]
    fn emits_description_only_notice_for_unloaded_skill_description_change() {
        let temp_dir = TempDir::new().unwrap();
        let workspace_manager = WorkspaceManager::load_or_create(temp_dir.path()).unwrap();
        let mut sessions = SessionManager::new(temp_dir.path(), workspace_manager).unwrap();
        let address = test_address();
        sessions.ensure_foreground(&address).unwrap();

        sessions
            .observe_skill_changes(
                &address,
                &[SessionSkillObservation {
                    name: "skill-b".to_string(),
                    description: "old desc".to_string(),
                    content: "same content".to_string(),
                }],
            )
            .unwrap();

        let notices = sessions
            .observe_skill_changes(
                &address,
                &[SessionSkillObservation {
                    name: "skill-b".to_string(),
                    description: "new desc".to_string(),
                    content: "same content".to_string(),
                }],
            )
            .unwrap();

        assert!(matches!(
            notices.as_slice(),
            [SkillChangeNotice::DescriptionChanged { name, description }]
                if name == "skill-b" && description == "new desc"
        ));
    }

    #[test]
    fn emits_description_then_content_notices_when_both_changed_for_loaded_skill() {
        let temp_dir = TempDir::new().unwrap();
        let workspace_manager = WorkspaceManager::load_or_create(temp_dir.path()).unwrap();
        let mut sessions = SessionManager::new(temp_dir.path(), workspace_manager).unwrap();
        let address = test_address();
        let session = sessions.ensure_foreground(&address).unwrap();

        sessions
            .observe_skill_changes(
                &address,
                &[SessionSkillObservation {
                    name: "skill-c".to_string(),
                    description: "old desc".to_string(),
                    content: "old content".to_string(),
                }],
            )
            .unwrap();

        sessions
            .record_agent_turn(
                &address,
                session.agent_messages.clone(),
                &TokenUsage::default(),
                &SessionCompactionStats::default(),
                None,
            )
            .unwrap();
        sessions
            .mark_skills_loaded_current_turn(&address, &["skill-c".to_string()])
            .unwrap();

        let notices = sessions
            .observe_skill_changes(
                &address,
                &[SessionSkillObservation {
                    name: "skill-c".to_string(),
                    description: "new desc".to_string(),
                    content: "new content".to_string(),
                }],
            )
            .unwrap();

        assert!(matches!(
            notices.as_slice(),
            [
                SkillChangeNotice::DescriptionChanged { name, description },
                SkillChangeNotice::ContentChanged {
                    name: content_name,
                    description: content_description,
                    content,
                }
            ] if name == "skill-c"
                && description == "new desc"
                && content_name == "skill-c"
                && content_description == "new desc"
                && content == "new content"
        ));
    }

    #[test]
    fn shared_profile_changes_queue_until_taken() {
        let temp_dir = TempDir::new().unwrap();
        let workspace_manager = WorkspaceManager::load_or_create(temp_dir.path()).unwrap();
        let mut sessions = SessionManager::new(temp_dir.path(), workspace_manager).unwrap();
        let address = test_address();
        sessions.ensure_foreground(&address).unwrap();

        let first = sessions
            .observe_shared_profile_changes(
                &address,
                "user-v1".to_string(),
                "identity-v1".to_string(),
            )
            .unwrap();
        assert!(first.is_empty());

        let second = sessions
            .observe_shared_profile_changes(
                &address,
                "user-v2".to_string(),
                "identity-v1".to_string(),
            )
            .unwrap();
        assert_eq!(second, vec![SharedProfileChangeNotice::UserUpdated]);
        let queued = sessions
            .take_shared_profile_change_notices(&address)
            .unwrap();
        assert_eq!(queued, vec![SharedProfileChangeNotice::UserUpdated]);
        assert!(
            sessions
                .take_shared_profile_change_notices(&address)
                .unwrap()
                .is_empty()
        );

        let third = sessions
            .observe_shared_profile_changes(
                &address,
                "user-v2".to_string(),
                "identity-v2".to_string(),
            )
            .unwrap();
        assert_eq!(third, vec![SharedProfileChangeNotice::IdentityUpdated]);
        sessions
            .stage_shared_profile_change_notices(
                &address,
                &[SharedProfileChangeNotice::UserUpdated],
            )
            .unwrap();
        let delivered = sessions
            .take_shared_profile_change_notices(&address)
            .unwrap();
        assert_eq!(
            delivered,
            vec![
                SharedProfileChangeNotice::UserUpdated,
                SharedProfileChangeNotice::IdentityUpdated
            ]
        );
    }

    #[test]
    fn idle_compaction_retry_state_can_be_set_and_cleared() {
        let temp_dir = TempDir::new().unwrap();
        let workspace_manager = WorkspaceManager::load_or_create(temp_dir.path()).unwrap();
        let mut sessions = SessionManager::new(temp_dir.path(), workspace_manager).unwrap();
        let address = test_address();
        sessions.ensure_foreground(&address).unwrap();

        sessions
            .mark_idle_compaction_retry_needed(&address, "idle compaction failed".to_string())
            .unwrap();
        let snapshot = sessions.get_snapshot(&address).unwrap();
        assert_eq!(
            snapshot
                .idle_compaction_retry
                .as_ref()
                .map(|state| state.error_summary.as_str()),
            Some("idle compaction failed")
        );

        sessions.clear_idle_compaction_retry(&address).unwrap();
        let snapshot = sessions.get_snapshot(&address).unwrap();
        assert!(snapshot.idle_compaction_retry.is_none());
    }

    #[test]
    fn exports_and_restores_session_checkpoint() {
        let temp_dir = TempDir::new().unwrap();
        let workspace_manager = WorkspaceManager::load_or_create(temp_dir.path()).unwrap();
        let mut sessions = SessionManager::new(temp_dir.path(), workspace_manager.clone()).unwrap();
        let address = test_address();
        sessions.ensure_foreground(&address).unwrap();

        sessions
            .append_user_message(
                &address,
                Some("hello".to_string()),
                Vec::<StoredAttachment>::new(),
            )
            .unwrap();
        sessions
            .record_agent_turn(
                &address,
                vec![ChatMessage::text("assistant", "hi")],
                &TokenUsage::default(),
                &SessionCompactionStats::default(),
                None,
            )
            .unwrap();

        let checkpoint = sessions.export_checkpoint(&address).unwrap();
        let workspace = workspace_manager
            .create_workspace(uuid::Uuid::new_v4(), uuid::Uuid::new_v4(), Some("restored"))
            .unwrap();
        let restored = sessions
            .restore_foreground_from_checkpoint(
                &address,
                checkpoint,
                workspace.id.clone(),
                workspace.files_dir.clone(),
            )
            .unwrap();

        assert_eq!(restored.workspace_id, workspace.id);
        assert_eq!(restored.message_count, 1);
        assert_eq!(restored.agent_message_count, 1);
    }

    #[test]
    fn background_sessions_can_share_workspace_without_sharing_memory() {
        let temp_dir = TempDir::new().unwrap();
        let workspace_manager = WorkspaceManager::load_or_create(temp_dir.path()).unwrap();
        let mut sessions = SessionManager::new(temp_dir.path(), workspace_manager).unwrap();
        let address = test_address();
        let foreground = sessions.ensure_foreground(&address).unwrap();
        let background = sessions
            .create_background_in_workspace(&address, Uuid::new_v4(), &foreground.workspace_id)
            .unwrap();

        assert_ne!(foreground.id, background.id);
        assert_eq!(foreground.workspace_id, background.workspace_id);

        sessions
            .record_background_turn(
                background.id,
                vec![ChatMessage::text("assistant", "background memory")],
                &TokenUsage::default(),
                &SessionCompactionStats::default(),
                None,
            )
            .unwrap();

        let foreground_after = sessions.get_snapshot(&address).unwrap();
        let background_after = sessions.background_snapshot(background.id).unwrap();
        assert!(foreground_after.agent_messages.is_empty());
        assert_eq!(
            background_after.agent_messages[0]
                .content
                .as_ref()
                .and_then(|value| value.as_str()),
            Some("background memory")
        );
    }
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
