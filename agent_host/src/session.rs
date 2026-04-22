use crate::channel::{ProgressFeedback, ProgressFeedbackFinalState, ProgressFeedbackUpdate};
use crate::domain::{
    ChannelAddress, MessageRole, SessionMessage, StoredAttachment, validate_conversation_id,
};
use crate::transcript::{SessionTranscript, TranscriptEntrySkeleton};
use crate::workspace::WorkspaceManager;
use agent_frame::{
    ChatMessage, ExecutionProgress, ExecutionProgressPhase, SessionCompactionStats, SessionEvent,
    SessionExecutionControl, TokenUsage,
};
pub use agent_frame::{SessionErrno, SessionPhase};
use anyhow::{Context, Result, anyhow};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap, VecDeque};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, mpsc};
use std::thread;
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
pub(crate) struct SessionSkillObservation {
    pub(crate) name: String,
    pub(crate) description: String,
    pub(crate) content: String,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct SessionPromptComponentState {
    #[serde(default)]
    pub system_prompt_value: String,
    #[serde(default)]
    pub system_prompt_hash: String,
    #[serde(default)]
    pub notified_value: String,
    #[serde(default)]
    pub notified_hash: String,
}

#[derive(Clone, Debug)]
pub(crate) enum SkillChangeNotice {
    MetadataChanged {
        metadata_prompt: String,
    },
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
pub(crate) struct PromptComponentChangeNotice {
    pub(crate) key: String,
    pub(crate) value: String,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct SessionCheckpointData {
    #[serde(default)]
    pub history: Vec<SessionMessage>,
    #[serde(default)]
    pub messages: Vec<ChatMessage>,
    #[serde(default)]
    pub last_user_message_at: Option<DateTime<Utc>>,
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
    pub system_prompt_static_hash: Option<String>,
    #[serde(default)]
    pub prompt_components: BTreeMap<String, SessionPromptComponentState>,
    #[serde(default)]
    pub actor_mailbox: Vec<DurableSessionActorMessage>,
    #[serde(default)]
    pub user_mailbox: Vec<DurableSessionUserMessage>,
    #[serde(default)]
    pub current_plan: Option<SessionPlan>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct SessionProgressMessageState {
    pub turn_id: String,
    pub message_id: String,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SessionPlanStepStatus {
    Pending,
    InProgress,
    Completed,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct SessionPlanStep {
    pub step: String,
    pub status: SessionPlanStepStatus,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct SessionPlan {
    #[serde(default)]
    pub explanation: Option<String>,
    #[serde(default)]
    pub steps: Vec<SessionPlanStep>,
    #[serde(default = "Utc::now")]
    pub updated_at: DateTime<Utc>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct DurableSessionState {
    #[serde(default)]
    pub messages: Vec<ChatMessage>,
    #[serde(default)]
    pub pending_messages: Vec<ChatMessage>,
    #[serde(default)]
    pub system_prompt_static_hash: Option<String>,
    #[serde(default)]
    pub prompt_components: BTreeMap<String, SessionPromptComponentState>,
    #[serde(default)]
    pub phase: SessionPhase,
    #[serde(default)]
    pub errno: Option<SessionErrno>,
    #[serde(default)]
    pub errinfo: Option<String>,
    #[serde(default)]
    pub progress_message: Option<SessionProgressMessageState>,
    #[serde(default)]
    pub actor_mailbox: Vec<DurableSessionActorMessage>,
    #[serde(default)]
    pub user_mailbox: Vec<DurableSessionUserMessage>,
    #[serde(default)]
    pub current_plan: Option<SessionPlan>,
}

#[derive(Clone, Debug)]
pub struct SessionSnapshot {
    pub kind: SessionKind,
    pub id: Uuid,
    pub agent_id: Uuid,
    pub address: ChannelAddress,
    pub root_dir: PathBuf,
    pub attachments_dir: PathBuf,
    pub workspace_id: String,
    pub workspace_root: PathBuf,
    pub last_user_message_at: Option<DateTime<Utc>>,
    pub last_agent_returned_at: Option<DateTime<Utc>>,
    pub last_compacted_at: Option<DateTime<Utc>>,
    pub turn_count: u64,
    pub last_compacted_turn_count: u64,
    pub cumulative_usage: TokenUsage,
    pub cumulative_compaction: SessionCompactionStats,
    pub api_timeout_override_seconds: Option<f64>,
    pub skill_states: HashMap<String, SessionSkillState>,
    pub pending_workspace_summary: bool,
    pub close_after_summary: bool,
    pub session_state: DurableSessionState,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct SystemPromptStateObservation {
    pub(crate) static_changed: bool,
}

pub const INTERRUPTED_FOLLOWUP_MARKER: &str = "[Interrupted Follow-up]";
pub const QUEUED_USER_UPDATES_MARKER: &str = "[Queued User Updates]";
const COMPACTION_WAIT_NOTICE_TEXT: &str = "正在压缩上下文，可能要等待压缩完毕后才能回复。";
pub(crate) const IDENTITY_PROMPT_COMPONENT: &str = "identity";
pub(crate) const REMOTE_ALIASES_PROMPT_COMPONENT: &str = "ssh_remote_aliases";
pub(crate) const SKILLS_METADATA_PROMPT_COMPONENT: &str = "skills_metadata";
pub(crate) const USER_META_PROMPT_COMPONENT: &str = "user_meta";

fn prompt_component_hash(value: &str) -> String {
    use std::hash::{Hash, Hasher};

    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    value.hash(&mut hasher);
    format!("{:016x}", hasher.finish())
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum SessionActorOutbound {
    UserVisibleText(String),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SessionRuntimePhase {
    Running,
    Compacting,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct SessionYieldDisposition {
    interrupted: bool,
    compaction_in_progress: bool,
}

#[derive(Clone, Debug, Default)]
pub(crate) struct SessionUserMessageReceipt {
    pub(crate) text: Option<String>,
    pub(crate) interrupted: bool,
    pub(crate) compaction_in_progress: bool,
    pub(crate) outbound: Vec<SessionActorOutbound>,
    pub(crate) transcript_entry: Option<TranscriptEntrySkeleton>,
}

#[derive(Clone, Debug)]
pub(crate) struct SessionActorMessage {
    pub(crate) from_session_id: Uuid,
    pub(crate) role: MessageRole,
    pub(crate) text: Option<String>,
    pub(crate) attachments: Vec<StoredAttachment>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct SessionActorMessageReceipt {
    pub(crate) message_id: Uuid,
    pub(crate) applied_to_context: bool,
}

#[derive(Clone, Debug)]
pub(crate) struct SessionUserMessage {
    pub(crate) pending_message: ChatMessage,
    pub(crate) text: Option<String>,
    pub(crate) attachments: Vec<StoredAttachment>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DurableSessionActorMessage {
    #[serde(default = "Uuid::new_v4")]
    pub id: Uuid,
    pub from_session_id: Uuid,
    #[serde(default = "Utc::now")]
    pub created_at: DateTime<Utc>,
    #[serde(default = "default_actor_message_role")]
    pub role: MessageRole,
    #[serde(default)]
    pub text: Option<String>,
    #[serde(default)]
    pub attachments: Vec<StoredAttachment>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DurableSessionUserMessage {
    #[serde(default = "Uuid::new_v4")]
    pub id: Uuid,
    #[serde(default = "Utc::now")]
    pub created_at: DateTime<Utc>,
    pub pending_message: ChatMessage,
    #[serde(default)]
    pub text: Option<String>,
    #[serde(default)]
    pub attachments: Vec<StoredAttachment>,
}

fn default_actor_message_role() -> MessageRole {
    MessageRole::Assistant
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct SessionTurnTimeHintConfig {
    pub(crate) emit_idle_time_gap_hint: bool,
    pub(crate) emit_system_date_on_user_message: bool,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct SessionTurnTimeHints {
    pub(crate) system_date: Option<String>,
}

pub struct SessionRuntimeTurnCommit {
    pub messages: Vec<ChatMessage>,
    pub consumed_pending_messages: Vec<ChatMessage>,
    pub usage: TokenUsage,
    pub compaction: SessionCompactionStats,
    pub phase: SessionPhase,
    pub system_prompt_static_hash_after_compaction: Option<String>,
    pub loaded_skills: Vec<String>,
    pub user_history_text: Option<String>,
    pub assistant_history_text: Option<String>,
}

pub(crate) struct SessionRuntimeTurnFailure {
    pub(crate) resume_messages: Vec<ChatMessage>,
    pub(crate) errno: SessionErrno,
    pub(crate) errinfo: Option<String>,
    pub(crate) compaction: SessionCompactionStats,
    pub(crate) system_prompt_static_hash_after_compaction: Option<String>,
}

#[derive(Clone, Debug)]
pub(crate) enum SessionEffect {
    UpdateProgress(ProgressFeedback),
    UserVisibleText(String),
}

#[derive(Default)]
struct SessionRuntimeState {
    active_control: Option<SessionExecutionControl>,
    active_phase: Option<SessionRuntimePhase>,
    pending_interrupt: bool,
    turn_runner_claimed: bool,
    cache_health: SessionCacheHealthState,
}

const CACHE_WARNING_RECENT_CALL_LIMIT: usize = 10;
const CACHE_WARNING_REQUIRED_CONSECUTIVE_ZERO_READS: usize = 3;
const CACHE_WARNING_REQUIRED_RECENT_ZERO_READS: usize = 2;
const CACHE_WARNING_MAX_GAP_SECONDS: i64 = 5 * 60;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum CacheWarningKind {
    ConsecutiveZeroReads,
    BurstyZeroReads,
}

#[derive(Clone, Debug)]
struct CacheReadObservation {
    at: DateTime<Utc>,
    cache_read_tokens: u64,
}

#[derive(Default)]
struct SessionCacheHealthState {
    recent_model_calls: VecDeque<CacheReadObservation>,
    warning_active: bool,
}

impl SessionCacheHealthState {
    fn record_compaction_boundary(&mut self) {
        self.recent_model_calls.clear();
        self.warning_active = false;
    }

    fn record_model_call(
        &mut self,
        now: DateTime<Utc>,
        cache_read_tokens: u64,
    ) -> Option<CacheWarningKind> {
        self.recent_model_calls.push_back(CacheReadObservation {
            at: now,
            cache_read_tokens,
        });
        while self.recent_model_calls.len() > CACHE_WARNING_RECENT_CALL_LIMIT {
            self.recent_model_calls.pop_front();
        }

        if cache_read_tokens > 0 {
            self.warning_active = false;
            return None;
        }
        if self.warning_active {
            return None;
        }
        if self.last_n_are_zero_with_recent_gap(CACHE_WARNING_REQUIRED_CONSECUTIVE_ZERO_READS) {
            self.warning_active = true;
            return Some(CacheWarningKind::ConsecutiveZeroReads);
        }
        if self.has_recent_bursty_zero_reads() {
            self.warning_active = true;
            return Some(CacheWarningKind::BurstyZeroReads);
        }
        None
    }

    fn last_n_are_zero_with_recent_gap(&self, n: usize) -> bool {
        if self.recent_model_calls.len() < n {
            return false;
        }
        let tail = self
            .recent_model_calls
            .iter()
            .rev()
            .take(n)
            .collect::<Vec<_>>();
        tail.len() == n
            && tail.iter().all(|call| call.cache_read_tokens == 0)
            && self.zero_call_slice_within_gap(&tail)
    }

    fn has_recent_bursty_zero_reads(&self) -> bool {
        let zero_calls = self
            .recent_model_calls
            .iter()
            .enumerate()
            .filter(|(_, call)| call.cache_read_tokens == 0)
            .collect::<Vec<_>>();
        if zero_calls.len() < CACHE_WARNING_REQUIRED_RECENT_ZERO_READS {
            return false;
        }
        let tail = &zero_calls[zero_calls.len() - CACHE_WARNING_REQUIRED_RECENT_ZERO_READS..];
        let separated_by_model_call = tail
            .windows(2)
            .all(|pair| pair[1].0.saturating_sub(pair[0].0) > 1);
        separated_by_model_call
            && self
                .zero_call_slice_within_gap(&tail.iter().map(|(_, call)| *call).collect::<Vec<_>>())
    }

    fn zero_call_slice_within_gap(&self, calls: &[&CacheReadObservation]) -> bool {
        calls.windows(2).all(|pair| {
            pair[0]
                .at
                .signed_duration_since(pair[1].at)
                .num_seconds()
                .abs()
                <= CACHE_WARNING_MAX_GAP_SECONDS
        })
    }
}

type SessionActorCommandFn = Box<dyn FnOnce(&mut SessionActor) + Send + 'static>;

enum SessionActorCommand {
    Run(SessionActorCommandFn),
    Shutdown(mpsc::Sender<()>),
}

struct SessionActorLifecycle {
    closing: bool,
    worker: Option<thread::JoinHandle<()>>,
}

struct SessionActorHandleInner {
    sender: mpsc::Sender<SessionActorCommand>,
    lifecycle: Mutex<SessionActorLifecycle>,
}

#[derive(Clone)]
pub struct SessionActorRef {
    inner: Arc<SessionActorHandleInner>,
}

impl SessionActorRef {
    #[cfg(test)]
    pub fn ptr_eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.inner, &other.inner)
    }

    pub fn snapshot(&self) -> Result<SessionSnapshot> {
        self.read(|actor| Ok(actor.snapshot()))
    }

    pub(crate) fn export_checkpoint(&self) -> Result<SessionCheckpointData> {
        self.read(|actor| Ok(actor.export_checkpoint()))
    }

    fn read<R: Send + 'static>(
        &self,
        f: impl FnOnce(&SessionActor) -> Result<R> + Send + 'static,
    ) -> Result<R> {
        self.ask(|actor| f(actor))
    }

    fn update<R: Send + 'static>(
        &self,
        f: impl FnOnce(&mut SessionActor) -> Result<R> + Send + 'static,
    ) -> Result<R> {
        self.ask(f)
    }

    fn ask<R: Send + 'static>(
        &self,
        f: impl FnOnce(&mut SessionActor) -> Result<R> + Send + 'static,
    ) -> Result<R> {
        let (sender, receiver) = mpsc::channel();
        {
            let lifecycle = self
                .inner
                .lifecycle
                .lock()
                .map_err(|_| anyhow!("session actor lifecycle lock poisoned"))?;
            if lifecycle.closing {
                return Err(anyhow!("session actor is shutting down"));
            }
            self.inner
                .sender
                .send(SessionActorCommand::Run(Box::new(move |actor| {
                    let _ = sender.send(f(actor));
                })))
                .map_err(|_| anyhow!("session actor mailbox closed"))?;
        }
        receiver
            .recv()
            .map_err(|_| anyhow!("session actor mailbox response dropped"))?
    }

    pub(crate) fn shutdown(&self) -> Result<()> {
        let (sender, receiver) = mpsc::channel();
        let worker = {
            let mut lifecycle = self
                .inner
                .lifecycle
                .lock()
                .map_err(|_| anyhow!("session actor lifecycle lock poisoned"))?;
            if lifecycle.closing {
                return Ok(());
            }
            lifecycle.closing = true;
            let worker = lifecycle.worker.take();
            if self
                .inner
                .sender
                .send(SessionActorCommand::Shutdown(sender))
                .is_err()
            {
                return join_session_actor_worker(worker);
            }
            worker
        };
        receiver
            .recv()
            .map_err(|_| anyhow!("session actor shutdown acknowledgement dropped"))?;
        join_session_actor_worker(worker)
    }

    pub(crate) fn close_and_shutdown(&self) -> Result<()> {
        self.update(|actor| actor.close())?;
        self.shutdown()
    }

    pub(crate) fn tell_user_message(
        &self,
        message: SessionUserMessage,
    ) -> Result<SessionUserMessageReceipt> {
        self.update(move |actor| actor.tell_user_message(message))
    }

    pub(crate) fn tell_actor_message(
        &self,
        message: SessionActorMessage,
    ) -> Result<SessionActorMessageReceipt> {
        self.update(move |actor| actor.tell_actor_message(message))
    }

    pub(crate) fn register_control(&self, control: SessionExecutionControl) -> Result<()> {
        self.update(|actor| {
            actor.register_control(control);
            Ok(())
        })
    }

    pub(crate) fn unregister_control(&self) -> Result<bool> {
        self.update(|actor| actor.unregister_control())
    }

    pub(crate) fn request_cancel(&self) -> Result<bool> {
        self.update(|actor| Ok(actor.request_cancel()))
    }

    pub(crate) fn clear_pending_interrupt(&self) -> Result<()> {
        self.update(|actor| {
            actor.clear_pending_interrupt();
            Ok(())
        })
    }

    pub(crate) fn has_pending_interrupt(&self) -> Result<bool> {
        self.read(|actor| Ok(actor.has_pending_interrupt()))
    }

    pub(crate) fn try_claim_turn_runner(&self) -> Result<bool> {
        self.update(|actor| Ok(actor.try_claim_turn_runner()))
    }

    pub(crate) fn user_turn_time_hints(
        &self,
        config: SessionTurnTimeHintConfig,
        now: DateTime<Utc>,
    ) -> Result<SessionTurnTimeHints> {
        self.read(move |actor| Ok(actor.user_turn_time_hints(config, now)))
    }

    pub(crate) fn receive_runtime_event_with_effects(
        &self,
        model_key: &str,
        event: &SessionEvent,
    ) -> Result<Vec<SessionEffect>> {
        let model_key = model_key.to_string();
        let event = event.clone();
        self.update(move |actor| Ok(actor.receive_runtime_event_with_effects(&model_key, &event)))
    }

    pub(crate) fn receive_runtime_event(&self, event: &SessionEvent) -> Result<()> {
        let event = event.clone();
        self.update(move |actor| {
            actor.receive_runtime_event(&event);
            Ok(())
        })
    }

    pub(crate) fn receive_runtime_progress(
        &self,
        model_key: &str,
        progress: &ExecutionProgress,
    ) -> Result<Vec<SessionEffect>> {
        let model_key = model_key.to_string();
        let progress = progress.clone();
        self.read(move |actor| Ok(actor.receive_runtime_progress(&model_key, &progress)))
    }

    pub(crate) fn receive_runtime_failure(
        &self,
        model_key: &str,
        error: &anyhow::Error,
    ) -> Result<Vec<SessionEffect>> {
        let model_key = model_key.to_string();
        let error_summary = format!("{error:#}");
        self.read(
            move |actor| Ok(actor.receive_runtime_failure_summary(&model_key, &error_summary)),
        )
    }

    pub(crate) fn apply_progress_feedback_update(
        &self,
        update: ProgressFeedbackUpdate,
    ) -> Result<()> {
        self.update(|actor| actor.apply_progress_feedback_update(update))
    }

    pub(crate) fn update_plan(&self, plan: SessionPlan) -> Result<()> {
        self.update(move |actor| actor.update_plan(plan))
    }

    pub(crate) fn mark_workspace_summary_state(
        &self,
        pending: bool,
        close_after_summary: bool,
    ) -> Result<()> {
        self.update(move |actor| actor.mark_workspace_summary_state(pending, close_after_summary))
    }

    pub fn set_api_timeout_override(&self, timeout_seconds: Option<f64>) -> Result<()> {
        self.update(move |actor| actor.set_api_timeout_override(timeout_seconds))
    }

    pub(crate) fn observe_skill_changes(
        &self,
        observed_skills: &[SessionSkillObservation],
        metadata_prompt: String,
    ) -> Result<Vec<SkillChangeNotice>> {
        let observed_skills = observed_skills.to_vec();
        self.update(move |actor| actor.observe_skill_changes(&observed_skills, metadata_prompt))
    }

    pub(crate) fn observe_prompt_component_change(
        &self,
        key: &str,
        value: String,
    ) -> Result<Option<PromptComponentChangeNotice>> {
        let key = key.to_string();
        self.update(move |actor| actor.observe_prompt_component_change(key, value))
    }

    pub(crate) fn initialize_prompt_component_if_missing(
        &self,
        key: &str,
        value: String,
    ) -> Result<()> {
        let key = key.to_string();
        self.update(move |actor| actor.initialize_prompt_component_if_missing(key, value))
    }

    pub(crate) fn observe_system_prompt_state(
        &self,
        static_hash: String,
    ) -> Result<SystemPromptStateObservation> {
        self.update(|actor| actor.observe_system_prompt_state(static_hash))
    }

    pub(crate) fn mark_system_prompt_state_current(&self, static_hash: String) -> Result<()> {
        self.update(|actor| actor.mark_system_prompt_state_current(static_hash))
    }

    pub fn commit_runtime_turn(&self, commit: SessionRuntimeTurnCommit) -> Result<()> {
        self.update(|actor| actor.commit_runtime_turn(commit))
    }

    pub(crate) fn fail_runtime_turn(&self, failure: SessionRuntimeTurnFailure) -> Result<()> {
        self.update(|actor| actor.fail_runtime_turn(failure))
    }

    pub fn record_idle_compaction(
        &self,
        messages: Vec<ChatMessage>,
        compaction: &SessionCompactionStats,
    ) -> Result<()> {
        let compaction = compaction.clone();
        self.update(move |actor| actor.record_idle_compaction(messages, &compaction))
    }

    pub(crate) fn update_checkpoint(
        &self,
        messages: Vec<ChatMessage>,
        usage: &TokenUsage,
        compaction: &SessionCompactionStats,
    ) -> Result<()> {
        let usage = usage.clone();
        let compaction = compaction.clone();
        self.update(move |actor| actor.update_checkpoint(messages, &usage, &compaction))
    }

    pub(crate) fn mark_idle_compaction_failed(&self, error_summary: String) -> Result<()> {
        self.update(|actor| actor.mark_idle_compaction_failed(error_summary))
    }

    pub(crate) fn clear_idle_compaction_failure(&self) -> Result<()> {
        self.update(|actor| actor.clear_idle_compaction_failure())
    }

    pub(crate) fn drain_mailboxes_if_idle(&self) -> Result<bool> {
        self.update(|actor| actor.drain_mailboxes_if_idle())
    }
}

struct SessionActor {
    session: Session,
    runtime: SessionRuntimeState,
}

impl SessionActor {
    fn new(session: Session) -> Self {
        Self {
            session,
            runtime: SessionRuntimeState::default(),
        }
    }

    fn snapshot(&self) -> SessionSnapshot {
        self.session.snapshot()
    }

    fn register_control(&mut self, control: SessionExecutionControl) {
        if self.runtime.pending_interrupt {
            control.request_yield();
        }
        self.runtime.turn_runner_claimed = true;
        self.runtime.active_phase = Some(SessionRuntimePhase::Running);
        self.runtime.active_control = Some(control);
    }

    fn unregister_control(&mut self) -> Result<bool> {
        self.runtime.active_control = None;
        self.runtime.active_phase = None;
        self.runtime.turn_runner_claimed = false;
        self.drain_mailboxes_if_idle()
    }

    fn receive_user_message(&mut self, text: Option<String>) -> SessionUserMessageReceipt {
        let disposition = self.request_yield_for_user_message();
        let mut outbound = Vec::new();
        if disposition.compaction_in_progress {
            outbound.push(SessionActorOutbound::UserVisibleText(
                COMPACTION_WAIT_NOTICE_TEXT.to_string(),
            ));
        }
        let text = if disposition.interrupted {
            tag_interrupted_followup_text(text)
        } else {
            text
        };
        SessionUserMessageReceipt {
            text,
            interrupted: disposition.interrupted,
            compaction_in_progress: disposition.compaction_in_progress,
            outbound,
            transcript_entry: None,
        }
    }

    fn user_turn_time_hints(
        &self,
        config: SessionTurnTimeHintConfig,
        now: DateTime<Utc>,
    ) -> SessionTurnTimeHints {
        SessionTurnTimeHints {
            system_date: config
                .emit_system_date_on_user_message
                .then(|| render_system_date_on_user_message(now)),
        }
    }

    fn receive_runtime_event(&mut self, event: &SessionEvent) {
        if matches!(
            event,
            SessionEvent::CompactionStarted { .. } | SessionEvent::ToolWaitCompactionStarted { .. }
        ) {
            self.runtime.cache_health.record_compaction_boundary();
        }
        if let Some(phase) = session_runtime_phase_for_event(event)
            && self.runtime.active_control.is_some()
        {
            self.runtime.active_phase = Some(phase);
        }
    }

    fn receive_runtime_event_with_effects(
        &mut self,
        model_key: &str,
        event: &SessionEvent,
    ) -> Vec<SessionEffect> {
        self.receive_runtime_event(event);
        let mut effects = self
            .progress_feedback_for_event(model_key, event)
            .map(SessionEffect::UpdateProgress)
            .into_iter()
            .collect::<Vec<_>>();
        if let Some(text) = self.cache_warning_text_for_event(event) {
            effects.push(SessionEffect::UserVisibleText(text));
        }
        effects
    }

    fn receive_runtime_progress(
        &self,
        model_key: &str,
        progress: &ExecutionProgress,
    ) -> Vec<SessionEffect> {
        vec![SessionEffect::UpdateProgress(ProgressFeedback {
            turn_id: self.session.id.to_string(),
            text: progress_text_for_execution(
                model_key,
                progress,
                self.session.session_state.current_plan.as_ref(),
            ),
            important: true,
            final_state: None,
            message_id: self
                .session
                .session_state
                .progress_message
                .as_ref()
                .map(|state| state.message_id.clone()),
        })]
    }

    fn receive_runtime_failure_summary(
        &self,
        model_key: &str,
        error_summary: &str,
    ) -> Vec<SessionEffect> {
        vec![SessionEffect::UpdateProgress(ProgressFeedback {
            turn_id: self.session.id.to_string(),
            text: progress_text(
                model_key,
                &format!("❌ 失败：{}", truncate_single_line(error_summary, 160)),
            ),
            important: true,
            final_state: Some(ProgressFeedbackFinalState::Failed),
            message_id: self
                .session
                .session_state
                .progress_message
                .as_ref()
                .map(|state| state.message_id.clone()),
        })]
    }

    fn cache_warning_text_for_event(&mut self, event: &SessionEvent) -> Option<String> {
        let SessionEvent::ModelCallCompleted {
            cache_read_tokens, ..
        } = event
        else {
            return None;
        };
        let warning = self
            .runtime
            .cache_health
            .record_model_call(Utc::now(), *cache_read_tokens)?;
        Some(render_cache_warning_text(&self.session.address, warning))
    }

    fn apply_progress_feedback_update(&mut self, update: ProgressFeedbackUpdate) -> Result<()> {
        match update {
            ProgressFeedbackUpdate::Unchanged => Ok(()),
            ProgressFeedbackUpdate::StoreMessage { message_id } => {
                self.set_progress_message(Some(SessionProgressMessageState {
                    turn_id: self.session.id.to_string(),
                    message_id,
                }))
            }
            ProgressFeedbackUpdate::ClearMessage => self.set_progress_message(None),
        }
    }

    fn update_plan(&mut self, plan: SessionPlan) -> Result<()> {
        self.session.session_state.current_plan = Some(plan);
        self.session.persist()
    }

    fn progress_feedback_for_event(
        &self,
        model_key: &str,
        event: &SessionEvent,
    ) -> Option<ProgressFeedback> {
        let (activity, important, final_state) = match event {
            SessionEvent::CompactionStarted { .. } => ("🗜️ 压缩中...".to_string(), true, None),
            SessionEvent::SessionStarted { .. } | SessionEvent::CompactionCompleted { .. } => {
                return None;
            }
            SessionEvent::RoundStarted { .. }
            | SessionEvent::ModelCallStarted { .. }
            | SessionEvent::ModelCallCompleted { .. } => return None,
            SessionEvent::ToolWaitCompactionStarted { .. } => {
                ("🗜️ 压缩中...".to_string(), true, None)
            }
            SessionEvent::ToolWaitCompactionScheduled { .. }
            | SessionEvent::ToolWaitCompactionCompleted { .. } => return None,
            SessionEvent::ToolCallStarted { .. } | SessionEvent::ToolCallCompleted { .. } => {
                if let SessionEvent::ToolCallCompleted {
                    tool_name, errored, ..
                } = event
                {
                    let activity = if *errored {
                        format!("❌ 工具失败：{tool_name}")
                    } else if tool_name == "update_plan" {
                        "📋 计划已更新".to_string()
                    } else {
                        format!("✅ 工具完成：{tool_name}")
                    };
                    return Some(ProgressFeedback {
                        turn_id: self.session.id.to_string(),
                        text: progress_text_with_plan(
                            model_key,
                            &activity,
                            self.session.session_state.current_plan.as_ref(),
                        ),
                        important: tool_name == "update_plan" || *errored,
                        final_state: None,
                        message_id: self
                            .session
                            .session_state
                            .progress_message
                            .as_ref()
                            .map(|state| state.message_id.clone()),
                    });
                }
                return None;
            }
            SessionEvent::SessionYielded { .. } | SessionEvent::PrefixRewriteApplied { .. } => {
                return None;
            }
            SessionEvent::SessionCompleted { .. } => (
                "✅ 完成".to_string(),
                true,
                Some(ProgressFeedbackFinalState::Done),
            ),
        };

        Some(ProgressFeedback {
            turn_id: self.session.id.to_string(),
            text: progress_text(model_key, &activity),
            important,
            final_state,
            message_id: self
                .session
                .session_state
                .progress_message
                .as_ref()
                .map(|state| state.message_id.clone()),
        })
    }

    fn request_yield_for_user_message(&mut self) -> SessionYieldDisposition {
        let compaction_in_progress =
            self.runtime.active_phase == Some(SessionRuntimePhase::Compacting);
        if self.runtime.active_control.is_none() && !self.runtime.turn_runner_claimed {
            return SessionYieldDisposition {
                interrupted: false,
                compaction_in_progress: false,
            };
        }
        if let Some(control) = self.runtime.active_control.clone() {
            control.request_yield();
        }
        self.runtime.pending_interrupt = true;
        SessionYieldDisposition {
            interrupted: true,
            compaction_in_progress,
        }
    }

    fn request_cancel(&mut self) -> bool {
        let Some(control) = self.runtime.active_control.clone() else {
            return false;
        };
        control.request_cancel();
        true
    }

    fn clear_pending_interrupt(&mut self) {
        self.runtime.pending_interrupt = false;
    }

    fn has_pending_interrupt(&self) -> bool {
        self.runtime.pending_interrupt
    }

    fn is_running(&self) -> bool {
        self.runtime.active_control.is_some() || self.runtime.turn_runner_claimed
    }

    fn try_claim_turn_runner(&mut self) -> bool {
        if self.runtime.active_control.is_some() || self.runtime.turn_runner_claimed {
            return false;
        }
        self.runtime.turn_runner_claimed = true;
        self.runtime.active_phase = Some(SessionRuntimePhase::Running);
        true
    }

    fn set_progress_message(
        &mut self,
        progress_message: Option<SessionProgressMessageState>,
    ) -> Result<()> {
        self.session.session_state.progress_message = progress_message;
        self.session.persist()
    }

    fn close(&mut self) -> Result<()> {
        self.session.closed_at = Some(Utc::now());
        self.session.persist()
    }

    fn mark_workspace_summary_state(
        &mut self,
        pending: bool,
        close_after_summary: bool,
    ) -> Result<()> {
        self.session.pending_workspace_summary = pending;
        self.session.close_after_summary = close_after_summary;
        self.session.persist()
    }

    fn set_api_timeout_override(&mut self, timeout_seconds: Option<f64>) -> Result<()> {
        self.session.api_timeout_override_seconds = timeout_seconds;
        self.session.persist()
    }

    fn set_failed_turn(
        &mut self,
        resume_messages: Vec<ChatMessage>,
        errno: SessionErrno,
        errinfo: Option<String>,
    ) -> Result<()> {
        let existing_pending_messages = self.session.session_state.pending_messages.clone();
        self.session.session_state.messages = resume_messages;
        self.session.session_state.pending_messages = if !existing_pending_messages.is_empty()
            && !self
                .session
                .session_state
                .messages
                .ends_with(&existing_pending_messages)
        {
            existing_pending_messages
        } else {
            Vec::new()
        };
        self.session.session_state.phase = SessionPhase::Yielded;
        self.session.session_state.errno = Some(errno);
        self.session.session_state.errinfo = errinfo;
        self.session.persist()
    }

    fn append_visible_message(
        &mut self,
        role: MessageRole,
        text: Option<String>,
        attachments: Vec<StoredAttachment>,
    ) -> Result<()> {
        let attachment_count = attachments.len();
        self.session.push_message(role.clone(), text, attachments);
        tracing::debug!(
            log_stream = "session",
            log_key = %self.session.id,
            kind = "message_appended",
            role = ?role,
            message_count = self.session.history.len() as u64,
            attachment_count = attachment_count as u64,
            "appended message to session history"
        );
        self.session.persist()
    }

    fn tell_actor_message(
        &mut self,
        message: SessionActorMessage,
    ) -> Result<SessionActorMessageReceipt> {
        let message = DurableSessionActorMessage {
            id: Uuid::new_v4(),
            from_session_id: message.from_session_id,
            created_at: Utc::now(),
            role: message.role,
            text: message.text,
            attachments: message.attachments,
        };
        let message_id = message.id;
        info!(
            log_stream = "session",
            log_key = %self.session.id,
            kind = "actor_message_enqueued",
            from_session_id = %message.from_session_id,
            "enqueued actor message in durable session mailbox"
        );
        self.session.session_state.actor_mailbox.push(message);
        self.session.persist()?;
        let applied_to_context = self.drain_mailboxes_if_idle()?;
        Ok(SessionActorMessageReceipt {
            message_id,
            applied_to_context,
        })
    }

    fn tell_user_message(
        &mut self,
        mut message: SessionUserMessage,
    ) -> Result<SessionUserMessageReceipt> {
        let transcript_text = message.text.clone();
        let transcript_attachment_count = message.attachments.len();
        let mut receipt = self.receive_user_message(message.text);
        if receipt.interrupted {
            tag_interrupted_user_chat_message(&mut message.pending_message);
        }
        let queued = DurableSessionUserMessage {
            id: Uuid::new_v4(),
            created_at: Utc::now(),
            pending_message: message.pending_message,
            text: receipt.text.clone(),
            attachments: message.attachments,
        };
        info!(
            log_stream = "session",
            log_key = %self.session.id,
            kind = "user_message_enqueued",
            interrupted = receipt.interrupted,
            compaction_in_progress = receipt.compaction_in_progress,
            "enqueued user message in durable session mailbox"
        );
        self.session.session_state.user_mailbox.push(queued);
        self.session.persist()?;
        match SessionTranscript::open(&self.session.root_dir).and_then(|mut transcript| {
            transcript
                .record_user_message(transcript_text, transcript_attachment_count)
                .map(|entry| entry.to_skeleton())
        }) {
            Ok(entry) => {
                receipt.transcript_entry = Some(entry);
            }
            Err(error) => {
                warn!(
                    log_stream = "session",
                    log_key = %self.session.id,
                    kind = "transcript_record_failed",
                    error = %format!("{error:#}"),
                    "failed to record user message transcript entry"
                );
            }
        }
        self.drain_mailboxes_if_idle()?;
        Ok(receipt)
    }

    fn drain_mailboxes_if_idle(&mut self) -> Result<bool> {
        if self.is_running() {
            return Ok(false);
        }
        let actor_drained = self.drain_actor_mailbox_if_idle()?;
        let user_drained = self.drain_user_mailbox_if_idle()?;
        Ok(actor_drained || user_drained)
    }

    fn drain_actor_mailbox_if_idle(&mut self) -> Result<bool> {
        if self.is_running() || self.session.session_state.actor_mailbox.is_empty() {
            return Ok(false);
        }
        let messages = std::mem::take(&mut self.session.session_state.actor_mailbox);
        let drained_count = messages.len();
        for message in messages {
            self.apply_actor_message(message);
        }
        tracing::debug!(
            log_stream = "session",
            log_key = %self.session.id,
            kind = "actor_mailbox_drained",
            drained_count = drained_count as u64,
            message_count = self.session.history.len() as u64,
            agent_message_count = self.session.session_state.messages.len() as u64,
            "drained actor mailbox into session context"
        );
        self.session.persist()?;
        Ok(true)
    }

    fn drain_user_mailbox_if_idle(&mut self) -> Result<bool> {
        if self.is_running() || self.session.session_state.user_mailbox.is_empty() {
            return Ok(false);
        }
        let messages = std::mem::take(&mut self.session.session_state.user_mailbox);
        let drained_count = messages.len();
        for message in messages {
            self.apply_user_message(message);
        }
        tracing::debug!(
            log_stream = "session",
            log_key = %self.session.id,
            kind = "user_mailbox_drained",
            drained_count = drained_count as u64,
            message_count = self.session.history.len() as u64,
            pending_message_count = self.session.session_state.pending_messages.len() as u64,
            "drained user mailbox into pending session context"
        );
        self.session.persist()?;
        Ok(true)
    }

    fn apply_actor_message(&mut self, message: DurableSessionActorMessage) {
        if let Some(stable_text) = message
            .text
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(ToOwned::to_owned)
        {
            let role = match message.role {
                MessageRole::User => "user",
                MessageRole::Assistant => "assistant",
                MessageRole::System => "system",
            };
            self.session
                .session_state
                .messages
                .push(ChatMessage::text(role, stable_text));
        }
        self.session
            .push_message(message.role, message.text, message.attachments);
    }

    fn apply_user_message(&mut self, message: DurableSessionUserMessage) {
        let attachment_count = message.attachments.len();
        self.session
            .session_state
            .pending_messages
            .push(message.pending_message);
        self.session
            .push_message(MessageRole::User, message.text, message.attachments);
        tracing::debug!(
            log_stream = "session",
            log_key = %self.session.id,
            kind = "foreground_user_turn_staged",
            message_count = self.session.history.len() as u64,
            pending_message_count = self.session.session_state.pending_messages.len() as u64,
            attachment_count = attachment_count as u64,
            "staged user mailbox item into pending request queue and visible history"
        );
    }

    fn observe_skill_changes(
        &mut self,
        observed_skills: &[SessionSkillObservation],
        metadata_prompt: String,
    ) -> Result<Vec<SkillChangeNotice>> {
        let mut notices = Vec::new();
        let metadata_hash = prompt_component_hash(&metadata_prompt);
        let metadata_state = self
            .session
            .session_state
            .prompt_components
            .entry(SKILLS_METADATA_PROMPT_COMPONENT.to_string())
            .or_default();
        if metadata_state.system_prompt_hash.is_empty()
            && metadata_state.system_prompt_value.is_empty()
            && metadata_state.notified_hash.is_empty()
            && metadata_state.notified_value.is_empty()
        {
            metadata_state.system_prompt_value = metadata_prompt.clone();
            metadata_state.system_prompt_hash = metadata_hash.clone();
            metadata_state.notified_value = metadata_prompt.clone();
            metadata_state.notified_hash = metadata_hash.clone();
        } else if metadata_state.notified_hash != metadata_hash {
            notices.push(SkillChangeNotice::MetadataChanged {
                metadata_prompt: metadata_prompt.clone(),
            });
            metadata_state.notified_value = metadata_prompt.clone();
            metadata_state.notified_hash = metadata_hash.clone();
        }

        let last_compacted_turn_count = self.session.last_compacted_turn_count;
        for observed in observed_skills {
            match self.session.skill_states.get_mut(&observed.name) {
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
                            .is_some_and(|turn| turn > last_compacted_turn_count)
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
                    self.session.skill_states.insert(
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
        self.session.persist()?;
        Ok(notices)
    }

    fn observe_prompt_component_change(
        &mut self,
        key: String,
        value: String,
    ) -> Result<Option<PromptComponentChangeNotice>> {
        let hash = prompt_component_hash(&value);
        let state = self
            .session
            .session_state
            .prompt_components
            .entry(key.clone())
            .or_default();
        let notice = if state.system_prompt_hash.is_empty()
            && state.system_prompt_value.is_empty()
            && state.notified_hash.is_empty()
            && state.notified_value.is_empty()
        {
            state.system_prompt_value = value.clone();
            state.system_prompt_hash = hash.clone();
            state.notified_value = value;
            state.notified_hash = hash;
            None
        } else if state.notified_hash != hash {
            state.notified_value = value.clone();
            state.notified_hash = hash;
            Some(PromptComponentChangeNotice { key, value })
        } else {
            None
        };
        self.session.persist()?;
        Ok(notice)
    }

    fn initialize_prompt_component_if_missing(&mut self, key: String, value: String) -> Result<()> {
        let state = self
            .session
            .session_state
            .prompt_components
            .entry(key)
            .or_default();
        if state.system_prompt_hash.is_empty()
            && state.system_prompt_value.is_empty()
            && state.notified_hash.is_empty()
            && state.notified_value.is_empty()
        {
            let hash = prompt_component_hash(&value);
            state.system_prompt_value = value.clone();
            state.system_prompt_hash = hash.clone();
            state.notified_value = value;
            state.notified_hash = hash;
            self.session.persist()?;
        }
        Ok(())
    }

    fn observe_system_prompt_state(
        &mut self,
        static_hash: String,
    ) -> Result<SystemPromptStateObservation> {
        let mut static_changed = false;
        match self
            .session
            .session_state
            .system_prompt_static_hash
            .as_deref()
        {
            None => {
                self.session.session_state.system_prompt_static_hash = Some(static_hash);
            }
            Some(previous) if previous != static_hash => {
                self.session.session_state.system_prompt_static_hash = Some(static_hash);
                static_changed = true;
            }
            Some(_) => {}
        }

        let observation = SystemPromptStateObservation { static_changed };
        self.session.persist()?;
        Ok(observation)
    }

    fn mark_system_prompt_state_current(&mut self, static_hash: String) -> Result<()> {
        self.session.session_state.system_prompt_static_hash = Some(static_hash);
        self.session.persist()
    }

    fn mark_skills_loaded_current_turn(&mut self, skill_names: &[String]) -> Result<()> {
        if skill_names.is_empty() {
            return Ok(());
        }
        for skill_name in skill_names {
            self.session
                .skill_states
                .entry(skill_name.clone())
                .or_insert_with(SessionSkillState::default)
                .last_loaded_turn = Some(self.session.turn_count);
        }
        self.session.persist()
    }

    fn commit_runtime_turn(&mut self, commit: SessionRuntimeTurnCommit) -> Result<()> {
        let consumed_pending_messages = match self.session.kind {
            SessionKind::Foreground => commit.consumed_pending_messages.as_slice(),
            SessionKind::Background => &[],
        };
        let log_kind = match commit.phase {
            SessionPhase::Yielded => "agent_turn_yielded",
            _ => "agent_turn_recorded",
        };
        record_turn(
            &mut self.session,
            commit.messages,
            consumed_pending_messages,
            &commit.usage,
            &commit.compaction,
            commit.phase,
            log_kind,
        )?;
        self.mark_system_prompt_state_after_compaction(
            &commit.compaction,
            commit.system_prompt_static_hash_after_compaction,
        )?;
        self.mark_skills_loaded_current_turn(&commit.loaded_skills)?;
        if let Some(text) = commit.user_history_text {
            self.append_visible_message(MessageRole::User, Some(text), Vec::new())?;
        }
        if let Some(text) = commit.assistant_history_text {
            self.append_visible_message(MessageRole::Assistant, Some(text), Vec::new())?;
        }
        Ok(())
    }

    fn fail_runtime_turn(&mut self, failure: SessionRuntimeTurnFailure) -> Result<()> {
        self.set_failed_turn(failure.resume_messages, failure.errno, failure.errinfo)?;
        self.mark_system_prompt_state_after_compaction(
            &failure.compaction,
            failure.system_prompt_static_hash_after_compaction,
        )
    }

    fn mark_system_prompt_state_after_compaction(
        &mut self,
        compaction: &SessionCompactionStats,
        static_hash: Option<String>,
    ) -> Result<()> {
        if compaction.compacted_run_count == 0 {
            return Ok(());
        }
        let Some(static_hash) = static_hash else {
            return Ok(());
        };
        self.promote_notified_prompt_components_after_compaction();
        self.mark_system_prompt_state_current(static_hash)
    }

    fn promote_notified_prompt_components_after_compaction(&mut self) {
        for state in self.session.session_state.prompt_components.values_mut() {
            if state.notified_hash.is_empty() && state.notified_value.is_empty() {
                continue;
            }
            state.system_prompt_value = state.notified_value.clone();
            state.system_prompt_hash = state.notified_hash.clone();
        }
    }

    fn update_checkpoint(
        &mut self,
        messages: Vec<ChatMessage>,
        usage: &TokenUsage,
        compaction: &SessionCompactionStats,
    ) -> Result<()> {
        commit_stable_messages(&mut self.session, messages, Vec::new(), SessionPhase::End);
        self.session.last_agent_returned_at = Some(Utc::now());
        self.session.cumulative_usage.add_assign(usage);
        accumulate_compaction_stats(&mut self.session, compaction);
        self.session.persist()
    }

    fn record_idle_compaction(
        &mut self,
        messages: Vec<ChatMessage>,
        compaction: &SessionCompactionStats,
    ) -> Result<()> {
        let pending_messages = self.session.session_state.pending_messages.clone();
        let phase = self.session.session_state.phase;
        commit_stable_messages(&mut self.session, messages, pending_messages, phase);
        self.session.last_compacted_at = Some(Utc::now());
        self.session.last_compacted_turn_count = self.session.turn_count;
        self.promote_notified_prompt_components_after_compaction();
        accumulate_compaction_stats(&mut self.session, compaction);
        if self.session.session_state.errno == Some(SessionErrno::IdleCompactionFailure) {
            self.session.session_state.errno = None;
            self.session.session_state.errinfo = None;
        }
        info!(
            log_stream = "session",
            log_key = %self.session.id,
            kind = "idle_context_compacted",
            agent_message_count = self.session.session_state.messages.len() as u64,
            turn_count = self.session.turn_count,
            "persisted idle context compaction"
        );
        self.session.persist()
    }

    fn mark_idle_compaction_failed(&mut self, error_summary: String) -> Result<()> {
        self.session.session_state.errno = Some(SessionErrno::IdleCompactionFailure);
        self.session.session_state.errinfo = Some(error_summary);
        self.session.persist()
    }

    fn clear_idle_compaction_failure(&mut self) -> Result<()> {
        if self.session.session_state.errno == Some(SessionErrno::IdleCompactionFailure) {
            self.session.session_state.errno = None;
            self.session.session_state.errinfo = None;
        }
        self.session.persist()
    }

    fn export_checkpoint(&self) -> SessionCheckpointData {
        SessionCheckpointData {
            history: self.session.history.clone(),
            messages: self.session.session_state.messages.clone(),
            last_user_message_at: self.session.last_user_message_at,
            last_agent_returned_at: self.session.last_agent_returned_at,
            last_compacted_at: self.session.last_compacted_at,
            turn_count: self.session.turn_count,
            last_compacted_turn_count: self.session.last_compacted_turn_count,
            cumulative_usage: self.session.cumulative_usage.clone(),
            cumulative_compaction: self.session.cumulative_compaction.clone(),
            api_timeout_override_seconds: self.session.api_timeout_override_seconds,
            skill_states: self.session.skill_states.clone(),
            system_prompt_static_hash: self.session.session_state.system_prompt_static_hash.clone(),
            prompt_components: self.session.session_state.prompt_components.clone(),
            actor_mailbox: self.session.session_state.actor_mailbox.clone(),
            user_mailbox: self.session.session_state.user_mailbox.clone(),
            current_plan: self.session.session_state.current_plan.clone(),
        }
    }
}

impl SessionSnapshot {
    pub fn stable_messages(&self) -> &[ChatMessage] {
        &self.session_state.messages
    }

    pub fn stable_message_count(&self) -> usize {
        self.stable_messages().len()
    }

    pub(crate) fn prompt_component_system_value(&self, key: &str) -> Option<&str> {
        self.session_state
            .prompt_components
            .get(key)
            .map(|state| state.system_prompt_value.as_str())
    }

    pub fn request_messages(&self) -> Vec<ChatMessage> {
        let mut messages = self.session_state.messages.clone();
        messages.extend(self.session_state.pending_messages.clone());
        messages
    }
}

#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SessionKind {
    #[default]
    Foreground,
    Background,
}

pub(crate) fn session_conversation_dir_name(conversation_id: &str) -> String {
    let mut encoded = String::new();
    for byte in conversation_id.as_bytes() {
        let character = *byte as char;
        if character.is_ascii_alphanumeric() || matches!(character, '-' | '_') {
            encoded.push(character);
        } else {
            encoded.push_str(&format!("%{byte:02X}"));
        }
    }
    if encoded.is_empty() {
        "_".to_string()
    } else {
        encoded
    }
}

pub(crate) fn session_kind_dir_name(kind: SessionKind) -> &'static str {
    match kind {
        SessionKind::Foreground => "foreground",
        SessionKind::Background => "background",
    }
}

pub(crate) fn session_root_dir(
    sessions_root: &Path,
    address: &ChannelAddress,
    kind: SessionKind,
    session_id: Uuid,
) -> PathBuf {
    sessions_root
        .join(session_conversation_dir_name(&address.conversation_id))
        .join(session_kind_dir_name(kind))
        .join(session_id.to_string())
}

pub(crate) fn find_session_roots(sessions_root: &Path) -> Result<Vec<PathBuf>> {
    let mut roots = Vec::new();
    collect_session_roots(sessions_root, &mut roots)?;
    roots.sort();
    Ok(roots)
}

fn collect_session_roots(root: &Path, roots: &mut Vec<PathBuf>) -> Result<()> {
    if !root.is_dir() {
        return Ok(());
    }
    for entry in fs::read_dir(root).with_context(|| format!("failed to read {}", root.display()))? {
        let path = entry?.path();
        if !path.is_dir() {
            continue;
        }
        if path.join("session.json").is_file() {
            roots.push(path);
        } else {
            collect_session_roots(&path, roots)?;
        }
    }
    Ok(())
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
    last_user_message_at: Option<DateTime<Utc>>,
    last_agent_returned_at: Option<DateTime<Utc>>,
    last_compacted_at: Option<DateTime<Utc>>,
    turn_count: u64,
    last_compacted_turn_count: u64,
    cumulative_usage: TokenUsage,
    cumulative_compaction: SessionCompactionStats,
    api_timeout_override_seconds: Option<f64>,
    skill_states: HashMap<String, SessionSkillState>,
    session_state: DurableSessionState,
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
            kind: self.kind,
            id: self.id,
            agent_id: self.agent_id,
            address: self.address.clone(),
            root_dir: self.root_dir.clone(),
            attachments_dir: self.attachments_dir.clone(),
            workspace_id: self.workspace_id.clone(),
            workspace_root: self.workspace_root.clone(),
            last_user_message_at: self.last_user_message_at,
            last_agent_returned_at: self.last_agent_returned_at,
            last_compacted_at: self.last_compacted_at,
            turn_count: self.turn_count,
            last_compacted_turn_count: self.last_compacted_turn_count,
            cumulative_usage: self.cumulative_usage.clone(),
            cumulative_compaction: self.cumulative_compaction.clone(),
            api_timeout_override_seconds: self.api_timeout_override_seconds,
            skill_states: self.skill_states.clone(),
            pending_workspace_summary: self.pending_workspace_summary,
            close_after_summary: self.close_after_summary,
            session_state: self.session_state.clone(),
        }
    }

    fn push_message(
        &mut self,
        role: MessageRole,
        text: Option<String>,
        attachments: Vec<StoredAttachment>,
    ) {
        if role == MessageRole::User {
            self.last_user_message_at = Some(Utc::now());
        }
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
            last_user_message_at: self.last_user_message_at,
            last_agent_returned_at: self.last_agent_returned_at,
            last_compacted_at: self.last_compacted_at,
            turn_count: self.turn_count,
            last_compacted_turn_count: self.last_compacted_turn_count,
            cumulative_usage: self.cumulative_usage.clone(),
            cumulative_compaction: self.cumulative_compaction.clone(),
            api_timeout_override_seconds: self.api_timeout_override_seconds,
            skill_states: self.skill_states.clone(),
            session_state: self.session_state.clone(),
            pending_workspace_summary: self.pending_workspace_summary,
            close_after_summary: self.close_after_summary,
            closed_at: self.closed_at,
        };
        let raw =
            serde_json::to_string_pretty(&state).context("failed to serialize session state")?;
        write_file_atomically(&self.state_path(), raw.as_bytes())
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
            kind: persisted.kind,
            id: persisted.id,
            agent_id: persisted.agent_id,
            address: persisted.address,
            root_dir,
            attachments_dir,
            workspace_id,
            workspace_root,
            history: persisted.history,
            last_user_message_at: persisted.last_user_message_at,
            last_agent_returned_at: persisted.last_agent_returned_at,
            last_compacted_at: persisted.last_compacted_at,
            turn_count: persisted.turn_count,
            last_compacted_turn_count: persisted.last_compacted_turn_count,
            cumulative_usage: persisted.cumulative_usage,
            cumulative_compaction: persisted.cumulative_compaction,
            api_timeout_override_seconds: persisted.api_timeout_override_seconds,
            skill_states: persisted.skill_states,
            session_state: persisted.session_state,
            pending_workspace_summary: persisted.pending_workspace_summary,
            close_after_summary: persisted.close_after_summary,
            closed_at: persisted.closed_at,
        })
    }
}

fn write_file_atomically(path: &Path, contents: &[u8]) -> Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| anyhow!("path {} has no parent", path.display()))?;
    fs::create_dir_all(parent).with_context(|| format!("failed to create {}", parent.display()))?;
    let file_name = path
        .file_name()
        .and_then(|value| value.to_str())
        .unwrap_or("state");
    let temp_path = parent.join(format!(".{file_name}.{}.tmp", Uuid::new_v4().simple()));
    fs::write(&temp_path, contents)
        .with_context(|| format!("failed to write {}", temp_path.display()))?;
    fs::rename(&temp_path, path).with_context(|| {
        format!(
            "failed to move {} into place at {}",
            temp_path.display(),
            path.display()
        )
    })?;
    Ok(())
}

fn record_turn(
    session: &mut Session,
    messages: Vec<ChatMessage>,
    consumed_pending_messages: &[ChatMessage],
    usage: &TokenUsage,
    compaction: &SessionCompactionStats,
    phase: SessionPhase,
    log_kind: &str,
) -> Result<()> {
    let remaining_pending_messages = if consumed_pending_messages.is_empty() {
        session.session_state.pending_messages.clone()
    } else if session
        .session_state
        .pending_messages
        .starts_with(consumed_pending_messages)
    {
        session.session_state.pending_messages[consumed_pending_messages.len()..].to_vec()
    } else {
        session.session_state.pending_messages.clone()
    };
    commit_stable_messages(session, messages, remaining_pending_messages, phase);
    session.last_agent_returned_at = Some(Utc::now());
    session.turn_count = session.turn_count.saturating_add(1);
    session.cumulative_usage.add_assign(usage);
    accumulate_compaction_stats(session, compaction);
    info!(
        log_stream = "session",
        log_key = %session.id,
        kind = log_kind,
        agent_message_count = session.session_state.messages.len() as u64,
        turn_count = session.turn_count,
        "recorded agent turn"
    );
    session.persist()?;
    Ok(())
}

fn commit_stable_messages(
    session: &mut Session,
    messages: Vec<ChatMessage>,
    pending_messages: Vec<ChatMessage>,
    phase: SessionPhase,
) {
    session.session_state.messages = messages;
    session.session_state.pending_messages = pending_messages;
    session.session_state.phase = phase;
    session.session_state.errno = None;
    session.session_state.errinfo = None;
}

fn accumulate_compaction_stats(session: &mut Session, compaction: &SessionCompactionStats) {
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
    last_user_message_at: Option<DateTime<Utc>>,
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
    session_state: DurableSessionState,
    #[serde(default)]
    pending_workspace_summary: bool,
    #[serde(default)]
    close_after_summary: bool,
    #[serde(default)]
    closed_at: Option<DateTime<Utc>>,
}

pub fn tag_interrupted_followup_text(text: Option<String>) -> Option<String> {
    match text {
        Some(text) if !text.trim().is_empty() => {
            Some(format!("{INTERRUPTED_FOLLOWUP_MARKER}\n{text}"))
        }
        _ => Some(INTERRUPTED_FOLLOWUP_MARKER.to_string()),
    }
}

fn tag_interrupted_user_chat_message(message: &mut ChatMessage) {
    match message.content.as_mut() {
        Some(serde_json::Value::String(text)) => {
            let tagged = tag_interrupted_followup_text(Some(std::mem::take(text)))
                .unwrap_or_else(|| INTERRUPTED_FOLLOWUP_MARKER.to_string());
            *text = tagged;
        }
        Some(serde_json::Value::Array(parts)) => {
            let mut tagged_existing_text = false;
            for part in parts.iter_mut() {
                let Some(object) = part.as_object_mut() else {
                    continue;
                };
                if object.get("type").and_then(serde_json::Value::as_str) != Some("text") {
                    continue;
                }
                let Some(serde_json::Value::String(text)) = object.get_mut("text") else {
                    continue;
                };
                let tagged = tag_interrupted_followup_text(Some(std::mem::take(text)))
                    .unwrap_or_else(|| INTERRUPTED_FOLLOWUP_MARKER.to_string());
                *text = tagged;
                tagged_existing_text = true;
                break;
            }
            if !tagged_existing_text {
                parts.insert(
                    0,
                    serde_json::json!({
                        "type": "text",
                        "text": INTERRUPTED_FOLLOWUP_MARKER,
                    }),
                );
            }
        }
        _ => {
            message.content = Some(serde_json::Value::String(
                INTERRUPTED_FOLLOWUP_MARKER.to_string(),
            ));
        }
    }
}

fn session_runtime_phase_for_event(event: &SessionEvent) -> Option<SessionRuntimePhase> {
    match event {
        SessionEvent::CompactionStarted { .. } | SessionEvent::ToolWaitCompactionStarted { .. } => {
            Some(SessionRuntimePhase::Compacting)
        }
        SessionEvent::CompactionCompleted { .. }
        | SessionEvent::ToolWaitCompactionCompleted { .. }
        | SessionEvent::SessionStarted { .. }
        | SessionEvent::RoundStarted { .. }
        | SessionEvent::ModelCallStarted { .. }
        | SessionEvent::ModelCallCompleted { .. }
        | SessionEvent::ToolWaitCompactionScheduled { .. }
        | SessionEvent::ToolCallStarted { .. }
        | SessionEvent::ToolCallCompleted { .. }
        | SessionEvent::SessionYielded { .. }
        | SessionEvent::PrefixRewriteApplied { .. }
        | SessionEvent::SessionCompleted { .. } => Some(SessionRuntimePhase::Running),
    }
}

fn render_system_date_on_user_message(now: DateTime<Utc>) -> String {
    let local_now = now.with_timezone(&chrono::Local);
    format!(
        "[System Date: {}]",
        local_now.format("%Y-%m-%d %H:%M:%S %:z")
    )
}

fn progress_text(model_key: &str, activity: &str) -> String {
    progress_text_with_plan(model_key, activity, None)
}

fn progress_text_with_plan(model_key: &str, activity: &str, plan: Option<&SessionPlan>) -> String {
    let mut text = format!(
        "⚙️ 正在执行\n🤖 模型：{}\n📌 阶段：{}\n\n💡 发送新消息可打断；/continue 可继续最近中断的回合。",
        model_key, activity
    );
    if let Some(plan_text) = render_plan_progress(plan) {
        text.push_str("\n\n");
        text.push_str(&plan_text);
    }
    text
}

fn progress_text_for_execution(
    model_key: &str,
    progress: &ExecutionProgress,
    plan: Option<&SessionPlan>,
) -> String {
    let mut text = match progress.phase {
        ExecutionProgressPhase::Thinking => format!(
            "⚙️ 正在执行\n🤖 模型：{}\n🧠 状态：思考中...\n\n💡 发送新消息可打断；/continue 可继续最近中断的回合。",
            model_key
        ),
        ExecutionProgressPhase::Tools => {
            let mut lines = vec!["⚙️ 正在执行".to_string(), format!("🤖 模型：{model_key}")];
            lines.push("🔧 状态：工具执行中".to_string());
            for tool in &progress.tools {
                let status_icon = match tool.status {
                    agent_frame::ToolExecutionStatus::Running => "⏳",
                    agent_frame::ToolExecutionStatus::Completed => "✅",
                    agent_frame::ToolExecutionStatus::Failed => "❌",
                };
                lines.push(format!(
                    "  {} {}：{}",
                    status_icon,
                    tool.tool_name,
                    render_tool_brief_arguments(&tool.tool_name, tool.arguments.as_deref())
                ));
            }
            lines.push(String::new());
            lines.push("💡 发送新消息可打断；/continue 可继续最近中断的回合。".to_string());
            lines.join("\n")
        }
    };
    if let Some(plan_text) = render_plan_progress(plan) {
        text.push_str("\n\n");
        text.push_str(&plan_text);
    }
    text
}

fn render_cache_warning_text(_address: &ChannelAddress, warning: CacheWarningKind) -> String {
    let trigger = match warning {
        CacheWarningKind::ConsecutiveZeroReads => {
            "连续 3 次模型调用的 cache read 都是 0，且它们之间的间隔都不超过 5 分钟"
        }
        CacheWarningKind::BurstyZeroReads => {
            "最近 10 次模型调用里已有 2 次 cache read 为 0，且它们之间的间隔都不超过 5 分钟，且中间没有发生压缩"
        }
    };
    format!(
        "缓存告警：检测到 prompt cache 可能没有正常命中。\n触发条件：{trigger}。\n建议检查最近的 system/runtime notice、provider 路由、历史消息形状或 cache_control 是否发生变化。"
    )
}

fn render_plan_progress(plan: Option<&SessionPlan>) -> Option<String> {
    let plan = plan?;
    if plan.steps.is_empty() {
        return None;
    }
    let mut lines = Vec::with_capacity(plan.steps.len() + 2);
    lines.push("📋 执行计划：".to_string());
    if let Some(explanation) = plan
        .explanation
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        lines.push(format!(
            "  💬 {}",
            truncate_single_line_strict(explanation, 60)
        ));
    }
    for step in plan.steps.iter().take(7) {
        let marker = match step.status {
            SessionPlanStepStatus::Completed => "✅",
            SessionPlanStepStatus::InProgress => "▶️",
            SessionPlanStepStatus::Pending => "⬜",
        };
        lines.push(format!(
            "  {} {}",
            marker,
            truncate_single_line_strict(&step.step, 50)
        ));
    }
    Some(lines.join("\n"))
}

fn render_tool_brief_arguments(tool_name: &str, arguments: Option<&str>) -> String {
    let args = arguments
        .and_then(|raw| serde_json::from_str::<serde_json::Value>(raw).ok())
        .unwrap_or(serde_json::Value::Null);
    let object = args.as_object();
    let detail = match tool_name {
        "shell" => first_string(object, &["cmd", "command"]),
        "shell_close" => first_string(object, &["session_id", "id"]),
        "file_read" | "read_file" | "file_write" | "file_edit" | "ls" | "image_load"
        | "pdf_read" | "audio_transcribe" => first_string(object, &["path", "file_path"]),
        "glob" | "grep" | "web_search" => first_string(object, &["pattern", "query", "q"]),
        "web_fetch" => first_string(object, &["url"]),
        "image_generate" | "subagent_start" | "spawn_agent" => {
            first_string(object, &["prompt", "message", "task"])
        }
        _ => object.and_then(|object| {
            object
                .values()
                .find_map(|value| value.as_str().filter(|value| !value.trim().is_empty()))
        }),
    };
    detail
        .map(|value| truncate_single_line_strict(value, 20))
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| "-".to_string())
}

fn first_string<'a>(
    object: Option<&'a serde_json::Map<String, serde_json::Value>>,
    keys: &[&str],
) -> Option<&'a str> {
    let object = object?;
    keys.iter()
        .find_map(|key| object.get(*key).and_then(serde_json::Value::as_str))
}

fn truncate_single_line(input: &str, max_chars: usize) -> String {
    let compact = input.split_whitespace().collect::<Vec<_>>().join(" ");
    if compact.chars().count() <= max_chars {
        return compact;
    }
    let mut out = compact.chars().take(max_chars).collect::<String>();
    out.push_str("...");
    out
}

fn truncate_single_line_strict(input: &str, max_chars: usize) -> String {
    let compact = input.split_whitespace().collect::<Vec<_>>().join(" ");
    if compact.chars().count() <= max_chars {
        return compact;
    }
    if max_chars <= 3 {
        return compact.chars().take(max_chars).collect();
    }
    let mut out = compact.chars().take(max_chars - 3).collect::<String>();
    out.push_str("...");
    out
}

pub struct SessionManager {
    sessions_root: PathBuf,
    workspace_manager: WorkspaceManager,
    foreground_actors: HashMap<String, SessionActorRef>,
    background_actors: HashMap<Uuid, SessionActorRef>,
}

fn join_session_actor_worker(worker: Option<thread::JoinHandle<()>>) -> Result<()> {
    if let Some(worker) = worker {
        worker
            .join()
            .map_err(|_| anyhow!("session actor worker panicked"))?;
    }
    Ok(())
}

fn actor_ref(session: Session) -> SessionActorRef {
    let session_id = session.id;
    let actor = Arc::new(Mutex::new(SessionActor::new(session)));
    let (sender, receiver) = mpsc::channel::<SessionActorCommand>();
    let worker_actor = Arc::clone(&actor);
    let worker = thread::Builder::new()
        .name(format!("session-actor-{session_id}"))
        .spawn(move || {
            for command in receiver {
                match command {
                    SessionActorCommand::Run(command) => {
                        let mut actor = worker_actor
                            .lock()
                            .unwrap_or_else(|poisoned| poisoned.into_inner());
                        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                            command(&mut actor);
                        }));
                        if result.is_err() {
                            warn!(
                                log_stream = "session",
                                log_key = %session_id,
                                kind = "session_actor_command_panicked",
                                "session actor command panicked and was dropped"
                            );
                        }
                    }
                    SessionActorCommand::Shutdown(done) => {
                        let _ = done.send(());
                        break;
                    }
                }
            }
        })
        .expect("failed to spawn session actor worker");

    SessionActorRef {
        inner: Arc::new(SessionActorHandleInner {
            sender,
            lifecycle: Mutex::new(SessionActorLifecycle {
                closing: false,
                worker: Some(worker),
            }),
        }),
    }
}

impl SessionManager {
    pub fn new(workdir: impl AsRef<Path>, workspace_manager: WorkspaceManager) -> Result<Self> {
        let sessions_root = workdir.as_ref().join("sessions");
        fs::create_dir_all(&sessions_root)
            .with_context(|| format!("failed to create {}", sessions_root.display()))?;
        let foreground_actors = load_persisted_sessions(&sessions_root, &workspace_manager)?;
        Ok(Self {
            sessions_root,
            workspace_manager,
            foreground_actors,
            background_actors: HashMap::new(),
        })
    }

    pub fn ensure_foreground_actor(&mut self, address: &ChannelAddress) -> Result<SessionActorRef> {
        let key = address.session_key();
        if !self.foreground_actors.contains_key(&key) {
            let session = self.create_session_with_optional_workspace(address, None)?;
            self.insert_foreground_actor(key.clone(), session);
        }
        self.resolve_foreground(&key)
    }

    pub fn ensure_foreground_in_workspace_actor(
        &mut self,
        address: &ChannelAddress,
        workspace_id: &str,
    ) -> Result<SessionActorRef> {
        let key = address.session_key();
        if !self.foreground_actors.contains_key(&key) {
            let session =
                self.create_session_with_optional_workspace(address, Some(workspace_id))?;
            self.insert_foreground_actor(key.clone(), session);
        }
        self.resolve_foreground(&key)
    }

    fn insert_foreground_actor(&mut self, key: String, session: Session) {
        info!(
            log_stream = "session",
            log_key = %session.id,
            kind = "session_created",
            channel_id = %session.address.channel_id,
            conversation_id = %session.address.conversation_id,
            root_dir = %session.root_dir.display(),
            workspace_id = %session.workspace_id,
            "created foreground session"
        );
        self.foreground_actors.insert(key, actor_ref(session));
    }

    pub fn destroy_foreground(&mut self, address: &ChannelAddress) -> Result<()> {
        let key = address.session_key();
        if let Some(actor) = self.foreground_actors.remove(&key) {
            let snapshot = actor.snapshot()?;
            info!(
                log_stream = "session",
                log_key = %snapshot.id,
                kind = "session_destroying",
                root_dir = %snapshot.root_dir.display(),
                "destroying foreground session"
            );
            actor.close_and_shutdown()?;
            info!(
                log_stream = "session",
                log_key = %snapshot.id,
                kind = "session_destroyed",
                root_dir = %snapshot.root_dir.display(),
                "foreground session closed and actor worker stopped"
            );
        }
        Ok(())
    }

    pub fn remove_foreground(&mut self, address: &ChannelAddress) -> Result<bool> {
        let key = address.session_key();
        let Some(actor) = self.foreground_actors.remove(&key) else {
            return Ok(false);
        };
        let snapshot = actor.snapshot()?;
        info!(
            log_stream = "session",
            log_key = %snapshot.id,
            kind = "session_removing",
            root_dir = %snapshot.root_dir.display(),
            "removing foreground session"
        );
        actor.close_and_shutdown()?;
        if snapshot.root_dir.exists() {
            fs::remove_dir_all(&snapshot.root_dir)
                .with_context(|| format!("failed to remove {}", snapshot.root_dir.display()))?;
        }
        Ok(true)
    }

    pub fn get_snapshot(&self, address: &ChannelAddress) -> Option<SessionSnapshot> {
        self.foreground_actors
            .get(&address.session_key())
            .and_then(|actor| actor.snapshot().ok())
    }

    pub fn resolve_foreground(&self, session_key: &str) -> Result<SessionActorRef> {
        self.foreground_actors
            .get(session_key)
            .cloned()
            .with_context(|| format!("no active session for {}", session_key))
    }

    pub fn resolve_foreground_by_address(
        &self,
        address: &ChannelAddress,
    ) -> Result<SessionActorRef> {
        self.resolve_foreground(&address.session_key())
    }

    pub fn resolve_background(&self, session_id: Uuid) -> Result<SessionActorRef> {
        self.background_actors
            .get(&session_id)
            .cloned()
            .with_context(|| format!("no active background session for {}", session_id))
    }

    pub fn resolve_snapshot(&self, snapshot: &SessionSnapshot) -> Result<SessionActorRef> {
        match snapshot.kind {
            SessionKind::Foreground => self.resolve_foreground_by_address(&snapshot.address),
            SessionKind::Background => self.resolve_background(snapshot.id),
        }
    }

    pub fn list_foreground_snapshots(&self) -> Vec<SessionSnapshot> {
        self.foreground_actors
            .values()
            .filter_map(|actor| actor.snapshot().ok())
            .collect()
    }

    pub fn create_background_actor(
        &mut self,
        address: &ChannelAddress,
        agent_id: Uuid,
    ) -> Result<SessionActorRef> {
        self.create_background_actor_with_optional_workspace(address, agent_id, None)
    }

    pub fn create_background_in_workspace_actor(
        &mut self,
        address: &ChannelAddress,
        agent_id: Uuid,
        workspace_id: &str,
    ) -> Result<SessionActorRef> {
        self.create_background_actor_with_optional_workspace(address, agent_id, Some(workspace_id))
    }

    fn create_background_actor_with_optional_workspace(
        &mut self,
        address: &ChannelAddress,
        agent_id: Uuid,
        workspace_id: Option<&str>,
    ) -> Result<SessionActorRef> {
        let session = self.create_session_with_kind(
            address,
            agent_id,
            workspace_id,
            SessionKind::Background,
        )?;
        let session_id = session.id;
        self.background_actors
            .insert(session_id, actor_ref(session));
        self.resolve_background(session_id)
    }

    pub fn background_snapshot(&self, session_id: Uuid) -> Result<SessionSnapshot> {
        let actor = self.resolve_background(session_id)?;
        actor.snapshot()
    }

    pub fn close_background(&mut self, session_id: Uuid) -> Result<()> {
        if let Some(actor) = self.background_actors.remove(&session_id) {
            actor.close_and_shutdown()?;
        }
        Ok(())
    }

    pub fn pending_workspace_summary_snapshots(&self) -> Vec<SessionSnapshot> {
        self.foreground_actors
            .values()
            .filter_map(|actor| {
                let snapshot = actor.snapshot().ok()?;
                snapshot.pending_workspace_summary.then_some(snapshot)
            })
            .collect()
    }

    pub fn export_checkpoint(&self, address: &ChannelAddress) -> Result<SessionCheckpointData> {
        let actor = self.resolve_foreground_by_address(address)?;
        actor.export_checkpoint()
    }

    pub fn restore_foreground_from_checkpoint(
        &mut self,
        address: &ChannelAddress,
        checkpoint: SessionCheckpointData,
        workspace_id: String,
        workspace_root: PathBuf,
    ) -> Result<SessionSnapshot> {
        validate_conversation_id(&address.conversation_id)?;
        self.destroy_foreground(address)?;
        let checkpoint_messages = checkpoint.messages.clone();
        let session_id = Uuid::new_v4();
        let agent_id = Uuid::new_v4();
        let root_dir = session_root_dir(
            &self.sessions_root,
            address,
            SessionKind::Foreground,
            session_id,
        );
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
            last_user_message_at: checkpoint.last_user_message_at,
            last_agent_returned_at: checkpoint.last_agent_returned_at,
            last_compacted_at: checkpoint.last_compacted_at,
            turn_count: checkpoint.turn_count,
            last_compacted_turn_count: checkpoint.last_compacted_turn_count,
            cumulative_usage: checkpoint.cumulative_usage,
            cumulative_compaction: checkpoint.cumulative_compaction,
            api_timeout_override_seconds: checkpoint.api_timeout_override_seconds,
            skill_states: checkpoint.skill_states,
            session_state: DurableSessionState {
                messages: checkpoint_messages,
                pending_messages: Vec::new(),
                system_prompt_static_hash: checkpoint.system_prompt_static_hash,
                prompt_components: checkpoint.prompt_components,
                phase: SessionPhase::End,
                errno: None,
                errinfo: None,
                progress_message: None,
                actor_mailbox: checkpoint.actor_mailbox,
                user_mailbox: checkpoint.user_mailbox,
                current_plan: checkpoint.current_plan,
            },
            pending_workspace_summary: false,
            close_after_summary: false,
            closed_at: None,
        };
        session.persist()?;
        let key = address.session_key();
        self.foreground_actors
            .insert(key.clone(), actor_ref(session));
        let actor = self.resolve_foreground(&key)?;
        actor.snapshot()
    }

    fn create_session_with_optional_workspace(
        &self,
        address: &ChannelAddress,
        workspace_id: Option<&str>,
    ) -> Result<Session> {
        self.create_session_with_kind(
            address,
            Uuid::new_v4(),
            workspace_id,
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
        validate_conversation_id(&address.conversation_id)?;
        let session_id = Uuid::new_v4();
        let workspace = match workspace_id {
            Some(workspace_id) => self
                .workspace_manager
                .ensure_workspace_exists(workspace_id)?,
            None => self
                .workspace_manager
                .create_workspace(agent_id, session_id, None)?,
        };
        let root_dir = session_root_dir(&self.sessions_root, address, kind, session_id);
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
            last_user_message_at: None,
            last_agent_returned_at: None,
            last_compacted_at: None,
            turn_count: 0,
            last_compacted_turn_count: 0,
            cumulative_usage: TokenUsage::default(),
            cumulative_compaction: SessionCompactionStats::default(),
            api_timeout_override_seconds: None,
            skill_states: HashMap::new(),
            session_state: DurableSessionState::default(),
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
) -> Result<HashMap<String, SessionActorRef>> {
    let mut sessions = HashMap::new();
    for path in find_session_roots(sessions_root)? {
        let state_path = path.join("session.json");
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
                let actor = actor_ref(session);
                actor.drain_mailboxes_if_idle()?;
                sessions.insert(key, actor);
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
        IDENTITY_PROMPT_COMPONENT, SKILLS_METADATA_PROMPT_COMPONENT, SessionActorMessage,
        SessionActorOutbound, SessionCacheHealthState, SessionEffect, SessionErrno, SessionKind,
        SessionManager, SessionPhase, SessionRuntimeTurnCommit, SessionRuntimeTurnFailure,
        SessionSkillObservation, SkillChangeNotice, session_conversation_dir_name,
    };
    use crate::channel::ProgressFeedbackFinalState;
    use crate::domain::{ChannelAddress, MessageRole};
    use crate::workspace::WorkspaceManager;
    use agent_frame::{
        ChatMessage, ExecutionProgress, ExecutionProgressPhase, SessionCompactionStats,
        SessionEvent, SessionExecutionControl, TokenUsage,
    };
    use chrono::Utc;
    use std::fs;
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

    fn ensure_foreground_snapshot(
        sessions: &mut SessionManager,
        address: &ChannelAddress,
    ) -> super::SessionSnapshot {
        let actor = sessions.ensure_foreground_actor(address).unwrap();
        actor.snapshot().unwrap()
    }

    fn foreground_actor(
        sessions: &SessionManager,
        address: &ChannelAddress,
    ) -> super::SessionActorRef {
        sessions.resolve_foreground_by_address(address).unwrap()
    }

    fn user_message(text: &str) -> super::SessionUserMessage {
        super::SessionUserMessage {
            pending_message: ChatMessage::text("user", text),
            text: Some(text.to_string()),
            attachments: Vec::new(),
        }
    }

    fn model_call_completed_event(cache_read_tokens: u64) -> SessionEvent {
        SessionEvent::ModelCallCompleted {
            round_index: 0,
            tool_call_count: 0,
            api_request_id: None,
            request_cache_control_type: None,
            request_cache_control_ttl: None,
            request_has_cache_breakpoint: false,
            request_cache_breakpoint_count: 0,
            prompt_tokens: 10,
            completion_tokens: 5,
            total_tokens: 15,
            cache_hit_tokens: cache_read_tokens,
            cache_miss_tokens: 10,
            cache_read_tokens,
            cache_write_tokens: 0,
            assistant_message: Some(ChatMessage::text("assistant", "ok")),
        }
    }

    fn commit_foreground_turn(
        sessions: &SessionManager,
        address: &ChannelAddress,
        messages: Vec<ChatMessage>,
        consumed_pending_messages: &[ChatMessage],
        phase: SessionPhase,
    ) -> anyhow::Result<()> {
        commit_foreground_turn_with_loaded_skills(
            sessions,
            address,
            messages,
            consumed_pending_messages,
            phase,
            Vec::new(),
        )
    }

    fn commit_foreground_turn_with_loaded_skills(
        sessions: &SessionManager,
        address: &ChannelAddress,
        messages: Vec<ChatMessage>,
        consumed_pending_messages: &[ChatMessage],
        phase: SessionPhase,
        loaded_skills: Vec<String>,
    ) -> anyhow::Result<()> {
        let actor = sessions.resolve_foreground_by_address(address)?;
        actor.commit_runtime_turn(SessionRuntimeTurnCommit {
            messages,
            consumed_pending_messages: consumed_pending_messages.to_vec(),
            usage: TokenUsage::default(),
            compaction: SessionCompactionStats::default(),
            phase,
            system_prompt_static_hash_after_compaction: None,
            loaded_skills,
            user_history_text: None,
            assistant_history_text: None,
        })
    }

    fn commit_snapshot_turn(
        sessions: &SessionManager,
        snapshot: &super::SessionSnapshot,
        messages: Vec<ChatMessage>,
        phase: SessionPhase,
    ) -> anyhow::Result<()> {
        let actor = sessions.resolve_snapshot(snapshot)?;
        actor.commit_runtime_turn(SessionRuntimeTurnCommit {
            messages,
            consumed_pending_messages: Vec::new(),
            usage: TokenUsage::default(),
            compaction: SessionCompactionStats::default(),
            phase,
            system_prompt_static_hash_after_compaction: None,
            loaded_skills: Vec::new(),
            user_history_text: None,
            assistant_history_text: None,
        })
    }

    fn fail_foreground_turn(
        sessions: &SessionManager,
        address: &ChannelAddress,
        resume_messages: Vec<ChatMessage>,
        errno: SessionErrno,
        errinfo: Option<String>,
    ) -> anyhow::Result<()> {
        let actor = sessions.resolve_foreground_by_address(address)?;
        actor.fail_runtime_turn(SessionRuntimeTurnFailure {
            resume_messages,
            errno,
            errinfo,
            compaction: SessionCompactionStats::default(),
            system_prompt_static_hash_after_compaction: None,
        })
    }

    #[test]
    fn system_prompt_state_tracks_static_prompt_rebuilds() {
        let temp_dir = TempDir::new().unwrap();
        let workspace_manager = WorkspaceManager::load_or_create(temp_dir.path()).unwrap();
        let mut sessions = SessionManager::new(temp_dir.path(), workspace_manager).unwrap();
        let address = test_address();
        sessions.ensure_foreground_actor(&address).unwrap();
        let actor = foreground_actor(&sessions, &address);

        let observed = actor
            .observe_system_prompt_state("static-a".to_string())
            .unwrap();
        assert!(!observed.static_changed);

        let observed = actor
            .observe_system_prompt_state("static-b".to_string())
            .unwrap();
        assert!(observed.static_changed);
    }

    #[test]
    fn emits_content_change_notice_for_loaded_skill_after_baseline() {
        let temp_dir = TempDir::new().unwrap();
        let workspace_manager = WorkspaceManager::load_or_create(temp_dir.path()).unwrap();
        let mut sessions = SessionManager::new(temp_dir.path(), workspace_manager).unwrap();
        let address = test_address();
        sessions.ensure_foreground_actor(&address).unwrap();
        let actor = foreground_actor(&sessions, &address);

        actor
            .observe_skill_changes(
                &[SessionSkillObservation {
                    name: "skill-a".to_string(),
                    description: "old desc".to_string(),
                    content: "old content".to_string(),
                }],
                "skills metadata".to_string(),
            )
            .unwrap();

        commit_foreground_turn_with_loaded_skills(
            &sessions,
            &address,
            Vec::new(),
            &[],
            SessionPhase::End,
            vec!["skill-a".to_string()],
        )
        .unwrap();

        let notices = actor
            .observe_skill_changes(
                &[SessionSkillObservation {
                    name: "skill-a".to_string(),
                    description: "new desc".to_string(),
                    content: "new content".to_string(),
                }],
                "skills metadata".to_string(),
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
        sessions.ensure_foreground_actor(&address).unwrap();
        let actor = foreground_actor(&sessions, &address);

        actor
            .observe_skill_changes(
                &[SessionSkillObservation {
                    name: "skill-b".to_string(),
                    description: "old desc".to_string(),
                    content: "same content".to_string(),
                }],
                "skills metadata".to_string(),
            )
            .unwrap();

        let notices = actor
            .observe_skill_changes(
                &[SessionSkillObservation {
                    name: "skill-b".to_string(),
                    description: "new desc".to_string(),
                    content: "same content".to_string(),
                }],
                "skills metadata".to_string(),
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
        sessions.ensure_foreground_actor(&address).unwrap();
        let actor = foreground_actor(&sessions, &address);

        actor
            .observe_skill_changes(
                &[SessionSkillObservation {
                    name: "skill-c".to_string(),
                    description: "old desc".to_string(),
                    content: "old content".to_string(),
                }],
                "skills metadata".to_string(),
            )
            .unwrap();

        commit_foreground_turn_with_loaded_skills(
            &sessions,
            &address,
            Vec::new(),
            &[],
            SessionPhase::End,
            vec!["skill-c".to_string()],
        )
        .unwrap();

        let notices = actor
            .observe_skill_changes(
                &[SessionSkillObservation {
                    name: "skill-c".to_string(),
                    description: "new desc".to_string(),
                    content: "new content".to_string(),
                }],
                "skills metadata".to_string(),
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
    fn skill_metadata_prompt_tracks_notified_and_compaction_snapshots() {
        let temp_dir = TempDir::new().unwrap();
        let workspace_manager = WorkspaceManager::load_or_create(temp_dir.path()).unwrap();
        let mut sessions = SessionManager::new(temp_dir.path(), workspace_manager).unwrap();
        let address = test_address();
        sessions.ensure_foreground_actor(&address).unwrap();
        let actor = foreground_actor(&sessions, &address);
        let observed = [SessionSkillObservation {
            name: "skill-d".to_string(),
            description: "desc".to_string(),
            content: "content".to_string(),
        }];

        let first = actor
            .observe_skill_changes(&observed, "metadata v1".to_string())
            .unwrap();
        assert!(first.is_empty());

        let second = actor
            .observe_skill_changes(&observed, "metadata v2".to_string())
            .unwrap();
        assert!(matches!(
            second.as_slice(),
            [SkillChangeNotice::MetadataChanged { metadata_prompt }]
                if metadata_prompt == "metadata v2"
        ));

        let snapshot = actor.snapshot().unwrap();
        let state = snapshot
            .session_state
            .prompt_components
            .get(SKILLS_METADATA_PROMPT_COMPONENT)
            .unwrap();
        assert_eq!(state.system_prompt_value, "metadata v1");
        assert_eq!(state.notified_value, "metadata v2");

        actor
            .commit_runtime_turn(SessionRuntimeTurnCommit {
                messages: vec![ChatMessage::text("assistant", "compacted")],
                consumed_pending_messages: Vec::new(),
                usage: TokenUsage::default(),
                compaction: SessionCompactionStats {
                    compacted_run_count: 1,
                    ..SessionCompactionStats::default()
                },
                phase: SessionPhase::End,
                system_prompt_static_hash_after_compaction: Some("static".to_string()),
                loaded_skills: Vec::new(),
                user_history_text: None,
                assistant_history_text: None,
            })
            .unwrap();

        let snapshot = actor.snapshot().unwrap();
        let state = snapshot
            .session_state
            .prompt_components
            .get(SKILLS_METADATA_PROMPT_COMPONENT)
            .unwrap();
        assert_eq!(state.system_prompt_value, "metadata v2");
        assert_eq!(state.notified_value, "metadata v2");
    }

    #[test]
    fn prompt_component_change_tracks_notified_until_compaction() {
        let temp_dir = TempDir::new().unwrap();
        let workspace_manager = WorkspaceManager::load_or_create(temp_dir.path()).unwrap();
        let mut sessions = SessionManager::new(temp_dir.path(), workspace_manager).unwrap();
        let address = test_address();
        sessions.ensure_foreground_actor(&address).unwrap();
        let actor = foreground_actor(&sessions, &address);

        let first = actor
            .observe_prompt_component_change(IDENTITY_PROMPT_COMPONENT, "identity v1".to_string())
            .unwrap();
        assert!(first.is_none());

        let second = actor
            .observe_prompt_component_change(IDENTITY_PROMPT_COMPONENT, "identity v2".to_string())
            .unwrap()
            .expect("changed identity should emit a notice");
        assert_eq!(second.key, IDENTITY_PROMPT_COMPONENT);
        assert_eq!(second.value, "identity v2");

        let snapshot = actor.snapshot().unwrap();
        let state = snapshot
            .session_state
            .prompt_components
            .get(IDENTITY_PROMPT_COMPONENT)
            .unwrap();
        assert_eq!(state.system_prompt_value, "identity v1");
        assert_eq!(state.notified_value, "identity v2");

        actor
            .commit_runtime_turn(SessionRuntimeTurnCommit {
                messages: vec![ChatMessage::text("assistant", "compacted")],
                consumed_pending_messages: Vec::new(),
                usage: TokenUsage::default(),
                compaction: SessionCompactionStats {
                    compacted_run_count: 1,
                    ..SessionCompactionStats::default()
                },
                phase: SessionPhase::End,
                system_prompt_static_hash_after_compaction: Some("static".to_string()),
                loaded_skills: Vec::new(),
                user_history_text: None,
                assistant_history_text: None,
            })
            .unwrap();

        let snapshot = actor.snapshot().unwrap();
        let state = snapshot
            .session_state
            .prompt_components
            .get(IDENTITY_PROMPT_COMPONENT)
            .unwrap();
        assert_eq!(state.system_prompt_value, "identity v2");
        assert_eq!(state.notified_value, "identity v2");
    }

    #[test]
    fn idle_compaction_failure_can_be_set_and_cleared() {
        let temp_dir = TempDir::new().unwrap();
        let workspace_manager = WorkspaceManager::load_or_create(temp_dir.path()).unwrap();
        let mut sessions = SessionManager::new(temp_dir.path(), workspace_manager).unwrap();
        let address = test_address();
        sessions.ensure_foreground_actor(&address).unwrap();
        let actor = foreground_actor(&sessions, &address);

        actor
            .mark_idle_compaction_failed("idle compaction failed".to_string())
            .unwrap();
        let snapshot = sessions.get_snapshot(&address).unwrap();
        assert_eq!(
            snapshot.session_state.errno,
            Some(SessionErrno::IdleCompactionFailure)
        );
        assert_eq!(
            snapshot.session_state.errinfo.as_deref(),
            Some("idle compaction failed")
        );

        actor.clear_idle_compaction_failure().unwrap();
        let snapshot = sessions.get_snapshot(&address).unwrap();
        assert_eq!(snapshot.session_state.errno, None);
        assert_eq!(snapshot.session_state.errinfo, None);
    }

    #[test]
    fn staged_foreground_user_turn_updates_pending_queue_and_visible_history() {
        let temp_dir = TempDir::new().unwrap();
        let workspace_manager = WorkspaceManager::load_or_create(temp_dir.path()).unwrap();
        let mut sessions = SessionManager::new(temp_dir.path(), workspace_manager).unwrap();
        let address = test_address();
        sessions.ensure_foreground_actor(&address).unwrap();
        let actor = foreground_actor(&sessions, &address);

        actor
            .tell_user_message(super::SessionUserMessage {
                pending_message: ChatMessage::text("user", "queued"),
                text: Some("queued".to_string()),
                attachments: Vec::new(),
            })
            .unwrap();

        let snapshot = sessions.get_snapshot(&address).unwrap();
        assert_eq!(
            snapshot.session_state.pending_messages,
            vec![ChatMessage::text("user", "queued")]
        );
        let checkpoint = sessions.export_checkpoint(&address).unwrap();
        assert_eq!(checkpoint.history.len(), 1);
        assert_eq!(checkpoint.history[0].role, MessageRole::User);
        assert_eq!(checkpoint.history[0].text.as_deref(), Some("queued"));
    }

    #[test]
    fn failed_foreground_turn_projects_errno_into_session_state() {
        let temp_dir = TempDir::new().unwrap();
        let workspace_manager = WorkspaceManager::load_or_create(temp_dir.path()).unwrap();
        let mut sessions = SessionManager::new(temp_dir.path(), workspace_manager).unwrap();
        let address = test_address();
        sessions.ensure_foreground_actor(&address).unwrap();

        fail_foreground_turn(
            &sessions,
            &address,
            vec![ChatMessage::text("assistant", "preserved")],
            SessionErrno::ApiFailure,
            Some("upstream timed out".to_string()),
        )
        .unwrap();

        let snapshot = sessions.get_snapshot(&address).unwrap();
        assert_eq!(snapshot.session_state.phase, SessionPhase::Yielded);
        assert_eq!(snapshot.session_state.errno, Some(SessionErrno::ApiFailure));
        assert_eq!(
            snapshot.session_state.errinfo.as_deref(),
            Some("upstream timed out")
        );
        assert_eq!(
            snapshot.session_state.messages,
            vec![ChatMessage::text("assistant", "preserved")]
        );
    }

    #[test]
    fn pending_request_messages_are_included_in_request_view_and_cleared_after_turn() {
        let temp_dir = TempDir::new().unwrap();
        let workspace_manager = WorkspaceManager::load_or_create(temp_dir.path()).unwrap();
        let mut sessions = SessionManager::new(temp_dir.path(), workspace_manager).unwrap();
        let address = test_address();
        sessions.ensure_foreground_actor(&address).unwrap();
        commit_foreground_turn(
            &sessions,
            &address,
            vec![ChatMessage::text("assistant", "tail")],
            &[],
            SessionPhase::End,
        )
        .unwrap();
        foreground_actor(&sessions, &address)
            .tell_user_message(user_message("queued"))
            .unwrap();

        let snapshot = sessions.get_snapshot(&address).unwrap();
        assert_eq!(
            snapshot.request_messages(),
            vec![
                ChatMessage::text("assistant", "tail"),
                ChatMessage::text("user", "queued")
            ]
        );

        commit_foreground_turn(
            &sessions,
            &address,
            vec![ChatMessage::text("assistant", "done")],
            &[ChatMessage::text("user", "queued")],
            SessionPhase::End,
        )
        .unwrap();

        let snapshot = sessions.get_snapshot(&address).unwrap();
        assert!(snapshot.session_state.pending_messages.is_empty());
        assert_eq!(
            snapshot.session_state.messages,
            vec![ChatMessage::text("assistant", "done")]
        );
    }

    #[test]
    fn staged_pending_messages_survive_older_failure_checkpoint() {
        let temp_dir = TempDir::new().unwrap();
        let workspace_manager = WorkspaceManager::load_or_create(temp_dir.path()).unwrap();
        let mut sessions = SessionManager::new(temp_dir.path(), workspace_manager).unwrap();
        let address = test_address();
        sessions.ensure_foreground_actor(&address).unwrap();
        commit_foreground_turn(
            &sessions,
            &address,
            vec![ChatMessage::text("assistant", "stable-tail")],
            &[],
            SessionPhase::End,
        )
        .unwrap();
        foreground_actor(&sessions, &address)
            .tell_user_message(user_message("queued"))
            .unwrap();

        fail_foreground_turn(
            &sessions,
            &address,
            vec![ChatMessage::text("assistant", "stable-tail")],
            SessionErrno::ApiFailure,
            Some("upstream timed out".to_string()),
        )
        .unwrap();

        let snapshot = sessions.get_snapshot(&address).unwrap();
        assert_eq!(
            snapshot.session_state.messages,
            vec![ChatMessage::text("assistant", "stable-tail")]
        );
        assert_eq!(
            snapshot.session_state.pending_messages,
            vec![ChatMessage::text("user", "queued")]
        );
        assert_eq!(
            snapshot.request_messages(),
            vec![
                ChatMessage::text("assistant", "stable-tail"),
                ChatMessage::text("user", "queued")
            ]
        );
    }

    #[test]
    fn record_turn_preserves_new_pending_tail_after_consumed_prefix() {
        let temp_dir = TempDir::new().unwrap();
        let workspace_manager = WorkspaceManager::load_or_create(temp_dir.path()).unwrap();
        let mut sessions = SessionManager::new(temp_dir.path(), workspace_manager).unwrap();
        let address = test_address();
        sessions.ensure_foreground_actor(&address).unwrap();
        commit_foreground_turn(
            &sessions,
            &address,
            vec![ChatMessage::text("assistant", "stable")],
            &[],
            SessionPhase::End,
        )
        .unwrap();
        foreground_actor(&sessions, &address)
            .tell_user_message(user_message("consumed"))
            .unwrap();
        foreground_actor(&sessions, &address)
            .tell_user_message(user_message("new-tail"))
            .unwrap();

        commit_foreground_turn(
            &sessions,
            &address,
            vec![
                ChatMessage::text("assistant", "stable"),
                ChatMessage::text("user", "consumed"),
                ChatMessage::text("assistant", "tool batch settled"),
            ],
            &[ChatMessage::text("user", "consumed")],
            SessionPhase::Yielded,
        )
        .unwrap();

        let snapshot = sessions.get_snapshot(&address).unwrap();
        assert_eq!(
            snapshot.session_state.pending_messages,
            vec![ChatMessage::text("user", "new-tail")]
        );
        assert_eq!(
            snapshot.request_messages(),
            vec![
                ChatMessage::text("assistant", "stable"),
                ChatMessage::text("user", "consumed"),
                ChatMessage::text("assistant", "tool batch settled"),
                ChatMessage::text("user", "new-tail"),
            ]
        );
    }

    #[test]
    fn exports_and_restores_session_checkpoint() {
        let temp_dir = TempDir::new().unwrap();
        let workspace_manager = WorkspaceManager::load_or_create(temp_dir.path()).unwrap();
        let mut sessions = SessionManager::new(temp_dir.path(), workspace_manager.clone()).unwrap();
        let address = test_address();
        sessions.ensure_foreground_actor(&address).unwrap();

        foreground_actor(&sessions, &address)
            .tell_user_message(user_message("hello"))
            .unwrap();
        commit_foreground_turn(
            &sessions,
            &address,
            vec![ChatMessage::text("assistant", "hi")],
            &[ChatMessage::text("user", "hello")],
            SessionPhase::End,
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
        assert_eq!(restored.stable_message_count(), 1);
    }

    #[test]
    fn background_actors_can_share_workspace_without_sharing_memory() {
        let temp_dir = TempDir::new().unwrap();
        let workspace_manager = WorkspaceManager::load_or_create(temp_dir.path()).unwrap();
        let mut sessions = SessionManager::new(temp_dir.path(), workspace_manager).unwrap();
        let address = test_address();
        let foreground = ensure_foreground_snapshot(&mut sessions, &address);
        let background_actor = sessions
            .create_background_in_workspace_actor(
                &address,
                Uuid::new_v4(),
                &foreground.workspace_id,
            )
            .unwrap();
        let background = background_actor.snapshot().unwrap();

        assert_eq!(foreground.kind, SessionKind::Foreground);
        assert_eq!(background.kind, SessionKind::Background);
        assert_ne!(foreground.id, background.id);
        assert_eq!(foreground.workspace_id, background.workspace_id);

        commit_snapshot_turn(
            &sessions,
            &background,
            vec![ChatMessage::text("assistant", "background memory")],
            SessionPhase::End,
        )
        .unwrap();

        let foreground_after = sessions.get_snapshot(&address).unwrap();
        let background_after = sessions.background_snapshot(background.id).unwrap();
        assert!(foreground_after.stable_messages().is_empty());
        assert_eq!(
            background_after.stable_messages()[0]
                .content
                .as_ref()
                .and_then(serde_json::Value::as_str),
            Some("background memory")
        );
    }

    #[test]
    fn foreground_and_background_session_roots_are_nested_by_conversation_and_kind() {
        let temp_dir = TempDir::new().unwrap();
        let workspace_manager = WorkspaceManager::load_or_create(temp_dir.path()).unwrap();
        let mut sessions = SessionManager::new(temp_dir.path(), workspace_manager).unwrap();
        let address = test_address();
        let foreground = ensure_foreground_snapshot(&mut sessions, &address);
        let background = sessions
            .create_background_actor(&address, Uuid::new_v4())
            .unwrap()
            .snapshot()
            .unwrap();
        let conversation_dir = session_conversation_dir_name(&address.conversation_id);

        assert_eq!(
            foreground.root_dir,
            temp_dir
                .path()
                .join("sessions")
                .join(&conversation_dir)
                .join("foreground")
                .join(foreground.id.to_string())
        );
        assert_eq!(
            background.root_dir,
            temp_dir
                .path()
                .join("sessions")
                .join(conversation_dir)
                .join("background")
                .join(background.id.to_string())
        );
    }

    #[test]
    fn session_conversation_dir_name_encodes_disallowed_path_segments() {
        assert_eq!(session_conversation_dir_name("."), "%2E");
        assert_eq!(session_conversation_dir_name(".."), "%2E%2E");
        assert_eq!(session_conversation_dir_name("room@1"), "room%401");
    }

    #[test]
    fn idle_actor_tell_persists_and_drains_mailbox() {
        let temp_dir = TempDir::new().unwrap();
        let workspace_manager = WorkspaceManager::load_or_create(temp_dir.path()).unwrap();
        let mut sessions = SessionManager::new(temp_dir.path(), workspace_manager).unwrap();
        let address = test_address();
        let actor = sessions.ensure_foreground_actor(&address).unwrap();
        let sender_id = Uuid::new_v4();

        let receipt = actor
            .tell_actor_message(SessionActorMessage {
                from_session_id: sender_id,
                role: MessageRole::Assistant,
                text: Some("background finished".to_string()),
                attachments: Vec::new(),
            })
            .unwrap();
        let snapshot = actor.snapshot().unwrap();

        assert_ne!(receipt.message_id, Uuid::nil());
        assert!(receipt.applied_to_context);
        assert!(snapshot.session_state.actor_mailbox.is_empty());
        assert_eq!(
            snapshot.stable_messages(),
            &[ChatMessage::text("assistant", "background finished")]
        );
        let checkpoint = actor.export_checkpoint().unwrap();
        assert_eq!(checkpoint.history.len(), 1);
        assert_eq!(checkpoint.history[0].role, MessageRole::Assistant);

        let persisted: serde_json::Value = serde_json::from_str(
            &fs::read_to_string(snapshot.root_dir.join("session.json")).unwrap(),
        )
        .unwrap();
        assert!(
            persisted["session_state"]["actor_mailbox"]
                .as_array()
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn running_actor_tell_persists_mailbox_and_unregister_drains() {
        let temp_dir = TempDir::new().unwrap();
        let workspace_manager = WorkspaceManager::load_or_create(temp_dir.path()).unwrap();
        let mut sessions = SessionManager::new(temp_dir.path(), workspace_manager).unwrap();
        let address = test_address();
        let actor = sessions.ensure_foreground_actor(&address).unwrap();
        actor
            .register_control(SessionExecutionControl::new())
            .unwrap();
        let sender_id = Uuid::new_v4();

        let receipt = actor
            .tell_actor_message(SessionActorMessage {
                from_session_id: sender_id,
                role: MessageRole::Assistant,
                text: Some("queued actor message".to_string()),
                attachments: Vec::new(),
            })
            .unwrap();
        let queued = actor.snapshot().unwrap();

        assert_ne!(receipt.message_id, Uuid::nil());
        assert!(!receipt.applied_to_context);
        assert_eq!(queued.session_state.actor_mailbox.len(), 1);
        assert!(queued.stable_messages().is_empty());
        let persisted_queued: serde_json::Value = serde_json::from_str(
            &fs::read_to_string(queued.root_dir.join("session.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(
            persisted_queued["session_state"]["actor_mailbox"]
                .as_array()
                .unwrap()
                .len(),
            1
        );

        let drained = actor.unregister_control().unwrap();
        assert!(drained);
        let snapshot = actor.snapshot().unwrap();
        assert!(snapshot.session_state.actor_mailbox.is_empty());
        assert_eq!(
            snapshot.stable_messages(),
            &[ChatMessage::text("assistant", "queued actor message")]
        );
    }

    #[test]
    fn background_actor_also_queues_actor_messages_while_running() {
        let temp_dir = TempDir::new().unwrap();
        let workspace_manager = WorkspaceManager::load_or_create(temp_dir.path()).unwrap();
        let mut sessions = SessionManager::new(temp_dir.path(), workspace_manager).unwrap();
        let address = test_address();
        let actor = sessions
            .create_background_actor(&address, Uuid::new_v4())
            .unwrap();
        actor
            .register_control(SessionExecutionControl::new())
            .unwrap();

        let receipt = actor
            .tell_actor_message(SessionActorMessage {
                from_session_id: Uuid::new_v4(),
                role: MessageRole::Assistant,
                text: Some("background inbox".to_string()),
                attachments: Vec::new(),
            })
            .unwrap();
        let queued = actor.snapshot().unwrap();

        assert_ne!(receipt.message_id, Uuid::nil());
        assert!(!receipt.applied_to_context);
        assert_eq!(queued.kind, SessionKind::Background);
        assert_eq!(queued.session_state.actor_mailbox.len(), 1);
        assert!(queued.stable_messages().is_empty());

        assert!(actor.unregister_control().unwrap());
        let snapshot = actor.snapshot().unwrap();
        assert!(snapshot.session_state.actor_mailbox.is_empty());
        assert_eq!(
            snapshot.stable_messages(),
            &[ChatMessage::text("assistant", "background inbox")]
        );
    }

    #[test]
    fn persisted_actor_mailbox_drains_after_restart() {
        let temp_dir = TempDir::new().unwrap();
        let address = test_address();
        let workspace_id = {
            let workspace_manager = WorkspaceManager::load_or_create(temp_dir.path()).unwrap();
            let mut sessions = SessionManager::new(temp_dir.path(), workspace_manager).unwrap();
            let actor = sessions.ensure_foreground_actor(&address).unwrap();
            actor
                .register_control(SessionExecutionControl::new())
                .unwrap();
            actor
                .tell_actor_message(SessionActorMessage {
                    from_session_id: Uuid::new_v4(),
                    role: MessageRole::Assistant,
                    text: Some("survived restart".to_string()),
                    attachments: Vec::new(),
                })
                .unwrap();
            actor.snapshot().unwrap().workspace_id
        };

        let workspace_manager = WorkspaceManager::load_or_create(temp_dir.path()).unwrap();
        let sessions = SessionManager::new(temp_dir.path(), workspace_manager).unwrap();
        let snapshot = sessions.get_snapshot(&address).unwrap();

        assert_eq!(snapshot.workspace_id, workspace_id);
        assert!(snapshot.session_state.actor_mailbox.is_empty());
        assert_eq!(
            snapshot.stable_messages(),
            &[ChatMessage::text("assistant", "survived restart")]
        );
    }

    #[test]
    fn foreground_runtime_control_lives_inside_session_actor() {
        let temp_dir = TempDir::new().unwrap();
        let workspace_manager = WorkspaceManager::load_or_create(temp_dir.path()).unwrap();
        let mut sessions = SessionManager::new(temp_dir.path(), workspace_manager).unwrap();
        let address = test_address();
        let actor = sessions.ensure_foreground_actor(&address).unwrap();

        assert!(
            !actor
                .tell_user_message(user_message("hello"))
                .unwrap()
                .interrupted
        );

        let control = SessionExecutionControl::new();
        actor.register_control(control.clone()).unwrap();
        actor
            .receive_runtime_event(&SessionEvent::CompactionStarted {
                phase: "initial".to_string(),
                message_count: 3,
            })
            .unwrap();

        let disposition = actor.tell_user_message(user_message("进度如何？")).unwrap();
        assert!(disposition.interrupted);
        assert!(disposition.compaction_in_progress);
        assert_eq!(
            disposition.text.as_deref(),
            Some("[Interrupted Follow-up]\n进度如何？")
        );
        assert_eq!(
            disposition.outbound,
            vec![SessionActorOutbound::UserVisibleText(
                "正在压缩上下文，可能要等待压缩完毕后才能回复。".to_string()
            )]
        );
        assert!(actor.has_pending_interrupt().unwrap());
        assert!(control.take_yield_requested());

        let next_control = SessionExecutionControl::new();
        actor.register_control(next_control.clone()).unwrap();
        assert!(next_control.take_yield_requested());

        actor.clear_pending_interrupt().unwrap();
        assert!(!actor.has_pending_interrupt().unwrap());
        assert!(!next_control.take_yield_requested());
        assert!(actor.request_cancel().unwrap());

        actor.unregister_control().unwrap();
        assert!(!actor.request_cancel().unwrap());
    }

    #[test]
    fn session_actor_claim_serializes_turn_runners_without_external_state() {
        let temp_dir = TempDir::new().unwrap();
        let workspace_manager = WorkspaceManager::load_or_create(temp_dir.path()).unwrap();
        let mut sessions = SessionManager::new(temp_dir.path(), workspace_manager).unwrap();
        let address = test_address();
        let actor = sessions.ensure_foreground_actor(&address).unwrap();

        assert!(actor.try_claim_turn_runner().unwrap());
        assert!(!actor.try_claim_turn_runner().unwrap());

        let disposition = actor.tell_user_message(user_message("next")).unwrap();
        assert!(disposition.interrupted);
        assert!(actor.has_pending_interrupt().unwrap());

        actor.unregister_control().unwrap();
        assert!(actor.try_claim_turn_runner().unwrap());
    }

    #[test]
    fn staging_user_turn_owns_interrupt_tagging_and_pending_persistence() {
        let temp_dir = TempDir::new().unwrap();
        let workspace_manager = WorkspaceManager::load_or_create(temp_dir.path()).unwrap();
        let mut sessions = SessionManager::new(temp_dir.path(), workspace_manager).unwrap();
        let address = test_address();
        let actor = sessions.ensure_foreground_actor(&address).unwrap();
        let control = SessionExecutionControl::new();
        actor.register_control(control.clone()).unwrap();
        actor
            .receive_runtime_event(&SessionEvent::CompactionStarted {
                phase: "initial".to_string(),
                message_count: 3,
            })
            .unwrap();

        let receipt = actor.tell_user_message(user_message("status?")).unwrap();

        assert!(receipt.interrupted);
        assert!(receipt.compaction_in_progress);
        assert_eq!(
            receipt.text.as_deref(),
            Some("[Interrupted Follow-up]\nstatus?")
        );
        assert_eq!(
            receipt.outbound,
            vec![SessionActorOutbound::UserVisibleText(
                "正在压缩上下文，可能要等待压缩完毕后才能回复。".to_string()
            )]
        );
        assert!(control.take_yield_requested());
        let snapshot = actor.snapshot().unwrap();
        assert!(snapshot.session_state.pending_messages.is_empty());
        assert_eq!(snapshot.session_state.user_mailbox.len(), 1);
        assert_eq!(
            snapshot.session_state.user_mailbox[0]
                .pending_message
                .content
                .as_ref()
                .and_then(serde_json::Value::as_str),
            Some("[Interrupted Follow-up]\nstatus?")
        );

        actor.unregister_control().unwrap();
        let snapshot = actor.snapshot().unwrap();
        assert!(snapshot.session_state.user_mailbox.is_empty());
        assert_eq!(snapshot.session_state.pending_messages.len(), 1);
        assert_eq!(
            snapshot.session_state.pending_messages[0]
                .content
                .as_ref()
                .and_then(serde_json::Value::as_str),
            Some("[Interrupted Follow-up]\nstatus?")
        );
    }

    #[test]
    fn cache_warning_triggers_after_three_consecutive_zero_reads() {
        let temp_dir = TempDir::new().unwrap();
        let workspace_manager = WorkspaceManager::load_or_create(temp_dir.path()).unwrap();
        let mut sessions = SessionManager::new(temp_dir.path(), workspace_manager).unwrap();
        let address = test_address();
        let actor = sessions.ensure_foreground_actor(&address).unwrap();

        assert!(
            actor
                .receive_runtime_event_with_effects("opus-4.6", &model_call_completed_event(0))
                .unwrap()
                .is_empty()
        );
        assert!(
            actor
                .receive_runtime_event_with_effects("opus-4.6", &model_call_completed_event(0))
                .unwrap()
                .is_empty()
        );

        let effects = actor
            .receive_runtime_event_with_effects("opus-4.6", &model_call_completed_event(0))
            .unwrap();
        assert!(matches!(
            effects.as_slice(),
            [SessionEffect::UserVisibleText(text)] if text.contains("cache read 都是 0") && text.contains("不超过 5 分钟")
        ));
    }

    #[test]
    fn consecutive_zero_read_warning_requires_recent_gaps() {
        let mut cache_health = SessionCacheHealthState::default();
        let start = Utc::now();

        assert_eq!(cache_health.record_model_call(start, 0), None);
        assert_eq!(
            cache_health.record_model_call(start + chrono::Duration::minutes(6), 0),
            None
        );
        assert_eq!(
            cache_health.record_model_call(start + chrono::Duration::minutes(12), 0),
            None
        );
    }

    #[test]
    fn cache_warning_deduplicates_until_cache_read_recovers() {
        let temp_dir = TempDir::new().unwrap();
        let workspace_manager = WorkspaceManager::load_or_create(temp_dir.path()).unwrap();
        let mut sessions = SessionManager::new(temp_dir.path(), workspace_manager).unwrap();
        let address = test_address();
        let actor = sessions.ensure_foreground_actor(&address).unwrap();

        for _ in 0..3 {
            let _ = actor
                .receive_runtime_event_with_effects("opus-4.6", &model_call_completed_event(0))
                .unwrap();
        }
        assert!(
            actor
                .receive_runtime_event_with_effects("opus-4.6", &model_call_completed_event(0))
                .unwrap()
                .is_empty()
        );
        assert!(
            actor
                .receive_runtime_event_with_effects("opus-4.6", &model_call_completed_event(64))
                .unwrap()
                .is_empty()
        );
        let effects = actor
            .receive_runtime_event_with_effects("opus-4.6", &model_call_completed_event(0))
            .unwrap();
        assert!(matches!(
            effects.as_slice(),
            [SessionEffect::UserVisibleText(text)] if text.contains("最近 10 次模型调用里已有 2 次")
        ));
    }

    #[test]
    fn cache_warning_triggers_for_two_zero_reads_within_ten_calls() {
        let temp_dir = TempDir::new().unwrap();
        let workspace_manager = WorkspaceManager::load_or_create(temp_dir.path()).unwrap();
        let mut sessions = SessionManager::new(temp_dir.path(), workspace_manager).unwrap();
        let address = test_address();
        let actor = sessions.ensure_foreground_actor(&address).unwrap();

        assert!(
            actor
                .receive_runtime_event_with_effects("opus-4.6", &model_call_completed_event(0))
                .unwrap()
                .is_empty()
        );
        assert!(
            actor
                .receive_runtime_event_with_effects("opus-4.6", &model_call_completed_event(12))
                .unwrap()
                .is_empty()
        );

        let effects = actor
            .receive_runtime_event_with_effects("opus-4.6", &model_call_completed_event(0))
            .unwrap();
        assert!(matches!(
            effects.as_slice(),
            [SessionEffect::UserVisibleText(text)]
                if text.contains("最近 10 次模型调用里已有 2 次")
        ));
    }

    #[test]
    fn cache_warning_recent_zero_reads_ignore_calls_across_compaction() {
        let temp_dir = TempDir::new().unwrap();
        let workspace_manager = WorkspaceManager::load_or_create(temp_dir.path()).unwrap();
        let mut sessions = SessionManager::new(temp_dir.path(), workspace_manager).unwrap();
        let address = test_address();
        let actor = sessions.ensure_foreground_actor(&address).unwrap();

        assert!(
            actor
                .receive_runtime_event_with_effects("opus-4.6", &model_call_completed_event(0))
                .unwrap()
                .is_empty()
        );
        actor
            .receive_runtime_event(&SessionEvent::CompactionStarted {
                phase: "threshold".to_string(),
                message_count: 8,
            })
            .unwrap();
        assert!(
            actor
                .receive_runtime_event_with_effects("opus-4.6", &model_call_completed_event(0))
                .unwrap()
                .is_empty()
        );
        assert!(
            actor
                .receive_runtime_event_with_effects("opus-4.6", &model_call_completed_event(7))
                .unwrap()
                .is_empty()
        );

        let effects = actor
            .receive_runtime_event_with_effects("opus-4.6", &model_call_completed_event(0))
            .unwrap();
        assert!(matches!(
            effects.as_slice(),
            [SessionEffect::UserVisibleText(text)]
                if text.contains("最近 10 次模型调用里已有 2 次")
                    && text.contains("中间没有发生压缩")
        ));
    }

    #[test]
    fn persisted_user_mailbox_drains_after_restart() {
        let temp_dir = TempDir::new().unwrap();
        let address = test_address();
        {
            let workspace_manager = WorkspaceManager::load_or_create(temp_dir.path()).unwrap();
            let mut sessions = SessionManager::new(temp_dir.path(), workspace_manager).unwrap();
            let actor = sessions.ensure_foreground_actor(&address).unwrap();
            actor
                .register_control(SessionExecutionControl::new())
                .unwrap();
            actor
                .tell_user_message(user_message("survived restart"))
                .unwrap();

            let snapshot = actor.snapshot().unwrap();
            assert!(snapshot.session_state.pending_messages.is_empty());
            assert_eq!(snapshot.session_state.user_mailbox.len(), 1);
        }

        let workspace_manager = WorkspaceManager::load_or_create(temp_dir.path()).unwrap();
        let sessions = SessionManager::new(temp_dir.path(), workspace_manager).unwrap();
        let snapshot = sessions.get_snapshot(&address).unwrap();

        assert!(snapshot.session_state.user_mailbox.is_empty());
        assert_eq!(snapshot.session_state.pending_messages.len(), 1);
        assert_eq!(
            snapshot.session_state.pending_messages[0],
            ChatMessage::text("user", "[Interrupted Follow-up]\nsurvived restart")
        );
    }

    #[test]
    fn session_actor_ref_serializes_concurrent_user_messages() {
        let temp_dir = TempDir::new().unwrap();
        let workspace_manager = WorkspaceManager::load_or_create(temp_dir.path()).unwrap();
        let mut sessions = SessionManager::new(temp_dir.path(), workspace_manager).unwrap();
        let address = test_address();
        let actor = sessions.ensure_foreground_actor(&address).unwrap();

        let handles = (0..8)
            .map(|index| {
                let actor = actor.clone();
                std::thread::spawn(move || {
                    let text = format!("mailbox message {index}");
                    actor.tell_user_message(super::SessionUserMessage {
                        pending_message: ChatMessage::text("user", text.clone()),
                        text: Some(text),
                        attachments: Vec::new(),
                    })
                })
            })
            .collect::<Vec<_>>();

        for handle in handles {
            handle.join().unwrap().unwrap();
        }
        let snapshot = actor.snapshot().unwrap();
        assert!(snapshot.session_state.user_mailbox.is_empty());
        assert_eq!(snapshot.session_state.pending_messages.len(), 8);
        let checkpoint = sessions.export_checkpoint(&address).unwrap();
        assert_eq!(checkpoint.history.len(), 8);
    }

    #[test]
    fn destroying_foreground_session_shuts_down_actor_mailbox() {
        let temp_dir = TempDir::new().unwrap();
        let workspace_manager = WorkspaceManager::load_or_create(temp_dir.path()).unwrap();
        let mut sessions = SessionManager::new(temp_dir.path(), workspace_manager).unwrap();
        let address = test_address();
        let actor = sessions.ensure_foreground_actor(&address).unwrap();

        sessions.destroy_foreground(&address).unwrap();

        let error = actor.snapshot().unwrap_err().to_string();
        assert!(
            error.contains("shutting down") || error.contains("mailbox closed"),
            "unexpected error: {error}"
        );
    }

    #[test]
    fn runtime_turn_commit_updates_session_state_inside_actor() {
        let temp_dir = TempDir::new().unwrap();
        let workspace_manager = WorkspaceManager::load_or_create(temp_dir.path()).unwrap();
        let mut sessions = SessionManager::new(temp_dir.path(), workspace_manager).unwrap();
        let address = test_address();
        let actor = sessions.ensure_foreground_actor(&address).unwrap();
        let compaction = SessionCompactionStats {
            compacted_run_count: 1,
            ..SessionCompactionStats::default()
        };
        actor
            .commit_runtime_turn(SessionRuntimeTurnCommit {
                messages: vec![ChatMessage::text("assistant", "stable reply")],
                consumed_pending_messages: Vec::new(),
                usage: TokenUsage::default(),
                compaction,
                phase: SessionPhase::End,
                system_prompt_static_hash_after_compaction: Some("static-hash".to_string()),
                loaded_skills: vec!["skill-a".to_string()],
                user_history_text: Some("run background job".to_string()),
                assistant_history_text: Some("done".to_string()),
            })
            .unwrap();

        let snapshot = actor.snapshot().unwrap();
        assert_eq!(snapshot.session_state.phase, SessionPhase::End);
        assert_eq!(
            snapshot.stable_messages(),
            &[ChatMessage::text("assistant", "stable reply")]
        );
        assert_eq!(
            snapshot.session_state.system_prompt_static_hash.as_deref(),
            Some("static-hash")
        );
        assert_eq!(
            snapshot
                .skill_states
                .get("skill-a")
                .and_then(|state| state.last_loaded_turn),
            Some(1)
        );
        let checkpoint = actor.export_checkpoint().unwrap();
        assert_eq!(checkpoint.history.len(), 2);
        assert_eq!(checkpoint.history[0].role, MessageRole::User);
        assert_eq!(
            checkpoint.history[0].text.as_deref(),
            Some("run background job")
        );
        assert_eq!(checkpoint.history[1].role, MessageRole::Assistant);
        assert_eq!(checkpoint.history[1].text.as_deref(), Some("done"));
    }

    #[test]
    fn session_actor_renders_runtime_progress_effects() {
        let temp_dir = TempDir::new().unwrap();
        let workspace_manager = WorkspaceManager::load_or_create(temp_dir.path()).unwrap();
        let mut sessions = SessionManager::new(temp_dir.path(), workspace_manager).unwrap();
        let address = test_address();
        let actor = sessions.ensure_foreground_actor(&address).unwrap();

        let effects = actor
            .receive_runtime_progress(
                "gpt54",
                &ExecutionProgress {
                    round_index: 0,
                    phase: ExecutionProgressPhase::Tools,
                    tools: vec![
                        agent_frame::ToolExecutionProgress {
                            tool_call_id: "call-1".to_string(),
                            tool_name: "shell".to_string(),
                            arguments: Some(
                                r#"{"command":"cargo test --manifest-path agent_host/Cargo.toml"}"#
                                    .to_string(),
                            ),
                            status: agent_frame::ToolExecutionStatus::Running,
                        },
                        agent_frame::ToolExecutionProgress {
                            tool_call_id: "call-2".to_string(),
                            tool_name: "file_read".to_string(),
                            arguments: Some(r#"{"path":"src/main.rs"}"#.to_string()),
                            status: agent_frame::ToolExecutionStatus::Completed,
                        },
                    ],
                },
            )
            .unwrap();

        let feedback = match &effects[0] {
            SessionEffect::UpdateProgress(feedback) => feedback,
            SessionEffect::UserVisibleText(text) => {
                panic!("expected progress feedback, got user-visible text: {text}")
            }
        };
        assert!(feedback.text.contains("状态：工具执行中"));
        assert!(feedback.text.contains("shell：cargo test --mani..."));
        assert!(feedback.text.contains("file_read：src/main.rs"));
        assert!(!feedback.text.contains("已完成"));
    }

    #[test]
    fn session_actor_marks_completed_progress_as_final() {
        let temp_dir = TempDir::new().unwrap();
        let workspace_manager = WorkspaceManager::load_or_create(temp_dir.path()).unwrap();
        let mut sessions = SessionManager::new(temp_dir.path(), workspace_manager).unwrap();
        let address = test_address();
        let actor = sessions.ensure_foreground_actor(&address).unwrap();

        let effects = actor
            .receive_runtime_event_with_effects(
                "gpt54",
                &SessionEvent::SessionCompleted {
                    message_count: 3,
                    total_tokens: 42,
                },
            )
            .unwrap();

        let feedback = match &effects[0] {
            SessionEffect::UpdateProgress(feedback) => feedback,
            SessionEffect::UserVisibleText(text) => {
                panic!("expected progress feedback, got user-visible text: {text}")
            }
        };
        assert_eq!(feedback.final_state, Some(ProgressFeedbackFinalState::Done));
    }

    #[test]
    fn background_result_is_inserted_into_foreground_stable_context() {
        let temp_dir = TempDir::new().unwrap();
        let workspace_manager = WorkspaceManager::load_or_create(temp_dir.path()).unwrap();
        let mut sessions = SessionManager::new(temp_dir.path(), workspace_manager).unwrap();
        let address = test_address();
        sessions.ensure_foreground_actor(&address).unwrap();

        let actor = foreground_actor(&sessions, &address);
        actor
            .tell_actor_message(SessionActorMessage {
                from_session_id: Uuid::new_v4(),
                role: MessageRole::Assistant,
                text: Some("background finished".to_string()),
                attachments: Vec::new(),
            })
            .unwrap();
        let snapshot = actor.snapshot().unwrap();

        assert_eq!(
            snapshot.stable_messages(),
            &[ChatMessage::text("assistant", "background finished")]
        );
        let checkpoint = sessions.export_checkpoint(&address).unwrap();
        assert_eq!(checkpoint.history.len(), 1);
        assert_eq!(checkpoint.history[0].role, MessageRole::Assistant);
        assert_eq!(
            checkpoint.history[0].text.as_deref(),
            Some("background finished")
        );
    }

    #[test]
    fn session_manager_skips_malformed_persisted_session() {
        let temp_dir = TempDir::new().unwrap();
        let bad_session_root = temp_dir
            .path()
            .join("sessions")
            .join(session_conversation_dir_name("123"))
            .join("foreground")
            .join(Uuid::new_v4().to_string());
        fs::create_dir_all(&bad_session_root).unwrap();
        fs::write(
            bad_session_root.join("session.json"),
            r#"{"id":"broken","address":{"channel_id":"telegram-main","conversation_id":"123"},"history":["#,
        )
        .unwrap();

        let workspace_manager = WorkspaceManager::load_or_create(temp_dir.path()).unwrap();
        let mut sessions = SessionManager::new(temp_dir.path(), workspace_manager).unwrap();
        let address = test_address();

        let actor = sessions.ensure_foreground_actor(&address).unwrap();
        let snapshot = actor.snapshot().unwrap();
        assert_eq!(snapshot.address.conversation_id, "123");
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
