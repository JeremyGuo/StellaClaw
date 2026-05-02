use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc, Arc,
    },
    thread,
    time::{Duration, Instant},
};

use anyhow::{anyhow, Context, Result};
use crossbeam_channel::{Receiver, Sender};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use stellaclaw_core::{
    model_config::{ModelCapability, ModelConfig},
    session_actor::{
        ChatMessage, ChatMessageItem, ChatRole, ContextItem, ConversationBridgeRequest,
        ConversationBridgeResponse, FileItem, SessionErrorDetail, SessionEvent, SessionInitial,
        SessionRequest, SessionType, ToolRemoteMode, ToolResultContent, ToolResultItem,
    },
};

use crate::{
    channels::types::{
        ChannelEvent, OutgoingAttachment, OutgoingConversationUpdated, OutgoingDelivery,
        OutgoingDispatch, OutgoingError, OutgoingErrorScope, OutgoingErrorSeverity,
        OutgoingMessageAppended, OutgoingOption, OutgoingOptions, OutgoingProcessing,
        OutgoingProgressFeedback, OutgoingStatus, ProcessingState, ProgressFeedbackFinalState,
        TurnProgress, TurnProgressPhase, TurnProgressPlan, TurnProgressPlanItem,
        TurnProgressPlanItemStatus,
    },
    config::{
        ModelSelection, SandboxConfig, SandboxMode, SessionDefaults, SessionProfile,
        StellaclawConfig, ToolModelTarget,
    },
    cron::{
        cron_schedule_from_required_tool_args, optional_cron_schedule_from_tool_args,
        optional_positive_f64_arg, optional_string_arg, parse_enabled_flag, string_arg_required,
        timezone_or_default, CreateCronTaskRequest, CronManager, CronTaskRecord,
        UpdateCronTaskRequest,
    },
    logger::StellaclawLogger,
    sandbox::bubblewrap_support_error,
    session_client::AgentServerClient,
    workspace::{
        ensure_workspace_for_remote_mode, ensure_workspace_seed, sshfs_health_check,
        sshfs_workspace_root, unmount_sshfs_workspace,
    },
};

mod attachments;
mod cron_script;
mod skill_sync;
mod status;

pub use attachments::render_chat_message;
pub(crate) use attachments::{
    attachment_marker, extract_attachment_references, extract_attachment_references_with_markers,
    strip_attachment_tags,
};
use cron_script::{parse_script_stdout, run_script_command, CronScriptMessage, CronScriptTarget};
pub(crate) use skill_sync::push_configured_skill_sync_on_startup;
use skill_sync::{
    copy_skill_atomically, push_skill_sync_if_configured, sync_skill_to_conversation_workspaces,
    validate_skill_directory, validate_skill_name, SkillPersistMode,
};
use status::conversation_status_snapshot;

#[derive(Debug, Clone)]
pub struct IncomingConversationMessage {
    pub remote_message_id: String,
    pub user_name: Option<String>,
    pub message_time: Option<String>,
    pub text: Option<String>,
    pub files: Vec<FileItem>,
    pub control: Option<ConversationControl>,
}

#[derive(Debug, Clone)]
pub enum ConversationControl {
    Continue,
    Cancel,
    Compact,
    ShowStatus,
    ShowModel,
    SwitchModel { model_name: String },
    ShowReasoning,
    SetReasoning { effort: Option<String> },
    InvalidReasoning { reason: String },
    ShowRemote,
    SetRemote { host: String, path: String },
    DisableRemote,
    InvalidRemote { reason: String },
    ShowSandbox,
    SetSandbox { mode: Option<SandboxMode> },
    InvalidSandbox { reason: String },
}

pub(crate) fn parse_reasoning_control_argument(argument: &str) -> ConversationControl {
    let argument = argument.trim();
    if argument.is_empty() {
        return ConversationControl::ShowReasoning;
    }
    match argument.to_ascii_lowercase().as_str() {
        "default" | "model" | "model_default" | "model-default" | "global" => {
            ConversationControl::SetReasoning { effort: None }
        }
        "minimal" | "low" | "medium" | "high" | "xhigh" => ConversationControl::SetReasoning {
            effort: Some(argument.to_ascii_lowercase()),
        },
        _ => ConversationControl::InvalidReasoning {
            reason: format!("未知 reasoning effort `{argument}`。"),
        },
    }
}

#[derive(Debug, Clone)]
pub enum ConversationCommand {
    Incoming(IncomingConversationMessage),
    RunCronTask {
        task: CronTaskRecord,
    },
    Shutdown {
        reason: &'static str,
        ack_tx: Sender<()>,
    },
}

const TYPING_KEEPALIVE_INTERVAL: Duration = Duration::from_secs(4);

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConversationState {
    pub version: u32,
    pub conversation_id: String,
    #[serde(default)]
    pub nickname: String,
    pub channel_id: String,
    pub platform_chat_id: String,
    pub session_profile: SessionProfile,
    #[serde(default)]
    pub model_selection_pending: bool,
    #[serde(default)]
    pub tool_remote_mode: ToolRemoteMode,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sandbox: Option<SandboxConfig>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_effort: Option<String>,
    pub session_binding: ConversationSessionBinding,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConversationSessionBinding {
    pub foreground_session_id: String,
    #[serde(default = "default_index")]
    pub next_background_index: u64,
    #[serde(default = "default_index")]
    pub next_subagent_index: u64,
    #[serde(default)]
    pub background_sessions: BTreeMap<String, ManagedSessionRecord>,
    #[serde(default)]
    pub subagent_sessions: BTreeMap<String, ManagedSessionRecord>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ManagedSessionRecord {
    pub agent_id: String,
    pub session_id: String,
    pub session_type: ManagedSessionType,
    pub status: ManagedSessionStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_message: Option<ChatMessage>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_error: Option<String>,
    #[serde(default)]
    pub suppress_output: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model_override: Option<String>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ManagedSessionType {
    Background,
    Subagent,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ManagedSessionStatus {
    Running,
    Completed,
    Failed,
    Killed,
}

fn default_index() -> u64 {
    1
}

fn format_compact_completed_message(
    compressed: bool,
    estimated_tokens_before: u64,
    estimated_tokens_after: u64,
    threshold_tokens: u64,
    retained_message_count: usize,
    compressed_message_count: usize,
) -> String {
    if compressed {
        format!(
            "✅ 已主动压缩当前上下文。\n\nBefore: {estimated_tokens_before}\nAfter: {estimated_tokens_after}\nThreshold: {threshold_tokens}\nCompressed messages: {compressed_message_count}\nRetained recent messages: {retained_message_count}"
        )
    } else {
        format!(
            "当前上下文暂时无法进一步压缩。\n\nEstimated tokens: {estimated_tokens_before}\nThreshold: {threshold_tokens}"
        )
    }
}

pub fn spawn_conversation(
    workdir: PathBuf,
    state: ConversationState,
    config: Arc<StellaclawConfig>,
    agent_server_path: PathBuf,
    cron_manager: Arc<CronManager>,
    outgoing_tx: Sender<OutgoingDispatch>,
    host_logger: Arc<StellaclawLogger>,
) -> Sender<ConversationCommand> {
    let (tx, rx) = crossbeam_channel::unbounded();
    thread::spawn(move || {
        if let Err(error) = run_conversation(
            workdir,
            state,
            config,
            agent_server_path,
            cron_manager,
            rx,
            outgoing_tx,
            host_logger,
        ) {
            eprintln!("stellaclaw conversation thread failed: {error:#}");
        }
    });
    tx
}

const SSHFS_WATCHDOG_INTERVAL: Duration = Duration::from_secs(20);
const SSHFS_WATCHDOG_PROBE_TIMEOUT: Duration = Duration::from_secs(10);

fn sshfs_watchdog_loop(
    mountpoint: &Path,
    failed: &AtomicBool,
    logger: &StellaclawLogger,
    conversation_id: &str,
) {
    loop {
        thread::sleep(SSHFS_WATCHDOG_INTERVAL);
        if failed.load(Ordering::Relaxed) {
            // Already flagged; nothing more to do.
            return;
        }
        if !sshfs_health_check(mountpoint, SSHFS_WATCHDOG_PROBE_TIMEOUT) {
            logger.warn(
                "sshfs_watchdog_failed",
                json!({
                    "conversation_id": conversation_id,
                    "mountpoint": mountpoint.display().to_string(),
                }),
            );
            failed.store(true, Ordering::SeqCst);
            return;
        }
    }
}

fn run_conversation(
    workdir: PathBuf,
    state: ConversationState,
    config: Arc<StellaclawConfig>,
    agent_server_path: PathBuf,
    cron_manager: Arc<CronManager>,
    rx: Receiver<ConversationCommand>,
    outgoing_tx: Sender<OutgoingDispatch>,
    host_logger: Arc<StellaclawLogger>,
) -> Result<()> {
    let conversation_root = workdir.join("conversations").join(&state.conversation_id);
    fs::create_dir_all(&conversation_root)
        .with_context(|| format!("failed to create {}", conversation_root.display()))?;
    ensure_workspace_seed(&workdir, &conversation_root)?;
    let logger = Arc::new(
        StellaclawLogger::open_under_stellaclaw(&conversation_root, "conversation.log")
            .map_err(anyhow::Error::msg)?,
    );
    logger.info(
        "conversation_started",
        json!({
            "conversation_id": state.conversation_id,
            "channel_id": state.channel_id,
            "platform_chat_id": state.platform_chat_id,
        }),
    );

    let mut runtime = ConversationRuntime::new(
        workdir.clone(),
        conversation_root.clone(),
        state,
        config,
        cron_manager,
        agent_server_path,
        outgoing_tx,
        logger,
        host_logger,
    )?;
    runtime.persist_state()?;

    loop {
        // Check if the sshfs watchdog has flagged a failure.
        if runtime.sshfs_failed.load(Ordering::Relaxed) {
            runtime.logger.warn(
                "sshfs_watchdog_conversation_stopping",
                json!({
                    "conversation_id": runtime.state.conversation_id,
                    "workspace_root": runtime.workspace_root.display().to_string(),
                }),
            );
            let mountpoint = runtime.workspace_root.display().to_string();
            let sshfs_error_msg = format!(
                "Remote workspace ({mountpoint}) is unresponsive. \
                 Session stopped.\n\
                 Send any message to attempt automatic recovery.",
            );
            let _ = runtime.send_channel_error(
                OutgoingErrorScope::RemoteWorkspace,
                OutgoingErrorSeverity::Error,
                "sshfs_unresponsive",
                sshfs_error_msg.clone(),
                Some(json!({"mountpoint": mountpoint})),
                false,
                None,
            );
            let _ = runtime.send_delivery_from_text(sshfs_error_msg);
            // Attempt lazy unmount so the next conversation start can remount
            // cleanly.
            let _ = unmount_sshfs_workspace(&runtime.workdir, &runtime.state.conversation_id);
            runtime.shutdown();
            break;
        }

        let mut changed = false;
        while runtime.pump_session_events()? {
            changed = true;
        }
        runtime.pump_processing_keepalive()?;
        if changed {
            runtime.persist_state_and_publish()?;
        }

        match rx.recv_timeout(Duration::from_millis(100)) {
            Ok(ConversationCommand::Incoming(message)) => {
                if runtime.handle_incoming(message)? {
                    runtime.persist_state_and_publish()?;
                }
            }
            Ok(ConversationCommand::RunCronTask { task }) => {
                if runtime.run_cron_task(task)? {
                    runtime.persist_state_and_publish()?;
                }
            }
            Ok(ConversationCommand::Shutdown { reason, ack_tx }) => {
                runtime.logger.info(
                    "conversation_shutdown_requested",
                    json!({
                        "conversation_id": runtime.state.conversation_id,
                        "reason": reason,
                    }),
                );
                runtime.shutdown();
                let _ = ack_tx.send(());
                break;
            }
            Err(crossbeam_channel::RecvTimeoutError::Timeout) => continue,
            Err(crossbeam_channel::RecvTimeoutError::Disconnected) => {
                runtime.shutdown();
                break;
            }
        }
    }
    Ok(())
}

pub fn load_or_create_conversation_state(
    workdir: &Path,
    conversation_id: &str,
    channel_id: &str,
    platform_chat_id: &str,
    config: &StellaclawConfig,
) -> Result<ConversationState> {
    let root = workdir.join("conversations").join(conversation_id);
    fs::create_dir_all(&root).with_context(|| format!("failed to create {}", root.display()))?;
    let path = root.join("conversation.json");
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
            background_sessions: BTreeMap::new(),
            subagent_sessions: BTreeMap::new(),
        },
    })
}

pub fn persist_conversation_state(workdir: &Path, state: &ConversationState) -> Result<()> {
    let root = workdir.join("conversations").join(&state.conversation_id);
    fs::create_dir_all(&root).with_context(|| format!("failed to create {}", root.display()))?;
    let path = root.join("conversation.json");
    let raw =
        serde_json::to_string_pretty(state).context("failed to serialize conversation state")?;
    fs::write(&path, raw).with_context(|| format!("failed to write {}", path.display()))
}

pub fn load_conversation_status_snapshot(
    workdir: &Path,
    config: &StellaclawConfig,
    conversation_id: &str,
) -> Result<OutgoingStatus> {
    let state = load_existing_conversation_state(workdir, conversation_id)?;
    let conversation_root = workdir.join("conversations").join(conversation_id);
    let workspace_root = match &state.tool_remote_mode {
        ToolRemoteMode::Selectable => conversation_root,
        ToolRemoteMode::FixedSsh { .. } => sshfs_workspace_root(workdir, conversation_id),
    };
    let session_root = workdir.join("conversations").join(conversation_id);
    conversation_status_snapshot(workdir, &session_root, &workspace_root, &state, config)
}

fn load_existing_conversation_state(
    workdir: &Path,
    conversation_id: &str,
) -> Result<ConversationState> {
    let path = workdir
        .join("conversations")
        .join(conversation_id)
        .join("conversation.json");
    let raw =
        fs::read_to_string(&path).with_context(|| format!("failed to read {}", path.display()))?;
    serde_json::from_str(&raw).with_context(|| format!("failed to parse {}", path.display()))
}

struct ConversationRuntime {
    workdir: PathBuf,
    conversation_root: PathBuf,
    workspace_root: PathBuf,
    agent_server_path: PathBuf,
    state: ConversationState,
    config: Arc<StellaclawConfig>,
    cron_manager: Arc<CronManager>,
    outgoing_tx: Sender<OutgoingDispatch>,
    logger: Arc<StellaclawLogger>,
    host_logger: Arc<StellaclawLogger>,
    foreground: ForegroundSessionRuntime,
    background: BTreeMap<String, ManagedSessionRuntime>,
    subagents: BTreeMap<String, ManagedSessionRuntime>,
    foreground_progress: Option<ActiveForegroundProgress>,
    session_plans: BTreeMap<String, TaskPlanView>,
    /// Set to `true` by the sshfs watchdog thread when the mount becomes
    /// unresponsive.  The main conversation loop checks this flag each
    /// iteration and tears down the conversation when it fires.
    sshfs_failed: Arc<AtomicBool>,
}

struct ForegroundSessionRuntime {
    client: Option<AgentServerClient>,
    events: Option<mpsc::Receiver<SessionEvent>>,
}

struct ManagedSessionRuntime {
    record: ManagedSessionRecord,
    client: Option<AgentServerClient>,
    events: Option<mpsc::Receiver<SessionEvent>>,
}

#[derive(Debug, Clone)]
struct ActiveForegroundProgress {
    turn_id: String,
    next_typing_at: Instant,
    activity: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct TaskPlanView {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    explanation: Option<String>,
    #[serde(default)]
    plan: Vec<TaskPlanItemView>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct TaskPlanItemView {
    step: String,
    status: TaskPlanItemStatus,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum TaskPlanItemStatus {
    Pending,
    InProgress,
    Completed,
}

impl ConversationRuntime {
    fn new(
        workdir: PathBuf,
        conversation_root: PathBuf,
        state: ConversationState,
        config: Arc<StellaclawConfig>,
        cron_manager: Arc<CronManager>,
        agent_server_path: PathBuf,
        outgoing_tx: Sender<OutgoingDispatch>,
        logger: Arc<StellaclawLogger>,
        host_logger: Arc<StellaclawLogger>,
    ) -> Result<Self> {
        let workspace_root = ensure_workspace_for_remote_mode(
            &workdir,
            &conversation_root,
            &state.conversation_id,
            &state.tool_remote_mode,
        )?;
        let main_model = config
            .resolve_session_model(&state.session_profile)
            .ok_or_else(|| anyhow!("unknown main model selection"))?;
        let foreground = start_foreground_session(
            &agent_server_path,
            &conversation_root,
            &workspace_root,
            &state.session_binding.foreground_session_id,
            &main_model,
            &state.tool_remote_mode,
            state.sandbox.as_ref().unwrap_or(&config.sandbox),
            state.reasoning_effort.as_deref(),
            &config.models,
            &config.session_defaults,
        )?;

        let mut background = BTreeMap::new();
        for (agent_id, record) in &state.session_binding.background_sessions {
            if record.status != ManagedSessionStatus::Running {
                continue;
            }
            let model = resolve_managed_session_model(&config, record, &main_model)?;
            background.insert(
                agent_id.clone(),
                start_managed_session_runtime(
                    &agent_server_path,
                    &conversation_root,
                    &workspace_root,
                    record.clone(),
                    &model,
                    &state.tool_remote_mode,
                    state.sandbox.as_ref().unwrap_or(&config.sandbox),
                    state.reasoning_effort.as_deref(),
                    &config.models,
                    &config.session_defaults,
                )?,
            );
        }

        let mut subagents = BTreeMap::new();
        for (agent_id, record) in &state.session_binding.subagent_sessions {
            if record.status != ManagedSessionStatus::Running {
                continue;
            }
            let model = resolve_managed_session_model(&config, record, &main_model)?;
            subagents.insert(
                agent_id.clone(),
                start_managed_session_runtime(
                    &agent_server_path,
                    &conversation_root,
                    &workspace_root,
                    record.clone(),
                    &model,
                    &state.tool_remote_mode,
                    state.sandbox.as_ref().unwrap_or(&config.sandbox),
                    state.reasoning_effort.as_deref(),
                    &config.models,
                    &config.session_defaults,
                )?,
            );
        }

        let sshfs_failed = Arc::new(AtomicBool::new(false));

        // Spawn an sshfs watchdog thread for FixedSsh conversations.
        // Skip the watchdog for localhost workspaces (symlinks, not FUSE mounts).
        let is_symlink_workspace = workspace_root
            .symlink_metadata()
            .map_or(false, |m| m.file_type().is_symlink());
        if matches!(state.tool_remote_mode, ToolRemoteMode::FixedSsh { .. })
            && !is_symlink_workspace
        {
            let flag = sshfs_failed.clone();
            let check_path = workspace_root.clone();
            let watchdog_logger = logger.clone();
            let conv_id = state.conversation_id.clone();
            thread::Builder::new()
                .name(format!("sshfs-watchdog-{conv_id}"))
                .spawn(move || {
                    sshfs_watchdog_loop(&check_path, &flag, &watchdog_logger, &conv_id);
                })
                .context("failed to spawn sshfs watchdog thread")?;
        }

        Ok(Self {
            workdir,
            conversation_root,
            workspace_root,
            agent_server_path,
            state,
            config,
            cron_manager,
            outgoing_tx,
            logger,
            host_logger,
            foreground,
            background,
            subagents,
            foreground_progress: None,
            session_plans: BTreeMap::new(),
            sshfs_failed,
        })
    }

    fn persist_state(&self) -> Result<()> {
        let path = self.conversation_root.join("conversation.json");
        let raw = serde_json::to_string_pretty(&self.state)
            .context("failed to serialize conversation state")?;
        fs::write(&path, raw).with_context(|| format!("failed to write {}", path.display()))
    }

    fn persist_state_and_publish(&self) -> Result<()> {
        self.persist_state()?;
        self.send_conversation_updated()
    }

    fn current_main_model(&self) -> Result<ModelConfig> {
        self.config
            .resolve_session_model(&self.state.session_profile)
            .ok_or_else(|| anyhow!("unknown main model selection"))
    }

    fn current_main_model_name(&self) -> String {
        self.state
            .session_profile
            .main_model
            .display_name(&self.config.models)
    }

    fn handle_incoming(&mut self, message: IncomingConversationMessage) -> Result<bool> {
        if self.state.model_selection_pending {
            match &message.control {
                Some(ConversationControl::ShowModel) => {
                    self.send_model_selection()?;
                    return Ok(false);
                }
                Some(ConversationControl::SwitchModel { model_name }) => {
                    match self.switch_main_model(model_name) {
                        Ok(()) => return Ok(true),
                        Err(error) => {
                            self.send_channel_error(
                                OutgoingErrorScope::Configuration,
                                OutgoingErrorSeverity::Error,
                                "model_switch_failed",
                                format!("模型切换失败: {error}"),
                                Some(json!({"error": format!("{error:#}")})),
                                false,
                                None,
                            )?;
                            return Ok(false);
                        }
                    }
                }
                Some(
                    ConversationControl::ShowStatus
                    | ConversationControl::Compact
                    | ConversationControl::ShowRemote
                    | ConversationControl::SetRemote { .. }
                    | ConversationControl::DisableRemote
                    | ConversationControl::InvalidRemote { .. }
                    | ConversationControl::ShowReasoning
                    | ConversationControl::SetReasoning { .. }
                    | ConversationControl::InvalidReasoning { .. }
                    | ConversationControl::ShowSandbox
                    | ConversationControl::SetSandbox { .. }
                    | ConversationControl::InvalidSandbox { .. },
                ) => {}
                _ => {
                    self.send_model_selection()?;
                    return Ok(false);
                }
            }
        }

        match message.control {
            Some(ConversationControl::Continue) => {
                self.foreground_client()?
                    .send_session_request(&SessionRequest::ContinueTurn { reason: None })
                    .map_err(anyhow::Error::msg)?;
                return Ok(false);
            }
            Some(ConversationControl::Cancel) => {
                self.foreground_client()?
                    .send_session_request(&SessionRequest::CancelTurn { reason: None })
                    .map_err(anyhow::Error::msg)?;
                return Ok(false);
            }
            Some(ConversationControl::Compact) => {
                self.foreground_client()?
                    .send_session_request(&SessionRequest::CompactNow)
                    .map_err(anyhow::Error::msg)?;
                return Ok(false);
            }
            Some(ConversationControl::ShowStatus) => {
                self.send_status()?;
                return Ok(false);
            }
            Some(ConversationControl::ShowModel) => {
                self.send_model_selection()?;
                return Ok(false);
            }
            Some(ConversationControl::SwitchModel { model_name }) => {
                match self.switch_main_model(&model_name) {
                    Ok(()) => return Ok(true),
                    Err(error) => {
                        self.send_channel_error(
                            OutgoingErrorScope::Configuration,
                            OutgoingErrorSeverity::Error,
                            "model_switch_failed",
                            format!("模型切换失败: {error}"),
                            Some(json!({"model_name": model_name, "error": format!("{error:#}")})),
                            false,
                            None,
                        )?;
                        return Ok(false);
                    }
                }
            }
            Some(ConversationControl::ShowReasoning) => {
                self.send_reasoning_status()?;
                return Ok(false);
            }
            Some(ConversationControl::SetReasoning { effort }) => {
                match self.set_reasoning_effort(effort) {
                    Ok(()) => return Ok(true),
                    Err(error) => {
                        self.send_channel_error(
                            OutgoingErrorScope::Configuration,
                            OutgoingErrorSeverity::Error,
                            "reasoning_effort_switch_failed",
                            format!("reasoning effort 切换失败: {error}"),
                            Some(json!({"error": format!("{error:#}")})),
                            false,
                            None,
                        )?;
                        return Ok(false);
                    }
                }
            }
            Some(ConversationControl::InvalidReasoning { reason }) => {
                self.send_delivery_from_text(format!(
                    "{reason}\n用法: `/reasoning`，`/reasoning low`，`/reasoning medium`，`/reasoning high`，`/reasoning xhigh`，`/reasoning default`。"
                ))?;
                return Ok(false);
            }
            Some(ConversationControl::ShowRemote) => {
                self.send_remote_status()?;
                return Ok(false);
            }
            Some(ConversationControl::SetRemote { host, path }) => {
                match self.set_remote_mode(host, path) {
                    Ok(()) => return Ok(true),
                    Err(error) => {
                        self.send_channel_error(
                            OutgoingErrorScope::RemoteWorkspace,
                            OutgoingErrorSeverity::Error,
                            "remote_workspace_switch_failed",
                            format!("远程 workspace 切换失败: {error}"),
                            Some(json!({"error": format!("{error:#}")})),
                            false,
                            None,
                        )?;
                        return Ok(false);
                    }
                }
            }
            Some(ConversationControl::DisableRemote) => match self.disable_remote_mode() {
                Ok(()) => return Ok(true),
                Err(error) => {
                    self.send_channel_error(
                        OutgoingErrorScope::RemoteWorkspace,
                        OutgoingErrorSeverity::Error,
                        "remote_workspace_disable_failed",
                        format!("关闭远程 workspace 失败: {error}"),
                        Some(json!({"error": format!("{error:#}")})),
                        false,
                        None,
                    )?;
                    return Ok(false);
                }
            },
            Some(ConversationControl::InvalidRemote { reason }) => {
                self.send_delivery_from_text(format!(
                    "{reason}\n用法: `/remote <ssh-host> <path>`，查看状态: `/remote`，关闭: `/remote off`。"
                ))?;
                return Ok(false);
            }
            Some(ConversationControl::ShowSandbox) => {
                self.send_sandbox_status()?;
                return Ok(false);
            }
            Some(ConversationControl::SetSandbox { mode }) => match self.set_sandbox_mode(mode) {
                Ok(()) => return Ok(true),
                Err(error) => {
                    self.send_channel_error(
                        OutgoingErrorScope::Sandbox,
                        OutgoingErrorSeverity::Error,
                        "sandbox_switch_failed",
                        format!("沙盒模式切换失败: {error}"),
                        Some(json!({"error": format!("{error:#}")})),
                        false,
                        None,
                    )?;
                    return Ok(false);
                }
            },
            Some(ConversationControl::InvalidSandbox { reason }) => {
                self.send_delivery_from_text(format!(
                    "{reason}\n用法: `/sandbox`，`/sandbox bubblewrap`，`/sandbox subprocess`，`/sandbox default`。"
                ))?;
                return Ok(false);
            }
            None => {}
        }

        let mut data = Vec::new();
        if let Some(text) = message.text.filter(|text| !text.trim().is_empty()) {
            data.push(ChatMessageItem::Context(ContextItem { text }));
        }
        for file in message.files {
            data.push(ChatMessageItem::File(file));
        }
        if data.is_empty() {
            return Ok(false);
        }

        self.foreground_client()?
            .send_session_request(&SessionRequest::EnqueueUserMessage {
                message: ChatMessage::new(ChatRole::User, data)
                    .with_user_name_option(message.user_name)
                    .with_message_time_option(message.message_time),
            })
            .map_err(anyhow::Error::msg)?;
        self.logger.info(
            "foreground_user_message_enqueued",
            json!({
                "conversation_id": self.state.conversation_id,
                "remote_message_id": message.remote_message_id,
            }),
        );
        Ok(false)
    }

    fn pump_session_events(&mut self) -> Result<bool> {
        if self.pump_one_foreground_event()? {
            return Ok(true);
        }

        let background_ids = self.background.keys().cloned().collect::<Vec<_>>();
        for agent_id in background_ids {
            if self.pump_one_managed_event(&agent_id, ManagedSessionType::Background)? {
                return Ok(true);
            }
        }

        let subagent_ids = self.subagents.keys().cloned().collect::<Vec<_>>();
        for agent_id in subagent_ids {
            if self.pump_one_managed_event(&agent_id, ManagedSessionType::Subagent)? {
                return Ok(true);
            }
        }

        Ok(false)
    }

    fn pump_one_foreground_event(&mut self) -> Result<bool> {
        let Some(events) = self.foreground.events.as_ref() else {
            return Ok(false);
        };
        let Ok(event) = events.try_recv() else {
            return Ok(false);
        };
        self.handle_session_event(None, SessionType::Foreground, event)
    }

    fn pump_one_managed_event(&mut self, agent_id: &str, kind: ManagedSessionType) -> Result<bool> {
        let runtime = match kind {
            ManagedSessionType::Background => self.background.get(agent_id),
            ManagedSessionType::Subagent => self.subagents.get(agent_id),
        };
        let Some(runtime) = runtime else {
            return Ok(false);
        };
        let Some(events) = runtime.events.as_ref() else {
            return Ok(false);
        };
        let Ok(event) = events.try_recv() else {
            return Ok(false);
        };
        self.handle_session_event(Some(agent_id.to_string()), to_session_type(kind), event)
    }

    fn handle_session_event(
        &mut self,
        agent_id: Option<String>,
        session_type: SessionType,
        event: SessionEvent,
    ) -> Result<bool> {
        match event {
            SessionEvent::MessageAppended { index, message } => {
                self.logger.info(
                    "message_appended",
                    json!({
                        "session_type": format!("{session_type:?}"),
                        "agent_id": agent_id,
                        "index": index,
                        "role": message.role,
                        "items": message.data.len(),
                    }),
                );
                if session_type == SessionType::Foreground {
                    self.send_foreground_message_appended(index, message)?;
                }
                Ok(false)
            }
            SessionEvent::TurnStarted { turn_id } => {
                self.clear_session_plan(agent_id.as_deref(), session_type);
                self.logger.info(
                    "turn_started",
                    json!({
                        "session_type": format!("{session_type:?}"),
                        "agent_id": agent_id,
                        "turn_id": turn_id,
                    }),
                );
                if session_type == SessionType::Foreground {
                    self.start_foreground_progress(turn_id)?;
                }
                Ok(false)
            }
            SessionEvent::Progress { message } => {
                self.logger.info(
                    "progress",
                    json!({
                        "session_type": format!("{session_type:?}"),
                        "agent_id": agent_id,
                        "message": message,
                    }),
                );
                if session_type == SessionType::Foreground {
                    self.update_foreground_progress(&message)?;
                }
                Ok(false)
            }
            SessionEvent::TurnCompleted { message } => {
                self.on_turn_completed(agent_id, session_type, message)
            }
            SessionEvent::TurnFailed {
                error,
                error_detail,
                can_continue,
            } => self.on_turn_failed(agent_id, session_type, error, error_detail, can_continue),
            SessionEvent::HostCoordinationRequested { request } => {
                self.on_host_coordination(agent_id, session_type, request)?;
                Ok(false)
            }
            SessionEvent::InteractiveOutputRequested { payload } => {
                self.logger.info("interactive_output_requested", payload);
                Ok(false)
            }
            SessionEvent::SessionViewResult { query_id, payload } => {
                self.logger.info(
                    "session_view_result",
                    json!({"query_id": query_id, "payload": payload}),
                );
                Ok(false)
            }
            SessionEvent::CompactCompleted {
                compressed,
                estimated_tokens_before,
                estimated_tokens_after,
                threshold_tokens,
                retained_message_count,
                compressed_message_count,
            } => {
                self.logger.info(
                    "compact_completed",
                    json!({
                        "compressed": compressed,
                        "estimated_tokens_before": estimated_tokens_before,
                        "estimated_tokens_after": estimated_tokens_after,
                        "threshold_tokens": threshold_tokens,
                        "retained_message_count": retained_message_count,
                        "compressed_message_count": compressed_message_count,
                    }),
                );
                if session_type == SessionType::Foreground {
                    self.send_delivery_from_text(format_compact_completed_message(
                        compressed,
                        estimated_tokens_before,
                        estimated_tokens_after,
                        threshold_tokens,
                        retained_message_count,
                        compressed_message_count,
                    ))?;
                }
                Ok(false)
            }
            SessionEvent::ControlRejected { reason, payload } => {
                self.logger.warn(
                    "control_rejected",
                    json!({"reason": reason, "payload": payload, "agent_id": agent_id}),
                );
                if session_type == SessionType::Foreground
                    && payload.get("type").and_then(serde_json::Value::as_str)
                        == Some("compact_now")
                {
                    self.send_channel_error(
                        OutgoingErrorScope::Control,
                        OutgoingErrorSeverity::Warning,
                        "control_rejected",
                        format!("无法执行 /compact: {reason}"),
                        Some(json!({"reason": reason, "payload": payload})),
                        false,
                        None,
                    )?;
                }
                Ok(false)
            }
            SessionEvent::RuntimeCrashed {
                error,
                error_detail,
            } => {
                self.clear_session_plan(agent_id.as_deref(), session_type);
                self.host_logger.warn(
                    "session_runtime_crashed",
                    json!({
                        "conversation_id": self.state.conversation_id,
                        "session_type": format!("{session_type:?}"),
                        "agent_id": agent_id,
                        "error": error,
                        "error_detail": error_detail,
                    }),
                );
                self.send_channel_error(
                    OutgoingErrorScope::Runtime,
                    OutgoingErrorSeverity::Error,
                    "session_runtime_crashed",
                    "Session runtime crashed.",
                    Some(json!({"error": error, "error_detail": error_detail})),
                    true,
                    Some("发送 /continue 可尝试继续。".to_string()),
                )?;
                Ok(false)
            }
        }
    }

    fn on_turn_completed(
        &mut self,
        agent_id: Option<String>,
        session_type: SessionType,
        message: ChatMessage,
    ) -> Result<bool> {
        self.clear_session_plan(agent_id.as_deref(), session_type);
        match session_type {
            SessionType::Foreground => {
                self.finish_foreground_progress(ProgressFeedbackFinalState::Done, None)?;
                self.send_delivery_from_message(message)?;
                Ok(false)
            }
            SessionType::Background => {
                let Some(agent_id) = agent_id else {
                    return Ok(false);
                };
                let Some(runtime) = self.background.get_mut(&agent_id) else {
                    return Ok(false);
                };
                runtime.record.status = ManagedSessionStatus::Completed;
                runtime.record.last_message = Some(message.clone());
                if let Some(record) = self
                    .state
                    .session_binding
                    .background_sessions
                    .get_mut(&agent_id)
                {
                    record.status = ManagedSessionStatus::Completed;
                    record.last_message = Some(message.clone());
                }
                if !runtime.record.suppress_output {
                    let rendered = render_chat_message(&message);
                    self.send_delivery_from_text(rendered.clone())?;
                    let actor_message = ChatMessage::new(
                        ChatRole::Assistant,
                        vec![ChatMessageItem::Context(ContextItem { text: rendered })],
                    );
                    let _ = self.foreground_client()?.send_session_request(
                        &SessionRequest::EnqueueActorMessage {
                            message: actor_message,
                        },
                    );
                }
                Ok(true)
            }
            SessionType::Subagent => {
                let Some(agent_id) = agent_id else {
                    return Ok(false);
                };
                let Some(runtime) = self.subagents.get_mut(&agent_id) else {
                    return Ok(false);
                };
                runtime.record.status = ManagedSessionStatus::Completed;
                runtime.record.last_message = Some(message.clone());
                if let Some(record) = self
                    .state
                    .session_binding
                    .subagent_sessions
                    .get_mut(&agent_id)
                {
                    record.status = ManagedSessionStatus::Completed;
                    record.last_message = Some(message);
                }
                Ok(true)
            }
        }
    }

    fn on_turn_failed(
        &mut self,
        agent_id: Option<String>,
        session_type: SessionType,
        error: String,
        error_detail: SessionErrorDetail,
        can_continue: bool,
    ) -> Result<bool> {
        self.clear_session_plan(agent_id.as_deref(), session_type);
        let visible_error = format_session_error(&error, &error_detail);
        match session_type {
            SessionType::Foreground => {
                self.finish_foreground_progress(
                    ProgressFeedbackFinalState::Failed,
                    Some(&visible_error),
                )?;
                self.send_channel_error(
                    OutgoingErrorScope::Turn,
                    OutgoingErrorSeverity::Error,
                    "turn_failed",
                    format!("本轮失败: {visible_error}"),
                    Some(json!({
                        "error": error,
                        "error_detail": error_detail,
                        "session_type": format!("{session_type:?}"),
                    })),
                    can_continue,
                    can_continue
                        .then(|| "发送 /continue 继续，或 /cancel 取消当前回合。".to_string()),
                )?;
                Ok(false)
            }
            SessionType::Background => {
                let detail_agent_id = agent_id.clone();
                if let Some(agent_id) = agent_id {
                    if let Some(runtime) = self.background.get_mut(&agent_id) {
                        runtime.record.status = ManagedSessionStatus::Failed;
                        runtime.record.last_error = Some(visible_error.clone());
                    }
                    if let Some(record) = self
                        .state
                        .session_binding
                        .background_sessions
                        .get_mut(&agent_id)
                    {
                        record.status = ManagedSessionStatus::Failed;
                        record.last_error = Some(visible_error.clone());
                    }
                }
                self.send_channel_error(
                    OutgoingErrorScope::BackgroundSession,
                    OutgoingErrorSeverity::Error,
                    "background_session_failed",
                    format!("后台任务失败: {visible_error}"),
                    Some(json!({
                        "agent_id": detail_agent_id,
                        "error": error,
                        "error_detail": error_detail,
                    })),
                    can_continue,
                    None,
                )?;
                Ok(true)
            }
            SessionType::Subagent => {
                if let Some(agent_id) = agent_id {
                    if let Some(runtime) = self.subagents.get_mut(&agent_id) {
                        runtime.record.status = ManagedSessionStatus::Failed;
                        runtime.record.last_error = Some(visible_error.clone());
                    }
                    if let Some(record) = self
                        .state
                        .session_binding
                        .subagent_sessions
                        .get_mut(&agent_id)
                    {
                        record.status = ManagedSessionStatus::Failed;
                        record.last_error = Some(visible_error);
                    }
                }
                Ok(true)
            }
        }
    }

    fn on_host_coordination(
        &mut self,
        agent_id: Option<String>,
        session_type: SessionType,
        request: ConversationBridgeRequest,
    ) -> Result<()> {
        self.logger.info(
            "host_coordination_requested",
            json!({
                "agent_id": &agent_id,
                "session_type": format!("{session_type:?}"),
                "request_id": &request.request_id,
                "action": &request.action,
                "tool_name": &request.tool_name,
            }),
        );
        let tool_result = match self.handle_bridge_action(agent_id.clone(), session_type, &request)
        {
            Ok(result) => result,
            Err(error) => {
                bridge_result(&request, json!({"error": format!("{error:#}")}).to_string())
            }
        };
        self.logger.info(
            "host_coordination_action_completed",
            json!({
                "agent_id": &agent_id,
                "session_type": format!("{session_type:?}"),
                "request_id": &request.request_id,
                "action": &request.action,
                "tool_name": &request.tool_name,
                "tool_call_id": &request.tool_call_id,
            }),
        );
        let response = ConversationBridgeResponse {
            request_id: request.request_id.clone(),
            tool_call_id: request.tool_call_id.clone(),
            tool_name: request.tool_name.clone(),
            result: tool_result,
        };
        self.logger.info(
            "host_coordination_resolve_started",
            json!({
                "agent_id": &agent_id,
                "session_type": format!("{session_type:?}"),
                "request_id": &response.request_id,
                "tool_name": &response.tool_name,
                "tool_call_id": &response.tool_call_id,
            }),
        );
        match self
            .client_for_session(agent_id.as_deref(), session_type)?
            .send_session_request(&SessionRequest::ResolveHostCoordination { response })
        {
            Ok(()) => {
                self.logger.info(
                    "host_coordination_resolved",
                    json!({
                        "agent_id": &agent_id,
                        "session_type": format!("{session_type:?}"),
                        "request_id": &request.request_id,
                        "action": &request.action,
                        "tool_name": &request.tool_name,
                    }),
                );
                Ok(())
            }
            Err(error) => {
                let error = error.to_string();
                self.logger.warn(
                    "host_coordination_resolution_failed",
                    json!({
                        "agent_id": &agent_id,
                        "session_type": format!("{session_type:?}"),
                        "request_id": &request.request_id,
                        "action": &request.action,
                        "tool_name": &request.tool_name,
                        "error": error,
                    }),
                );
                Ok(())
            }
        }
    }

    fn handle_bridge_action(
        &mut self,
        agent_id: Option<String>,
        session_type: SessionType,
        request: &ConversationBridgeRequest,
    ) -> Result<ToolResultItem> {
        match request.action.as_str() {
            "user_tell" => {
                let text = request
                    .payload
                    .get("text")
                    .and_then(Value::as_str)
                    .ok_or_else(|| anyhow!("user_tell requires text"))?;
                self.send_delivery_from_text(text.to_string())?;
                Ok(bridge_result(request, json!({"sent": true}).to_string()))
            }
            "update_plan" => {
                let plan = parse_task_plan_view(&request.payload)?;
                self.update_session_plan(agent_id.as_deref(), session_type, plan.clone())?;
                Ok(bridge_result(
                    request,
                    json!({"updated": true, "plan": plan}).to_string(),
                ))
            }
            "start_background_agent" => {
                let task = request
                    .payload
                    .get("task")
                    .and_then(Value::as_str)
                    .ok_or_else(|| anyhow!("start_background_agent requires task"))?;
                let model_override = self.resolve_model_override(&request.payload)?;
                let started = self.start_managed_session(
                    ManagedSessionType::Background,
                    task.to_string(),
                    model_override,
                )?;
                Ok(bridge_result(request, serde_json::to_string(&started)?))
            }
            "subagent_start" => {
                let description = request
                    .payload
                    .get("description")
                    .and_then(Value::as_str)
                    .ok_or_else(|| anyhow!("subagent_start requires description"))?;
                let started = self.start_managed_session(
                    ManagedSessionType::Subagent,
                    description.to_string(),
                    None,
                )?;
                Ok(bridge_result(request, serde_json::to_string(&started)?))
            }
            "subagent_kill" => {
                let target = request
                    .payload
                    .get("agent_id")
                    .and_then(Value::as_str)
                    .ok_or_else(|| anyhow!("subagent_kill requires agent_id"))?;
                self.kill_subagent(target)?;
                Ok(bridge_result(request, json!({"killed": true}).to_string()))
            }
            "subagent_join" => {
                let target = request
                    .payload
                    .get("agent_id")
                    .and_then(Value::as_str)
                    .ok_or_else(|| anyhow!("subagent_join requires agent_id"))?;
                let timeout_seconds = request
                    .payload
                    .get("timeout_seconds")
                    .and_then(Value::as_f64)
                    .unwrap_or(0.0);
                let snapshot = self.join_subagent(target, timeout_seconds)?;
                Ok(bridge_result(request, serde_json::to_string(&snapshot)?))
            }
            "terminate" => {
                if session_type != SessionType::Background {
                    return Err(anyhow!("terminate is only valid in background sessions"));
                }
                let Some(agent_id) = agent_id else {
                    return Err(anyhow!("missing background agent id"));
                };
                let Some(runtime) = self.background.get_mut(&agent_id) else {
                    return Err(anyhow!("unknown background agent {agent_id}"));
                };
                runtime.record.suppress_output = true;
                runtime.record.status = ManagedSessionStatus::Killed;
                if let Some(record) = self
                    .state
                    .session_binding
                    .background_sessions
                    .get_mut(&agent_id)
                {
                    record.suppress_output = true;
                    record.status = ManagedSessionStatus::Killed;
                }
                Ok(bridge_result(
                    request,
                    json!({"terminated": true}).to_string(),
                ))
            }
            "skill_create" => self.persist_skill(request, SkillPersistMode::Create),
            "skill_update" => self.persist_skill(request, SkillPersistMode::Update),
            "skill_delete" => self.persist_skill(request, SkillPersistMode::Delete),
            "list_cron_tasks" => {
                let tasks = self
                    .cron_manager
                    .list_for_conversation(&self.state.conversation_id)?;
                Ok(bridge_result(request, serde_json::to_string(&tasks)?))
            }
            "get_cron_task" => {
                let id = request
                    .payload
                    .get("id")
                    .and_then(Value::as_str)
                    .ok_or_else(|| anyhow!("get_cron_task requires id"))?;
                let task = self
                    .cron_manager
                    .get_for_conversation(&self.state.conversation_id, id)?;
                Ok(bridge_result(request, serde_json::to_string(&task)?))
            }
            "create_cron_task" => {
                let object = request
                    .payload
                    .as_object()
                    .ok_or_else(|| anyhow!("create_cron_task payload must be an object"))?;
                let task = self.cron_manager.create_task(CreateCronTaskRequest {
                    conversation_id: self.state.conversation_id.clone(),
                    channel_id: self.state.channel_id.clone(),
                    platform_chat_id: self.state.platform_chat_id.clone(),
                    name: string_arg_required(object, "name")?,
                    description: string_arg_required(object, "description")?,
                    schedule: cron_schedule_from_required_tool_args(object)?,
                    timezone: timezone_or_default(optional_string_arg(object, "timezone")?)?,
                    task: optional_string_arg(object, "task")?.unwrap_or_default(),
                    model: optional_string_arg(object, "model")?,
                    script_command: optional_string_arg(object, "script_command")?,
                    script_timeout_seconds: optional_positive_f64_arg(
                        object,
                        "script_timeout_seconds",
                    )?,
                    script_cwd: optional_string_arg(object, "script_cwd")?,
                })?;
                Ok(bridge_result(request, serde_json::to_string(&task)?))
            }
            "update_cron_task" => {
                let object = request
                    .payload
                    .as_object()
                    .ok_or_else(|| anyhow!("update_cron_task payload must be an object"))?;
                let id = string_arg_required(object, "id")?;
                let schedule = optional_cron_schedule_from_tool_args(object)?;
                let timezone = match optional_string_arg(object, "timezone")? {
                    Some(value) => Some(timezone_or_default(Some(value))?),
                    None => None,
                };
                let _clear_script = object
                    .get("clear_script")
                    .and_then(Value::as_bool)
                    .unwrap_or(false);
                let clear_task = object
                    .get("clear_task")
                    .and_then(Value::as_bool)
                    .unwrap_or(false);
                let model = optional_string_arg(object, "model")?.map(Some);
                let (script_command, script_timeout_seconds, script_cwd) = if _clear_script {
                    (Some(None), Some(None), Some(None))
                } else {
                    (
                        optional_string_arg(object, "script_command")?.map(Some),
                        optional_positive_f64_arg(object, "script_timeout_seconds")?.map(Some),
                        optional_string_arg(object, "script_cwd")?.map(Some),
                    )
                };
                let task_prompt = if clear_task {
                    Some(String::new())
                } else {
                    optional_string_arg(object, "task")?
                };
                let task = self.cron_manager.update_task(
                    &self.state.conversation_id,
                    &id,
                    UpdateCronTaskRequest {
                        name: optional_string_arg(object, "name")?,
                        description: optional_string_arg(object, "description")?,
                        schedule,
                        timezone,
                        task: task_prompt,
                        model,
                        script_command,
                        script_timeout_seconds,
                        script_cwd,
                        enabled: parse_enabled_flag(object)?,
                    },
                )?;
                Ok(bridge_result(request, serde_json::to_string(&task)?))
            }
            "remove_cron_task" => {
                let object = request
                    .payload
                    .as_object()
                    .ok_or_else(|| anyhow!("remove_cron_task payload must be an object"))?;
                let id = string_arg_required(object, "id")?;
                let removed = self
                    .cron_manager
                    .remove_task(&self.state.conversation_id, &id)?;
                Ok(bridge_result(request, serde_json::to_string(&removed)?))
            }
            _ => Ok(bridge_result(
                request,
                json!({"error": format!("unsupported host action {}", request.action)}).to_string(),
            )),
        }
    }

    fn update_session_plan(
        &mut self,
        agent_id: Option<&str>,
        session_type: SessionType,
        plan: TaskPlanView,
    ) -> Result<()> {
        let key = self.session_plan_key(agent_id, session_type);
        self.session_plans.insert(key, plan);
        if session_type == SessionType::Foreground {
            self.update_foreground_progress_from_state(true)?;
        }
        Ok(())
    }

    fn clear_session_plan(&mut self, agent_id: Option<&str>, session_type: SessionType) {
        let key = self.session_plan_key(agent_id, session_type);
        self.session_plans.remove(&key);
    }

    fn current_session_plan(
        &self,
        agent_id: Option<&str>,
        session_type: SessionType,
    ) -> Option<&TaskPlanView> {
        let key = self.session_plan_key(agent_id, session_type);
        self.session_plans.get(&key)
    }

    fn session_plan_key(&self, agent_id: Option<&str>, session_type: SessionType) -> String {
        match session_type {
            SessionType::Foreground => format!(
                "foreground:{}",
                self.state.session_binding.foreground_session_id
            ),
            SessionType::Background => format!("background:{}", agent_id.unwrap_or("unknown")),
            SessionType::Subagent => format!("subagent:{}", agent_id.unwrap_or("unknown")),
        }
    }

    fn persist_skill(
        &self,
        request: &ConversationBridgeRequest,
        mode: SkillPersistMode,
    ) -> Result<ToolResultItem> {
        let skill_name = request
            .payload
            .get("skill_name")
            .or_else(|| request.payload.get("name"))
            .and_then(Value::as_str)
            .ok_or_else(|| anyhow!("{} requires skill_name", request.action))?;
        validate_skill_name(skill_name)?;

        let runtime_skill_root = self.workdir.join("rundir").join(".skill");
        let runtime_skill_path = runtime_skill_root.join(skill_name);
        let staged_skill_path = self
            .workspace_root
            .join(".stellaclaw")
            .join("skill")
            .join(skill_name);

        match mode {
            SkillPersistMode::Create => {
                if runtime_skill_path.exists() {
                    return Err(anyhow!(
                        "skill {skill_name} already exists in runtime store"
                    ));
                }
                validate_skill_directory(&staged_skill_path, skill_name)?;
                copy_skill_atomically(&staged_skill_path, &runtime_skill_path)?;
                let synced = sync_skill_to_conversation_workspaces(
                    &self.workdir,
                    skill_name,
                    Some(&staged_skill_path),
                )?;
                Ok(bridge_result(
                    request,
                    json!({"created": true, "skill_name": skill_name, "synced_workspaces": synced})
                        .to_string(),
                ))
            }
            SkillPersistMode::Update => {
                if !runtime_skill_path.exists() {
                    return Err(anyhow!(
                        "skill {skill_name} does not exist in runtime store"
                    ));
                }
                validate_skill_directory(&staged_skill_path, skill_name)?;
                copy_skill_atomically(&staged_skill_path, &runtime_skill_path)?;
                let upstream_push = push_skill_sync_if_configured(
                    &self.config.skill_sync,
                    skill_name,
                    &runtime_skill_path,
                    &self.logger,
                );
                let synced = sync_skill_to_conversation_workspaces(
                    &self.workdir,
                    skill_name,
                    Some(&staged_skill_path),
                )?;
                Ok(bridge_result(
                    request,
                    json!({
                        "updated": true,
                        "skill_name": skill_name,
                        "synced_workspaces": synced,
                        "upstream_push": upstream_push,
                    })
                    .to_string(),
                ))
            }
            SkillPersistMode::Delete => {
                if !runtime_skill_path.exists() {
                    return Err(anyhow!(
                        "skill {skill_name} does not exist in runtime store"
                    ));
                }
                fs::remove_dir_all(&runtime_skill_path).with_context(|| {
                    format!("failed to remove {}", runtime_skill_path.display())
                })?;
                let synced =
                    sync_skill_to_conversation_workspaces(&self.workdir, skill_name, None)?;
                Ok(bridge_result(
                    request,
                    json!({"deleted": true, "skill_name": skill_name, "synced_workspaces": synced})
                        .to_string(),
                ))
            }
        }
    }

    fn resolve_model_override(&self, payload: &Value) -> Result<Option<String>> {
        let Some(name) = payload.get("model").and_then(Value::as_str) else {
            return Ok(None);
        };
        let model = self
            .config
            .resolve_named_model(name)
            .ok_or_else(|| anyhow!("unknown named model {name}"))?;
        if !self.config.is_available_agent_model(name) {
            return Err(anyhow!("model {name} is not available for agent selection"));
        }
        if !model.supports(ModelCapability::Chat) {
            return Err(anyhow!("model {name} is not chat-capable"));
        }
        Ok(Some(name.to_string()))
    }

    fn start_managed_session(
        &mut self,
        kind: ManagedSessionType,
        task: String,
        model_override: Option<String>,
    ) -> Result<Value> {
        let (agent_id, session_id) = match kind {
            ManagedSessionType::Background => {
                let index = self.state.session_binding.next_background_index;
                self.state.session_binding.next_background_index = index.saturating_add(1);
                (
                    format!("background_{index:04}"),
                    format!("{}.background.{index:04}", self.state.conversation_id),
                )
            }
            ManagedSessionType::Subagent => {
                let index = self.state.session_binding.next_subagent_index;
                self.state.session_binding.next_subagent_index = index.saturating_add(1);
                (
                    format!("subagent_{index:04}"),
                    format!("{}.subagent.{index:04}", self.state.conversation_id),
                )
            }
        };

        let record = ManagedSessionRecord {
            agent_id: agent_id.clone(),
            session_id: session_id.clone(),
            session_type: kind,
            status: ManagedSessionStatus::Running,
            last_message: None,
            last_error: None,
            suppress_output: false,
            model_override: model_override.clone(),
        };
        let main_model = self.current_main_model()?;
        let model = resolve_managed_session_model(&self.config, &record, &main_model)?;
        let runtime = start_managed_session_runtime(
            &self.agent_server_path,
            &self.conversation_root,
            &self.workspace_root,
            record.clone(),
            &model,
            &self.state.tool_remote_mode,
            self.state.sandbox.as_ref().unwrap_or(&self.config.sandbox),
            self.state.reasoning_effort.as_deref(),
            &self.config.models,
            &self.config.session_defaults,
        )?;

        runtime
            .client
            .as_ref()
            .context("missing managed session client")?
            .send_session_request(&SessionRequest::EnqueueUserMessage {
                message: ChatMessage::new(
                    ChatRole::User,
                    vec![ChatMessageItem::Context(ContextItem { text: task.clone() })],
                ),
            })
            .map_err(anyhow::Error::msg)?;

        match kind {
            ManagedSessionType::Background => {
                self.state
                    .session_binding
                    .background_sessions
                    .insert(agent_id.clone(), record.clone());
                self.background.insert(agent_id.clone(), runtime);
            }
            ManagedSessionType::Subagent => {
                self.state
                    .session_binding
                    .subagent_sessions
                    .insert(agent_id.clone(), record.clone());
                self.subagents.insert(agent_id.clone(), runtime);
            }
        }

        Ok(json!({
            "agent_id": agent_id,
            "session_id": session_id,
            "status": "started",
            "task": task,
        }))
    }

    fn join_subagent(&mut self, agent_id: &str, timeout_seconds: f64) -> Result<Value> {
        let deadline = if timeout_seconds > 0.0 {
            Some(Instant::now() + Duration::from_secs_f64(timeout_seconds))
        } else {
            None
        };

        loop {
            let Some(record) = self.state.session_binding.subagent_sessions.get(agent_id) else {
                return Err(anyhow!("unknown subagent {agent_id}"));
            };
            match record.status {
                ManagedSessionStatus::Completed => {
                    return Ok(json!({
                        "status": "completed",
                        "agent_id": agent_id,
                        "message": record.last_message.as_ref().map(render_chat_message),
                    }));
                }
                ManagedSessionStatus::Failed => {
                    return Ok(json!({
                        "status": "failed",
                        "agent_id": agent_id,
                        "error": record.last_error,
                    }));
                }
                ManagedSessionStatus::Killed => {
                    return Ok(json!({
                        "status": "killed",
                        "agent_id": agent_id,
                    }));
                }
                ManagedSessionStatus::Running => {}
            }

            if let Some(deadline) = deadline {
                if Instant::now() >= deadline {
                    return Ok(json!({
                        "status": "running",
                        "agent_id": agent_id,
                    }));
                }
            } else {
                return Ok(json!({
                    "status": "running",
                    "agent_id": agent_id,
                }));
            }

            while self.pump_session_events()? {}
            thread::sleep(Duration::from_millis(100));
        }
    }

    fn kill_subagent(&mut self, agent_id: &str) -> Result<()> {
        let mut runtime = self
            .subagents
            .remove(agent_id)
            .ok_or_else(|| anyhow!("unknown subagent {agent_id}"))?;
        if let Some(record) = self
            .state
            .session_binding
            .subagent_sessions
            .get_mut(agent_id)
        {
            record.status = ManagedSessionStatus::Killed;
        }
        if let Some(client) = runtime.client.take() {
            let _ = client.shutdown();
        }
        Ok(())
    }

    fn foreground_client(&self) -> Result<&AgentServerClient> {
        self.foreground
            .client
            .as_ref()
            .ok_or_else(|| anyhow!("missing foreground session client"))
    }

    fn client_for_session(
        &self,
        agent_id: Option<&str>,
        session_type: SessionType,
    ) -> Result<&AgentServerClient> {
        match session_type {
            SessionType::Foreground => self.foreground_client(),
            SessionType::Background => self
                .background
                .get(agent_id.context("missing background agent id")?)
                .and_then(|runtime| runtime.client.as_ref())
                .ok_or_else(|| anyhow!("missing background session client")),
            SessionType::Subagent => self
                .subagents
                .get(agent_id.context("missing subagent id")?)
                .and_then(|runtime| runtime.client.as_ref())
                .ok_or_else(|| anyhow!("missing subagent session client")),
        }
    }

    fn send_delivery_from_text(&self, text: String) -> Result<()> {
        let (clean_text, attachments) = match extract_attachment_references(
            &text,
            &self.workspace_root,
            &self.workdir.join("rundir").join("shared"),
        ) {
            Ok(parsed) => parsed,
            Err(error) => {
                let fallback = format!(
                    "{}\n\n⚠️ 附件发送失败: {error:#}",
                    strip_attachment_tags(&text).trim()
                );
                self.logger.warn(
                    "outgoing_attachment_resolution_failed",
                    json!({"error": format!("{error:#}")}),
                );
                self.send_channel_error(
                    OutgoingErrorScope::Attachment,
                    OutgoingErrorSeverity::Warning,
                    "attachment_resolution_failed",
                    format!("附件发送失败: {error}"),
                    Some(json!({"error": format!("{error:#}")})),
                    false,
                    None,
                )?;
                (fallback, Vec::new())
            }
        };
        if clean_text.trim().is_empty() && attachments.is_empty() {
            return Ok(());
        }
        self.send_delivery(clean_text, attachments, None)
    }

    fn send_delivery_from_message(&self, message: ChatMessage) -> Result<()> {
        self.outgoing_tx
            .send(OutgoingDispatch::Event(ChannelEvent::Delivery(
                OutgoingDelivery {
                    channel_id: self.state.channel_id.clone(),
                    platform_chat_id: self.state.platform_chat_id.clone(),
                    conversation_id: self.state.conversation_id.clone(),
                    session_id: Some(self.state.session_binding.foreground_session_id.clone()),
                    message: Some(message),
                    text: String::new(),
                    attachments: Vec::new(),
                    options: None,
                },
            )))
            .map_err(|_| anyhow!("outgoing delivery channel closed"))
    }

    fn send_foreground_message_appended(&self, index: usize, message: ChatMessage) -> Result<()> {
        self.outgoing_tx
            .send(OutgoingDispatch::Event(ChannelEvent::MessageAppended(
                OutgoingMessageAppended {
                    channel_id: self.state.channel_id.clone(),
                    platform_chat_id: self.state.platform_chat_id.clone(),
                    conversation_id: self.state.conversation_id.clone(),
                    session_id: self.state.session_binding.foreground_session_id.clone(),
                    index,
                    message,
                },
            )))
            .map_err(|_| anyhow!("outgoing message appended channel closed"))
    }

    fn send_delivery(
        &self,
        text: String,
        attachments: Vec<OutgoingAttachment>,
        options: Option<OutgoingOptions>,
    ) -> Result<()> {
        self.outgoing_tx
            .send(OutgoingDispatch::Event(ChannelEvent::Delivery(
                OutgoingDelivery {
                    channel_id: self.state.channel_id.clone(),
                    platform_chat_id: self.state.platform_chat_id.clone(),
                    conversation_id: self.state.conversation_id.clone(),
                    session_id: None,
                    message: None,
                    text,
                    attachments,
                    options,
                },
            )))
            .map_err(|_| anyhow!("outgoing delivery channel closed"))
    }

    fn send_channel_error(
        &self,
        scope: OutgoingErrorScope,
        severity: OutgoingErrorSeverity,
        code: impl Into<String>,
        message: impl Into<String>,
        detail: Option<Value>,
        can_continue: bool,
        suggested_action: Option<String>,
    ) -> Result<()> {
        self.outgoing_tx
            .send(OutgoingDispatch::Event(ChannelEvent::Error(
                OutgoingError {
                    channel_id: self.state.channel_id.clone(),
                    platform_chat_id: self.state.platform_chat_id.clone(),
                    conversation_id: self.state.conversation_id.clone(),
                    scope,
                    severity,
                    code: code.into(),
                    message: message.into(),
                    detail,
                    can_continue,
                    suggested_action,
                },
            )))
            .map_err(|_| anyhow!("outgoing event channel closed"))
    }

    fn send_processing_state(&self, state: ProcessingState) -> Result<()> {
        self.outgoing_tx
            .send(OutgoingDispatch::Event(ChannelEvent::Processing(
                OutgoingProcessing {
                    channel_id: self.state.channel_id.clone(),
                    platform_chat_id: self.state.platform_chat_id.clone(),
                    state,
                },
            )))
            .map_err(|_| anyhow!("outgoing processing channel closed"))
    }

    fn send_progress_feedback(
        &self,
        turn_id: String,
        progress: TurnProgress,
        final_state: Option<ProgressFeedbackFinalState>,
        important: bool,
    ) -> Result<()> {
        self.outgoing_tx
            .send(OutgoingDispatch::Event(ChannelEvent::ProgressFeedback(
                OutgoingProgressFeedback {
                    channel_id: self.state.channel_id.clone(),
                    platform_chat_id: self.state.platform_chat_id.clone(),
                    turn_id,
                    progress,
                    final_state,
                    important,
                },
            )))
            .map_err(|_| anyhow!("outgoing progress channel closed"))
    }

    fn send_conversation_updated(&self) -> Result<()> {
        self.outgoing_tx
            .send(OutgoingDispatch::Event(ChannelEvent::ConversationUpdated(
                OutgoingConversationUpdated {
                    channel_id: self.state.channel_id.clone(),
                    platform_chat_id: self.state.platform_chat_id.clone(),
                    conversation_id: self.state.conversation_id.clone(),
                },
            )))
            .map_err(|_| anyhow!("outgoing conversation update channel closed"))
    }

    fn start_foreground_progress(&mut self, turn_id: String) -> Result<()> {
        let now = Instant::now();
        let model_name = self.current_main_model_name();
        let progress_turn_id = self
            .foreground_progress
            .as_ref()
            .map(|progress| progress.turn_id.clone())
            .unwrap_or_else(|| turn_id.clone());
        self.foreground_progress = Some(ActiveForegroundProgress {
            turn_id: progress_turn_id.clone(),
            next_typing_at: now + TYPING_KEEPALIVE_INTERVAL,
            activity: None,
        });
        self.send_processing_state(ProcessingState::Typing)?;
        self.send_progress_feedback(
            progress_turn_id,
            progress_thinking(&model_name, None),
            None,
            true,
        )
    }

    fn update_foreground_progress(&mut self, message: &str) -> Result<()> {
        let Some(progress) = &mut self.foreground_progress else {
            return Ok(());
        };
        progress.activity = Some(message.trim().to_string()).filter(|value| !value.is_empty());
        self.update_foreground_progress_from_state(false)
    }

    fn update_foreground_progress_from_state(&self, important: bool) -> Result<()> {
        let Some(progress) = &self.foreground_progress else {
            return Ok(());
        };
        let model_name = self.current_main_model_name();
        let plan = self.current_session_plan(None, SessionType::Foreground);
        self.send_progress_feedback(
            progress.turn_id.clone(),
            progress_update(&model_name, progress.activity.as_deref(), plan),
            None,
            important,
        )
    }

    fn finish_foreground_progress(
        &mut self,
        final_state: ProgressFeedbackFinalState,
        error: Option<&str>,
    ) -> Result<()> {
        let Some(progress) = self.foreground_progress.take() else {
            return Ok(());
        };
        let model_name = self.current_main_model_name();
        self.send_processing_state(ProcessingState::Idle)?;
        let turn_progress = match final_state {
            ProgressFeedbackFinalState::Done => progress_done(&model_name),
            ProgressFeedbackFinalState::Failed => progress_failed(&model_name, error),
        };
        self.send_progress_feedback(progress.turn_id, turn_progress, Some(final_state), true)
    }

    fn pump_processing_keepalive(&mut self) -> Result<()> {
        let Some(progress) = &mut self.foreground_progress else {
            return Ok(());
        };
        let now = Instant::now();
        if now < progress.next_typing_at {
            return Ok(());
        }
        progress.next_typing_at = now + TYPING_KEEPALIVE_INTERVAL;
        self.send_processing_state(ProcessingState::Typing)
    }

    fn shutdown(&mut self) {
        let _ = self.finish_foreground_progress(
            ProgressFeedbackFinalState::Failed,
            Some("conversation stopped"),
        );
        if let Some(client) = self.foreground.client.take() {
            let _ = client.shutdown();
        }
        for runtime in self.background.values_mut() {
            if let Some(client) = runtime.client.take() {
                let _ = client.shutdown();
            }
        }
        for runtime in self.subagents.values_mut() {
            if let Some(client) = runtime.client.take() {
                let _ = client.shutdown();
            }
        }
    }

    fn run_cron_task(&mut self, task: CronTaskRecord) -> Result<bool> {
        if task.script_command.is_some() {
            return self.run_cron_script_task(task);
        }
        self.start_cron_background_agent(&task, task.task.clone())?;
        Ok(true)
    }

    fn start_cron_background_agent(&mut self, task: &CronTaskRecord, prompt: String) -> Result<()> {
        self.logger.info(
            "cron_task_starting_background_agent",
            json!({
                "task_id": task.id,
                "conversation_id": self.state.conversation_id,
                "model": task.model,
            }),
        );
        let model_override = match task.model.as_deref() {
            Some(name) => {
                let model = self
                    .config
                    .resolve_named_model(name)
                    .ok_or_else(|| anyhow!("unknown cron model {name}"))?;
                if !model.supports(ModelCapability::Chat) {
                    return Err(anyhow!("cron model {name} is not chat-capable"));
                }
                Some(name.to_string())
            }
            None => None,
        };
        let _ =
            self.start_managed_session(ManagedSessionType::Background, prompt, model_override)?;
        Ok(())
    }

    fn run_cron_script_task(&mut self, task: CronTaskRecord) -> Result<bool> {
        let command = task
            .script_command
            .as_deref()
            .context("cron script task missing script_command")?;
        let result = match run_script_command(
            &task,
            command,
            &self.conversation_root,
            &self.workspace_root,
            self.state.sandbox.as_ref().unwrap_or(&self.config.sandbox),
        ) {
            Ok(result) => result,
            Err(error) => {
                self.disable_cron_script_task(
                    &task,
                    format!("cron script execution failed: {error:#}"),
                    "",
                    "",
                )?;
                return Ok(false);
            }
        };
        if result.exit_code != Some(0) {
            let reason = format!(
                "cron script exited with code {}",
                result
                    .exit_code
                    .map(|code| code.to_string())
                    .unwrap_or_else(|| "unknown".to_string())
            );
            self.disable_cron_script_task(&task, reason, &result.stdout, &result.stderr)?;
            return Ok(false);
        }

        let messages = match parse_script_stdout(&result.stdout) {
            Ok(messages) => messages,
            Err(error) => {
                self.disable_cron_script_task(
                    &task,
                    format!("cron script stdout parse failed: {error:#}"),
                    &result.stdout,
                    &result.stderr,
                )?;
                return Ok(false);
            }
        };

        self.logger.info(
            "cron_script_emitted_messages",
            json!({
                "task_id": task.id,
                "conversation_id": self.state.conversation_id,
                "exit_code": result.exit_code,
                "messages": messages.len(),
                "stdout_bytes": result.stdout.len(),
                "stderr_bytes": result.stderr.len(),
            }),
        );

        let mut started_background = false;
        for message in messages {
            if self.deliver_cron_script_message(&task, message)? {
                started_background = true;
            }
        }
        Ok(started_background)
    }

    fn deliver_cron_script_message(
        &mut self,
        task: &CronTaskRecord,
        message: CronScriptMessage,
    ) -> Result<bool> {
        let mut started_background = false;
        for target in message.targets {
            match target {
                CronScriptTarget::User => {
                    self.send_delivery_from_text(message.text.clone())?;
                }
                CronScriptTarget::Foreground => {
                    self.send_foreground_actor_message(message.text.clone())?;
                }
                CronScriptTarget::Background => {
                    self.start_cron_background_agent(task, message.text.clone())?;
                    started_background = true;
                }
            }
        }
        Ok(started_background)
    }

    fn disable_cron_script_task(
        &mut self,
        task: &CronTaskRecord,
        reason: String,
        stdout: &str,
        stderr: &str,
    ) -> Result<()> {
        let _ = self.cron_manager.disable_task(
            &self.state.conversation_id,
            &task.id,
            reason.clone(),
        )?;
        self.logger.warn(
            "cron_script_disabled_task",
            json!({
                "task_id": task.id,
                "conversation_id": self.state.conversation_id,
                "reason": reason.clone(),
                "stdout_bytes": stdout.len(),
                "stderr_bytes": stderr.len(),
            }),
        );
        let notice = format!(
            "Cron task `{}` has been disabled because its script failed: {}{}{}",
            task.id,
            reason,
            format_script_output_section("stdout", stdout),
            format_script_output_section("stderr", stderr),
        );
        self.send_delivery_from_text(notice.clone())?;
        self.send_foreground_actor_message(notice)?;
        Ok(())
    }

    fn send_foreground_actor_message(&self, text: String) -> Result<()> {
        self.foreground_client()?
            .send_session_request(&SessionRequest::EnqueueActorMessage {
                message: ChatMessage::new(
                    ChatRole::User,
                    vec![ChatMessageItem::Context(ContextItem { text })],
                ),
            })
            .map_err(anyhow::Error::msg)
    }

    fn render_model_selection(&self) -> String {
        let current_alias = self.state.session_profile.main_model.alias_name();
        let current_name = self.current_main_model_name();
        let mut lines = if self.state.model_selection_pending {
            vec![format!("请选择 foreground 模型。当前预选: {current_name}")]
        } else {
            vec![format!("当前模型: {current_name}")]
        };
        if self.config.available_agent_models().is_empty() {
            return lines.join("\n");
        }

        lines.push("可切换模型:".to_string());
        for (name, model) in self.config.available_agent_models() {
            let marker = if Some(name.as_str()) == current_alias {
                " [current]"
            } else {
                ""
            };
            lines.push(format!("- {name}: {}{marker}", model.model_name));
        }
        lines.push("使用 `/model <name>` 切换。".to_string());
        lines.join("\n")
    }

    fn send_model_selection(&self) -> Result<()> {
        let prompt = self.render_model_selection();
        let options = self
            .config
            .available_agent_models()
            .into_iter()
            .map(|(name, _model)| {
                let marker =
                    if Some(name.as_str()) == self.state.session_profile.main_model.alias_name() {
                        " [current]"
                    } else {
                        ""
                    };
                OutgoingOption {
                    label: format!("{name}{marker}"),
                    value: format!("/model {name}"),
                }
            })
            .collect::<Vec<_>>();
        self.send_delivery(
            prompt,
            Vec::new(),
            (!options.is_empty()).then_some(OutgoingOptions {
                prompt: "选择要切换的模型".to_string(),
                options,
            }),
        )
    }

    fn send_status(&self) -> Result<()> {
        let status = conversation_status_snapshot(
            &self.workdir,
            &self.conversation_root,
            &self.workspace_root,
            &self.state,
            &self.config,
        )?;
        self.outgoing_tx
            .send(OutgoingDispatch::Event(ChannelEvent::Status(status)))
            .map_err(|_| anyhow!("outgoing status channel closed"))
    }

    fn send_reasoning_status(&self) -> Result<()> {
        let current = self
            .state
            .reasoning_effort
            .as_deref()
            .unwrap_or("model default");
        self.send_delivery_from_text(format!(
            "当前 reasoning effort: `{current}`。\n用法: `/reasoning low`，`/reasoning medium`，`/reasoning high`，`/reasoning xhigh`，`/reasoning default`。"
        ))
    }

    fn send_remote_status(&self) -> Result<()> {
        let text = match &self.state.tool_remote_mode {
            ToolRemoteMode::Selectable => {
                "当前 remote 模式: selectable。\n用法: `/remote <ssh-host> <path>`。".to_string()
            }
            ToolRemoteMode::FixedSsh { host, cwd } => {
                let path = cwd.as_deref().unwrap_or("");
                format!(
                    "当前 remote 模式: fixed ssh `{host}` `{path}`。\nworkspace: `{}`\n关闭: `/remote off`。",
                    self.workspace_root.display()
                )
            }
        };
        self.send_delivery_from_text(text)
    }

    fn send_sandbox_status(&self) -> Result<()> {
        let sandbox = effective_sandbox_config(self.state.sandbox.as_ref(), &self.config.sandbox);
        let source = if self.state.sandbox.is_some() {
            "conversation override"
        } else {
            "default config"
        };
        let support = match sandbox.mode {
            SandboxMode::Bubblewrap => bubblewrap_support_error(&sandbox)
                .map(|reason| format!("\n当前 bubblewrap 不可用: {reason}"))
                .unwrap_or_else(|| "\nbubblewrap 可用。".to_string()),
            SandboxMode::Subprocess => String::new(),
        };
        let software = match sandbox.software_dir.as_deref().map(str::trim) {
            Some(path) if !path.is_empty() => format!(
                "\nsoftware_dir: `{}` -> `{}`",
                path, sandbox.software_mount_path
            ),
            _ => "\nsoftware_dir: unset".to_string(),
        };
        self.send_delivery_from_text(format!(
            "当前 sandbox: `{}` ({source})\nbubblewrap_binary: `{}`{software}{support}\n用法: `/sandbox bubblewrap`，`/sandbox subprocess`，`/sandbox default`。",
            sandbox_mode_label(&sandbox.mode),
            sandbox.bubblewrap_binary,
        ))
    }

    fn set_remote_mode(&mut self, host: String, path: String) -> Result<()> {
        let old_mode = self.state.tool_remote_mode.clone();
        let new_mode = ToolRemoteMode::FixedSsh {
            host: host.trim().to_string(),
            cwd: Some(path.trim().to_string()),
        };
        if old_mode == new_mode {
            self.send_remote_status()?;
            return Ok(());
        }

        if let ToolRemoteMode::FixedSsh { .. } = old_mode {
            self.stop_running_managed_sessions_for_config_change(
                "stopped because conversation remote workspace changed",
            );
            let _ = unmount_sshfs_workspace(&self.workdir, &self.state.conversation_id);
        }
        let workspace_root = match ensure_workspace_for_remote_mode(
            &self.workdir,
            &self.conversation_root,
            &self.state.conversation_id,
            &new_mode,
        ) {
            Ok(workspace_root) => workspace_root,
            Err(error) => {
                if matches!(old_mode, ToolRemoteMode::FixedSsh { .. }) {
                    self.state.tool_remote_mode = ToolRemoteMode::Selectable;
                    self.workspace_root = ensure_workspace_for_remote_mode(
                        &self.workdir,
                        &self.conversation_root,
                        &self.state.conversation_id,
                        &self.state.tool_remote_mode,
                    )?;
                    self.restart_foreground_session()?;
                }
                return Err(error);
            }
        };
        self.stop_running_managed_sessions_for_config_change(
            "stopped because conversation remote workspace changed",
        );
        self.state.tool_remote_mode = new_mode;
        self.workspace_root = workspace_root;
        self.restart_foreground_session()?;
        self.send_delivery_from_text(format!(
            "已切换到远程 workspace `{host}` `{path}`。\n本地 conversation 目录保留在 `{}`，sshfs workspace 为 `{}`。",
            self.conversation_root.display(),
            self.workspace_root.display()
        ))?;
        self.send_status()?;
        Ok(())
    }

    fn set_sandbox_mode(&mut self, mode: Option<SandboxMode>) -> Result<()> {
        let new_sandbox = mode.map(|mode| SandboxConfig {
            mode,
            ..self.config.sandbox.clone()
        });
        let old_effective =
            effective_sandbox_config(self.state.sandbox.as_ref(), &self.config.sandbox).clone();
        let new_effective =
            effective_sandbox_config(new_sandbox.as_ref(), &self.config.sandbox).clone();
        let old_mode_label = sandbox_mode_label(&old_effective.mode);
        let new_mode_label = sandbox_mode_label(&new_effective.mode);
        if old_effective.mode == new_effective.mode
            && old_effective.bubblewrap_binary == new_effective.bubblewrap_binary
            && self.state.sandbox.is_some() == new_sandbox.is_some()
        {
            self.send_sandbox_status()?;
            return Ok(());
        }
        if matches!(new_effective.mode, SandboxMode::Bubblewrap) {
            if let Some(reason) = bubblewrap_support_error(&new_effective) {
                return Err(anyhow!(reason));
            }
        }

        self.stop_running_managed_sessions_for_config_change(
            "stopped because conversation sandbox changed",
        );
        self.state.sandbox = new_sandbox;
        self.restart_foreground_session()?;
        self.send_delivery_from_text(format!(
            "已切换 sandbox: `{}` -> `{}`{}。",
            old_mode_label,
            new_mode_label,
            if self.state.sandbox.is_some() {
                " (conversation override)"
            } else {
                " (default config)"
            }
        ))?;
        Ok(())
    }

    fn set_reasoning_effort(&mut self, effort: Option<String>) -> Result<()> {
        let old_label = self
            .state
            .reasoning_effort
            .as_deref()
            .unwrap_or("model default")
            .to_string();
        let new_effort = effort
            .map(|value| value.trim().to_ascii_lowercase())
            .filter(|value| !value.is_empty());
        if let Some(effort) = new_effort.as_deref() {
            match parse_reasoning_control_argument(effort) {
                ConversationControl::SetReasoning { effort: Some(_) } => {}
                _ => return Err(anyhow!("unknown reasoning effort {effort}")),
            }
        }
        let new_label = new_effort.as_deref().unwrap_or("model default").to_string();
        if self.state.reasoning_effort == new_effort {
            self.send_reasoning_status()?;
            return Ok(());
        }

        self.state.reasoning_effort = new_effort;
        self.restart_foreground_session()?;
        self.send_delivery_from_text(format!(
            "已切换 reasoning effort: `{old_label}` -> `{new_label}`。"
        ))?;
        self.send_status()?;
        Ok(())
    }

    fn disable_remote_mode(&mut self) -> Result<()> {
        if matches!(self.state.tool_remote_mode, ToolRemoteMode::Selectable) {
            self.send_remote_status()?;
            return Ok(());
        }
        self.stop_running_managed_sessions_for_config_change(
            "stopped because conversation remote workspace changed",
        );
        let _ = unmount_sshfs_workspace(&self.workdir, &self.state.conversation_id);
        self.state.tool_remote_mode = ToolRemoteMode::Selectable;
        self.workspace_root = ensure_workspace_for_remote_mode(
            &self.workdir,
            &self.conversation_root,
            &self.state.conversation_id,
            &self.state.tool_remote_mode,
        )?;
        self.restart_foreground_session()?;
        self.send_delivery_from_text(format!(
            "已关闭远程 workspace，当前 workspace 为 `{}`。",
            self.workspace_root.display()
        ))?;
        self.send_status()?;
        Ok(())
    }

    fn restart_foreground_session(&mut self) -> Result<()> {
        if let Some(client) = self.foreground.client.take() {
            let _ = client.shutdown();
        }
        self.foreground.events = None;
        let main_model = self.current_main_model()?;
        self.foreground = start_foreground_session(
            &self.agent_server_path,
            &self.conversation_root,
            &self.workspace_root,
            &self.state.session_binding.foreground_session_id,
            &main_model,
            &self.state.tool_remote_mode,
            self.state.sandbox.as_ref().unwrap_or(&self.config.sandbox),
            self.state.reasoning_effort.as_deref(),
            &self.config.models,
            &self.config.session_defaults,
        )?;
        Ok(())
    }

    fn stop_running_managed_sessions_for_config_change(&mut self, reason: &'static str) {
        for (agent_id, mut runtime) in std::mem::take(&mut self.background) {
            if let Some(client) = runtime.client.take() {
                let _ = client.shutdown();
            }
            if let Some(record) = self
                .state
                .session_binding
                .background_sessions
                .get_mut(&agent_id)
            {
                if record.status == ManagedSessionStatus::Running {
                    record.status = ManagedSessionStatus::Killed;
                    record.last_error = Some(reason.to_string());
                }
            }
        }
        for (agent_id, mut runtime) in std::mem::take(&mut self.subagents) {
            if let Some(client) = runtime.client.take() {
                let _ = client.shutdown();
            }
            if let Some(record) = self
                .state
                .session_binding
                .subagent_sessions
                .get_mut(&agent_id)
            {
                if record.status == ManagedSessionStatus::Running {
                    record.status = ManagedSessionStatus::Killed;
                    record.last_error = Some(reason.to_string());
                }
            }
        }
    }

    fn switch_main_model(&mut self, model_name: &str) -> Result<()> {
        let new_model = self
            .config
            .resolve_named_model(model_name)
            .ok_or_else(|| anyhow!("unknown model {model_name}"))?;
        if !self.config.is_available_agent_model(model_name) {
            return Err(anyhow!(
                "model {model_name} is not available for agent selection"
            ));
        }
        if !new_model.supports(ModelCapability::Chat) {
            return Err(anyhow!("model {model_name} is not chat-capable"));
        }
        let old_model_name = self.current_main_model_name();
        if self.state.session_profile.main_model.alias_name() == Some(model_name) {
            self.state.model_selection_pending = false;
            self.send_delivery_from_text(format!(
                "当前 foreground 模型已经是 `{}`。",
                old_model_name
            ))?;
            return Ok(());
        }

        if let Some(client) = self.foreground.client.take() {
            let _ = client.shutdown();
        }
        self.foreground.events = None;
        self.state.session_profile.main_model = ModelSelection::alias(model_name.to_string());
        self.state.model_selection_pending = false;
        self.restart_foreground_session()?;
        self.send_delivery_from_text(format!(
            "已切换主模型: `{}` -> `{}`",
            old_model_name, new_model.model_name
        ))?;
        Ok(())
    }
}

const TURN_PROGRESS_HINT: &str = "发送新消息可打断；/continue 可继续最近中断的回合。";

fn progress_thinking(model_key: &str, plan: Option<&TaskPlanView>) -> TurnProgress {
    TurnProgress {
        phase: TurnProgressPhase::Thinking,
        model: model_key.to_string(),
        activity: "思考中".to_string(),
        hint: Some(TURN_PROGRESS_HINT.to_string()),
        plan: turn_progress_plan(plan),
        error: None,
    }
}

fn progress_update(
    model_key: &str,
    activity: Option<&str>,
    plan: Option<&TaskPlanView>,
) -> TurnProgress {
    let Some(activity) = activity.map(str::trim).filter(|value| !value.is_empty()) else {
        return progress_thinking(model_key, plan);
    };
    TurnProgress {
        phase: TurnProgressPhase::Working,
        model: model_key.to_string(),
        activity: activity.to_string(),
        hint: Some(TURN_PROGRESS_HINT.to_string()),
        plan: turn_progress_plan(plan),
        error: None,
    }
}

fn turn_progress_plan(plan: Option<&TaskPlanView>) -> Option<TurnProgressPlan> {
    let Some(plan) = plan else {
        return None;
    };
    if plan.explanation.is_none() && plan.plan.is_empty() {
        return None;
    }
    let explanation = plan
        .explanation
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToString::to_string);
    let mut items = Vec::new();
    for item in &plan.plan {
        let step = item.step.trim();
        if step.is_empty() {
            continue;
        }
        items.push(TurnProgressPlanItem {
            step: step.to_string(),
            status: turn_progress_plan_item_status(item.status),
        });
    }
    Some(TurnProgressPlan { explanation, items })
}

fn turn_progress_plan_item_status(status: TaskPlanItemStatus) -> TurnProgressPlanItemStatus {
    match status {
        TaskPlanItemStatus::Pending => TurnProgressPlanItemStatus::Pending,
        TaskPlanItemStatus::InProgress => TurnProgressPlanItemStatus::InProgress,
        TaskPlanItemStatus::Completed => TurnProgressPlanItemStatus::Completed,
    }
}

fn parse_task_plan_view(payload: &Value) -> Result<TaskPlanView> {
    let mut plan: TaskPlanView =
        serde_json::from_value(payload.clone()).context("failed to parse update_plan payload")?;
    plan.explanation = plan
        .explanation
        .take()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty());
    for item in &mut plan.plan {
        item.step = item.step.trim().to_string();
        if item.step.is_empty() {
            return Err(anyhow!("update_plan step must not be empty"));
        }
    }
    let in_progress_count = plan
        .plan
        .iter()
        .filter(|item| matches!(item.status, TaskPlanItemStatus::InProgress))
        .count();
    if in_progress_count > 1 {
        return Err(anyhow!(
            "update_plan may include at most one in_progress step"
        ));
    }
    Ok(plan)
}

fn progress_done(model_key: &str) -> TurnProgress {
    TurnProgress {
        phase: TurnProgressPhase::Done,
        model: model_key.to_string(),
        activity: "已完成".to_string(),
        hint: None,
        plan: None,
        error: None,
    }
}

fn progress_failed(model_key: &str, error: Option<&str>) -> TurnProgress {
    let error = error
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToString::to_string);
    TurnProgress {
        phase: TurnProgressPhase::Failed,
        model: model_key.to_string(),
        activity: "本轮失败".to_string(),
        hint: None,
        plan: None,
        error,
    }
}

fn start_foreground_session(
    agent_server_path: &Path,
    session_root: &Path,
    workspace_root: &Path,
    session_id: &str,
    model_config: &ModelConfig,
    tool_remote_mode: &ToolRemoteMode,
    sandbox: &SandboxConfig,
    reasoning_effort: Option<&str>,
    models: &BTreeMap<String, ModelConfig>,
    defaults: &SessionDefaults,
) -> Result<ForegroundSessionRuntime> {
    let (client, events) = start_session_process(
        agent_server_path,
        session_root,
        workspace_root,
        session_id,
        SessionType::Foreground,
        model_config,
        tool_remote_mode,
        sandbox,
        reasoning_effort,
        models,
        defaults,
    )?;
    Ok(ForegroundSessionRuntime {
        client: Some(client),
        events: Some(events),
    })
}

fn start_managed_session_runtime(
    agent_server_path: &Path,
    session_root: &Path,
    workspace_root: &Path,
    record: ManagedSessionRecord,
    model_config: &ModelConfig,
    tool_remote_mode: &ToolRemoteMode,
    sandbox: &SandboxConfig,
    reasoning_effort: Option<&str>,
    models: &BTreeMap<String, ModelConfig>,
    defaults: &SessionDefaults,
) -> Result<ManagedSessionRuntime> {
    let (client, events) = start_session_process(
        agent_server_path,
        session_root,
        workspace_root,
        &record.session_id,
        to_session_type(record.session_type),
        model_config,
        tool_remote_mode,
        sandbox,
        reasoning_effort,
        models,
        defaults,
    )?;
    Ok(ManagedSessionRuntime {
        record,
        client: Some(client),
        events: Some(events),
    })
}

fn resolve_managed_session_model(
    config: &StellaclawConfig,
    record: &ManagedSessionRecord,
    main_model: &ModelConfig,
) -> Result<ModelConfig> {
    let Some(alias) = record.model_override.as_deref() else {
        return Ok(main_model.clone());
    };
    let model = config
        .resolve_named_model(alias)
        .ok_or_else(|| anyhow!("unknown managed session model {alias}"))?;
    if !model.supports(ModelCapability::Chat) {
        return Err(anyhow!("managed session model {alias} is not chat-capable"));
    }
    Ok(model)
}

fn start_session_process(
    agent_server_path: &Path,
    session_root: &Path,
    workspace_root: &Path,
    session_id: &str,
    session_type: SessionType,
    model_config: &ModelConfig,
    tool_remote_mode: &ToolRemoteMode,
    sandbox: &SandboxConfig,
    reasoning_effort: Option<&str>,
    models: &BTreeMap<String, ModelConfig>,
    defaults: &SessionDefaults,
) -> Result<(AgentServerClient, mpsc::Receiver<SessionEvent>)> {
    let (client, events) =
        AgentServerClient::spawn(agent_server_path, workspace_root, session_root, sandbox)
            .map_err(anyhow::Error::msg)?;
    let mut initial = SessionInitial::new(session_id.to_string(), session_type);
    initial.tool_remote_mode = tool_remote_mode.clone();
    initial.compression_threshold_tokens = defaults.compression_threshold_tokens;
    initial.compression_retain_recent_tokens = defaults.compression_retain_recent_tokens;
    let effective_model = effective_model_config(model_config, reasoning_effort);
    initial.image_tool_model = resolve_tool_model_target(
        "image_tool_model",
        defaults.image_tool_model.as_ref(),
        models,
        &effective_model,
    )?;
    initial.pdf_tool_model = resolve_tool_model_target(
        "pdf_tool_model",
        defaults.pdf_tool_model.as_ref(),
        models,
        &effective_model,
    )?;
    initial.audio_tool_model = resolve_tool_model_target(
        "audio_tool_model",
        defaults.audio_tool_model.as_ref(),
        models,
        &effective_model,
    )?;
    initial.image_generation_tool_model = resolve_tool_model_target(
        "image_generation_tool_model",
        defaults.image_generation_tool_model.as_ref(),
        models,
        &effective_model,
    )?;
    initial.search_tool_model = resolve_tool_model_target_with_capability(
        "search_tool_model",
        defaults.search_tool_model.as_ref(),
        models,
        &effective_model,
        ModelCapability::WebSearch,
    )?;
    initial.search_image_tool_model = resolve_tool_model_target_with_capability(
        "search_image_tool_model",
        defaults.search_image_tool_model.as_ref(),
        models,
        &effective_model,
        ModelCapability::WebSearch,
    )?;
    initial.search_video_tool_model = resolve_tool_model_target_with_capability(
        "search_video_tool_model",
        defaults.search_video_tool_model.as_ref(),
        models,
        &effective_model,
        ModelCapability::WebSearch,
    )?;
    initial.search_news_tool_model = resolve_tool_model_target_with_capability(
        "search_news_tool_model",
        defaults.search_news_tool_model.as_ref(),
        models,
        &effective_model,
        ModelCapability::WebSearch,
    )?;
    client
        .initialize(&effective_model, &initial)
        .map_err(anyhow::Error::msg)?;
    Ok((client, events))
}

fn resolve_tool_model_target(
    field_name: &str,
    target: Option<&ToolModelTarget>,
    models: &BTreeMap<String, ModelConfig>,
    session_model: &ModelConfig,
) -> Result<Option<ModelConfig>> {
    target
        .map(|target| {
            target
                .resolve(models, session_model)
                .map_err(|error| anyhow!("failed to resolve {field_name}: {error}"))
        })
        .transpose()
}

fn resolve_tool_model_target_with_capability(
    field_name: &str,
    target: Option<&ToolModelTarget>,
    models: &BTreeMap<String, ModelConfig>,
    session_model: &ModelConfig,
    capability: ModelCapability,
) -> Result<Option<ModelConfig>> {
    let model = resolve_tool_model_target(field_name, target, models, session_model)?;
    if let Some(model) = model.as_ref() {
        if !model.supports(capability.clone()) {
            return Err(anyhow!(
                "{field_name} model {} does not support {:?}",
                model.model_name,
                capability
            ));
        }
    }
    Ok(model)
}

fn effective_model_config(
    model_config: &ModelConfig,
    reasoning_effort: Option<&str>,
) -> ModelConfig {
    let Some(reasoning_effort) = reasoning_effort.filter(|value| !value.trim().is_empty()) else {
        return model_config.clone();
    };

    let mut effective = model_config.clone();
    let reasoning = effective
        .reasoning
        .take()
        .unwrap_or_else(|| Value::Object(Default::default()));
    let object = match reasoning {
        Value::Object(object) => object,
        _ => Default::default(),
    };
    let mut object = object;
    object.insert(
        "effort".to_string(),
        Value::String(reasoning_effort.to_string()),
    );
    effective.reasoning = Some(Value::Object(object));
    effective
}

fn to_session_type(kind: ManagedSessionType) -> SessionType {
    match kind {
        ManagedSessionType::Background => SessionType::Background,
        ManagedSessionType::Subagent => SessionType::Subagent,
    }
}

fn effective_sandbox_config<'a>(
    conversation_sandbox: Option<&'a SandboxConfig>,
    default_sandbox: &'a SandboxConfig,
) -> &'a SandboxConfig {
    conversation_sandbox.unwrap_or(default_sandbox)
}

fn sandbox_mode_label(mode: &SandboxMode) -> &'static str {
    match mode {
        SandboxMode::Subprocess => "subprocess",
        SandboxMode::Bubblewrap => "bubblewrap",
    }
}

fn bridge_result(request: &ConversationBridgeRequest, text: String) -> ToolResultItem {
    ToolResultItem {
        tool_call_id: request.tool_call_id.clone(),
        tool_name: request.tool_name.clone(),
        result: ToolResultContent {
            context: Some(ContextItem { text }),
            file: None,
        },
    }
}

fn format_script_output_section(label: &str, value: &str) -> String {
    let value = value.trim();
    if value.is_empty() {
        return String::new();
    }
    format!("\n\n{label}:\n{}", truncate_text_for_notice(value, 2_000))
}

fn truncate_text_for_notice(value: &str, max_chars: usize) -> String {
    let mut output = String::new();
    for (index, ch) in value.chars().enumerate() {
        if index >= max_chars {
            output.push_str("\n[truncated]");
            return output;
        }
        output.push(ch);
    }
    output
}

fn format_session_error(error: &str, detail: &SessionErrorDetail) -> String {
    let summary = detail.summary();
    if error == summary || error.contains(&summary) {
        error.to_string()
    } else {
        format!("{summary}\n{error}")
    }
}
