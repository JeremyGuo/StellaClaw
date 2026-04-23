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
    model_config::ModelConfig,
    session_actor::{
        ChatMessage, ChatMessageItem, ChatRole, ContextItem, ConversationBridgeRequest,
        ConversationBridgeResponse, FileItem, SessionEvent, SessionInitial, SessionRequest,
        SessionType, ToolCallItem, ToolRemoteMode, ToolResultContent, ToolResultItem,
    },
};

use crate::{
    channels::types::{
        OutgoingAttachment, OutgoingAttachmentKind, OutgoingDelivery, OutgoingOption,
        OutgoingOptions,
    },
    config::{SandboxConfig, SessionDefaults, SessionProfile, StellaclawConfig},
    cron::{
        cron_schedule_from_required_tool_args, optional_cron_schedule_from_tool_args,
        optional_string_arg, parse_enabled_flag, string_arg_required, timezone_or_default,
        CreateCronTaskRequest, CronManager, CronTaskRecord, UpdateCronTaskRequest,
    },
    logger::StellaclawLogger,
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
    ShowModel,
    SwitchModel { model_name: String },
    ShowRemote,
    SetRemote { host: String, path: String },
    DisableRemote,
    InvalidRemote { reason: String },
}

#[derive(Debug)]
pub enum ConversationCommand {
    Incoming(IncomingConversationMessage),
    RunCronTask { task: CronTaskRecord },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConversationState {
    pub version: u32,
    pub conversation_id: String,
    pub channel_id: String,
    pub platform_chat_id: String,
    pub session_profile: SessionProfile,
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
    outgoing_tx: Sender<OutgoingDelivery>,
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
    outgoing_tx: Sender<OutgoingDelivery>,
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
    default_profile: &SessionProfile,
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
        session_profile: default_profile.clone(),
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
    outgoing_tx: Sender<OutgoingDelivery>,
    logger: Arc<StellaclawLogger>,
    host_logger: Arc<StellaclawLogger>,
    foreground: ForegroundSessionRuntime,
    background: BTreeMap<String, ManagedSessionRuntime>,
    subagents: BTreeMap<String, ManagedSessionRuntime>,
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

impl ConversationRuntime {
    fn new(
        workdir: PathBuf,
        conversation_root: PathBuf,
        state: ConversationState,
        config: Arc<StellaclawConfig>,
        cron_manager: Arc<CronManager>,
        agent_server_path: PathBuf,
        outgoing_tx: Sender<OutgoingDelivery>,
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
        })
    }

    fn persist_state(&self) -> Result<()> {
        let path = self.conversation_root.join("conversation.json");
        let raw = serde_json::to_string_pretty(&self.state)
            .context("failed to serialize conversation state")?;
        fs::write(&path, raw).with_context(|| format!("failed to write {}", path.display()))
    }

    fn handle_incoming(&mut self, message: IncomingConversationMessage) -> Result<bool> {
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
                let model_override = self.resolve_model_override(&request.payload)?;
                let started = self.start_managed_session(
                    ManagedSessionType::Subagent,
                    description.to_string(),
                    model_override,
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
        self.send_delivery(clean_text, attachments, None)
    }

    fn send_delivery(
        &self,
        text: String,
        attachments: Vec<OutgoingAttachment>,
        options: Option<OutgoingOptions>,
    ) -> Result<()> {
        self.outgoing_tx
            .send(OutgoingDelivery {
                channel_id: self.state.channel_id.clone(),
                platform_chat_id: self.state.platform_chat_id.clone(),
                text,
                attachments,
                options,
            })
            .map_err(|_| anyhow!("outgoing delivery channel closed"))
    }

    fn shutdown(&mut self) {
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
        let mut lines = vec![format!("当前模型: {current_name}")];
        if self.config.named_models.is_empty() {
            return lines.join("\n");
        }

        lines.push("可切换模型:".to_string());
        for (name, model) in &self.config.named_models {
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
            .named_models
            .iter()
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
            self.stop_running_managed_sessions_for_workspace_change();
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
        self.stop_running_managed_sessions_for_workspace_change();
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

    fn disable_remote_mode(&mut self) -> Result<()> {
        if matches!(self.state.tool_remote_mode, ToolRemoteMode::Selectable) {
            self.send_remote_status()?;
            return Ok(());
        }
        self.stop_running_managed_sessions_for_workspace_change();
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
            &self.config.session_defaults,
        )?;
        Ok(())
    }

    fn stop_running_managed_sessions_for_workspace_change(&mut self) {
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
                    record.last_error =
                        Some("stopped because conversation remote workspace changed".to_string());
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
                    record.last_error =
                        Some("stopped because conversation remote workspace changed".to_string());
                }
            }
        }
    }

    fn switch_main_model(&mut self, model_name: &str) -> Result<()> {
        let new_model = self
            .config
            .resolve_named_model(model_name)
            .ok_or_else(|| anyhow!("unknown model {model_name}"))?;
        let old_model_name = self.state.session_profile.main_model.model_name.clone();
        if self.state.session_profile.main_model == new_model {
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
        self.restart_foreground_session()?;
        self.send_delivery_from_text(format!(
            "已切换主模型: `{}` -> `{}`",
            old_model_name, self.state.session_profile.main_model.model_name
        ))?;
        Ok(())
    }
}

fn start_foreground_session(
    agent_server_path: &Path,
    conversation_root: &Path,
    session_id: &str,
    model_config: &ModelConfig,
    tool_remote_mode: &ToolRemoteMode,
    sandbox: &SandboxConfig,
    reasoning_effort: Option<&str>,
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
    defaults: &SessionDefaults,
) -> Result<(AgentServerClient, mpsc::Receiver<SessionEvent>)> {
    let (client, events) = AgentServerClient::spawn(agent_server_path, conversation_root, sandbox)
        .map_err(anyhow::Error::msg)?;
    let mut initial = SessionInitial::new(session_id.to_string(), session_type);
    initial.tool_remote_mode = tool_remote_mode.clone();
    initial.compression_threshold_tokens = defaults.compression_threshold_tokens;
    initial.compression_retain_recent_tokens = defaults.compression_retain_recent_tokens;
    initial.image_tool_model = defaults.image_tool_model.clone();
    initial.pdf_tool_model = defaults.pdf_tool_model.clone();
    initial.audio_tool_model = defaults.audio_tool_model.clone();
    initial.image_generation_tool_model = defaults.image_generation_tool_model.clone();
    initial.search_tool_model = defaults.search_tool_model.clone();
    client
        .initialize(
            &effective_model_config(model_config, reasoning_effort),
            &initial,
        )
        .map_err(anyhow::Error::msg)?;
    Ok((client, events))
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
            ChatMessageItem::Reasoning(reasoning) => {
                parts.push(format!("[reasoning] {}", reasoning.text))
            }
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
        serde_json::to_string_pretty(message).unwrap_or_else(|_| "{}".to_string())
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
