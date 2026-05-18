#![allow(dead_code)]

use anyhow::Result;
use crossbeam_channel::select;

use crate::{
    conversation_new::{
        ConversationService, ServiceCall, ServiceOutput, ServiceRunContext, ServiceStopped,
    },
    service_protos::control::{decode_request, encode_response, ControlRequest, ControlResponse},
};

pub struct ControlService;

impl ControlService {
    pub fn new() -> Self {
        Self
    }
}

impl ConversationService for ControlService {
    fn run(self: Box<Self>, ctx: ServiceRunContext) -> Result<()> {
        loop {
            select! {
                recv(ctx.stop_rx) -> stop => {
                    ctx.outbox.send(ServiceOutput::Stopped(ServiceStopped {
                        addr: ctx.addr.clone(),
                        reason: stop.ok().map(|stop| stop.reason),
                    }))?;
                    return Ok(());
                }
                recv(ctx.inbox) -> call => {
                    let call = call?;
                    match decode_request(call.payload) {
                        Ok(ControlRequest::Apply { .. }) => {
                            ctx.outbox.send(ServiceOutput::Call(reply(
                                &ctx.addr,
                                &call.source,
                                encode_response(ControlResponse::Accepted)?,
                                call.request_id.clone(),
                            )))?;
                        }
                        Ok(ControlRequest::Query { name }) => {
                            ctx.outbox.send(ServiceOutput::Call(reply(
                                &ctx.addr,
                                &call.source,
                                encode_response(ControlResponse::Value {
                                    name,
                                    value: serde_json::Value::Null,
                                })?,
                                call.request_id.clone(),
                            )))?;
                        }
                        Err(error) => {
                            ctx.outbox.send(ServiceOutput::Failed(crate::conversation_new::ServiceFailure {
                                addr: ctx.addr.clone(),
                                error: format!("bad control payload: {error}"),
                            }))?;
                        }
                    }
                }
            }
        }
    }
}

fn reply(
    source: &crate::conversation_new::ServiceAddr,
    target: &crate::conversation_new::ServiceAddr,
    payload: serde_json::Value,
    response_id: Option<String>,
) -> ServiceCall {
    ServiceCall::response_to(source.clone(), target.clone(), payload, response_id)
}
