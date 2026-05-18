#![allow(dead_code)]

use anyhow::Result;
use crossbeam_channel::select;

use crate::conversation_new::{
    ConversationService, ServiceOutput, ServiceRunContext, ServiceStatusUpdate, ServiceStopped,
};

pub struct NoopService {
    name: String,
}

impl NoopService {
    pub fn new(name: impl Into<String>) -> Self {
        Self { name: name.into() }
    }
}

impl ConversationService for NoopService {
    fn run(self: Box<Self>, ctx: ServiceRunContext) -> Result<()> {
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
                    ctx.outbox.send(ServiceOutput::Status(ServiceStatusUpdate {
                        addr: ctx.addr.clone(),
                        label: "noop_call_received".to_string(),
                        detail: serde_json::json!({
                            "service": self.name,
                            "source": call.source,
                        }),
                    }))?;
                }
            }
        }
    }
}
