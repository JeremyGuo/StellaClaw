use crate::backend::AgentBackendKind;
use crate::domain::ChannelAddress;
use agent_frame::{ChatMessage, SessionCompactionStats, SessionExecutionControl, TokenUsage};
use anyhow::{Context, Result, anyhow};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::VecDeque;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex};
use uuid::Uuid;

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SubagentState {
    Running,
    WaitingForCharge,
    Ready,
    Failed,
    Destroyed,
}

impl SubagentState {
    pub fn is_alive(self) -> bool {
        matches!(self, SubagentState::Running | SubagentState::Ready)
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PersistedSubagentState {
    pub id: Uuid,
    pub parent_agent_id: Uuid,
    pub session_id: Uuid,
    pub channel_id: String,
    pub conversation_id: String,
    pub workspace_id: String,
    pub agent_backend: AgentBackendKind,
    pub model_key: String,
    pub description: String,
    pub workbook_relative_path: String,
    pub state: SubagentState,
    #[serde(default)]
    pub resume_pending: bool,
    #[serde(default)]
    pub messages: Vec<ChatMessage>,
    #[serde(default)]
    pub pending_prompts: Vec<String>,
    #[serde(default)]
    pub available_charge_seconds: f64,
    pub default_charge_seconds: f64,
    #[serde(default)]
    pub last_result_text: Option<String>,
    #[serde(default)]
    pub last_attachment_paths: Vec<String>,
    #[serde(default)]
    pub last_error: Option<String>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    #[serde(default)]
    pub last_returned_at: Option<DateTime<Utc>>,
    #[serde(default)]
    pub turn_count: u64,
    #[serde(default)]
    pub last_compacted_turn_count: u64,
    #[serde(default)]
    pub cumulative_usage: TokenUsage,
    #[serde(default)]
    pub cumulative_compaction: SessionCompactionStats,
    #[serde(default)]
    pub wait_has_returned_ready: bool,
}

pub struct HostedSubagent {
    pub id: Uuid,
    pub session_id: Uuid,
    pub address: ChannelAddress,
    pub workspace_id: String,
    pub workspace_root: PathBuf,
    pub runtime_state_root: PathBuf,
    pub workbook_path: PathBuf,
    pub state_path: PathBuf,
    pub inner: Mutex<HostedSubagentInner>,
    pub condvar: Condvar,
}

pub struct HostedSubagentInner {
    pub persisted: PersistedSubagentState,
    pub queued_prompts: VecDeque<String>,
    pub active_control: Option<SessionExecutionControl>,
}

impl HostedSubagent {
    pub fn create(
        id: Uuid,
        parent_agent_id: Uuid,
        session_id: Uuid,
        address: ChannelAddress,
        workspace_id: String,
        workspace_root: PathBuf,
        runtime_state_root: PathBuf,
        agent_backend: AgentBackendKind,
        model_key: String,
        description: String,
        default_charge_seconds: f64,
        initial_charge_seconds: f64,
        initial_prompt: String,
    ) -> Result<Arc<Self>> {
        let workbook_dir = workspace_root.join(".subagent");
        fs::create_dir_all(&workbook_dir)
            .with_context(|| format!("failed to create {}", workbook_dir.display()))?;
        let workbook_file_name = format!("{id}-workbook.md");
        let workbook_path = workbook_dir.join(&workbook_file_name);
        if !workbook_path.exists() {
            fs::write(
                &workbook_path,
                format!(
                    "# Subagent Workbook\n\n- id: {id}\n- description: {description}\n\n## Progress\n\n"
                ),
            )
            .with_context(|| format!("failed to write {}", workbook_path.display()))?;
        }

        let state_dir = subagent_state_dir(&runtime_state_root)?;
        let state_path = state_dir.join(format!("{id}.json"));
        let persisted = PersistedSubagentState {
            id,
            parent_agent_id,
            session_id,
            channel_id: address.channel_id.clone(),
            conversation_id: address.conversation_id.clone(),
            workspace_id: workspace_id.clone(),
            agent_backend,
            model_key,
            description,
            workbook_relative_path: format!(".subagent/{workbook_file_name}"),
            state: SubagentState::Ready,
            resume_pending: false,
            messages: Vec::new(),
            pending_prompts: vec![initial_prompt.clone()],
            available_charge_seconds: initial_charge_seconds,
            default_charge_seconds,
            last_result_text: None,
            last_attachment_paths: Vec::new(),
            last_error: None,
            created_at: Utc::now(),
            updated_at: Utc::now(),
            last_returned_at: None,
            turn_count: 0,
            last_compacted_turn_count: 0,
            cumulative_usage: TokenUsage::default(),
            cumulative_compaction: SessionCompactionStats::default(),
            wait_has_returned_ready: false,
        };
        let queued_prompts = VecDeque::from(vec![initial_prompt]);
        let hosted = Arc::new(Self {
            id,
            session_id,
            address,
            workspace_id,
            workspace_root,
            runtime_state_root,
            workbook_path,
            state_path,
            inner: Mutex::new(HostedSubagentInner {
                persisted,
                queued_prompts,
                active_control: None,
            }),
            condvar: Condvar::new(),
        });
        hosted.persist()?;
        Ok(hosted)
    }

    pub fn persist(&self) -> Result<()> {
        let inner = self
            .inner
            .lock()
            .map_err(|_| anyhow!("subagent state lock poisoned"))?;
        self.persist_locked(&inner)
    }

    pub fn persist_locked(&self, inner: &HostedSubagentInner) -> Result<()> {
        if let Some(parent) = self.state_path.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("failed to create {}", parent.display()))?;
        }
        let raw = serde_json::to_string_pretty(&inner.persisted)
            .context("failed to serialize subagent state")?;
        fs::write(&self.state_path, raw)
            .with_context(|| format!("failed to write {}", self.state_path.display()))
    }

    pub fn remove_state_file(&self) -> Result<()> {
        match fs::remove_file(&self.state_path) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error)
                .with_context(|| format!("failed to remove {}", self.state_path.display())),
        }
    }
}

pub fn subagent_state_dir(runtime_state_root: &Path) -> Result<PathBuf> {
    let dir = runtime_state_root.join("agent_frame").join("subagents");
    fs::create_dir_all(&dir).with_context(|| format!("failed to create {}", dir.display()))?;
    Ok(dir)
}
