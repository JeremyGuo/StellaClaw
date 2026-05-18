#![allow(dead_code)]

use std::{
    collections::{BTreeMap, VecDeque},
    fs,
    io::{BufRead, BufReader, Seek, SeekFrom},
    path::{Path, PathBuf},
    process::Command,
    sync::atomic::{AtomicU64, Ordering},
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

use anyhow::{anyhow, Context, Result};
use crossbeam_channel::select;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::{
    config::{SandboxConfig, SessionDefaults, SessionProfile, ToolModelTarget},
    conversation_new::{
        ConversationService, ServiceAddr, ServiceCall, ServiceOutput, ServiceRunContext,
        ServiceStatusUpdate, ServiceStopped,
    },
    service_protos::{
        agent_session::{
            self, decode_request, encode_response, text_message, AgentMessageOrigin,
            AgentSessionBinding, AgentSessionContext, AgentSessionEvent, AgentSessionKind,
            AgentSessionMessageHistory, AgentSessionMessageRecord, AgentSessionRequest,
            AgentSessionResponse, AgentSessionState, AgentSessionStatus,
        },
        channel, cron,
        cron::{
            CronRequest, CronResponse, CronSchedule, CronTaskOutputPolicy, CronTaskPatch,
            CronTaskPayload, CronTaskRegistration,
        },
        kernel,
        kernel::KernelResponse,
        memory::{self, MemoryRequest, MemoryResponse, MemorySearchResult, MemorySourceRef},
        skill::{self, SkillPersistMode, SkillRequest, SkillResponse},
        tool_binary::{self, ToolBinaryRequest, ToolBinaryResponse},
    },
    session_client::AgentServerClient,
};
use stellaclaw_core::model_config::{ModelCapability, ModelConfig};
use stellaclaw_core::session_actor::{
    ChatMessage, ChatRole, ConversationBridgeRequest, ConversationBridgeResponse,
    SessionErrorDetail, SessionEvent as CoreSessionEvent, SessionInitial,
    SessionRequest as CoreSessionRequest, SessionType, TaskPlanItemStatus, TaskPlanView,
    ToolRemoteMode, ToolResultContent, ToolResultItem,
};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentSessionLaunchConfig {
    pub session_id: String,
    pub conversation_root: PathBuf,
    pub workspace_root: PathBuf,
    pub agent_server_path: Option<PathBuf>,
    pub session_profile: Option<SessionProfile>,
    pub models: BTreeMap<String, ModelConfig>,
    pub session_defaults: SessionDefaults,
    pub memory_enabled: bool,
    pub tool_remote_mode: ToolRemoteMode,
    pub sandbox: Option<SandboxConfig>,
    pub reasoning_effort: Option<String>,
    pub idle_timeout_compact_enabled: Option<bool>,
}

pub struct AgentSessionService {
    kind: AgentSessionKind,
    binding: AgentSessionBinding,
    launch: AgentSessionLaunchConfig,
}

impl AgentSessionService {
    pub fn new(
        kind: AgentSessionKind,
        binding: AgentSessionBinding,
        launch: AgentSessionLaunchConfig,
    ) -> Self {
        Self {
            kind,
            binding,
            launch,
        }
    }
}

impl ConversationService for AgentSessionService {
    fn run(self: Box<Self>, ctx: ServiceRunContext) -> Result<()> {
        let mut launch = self.launch.clone();
        let mut state = load_service_state(&ctx.storage)?.unwrap_or_else(|| {
            AgentSessionRuntimeState::new(self.kind.clone(), self.binding.clone())
        });
        state.kind = self.kind.clone();
        state.binding = self.binding.clone();
        state.persist(&ctx.storage)?;
        let event_sink = self.binding.event_sink.clone();
        let mut runner = start_real_session(&launch, &self.kind)?;
        let mut pending_launch: Option<AgentSessionLaunchConfig> = None;
        let mut current_plan: Option<TaskPlanView> = None;
        let mut pending_memory_requests: VecDeque<PendingBridgeRequest> = VecDeque::new();
        let mut pending_skill_requests: VecDeque<PendingBridgeRequest> = VecDeque::new();
        let mut pending_tool_binary_requests: VecDeque<PendingBridgeRequest> = VecDeque::new();
        let mut pending_cron_requests: VecDeque<PendingBridgeRequest> = VecDeque::new();
        let mut pending_child_starts = VecDeque::new();
        let mut pending_service_responses = BTreeMap::new();
        let mut pending_subagent_joins = VecDeque::new();
        let mut session_event_rx = runner
            .as_ref()
            .map(|runner| runner.events.clone())
            .unwrap_or_else(crossbeam_channel::never);
        if runner.is_some() {
            ctx.outbox.send(ServiceOutput::Status(ServiceStatusUpdate {
                addr: ctx.addr.clone(),
                label: "real_session_started".to_string(),
                detail: serde_json::json!({
                    "session_id": self.launch.session_id,
                    "kind": self.kind,
                }),
            }))?;
        }
        loop {
            let join_timer = subagent_join_timer(&pending_subagent_joins);
            select! {
                recv(join_timer) -> _ => {
                    resolve_due_subagent_joins(
                        &ctx,
                        &mut runner,
                        &mut pending_subagent_joins,
                        &state,
                    )?;
                }
                recv(ctx.stop_rx) -> stop => {
                    let reason = stop.ok().map(|stop| stop.reason);
                    if let Some(runner) = runner.take() {
                        runner.shutdown();
                    }
                    state.state = AgentSessionState::Stopped;
                    state.persist(&ctx.storage)?;
                    ctx.outbox.send(ServiceOutput::Stopped(ServiceStopped {
                        addr: ctx.addr.clone(),
                        reason,
                    }))?;
                    return Ok(());
                }
                recv(session_event_rx) -> event => {
                    match event {
                        Ok(event) => {
                            handle_core_session_event(
                                &ctx,
                                &event_sink,
                                &self.kind,
                                &mut runner,
                                &mut pending_memory_requests,
                                &mut pending_skill_requests,
                                &mut pending_tool_binary_requests,
                                &mut pending_cron_requests,
                                &mut pending_child_starts,
                                &mut pending_service_responses,
                                &mut pending_subagent_joins,
                                &mut state,
                                &mut current_plan,
                                &event,
                            )?;
                            maybe_apply_pending_launch(
                                &ctx,
                                &self.kind,
                                &mut launch,
                                &mut pending_launch,
                                &mut runner,
                                &mut session_event_rx,
                                &state,
                            )?;
                        }
                        Err(_) => {
                            session_event_rx = crossbeam_channel::never();
                            state.state = AgentSessionState::Crashed;
                            state.last_error = Some("agent session event stream closed".to_string());
                            state.persist(&ctx.storage)?;
                            emit_session_event(
                                &ctx,
                                &event_sink,
                                AgentSessionEvent::RuntimeCrashed {
                                    error: "agent session event stream closed".to_string(),
                                    error_detail: SessionErrorDetail::new(
                                        "agent_session",
                                        "event_stream_closed",
                                        "agent session event stream closed",
                                    ),
                                },
                            )?;
                        }
                    }
                }
                recv(ctx.inbox) -> call => {
                    let call = call?;
                    match decode_request(call.payload.clone()) {
                        Ok(AgentSessionRequest::EnqueueMessage { origin, message, ingress_id }) => {
                            if let Some(runner) = runner.as_mut() {
                                ctx.outbox.send(ServiceOutput::Status(ServiceStatusUpdate {
                                    addr: ctx.addr.clone(),
                                    label: "message_received".to_string(),
                                    detail: serde_json::json!({
                                        "origin": origin,
                                        "ingress_id": ingress_id,
                                        "runner": "agent_server",
                                    }),
                                }))?;
                                if matches!(origin, AgentMessageOrigin::User) {
                                    emit_session_event(
                                        &ctx,
                                        &event_sink,
                                        AgentSessionEvent::UserMessageStarted {
                                            origin: origin.clone(),
                                            ingress_id: ingress_id.clone(),
                                            message: message.clone(),
                                        },
                                    )?;
                                }
                                runner.send(AgentSessionRequest::EnqueueMessage {
                                    origin,
                                    message,
                                    ingress_id,
                                })?;
                            } else {
                                handle_skeleton_enqueue(
                                    &ctx,
                                    &event_sink,
                                    &self.kind,
                                    &mut state,
                                    origin,
                                    message,
                                    ingress_id,
                                )?;
                            }
                        }
                        Ok(AgentSessionRequest::CancelTurn { reason }) => {
                            if let Some(runner) = runner.as_mut() {
                                runner.send(AgentSessionRequest::CancelTurn { reason: reason.clone() })?;
                            } else {
                                state.state = AgentSessionState::Idle;
                                state.active_turn_id = None;
                                state.last_error = reason.clone();
                                state.persist(&ctx.storage)?;
                            }
                            ctx.outbox.send(ServiceOutput::Call(reply(
                                &ctx.addr,
                                &call.source,
                                AgentSessionResponse::Accepted,
                            )?))?;
                        }
                        Ok(AgentSessionRequest::ContinueTurn { reason }) => {
                            if let Some(runner) = runner.as_mut() {
                                runner.send(AgentSessionRequest::ContinueTurn { reason: reason.clone() })?;
                            } else {
                                state.state = AgentSessionState::Running;
                                state.active_turn_id = Some(format!(
                                    "continued_{}",
                                    state.message_count.saturating_add(1)
                                ));
                                state.persist(&ctx.storage)?;
                            }
                            ctx.outbox.send(ServiceOutput::Status(ServiceStatusUpdate {
                                addr: ctx.addr.clone(),
                                label: "continue_requested".to_string(),
                                detail: serde_json::json!({"reason": reason}),
                            }))?;
                            ctx.outbox.send(ServiceOutput::Call(reply(
                                &ctx.addr,
                                &call.source,
                                AgentSessionResponse::Accepted,
                            )?))?;
                        }
                        Ok(AgentSessionRequest::CompactNow) => {
                            if let Some(runner) = runner.as_mut() {
                                runner.send(AgentSessionRequest::CompactNow)?;
                            } else {
                                emit_session_event(
                                    &ctx,
                                    &event_sink,
                                    AgentSessionEvent::CompactCompleted {
                                        compressed: false,
                                        estimated_tokens_before: 0,
                                        estimated_tokens_after: 0,
                                        threshold_tokens: 0,
                                        retained_message_count: state.message_count,
                                        compressed_message_count: 0,
                                    },
                                )?;
                            }
                            ctx.outbox.send(ServiceOutput::Call(reply(
                                &ctx.addr,
                                &call.source,
                                AgentSessionResponse::Accepted,
                            )?))?;
                        }
                        Ok(AgentSessionRequest::ResolveHostCoordination { response }) => {
                            if let Some(runner) = runner.as_mut() {
                                runner.send(AgentSessionRequest::ResolveHostCoordination {
                                    response: response.clone(),
                                })?;
                            }
                            ctx.outbox.send(ServiceOutput::Status(ServiceStatusUpdate {
                                addr: ctx.addr.clone(),
                                label: "host_coordination_resolved".to_string(),
                                detail: response,
                            }))?;
                            ctx.outbox.send(ServiceOutput::Call(reply(
                                &ctx.addr,
                                &call.source,
                                AgentSessionResponse::Accepted,
                            )?))?;
                        }
                        Ok(AgentSessionRequest::ChildSessionEvent { session_addr, event }) => {
                            handle_child_session_event(
                                &ctx,
                                &mut runner,
                                &mut pending_subagent_joins,
                                &mut state,
                                session_addr,
                                event,
                            )?;
                        }
                        Ok(AgentSessionRequest::QueryContext { query_id, payload }) => {
                            if let Some(runner) = runner.as_mut() {
                                runner.send(AgentSessionRequest::QueryContext {
                                    query_id: query_id.clone(),
                                    payload: payload.clone(),
                                })?;
                            }
                            ctx.outbox.send(ServiceOutput::Status(ServiceStatusUpdate {
                                addr: ctx.addr.clone(),
                                label: "context_queried".to_string(),
                                detail: serde_json::json!({
                                    "query_id": query_id,
                                    "payload": payload,
                                }),
                            }))?;
                            ctx.outbox.send(ServiceOutput::Call(reply(
                                &ctx.addr,
                                &call.source,
                                AgentSessionResponse::Context {
                                    query_id,
                                    context: state.context(&launch, pending_launch.as_ref(), current_plan.clone()),
                                },
                            )?))?;
                        }
                        Ok(AgentSessionRequest::QueryMessages {
                            request_id,
                            offset,
                            limit,
                        }) => {
                            let history = read_agent_session_history(
                                &launch.conversation_root,
                                &launch.session_id,
                                request_id,
                                offset,
                                limit,
                            )?;
                            ctx.outbox.send(ServiceOutput::Call(
                                crate::conversation_new::ServiceCall::response_to(
                                    ctx.addr.clone(),
                                    call.source,
                                    encode_response(AgentSessionResponse::MessageHistory {
                                        history,
                                    })?,
                                    call.request_id.clone(),
                                ),
                            ))?;
                        }
                        Ok(AgentSessionRequest::QueryMessageDetail {
                            request_id,
                            message_id,
                        }) => {
                            let record = read_agent_session_message_detail(
                                &launch.conversation_root,
                                &launch.session_id,
                                &message_id,
                            )?;
                            ctx.outbox.send(ServiceOutput::Call(
                                crate::conversation_new::ServiceCall::response_to(
                                    ctx.addr.clone(),
                                    call.source,
                                    encode_response(AgentSessionResponse::MessageDetail {
                                        request_id,
                                        record,
                                    })?,
                                    call.request_id.clone(),
                                ),
                            ))?;
                        }
                        Ok(AgentSessionRequest::QueryStatus) => {
                            ctx.outbox.send(ServiceOutput::Call(reply(
                                &ctx.addr,
                                &call.source,
                                AgentSessionResponse::Status {
                                    status: state.status(current_plan.clone()),
                                },
                            )?))?;
                        }
                        Ok(AgentSessionRequest::UpdateLaunchConfig { launch: next_launch }) => {
                            if state.state == AgentSessionState::Idle {
                                restart_runner_for_launch(
                                    &ctx,
                                    &self.kind,
                                    &mut launch,
                                    next_launch,
                                    &mut runner,
                                    &mut session_event_rx,
                                    "runtime_config_updated",
                                )?;
                            } else {
                                pending_launch = Some(next_launch);
                                ctx.outbox.send(ServiceOutput::Status(ServiceStatusUpdate {
                                    addr: ctx.addr.clone(),
                                    label: "launch_config_update_pending".to_string(),
                                    detail: serde_json::json!({
                                        "state": state.state,
                                        "active_turn_id": state.active_turn_id,
                                    }),
                                }))?;
                            }
                        }
                        Ok(AgentSessionRequest::Shutdown { reason }) => {
                            state.state = AgentSessionState::Stopping;
                            state.persist(&ctx.storage)?;
                            if let Some(runner) = runner.take() {
                                runner.shutdown();
                            }
                            ctx.outbox.send(ServiceOutput::Call(reply(
                                &ctx.addr,
                                &call.source,
                                AgentSessionResponse::Stopped,
                            )?))?;
                            ctx.outbox.send(ServiceOutput::Stopped(ServiceStopped {
                                addr: ctx.addr.clone(),
                                reason: reason.or_else(|| Some("agent session shutdown request".to_string())),
                            }))?;
                            return Ok(());
                        }
                        Err(error) => {
                            if handle_service_response(
                                &ctx,
                                &mut runner,
                                &mut pending_memory_requests,
                                &mut pending_skill_requests,
                                &mut pending_tool_binary_requests,
                                &mut pending_cron_requests,
                                &mut pending_child_starts,
                                &mut pending_service_responses,
                                &mut state,
                                call.response_id.as_deref(),
                                call.payload.clone(),
                            )
                            .with_context(|| {
                                format!("failed to handle service response from {}", call.source)
                            })?
                            {
                                continue;
                            } else {
                                state.state = AgentSessionState::Crashed;
                                state.last_error = Some(error.to_string());
                                state.persist(&ctx.storage)?;
                                emit_bad_payload_error(&ctx, &event_sink, error.to_string())?;
                            }
                        }
                    }
                }
            }
        }
    }
}

struct RealAgentSessionRuntime {
    client: AgentServerClient,
    events: crossbeam_channel::Receiver<CoreSessionEvent>,
    event_forwarder: Option<JoinHandle<()>>,
}

fn restart_runner_for_launch(
    ctx: &ServiceRunContext,
    kind: &AgentSessionKind,
    launch: &mut AgentSessionLaunchConfig,
    next_launch: AgentSessionLaunchConfig,
    runner: &mut Option<RealAgentSessionRuntime>,
    session_event_rx: &mut crossbeam_channel::Receiver<CoreSessionEvent>,
    reason: &str,
) -> Result<()> {
    if let Some(current) = runner.take() {
        current.shutdown();
    }
    *launch = next_launch;
    *runner = start_real_session(launch, kind)?;
    *session_event_rx = runner
        .as_ref()
        .map(|runner| runner.events.clone())
        .unwrap_or_else(crossbeam_channel::never);
    ctx.outbox.send(ServiceOutput::Status(ServiceStatusUpdate {
        addr: ctx.addr.clone(),
        label: "launch_config_applied".to_string(),
        detail: serde_json::json!({
            "reason": reason,
            "agent_server_configured": launch.agent_server_path.is_some(),
            "has_session_profile": launch.session_profile.is_some(),
            "model_count": launch.models.len(),
            "memory_enabled": launch.memory_enabled,
            "tool_remote_mode": launch.tool_remote_mode,
            "has_sandbox_override": launch.sandbox.is_some(),
            "reasoning_effort": launch.reasoning_effort,
            "runner": if runner.is_some() { "agent_server" } else { "skeleton" },
        }),
    }))?;
    Ok(())
}

fn maybe_apply_pending_launch(
    ctx: &ServiceRunContext,
    kind: &AgentSessionKind,
    launch: &mut AgentSessionLaunchConfig,
    pending_launch: &mut Option<AgentSessionLaunchConfig>,
    runner: &mut Option<RealAgentSessionRuntime>,
    session_event_rx: &mut crossbeam_channel::Receiver<CoreSessionEvent>,
    state: &AgentSessionRuntimeState,
) -> Result<()> {
    if state.state != AgentSessionState::Idle {
        return Ok(());
    }
    let Some(next_launch) = pending_launch.take() else {
        return Ok(());
    };
    restart_runner_for_launch(
        ctx,
        kind,
        launch,
        next_launch,
        runner,
        session_event_rx,
        "idle_boundary",
    )
}

impl RealAgentSessionRuntime {
    fn send(&mut self, request: AgentSessionRequest) -> Result<()> {
        let request = to_core_session_request(request).map_err(anyhow::Error::msg)?;
        self.client
            .send_session_request(&request)
            .map_err(anyhow::Error::msg)
    }

    fn shutdown(mut self) {
        let _ = self
            .client
            .send_session_request(&CoreSessionRequest::Shutdown);
        let _ = self.client.shutdown();
        if let Some(handle) = self.event_forwarder.take() {
            let _ = handle.join();
        }
    }
}

fn start_real_session(
    launch: &AgentSessionLaunchConfig,
    kind: &AgentSessionKind,
) -> Result<Option<RealAgentSessionRuntime>> {
    let Some(agent_server_path) = launch.agent_server_path.as_deref() else {
        return Ok(None);
    };
    let model_config = resolve_session_model(launch)?.ok_or_else(|| {
        anyhow!("agent_server_path is configured but no chat-capable model is available")
    })?;
    let sandbox = launch.sandbox.clone().unwrap_or_default();
    let (client, std_events) = AgentServerClient::spawn(
        agent_server_path,
        &launch.workspace_root,
        &launch.conversation_root,
        &sandbox,
    )
    .map_err(anyhow::Error::msg)?;

    let mut initial = SessionInitial::new(launch.session_id.clone(), session_type(kind));
    initial.tool_remote_mode = launch.tool_remote_mode.clone();
    initial.remote_workspace_instructions = remote_workspace_instructions(&launch.tool_remote_mode);
    initial.compression_threshold_tokens = launch.session_defaults.compression_threshold_tokens;
    initial.compression_retain_recent_tokens =
        launch.session_defaults.compression_retain_recent_tokens;
    initial.memory_enabled = launch.memory_enabled;

    let effective_model = effective_model_config(&model_config, launch.reasoning_effort.as_deref());
    initial.idle_timeout_compact_enabled = launch
        .idle_timeout_compact_enabled
        .unwrap_or(effective_model.idle_timeout_compact_enabled);
    initial.image_tool_model = resolve_tool_model_target(
        "image_tool_model",
        launch.session_defaults.image_tool_model.as_ref(),
        &launch.models,
        &effective_model,
    )?;
    initial.pdf_tool_model = resolve_tool_model_target(
        "pdf_tool_model",
        launch.session_defaults.pdf_tool_model.as_ref(),
        &launch.models,
        &effective_model,
    )?;
    initial.audio_tool_model = resolve_tool_model_target(
        "audio_tool_model",
        launch.session_defaults.audio_tool_model.as_ref(),
        &launch.models,
        &effective_model,
    )?;
    initial.image_generation_tool_model = resolve_tool_model_target(
        "image_generation_tool_model",
        launch.session_defaults.image_generation_tool_model.as_ref(),
        &launch.models,
        &effective_model,
    )?;
    initial.search_tool_model = resolve_tool_model_target_with_capability(
        "search_tool_model",
        launch.session_defaults.search_tool_model.as_ref(),
        &launch.models,
        &effective_model,
        ModelCapability::WebSearch,
    )?;
    initial.search_image_tool_model = resolve_tool_model_target_with_capability(
        "search_image_tool_model",
        launch.session_defaults.search_image_tool_model.as_ref(),
        &launch.models,
        &effective_model,
        ModelCapability::WebSearch,
    )?;
    initial.search_video_tool_model = resolve_tool_model_target_with_capability(
        "search_video_tool_model",
        launch.session_defaults.search_video_tool_model.as_ref(),
        &launch.models,
        &effective_model,
        ModelCapability::WebSearch,
    )?;
    initial.search_news_tool_model = resolve_tool_model_target_with_capability(
        "search_news_tool_model",
        launch.session_defaults.search_news_tool_model.as_ref(),
        &launch.models,
        &effective_model,
        ModelCapability::WebSearch,
    )?;

    if let Err(error) = client.initialize(&effective_model, &initial) {
        let _ = client.shutdown();
        return Err(anyhow!(error));
    }

    let (event_tx, event_rx) = crossbeam_channel::unbounded();
    let event_forwarder = thread::Builder::new()
        .name(format!("agent-session-events-{}", launch.session_id))
        .spawn(move || {
            while let Ok(event) = std_events.recv() {
                if event_tx.send(event).is_err() {
                    break;
                }
            }
        })
        .context("failed to spawn agent session event forwarder")?;

    Ok(Some(RealAgentSessionRuntime {
        client,
        events: event_rx,
        event_forwarder: Some(event_forwarder),
    }))
}

fn resolve_session_model(launch: &AgentSessionLaunchConfig) -> Result<Option<ModelConfig>> {
    let model = launch
        .session_profile
        .as_ref()
        .and_then(|profile| profile.main_model.resolve(&launch.models))
        .or_else(|| {
            launch
                .models
                .values()
                .find(|model| model.supports(ModelCapability::Chat))
                .cloned()
        });
    if let Some(model) = model.as_ref() {
        if !model.supports(ModelCapability::Chat) {
            return Err(anyhow!(
                "agent session model {} is not chat-capable",
                model.model_name
            ));
        }
    }
    Ok(model)
}

fn session_type(kind: &AgentSessionKind) -> SessionType {
    match kind {
        AgentSessionKind::Foreground => SessionType::Foreground,
        AgentSessionKind::Background => SessionType::Background,
        AgentSessionKind::Subagent => SessionType::Subagent,
    }
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
        if !model.supports(capability) {
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

fn remote_workspace_instructions(remote_mode: &ToolRemoteMode) -> Option<String> {
    let ToolRemoteMode::FixedSsh { host, cwd } = remote_mode else {
        return None;
    };
    let Some(cwd) = cwd
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
    else {
        return Some(
            "[Remote AGENTS.md Notice]\nCould not read remote AGENTS.md: fixed SSH workspace path is empty."
                .to_string(),
        );
    };
    match read_remote_agents_md(host, cwd) {
        RemoteAgentsRead::Found { content, truncated } => {
            let mut section = format!(
                "[Remote AGENTS.md]\nThese scoped repository instructions were read from the fixed SSH workspace root `{}:{}/AGENTS.md`:\n{}",
                host, cwd, content
            );
            if truncated {
                section.push_str("\n\n[Remote AGENTS.md Notice]\nThe file was truncated to the first 65536 bytes.");
            }
            Some(section)
        }
        RemoteAgentsRead::Missing => None,
        RemoteAgentsRead::Failed(reason) => Some(format!(
            "[Remote AGENTS.md Notice]\nCould not read remote AGENTS.md from fixed SSH workspace `{}:{}/AGENTS.md`: {}",
            host, cwd, reason
        )),
    }
}

const REMOTE_AGENTS_MAX_BYTES: usize = 64 * 1024;

enum RemoteAgentsRead {
    Found { content: String, truncated: bool },
    Missing,
    Failed(String),
}

fn read_remote_agents_md(host: &str, cwd: &str) -> RemoteAgentsRead {
    let script = format!(
        "cd {} && if [ -f AGENTS.md ]; then bytes=$(wc -c < AGENTS.md | tr -d ' '); head -c {} AGENTS.md; if [ \"${{bytes:-0}}\" -gt {} ]; then exit 4; fi; elif [ -e AGENTS.md ]; then echo 'AGENTS.md exists but is not a regular file' >&2; exit 2; else exit 3; fi",
        shell_quote(cwd),
        REMOTE_AGENTS_MAX_BYTES,
        REMOTE_AGENTS_MAX_BYTES,
    );
    let output = Command::new("ssh")
        .arg("-o")
        .arg("BatchMode=yes")
        .arg("-o")
        .arg("ConnectTimeout=10")
        .arg("-T")
        .arg(host)
        .arg(script)
        .output();
    let output = match output {
        Ok(output) => output,
        Err(error) => return RemoteAgentsRead::Failed(format!("failed to run ssh: {error}")),
    };
    match output.status.code() {
        Some(0) | Some(4) => {
            let mut content = String::from_utf8_lossy(&output.stdout).to_string();
            if !content.ends_with('\n') {
                content.push('\n');
            }
            RemoteAgentsRead::Found {
                content,
                truncated: output.status.code() == Some(4),
            }
        }
        Some(3) => RemoteAgentsRead::Missing,
        _ => {
            let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
            let message = if stderr.is_empty() {
                format!(
                    "ssh exited with status {}",
                    output.status.code().unwrap_or(-1)
                )
            } else {
                stderr
            };
            RemoteAgentsRead::Failed(message)
        }
    }
}

fn shell_quote(value: &str) -> String {
    if value.is_empty() {
        return "''".to_string();
    }
    let escaped = value.replace('\'', "'\"'\"'");
    format!("'{escaped}'")
}

fn handle_skeleton_enqueue(
    ctx: &ServiceRunContext,
    event_sink: &crate::conversation_new::ServiceAddr,
    kind: &AgentSessionKind,
    state: &mut AgentSessionRuntimeState,
    origin: AgentMessageOrigin,
    message: ChatMessage,
    ingress_id: Option<String>,
) -> Result<()> {
    let index = state.append_message(message.clone());
    state.persist(&ctx.storage)?;
    ctx.outbox.send(ServiceOutput::Status(ServiceStatusUpdate {
        addr: ctx.addr.clone(),
        label: "message_received".to_string(),
        detail: serde_json::json!({
            "origin": origin,
            "ingress_id": ingress_id,
            "index": index,
            "runner": "skeleton",
        }),
    }))?;
    if matches!(origin, AgentMessageOrigin::User) {
        emit_session_event(
            ctx,
            event_sink,
            AgentSessionEvent::UserMessageStarted {
                origin: origin.clone(),
                ingress_id: ingress_id.clone(),
                message: message.clone(),
            },
        )?;
    }
    emit_session_event(
        ctx,
        event_sink,
        AgentSessionEvent::MessageAppended {
            index,
            message: message.clone(),
        },
    )?;
    if matches!(origin, AgentMessageOrigin::User) {
        emit_session_event(
            ctx,
            event_sink,
            AgentSessionEvent::UserMessageCommitted { index, message },
        )?;
    }
    if should_start_turn(kind, &origin) {
        let turn_id = format!("turn_{}", state.message_count);
        state.state = AgentSessionState::Running;
        state.active_turn_id = Some(turn_id.clone());
        state.persist(&ctx.storage)?;
        emit_session_event(
            ctx,
            event_sink,
            AgentSessionEvent::TurnStarted {
                turn_id: turn_id.clone(),
                plan: None,
            },
        )?;
        let response = text_message(
            ChatRole::Assistant,
            format!("agent {:?} accepted message", kind),
        );
        state.state = AgentSessionState::Idle;
        state.active_turn_id = None;
        state.persist(&ctx.storage)?;
        emit_session_event(
            ctx,
            event_sink,
            AgentSessionEvent::TurnCompleted { message: response },
        )?;
    }
    Ok(())
}

fn handle_core_session_event(
    ctx: &ServiceRunContext,
    event_sink: &crate::conversation_new::ServiceAddr,
    kind: &AgentSessionKind,
    runner: &mut Option<RealAgentSessionRuntime>,
    pending_memory_requests: &mut VecDeque<PendingBridgeRequest>,
    pending_skill_requests: &mut VecDeque<PendingBridgeRequest>,
    pending_tool_binary_requests: &mut VecDeque<PendingBridgeRequest>,
    pending_cron_requests: &mut VecDeque<PendingBridgeRequest>,
    pending_child_starts: &mut VecDeque<PendingChildAgentStart>,
    pending_service_responses: &mut BTreeMap<String, PendingServiceResponseKind>,
    pending_subagent_joins: &mut VecDeque<PendingSubagentJoin>,
    state: &mut AgentSessionRuntimeState,
    current_plan: &mut Option<TaskPlanView>,
    event: &CoreSessionEvent,
) -> Result<()> {
    if let CoreSessionEvent::HostCoordinationRequested { request } = event {
        if let Some(response) = update_plan_bridge_response(ctx, event_sink, current_plan, request)?
        {
            if let Some(runner) = runner.as_mut() {
                runner.send(AgentSessionRequest::ResolveHostCoordination {
                    response: serde_json::to_value(&response)?,
                })?;
            }
            return Ok(());
        }
        if let Some(response) = terminate_bridge_response(ctx, kind, request, state)? {
            if let Some(runner) = runner.as_mut() {
                runner.send(AgentSessionRequest::ResolveHostCoordination {
                    response: serde_json::to_value(&response)?,
                })?;
            }
            return Ok(());
        }
        if let Some(call) = memory_bridge_call(ctx, kind, request)? {
            let (call, pending) = track_service_request(
                call,
                request,
                PendingServiceResponseKind::Memory,
                pending_service_responses,
            );
            pending_memory_requests.push_back(pending);
            ctx.outbox.send(ServiceOutput::Call(call))?;
            ctx.outbox.send(ServiceOutput::Status(ServiceStatusUpdate {
                addr: ctx.addr.clone(),
                label: "memory_bridge_requested".to_string(),
                detail: serde_json::json!({
                    "request_id": request.request_id,
                    "action": request.action,
                    "runner": if runner.is_some() { "agent_server" } else { "skeleton" },
                }),
            }))?;
            return Ok(());
        }
        if let Some(call) = skill_bridge_call(ctx, request)? {
            let (call, pending) = track_service_request(
                call,
                request,
                PendingServiceResponseKind::Skill,
                pending_service_responses,
            );
            pending_skill_requests.push_back(pending);
            ctx.outbox.send(ServiceOutput::Call(call))?;
            ctx.outbox.send(ServiceOutput::Status(ServiceStatusUpdate {
                addr: ctx.addr.clone(),
                label: "skill_bridge_requested".to_string(),
                detail: serde_json::json!({
                    "request_id": request.request_id,
                    "action": request.action,
                    "runner": if runner.is_some() { "agent_server" } else { "skeleton" },
                }),
            }))?;
            return Ok(());
        }
        if let Some(call) = tool_binary_bridge_call(ctx, request)? {
            let (call, pending) = track_service_request(
                call,
                request,
                PendingServiceResponseKind::ToolBinary,
                pending_service_responses,
            );
            pending_tool_binary_requests.push_back(pending);
            ctx.outbox.send(ServiceOutput::Call(call))?;
            ctx.outbox.send(ServiceOutput::Status(ServiceStatusUpdate {
                addr: ctx.addr.clone(),
                label: "tool_binary_bridge_requested".to_string(),
                detail: serde_json::json!({
                    "request_id": request.request_id,
                    "action": request.action,
                    "runner": if runner.is_some() { "agent_server" } else { "skeleton" },
                }),
            }))?;
            return Ok(());
        }
        if let Some(call) = cron_bridge_call(ctx, request, state)? {
            let (call, pending) = track_service_request(
                call,
                request,
                PendingServiceResponseKind::Cron,
                pending_service_responses,
            );
            pending_cron_requests.push_back(pending);
            ctx.outbox.send(ServiceOutput::Call(call))?;
            ctx.outbox.send(ServiceOutput::Status(ServiceStatusUpdate {
                addr: ctx.addr.clone(),
                label: "cron_bridge_requested".to_string(),
                detail: serde_json::json!({
                    "request_id": request.request_id,
                    "action": request.action,
                    "runner": if runner.is_some() { "agent_server" } else { "skeleton" },
                }),
            }))?;
            return Ok(());
        }
        if let Some(response) = managed_agent_bridge_response(request, state)? {
            if let Some(runner) = runner.as_mut() {
                runner.send(AgentSessionRequest::ResolveHostCoordination {
                    response: serde_json::to_value(&response)?,
                })?;
            }
            return Ok(());
        }
        if let Some((call, pending)) = child_agent_start_bridge_call(ctx, request, state)? {
            let (call, pending) = track_child_start_request(
                call,
                pending,
                PendingServiceResponseKind::Kernel,
                pending_service_responses,
            );
            pending_child_starts.push_back(pending);
            ctx.outbox.send(ServiceOutput::Call(call))?;
            ctx.outbox.send(ServiceOutput::Status(ServiceStatusUpdate {
                addr: ctx.addr.clone(),
                label: "subagent_start_requested".to_string(),
                detail: serde_json::json!({
                    "request_id": request.request_id,
                    "action": request.action,
                    "runner": if runner.is_some() { "agent_server" } else { "skeleton" },
                }),
            }))?;
            return Ok(());
        }
        if let Some(response) =
            subagent_control_bridge_response(ctx, runner, request, state, pending_subagent_joins)?
        {
            if let Some(runner) = runner.as_mut() {
                runner.send(AgentSessionRequest::ResolveHostCoordination {
                    response: serde_json::to_value(&response)?,
                })?;
            }
            return Ok(());
        } else if matches!(request.action.as_str(), "subagent_join") {
            return Ok(());
        }
    }
    apply_core_session_event(state, current_plan, event);
    state.persist(&ctx.storage)?;
    match event {
        CoreSessionEvent::Progress {
            plan: Some(plan), ..
        } => emit_session_event(
            ctx,
            event_sink,
            AgentSessionEvent::PlanUpdated {
                plan: Some(plan.clone()),
            },
        ),
        CoreSessionEvent::Progress { .. } => Ok(()),
        CoreSessionEvent::MessageAppended { index, message }
            if matches!(message.role, ChatRole::User) =>
        {
            emit_session_event(ctx, event_sink, from_core_session_event(event.clone()))?;
            emit_session_event(
                ctx,
                event_sink,
                AgentSessionEvent::UserMessageCommitted {
                    index: *index,
                    message: message.clone(),
                },
            )
        }
        _ => emit_session_event(ctx, event_sink, from_core_session_event(event.clone())),
    }
}

fn update_plan_bridge_response(
    ctx: &ServiceRunContext,
    event_sink: &crate::conversation_new::ServiceAddr,
    current_plan: &mut Option<TaskPlanView>,
    request: &ConversationBridgeRequest,
) -> Result<Option<ConversationBridgeResponse>> {
    if request.action != "update_plan" {
        return Ok(None);
    }
    let payload = match parse_task_plan_view(request.payload.clone()) {
        Ok(plan) => {
            *current_plan = Some(plan.clone());
            emit_session_event(
                ctx,
                event_sink,
                AgentSessionEvent::PlanUpdated {
                    plan: Some(plan.clone()),
                },
            )?;
            ctx.outbox.send(ServiceOutput::Status(ServiceStatusUpdate {
                addr: ctx.addr.clone(),
                label: "plan_updated".to_string(),
                detail: serde_json::json!({ "plan": plan }),
            }))?;
            serde_json::json!({"updated": true})
        }
        Err(error) => serde_json::json!({
            "updated": false,
            "error": error,
        }),
    };
    Ok(Some(bridge_json_response(request.clone(), payload)))
}

fn parse_task_plan_view(payload: serde_json::Value) -> Result<TaskPlanView, String> {
    let mut plan: TaskPlanView = serde_json::from_value(payload)
        .map_err(|error| format!("failed to parse update_plan payload: {error}"))?;
    plan.explanation = plan
        .explanation
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty());
    for item in &mut plan.plan {
        item.step = item.step.trim().to_string();
        if item.step.is_empty() {
            return Err("update_plan step must not be empty".to_string());
        }
    }
    let in_progress_count = plan
        .plan
        .iter()
        .filter(|item| matches!(item.status, TaskPlanItemStatus::InProgress))
        .count();
    if in_progress_count > 1 {
        return Err("update_plan may include at most one in_progress step".to_string());
    }
    Ok(plan)
}

fn terminate_bridge_response(
    ctx: &ServiceRunContext,
    kind: &AgentSessionKind,
    request: &ConversationBridgeRequest,
    state: &AgentSessionRuntimeState,
) -> Result<Option<ConversationBridgeResponse>> {
    if request.action != "terminate" {
        return Ok(None);
    }
    if *kind != AgentSessionKind::Background {
        return Ok(Some(bridge_json_response(
            request.clone(),
            serde_json::json!({
                "status": "failure",
                "reason": "terminate is only available to background agents",
            }),
        )));
    }
    let Some(parent_addr) = state.binding.parent_addr.clone() else {
        return Ok(Some(bridge_json_response(
            request.clone(),
            serde_json::json!({
                "status": "failure",
                "reason": "background agent has no parent session",
            }),
        )));
    };
    let reason = "background_agent_terminate".to_string();
    ctx.outbox.send(ServiceOutput::Call(
        agent_session::child_session_event_call(
            ctx.addr.clone(),
            parent_addr,
            AgentSessionEvent::Terminated {
                reason: Some(reason.clone()),
            },
        )?,
    ))?;
    ctx.outbox
        .send(ServiceOutput::Call(agent_session::shutdown_call(
            ctx.addr.clone(),
            ctx.addr.clone(),
            Some(reason),
        )?))?;
    Ok(Some(bridge_json_response(
        request.clone(),
        serde_json::json!({"status": "terminating"}),
    )))
}

static NEXT_AGENT_SESSION_SERVICE_REQUEST_ID: AtomicU64 = AtomicU64::new(1);

fn next_agent_session_service_request_id(kind: PendingServiceResponseKind) -> String {
    let index = NEXT_AGENT_SESSION_SERVICE_REQUEST_ID.fetch_add(1, Ordering::Relaxed);
    format!("agent_session_{}_{index}", kind.label())
}

fn track_service_request(
    call: ServiceCall,
    request: &ConversationBridgeRequest,
    kind: PendingServiceResponseKind,
    pending_service_responses: &mut BTreeMap<String, PendingServiceResponseKind>,
) -> (ServiceCall, PendingBridgeRequest) {
    let service_request_id = next_agent_session_service_request_id(kind);
    pending_service_responses.insert(service_request_id.clone(), kind);
    (
        call.with_request_id(service_request_id.clone()),
        PendingBridgeRequest {
            service_request_id,
            request: request.clone(),
        },
    )
}

fn track_child_start_request(
    call: ServiceCall,
    mut pending: PendingChildAgentStart,
    kind: PendingServiceResponseKind,
    pending_service_responses: &mut BTreeMap<String, PendingServiceResponseKind>,
) -> (ServiceCall, PendingChildAgentStart) {
    let service_request_id = next_agent_session_service_request_id(kind);
    pending_service_responses.insert(service_request_id.clone(), kind);
    pending.service_request_id = service_request_id.clone();
    (call.with_request_id(service_request_id), pending)
}

fn pop_pending_bridge_request(
    pending: &mut VecDeque<PendingBridgeRequest>,
    response_id: Option<&str>,
) -> Option<ConversationBridgeRequest> {
    if let Some(response_id) = response_id {
        if let Some(index) = pending
            .iter()
            .position(|request| request.service_request_id == response_id)
        {
            return pending.remove(index).map(|pending| pending.request);
        }
    }
    pending.pop_front().map(|pending| pending.request)
}

fn pop_pending_child_start(
    pending: &mut VecDeque<PendingChildAgentStart>,
    response_id: Option<&str>,
) -> Option<PendingChildAgentStart> {
    if let Some(response_id) = response_id {
        if let Some(index) = pending
            .iter()
            .position(|start| start.service_request_id == response_id)
        {
            return pending.remove(index);
        }
    }
    pending.pop_front()
}

fn handle_service_response(
    ctx: &ServiceRunContext,
    runner: &mut Option<RealAgentSessionRuntime>,
    pending_memory_requests: &mut VecDeque<PendingBridgeRequest>,
    pending_skill_requests: &mut VecDeque<PendingBridgeRequest>,
    pending_tool_binary_requests: &mut VecDeque<PendingBridgeRequest>,
    pending_cron_requests: &mut VecDeque<PendingBridgeRequest>,
    pending_child_starts: &mut VecDeque<PendingChildAgentStart>,
    pending_service_responses: &mut BTreeMap<String, PendingServiceResponseKind>,
    state: &mut AgentSessionRuntimeState,
    response_id: Option<&str>,
    payload: Value,
) -> Result<bool> {
    if let Some(response_id) = response_id {
        if let Some(kind) = pending_service_responses.get(response_id).copied() {
            match kind {
                PendingServiceResponseKind::Memory => {
                    let response = memory::decode_response(payload)
                        .context("failed to decode memory response")?;
                    handle_memory_response(
                        ctx,
                        runner,
                        pending_memory_requests,
                        Some(response_id),
                        response,
                    )?;
                }
                PendingServiceResponseKind::Skill => {
                    let response = skill::decode_response(payload)
                        .context("failed to decode skill response")?;
                    handle_skill_response(
                        ctx,
                        runner,
                        pending_skill_requests,
                        Some(response_id),
                        response,
                    )?;
                }
                PendingServiceResponseKind::ToolBinary => {
                    let response = tool_binary::decode_response(payload)
                        .context("failed to decode tool binary response")?;
                    handle_tool_binary_response(
                        ctx,
                        runner,
                        pending_tool_binary_requests,
                        Some(response_id),
                        response,
                    )?;
                }
                PendingServiceResponseKind::Cron => {
                    let response =
                        cron::decode_response(payload).context("failed to decode cron response")?;
                    handle_cron_response(
                        ctx,
                        runner,
                        pending_cron_requests,
                        Some(response_id),
                        response,
                    )?;
                }
                PendingServiceResponseKind::Kernel => {
                    let response = kernel::decode_response(payload)
                        .context("failed to decode kernel response")?;
                    handle_kernel_response(
                        ctx,
                        runner,
                        pending_child_starts,
                        state,
                        Some(response_id),
                        response,
                    )?;
                }
            }
            pending_service_responses.remove(response_id);
            return Ok(true);
        }
    }

    if let Ok(response) = memory::decode_response(payload.clone()) {
        handle_memory_response(ctx, runner, pending_memory_requests, None, response)?;
        Ok(true)
    } else if let Ok(response) = skill::decode_response(payload.clone()) {
        handle_skill_response(ctx, runner, pending_skill_requests, None, response)?;
        Ok(true)
    } else if let Ok(response) = tool_binary::decode_response(payload.clone()) {
        handle_tool_binary_response(ctx, runner, pending_tool_binary_requests, None, response)?;
        Ok(true)
    } else if let Ok(response) = cron::decode_response(payload.clone()) {
        handle_cron_response(ctx, runner, pending_cron_requests, None, response)?;
        Ok(true)
    } else if let Ok(response) = kernel::decode_response(payload) {
        handle_kernel_response(ctx, runner, pending_child_starts, state, None, response)?;
        Ok(true)
    } else {
        Ok(false)
    }
}

fn handle_memory_response(
    ctx: &ServiceRunContext,
    runner: &mut Option<RealAgentSessionRuntime>,
    pending_memory_requests: &mut VecDeque<PendingBridgeRequest>,
    response_id: Option<&str>,
    response: MemoryResponse,
) -> Result<()> {
    let Some(request) = pop_pending_bridge_request(pending_memory_requests, response_id) else {
        ctx.outbox.send(ServiceOutput::Status(ServiceStatusUpdate {
            addr: ctx.addr.clone(),
            label: "unexpected_memory_response".to_string(),
            detail: serde_json::to_value(response)?,
        }))?;
        return Ok(());
    };
    let bridge_response = memory_bridge_response(request, response)?;
    if let Some(runner) = runner.as_mut() {
        runner.send(AgentSessionRequest::ResolveHostCoordination {
            response: serde_json::to_value(&bridge_response)?,
        })?;
    }
    ctx.outbox.send(ServiceOutput::Status(ServiceStatusUpdate {
        addr: ctx.addr.clone(),
        label: "memory_bridge_resolved".to_string(),
        detail: serde_json::json!({
            "request_id": bridge_response.request_id,
            "tool_name": bridge_response.tool_name,
        }),
    }))?;
    Ok(())
}

fn handle_skill_response(
    ctx: &ServiceRunContext,
    runner: &mut Option<RealAgentSessionRuntime>,
    pending_skill_requests: &mut VecDeque<PendingBridgeRequest>,
    response_id: Option<&str>,
    response: SkillResponse,
) -> Result<()> {
    let Some(request) = pop_pending_bridge_request(pending_skill_requests, response_id) else {
        ctx.outbox.send(ServiceOutput::Status(ServiceStatusUpdate {
            addr: ctx.addr.clone(),
            label: "unexpected_skill_response".to_string(),
            detail: serde_json::to_value(response)?,
        }))?;
        return Ok(());
    };
    let bridge_response = skill_bridge_response(request, response)?;
    if let Some(runner) = runner.as_mut() {
        runner.send(AgentSessionRequest::ResolveHostCoordination {
            response: serde_json::to_value(&bridge_response)?,
        })?;
    }
    ctx.outbox.send(ServiceOutput::Status(ServiceStatusUpdate {
        addr: ctx.addr.clone(),
        label: "skill_bridge_resolved".to_string(),
        detail: serde_json::json!({
            "request_id": bridge_response.request_id,
            "tool_name": bridge_response.tool_name,
        }),
    }))?;
    Ok(())
}

fn handle_tool_binary_response(
    ctx: &ServiceRunContext,
    runner: &mut Option<RealAgentSessionRuntime>,
    pending_tool_binary_requests: &mut VecDeque<PendingBridgeRequest>,
    response_id: Option<&str>,
    response: ToolBinaryResponse,
) -> Result<()> {
    let Some(request) = pop_pending_bridge_request(pending_tool_binary_requests, response_id)
    else {
        ctx.outbox.send(ServiceOutput::Status(ServiceStatusUpdate {
            addr: ctx.addr.clone(),
            label: "unexpected_tool_binary_response".to_string(),
            detail: serde_json::to_value(response)?,
        }))?;
        return Ok(());
    };
    let bridge_response = tool_binary_bridge_response(request, response)?;
    if let Some(runner) = runner.as_mut() {
        runner.send(AgentSessionRequest::ResolveHostCoordination {
            response: serde_json::to_value(&bridge_response)?,
        })?;
    }
    ctx.outbox.send(ServiceOutput::Status(ServiceStatusUpdate {
        addr: ctx.addr.clone(),
        label: "tool_binary_bridge_resolved".to_string(),
        detail: serde_json::json!({
            "request_id": bridge_response.request_id,
            "tool_name": bridge_response.tool_name,
        }),
    }))?;
    Ok(())
}

fn handle_cron_response(
    ctx: &ServiceRunContext,
    runner: &mut Option<RealAgentSessionRuntime>,
    pending_cron_requests: &mut VecDeque<PendingBridgeRequest>,
    response_id: Option<&str>,
    response: CronResponse,
) -> Result<()> {
    let Some(request) = pop_pending_bridge_request(pending_cron_requests, response_id) else {
        ctx.outbox.send(ServiceOutput::Status(ServiceStatusUpdate {
            addr: ctx.addr.clone(),
            label: "unexpected_cron_response".to_string(),
            detail: serde_json::to_value(response)?,
        }))?;
        return Ok(());
    };
    let bridge_response = bridge_json_response(request, cron_tool_payload(response));
    if let Some(runner) = runner.as_mut() {
        runner.send(AgentSessionRequest::ResolveHostCoordination {
            response: serde_json::to_value(&bridge_response)?,
        })?;
    }
    ctx.outbox.send(ServiceOutput::Status(ServiceStatusUpdate {
        addr: ctx.addr.clone(),
        label: "cron_bridge_resolved".to_string(),
        detail: serde_json::json!({
            "request_id": bridge_response.request_id,
            "tool_name": bridge_response.tool_name,
        }),
    }))?;
    Ok(())
}

fn handle_kernel_response(
    ctx: &ServiceRunContext,
    runner: &mut Option<RealAgentSessionRuntime>,
    pending_child_starts: &mut VecDeque<PendingChildAgentStart>,
    state: &mut AgentSessionRuntimeState,
    response_id: Option<&str>,
    response: KernelResponse,
) -> Result<()> {
    match response {
        KernelResponse::AgentSessionCreated { addr } => {
            let Some(pending) = pop_pending_child_start(pending_child_starts, response_id) else {
                ctx.outbox.send(ServiceOutput::Status(ServiceStatusUpdate {
                    addr: ctx.addr.clone(),
                    label: "unexpected_agent_session_created".to_string(),
                    detail: serde_json::json!({ "addr": addr }),
                }))?;
                return Ok(());
            };
            let record = ChildAgentRuntimeRecord {
                agent_id: pending.agent_id.clone(),
                addr: addr.clone(),
                status: ChildAgentRuntimeStatus::Running,
                task: pending.task.clone(),
                last_message: None,
                last_error: None,
            };
            match pending.kind {
                AgentSessionKind::Background => {
                    state
                        .background_agents
                        .insert(pending.agent_id.clone(), record);
                }
                AgentSessionKind::Subagent => {
                    state.subagents.insert(pending.agent_id.clone(), record);
                }
                AgentSessionKind::Foreground => {}
            }
            state.persist(&ctx.storage)?;
            ctx.outbox
                .send(ServiceOutput::Call(agent_session::enqueue_message_call(
                    ctx.addr.clone(),
                    addr.clone(),
                    AgentMessageOrigin::User,
                    text_message(ChatRole::User, pending.task.clone()),
                    Some(format!(
                        "{}_start:{}",
                        child_agent_kind_label(&pending.kind),
                        pending.agent_id
                    )),
                )?))?;
            let bridge_response = bridge_json_response(
                pending.request,
                serde_json::json!({
                    "agent_id": pending.agent_id,
                    "session_id": addr.to_string(),
                    "status": "started",
                    "task": pending.task,
                }),
            );
            if let Some(runner) = runner.as_mut() {
                runner.send(AgentSessionRequest::ResolveHostCoordination {
                    response: serde_json::to_value(&bridge_response)?,
                })?;
            }
            ctx.outbox.send(ServiceOutput::Status(ServiceStatusUpdate {
                addr: ctx.addr.clone(),
                label: format!("{}_started", child_agent_kind_label(&pending.kind)),
                detail: serde_json::json!({ "agent_addr": addr }),
            }))?;
        }
        KernelResponse::Error { code, message } => {
            let Some(pending) = pop_pending_child_start(pending_child_starts, response_id) else {
                ctx.outbox.send(ServiceOutput::Status(ServiceStatusUpdate {
                    addr: ctx.addr.clone(),
                    label: "unexpected_kernel_error".to_string(),
                    detail: serde_json::json!({ "code": code, "message": message }),
                }))?;
                return Ok(());
            };
            let bridge_response = bridge_json_response(
                pending.request,
                serde_json::json!({
                    "status": "failure",
                    "reason": message,
                    "code": code,
                }),
            );
            if let Some(runner) = runner.as_mut() {
                runner.send(AgentSessionRequest::ResolveHostCoordination {
                    response: serde_json::to_value(&bridge_response)?,
                })?;
            }
        }
        other => {
            ctx.outbox.send(ServiceOutput::Status(ServiceStatusUpdate {
                addr: ctx.addr.clone(),
                label: "unexpected_kernel_response".to_string(),
                detail: serde_json::to_value(other)?,
            }))?;
        }
    }
    Ok(())
}

fn handle_child_session_event(
    ctx: &ServiceRunContext,
    runner: &mut Option<RealAgentSessionRuntime>,
    pending_subagent_joins: &mut VecDeque<PendingSubagentJoin>,
    state: &mut AgentSessionRuntimeState,
    session_addr: crate::conversation_new::ServiceAddr,
    event: AgentSessionEvent,
) -> Result<()> {
    let mut completed_background_message = None;
    let mut failed_background_error = None;
    let mut terminal_child_shutdown = None;
    let mut handled = false;
    if let Some(record) = state
        .subagents
        .values_mut()
        .find(|record| record.addr == session_addr)
    {
        handled = true;
        match event {
            AgentSessionEvent::MessageAppended { message, .. } => {
                record.last_message = Some(message);
            }
            AgentSessionEvent::TurnStarted { .. } => {
                if record.status == ChildAgentRuntimeStatus::Running {
                    record.last_error = None;
                }
            }
            AgentSessionEvent::TurnCompleted { message } => {
                if record.status == ChildAgentRuntimeStatus::Running {
                    record.status = ChildAgentRuntimeStatus::Completed;
                    record.last_message = Some(message);
                    terminal_child_shutdown =
                        Some((record.addr.clone(), "subagent_completed".to_string()));
                }
            }
            AgentSessionEvent::TurnFailed { error, .. } => {
                if record.status == ChildAgentRuntimeStatus::Running {
                    record.status = ChildAgentRuntimeStatus::Failed;
                    record.last_error = Some(error);
                    terminal_child_shutdown =
                        Some((record.addr.clone(), "subagent_failed".to_string()));
                }
            }
            AgentSessionEvent::RuntimeCrashed { error, .. } => {
                if record.status == ChildAgentRuntimeStatus::Running {
                    record.status = ChildAgentRuntimeStatus::Failed;
                    record.last_error = Some(error);
                    terminal_child_shutdown =
                        Some((record.addr.clone(), "subagent_crashed".to_string()));
                }
            }
            AgentSessionEvent::Terminated { reason } => {
                if record.status == ChildAgentRuntimeStatus::Running {
                    record.status = ChildAgentRuntimeStatus::Killed;
                    record.last_error = reason;
                }
            }
            _ => {}
        }
    } else if let Some(record) = state
        .background_agents
        .values_mut()
        .find(|record| record.addr == session_addr)
    {
        handled = true;
        match event {
            AgentSessionEvent::MessageAppended { message, .. } => {
                record.last_message = Some(message);
            }
            AgentSessionEvent::TurnStarted { .. } => {
                if record.status == ChildAgentRuntimeStatus::Running {
                    record.last_error = None;
                }
            }
            AgentSessionEvent::TurnCompleted { message } => {
                if record.status == ChildAgentRuntimeStatus::Running {
                    record.status = ChildAgentRuntimeStatus::Completed;
                    record.last_message = Some(message.clone());
                    completed_background_message = Some(message);
                    terminal_child_shutdown = Some((
                        record.addr.clone(),
                        "background_agent_completed".to_string(),
                    ));
                }
            }
            AgentSessionEvent::TurnFailed { error, .. } => {
                if record.status == ChildAgentRuntimeStatus::Running {
                    record.status = ChildAgentRuntimeStatus::Failed;
                    record.last_error = Some(error.clone());
                    failed_background_error = Some(error);
                    terminal_child_shutdown =
                        Some((record.addr.clone(), "background_agent_failed".to_string()));
                }
            }
            AgentSessionEvent::RuntimeCrashed { error, .. } => {
                if record.status == ChildAgentRuntimeStatus::Running {
                    record.status = ChildAgentRuntimeStatus::Failed;
                    record.last_error = Some(error.clone());
                    failed_background_error = Some(error);
                    terminal_child_shutdown =
                        Some((record.addr.clone(), "background_agent_crashed".to_string()));
                }
            }
            AgentSessionEvent::Terminated { reason } => {
                if record.status == ChildAgentRuntimeStatus::Running {
                    record.status = ChildAgentRuntimeStatus::Killed;
                    record.last_error = reason;
                }
            }
            _ => {}
        }
    }
    if handled {
        state.persist(&ctx.storage)?;
        if let Some(message) = completed_background_message {
            ctx.outbox
                .send(ServiceOutput::Call(agent_session::enqueue_message_call(
                    ctx.addr.clone(),
                    ctx.addr.clone(),
                    AgentMessageOrigin::Actor,
                    background_result_message(message),
                    Some("background_agent_result".to_string()),
                )?))?;
        }
        if let Some(error) = failed_background_error {
            if state
                .binding
                .event_sink
                .local_service_id("channel")
                .is_some()
            {
                ctx.outbox.send(ServiceOutput::Call(channel::error_call(
                    ctx.addr.clone(),
                    state.binding.event_sink.clone(),
                    "background_agent_failed",
                    format!("后台任务失败: {error}"),
                    Some(error),
                )?))?;
            }
        }
        resolve_ready_subagent_joins(ctx, runner, pending_subagent_joins, state)?;
        if let Some((addr, reason)) = terminal_child_shutdown {
            ctx.outbox
                .send(ServiceOutput::Call(agent_session::shutdown_call(
                    ctx.addr.clone(),
                    addr,
                    Some(reason),
                )?))?;
        }
    } else {
        ctx.outbox.send(ServiceOutput::Status(ServiceStatusUpdate {
            addr: ctx.addr.clone(),
            label: "unknown_child_session_event".to_string(),
            detail: serde_json::json!({ "session_addr": session_addr }),
        }))?;
    }
    Ok(())
}

fn memory_bridge_call(
    ctx: &ServiceRunContext,
    kind: &AgentSessionKind,
    request: &ConversationBridgeRequest,
) -> Result<Option<ServiceCall>> {
    let source = Some(MemorySourceRef {
        conversation_id: ctx.conversation.conversation_id.clone(),
        agent_addr: Some(ctx.addr.clone()),
        session_type: session_type_name(kind).to_string(),
    });
    let memory_request = match request.action.as_str() {
        "memory_search" => {
            let payload: LegacyMemorySearchPayload =
                serde_json::from_value(request.payload.clone())
                    .context("failed to parse memory_search request")?;
            MemoryRequest::Search {
                source,
                query: payload.query,
                scopes: parse_memory_scopes(payload.scopes)?,
                limit: payload.limit,
            }
        }
        "memory_write" => {
            let payload: LegacyMemoryWritePayload = serde_json::from_value(request.payload.clone())
                .context("failed to parse memory_write request")?;
            MemoryRequest::Write {
                source,
                scope: parse_memory_scope(&payload.scope)?,
                subject: payload.subject,
                text: payload.text,
                tags: payload.tags,
            }
        }
        "memory_update" => {
            let payload: LegacyMemoryUpdatePayload =
                serde_json::from_value(request.payload.clone())
                    .context("failed to parse memory_update request")?;
            MemoryRequest::Update {
                source,
                memory_id: payload.memory_id,
                text: payload.text,
            }
        }
        "memory_delete" => {
            let payload: LegacyMemoryDeletePayload =
                serde_json::from_value(request.payload.clone())
                    .context("failed to parse memory_delete request")?;
            MemoryRequest::Delete {
                source,
                memory_id: payload.memory_id,
            }
        }
        _ => return Ok(None),
    };
    Ok(Some(memory::memory_call(ctx.addr.clone(), memory_request)?))
}

fn skill_bridge_call(
    ctx: &ServiceRunContext,
    request: &ConversationBridgeRequest,
) -> Result<Option<ServiceCall>> {
    if request.action == "skill_load" {
        let payload: LegacySkillLoadPayload = serde_json::from_value(request.payload.clone())
            .context("failed to parse skill_load request")?;
        return Ok(Some(skill::skill_call(
            ctx.addr.clone(),
            SkillRequest::Load {
                skill_name: payload.skill_name,
            },
        )?));
    }

    let mode = match request.action.as_str() {
        "skill_create" => SkillPersistMode::Create,
        "skill_update" => SkillPersistMode::Update,
        "skill_delete" => SkillPersistMode::Delete,
        _ => return Ok(None),
    };
    let payload: LegacySkillPersistPayload = serde_json::from_value(request.payload.clone())
        .with_context(|| format!("failed to parse {} request", request.action))?;
    Ok(Some(skill::skill_call(
        ctx.addr.clone(),
        SkillRequest::Persist {
            skill_name: payload.skill_name,
            mode,
        },
    )?))
}

fn tool_binary_bridge_call(
    ctx: &ServiceRunContext,
    request: &ConversationBridgeRequest,
) -> Result<Option<ServiceCall>> {
    if request.action != "tool_binary_ensure" {
        return Ok(None);
    }
    let payload: LegacyToolBinaryEnsurePayload = serde_json::from_value(request.payload.clone())
        .context("failed to parse tool_binary_ensure request")?;
    Ok(Some(tool_binary::tool_binary_call(
        ctx.addr.clone(),
        ToolBinaryRequest::Ensure {
            tool: payload.tool,
            host: payload.host,
        },
    )?))
}

fn cron_bridge_call(
    ctx: &ServiceRunContext,
    request: &ConversationBridgeRequest,
    state: &mut AgentSessionRuntimeState,
) -> Result<Option<ServiceCall>> {
    let cron_request = match request.action.as_str() {
        "cron_tasks_list" => CronRequest::ListTasks {
            owner: Some(ctx.addr.clone()),
        },
        "cron_task_get" => {
            let payload: LegacyCronIdPayload = serde_json::from_value(request.payload.clone())
                .context("failed to parse cron_task_get request")?;
            CronRequest::GetTaskStatus {
                task_id: payload.id,
                owner: Some(ctx.addr.clone()),
            }
        }
        "cron_task_create" => {
            let payload: LegacyCronCreatePayload = serde_json::from_value(request.payload.clone())
                .context("failed to parse cron_task_create request")?;
            let schedule = payload.schedule()?;
            let enabled = payload.enabled.unwrap_or(true);
            let prompt = payload.task.unwrap_or_default();
            if prompt.trim().is_empty() {
                return Err(anyhow!("cron_task_create requires task"));
            }
            let index = state.next_cron_index;
            state.next_cron_index = state.next_cron_index.saturating_add(1);
            let task_id = format!("cron_{index:04}");
            state.persist(&ctx.storage)?;
            CronRequest::RegisterTask {
                task: CronTaskRegistration {
                    task_id,
                    registered_by: ctx.addr.clone(),
                    channel_addr: state.binding.event_sink.clone(),
                    name: Some(payload.name),
                    description: Some(payload.description),
                    enabled,
                    foreground_session_addr: foreground_addr_for_child_agent(ctx, state),
                    schedule,
                    payload: CronTaskPayload::Prompt {
                        prompt,
                        output_policy: CronTaskOutputPolicy::ForwardResultToForeground,
                    },
                },
            }
        }
        "cron_task_update" => {
            let payload: LegacyCronUpdatePayload = serde_json::from_value(request.payload.clone())
                .context("failed to parse cron_task_update request")?;
            let task_id = payload.id.clone();
            let patch = payload.patch()?;
            CronRequest::UpdateTask { task_id, patch }
        }
        "cron_task_remove" => {
            let payload: LegacyCronIdPayload = serde_json::from_value(request.payload.clone())
                .context("failed to parse cron_task_remove request")?;
            CronRequest::RemoveTask {
                task_id: payload.id,
            }
        }
        _ => return Ok(None),
    };
    Ok(Some(ServiceCall::new(
        ctx.addr.clone(),
        ServiceAddr::cron(),
        cron::encode_request(cron_request)?,
    )))
}

fn managed_agent_bridge_response(
    request: &ConversationBridgeRequest,
    state: &AgentSessionRuntimeState,
) -> Result<Option<ConversationBridgeResponse>> {
    match request.action.as_str() {
        "background_agents_list" => Ok(Some(bridge_json_response(
            request.clone(),
            serde_json::json!({
                "background_agents": state.background_agents.values().map(child_agent_payload).collect::<Vec<_>>(),
                "subagents": state.subagents.values().map(child_agent_payload).collect::<Vec<_>>(),
            }),
        ))),
        _ => Ok(None),
    }
}

fn child_agent_start_bridge_call(
    ctx: &ServiceRunContext,
    request: &ConversationBridgeRequest,
    state: &mut AgentSessionRuntimeState,
) -> Result<Option<(ServiceCall, PendingChildAgentStart)>> {
    let (kind, task, agent_id) = match request.action.as_str() {
        "subagent_start" => {
            let payload: LegacySubagentStartPayload =
                serde_json::from_value(request.payload.clone())
                    .context("failed to parse subagent_start request")?;
            let description = payload.description.trim();
            if description.is_empty() {
                return Err(anyhow!("subagent_start requires description"));
            }
            let index = state.next_subagent_index;
            state.next_subagent_index = state.next_subagent_index.saturating_add(1);
            (
                AgentSessionKind::Subagent,
                description.to_string(),
                format!("subagent_{index:04}"),
            )
        }
        "background_agent_start" => {
            let payload: LegacyBackgroundStartPayload =
                serde_json::from_value(request.payload.clone())
                    .context("failed to parse background_agent_start request")?;
            let task = payload.task.trim();
            if task.is_empty() {
                return Err(anyhow!("background_agent_start requires task"));
            }
            let index = state.next_background_index;
            state.next_background_index = state.next_background_index.saturating_add(1);
            (
                AgentSessionKind::Background,
                task.to_string(),
                format!("background_{index:04}"),
            )
        }
        _ => return Ok(None),
    };
    state.persist(&ctx.storage)?;
    let pending = PendingChildAgentStart {
        service_request_id: String::new(),
        request: request.clone(),
        agent_id: agent_id.clone(),
        task: task.clone(),
        kind: kind.clone(),
    };
    let call = kernel::create_agent_session_with_binding_call(
        ctx.addr.clone(),
        kind,
        Some(agent_id),
        AgentSessionBinding {
            event_sink: ctx.addr.clone(),
            parent_addr: Some(ctx.addr.clone()),
        },
    )?;
    Ok(Some((call, pending)))
}

fn subagent_control_bridge_response(
    ctx: &ServiceRunContext,
    runner: &mut Option<RealAgentSessionRuntime>,
    request: &ConversationBridgeRequest,
    state: &mut AgentSessionRuntimeState,
    pending_subagent_joins: &mut VecDeque<PendingSubagentJoin>,
) -> Result<Option<ConversationBridgeResponse>> {
    match request.action.as_str() {
        "subagent_kill" => {
            let payload: LegacySubagentAgentPayload =
                serde_json::from_value(request.payload.clone())
                    .context("failed to parse subagent_kill request")?;
            let Some(record) = state.subagents.get_mut(&payload.agent_id) else {
                return Ok(Some(bridge_json_response(
                    request.clone(),
                    serde_json::json!({
                        "status": "failure",
                        "reason": format!("unknown subagent {}", payload.agent_id),
                    }),
                )));
            };
            if record.status != ChildAgentRuntimeStatus::Running {
                return Ok(Some(bridge_json_response(
                    request.clone(),
                    serde_json::json!({
                        "status": "failure",
                        "reason": format!(
                            "subagent {} is already {}",
                            payload.agent_id,
                            child_agent_status_label(record.status)
                        ),
                    }),
                )));
            }
            record.status = ChildAgentRuntimeStatus::Killed;
            let addr = record.addr.clone();
            state.persist(&ctx.storage)?;
            ctx.outbox
                .send(ServiceOutput::Call(agent_session::shutdown_call(
                    ctx.addr.clone(),
                    addr,
                    Some("subagent_kill".to_string()),
                )?))?;
            Ok(Some(bridge_json_response(
                request.clone(),
                serde_json::json!({"killed": true}),
            )))
        }
        "subagent_join" => {
            let payload: LegacySubagentJoinPayload =
                serde_json::from_value(request.payload.clone())
                    .context("failed to parse subagent_join request")?;
            let response = subagent_join_payload(state, &payload.agent_id);
            if response["status"] != "running" || payload.timeout_seconds.unwrap_or(0.0) <= 0.0 {
                return Ok(Some(bridge_json_response(request.clone(), response)));
            }
            let timeout_seconds = payload.timeout_seconds.unwrap_or(0.0);
            pending_subagent_joins.push_back(PendingSubagentJoin {
                request: request.clone(),
                agent_id: payload.agent_id,
                deadline: Instant::now() + Duration::from_secs_f64(timeout_seconds),
            });
            Ok(None)
        }
        "subagent_join_cancel" => {
            let payload: LegacySubagentJoinCancelPayload =
                serde_json::from_value(request.payload.clone())
                    .context("failed to parse subagent_join_cancel request")?;
            let Some(index) = pending_subagent_joins
                .iter()
                .position(|join| join.request.request_id == payload.request_id)
            else {
                return Ok(Some(bridge_json_response(
                    request.clone(),
                    serde_json::json!({
                        "status": "not_found",
                        "request_id": payload.request_id,
                    }),
                )));
            };
            let join = pending_subagent_joins
                .remove(index)
                .expect("pending join index should exist");
            let agent_id = join.agent_id.clone();
            resolve_bridge_json(
                runner,
                bridge_json_response(
                    join.request,
                    serde_json::json!({
                        "status": "interrupted",
                        "agent_id": agent_id,
                        "reason": payload.reason.unwrap_or_else(|| "subagent_join_cancel".to_string()),
                    }),
                ),
            )?;
            Ok(Some(bridge_json_response(
                request.clone(),
                serde_json::json!({
                    "status": "cancelled",
                    "request_id": payload.request_id,
                }),
            )))
        }
        _ => Ok(None),
    }
}

fn memory_bridge_response(
    request: ConversationBridgeRequest,
    response: MemoryResponse,
) -> Result<ConversationBridgeResponse> {
    let payload = memory_tool_payload(response)?;
    Ok(ConversationBridgeResponse {
        request_id: request.request_id,
        tool_call_id: request.tool_call_id.clone(),
        tool_name: request.tool_name.clone(),
        result: ToolResultItem {
            tool_call_id: request.tool_call_id,
            tool_name: request.tool_name,
            result: ToolResultContent::from_json(payload),
        },
    })
}

fn skill_bridge_response(
    request: ConversationBridgeRequest,
    response: SkillResponse,
) -> Result<ConversationBridgeResponse> {
    let payload = skill_tool_payload(response)?;
    Ok(ConversationBridgeResponse {
        request_id: request.request_id,
        tool_call_id: request.tool_call_id.clone(),
        tool_name: request.tool_name.clone(),
        result: ToolResultItem {
            tool_call_id: request.tool_call_id,
            tool_name: request.tool_name,
            result: ToolResultContent::from_json(payload),
        },
    })
}

fn tool_binary_bridge_response(
    request: ConversationBridgeRequest,
    response: ToolBinaryResponse,
) -> Result<ConversationBridgeResponse> {
    let payload = tool_binary_tool_payload(response)?;
    Ok(ConversationBridgeResponse {
        request_id: request.request_id,
        tool_call_id: request.tool_call_id.clone(),
        tool_name: request.tool_name.clone(),
        result: ToolResultItem {
            tool_call_id: request.tool_call_id,
            tool_name: request.tool_name,
            result: ToolResultContent::from_json(payload),
        },
    })
}

fn memory_tool_payload(response: MemoryResponse) -> Result<serde_json::Value> {
    match response {
        MemoryResponse::SearchResults { results, truncated } => Ok(serde_json::json!({
            "status": "success",
            "results": results.into_iter().map(memory_result_payload).collect::<Result<Vec<_>>>()?,
            "truncated": truncated,
        })),
        MemoryResponse::Accepted => Ok(serde_json::json!({"status": "success"})),
        MemoryResponse::Failure { reason } => Ok(serde_json::json!({
            "status": "failure",
            "reason": reason,
        })),
        MemoryResponse::PromptContext {
            scope,
            text,
            entries_hash,
            rendered_size_bytes,
            truncated,
        } => Ok(serde_json::json!({
            "status": "success",
            "scope": scope,
            "text": text,
            "entries_hash": entries_hash,
            "rendered_size_bytes": rendered_size_bytes,
            "truncated": truncated,
        })),
        MemoryResponse::MaintenanceCompleted => Ok(serde_json::json!({"status": "success"})),
    }
}

fn memory_result_payload(result: MemorySearchResult) -> Result<serde_json::Value> {
    Ok(serde_json::json!({
        "id": result.id,
        "scope": result.scope,
        "subject": result.subject,
        "text": result.text,
        "tags": result.tags,
        "updated_at": result.updated_at,
        "score": result.score,
    }))
}

fn skill_tool_payload(response: SkillResponse) -> Result<serde_json::Value> {
    match response {
        SkillResponse::Persisted {
            skill_name,
            mode,
            synced_workspaces,
        } => {
            let mut payload = serde_json::Map::new();
            payload.insert("skill_name".to_string(), serde_json::json!(skill_name));
            payload.insert(
                "synced_workspaces".to_string(),
                serde_json::json!(synced_workspaces),
            );
            match mode {
                SkillPersistMode::Create => {
                    payload.insert("created".to_string(), serde_json::json!(true));
                }
                SkillPersistMode::Update => {
                    payload.insert("updated".to_string(), serde_json::json!(true));
                }
                SkillPersistMode::Delete => {
                    payload.insert("deleted".to_string(), serde_json::json!(true));
                }
            }
            Ok(serde_json::Value::Object(payload))
        }
        SkillResponse::Loaded {
            skill_name,
            description,
            content,
        } => Ok(serde_json::json!({
            "name": skill_name,
            "skill_name": skill_name,
            "description": description,
            "content": content,
        })),
        SkillResponse::Failure { reason } => Ok(serde_json::json!({
            "status": "failure",
            "reason": reason,
        })),
        other => Ok(serde_json::json!({
            "status": "failure",
            "reason": format!("unexpected skill response: {other:?}"),
        })),
    }
}

fn tool_binary_tool_payload(response: ToolBinaryResponse) -> Result<serde_json::Value> {
    match response {
        ToolBinaryResponse::Ready {
            tool,
            version,
            platform,
            local_path,
            remote_path,
            path_dir,
        } => Ok(serde_json::json!({
            "status": "success",
            "tool": tool,
            "version": version,
            "platform": platform,
            "local_path": local_path,
            "remote_path": remote_path,
            "path_dir": path_dir,
        })),
        ToolBinaryResponse::Failure { reason } => Ok(serde_json::json!({
            "status": "failure",
            "reason": reason,
        })),
    }
}

fn cron_tool_payload(response: CronResponse) -> serde_json::Value {
    match response {
        CronResponse::Tasks { tasks } => serde_json::to_value(tasks).unwrap_or_else(
            |error| serde_json::json!({"status": "failure", "reason": error.to_string()}),
        ),
        CronResponse::TaskStatus { status } => serde_json::to_value(status).unwrap_or_else(
            |error| serde_json::json!({"status": "failure", "reason": error.to_string()}),
        ),
        CronResponse::Task { task } => serde_json::to_value(task).unwrap_or_else(
            |error| serde_json::json!({"status": "failure", "reason": error.to_string()}),
        ),
        CronResponse::Accepted => serde_json::json!({"status": "success"}),
        CronResponse::Rejected { reason } => serde_json::json!({
            "status": "failure",
            "reason": reason,
        }),
    }
}

fn foreground_addr_for_child_agent(
    ctx: &ServiceRunContext,
    state: &AgentSessionRuntimeState,
) -> Option<ServiceAddr> {
    if state.kind == AgentSessionKind::Foreground {
        Some(ctx.addr.clone())
    } else {
        state.binding.parent_addr.clone()
    }
}

fn child_agent_payload(record: &ChildAgentRuntimeRecord) -> serde_json::Value {
    serde_json::json!({
        "agent_id": record.agent_id,
        "addr": record.addr,
        "status": record.status,
        "task": record.task,
        "last_message": record.last_message,
        "last_error": record.last_error,
    })
}

fn child_agent_status_label(status: ChildAgentRuntimeStatus) -> &'static str {
    match status {
        ChildAgentRuntimeStatus::Running => "running",
        ChildAgentRuntimeStatus::Completed => "completed",
        ChildAgentRuntimeStatus::Failed => "failed",
        ChildAgentRuntimeStatus::Killed => "killed",
    }
}

fn background_result_message(mut message: ChatMessage) -> ChatMessage {
    message.role = ChatRole::User;
    message.token_usage = None;
    message
}

fn subagent_join_timer(
    pending: &VecDeque<PendingSubagentJoin>,
) -> crossbeam_channel::Receiver<Instant> {
    let Some(next) = pending.iter().map(|join| join.deadline).min() else {
        return crossbeam_channel::never::<Instant>();
    };
    let delay = next.saturating_duration_since(Instant::now());
    crossbeam_channel::after(delay)
}

fn resolve_due_subagent_joins(
    ctx: &ServiceRunContext,
    runner: &mut Option<RealAgentSessionRuntime>,
    pending: &mut VecDeque<PendingSubagentJoin>,
    state: &AgentSessionRuntimeState,
) -> Result<()> {
    let now = Instant::now();
    let mut remaining = VecDeque::new();
    while let Some(join) = pending.pop_front() {
        let payload = subagent_join_payload(state, &join.agent_id);
        if payload["status"] != "running" || now >= join.deadline {
            resolve_bridge_json(runner, bridge_json_response(join.request, payload))?;
        } else {
            remaining.push_back(join);
        }
    }
    *pending = remaining;
    ctx.outbox.send(ServiceOutput::Status(ServiceStatusUpdate {
        addr: ctx.addr.clone(),
        label: "subagent_join_timer_checked".to_string(),
        detail: serde_json::json!({ "pending": pending.len() }),
    }))?;
    Ok(())
}

fn resolve_ready_subagent_joins(
    _ctx: &ServiceRunContext,
    runner: &mut Option<RealAgentSessionRuntime>,
    pending: &mut VecDeque<PendingSubagentJoin>,
    state: &AgentSessionRuntimeState,
) -> Result<()> {
    let mut remaining = VecDeque::new();
    while let Some(join) = pending.pop_front() {
        let payload = subagent_join_payload(state, &join.agent_id);
        if payload["status"] != "running" {
            resolve_bridge_json(runner, bridge_json_response(join.request, payload))?;
        } else {
            remaining.push_back(join);
        }
    }
    *pending = remaining;
    Ok(())
}

fn subagent_join_payload(state: &AgentSessionRuntimeState, agent_id: &str) -> serde_json::Value {
    let Some(record) = state.subagents.get(agent_id) else {
        return serde_json::json!({
            "status": "failure",
            "agent_id": agent_id,
            "error": format!("unknown subagent {agent_id}"),
        });
    };
    match record.status {
        ChildAgentRuntimeStatus::Completed => serde_json::json!({
            "status": "completed",
            "agent_id": agent_id,
            "message": record.last_message,
        }),
        ChildAgentRuntimeStatus::Failed => serde_json::json!({
            "status": "failed",
            "agent_id": agent_id,
            "error": record.last_error,
        }),
        ChildAgentRuntimeStatus::Killed => serde_json::json!({
            "status": "killed",
            "agent_id": agent_id,
        }),
        ChildAgentRuntimeStatus::Running => serde_json::json!({
            "status": "running",
            "agent_id": agent_id,
        }),
    }
}

fn resolve_bridge_json(
    runner: &mut Option<RealAgentSessionRuntime>,
    response: ConversationBridgeResponse,
) -> Result<()> {
    if let Some(runner) = runner.as_mut() {
        runner.send(AgentSessionRequest::ResolveHostCoordination {
            response: serde_json::to_value(&response)?,
        })?;
    }
    Ok(())
}

fn bridge_json_response(
    request: ConversationBridgeRequest,
    payload: serde_json::Value,
) -> ConversationBridgeResponse {
    ConversationBridgeResponse {
        request_id: request.request_id,
        tool_call_id: request.tool_call_id.clone(),
        tool_name: request.tool_name.clone(),
        result: ToolResultItem {
            tool_call_id: request.tool_call_id,
            tool_name: request.tool_name,
            result: ToolResultContent::from_json(payload),
        },
    }
}

#[derive(Debug, Deserialize)]
struct LegacyMemorySearchPayload {
    query: String,
    #[serde(default)]
    scopes: Vec<String>,
    #[serde(default)]
    limit: Option<usize>,
}

#[derive(Debug, Deserialize)]
struct LegacyMemoryWritePayload {
    scope: String,
    #[serde(default)]
    subject: Option<String>,
    text: String,
    #[serde(default)]
    tags: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct LegacyMemoryUpdatePayload {
    memory_id: String,
    text: String,
}

#[derive(Debug, Deserialize)]
struct LegacyMemoryDeletePayload {
    memory_id: String,
}

#[derive(Debug, Deserialize)]
struct LegacySkillPersistPayload {
    #[serde(alias = "name")]
    skill_name: String,
}

#[derive(Debug, Deserialize)]
struct LegacySkillLoadPayload {
    #[serde(alias = "name")]
    skill_name: String,
}

#[derive(Debug, Deserialize)]
struct LegacyToolBinaryEnsurePayload {
    #[serde(alias = "name")]
    tool: String,
    #[serde(default, alias = "remote_host")]
    host: Option<String>,
}

#[derive(Debug, Deserialize)]
struct LegacySubagentStartPayload {
    description: String,
}

#[derive(Debug, Deserialize)]
struct LegacyBackgroundStartPayload {
    task: String,
}

#[derive(Debug, Deserialize)]
struct LegacySubagentAgentPayload {
    agent_id: String,
}

#[derive(Debug, Deserialize)]
struct LegacySubagentJoinPayload {
    agent_id: String,
    #[serde(default)]
    timeout_seconds: Option<f64>,
}

#[derive(Debug, Deserialize)]
struct LegacySubagentJoinCancelPayload {
    request_id: String,
    #[serde(default)]
    reason: Option<String>,
}

#[derive(Debug, Deserialize)]
struct LegacyCronIdPayload {
    id: String,
}

#[derive(Debug, Deserialize)]
struct LegacyCronCreatePayload {
    name: String,
    description: String,
    #[serde(default)]
    task: Option<String>,
    #[serde(default)]
    enabled: Option<bool>,
    #[serde(default)]
    timezone: Option<String>,
    cron_second: String,
    cron_minute: String,
    cron_hour: String,
    cron_day_of_month: String,
    cron_month: String,
    cron_day_of_week: String,
    #[serde(default)]
    cron_year: Option<String>,
}

impl LegacyCronCreatePayload {
    fn schedule(&self) -> Result<CronSchedule> {
        Ok(CronSchedule::CronExpression {
            expression: cron_expression(
                &self.cron_second,
                &self.cron_minute,
                &self.cron_hour,
                &self.cron_day_of_month,
                &self.cron_month,
                &self.cron_day_of_week,
                self.cron_year.as_deref(),
            ),
            timezone: self.timezone.clone(),
        })
    }
}

#[derive(Debug, Deserialize)]
struct LegacyCronUpdatePayload {
    id: String,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    description: Option<String>,
    #[serde(default)]
    task: Option<String>,
    #[serde(default)]
    clear_task: Option<bool>,
    #[serde(default)]
    enabled: Option<bool>,
    #[serde(default)]
    timezone: Option<String>,
    #[serde(default)]
    cron_second: Option<String>,
    #[serde(default)]
    cron_minute: Option<String>,
    #[serde(default)]
    cron_hour: Option<String>,
    #[serde(default)]
    cron_day_of_month: Option<String>,
    #[serde(default)]
    cron_month: Option<String>,
    #[serde(default)]
    cron_day_of_week: Option<String>,
    #[serde(default)]
    cron_year: Option<String>,
}

impl LegacyCronUpdatePayload {
    fn patch(self) -> Result<CronTaskPatch> {
        let schedule = match (
            self.cron_second,
            self.cron_minute,
            self.cron_hour,
            self.cron_day_of_month,
            self.cron_month,
            self.cron_day_of_week,
        ) {
            (
                Some(second),
                Some(minute),
                Some(hour),
                Some(day_of_month),
                Some(month),
                Some(day_of_week),
            ) => Some(CronSchedule::CronExpression {
                expression: cron_expression(
                    &second,
                    &minute,
                    &hour,
                    &day_of_month,
                    &month,
                    &day_of_week,
                    self.cron_year.as_deref(),
                ),
                timezone: self.timezone,
            }),
            (None, None, None, None, None, None) => None,
            _ => {
                return Err(anyhow!(
                    "cron_task_update timing fields must be provided together"
                ))
            }
        };
        let payload = if self.clear_task.unwrap_or(false) {
            Some(CronTaskPayload::Prompt {
                prompt: String::new(),
                output_policy: CronTaskOutputPolicy::ForwardResultToForeground,
            })
        } else {
            self.task.map(|task| CronTaskPayload::Prompt {
                prompt: task,
                output_policy: CronTaskOutputPolicy::ForwardResultToForeground,
            })
        };
        Ok(CronTaskPatch {
            name: self.name.map(Some),
            description: self.description.map(Some),
            enabled: self.enabled,
            schedule,
            payload,
        })
    }
}

fn cron_expression(
    second: &str,
    minute: &str,
    hour: &str,
    day_of_month: &str,
    month: &str,
    day_of_week: &str,
    year: Option<&str>,
) -> String {
    let mut parts = vec![
        second.trim().to_string(),
        minute.trim().to_string(),
        hour.trim().to_string(),
        day_of_month.trim().to_string(),
        month.trim().to_string(),
        day_of_week.trim().to_string(),
    ];
    if let Some(year) = year.map(str::trim).filter(|year| !year.is_empty()) {
        parts.push(year.to_string());
    }
    parts.join(" ")
}

struct PendingChildAgentStart {
    service_request_id: String,
    request: ConversationBridgeRequest,
    agent_id: String,
    task: String,
    kind: AgentSessionKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PendingServiceResponseKind {
    Memory,
    Skill,
    ToolBinary,
    Cron,
    Kernel,
}

impl PendingServiceResponseKind {
    fn label(self) -> &'static str {
        match self {
            PendingServiceResponseKind::Memory => "memory",
            PendingServiceResponseKind::Skill => "skill",
            PendingServiceResponseKind::ToolBinary => "tool_binary",
            PendingServiceResponseKind::Cron => "cron",
            PendingServiceResponseKind::Kernel => "kernel",
        }
    }
}

struct PendingBridgeRequest {
    service_request_id: String,
    request: ConversationBridgeRequest,
}

struct PendingSubagentJoin {
    request: ConversationBridgeRequest,
    agent_id: String,
    deadline: Instant,
}

fn parse_memory_scopes(scopes: Vec<String>) -> Result<Vec<crate::memory::MemoryScope>> {
    scopes
        .into_iter()
        .map(|scope| parse_memory_scope(&scope))
        .collect()
}

fn parse_memory_scope(scope: &str) -> Result<crate::memory::MemoryScope> {
    crate::memory::MemoryScope::parse(scope)
}

fn session_type_name(kind: &AgentSessionKind) -> &'static str {
    match kind {
        AgentSessionKind::Foreground => "foreground",
        AgentSessionKind::Background => "background",
        AgentSessionKind::Subagent => "subagent",
    }
}

fn child_agent_kind_label(kind: &AgentSessionKind) -> &'static str {
    match kind {
        AgentSessionKind::Foreground => "foreground",
        AgentSessionKind::Background => "background",
        AgentSessionKind::Subagent => "subagent",
    }
}

fn apply_core_session_event(
    state: &mut AgentSessionRuntimeState,
    current_plan: &mut Option<TaskPlanView>,
    event: &CoreSessionEvent,
) {
    match event {
        CoreSessionEvent::MessageAppended { index, message } => {
            state.message_count = state.message_count.max(index.saturating_add(1));
            state.last_message = Some(message.clone());
        }
        CoreSessionEvent::TurnStarted { turn_id, plan } => {
            state.state = AgentSessionState::Running;
            state.active_turn_id = Some(turn_id.clone());
            state.last_error = None;
            *current_plan = plan.clone();
        }
        CoreSessionEvent::Progress { plan, .. } => {
            if plan.is_some() {
                *current_plan = plan.clone();
            }
        }
        CoreSessionEvent::PlanUpdated { plan } => {
            *current_plan = plan.clone();
        }
        CoreSessionEvent::TurnCompleted { message } => {
            state.state = AgentSessionState::Idle;
            state.active_turn_id = None;
            state.last_message = Some(message.clone());
            *current_plan = None;
        }
        CoreSessionEvent::TurnFailed { error, .. } => {
            state.state = AgentSessionState::Idle;
            state.active_turn_id = None;
            state.last_error = Some(error.clone());
            *current_plan = None;
        }
        CoreSessionEvent::RuntimeCrashed { error, .. } => {
            state.state = AgentSessionState::Crashed;
            state.active_turn_id = None;
            state.last_error = Some(error.clone());
            *current_plan = None;
        }
        _ => {}
    }
}

fn should_start_turn(kind: &AgentSessionKind, origin: &AgentMessageOrigin) -> bool {
    match kind {
        AgentSessionKind::Foreground => {
            matches!(origin, AgentMessageOrigin::User | AgentMessageOrigin::Actor)
        }
        AgentSessionKind::Background | AgentSessionKind::Subagent => {
            matches!(
                origin,
                AgentMessageOrigin::User | AgentMessageOrigin::System
            )
        }
    }
}

fn to_core_session_request(request: AgentSessionRequest) -> Result<CoreSessionRequest, String> {
    match request {
        AgentSessionRequest::EnqueueMessage {
            origin,
            message,
            ingress_id: _,
        } => match origin {
            AgentMessageOrigin::User | AgentMessageOrigin::System => {
                Ok(CoreSessionRequest::EnqueueUserMessage { message })
            }
            AgentMessageOrigin::Actor => Ok(CoreSessionRequest::EnqueueActorMessage { message }),
        },
        AgentSessionRequest::CancelTurn { reason } => Ok(CoreSessionRequest::CancelTurn { reason }),
        AgentSessionRequest::ContinueTurn { reason } => {
            Ok(CoreSessionRequest::ContinueTurn { reason })
        }
        AgentSessionRequest::CompactNow => Ok(CoreSessionRequest::CompactNow),
        AgentSessionRequest::ResolveHostCoordination { response } => {
            let response = serde_json::from_value::<ConversationBridgeResponse>(response)
                .map_err(|error| format!("bad host coordination response: {error}"))?;
            Ok(CoreSessionRequest::ResolveHostCoordination { response })
        }
        AgentSessionRequest::ChildSessionEvent { .. } => {
            Err("child_session_event is handled by service".to_string())
        }
        AgentSessionRequest::QueryContext { query_id, payload } => {
            Ok(CoreSessionRequest::QuerySessionView { query_id, payload })
        }
        AgentSessionRequest::QueryMessages { .. }
        | AgentSessionRequest::QueryMessageDetail { .. } => {
            Err("message history queries are handled by service".to_string())
        }
        AgentSessionRequest::QueryStatus => Err("query_status is handled by service".to_string()),
        AgentSessionRequest::UpdateLaunchConfig { .. } => {
            Err("update_launch_config is handled by service".to_string())
        }
        AgentSessionRequest::Shutdown { .. } => Ok(CoreSessionRequest::Shutdown),
    }
}

fn from_core_session_event(event: CoreSessionEvent) -> AgentSessionEvent {
    match event {
        CoreSessionEvent::MessageAppended { index, message } => {
            AgentSessionEvent::MessageAppended { index, message }
        }
        CoreSessionEvent::TurnStarted { turn_id, plan } => {
            AgentSessionEvent::TurnStarted { turn_id, plan }
        }
        CoreSessionEvent::Progress { .. } => AgentSessionEvent::PlanUpdated { plan: None },
        CoreSessionEvent::PlanUpdated { plan } => AgentSessionEvent::PlanUpdated { plan },
        CoreSessionEvent::StreamAssistantMessageDelta {
            message_id,
            turn_id,
            in_message_index,
            item_id,
            delta,
            message_index,
        } => AgentSessionEvent::StreamAssistantMessageDelta {
            message_id,
            turn_id,
            in_message_index,
            item_id,
            delta,
            message_index,
        },
        CoreSessionEvent::StreamToolCallDelta {
            message_id,
            turn_id,
            in_message_index,
            item_id,
            call_id,
            tool_name,
            delta,
        } => AgentSessionEvent::StreamToolCallDelta {
            message_id,
            turn_id,
            in_message_index,
            item_id,
            call_id,
            tool_name,
            delta,
        },
        CoreSessionEvent::StreamReasoningSummaryDelta {
            message_id,
            turn_id,
            in_message_index,
            item_id,
            summary_index,
            delta,
        } => AgentSessionEvent::StreamReasoningSummaryDelta {
            message_id,
            turn_id,
            in_message_index,
            item_id,
            summary_index,
            delta,
        },
        CoreSessionEvent::StreamReasoningSummaryPartAdded {
            message_id,
            turn_id,
            in_message_index,
            item_id,
            summary_index,
        } => AgentSessionEvent::StreamReasoningSummaryPartAdded {
            message_id,
            turn_id,
            in_message_index,
            item_id,
            summary_index,
        },
        CoreSessionEvent::StreamError {
            message_id,
            turn_id,
            in_message_index,
            item_id,
            message_index,
            error,
            error_detail,
        } => AgentSessionEvent::StreamError {
            message_id,
            turn_id,
            in_message_index,
            item_id,
            message_index,
            error,
            error_detail,
        },
        CoreSessionEvent::StreamToolResultDone {
            turn_id,
            batch_id,
            tool_result,
        } => AgentSessionEvent::StreamToolResultDone {
            turn_id,
            batch_id,
            tool_result,
        },
        CoreSessionEvent::TurnCompleted { message } => AgentSessionEvent::TurnCompleted { message },
        CoreSessionEvent::TurnFailed {
            error,
            error_detail,
            can_continue,
        } => AgentSessionEvent::TurnFailed {
            error,
            error_detail,
            can_continue,
        },
        CoreSessionEvent::HostCoordinationRequested { request } => {
            AgentSessionEvent::HostCoordinationRequested {
                request: serde_json::to_value(request).unwrap_or_else(|error| {
                    serde_json::json!({
                        "error": format!("failed to encode host coordination request: {error}"),
                    })
                }),
            }
        }
        CoreSessionEvent::InteractiveOutputRequested { payload } => {
            AgentSessionEvent::InteractiveOutputRequested { payload }
        }
        CoreSessionEvent::SessionViewResult { query_id, payload } => {
            AgentSessionEvent::SessionViewResult { query_id, payload }
        }
        CoreSessionEvent::CompactCompleted {
            compressed,
            estimated_tokens_before,
            estimated_tokens_after,
            threshold_tokens,
            retained_message_count,
            compressed_message_count,
        } => AgentSessionEvent::CompactCompleted {
            compressed,
            estimated_tokens_before,
            estimated_tokens_after,
            threshold_tokens,
            retained_message_count,
            compressed_message_count,
        },
        CoreSessionEvent::CompactFailed { phase, reason } => {
            AgentSessionEvent::CompactFailed { phase, reason }
        }
        CoreSessionEvent::ControlRejected { reason, payload } => {
            AgentSessionEvent::ControlRejected { reason, payload }
        }
        CoreSessionEvent::RuntimeCrashed {
            error,
            error_detail,
        } => AgentSessionEvent::RuntimeCrashed {
            error,
            error_detail,
        },
    }
}

fn emit_session_event(
    ctx: &ServiceRunContext,
    event_sink: &crate::conversation_new::ServiceAddr,
    event: AgentSessionEvent,
) -> Result<()> {
    if *event_sink == crate::conversation_new::ServiceAddr::cron() {
        ctx.outbox
            .send(ServiceOutput::Call(cron::agent_session_event_call(
                ctx.addr.clone(),
                event_sink.clone(),
                event,
            )?))?;
        return Ok(());
    }
    if event_sink.local_service_id("agent").is_some() {
        ctx.outbox.send(ServiceOutput::Call(
            agent_session::child_session_event_call(ctx.addr.clone(), event_sink.clone(), event)?,
        ))?;
        return Ok(());
    }
    ctx.outbox
        .send(ServiceOutput::Call(channel::session_event_call(
            ctx.addr.clone(),
            event_sink.clone(),
            event,
        )?))?;
    Ok(())
}

fn emit_bad_payload_error(
    ctx: &ServiceRunContext,
    event_sink: &crate::conversation_new::ServiceAddr,
    detail: String,
) -> Result<()> {
    if event_sink.local_service_id("channel").is_some() {
        ctx.outbox.send(ServiceOutput::Call(channel::error_call(
            ctx.addr.clone(),
            event_sink.clone(),
            "agent_session.bad_payload",
            "Agent session request was not understood.",
            Some(detail),
        )?))?;
        return Ok(());
    }
    ctx.outbox
        .send(ServiceOutput::Call(cron::agent_session_event_call(
            ctx.addr.clone(),
            event_sink.clone(),
            AgentSessionEvent::RuntimeCrashed {
                error: "Agent session request was not understood.".to_string(),
                error_detail: SessionErrorDetail::new("agent_session", "bad_payload", detail),
            },
        )?))?;
    Ok(())
}

fn reply(
    source: &crate::conversation_new::ServiceAddr,
    target: &crate::conversation_new::ServiceAddr,
    response: AgentSessionResponse,
) -> Result<ServiceCall> {
    Ok(ServiceCall::new(
        source.clone(),
        target.clone(),
        encode_response(response)?,
    ))
}

#[derive(Debug, Deserialize)]
struct SessionMessagesIndex {
    messages: BTreeMap<String, SessionMessageIndexEntry>,
}

#[derive(Debug, Deserialize)]
struct SessionMessageIndexEntry {
    index: usize,
    byte_offset: u64,
}

fn read_agent_session_history(
    conversation_root: &Path,
    session_id: &str,
    request_id: String,
    offset: usize,
    limit: usize,
) -> Result<AgentSessionMessageHistory> {
    let path = session_all_messages_path(conversation_root, session_id);
    if !path.exists() {
        return Ok(AgentSessionMessageHistory {
            request_id,
            offset,
            limit,
            total: 0,
            last_message: None,
            messages: Vec::new(),
        });
    }
    let file =
        fs::File::open(&path).with_context(|| format!("failed to open {}", path.display()))?;
    let reader = BufReader::new(file);
    let mut total = 0usize;
    let mut messages = Vec::new();
    let mut last_message = None;
    let end = offset.saturating_add(limit.min(500));
    for (index, line) in reader.lines().enumerate() {
        let line = line.with_context(|| format!("failed to read {}", path.display()))?;
        if line.trim().is_empty() {
            continue;
        }
        let message: ChatMessage = serde_json::from_str(&line)
            .with_context(|| format!("failed to parse message {index} in {}", path.display()))?;
        let record = AgentSessionMessageRecord { index, message };
        if index >= offset && index < end {
            messages.push(record.clone());
        }
        last_message = Some(record);
        total = index.saturating_add(1);
    }
    Ok(AgentSessionMessageHistory {
        request_id,
        offset,
        limit,
        total,
        last_message,
        messages,
    })
}

fn read_agent_session_message_detail(
    conversation_root: &Path,
    session_id: &str,
    message_id: &str,
) -> Result<Option<AgentSessionMessageRecord>> {
    let path = session_all_messages_path(conversation_root, session_id);
    if !path.exists() {
        return Ok(None);
    }
    if let Some(record) = read_agent_session_message_detail_from_index(
        conversation_root,
        session_id,
        message_id,
        &path,
    )? {
        return Ok(Some(record));
    }
    read_agent_session_message_detail_by_scan(&path, message_id)
}

fn read_agent_session_message_detail_from_index(
    conversation_root: &Path,
    session_id: &str,
    message_id: &str,
    messages_path: &Path,
) -> Result<Option<AgentSessionMessageRecord>> {
    let index_path = session_messages_index_path(conversation_root, session_id);
    if !index_path.exists() {
        return Ok(None);
    }
    let raw = fs::read_to_string(&index_path)
        .with_context(|| format!("failed to read {}", index_path.display()))?;
    let index: SessionMessagesIndex = serde_json::from_str(&raw)
        .with_context(|| format!("failed to parse {}", index_path.display()))?;
    let Some(entry) = index.messages.get(message_id) else {
        return Ok(None);
    };
    let mut file = fs::File::open(messages_path)
        .with_context(|| format!("failed to open {}", messages_path.display()))?;
    file.seek(SeekFrom::Start(entry.byte_offset))
        .with_context(|| format!("failed to seek {}", messages_path.display()))?;
    let mut reader = BufReader::new(file);
    let mut line = String::new();
    reader
        .read_line(&mut line)
        .with_context(|| format!("failed to read {}", messages_path.display()))?;
    if line.trim().is_empty() {
        return Ok(None);
    }
    let message: ChatMessage = serde_json::from_str(&line).with_context(|| {
        format!(
            "failed to parse message {} in {}",
            entry.index,
            messages_path.display()
        )
    })?;
    Ok(Some(AgentSessionMessageRecord {
        index: entry.index,
        message,
    }))
}

fn read_agent_session_message_detail_by_scan(
    path: &Path,
    message_id: &str,
) -> Result<Option<AgentSessionMessageRecord>> {
    let requested_index = message_id.parse::<usize>().ok();
    let file =
        fs::File::open(path).with_context(|| format!("failed to open {}", path.display()))?;
    let reader = BufReader::new(file);
    for (index, line) in reader.lines().enumerate() {
        let line = line.with_context(|| format!("failed to read {}", path.display()))?;
        if line.trim().is_empty() {
            continue;
        }
        let message: ChatMessage = serde_json::from_str(&line)
            .with_context(|| format!("failed to parse message {index} in {}", path.display()))?;
        if message.message_id == message_id || requested_index.is_some_and(|value| value == index) {
            return Ok(Some(AgentSessionMessageRecord { index, message }));
        }
    }
    Ok(None)
}

fn session_all_messages_path(conversation_root: &Path, session_id: &str) -> PathBuf {
    session_log_dir(conversation_root, session_id).join("all_messages.jsonl")
}

fn session_messages_index_path(conversation_root: &Path, session_id: &str) -> PathBuf {
    session_log_dir(conversation_root, session_id).join("messages_index.json")
}

fn session_log_dir(conversation_root: &Path, session_id: &str) -> PathBuf {
    conversation_root
        .join(".stellaclaw")
        .join("log")
        .join(sanitize_session_id_for_log_path(session_id))
}

fn sanitize_session_id_for_log_path(session_id: &str) -> String {
    session_id
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '.') {
                ch
            } else {
                '_'
            }
        })
        .collect()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct AgentSessionRuntimeState {
    kind: AgentSessionKind,
    binding: AgentSessionBinding,
    state: AgentSessionState,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    active_turn_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    last_error: Option<String>,
    #[serde(default)]
    message_count: usize,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    last_message: Option<ChatMessage>,
    #[serde(default)]
    next_subagent_index: u64,
    #[serde(default)]
    next_background_index: u64,
    #[serde(default)]
    next_cron_index: u64,
    #[serde(default)]
    subagents: BTreeMap<String, ChildAgentRuntimeRecord>,
    #[serde(default)]
    background_agents: BTreeMap<String, ChildAgentRuntimeRecord>,
}

impl AgentSessionRuntimeState {
    fn new(kind: AgentSessionKind, binding: AgentSessionBinding) -> Self {
        Self {
            kind,
            binding,
            state: AgentSessionState::Idle,
            active_turn_id: None,
            last_error: None,
            message_count: 0,
            last_message: None,
            next_subagent_index: 1,
            next_background_index: 1,
            next_cron_index: 1,
            subagents: BTreeMap::new(),
            background_agents: BTreeMap::new(),
        }
    }

    fn append_message(&mut self, message: ChatMessage) -> usize {
        let index = self.message_count;
        self.message_count += 1;
        self.last_message = Some(message);
        index
    }

    fn status(&self, current_plan: Option<TaskPlanView>) -> AgentSessionStatus {
        AgentSessionStatus {
            kind: self.kind.clone(),
            binding: self.binding.clone(),
            state: self.state.clone(),
            active_turn_id: self.active_turn_id.clone(),
            current_plan,
            last_error: self.last_error.clone(),
            message_count: self.message_count,
        }
    }

    fn context(
        &self,
        launch: &AgentSessionLaunchConfig,
        pending_launch: Option<&AgentSessionLaunchConfig>,
        current_plan: Option<TaskPlanView>,
    ) -> AgentSessionContext {
        AgentSessionContext {
            status: self.status(current_plan),
            last_message: self.last_message.clone(),
            metadata: launch_metadata(launch, pending_launch),
        }
    }

    fn persist(&self, storage: &Path) -> Result<()> {
        fs::create_dir_all(storage)
            .with_context(|| format!("failed to create {}", storage.display()))?;
        let content =
            serde_json::to_string_pretty(self).context("failed to encode agent session state")?;
        let path = service_state_path(storage);
        let tmp_path = storage.join("service_state.json.tmp");
        fs::write(&tmp_path, content)
            .with_context(|| format!("failed to write {}", tmp_path.display()))?;
        fs::rename(&tmp_path, &path).with_context(|| {
            format!(
                "failed to rename {} to {}",
                tmp_path.display(),
                path.display()
            )
        })
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ChildAgentRuntimeRecord {
    agent_id: String,
    addr: crate::conversation_new::ServiceAddr,
    status: ChildAgentRuntimeStatus,
    task: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    last_message: Option<ChatMessage>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    last_error: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum ChildAgentRuntimeStatus {
    Running,
    Completed,
    Failed,
    Killed,
}

fn launch_metadata(
    launch: &AgentSessionLaunchConfig,
    pending_launch: Option<&AgentSessionLaunchConfig>,
) -> serde_json::Value {
    serde_json::json!({
        "session_id": launch.session_id,
        "conversation_root": launch.conversation_root.display().to_string(),
        "workspace_root": launch.workspace_root.display().to_string(),
        "agent_server_configured": launch.agent_server_path.is_some(),
        "has_session_profile": launch.session_profile.is_some(),
        "model_count": launch.models.len(),
        "memory_enabled": launch.memory_enabled,
        "tool_remote_mode": &launch.tool_remote_mode,
        "has_sandbox_override": launch.sandbox.is_some(),
        "reasoning_effort": &launch.reasoning_effort,
        "idle_timeout_compact_enabled": launch.idle_timeout_compact_enabled,
        "session_defaults": {
            "compression_threshold_tokens": launch.session_defaults.compression_threshold_tokens,
            "compression_retain_recent_tokens": launch.session_defaults.compression_retain_recent_tokens,
        },
        "pending_launch_update": pending_launch.is_some(),
    })
}

fn load_service_state(storage: &Path) -> Result<Option<AgentSessionRuntimeState>> {
    let path = service_state_path(storage);
    if !path.is_file() {
        return Ok(None);
    }
    let content =
        fs::read_to_string(&path).with_context(|| format!("failed to read {}", path.display()))?;
    let state = serde_json::from_str(&content)
        .with_context(|| format!("failed to parse {}", path.display()))?;
    Ok(Some(state))
}

fn service_state_path(storage: &Path) -> std::path::PathBuf {
    storage.join("service_state.json")
}

#[cfg(test)]
mod tests {
    use super::*;
    use stellaclaw_core::model_config::{ProviderType, TokenEstimatorType};
    use stellaclaw_core::session_actor::ChatRole;

    #[test]
    fn persists_lightweight_service_state() {
        let storage = std::env::temp_dir().join(format!(
            "stellaclaw-agent-session-state-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::SystemTime::UNIX_EPOCH)
                .expect("clock works")
                .as_nanos()
        ));
        let mut state = AgentSessionRuntimeState::new(
            AgentSessionKind::Foreground,
            AgentSessionBinding {
                event_sink: crate::conversation_new::ServiceAddr::channel_id("scratch"),
                parent_addr: None,
            },
        );
        state.append_message(text_message(ChatRole::User, "hello"));
        state.persist(&storage).expect("state persists");

        let loaded = load_service_state(&storage)
            .expect("state loads")
            .expect("state exists");

        assert_eq!(loaded.kind, AgentSessionKind::Foreground);
        assert_eq!(loaded.message_count, 1);
        assert!(loaded.last_message.is_some());
    }

    #[test]
    fn maps_agent_request_to_core_session_request() {
        let request = AgentSessionRequest::EnqueueMessage {
            origin: AgentMessageOrigin::Actor,
            message: text_message(ChatRole::User, "background result"),
            ingress_id: Some("ingress".to_string()),
        };

        let mapped = to_core_session_request(request).expect("request maps");

        assert!(matches!(
            mapped,
            CoreSessionRequest::EnqueueActorMessage { .. }
        ));
    }

    #[test]
    fn maps_core_session_event_to_agent_session_event() {
        let event = CoreSessionEvent::TurnCompleted {
            message: text_message(ChatRole::Assistant, "done"),
        };

        let mapped = from_core_session_event(event);

        assert!(matches!(mapped, AgentSessionEvent::TurnCompleted { .. }));
    }

    #[test]
    fn real_session_is_disabled_without_agent_server_path() {
        let launch = test_launch_config(None);

        let runner = start_real_session(&launch, &AgentSessionKind::Foreground)
            .expect("missing agent server path is not an error");

        assert!(runner.is_none());
    }

    #[test]
    fn configured_agent_server_requires_chat_model() {
        let mut launch = test_launch_config(Some(PathBuf::from("/tmp/agent-server")));
        launch.models.clear();

        let error = match start_real_session(&launch, &AgentSessionKind::Foreground) {
            Ok(_) => panic!("configured agent server without model should fail"),
            Err(error) => error,
        };

        assert!(error
            .to_string()
            .contains("no chat-capable model is available"));
    }

    #[test]
    fn core_events_update_lightweight_state() {
        let mut state = AgentSessionRuntimeState::new(
            AgentSessionKind::Foreground,
            AgentSessionBinding {
                event_sink: crate::conversation_new::ServiceAddr::channel_id("scratch"),
                parent_addr: None,
            },
        );
        let mut current_plan = None;

        apply_core_session_event(
            &mut state,
            &mut current_plan,
            &CoreSessionEvent::TurnStarted {
                turn_id: "turn_1".to_string(),
                plan: None,
            },
        );
        assert_eq!(state.state, AgentSessionState::Running);
        assert_eq!(state.active_turn_id.as_deref(), Some("turn_1"));

        apply_core_session_event(
            &mut state,
            &mut current_plan,
            &CoreSessionEvent::MessageAppended {
                index: 3,
                message: text_message(ChatRole::Assistant, "hello"),
            },
        );
        assert_eq!(state.message_count, 4);
        assert!(state.last_message.is_some());
    }

    #[test]
    fn current_plan_is_runtime_status_only_and_clears_on_terminal_events() {
        let mut state = AgentSessionRuntimeState::new(
            AgentSessionKind::Foreground,
            AgentSessionBinding {
                event_sink: crate::conversation_new::ServiceAddr::channel_id("scratch"),
                parent_addr: None,
            },
        );
        let mut current_plan = None;
        let plan = TaskPlanView {
            explanation: None,
            plan: vec![stellaclaw_core::session_actor::TaskPlanItemView {
                step: "inspect".to_string(),
                status: stellaclaw_core::session_actor::TaskPlanItemStatus::InProgress,
            }],
        };

        apply_core_session_event(
            &mut state,
            &mut current_plan,
            &CoreSessionEvent::PlanUpdated {
                plan: Some(plan.clone()),
            },
        );

        assert!(state.status(current_plan.clone()).current_plan.is_some());
        let encoded = serde_json::to_value(&state).expect("state encodes");
        assert!(encoded.get("current_plan").is_none());

        apply_core_session_event(
            &mut state,
            &mut current_plan,
            &CoreSessionEvent::TurnCompleted {
                message: text_message(ChatRole::Assistant, "done"),
            },
        );

        assert!(state.status(current_plan).current_plan.is_none());
    }

    #[test]
    fn update_plan_bridge_updates_runtime_plan_without_persisting() {
        let (ctx, output_rx) = test_run_context("update_plan_bridge");
        let event_sink = crate::conversation_new::ServiceAddr::channel_id("scratch");
        let mut current_plan = None;
        let request = ConversationBridgeRequest {
            request_id: "req_plan".to_string(),
            tool_call_id: "call_plan".to_string(),
            tool_name: "update_plan".to_string(),
            action: "update_plan".to_string(),
            payload: serde_json::json!({
                "explanation": "  work carefully  ",
                "plan": [
                    {"step": " inspect ", "status": "in_progress"},
                    {"step": "report", "status": "pending"}
                ]
            }),
        };

        let response = update_plan_bridge_response(&ctx, &event_sink, &mut current_plan, &request)
            .expect("update_plan bridge should handle request")
            .expect("response should be returned");

        let plan = current_plan.expect("plan should be updated");
        assert_eq!(plan.explanation.as_deref(), Some("work carefully"));
        assert_eq!(plan.plan[0].step, "inspect");
        assert_eq!(response.tool_name, "update_plan");
        assert!(
            stellaclaw_core::session_actor::tool_result_text(&response.result)
                .contains("\"updated\": true")
        );
        assert!(!ctx.storage.join("service_state.json").exists());
        assert!(matches!(output_rx.try_recv(), Ok(ServiceOutput::Call(_))));
        assert!(matches!(output_rx.try_recv(), Ok(ServiceOutput::Status(_))));
    }

    #[test]
    fn pending_launch_update_applies_at_idle_boundary() {
        let (ctx, output_rx) = test_run_context("pending_launch_update");
        let mut launch = test_launch_config(None);
        let mut pending_launch = Some({
            let mut launch = test_launch_config(None);
            launch.memory_enabled = true;
            launch.reasoning_effort = Some("high".to_string());
            launch
        });
        let mut runner = None;
        let mut session_event_rx = crossbeam_channel::never();
        let mut current_plan = None;
        let mut state = AgentSessionRuntimeState::new(
            AgentSessionKind::Foreground,
            AgentSessionBinding {
                event_sink: crate::conversation_new::ServiceAddr::channel_id("scratch"),
                parent_addr: None,
            },
        );

        state.state = AgentSessionState::Running;
        maybe_apply_pending_launch(
            &ctx,
            &AgentSessionKind::Foreground,
            &mut launch,
            &mut pending_launch,
            &mut runner,
            &mut session_event_rx,
            &state,
        )
        .expect("running state does not apply");
        assert!(pending_launch.is_some());
        assert!(!launch.memory_enabled);

        apply_core_session_event(
            &mut state,
            &mut current_plan,
            &CoreSessionEvent::TurnCompleted {
                message: text_message(ChatRole::Assistant, "done"),
            },
        );
        maybe_apply_pending_launch(
            &ctx,
            &AgentSessionKind::Foreground,
            &mut launch,
            &mut pending_launch,
            &mut runner,
            &mut session_event_rx,
            &state,
        )
        .expect("idle state applies pending launch");

        assert!(pending_launch.is_none());
        assert!(launch.memory_enabled);
        assert_eq!(launch.reasoning_effort.as_deref(), Some("high"));
        assert!(output_rx.try_iter().any(|output| {
            matches!(
                output,
                ServiceOutput::Status(ServiceStatusUpdate { label, .. })
                    if label == "launch_config_applied"
            )
        }));
    }

    #[test]
    fn memory_bridge_request_routes_to_memory_service() {
        let (ctx, _output_rx) = test_run_context("memory_bridge_request");
        let request = ConversationBridgeRequest {
            request_id: "req_1".to_string(),
            tool_call_id: "call_1".to_string(),
            tool_name: "memory_search".to_string(),
            action: "memory_search".to_string(),
            payload: serde_json::json!({
                "query": "service tree",
                "scopes": ["conversation"],
                "limit": 3,
            }),
        };

        let call = memory_bridge_call(&ctx, &AgentSessionKind::Foreground, &request)
            .expect("bridge request parses")
            .expect("memory request should route");

        assert_eq!(call.target, crate::conversation_new::ServiceAddr::memory());
        let decoded = memory::decode_request(call.payload).expect("memory request decodes");
        assert!(matches!(
            decoded,
            MemoryRequest::Search {
                query,
                scopes,
                limit: Some(3),
                ..
            } if query == "service tree"
                && scopes == vec![crate::memory::MemoryScope::Conversation]
        ));
    }

    #[test]
    fn memory_bridge_response_preserves_legacy_tool_payload_shape() {
        let request = ConversationBridgeRequest {
            request_id: "req_1".to_string(),
            tool_call_id: "call_1".to_string(),
            tool_name: "memory_search".to_string(),
            action: "memory_search".to_string(),
            payload: serde_json::json!({}),
        };
        let response = memory_bridge_response(
            request,
            MemoryResponse::SearchResults {
                results: vec![MemorySearchResult {
                    id: "c_1".to_string(),
                    scope: crate::memory::MemoryScope::Conversation,
                    subject: Some("repo".to_string()),
                    text: "service tree runtime".to_string(),
                    tags: vec!["architecture".to_string()],
                    updated_at: "2026-01-01T00:00:00Z".to_string(),
                    score: 1.0,
                }],
                truncated: false,
            },
        )
        .expect("response builds");

        let text = stellaclaw_core::session_actor::tool_result_text(&response.result);
        let value: serde_json::Value = serde_json::from_str(&text).expect("json result renders");
        assert_eq!(value["status"], "success");
        assert_eq!(value["results"][0]["id"], "c_1");
        assert_eq!(value["results"][0]["scope"], "conversation");
    }

    #[test]
    fn skill_bridge_request_routes_to_skill_service() {
        let (ctx, _output_rx) = test_run_context("skill_bridge_request");
        let request = ConversationBridgeRequest {
            request_id: "req_1".to_string(),
            tool_call_id: "call_1".to_string(),
            tool_name: "skill_create".to_string(),
            action: "skill_create".to_string(),
            payload: serde_json::json!({
                "name": "demo_skill",
            }),
        };

        let call = skill_bridge_call(&ctx, &request)
            .expect("bridge request parses")
            .expect("skill request should route");

        assert_eq!(call.target, crate::conversation_new::ServiceAddr::skill());
        let decoded = skill::decode_request(call.payload).expect("skill request decodes");
        assert!(matches!(
            decoded,
            SkillRequest::Persist {
                skill_name,
                mode: SkillPersistMode::Create,
            } if skill_name == "demo_skill"
        ));

        let load_request = ConversationBridgeRequest {
            request_id: "req_2".to_string(),
            tool_call_id: "call_2".to_string(),
            tool_name: "skill_load".to_string(),
            action: "skill_load".to_string(),
            payload: serde_json::json!({
                "skill_name": "demo_skill",
            }),
        };
        let call = skill_bridge_call(&ctx, &load_request)
            .expect("skill load request parses")
            .expect("skill load should route");
        let decoded = skill::decode_request(call.payload).expect("skill load request decodes");
        assert!(matches!(
            decoded,
            SkillRequest::Load { skill_name } if skill_name == "demo_skill"
        ));
    }

    #[test]
    fn skill_bridge_response_preserves_legacy_tool_payload_shape() {
        let request = ConversationBridgeRequest {
            request_id: "req_1".to_string(),
            tool_call_id: "call_1".to_string(),
            tool_name: "skill_update".to_string(),
            action: "skill_update".to_string(),
            payload: serde_json::json!({}),
        };

        let response = skill_bridge_response(
            request,
            SkillResponse::Persisted {
                skill_name: "demo_skill".to_string(),
                mode: SkillPersistMode::Update,
                synced_workspaces: 2,
            },
        )
        .expect("response builds");

        let text = stellaclaw_core::session_actor::tool_result_text(&response.result);
        let value: serde_json::Value = serde_json::from_str(&text).expect("json result renders");
        assert_eq!(value["updated"], true);
        assert_eq!(value["skill_name"], "demo_skill");
        assert_eq!(value["synced_workspaces"], 2);

        let loaded = skill_bridge_response(
            ConversationBridgeRequest {
                request_id: "req_2".to_string(),
                tool_call_id: "call_2".to_string(),
                tool_name: "skill_load".to_string(),
                action: "skill_load".to_string(),
                payload: serde_json::json!({}),
            },
            SkillResponse::Loaded {
                skill_name: "demo_skill".to_string(),
                description: "Demo skill".to_string(),
                content: "# Demo\nUse demo carefully.".to_string(),
            },
        )
        .expect("loaded response builds");
        let text = stellaclaw_core::session_actor::tool_result_text(&loaded.result);
        let value: serde_json::Value = serde_json::from_str(&text).expect("json result renders");
        assert_eq!(value["name"], "demo_skill");
        assert_eq!(value["skill_name"], "demo_skill");
        assert_eq!(value["description"], "Demo skill");
        assert!(value["content"]
            .as_str()
            .is_some_and(|content| content.contains("Use demo carefully")));
    }

    #[test]
    fn tool_binary_bridge_request_routes_to_tool_binary_service() {
        let (ctx, _output_rx) = test_run_context("tool_binary_bridge_request");
        let request = ConversationBridgeRequest {
            request_id: "req_1".to_string(),
            tool_call_id: "call_1".to_string(),
            tool_name: "tool_binary_ensure".to_string(),
            action: "tool_binary_ensure".to_string(),
            payload: serde_json::json!({
                "tool": "ripgrep",
                "host": "remote-host",
            }),
        };

        let call = tool_binary_bridge_call(&ctx, &request)
            .expect("bridge request parses")
            .expect("tool binary request should route");

        assert_eq!(
            call.target,
            crate::conversation_new::ServiceAddr::tool_binary()
        );
        let decoded = tool_binary::decode_request(call.payload).expect("tool binary decodes");
        assert!(matches!(
            decoded,
            ToolBinaryRequest::Ensure {
                tool,
                host: Some(host),
            } if tool == "ripgrep" && host == "remote-host"
        ));
    }

    #[test]
    fn tool_binary_bridge_response_preserves_core_payload_shape() {
        let request = ConversationBridgeRequest {
            request_id: "req_1".to_string(),
            tool_call_id: "call_1".to_string(),
            tool_name: "tool_binary_ensure".to_string(),
            action: "tool_binary_ensure".to_string(),
            payload: serde_json::json!({}),
        };

        let response = tool_binary_bridge_response(
            request,
            ToolBinaryResponse::Ready {
                tool: "ripgrep".to_string(),
                version: "15.1.0".to_string(),
                platform: Some("macos-arm64".to_string()),
                local_path: Some("/tmp/rg".to_string()),
                remote_path: None,
                path_dir: Some("/tmp".to_string()),
            },
        )
        .expect("response builds");

        let text = stellaclaw_core::session_actor::tool_result_text(&response.result);
        let value: serde_json::Value = serde_json::from_str(&text).expect("json result renders");
        assert_eq!(value["status"], "success");
        assert_eq!(value["tool"], "ripgrep");
        assert_eq!(value["version"], "15.1.0");
        assert_eq!(value["local_path"], "/tmp/rg");
    }

    #[test]
    fn cron_bridge_tools_route_to_owner_scoped_cron_service() {
        let (ctx, _output_rx) = test_run_context("cron_bridge_tools");
        let mut state = AgentSessionRuntimeState::new(
            AgentSessionKind::Foreground,
            AgentSessionBinding {
                event_sink: crate::conversation_new::ServiceAddr::channel_id("scratch"),
                parent_addr: None,
            },
        );

        let create = ConversationBridgeRequest {
            request_id: "req_create".to_string(),
            tool_call_id: "call_create".to_string(),
            tool_name: "cron_task_create".to_string(),
            action: "cron_task_create".to_string(),
            payload: serde_json::json!({
                "name": "Daily check",
                "description": "run daily",
                "task": "check the repo",
                "cron_second": "0",
                "cron_minute": "5",
                "cron_hour": "9",
                "cron_day_of_month": "*",
                "cron_month": "*",
                "cron_day_of_week": "*",
                "timezone": "Asia/Shanghai",
            }),
        };

        let call = cron_bridge_call(&ctx, &create, &mut state)
            .expect("create parses")
            .expect("create routes");
        assert_eq!(call.target, crate::conversation_new::ServiceAddr::cron());
        let decoded = cron::decode_request(call.payload).expect("cron request decodes");
        assert!(matches!(
            decoded,
            CronRequest::RegisterTask { task }
                if task.task_id == "cron_0001"
                    && task.registered_by == ctx.addr
                    && task.channel_addr == crate::conversation_new::ServiceAddr::channel_id("scratch")
                    && task.foreground_session_addr == Some(ctx.addr.clone())
                    && matches!(task.schedule, CronSchedule::CronExpression { .. })
        ));
        assert_eq!(state.next_cron_index, 2);

        let list = ConversationBridgeRequest {
            request_id: "req_list".to_string(),
            tool_call_id: "call_list".to_string(),
            tool_name: "cron_tasks_list".to_string(),
            action: "cron_tasks_list".to_string(),
            payload: serde_json::json!({}),
        };
        let call = cron_bridge_call(&ctx, &list, &mut state)
            .expect("list parses")
            .expect("list routes");
        assert!(matches!(
            cron::decode_request(call.payload).expect("cron request decodes"),
            CronRequest::ListTasks { owner: Some(owner) } if owner == ctx.addr
        ));

        let get = ConversationBridgeRequest {
            request_id: "req_get".to_string(),
            tool_call_id: "call_get".to_string(),
            tool_name: "cron_task_get".to_string(),
            action: "cron_task_get".to_string(),
            payload: serde_json::json!({"id": "cron_0001"}),
        };
        let call = cron_bridge_call(&ctx, &get, &mut state)
            .expect("get parses")
            .expect("get routes");
        assert!(matches!(
            cron::decode_request(call.payload).expect("cron request decodes"),
            CronRequest::GetTaskStatus { task_id, owner: Some(owner) }
                if task_id == "cron_0001" && owner == ctx.addr
        ));

        let update = ConversationBridgeRequest {
            request_id: "req_update".to_string(),
            tool_call_id: "call_update".to_string(),
            tool_name: "cron_task_update".to_string(),
            action: "cron_task_update".to_string(),
            payload: serde_json::json!({"id": "cron_0001", "enabled": false}),
        };
        let call = cron_bridge_call(&ctx, &update, &mut state)
            .expect("update parses")
            .expect("update routes");
        assert!(matches!(
            cron::decode_request(call.payload).expect("cron request decodes"),
            CronRequest::UpdateTask { task_id, patch }
                if task_id == "cron_0001" && patch.enabled == Some(false)
        ));

        let remove = ConversationBridgeRequest {
            request_id: "req_remove".to_string(),
            tool_call_id: "call_remove".to_string(),
            tool_name: "cron_task_remove".to_string(),
            action: "cron_task_remove".to_string(),
            payload: serde_json::json!({"id": "cron_0001"}),
        };
        let call = cron_bridge_call(&ctx, &remove, &mut state)
            .expect("remove parses")
            .expect("remove routes");
        assert!(matches!(
            cron::decode_request(call.payload).expect("cron request decodes"),
            CronRequest::RemoveTask { task_id } if task_id == "cron_0001"
        ));
    }

    #[test]
    fn accepted_cron_response_routes_by_response_id_before_payload_shape() {
        let (ctx, output_rx) = test_run_context("cron_response_dispatch");
        let mut runner = None;
        let mut pending_memory_requests = VecDeque::new();
        let mut pending_skill_requests = VecDeque::new();
        let mut pending_tool_binary_requests = VecDeque::new();
        let mut pending_cron_requests = VecDeque::new();
        let mut pending_child_starts = VecDeque::new();
        let mut pending_service_responses = BTreeMap::new();
        let mut state = AgentSessionRuntimeState::new(
            AgentSessionKind::Foreground,
            AgentSessionBinding {
                event_sink: crate::conversation_new::ServiceAddr::channel_id("scratch"),
                parent_addr: None,
            },
        );

        pending_cron_requests.push_back(PendingBridgeRequest {
            service_request_id: "svc_create".to_string(),
            request: ConversationBridgeRequest {
                request_id: "req_create".to_string(),
                tool_call_id: "call_create".to_string(),
                tool_name: "cron_task_create".to_string(),
                action: "cron_task_create".to_string(),
                payload: serde_json::json!({}),
            },
        });
        pending_service_responses
            .insert("svc_create".to_string(), PendingServiceResponseKind::Cron);

        let handled = handle_service_response(
            &ctx,
            &mut runner,
            &mut pending_memory_requests,
            &mut pending_skill_requests,
            &mut pending_tool_binary_requests,
            &mut pending_cron_requests,
            &mut pending_child_starts,
            &mut pending_service_responses,
            &mut state,
            Some("svc_create"),
            cron::encode_response(CronResponse::Accepted).expect("cron response encodes"),
        )
        .expect("cron response dispatches");

        assert!(handled);
        assert!(pending_memory_requests.is_empty());
        assert!(pending_cron_requests.is_empty());
        assert!(pending_service_responses.is_empty());
        assert!(output_rx.try_iter().any(|output| matches!(
            output,
            ServiceOutput::Status(ServiceStatusUpdate { label, .. }) if label == "cron_bridge_resolved"
        )));
    }

    #[test]
    fn managed_agent_tools_report_background_and_subagent_state() {
        let mut state = AgentSessionRuntimeState::new(
            AgentSessionKind::Foreground,
            AgentSessionBinding {
                event_sink: crate::conversation_new::ServiceAddr::channel_id("scratch"),
                parent_addr: None,
            },
        );
        state.background_agents.insert(
            "background_0001".to_string(),
            ChildAgentRuntimeRecord {
                agent_id: "background_0001".to_string(),
                addr: crate::conversation_new::ServiceAddr::agent_background("background_0001"),
                status: ChildAgentRuntimeStatus::Running,
                task: "watch build".to_string(),
                last_message: None,
                last_error: None,
            },
        );
        state.subagents.insert(
            "subagent_0001".to_string(),
            ChildAgentRuntimeRecord {
                agent_id: "subagent_0001".to_string(),
                addr: crate::conversation_new::ServiceAddr::agent_subagent("subagent_0001"),
                status: ChildAgentRuntimeStatus::Completed,
                task: "inspect module".to_string(),
                last_message: Some(text_message(ChatRole::Assistant, "done")),
                last_error: None,
            },
        );

        let list_request = ConversationBridgeRequest {
            request_id: "req_list".to_string(),
            tool_call_id: "call_list".to_string(),
            tool_name: "background_agents_list".to_string(),
            action: "background_agents_list".to_string(),
            payload: serde_json::json!({}),
        };
        let response = managed_agent_bridge_response(&list_request, &state)
            .expect("list builds")
            .expect("list handles");
        let text = stellaclaw_core::session_actor::tool_result_text(&response.result);
        let value: serde_json::Value = serde_json::from_str(&text).expect("json result renders");
        assert_eq!(value["background_agents"][0]["agent_id"], "background_0001");
        assert_eq!(value["subagents"][0]["agent_id"], "subagent_0001");
        assert_eq!(value["subagents"][0]["status"], "completed");
        assert_eq!(value["subagents"][0]["last_message"]["role"], "assistant");
        assert_eq!(
            value["subagents"][0]["last_message"]["data"][0]["payload"]["text"],
            "done"
        );
    }

    #[test]
    fn subagent_start_routes_to_kernel_with_parent_sink() {
        let (ctx, _output_rx) = test_run_context("subagent_start_bridge");
        let mut state = AgentSessionRuntimeState::new(
            AgentSessionKind::Foreground,
            AgentSessionBinding {
                event_sink: crate::conversation_new::ServiceAddr::channel_id("scratch"),
                parent_addr: None,
            },
        );
        let request = ConversationBridgeRequest {
            request_id: "req_1".to_string(),
            tool_call_id: "call_1".to_string(),
            tool_name: "subagent_start".to_string(),
            action: "subagent_start".to_string(),
            payload: serde_json::json!({
                "description": "inspect the module",
            }),
        };

        let (call, pending) = child_agent_start_bridge_call(&ctx, &request, &mut state)
            .expect("bridge request parses")
            .expect("subagent start should route");

        assert_eq!(call.target, crate::conversation_new::ServiceAddr::kernel());
        let decoded = kernel::decode_request(call.payload).expect("kernel request decodes");
        assert!(matches!(
            decoded,
            kernel::KernelRequest::CreateAgentSession {
                kind: AgentSessionKind::Subagent,
                id: Some(id),
                binding: Some(AgentSessionBinding {
                    event_sink,
                    parent_addr: Some(parent),
                }),
            } if id == "subagent_0001" && event_sink == ctx.addr && parent == ctx.addr
        ));
        assert_eq!(pending.agent_id, "subagent_0001");
        assert_eq!(state.next_subagent_index, 2);
    }

    #[test]
    fn background_start_routes_to_kernel_with_parent_sink() {
        let (ctx, _output_rx) = test_run_context("background_start_bridge");
        let mut state = AgentSessionRuntimeState::new(
            AgentSessionKind::Foreground,
            AgentSessionBinding {
                event_sink: crate::conversation_new::ServiceAddr::channel_id("scratch"),
                parent_addr: None,
            },
        );
        let request = ConversationBridgeRequest {
            request_id: "req_1".to_string(),
            tool_call_id: "call_1".to_string(),
            tool_name: "background_agent_start".to_string(),
            action: "background_agent_start".to_string(),
            payload: serde_json::json!({
                "task": "watch the build",
            }),
        };

        let (call, pending) = child_agent_start_bridge_call(&ctx, &request, &mut state)
            .expect("bridge request parses")
            .expect("background start should route");

        assert_eq!(call.target, crate::conversation_new::ServiceAddr::kernel());
        let decoded = kernel::decode_request(call.payload).expect("kernel request decodes");
        assert!(matches!(
            decoded,
            kernel::KernelRequest::CreateAgentSession {
                kind: AgentSessionKind::Background,
                id: Some(id),
                binding: Some(AgentSessionBinding {
                    event_sink,
                    parent_addr: Some(parent),
                }),
            } if id == "background_0001" && event_sink == ctx.addr && parent == ctx.addr
        ));
        assert_eq!(pending.agent_id, "background_0001");
        assert_eq!(state.next_background_index, 2);
    }

    #[test]
    fn kernel_created_subagent_gets_initial_message_and_started_result() {
        let (ctx, output_rx) = test_run_context("subagent_kernel_created");
        let mut state = AgentSessionRuntimeState::new(
            AgentSessionKind::Foreground,
            AgentSessionBinding {
                event_sink: crate::conversation_new::ServiceAddr::channel_id("scratch"),
                parent_addr: None,
            },
        );
        let mut pending = VecDeque::from([PendingChildAgentStart {
            service_request_id: "svc_1".to_string(),
            request: ConversationBridgeRequest {
                request_id: "req_1".to_string(),
                tool_call_id: "call_1".to_string(),
                tool_name: "subagent_start".to_string(),
                action: "subagent_start".to_string(),
                payload: serde_json::json!({}),
            },
            agent_id: "subagent_0001".to_string(),
            task: "inspect the module".to_string(),
            kind: AgentSessionKind::Subagent,
        }]);
        let mut runner = None;

        handle_kernel_response(
            &ctx,
            &mut runner,
            &mut pending,
            &mut state,
            Some("svc_1"),
            KernelResponse::AgentSessionCreated {
                addr: crate::conversation_new::ServiceAddr::agent_subagent("subagent_0001"),
            },
        )
        .expect("kernel response handled");

        assert!(pending.is_empty());
        assert_eq!(
            state
                .subagents
                .get("subagent_0001")
                .expect("record exists")
                .status,
            ChildAgentRuntimeStatus::Running
        );
        assert!(output_rx.try_iter().any(|output| {
            matches!(
                output,
                ServiceOutput::Call(ServiceCall { target, .. })
                    if target == crate::conversation_new::ServiceAddr::agent_subagent("subagent_0001")
            )
        }));
    }

    #[test]
    fn child_turn_completed_resolves_join_payload() {
        let (ctx, output_rx) = test_run_context("subagent_join_completed");
        let subagent_addr = crate::conversation_new::ServiceAddr::agent_subagent("subagent_0001");
        let mut state = AgentSessionRuntimeState::new(
            AgentSessionKind::Foreground,
            AgentSessionBinding {
                event_sink: crate::conversation_new::ServiceAddr::channel_id("scratch"),
                parent_addr: None,
            },
        );
        state.subagents.insert(
            "subagent_0001".to_string(),
            ChildAgentRuntimeRecord {
                agent_id: "subagent_0001".to_string(),
                addr: subagent_addr.clone(),
                status: ChildAgentRuntimeStatus::Running,
                task: "inspect".to_string(),
                last_message: None,
                last_error: None,
            },
        );
        let mut runner = None;
        let mut joins = VecDeque::new();

        handle_child_session_event(
            &ctx,
            &mut runner,
            &mut joins,
            &mut state,
            subagent_addr.clone(),
            AgentSessionEvent::TurnCompleted {
                message: text_message(ChatRole::Assistant, "done"),
            },
        )
        .expect("child event handled");

        let payload = subagent_join_payload(&state, "subagent_0001");
        assert_eq!(payload["status"], "completed");
        assert_eq!(payload["agent_id"], "subagent_0001");
        assert_eq!(payload["message"]["role"], "assistant");
        assert_eq!(payload["message"]["data"][0]["payload"]["text"], "done");
        assert!(output_rx.try_iter().any(|output| {
            matches!(
                output,
                ServiceOutput::Call(ServiceCall { target, payload, .. })
                    if target == subagent_addr
                        && matches!(
                            agent_session::decode_request(payload.clone()),
                            Ok(AgentSessionRequest::Shutdown { reason })
                                if reason.as_deref() == Some("subagent_completed")
                        )
            )
        }));
    }

    #[test]
    fn background_completion_delivers_and_reinserts_actor_message() {
        let (ctx, output_rx) = test_run_context("background_completion");
        let background_addr =
            crate::conversation_new::ServiceAddr::agent_background("background_0001");
        let mut state = AgentSessionRuntimeState::new(
            AgentSessionKind::Foreground,
            AgentSessionBinding {
                event_sink: crate::conversation_new::ServiceAddr::channel_id("scratch"),
                parent_addr: None,
            },
        );
        state.background_agents.insert(
            "background_0001".to_string(),
            ChildAgentRuntimeRecord {
                agent_id: "background_0001".to_string(),
                addr: background_addr.clone(),
                status: ChildAgentRuntimeStatus::Running,
                task: "watch".to_string(),
                last_message: None,
                last_error: None,
            },
        );
        let mut runner = None;
        let mut joins = VecDeque::new();

        handle_child_session_event(
            &ctx,
            &mut runner,
            &mut joins,
            &mut state,
            background_addr.clone(),
            AgentSessionEvent::TurnCompleted {
                message: text_message(ChatRole::Assistant, "build passed"),
            },
        )
        .expect("background event handled");

        assert_eq!(
            state
                .background_agents
                .get("background_0001")
                .expect("record exists")
                .status,
            ChildAgentRuntimeStatus::Completed
        );
        let outputs = output_rx.try_iter().collect::<Vec<_>>();
        assert!(outputs.iter().any(|output| {
            matches!(
                output,
                ServiceOutput::Call(ServiceCall { target, .. })
                    if target == &ctx.addr
            )
        }));
        assert!(outputs.iter().any(|output| {
            matches!(
                output,
                ServiceOutput::Call(ServiceCall { target, payload, .. })
                    if target == &background_addr
                        && matches!(
                            agent_session::decode_request(payload.clone()),
                            Ok(AgentSessionRequest::Shutdown { reason })
                                if reason.as_deref() == Some("background_agent_completed")
                        )
            )
        }));
    }

    #[test]
    fn pending_subagent_join_can_be_cancelled_by_bridge_request() {
        let (ctx, _output_rx) = test_run_context("subagent_join_cancelled");
        let mut runner = None;
        let mut joins = VecDeque::from([PendingSubagentJoin {
            request: ConversationBridgeRequest {
                request_id: "req_1".to_string(),
                tool_call_id: "call_1".to_string(),
                tool_name: "subagent_join".to_string(),
                action: "subagent_join".to_string(),
                payload: serde_json::json!({}),
            },
            agent_id: "subagent_0001".to_string(),
            deadline: Instant::now() + Duration::from_secs(30),
        }]);
        let request = ConversationBridgeRequest {
            request_id: "req_1_cancel".to_string(),
            tool_call_id: "call_1".to_string(),
            tool_name: "subagent_join".to_string(),
            action: "subagent_join_cancel".to_string(),
            payload: serde_json::json!({
                "request_id": "req_1",
                "reason": "new_user_message",
            }),
        };
        let mut state = AgentSessionRuntimeState::new(
            AgentSessionKind::Foreground,
            AgentSessionBinding {
                event_sink: crate::conversation_new::ServiceAddr::channel_id("scratch"),
                parent_addr: None,
            },
        );

        let response =
            subagent_control_bridge_response(&ctx, &mut runner, &request, &mut state, &mut joins)
                .expect("join cancel handled")
                .expect("cancel request should return ack");

        assert!(joins.is_empty());
        assert_eq!(
            response
                .result
                .result
                .structured
                .as_ref()
                .and_then(|value| {
                    value
                        .get("value")
                        .and_then(|value| value.get("status"))
                        .and_then(serde_json::Value::as_str)
                }),
            Some("cancelled")
        );
    }

    #[test]
    fn subagent_kill_rejects_already_terminal_agent_without_shutdown() {
        let (ctx, output_rx) = test_run_context("subagent_kill_terminal");
        let mut runner = None;
        let mut joins = VecDeque::new();
        let mut state = AgentSessionRuntimeState::new(
            AgentSessionKind::Foreground,
            AgentSessionBinding {
                event_sink: crate::conversation_new::ServiceAddr::channel_id("scratch"),
                parent_addr: None,
            },
        );
        state.subagents.insert(
            "subagent_0001".to_string(),
            ChildAgentRuntimeRecord {
                agent_id: "subagent_0001".to_string(),
                addr: ServiceAddr::agent_subagent("subagent_0001"),
                status: ChildAgentRuntimeStatus::Completed,
                task: "inspect".to_string(),
                last_message: Some(text_message(ChatRole::Assistant, "done")),
                last_error: None,
            },
        );
        let request = ConversationBridgeRequest {
            request_id: "req_1".to_string(),
            tool_call_id: "call_1".to_string(),
            tool_name: "subagent_kill".to_string(),
            action: "subagent_kill".to_string(),
            payload: serde_json::json!({ "agent_id": "subagent_0001" }),
        };

        let response =
            subagent_control_bridge_response(&ctx, &mut runner, &request, &mut state, &mut joins)
                .expect("kill handled")
                .expect("kill should return a bridge result");

        assert_eq!(
            response
                .result
                .result
                .structured
                .as_ref()
                .and_then(|value| {
                    value
                        .get("value")
                        .and_then(|value| value.get("status"))
                        .and_then(serde_json::Value::as_str)
                }),
            Some("failure")
        );
        assert!(output_rx.try_iter().all(|output| !matches!(
            output,
            ServiceOutput::Call(ServiceCall { target, .. })
                if target == ServiceAddr::agent_subagent("subagent_0001")
        )));
    }

    #[test]
    fn repeated_terminal_child_events_shutdown_child_once() {
        let (ctx, output_rx) = test_run_context("subagent_terminal_once");
        let subagent_addr = ServiceAddr::agent_subagent("subagent_0001");
        let mut state = AgentSessionRuntimeState::new(
            AgentSessionKind::Foreground,
            AgentSessionBinding {
                event_sink: ServiceAddr::channel_id("scratch"),
                parent_addr: None,
            },
        );
        state.subagents.insert(
            "subagent_0001".to_string(),
            ChildAgentRuntimeRecord {
                agent_id: "subagent_0001".to_string(),
                addr: subagent_addr.clone(),
                status: ChildAgentRuntimeStatus::Running,
                task: "inspect".to_string(),
                last_message: None,
                last_error: None,
            },
        );
        let mut runner = None;
        let mut joins = VecDeque::new();

        handle_child_session_event(
            &ctx,
            &mut runner,
            &mut joins,
            &mut state,
            subagent_addr.clone(),
            AgentSessionEvent::TurnCompleted {
                message: text_message(ChatRole::Assistant, "done"),
            },
        )
        .expect("first terminal event handled");
        handle_child_session_event(
            &ctx,
            &mut runner,
            &mut joins,
            &mut state,
            subagent_addr.clone(),
            AgentSessionEvent::TurnCompleted {
                message: text_message(ChatRole::Assistant, "done again"),
            },
        )
        .expect("second terminal event handled");

        let shutdown_count = output_rx
            .try_iter()
            .filter(|output| {
                matches!(
                    output,
                    ServiceOutput::Call(ServiceCall { target, payload, .. })
                        if target == &subagent_addr
                            && matches!(
                                agent_session::decode_request(payload.clone()),
                                Ok(AgentSessionRequest::Shutdown { .. })
                            )
                )
            })
            .count();
        assert_eq!(shutdown_count, 1);
    }

    fn test_run_context(
        name: &str,
    ) -> (
        ServiceRunContext,
        crossbeam_channel::Receiver<ServiceOutput>,
    ) {
        let root = std::env::temp_dir().join(format!(
            "stellaclaw-agent-session-{name}-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::SystemTime::UNIX_EPOCH)
                .expect("clock works")
                .as_nanos()
        ));
        let (_inbox_tx, inbox) = crossbeam_channel::unbounded();
        let (outbox, output_rx) = crossbeam_channel::unbounded();
        let (_stop_tx, stop_rx) = crossbeam_channel::bounded(1);
        (
            ServiceRunContext {
                addr: crate::conversation_new::ServiceAddr::agent_foreground_id("scratch"),
                conversation: crate::conversation_new::ConversationRef {
                    conversation_id: name.to_string(),
                    workdir: root.clone(),
                    conversation_root: root.clone(),
                },
                storage: root.join("storage"),
                refs: crate::conversation_new::ServiceRefs::default(),
                inbox,
                outbox,
                stop_rx,
            },
            output_rx,
        )
    }

    fn test_launch_config(agent_server_path: Option<PathBuf>) -> AgentSessionLaunchConfig {
        let mut models = BTreeMap::new();
        models.insert("main".to_string(), test_model_config());
        AgentSessionLaunchConfig {
            session_id: "test_session".to_string(),
            conversation_root: std::env::temp_dir(),
            workspace_root: std::env::temp_dir(),
            agent_server_path,
            session_profile: None,
            models,
            session_defaults: SessionDefaults::default(),
            memory_enabled: false,
            tool_remote_mode: ToolRemoteMode::Selectable,
            sandbox: None,
            reasoning_effort: None,
            idle_timeout_compact_enabled: None,
        }
    }

    fn test_model_config() -> ModelConfig {
        ModelConfig {
            provider_type: ProviderType::OpenRouterCompletion,
            model_name: "test-chat".to_string(),
            url: "https://example.test".to_string(),
            api_key_env: "TEST_API_KEY".to_string(),
            capabilities: vec![ModelCapability::Chat],
            token_max_context: 128_000,
            max_tokens: 0,
            cache_timeout: 0,
            idle_timeout_compact_enabled: true,
            conn_timeout: 30,
            request_timeout: 600,
            max_request_size: 30 * 1024 * 1024,
            retry_mode: Default::default(),
            reasoning: None,
            token_estimator_type: TokenEstimatorType::Local,
            multimodal_estimator: None,
            multimodal_input: None,
            token_estimator_url: None,
        }
    }
}
