#![allow(dead_code)]

use anyhow::Result;
use crossbeam_channel::select;

use crate::{
    channels::web_terminal::{TerminalManager, TerminalRuntimeContext, WebTerminalError},
    conversation_new::{
        ConversationRuntimeConfig, ConversationService, ServiceAddr, ServiceCall, ServiceFailure,
        ServiceOutput, ServiceRunContext, ServiceStatusUpdate, ServiceStopped,
    },
    service_protos::terminal::{
        chunk_snapshot, decode_request, encode_response, replay_snapshot, TerminalRequest,
        TerminalResponse,
    },
};

pub struct TerminalService {
    runtime_config: ConversationRuntimeConfig,
}

impl TerminalService {
    pub fn new(runtime_config: ConversationRuntimeConfig) -> Self {
        Self { runtime_config }
    }
}

impl ConversationService for TerminalService {
    fn run(self: Box<Self>, ctx: ServiceRunContext) -> Result<()> {
        let manager = TerminalManager::new();
        let mut runtime_config = self.runtime_config;
        loop {
            select! {
                recv(ctx.stop_rx) -> stop => {
                    let terminated = manager.terminate_conversation(&ctx.conversation.conversation_id);
                    ctx.outbox.send(ServiceOutput::Status(ServiceStatusUpdate {
                        addr: ctx.addr.clone(),
                        label: "terminals_terminated".to_string(),
                        detail: serde_json::json!({ "count": terminated }),
                    }))?;
                    ctx.outbox.send(ServiceOutput::Stopped(ServiceStopped {
                        addr: ctx.addr.clone(),
                        reason: stop.ok().map(|stop| stop.reason),
                    }))?;
                    return Ok(());
                }
                recv(ctx.inbox) -> call => {
                    let call = call?;
                    match decode_request(call.payload) {
                        Ok(request) => {
                            if let Err(error) = handle_request(
                                &ctx,
                                &manager,
                                &mut runtime_config,
                                call.source,
                                call.request_id.clone(),
                                request,
                            ) {
                                ctx.outbox.send(ServiceOutput::Failed(ServiceFailure {
                                    addr: ctx.addr.clone(),
                                    error: format!("terminal service failed: {error:#}"),
                                }))?;
                            }
                        }
                        Err(error) => {
                            send_response(
                                &ctx,
                                &call.source,
                                TerminalResponse::Error {
                                    code: "bad_terminal_request".to_string(),
                                    message: format!("{error:#}"),
                                },
                                call.request_id.clone(),
                            )?;
                        }
                    }
                }
            }
        }
    }
}

fn handle_request(
    ctx: &ServiceRunContext,
    manager: &TerminalManager,
    runtime_config: &mut ConversationRuntimeConfig,
    source: ServiceAddr,
    response_id: Option<String>,
    request: TerminalRequest,
) -> Result<()> {
    let current_context = terminal_context(ctx, runtime_config);
    match request {
        TerminalRequest::UpdateRuntimeConfig { config } => {
            *runtime_config = config;
            let _ = manager.list_for_context(&terminal_context(ctx, runtime_config));
            ctx.outbox.send(ServiceOutput::Status(ServiceStatusUpdate {
                addr: ctx.addr.clone(),
                label: "runtime_config_updated".to_string(),
                detail: serde_json::json!({
                    "tool_remote_mode": runtime_config.tool_remote_mode,
                }),
            }))?;
            send_response(
                ctx,
                &source,
                TerminalResponse::RuntimeConfigUpdated,
                response_id,
            )
        }
        TerminalRequest::List => send_response(
            ctx,
            &source,
            TerminalResponse::Terminals {
                terminals: manager.list_for_context(&current_context),
            },
            response_id,
        ),
        TerminalRequest::Get { terminal_id } => reply_terminal(
            ctx,
            &source,
            response_id,
            manager.get_for_context(&current_context, &terminal_id),
        ),
        TerminalRequest::Create { request } => reply_terminal(
            ctx,
            &source,
            response_id,
            manager.create_for_context(&current_context, request),
        ),
        TerminalRequest::Terminate { terminal_id } => reply_terminal(
            ctx,
            &source,
            response_id,
            manager.terminate_for_context(&current_context, &terminal_id),
        ),
        TerminalRequest::Input {
            terminal_id,
            encoding,
            data,
        } => match encoding.decode(&data) {
            Ok(bytes) => reply_terminal(
                ctx,
                &source,
                response_id,
                manager.input_bytes_for_context(&current_context, &terminal_id, &bytes),
            ),
            Err(error) => send_response(
                ctx,
                &source,
                TerminalResponse::Error {
                    code: "invalid_terminal_input".to_string(),
                    message: format!("{error:#}"),
                },
                response_id,
            ),
        },
        TerminalRequest::Resize {
            terminal_id,
            request,
        } => reply_terminal(
            ctx,
            &source,
            response_id,
            manager.resize_for_context(&current_context, &terminal_id, request),
        ),
        TerminalRequest::Replay {
            terminal_id,
            offset,
        } => match manager.replay_for_context(&current_context, &terminal_id, offset) {
            Ok(replay) => send_response(
                ctx,
                &source,
                TerminalResponse::Replay {
                    replay: replay_snapshot(replay),
                },
                response_id,
            ),
            Err(error) => reply_error(ctx, &source, response_id, error),
        },
        TerminalRequest::Attach {
            terminal_id,
            offset,
        } => match manager.attach_for_context(&current_context, &terminal_id, offset) {
            Ok(attach) => {
                let subscriber_id = attach.subscriber_id;
                forward_terminal_output(
                    ctx.addr.clone(),
                    source.clone(),
                    ctx.outbox.clone(),
                    terminal_id,
                    subscriber_id,
                    attach.receiver,
                );
                send_response(
                    ctx,
                    &source,
                    TerminalResponse::Attached {
                        replay: replay_snapshot(attach.replay),
                        subscriber_id,
                    },
                    response_id,
                )
            }
            Err(error) => reply_error(ctx, &source, response_id, error),
        },
        TerminalRequest::Detach {
            terminal_id,
            subscriber_id,
        } => match manager.detach_for_context(&current_context, &terminal_id, subscriber_id) {
            Ok(()) => send_response(
                ctx,
                &source,
                TerminalResponse::Detached {
                    terminal_id,
                    subscriber_id,
                },
                response_id,
            ),
            Err(error) => reply_error(ctx, &source, response_id, error),
        },
    }
}

fn terminal_context(
    ctx: &ServiceRunContext,
    runtime_config: &ConversationRuntimeConfig,
) -> TerminalRuntimeContext {
    TerminalRuntimeContext::new(
        ctx.conversation.workdir.clone(),
        ctx.conversation.conversation_id.clone(),
        ctx.conversation.conversation_root.clone(),
        runtime_config.tool_remote_mode.clone(),
    )
}

fn reply_terminal(
    ctx: &ServiceRunContext,
    target: &ServiceAddr,
    response_id: Option<String>,
    result: Result<crate::channels::web_terminal::TerminalSummary, WebTerminalError>,
) -> Result<()> {
    match result {
        Ok(terminal) => send_response(
            ctx,
            target,
            TerminalResponse::Terminal { terminal },
            response_id,
        ),
        Err(error) => reply_error(ctx, target, response_id, error),
    }
}

fn reply_error(
    ctx: &ServiceRunContext,
    target: &ServiceAddr,
    response_id: Option<String>,
    error: WebTerminalError,
) -> Result<()> {
    let code = match &error {
        WebTerminalError::InvalidRequest(_) => "invalid_terminal_request",
        WebTerminalError::NotFound => "terminal_not_found",
        WebTerminalError::LimitExceeded(_) => "terminal_limit_exceeded",
        WebTerminalError::Internal(_) => "terminal_internal_error",
    };
    send_response(
        ctx,
        target,
        TerminalResponse::Error {
            code: code.to_string(),
            message: error.to_string(),
        },
        response_id,
    )
}

fn send_response(
    ctx: &ServiceRunContext,
    target: &ServiceAddr,
    response: TerminalResponse,
    response_id: Option<String>,
) -> Result<()> {
    ctx.outbox
        .send(ServiceOutput::Call(ServiceCall::response_to(
            ctx.addr.clone(),
            target.clone(),
            encode_response(response)?,
            response_id,
        )))?;
    Ok(())
}

fn forward_terminal_output(
    source: ServiceAddr,
    target: ServiceAddr,
    outbox: crossbeam_channel::Sender<ServiceOutput>,
    terminal_id: String,
    subscriber_id: Option<u64>,
    receiver: crossbeam_channel::Receiver<crate::channels::web_terminal::TerminalOutputChunk>,
) {
    std::thread::spawn(move || {
        while let Ok(chunk) = receiver.recv() {
            let response = TerminalResponse::Output {
                terminal_id: terminal_id.clone(),
                subscriber_id,
                encoding: crate::service_protos::terminal::TerminalDataEncoding::Base64,
                data: chunk_snapshot(chunk).data,
            };
            let Ok(payload) = encode_response(response) else {
                break;
            };
            if outbox
                .send(ServiceOutput::Call(ServiceCall::new(
                    source.clone(),
                    target.clone(),
                    payload,
                )))
                .is_err()
            {
                break;
            }
        }
    });
}
