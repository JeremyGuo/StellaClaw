#![allow(dead_code)]

use std::{
    collections::VecDeque,
    sync::atomic::{AtomicU64, Ordering},
};

use anyhow::Result;
use crossbeam_channel::{select, Receiver, Sender};

use crate::{
    conversation_new::{
        ConversationService, ServiceAddr, ServiceOutput, ServiceRunContext, ServiceStatusUpdate,
        ServiceStopped,
    },
    logger::append_workdir_level_log,
    service_protos::{
        agent_session::{
            self, decode_response as decode_agent_response, AgentMessageOrigin, AgentSessionKind,
            AgentSessionResponse,
        },
        channel::{decode_request, ChannelDelivery, ChannelEvent, ChannelIngress, ChannelRequest},
        kernel::{self, decode_response as decode_kernel_response, KernelResponse},
        status::{self, decode_response as decode_status_response, StatusResponse},
        terminal::{self, decode_response as decode_terminal_response, TerminalResponse},
        workspace::{
            self, decode_response as decode_workspace_response, WorkspaceRequest, WorkspaceResponse,
        },
    },
};
use stellaclaw_core::session_actor::{ChatMessage, ChatMessageItem};

pub struct ChannelService {
    ingress_rx: Receiver<ChannelIngress>,
    event_tx: Option<Sender<ChannelEvent>>,
}

impl ChannelService {
    pub fn new() -> Self {
        Self {
            ingress_rx: crossbeam_channel::never(),
            event_tx: None,
        }
    }

    pub fn with_ingress(ingress_rx: Receiver<ChannelIngress>) -> Self {
        Self {
            ingress_rx,
            event_tx: None,
        }
    }

    pub fn with_platform_events(
        ingress_rx: Receiver<ChannelIngress>,
        event_tx: Sender<ChannelEvent>,
    ) -> Self {
        Self {
            ingress_rx,
            event_tx: Some(event_tx),
        }
    }
}

impl ConversationService for ChannelService {
    fn run(self: Box<Self>, ctx: ServiceRunContext) -> Result<()> {
        let ingress_rx = self.ingress_rx.clone();
        let event_tx = self.event_tx.clone();
        let mut pending_workspace = VecDeque::new();
        let mut pending_status = VecDeque::new();
        let mut pending_terminal = VecDeque::new();
        let mut pending_kernel_metadata = VecDeque::new();
        loop {
            select! {
                recv(ctx.stop_rx) -> stop => {
                    let reason = stop.ok().map(|stop| stop.reason);
                    ctx.outbox.send(ServiceOutput::Stopped(ServiceStopped {
                        addr: ctx.addr.clone(),
                        reason,
                    }))?;
                    return Ok(());
                }
                recv(ctx.inbox) -> call => {
                    let call = call?;
                    handle_channel_request(
                        &ctx,
                        event_tx.as_ref(),
                        &mut pending_workspace,
                        &mut pending_status,
                        &mut pending_terminal,
                        &mut pending_kernel_metadata,
                        call.source,
                        call.response_id,
                        call.payload,
                    )?;
                }
                recv(ingress_rx) -> ingress => {
                    let ingress = ingress?;
                    handle_channel_ingress(
                        &ctx,
                        event_tx.as_ref(),
                        &mut pending_workspace,
                        &mut pending_status,
                        &mut pending_terminal,
                        &mut pending_kernel_metadata,
                        ingress,
                    )?;
                }
            }
        }
    }
}

#[derive(Debug)]
enum PendingWorkspaceRequest {
    Platform {
        request_id: String,
    },
    IncomingMessage {
        request_id: String,
        foreground_session_id: Option<String>,
        platform_message_id: Option<String>,
        origin: AgentMessageOrigin,
        metadata: serde_json::Value,
    },
}

impl PendingWorkspaceRequest {
    fn request_id(&self) -> &str {
        match self {
            PendingWorkspaceRequest::Platform { request_id }
            | PendingWorkspaceRequest::IncomingMessage { request_id, .. } => request_id,
        }
    }
}

#[derive(Debug)]
struct PendingTerminalRequest {
    request_id: String,
}

#[derive(Debug)]
struct PendingStatusRequest {
    request_id: String,
}

#[derive(Debug)]
struct PendingKernelMetadataRequest {
    request_id: String,
}

fn handle_channel_request(
    ctx: &ServiceRunContext,
    event_tx: Option<&Sender<ChannelEvent>>,
    pending_workspace: &mut VecDeque<PendingWorkspaceRequest>,
    pending_status: &mut VecDeque<PendingStatusRequest>,
    pending_terminal: &mut VecDeque<PendingTerminalRequest>,
    pending_kernel_metadata: &mut VecDeque<PendingKernelMetadataRequest>,
    source: ServiceAddr,
    response_id: Option<String>,
    payload: serde_json::Value,
) -> Result<()> {
    if response_id.as_deref().is_some_and(|id| {
        pending_workspace
            .iter()
            .any(|pending| pending.request_id() == id)
    }) || source == ServiceAddr::workspace()
    {
        match decode_workspace_response(payload.clone()) {
            Ok(response) => {
                handle_workspace_response(
                    ctx,
                    event_tx,
                    pending_workspace,
                    response_id.as_deref(),
                    response,
                )?;
                return Ok(());
            }
            Err(error) if source == ServiceAddr::workspace() => {
                let detail = serde_json::json!({
                    "conversation_id": &ctx.conversation.conversation_id,
                    "channel_addr": &ctx.addr,
                    "source": &source,
                    "response_id": &response_id,
                    "workspace_decode_error": error.to_string(),
                    "payload": &payload,
                });
                let _ = append_workdir_level_log(
                    &ctx.conversation.workdir,
                    "warn",
                    "bad_workspace_payload",
                    detail.clone(),
                );
                emit_channel_event(
                    event_tx,
                    ChannelEvent::Error {
                        code: "channel.bad_workspace_payload".to_string(),
                        message: "Channel received an unsupported workspace response.".to_string(),
                        detail: Some(error.to_string()),
                    },
                )?;
                ctx.outbox.send(ServiceOutput::Status(ServiceStatusUpdate {
                    addr: ctx.addr.clone(),
                    label: "bad_workspace_payload".to_string(),
                    detail,
                }))?;
                return Ok(());
            }
            Err(_) => {}
        }
    }

    match decode_request(payload.clone()) {
        Ok(ChannelRequest::Deliver { delivery }) => {
            let text = delivery.text.clone();
            let chars = text.chars().count();
            emit_channel_event(
                event_tx,
                ChannelEvent::Delivery {
                    delivery: delivery.clone(),
                    text,
                },
            )?;
            ctx.outbox.send(ServiceOutput::Status(ServiceStatusUpdate {
                addr: ctx.addr.clone(),
                label: "delivered_text".to_string(),
                detail: serde_json::json!({
                    "session_addr": delivery.session_addr,
                    "chars": chars,
                    "attachments": delivery.attachments.len(),
                    "has_message": delivery.message.is_some(),
                }),
            }))?;
        }
        Ok(ChannelRequest::SessionEvent {
            session_addr,
            event,
        }) => handle_session_event(ctx, event_tx, session_addr, event)?,
        Ok(ChannelRequest::Status { label, detail }) => {
            emit_channel_event(
                event_tx,
                ChannelEvent::Status {
                    label: label.clone(),
                    detail: detail.clone(),
                },
            )?;
            ctx.outbox.send(ServiceOutput::Status(ServiceStatusUpdate {
                addr: ctx.addr.clone(),
                label,
                detail,
            }))?;
        }
        Ok(ChannelRequest::Error {
            code,
            message,
            detail,
        }) => {
            emit_channel_event(
                event_tx,
                ChannelEvent::Error {
                    code: code.clone(),
                    message: message.clone(),
                    detail: detail.clone(),
                },
            )?;
            ctx.outbox.send(ServiceOutput::Status(ServiceStatusUpdate {
                addr: ctx.addr.clone(),
                label: "error".to_string(),
                detail: serde_json::json!({
                    "code": code,
                    "message": message,
                    "detail": detail,
                }),
            }))?;
        }
        Err(error) => match decode_agent_response(payload.clone()) {
            Ok(AgentSessionResponse::Accepted) => {
                emit_channel_event(event_tx, ChannelEvent::AgentSessionAccepted)?;
                ctx.outbox.send(ServiceOutput::Status(ServiceStatusUpdate {
                    addr: ctx.addr.clone(),
                    label: "agent_session_accepted".to_string(),
                    detail: serde_json::json!({}),
                }))?;
            }
            Ok(AgentSessionResponse::Status { status }) => {
                emit_channel_event(
                    event_tx,
                    ChannelEvent::AgentSessionStatus {
                        status: status.clone(),
                    },
                )?;
                ctx.outbox.send(ServiceOutput::Status(ServiceStatusUpdate {
                    addr: ctx.addr.clone(),
                    label: "agent_session_status".to_string(),
                    detail: serde_json::to_value(status)?,
                }))?;
            }
            Ok(AgentSessionResponse::Context { query_id, context }) => {
                emit_channel_event(
                    event_tx,
                    ChannelEvent::ForegroundContext {
                        query_id: query_id.clone(),
                        context: context.clone(),
                    },
                )?;
                ctx.outbox.send(ServiceOutput::Status(ServiceStatusUpdate {
                    addr: ctx.addr.clone(),
                    label: "foreground_context".to_string(),
                    detail: serde_json::json!({
                        "query_id": query_id,
                        "context": context,
                    }),
                }))?;
            }
            Ok(AgentSessionResponse::Stopped) => {
                emit_channel_event(event_tx, ChannelEvent::AgentSessionStopped)?;
                ctx.outbox.send(ServiceOutput::Status(ServiceStatusUpdate {
                    addr: ctx.addr.clone(),
                    label: "agent_session_stopped".to_string(),
                    detail: serde_json::json!({}),
                }))?;
            }
            Ok(AgentSessionResponse::Rejected { reason }) => {
                emit_channel_event(
                    event_tx,
                    ChannelEvent::AgentSessionRejected {
                        reason: reason.clone(),
                    },
                )?;
                ctx.outbox.send(ServiceOutput::Status(ServiceStatusUpdate {
                    addr: ctx.addr.clone(),
                    label: "agent_session_rejected".to_string(),
                    detail: serde_json::json!({"reason": reason}),
                }))?;
            }
            Err(_) => match decode_kernel_response(payload.clone()) {
                Ok(KernelResponse::AgentSessionCreated { addr }) => {
                    emit_channel_event(
                        event_tx,
                        ChannelEvent::AgentSessionCreated { addr: addr.clone() },
                    )?;
                    ctx.outbox.send(ServiceOutput::Status(ServiceStatusUpdate {
                        addr: ctx.addr.clone(),
                        label: "agent_session_created".to_string(),
                        detail: serde_json::json!({"addr": addr}),
                    }))?;
                }
                Ok(KernelResponse::Error { code, message }) => {
                    emit_channel_event(
                        event_tx,
                        ChannelEvent::Error {
                            code: code.clone(),
                            message: message.clone(),
                            detail: None,
                        },
                    )?;
                    ctx.outbox.send(ServiceOutput::Status(ServiceStatusUpdate {
                        addr: ctx.addr.clone(),
                        label: "error".to_string(),
                        detail: serde_json::json!({
                            "code": code,
                            "message": message,
                        }),
                    }))?;
                }
                Ok(KernelResponse::RuntimeConfigUpdated {
                    config,
                    updated_services,
                }) => {
                    let detail = serde_json::json!({
                        "agent_server_configured": config.agent_server_path.is_some(),
                        "has_session_profile": config.session_profile.is_some(),
                        "model_count": config.models.len(),
                        "memory_enabled": config.memory_enabled,
                        "tool_remote_mode": config.tool_remote_mode,
                        "has_sandbox_override": config.sandbox.is_some(),
                        "reasoning_effort": config.reasoning_effort,
                        "updated_services": updated_services,
                    });
                    emit_channel_event(
                        event_tx,
                        ChannelEvent::Status {
                            label: "runtime_config_updated".to_string(),
                            detail: detail.clone(),
                        },
                    )?;
                    ctx.outbox.send(ServiceOutput::Status(ServiceStatusUpdate {
                        addr: ctx.addr.clone(),
                        label: "runtime_config_updated".to_string(),
                        detail,
                    }))?;
                }
                Ok(response @ KernelResponse::Metadata { .. })
                | Ok(response @ KernelResponse::MetadataUpdated { .. }) => {
                    handle_kernel_metadata_response(
                        ctx,
                        event_tx,
                        pending_kernel_metadata,
                        response,
                    )?;
                }
                Ok(response) => {
                    ctx.outbox.send(ServiceOutput::Status(ServiceStatusUpdate {
                        addr: ctx.addr.clone(),
                        label: "kernel_response".to_string(),
                        detail: serde_json::to_value(response)?,
                    }))?;
                }
                Err(_) => match decode_workspace_response(payload.clone()) {
                    Ok(response) => {
                        handle_workspace_response(
                            ctx,
                            event_tx,
                            pending_workspace,
                            response_id.as_deref(),
                            response,
                        )?;
                    }
                    Err(_) => match decode_status_response(payload.clone()) {
                        Ok(response) => {
                            handle_status_response(ctx, event_tx, pending_status, response)?;
                        }
                        Err(_) => match decode_terminal_response(payload.clone()) {
                            Ok(response) => {
                                handle_terminal_response(
                                    ctx,
                                    event_tx,
                                    pending_terminal,
                                    response,
                                )?;
                            }
                            Err(_) => {
                                let detail = serde_json::json!({
                                    "conversation_id": &ctx.conversation.conversation_id,
                                    "channel_addr": &ctx.addr,
                                    "source": &source,
                                    "channel_decode_error": error.to_string(),
                                    "payload": &payload,
                                });
                                let _ = append_workdir_level_log(
                                    &ctx.conversation.workdir,
                                    "warn",
                                    "bad_channel_payload",
                                    detail.clone(),
                                );
                                emit_channel_event(
                                    event_tx,
                                    ChannelEvent::Error {
                                        code: "channel.bad_payload".to_string(),
                                        message: "Channel received an unsupported payload."
                                            .to_string(),
                                        detail: Some(error.to_string()),
                                    },
                                )?;
                                ctx.outbox.send(ServiceOutput::Status(ServiceStatusUpdate {
                                    addr: ctx.addr.clone(),
                                    label: "bad_channel_payload".to_string(),
                                    detail,
                                }))?;
                            }
                        },
                    },
                },
            },
        },
    }
    Ok(())
}

fn handle_channel_ingress(
    ctx: &ServiceRunContext,
    event_tx: Option<&Sender<ChannelEvent>>,
    pending_workspace: &mut VecDeque<PendingWorkspaceRequest>,
    pending_status: &mut VecDeque<PendingStatusRequest>,
    pending_terminal: &mut VecDeque<PendingTerminalRequest>,
    pending_kernel_metadata: &mut VecDeque<PendingKernelMetadataRequest>,
    ingress: ChannelIngress,
) -> Result<()> {
    match ingress {
        ChannelIngress::IncomingMessage {
            foreground_session_id,
            platform_message_id,
            origin,
            message,
            metadata,
        } => {
            let origin = origin.unwrap_or(AgentMessageOrigin::User);
            let target = foreground_target(ctx, foreground_session_id.as_deref());
            if message_needs_materialization(&message) {
                let request_id = channel_service_request_id("workspace-materialize");
                pending_workspace.push_back(PendingWorkspaceRequest::IncomingMessage {
                    request_id: request_id.clone(),
                    foreground_session_id,
                    platform_message_id,
                    origin,
                    metadata,
                });
                ctx.outbox.send(ServiceOutput::Call(
                    workspace::workspace_call(
                        ctx.addr.clone(),
                        WorkspaceRequest::MaterializeMessage { message },
                    )?
                    .with_request_id(request_id),
                ))?;
                return Ok(());
            }
            ctx.outbox.send(ServiceOutput::Status(ServiceStatusUpdate {
                addr: ctx.addr.clone(),
                label: "incoming_message".to_string(),
                detail: serde_json::json!({
                    "platform_message_id": platform_message_id,
                    "channel_id": channel_id(&ctx.addr),
                    "foreground_session_id": foreground_session_id,
                    "origin": origin,
                    "role": message.role,
                    "items": message.data.len(),
                    "metadata": metadata,
                }),
            }))?;
            ctx.outbox
                .send(ServiceOutput::Call(agent_session::enqueue_message_call(
                    ctx.addr.clone(),
                    target,
                    origin,
                    message,
                    platform_message_id,
                )?))?;
        }
        ChannelIngress::QueryForegroundContext {
            foreground_session_id,
            query_id,
            payload,
        } => {
            ctx.outbox
                .send(ServiceOutput::Call(agent_session::query_context_call(
                    ctx.addr.clone(),
                    foreground_target(ctx, foreground_session_id.as_deref()),
                    query_id,
                    payload,
                )?))?;
        }
        ChannelIngress::QueryForegroundStatus {
            foreground_session_id,
        } => {
            ctx.outbox
                .send(ServiceOutput::Call(agent_session::query_status_call(
                    ctx.addr.clone(),
                    foreground_target(ctx, foreground_session_id.as_deref()),
                )?))?;
        }
        ChannelIngress::CreateForegroundSession { requested_id } => {
            ctx.outbox
                .send(ServiceOutput::Call(kernel::create_agent_session_call(
                    ctx.addr.clone(),
                    AgentSessionKind::Foreground,
                    requested_id,
                )?))?;
        }
        ChannelIngress::CancelForegroundTurn {
            foreground_session_id,
            reason,
        } => {
            ctx.outbox
                .send(ServiceOutput::Call(agent_session::cancel_turn_call(
                    ctx.addr.clone(),
                    foreground_target(ctx, foreground_session_id.as_deref()),
                    reason,
                )?))?;
        }
        ChannelIngress::ContinueForegroundTurn {
            foreground_session_id,
            reason,
        } => {
            ctx.outbox
                .send(ServiceOutput::Call(agent_session::continue_turn_call(
                    ctx.addr.clone(),
                    foreground_target(ctx, foreground_session_id.as_deref()),
                    reason,
                )?))?;
        }
        ChannelIngress::CompactForegroundNow {
            foreground_session_id,
        } => {
            ctx.outbox
                .send(ServiceOutput::Call(agent_session::compact_now_call(
                    ctx.addr.clone(),
                    foreground_target(ctx, foreground_session_id.as_deref()),
                )?))?;
        }
        ChannelIngress::DeleteForegroundSession {
            foreground_session_id,
            reason,
        } => {
            ctx.outbox
                .send(ServiceOutput::Call(agent_session::shutdown_call(
                    ctx.addr.clone(),
                    foreground_target(ctx, foreground_session_id.as_deref()),
                    reason,
                )?))?;
        }
        ChannelIngress::ResolveHostCoordination {
            foreground_session_id,
            response,
        } => {
            ctx.outbox.send(ServiceOutput::Call(
                agent_session::resolve_host_coordination_call(
                    ctx.addr.clone(),
                    foreground_target(ctx, foreground_session_id.as_deref()),
                    response,
                )?,
            ))?;
        }
        ChannelIngress::UpdateRuntimeConfig { patch } => {
            emit_channel_event(
                event_tx,
                ChannelEvent::Status {
                    label: "runtime_config_update_requested".to_string(),
                    detail: serde_json::json!({}),
                },
            )?;
            ctx.outbox
                .send(ServiceOutput::Call(kernel::update_runtime_config_call(
                    ctx.addr.clone(),
                    patch,
                )?))?;
        }
        ChannelIngress::QueryKernelMetadata { request_id } => {
            pending_kernel_metadata.push_back(PendingKernelMetadataRequest {
                request_id: request_id.clone(),
            });
            ctx.outbox
                .send(ServiceOutput::Call(kernel::query_metadata_call(
                    ctx.addr.clone(),
                )?))?;
        }
        ChannelIngress::UpdateKernelMetadata { request_id, patch } => {
            pending_kernel_metadata.push_back(PendingKernelMetadataRequest {
                request_id: request_id.clone(),
            });
            ctx.outbox
                .send(ServiceOutput::Call(kernel::update_metadata_call(
                    ctx.addr.clone(),
                    patch,
                )?))?;
        }
        ChannelIngress::Workspace {
            request_id,
            request,
        } => {
            pending_workspace.push_back(PendingWorkspaceRequest::Platform {
                request_id: request_id.clone(),
            });
            emit_channel_event(
                event_tx,
                ChannelEvent::Status {
                    label: "workspace_request_forwarded".to_string(),
                    detail: serde_json::json!({"request_id": request_id}),
                },
            )?;
            ctx.outbox.send(ServiceOutput::Call(
                workspace::workspace_call(ctx.addr.clone(), request)?.with_request_id(request_id),
            ))?;
        }
        ChannelIngress::Status {
            request_id,
            request,
        } => {
            pending_status.push_back(PendingStatusRequest {
                request_id: request_id.clone(),
            });
            emit_channel_event(
                event_tx,
                ChannelEvent::Status {
                    label: "status_request_forwarded".to_string(),
                    detail: serde_json::json!({"request_id": request_id}),
                },
            )?;
            ctx.outbox.send(ServiceOutput::Call(status::status_call(
                ctx.addr.clone(),
                ServiceAddr::status(),
                request,
            )?))?;
        }
        ChannelIngress::Terminal {
            request_id,
            request,
        } => {
            pending_terminal.push_back(PendingTerminalRequest {
                request_id: request_id.clone(),
            });
            emit_channel_event(
                event_tx,
                ChannelEvent::Status {
                    label: "terminal_request_forwarded".to_string(),
                    detail: serde_json::json!({"request_id": request_id}),
                },
            )?;
            ctx.outbox
                .send(ServiceOutput::Call(terminal::terminal_call(
                    ctx.addr.clone(),
                    ServiceAddr::terminal(),
                    request,
                )?))?;
        }
    }
    Ok(())
}

fn handle_kernel_metadata_response(
    ctx: &ServiceRunContext,
    event_tx: Option<&Sender<ChannelEvent>>,
    pending_kernel_metadata: &mut VecDeque<PendingKernelMetadataRequest>,
    response: KernelResponse,
) -> Result<()> {
    let request_id = pending_kernel_metadata
        .pop_front()
        .map(|pending| pending.request_id)
        .unwrap_or_default();
    emit_channel_event(
        event_tx,
        ChannelEvent::KernelMetadata {
            request_id: request_id.clone(),
            response: response.clone(),
        },
    )?;
    ctx.outbox.send(ServiceOutput::Status(ServiceStatusUpdate {
        addr: ctx.addr.clone(),
        label: "kernel_metadata_response".to_string(),
        detail: serde_json::json!({
            "request_id": request_id,
            "response": response,
        }),
    }))?;
    Ok(())
}

fn handle_status_response(
    ctx: &ServiceRunContext,
    event_tx: Option<&Sender<ChannelEvent>>,
    pending_status: &mut VecDeque<PendingStatusRequest>,
    response: StatusResponse,
) -> Result<()> {
    let request_id = pending_status
        .pop_front()
        .map(|pending| pending.request_id)
        .unwrap_or_default();
    emit_channel_event(
        event_tx,
        ChannelEvent::StatusSnapshot {
            request_id: request_id.clone(),
            response: response.clone(),
        },
    )?;
    ctx.outbox.send(ServiceOutput::Status(ServiceStatusUpdate {
        addr: ctx.addr.clone(),
        label: "status_response".to_string(),
        detail: serde_json::json!({
            "request_id": request_id,
            "response": response,
        }),
    }))?;
    Ok(())
}

fn handle_workspace_response(
    ctx: &ServiceRunContext,
    event_tx: Option<&Sender<ChannelEvent>>,
    pending_workspace: &mut VecDeque<PendingWorkspaceRequest>,
    response_id: Option<&str>,
    response: WorkspaceResponse,
) -> Result<()> {
    match pop_pending_workspace(pending_workspace, response_id) {
        Some(PendingWorkspaceRequest::Platform { request_id }) => {
            emit_channel_event(
                event_tx,
                ChannelEvent::Workspace {
                    request_id: request_id.clone(),
                    response: response.clone(),
                },
            )?;
            ctx.outbox.send(ServiceOutput::Status(ServiceStatusUpdate {
                addr: ctx.addr.clone(),
                label: "workspace_response".to_string(),
                detail: serde_json::json!({
                    "request_id": request_id,
                    "response": response,
                }),
            }))?;
        }
        Some(PendingWorkspaceRequest::IncomingMessage {
            request_id,
            foreground_session_id,
            platform_message_id,
            origin,
            metadata,
        }) => {
            let WorkspaceResponse::MessageMaterialized { message } = response else {
                ctx.outbox.send(ServiceOutput::Failed(
                    crate::conversation_new::ServiceFailure {
                        addr: ctx.addr.clone(),
                        error: "workspace returned non-message response for incoming message"
                            .to_string(),
                    },
                ))?;
                return Ok(());
            };
            ctx.outbox.send(ServiceOutput::Status(ServiceStatusUpdate {
                addr: ctx.addr.clone(),
                label: "incoming_message_materialized".to_string(),
                detail: serde_json::json!({
                    "request_id": request_id,
                    "platform_message_id": platform_message_id,
                    "channel_id": channel_id(&ctx.addr),
                    "foreground_session_id": foreground_session_id,
                    "origin": origin,
                    "role": message.role,
                    "items": message.data.len(),
                    "metadata": metadata,
                }),
            }))?;
            ctx.outbox
                .send(ServiceOutput::Call(agent_session::enqueue_message_call(
                    ctx.addr.clone(),
                    foreground_target(ctx, foreground_session_id.as_deref()),
                    origin,
                    message,
                    platform_message_id,
                )?))?;
        }
        None => {
            emit_channel_event(
                event_tx,
                ChannelEvent::Error {
                    code: "channel.unexpected_workspace_response".to_string(),
                    message: "Workspace response arrived without a pending channel request."
                        .to_string(),
                    detail: Some(serde_json::to_string(&response)?),
                },
            )?;
            ctx.outbox.send(ServiceOutput::Status(ServiceStatusUpdate {
                addr: ctx.addr.clone(),
                label: "unexpected_workspace_response".to_string(),
                detail: serde_json::to_value(response)?,
            }))?;
        }
    }
    Ok(())
}

fn pop_pending_workspace(
    pending_workspace: &mut VecDeque<PendingWorkspaceRequest>,
    response_id: Option<&str>,
) -> Option<PendingWorkspaceRequest> {
    if let Some(response_id) = response_id {
        if let Some(index) = pending_workspace
            .iter()
            .position(|pending| pending.request_id() == response_id)
        {
            return pending_workspace.remove(index);
        }
    }
    pending_workspace.pop_front()
}

fn handle_terminal_response(
    ctx: &ServiceRunContext,
    event_tx: Option<&Sender<ChannelEvent>>,
    pending_terminal: &mut VecDeque<PendingTerminalRequest>,
    response: TerminalResponse,
) -> Result<()> {
    let request_id = match &response {
        TerminalResponse::Output { .. } => None,
        _ => pending_terminal
            .pop_front()
            .map(|pending| pending.request_id),
    };
    emit_channel_event(
        event_tx,
        ChannelEvent::Terminal {
            request_id: request_id.clone(),
            response: response.clone(),
        },
    )?;
    ctx.outbox.send(ServiceOutput::Status(ServiceStatusUpdate {
        addr: ctx.addr.clone(),
        label: "terminal_response".to_string(),
        detail: serde_json::json!({
            "request_id": request_id,
            "response": response,
        }),
    }))?;
    Ok(())
}

fn message_needs_materialization(message: &ChatMessage) -> bool {
    message.data.iter().any(|item| {
        matches!(
            item,
            ChatMessageItem::File(file) if file.uri.starts_with("data:")
        )
    })
}

fn channel_id(addr: &crate::conversation_new::ServiceAddr) -> &str {
    addr.local_service_id("channel").unwrap_or("main")
}

fn foreground_target(ctx: &ServiceRunContext, foreground_session_id: Option<&str>) -> ServiceAddr {
    crate::conversation_new::ServiceAddr::agent_foreground_id(
        foreground_session_id.unwrap_or_else(|| channel_id(&ctx.addr)),
    )
}

fn emit_channel_event(event_tx: Option<&Sender<ChannelEvent>>, event: ChannelEvent) -> Result<()> {
    if let Some(event_tx) = event_tx {
        event_tx.send(event)?;
    }
    Ok(())
}

static CHANNEL_SERVICE_REQUEST_COUNTER: AtomicU64 = AtomicU64::new(1);

fn channel_service_request_id(prefix: &str) -> String {
    let id = CHANNEL_SERVICE_REQUEST_COUNTER.fetch_add(1, Ordering::Relaxed);
    format!("{prefix}-{id}")
}

fn session_event_belongs_to_channel(
    channel_addr: &ServiceAddr,
    session_addr: &ServiceAddr,
) -> bool {
    let channel_id = channel_id(channel_addr);
    if let Some(foreground_id) = local_agent_id(session_addr, "foreground") {
        return foreground_id == channel_id || channel_id == "main";
    }
    if local_agent_id(session_addr, "subagent").is_some() {
        return true;
    }
    false
}

fn local_agent_id<'a>(addr: &'a ServiceAddr, kind: &str) -> Option<&'a str> {
    if addr.scope == crate::conversation_new::ServiceScope::Local
        && addr.path.len() == 3
        && addr.path.first().map(String::as_str) == Some("agent")
        && addr.path.get(1).map(String::as_str) == Some(kind)
    {
        addr.path.get(2).map(String::as_str)
    } else {
        None
    }
}

fn handle_session_event(
    ctx: &ServiceRunContext,
    event_tx: Option<&Sender<ChannelEvent>>,
    session_addr: ServiceAddr,
    event: agent_session::AgentSessionEvent,
) -> Result<()> {
    if !session_event_belongs_to_channel(&ctx.addr, &session_addr) {
        emit_channel_event(
            event_tx,
            ChannelEvent::Error {
                code: "channel.foreign_session_event".to_string(),
                message: "Session event does not belong to this channel.".to_string(),
                detail: Some(session_addr.to_string()),
            },
        )?;
        ctx.outbox.send(ServiceOutput::Status(ServiceStatusUpdate {
            addr: ctx.addr.clone(),
            label: "error".to_string(),
            detail: serde_json::json!({
                "session_addr": session_addr,
                "code": "channel.foreign_session_event",
                "message": "Session event does not belong to this channel.",
            }),
        }))?;
        return Ok(());
    }
    emit_channel_event(
        event_tx,
        ChannelEvent::SessionEvent {
            session_addr: session_addr.clone(),
            event: event.clone(),
        },
    )?;
    match event {
        agent_session::AgentSessionEvent::MessageAppended { index, message } => {
            ctx.outbox.send(ServiceOutput::Status(ServiceStatusUpdate {
                addr: ctx.addr.clone(),
                label: "message_appended".to_string(),
                detail: serde_json::json!({
                    "session_addr": session_addr,
                    "index": index,
                    "role": message.role,
                    "items": message.data.len(),
                }),
            }))?;
        }
        agent_session::AgentSessionEvent::TurnStarted { turn_id, plan } => {
            ctx.outbox.send(ServiceOutput::Status(ServiceStatusUpdate {
                addr: ctx.addr.clone(),
                label: "processing".to_string(),
                detail: serde_json::json!({
                    "session_addr": session_addr,
                    "active": true,
                    "turn_id": turn_id,
                    "plan": plan,
                }),
            }))?;
        }
        agent_session::AgentSessionEvent::Progress { message, plan } => {
            ctx.outbox.send(ServiceOutput::Status(ServiceStatusUpdate {
                addr: ctx.addr.clone(),
                label: "progress_feedback".to_string(),
                detail: serde_json::json!({
                    "session_addr": session_addr,
                    "message": message,
                    "plan": plan,
                }),
            }))?;
        }
        agent_session::AgentSessionEvent::PlanUpdated { plan } => {
            ctx.outbox.send(ServiceOutput::Status(ServiceStatusUpdate {
                addr: ctx.addr.clone(),
                label: "plan_updated".to_string(),
                detail: serde_json::json!({
                    "session_addr": session_addr,
                    "plan": plan,
                }),
            }))?;
        }
        agent_session::AgentSessionEvent::StreamAssistantMessageDelta {
            message_id, delta, ..
        } => {
            ctx.outbox.send(ServiceOutput::Status(ServiceStatusUpdate {
                addr: ctx.addr.clone(),
                label: "stream_assistant_message_delta".to_string(),
                detail: serde_json::json!({
                    "session_addr": session_addr,
                    "message_id": message_id,
                    "chars": delta.chars().count(),
                }),
            }))?;
        }
        agent_session::AgentSessionEvent::StreamToolCallDelta {
            message_id, delta, ..
        } => {
            ctx.outbox.send(ServiceOutput::Status(ServiceStatusUpdate {
                addr: ctx.addr.clone(),
                label: "stream_tool_call_delta".to_string(),
                detail: serde_json::json!({
                    "session_addr": session_addr,
                    "message_id": message_id,
                    "chars": delta.chars().count(),
                }),
            }))?;
        }
        agent_session::AgentSessionEvent::StreamReasoningSummaryDelta {
            message_id,
            summary_index,
            delta,
            ..
        } => {
            ctx.outbox.send(ServiceOutput::Status(ServiceStatusUpdate {
                addr: ctx.addr.clone(),
                label: "stream_reasoning_summary_delta".to_string(),
                detail: serde_json::json!({
                    "session_addr": session_addr,
                    "message_id": message_id,
                    "summary_index": summary_index,
                    "chars": delta.chars().count(),
                }),
            }))?;
        }
        agent_session::AgentSessionEvent::StreamReasoningSummaryPartAdded {
            message_id,
            summary_index,
            ..
        } => {
            ctx.outbox.send(ServiceOutput::Status(ServiceStatusUpdate {
                addr: ctx.addr.clone(),
                label: "stream_reasoning_summary_part_added".to_string(),
                detail: serde_json::json!({
                    "session_addr": session_addr,
                    "message_id": message_id,
                    "summary_index": summary_index,
                }),
            }))?;
        }
        agent_session::AgentSessionEvent::StreamError {
            message_id, error, ..
        } => {
            ctx.outbox.send(ServiceOutput::Status(ServiceStatusUpdate {
                addr: ctx.addr.clone(),
                label: "stream_error".to_string(),
                detail: serde_json::json!({
                    "session_addr": session_addr,
                    "message_id": message_id,
                    "error": error,
                }),
            }))?;
        }
        agent_session::AgentSessionEvent::StreamToolResultDone {
            turn_id,
            batch_id,
            tool_result,
        } => {
            ctx.outbox.send(ServiceOutput::Status(ServiceStatusUpdate {
                addr: ctx.addr.clone(),
                label: "stream_tool_result_done".to_string(),
                detail: serde_json::json!({
                    "session_addr": session_addr,
                    "turn_id": turn_id,
                    "batch_id": batch_id,
                    "tool_name": tool_result.tool_name,
                    "tool_call_id": tool_result.tool_call_id,
                }),
            }))?;
        }
        agent_session::AgentSessionEvent::TurnCompleted { message } => {
            ctx.outbox.send(ServiceOutput::Status(ServiceStatusUpdate {
                addr: ctx.addr.clone(),
                label: "processing".to_string(),
                detail: serde_json::json!({
                    "session_addr": session_addr,
                    "active": false,
                }),
            }))?;
            emit_channel_event(
                event_tx,
                ChannelEvent::Delivery {
                    delivery: ChannelDelivery {
                        session_addr: Some(session_addr.clone()),
                        message: Some(message),
                        text: String::new(),
                        attachments: Vec::new(),
                        options: None,
                    },
                    text: String::new(),
                },
            )?;
            ctx.outbox.send(ServiceOutput::Status(ServiceStatusUpdate {
                addr: ctx.addr.clone(),
                label: "delivered_text".to_string(),
                detail: serde_json::json!({
                    "session_addr": session_addr,
                    "chars": 0,
                    "has_message": true,
                }),
            }))?;
        }
        agent_session::AgentSessionEvent::TurnFailed {
            error,
            error_detail,
            can_continue,
        } => {
            ctx.outbox.send(ServiceOutput::Status(ServiceStatusUpdate {
                addr: ctx.addr.clone(),
                label: "error".to_string(),
                detail: serde_json::json!({
                    "session_addr": session_addr,
                    "code": "turn_failed",
                    "message": error,
                    "detail": error_detail,
                    "can_continue": can_continue,
                }),
            }))?;
        }
        agent_session::AgentSessionEvent::HostCoordinationRequested { request } => {
            ctx.outbox.send(ServiceOutput::Status(ServiceStatusUpdate {
                addr: ctx.addr.clone(),
                label: "host_coordination_requested".to_string(),
                detail: serde_json::json!({
                    "session_addr": session_addr,
                    "request": request,
                }),
            }))?;
        }
        agent_session::AgentSessionEvent::InteractiveOutputRequested { payload } => {
            ctx.outbox.send(ServiceOutput::Status(ServiceStatusUpdate {
                addr: ctx.addr.clone(),
                label: "interactive_output_requested".to_string(),
                detail: serde_json::json!({
                    "session_addr": session_addr,
                    "payload": payload,
                }),
            }))?;
        }
        agent_session::AgentSessionEvent::SessionViewResult { query_id, payload } => {
            ctx.outbox.send(ServiceOutput::Status(ServiceStatusUpdate {
                addr: ctx.addr.clone(),
                label: "session_view_result".to_string(),
                detail: serde_json::json!({
                    "session_addr": session_addr,
                    "query_id": query_id,
                    "payload": payload,
                }),
            }))?;
        }
        agent_session::AgentSessionEvent::CompactCompleted {
            compressed,
            estimated_tokens_before,
            estimated_tokens_after,
            threshold_tokens,
            retained_message_count,
            compressed_message_count,
        } => {
            ctx.outbox.send(ServiceOutput::Status(ServiceStatusUpdate {
                addr: ctx.addr.clone(),
                label: "compact_completed".to_string(),
                detail: serde_json::json!({
                    "session_addr": session_addr,
                    "compressed": compressed,
                    "estimated_tokens_before": estimated_tokens_before,
                    "estimated_tokens_after": estimated_tokens_after,
                    "threshold_tokens": threshold_tokens,
                    "retained_message_count": retained_message_count,
                    "compressed_message_count": compressed_message_count,
                }),
            }))?;
        }
        agent_session::AgentSessionEvent::CompactFailed { phase, reason } => {
            ctx.outbox.send(ServiceOutput::Status(ServiceStatusUpdate {
                addr: ctx.addr.clone(),
                label: "error".to_string(),
                detail: serde_json::json!({
                    "session_addr": session_addr,
                    "code": "compact_failed",
                    "phase": phase,
                    "message": reason,
                }),
            }))?;
        }
        agent_session::AgentSessionEvent::ControlRejected { reason, payload } => {
            ctx.outbox.send(ServiceOutput::Status(ServiceStatusUpdate {
                addr: ctx.addr.clone(),
                label: "error".to_string(),
                detail: serde_json::json!({
                    "session_addr": session_addr,
                    "code": "control_rejected",
                    "message": reason,
                    "payload": payload,
                }),
            }))?;
        }
        agent_session::AgentSessionEvent::RuntimeCrashed {
            error,
            error_detail,
        } => {
            ctx.outbox.send(ServiceOutput::Status(ServiceStatusUpdate {
                addr: ctx.addr.clone(),
                label: "error".to_string(),
                detail: serde_json::json!({
                    "session_addr": session_addr,
                    "code": "session_runtime_crashed",
                    "message": error,
                    "detail": error_detail,
                }),
            }))?;
        }
    }
    Ok(())
}
