use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
    sync::{mpsc, Arc},
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
        ConversationBridgeResponse, FileItem, SessionEvent, SessionInitial, SessionRequest,
        SessionType, ToolCallItem, ToolRemoteMode, ToolResultContent, ToolResultItem,
    },
};

use crate::{
    channels::types::{
        OutgoingAttachment, OutgoingAttachmentKind, OutgoingDelivery, OutgoingDispatch,
        OutgoingOption, OutgoingOptions, OutgoingProcessing, OutgoingProgressFeedback,
        ProcessingState, ProgressFeedbackFinalState,
    },
    config::{
        SandboxConfig, SandboxMode, SessionDefaults, SessionProfile, StellaclawConfig,
        ToolModelTarget,
    },
    cron::{
        cron_schedule_from_required_tool_args, optional_cron_schedule_from_tool_args,
        optional_string_arg, parse_enabled_flag, string_arg_required, timezone_or_default,
        CreateCronTaskRequest, CronManager, CronTaskRecord, UpdateCronTaskRequest,
    },
    logger::StellaclawLogger,
    sandbox::bubblewrap_support_error,
    session_client::AgentServerClient,
    workspace::{ensure_workspace_for_remote_mode, ensure_workspace_seed, unmount_sshfs_workspace},
};

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
    ShowStatus,
    ShowModel,
    SwitchModel { model_name: String },
    ShowRemote,
    SetRemote { host: String, path: String },
    DisableRemote,
    InvalidRemote { reason: String },
    ShowSandbox,
    SetSandbox { mode: Option<SandboxMode> },
    InvalidSandbox { reason: String },
}

#[derive(Debug)]
pub enum ConversationCommand {
    Incoming(IncomingConversationMessage),
    RunCronTask { task: CronTaskRecord },
}

const TYPING_KEEPALIVE_INTERVAL: Duration = Duration::from_secs(4);

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConversationState {
    pub version: u32,
    pub conversation_id: String,
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
    pub model_override: Option<ModelConfig>,
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
        StellaclawLogger::open_under(&conversation_root, "conversation.log")
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
        let mut changed = false;
        while runtime.pump_session_events()? {
            changed = true;
        }
        runtime.pump_processing_keepalive()?;
        if changed {
            runtime.persist_state()?;
        }

        match rx.recv_timeout(Duration::from_millis(100)) {
            Ok(ConversationCommand::Incoming(message)) => {
                if runtime.handle_incoming(message)? {
                    runtime.persist_state()?;
                }
            }
            Ok(ConversationCommand::RunCronTask { task }) => {
                if runtime.run_cron_task(task)? {
                    runtime.persist_state()?;
                }
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
        return serde_json::from_str(&raw)
            .with_context(|| format!("failed to parse {}", path.display()));
    }
    Ok(ConversationState {
        version: 1,
        conversation_id: conversation_id.to_string(),
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
        let foreground = start_foreground_session(
            &agent_server_path,
            &workspace_root,
            &state.session_binding.foreground_session_id,
            &state.session_profile.main_model,
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
            let model = record
                .model_override
                .as_ref()
                .unwrap_or(&state.session_profile.main_model);
            background.insert(
                agent_id.clone(),
                start_managed_session_runtime(
                    &agent_server_path,
                    &workspace_root,
                    record.clone(),
                    model,
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
            let model = record
                .model_override
                .as_ref()
                .unwrap_or(&state.session_profile.main_model);
            subagents.insert(
                agent_id.clone(),
                start_managed_session_runtime(
                    &agent_server_path,
                    &workspace_root,
                    record.clone(),
                    model,
                    &state.tool_remote_mode,
                    state.sandbox.as_ref().unwrap_or(&config.sandbox),
                    state.reasoning_effort.as_deref(),
                    &config.models,
                    &config.session_defaults,
                )?,
            );
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
        })
    }

    fn persist_state(&self) -> Result<()> {
        let path = self.conversation_root.join("conversation.json");
        let raw = serde_json::to_string_pretty(&self.state)
            .context("failed to serialize conversation state")?;
        fs::write(&path, raw).with_context(|| format!("failed to write {}", path.display()))
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
                            self.send_delivery_from_text(format!("模型切换失败: {error}"))?;
                            return Ok(false);
                        }
                    }
                }
                Some(
                    ConversationControl::ShowStatus
                    | ConversationControl::ShowRemote
                    | ConversationControl::SetRemote { .. }
                    | ConversationControl::DisableRemote
                    | ConversationControl::InvalidRemote { .. }
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
                        self.send_delivery_from_text(format!("模型切换失败: {error}"))?;
                        return Ok(false);
                    }
                }
            }
            Some(ConversationControl::ShowRemote) => {
                self.send_remote_status()?;
                return Ok(false);
            }
            Some(ConversationControl::SetRemote { host, path }) => {
                match self.set_remote_mode(host, path) {
                    Ok(()) => return Ok(true),
                    Err(error) => {
                        self.send_delivery_from_text(format!("远程 workspace 切换失败: {error}"))?;
                        return Ok(false);
                    }
                }
            }
            Some(ConversationControl::DisableRemote) => match self.disable_remote_mode() {
                Ok(()) => return Ok(true),
                Err(error) => {
                    self.send_delivery_from_text(format!("关闭远程 workspace 失败: {error}"))?;
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
                    self.send_delivery_from_text(format!("沙盒模式切换失败: {error}"))?;
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
            SessionEvent::TurnStarted { turn_id } => {
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
                can_continue,
            } => self.on_turn_failed(agent_id, session_type, error, can_continue),
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
            SessionEvent::ControlRejected { reason, payload } => {
                self.logger.warn(
                    "control_rejected",
                    json!({"reason": reason, "payload": payload, "agent_id": agent_id}),
                );
                Ok(false)
            }
            SessionEvent::RuntimeCrashed { error } => {
                self.host_logger.warn(
                    "session_runtime_crashed",
                    json!({
                        "conversation_id": self.state.conversation_id,
                        "session_type": format!("{session_type:?}"),
                        "agent_id": agent_id,
                        "error": error,
                    }),
                );
                self.send_delivery_from_text(
                    "Session runtime crashed. 发送 /continue 可尝试继续。".to_string(),
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
        match session_type {
            SessionType::Foreground => {
                self.finish_foreground_progress(ProgressFeedbackFinalState::Done, None)?;
                self.send_delivery_from_text(render_chat_message(&message))?;
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
        can_continue: bool,
    ) -> Result<bool> {
        match session_type {
            SessionType::Foreground => {
                self.finish_foreground_progress(ProgressFeedbackFinalState::Failed, Some(&error))?;
                let suffix = if can_continue {
                    "\n发送 /continue 继续，或 /cancel 取消当前回合。"
                } else {
                    ""
                };
                self.send_delivery_from_text(format!("本轮失败: {error}{suffix}"))?;
                Ok(false)
            }
            SessionType::Background => {
                if let Some(agent_id) = agent_id {
                    if let Some(runtime) = self.background.get_mut(&agent_id) {
                        runtime.record.status = ManagedSessionStatus::Failed;
                        runtime.record.last_error = Some(error.clone());
                    }
                    if let Some(record) = self
                        .state
                        .session_binding
                        .background_sessions
                        .get_mut(&agent_id)
                    {
                        record.status = ManagedSessionStatus::Failed;
                        record.last_error = Some(error.clone());
                    }
                }
                self.send_delivery_from_text(format!("后台任务失败: {error}"))?;
                Ok(true)
            }
            SessionType::Subagent => {
                if let Some(agent_id) = agent_id {
                    if let Some(runtime) = self.subagents.get_mut(&agent_id) {
                        runtime.record.status = ManagedSessionStatus::Failed;
                        runtime.record.last_error = Some(error.clone());
                    }
                    if let Some(record) = self
                        .state
                        .session_binding
                        .subagent_sessions
                        .get_mut(&agent_id)
                    {
                        record.status = ManagedSessionStatus::Failed;
                        record.last_error = Some(error);
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
        let tool_result = match self.handle_bridge_action(agent_id.clone(), session_type, &request)
        {
            Ok(result) => result,
            Err(error) => {
                bridge_result(&request, json!({"error": format!("{error:#}")}).to_string())
            }
        };
        let response = ConversationBridgeResponse {
            request_id: request.request_id.clone(),
            tool_call_id: request.tool_call_id.clone(),
            tool_name: request.tool_name.clone(),
            result: tool_result,
        };
        self.client_for_session(agent_id.as_deref(), session_type)?
            .send_session_request(&SessionRequest::ResolveHostCoordination { response })
            .map_err(anyhow::Error::msg)
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
            "update_plan" => Ok(bridge_result(request, json!({"updated": true}).to_string())),
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
                    task: string_arg_required(object, "task")?,
                    model: optional_string_arg(object, "model")?,
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
                let _clear_checker = object
                    .get("clear_checker")
                    .and_then(Value::as_bool)
                    .unwrap_or(false);
                let model = optional_string_arg(object, "model")?.map(Some);
                let task = self.cron_manager.update_task(
                    &self.state.conversation_id,
                    &id,
                    UpdateCronTaskRequest {
                        name: optional_string_arg(object, "name")?,
                        description: optional_string_arg(object, "description")?,
                        schedule,
                        timezone,
                        task: optional_string_arg(object, "task")?,
                        model,
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
        let staged_skill_path = self.workspace_root.join(".skill").join(skill_name);

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
                let synced = sync_skill_to_conversation_workspaces(
                    &self.workdir,
                    skill_name,
                    Some(&staged_skill_path),
                )?;
                Ok(bridge_result(
                    request,
                    json!({"updated": true, "skill_name": skill_name, "synced_workspaces": synced})
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

    fn resolve_model_override(&self, payload: &Value) -> Result<Option<ModelConfig>> {
        let Some(name) = payload.get("model").and_then(Value::as_str) else {
            return Ok(None);
        };
        self.config
            .resolve_named_model(name)
            .ok_or_else(|| anyhow!("unknown named model {name}"))
            .map(Some)
    }

    fn start_managed_session(
        &mut self,
        kind: ManagedSessionType,
        task: String,
        model_override: Option<ModelConfig>,
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
        let model = model_override
            .as_ref()
            .unwrap_or(&self.state.session_profile.main_model);
        let runtime = start_managed_session_runtime(
            &self.agent_server_path,
            &self.workspace_root,
            record.clone(),
            model,
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
        let (clean_text, attachments) = extract_attachment_references(&text, &self.workspace_root)?;
        if clean_text.trim().is_empty() && attachments.is_empty() {
            return Ok(());
        }
        self.send_delivery(clean_text, attachments, None)
    }

    fn send_delivery(
        &self,
        text: String,
        attachments: Vec<OutgoingAttachment>,
        options: Option<OutgoingOptions>,
    ) -> Result<()> {
        self.outgoing_tx
            .send(OutgoingDispatch::Delivery(OutgoingDelivery {
                channel_id: self.state.channel_id.clone(),
                platform_chat_id: self.state.platform_chat_id.clone(),
                text,
                attachments,
                options,
            }))
            .map_err(|_| anyhow!("outgoing delivery channel closed"))
    }

    fn send_processing_state(&self, state: ProcessingState) -> Result<()> {
        self.outgoing_tx
            .send(OutgoingDispatch::Processing(OutgoingProcessing {
                channel_id: self.state.channel_id.clone(),
                platform_chat_id: self.state.platform_chat_id.clone(),
                state,
            }))
            .map_err(|_| anyhow!("outgoing processing channel closed"))
    }

    fn send_progress_feedback(
        &self,
        turn_id: String,
        text: String,
        final_state: Option<ProgressFeedbackFinalState>,
        important: bool,
    ) -> Result<()> {
        self.outgoing_tx
            .send(OutgoingDispatch::ProgressFeedback(
                OutgoingProgressFeedback {
                    channel_id: self.state.channel_id.clone(),
                    platform_chat_id: self.state.platform_chat_id.clone(),
                    turn_id,
                    text,
                    final_state,
                    important,
                },
            ))
            .map_err(|_| anyhow!("outgoing progress channel closed"))
    }

    fn start_foreground_progress(&mut self, turn_id: String) -> Result<()> {
        let now = Instant::now();
        let progress_turn_id = self
            .foreground_progress
            .as_ref()
            .map(|progress| progress.turn_id.clone())
            .unwrap_or_else(|| turn_id.clone());
        self.foreground_progress = Some(ActiveForegroundProgress {
            turn_id: progress_turn_id.clone(),
            next_typing_at: now + TYPING_KEEPALIVE_INTERVAL,
        });
        self.send_processing_state(ProcessingState::Typing)?;
        self.send_progress_feedback(
            progress_turn_id,
            progress_text_thinking(&self.state.session_profile.main_model.model_name),
            None,
            true,
        )
    }

    fn update_foreground_progress(&self, message: &str) -> Result<()> {
        let Some(progress) = &self.foreground_progress else {
            return Ok(());
        };
        self.send_progress_feedback(
            progress.turn_id.clone(),
            progress_text_update(&self.state.session_profile.main_model.model_name, message),
            None,
            false,
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
        self.send_processing_state(ProcessingState::Idle)?;
        let text = match final_state {
            ProgressFeedbackFinalState::Done => {
                progress_text_done(&self.state.session_profile.main_model.model_name)
            }
            ProgressFeedbackFinalState::Failed => {
                progress_text_failed(&self.state.session_profile.main_model.model_name, error)
            }
        };
        self.send_progress_feedback(progress.turn_id, text, Some(final_state), true)
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
        self.logger.info(
            "cron_task_starting_background_agent",
            json!({
                "task_id": task.id,
                "conversation_id": self.state.conversation_id,
                "model": task.model,
            }),
        );
        let model_override = match task.model.as_deref() {
            Some(name) => Some(
                self.config
                    .resolve_named_model(name)
                    .ok_or_else(|| anyhow!("unknown cron model {name}"))?,
            ),
            None => None,
        };
        let _ =
            self.start_managed_session(ManagedSessionType::Background, task.task, model_override)?;
        Ok(true)
    }

    fn render_model_selection(&self) -> String {
        let current_name = &self.state.session_profile.main_model.model_name;
        let mut lines = if self.state.model_selection_pending {
            vec![format!("请选择 foreground 模型。当前预选: {current_name}")]
        } else {
            vec![format!("当前模型: {current_name}")]
        };
        if !self
            .config
            .models
            .values()
            .any(|model| model.supports(ModelCapability::Chat))
        {
            return lines.join("\n");
        }

        lines.push("可切换模型:".to_string());
        for (name, model) in &self.config.models {
            if !model.supports(ModelCapability::Chat) {
                continue;
            }
            let marker = if model.model_name == *current_name {
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
            .models
            .iter()
            .filter(|(_, model)| model.supports(ModelCapability::Chat))
            .map(|(name, model)| {
                let marker = if model.model_name == self.state.session_profile.main_model.model_name
                {
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
        let sandbox = effective_sandbox_config(self.state.sandbox.as_ref(), &self.config.sandbox);
        let sandbox_source = if self.state.sandbox.is_some() {
            "conversation"
        } else {
            "default"
        };
        let remote = match &self.state.tool_remote_mode {
            ToolRemoteMode::Selectable => "selectable".to_string(),
            ToolRemoteMode::FixedSsh { host, cwd } => {
                format!("fixed ssh `{host}` `{}`", cwd.as_deref().unwrap_or(""))
            }
        };
        let running_background = self
            .state
            .session_binding
            .background_sessions
            .values()
            .filter(|record| record.status == ManagedSessionStatus::Running)
            .count();
        let running_subagents = self
            .state
            .session_binding
            .subagent_sessions
            .values()
            .filter(|record| record.status == ManagedSessionStatus::Running)
            .count();
        self.send_delivery_from_text(format!(
            "当前状态\nconversation: `{}`\nmodel: `{}`\nreasoning: `{}`\nsandbox: `{}` ({sandbox_source})\nremote: {remote}\nworkspace: `{}`\nbackground: {running_background} running / {} total\nsubagents: {running_subagents} running / {} total",
            self.state.conversation_id,
            self.state.session_profile.main_model.model_name,
            self.state.reasoning_effort.as_deref().unwrap_or("model default"),
            sandbox_mode_label(&sandbox.mode),
            self.workspace_root.display(),
            self.state.session_binding.background_sessions.len(),
            self.state.session_binding.subagent_sessions.len(),
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
        self.send_delivery_from_text(format!(
            "当前 sandbox: `{}` ({source})\nbubblewrap_binary: `{}`{support}\n用法: `/sandbox bubblewrap`，`/sandbox subprocess`，`/sandbox default`。",
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
        Ok(())
    }

    fn restart_foreground_session(&mut self) -> Result<()> {
        if let Some(client) = self.foreground.client.take() {
            let _ = client.shutdown();
        }
        self.foreground.events = None;
        self.foreground = start_foreground_session(
            &self.agent_server_path,
            &self.workspace_root,
            &self.state.session_binding.foreground_session_id,
            &self.state.session_profile.main_model,
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
        if !new_model.supports(ModelCapability::Chat) {
            return Err(anyhow!("model {model_name} is not chat-capable"));
        }
        let old_model_name = self.state.session_profile.main_model.model_name.clone();
        if self.state.session_profile.main_model == new_model {
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
        self.state.session_profile.main_model = new_model;
        self.state.model_selection_pending = false;
        self.restart_foreground_session()?;
        self.send_delivery_from_text(format!(
            "已切换主模型: `{}` -> `{}`",
            old_model_name, self.state.session_profile.main_model.model_name
        ))?;
        Ok(())
    }
}

fn progress_text_thinking(model_key: &str) -> String {
    format!(
        "⚙️ 正在执行\n🤖 模型：{}\n🧠 状态：思考中...\n\n💡 发送新消息可打断；/continue 可继续最近中断的回合。",
        model_key
    )
}

fn progress_text_update(model_key: &str, activity: &str) -> String {
    let activity = activity.trim();
    if activity.is_empty() {
        return progress_text_thinking(model_key);
    }
    format!(
        "⚙️ 正在执行\n🤖 模型：{}\n📌 阶段：{}\n\n💡 发送新消息可打断；/continue 可继续最近中断的回合。",
        model_key, activity
    )
}

fn progress_text_done(model_key: &str) -> String {
    format!("✅ 已完成\n🤖 模型：{model_key}")
}

fn progress_text_failed(model_key: &str, error: Option<&str>) -> String {
    let Some(error) = error.map(str::trim).filter(|value| !value.is_empty()) else {
        return format!("❌ 本轮失败\n🤖 模型：{model_key}");
    };
    format!("❌ 本轮失败\n🤖 模型：{model_key}\n📌 {error}")
}

fn start_foreground_session(
    agent_server_path: &Path,
    conversation_root: &Path,
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
        conversation_root,
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
    conversation_root: &Path,
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
        conversation_root,
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

fn start_session_process(
    agent_server_path: &Path,
    conversation_root: &Path,
    session_id: &str,
    session_type: SessionType,
    model_config: &ModelConfig,
    tool_remote_mode: &ToolRemoteMode,
    sandbox: &SandboxConfig,
    reasoning_effort: Option<&str>,
    models: &BTreeMap<String, ModelConfig>,
    defaults: &SessionDefaults,
) -> Result<(AgentServerClient, mpsc::Receiver<SessionEvent>)> {
    let (client, events) = AgentServerClient::spawn(agent_server_path, conversation_root, sandbox)
        .map_err(anyhow::Error::msg)?;
    let mut initial = SessionInitial::new(session_id.to_string(), session_type);
    initial.tool_remote_mode = tool_remote_mode.clone();
    initial.compression_threshold_tokens = defaults.compression_threshold_tokens;
    initial.compression_retain_recent_tokens = defaults.compression_retain_recent_tokens;
    initial.image_tool_model = resolve_tool_model_target(
        "image_tool_model",
        defaults.image_tool_model.as_ref(),
        models,
        model_config,
    )?;
    initial.pdf_tool_model = resolve_tool_model_target(
        "pdf_tool_model",
        defaults.pdf_tool_model.as_ref(),
        models,
        model_config,
    )?;
    initial.audio_tool_model = resolve_tool_model_target(
        "audio_tool_model",
        defaults.audio_tool_model.as_ref(),
        models,
        model_config,
    )?;
    initial.image_generation_tool_model = resolve_tool_model_target(
        "image_generation_tool_model",
        defaults.image_generation_tool_model.as_ref(),
        models,
        model_config,
    )?;
    initial.search_tool_model = resolve_tool_model_target(
        "search_tool_model",
        defaults.search_tool_model.as_ref(),
        models,
        model_config,
    )?;
    client
        .initialize(
            &effective_model_config(model_config, reasoning_effort),
            &initial,
        )
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

#[derive(Debug, Clone, Copy)]
enum SkillPersistMode {
    Create,
    Update,
    Delete,
}

fn validate_skill_name(skill_name: &str) -> Result<()> {
    let name = skill_name.trim();
    if name.is_empty() {
        return Err(anyhow!("skill_name must not be empty"));
    }
    if name != skill_name {
        return Err(anyhow!(
            "skill_name must not contain leading or trailing whitespace"
        ));
    }
    if !name
        .chars()
        .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '_' | '-'))
    {
        return Err(anyhow!(
            "skill_name may only contain ASCII letters, digits, '_' and '-'"
        ));
    }
    Ok(())
}

fn validate_skill_directory(skill_path: &Path, skill_name: &str) -> Result<()> {
    if !skill_path.is_dir() {
        return Err(anyhow!(
            "staged skill directory {} does not exist",
            skill_path.display()
        ));
    }
    let entry_path = skill_path.join("SKILL.md");
    let content = fs::read_to_string(&entry_path)
        .with_context(|| format!("failed to read {}", entry_path.display()))?;
    let frontmatter = extract_yaml_frontmatter(&content)
        .ok_or_else(|| anyhow!("{} must start with YAML frontmatter", entry_path.display()))?;
    let name = frontmatter_scalar(frontmatter, "name")
        .ok_or_else(|| anyhow!("{} frontmatter must contain name", entry_path.display()))?;
    if name != skill_name {
        return Err(anyhow!(
            "{} frontmatter name `{}` does not match folder `{}`",
            entry_path.display(),
            name,
            skill_name
        ));
    }
    let description = frontmatter_scalar(frontmatter, "description").ok_or_else(|| {
        anyhow!(
            "{} frontmatter must contain description",
            entry_path.display()
        )
    })?;
    if description.trim().is_empty() {
        return Err(anyhow!(
            "{} frontmatter description must not be empty",
            entry_path.display()
        ));
    }
    Ok(())
}

fn extract_yaml_frontmatter(content: &str) -> Option<&str> {
    let mut lines = content.lines();
    if lines.next()? != "---" {
        return None;
    }
    let body_start = 4;
    let end = content[body_start..].find("\n---")?;
    Some(&content[body_start..body_start + end])
}

fn frontmatter_scalar(frontmatter: &str, key: &str) -> Option<String> {
    let prefix = format!("{key}:");
    let lines: Vec<&str> = frontmatter.lines().collect();
    let mut index = 0usize;
    while index < lines.len() {
        let line = lines[index].trim();
        let Some(value) = line.strip_prefix(&prefix) else {
            index += 1;
            continue;
        };
        let value = value.trim();
        if value.is_empty() {
            return None;
        }
        if value == "|" || value == ">" || value.starts_with("|-") || value.starts_with(">-") {
            let folded = value.starts_with('>');
            let mut block = Vec::new();
            for next in lines.iter().skip(index + 1) {
                if !next.trim().is_empty() && !next.starts_with(char::is_whitespace) {
                    break;
                }
                let trimmed = next.trim();
                if !trimmed.is_empty() {
                    block.push(trimmed);
                }
            }
            let joined = if folded {
                block.join(" ")
            } else {
                block.join("\n")
            };
            let joined = joined.trim().to_string();
            return (!joined.is_empty()).then_some(joined);
        }
        return Some(unquote_yaml_scalar(value));
    }
    None
}

fn unquote_yaml_scalar(value: &str) -> String {
    let trimmed = value.trim();
    if trimmed.len() >= 2 {
        let bytes = trimmed.as_bytes();
        if (bytes[0] == b'"' && bytes[trimmed.len() - 1] == b'"')
            || (bytes[0] == b'\'' && bytes[trimmed.len() - 1] == b'\'')
        {
            return trimmed[1..trimmed.len() - 1].trim().to_string();
        }
    }
    trimmed.to_string()
}

fn copy_skill_atomically(source: &Path, destination: &Path) -> Result<()> {
    let parent = destination
        .parent()
        .ok_or_else(|| anyhow!("{} has no parent", destination.display()))?;
    fs::create_dir_all(parent).with_context(|| format!("failed to create {}", parent.display()))?;
    let tmp = destination.with_extension("tmp-skill-copy");
    if tmp.exists() {
        fs::remove_dir_all(&tmp).with_context(|| format!("failed to remove {}", tmp.display()))?;
    }
    copy_directory_recursive_local(source, &tmp)?;
    if destination.exists() {
        fs::remove_dir_all(destination)
            .with_context(|| format!("failed to remove {}", destination.display()))?;
    }
    fs::rename(&tmp, destination).with_context(|| {
        format!(
            "failed to rename {} to {}",
            tmp.display(),
            destination.display()
        )
    })
}

fn sync_skill_to_conversation_workspaces(
    workdir: &Path,
    skill_name: &str,
    source: Option<&Path>,
) -> Result<usize> {
    let conversations_root = workdir.join("conversations");
    if !conversations_root.is_dir() {
        return Ok(0);
    }
    let mut synced = 0usize;
    for entry in fs::read_dir(&conversations_root)
        .with_context(|| format!("failed to read {}", conversations_root.display()))?
    {
        let entry = entry
            .with_context(|| format!("failed to enumerate {}", conversations_root.display()))?;
        if !entry
            .file_type()
            .with_context(|| format!("failed to inspect {}", entry.path().display()))?
            .is_dir()
        {
            continue;
        }
        let skill_root = entry.path().join(".skill");
        if !skill_root.is_dir() {
            continue;
        }
        let destination = skill_root.join(skill_name);
        match source {
            Some(source) => {
                copy_skill_atomically(source, &destination)?;
                synced += 1;
            }
            None => {
                if destination.exists() {
                    fs::remove_dir_all(&destination)
                        .with_context(|| format!("failed to remove {}", destination.display()))?;
                    synced += 1;
                }
            }
        }
    }
    Ok(synced)
}

fn copy_directory_recursive_local(source: &Path, destination: &Path) -> Result<()> {
    fs::create_dir_all(destination)
        .with_context(|| format!("failed to create {}", destination.display()))?;
    for entry in
        fs::read_dir(source).with_context(|| format!("failed to read {}", source.display()))?
    {
        let entry = entry.with_context(|| format!("failed to enumerate {}", source.display()))?;
        let source_path = entry.path();
        let destination_path = destination.join(entry.file_name());
        if entry
            .file_type()
            .with_context(|| format!("failed to inspect {}", source_path.display()))?
            .is_dir()
        {
            copy_directory_recursive_local(&source_path, &destination_path)?;
        } else {
            fs::copy(&source_path, &destination_path).with_context(|| {
                format!(
                    "failed to copy {} to {}",
                    source_path.display(),
                    destination_path.display()
                )
            })?;
        }
    }
    Ok(())
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

fn extract_attachment_references(
    text: &str,
    workspace_root: &Path,
) -> Result<(String, Vec<OutgoingAttachment>)> {
    const START: &str = "<attachment>";
    const END: &str = "</attachment>";

    let mut clean = String::with_capacity(text.len());
    let mut attachments = Vec::new();
    let mut cursor = 0usize;

    while let Some(start_rel) = text[cursor..].find(START) {
        let start = cursor + start_rel;
        clean.push_str(&text[cursor..start]);
        let path_start = start + START.len();
        let Some(end_rel) = text[path_start..].find(END) else {
            clean.push_str(&text[start..]);
            return Ok((clean.trim().to_string(), attachments));
        };
        let path_end = path_start + end_rel;
        let path_text = text[path_start..path_end].trim();
        if !path_text.is_empty() {
            attachments.push(resolve_outgoing_attachment(workspace_root, path_text)?);
        }
        cursor = path_end + END.len();
    }

    clean.push_str(&text[cursor..]);
    Ok((clean.trim().to_string(), attachments))
}

fn resolve_outgoing_attachment(
    workspace_root: &Path,
    path_text: &str,
) -> Result<OutgoingAttachment> {
    let joined = workspace_root.join(path_text);
    let canonical = joined
        .canonicalize()
        .with_context(|| format!("attachment path does not exist: {}", joined.display()))?;
    let root = workspace_root
        .canonicalize()
        .with_context(|| format!("failed to canonicalize {}", workspace_root.display()))?;
    if !canonical.starts_with(&root) {
        return Err(anyhow!(
            "attachment path escapes conversation root: {}",
            canonical.display()
        ));
    }
    if !canonical.is_file() {
        return Err(anyhow!(
            "attachment path is not a regular file: {}",
            canonical.display()
        ));
    }
    Ok(OutgoingAttachment {
        kind: infer_outgoing_attachment_kind(&canonical),
        path: canonical,
    })
}

fn infer_outgoing_attachment_kind(path: &Path) -> OutgoingAttachmentKind {
    match path
        .extension()
        .and_then(|value| value.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase()
        .as_str()
    {
        "png" | "jpg" | "jpeg" | "webp" => OutgoingAttachmentKind::Image,
        "gif" => OutgoingAttachmentKind::Animation,
        "mp3" | "wav" => OutgoingAttachmentKind::Audio,
        "ogg" => OutgoingAttachmentKind::Voice,
        "mp4" | "mov" | "mkv" => OutgoingAttachmentKind::Video,
        _ => OutgoingAttachmentKind::Document,
    }
}

pub fn render_chat_message(message: &ChatMessage) -> String {
    let mut parts = Vec::new();
    for item in &message.data {
        match item {
            ChatMessageItem::Context(context) => parts.push(context.text.clone()),
            ChatMessageItem::File(file) => parts.push(render_file_item(file)),
            ChatMessageItem::Reasoning(_) => {}
            ChatMessageItem::ToolCall(ToolCallItem {
                tool_name,
                arguments,
                ..
            }) => parts.push(format!("[tool_call {tool_name}] {}", arguments.text)),
            ChatMessageItem::ToolResult(tool_result) => {
                let mut line = format!("[tool_result {}]", tool_result.tool_name);
                if let Some(context) = &tool_result.result.context {
                    line.push('\n');
                    line.push_str(&context.text);
                }
                if let Some(file) = &tool_result.result.file {
                    line.push('\n');
                    line.push_str(&render_file_item(file));
                }
                parts.push(line);
            }
        }
    }
    if parts.is_empty() {
        String::new()
    } else {
        parts.join("\n\n")
    }
}

fn render_file_item(file: &FileItem) -> String {
    match &file.name {
        Some(name) => format!("[file] {name} ({})", file.uri),
        None => format!("[file] {}", file.uri),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frontmatter_scalar_finds_description_after_name() {
        let frontmatter = "name: web-report-deploy\ndescription: Deploy reports\n";

        assert_eq!(
            frontmatter_scalar(frontmatter, "description").as_deref(),
            Some("Deploy reports")
        );
    }

    #[test]
    fn frontmatter_scalar_supports_quoted_and_folded_values() {
        let quoted = "name: demo\ndescription: \"Deploy reports: safely\"\n";
        assert_eq!(
            frontmatter_scalar(quoted, "description").as_deref(),
            Some("Deploy reports: safely")
        );

        let folded = "name: demo\ndescription: >\n  Deploy reports\n  safely\nnext: value\n";
        assert_eq!(
            frontmatter_scalar(folded, "description").as_deref(),
            Some("Deploy reports safely")
        );
    }

    #[test]
    fn render_chat_message_hides_reasoning_items() {
        let message = ChatMessage::new(
            ChatRole::Assistant,
            vec![
                ChatMessageItem::Reasoning(stellaclaw_core::session_actor::ReasoningItem::codex(
                    None,
                    Some("opaque".to_string()),
                    None,
                )),
                ChatMessageItem::Context(ContextItem {
                    text: "visible answer".to_string(),
                }),
            ],
        );

        assert_eq!(render_chat_message(&message), "visible answer");
    }
}
