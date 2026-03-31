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
    BotCommandConfig, ChannelConfig, MainAgentConfig, ModelConfig, ServerConfig,
    default_bot_commands,
};
use crate::cron::{
    ClaimedCronTask, CronCheckerConfig, CronCreateRequest, CronManager, CronUpdateRequest,
};
use crate::domain::{
    AttachmentKind, ChannelAddress, OutgoingAttachment, OutgoingMessage, ProcessingState,
    StoredAttachment,
};
use crate::prompt::{AgentPromptKind, build_agent_system_prompt, greeting_for_language};
use crate::session::{SessionManager, SessionSnapshot};
use crate::sink::{SinkRouter, SinkTarget};
use agent_frame::config::{AgentConfig as FrameAgentConfig, CacheControlConfig, UpstreamConfig};
use agent_frame::{
    ChatMessage, SessionExecutionControl, SessionRunReport, TokenUsage, Tool,
    extract_assistant_text,
};
use anyhow::{Context, Result, anyhow};
use base64::Engine;
use chrono::Utc;
use humantime::parse_duration;
use serde_json::{Value, json};
use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use tokio::select;
use tokio::sync::{RwLock, mpsc, oneshot};
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
    channels: Arc<HashMap<String, Arc<dyn Channel>>>,
    command_catalog: HashMap<String, Vec<BotCommandConfig>>,
    models: BTreeMap<String, ModelConfig>,
    main_agent: MainAgentConfig,
    sink_router: Arc<RwLock<SinkRouter>>,
    cron_manager: Arc<Mutex<CronManager>>,
    agent_registry: Arc<Mutex<AgentRegistry>>,
    max_global_sub_agents: usize,
    subagent_count: Arc<AtomicUsize>,
    cron_poll_interval_seconds: u64,
    background_job_sender: mpsc::Sender<BackgroundJobRequest>,
}

struct SubAgentSlot {
    counter: Arc<AtomicUsize>,
}

enum TimedRunOutcome {
    Completed(SessionRunReport),
    TimedOut {
        checkpoint: Option<SessionRunReport>,
        error: anyhow::Error,
    },
}

impl Drop for SubAgentSlot {
    fn drop(&mut self) {
        self.counter.fetch_sub(1, Ordering::SeqCst);
    }
}

impl ServerRuntime {
    fn model_config(&self, model_key: &str) -> Result<&ModelConfig> {
        self.models
            .get(model_key)
            .with_context(|| format!("unknown model {}", model_key))
    }

    fn main_agent_timeout_seconds(&self, model_key: &str) -> Result<f64> {
        if let Some(timeout_seconds) = self.main_agent.timeout_seconds {
            return Ok(timeout_seconds);
        }
        Ok(background_agent_timeout_seconds(
            self.models
                .get(model_key)
                .with_context(|| format!("unknown model {}", model_key))?
                .timeout_seconds,
        ))
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

    fn has_active_child_agents(&self, parent_agent_id: uuid::Uuid) -> bool {
        self.agent_registry
            .lock()
            .map(|registry| registry.has_active_children(parent_agent_id))
            .unwrap_or(false)
    }

    async fn wait_for_child_agents_to_finish(&self, parent_agent_id: uuid::Uuid) {
        while self.has_active_child_agents(parent_agent_id) {
            tokio::time::sleep(Duration::from_millis(250)).await;
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
                    supports_vision_input: image_model.supports_vision_input,
                    api_key: image_model.api_key.clone(),
                    api_key_env: image_model.api_key_env.clone(),
                    chat_completions_path: image_model.chat_completions_path.clone(),
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

        Ok(FrameAgentConfig {
            enabled_tools: self.main_agent.enabled_tools.clone(),
            upstream: UpstreamConfig {
                base_url: model.api_endpoint.clone(),
                model: model.model.clone(),
                supports_vision_input: model.supports_vision_input,
                api_key: model.api_key.clone(),
                api_key_env: model.api_key_env.clone(),
                chat_completions_path: model.chat_completions_path.clone(),
                timeout_seconds: upstream_timeout_seconds
                    .unwrap_or(model.timeout_seconds)
                    .min(model.timeout_seconds),
                context_window_tokens: model.context_window_tokens,
                cache_control: model.cache_ttl.as_ref().map(|ttl| CacheControlConfig {
                    cache_type: "ephemeral".to_string(),
                    ttl: Some(ttl.clone()),
                }),
                reasoning: model.reasoning.clone(),
                headers: model.headers.clone(),
                native_web_search: model.native_web_search.clone(),
                external_web_search: model.external_web_search.clone(),
            },
            image_tool_upstream,
            skills_dirs: vec![self.agent_workspace.skills_dir.clone()],
            system_prompt: build_agent_system_prompt(
                &self.agent_workspace,
                session,
                kind,
                model_key,
                model,
                &self.models,
                &self.main_agent,
                &commands,
            ),
            max_tool_roundtrips: self.main_agent.max_tool_roundtrips,
            workspace_root: workspace_root.to_path_buf(),
            enable_context_compression: self.main_agent.enable_context_compression,
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
    ) -> Vec<Tool> {
        let mut tools = Vec::new();
        if matches!(
            kind,
            AgentPromptKind::MainForeground | AgentPromptKind::MainBackground
        ) {
            let runtime = self.clone();
            let session = session.clone();
            tools.push(Tool::new(
                "run_subagent",
                "Run delegated subagent work in the shared rundir. Use either task/model/timeout_seconds for a single subagent, or tasks:[{task, model?, timeout_seconds}, ...] to run multiple subagents in parallel. Returns subagent reply text, optional attachment_paths, timeout status, and token usage.",
                json!({
                    "type": "object",
                    "properties": {
                        "task": {"type": "string"},
                        "model": {"type": "string"},
                        "timeout_seconds": {"type": "number"},
                        "tasks": {
                            "type": "array",
                            "items": {
                                "type": "object",
                                "properties": {
                                    "task": {"type": "string"},
                                    "model": {"type": "string"},
                                    "timeout_seconds": {"type": "number"}
                                },
                                "required": ["task", "timeout_seconds"],
                                "additionalProperties": false
                            }
                        }
                    },
                    "additionalProperties": false
                }),
                move |arguments| {
                    let object = arguments
                        .as_object()
                        .ok_or_else(|| anyhow!("tool arguments must be an object"))?;
                    if let Some(tasks) = object.get("tasks").and_then(Value::as_array) {
                        let requests = tasks
                            .iter()
                            .map(|task| {
                                let item = task
                                    .as_object()
                                    .ok_or_else(|| anyhow!("each task must be an object"))?;
                                let task = item
                                    .get("task")
                                    .and_then(Value::as_str)
                                    .map(str::trim)
                                    .filter(|value| !value.is_empty())
                                    .ok_or_else(|| anyhow!("task must be a non-empty string"))?;
                                let model_key = item
                                    .get("model")
                                    .and_then(Value::as_str)
                                    .map(ToOwned::to_owned);
                                let timeout_seconds = item
                                    .get("timeout_seconds")
                                    .and_then(Value::as_f64)
                                    .filter(|value| *value > 0.0)
                                    .ok_or_else(|| anyhow!("timeout_seconds must be a positive number"))?;
                                Ok((task.to_string(), model_key, timeout_seconds))
                            })
                            .collect::<Result<Vec<_>>>()?;
                        if requests.is_empty() {
                            return Err(anyhow!("tasks must not be empty"));
                        }
                        runtime.run_subagent_batch(agent_id, session.clone(), requests)
                    } else {
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
                        let timeout_seconds = object
                            .get("timeout_seconds")
                            .and_then(Value::as_f64)
                            .filter(|value| *value > 0.0)
                            .ok_or_else(|| anyhow!("timeout_seconds must be a positive number"))?;
                        runtime.run_subagent(
                            agent_id,
                            session.clone(),
                            model_key,
                            task.to_string(),
                            timeout_seconds,
                        )
                    }
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
                        "model": {"type": "string"},
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
                            model_key: object
                                .get("model")
                                .and_then(Value::as_str)
                                .map(ToOwned::to_owned)
                                .unwrap_or_else(|| runtime.main_agent.model.clone()),
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
                "list_background_agents",
                "List tracked background agents with status, model, and token usage statistics.",
                json!({
                    "type": "object",
                    "properties": {},
                    "additionalProperties": false
                }),
                move |_| runtime.list_managed_agents(ManagedAgentKind::Background),
            ));

            let runtime = self.clone();
            tools.push(Tool::new(
                "list_subagents",
                "List tracked subagents with status, model, parent relationships, and token usage statistics.",
                json!({
                    "type": "object",
                    "properties": {},
                    "additionalProperties": false
                }),
                move |_| runtime.list_managed_agents(ManagedAgentKind::Subagent),
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
        let workspace_root = self.agent_workspace.rundir.clone();
        let _agent_tmp_dir = self.ensure_agent_tmp_dir(agent_id)?;
        let config = self.build_agent_frame_config(
            &session,
            &workspace_root,
            kind,
            &model_key,
            upstream_timeout_seconds,
        )?;
        let backend = self.model_config(&model_key)?.backend;
        let extra_tools = self.build_extra_tools(&session, kind, agent_id);
        run_backend_session(
            backend,
            previous_messages,
            prompt,
            config,
            extra_tools,
            execution_control,
        )
    }

    async fn run_agent_turn_with_timeout(
        &self,
        session: SessionSnapshot,
        kind: AgentPromptKind,
        agent_id: uuid::Uuid,
        model_key: String,
        previous_messages: Vec<ChatMessage>,
        prompt: String,
        timeout_seconds: f64,
        upstream_timeout_seconds: Option<f64>,
        timeout_label: &str,
        join_label: &str,
    ) -> Result<TimedRunOutcome> {
        let runtime = self.clone();
        let timeout_label = timeout_label.to_string();
        let join_label = join_label.to_string();
        let (checkpoint_sender, mut checkpoint_receiver) = mpsc::unbounded_channel();
        let execution_control = SessionExecutionControl::with_checkpoint_callback(move |report| {
            let _ = checkpoint_sender.send(report);
        });
        let cancellation_handle = execution_control.clone();
        let join_handle = tokio::task::spawn_blocking(move || {
            runtime.run_agent_turn_sync(
                session,
                kind,
                agent_id,
                model_key,
                previous_messages,
                prompt,
                upstream_timeout_seconds,
                Some(execution_control),
            )
        });
        let deadline = tokio::time::sleep(Duration::from_secs_f64(timeout_seconds));
        tokio::pin!(deadline);
        let mut latest_checkpoint = None;
        tokio::pin!(join_handle);
        loop {
            select! {
                checkpoint = checkpoint_receiver.recv() => {
                    if checkpoint.is_none() {
                        continue;
                    }
                    latest_checkpoint = checkpoint;
                }
                join_result = &mut join_handle => {
                    let report = join_result.context(join_label)?.context("agent turn failed")?;
                    return Ok(TimedRunOutcome::Completed(report));
                }
                _ = &mut deadline => {
                    cancellation_handle.request_cancel();
                    return Ok(TimedRunOutcome::TimedOut {
                        checkpoint: latest_checkpoint,
                        error: anyhow!(
                            "{} timed out after {:.1} seconds",
                            timeout_label,
                            timeout_seconds
                        ),
                    });
                }
            }
        }
    }

    fn run_agent_turn_with_timeout_blocking(
        &self,
        session: SessionSnapshot,
        kind: AgentPromptKind,
        agent_id: uuid::Uuid,
        model_key: String,
        previous_messages: Vec<ChatMessage>,
        prompt: String,
        timeout_seconds: f64,
        upstream_timeout_seconds: Option<f64>,
        timeout_label: &str,
    ) -> Result<TimedRunOutcome> {
        let (checkpoint_sender, checkpoint_receiver) = std::sync::mpsc::channel();
        let execution_control = SessionExecutionControl::with_checkpoint_callback(move |report| {
            let _ = checkpoint_sender.send(report);
        });
        let cancellation_handle = execution_control.clone();
        let runtime = self.clone();
        let timeout_label = timeout_label.to_string();
        let handle = std::thread::spawn(move || {
            runtime.run_agent_turn_sync(
                session,
                kind,
                agent_id,
                model_key,
                previous_messages,
                prompt,
                upstream_timeout_seconds,
                Some(execution_control),
            )
        });
        let deadline = std::time::Instant::now() + Duration::from_secs_f64(timeout_seconds);
        let mut latest_checkpoint = None;
        loop {
            match checkpoint_receiver.recv_timeout(Duration::from_millis(25)) {
                Ok(report) => latest_checkpoint = Some(report),
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {}
            }
            if handle.is_finished() {
                let report = handle
                    .join()
                    .map_err(|_| anyhow!("agent worker thread panicked"))??;
                return Ok(TimedRunOutcome::Completed(report));
            }
            if std::time::Instant::now() >= deadline {
                cancellation_handle.request_cancel();
                return Ok(TimedRunOutcome::TimedOut {
                    checkpoint: latest_checkpoint,
                    error: anyhow!(
                        "{} timed out after {:.1} seconds",
                        timeout_label,
                        timeout_seconds
                    ),
                });
            }
        }
    }

    fn run_subagent(
        &self,
        parent_agent_id: uuid::Uuid,
        session: SessionSnapshot,
        model_key: Option<String>,
        prompt: String,
        timeout_seconds: f64,
    ) -> Result<Value> {
        let _slot = self.try_acquire_subagent_slot()?;
        let subagent_id = uuid::Uuid::new_v4();
        let model_key = model_key.unwrap_or_else(|| self.main_agent.model.clone());
        self.model_config(&model_key)?;
        self.register_managed_agent(
            subagent_id,
            ManagedAgentKind::Subagent,
            model_key.clone(),
            Some(parent_agent_id),
            &session,
            ManagedAgentState::Running,
        );

        info!(
            log_stream = "agent",
            log_key = %subagent_id,
            kind = "subagent_started",
            parent_agent_id = %parent_agent_id,
            session_id = %session.id,
            channel_id = %session.address.channel_id,
            model = %model_key,
            "subagent started"
        );

        let report = self.run_agent_turn_with_timeout_blocking(
            session.clone(),
            AgentPromptKind::SubAgent,
            subagent_id,
            model_key.clone(),
            Vec::new(),
            prompt,
            timeout_seconds,
            Some(timeout_seconds),
            "subagent",
        );
        let (report, timed_out) = match report {
            Ok(TimedRunOutcome::Completed(report)) => {
                self.mark_managed_agent_completed(subagent_id, &report.usage);
                (report, false)
            }
            Ok(TimedRunOutcome::TimedOut { checkpoint, error }) => {
                let usage = checkpoint
                    .as_ref()
                    .map(|report| report.usage.clone())
                    .unwrap_or_default();
                self.mark_managed_agent_timed_out(subagent_id, &usage, &error);
                let report = checkpoint.ok_or(error)?;
                (report, true)
            }
            Err(error) => {
                self.mark_managed_agent_failed(subagent_id, &TokenUsage::default(), &error);
                return Err(error);
            }
        };
        log_turn_usage(
            subagent_id,
            &session,
            &report.usage,
            false,
            "subagent",
            Some(parent_agent_id),
        );
        let assistant_text = extract_assistant_text(&report.messages);
        let (clean_text, attachments) =
            extract_attachment_references(&assistant_text, &self.agent_workspace.rundir)?;
        let attachment_paths = attachments
            .iter()
            .map(|item| relative_attachment_path(&self.agent_workspace.rundir, &item.path))
            .collect::<Result<Vec<_>>>()?;
        info!(
            log_stream = "agent",
            log_key = %subagent_id,
            kind = "subagent_completed",
            parent_agent_id = %parent_agent_id,
            session_id = %session.id,
            channel_id = %session.address.channel_id,
            has_text = !clean_text.trim().is_empty(),
            attachment_count = attachment_paths.len() as u64,
            "subagent completed"
        );
        Ok(json!({
            "agent_id": subagent_id,
            "parent_agent_id": parent_agent_id,
            "model": model_key,
            "text": clean_text,
            "attachment_paths": attachment_paths,
            "timed_out": timed_out,
            "usage": {
                "llm_calls": report.usage.llm_calls,
                "prompt_tokens": report.usage.prompt_tokens,
                "completion_tokens": report.usage.completion_tokens,
                "total_tokens": report.usage.total_tokens,
                "cache_hit_tokens": report.usage.cache_hit_tokens,
                "cache_miss_tokens": report.usage.cache_miss_tokens,
                "cache_read_tokens": report.usage.cache_read_tokens,
                "cache_write_tokens": report.usage.cache_write_tokens
            }
        }))
    }

    fn run_subagent_batch(
        &self,
        parent_agent_id: uuid::Uuid,
        session: SessionSnapshot,
        requests: Vec<(String, Option<String>, f64)>,
    ) -> Result<Value> {
        let mut handles = Vec::with_capacity(requests.len());
        for (task, model_key, timeout_seconds) in requests {
            let runtime = self.clone();
            let session = session.clone();
            let handle = std::thread::spawn(move || {
                runtime.run_subagent(parent_agent_id, session, model_key, task, timeout_seconds)
            });
            handles.push(handle);
        }

        let mut results = Vec::new();
        for handle in handles {
            let result = handle
                .join()
                .map_err(|_| anyhow!("subagent worker thread panicked"))??;
            results.push(result);
        }

        Ok(json!({
            "results": results,
            "count": results.len()
        }))
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
        let model_key = model_key.unwrap_or_else(|| self.main_agent.model.clone());
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
        let run_result = self
            .run_agent_turn_with_timeout(
                job.session.clone(),
                AgentPromptKind::MainBackground,
                job.agent_id,
                job.model_key.clone(),
                Vec::new(),
                job.prompt.clone(),
                timeout_seconds,
                Some(timeout_seconds),
                "background agent",
                "background agent task join failed",
            )
            .await;

        match run_result {
            Ok(TimedRunOutcome::Completed(report)) => {
                let assistant_text = extract_assistant_text(&report.messages);
                let outgoing = build_outgoing_message_for_session(
                    &job.session,
                    &assistant_text,
                    &self.agent_workspace.rundir,
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
                    cleanup_detached_session_root(&job).ok();
                    return self
                        .handle_background_job_failure(&job, &error)
                        .await
                        .with_context(|| format!("{error:#}"));
                }
                self.mark_managed_agent_completed(job.agent_id, &report.usage);
                cleanup_detached_session_root(&job).ok();
                Ok(())
            }
            Ok(TimedRunOutcome::TimedOut { checkpoint, error }) => {
                let usage = checkpoint
                    .as_ref()
                    .map(|report| report.usage.clone())
                    .unwrap_or_default();
                self.mark_managed_agent_timed_out(job.agent_id, &usage, &error);
                cleanup_detached_session_root(&job).ok();
                self.handle_background_job_failure(&job, &error).await
            }
            Err(error) => {
                self.mark_managed_agent_failed(job.agent_id, &TokenUsage::default(), &error);
                cleanup_detached_session_root(&job).ok();
                self.handle_background_job_failure(&job, &error).await
            }
        }
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
            self.main_agent_timeout_seconds(&job.model_key)?,
            error,
        );
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
                recovery_timeout,
                Some(recovery_timeout),
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
                    &self.agent_workspace.rundir,
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

pub struct Server {
    workdir: PathBuf,
    agent_workspace: AgentWorkspace,
    channels: Arc<HashMap<String, Arc<dyn Channel>>>,
    command_catalog: HashMap<String, Vec<BotCommandConfig>>,
    models: BTreeMap<String, ModelConfig>,
    main_agent: MainAgentConfig,
    sessions: SessionManager,
    sink_router: Arc<RwLock<SinkRouter>>,
    cron_manager: Arc<Mutex<CronManager>>,
    agent_registry: Arc<Mutex<AgentRegistry>>,
    max_global_sub_agents: usize,
    subagent_count: Arc<AtomicUsize>,
    cron_poll_interval_seconds: u64,
    background_job_sender: mpsc::Sender<BackgroundJobRequest>,
    background_job_receiver: Option<mpsc::Receiver<BackgroundJobRequest>>,
}

impl Server {
    pub fn from_config(config: ServerConfig, workdir: impl AsRef<Path>) -> Result<Self> {
        let workdir = workdir.as_ref().to_path_buf();
        std::fs::create_dir_all(&workdir)
            .with_context(|| format!("failed to create workdir {}", workdir.display()))?;
        let agent_workspace = AgentWorkspace::initialize(&workdir)?;

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
            main_model = %config.main_agent.model,
            main_backend = ?config.models[&config.main_agent.model].backend,
            "server initialized"
        );

        let (background_job_sender, background_job_receiver) = mpsc::channel(64);
        let cron_manager = Arc::new(Mutex::new(CronManager::load_or_create(&workdir)?));
        let agent_registry = Arc::new(Mutex::new(AgentRegistry::load_or_create(&workdir)?));

        Ok(Self {
            sessions: SessionManager::new(&workdir)?,
            workdir,
            agent_workspace,
            channels: Arc::new(channels),
            command_catalog,
            models: config.models,
            main_agent: config.main_agent,
            sink_router: Arc::new(RwLock::new(SinkRouter::new())),
            cron_manager,
            agent_registry,
            max_global_sub_agents: config.max_global_sub_agents,
            subagent_count: Arc::new(AtomicUsize::new(0)),
            cron_poll_interval_seconds: config.cron_poll_interval_seconds,
            background_job_sender,
            background_job_receiver: Some(background_job_receiver),
        })
    }

    pub async fn run(mut self) -> Result<()> {
        let (sender, mut receiver) = mpsc::channel::<IncomingMessage>(128);
        {
            let runtime = self.tool_runtime();
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
        if let Some(mut background_receiver) = self.background_job_receiver.take() {
            let runtime = self.tool_runtime();
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

        for channel in self.channels.values() {
            spawn_channel_supervisor(Arc::clone(channel), sender.clone());
        }
        drop(sender);

        let mut idle_compaction_ticker = interval(Duration::from_secs(
            self.main_agent
                .idle_context_compaction_poll_interval_seconds,
        ));
        idle_compaction_ticker.set_missed_tick_behavior(MissedTickBehavior::Skip);

        loop {
            select! {
                _ = idle_compaction_ticker.tick() => {
                    if self.main_agent.enable_idle_context_compaction
                        && let Err(error) = self.run_idle_context_compaction_once().await
                    {
                        error!(
                            log_stream = "server",
                            kind = "idle_context_compaction_failed",
                            error = %format!("{error:#}"),
                            "idle context compaction pass failed"
                        );
                    }
                }
                maybe_message = receiver.recv() => {
                    let Some(message) = maybe_message else {
                        break;
                    };
                    if let Err(error) = self.handle_incoming(message).await {
                        error!(
                            log_stream = "server",
                            kind = "handle_incoming_failed",
                            error = %format!("{error:#}"),
                            "failed to handle incoming message"
                        );
                    }
                }
            }
        }

        warn!(
            log_stream = "server",
            kind = "message_loop_ended",
            "all channel senders closed; server loop ended"
        );
        Ok(())
    }

    async fn run_idle_context_compaction_once(&mut self) -> Result<()> {
        let model = self.main_model()?.clone();
        let Some(ttl) = model.cache_ttl.as_deref() else {
            return Ok(());
        };
        let ttl = parse_duration(ttl)
            .with_context(|| format!("failed to parse model cache_ttl '{}'", ttl))?;
        let lead_time = Duration::from_secs(30);
        let Some(idle_threshold) = ttl.checked_sub(lead_time) else {
            return Ok(());
        };
        let now = Utc::now();
        let runtime = self.tool_runtime();
        let snapshots = self.sessions.list_foreground_snapshots();

        for session in snapshots {
            if !should_attempt_idle_context_compaction(&session, now, idle_threshold) {
                continue;
            }

            let config = runtime.build_agent_frame_config(
                &session,
                &self.agent_workspace.rundir,
                AgentPromptKind::MainForeground,
                &self.main_agent.model,
                None,
            )?;
            let extra_tools = runtime.build_extra_tools(
                &session,
                AgentPromptKind::MainForeground,
                session.agent_id,
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

            self.sessions
                .record_idle_compaction(&session.address, report.messages)
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
            channels: Arc::clone(&self.channels),
            command_catalog: self.command_catalog.clone(),
            models: self.models.clone(),
            main_agent: self.main_agent.clone(),
            sink_router: Arc::clone(&self.sink_router),
            cron_manager: Arc::clone(&self.cron_manager),
            agent_registry: Arc::clone(&self.agent_registry),
            max_global_sub_agents: self.max_global_sub_agents,
            subagent_count: Arc::clone(&self.subagent_count),
            cron_poll_interval_seconds: self.cron_poll_interval_seconds,
            background_job_sender: self.background_job_sender.clone(),
        }
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

    async fn handle_incoming(&mut self, incoming: IncomingMessage) -> Result<()> {
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

        if incoming
            .text
            .as_deref()
            .map(str::trim)
            .is_some_and(|text| text == "/new")
        {
            let session = self.sessions.reset_foreground(&incoming.address)?;
            let welcome = match self.initialize_foreground_session(&session, true).await {
                Ok(welcome) => welcome,
                Err(error) => {
                    self.send_user_error_message(&channel, &incoming.address, &error)
                        .await;
                    return Err(error);
                }
            };
            info!(
                log_stream = "session",
                log_key = %session.id,
                kind = "session_reset",
                channel_id = %incoming.address.channel_id,
                conversation_id = %incoming.address.conversation_id,
                "foreground session reset"
            );
            channel.send(&incoming.address, welcome).await?;
            return Ok(());
        }

        if incoming
            .text
            .as_deref()
            .map(str::trim)
            .is_some_and(|text| text == "/help")
        {
            let help_text = self.help_text_for_channel(&incoming.address.channel_id);
            info!(
                log_stream = "server",
                kind = "help_requested",
                channel_id = %incoming.address.channel_id,
                conversation_id = %incoming.address.conversation_id,
                "rendering help text"
            );
            channel
                .send(&incoming.address, OutgoingMessage::text(help_text))
                .await?;
            return Ok(());
        }

        let session = self.sessions.ensure_foreground(&incoming.address)?;
        if session.agent_message_count == 0 {
            if let Err(error) = self.initialize_foreground_session(&session, false).await {
                self.send_user_error_message(&channel, &incoming.address, &error)
                    .await;
                return Err(error);
            }
        }
        let session = self
            .sessions
            .get_snapshot(&incoming.address)
            .expect("session should exist after initialization");

        let stored_attachments = self
            .materialize_attachments(&session.attachments_dir, incoming.attachments)
            .await?;
        let user_message = build_user_turn_message(
            incoming.text.as_deref(),
            &stored_attachments,
            self.main_model()?,
            backend_supports_native_multimodal_input(self.main_model()?.backend),
        )?;

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
            .run_main_agent_turn(&session, user_message)
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
        let (messages, outgoing, usage, timed_out) = turn_result?;

        self.sessions
            .record_agent_turn(&incoming.address, messages)
            .context("failed to persist agent_frame messages")?;
        self.sessions.append_user_message(
            &incoming.address,
            incoming.text.clone(),
            stored_attachments.clone(),
        )?;
        self.sessions.append_assistant_message(
            &incoming.address,
            outgoing.text.clone(),
            Vec::new(),
        )?;

        let foreground = self.build_foreground_agent(&session)?;
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

        channel.send(&incoming.address, outgoing).await?;
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

    async fn initialize_foreground_session(
        &mut self,
        session: &SessionSnapshot,
        show_reply: bool,
    ) -> Result<OutgoingMessage> {
        let greeting = ChatMessage::text("user", greeting_for_language(&self.main_agent.language));
        let (messages, outgoing, usage, timed_out) = self
            .run_main_agent_turn(session, greeting)
            .await
            .context("failed to initialize foreground session")?;
        self.sessions
            .record_agent_turn(&session.address, messages)?;
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
            self.sessions.append_assistant_message(
                &session.address,
                outgoing.text.clone(),
                Vec::new(),
            )?;
        }
        Ok(outgoing)
    }

    async fn run_main_agent_turn(
        &self,
        session: &SessionSnapshot,
        next_user_message: ChatMessage,
    ) -> Result<(Vec<ChatMessage>, OutgoingMessage, TokenUsage, bool)> {
        let workspace_root = self.agent_workspace.rundir.clone();
        let mut previous_messages = session.agent_messages.clone();
        previous_messages.push(next_user_message);
        let timeout_seconds = self.main_agent_timeout_seconds(&self.main_agent.model)?;
        let runtime = self.tool_runtime();
        let run_result = runtime
            .run_agent_turn_with_timeout(
                session.clone(),
                AgentPromptKind::MainForeground,
                session.agent_id,
                self.main_agent.model.clone(),
                previous_messages,
                String::new(),
                timeout_seconds,
                Some(timeout_seconds),
                "foreground agent turn",
                "agent_frame task join failed",
            )
            .await?;

        match run_result {
            TimedRunOutcome::Completed(report) => {
                let assistant_text = extract_assistant_text(&report.messages);
                let outgoing =
                    build_outgoing_message_for_session(session, &assistant_text, &workspace_root)?;
                Ok((report.messages, outgoing, report.usage, false))
            }
            TimedRunOutcome::TimedOut { checkpoint, error } => {
                let report = checkpoint.ok_or(error)?;
                let assistant_text = extract_assistant_text(&report.messages);
                let outgoing =
                    build_outgoing_message_for_session(session, &assistant_text, &workspace_root)?;
                Ok((report.messages, outgoing, report.usage, true))
            }
        }
    }

    fn main_model(&self) -> Result<&ModelConfig> {
        self.models
            .get(&self.main_agent.model)
            .with_context(|| format!("unknown main_agent model {}", self.main_agent.model))
    }

    fn main_agent_timeout_seconds(&self, model_key: &str) -> Result<f64> {
        if let Some(timeout_seconds) = self.main_agent.timeout_seconds {
            return Ok(timeout_seconds);
        }
        Ok(background_agent_timeout_seconds(
            self.models
                .get(model_key)
                .with_context(|| format!("unknown model {}", model_key))?
                .timeout_seconds,
        ))
    }

    fn build_foreground_agent(&self, session: &SessionSnapshot) -> Result<ForegroundAgent> {
        let model = self.main_model()?;
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
                AgentPromptKind::MainForeground,
                &self.main_agent.model,
                model,
                &self.models,
                &self.main_agent,
                &commands,
            ),
        })
    }

    async fn send_user_error_message(
        &self,
        channel: &Arc<dyn Channel>,
        address: &ChannelAddress,
        error: &anyhow::Error,
    ) {
        let text = user_facing_error_text(&self.main_agent.language, error);
        if let Err(send_error) = channel.send(address, OutgoingMessage::text(text)).await {
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

fn build_user_turn_message(
    text: Option<&str>,
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
        return Ok(ChatMessage::text(
            "user",
            compose_user_prompt(text, attachments),
        ));
    }

    let mut text_sections = Vec::new();
    if let Some(text) = text.map(str::trim).filter(|value| !value.is_empty()) {
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
            "attachment path must be relative to rundir, got absolute path {}",
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
            "attachment path escapes rundir: {}",
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
    workdir: &Path,
    address: ChannelAddress,
    agent_id: uuid::Uuid,
) -> Result<SessionSnapshot> {
    let session_id = uuid::Uuid::new_v4();
    let root_dir = workdir.join("sessions").join(session_id.to_string());
    let attachments_dir = root_dir.join("attachments");
    std::fs::create_dir_all(&attachments_dir)
        .with_context(|| format!("failed to create {}", attachments_dir.display()))?;
    Ok(SessionSnapshot {
        id: session_id,
        agent_id,
        address,
        root_dir,
        attachments_dir,
        message_count: 0,
        agent_message_count: 0,
        agent_messages: Vec::new(),
        last_agent_returned_at: None,
        last_compacted_at: None,
        turn_count: 0,
        last_compacted_turn_count: 0,
    })
}

fn cleanup_detached_session_root(job: &BackgroundJobRequest) -> Result<()> {
    if job.cron_task_id.is_some() && job.session.root_dir.exists() {
        std::fs::remove_dir_all(&job.session.root_dir).with_context(|| {
            format!(
                "failed to remove detached cron session directory {}",
                job.session.root_dir.display()
            )
        })?;
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
    let timeout_like = is_timeout_like(error);
    if language.starts_with("zh") {
        if timeout_like {
            "这一轮处理超时了。请稍后重试，或者发送 /new 重新开始。".to_string()
        } else {
            "这一轮处理失败了。请稍后重试，或者发送 /new 重新开始。".to_string()
        }
    } else if timeout_like {
        "This turn timed out. Please try again, or send /new to start over.".to_string()
    } else {
        "This turn failed. Please try again, or send /new to start over.".to_string()
    }
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

#[cfg(test)]
mod tests {
    use super::{
        SinkTarget, background_agent_timeout_seconds, background_recovery_timeout_seconds,
        background_timeout_with_active_children_text, build_user_turn_message,
        channel_restart_backoff_seconds, extract_attachment_references, is_timeout_like,
        parse_sink_target, should_attempt_idle_context_compaction,
    };
    use crate::backend::AgentBackendKind;
    use crate::config::ModelConfig;
    use crate::domain::ChannelAddress;
    use crate::domain::{AttachmentKind, StoredAttachment};
    use crate::session::SessionSnapshot;
    use anyhow::anyhow;
    use chrono::{Duration as ChronoDuration, Utc};
    use serde_json::json;
    use std::fs;
    use std::path::PathBuf;
    use std::time::Duration;
    use tempfile::TempDir;
    use uuid::Uuid;

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
            api_endpoint: "https://example.com/v1".to_string(),
            model: "demo-vision".to_string(),
            backend: AgentBackendKind::AgentFrame,
            supports_vision_input: true,
            image_tool_model: None,
            api_key: None,
            api_key_env: "TEST_API_KEY".to_string(),
            chat_completions_path: "/chat/completions".to_string(),
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
            build_user_turn_message(Some("看看这张图"), &[attachment], &model, true).unwrap();

        let content = message.content.unwrap();
        let items = content.as_array().unwrap();
        assert_eq!(items[0]["type"], "text");
        assert_eq!(items[1]["type"], "image_url");
        let url = items[1]["image_url"]["url"].as_str().unwrap();
        assert!(url.starts_with("data:image/png;base64,"));
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
            attachments_dir: PathBuf::from("/tmp/session/attachments"),
            message_count: 0,
            agent_message_count: 3,
            agent_messages: Vec::new(),
            last_agent_returned_at: Some(now - ChronoDuration::seconds(400)),
            last_compacted_at: None,
            turn_count: 2,
            last_compacted_turn_count: 1,
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
}
