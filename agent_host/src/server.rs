use crate::agent_status::{AgentRegistry, ManagedAgentKind, ManagedAgentRecord, ManagedAgentState};
use crate::agents::{ForegroundAgent, SubAgentSpec};
use crate::backend::{
    backend_supports_native_multimodal_input,
    compact_session_messages_with_report as run_backend_compaction,
    run_session_with_report_controlled as run_backend_session,
};
use crate::bootstrap::AgentWorkspace;
use crate::channel::{Channel, IncomingMessage};
use crate::channels::command_line::CommandLineChannel;
use crate::channels::telegram::TelegramChannel;
use crate::config::{
    BotCommandConfig, ChannelConfig, MainAgentConfig, ModelConfig, SandboxConfig, SandboxMode,
    ServerConfig, default_bot_commands,
};
use crate::conversation::{ConversationManager, ConversationSettings};
use crate::cron::{
    ClaimedCronTask, CronCheckerConfig, CronCreateRequest, CronManager, CronUpdateRequest,
};
use crate::domain::{
    AttachmentKind, ChannelAddress, OutgoingAttachment, OutgoingMessage, ProcessingState,
    ShowOption, StoredAttachment,
};
use crate::prompt::{AgentPromptKind, build_agent_system_prompt, greeting_for_language};
use crate::sandbox::run_turn_in_child_process;
use crate::session::{
    PendingContinueState, SessionManager, SessionSkillObservation, SessionSnapshot,
    SkillChangeNotice,
};
use crate::sink::{SinkRouter, SinkTarget};
use crate::snapshot::{SnapshotBundle, SnapshotManager};
use crate::subagent::{HostedSubagent, HostedSubagentInner, SubagentState};
use crate::workspace::{WorkspaceManager, WorkspaceMountMaterialization};
use agent_frame::config::{
    AgentConfig as FrameAgentConfig, CacheControlConfig, CodexAuthConfig, UpstreamConfig,
    load_codex_auth_tokens,
};
use agent_frame::skills::discover_skills;
use agent_frame::tooling::{build_tool_registry, terminate_runtime_state_tasks};
use agent_frame::{
    ChatMessage, SessionCompactionStats, SessionEvent, SessionExecutionControl, SessionRunReport,
    TokenUsage, Tool, estimate_session_tokens, extract_assistant_text,
};
use anyhow::{Context, Result, anyhow};
use base64::Engine;
use chrono::Utc;
use humantime::parse_duration;
use serde_json::{Value, json};
use std::collections::{BTreeMap, HashMap, VecDeque};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use tokio::select;
use tokio::sync::{Notify, RwLock, mpsc, oneshot};
use tokio::time::{Duration, MissedTickBehavior, interval};
use tracing::{error, info, warn};

const ATTACHMENT_OPEN_TAG: &str = "<attachment>";
const ATTACHMENT_CLOSE_TAG: &str = "</attachment>";
const CHANNEL_RESTART_MAX_BACKOFF_SECONDS: u64 = 30;

#[derive(Clone, Debug)]
struct BackgroundJobRequest {
    agent_id: uuid::Uuid,
    parent_agent_id: Option<uuid::Uuid>,
    cron_task_id: Option<uuid::Uuid>,
    session: SessionSnapshot,
    model_key: String,
    prompt: String,
    sink: SinkTarget,
}

#[derive(Clone)]
struct ServerRuntime {
    agent_workspace: AgentWorkspace,
    workspace_manager: WorkspaceManager,
    active_workspace_ids: Vec<String>,
    selected_main_model_key: Option<String>,
    selected_reasoning_effort: Option<String>,
    selected_context_compaction_enabled: Option<bool>,
    channels: Arc<HashMap<String, Arc<dyn Channel>>>,
    command_catalog: HashMap<String, Vec<BotCommandConfig>>,
    models: BTreeMap<String, ModelConfig>,
    chat_model_keys: Vec<String>,
    main_agent: MainAgentConfig,
    sandbox: SandboxConfig,
    sink_router: Arc<RwLock<SinkRouter>>,
    cron_manager: Arc<Mutex<CronManager>>,
    agent_registry: Arc<Mutex<AgentRegistry>>,
    agent_registry_notify: Arc<Notify>,
    max_global_sub_agents: usize,
    subagent_count: Arc<AtomicUsize>,
    cron_poll_interval_seconds: u64,
    background_job_sender: mpsc::Sender<BackgroundJobRequest>,
    summary_tracker: Arc<SummaryTracker>,
    subagents: Arc<Mutex<HashMap<uuid::Uuid, Arc<HostedSubagent>>>>,
}

struct SubAgentSlot {
    counter: Arc<AtomicUsize>,
}

struct SummaryInProgressGuard {
    tracker: Arc<SummaryTracker>,
}

struct SummaryTracker {
    count: Mutex<usize>,
    condvar: Condvar,
}

enum TimedRunOutcome {
    Completed(SessionRunReport),
    Yielded(SessionRunReport),
    TimedOut {
        checkpoint: Option<SessionRunReport>,
        error: anyhow::Error,
    },
    Failed {
        checkpoint: Option<SessionRunReport>,
        error: anyhow::Error,
    },
}

enum ForegroundTurnOutcome {
    Replied {
        messages: Vec<ChatMessage>,
        outgoing: OutgoingMessage,
        usage: TokenUsage,
        compaction: SessionCompactionStats,
        timed_out: bool,
    },
    Yielded {
        messages: Vec<ChatMessage>,
        usage: TokenUsage,
        compaction: SessionCompactionStats,
    },
    Failed {
        pending_continue: PendingContinueState,
        error: anyhow::Error,
    },
}

impl Drop for SubAgentSlot {
    fn drop(&mut self) {
        self.counter.fetch_sub(1, Ordering::SeqCst);
    }
}

impl Drop for SummaryInProgressGuard {
    fn drop(&mut self) {
        let mut count = self.tracker.count.lock().unwrap();
        *count = count.saturating_sub(1);
        if *count == 0 {
            self.tracker.condvar.notify_all();
        }
    }
}

impl SummaryInProgressGuard {
    fn new(tracker: Arc<SummaryTracker>) -> Self {
        let mut count = tracker.count.lock().unwrap();
        *count += 1;
        drop(count);
        Self { tracker }
    }
}

impl SummaryTracker {
    fn new() -> Self {
        Self {
            count: Mutex::new(0),
            condvar: Condvar::new(),
        }
    }

    fn wait_for_zero(&self) {
        let mut count = self.count.lock().unwrap();
        while *count > 0 {
            count = self.condvar.wait(count).unwrap();
        }
    }
}

impl ServerRuntime {
    fn resolved_codex_auth(&self, model: &ModelConfig) -> Result<Option<CodexAuthConfig>> {
        if model.upstream_auth_kind() != agent_frame::config::UpstreamAuthKind::CodexSubscription {
            return Ok(None);
        }
        let codex_home = model
            .codex_home
            .as_deref()
            .ok_or_else(|| anyhow!("codex subscription config must include codex_home"))?;
        Ok(Some(load_codex_auth_tokens(Path::new(codex_home))?))
    }

    fn effective_main_model_key(&self) -> Result<String> {
        self.selected_main_model_key.clone().ok_or_else(|| {
            anyhow!("this conversation does not have a main model yet; choose one with /model")
        })
    }
    fn model_config(&self, model_key: &str) -> Result<&ModelConfig> {
        self.models
            .get(model_key)
            .with_context(|| format!("unknown model {}", model_key))
    }

    fn main_agent_timeout_seconds(&self, model_key: &str) -> Result<Option<f64>> {
        if let Some(timeout_seconds) = self.main_agent.timeout_seconds {
            return Ok((timeout_seconds > 0.0).then_some(timeout_seconds));
        }
        Ok(Some(background_agent_timeout_seconds(
            self.models
                .get(model_key)
                .with_context(|| format!("unknown model {}", model_key))?
                .timeout_seconds,
        )))
    }

    fn model_upstream_timeout_seconds(&self, model_key: &str) -> Result<f64> {
        Ok(self
            .models
            .get(model_key)
            .with_context(|| format!("unknown model {}", model_key))?
            .timeout_seconds)
    }

    fn tell_user_now(&self, session: &SessionSnapshot, text: String) -> Result<Value> {
        let channel = self
            .channels
            .get(&session.address.channel_id)
            .with_context(|| format!("unknown channel {}", session.address.channel_id))?
            .clone();
        send_outgoing_message_now(
            channel,
            session.address.clone(),
            OutgoingMessage::text(text),
        )
        .context("failed to send immediate user_tell message")?;
        Ok(json!({
            "ok": true,
            "sent": true
        }))
    }

    fn default_subagent_charge_seconds(&self, model_key: &str) -> Result<f64> {
        Ok(self.main_agent_timeout_seconds(model_key)?.unwrap_or(300.0))
    }

    fn subagent_prompt(description: &str, workbook_relative_path: &str) -> String {
        format!(
            "{description}\n\nWhile you work, keep {workbook_relative_path} updated with concise progress notes, current status, and anything the caller should know before continuing. Write to that workbook as the task evolves, not only at the end."
        )
    }

    fn with_subagents<T>(
        &self,
        f: impl FnOnce(&mut HashMap<uuid::Uuid, Arc<HostedSubagent>>) -> Result<T>,
    ) -> Result<T> {
        let mut subagents = self
            .subagents
            .lock()
            .map_err(|_| anyhow!("subagent manager lock poisoned"))?;
        f(&mut subagents)
    }

    fn get_subagent_handle(&self, subagent_id: uuid::Uuid) -> Result<Arc<HostedSubagent>> {
        self.with_subagents(|subagents| {
            subagents
                .get(&subagent_id)
                .cloned()
                .ok_or_else(|| anyhow!("unknown subagent {}", subagent_id))
        })
    }

    fn subagent_state_view(subagent: &HostedSubagent, inner: &HostedSubagentInner) -> Value {
        json!({
            "agent_id": subagent.id,
            "session_id": subagent.session_id,
            "state": inner.persisted.state,
            "model": inner.persisted.model_key,
            "description": inner.persisted.description,
            "workbook_path": inner.persisted.workbook_relative_path,
            "available_charge_seconds": inner.persisted.available_charge_seconds,
            "resume_pending": inner.persisted.resume_pending,
            "last_result_text": inner.persisted.last_result_text,
            "last_attachment_paths": inner.persisted.last_attachment_paths,
            "last_error": inner.persisted.last_error,
            "turn_count": inner.persisted.turn_count,
        })
    }

    fn persist_subagent_locked(
        subagent: &HostedSubagent,
        inner: &HostedSubagentInner,
    ) -> Result<()> {
        subagent.persist_locked(inner)
    }

    fn create_subagent(
        &self,
        parent_agent_id: uuid::Uuid,
        session: SessionSnapshot,
        description: String,
        model_key: Option<String>,
        charge_seconds: Option<f64>,
    ) -> Result<Value> {
        let model_key = model_key.unwrap_or(self.effective_main_model_key()?);
        self.model_config(&model_key)?;
        let default_charge_seconds = self.default_subagent_charge_seconds(&model_key)?;
        let initial_charge_seconds = charge_seconds.unwrap_or(default_charge_seconds);
        if initial_charge_seconds <= 0.0 {
            return Err(anyhow!("charge_seconds must be positive"));
        }
        let subagent_id = uuid::Uuid::new_v4();
        let runtime_state_root = self
            .agent_workspace
            .root_dir
            .join("runtime")
            .join(&session.workspace_id);
        let workbook_relative_path = format!(".subagent/{subagent_id}-workbook.md");
        let initial_prompt = Self::subagent_prompt(&description, &workbook_relative_path);
        let subagent = HostedSubagent::create(
            subagent_id,
            parent_agent_id,
            session.id,
            session.address.clone(),
            session.workspace_id.clone(),
            session.workspace_root.clone(),
            runtime_state_root,
            model_key.clone(),
            description.clone(),
            default_charge_seconds,
            initial_charge_seconds,
            initial_prompt,
        )?;
        self.register_managed_agent(
            subagent_id,
            ManagedAgentKind::Subagent,
            model_key.clone(),
            Some(parent_agent_id),
            &session,
            ManagedAgentState::Running,
        );
        self.with_subagents(|subagents| {
            subagents.insert(subagent_id, Arc::clone(&subagent));
            Ok(())
        })?;
        let runtime = self.clone();
        thread::spawn(move || {
            if let Err(error) = runtime.run_subagent_worker(subagent) {
                error!(
                    log_stream = "agent",
                    kind = "subagent_worker_failed",
                    agent_id = %subagent_id,
                    error = %format!("{error:#}"),
                    "subagent worker failed"
                );
            }
        });
        Ok(json!({
            "agent_id": subagent_id,
            "model": model_key,
            "description": description,
            "workbook_path": workbook_relative_path,
            "initial_charge_seconds": initial_charge_seconds,
        }))
    }

    fn subagent_progress(
        &self,
        session: &SessionSnapshot,
        subagent_id: uuid::Uuid,
    ) -> Result<Value> {
        let persisted = HostedSubagent::load(
            &self
                .agent_workspace
                .root_dir
                .join("runtime")
                .join(&session.workspace_id),
            subagent_id,
        )?;
        if persisted.session_id != session.id {
            return Err(anyhow!(
                "subagent {} does not belong to this session",
                subagent_id
            ));
        }
        let path = session
            .workspace_root
            .join(&persisted.workbook_relative_path);
        let content = if path.exists() {
            fs::read_to_string(&path)
                .with_context(|| format!("failed to read {}", path.display()))?
        } else {
            String::new()
        };
        Ok(json!({
            "agent_id": subagent_id,
            "workbook_path": persisted.workbook_relative_path,
            "content": content
        }))
    }

    fn subagent_charge(
        &self,
        session: &SessionSnapshot,
        subagent_id: uuid::Uuid,
        charge_seconds: f64,
    ) -> Result<Value> {
        if charge_seconds <= 0.0 {
            return Err(anyhow!("charge_seconds must be positive"));
        }
        let subagent = self.get_subagent_handle(subagent_id)?;
        if subagent.session_id != session.id {
            return Err(anyhow!(
                "subagent {} does not belong to this session",
                subagent_id
            ));
        }
        let mut inner = subagent
            .inner
            .lock()
            .map_err(|_| anyhow!("subagent state lock poisoned"))?;
        if matches!(
            inner.persisted.state,
            SubagentState::Destroyed | SubagentState::Failed
        ) {
            return Err(anyhow!(
                "subagent {} is {}",
                subagent_id,
                serde_json::to_string(&inner.persisted.state).unwrap_or_default()
            ));
        }
        inner.persisted.available_charge_seconds += charge_seconds;
        inner.persisted.updated_at = Utc::now();
        Self::persist_subagent_locked(&subagent, &inner)?;
        drop(inner);
        subagent.condvar.notify_all();
        let inner = subagent
            .inner
            .lock()
            .map_err(|_| anyhow!("subagent state lock poisoned"))?;
        Ok(Self::subagent_state_view(&subagent, &inner))
    }

    fn subagent_tell(
        &self,
        session: &SessionSnapshot,
        subagent_id: uuid::Uuid,
        text: String,
    ) -> Result<Value> {
        let subagent = self.get_subagent_handle(subagent_id)?;
        if subagent.session_id != session.id {
            return Err(anyhow!(
                "subagent {} does not belong to this session",
                subagent_id
            ));
        }
        let mut inner = subagent
            .inner
            .lock()
            .map_err(|_| anyhow!("subagent state lock poisoned"))?;
        if inner.persisted.state != SubagentState::Ready || !inner.persisted.wait_has_returned_ready
        {
            return Err(anyhow!(
                "subagent_tell requires subagent_wait to return a completed turn first"
            ));
        }
        inner.queued_prompts.push_back(text.clone());
        inner.persisted.pending_prompts = inner.queued_prompts.iter().cloned().collect();
        inner.persisted.wait_has_returned_ready = false;
        inner.persisted.updated_at = Utc::now();
        Self::persist_subagent_locked(&subagent, &inner)?;
        drop(inner);
        subagent.condvar.notify_all();
        Ok(json!({
            "agent_id": subagent_id,
            "queued": true,
            "text": text
        }))
    }

    fn subagent_destroy(
        &self,
        session: &SessionSnapshot,
        subagent_id: uuid::Uuid,
    ) -> Result<Value> {
        let subagent = self.get_subagent_handle(subagent_id)?;
        if subagent.session_id != session.id {
            return Err(anyhow!(
                "subagent {} does not belong to this session",
                subagent_id
            ));
        }
        {
            let mut inner = subagent
                .inner
                .lock()
                .map_err(|_| anyhow!("subagent state lock poisoned"))?;
            inner.persisted.state = SubagentState::Destroyed;
            inner.persisted.updated_at = Utc::now();
            inner.persisted.last_error = Some("destroyed".to_string());
            if let Some(control) = inner.active_control.take() {
                control.request_cancel();
            }
            Self::persist_subagent_locked(&subagent, &inner)?;
        }
        subagent.condvar.notify_all();
        self.with_subagents(|subagents| {
            subagents.remove(&subagent_id);
            Ok(())
        })?;
        Ok(json!({
            "agent_id": subagent_id,
            "destroyed": true
        }))
    }

    fn subagent_wait(
        &self,
        session: &SessionSnapshot,
        subagent_id: uuid::Uuid,
        timeout_seconds: f64,
        control: Option<SessionExecutionControl>,
    ) -> Result<Value> {
        if timeout_seconds < 0.0 {
            return Err(anyhow!("timeout_seconds must be non-negative"));
        }
        let subagent = self.get_subagent_handle(subagent_id)?;
        if subagent.session_id != session.id {
            return Err(anyhow!(
                "subagent {} does not belong to this session",
                subagent_id
            ));
        }
        let deadline = (timeout_seconds > 0.0)
            .then(|| std::time::Instant::now() + Duration::from_secs_f64(timeout_seconds));
        let signal_receiver = control
            .as_ref()
            .map(SessionExecutionControl::signal_receiver);
        loop {
            let mut inner = subagent
                .inner
                .lock()
                .map_err(|_| anyhow!("subagent state lock poisoned"))?;
            match inner.persisted.state {
                SubagentState::Ready => {
                    inner.persisted.wait_has_returned_ready = true;
                    inner.persisted.updated_at = Utc::now();
                    Self::persist_subagent_locked(&subagent, &inner)?;
                    return Ok(json!({
                        "agent_id": subagent_id,
                        "running": false,
                        "completed": true,
                        "reason": "completed",
                        "text": inner.persisted.last_result_text,
                        "attachment_paths": inner.persisted.last_attachment_paths,
                    }));
                }
                SubagentState::WaitingForCharge => {
                    return Ok(json!({
                        "agent_id": subagent_id,
                        "running": false,
                        "completed": false,
                        "reason": "charge_exhausted",
                        "error": inner.persisted.last_error,
                        "text": inner.persisted.last_result_text,
                        "attachment_paths": inner.persisted.last_attachment_paths,
                    }));
                }
                SubagentState::Failed => {
                    return Ok(json!({
                        "agent_id": subagent_id,
                        "running": false,
                        "completed": false,
                        "reason": "failed",
                        "error": inner.persisted.last_error,
                    }));
                }
                SubagentState::Destroyed => {
                    return Ok(json!({
                        "agent_id": subagent_id,
                        "running": false,
                        "completed": false,
                        "reason": "destroyed",
                    }));
                }
                SubagentState::Running => {}
            }
            let wait_duration = deadline.map(|deadline| {
                deadline
                    .saturating_duration_since(std::time::Instant::now())
                    .min(Duration::from_millis(200))
            });
            drop(inner);
            if let Some(receiver) = &signal_receiver
                && receiver.try_recv().is_ok()
            {
                return Ok(json!({
                    "agent_id": subagent_id,
                    "running": true,
                    "completed": false,
                    "interrupted": true,
                    "reason": "agent_turn_interrupted",
                }));
            }
            if let Some(deadline) = deadline
                && std::time::Instant::now() >= deadline
            {
                return Ok(json!({
                    "agent_id": subagent_id,
                    "running": true,
                    "completed": false,
                    "timed_out": true,
                    "reason": "wait_timed_out",
                }));
            }
            let inner = subagent
                .inner
                .lock()
                .map_err(|_| anyhow!("subagent state lock poisoned"))?;
            let wait_duration = wait_duration.unwrap_or(Duration::from_millis(200));
            let _ = subagent
                .condvar
                .wait_timeout(inner, wait_duration)
                .map_err(|_| anyhow!("subagent state lock poisoned"))?;
        }
    }

    fn subagent_list(&self, session: &SessionSnapshot) -> Result<Value> {
        let runtime_state_root = self
            .agent_workspace
            .root_dir
            .join("runtime")
            .join(&session.workspace_id);
        let entries = HostedSubagent::list(&runtime_state_root)?
            .into_iter()
            .filter(|state| state.session_id == session.id)
            .map(|state| {
                json!({
                    "agent_id": state.id,
                    "description": state.description,
                    "state": state.state,
                    "model": state.model_key,
                    "workbook_path": state.workbook_relative_path,
                    "available_charge_seconds": state.available_charge_seconds,
                })
            })
            .collect::<Vec<_>>();
        Ok(json!({
            "subagents": entries,
            "count": entries.len()
        }))
    }

    fn destroy_subagents_for_session(&self, session_id: uuid::Uuid) -> Result<usize> {
        let targets = self.with_subagents(|subagents| {
            Ok(subagents
                .values()
                .filter(|subagent| subagent.session_id == session_id)
                .cloned()
                .collect::<Vec<_>>())
        })?;
        let mut destroyed = 0usize;
        for subagent in targets {
            {
                let mut inner = subagent
                    .inner
                    .lock()
                    .map_err(|_| anyhow!("subagent state lock poisoned"))?;
                if inner.persisted.state == SubagentState::Destroyed {
                    continue;
                }
                inner.persisted.state = SubagentState::Destroyed;
                inner.persisted.updated_at = Utc::now();
                inner.persisted.last_error = Some("session_destroyed".to_string());
                if let Some(control) = inner.active_control.take() {
                    control.request_cancel();
                }
                Self::persist_subagent_locked(&subagent, &inner)?;
            }
            subagent.condvar.notify_all();
            self.with_subagents(|subagents| {
                subagents.remove(&subagent.id);
                Ok(())
            })?;
            destroyed = destroyed.saturating_add(1);
        }
        Ok(destroyed)
    }

    fn subagent_session_snapshot(
        &self,
        subagent: &HostedSubagent,
        message_count: usize,
        _model_key: &str,
    ) -> SessionSnapshot {
        SessionSnapshot {
            id: subagent.session_id,
            agent_id: subagent.id,
            address: subagent.address.clone(),
            root_dir: self.agent_workspace.root_dir.join("subagent-sessions"),
            attachments_dir: subagent.workspace_root.join("upload"),
            workspace_id: subagent.workspace_id.clone(),
            workspace_root: subagent.workspace_root.clone(),
            message_count,
            agent_message_count: message_count,
            agent_messages: Vec::new(),
            last_agent_returned_at: None,
            last_compacted_at: None,
            turn_count: 0,
            last_compacted_turn_count: 0,
            cumulative_usage: TokenUsage::default(),
            cumulative_compaction: SessionCompactionStats::default(),
            api_timeout_override_seconds: None,
            skill_states: HashMap::new(),
            pending_continue: None,
            pending_workspace_summary: false,
            close_after_summary: false,
        }
    }

    fn idle_compact_subagents_for_session(
        &self,
        session: &SessionSnapshot,
        idle_threshold: Duration,
    ) -> Result<()> {
        let now = Utc::now();
        let subagents = self.with_subagents(|subagents| {
            Ok(subagents
                .values()
                .filter(|subagent| subagent.session_id == session.id)
                .cloned()
                .collect::<Vec<_>>())
        })?;
        for subagent in subagents {
            let (model_key, messages, turn_count) = {
                let inner = subagent
                    .inner
                    .lock()
                    .map_err(|_| anyhow!("subagent state lock poisoned"))?;
                if !matches!(
                    inner.persisted.state,
                    SubagentState::Ready | SubagentState::WaitingForCharge
                ) {
                    continue;
                }
                let Some(last_returned_at) = inner.persisted.last_returned_at else {
                    continue;
                };
                if now
                    .signed_duration_since(last_returned_at)
                    .to_std()
                    .unwrap_or_default()
                    < idle_threshold
                {
                    continue;
                }
                if inner.persisted.turn_count <= inner.persisted.last_compacted_turn_count
                    || inner.persisted.messages.is_empty()
                {
                    continue;
                }
                (
                    inner.persisted.model_key.clone(),
                    inner.persisted.messages.clone(),
                    inner.persisted.turn_count,
                )
            };
            let model = self.model_config(&model_key)?;
            let subagent_session =
                self.subagent_session_snapshot(&subagent, messages.len(), &model_key);
            let config = self.build_agent_frame_config(
                &subagent_session,
                &subagent.workspace_root,
                AgentPromptKind::SubAgent,
                &model_key,
                None,
            )?;
            let extra_tools = self.build_extra_tools(
                &subagent_session,
                AgentPromptKind::SubAgent,
                subagent.id,
                None,
            );
            let report = run_backend_compaction(model.backend, messages, config, extra_tools)?;
            if !report.compacted {
                continue;
            }
            let stats = compaction_stats_from_report(&report);
            let mut inner = subagent
                .inner
                .lock()
                .map_err(|_| anyhow!("subagent state lock poisoned"))?;
            inner.persisted.messages = report.messages;
            inner.persisted.last_compacted_turn_count = turn_count;
            inner.persisted.updated_at = Utc::now();
            inner.persisted.cumulative_compaction.run_count = inner
                .persisted
                .cumulative_compaction
                .run_count
                .saturating_add(stats.run_count);
            inner.persisted.cumulative_compaction.compacted_run_count = inner
                .persisted
                .cumulative_compaction
                .compacted_run_count
                .saturating_add(stats.compacted_run_count);
            inner
                .persisted
                .cumulative_compaction
                .estimated_tokens_before = inner
                .persisted
                .cumulative_compaction
                .estimated_tokens_before
                .saturating_add(stats.estimated_tokens_before);
            inner.persisted.cumulative_compaction.estimated_tokens_after = inner
                .persisted
                .cumulative_compaction
                .estimated_tokens_after
                .saturating_add(stats.estimated_tokens_after);
            inner
                .persisted
                .cumulative_compaction
                .usage
                .add_assign(&stats.usage);
            Self::persist_subagent_locked(&subagent, &inner)?;
        }
        Ok(())
    }

    fn register_managed_agent(
        &self,
        id: uuid::Uuid,
        kind: ManagedAgentKind,
        model_key: String,
        parent_agent_id: Option<uuid::Uuid>,
        session: &SessionSnapshot,
        state: ManagedAgentState,
    ) {
        if let Ok(mut registry) = self.agent_registry.lock() {
            if let Err(error) = registry.register(ManagedAgentRecord {
                id,
                kind,
                parent_agent_id,
                session_id: Some(session.id),
                channel_id: session.address.channel_id.clone(),
                model_key,
                state,
                created_at: Utc::now(),
                started_at: if state == ManagedAgentState::Running {
                    Some(Utc::now())
                } else {
                    None
                },
                finished_at: None,
                error: None,
                usage: TokenUsage::default(),
            }) {
                warn!(
                    log_stream = "server",
                    kind = "agent_registry_persist_failed",
                    agent_id = %id,
                    error = %format!("{error:#}"),
                    "failed to persist agent registry after register"
                );
            }
        }
        self.agent_registry_notify.notify_waiters();
    }

    fn mark_managed_agent_running(&self, id: uuid::Uuid) {
        if let Ok(mut registry) = self.agent_registry.lock() {
            if let Err(error) = registry.mark_running(id, Utc::now()) {
                warn!(
                    log_stream = "server",
                    kind = "agent_registry_persist_failed",
                    agent_id = %id,
                    error = %format!("{error:#}"),
                    "failed to persist agent registry after mark_running"
                );
            }
        }
        self.agent_registry_notify.notify_waiters();
    }

    fn mark_managed_agent_completed(&self, id: uuid::Uuid, usage: &TokenUsage) {
        if let Ok(mut registry) = self.agent_registry.lock() {
            if let Err(error) = registry.mark_completed(id, Utc::now(), usage.clone()) {
                warn!(
                    log_stream = "server",
                    kind = "agent_registry_persist_failed",
                    agent_id = %id,
                    error = %format!("{error:#}"),
                    "failed to persist agent registry after mark_completed"
                );
            }
        }
        self.agent_registry_notify.notify_waiters();
    }

    fn mark_managed_agent_failed(&self, id: uuid::Uuid, usage: &TokenUsage, error: &anyhow::Error) {
        if let Ok(mut registry) = self.agent_registry.lock() {
            if let Err(persist_error) =
                registry.mark_failed(id, Utc::now(), usage.clone(), format!("{error:#}"))
            {
                warn!(
                    log_stream = "server",
                    kind = "agent_registry_persist_failed",
                    agent_id = %id,
                    error = %format!("{persist_error:#}"),
                    "failed to persist agent registry after mark_failed"
                );
            }
        }
        self.agent_registry_notify.notify_waiters();
    }

    fn mark_managed_agent_timed_out(
        &self,
        id: uuid::Uuid,
        usage: &TokenUsage,
        error: &anyhow::Error,
    ) {
        if let Ok(mut registry) = self.agent_registry.lock() {
            if let Err(persist_error) =
                registry.mark_timed_out(id, Utc::now(), usage.clone(), format!("{error:#}"))
            {
                warn!(
                    log_stream = "server",
                    kind = "agent_registry_persist_failed",
                    agent_id = %id,
                    error = %format!("{persist_error:#}"),
                    "failed to persist agent registry after mark_timed_out"
                );
            }
        }
        self.agent_registry_notify.notify_waiters();
    }

    fn list_managed_agents(&self, kind: ManagedAgentKind) -> Result<Value> {
        let registry = self
            .agent_registry
            .lock()
            .map_err(|_| anyhow!("agent registry lock poisoned"))?;
        Ok(serde_json::to_value(registry.list_by_kind(kind))
            .context("failed to serialize agent registry view")?)
    }

    fn get_managed_agent(&self, agent_id: uuid::Uuid) -> Result<Value> {
        let registry = self
            .agent_registry
            .lock()
            .map_err(|_| anyhow!("agent registry lock poisoned"))?;
        let record = registry
            .get(agent_id)
            .ok_or_else(|| anyhow!("agent {} not found", agent_id))?;
        Ok(serde_json::to_value(record).context("failed to serialize agent record")?)
    }

    fn list_workspaces(&self, query: Option<String>, include_archived: bool) -> Result<Value> {
        self.summary_tracker.wait_for_zero();
        let items = self
            .workspace_manager
            .list_workspaces(query.as_deref(), include_archived)?
            .into_iter()
            .filter(|workspace| {
                workspace_visible_in_list(
                    &workspace.id,
                    &self.active_workspace_ids,
                    include_archived && workspace.state == "archived",
                )
            })
            .map(|workspace| {
                json!({
                    "workspace_id": workspace.id,
                    "title": workspace.title,
                    "summary": workspace.summary,
                    "state": workspace.state,
                    "updated_at": workspace.updated_at,
                    "last_content_modified_at": workspace.last_content_modified_at,
                })
            })
            .collect::<Vec<_>>();
        Ok(json!({ "workspaces": items }))
    }

    fn list_workspace_contents(
        &self,
        workspace_id: String,
        path: Option<String>,
        depth: usize,
        limit: usize,
    ) -> Result<Value> {
        let items = self.workspace_manager.list_workspace_contents(
            &workspace_id,
            path.as_deref(),
            depth,
            limit,
        )?;
        Ok(json!({
            "workspace_id": workspace_id,
            "path": path.unwrap_or_else(|| ".".to_string()),
            "entries": items,
        }))
    }

    fn mount_workspace(
        &self,
        session: &SessionSnapshot,
        source_workspace_id: String,
        mount_name: Option<String>,
    ) -> Result<Value> {
        let mount_name = mount_name
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty())
            .unwrap_or_else(|| {
                format!(
                    "workspace-{}",
                    &source_workspace_id[..8.min(source_workspace_id.len())]
                )
            });
        let materialization = if matches!(self.sandbox.mode, crate::config::SandboxMode::Bubblewrap)
        {
            WorkspaceMountMaterialization::HostSnapshotCopy
        } else {
            WorkspaceMountMaterialization::HostSymlink
        };
        let mount_path = self.workspace_manager.mount_workspace_snapshot(
            &session.workspace_id,
            &source_workspace_id,
            &mount_name,
            materialization,
        )?;
        let relative_mount = mount_path
            .strip_prefix(&session.workspace_root)
            .unwrap_or(&mount_path)
            .to_string_lossy()
            .to_string();
        Ok(json!({
            "workspace_id": source_workspace_id,
            "mount_name": mount_name,
            "mount_path": relative_mount,
            "read_only": true,
        }))
    }

    fn move_workspace_contents(
        &self,
        session: &SessionSnapshot,
        source_workspace_id: String,
        paths: Vec<String>,
        target_dir: Option<String>,
        source_summary_update: Option<String>,
        target_summary_update: Option<String>,
    ) -> Result<Value> {
        if source_workspace_id == session.workspace_id {
            return Err(anyhow!(
                "source workspace must differ from the current workspace"
            ));
        }
        let summary = self.workspace_manager.move_contents_between_workspaces(
            &source_workspace_id,
            &session.workspace_id,
            &paths,
            target_dir.as_deref(),
            source_summary_update,
            target_summary_update,
        )?;
        Ok(serde_json::to_value(summary).context("failed to serialize workspace move result")?)
    }

    fn has_active_child_agents(&self, parent_agent_id: uuid::Uuid) -> bool {
        self.agent_registry
            .lock()
            .map(|registry| registry.has_active_children(parent_agent_id))
            .unwrap_or(false)
    }

    async fn wait_for_child_agents_to_finish(&self, parent_agent_id: uuid::Uuid) {
        while self.has_active_child_agents(parent_agent_id) {
            self.agent_registry_notify.notified().await;
        }
    }

    fn ensure_agent_tmp_dir(&self, agent_id: uuid::Uuid) -> Result<PathBuf> {
        let path = self.agent_workspace.tmp_dir.join(agent_id.to_string());
        std::fs::create_dir_all(&path)
            .with_context(|| format!("failed to create agent tmp dir {}", path.display()))?;
        Ok(path)
    }

    fn build_agent_frame_config(
        &self,
        session: &SessionSnapshot,
        workspace_root: &Path,
        kind: AgentPromptKind,
        model_key: &str,
        upstream_timeout_seconds: Option<f64>,
    ) -> Result<FrameAgentConfig> {
        let model = self.model_config(model_key)?;
        let image_tool_upstream = match model.image_tool_model.as_deref() {
            None | Some("self") => None,
            Some(other_model_key) => {
                let image_model = self.model_config(other_model_key)?;
                Some(UpstreamConfig {
                    base_url: image_model.api_endpoint.clone(),
                    model: image_model.model.clone(),
                    api_kind: image_model.upstream_api_kind(),
                    auth_kind: image_model.upstream_auth_kind(),
                    supports_vision_input: image_model.supports_vision_input,
                    api_key: image_model.api_key.clone(),
                    api_key_env: image_model.api_key_env.clone(),
                    chat_completions_path: image_model.chat_completions_path.clone(),
                    codex_home: image_model.codex_home.clone().map(Into::into),
                    codex_auth: self.resolved_codex_auth(image_model)?,
                    auth_credentials_store_mode: image_model.auth_credentials_store_mode,
                    timeout_seconds: image_model.timeout_seconds,
                    context_window_tokens: image_model.context_window_tokens,
                    cache_control: image_model
                        .cache_ttl
                        .as_ref()
                        .map(|ttl| CacheControlConfig {
                            cache_type: "ephemeral".to_string(),
                            ttl: Some(ttl.clone()),
                        }),
                    reasoning: image_model.reasoning.clone(),
                    headers: image_model.headers.clone(),
                    native_web_search: image_model.native_web_search.clone(),
                    external_web_search: image_model.external_web_search.clone(),
                })
            }
        };
        let commands = self
            .command_catalog
            .get(&session.address.channel_id)
            .cloned()
            .unwrap_or_else(default_bot_commands);
        let workspace_summary = self
            .workspace_manager
            .ensure_workspace_exists(&session.workspace_id)
            .map(|workspace| workspace.summary)
            .unwrap_or_default();
        let reasoning =
            effective_reasoning_config(model, self.selected_reasoning_effort.as_deref());

        Ok(FrameAgentConfig {
            enabled_tools: self.main_agent.enabled_tools.clone(),
            upstream: UpstreamConfig {
                base_url: model.api_endpoint.clone(),
                model: model.model.clone(),
                api_kind: model.upstream_api_kind(),
                auth_kind: model.upstream_auth_kind(),
                supports_vision_input: model.supports_vision_input,
                api_key: model.api_key.clone(),
                api_key_env: model.api_key_env.clone(),
                chat_completions_path: model.chat_completions_path.clone(),
                codex_home: model.codex_home.clone().map(Into::into),
                codex_auth: self.resolved_codex_auth(model)?,
                auth_credentials_store_mode: model.auth_credentials_store_mode,
                timeout_seconds: upstream_timeout_seconds
                    .unwrap_or(model.timeout_seconds)
                    .min(model.timeout_seconds),
                context_window_tokens: model.context_window_tokens,
                cache_control: model.cache_ttl.as_ref().map(|ttl| CacheControlConfig {
                    cache_type: "ephemeral".to_string(),
                    ttl: Some(ttl.clone()),
                }),
                reasoning,
                headers: model.headers.clone(),
                native_web_search: model.native_web_search.clone(),
                external_web_search: model.external_web_search.clone(),
            },
            image_tool_upstream,
            skills_dirs: if matches!(self.sandbox.mode, crate::config::SandboxMode::Bubblewrap) {
                vec![workspace_root.join(".skills")]
            } else {
                vec![self.agent_workspace.skills_dir.clone()]
            },
            system_prompt: build_agent_system_prompt(
                &self.agent_workspace,
                session,
                &workspace_summary,
                kind,
                model_key,
                model,
                &self.models,
                &self.chat_model_keys,
                &self.main_agent,
                &commands,
            ),
            max_tool_roundtrips: self.main_agent.max_tool_roundtrips,
            workspace_root: workspace_root.to_path_buf(),
            runtime_state_root: self
                .agent_workspace
                .root_dir
                .join("runtime")
                .join(&session.workspace_id),
            enable_context_compression: self
                .selected_context_compaction_enabled
                .unwrap_or(self.main_agent.enable_context_compression),
            effective_context_window_percent: self.main_agent.effective_context_window_percent,
            auto_compact_token_limit: self.main_agent.auto_compact_token_limit,
            retain_recent_messages: self.main_agent.retain_recent_messages,
        })
    }

    fn build_extra_tools(
        &self,
        session: &SessionSnapshot,
        kind: AgentPromptKind,
        agent_id: uuid::Uuid,
        control: Option<SessionExecutionControl>,
    ) -> Vec<Tool> {
        let mut tools = Vec::new();
        {
            let runtime = self.clone();
            tools.push(Tool::new(
                "workspaces_list",
                "Call this tool to get historical information, including earlier chat content and the corresponding workspace. It lists known workspaces by id, title, summary, state, and timestamps. Archived workspaces are hidden by default.",
                json!({
                    "type": "object",
                    "properties": {
                        "query": {"type": "string"},
                        "include_archived": {"type": "boolean"}
                    },
                    "additionalProperties": false
                }),
                move |arguments| {
                    let object = arguments
                        .as_object()
                        .ok_or_else(|| anyhow!("tool arguments must be an object"))?;
                    runtime.list_workspaces(
                        optional_string_arg(object, "query")?,
                        object
                            .get("include_archived")
                            .and_then(Value::as_bool)
                            .unwrap_or(false),
                    )
                },
            ));

            let runtime = self.clone();
            tools.push(Tool::new(
                "workspace_content_list",
                "Call this tool after selecting a historical workspace to inspect what content exists there at a high level, without reading file bodies. Returns files and directories under the requested path.",
                json!({
                    "type": "object",
                    "properties": {
                        "workspace_id": {"type": "string"},
                        "path": {"type": "string"},
                        "depth": {"type": "integer"},
                        "limit": {"type": "integer"}
                    },
                    "required": ["workspace_id"],
                    "additionalProperties": false
                }),
                move |arguments| {
                    let object = arguments
                        .as_object()
                        .ok_or_else(|| anyhow!("tool arguments must be an object"))?;
                    let workspace_id = string_arg_required(object, "workspace_id")?;
                    let depth = object.get("depth").and_then(Value::as_u64).unwrap_or(2) as usize;
                    let limit = object.get("limit").and_then(Value::as_u64).unwrap_or(100) as usize;
                    runtime.list_workspace_contents(
                        workspace_id,
                        optional_string_arg(object, "path")?,
                        depth,
                        limit.clamp(1, 500),
                    )
                },
            ));

            let runtime = self.clone();
            let mount_session = session.clone();
            tools.push(Tool::new(
                "workspace_mount",
                "Call this tool to bring a historical workspace into the current workspace as a read-only mount so you can inspect or read its content safely. Returns the mount path relative to the current workspace root.",
                json!({
                    "type": "object",
                    "properties": {
                        "workspace_id": {"type": "string"},
                        "mount_name": {"type": "string"}
                    },
                    "required": ["workspace_id"],
                    "additionalProperties": false
                }),
                move |arguments| {
                    let object = arguments
                        .as_object()
                        .ok_or_else(|| anyhow!("tool arguments must be an object"))?;
                    runtime.mount_workspace(
                        &mount_session,
                        string_arg_required(object, "workspace_id")?,
                        optional_string_arg(object, "mount_name")?,
                    )
                },
            ));

            let runtime = self.clone();
            let move_session = session.clone();
            tools.push(Tool::new(
                "workspace_content_move",
                "Call this tool to carry forward selected content from an older workspace into the current workspace. Source and target summaries can be updated when the move changes what the workspaces represent.",
                json!({
                    "type": "object",
                    "properties": {
                        "source_workspace_id": {"type": "string"},
                        "paths": {
                            "type": "array",
                            "items": {"type": "string"}
                        },
                        "target_dir": {"type": "string"},
                        "source_summary_update": {"type": "string"},
                        "target_summary_update": {"type": "string"}
                    },
                    "required": ["source_workspace_id", "paths"],
                    "additionalProperties": false
                }),
                move |arguments| {
                    let object = arguments
                        .as_object()
                        .ok_or_else(|| anyhow!("tool arguments must be an object"))?;
                    let paths = object
                        .get("paths")
                        .and_then(Value::as_array)
                        .ok_or_else(|| anyhow!("paths must be an array"))?
                        .iter()
                        .map(|value| {
                            value
                                .as_str()
                                .map(ToOwned::to_owned)
                                .filter(|value| !value.trim().is_empty())
                                .ok_or_else(|| anyhow!("each path must be a non-empty string"))
                        })
                        .collect::<Result<Vec<_>>>()?;
                    runtime.move_workspace_contents(
                        &move_session,
                        string_arg_required(object, "source_workspace_id")?,
                        paths,
                        optional_string_arg(object, "target_dir")?,
                        optional_string_arg(object, "source_summary_update")?,
                        optional_string_arg(object, "target_summary_update")?,
                    )
                },
            ));
        }

        if matches!(
            kind,
            AgentPromptKind::MainForeground | AgentPromptKind::MainBackground
        ) {
            let runtime = self.clone();
            let tell_session = session.clone();
            tools.push(Tool::new(
                "user_tell",
                "Immediately send a short progress or coordination message to the current user conversation without waiting for the current turn to finish. Use this for any mid-task user-facing update that should appear as its own chat bubble while work is still ongoing. If you want to answer the user, explain what you are doing, report progress, or give a transitional update before the turn is finished, use user_tell instead of only putting that text in an assistant message with tool_calls.",
                json!({
                    "type": "object",
                    "properties": {
                        "text": {"type": "string"}
                    },
                    "required": ["text"],
                    "additionalProperties": false
                }),
                move |arguments| {
                    let object = arguments
                        .as_object()
                        .ok_or_else(|| anyhow!("tool arguments must be an object"))?;
                    runtime.tell_user_now(&tell_session, string_arg_required(object, "text")?)
                },
            ));

            let runtime = self.clone();
            let create_session = session.clone();
            tools.push(Tool::new(
                "subagent_create",
                "Start a session-bound subagent. Requires description. Optionally set model and charge_seconds.",
                json!({
                    "type": "object",
                    "properties": {
                        "description": {"type": "string"},
                        "model": {"type": "string"},
                        "charge_seconds": {"type": "number"}
                    },
                    "required": ["description"],
                    "additionalProperties": false
                }),
                move |arguments| {
                    let object = arguments
                        .as_object()
                        .ok_or_else(|| anyhow!("tool arguments must be an object"))?;
                    runtime.create_subagent(
                        agent_id,
                        create_session.clone(),
                        string_arg_required(object, "description")?,
                        optional_string_arg(object, "model")?,
                        object.get("charge_seconds").and_then(Value::as_f64),
                    )
                },
            ));

            let runtime = self.clone();
            let progress_session = session.clone();
            tools.push(Tool::new(
                "subagent_progress",
                "Read a subagent workbook.",
                json!({
                    "type": "object",
                    "properties": {
                        "agent_id": {"type": "string"}
                    },
                    "required": ["agent_id"],
                    "additionalProperties": false
                }),
                move |arguments| {
                    let object = arguments
                        .as_object()
                        .ok_or_else(|| anyhow!("tool arguments must be an object"))?;
                    runtime
                        .subagent_progress(&progress_session, parse_uuid_arg(object, "agent_id")?)
                },
            ));

            let runtime = self.clone();
            let charge_session = session.clone();
            tools.push(Tool::new(
                "subagent_charge",
                "Give a subagent more runtime. If the previous subagent_wait failed because of an upstream API timeout, recharge and retry at most once.",
                json!({
                    "type": "object",
                    "properties": {
                        "agent_id": {"type": "string"},
                        "charge_seconds": {"type": "number"}
                    },
                    "required": ["agent_id", "charge_seconds"],
                    "additionalProperties": false
                }),
                move |arguments| {
                    let object = arguments
                        .as_object()
                        .ok_or_else(|| anyhow!("tool arguments must be an object"))?;
                    runtime.subagent_charge(
                        &charge_session,
                        parse_uuid_arg(object, "agent_id")?,
                        f64_arg_required(object, "charge_seconds")?,
                    )
                },
            ));

            let runtime = self.clone();
            let tell_session = session.clone();
            tools.push(Tool::new(
                "subagent_tell",
                "Queue the next user turn for a subagent after subagent_wait has returned a completed turn.",
                json!({
                    "type": "object",
                    "properties": {
                        "agent_id": {"type": "string"},
                        "text": {"type": "string"}
                    },
                    "required": ["agent_id", "text"],
                    "additionalProperties": false
                }),
                move |arguments| {
                    let object = arguments
                        .as_object()
                        .ok_or_else(|| anyhow!("tool arguments must be an object"))?;
                    runtime.subagent_tell(
                        &tell_session,
                        parse_uuid_arg(object, "agent_id")?,
                        string_arg_required(object, "text")?,
                    )
                },
            ));

            let runtime = self.clone();
            let destroy_session = session.clone();
            tools.push(Tool::new(
                "subagent_destroy",
                "Destroy a subagent.",
                json!({
                    "type": "object",
                    "properties": {
                        "agent_id": {"type": "string"}
                    },
                    "required": ["agent_id"],
                    "additionalProperties": false
                }),
                move |arguments| {
                    let object = arguments
                        .as_object()
                        .ok_or_else(|| anyhow!("tool arguments must be an object"))?;
                    runtime.subagent_destroy(&destroy_session, parse_uuid_arg(object, "agent_id")?)
                },
            ));

            let runtime = self.clone();
            let wait_session = session.clone();
            let wait_control = control.clone();
            tools.push(Tool::new_interruptible(
                "subagent_wait",
                "Wait until a subagent finishes its current turn or runs out of charge. Completed subagents stay alive. If it returns a failed upstream API timeout, you may recharge and retry once, but do not loop repeated retries.",
                json!({
                    "type": "object",
                    "properties": {
                        "agent_id": {"type": "string"},
                        "timeout_seconds": {"type": "number"}
                    },
                    "required": ["agent_id"],
                    "additionalProperties": false
                }),
                move |arguments| {
                    let object = arguments
                        .as_object()
                        .ok_or_else(|| anyhow!("tool arguments must be an object"))?;
                    runtime.subagent_wait(
                        &wait_session,
                        parse_uuid_arg(object, "agent_id")?,
                        object.get("timeout_seconds").and_then(Value::as_f64).unwrap_or(0.0),
                        wait_control.clone(),
                    )
                },
            ));
        }

        if matches!(kind, AgentPromptKind::MainForeground) {
            let runtime = self.clone();
            let session = session.clone();
            tools.push(Tool::new(
                "start_background_agent",
                "Start a main background agent. Arguments: task (string), optional model (string), optional sink object. If sink is omitted, results go back to the current user conversation. Usually omit sink unless you really need custom routing. Never use session_id as conversation_id. Sink forms: {kind:\"current_session\"}, {kind:\"direct\", channel_id, conversation_id, user_id?, display_name?}, {kind:\"broadcast\", topic}, or {kind:\"multi\", targets:[...]}",
                json!({
                    "type": "object",
                    "properties": {
                        "task": {"type": "string"},
                        "model": {"type": "string"},
                        "sink": {"type": "object"}
                    },
                    "required": ["task"],
                    "additionalProperties": false
                }),
                move |arguments| {
                    let object = arguments
                        .as_object()
                        .ok_or_else(|| anyhow!("tool arguments must be an object"))?;
                    let task = object
                        .get("task")
                        .and_then(Value::as_str)
                        .map(str::trim)
                        .filter(|value| !value.is_empty())
                        .ok_or_else(|| anyhow!("task must be a non-empty string"))?;
                    let model_key = object
                        .get("model")
                        .and_then(Value::as_str)
                        .map(ToOwned::to_owned);
                    let sink = match object.get("sink") {
                        Some(value) => parse_sink_target(
                            value,
                            Some(SinkTarget::Direct(session.address.clone())),
                        )?,
                        None => SinkTarget::Direct(session.address.clone()),
                    };
                    let sink = normalize_sink_target(sink, &session);
                    runtime.start_background_agent(
                        agent_id,
                        session.clone(),
                        model_key,
                        task.to_string(),
                        sink,
                    )
                },
            ));
        }

        if matches!(
            kind,
            AgentPromptKind::MainForeground | AgentPromptKind::MainBackground
        ) {
            let runtime = self.clone();
            tools.push(Tool::new(
                "list_cron_tasks",
                "List configured cron tasks. Returns summaries including enabled state and next_run_at.",
                json!({
                    "type": "object",
                    "properties": {},
                    "additionalProperties": false
                }),
                move |_| runtime.list_cron_tasks(),
            ));

            let runtime = self.clone();
            tools.push(Tool::new(
                "get_cron_task",
                "Get full details for a cron task by id.",
                json!({
                    "type": "object",
                    "properties": {
                        "id": {"type": "string"}
                    },
                    "required": ["id"],
                    "additionalProperties": false
                }),
                move |arguments| {
                    let object = arguments
                        .as_object()
                        .ok_or_else(|| anyhow!("tool arguments must be an object"))?;
                    let id = parse_uuid_arg(object, "id")?;
                    runtime.get_cron_task(id)
                },
            ));

            let runtime = self.clone();
            let create_session = session.clone();
            tools.push(Tool::new(
                "create_cron_task",
                "Create a persisted cron task that later launches a main background agent. Use a standard cron expression. The checker is optional: checker exit code 0 triggers the LLM, non-zero skips the run, and checker execution errors or timeouts still trigger the LLM.",
                json!({
                    "type": "object",
                    "properties": {
                        "name": {"type": "string"},
                        "description": {"type": "string"},
                        "schedule": {"type": "string"},
                        "task": {"type": "string"},
                                "enabled": {"type": "boolean"},
                                "sink": {"type": "object"},
                                "checker_command": {"type": "string"},
                        "checker_timeout_seconds": {"type": "number"},
                        "checker_cwd": {"type": "string"}
                    },
                    "required": ["name", "description", "schedule", "task"],
                    "additionalProperties": false
                }),
                move |arguments| {
                    let object = arguments
                        .as_object()
                        .ok_or_else(|| anyhow!("tool arguments must be an object"))?;
                    let sink = match object.get("sink") {
                        Some(value) => parse_sink_target(
                            value,
                            Some(SinkTarget::Direct(create_session.address.clone())),
                        )?,
                        None => SinkTarget::Direct(create_session.address.clone()),
                    };
                    let sink = normalize_sink_target(sink, &create_session);
                    let checker = parse_checker_from_tool_args(object)?;
                    runtime.create_cron_task(
                        create_session.clone(),
                        CronCreateRequest {
                            name: string_arg_required(object, "name")?,
                            description: string_arg_required(object, "description")?,
                            schedule: string_arg_required(object, "schedule")?,
                            model_key: runtime.effective_main_model_key()?,
                            prompt: string_arg_required(object, "task")?,
                            sink,
                            address: create_session.address.clone(),
                            enabled: object
                                .get("enabled")
                                .and_then(Value::as_bool)
                                .unwrap_or(true),
                            checker,
                        },
                    )
                },
            ));

            let runtime = self.clone();
            let update_session = session.clone();
            tools.push(Tool::new(
                "update_cron_task",
                "Update a cron task. Use enabled to pause or resume it. Set clear_checker=true to remove the checker.",
                json!({
                    "type": "object",
                    "properties": {
                        "id": {"type": "string"},
                        "name": {"type": "string"},
                        "description": {"type": "string"},
                        "schedule": {"type": "string"},
                        "task": {"type": "string"},
                        "model": {"type": "string"},
                        "enabled": {"type": "boolean"},
                        "sink": {"type": "object"},
                        "checker_command": {"type": "string"},
                        "checker_timeout_seconds": {"type": "number"},
                        "checker_cwd": {"type": "string"},
                        "clear_checker": {"type": "boolean"}
                    },
                    "required": ["id"],
                    "additionalProperties": false
                }),
                move |arguments| {
                    let object = arguments
                        .as_object()
                        .ok_or_else(|| anyhow!("tool arguments must be an object"))?;
                    let id = parse_uuid_arg(object, "id")?;
                    let checker = if object
                        .get("clear_checker")
                        .and_then(Value::as_bool)
                        .unwrap_or(false)
                    {
                        Some(None)
                    } else if object.contains_key("checker_command")
                        || object.contains_key("checker_timeout_seconds")
                        || object.contains_key("checker_cwd")
                    {
                        Some(parse_checker_from_tool_args(object)?)
                    } else {
                        None
                    };
                    let sink = match object.get("sink") {
                        Some(value) => Some(normalize_sink_target(
                            parse_sink_target(value, Some(SinkTarget::Direct(update_session.address.clone())))?,
                            &update_session,
                        )),
                        None => None,
                    };
                    runtime.update_cron_task(
                        id,
                        CronUpdateRequest {
                            name: optional_string_arg(object, "name")?,
                            description: optional_string_arg(object, "description")?,
                            schedule: optional_string_arg(object, "schedule")?,
                            model_key: optional_string_arg(object, "model")?,
                            prompt: optional_string_arg(object, "task")?,
                            sink,
                            enabled: object.get("enabled").and_then(Value::as_bool),
                            checker,
                        },
                    )
                },
            ));

            let runtime = self.clone();
            tools.push(Tool::new(
                "remove_cron_task",
                "Remove a cron task permanently.",
                json!({
                    "type": "object",
                    "properties": {
                        "id": {"type": "string"}
                    },
                    "required": ["id"],
                    "additionalProperties": false
                }),
                move |arguments| {
                    let object = arguments
                        .as_object()
                        .ok_or_else(|| anyhow!("tool arguments must be an object"))?;
                    let id = parse_uuid_arg(object, "id")?;
                    runtime.remove_cron_task(id)
                },
            ));

            let runtime = self.clone();
            tools.push(Tool::new(
                "background_agents_list",
                "List tracked background agents with status, model, and token usage statistics.",
                json!({
                    "type": "object",
                    "properties": {},
                    "additionalProperties": false
                }),
                move |_| runtime.list_managed_agents(ManagedAgentKind::Background),
            ));

            let runtime = self.clone();
            let session = session.clone();
            tools.push(Tool::new(
                "subagent_list",
                "List session subagents.",
                json!({
                    "type": "object",
                    "properties": {},
                    "additionalProperties": false
                }),
                move |_| runtime.subagent_list(&session),
            ));

            let runtime = self.clone();
            tools.push(Tool::new(
                "get_agent_stats",
                "Get detailed status and token usage statistics for a tracked background agent or subagent by agent_id.",
                json!({
                    "type": "object",
                    "properties": {
                        "agent_id": {"type": "string"}
                    },
                    "required": ["agent_id"],
                    "additionalProperties": false
                }),
                move |arguments| {
                    let object = arguments
                        .as_object()
                        .ok_or_else(|| anyhow!("tool arguments must be an object"))?;
                    let agent_id = parse_uuid_arg(object, "agent_id")?;
                    runtime.get_managed_agent(agent_id)
                },
            ));
        }

        tools
    }

    fn try_acquire_subagent_slot(&self) -> Result<SubAgentSlot> {
        loop {
            let current = self.subagent_count.load(Ordering::SeqCst);
            if current >= self.max_global_sub_agents {
                return Err(anyhow!(
                    "global subagent limit reached ({})",
                    self.max_global_sub_agents
                ));
            }
            if self
                .subagent_count
                .compare_exchange(current, current + 1, Ordering::SeqCst, Ordering::SeqCst)
                .is_ok()
            {
                return Ok(SubAgentSlot {
                    counter: Arc::clone(&self.subagent_count),
                });
            }
        }
    }

    fn run_agent_turn_sync(
        &self,
        session: SessionSnapshot,
        kind: AgentPromptKind,
        agent_id: uuid::Uuid,
        model_key: String,
        previous_messages: Vec<ChatMessage>,
        prompt: String,
        upstream_timeout_seconds: Option<f64>,
        execution_control: Option<SessionExecutionControl>,
    ) -> Result<agent_frame::SessionRunReport> {
        let workspace_root = session.workspace_root.clone();
        let _agent_tmp_dir = self.ensure_agent_tmp_dir(agent_id)?;
        if matches!(self.sandbox.mode, crate::config::SandboxMode::Bubblewrap) {
            self.workspace_manager
                .cleanup_transient_mounts(&session.workspace_id)?;
            let _ = self
                .workspace_manager
                .prepare_bubblewrap_view(&session.workspace_id)?;
        }
        let config = self.build_agent_frame_config(
            &session,
            &workspace_root,
            kind,
            &model_key,
            upstream_timeout_seconds,
        )?;
        std::fs::create_dir_all(&config.runtime_state_root).with_context(|| {
            format!(
                "failed to create runtime state root {}",
                config.runtime_state_root.display()
            )
        })?;
        let backend = self.model_config(&model_key)?.backend;
        let extra_tools =
            self.build_extra_tools(&session, kind, agent_id, execution_control.clone());
        if matches!(self.sandbox.mode, crate::config::SandboxMode::Disabled) {
            run_backend_session(
                backend,
                previous_messages,
                prompt,
                config,
                extra_tools,
                execution_control,
            )
        } else {
            let result = run_turn_in_child_process(
                &self.sandbox,
                backend,
                previous_messages,
                prompt,
                config,
                self.agent_workspace.rundir.join("skill_memory"),
                self.agent_workspace.skills_dir.clone(),
                extra_tools,
                execution_control,
            );
            if matches!(self.sandbox.mode, crate::config::SandboxMode::Bubblewrap) {
                let _ = self
                    .workspace_manager
                    .cleanup_transient_mounts(&session.workspace_id);
            }
            result
        }
    }

    async fn run_agent_turn_with_timeout(
        &self,
        session: SessionSnapshot,
        kind: AgentPromptKind,
        agent_id: uuid::Uuid,
        model_key: String,
        previous_messages: Vec<ChatMessage>,
        prompt: String,
        timeout_seconds: Option<f64>,
        upstream_timeout_seconds: Option<f64>,
        control_observer: Option<Arc<dyn Fn(SessionExecutionControl) + Send + Sync>>,
        timeout_label: &str,
        join_label: &str,
    ) -> Result<TimedRunOutcome> {
        enum DriverEvent {
            Checkpoint(SessionRunReport),
            Runtime(SessionEvent),
            Completed(Result<SessionRunReport>),
            SoftDeadline,
            HardDeadline,
        }

        let runtime = self.clone();
        let timeout_label = timeout_label.to_string();
        let join_label = join_label.to_string();
        let event_session = session.clone();
        let event_model_key = model_key.clone();
        let (checkpoint_sender, mut checkpoint_receiver) = mpsc::unbounded_channel();
        let (event_sender, mut event_receiver) = mpsc::unbounded_channel();
        let execution_control = SessionExecutionControl::with_checkpoint_callback(move |report| {
            let _ = checkpoint_sender.send(report);
        })
        .with_event_callback(move |event| {
            let _ = event_sender.send(event);
        });
        if let Some(observer) = control_observer {
            observer(execution_control.clone());
        }
        let cancellation_handle = execution_control.clone();
        let worker_session = session;
        let worker_model_key = model_key;
        let join_handle = tokio::task::spawn_blocking(move || {
            runtime.run_agent_turn_sync(
                worker_session,
                kind,
                agent_id,
                worker_model_key,
                previous_messages,
                prompt,
                upstream_timeout_seconds,
                Some(execution_control),
            )
        });
        let (driver_sender, mut driver_receiver) = mpsc::unbounded_channel();
        let mut relay_tasks = Vec::new();
        {
            let driver_sender = driver_sender.clone();
            relay_tasks.push(tokio::spawn(async move {
                while let Some(checkpoint) = checkpoint_receiver.recv().await {
                    let _ = driver_sender.send(DriverEvent::Checkpoint(checkpoint));
                }
            }));
        }
        {
            let driver_sender = driver_sender.clone();
            relay_tasks.push(tokio::spawn(async move {
                while let Some(event) = event_receiver.recv().await {
                    let _ = driver_sender.send(DriverEvent::Runtime(event));
                }
            }));
        }
        {
            let driver_sender = driver_sender.clone();
            relay_tasks.push(tokio::spawn(async move {
                let result = join_handle
                    .await
                    .context(join_label)
                    .and_then(|report| report.context("agent turn failed"));
                let _ = driver_sender.send(DriverEvent::Completed(result));
            }));
        }
        let soft_deadline = timeout_seconds
            .map(|seconds| tokio::time::Instant::now() + Duration::from_secs_f64(seconds));
        let hard_deadline = timeout_seconds.map(|seconds| {
            tokio::time::Instant::now()
                + Duration::from_secs_f64(seconds + tool_phase_timeout_grace_seconds())
        });
        if let Some(deadline) = soft_deadline {
            let driver_sender = driver_sender.clone();
            relay_tasks.push(tokio::spawn(async move {
                tokio::time::sleep_until(deadline).await;
                let _ = driver_sender.send(DriverEvent::SoftDeadline);
            }));
        }
        if let Some(deadline) = hard_deadline {
            let driver_sender = driver_sender.clone();
            relay_tasks.push(tokio::spawn(async move {
                tokio::time::sleep_until(deadline).await;
                let _ = driver_sender.send(DriverEvent::HardDeadline);
            }));
        }
        drop(driver_sender);
        let mut latest_checkpoint = None;
        let mut soft_timeout_error = None;
        while let Some(driver_event) = driver_receiver.recv().await {
            match driver_event {
                DriverEvent::Checkpoint(checkpoint) => latest_checkpoint = Some(checkpoint),
                DriverEvent::Runtime(event) => {
                    log_agent_frame_event(agent_id, &event_session, kind, &event_model_key, &event);
                }
                DriverEvent::Completed(result) => {
                    for task in relay_tasks {
                        task.abort();
                    }
                    let report = match result {
                        Ok(report) => report,
                        Err(error) => {
                            return Ok(TimedRunOutcome::Failed {
                                checkpoint: latest_checkpoint,
                                error,
                            });
                        }
                    };
                    if report.yielded {
                        return Ok(TimedRunOutcome::Yielded(report));
                    }
                    if let Some(error) = soft_timeout_error {
                        return Ok(TimedRunOutcome::TimedOut {
                            checkpoint: Some(report),
                            error,
                        });
                    }
                    return Ok(TimedRunOutcome::Completed(report));
                }
                DriverEvent::SoftDeadline => {
                    if soft_timeout_error.is_none() {
                        let timeout_seconds = timeout_seconds.expect("soft deadline exists");
                        soft_timeout_error = Some(anyhow!(
                            "{} timed out after {:.1} seconds",
                            timeout_label,
                            timeout_seconds
                        ));
                        cancellation_handle.request_timeout_observation();
                    }
                }
                DriverEvent::HardDeadline => {
                    let timeout_seconds = timeout_seconds.expect("hard deadline exists");
                    cancellation_handle.request_cancel();
                    for task in relay_tasks {
                        task.abort();
                    }
                    return Ok(TimedRunOutcome::TimedOut {
                        checkpoint: latest_checkpoint,
                        error: anyhow!(
                            "{} hard timed out after {:.1} seconds",
                            timeout_label,
                            timeout_seconds + tool_phase_timeout_grace_seconds()
                        ),
                    });
                }
            }
        }
        Err(anyhow!("agent turn driver channel closed unexpectedly"))
    }

    fn run_agent_turn_with_timeout_blocking(
        &self,
        session: SessionSnapshot,
        kind: AgentPromptKind,
        agent_id: uuid::Uuid,
        model_key: String,
        previous_messages: Vec<ChatMessage>,
        prompt: String,
        timeout_seconds: Option<f64>,
        upstream_timeout_seconds: Option<f64>,
        control_observer: Option<Arc<dyn Fn(SessionExecutionControl) + Send + Sync>>,
        timeout_label: &str,
    ) -> Result<TimedRunOutcome> {
        enum DriverEvent {
            Checkpoint(SessionRunReport),
            Runtime(SessionEvent),
            Completed(Result<SessionRunReport>),
            SoftDeadline,
            HardDeadline,
        }

        let event_session = session.clone();
        let event_model_key = model_key.clone();
        let (checkpoint_sender, checkpoint_receiver) = std::sync::mpsc::channel();
        let (event_sender, event_receiver) = std::sync::mpsc::channel();
        let execution_control = SessionExecutionControl::with_checkpoint_callback(move |report| {
            let _ = checkpoint_sender.send(report);
        })
        .with_event_callback(move |event| {
            let _ = event_sender.send(event);
        });
        if let Some(observer) = control_observer {
            observer(execution_control.clone());
        }
        let cancellation_handle = execution_control.clone();
        let runtime = self.clone();
        let timeout_label = timeout_label.to_string();
        let worker_session = session;
        let worker_model_key = model_key;
        let handle = std::thread::spawn(move || {
            runtime.run_agent_turn_sync(
                worker_session,
                kind,
                agent_id,
                worker_model_key,
                previous_messages,
                prompt,
                upstream_timeout_seconds,
                Some(execution_control),
            )
        });
        let (driver_sender, driver_receiver) = std::sync::mpsc::channel();
        {
            let driver_sender = driver_sender.clone();
            std::thread::spawn(move || {
                while let Ok(report) = checkpoint_receiver.recv() {
                    if driver_sender.send(DriverEvent::Checkpoint(report)).is_err() {
                        break;
                    }
                }
            });
        }
        {
            let driver_sender = driver_sender.clone();
            std::thread::spawn(move || {
                while let Ok(event) = event_receiver.recv() {
                    if driver_sender.send(DriverEvent::Runtime(event)).is_err() {
                        break;
                    }
                }
            });
        }
        {
            let driver_sender = driver_sender.clone();
            std::thread::spawn(move || {
                let result = handle
                    .join()
                    .map_err(|_| anyhow!("agent worker thread panicked"))
                    .and_then(|report| report.context("agent turn failed"));
                let _ = driver_sender.send(DriverEvent::Completed(result));
            });
        }
        let soft_deadline = timeout_seconds
            .map(|seconds| std::time::Instant::now() + Duration::from_secs_f64(seconds));
        let hard_deadline = timeout_seconds.map(|seconds| {
            std::time::Instant::now()
                + Duration::from_secs_f64(seconds + tool_phase_timeout_grace_seconds())
        });
        if let Some(deadline) = soft_deadline {
            let driver_sender = driver_sender.clone();
            std::thread::spawn(move || {
                let now = std::time::Instant::now();
                if deadline > now {
                    std::thread::sleep(deadline.duration_since(now));
                }
                let _ = driver_sender.send(DriverEvent::SoftDeadline);
            });
        }
        if let Some(deadline) = hard_deadline {
            let driver_sender = driver_sender.clone();
            std::thread::spawn(move || {
                let now = std::time::Instant::now();
                if deadline > now {
                    std::thread::sleep(deadline.duration_since(now));
                }
                let _ = driver_sender.send(DriverEvent::HardDeadline);
            });
        }
        drop(driver_sender);
        let mut latest_checkpoint = None;
        let mut soft_timeout_error = None;
        while let Ok(driver_event) = driver_receiver.recv() {
            match driver_event {
                DriverEvent::Checkpoint(report) => latest_checkpoint = Some(report),
                DriverEvent::Runtime(event) => {
                    log_agent_frame_event(agent_id, &event_session, kind, &event_model_key, &event)
                }
                DriverEvent::Completed(result) => {
                    let report = match result {
                        Ok(report) => report,
                        Err(error) => {
                            return Ok(TimedRunOutcome::Failed {
                                checkpoint: latest_checkpoint,
                                error,
                            });
                        }
                    };
                    if report.yielded {
                        return Ok(TimedRunOutcome::Yielded(report));
                    }
                    if let Some(error) = soft_timeout_error {
                        return Ok(TimedRunOutcome::TimedOut {
                            checkpoint: Some(report),
                            error,
                        });
                    }
                    return Ok(TimedRunOutcome::Completed(report));
                }
                DriverEvent::SoftDeadline => {
                    if soft_timeout_error.is_none() {
                        let timeout_seconds = timeout_seconds.expect("soft deadline exists");
                        soft_timeout_error = Some(anyhow!(
                            "{} timed out after {:.1} seconds",
                            timeout_label,
                            timeout_seconds
                        ));
                        cancellation_handle.request_timeout_observation();
                    }
                }
                DriverEvent::HardDeadline => {
                    let timeout_seconds = timeout_seconds.expect("hard deadline exists");
                    cancellation_handle.request_cancel();
                    return Ok(TimedRunOutcome::TimedOut {
                        checkpoint: latest_checkpoint,
                        error: anyhow!(
                            "{} hard timed out after {:.1} seconds",
                            timeout_label,
                            timeout_seconds + tool_phase_timeout_grace_seconds()
                        ),
                    });
                }
            }
        }
        Err(anyhow!("agent turn driver channel closed unexpectedly"))
    }

    fn run_subagent_worker(&self, subagent: Arc<HostedSubagent>) -> Result<()> {
        let _slot = self.try_acquire_subagent_slot()?;
        loop {
            let (model_key, previous_messages, prompt, timeout_seconds) = {
                let mut inner = subagent
                    .inner
                    .lock()
                    .map_err(|_| anyhow!("subagent state lock poisoned"))?;
                loop {
                    match inner.persisted.state {
                        SubagentState::Destroyed | SubagentState::Failed => {
                            Self::persist_subagent_locked(&subagent, &inner)?;
                            return Ok(());
                        }
                        SubagentState::Running => break,
                        SubagentState::WaitingForCharge => {
                            if inner.persisted.available_charge_seconds > 0.0 {
                                inner.persisted.state = SubagentState::Running;
                                break;
                            }
                        }
                        SubagentState::Ready => {
                            if inner.persisted.resume_pending
                                && inner.persisted.available_charge_seconds > 0.0
                            {
                                inner.persisted.state = SubagentState::Running;
                                break;
                            }
                            if !inner.queued_prompts.is_empty()
                                && inner.persisted.available_charge_seconds > 0.0
                            {
                                inner.persisted.state = SubagentState::Running;
                                break;
                            }
                        }
                    }
                    inner = subagent
                        .condvar
                        .wait(inner)
                        .map_err(|_| anyhow!("subagent state lock poisoned"))?;
                }

                let prompt = if inner.persisted.resume_pending {
                    String::new()
                } else {
                    inner.queued_prompts.pop_front().unwrap_or_default()
                };
                inner.persisted.pending_prompts = inner.queued_prompts.iter().cloned().collect();
                let timeout_seconds = inner.persisted.available_charge_seconds;
                inner.persisted.available_charge_seconds = 0.0;
                inner.persisted.updated_at = Utc::now();
                Self::persist_subagent_locked(&subagent, &inner)?;
                (
                    inner.persisted.model_key.clone(),
                    inner.persisted.messages.clone(),
                    prompt,
                    timeout_seconds,
                )
            };

            self.mark_managed_agent_running(subagent.id);
            let runtime = self.clone();
            let subagent_for_observer = Arc::clone(&subagent);
            let control_observer: Arc<dyn Fn(SessionExecutionControl) + Send + Sync> =
                Arc::new(move |control| {
                    if let Ok(mut inner) = subagent_for_observer.inner.lock() {
                        inner.active_control = Some(control);
                    }
                });
            let session = SessionSnapshot {
                id: subagent.session_id,
                agent_id: subagent.id,
                address: subagent.address.clone(),
                root_dir: self.agent_workspace.root_dir.join("subagent-sessions"),
                attachments_dir: subagent.workspace_root.join("upload"),
                workspace_id: subagent.workspace_id.clone(),
                workspace_root: subagent.workspace_root.clone(),
                message_count: 0,
                agent_message_count: previous_messages.len(),
                agent_messages: previous_messages.clone(),
                last_agent_returned_at: None,
                last_compacted_at: None,
                turn_count: 0,
                last_compacted_turn_count: 0,
                cumulative_usage: TokenUsage::default(),
                cumulative_compaction: SessionCompactionStats::default(),
                api_timeout_override_seconds: None,
                skill_states: HashMap::new(),
                pending_continue: None,
                pending_workspace_summary: false,
                close_after_summary: false,
            };
            let upstream_timeout_seconds = self.model_upstream_timeout_seconds(&model_key)?;
            let outcome = runtime.run_agent_turn_with_timeout_blocking(
                session.clone(),
                AgentPromptKind::SubAgent,
                subagent.id,
                model_key.clone(),
                previous_messages,
                prompt,
                Some(timeout_seconds),
                Some(upstream_timeout_seconds),
                Some(control_observer),
                "subagent",
            )?;
            let mut inner = subagent
                .inner
                .lock()
                .map_err(|_| anyhow!("subagent state lock poisoned"))?;
            inner.active_control = None;
            match outcome {
                TimedRunOutcome::Completed(report) | TimedRunOutcome::Yielded(report) => {
                    inner.persisted.messages = report.messages;
                    inner.persisted.resume_pending = false;
                    inner.persisted.state = SubagentState::Ready;
                    inner.persisted.updated_at = Utc::now();
                    inner.persisted.last_returned_at = Some(Utc::now());
                    inner.persisted.turn_count = inner.persisted.turn_count.saturating_add(1);
                    inner.persisted.cumulative_usage.add_assign(&report.usage);
                    inner.persisted.cumulative_compaction.run_count = inner
                        .persisted
                        .cumulative_compaction
                        .run_count
                        .saturating_add(report.compaction.run_count);
                    inner.persisted.cumulative_compaction.compacted_run_count = inner
                        .persisted
                        .cumulative_compaction
                        .compacted_run_count
                        .saturating_add(report.compaction.compacted_run_count);
                    inner
                        .persisted
                        .cumulative_compaction
                        .estimated_tokens_before = inner
                        .persisted
                        .cumulative_compaction
                        .estimated_tokens_before
                        .saturating_add(report.compaction.estimated_tokens_before);
                    inner.persisted.cumulative_compaction.estimated_tokens_after = inner
                        .persisted
                        .cumulative_compaction
                        .estimated_tokens_after
                        .saturating_add(report.compaction.estimated_tokens_after);
                    inner
                        .persisted
                        .cumulative_compaction
                        .usage
                        .add_assign(&report.compaction.usage);
                    let assistant_text = extract_assistant_text(&inner.persisted.messages);
                    let (clean_text, attachments) =
                        extract_attachment_references(&assistant_text, &subagent.workspace_root)?;
                    inner.persisted.last_result_text = Some(clean_text);
                    inner.persisted.last_attachment_paths = attachments
                        .iter()
                        .map(|item| relative_attachment_path(&subagent.workspace_root, &item.path))
                        .collect::<Result<Vec<_>>>()?;
                    inner.persisted.last_error = None;
                    self.mark_managed_agent_completed(subagent.id, &report.usage);
                    log_turn_usage(
                        subagent.id,
                        &session,
                        &report.usage,
                        false,
                        "subagent",
                        Some(inner.persisted.parent_agent_id),
                    );
                }
                TimedRunOutcome::TimedOut { checkpoint, error } => {
                    if let Some(report) = checkpoint {
                        inner.persisted.messages = report.messages;
                        inner.persisted.cumulative_usage.add_assign(&report.usage);
                        inner.persisted.cumulative_compaction.run_count = inner
                            .persisted
                            .cumulative_compaction
                            .run_count
                            .saturating_add(report.compaction.run_count);
                        inner.persisted.cumulative_compaction.compacted_run_count = inner
                            .persisted
                            .cumulative_compaction
                            .compacted_run_count
                            .saturating_add(report.compaction.compacted_run_count);
                        inner
                            .persisted
                            .cumulative_compaction
                            .estimated_tokens_before = inner
                            .persisted
                            .cumulative_compaction
                            .estimated_tokens_before
                            .saturating_add(report.compaction.estimated_tokens_before);
                        inner.persisted.cumulative_compaction.estimated_tokens_after = inner
                            .persisted
                            .cumulative_compaction
                            .estimated_tokens_after
                            .saturating_add(report.compaction.estimated_tokens_after);
                        inner
                            .persisted
                            .cumulative_compaction
                            .usage
                            .add_assign(&report.compaction.usage);
                    }
                    inner.persisted.resume_pending = true;
                    inner.persisted.state = SubagentState::WaitingForCharge;
                    inner.persisted.updated_at = Utc::now();
                    inner.persisted.last_returned_at = Some(Utc::now());
                    inner.persisted.last_error = Some(format!("{error:#}"));
                    self.mark_managed_agent_timed_out(
                        subagent.id,
                        &inner.persisted.cumulative_usage,
                        &error,
                    );
                }
                TimedRunOutcome::Failed { checkpoint, error } => {
                    if let Some(report) = checkpoint {
                        inner.persisted.messages = report.messages;
                        inner.persisted.cumulative_usage.add_assign(&report.usage);
                    }
                    inner.persisted.resume_pending = false;
                    inner.persisted.state = SubagentState::Failed;
                    inner.persisted.updated_at = Utc::now();
                    inner.persisted.last_error = Some(format!("{error:#}"));
                    self.mark_managed_agent_failed(
                        subagent.id,
                        &inner.persisted.cumulative_usage,
                        &error,
                    );
                }
            }
            Self::persist_subagent_locked(&subagent, &inner)?;
            drop(inner);
            subagent.condvar.notify_all();
        }
    }

    fn start_background_agent(
        &self,
        parent_agent_id: uuid::Uuid,
        session: SessionSnapshot,
        model_key: Option<String>,
        prompt: String,
        sink: SinkTarget,
    ) -> Result<Value> {
        let background_agent_id = uuid::Uuid::new_v4();
        let model_key = match model_key {
            Some(model_key) => model_key,
            None => self.effective_main_model_key()?,
        };
        self.model_config(&model_key)?;
        self.register_managed_agent(
            background_agent_id,
            ManagedAgentKind::Background,
            model_key.clone(),
            Some(parent_agent_id),
            &session,
            ManagedAgentState::Enqueued,
        );
        self.background_job_sender
            .blocking_send(BackgroundJobRequest {
                agent_id: background_agent_id,
                parent_agent_id: Some(parent_agent_id),
                cron_task_id: None,
                session: session.clone(),
                model_key: model_key.clone(),
                prompt,
                sink: sink.clone(),
            })
            .context("failed to enqueue background agent")?;
        info!(
            log_stream = "agent",
            log_key = %background_agent_id,
            kind = "background_agent_enqueued",
            parent_agent_id = %parent_agent_id,
            session_id = %session.id,
            channel_id = %session.address.channel_id,
            model = %model_key,
            "background agent enqueued"
        );
        Ok(json!({
            "agent_id": background_agent_id,
            "parent_agent_id": parent_agent_id,
            "model": model_key,
            "sink": sink_target_to_value(&sink)
        }))
    }

    async fn run_background_job(&self, job: BackgroundJobRequest) -> Result<()> {
        self.mark_managed_agent_running(job.agent_id);
        info!(
            log_stream = "agent",
            log_key = %job.agent_id,
            kind = "background_agent_started",
            parent_agent_id = job.parent_agent_id.map(|value| value.to_string()),
            cron_task_id = job.cron_task_id.map(|value| value.to_string()),
            session_id = %job.session.id,
            channel_id = %job.session.address.channel_id,
            model = %job.model_key,
            "background agent started"
        );
        let timeout_seconds = self.main_agent_timeout_seconds(&job.model_key)?;
        let upstream_timeout_seconds = self.model_upstream_timeout_seconds(&job.model_key)?;
        let run_result = self
            .run_agent_turn_with_timeout(
                job.session.clone(),
                AgentPromptKind::MainBackground,
                job.agent_id,
                job.model_key.clone(),
                Vec::new(),
                job.prompt.clone(),
                timeout_seconds,
                Some(upstream_timeout_seconds),
                None,
                "background agent",
                "background agent task join failed",
            )
            .await;

        let outcome = match run_result {
            Ok(TimedRunOutcome::Completed(report)) => {
                let assistant_text = extract_assistant_text(&report.messages);
                let outgoing = build_outgoing_message_for_session(
                    &job.session,
                    &assistant_text,
                    &job.session.workspace_root,
                )?;
                log_turn_usage(
                    job.agent_id,
                    &job.session,
                    &report.usage,
                    false,
                    "main_background",
                    job.parent_agent_id,
                );
                info!(
                    log_stream = "agent",
                    log_key = %job.agent_id,
                    kind = "background_agent_replied",
                    parent_agent_id = job.parent_agent_id.map(|value| value.to_string()),
                    cron_task_id = job.cron_task_id.map(|value| value.to_string()),
                    session_id = %job.session.id,
                    channel_id = %job.session.address.channel_id,
                    has_text = outgoing.text.as_deref().is_some_and(|text| !text.trim().is_empty()),
                    attachment_count = outgoing.attachments.len() as u64 + outgoing.images.len() as u64,
                    "background agent produced reply"
                );
                let sink_router = self.sink_router.read().await;
                if let Err(error) = sink_router
                    .dispatch(&self.channels, &job.sink, outgoing)
                    .await
                    .context("failed to dispatch background agent reply")
                {
                    self.mark_managed_agent_failed(job.agent_id, &report.usage, &error);
                    let recovery = self
                        .handle_background_job_failure(&job, &error)
                        .await
                        .with_context(|| format!("{error:#}"));
                    cleanup_detached_session_root(self, &job).ok();
                    return recovery;
                }
                self.mark_managed_agent_completed(job.agent_id, &report.usage);
                Ok(())
            }
            Ok(TimedRunOutcome::Yielded(report)) => {
                let assistant_text = extract_assistant_text(&report.messages);
                let outgoing = build_outgoing_message_for_session(
                    &job.session,
                    &assistant_text,
                    &job.session.workspace_root,
                )?;
                log_turn_usage(
                    job.agent_id,
                    &job.session,
                    &report.usage,
                    false,
                    "main_background",
                    job.parent_agent_id,
                );
                let sink_router = self.sink_router.read().await;
                sink_router
                    .dispatch(&self.channels, &job.sink, outgoing)
                    .await
                    .context("failed to dispatch yielded background agent reply")?;
                self.mark_managed_agent_completed(job.agent_id, &report.usage);
                Ok(())
            }
            Ok(TimedRunOutcome::TimedOut { checkpoint, error }) => {
                let usage = checkpoint
                    .as_ref()
                    .map(|report| report.usage.clone())
                    .unwrap_or_default();
                self.mark_managed_agent_timed_out(job.agent_id, &usage, &error);
                self.handle_background_job_failure(&job, &error).await
            }
            Ok(TimedRunOutcome::Failed { checkpoint, error }) => {
                let usage = checkpoint
                    .as_ref()
                    .map(|report| report.usage.clone())
                    .unwrap_or_default();
                self.mark_managed_agent_failed(job.agent_id, &usage, &error);
                self.handle_background_job_failure(&job, &error).await
            }
            Err(error) => {
                self.mark_managed_agent_failed(job.agent_id, &TokenUsage::default(), &error);
                self.handle_background_job_failure(&job, &error).await
            }
        };
        cleanup_detached_session_root(self, &job).ok();
        outcome
    }

    async fn handle_background_job_failure(
        &self,
        job: &BackgroundJobRequest,
        error: &anyhow::Error,
    ) -> Result<()> {
        if error
            .to_string()
            .contains("failed to dispatch background agent reply")
            || error
                .to_string()
                .contains("background agent error dispatch failed")
        {
            return Err(anyhow!(
                "background job failed and frontend dispatch is unavailable"
            ));
        }

        if is_timeout_like(error) && self.has_active_child_agents(job.agent_id) {
            warn!(
                log_stream = "agent",
                log_key = %job.agent_id,
                kind = "background_agent_recovery_skipped_active_children",
                session_id = %job.session.id,
                channel_id = %job.session.address.channel_id,
                "background agent timed out while child agents were still active; skipping automatic recovery"
            );
            self.wait_for_child_agents_to_finish(job.agent_id).await;
            let text = background_timeout_with_active_children_text(&self.main_agent.language);
            let sink_router = self.sink_router.read().await;
            sink_router
                .dispatch(&self.channels, &job.sink, OutgoingMessage::text(text))
                .await
                .context("failed to dispatch background timeout notification")?;
            return Ok(());
        }

        let recovery_agent_id = uuid::Uuid::new_v4();
        self.register_managed_agent(
            recovery_agent_id,
            ManagedAgentKind::Background,
            job.model_key.clone(),
            Some(job.agent_id),
            &job.session,
            ManagedAgentState::Running,
        );
        info!(
            log_stream = "agent",
            log_key = %recovery_agent_id,
            kind = "background_agent_recovery_started",
            failed_agent_id = %job.agent_id,
            parent_agent_id = job.parent_agent_id.map(|value| value.to_string()),
            session_id = %job.session.id,
            channel_id = %job.session.address.channel_id,
            model = %job.model_key,
            "background failure recovery agent started"
        );

        let recovery_timeout = background_recovery_timeout_seconds(
            self.main_agent_timeout_seconds(&job.model_key)?
                .unwrap_or_else(|| {
                    background_agent_timeout_seconds(
                        self.models
                            .get(&job.model_key)
                            .map(|model| model.timeout_seconds)
                            .unwrap_or(120.0),
                    )
                }),
            error,
        );
        let upstream_timeout_seconds = self.model_upstream_timeout_seconds(&job.model_key)?;
        let recovery_prompt = format!(
            "A previous main background agent failed before completing its work.\n\nOriginal task:\n{}\n\nFailure:\n{}\n\nYour job now:\n1. Diagnose the failure.\n2. If it is recoverable without user intervention, continue or retry the original task yourself now and produce the final user-facing result. Do not mention the failure unless it is relevant.\n3. If it is not recoverable, produce a concise user-facing explanation of the problem and what the user should do next.\n4. Do not say that you will continue later. Either complete the work now or explain the blocker clearly.",
            job.prompt, error
        );
        let run_result = self
            .run_agent_turn_with_timeout(
                job.session.clone(),
                AgentPromptKind::MainBackground,
                recovery_agent_id,
                job.model_key.clone(),
                Vec::new(),
                recovery_prompt,
                Some(recovery_timeout),
                Some(upstream_timeout_seconds),
                None,
                "background failure recovery",
                "background failure recovery task join failed",
            )
            .await;

        match run_result {
            Ok(TimedRunOutcome::Completed(report)) => {
                let assistant_text = extract_assistant_text(&report.messages);
                let outgoing = build_outgoing_message_for_session(
                    &job.session,
                    &assistant_text,
                    &job.session.workspace_root,
                )?;
                let sink_router = self.sink_router.read().await;
                sink_router
                    .dispatch(&self.channels, &job.sink, outgoing)
                    .await
                    .context("failed to dispatch recovered background agent reply")?;
                self.mark_managed_agent_completed(recovery_agent_id, &report.usage);
                info!(
                    log_stream = "agent",
                    log_key = %recovery_agent_id,
                    kind = "background_agent_recovery_completed",
                    failed_agent_id = %job.agent_id,
                    "background failure recovery agent completed"
                );
                Ok(())
            }
            Ok(TimedRunOutcome::Yielded(report)) => {
                let assistant_text = extract_assistant_text(&report.messages);
                let outgoing = build_outgoing_message_for_session(
                    &job.session,
                    &assistant_text,
                    &job.session.workspace_root,
                )?;
                let sink_router = self.sink_router.read().await;
                sink_router
                    .dispatch(&self.channels, &job.sink, outgoing)
                    .await
                    .context("failed to dispatch yielded recovered background agent reply")?;
                self.mark_managed_agent_completed(recovery_agent_id, &report.usage);
                Ok(())
            }
            Ok(TimedRunOutcome::TimedOut {
                checkpoint,
                error: recovery_error,
            }) => {
                let usage = checkpoint
                    .as_ref()
                    .map(|report| report.usage.clone())
                    .unwrap_or_default();
                self.mark_managed_agent_timed_out(recovery_agent_id, &usage, &recovery_error);
                let text = user_facing_error_text(&self.main_agent.language, error);
                let sink_router = self.sink_router.read().await;
                sink_router
                    .dispatch(&self.channels, &job.sink, OutgoingMessage::text(text))
                    .await
                    .context("failed to dispatch background failure notification")?;
                warn!(
                    log_stream = "agent",
                    log_key = %recovery_agent_id,
                    kind = "background_agent_recovery_failed",
                    failed_agent_id = %job.agent_id,
                    error = %format!("{recovery_error:#}"),
                    "background failure recovery agent timed out; user was notified"
                );
                Ok(())
            }
            Ok(TimedRunOutcome::Failed {
                checkpoint,
                error: recovery_error,
            }) => {
                let usage = checkpoint
                    .as_ref()
                    .map(|report| report.usage.clone())
                    .unwrap_or_default();
                self.mark_managed_agent_failed(recovery_agent_id, &usage, &recovery_error);
                let text = user_facing_error_text(&self.main_agent.language, error);
                let sink_router = self.sink_router.read().await;
                sink_router
                    .dispatch(&self.channels, &job.sink, OutgoingMessage::text(text))
                    .await
                    .context("failed to dispatch background failure notification")?;
                warn!(
                    log_stream = "agent",
                    log_key = %recovery_agent_id,
                    kind = "background_agent_recovery_failed",
                    failed_agent_id = %job.agent_id,
                    error = %format!("{recovery_error:#}"),
                    "background failure recovery agent failed; user was notified"
                );
                Ok(())
            }
            Err(recovery_error) => {
                self.mark_managed_agent_failed(
                    recovery_agent_id,
                    &TokenUsage::default(),
                    &recovery_error,
                );
                let text = user_facing_error_text(&self.main_agent.language, error);
                let sink_router = self.sink_router.read().await;
                sink_router
                    .dispatch(&self.channels, &job.sink, OutgoingMessage::text(text))
                    .await
                    .context("failed to dispatch background failure notification")?;
                warn!(
                    log_stream = "agent",
                    log_key = %recovery_agent_id,
                    kind = "background_agent_recovery_failed",
                    failed_agent_id = %job.agent_id,
                    error = %format!("{recovery_error:#}"),
                    "background failure recovery agent failed; user was notified"
                );
                Ok(())
            }
        }
    }

    fn list_cron_tasks(&self) -> Result<Value> {
        let manager = self
            .cron_manager
            .lock()
            .map_err(|_| anyhow!("cron manager lock poisoned"))?;
        Ok(serde_json::to_value(manager.list()?).context("failed to serialize cron task list")?)
    }

    fn get_cron_task(&self, id: uuid::Uuid) -> Result<Value> {
        let manager = self
            .cron_manager
            .lock()
            .map_err(|_| anyhow!("cron manager lock poisoned"))?;
        Ok(serde_json::to_value(manager.get(id)?).context("failed to serialize cron task")?)
    }

    fn create_cron_task(
        &self,
        session: SessionSnapshot,
        request: CronCreateRequest,
    ) -> Result<Value> {
        self.model_config(&request.model_key)?;
        let mut manager = self
            .cron_manager
            .lock()
            .map_err(|_| anyhow!("cron manager lock poisoned"))?;
        let view = manager.create(request)?;
        info!(
            log_stream = "agent",
            log_key = %session.agent_id,
            kind = "cron_task_created",
            session_id = %session.id,
            channel_id = %session.address.channel_id,
            cron_task_id = %view.id,
            "cron task created"
        );
        Ok(serde_json::to_value(view).context("failed to serialize cron task")?)
    }

    fn update_cron_task(&self, id: uuid::Uuid, request: CronUpdateRequest) -> Result<Value> {
        if let Some(model_key) = request.model_key.as_deref() {
            self.model_config(model_key)?;
        }
        let mut manager = self
            .cron_manager
            .lock()
            .map_err(|_| anyhow!("cron manager lock poisoned"))?;
        let view = manager.update(id, request)?;
        Ok(serde_json::to_value(view).context("failed to serialize cron task")?)
    }

    fn remove_cron_task(&self, id: uuid::Uuid) -> Result<Value> {
        let mut manager = self
            .cron_manager
            .lock()
            .map_err(|_| anyhow!("cron manager lock poisoned"))?;
        let view = manager.remove(id)?;
        Ok(serde_json::to_value(view).context("failed to serialize removed cron task")?)
    }

    async fn poll_cron_once(&self) -> Result<()> {
        let now = Utc::now();
        let due_tasks = {
            let mut manager = self
                .cron_manager
                .lock()
                .map_err(|_| anyhow!("cron manager lock poisoned"))?;
            manager.claim_due_tasks(now)?
        };

        for claimed in due_tasks {
            if let Err(error) = self.handle_claimed_cron_task(claimed).await {
                error!(
                    log_stream = "agent",
                    kind = "cron_task_dispatch_failed",
                    error = %format!("{error:#}"),
                    "failed to dispatch cron task"
                );
            }
        }
        Ok(())
    }

    async fn handle_claimed_cron_task(&self, claimed: ClaimedCronTask) -> Result<()> {
        let task = claimed.task;
        let checked_at = Utc::now();
        let (should_trigger, check_outcome) = match &task.checker {
            Some(checker) => evaluate_cron_checker(checker, &self.agent_workspace.rundir)
                .with_context(|| format!("checker failed for cron task {}", task.id))
                .map(|passed| {
                    if passed {
                        (true, "checker_passed".to_string())
                    } else {
                        (false, "checker_blocked".to_string())
                    }
                })
                .unwrap_or_else(|error| (true, format!("checker_error_triggered: {}", error))),
            None => (true, "no_checker".to_string()),
        };
        {
            let mut manager = self
                .cron_manager
                .lock()
                .map_err(|_| anyhow!("cron manager lock poisoned"))?;
            manager.record_check_result(task.id, checked_at, check_outcome.clone())?;
        }

        info!(
            log_stream = "agent",
            kind = "cron_task_checked",
            cron_task_id = %task.id,
            scheduled_for = %claimed.scheduled_for,
            should_trigger,
            outcome = %check_outcome,
            "cron task checker evaluated"
        );

        if !should_trigger {
            return Ok(());
        }

        self.model_config(&task.model_key)?;
        let background_agent_id = uuid::Uuid::new_v4();
        let session = create_detached_session_snapshot(
            &self.workspace_manager,
            &self.agent_workspace.root_dir,
            task.address.clone(),
            background_agent_id,
        )?;
        self.register_managed_agent(
            background_agent_id,
            ManagedAgentKind::Background,
            task.model_key.clone(),
            None,
            &session,
            ManagedAgentState::Enqueued,
        );
        self.background_job_sender
            .send(BackgroundJobRequest {
                agent_id: background_agent_id,
                parent_agent_id: None,
                cron_task_id: Some(task.id),
                session,
                model_key: task.model_key.clone(),
                prompt: task.prompt.clone(),
                sink: task.sink.clone(),
            })
            .await
            .context("failed to enqueue cron background agent")?;
        {
            let mut manager = self
                .cron_manager
                .lock()
                .map_err(|_| anyhow!("cron manager lock poisoned"))?;
            manager.record_trigger_result(task.id, Utc::now(), "enqueued".to_string())?;
        }
        info!(
            log_stream = "agent",
            kind = "cron_task_enqueued",
            cron_task_id = %task.id,
            background_agent_id = %background_agent_id,
            scheduled_for = %claimed.scheduled_for,
            model = %task.model_key,
            "cron task enqueued background agent"
        );
        Ok(())
    }
}

fn send_outgoing_message_now(
    channel: Arc<dyn Channel>,
    address: ChannelAddress,
    message: OutgoingMessage,
) -> Result<()> {
    let (sender, receiver) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let result = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .context("failed to build temporary Tokio runtime for immediate channel send")
            .and_then(|runtime| {
                runtime
                    .block_on(async move { channel.send(&address, message).await })
                    .context("failed to send immediate channel message")
            })
            .map_err(|error| format!("{error:#}"));
        let _ = sender.send(result);
    });
    match receiver.recv() {
        Ok(Ok(())) => Ok(()),
        Ok(Err(error)) => Err(anyhow!(error)),
        Err(_) => Err(anyhow!("immediate channel send thread closed unexpectedly")),
    }
}

pub struct Server {
    workdir: PathBuf,
    agent_workspace: AgentWorkspace,
    workspace_manager: WorkspaceManager,
    channels: Arc<HashMap<String, Arc<dyn Channel>>>,
    command_catalog: HashMap<String, Vec<BotCommandConfig>>,
    models: BTreeMap<String, ModelConfig>,
    chat_model_keys: Vec<String>,
    main_agent: MainAgentConfig,
    sandbox: SandboxConfig,
    conversations: Arc<Mutex<ConversationManager>>,
    snapshots: Arc<Mutex<SnapshotManager>>,
    sessions: Arc<Mutex<SessionManager>>,
    sink_router: Arc<RwLock<SinkRouter>>,
    cron_manager: Arc<Mutex<CronManager>>,
    agent_registry: Arc<Mutex<AgentRegistry>>,
    agent_registry_notify: Arc<Notify>,
    max_global_sub_agents: usize,
    subagent_count: Arc<AtomicUsize>,
    cron_poll_interval_seconds: u64,
    background_job_sender: mpsc::Sender<BackgroundJobRequest>,
    background_job_receiver: Option<mpsc::Receiver<BackgroundJobRequest>>,
    summary_tracker: Arc<SummaryTracker>,
    active_foreground_controls: Arc<Mutex<HashMap<String, SessionExecutionControl>>>,
    subagents: Arc<Mutex<HashMap<uuid::Uuid, Arc<HostedSubagent>>>>,
}

impl Server {
    fn clear_missing_selected_main_model(
        &self,
        address: &ChannelAddress,
    ) -> Result<Option<String>> {
        let Some(model_key) = self.selected_main_model_key(address)? else {
            return Ok(None);
        };
        if self.models.contains_key(&model_key) {
            return Ok(None);
        }
        self.with_conversations(|conversations| {
            conversations.set_main_model(address, None).map(|_| ())
        })?;
        Ok(Some(model_key))
    }

    async fn prompt_missing_conversation_model(
        &self,
        channel: &Arc<dyn Channel>,
        address: &ChannelAddress,
        missing_model_key: &str,
    ) -> Result<()> {
        self.send_channel_message(
            channel,
            address,
            OutgoingMessage::text(format!(
                "The previously selected model `{}` is no longer available in the current config. Please run `/model` to choose a new model.",
                missing_model_key
            )),
        )
        .await
    }

    fn with_sessions<T>(&self, f: impl FnOnce(&mut SessionManager) -> Result<T>) -> Result<T> {
        let mut sessions = self
            .sessions
            .lock()
            .map_err(|_| anyhow!("session manager lock poisoned"))?;
        f(&mut sessions)
    }

    fn with_conversations<T>(
        &self,
        f: impl FnOnce(&mut ConversationManager) -> Result<T>,
    ) -> Result<T> {
        let mut conversations = self
            .conversations
            .lock()
            .map_err(|_| anyhow!("conversation manager lock poisoned"))?;
        f(&mut conversations)
    }

    fn with_snapshots<T>(&self, f: impl FnOnce(&mut SnapshotManager) -> Result<T>) -> Result<T> {
        let mut snapshots = self
            .snapshots
            .lock()
            .map_err(|_| anyhow!("snapshot manager lock poisoned"))?;
        f(&mut snapshots)
    }

    pub fn from_config(config: ServerConfig, workdir: impl AsRef<Path>) -> Result<Self> {
        let workdir = workdir.as_ref().to_path_buf();
        std::fs::create_dir_all(&workdir)
            .with_context(|| format!("failed to create workdir {}", workdir.display()))?;
        let agent_workspace = AgentWorkspace::initialize(&workdir)?;
        let workspace_manager = WorkspaceManager::load_or_create(&workdir)?;

        let mut channels: HashMap<String, Arc<dyn Channel>> = HashMap::new();
        let mut command_catalog: HashMap<String, Vec<BotCommandConfig>> = HashMap::new();
        for channel_config in config.channels {
            match channel_config {
                ChannelConfig::CommandLine(command_line) => {
                    let id = command_line.id.clone();
                    command_catalog.insert(id.clone(), default_bot_commands());
                    channels.insert(id, Arc::new(CommandLineChannel::new(command_line)));
                }
                ChannelConfig::Telegram(telegram) => {
                    let id = telegram.id.clone();
                    command_catalog.insert(id.clone(), telegram.commands.clone());
                    channels.insert(id, Arc::new(TelegramChannel::from_config(telegram)?));
                }
            }
        }

        info!(
            log_stream = "server",
            kind = "server_initialized",
            workdir = %workdir.display(),
            channel_count = channels.len() as u64,
            identity_path = %agent_workspace.identity_md_path.display(),
            user_profile_path = %agent_workspace.user_md_path.display(),
            agents_md_path = %agent_workspace.agents_md_path.display(),
            skills_dir = %agent_workspace.skills_dir.display(),
            configured_main_model = ?config.main_agent.model,
            sandbox_mode = ?config.sandbox.mode,
            "server initialized"
        );

        let (background_job_sender, background_job_receiver) = mpsc::channel(64);
        let cron_manager = Arc::new(Mutex::new(CronManager::load_or_create(&workdir)?));
        let agent_registry = Arc::new(Mutex::new(AgentRegistry::load_or_create(&workdir)?));
        let agent_registry_notify = Arc::new(Notify::new());

        Ok(Self {
            sessions: Arc::new(Mutex::new(SessionManager::new(
                &workdir,
                workspace_manager.clone(),
            )?)),
            workdir: workdir.clone(),
            agent_workspace,
            workspace_manager,
            channels: Arc::new(channels),
            command_catalog,
            models: config.models,
            chat_model_keys: config.chat_model_keys,
            main_agent: config.main_agent,
            sandbox: config.sandbox,
            conversations: Arc::new(Mutex::new(ConversationManager::new(&workdir)?)),
            snapshots: Arc::new(Mutex::new(SnapshotManager::new(&workdir)?)),
            sink_router: Arc::new(RwLock::new(SinkRouter::new())),
            cron_manager,
            agent_registry,
            agent_registry_notify,
            max_global_sub_agents: config.max_global_sub_agents,
            subagent_count: Arc::new(AtomicUsize::new(0)),
            cron_poll_interval_seconds: config.cron_poll_interval_seconds,
            background_job_sender,
            background_job_receiver: Some(background_job_receiver),
            summary_tracker: Arc::new(SummaryTracker::new()),
            active_foreground_controls: Arc::new(Mutex::new(HashMap::new())),
            subagents: Arc::new(Mutex::new(HashMap::new())),
        })
    }

    pub async fn run(mut self) -> Result<()> {
        self.retry_pending_workspace_summaries().await?;
        let (sender, mut receiver) = mpsc::channel::<IncomingMessage>(128);
        let background_receiver = self.background_job_receiver.take();
        let server = Arc::new(self);
        {
            let runtime = server.tool_runtime();
            tokio::spawn(async move {
                let mut ticker = interval(Duration::from_secs(runtime.cron_poll_interval_seconds));
                ticker.set_missed_tick_behavior(MissedTickBehavior::Skip);
                loop {
                    ticker.tick().await;
                    if let Err(error) = runtime.poll_cron_once().await {
                        error!(
                            log_stream = "server",
                            kind = "cron_poll_failed",
                            error = %format!("{error:#}"),
                            "cron poll failed"
                        );
                    }
                }
            });
        }
        if let Some(mut background_receiver) = background_receiver {
            let runtime = server.tool_runtime();
            tokio::spawn(async move {
                while let Some(job) = background_receiver.recv().await {
                    let runtime = runtime.clone();
                    tokio::spawn(async move {
                        if let Err(error) = runtime.run_background_job(job).await {
                            error!(
                                log_stream = "agent",
                                kind = "background_agent_failed",
                                error = %format!("{error:#}"),
                                "background agent failed"
                            );
                        }
                    });
                }
            });
        }

        for channel in server.channels.values() {
            spawn_channel_supervisor(Arc::clone(channel), sender.clone());
        }
        drop(sender);

        let mut idle_compaction_ticker = interval(Duration::from_secs(
            server
                .main_agent
                .idle_context_compaction_poll_interval_seconds,
        ));
        idle_compaction_ticker.set_missed_tick_behavior(MissedTickBehavior::Skip);
        let conversation_workers: Arc<
            Mutex<HashMap<String, tokio::sync::mpsc::UnboundedSender<IncomingMessage>>>,
        > = Arc::new(Mutex::new(HashMap::new()));
        let active_worker_count = Arc::new(AtomicUsize::new(0));
        let active_worker_notify = Arc::new(Notify::new());
        let mut receiver_closed = false;

        loop {
            if receiver_closed && active_worker_count.load(Ordering::SeqCst) == 0 {
                break;
            }

            select! {
                maybe_message = receiver.recv(), if !receiver_closed => {
                    match maybe_message {
                        Some(message) => {
                            if let Some(outgoing) =
                                fast_path_model_selection_message(&server.workdir, &server.models, &server.chat_model_keys, &message)
                            {
                                if let Some(channel) = server.channels.get(&message.address.channel_id) {
                                    if let Err(error) = channel.send(&message.address, outgoing).await {
                                        error!(
                                            log_stream = "channel",
                                            log_key = %message.address.channel_id,
                                            kind = "fast_path_send_failed",
                                            conversation_id = %message.address.conversation_id,
                                            error = %format!("{error:#}"),
                                            "failed to send fast-path model selection message"
                                        );
                                    }
                                }
                                continue;
                            }
                            let interrupted_followup =
                                request_yield_for_incoming(&server.active_foreground_controls, &message);
                            let message = if interrupted_followup {
                                let mut message = message;
                                message.text = tag_interrupted_followup_text(message.text);
                                message
                            } else {
                                message
                            };
                            let session_key = message.address.session_key();
                            let mut pending_message = Some(message);
                            loop {
                                let worker_sender = conversation_workers
                                    .lock()
                                    .map_err(|_| anyhow!("conversation workers lock poisoned"))?
                                    .get(&session_key)
                                    .cloned();
                                let worker_sender = match worker_sender {
                                    Some(worker_sender) => worker_sender,
                                    None => {
                                        let (worker_tx, mut worker_rx) = tokio::sync::mpsc::unbounded_channel();
                                        conversation_workers
                                            .lock()
                                            .map_err(|_| anyhow!("conversation workers lock poisoned"))?
                                            .insert(session_key.clone(), worker_tx.clone());
                                        active_worker_count.fetch_add(1, Ordering::SeqCst);
                                        let server = Arc::clone(&server);
                                        let conversation_workers = Arc::clone(&conversation_workers);
                                        let active_worker_count = Arc::clone(&active_worker_count);
                                        let active_worker_notify = Arc::clone(&active_worker_notify);
                                        let worker_session_key = session_key.clone();
                                        tokio::spawn(async move {
                                            let mut local_queue = VecDeque::new();
                                            while let Some(message) = worker_rx.recv().await {
                                                local_queue.push_back(message);
                                                while let Ok(message) = worker_rx.try_recv() {
                                                    local_queue.push_back(message);
                                                }
                                                while let Some(message) = local_queue.pop_front() {
                                                    let merged =
                                                        coalesce_buffered_conversation_messages(message, &mut local_queue);
                                                    if let Err(error) = server.handle_incoming(merged).await {
                                                        error!(
                                                            log_stream = "server",
                                                            kind = "handle_incoming_failed",
                                                            error = %format!("{error:#}"),
                                                            "failed to handle incoming message"
                                                        );
                                                    }
                                                    while let Ok(message) = worker_rx.try_recv() {
                                                        local_queue.push_back(message);
                                                    }
                                                }
                                            }
                                            if let Ok(mut workers) = conversation_workers.lock() {
                                                workers.remove(&worker_session_key);
                                            }
                                            active_worker_count.fetch_sub(1, Ordering::SeqCst);
                                            active_worker_notify.notify_waiters();
                                        });
                                        worker_tx
                                    }
                                };
                                let message = pending_message
                                    .take()
                                    .expect("pending message should exist while dispatching");
                                match worker_sender.send(message) {
                                    Ok(()) => break,
                                    Err(error) => {
                                        if let Ok(mut workers) = conversation_workers.lock() {
                                            workers.remove(&session_key);
                                        }
                                        pending_message = Some(error.0);
                                    }
                                }
                            }
                        }
                        None => receiver_closed = true,
                    }
                }
                _ = idle_compaction_ticker.tick() => {
                    if server.main_agent.enable_idle_context_compaction
                        && let Err(error) = server.run_idle_context_compaction_once().await
                    {
                        error!(
                            log_stream = "server",
                            kind = "idle_context_compaction_failed",
                            error = %format!("{error:#}"),
                            "idle context compaction pass failed"
                        );
                    }
                }
                _ = active_worker_notify.notified(), if receiver_closed => {}
            }
        }

        if let Err(error) = server.summarize_active_workspaces_on_shutdown().await {
            warn!(
                log_stream = "server",
                kind = "workspace_shutdown_summary_failed",
                error = %format!("{error:#}"),
                "failed to summarize one or more active workspaces during shutdown"
            );
        }

        warn!(
            log_stream = "server",
            kind = "message_loop_ended",
            "all channel senders closed; server loop ended"
        );
        Ok(())
    }

    async fn run_idle_context_compaction_once(&self) -> Result<()> {
        let lead_time = Duration::from_secs(30);
        let now = Utc::now();
        let snapshots = self.with_sessions(|sessions| Ok(sessions.list_foreground_snapshots()))?;

        for session in snapshots {
            if !self.effective_context_compaction_enabled(&session.address)? {
                continue;
            }
            let model_key = self.effective_main_model_key(&session.address)?;
            let model = self.model_config_or_main(&model_key)?.clone();
            let runtime = self.tool_runtime_for_address(&session.address)?;
            let Some(ttl) = model.cache_ttl.as_deref() else {
                continue;
            };
            let ttl = parse_duration(ttl)
                .with_context(|| format!("failed to parse model cache_ttl '{}'", ttl))?;
            let Some(idle_threshold) = ttl.checked_sub(lead_time) else {
                continue;
            };
            if !should_attempt_idle_context_compaction(&session, now, idle_threshold) {
                continue;
            }
            runtime.idle_compact_subagents_for_session(&session, idle_threshold)?;

            let config = runtime.build_agent_frame_config(
                &session,
                &session.workspace_root,
                AgentPromptKind::MainForeground,
                &model_key,
                None,
            )?;
            let extra_tools = runtime.build_extra_tools(
                &session,
                AgentPromptKind::MainForeground,
                session.agent_id,
                None,
            );
            let report = run_backend_compaction(
                model.backend,
                session.agent_messages.clone(),
                config,
                extra_tools,
            )
            .with_context(|| format!("failed to compact idle session {}", session.id))?;
            if !report.compacted {
                continue;
            }

            let compaction_stats = compaction_stats_from_report(&report);
            self.with_sessions(|sessions| {
                sessions.record_idle_compaction(
                    &session.address,
                    report.messages,
                    &compaction_stats,
                )
            })
            .with_context(|| format!("failed to persist idle compaction for {}", session.id))?;
            info!(
                log_stream = "session",
                log_key = %session.id,
                kind = "idle_context_compaction_completed",
                channel_id = %session.address.channel_id,
                agent_id = %session.agent_id,
                turn_count = session.turn_count,
                token_limit = report.token_limit as u64,
                estimated_tokens_before = report.estimated_tokens_before as u64,
                estimated_tokens_after = report.estimated_tokens_after as u64,
                llm_calls = report.usage.llm_calls,
                prompt_tokens = report.usage.prompt_tokens,
                completion_tokens = report.usage.completion_tokens,
                total_tokens = report.usage.total_tokens,
                cache_hit_tokens = report.usage.cache_hit_tokens,
                cache_miss_tokens = report.usage.cache_miss_tokens,
                cache_read_tokens = report.usage.cache_read_tokens,
                cache_write_tokens = report.usage.cache_write_tokens,
                "idle context compaction completed"
            );
        }

        Ok(())
    }

    fn tool_runtime(&self) -> ServerRuntime {
        ServerRuntime {
            agent_workspace: self.agent_workspace.clone(),
            workspace_manager: self.workspace_manager.clone(),
            active_workspace_ids: self
                .with_sessions(|sessions| Ok(sessions.list_foreground_snapshots()))
                .unwrap_or_default()
                .into_iter()
                .map(|session| session.workspace_id)
                .collect(),
            selected_main_model_key: None,
            selected_reasoning_effort: None,
            selected_context_compaction_enabled: None,
            channels: Arc::clone(&self.channels),
            command_catalog: self.command_catalog.clone(),
            models: self.models.clone(),
            chat_model_keys: self.chat_model_keys.clone(),
            main_agent: self.main_agent.clone(),
            sandbox: self.sandbox.clone(),
            sink_router: Arc::clone(&self.sink_router),
            cron_manager: Arc::clone(&self.cron_manager),
            agent_registry: Arc::clone(&self.agent_registry),
            agent_registry_notify: Arc::clone(&self.agent_registry_notify),
            max_global_sub_agents: self.max_global_sub_agents,
            subagent_count: Arc::clone(&self.subagent_count),
            cron_poll_interval_seconds: self.cron_poll_interval_seconds,
            background_job_sender: self.background_job_sender.clone(),
            summary_tracker: Arc::clone(&self.summary_tracker),
            subagents: Arc::clone(&self.subagents),
        }
    }

    fn tool_runtime_for_sandbox_mode(&self, sandbox_mode: SandboxMode) -> ServerRuntime {
        let mut runtime = self.tool_runtime();
        runtime.sandbox.mode = sandbox_mode;
        runtime
    }

    fn tool_runtime_for_address(&self, address: &ChannelAddress) -> Result<ServerRuntime> {
        let sandbox_mode = self.effective_sandbox_mode(address)?;
        let mut runtime = self.tool_runtime_for_sandbox_mode(sandbox_mode);
        let settings = self.effective_conversation_settings(address)?;
        runtime.selected_main_model_key = settings.main_model.clone();
        runtime.selected_reasoning_effort = settings.reasoning_effort.clone();
        runtime.selected_context_compaction_enabled = settings.context_compaction_enabled;
        Ok(runtime)
    }

    fn unregister_active_foreground_control(&self, address: &ChannelAddress) -> Result<()> {
        let mut controls = self
            .active_foreground_controls
            .lock()
            .map_err(|_| anyhow!("active foreground controls lock poisoned"))?;
        controls.remove(&address.session_key());
        Ok(())
    }

    fn destroy_foreground_session(&self, address: &ChannelAddress) -> Result<()> {
        let snapshot = self.with_sessions(|sessions| Ok(sessions.get_snapshot(address)))?;
        if let Some(control) = self
            .active_foreground_controls
            .lock()
            .ok()
            .and_then(|controls| controls.get(&address.session_key()).cloned())
        {
            control.request_cancel();
        }
        self.unregister_active_foreground_control(address)?;
        if let Some(session) = snapshot {
            let destroyed_subagents = self
                .tool_runtime()
                .destroy_subagents_for_session(session.id)?;
            let runtime_state_root = self
                .agent_workspace
                .root_dir
                .join("runtime")
                .join(&session.workspace_id);
            let report = terminate_runtime_state_tasks(&runtime_state_root)?;
            if destroyed_subagents > 0
                || report.exec_processes_killed > 0
                || report.file_downloads_cancelled > 0
                || report.image_tasks_cancelled > 0
            {
                info!(
                    log_stream = "session",
                    log_key = %session.id,
                    kind = "session_runtime_tasks_destroyed",
                    workspace_id = %session.workspace_id,
                    subagents_destroyed = destroyed_subagents as u64,
                    exec_processes_killed = report.exec_processes_killed as u64,
                    file_downloads_cancelled = report.file_downloads_cancelled as u64,
                    image_tasks_cancelled = report.image_tasks_cancelled as u64,
                    "destroyed background runtime tasks for session"
                );
            }
        }
        self.with_sessions(|sessions| sessions.destroy_foreground(address))
    }

    pub fn workdir(&self) -> &Path {
        &self.workdir
    }

    pub fn create_sub_agent_placeholder(&self, parent_agent_id: uuid::Uuid) -> SubAgentSpec {
        SubAgentSpec {
            id: uuid::Uuid::new_v4(),
            parent_agent_id,
            docker_image: None,
            can_spawn_sub_agents: false,
        }
    }

    async fn handle_incoming(&self, incoming: IncomingMessage) -> Result<()> {
        self.with_conversations(|conversations| {
            conversations.ensure_conversation(&incoming.address)
        })?;
        if let Err(error) = self.archive_stale_workspaces_if_needed() {
            warn!(
                log_stream = "server",
                kind = "workspace_archive_pass_failed",
                error = %format!("{error:#}"),
                "failed to archive stale workspaces before handling message"
            );
        }
        info!(
            log_stream = "channel",
            log_key = %incoming.address.channel_id,
            kind = "incoming_message",
            conversation_id = %incoming.address.conversation_id,
            remote_message_id = %incoming.remote_message_id,
            has_text = incoming.text.is_some(),
            attachments_count = incoming.attachments.len() as u64,
            "received normalized incoming message"
        );

        let channel = self
            .channels
            .get(&incoming.address.channel_id)
            .with_context(|| format!("unknown channel {}", incoming.address.channel_id))?
            .clone();

        if let Some(control) = incoming.control.as_ref() {
            match control {
                crate::channel::IncomingControl::ConversationClosed { reason } => {
                    info!(
                        log_stream = "session",
                        kind = "channel_conversation_closed",
                        channel_id = %incoming.address.channel_id,
                        conversation_id = %incoming.address.conversation_id,
                        reason = reason,
                        "channel reported that the conversation should be closed"
                    );
                    self.destroy_foreground_session(&incoming.address)?;
                    let disabled = self.disable_cron_tasks_for_conversation(&incoming.address)?;
                    if disabled > 0 {
                        warn!(
                            log_stream = "cron",
                            kind = "cron_tasks_auto_disabled_for_closed_conversation",
                            channel_id = %incoming.address.channel_id,
                            conversation_id = %incoming.address.conversation_id,
                            disabled_count = disabled as u64,
                            "disabled cron tasks because the conversation was closed"
                        );
                    }
                    return Ok(());
                }
            }
        }

        if incoming
            .text
            .as_deref()
            .map(str::trim)
            .is_some_and(|text| command_matches(text, "/new"))
        {
            if let Some(previous_session) =
                self.with_sessions(|sessions| Ok(sessions.get_snapshot(&incoming.address)))?
            {
                self.with_sessions(|sessions| {
                    sessions.mark_workspace_summary_state(&incoming.address, true, true)
                })?;
                if let Err(error) = self
                    .summarize_workspace_before_destroy(&previous_session)
                    .await
                {
                    warn!(
                        log_stream = "session",
                        log_key = %previous_session.id,
                        kind = "workspace_summary_before_reset_failed",
                        workspace_id = %previous_session.workspace_id,
                        error = %format!("{error:#}"),
                        "failed to summarize workspace before session reset"
                    );
                    self.send_user_error_message(&channel, &incoming.address, &error)
                        .await;
                    return Err(error);
                }
                self.destroy_foreground_session(&incoming.address)?;
            }
            let session =
                self.with_sessions(|sessions| sessions.reset_foreground(&incoming.address))?;
            info!(
                log_stream = "session",
                log_key = %session.id,
                kind = "session_reset",
                channel_id = %incoming.address.channel_id,
                conversation_id = %incoming.address.conversation_id,
                "foreground session reset"
            );
            if let Some(missing_model_key) =
                self.clear_missing_selected_main_model(&incoming.address)?
            {
                self.prompt_missing_conversation_model(
                    &channel,
                    &incoming.address,
                    &missing_model_key,
                )
                .await?;
                return Ok(());
            }
            if self.selected_main_model_key(&incoming.address)?.is_some() {
                let welcome = match self.initialize_foreground_session(&session, true).await {
                    Ok(welcome) => welcome,
                    Err(error) => {
                        self.send_user_error_message(&channel, &incoming.address, &error)
                            .await;
                        return Err(error);
                    }
                };
                self.send_channel_message(&channel, &incoming.address, welcome)
                    .await?;
            } else {
                self.send_channel_message(
                    &channel,
                    &incoming.address,
                    self.model_selection_message(
                        &incoming.address,
                        "This conversation has no model yet. Choose one to start a new session.",
                    )?,
                )
                .await?;
            }
            return Ok(());
        }

        if let Some(workspace_id) = parse_oldspace_command(incoming.text.as_deref()) {
            if let Some(previous_session) =
                self.with_sessions(|sessions| Ok(sessions.get_snapshot(&incoming.address)))?
                && previous_session.workspace_id != workspace_id
            {
                self.with_sessions(|sessions| {
                    sessions.mark_workspace_summary_state(&incoming.address, true, true)
                })?;
                if let Err(error) = self
                    .summarize_workspace_before_destroy(&previous_session)
                    .await
                {
                    warn!(
                        log_stream = "session",
                        log_key = %previous_session.id,
                        kind = "workspace_summary_before_oldspace_failed",
                        workspace_id = %previous_session.workspace_id,
                        error = %format!("{error:#}"),
                        "failed to summarize workspace before switching workspaces"
                    );
                    self.send_user_error_message(&channel, &incoming.address, &error)
                        .await;
                    return Err(error);
                }
                self.destroy_foreground_session(&incoming.address)?;
            }
            let session = match self.activate_existing_workspace(&incoming.address, &workspace_id) {
                Ok(session) => session,
                Err(error) => {
                    self.send_user_error_message(&channel, &incoming.address, &error)
                        .await;
                    return Err(error);
                }
            };
            if let Some(missing_model_key) =
                self.clear_missing_selected_main_model(&incoming.address)?
            {
                self.prompt_missing_conversation_model(
                    &channel,
                    &incoming.address,
                    &missing_model_key,
                )
                .await?;
                return Ok(());
            }
            if self.selected_main_model_key(&incoming.address)?.is_some() {
                let welcome = match self.initialize_foreground_session(&session, true).await {
                    Ok(welcome) => welcome,
                    Err(error) => {
                        self.send_user_error_message(&channel, &incoming.address, &error)
                            .await;
                        return Err(error);
                    }
                };
                self.send_channel_message(&channel, &incoming.address, welcome)
                    .await?;
            } else {
                self.send_channel_message(
                    &channel,
                    &incoming.address,
                    self.model_selection_message(
                        &incoming.address,
                        "Choose a model for this conversation before activating a workspace.",
                    )?,
                )
                .await?;
            }
            return Ok(());
        }

        if incoming
            .text
            .as_deref()
            .map(str::trim)
            .is_some_and(|text| command_matches(text, "/help"))
        {
            let help_text = self.help_text_for_channel(&incoming.address.channel_id);
            info!(
                log_stream = "server",
                kind = "help_requested",
                channel_id = %incoming.address.channel_id,
                conversation_id = %incoming.address.conversation_id,
                "rendering help text"
            );
            self.send_channel_message(
                &channel,
                &incoming.address,
                OutgoingMessage::text(help_text),
            )
            .await?;
            return Ok(());
        }

        if parse_model_command(incoming.text.as_deref()).is_none()
            && let Some(missing_model_key) =
                self.clear_missing_selected_main_model(&incoming.address)?
        {
            self.prompt_missing_conversation_model(
                &channel,
                &incoming.address,
                &missing_model_key,
            )
            .await?;
            return Ok(());
        }

        if incoming
            .text
            .as_deref()
            .map(str::trim)
            .is_some_and(|text| command_matches(text, "/status"))
        {
            let Some(effective_model_key) = self.selected_main_model_key(&incoming.address)? else {
                self.send_channel_message(
                    &channel,
                    &incoming.address,
                    self.model_selection_message(
                        &incoming.address,
                        "Choose a model for this conversation before using `/status`.",
                    )?,
                )
                .await?;
                return Ok(());
            };
            let session =
                self.with_sessions(|sessions| sessions.ensure_foreground(&incoming.address))?;
            let status_text = self.status_text_for_session(&session, &effective_model_key)?;
            self.send_channel_message(
                &channel,
                &incoming.address,
                OutgoingMessage::text(status_text),
            )
            .await?;
            return Ok(());
        }

        if incoming
            .text
            .as_deref()
            .map(str::trim)
            .is_some_and(|text| command_matches(text, "/compact"))
        {
            let Some(effective_model_key) = self.selected_main_model_key(&incoming.address)? else {
                self.send_channel_message(
                    &channel,
                    &incoming.address,
                    self.model_selection_message(
                        &incoming.address,
                        "Choose a model for this conversation before using `/compact`.",
                    )?,
                )
                .await?;
                return Ok(());
            };
            let session =
                self.with_sessions(|sessions| sessions.ensure_foreground(&incoming.address))?;
            let compacted = self
                .compact_session_now(&session, &effective_model_key, true)
                .await?;
            let message = if compacted {
                "Compacted the current conversation context.".to_string()
            } else {
                "The current conversation context did not need compaction.".to_string()
            };
            self.send_channel_message(&channel, &incoming.address, OutgoingMessage::text(message))
                .await?;
            return Ok(());
        }

        if let Some(argument) = parse_compact_mode_command(incoming.text.as_deref()) {
            if let Some(mode_name) = argument {
                let enabled = match mode_name.trim() {
                    "on" | "enable" | "enabled" => true,
                    "off" | "disable" | "disabled" => false,
                    _ => {
                        let error = anyhow!("unknown compact mode {}", mode_name);
                        self.send_user_error_message(&channel, &incoming.address, &error)
                            .await;
                        return Err(error);
                    }
                };
                self.with_conversations(|conversations| {
                    conversations.set_context_compaction_enabled(&incoming.address, Some(enabled))
                })?;
                self.send_channel_message(
                    &channel,
                    &incoming.address,
                    OutgoingMessage::text(format!(
                        "Automatic context compaction is now `{}` for this conversation.",
                        if enabled { "enabled" } else { "disabled" }
                    )),
                )
                .await?;
                return Ok(());
            }
            self.send_channel_message(
                &channel,
                &incoming.address,
                self.compact_mode_message(&incoming.address)?,
            )
            .await?;
            return Ok(());
        }

        if let Some(argument) = parse_model_command(incoming.text.as_deref()) {
            if argument.is_none() {
                let _ = self.clear_missing_selected_main_model(&incoming.address)?;
            }
            if let Some(model_key) = argument {
                if !self.models.contains_key(&model_key) {
                    let error = anyhow!("unknown model {}", model_key);
                    self.send_user_error_message(&channel, &incoming.address, &error)
                        .await;
                    return Err(error);
                }
                let current_model_key = self.selected_main_model_key(&incoming.address)?;
                if current_model_key.as_deref() == Some(model_key.as_str()) {
                    self.send_channel_message(
                        &channel,
                        &incoming.address,
                        OutgoingMessage::text(format!(
                            "Conversation model is already `{}`. No change was made.",
                            model_key
                        )),
                    )
                    .await?;
                    return Ok(());
                }
                let compacted = if let Some(previous_model_key) = current_model_key {
                    let session = self
                        .with_sessions(|sessions| sessions.ensure_foreground(&incoming.address))?;
                    self.compact_session_now(&session, &previous_model_key, false)
                        .await
                        .unwrap_or(false)
                } else {
                    false
                };
                let conversation = self.with_conversations(|conversations| {
                    conversations.set_main_model(&incoming.address, Some(model_key.clone()))
                })?;
                let effective_model_key = conversation
                    .settings
                    .main_model
                    .clone()
                    .expect("model just set");
                self.send_channel_message(
                    &channel,
                    &incoming.address,
                    OutgoingMessage::text(format!(
                        "Conversation model updated to `{}`.{}",
                        effective_model_key,
                        if compacted {
                            " Existing context was compacted before the switch."
                        } else {
                            ""
                        }
                    )),
                )
                .await?;
                return Ok(());
            }
            self.send_channel_message(
                &channel,
                &incoming.address,
                self.model_selection_message(
                    &incoming.address,
                    "Choose a model for this conversation.",
                )?,
            )
            .await?;
            return Ok(());
        }

        if let Some(argument) = parse_sandbox_command(incoming.text.as_deref()) {
            if let Some(mode_name) = argument {
                let selected_mode = if mode_name == "default" {
                    None
                } else {
                    let parsed = parse_sandbox_mode_value(&mode_name)
                        .ok_or_else(|| anyhow!("unknown sandbox mode {}", mode_name));
                    let parsed = match parsed {
                        Ok(mode) => mode,
                        Err(error) => {
                            self.send_user_error_message(&channel, &incoming.address, &error)
                                .await;
                            return Err(error);
                        }
                    };
                    if !self.available_sandbox_modes().contains(&parsed) {
                        let error =
                            anyhow!("sandbox mode {} is not available on this system", mode_name);
                        self.send_user_error_message(&channel, &incoming.address, &error)
                            .await;
                        return Err(error);
                    }
                    Some(parsed)
                };
                let conversation = self.with_conversations(|conversations| {
                    conversations.set_sandbox_mode(&incoming.address, selected_mode)
                })?;
                let effective_mode = conversation
                    .settings
                    .sandbox_mode
                    .unwrap_or(self.sandbox.mode);
                self.send_channel_message(
                    &channel,
                    &incoming.address,
                    OutgoingMessage::text(format!(
                        "Conversation sandbox mode updated to `{}`.",
                        sandbox_mode_label(effective_mode)
                    )),
                )
                .await?;
                return Ok(());
            }
            let current_mode = self.effective_sandbox_mode(&incoming.address)?;
            let options = self
                .available_sandbox_modes()
                .into_iter()
                .map(|mode| ShowOption {
                    label: sandbox_mode_label(mode).to_string(),
                    value: format!("/sandbox {}", sandbox_mode_value(mode)),
                })
                .chain(std::iter::once(ShowOption {
                    label: "default".to_string(),
                    value: "/sandbox default".to_string(),
                }))
                .collect::<Vec<_>>();
            self.send_channel_message(
                &channel,
                &incoming.address,
                OutgoingMessage::with_options(
                    format!(
                        "Current conversation sandbox mode: `{}`\nChoose a mode below or send `/sandbox <mode>`.",
                        sandbox_mode_label(current_mode)
                    ),
                    "Choose a sandbox mode",
                    options,
                ),
            )
            .await?;
            return Ok(());
        }

        if let Some(argument) = parse_think_command(incoming.text.as_deref()) {
            if let Some(effort_name) = argument {
                let selected_effort = if effort_name == "default" {
                    None
                } else {
                    let parsed = parse_reasoning_effort_value(&effort_name)
                        .ok_or_else(|| anyhow!("unknown reasoning effort {}", effort_name));
                    let parsed = match parsed {
                        Ok(effort) => effort,
                        Err(error) => {
                            self.send_user_error_message(&channel, &incoming.address, &error)
                                .await;
                            return Err(error);
                        }
                    };
                    Some(parsed.to_string())
                };
                let conversation = self.with_conversations(|conversations| {
                    conversations.set_reasoning_effort(&incoming.address, selected_effort)
                })?;
                let effective_effort = conversation
                    .settings
                    .reasoning_effort
                    .clone()
                    .or_else(|| {
                        self.selected_main_model_key(&incoming.address)
                            .ok()
                            .flatten()
                            .and_then(|model_key| {
                                self.models.get(&model_key).and_then(|model| {
                                    model
                                        .reasoning
                                        .as_ref()
                                        .and_then(|reasoning| reasoning.effort.clone())
                                })
                            })
                    })
                    .unwrap_or_else(|| "default".to_string());
                self.send_channel_message(
                    &channel,
                    &incoming.address,
                    OutgoingMessage::text(format!(
                        "Conversation reasoning effort updated to `{}`.",
                        effective_effort
                    )),
                )
                .await?;
                return Ok(());
            }
            self.send_channel_message(
                &channel,
                &incoming.address,
                self.reasoning_effort_message(&incoming.address)?,
            )
            .await?;
            return Ok(());
        }

        if matches!(
            parse_optional_command_argument(incoming.text.as_deref(), "/snapsave"),
            Some(None)
        ) {
            self.send_channel_message(
                &channel,
                &incoming.address,
                OutgoingMessage::text(
                    "Usage: `/snapsave <name>`\nExample: `/snapsave demo`".to_string(),
                ),
            )
            .await?;
            return Ok(());
        }

        if let Some(checkpoint_name) = parse_snap_save_command(incoming.text.as_deref()) {
            let session =
                self.with_sessions(|sessions| sessions.ensure_foreground(&incoming.address))?;
            let checkpoint =
                self.with_sessions(|sessions| sessions.export_checkpoint(&incoming.address))?;
            let bundle = SnapshotBundle {
                saved_at: Utc::now(),
                source_address: incoming.address.clone(),
                settings: self.effective_conversation_settings(&incoming.address)?,
                session: checkpoint,
            };
            let record = self.with_snapshots(|snapshots| {
                snapshots.save_snapshot(
                    &incoming.address,
                    &checkpoint_name,
                    bundle,
                    &session.workspace_root,
                )
            })?;
            self.send_channel_message(
                &channel,
                &incoming.address,
                OutgoingMessage::text(format!(
                    "Saved global snapshot `{}` at {}.",
                    record.name, record.saved_at
                )),
            )
            .await?;
            return Ok(());
        }

        if parse_snap_list_command(incoming.text.as_deref())
            || matches!(
                parse_optional_command_argument(incoming.text.as_deref(), "/snapload"),
                Some(None)
            )
        {
            let snapshots = self.with_snapshots(|snapshots| Ok(snapshots.list_snapshots()))?;
            if snapshots.is_empty() {
                self.send_channel_message(
                    &channel,
                    &incoming.address,
                    OutgoingMessage::text(
                        "There are no saved snapshots yet. Use `/snapsave <name>` first."
                            .to_string(),
                    ),
                )
                .await?;
                return Ok(());
            }
            let lines = snapshots
                .iter()
                .map(|record| {
                    format!(
                        "- `{}` ({}, from `{}`)",
                        record.name, record.saved_at, record.source_conversation_id
                    )
                })
                .collect::<Vec<_>>();
            let options = snapshots
                .iter()
                .map(|record| ShowOption {
                    label: record.name.clone(),
                    value: format!("/snapload {}", record.name),
                })
                .collect::<Vec<_>>();
            self.send_channel_message(
                &channel,
                &incoming.address,
                OutgoingMessage::with_options(
                    format!(
                        "Saved global snapshots:\n{}\n\nChoose one below or send `/snapload <name>`.",
                        lines.join("\n")
                    ),
                    "Choose a snapshot to load",
                    options,
                ),
            )
            .await?;
            return Ok(());
        }

        if let Some(checkpoint_name) = parse_snap_load_command(incoming.text.as_deref()) {
            let loaded =
                match self.with_snapshots(|snapshots| snapshots.load_snapshot(&checkpoint_name)) {
                    Ok(loaded) => loaded,
                    Err(error) => {
                        self.send_user_error_message(&channel, &incoming.address, &error)
                            .await;
                        return Err(error);
                    }
                };
            self.with_conversations(|conversations| {
                conversations
                    .set_main_model(&incoming.address, loaded.bundle.settings.main_model.clone())
            })?;
            self.with_conversations(|conversations| {
                conversations
                    .set_sandbox_mode(&incoming.address, loaded.bundle.settings.sandbox_mode)
            })?;
            self.with_conversations(|conversations| {
                conversations.set_reasoning_effort(
                    &incoming.address,
                    loaded.bundle.settings.reasoning_effort.clone(),
                )
            })?;
            self.with_conversations(|conversations| {
                conversations.set_context_compaction_enabled(
                    &incoming.address,
                    loaded.bundle.settings.context_compaction_enabled,
                )
            })?;
            self.destroy_foreground_session(&incoming.address)?;
            let workspace = self.workspace_manager.create_workspace(
                uuid::Uuid::new_v4(),
                uuid::Uuid::new_v4(),
                Some(&format!("snapshot-{}", loaded.record.name)),
            )?;
            replace_directory_contents(&workspace.files_dir, &loaded.workspace_dir)?;
            let restored = self.with_sessions(|sessions| {
                sessions.restore_foreground_from_checkpoint(
                    &incoming.address,
                    loaded.bundle.session,
                    workspace.id.clone(),
                    workspace.files_dir.clone(),
                )
            })?;
            self.send_channel_message(
                &channel,
                &incoming.address,
                OutgoingMessage::text(format!(
                    "Loaded snapshot `{}` into a new session with workspace `{}`.",
                    loaded.record.name, restored.workspace_id
                )),
            )
            .await?;
            return Ok(());
        }

        if incoming
            .text
            .as_deref()
            .map(str::trim)
            .is_some_and(|text| command_starts_with(text, "/set_api_timeout"))
            && parse_set_api_timeout_command(incoming.text.as_deref()).is_none()
        {
            let usage = "Usage: /set_api_timeout <seconds|default>\nExamples:\n/set_api_timeout 300\n/set_api_timeout default";
            self.send_channel_message(&channel, &incoming.address, OutgoingMessage::text(usage))
                .await?;
            return Ok(());
        }

        if let Some(argument) = parse_set_api_timeout_command(incoming.text.as_deref()) {
            let session =
                self.with_sessions(|sessions| sessions.ensure_foreground(&incoming.address))?;
            let effective_model_key = self.effective_main_model_key(&incoming.address)?;
            let model_timeout_seconds =
                self.model_upstream_timeout_seconds(&effective_model_key)?;
            let (override_timeout, status_text) =
                match format_api_timeout_update(&session, model_timeout_seconds, &argument) {
                    Ok(result) => result,
                    Err(error) => {
                        self.send_user_error_message(&channel, &incoming.address, &error)
                            .await;
                        return Err(error);
                    }
                };
            self.with_sessions(|sessions| {
                sessions.set_api_timeout_override(&incoming.address, override_timeout)
            })?;
            self.send_channel_message(
                &channel,
                &incoming.address,
                OutgoingMessage::text(status_text),
            )
            .await?;
            return Ok(());
        }

        if parse_continue_command(incoming.text.as_deref()) {
            let session =
                self.with_sessions(|sessions| sessions.ensure_foreground(&incoming.address))?;
            let Some(pending_continue) =
                self.with_sessions(|sessions| sessions.pending_continue(&incoming.address))?
            else {
                self.send_channel_message(
                    &channel,
                    &incoming.address,
                    OutgoingMessage::text(
                        "There is no interrupted turn to continue right now.".to_string(),
                    ),
                )
                .await?;
                return Ok(());
            };
            channel
                .set_processing(&incoming.address, ProcessingState::Typing)
                .await
                .ok();
            let typing_guard = spawn_processing_keepalive(
                channel.clone(),
                incoming.address.clone(),
                ProcessingState::Typing,
            );
            let outcome = self
                .run_main_agent_turn_with_previous_messages(
                    &session,
                    &pending_continue.model_key,
                    pending_continue.resume_messages.clone(),
                    pending_continue.original_user_text.clone(),
                    pending_continue.original_attachments.clone(),
                )
                .await
                .context("failed to continue interrupted foreground turn");
            if let Some(stop_sender) = typing_guard {
                let _ = stop_sender.send(());
            }
            if let Err(error) = &outcome {
                channel
                    .set_processing(&incoming.address, ProcessingState::Idle)
                    .await
                    .ok();
                self.send_user_error_message(&channel, &incoming.address, error)
                    .await;
            }
            match outcome? {
                ForegroundTurnOutcome::Replied {
                    messages,
                    outgoing,
                    usage,
                    compaction,
                    timed_out,
                } => {
                    let loaded_skills =
                        extract_loaded_skill_names(&messages, session.agent_message_count);
                    self.with_sessions(|sessions| {
                        sessions.record_agent_turn(&incoming.address, messages, &usage, &compaction)
                    })
                    .context("failed to persist continued agent_frame messages")?;
                    self.with_sessions(|sessions| {
                        sessions.mark_skills_loaded_current_turn(&incoming.address, &loaded_skills)
                    })?;
                    self.with_sessions(|sessions| {
                        sessions.append_user_message(
                            &incoming.address,
                            pending_continue.original_user_text.clone(),
                            pending_continue.original_attachments.clone(),
                        )
                    })?;
                    self.with_sessions(|sessions| {
                        sessions.append_assistant_message(
                            &incoming.address,
                            outgoing.text.clone(),
                            Vec::new(),
                        )
                    })?;
                    let foreground =
                        self.build_foreground_agent(&session, &pending_continue.model_key)?;
                    self.log_turn_usage(&session, &usage, false);
                    info!(
                        log_stream = "agent",
                        log_key = %foreground.id,
                        kind = "foreground_agent_replied",
                        session_id = %foreground.session_id,
                        channel_id = %foreground.channel_id,
                        system_prompt_len = foreground.system_prompt.len() as u64,
                        timed_out,
                        has_text = outgoing.text.as_deref().is_some_and(|text| !text.trim().is_empty()),
                        attachment_count = outgoing.attachments.len() as u64 + outgoing.images.len() as u64,
                        "foreground agent continued interrupted turn"
                    );
                    self.send_channel_message(&channel, &incoming.address, outgoing)
                        .await?;
                    channel
                        .set_processing(&incoming.address, ProcessingState::Idle)
                        .await
                        .ok();
                    return Ok(());
                }
                ForegroundTurnOutcome::Yielded {
                    messages,
                    usage,
                    compaction,
                } => {
                    let loaded_skills =
                        extract_loaded_skill_names(&messages, session.agent_message_count);
                    self.with_sessions(|sessions| {
                        sessions.record_yielded_turn(
                            &incoming.address,
                            messages,
                            &usage,
                            &compaction,
                        )
                    })
                    .context("failed to persist yielded continued agent_frame messages")?;
                    self.with_sessions(|sessions| {
                        sessions.mark_skills_loaded_current_turn(&incoming.address, &loaded_skills)
                    })?;
                    channel
                        .set_processing(&incoming.address, ProcessingState::Idle)
                        .await
                        .ok();
                    return Ok(());
                }
                ForegroundTurnOutcome::Failed {
                    pending_continue,
                    error,
                } => {
                    self.with_sessions(|sessions| {
                        sessions
                            .set_pending_continue(&incoming.address, Some(pending_continue.clone()))
                    })?;
                    channel
                        .set_processing(&incoming.address, ProcessingState::Idle)
                        .await
                        .ok();
                    self.send_channel_message(
                        &channel,
                        &incoming.address,
                        OutgoingMessage::text(user_facing_continue_error_text(
                            &self.main_agent.language,
                            &error,
                            &pending_continue.progress_summary,
                        )),
                    )
                    .await?;
                    return Ok(());
                }
            }
        }

        if self.selected_main_model_key(&incoming.address)?.is_none() {
            self.send_channel_message(
                &channel,
                &incoming.address,
                self.model_selection_message(
                    &incoming.address,
                    "Choose a model for this conversation before sending messages.",
                )?,
            )
            .await?;
            return Ok(());
        }

        let session =
            self.with_sessions(|sessions| sessions.ensure_foreground(&incoming.address))?;
        if session.agent_message_count == 0 {
            if let Err(error) = self.initialize_foreground_session(&session, false).await {
                self.send_user_error_message(&channel, &incoming.address, &error)
                    .await;
                return Err(error);
            }
        }
        let session = self
            .with_sessions(|sessions| Ok(sessions.get_snapshot(&incoming.address)))?
            .expect("session should exist after initialization");

        let stored_attachments = self
            .materialize_attachments(&session.attachments_dir, incoming.attachments)
            .await?;
        let skill_updates_prefix = self.observe_runtime_skill_changes(&session)?;
        let effective_model_key = self.effective_main_model_key(&incoming.address)?;
        let effective_model = self.model_config_or_main(&effective_model_key)?.clone();
        let user_message = build_user_turn_message(
            incoming.text.as_deref(),
            skill_updates_prefix.as_deref(),
            &stored_attachments,
            &effective_model,
            backend_supports_native_multimodal_input(effective_model.backend),
        )?;
        let pending_continue =
            self.with_sessions(|sessions| sessions.pending_continue(&incoming.address))?;
        let previous_messages = build_previous_messages_for_turn(
            &session.agent_messages,
            pending_continue.as_ref(),
            Some(user_message),
        );

        channel
            .set_processing(&incoming.address, ProcessingState::Typing)
            .await
            .ok();
        let typing_guard = spawn_processing_keepalive(
            channel.clone(),
            incoming.address.clone(),
            ProcessingState::Typing,
        );

        let turn_result = self
            .run_main_agent_turn_with_previous_messages(
                &session,
                &effective_model_key,
                previous_messages,
                incoming.text.clone(),
                stored_attachments.clone(),
            )
            .await
            .context("foreground agent turn failed");
        if let Some(stop_sender) = typing_guard {
            let _ = stop_sender.send(());
        }
        if let Err(error) = &turn_result {
            channel
                .set_processing(&incoming.address, ProcessingState::Idle)
                .await
                .ok();
            self.send_user_error_message(&channel, &incoming.address, error)
                .await;
        }
        let outcome = turn_result?;
        let (messages, outgoing, usage, compaction, timed_out) = match outcome {
            ForegroundTurnOutcome::Replied {
                messages,
                outgoing,
                usage,
                compaction,
                timed_out,
            } => (messages, outgoing, usage, compaction, timed_out),
            ForegroundTurnOutcome::Yielded {
                messages,
                usage,
                compaction,
            } => {
                let loaded_skills =
                    extract_loaded_skill_names(&messages, session.agent_message_count);
                self.with_sessions(|sessions| {
                    sessions.record_yielded_turn(&incoming.address, messages, &usage, &compaction)
                })
                .context("failed to persist yielded agent_frame messages")?;
                self.with_sessions(|sessions| {
                    sessions.mark_skills_loaded_current_turn(&incoming.address, &loaded_skills)
                })?;
                self.with_sessions(|sessions| {
                    sessions.append_user_message(
                        &incoming.address,
                        incoming.text.clone(),
                        stored_attachments.clone(),
                    )
                })?;
                channel
                    .set_processing(&incoming.address, ProcessingState::Idle)
                    .await
                    .ok();
                return Ok(());
            }
            ForegroundTurnOutcome::Failed {
                pending_continue,
                error,
            } => {
                self.with_sessions(|sessions| {
                    sessions.set_pending_continue(&incoming.address, Some(pending_continue.clone()))
                })?;
                channel
                    .set_processing(&incoming.address, ProcessingState::Idle)
                    .await
                    .ok();
                self.send_channel_message(
                    &channel,
                    &incoming.address,
                    OutgoingMessage::text(user_facing_continue_error_text(
                        &self.main_agent.language,
                        &error,
                        &pending_continue.progress_summary,
                    )),
                )
                .await?;
                return Ok(());
            }
        };

        let loaded_skills = extract_loaded_skill_names(&messages, session.agent_message_count);
        self.with_sessions(|sessions| {
            sessions.record_agent_turn(&incoming.address, messages, &usage, &compaction)
        })
        .context("failed to persist agent_frame messages")?;
        self.with_sessions(|sessions| {
            sessions.mark_skills_loaded_current_turn(&incoming.address, &loaded_skills)
        })?;
        self.with_sessions(|sessions| {
            sessions.append_user_message(
                &incoming.address,
                incoming.text.clone(),
                stored_attachments.clone(),
            )
        })?;
        self.with_sessions(|sessions| {
            sessions.append_assistant_message(&incoming.address, outgoing.text.clone(), Vec::new())
        })?;

        let foreground = self.build_foreground_agent(&session, &effective_model_key)?;
        self.log_turn_usage(&session, &usage, false);
        info!(
            log_stream = "agent",
            log_key = %foreground.id,
            kind = "foreground_agent_replied",
            session_id = %foreground.session_id,
            channel_id = %foreground.channel_id,
            system_prompt_len = foreground.system_prompt.len() as u64,
            timed_out,
            has_text = outgoing.text.as_deref().is_some_and(|text| !text.trim().is_empty()),
            attachment_count = outgoing.attachments.len() as u64 + outgoing.images.len() as u64,
            "foreground agent produced reply"
        );

        self.send_channel_message(&channel, &incoming.address, outgoing)
            .await?;
        channel
            .set_processing(&incoming.address, ProcessingState::Idle)
            .await
            .ok();
        Ok(())
    }

    async fn materialize_attachments(
        &self,
        attachments_dir: &Path,
        attachments: Vec<crate::channel::PendingAttachment>,
    ) -> Result<Vec<StoredAttachment>> {
        let mut stored = Vec::with_capacity(attachments.len());
        for attachment in attachments {
            let item = attachment.materialize(attachments_dir).await?;
            info!(
                log_stream = "server",
                kind = "attachment_materialized",
                attachment_id = %item.id,
                path = %item.path.display(),
                size_bytes = item.size_bytes,
                "attachment persisted to session storage"
            );
            stored.push(item);
        }
        Ok(stored)
    }

    pub async fn dispatch_background_message(
        &self,
        target: SinkTarget,
        message: OutgoingMessage,
    ) -> Result<()> {
        info!(
            log_stream = "server",
            kind = "background_dispatch_requested",
            "dispatching background message"
        );
        let sink_router = self.sink_router.read().await;
        sink_router.dispatch(&self.channels, &target, message).await
    }

    pub async fn subscribe_broadcast(&self, topic: impl Into<String>, address: ChannelAddress) {
        let mut sink_router = self.sink_router.write().await;
        sink_router.subscribe(topic, address);
    }

    fn help_text_for_channel(&self, channel_id: &str) -> String {
        let commands = self
            .command_catalog
            .get(channel_id)
            .cloned()
            .unwrap_or_else(default_bot_commands);

        let mut lines = vec!["Available commands:".to_string()];
        for command in commands {
            lines.push(format!("/{:<12} {}", command.command, command.description));
        }
        lines.join("\n")
    }

    fn status_text_for_session(
        &self,
        session: &SessionSnapshot,
        model_key: &str,
    ) -> Result<String> {
        let model = self.model_config_or_main(model_key)?;
        let effective_api_timeout = session
            .api_timeout_override_seconds
            .unwrap_or(model.timeout_seconds);
        let timeout_source = if session.api_timeout_override_seconds.is_some() {
            "session override"
        } else {
            "model default"
        };
        let runtime = self.tool_runtime_for_address(&session.address)?;
        let current_context_estimate =
            estimate_current_context_tokens_for_session(&runtime, session, model_key)?;
        let current_reasoning_effort = self
            .effective_conversation_settings(&session.address)?
            .reasoning_effort
            .or_else(|| {
                model
                    .reasoning
                    .as_ref()
                    .and_then(|reasoning| reasoning.effort.clone())
            });
        let context_compaction_enabled =
            self.effective_context_compaction_enabled(&session.address)?;
        Ok(format_session_status(
            &self.main_agent.language,
            model_key,
            model,
            session,
            effective_api_timeout,
            timeout_source,
            current_context_estimate,
            current_reasoning_effort.as_deref(),
            context_compaction_enabled,
        ))
    }

    fn archive_stale_workspaces_if_needed(&self) -> Result<()> {
        let protected = self
            .with_sessions(|sessions| Ok(sessions.list_foreground_snapshots()))?
            .into_iter()
            .map(|session| session.workspace_id)
            .collect::<Vec<_>>();
        let archived = self
            .workspace_manager
            .archive_stale_workspaces(chrono::Duration::days(30), &protected)?;
        if !archived.is_empty() {
            info!(
                log_stream = "server",
                kind = "workspace_archived",
                archived_count = archived.len() as u64,
                "archived stale workspaces"
            );
        }
        Ok(())
    }

    fn activate_existing_workspace(
        &self,
        address: &ChannelAddress,
        workspace_id: &str,
    ) -> Result<SessionSnapshot> {
        self.workspace_manager.reactivate_workspace(workspace_id)?;
        let session = self.with_sessions(|sessions| {
            sessions.reset_foreground_to_workspace(address, workspace_id)
        })?;
        info!(
            log_stream = "session",
            log_key = %session.id,
            kind = "workspace_reactivated",
            channel_id = %address.channel_id,
            conversation_id = %address.conversation_id,
            workspace_id = %session.workspace_id,
            "reactivated existing workspace in a new foreground session"
        );
        Ok(session)
    }

    async fn compact_session_now(
        &self,
        session: &SessionSnapshot,
        model_key: &str,
        force: bool,
    ) -> Result<bool> {
        if (!force && !self.effective_context_compaction_enabled(&session.address)?)
            || session.agent_message_count == 0
        {
            return Ok(false);
        }
        let runtime = self.tool_runtime_for_address(&session.address)?;
        let config = runtime.build_agent_frame_config(
            session,
            &session.workspace_root,
            AgentPromptKind::MainForeground,
            model_key,
            None,
        )?;
        let extra_tools = runtime.build_extra_tools(
            session,
            AgentPromptKind::MainForeground,
            session.agent_id,
            None,
        );
        let model = self.model_config_or_main(model_key)?;
        let report = run_backend_compaction(
            model.backend,
            session.agent_messages.clone(),
            config,
            extra_tools,
        )?;
        if !report.compacted {
            return Ok(false);
        }
        let compaction_stats = compaction_stats_from_report(&report);
        self.with_sessions(|sessions| {
            sessions.record_idle_compaction(&session.address, report.messages, &compaction_stats)
        })?;
        Ok(true)
    }

    fn available_sandbox_modes(&self) -> Vec<SandboxMode> {
        let mut modes = vec![SandboxMode::Disabled, SandboxMode::Subprocess];
        if cfg!(target_os = "linux") {
            modes.push(SandboxMode::Bubblewrap);
        }
        modes
    }

    async fn initialize_foreground_session(
        &self,
        session: &SessionSnapshot,
        show_reply: bool,
    ) -> Result<OutgoingMessage> {
        let greeting = ChatMessage::text("user", greeting_for_language(&self.main_agent.language));
        let effective_model_key = self.effective_main_model_key(&session.address)?;
        let outcome = self
            .run_main_agent_turn(session, &effective_model_key, greeting, None, Vec::new())
            .await
            .context("failed to initialize foreground session")?;
        let (messages, outgoing, usage, compaction, timed_out) = match outcome {
            ForegroundTurnOutcome::Replied {
                messages,
                outgoing,
                usage,
                compaction,
                timed_out,
            } => (messages, outgoing, usage, compaction, timed_out),
            ForegroundTurnOutcome::Yielded {
                messages,
                usage,
                compaction,
            } => {
                self.with_sessions(|sessions| {
                    sessions.record_yielded_turn(&session.address, messages, &usage, &compaction)
                })?;
                return Ok(OutgoingMessage::default());
            }
            ForegroundTurnOutcome::Failed { error, .. } => return Err(error),
        };
        self.with_sessions(|sessions| {
            sessions.record_agent_turn(&session.address, messages, &usage, &compaction)
        })?;
        self.log_turn_usage(session, &usage, true);
        if timed_out {
            warn!(
                log_stream = "agent",
                log_key = %session.agent_id,
                kind = "foreground_initialization_timed_out",
                session_id = %session.id,
                channel_id = %session.address.channel_id,
                "foreground initialization returned the latest stable checkpoint after timeout"
            );
        }
        if show_reply {
            self.with_sessions(|sessions| {
                sessions.append_assistant_message(
                    &session.address,
                    outgoing.text.clone(),
                    Vec::new(),
                )
            })?;
        }
        Ok(outgoing)
    }

    async fn summarize_active_workspaces_on_shutdown(&self) -> Result<()> {
        let snapshots = self.with_sessions(|sessions| Ok(sessions.list_foreground_snapshots()))?;
        let mut first_error = None;
        for session in snapshots {
            let _ = self.with_sessions(|sessions| {
                sessions.mark_workspace_summary_state(&session.address, true, false)
            });
            if let Err(error) = self.summarize_workspace_before_destroy(&session).await {
                warn!(
                    log_stream = "session",
                    log_key = %session.id,
                    kind = "workspace_summary_on_shutdown_failed",
                    workspace_id = %session.workspace_id,
                    error = %format!("{error:#}"),
                    "failed to summarize workspace on shutdown"
                );
                if first_error.is_none() {
                    first_error = Some(error);
                }
            }
        }
        if let Some(error) = first_error {
            return Err(error);
        }
        Ok(())
    }

    async fn summarize_workspace_before_destroy(&self, session: &SessionSnapshot) -> Result<()> {
        let _summary_guard = SummaryInProgressGuard::new(Arc::clone(&self.summary_tracker));
        let entries =
            self.workspace_manager
                .list_workspace_contents(&session.workspace_id, None, 3, 200)?;
        if session.agent_message_count == 0 && entries.is_empty() {
            self.with_sessions(|sessions| {
                sessions.mark_workspace_summary_state(&session.address, false, false)
            })?;
            return Ok(());
        }

        let mut previous_messages = session.agent_messages.clone();
        let effective_model_key = self.effective_main_model_key(&session.address)?;
        let effective_model = self.model_config_or_main(&effective_model_key)?.clone();
        if self.effective_context_compaction_enabled(&session.address)?
            && session.agent_message_count > self.main_agent.retain_recent_messages
        {
            let runtime = self.tool_runtime();
            let config = runtime.build_agent_frame_config(
                session,
                &session.workspace_root,
                AgentPromptKind::MainForeground,
                &effective_model_key,
                None,
            )?;
            let extra_tools = runtime.build_extra_tools(
                session,
                AgentPromptKind::MainForeground,
                session.agent_id,
                None,
            );
            let compaction = run_backend_compaction(
                effective_model.backend,
                previous_messages.clone(),
                config,
                extra_tools,
            )?;
            if compaction.compacted {
                previous_messages = compaction.messages;
            }
        }

        let workspace = self
            .workspace_manager
            .ensure_workspace_exists(&session.workspace_id)?;
        let tree = entries
            .iter()
            .map(|entry| format!("- {} ({})", entry.path, entry.entry_type))
            .collect::<Vec<_>>()
            .join("\n");
        let prompt = format!(
            "You are about to stop working in the current workspace.\n\nSummarize the work that has been done here for future agents.\nWrite a concise durable summary in plain text.\nFocus on:\n1. what work this workspace is mainly about\n2. what kinds of changes or outputs now exist in the workspace at a high level\n3. recent progress and current status\n4. any important unfinished direction a future agent should know\n\nKeep the summary at the level of work content and broad impact on the workspace.\nDo not explain files or directories one by one, and avoid file-level detail unless a path is truly the single most important entry point.\n\nCurrent stored summary:\n{}\n\nWorkspace file tree snapshot:\n{}\n\nReturn only the summary text. Do not use attachment tags.",
            workspace.summary,
            if tree.trim().is_empty() {
                "(workspace currently has no files)"
            } else {
                &tree
            }
        );
        let timeout_seconds = self.main_agent_timeout_seconds(&effective_model_key)?;
        let upstream_timeout_seconds = self.model_upstream_timeout_seconds(&effective_model_key)?;
        let runtime = self.tool_runtime_for_address(&session.address)?;
        let outcome = runtime
            .run_agent_turn_with_timeout(
                session.clone(),
                AgentPromptKind::MainForeground,
                session.agent_id,
                effective_model_key.clone(),
                previous_messages,
                prompt,
                timeout_seconds,
                Some(upstream_timeout_seconds),
                None,
                "workspace summary",
                "workspace summary task join failed",
            )
            .await?;
        let report = match outcome {
            TimedRunOutcome::Completed(report) => report,
            TimedRunOutcome::Yielded(report) => report,
            TimedRunOutcome::TimedOut {
                checkpoint: Some(report),
                ..
            } => report,
            TimedRunOutcome::TimedOut {
                checkpoint: None,
                error,
            } => return Err(error),
            TimedRunOutcome::Failed {
                checkpoint: Some(report),
                ..
            } => report,
            TimedRunOutcome::Failed {
                checkpoint: None,
                error,
            } => return Err(error),
        };
        let summary_text = extract_assistant_text(&report.messages);
        let (clean_summary, _) =
            extract_attachment_references(&summary_text, &session.workspace_root)?;
        let clean_summary = clean_summary.trim();
        if clean_summary.is_empty() {
            self.with_sessions(|sessions| {
                sessions.mark_workspace_summary_state(&session.address, false, false)
            })?;
            return Ok(());
        }
        let updated = self.workspace_manager.update_summary(
            &session.workspace_id,
            clean_summary.to_string(),
            None,
        )?;
        self.with_sessions(|sessions| {
            sessions.mark_workspace_summary_state(&session.address, false, false)
        })?;
        info!(
            log_stream = "session",
            log_key = %session.id,
            kind = "workspace_summary_updated",
            workspace_id = %session.workspace_id,
            summary_path = %updated.summary_path.display(),
            summary_len = clean_summary.len() as u64,
            "updated workspace summary before destroy"
        );
        Ok(())
    }

    async fn retry_pending_workspace_summaries(&self) -> Result<()> {
        let pending =
            self.with_sessions(|sessions| Ok(sessions.pending_workspace_summary_snapshots()))?;
        for session in pending {
            if let Err(error) = self.summarize_workspace_before_destroy(&session).await {
                warn!(
                    log_stream = "session",
                    log_key = %session.id,
                    kind = "workspace_summary_retry_failed",
                    workspace_id = %session.workspace_id,
                    error = %format!("{error:#}"),
                    "failed to retry pending workspace summary on startup"
                );
                continue;
            }
            self.with_sessions(|sessions| {
                sessions.mark_workspace_summary_state(&session.address, false, false)
            })?;
            if session.close_after_summary {
                self.destroy_foreground_session(&session.address)?;
            }
        }
        Ok(())
    }

    async fn run_main_agent_turn(
        &self,
        session: &SessionSnapshot,
        model_key: &str,
        next_user_message: ChatMessage,
        original_user_text: Option<String>,
        original_attachments: Vec<StoredAttachment>,
    ) -> Result<ForegroundTurnOutcome> {
        let mut previous_messages = session.agent_messages.clone();
        previous_messages.push(next_user_message);
        self.run_main_agent_turn_with_previous_messages(
            session,
            model_key,
            previous_messages,
            original_user_text,
            original_attachments,
        )
        .await
    }

    async fn run_main_agent_turn_with_previous_messages(
        &self,
        session: &SessionSnapshot,
        model_key: &str,
        previous_messages: Vec<ChatMessage>,
        original_user_text: Option<String>,
        original_attachments: Vec<StoredAttachment>,
    ) -> Result<ForegroundTurnOutcome> {
        let workspace_root = session.workspace_root.clone();
        let timeout_seconds = self.main_agent_timeout_seconds(model_key)?;
        let upstream_timeout_seconds = session
            .api_timeout_override_seconds
            .unwrap_or(self.model_upstream_timeout_seconds(model_key)?);
        let runtime = self.tool_runtime_for_address(&session.address)?;
        let active_controls = Arc::clone(&self.active_foreground_controls);
        let session_key = session.address.session_key();
        let control_observer: Arc<dyn Fn(SessionExecutionControl) + Send + Sync> =
            Arc::new(move |control| {
                if let Ok(mut controls) = active_controls.lock() {
                    controls.insert(session_key.clone(), control);
                }
            });
        let run_result = runtime
            .run_agent_turn_with_timeout(
                session.clone(),
                AgentPromptKind::MainForeground,
                session.agent_id,
                model_key.to_string(),
                previous_messages.clone(),
                String::new(),
                timeout_seconds,
                Some(upstream_timeout_seconds),
                Some(control_observer),
                "foreground agent turn",
                "agent_frame task join failed",
            )
            .await;
        self.unregister_active_foreground_control(&session.address)?;
        let run_result = run_result?;

        match run_result {
            TimedRunOutcome::Completed(report) => {
                let assistant_text = extract_assistant_text(&report.messages);
                let outgoing =
                    build_outgoing_message_for_session(session, &assistant_text, &workspace_root)?;
                Ok(ForegroundTurnOutcome::Replied {
                    messages: report.messages,
                    outgoing,
                    usage: report.usage,
                    compaction: report.compaction,
                    timed_out: false,
                })
            }
            TimedRunOutcome::Yielded(report) => Ok(ForegroundTurnOutcome::Yielded {
                messages: report.messages,
                usage: report.usage,
                compaction: report.compaction,
            }),
            TimedRunOutcome::TimedOut { checkpoint, error } => {
                let report = checkpoint.ok_or(error)?;
                let assistant_text = extract_assistant_text(&report.messages);
                let outgoing =
                    build_outgoing_message_for_session(session, &assistant_text, &workspace_root)?;
                Ok(ForegroundTurnOutcome::Replied {
                    messages: report.messages,
                    outgoing,
                    usage: report.usage,
                    compaction: report.compaction,
                    timed_out: true,
                })
            }
            TimedRunOutcome::Failed { checkpoint, error } => {
                let resume_messages = checkpoint
                    .map(|report| report.messages)
                    .unwrap_or_else(|| previous_messages.clone());
                Ok(ForegroundTurnOutcome::Failed {
                    pending_continue: PendingContinueState {
                        model_key: model_key.to_string(),
                        progress_summary: summarize_resume_progress(&resume_messages),
                        error_summary: format!("{error:#}"),
                        failed_at: Utc::now(),
                        resume_messages,
                        original_user_text,
                        original_attachments,
                    },
                    error,
                })
            }
        }
    }

    fn effective_conversation_settings(
        &self,
        address: &ChannelAddress,
    ) -> Result<ConversationSettings> {
        Ok(self
            .with_conversations(|conversations| Ok(conversations.get_snapshot(address)))?
            .map(|snapshot| snapshot.settings)
            .unwrap_or_default())
    }

    fn selected_main_model_key(&self, address: &ChannelAddress) -> Result<Option<String>> {
        Ok(self.effective_conversation_settings(address)?.main_model)
    }

    fn effective_main_model_key(&self, address: &ChannelAddress) -> Result<String> {
        self.selected_main_model_key(address)?.ok_or_else(|| {
            anyhow!("this conversation does not have a main model yet; choose one with /model")
        })
    }

    fn effective_sandbox_mode(&self, address: &ChannelAddress) -> Result<SandboxMode> {
        let settings = self.effective_conversation_settings(address)?;
        Ok(settings.sandbox_mode.unwrap_or(self.sandbox.mode))
    }

    fn effective_context_compaction_enabled(&self, address: &ChannelAddress) -> Result<bool> {
        Ok(self
            .effective_conversation_settings(address)?
            .context_compaction_enabled
            .unwrap_or(self.main_agent.enable_context_compression))
    }

    fn model_selection_message(
        &self,
        address: &ChannelAddress,
        intro: &str,
    ) -> Result<OutgoingMessage> {
        let current = self.selected_main_model_key(address)?;
        let mut options = self
            .chat_model_keys
            .iter()
            .filter(|model_key| self.models.contains_key(model_key.as_str()))
            .cloned()
            .map(|model_key| ShowOption {
                label: model_key.clone(),
                value: format!("/model {}", model_key),
            })
            .collect::<Vec<_>>();
        options.sort_by(|left, right| left.label.cmp(&right.label));
        Ok(OutgoingMessage::with_options(
            format!(
                "{}\nCurrent conversation model: {}\nChoose a model below or send `/model <name>`.",
                intro,
                current
                    .map(|value| format!("`{}`", value))
                    .unwrap_or_else(|| "`<not selected>`".to_string())
            ),
            "Choose a model",
            options,
        ))
    }

    fn reasoning_effort_message(&self, address: &ChannelAddress) -> Result<OutgoingMessage> {
        let settings = self.effective_conversation_settings(address)?;
        let effective_effort = settings.reasoning_effort.clone().or_else(|| {
            self.selected_main_model_key(address)
                .ok()
                .flatten()
                .and_then(|model_key| {
                    self.models.get(&model_key).and_then(|model| {
                        model
                            .reasoning
                            .as_ref()
                            .and_then(|reasoning| reasoning.effort.clone())
                    })
                })
        });
        let options = ["low", "medium", "high"]
            .into_iter()
            .map(|effort| ShowOption {
                label: effort.to_string(),
                value: format!("/think {}", effort),
            })
            .chain(std::iter::once(ShowOption {
                label: "default".to_string(),
                value: "/think default".to_string(),
            }))
            .collect::<Vec<_>>();
        Ok(OutgoingMessage::with_options(
            format!(
                "Current conversation reasoning effort: {}\nChoose an option below or send `/think <level>`.",
                effective_effort
                    .map(|value| format!("`{}`", value))
                    .unwrap_or_else(|| "`default`".to_string())
            ),
            "Choose a reasoning effort",
            options,
        ))
    }

    fn compact_mode_message(&self, address: &ChannelAddress) -> Result<OutgoingMessage> {
        let enabled = self.effective_context_compaction_enabled(address)?;
        let options = vec![
            ShowOption {
                label: "enabled".to_string(),
                value: "/compact_mode on".to_string(),
            },
            ShowOption {
                label: "disabled".to_string(),
                value: "/compact_mode off".to_string(),
            },
        ];
        Ok(OutgoingMessage::with_options(
            format!(
                "Automatic context compaction for this conversation is currently `{}`.\nChoose a mode below or send `/compact_mode <on|off>`.\nYou can always trigger a one-off compaction with `/compact`.",
                if enabled { "enabled" } else { "disabled" }
            ),
            "Choose automatic compaction mode",
            options,
        ))
    }

    fn model_config_or_main(&self, model_key: &str) -> Result<&ModelConfig> {
        self.models
            .get(model_key)
            .with_context(|| format!("unknown model {}", model_key))
    }

    fn main_agent_timeout_seconds(&self, model_key: &str) -> Result<Option<f64>> {
        if let Some(timeout_seconds) = self.main_agent.timeout_seconds {
            return Ok((timeout_seconds > 0.0).then_some(timeout_seconds));
        }
        Ok(Some(background_agent_timeout_seconds(
            self.models
                .get(model_key)
                .with_context(|| format!("unknown model {}", model_key))?
                .timeout_seconds,
        )))
    }

    fn model_upstream_timeout_seconds(&self, model_key: &str) -> Result<f64> {
        Ok(self
            .models
            .get(model_key)
            .with_context(|| format!("unknown model {}", model_key))?
            .timeout_seconds)
    }

    fn build_foreground_agent(
        &self,
        session: &SessionSnapshot,
        model_key: &str,
    ) -> Result<ForegroundAgent> {
        let model = self.model_config_or_main(model_key)?;
        let commands = self
            .command_catalog
            .get(&session.address.channel_id)
            .cloned()
            .unwrap_or_else(default_bot_commands);
        Ok(ForegroundAgent {
            id: session.agent_id,
            session_id: session.id,
            channel_id: session.address.channel_id.clone(),
            system_prompt: build_agent_system_prompt(
                &self.agent_workspace,
                session,
                &self
                    .workspace_manager
                    .ensure_workspace_exists(&session.workspace_id)
                    .map(|workspace| workspace.summary)
                    .unwrap_or_default(),
                AgentPromptKind::MainForeground,
                model_key,
                model,
                &self.models,
                &self.chat_model_keys,
                &self.main_agent,
                &commands,
            ),
        })
    }

    fn current_runtime_skill_observations(&self) -> Result<Vec<SessionSkillObservation>> {
        let discovered = discover_skills(std::slice::from_ref(&self.agent_workspace.skills_dir))?;
        let mut observed = Vec::with_capacity(discovered.len());
        for skill in discovered {
            let skill_file = skill.path.join("SKILL.md");
            let content = std::fs::read_to_string(&skill_file)
                .with_context(|| format!("failed to read {}", skill_file.display()))?;
            observed.push(SessionSkillObservation {
                name: skill.name,
                description: skill.description,
                content,
            });
        }
        observed.sort_by(|left, right| left.name.cmp(&right.name));
        Ok(observed)
    }

    fn observe_runtime_skill_changes(&self, session: &SessionSnapshot) -> Result<Option<String>> {
        let observed = self.current_runtime_skill_observations()?;
        let notices = self.with_sessions(|sessions| {
            sessions.observe_skill_changes(&session.address, &observed)
        })?;
        let rendered = render_skill_change_notices(&notices);
        Ok((!rendered.is_empty()).then_some(rendered))
    }

    fn should_auto_close_conversation_after_send_error(
        &self,
        address: &ChannelAddress,
        error: &anyhow::Error,
    ) -> bool {
        if !address.conversation_id.starts_with('-') {
            return false;
        }
        let message = format!("{error:#}").to_ascii_lowercase();
        message.contains("bot was kicked from the group chat")
            || message.contains("chat not found")
            || message.contains("group chat was deleted")
            || message.contains("bot is not a member of the channel chat")
            || message.contains("forbidden: bot was kicked")
    }

    async fn send_channel_message(
        &self,
        channel: &Arc<dyn Channel>,
        address: &ChannelAddress,
        message: OutgoingMessage,
    ) -> Result<()> {
        match channel.send(address, message).await {
            Ok(()) => Ok(()),
            Err(error) => {
                if self.should_auto_close_conversation_after_send_error(address, &error) {
                    warn!(
                        log_stream = "session",
                        kind = "channel_send_closed_conversation",
                        channel_id = %address.channel_id,
                        conversation_id = %address.conversation_id,
                        error = %format!("{error:#}"),
                        "channel send indicates the conversation no longer exists; closing foreground session"
                    );
                    self.destroy_foreground_session(address)?;
                    let disabled = self.disable_cron_tasks_for_conversation(address)?;
                    if disabled > 0 {
                        warn!(
                            log_stream = "cron",
                            kind = "cron_tasks_auto_disabled_after_send_error",
                            channel_id = %address.channel_id,
                            conversation_id = %address.conversation_id,
                            disabled_count = disabled as u64,
                            "disabled cron tasks because the conversation no longer exists"
                        );
                    }
                }
                Err(error)
            }
        }
    }

    fn disable_cron_tasks_for_conversation(&self, address: &ChannelAddress) -> Result<usize> {
        let mut manager = self
            .cron_manager
            .lock()
            .map_err(|_| anyhow!("cron manager lock poisoned"))?;
        manager.disable_for_address(address)
    }

    async fn send_user_error_message(
        &self,
        channel: &Arc<dyn Channel>,
        address: &ChannelAddress,
        error: &anyhow::Error,
    ) {
        let text = user_facing_error_text(&self.main_agent.language, error);
        if let Err(send_error) = self
            .send_channel_message(channel, address, OutgoingMessage::text(text))
            .await
        {
            error!(
                log_stream = "server",
                kind = "send_user_error_failed",
                error = %format!("{send_error:#}"),
                "failed to send user-facing error message"
            );
        }
    }

    fn log_turn_usage(&self, session: &SessionSnapshot, usage: &TokenUsage, initialization: bool) {
        log_turn_usage(
            session.agent_id,
            session,
            usage,
            initialization,
            "main_foreground",
            None,
        );
    }
}

fn channel_restart_backoff_seconds(consecutive_failures: u32) -> u64 {
    let exponent = consecutive_failures.saturating_sub(1).min(5);
    2_u64
        .saturating_pow(exponent)
        .min(CHANNEL_RESTART_MAX_BACKOFF_SECONDS)
        .max(1)
}

fn spawn_channel_supervisor(channel: Arc<dyn Channel>, sender: mpsc::Sender<IncomingMessage>) {
    info!(
        log_stream = "channel",
        log_key = %channel.id(),
        kind = "channel_starting",
        "starting channel supervisor"
    );
    tokio::spawn(async move {
        let channel_id = channel.id().to_string();
        let mut consecutive_failures = 0u32;
        loop {
            match channel.clone().run(sender.clone()).await {
                Ok(()) => {
                    warn!(
                        log_stream = "channel",
                        log_key = %channel_id,
                        kind = "channel_exited",
                        "channel task exited without error; restarting"
                    );
                }
                Err(error) => {
                    consecutive_failures = consecutive_failures.saturating_add(1);
                    let backoff_seconds = channel_restart_backoff_seconds(consecutive_failures);
                    error!(
                        log_stream = "channel",
                        log_key = %channel_id,
                        kind = "channel_stopped",
                        error = %format!("{error:#}"),
                        consecutive_failures = consecutive_failures,
                        backoff_seconds = backoff_seconds,
                        "channel task stopped with error; restarting"
                    );
                    tokio::time::sleep(Duration::from_secs(backoff_seconds)).await;
                    continue;
                }
            }
            let backoff_seconds = channel_restart_backoff_seconds(consecutive_failures.max(1));
            tokio::time::sleep(Duration::from_secs(backoff_seconds)).await;
        }
    });
}

fn compose_user_prompt(text: Option<&str>, attachments: &[StoredAttachment]) -> String {
    let mut sections = Vec::new();
    if let Some(text) = text.map(str::trim).filter(|value| !value.is_empty()) {
        sections.push(text.to_string());
    }
    if !attachments.is_empty() {
        let mut attachment_lines = vec!["Attachments available for this turn:".to_string()];
        for attachment in attachments {
            attachment_lines.push(format!(
                "- kind={:?}, path={}, original_name={}, media_type={}",
                attachment.kind,
                attachment.path.display(),
                attachment.original_name.as_deref().unwrap_or("unknown"),
                attachment.media_type.as_deref().unwrap_or("unknown")
            ));
        }
        attachment_lines.push(
            "Use tools if you need to inspect any text attachment or related files.".to_string(),
        );
        sections.push(attachment_lines.join("\n"));
    }
    if sections.is_empty() {
        "(No text content; inspect attachments if needed.)".to_string()
    } else {
        sections.join("\n\n")
    }
}

fn coalesce_buffered_conversation_messages(
    initial: IncomingMessage,
    pending_messages: &mut VecDeque<IncomingMessage>,
) -> IncomingMessage {
    if initial.control.is_some() {
        return initial;
    }

    let mut grouped = vec![initial];
    let mut remaining = VecDeque::new();
    while let Some(candidate) = pending_messages.pop_front() {
        if candidate.control.is_none() && candidate.address == grouped[0].address {
            grouped.push(candidate);
        } else {
            remaining.push_back(candidate);
        }
    }
    *pending_messages = remaining;
    merge_buffered_messages(grouped)
}

fn merge_buffered_messages(mut grouped: Vec<IncomingMessage>) -> IncomingMessage {
    if grouped.len() == 1 {
        return grouped.remove(0);
    }

    let remote_message_id = grouped
        .last()
        .map(|message| message.remote_message_id.clone())
        .expect("grouped messages should not be empty");
    let address = grouped
        .last()
        .map(|message| message.address.clone())
        .expect("grouped messages should not be empty");
    let mut flattened = Vec::new();
    let mut attachments = Vec::new();
    for message in grouped.drain(..) {
        flattened.push((message.text, message.attachments.len()));
        attachments.extend(message.attachments);
    }

    IncomingMessage {
        remote_message_id,
        address,
        text: Some(render_buffered_followup_messages(
            &flattened
                .iter()
                .map(|(text, attachment_count)| (text.as_deref(), *attachment_count))
                .collect::<Vec<_>>(),
        )),
        attachments,
        control: None,
    }
}

fn render_buffered_followup_messages(messages: &[(Option<&str>, usize)]) -> String {
    let mut sections = vec![
        "[Queued User Updates]".to_string(),
        "While you were still working on the previous turn, the user sent multiple follow-up messages. Treat later items as newer steering updates when they conflict.".to_string(),
    ];
    for (index, (text, attachment_count)) in messages.iter().enumerate() {
        let trimmed = text.map(str::trim).unwrap_or("");
        let body = match (trimmed, *attachment_count) {
            ("", 0) => "[empty message]".to_string(),
            ("", count) => format!("[attachments only: {count}]"),
            (value, 0) => value.to_string(),
            (value, count) => format!("{value}\n[attachments: {count}]"),
        };
        sections.push(format!("Follow-up {}:\n{}", index + 1, body));
    }
    sections.join("\n\n")
}

fn request_yield_for_incoming(
    active_controls: &Arc<Mutex<HashMap<String, SessionExecutionControl>>>,
    message: &IncomingMessage,
) -> bool {
    if message.control.is_some() {
        return false;
    }
    let session_key = message.address.session_key();
    let control = active_controls
        .lock()
        .ok()
        .and_then(|controls| controls.get(&session_key).cloned());
    if let Some(control) = control {
        control.request_yield();
        true
    } else {
        false
    }
}

fn tag_interrupted_followup_text(text: Option<String>) -> Option<String> {
    let marker = "[Interrupted Follow-up]";
    match text {
        Some(text) if !text.trim().is_empty() => Some(format!("{marker}\n{text}")),
        _ => Some(marker.to_string()),
    }
}

fn fast_path_model_selection_message(
    workdir: &Path,
    models: &BTreeMap<String, ModelConfig>,
    chat_model_keys: &[String],
    message: &IncomingMessage,
) -> Option<OutgoingMessage> {
    if message.control.is_some() {
        return None;
    }
    let text = message.text.as_deref()?.trim();
    if text.is_empty() {
        return None;
    }
    if text.starts_with('/') {
        return None;
    }

    let settings = ConversationManager::new(workdir)
        .ok()
        .and_then(|manager| manager.get_snapshot(&message.address))
        .map(|snapshot| snapshot.settings)
        .unwrap_or_default();
    if settings.main_model.is_some() {
        return None;
    }

    let mut options = chat_model_keys
        .iter()
        .filter(|model_key| models.contains_key(model_key.as_str()))
        .cloned()
        .map(|model_key| ShowOption {
            label: model_key.clone(),
            value: format!("/model {}", model_key),
        })
        .collect::<Vec<_>>();
    options.sort_by(|left, right| left.label.cmp(&right.label));
    Some(OutgoingMessage::with_options(
        "This conversation has no model yet. Choose one to start a new session.\nCurrent conversation model: `<not selected>`\nChoose a model below or send `/model <name>`.",
        "Choose a model",
        options,
    ))
}

fn build_user_turn_message(
    text: Option<&str>,
    skill_updates_prefix: Option<&str>,
    attachments: &[StoredAttachment],
    model: &ModelConfig,
    backend_supports_native_multimodal: bool,
) -> Result<ChatMessage> {
    let image_attachments = attachments
        .iter()
        .filter(|attachment| attachment.kind == AttachmentKind::Image)
        .collect::<Vec<_>>();
    if !backend_supports_native_multimodal
        || !model.supports_vision_input
        || image_attachments.is_empty()
    {
        let merged_text = merge_user_text(skill_updates_prefix, text);
        return Ok(ChatMessage::text(
            "user",
            compose_user_prompt(merged_text.as_deref(), attachments),
        ));
    }

    let mut text_sections = Vec::new();
    if let Some(text) = merge_user_text(skill_updates_prefix, text) {
        text_sections.push(text.to_string());
    }

    let file_attachments = attachments
        .iter()
        .filter(|attachment| attachment.kind != AttachmentKind::Image)
        .collect::<Vec<_>>();
    if !file_attachments.is_empty() {
        let mut attachment_lines =
            vec!["Non-image attachments available for this turn:".to_string()];
        for attachment in file_attachments {
            attachment_lines.push(format!(
                "- kind={:?}, path={}, original_name={}, media_type={}",
                attachment.kind,
                attachment.path.display(),
                attachment.original_name.as_deref().unwrap_or("unknown"),
                attachment.media_type.as_deref().unwrap_or("unknown")
            ));
        }
        attachment_lines.push(
            "Use tools if you need to inspect any non-image attachment or related files."
                .to_string(),
        );
        text_sections.push(attachment_lines.join("\n"));
    }

    if text_sections.is_empty() {
        text_sections.push(format!(
            "The user attached {} image(s). Inspect the images directly.",
            image_attachments.len()
        ));
    } else {
        text_sections.push(format!(
            "The user attached {} image(s), and those images are already directly visible in this request. Inspect them directly here instead of calling the image tool again for the same current-turn attachments.",
            image_attachments.len()
        ));
    }

    let mut content = vec![json!({
        "type": "text",
        "text": text_sections.join("\n\n")
    })];
    for image in image_attachments {
        content.push(json!({
            "type": "image_url",
            "image_url": {
                "url": build_image_data_url(image)?,
            }
        }));
    }

    Ok(ChatMessage {
        role: "user".to_string(),
        content: Some(Value::Array(content)),
        name: None,
        tool_call_id: None,
        tool_calls: None,
    })
}

fn merge_user_text<'a>(
    skill_updates_prefix: Option<&'a str>,
    text: Option<&'a str>,
) -> Option<String> {
    let skill_updates_prefix = skill_updates_prefix
        .map(str::trim)
        .filter(|value| !value.is_empty());
    let text = text.map(str::trim).filter(|value| !value.is_empty());
    match (skill_updates_prefix, text) {
        (Some(prefix), Some(text)) => Some(format!("{prefix}\n\n[User Message]\n{text}")),
        (Some(prefix), None) => Some(prefix.to_string()),
        (None, Some(text)) => Some(text.to_string()),
        (None, None) => None,
    }
}

fn build_previous_messages_for_turn(
    session_agent_messages: &[ChatMessage],
    pending_continue: Option<&PendingContinueState>,
    next_user_message: Option<ChatMessage>,
) -> Vec<ChatMessage> {
    let mut previous_messages = pending_continue
        .map(|pending| pending.resume_messages.clone())
        .unwrap_or_else(|| session_agent_messages.to_vec());
    if let Some(next_user_message) = next_user_message {
        previous_messages.push(next_user_message);
    }
    previous_messages
}

fn render_skill_change_notices(notices: &[SkillChangeNotice]) -> String {
    if notices.is_empty() {
        return String::new();
    }
    let mut sections = vec![
        "[Runtime Skill Updates]".to_string(),
        "The global skill registry changed since earlier in this session. Apply these updates before handling the user's new request.".to_string(),
    ];
    for notice in notices {
        match notice {
            SkillChangeNotice::DescriptionChanged { name, description } => {
                sections.push(format!(
                    "Skill \"{name}\" has an updated description:\n{description}"
                ));
            }
            SkillChangeNotice::ContentChanged {
                name,
                description,
                content,
            } => {
                sections.push(format!(
                    "Skill \"{name}\" changed after it was loaded earlier in this session and before that load was compacted away. Use the refreshed skill immediately.\nUpdated description: {description}\nRefreshed SKILL.md content:\n{content}"
                ));
            }
        }
    }
    sections.join("\n\n")
}

fn extract_loaded_skill_names(
    messages: &[ChatMessage],
    previous_message_count: usize,
) -> Vec<String> {
    let mut skill_names = Vec::new();
    for message in messages.iter().skip(previous_message_count) {
        if message.role != "tool" {
            continue;
        }
        let Some(tool_name) = message.name.as_deref() else {
            continue;
        };
        if tool_name != "skill_load" {
            continue;
        }
        let Some(content) = message.content.as_ref().and_then(|value| value.as_str()) else {
            continue;
        };
        let Ok(parsed) = serde_json::from_str::<Value>(content) else {
            continue;
        };
        let Some(skill_name) = parsed.get("name").and_then(|value| value.as_str()) else {
            continue;
        };
        if !skill_names.iter().any(|existing| existing == skill_name) {
            skill_names.push(skill_name.to_string());
        }
    }
    skill_names
}

fn build_image_data_url(attachment: &StoredAttachment) -> Result<String> {
    let bytes = std::fs::read(&attachment.path).with_context(|| {
        format!(
            "failed to read image attachment {}",
            attachment.path.display()
        )
    })?;
    let mime_type = attachment
        .media_type
        .as_deref()
        .filter(|value| value.starts_with("image/"))
        .map(ToOwned::to_owned)
        .unwrap_or_else(|| infer_image_media_type(&attachment.path));
    let encoded = base64::engine::general_purpose::STANDARD.encode(bytes);
    Ok(format!("data:{};base64,{}", mime_type, encoded))
}

fn infer_image_media_type(path: &Path) -> String {
    match path
        .extension()
        .and_then(|value| value.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase()
        .as_str()
    {
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "bmp" => "image/bmp",
        _ => "image/png",
    }
    .to_string()
}

fn spawn_processing_keepalive(
    channel: Arc<dyn Channel>,
    address: ChannelAddress,
    state: ProcessingState,
) -> Option<oneshot::Sender<()>> {
    let keepalive_interval = channel.processing_keepalive_interval(state)?;
    let (stop_sender, mut stop_receiver) = oneshot::channel();
    tokio::spawn(async move {
        let mut ticker = interval(keepalive_interval);
        ticker.set_missed_tick_behavior(MissedTickBehavior::Skip);
        loop {
            tokio::select! {
                        _ = &mut stop_receiver => break,
                        _ = ticker.tick() => {
                            if let Err(error) = channel.set_processing(&address, state).await {
                                warn!(
                                    log_stream = "channel",
                                    log_key = %channel.id(),
                                    kind = "processing_keepalive_failed",
                                    conversation_id = %address.conversation_id,
                                    error = %format!("{error:#}"),
                                    "processing keepalive failed"
                                );
                                break;
                    }
                }
            }
        }
    });
    Some(stop_sender)
}

fn extract_attachment_references(
    assistant_text: &str,
    workspace_root: &Path,
) -> Result<(String, Vec<OutgoingAttachment>)> {
    let mut clean = String::new();
    let mut remainder = assistant_text;
    let mut found_paths = Vec::new();

    loop {
        let Some(open_index) = remainder.find(ATTACHMENT_OPEN_TAG) else {
            clean.push_str(remainder);
            break;
        };
        clean.push_str(&remainder[..open_index]);
        let after_open = &remainder[open_index + ATTACHMENT_OPEN_TAG.len()..];
        let Some(close_index) = after_open.find(ATTACHMENT_CLOSE_TAG) else {
            clean.push_str(&remainder[open_index..]);
            break;
        };
        let path_text = after_open[..close_index].trim();
        if !path_text.is_empty() {
            found_paths.push(path_text.to_string());
        }
        remainder = &after_open[close_index + ATTACHMENT_CLOSE_TAG.len()..];
    }

    let attachments = found_paths
        .into_iter()
        .map(|path_text| resolve_outgoing_attachment(workspace_root, &path_text))
        .collect::<Result<Vec<_>>>()?;

    Ok((clean.trim().to_string(), attachments))
}

fn resolve_outgoing_attachment(
    workspace_root: &Path,
    relative_path: &str,
) -> Result<OutgoingAttachment> {
    let candidate = PathBuf::from(relative_path);
    if candidate.is_absolute() {
        return Err(anyhow::anyhow!(
            "attachment path must be relative to workspace root, got absolute path {}",
            candidate.display()
        ));
    }

    let joined = workspace_root.join(&candidate);
    let canonical_root = std::fs::canonicalize(workspace_root)
        .with_context(|| format!("failed to canonicalize {}", workspace_root.display()))?;
    let canonical_file = std::fs::canonicalize(&joined)
        .with_context(|| format!("attachment path does not exist: {}", joined.display()))?;
    if !canonical_file.starts_with(&canonical_root) {
        return Err(anyhow::anyhow!(
            "attachment path escapes workspace root: {}",
            canonical_file.display()
        ));
    }

    let extension = canonical_file
        .extension()
        .and_then(|value| value.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase();
    let kind = match extension.as_str() {
        "png" | "jpg" | "jpeg" | "gif" | "webp" | "bmp" => AttachmentKind::Image,
        _ => AttachmentKind::File,
    };

    Ok(OutgoingAttachment {
        kind,
        path: canonical_file,
        caption: None,
    })
}

fn build_outgoing_message_for_session(
    session: &SessionSnapshot,
    assistant_text: &str,
    workspace_root: &Path,
) -> Result<OutgoingMessage> {
    let (clean_text, attachments) = extract_attachment_references(assistant_text, workspace_root)?;
    let mut outgoing = OutgoingMessage {
        text: if clean_text.trim().is_empty() {
            None
        } else {
            Some(clean_text)
        },
        images: Vec::new(),
        attachments: Vec::new(),
        options: None,
    };
    for attachment in attachments {
        let attachment = persist_outgoing_attachment(session, attachment)?;
        match attachment.kind {
            AttachmentKind::Image => outgoing.images.push(attachment),
            AttachmentKind::File => outgoing.attachments.push(attachment),
        }
    }
    Ok(outgoing)
}

fn persist_outgoing_attachment(
    session: &SessionSnapshot,
    attachment: OutgoingAttachment,
) -> Result<OutgoingAttachment> {
    let outgoing_dir = session.root_dir.join("outgoing");
    std::fs::create_dir_all(&outgoing_dir)
        .with_context(|| format!("failed to create {}", outgoing_dir.display()))?;
    let file_name = attachment
        .path
        .file_name()
        .map(|value| value.to_os_string())
        .unwrap_or_else(|| format!("attachment-{}", uuid::Uuid::new_v4()).into());
    let persisted_path = outgoing_dir.join(file_name);
    std::fs::copy(&attachment.path, &persisted_path).with_context(|| {
        format!(
            "failed to copy outgoing attachment {} to {}",
            attachment.path.display(),
            persisted_path.display()
        )
    })?;
    Ok(OutgoingAttachment {
        kind: attachment.kind,
        path: persisted_path,
        caption: attachment.caption,
    })
}

fn relative_attachment_path(workspace_root: &Path, path: &Path) -> Result<String> {
    let relative = path.strip_prefix(workspace_root).with_context(|| {
        format!(
            "path {} is not under {}",
            path.display(),
            workspace_root.display()
        )
    })?;
    Ok(relative.to_string_lossy().to_string())
}

fn parse_sink_target(value: &Value, default_target: Option<SinkTarget>) -> Result<SinkTarget> {
    let object = value
        .as_object()
        .ok_or_else(|| anyhow!("sink must be an object"))?;
    let kind = object
        .get("kind")
        .and_then(Value::as_str)
        .unwrap_or("direct");
    match kind {
        "current_session" => default_target
            .ok_or_else(|| anyhow!("current_session sink requires a default session target")),
        "direct" => Ok(SinkTarget::Direct(ChannelAddress {
            channel_id: object
                .get("channel_id")
                .and_then(Value::as_str)
                .map(ToOwned::to_owned)
                .or_else(|| match &default_target {
                    Some(SinkTarget::Direct(address)) => Some(address.channel_id.clone()),
                    _ => None,
                })
                .ok_or_else(|| anyhow!("direct sink requires channel_id"))?,
            conversation_id: object
                .get("conversation_id")
                .and_then(Value::as_str)
                .map(ToOwned::to_owned)
                .or_else(|| match &default_target {
                    Some(SinkTarget::Direct(address)) => Some(address.conversation_id.clone()),
                    _ => None,
                })
                .ok_or_else(|| anyhow!("direct sink requires conversation_id"))?,
            user_id: object
                .get("user_id")
                .and_then(Value::as_str)
                .map(ToOwned::to_owned)
                .or_else(|| match &default_target {
                    Some(SinkTarget::Direct(address)) => address.user_id.clone(),
                    _ => None,
                }),
            display_name: object
                .get("display_name")
                .and_then(Value::as_str)
                .map(ToOwned::to_owned)
                .or_else(|| match &default_target {
                    Some(SinkTarget::Direct(address)) => address.display_name.clone(),
                    _ => None,
                }),
        })),
        "broadcast" => Ok(SinkTarget::Broadcast(
            object
                .get("topic")
                .and_then(Value::as_str)
                .map(ToOwned::to_owned)
                .filter(|value| !value.trim().is_empty())
                .ok_or_else(|| anyhow!("broadcast sink requires topic"))?,
        )),
        "multi" => {
            let targets = object
                .get("targets")
                .and_then(Value::as_array)
                .ok_or_else(|| anyhow!("multi sink requires targets"))?;
            let parsed = targets
                .iter()
                .map(|target| parse_sink_target(target, default_target.clone()))
                .collect::<Result<Vec<_>>>()?;
            Ok(SinkTarget::Multi(parsed))
        }
        other => Err(anyhow!("unsupported sink kind {}", other)),
    }
}

fn sink_target_to_value(target: &SinkTarget) -> Value {
    match target {
        SinkTarget::Direct(address) => json!({
            "kind": "direct",
            "channel_id": address.channel_id,
            "conversation_id": address.conversation_id,
            "user_id": address.user_id,
            "display_name": address.display_name
        }),
        SinkTarget::Broadcast(topic) => json!({
            "kind": "broadcast",
            "topic": topic
        }),
        SinkTarget::Multi(targets) => json!({
            "kind": "multi",
            "targets": targets.iter().map(sink_target_to_value).collect::<Vec<_>>()
        }),
    }
}

fn parse_uuid_arg(arguments: &serde_json::Map<String, Value>, key: &str) -> Result<uuid::Uuid> {
    let value = arguments
        .get(key)
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("{} must be a string UUID", key))?;
    uuid::Uuid::parse_str(value).with_context(|| format!("{} must be a valid UUID", key))
}

fn string_arg_required(arguments: &serde_json::Map<String, Value>, key: &str) -> Result<String> {
    arguments
        .get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned)
        .ok_or_else(|| anyhow!("{} must be a non-empty string", key))
}

fn optional_string_arg(
    arguments: &serde_json::Map<String, Value>,
    key: &str,
) -> Result<Option<String>> {
    match arguments.get(key) {
        Some(value) => value
            .as_str()
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(ToOwned::to_owned)
            .map(Some)
            .ok_or_else(|| anyhow!("{} must be a non-empty string", key)),
        None => Ok(None),
    }
}

fn parse_checker_from_tool_args(
    arguments: &serde_json::Map<String, Value>,
) -> Result<Option<CronCheckerConfig>> {
    let Some(command) = arguments.get("checker_command").and_then(Value::as_str) else {
        return Ok(None);
    };
    let command = command.trim();
    if command.is_empty() {
        return Err(anyhow!("checker_command must not be empty"));
    }
    let timeout_seconds = arguments
        .get("checker_timeout_seconds")
        .and_then(Value::as_f64)
        .unwrap_or(30.0);
    let cwd = arguments
        .get("checker_cwd")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned);
    Ok(Some(CronCheckerConfig {
        command: command.to_string(),
        timeout_seconds,
        cwd,
    }))
}

fn normalize_sink_target(target: SinkTarget, session: &SessionSnapshot) -> SinkTarget {
    match target {
        SinkTarget::Direct(address) => {
            if address.channel_id == session.address.channel_id
                && address.conversation_id == session.id.to_string()
            {
                warn!(
                    log_stream = "agent",
                    log_key = %session.agent_id,
                    kind = "background_sink_normalized",
                    session_id = %session.id,
                    channel_id = %session.address.channel_id,
                    incorrect_conversation_id = %address.conversation_id,
                    corrected_conversation_id = %session.address.conversation_id,
                    "background agent sink used session_id as conversation_id; correcting to the current channel conversation"
                );
                SinkTarget::Direct(session.address.clone())
            } else {
                SinkTarget::Direct(address)
            }
        }
        SinkTarget::Broadcast(topic) => SinkTarget::Broadcast(topic),
        SinkTarget::Multi(targets) => SinkTarget::Multi(
            targets
                .into_iter()
                .map(|target| normalize_sink_target(target, session))
                .collect(),
        ),
    }
}

fn evaluate_cron_checker(checker: &CronCheckerConfig, workspace_root: &Path) -> Result<bool> {
    let cwd = checker
        .cwd
        .as_deref()
        .map(PathBuf::from)
        .map(|path| {
            if path.is_absolute() {
                path
            } else {
                workspace_root.join(path)
            }
        })
        .unwrap_or_else(|| workspace_root.to_path_buf());
    let command = checker.command.clone();
    let timeout_seconds = checker.timeout_seconds;
    let (sender, receiver) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let result = Command::new("sh")
            .arg("-c")
            .arg(&command)
            .current_dir(&cwd)
            .output()
            .with_context(|| format!("failed to execute checker in {}", cwd.display()))
            .map(|output| output.status.success());
        let _ = sender.send(result);
    });
    receiver
        .recv_timeout(Duration::from_secs_f64(timeout_seconds))
        .map_err(|_| anyhow!("checker timed out after {} seconds", timeout_seconds))?
}

fn create_detached_session_snapshot(
    workspace_manager: &WorkspaceManager,
    workdir: &Path,
    address: ChannelAddress,
    agent_id: uuid::Uuid,
) -> Result<SessionSnapshot> {
    let session_id = uuid::Uuid::new_v4();
    let workspace_id = format!("detached-{}", session_id);
    let root_dir = workdir.join("sessions").join(session_id.to_string());
    std::fs::create_dir_all(&root_dir)
        .with_context(|| format!("failed to create {}", root_dir.display()))?;
    let workspace = workspace_manager.create_transient_workspace(
        workspace_id.clone(),
        Some("Detached Background Workspace"),
        agent_id,
        session_id,
    )?;
    let workspace_root = workspace.files_dir.clone();
    let attachments_dir = workspace_root.join("upload");
    Ok(SessionSnapshot {
        id: session_id,
        agent_id,
        address,
        root_dir,
        attachments_dir,
        workspace_id,
        workspace_root,
        message_count: 0,
        agent_message_count: 0,
        agent_messages: Vec::new(),
        last_agent_returned_at: None,
        last_compacted_at: None,
        turn_count: 0,
        last_compacted_turn_count: 0,
        cumulative_usage: TokenUsage::default(),
        cumulative_compaction: SessionCompactionStats::default(),
        api_timeout_override_seconds: None,
        skill_states: HashMap::new(),
        pending_continue: None,
        pending_workspace_summary: false,
        close_after_summary: false,
    })
}

fn f64_arg_required(arguments: &serde_json::Map<String, Value>, key: &str) -> Result<f64> {
    arguments
        .get(key)
        .and_then(Value::as_f64)
        .ok_or_else(|| anyhow!("{} must be a number", key))
}

fn cleanup_detached_session_root(
    runtime: &ServerRuntime,
    job: &BackgroundJobRequest,
) -> Result<()> {
    if job.cron_task_id.is_some() && job.session.root_dir.exists() {
        std::fs::remove_dir_all(&job.session.root_dir).with_context(|| {
            format!(
                "failed to remove detached cron session directory {}",
                job.session.root_dir.display()
            )
        })?;
    }
    if job.cron_task_id.is_some() {
        runtime
            .workspace_manager
            .delete_workspace(&job.session.workspace_id)?;
    }
    Ok(())
}

fn background_agent_timeout_seconds(model_timeout_seconds: f64) -> f64 {
    model_timeout_seconds + 15.0
}

fn background_recovery_timeout_seconds(model_timeout_seconds: f64, error: &anyhow::Error) -> f64 {
    if error.to_string().contains("timed out") {
        model_timeout_seconds.mul_add(2.0, 15.0)
    } else {
        model_timeout_seconds + 30.0
    }
}

fn is_timeout_like(error: &anyhow::Error) -> bool {
    error.to_string().contains("timed out")
}

fn should_attempt_idle_context_compaction(
    session: &SessionSnapshot,
    now: chrono::DateTime<Utc>,
    idle_threshold: Duration,
) -> bool {
    let Some(last_returned_at) = session.last_agent_returned_at else {
        return false;
    };
    if session.turn_count <= session.last_compacted_turn_count {
        return false;
    }
    let Ok(idle_elapsed) = now.signed_duration_since(last_returned_at).to_std() else {
        return false;
    };
    idle_elapsed > idle_threshold
}

fn background_timeout_with_active_children_text(language: &str) -> String {
    let language = language.to_ascii_lowercase();
    if language.starts_with("zh") {
        "后台任务超时了，而且它启动的子任务可能还在收尾，所以系统没有自动重试以避免冲突。请稍后查看结果，或重新发起一个新任务。".to_string()
    } else {
        "The background task timed out, and child agents may still be finishing work, so the system skipped automatic recovery to avoid conflicts. Please check back later or start a new task.".to_string()
    }
}

fn user_facing_error_text(language: &str, error: &anyhow::Error) -> String {
    let language = language.to_ascii_lowercase();
    let error_text = format!("{error:#}").to_ascii_lowercase();
    let timeout_like = is_timeout_like(error);
    let upstream_timeout = timeout_like
        && (error_text.contains("upstream")
            || error_text.contains("response body")
            || error_text.contains("chat completion")
            || error_text.contains("operation timed out"));
    let upstream_error = error_text.contains("upstream");
    if language.starts_with("zh") {
        if upstream_timeout {
            "这一轮请求上游模型超时了。通常是模型响应过慢或网络波动导致的。请稍后重试；如果反复出现，可以发送 /new 重新开始。".to_string()
        } else if upstream_error {
            "这一轮请求上游模型时失败了。请稍后重试；如果反复出现，可以发送 /new 重新开始。"
                .to_string()
        } else if timeout_like {
            "这一轮处理超时了。请稍后重试，或者发送 /new 重新开始。".to_string()
        } else {
            "这一轮处理失败了。请稍后重试，或者发送 /new 重新开始。".to_string()
        }
    } else if upstream_timeout {
        "This turn failed because the upstream model request timed out. Please try again; if it keeps happening, send /new to start over.".to_string()
    } else if upstream_error {
        "This turn failed while calling the upstream model. Please try again; if it keeps happening, send /new to start over.".to_string()
    } else if timeout_like {
        "This turn timed out. Please try again, or send /new to start over.".to_string()
    } else {
        "This turn failed. Please try again, or send /new to start over.".to_string()
    }
}

fn user_facing_continue_error_text(
    language: &str,
    error: &anyhow::Error,
    progress_summary: &str,
) -> String {
    let language = language.to_ascii_lowercase();
    let error_text = format!("{error:#}").to_ascii_lowercase();
    let upstream_like = error_text.contains("upstream")
        || error_text.contains("provider")
        || error_text.contains("chat completion")
        || is_timeout_like(error);
    if language.starts_with("zh") {
        if upstream_like {
            format!(
                "这一轮在调用上游模型时失败了，但系统已经保留到最近的稳定位置。\n\n当前进度：{}\n\n发送 /continue 可以从这里继续。",
                progress_summary
            )
        } else {
            format!(
                "这一轮在完成前失败了，但系统已经保留到最近的稳定位置。\n\n当前进度：{}\n\n发送 /continue 可以尝试继续。",
                progress_summary
            )
        }
    } else if upstream_like {
        format!(
            "This turn failed while calling the upstream model, but the session has been preserved at the latest stable point.\n\nProgress so far: {}\n\nSend /continue to resume from there.",
            progress_summary
        )
    } else {
        format!(
            "This turn failed before finishing, but the session has been preserved at the latest stable point.\n\nProgress so far: {}\n\nSend /continue to try resuming from there.",
            progress_summary
        )
    }
}

fn summarize_resume_progress(messages: &[ChatMessage]) -> String {
    let last_user_index = messages
        .iter()
        .rposition(|message| message.role == "user")
        .unwrap_or(0);
    let trailing = &messages[last_user_index.saturating_add(1)..];
    let tool_result_count = trailing
        .iter()
        .filter(|message| message.role == "tool")
        .count();
    let tool_names = trailing
        .iter()
        .filter(|message| message.role == "assistant")
        .filter_map(|message| message.tool_calls.as_ref())
        .flat_map(|tool_calls| {
            tool_calls
                .iter()
                .map(|tool_call| tool_call.function.name.clone())
        })
        .collect::<Vec<_>>();
    if tool_result_count > 0 {
        let recent_tools = tool_names.iter().rev().take(3).cloned().collect::<Vec<_>>();
        if recent_tools.is_empty() {
            format!(
                "the previous turn already reached tool execution and preserved {tool_result_count} tool result(s)"
            )
        } else {
            format!(
                "the previous turn already reached tool execution and preserved {} tool result(s); recent tools: {}",
                tool_result_count,
                recent_tools
                    .into_iter()
                    .rev()
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        }
    } else {
        let partial_text = trailing
            .iter()
            .filter(|message| message.role == "assistant")
            .filter_map(|message| message.content.as_ref())
            .filter_map(Value::as_str)
            .find(|text| !text.trim().is_empty())
            .map(str::trim)
            .map(|text| text.chars().take(120).collect::<String>());
        match partial_text {
            Some(text) => format!(
                "the previous turn preserved partial assistant progress: {}",
                text
            ),
            None => "the previous turn was preserved before the assistant could finish responding"
                .to_string(),
        }
    }
}

fn format_session_status(
    language: &str,
    model_key: &str,
    model: &ModelConfig,
    session: &SessionSnapshot,
    effective_api_timeout_seconds: f64,
    timeout_source: &str,
    current_context_estimate: usize,
    current_reasoning_effort: Option<&str>,
    context_compaction_enabled: bool,
) -> String {
    let usage = &session.cumulative_usage;
    let compaction = &session.cumulative_compaction;
    let cache_hit_rate = if usage.prompt_tokens == 0 {
        0.0
    } else {
        (usage.cache_hit_tokens as f64 / usage.prompt_tokens as f64) * 100.0
    };
    let pricing = estimate_cost_usd(model, usage);
    let compaction_pricing = estimate_compaction_savings_usd(model, compaction);
    let language = language.to_ascii_lowercase();
    if language.starts_with("zh") {
        let mut lines = vec![
            format!("Session: {}", session.id),
            format!("Workspace: {}", session.workspace_id),
            format!("Model: {} ({})", model_key, model.model),
            format!(
                "API timeout: {:.1}s ({})",
                effective_api_timeout_seconds, timeout_source
            ),
            format!(
                "Reasoning effort: {}",
                current_reasoning_effort.unwrap_or("default")
            ),
            format!(
                "Automatic context compaction: {}",
                if context_compaction_enabled {
                    "enabled"
                } else {
                    "disabled"
                }
            ),
            format!("Turns: {}", session.turn_count),
            format!(
                "Current context estimate: {} tokens (local estimate)",
                current_context_estimate
            ),
            String::new(),
            "Token 用量：".to_string(),
            format!("- llm_calls: {}", usage.llm_calls),
            format!("- prompt_tokens: {}", usage.prompt_tokens),
            format!("- completion_tokens: {}", usage.completion_tokens),
            format!("- total_tokens: {}", usage.total_tokens),
            format!("- cache_hit_tokens: {}", usage.cache_hit_tokens),
            format!("- cache_miss_tokens: {}", usage.cache_miss_tokens),
            format!("- cache_read_tokens: {}", usage.cache_read_tokens),
            format!("- cache_write_tokens: {}", usage.cache_write_tokens),
            format!("- cache_hit_rate: {:.2}%", cache_hit_rate),
        ];
        if let Some((formula, total_usd)) = pricing {
            lines.push(String::new());
            lines.push("价格估算：".to_string());
            lines.push(format!("- formula: {}", formula));
            lines.push(format!("- estimated_total_usd: ${:.6}", total_usd));
        } else {
            lines.push(String::new());
            lines.push("价格估算：当前模型没有内置价格表，无法直接估算。".to_string());
        }
        lines.push(String::new());
        lines.push("累计上下文压缩统计：".to_string());
        lines.push(format!("- compaction_runs: {}", compaction.run_count));
        lines.push(format!(
            "- compacted_runs: {}",
            compaction.compacted_run_count
        ));
        lines.push(format!(
            "- estimated_tokens_before: {}",
            compaction.estimated_tokens_before
        ));
        lines.push(format!(
            "- estimated_tokens_after: {}",
            compaction.estimated_tokens_after
        ));
        lines.push(format!(
            "- estimated_tokens_saved: {}",
            compaction
                .estimated_tokens_before
                .saturating_sub(compaction.estimated_tokens_after)
        ));
        if let Some((formula, gross_usd, compaction_cost_usd, net_usd)) = compaction_pricing {
            lines.push(format!("- formula: {}", formula));
            lines.push(format!(
                "- estimated_cold_start_gross_usd: ${:.6}",
                gross_usd
            ));
            lines.push(format!(
                "- estimated_compaction_cost_usd: ${:.6}",
                compaction_cost_usd
            ));
            lines.push(format!("- estimated_net_usd: ${:.6}", net_usd));
        } else {
            lines.push(
                "- estimated_net_usd: unavailable for the current model pricing table.".to_string(),
            );
        }
        lines.join("\n")
    } else {
        let mut lines = vec![
            format!("Session: {}", session.id),
            format!("Workspace: {}", session.workspace_id),
            format!("Model: {} ({})", model_key, model.model),
            format!(
                "API timeout: {:.1}s ({})",
                effective_api_timeout_seconds, timeout_source
            ),
            format!(
                "Reasoning effort: {}",
                current_reasoning_effort.unwrap_or("default")
            ),
            format!(
                "Automatic context compaction: {}",
                if context_compaction_enabled {
                    "enabled"
                } else {
                    "disabled"
                }
            ),
            format!("Turns: {}", session.turn_count),
            format!(
                "Current context estimate: {} tokens (local estimate)",
                current_context_estimate
            ),
            String::new(),
            "Token usage:".to_string(),
            format!("- llm_calls: {}", usage.llm_calls),
            format!("- prompt_tokens: {}", usage.prompt_tokens),
            format!("- completion_tokens: {}", usage.completion_tokens),
            format!("- total_tokens: {}", usage.total_tokens),
            format!("- cache_hit_tokens: {}", usage.cache_hit_tokens),
            format!("- cache_miss_tokens: {}", usage.cache_miss_tokens),
            format!("- cache_read_tokens: {}", usage.cache_read_tokens),
            format!("- cache_write_tokens: {}", usage.cache_write_tokens),
            format!("- cache_hit_rate: {:.2}%", cache_hit_rate),
        ];
        if let Some((formula, total_usd)) = pricing {
            lines.push(String::new());
            lines.push("Estimated cost:".to_string());
            lines.push(format!("- formula: {}", formula));
            lines.push(format!("- estimated_total_usd: ${:.6}", total_usd));
        } else {
            lines.push(String::new());
            lines.push(
                "Estimated cost: unavailable for the current model pricing table.".to_string(),
            );
        }
        lines.push(String::new());
        lines.push("Cumulative context compaction stats:".to_string());
        lines.push(format!("- compaction_runs: {}", compaction.run_count));
        lines.push(format!(
            "- compacted_runs: {}",
            compaction.compacted_run_count
        ));
        lines.push(format!(
            "- estimated_tokens_before: {}",
            compaction.estimated_tokens_before
        ));
        lines.push(format!(
            "- estimated_tokens_after: {}",
            compaction.estimated_tokens_after
        ));
        lines.push(format!(
            "- estimated_tokens_saved: {}",
            compaction
                .estimated_tokens_before
                .saturating_sub(compaction.estimated_tokens_after)
        ));
        if let Some((formula, gross_usd, compaction_cost_usd, net_usd)) = compaction_pricing {
            lines.push(format!("- formula: {}", formula));
            lines.push(format!(
                "- estimated_cold_start_gross_usd: ${:.6}",
                gross_usd
            ));
            lines.push(format!(
                "- estimated_compaction_cost_usd: ${:.6}",
                compaction_cost_usd
            ));
            lines.push(format!("- estimated_net_usd: ${:.6}", net_usd));
        } else {
            lines.push(
                "- estimated_net_usd: unavailable for the current model pricing table.".to_string(),
            );
        }
        lines.join("\n")
    }
}

struct ModelPricing {
    input_per_million: f64,
    output_per_million: f64,
}

fn model_pricing(model: &ModelConfig) -> Option<ModelPricing> {
    match (
        model.api_endpoint.contains("openrouter.ai"),
        model.model.as_str(),
    ) {
        (true, "anthropic/claude-opus-4.6") => Some(ModelPricing {
            input_per_million: 15.0,
            output_per_million: 75.0,
        }),
        (true, "anthropic/claude-sonnet-4.6") => Some(ModelPricing {
            input_per_million: 3.0,
            output_per_million: 15.0,
        }),
        (true, "qwen/qwen3.5-27b") => Some(ModelPricing {
            input_per_million: 0.195,
            output_per_million: 1.56,
        }),
        _ => None,
    }
}

fn estimate_cost_usd(model: &ModelConfig, usage: &TokenUsage) -> Option<(String, f64)> {
    let pricing = model_pricing(model)?;
    let input_per_million = pricing.input_per_million;
    let output_per_million = pricing.output_per_million;
    let cache_read_per_million = input_per_million * 0.1;
    let cache_write_per_million = input_per_million * 1.25;
    let uncached_input_tokens = usage
        .cache_miss_tokens
        .saturating_sub(usage.cache_write_tokens);
    let total_usd = (usage.cache_read_tokens as f64 / 1_000_000.0) * cache_read_per_million
        + (usage.cache_write_tokens as f64 / 1_000_000.0) * cache_write_per_million
        + (uncached_input_tokens as f64 / 1_000_000.0) * input_per_million
        + (usage.completion_tokens as f64 / 1_000_000.0) * output_per_million;
    let formula = format!(
        "cache_read_tokens * ${cache_read_per_million:.6}/1M + cache_write_tokens * ${cache_write_per_million:.6}/1M + (cache_miss_tokens - cache_write_tokens) * ${input_per_million:.6}/1M + completion_tokens * ${output_per_million:.6}/1M"
    );
    Some((formula, total_usd))
}

fn estimate_compaction_savings_usd(
    model: &ModelConfig,
    compaction: &SessionCompactionStats,
) -> Option<(String, f64, f64, f64)> {
    let pricing = model_pricing(model)?;
    let saved_tokens = compaction
        .estimated_tokens_before
        .saturating_sub(compaction.estimated_tokens_after);
    let cold_start_gross_usd = (saved_tokens as f64 / 1_000_000.0) * pricing.input_per_million;
    let (_, compaction_cost_usd) = estimate_cost_usd(model, &compaction.usage)?;
    let net_usd = cold_start_gross_usd - compaction_cost_usd;
    let formula = format!(
        "(estimated_tokens_before - estimated_tokens_after) * ${:.6}/1M - compaction_run_cost",
        pricing.input_per_million
    );
    Some((formula, cold_start_gross_usd, compaction_cost_usd, net_usd))
}

fn compaction_stats_from_report(
    report: &agent_frame::ContextCompactionReport,
) -> SessionCompactionStats {
    let mut stats = SessionCompactionStats::default();
    stats.run_count = 1;
    stats.compacted_run_count = u64::from(report.compacted);
    stats.estimated_tokens_before = report.estimated_tokens_before as u64;
    stats.estimated_tokens_after = report.estimated_tokens_after as u64;
    stats.usage = report.usage.clone();
    stats
}

fn format_api_timeout_update(
    session: &SessionSnapshot,
    model_timeout_seconds: f64,
    argument: &str,
) -> Result<(Option<f64>, String)> {
    let normalized = argument.trim().to_ascii_lowercase();
    if normalized == "default" || normalized == "reset" || normalized == "0" {
        return Ok((
            None,
            format!(
                "API timeout reset for session {}. Effective timeout is now {:.1}s (model default).",
                session.id, model_timeout_seconds
            ),
        ));
    }
    let timeout_seconds: f64 = argument
        .trim()
        .parse()
        .with_context(|| format!("invalid timeout value '{}'", argument.trim()))?;
    if timeout_seconds <= 0.0 {
        return Err(anyhow!(
            "API timeout must be greater than 0 seconds, or use 0/default/reset to restore the model default"
        ));
    }
    Ok((
        Some(timeout_seconds),
        format!(
            "API timeout updated for session {}. Effective timeout is now {:.1}s (session override).",
            session.id, timeout_seconds
        ),
    ))
}

fn parse_oldspace_command(text: Option<&str>) -> Option<String> {
    let text = normalized_command_text(text?)?;
    let suffix = text.strip_prefix("/oldspace")?.trim();
    if suffix.is_empty() {
        None
    } else {
        Some(suffix.to_string())
    }
}

fn parse_set_api_timeout_command(text: Option<&str>) -> Option<String> {
    let text = normalized_command_text(text?)?;
    let suffix = text.strip_prefix("/set_api_timeout")?.trim();
    if suffix.is_empty() {
        None
    } else {
        Some(suffix.to_string())
    }
}

fn parse_model_command(text: Option<&str>) -> Option<Option<String>> {
    parse_optional_command_argument(text, "/model")
}

fn parse_compact_mode_command(text: Option<&str>) -> Option<Option<String>> {
    parse_optional_command_argument(text, "/compact_mode")
}

fn parse_sandbox_command(text: Option<&str>) -> Option<Option<String>> {
    parse_optional_command_argument(text, "/sandbox")
}

fn parse_think_command(text: Option<&str>) -> Option<Option<String>> {
    parse_optional_command_argument(text, "/think")
}

fn parse_snap_save_command(text: Option<&str>) -> Option<String> {
    parse_required_command_argument(text, "/snapsave")
}

fn parse_snap_load_command(text: Option<&str>) -> Option<String> {
    parse_required_command_argument(text, "/snapload")
}

fn parse_snap_list_command(text: Option<&str>) -> bool {
    matches!(parse_optional_command_argument(text, "/snaplist"), Some(_))
}

fn parse_continue_command(text: Option<&str>) -> bool {
    matches!(parse_optional_command_argument(text, "/continue"), Some(_))
}

fn parse_optional_command_argument(text: Option<&str>, command: &str) -> Option<Option<String>> {
    let text = normalized_command_text(text?)?;
    if text == command {
        return Some(None);
    }
    let suffix = text.strip_prefix(command)?.trim();
    if suffix.is_empty() {
        Some(None)
    } else {
        Some(Some(suffix.to_string()))
    }
}

fn parse_required_command_argument(text: Option<&str>, command: &str) -> Option<String> {
    let text = normalized_command_text(text?)?;
    let suffix = text.strip_prefix(command)?.trim();
    if suffix.is_empty() {
        None
    } else {
        Some(suffix.to_string())
    }
}

fn normalized_command_text(text: &str) -> Option<String> {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return None;
    }
    let mut parts = trimmed.splitn(2, char::is_whitespace);
    let first = parts.next()?;
    let normalized_first = first_token_without_mention(first);
    let rest = parts.next().map(str::trim_start).unwrap_or("");
    if rest.is_empty() {
        Some(normalized_first.to_string())
    } else {
        Some(format!("{normalized_first} {rest}"))
    }
}

fn first_token_without_mention(token: &str) -> &str {
    token.split_once('@').map_or(token, |(base, _)| base)
}

fn command_matches(text: &str, command: &str) -> bool {
    normalized_command_text(text).as_deref() == Some(command)
}

fn command_starts_with(text: &str, command: &str) -> bool {
    normalized_command_text(text)
        .as_deref()
        .is_some_and(|normalized| normalized.starts_with(command))
}

fn sandbox_mode_label(mode: SandboxMode) -> &'static str {
    match mode {
        SandboxMode::Disabled => "disabled",
        SandboxMode::Subprocess => "subprocess",
        SandboxMode::Bubblewrap => "bubblewrap",
    }
}

fn sandbox_mode_value(mode: SandboxMode) -> &'static str {
    sandbox_mode_label(mode)
}

fn parse_sandbox_mode_value(value: &str) -> Option<SandboxMode> {
    match value.trim() {
        "disabled" => Some(SandboxMode::Disabled),
        "subprocess" => Some(SandboxMode::Subprocess),
        "bubblewrap" => Some(SandboxMode::Bubblewrap),
        _ => None,
    }
}

fn parse_reasoning_effort_value(value: &str) -> Option<&'static str> {
    match value.trim().to_ascii_lowercase().as_str() {
        "low" => Some("low"),
        "medium" => Some("medium"),
        "high" => Some("high"),
        _ => None,
    }
}

fn effective_reasoning_config(
    model: &ModelConfig,
    conversation_effort: Option<&str>,
) -> Option<agent_frame::config::ReasoningConfig> {
    let mut reasoning = model.reasoning.clone().unwrap_or_default();
    if let Some(effort) = conversation_effort {
        reasoning.effort = Some(effort.to_string());
    }
    if reasoning.effort.is_none()
        && reasoning.max_tokens.is_none()
        && reasoning.exclude.is_none()
        && reasoning.enabled.is_none()
    {
        None
    } else {
        Some(reasoning)
    }
}

fn estimate_current_context_tokens_for_session(
    runtime: &ServerRuntime,
    session: &SessionSnapshot,
    model_key: &str,
) -> Result<usize> {
    let frame_config = runtime.build_agent_frame_config(
        session,
        &session.workspace_root,
        AgentPromptKind::MainForeground,
        model_key,
        None,
    )?;
    let skills = discover_skills(&frame_config.skills_dirs)?;
    let extra_tools = runtime.build_extra_tools(
        session,
        AgentPromptKind::MainForeground,
        session.agent_id,
        None,
    );
    let registry = build_tool_registry(
        &frame_config.enabled_tools,
        &frame_config.workspace_root,
        &frame_config.runtime_state_root,
        &frame_config.upstream,
        frame_config.image_tool_upstream.as_ref(),
        &frame_config.skills_dirs,
        &skills,
        &extra_tools,
    )?;
    let tools = registry.into_values().collect::<Vec<_>>();
    Ok(estimate_session_tokens(&session.agent_messages, &tools, ""))
}

fn replace_directory_contents(target: &Path, source: &Path) -> Result<()> {
    if target.exists() {
        std::fs::remove_dir_all(target)
            .with_context(|| format!("failed to clear {}", target.display()))?;
    }
    copy_dir_recursive(source, target)
}

fn copy_dir_recursive(source: &Path, target: &Path) -> Result<()> {
    std::fs::create_dir_all(target)
        .with_context(|| format!("failed to create {}", target.display()))?;
    for entry in
        std::fs::read_dir(source).with_context(|| format!("failed to read {}", source.display()))?
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
            let link_target = std::fs::read_link(&source_path)
                .with_context(|| format!("failed to read link {}", source_path.display()))?;
            create_symlink(&link_target, &target_path)?;
        } else {
            std::fs::copy(&source_path, &target_path).with_context(|| {
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
        let metadata = std::fs::metadata(source)
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

fn workspace_visible_in_list(
    workspace_id: &str,
    active_workspace_ids: &[String],
    is_archived: bool,
) -> bool {
    is_archived || !active_workspace_ids.iter().any(|id| id == workspace_id)
}

fn tool_phase_timeout_grace_seconds() -> f64 {
    15.0
}

fn log_turn_usage(
    agent_id: uuid::Uuid,
    session: &SessionSnapshot,
    usage: &TokenUsage,
    initialization: bool,
    agent_kind: &str,
    parent_agent_id: Option<uuid::Uuid>,
) {
    info!(
        log_stream = "agent",
        log_key = %agent_id,
        kind = "turn_token_usage",
        session_id = %session.id,
        channel_id = %session.address.channel_id,
        agent_kind,
        initialization,
        parent_agent_id = parent_agent_id.map(|value| value.to_string()),
        llm_calls = usage.llm_calls,
        prompt_tokens = usage.prompt_tokens,
        completion_tokens = usage.completion_tokens,
        total_tokens = usage.total_tokens,
        cache_hit_tokens = usage.cache_hit_tokens,
        cache_miss_tokens = usage.cache_miss_tokens,
        cache_read_tokens = usage.cache_read_tokens,
        cache_write_tokens = usage.cache_write_tokens,
        "recorded turn token usage"
    );
}

fn log_agent_frame_event(
    agent_id: uuid::Uuid,
    session: &SessionSnapshot,
    kind: AgentPromptKind,
    model_key: &str,
    event: &SessionEvent,
) {
    let agent_kind = match kind {
        AgentPromptKind::MainForeground => "main_foreground",
        AgentPromptKind::MainBackground => "main_background",
        AgentPromptKind::SubAgent => "subagent",
    };
    match event {
        SessionEvent::SessionStarted {
            previous_message_count,
            prompt_len,
            tool_definition_count,
            skill_count,
        } => info!(
            log_stream = "agent",
            log_key = %agent_id,
            kind = "agent_frame_session_started",
            session_id = %session.id,
            channel_id = %session.address.channel_id,
            agent_kind,
            model = model_key,
            previous_message_count = *previous_message_count as u64,
            prompt_len = *prompt_len as u64,
            tool_definition_count = *tool_definition_count as u64,
            skill_count = *skill_count as u64,
            "agent_frame session started"
        ),
        SessionEvent::CompactionStarted {
            phase,
            message_count,
        } => info!(
            log_stream = "agent",
            log_key = %agent_id,
            kind = "agent_frame_compaction_started",
            session_id = %session.id,
            channel_id = %session.address.channel_id,
            agent_kind,
            model = model_key,
            phase,
            message_count = *message_count as u64,
            "agent_frame compaction started"
        ),
        SessionEvent::CompactionCompleted {
            phase,
            compacted,
            estimated_tokens_before,
            estimated_tokens_after,
            token_limit,
        } => info!(
            log_stream = "agent",
            log_key = %agent_id,
            kind = "agent_frame_compaction_completed",
            session_id = %session.id,
            channel_id = %session.address.channel_id,
            agent_kind,
            model = model_key,
            phase,
            compacted = *compacted,
            estimated_tokens_before = *estimated_tokens_before as u64,
            estimated_tokens_after = *estimated_tokens_after as u64,
            token_limit = *token_limit as u64,
            "agent_frame compaction completed"
        ),
        SessionEvent::RoundStarted {
            round_index,
            message_count,
        } => info!(
            log_stream = "agent",
            log_key = %agent_id,
            kind = "agent_frame_round_started",
            session_id = %session.id,
            channel_id = %session.address.channel_id,
            agent_kind,
            model = model_key,
            round_index = *round_index as u64,
            message_count = *message_count as u64,
            "agent_frame round started"
        ),
        SessionEvent::ModelCallStarted {
            round_index,
            message_count,
        } => info!(
            log_stream = "agent",
            log_key = %agent_id,
            kind = "agent_frame_model_call_started",
            session_id = %session.id,
            channel_id = %session.address.channel_id,
            agent_kind,
            model = model_key,
            round_index = *round_index as u64,
            message_count = *message_count as u64,
            "agent_frame model call started"
        ),
        SessionEvent::ModelCallCompleted {
            round_index,
            tool_call_count,
            prompt_tokens,
            completion_tokens,
            total_tokens,
        } => info!(
            log_stream = "agent",
            log_key = %agent_id,
            kind = "agent_frame_model_call_completed",
            session_id = %session.id,
            channel_id = %session.address.channel_id,
            agent_kind,
            model = model_key,
            round_index = *round_index as u64,
            tool_call_count = *tool_call_count as u64,
            prompt_tokens = *prompt_tokens,
            completion_tokens = *completion_tokens,
            total_tokens = *total_tokens,
            "agent_frame model call completed"
        ),
        SessionEvent::CheckpointEmitted {
            message_count,
            total_tokens,
        } => info!(
            log_stream = "agent",
            log_key = %agent_id,
            kind = "agent_frame_checkpoint_emitted",
            session_id = %session.id,
            channel_id = %session.address.channel_id,
            agent_kind,
            model = model_key,
            message_count = *message_count as u64,
            total_tokens = *total_tokens,
            "agent_frame checkpoint emitted"
        ),
        SessionEvent::ToolWaitCompactionScheduled {
            tool_name,
            stable_prefix_message_count,
            delay_ms,
        } => info!(
            log_stream = "agent",
            log_key = %agent_id,
            kind = "agent_frame_tool_wait_compaction_scheduled",
            session_id = %session.id,
            channel_id = %session.address.channel_id,
            agent_kind,
            model = model_key,
            tool_name,
            stable_prefix_message_count = *stable_prefix_message_count as u64,
            delay_ms = *delay_ms,
            "agent_frame tool-wait compaction scheduled"
        ),
        SessionEvent::ToolWaitCompactionStarted {
            tool_name,
            stable_prefix_message_count,
        } => info!(
            log_stream = "agent",
            log_key = %agent_id,
            kind = "agent_frame_tool_wait_compaction_started",
            session_id = %session.id,
            channel_id = %session.address.channel_id,
            agent_kind,
            model = model_key,
            tool_name,
            stable_prefix_message_count = *stable_prefix_message_count as u64,
            "agent_frame tool-wait compaction started"
        ),
        SessionEvent::ToolWaitCompactionCompleted {
            tool_name,
            compacted,
            estimated_tokens_before,
            estimated_tokens_after,
            token_limit,
        } => info!(
            log_stream = "agent",
            log_key = %agent_id,
            kind = "agent_frame_tool_wait_compaction_completed",
            session_id = %session.id,
            channel_id = %session.address.channel_id,
            agent_kind,
            model = model_key,
            tool_name,
            compacted = *compacted,
            estimated_tokens_before = *estimated_tokens_before as u64,
            estimated_tokens_after = *estimated_tokens_after as u64,
            token_limit = *token_limit as u64,
            "agent_frame tool-wait compaction completed"
        ),
        SessionEvent::ToolCallStarted {
            round_index,
            tool_name,
            tool_call_id,
        } => info!(
            log_stream = "agent",
            log_key = %agent_id,
            kind = "agent_frame_tool_call_started",
            session_id = %session.id,
            channel_id = %session.address.channel_id,
            agent_kind,
            model = model_key,
            round_index = *round_index as u64,
            tool_name,
            tool_call_id,
            "agent_frame tool call started"
        ),
        SessionEvent::ToolCallCompleted {
            round_index,
            tool_name,
            tool_call_id,
            output_len,
            errored,
        } => info!(
            log_stream = "agent",
            log_key = %agent_id,
            kind = "agent_frame_tool_call_completed",
            session_id = %session.id,
            channel_id = %session.address.channel_id,
            agent_kind,
            model = model_key,
            round_index = *round_index as u64,
            tool_name,
            tool_call_id,
            output_len = *output_len as u64,
            errored = *errored,
            "agent_frame tool call completed"
        ),
        SessionEvent::SessionYielded {
            phase,
            message_count,
            total_tokens,
        } => info!(
            log_stream = "agent",
            log_key = %agent_id,
            kind = "agent_frame_session_yielded",
            session_id = %session.id,
            channel_id = %session.address.channel_id,
            agent_kind,
            model = model_key,
            phase,
            message_count = *message_count as u64,
            total_tokens = *total_tokens,
            "agent_frame session yielded at a safe boundary"
        ),
        SessionEvent::PrefixRewriteApplied {
            previous_prefix_message_count,
            replacement_prefix_message_count,
        } => info!(
            log_stream = "agent",
            log_key = %agent_id,
            kind = "agent_frame_prefix_rewrite_applied",
            session_id = %session.id,
            channel_id = %session.address.channel_id,
            agent_kind,
            model = model_key,
            previous_prefix_message_count = *previous_prefix_message_count as u64,
            replacement_prefix_message_count = *replacement_prefix_message_count as u64,
            "agent_frame prefix rewrite applied"
        ),
        SessionEvent::SessionCompleted {
            message_count,
            total_tokens,
        } => info!(
            log_stream = "agent",
            log_key = %agent_id,
            kind = "agent_frame_session_completed",
            session_id = %session.id,
            channel_id = %session.address.channel_id,
            agent_kind,
            model = model_key,
            message_count = *message_count as u64,
            total_tokens = *total_tokens,
            "agent_frame session completed"
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::{
        SinkTarget, TokenUsage, background_agent_timeout_seconds,
        background_recovery_timeout_seconds, background_timeout_with_active_children_text,
        build_previous_messages_for_turn, build_user_turn_message, channel_restart_backoff_seconds,
        estimate_compaction_savings_usd, estimate_cost_usd, extract_attachment_references,
        is_timeout_like, parse_model_command, parse_oldspace_command, parse_sandbox_command,
        parse_set_api_timeout_command, parse_sink_target, parse_snap_list_command,
        parse_snap_load_command, parse_snap_save_command, parse_think_command,
        send_outgoing_message_now, should_attempt_idle_context_compaction,
        tag_interrupted_followup_text, workspace_visible_in_list,
    };
    use crate::backend::AgentBackendKind;
    use crate::channel::{Channel, IncomingMessage};
    use crate::config::ModelConfig;
    use crate::domain::ChannelAddress;
    use crate::domain::{AttachmentKind, OutgoingMessage, ProcessingState, StoredAttachment};
    use crate::session::{PendingContinueState, SessionSnapshot};
    use agent_frame::ChatMessage;
    use agent_frame::SessionCompactionStats;
    use anyhow::anyhow;
    use async_trait::async_trait;
    use chrono::{Duration as ChronoDuration, Utc};
    use serde_json::json;
    use std::collections::HashMap;
    use std::fs;
    use std::path::PathBuf;
    use std::sync::{Arc, Mutex};
    use std::time::Duration;
    use tempfile::TempDir;
    use tokio::sync::mpsc;
    use uuid::Uuid;

    #[derive(Default)]
    struct RecordingChannel {
        sent_messages: Mutex<Vec<(ChannelAddress, OutgoingMessage)>>,
    }

    #[async_trait]
    impl Channel for RecordingChannel {
        fn id(&self) -> &str {
            "recording"
        }

        async fn run(
            self: Arc<Self>,
            _sender: mpsc::Sender<IncomingMessage>,
        ) -> anyhow::Result<()> {
            Ok(())
        }

        async fn send_media_group(
            &self,
            _address: &ChannelAddress,
            _images: Vec<crate::domain::OutgoingAttachment>,
        ) -> anyhow::Result<()> {
            Ok(())
        }

        async fn send(
            &self,
            address: &ChannelAddress,
            message: OutgoingMessage,
        ) -> anyhow::Result<()> {
            self.sent_messages
                .lock()
                .unwrap()
                .push((address.clone(), message));
            Ok(())
        }

        async fn set_processing(
            &self,
            _address: &ChannelAddress,
            _state: ProcessingState,
        ) -> anyhow::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn extracts_multiple_attachments_and_strips_tags() {
        let temp_dir = TempDir::new().unwrap();
        let file_path = temp_dir.path().join("agent-1").join("note.txt");
        let image_path = temp_dir.path().join("agent-1").join("image.png");
        fs::create_dir_all(file_path.parent().unwrap()).unwrap();
        fs::write(&file_path, "hello").unwrap();
        fs::write(&image_path, "png").unwrap();

        let (text, attachments) = extract_attachment_references(
            "Here you go.\n<attachment>agent-1/note.txt</attachment>\n<attachment>agent-1/image.png</attachment>",
            temp_dir.path(),
        )
        .unwrap();

        assert_eq!(text, "Here you go.");
        assert_eq!(attachments.len(), 2);
    }

    #[test]
    fn builds_multimodal_user_message_for_vision_models() {
        let temp_dir = TempDir::new().unwrap();
        let image_path = temp_dir.path().join("photo.png");
        fs::write(&image_path, [0_u8, 1, 2, 3]).unwrap();
        let attachment = StoredAttachment {
            id: Uuid::new_v4(),
            kind: AttachmentKind::Image,
            original_name: Some("photo.png".to_string()),
            media_type: Some("image/png".to_string()),
            path: image_path,
            size_bytes: 4,
        };
        let model = ModelConfig {
            model_type: crate::config::ModelType::Openrouter,
            api_endpoint: "https://example.com/v1".to_string(),
            model: "demo-vision".to_string(),
            backend: AgentBackendKind::AgentFrame,
            supports_vision_input: true,
            image_tool_model: None,
            web_search_model: None,
            api_key: None,
            api_key_env: "TEST_API_KEY".to_string(),
            chat_completions_path: "/chat/completions".to_string(),
            codex_home: None,
            auth_credentials_store_mode: agent_frame::config::AuthCredentialsStoreMode::Auto,
            timeout_seconds: 30.0,
            context_window_tokens: 128_000,
            cache_ttl: None,
            reasoning: None,
            headers: serde_json::Map::new(),
            description: "vision".to_string(),
            native_web_search: None,
            external_web_search: None,
        };

        let message =
            build_user_turn_message(Some("看看这张图"), None, &[attachment], &model, true).unwrap();

        let content = message.content.unwrap();
        let items = content.as_array().unwrap();
        assert_eq!(items[0]["type"], "text");
        let text = items[0]["text"].as_str().unwrap();
        assert!(text.contains("already directly visible in this request"));
        assert!(text.contains("instead of calling the image tool again"));
        assert_eq!(items[1]["type"], "image_url");
        let url = items[1]["image_url"]["url"].as_str().unwrap();
        assert!(url.starts_with("data:image/png;base64,"));
    }

    #[test]
    fn immediate_channel_send_works_without_tokio_runtime() {
        let channel = Arc::new(RecordingChannel::default());
        let address = ChannelAddress {
            channel_id: "telegram-main".to_string(),
            conversation_id: "1717801091".to_string(),
            user_id: Some("user-1".to_string()),
            display_name: Some("Telegram User".to_string()),
        };

        send_outgoing_message_now(
            channel.clone(),
            address.clone(),
            OutgoingMessage::text("still working"),
        )
        .unwrap();

        let sent_messages = channel.sent_messages.lock().unwrap();
        assert_eq!(sent_messages.len(), 1);
        assert_eq!(sent_messages[0].0, address);
        assert_eq!(sent_messages[0].1.text.as_deref(), Some("still working"));
    }

    #[test]
    fn interrupted_followup_text_gets_marker_prefix() {
        assert_eq!(
            tag_interrupted_followup_text(Some("进度如何？".to_string())).as_deref(),
            Some("[Interrupted Follow-up]\n进度如何？")
        );
        assert_eq!(
            tag_interrupted_followup_text(None).as_deref(),
            Some("[Interrupted Follow-up]")
        );
    }

    #[test]
    fn channel_restart_backoff_grows_and_caps() {
        assert_eq!(channel_restart_backoff_seconds(1), 1);
        assert_eq!(channel_restart_backoff_seconds(2), 2);
        assert_eq!(channel_restart_backoff_seconds(3), 4);
        assert_eq!(channel_restart_backoff_seconds(10), 30);
    }

    #[test]
    fn parses_multi_sink_structure() {
        let sink = parse_sink_target(
            &json!({
                "kind": "multi",
                "targets": [
                    {
                        "kind": "direct",
                        "channel_id": "telegram-main",
                        "conversation_id": "123"
                    },
                    {
                        "kind": "broadcast",
                        "topic": "ops"
                    }
                ]
            }),
            None,
        )
        .unwrap();

        match sink {
            SinkTarget::Multi(targets) => assert_eq!(targets.len(), 2),
            other => panic!("expected multi sink, got {:?}", other),
        }
    }

    #[test]
    fn background_timeout_helpers_scale_as_expected() {
        let timeout_error = anyhow!("background agent timed out after 135.0 seconds");
        let generic_error = anyhow!("background agent failed for another reason");

        assert_eq!(background_agent_timeout_seconds(120.0), 135.0);
        assert_eq!(
            background_recovery_timeout_seconds(120.0, &timeout_error),
            255.0
        );
        assert_eq!(
            background_recovery_timeout_seconds(120.0, &generic_error),
            150.0
        );
    }

    #[test]
    fn idle_context_compaction_requires_idle_time_and_new_turns() {
        let now = Utc::now();
        let base_snapshot = SessionSnapshot {
            id: Uuid::new_v4(),
            agent_id: Uuid::new_v4(),
            address: ChannelAddress {
                channel_id: "telegram-main".to_string(),
                conversation_id: "1717801091".to_string(),
                user_id: Some("user-1".to_string()),
                display_name: Some("Telegram User".to_string()),
            },
            root_dir: PathBuf::from("/tmp/session"),
            attachments_dir: PathBuf::from("/tmp/workspaces/workspace-1/files/upload"),
            workspace_id: "workspace-1".to_string(),
            workspace_root: PathBuf::from("/tmp/workspaces/workspace-1/files"),
            message_count: 0,
            agent_message_count: 3,
            agent_messages: Vec::new(),
            last_agent_returned_at: Some(now - ChronoDuration::seconds(400)),
            last_compacted_at: None,
            turn_count: 2,
            last_compacted_turn_count: 1,
            cumulative_usage: TokenUsage::default(),
            cumulative_compaction: agent_frame::SessionCompactionStats::default(),
            api_timeout_override_seconds: None,
            skill_states: HashMap::new(),
            pending_continue: None,
            pending_workspace_summary: false,
            close_after_summary: false,
        };

        assert!(should_attempt_idle_context_compaction(
            &base_snapshot,
            now,
            Duration::from_secs(270)
        ));

        let no_new_turn = SessionSnapshot {
            last_compacted_turn_count: 2,
            ..base_snapshot.clone()
        };
        assert!(!should_attempt_idle_context_compaction(
            &no_new_turn,
            now,
            Duration::from_secs(270)
        ));

        let not_idle_long_enough = SessionSnapshot {
            last_agent_returned_at: Some(now - ChronoDuration::seconds(60)),
            ..base_snapshot.clone()
        };
        assert!(!should_attempt_idle_context_compaction(
            &not_idle_long_enough,
            now,
            Duration::from_secs(270)
        ));

        let no_return_yet = SessionSnapshot {
            last_agent_returned_at: None,
            ..base_snapshot
        };
        assert!(!should_attempt_idle_context_compaction(
            &no_return_yet,
            now,
            Duration::from_secs(270)
        ));
    }

    #[test]
    fn timeout_helpers_and_background_timeout_messages_behave_as_expected() {
        let timeout_error = anyhow!("background agent timed out after 135.0 seconds");
        let generic_error = anyhow!("background agent failed for another reason");

        assert!(is_timeout_like(&timeout_error));
        assert!(!is_timeout_like(&generic_error));
        assert!(background_timeout_with_active_children_text("zh-CN").contains("系统没有自动重试"));
        assert!(
            background_timeout_with_active_children_text("en")
                .contains("skipped automatic recovery")
        );
    }

    #[test]
    fn parses_oldspace_command_with_workspace_id() {
        assert_eq!(
            parse_oldspace_command(Some("/oldspace workspace-123")),
            Some("workspace-123".to_string())
        );
        assert_eq!(
            parse_oldspace_command(Some("  /oldspace   abc-def  ")),
            Some("abc-def".to_string())
        );
        assert_eq!(parse_oldspace_command(Some("/oldspace")), None);
        assert_eq!(parse_oldspace_command(Some("hello")), None);
    }

    #[test]
    fn parses_set_api_timeout_command_argument() {
        assert_eq!(
            parse_set_api_timeout_command(Some("/set_api_timeout 300")),
            Some("300".to_string())
        );
        assert_eq!(
            parse_set_api_timeout_command(Some("  /set_api_timeout   default ")),
            Some("default".to_string())
        );
        assert_eq!(
            parse_set_api_timeout_command(Some("/set_api_timeout")),
            None
        );
        assert_eq!(parse_set_api_timeout_command(Some("hello")), None);
    }

    #[test]
    fn parses_model_sandbox_and_think_commands_with_optional_arguments() {
        assert_eq!(parse_model_command(Some("/model")), Some(None));
        assert_eq!(
            parse_model_command(Some("/model demo-model")),
            Some(Some("demo-model".to_string()))
        );
        assert_eq!(
            parse_model_command(Some("/model@party_claw_bot demo-model")),
            Some(Some("demo-model".to_string()))
        );

        assert_eq!(parse_sandbox_command(Some("/sandbox")), Some(None));
        assert_eq!(
            parse_sandbox_command(Some("/sandbox subprocess")),
            Some(Some("subprocess".to_string()))
        );
        assert_eq!(
            parse_sandbox_command(Some("/sandbox@party_claw_bot bubblewrap")),
            Some(Some("bubblewrap".to_string()))
        );

        assert_eq!(parse_think_command(Some("/think")), Some(None));
        assert_eq!(
            parse_think_command(Some("/think high")),
            Some(Some("high".to_string()))
        );
        assert_eq!(
            parse_think_command(Some("/think@party_claw_bot medium")),
            Some(Some("medium".to_string()))
        );
    }

    #[test]
    fn model_reselect_short_circuits_without_change() {
        let current_model_key = Some("demo-model".to_string());
        let requested_model_key = "demo-model";
        assert_eq!(current_model_key.as_deref(), Some(requested_model_key));
    }

    #[test]
    fn parses_snap_commands_with_bot_suffix() {
        assert_eq!(
            parse_snap_save_command(Some("/snapsave demo-checkpoint")),
            Some("demo-checkpoint".to_string())
        );
        assert_eq!(
            parse_snap_save_command(Some("/snapsave@party_claw_bot demo-checkpoint")),
            Some("demo-checkpoint".to_string())
        );
        assert_eq!(
            parse_snap_load_command(Some("/snapload restore-point")),
            Some("restore-point".to_string())
        );
        assert_eq!(
            parse_snap_load_command(Some("/snapload@party_claw_bot restore-point")),
            Some("restore-point".to_string())
        );
        assert!(parse_snap_list_command(Some("/snaplist")));
        assert!(parse_snap_list_command(Some("/snaplist@party_claw_bot")));
    }

    #[test]
    fn pending_continue_resume_messages_drive_followup_turn_inputs() {
        let session_messages = vec![ChatMessage::text("assistant", "current session tail")];
        let resume_messages = vec![ChatMessage::text("assistant", "preserved failure point")];
        let pending_continue = PendingContinueState {
            model_key: "demo-model".to_string(),
            resume_messages: resume_messages.clone(),
            original_user_text: Some("original request".to_string()),
            original_attachments: Vec::new(),
            error_summary: "error".to_string(),
            progress_summary: "progress".to_string(),
            failed_at: Utc::now(),
        };

        let continue_messages =
            build_previous_messages_for_turn(&session_messages, Some(&pending_continue), None);
        assert_eq!(continue_messages, resume_messages);

        let followup_messages = build_previous_messages_for_turn(
            &session_messages,
            Some(&pending_continue),
            Some(ChatMessage::text("user", "new user message")),
        );
        assert_eq!(followup_messages.len(), 2);
        assert_eq!(
            followup_messages[0]
                .content
                .as_ref()
                .and_then(|value| value.as_str()),
            Some("preserved failure point")
        );
        assert_eq!(
            followup_messages[1]
                .content
                .as_ref()
                .and_then(|value| value.as_str()),
            Some("new user message")
        );
    }

    #[test]
    fn estimates_openrouter_opus_cost_with_cache_formula() {
        let model = ModelConfig {
            model_type: crate::config::ModelType::Openrouter,
            api_endpoint: "https://openrouter.ai/api/v1".to_string(),
            model: "anthropic/claude-opus-4.6".to_string(),
            backend: AgentBackendKind::AgentFrame,
            supports_vision_input: true,
            image_tool_model: None,
            web_search_model: None,
            api_key: None,
            api_key_env: "OPENROUTER_API_KEY".to_string(),
            chat_completions_path: "/chat/completions".to_string(),
            codex_home: None,
            auth_credentials_store_mode: agent_frame::config::AuthCredentialsStoreMode::Auto,
            timeout_seconds: 300.0,
            context_window_tokens: 262_144,
            cache_ttl: Some("5m".to_string()),
            reasoning: None,
            headers: serde_json::Map::new(),
            description: "demo".to_string(),
            native_web_search: None,
            external_web_search: None,
        };
        let usage = TokenUsage {
            llm_calls: 1,
            prompt_tokens: 10_000,
            completion_tokens: 2_000,
            total_tokens: 12_000,
            cache_hit_tokens: 8_000,
            cache_miss_tokens: 2_000,
            cache_read_tokens: 8_000,
            cache_write_tokens: 1_500,
        };

        let (formula, total_usd) = estimate_cost_usd(&model, &usage).unwrap();
        assert!(formula.contains("cache_read_tokens"));
        assert!(total_usd > 0.0);
    }

    #[test]
    fn estimates_compaction_savings_from_token_delta_and_compaction_cost() {
        let model = ModelConfig {
            model_type: crate::config::ModelType::Openrouter,
            api_endpoint: "https://openrouter.ai/api/v1".to_string(),
            model: "anthropic/claude-sonnet-4.6".to_string(),
            backend: AgentBackendKind::AgentFrame,
            supports_vision_input: true,
            image_tool_model: None,
            web_search_model: None,
            api_key: None,
            api_key_env: "OPENROUTER_API_KEY".to_string(),
            chat_completions_path: "/chat/completions".to_string(),
            codex_home: None,
            auth_credentials_store_mode: agent_frame::config::AuthCredentialsStoreMode::Auto,
            timeout_seconds: 300.0,
            context_window_tokens: 262_144,
            cache_ttl: Some("5m".to_string()),
            reasoning: None,
            headers: serde_json::Map::new(),
            description: "demo".to_string(),
            native_web_search: None,
            external_web_search: None,
        };
        let compaction = SessionCompactionStats {
            run_count: 2,
            compacted_run_count: 2,
            estimated_tokens_before: 90_000,
            estimated_tokens_after: 50_000,
            usage: TokenUsage {
                llm_calls: 2,
                prompt_tokens: 10_000,
                completion_tokens: 500,
                total_tokens: 10_500,
                cache_hit_tokens: 0,
                cache_miss_tokens: 10_000,
                cache_read_tokens: 0,
                cache_write_tokens: 0,
            },
        };

        let (formula, gross_usd, compaction_cost_usd, net_usd) =
            estimate_compaction_savings_usd(&model, &compaction).unwrap();
        assert!(formula.contains("estimated_tokens_before"));
        assert!(gross_usd > 0.0);
        assert!(compaction_cost_usd > 0.0);
        assert!(net_usd < gross_usd);
    }

    #[test]
    fn hides_active_workspaces_from_list_results() {
        assert!(workspace_visible_in_list("archived-1", &[], true));
        assert!(workspace_visible_in_list(
            "idle-1",
            &[String::from("active-1")],
            false
        ));
        assert!(!workspace_visible_in_list(
            "active-1",
            &[String::from("active-1")],
            false
        ));
    }
}
