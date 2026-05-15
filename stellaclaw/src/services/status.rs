#![allow(dead_code)]

use std::{fs, path::Path};

use anyhow::{Context, Result};
use crossbeam_channel::select;
use serde_json::{json, Value};

use crate::{
    conversation_metadata::ConversationMetadata,
    conversation_new::{
        ConversationRuntimeConfig, ConversationService, ServiceCall, ServiceManifest,
        ServiceOutput, ServiceRunContext, ServiceStopped,
    },
    service_protos::status::{decode_request, encode_response, StatusRequest, StatusResponse},
};

pub struct StatusService;

impl StatusService {
    pub fn new() -> Self {
        Self
    }
}

impl ConversationService for StatusService {
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
                        Ok(StatusRequest::Snapshot) => {
                            let response = match status_snapshot(&ctx) {
                                Ok(snapshot) => StatusResponse::Snapshot { snapshot },
                                Err(error) => StatusResponse::Error {
                                    message: format!("{error:#}"),
                                },
                            };
                            ctx.outbox.send(ServiceOutput::Call(reply(
                                &ctx.addr,
                                &call.source,
                                encode_response(response)?,
                            )))?;
                        }
                        Ok(StatusRequest::Observe { .. }) => {
                            ctx.outbox.send(ServiceOutput::Call(reply(
                                &ctx.addr,
                                &call.source,
                                encode_response(StatusResponse::Accepted)?,
                            )))?;
                        }
                        Err(error) => {
                            ctx.outbox.send(ServiceOutput::Failed(crate::conversation_new::ServiceFailure {
                                addr: ctx.addr.clone(),
                                error: format!("bad status payload: {error}"),
                            }))?;
                        }
                    }
                }
            }
        }
    }
}

fn status_snapshot(ctx: &ServiceRunContext) -> Result<Value> {
    let service_root = ctx
        .storage
        .parent()
        .ok_or_else(|| anyhow::anyhow!("status service storage has no service root"))?;
    let manifest_path = service_root.join("manifest.json");
    let runtime_config_path = service_root.join("runtime_config.json");
    let metadata_path = service_root.join("conversation_metadata.json");
    let manifest = read_json::<ServiceManifest>(&manifest_path)?;
    let runtime_config = read_json::<ConversationRuntimeConfig>(&runtime_config_path)?;
    let metadata = read_json::<ConversationMetadata>(&metadata_path)?;

    let services = manifest
        .services
        .iter()
        .map(|entry| {
            let service_state = read_optional_json(&entry.storage.join("service_state.json"));
            json!({
                "addr": entry.addr,
                "kind": entry.kind,
                "storage": entry.storage,
                "state": service_state,
            })
        })
        .collect::<Vec<_>>();
    let foreground_sessions = services
        .iter()
        .filter(|service| {
            service["addr"]
                .as_object()
                .and_then(|addr| addr.get("path"))
                .and_then(Value::as_array)
                .is_some_and(|path| {
                    path.first().and_then(Value::as_str) == Some("agent")
                        && path.get(1).and_then(Value::as_str) == Some("foreground")
                })
        })
        .cloned()
        .collect::<Vec<_>>();

    Ok(json!({
        "conversation_id": ctx.conversation.conversation_id,
        "conversation_root": ctx.conversation.conversation_root,
        "metadata": metadata,
        "runtime_config": runtime_config,
        "manifest": {
            "version": manifest.version,
            "next_background_id": manifest.next_background_id,
            "next_subagent_id": manifest.next_subagent_id,
        },
        "services": services,
        "foreground_sessions": foreground_sessions,
    }))
}

fn read_json<T: for<'de> serde::Deserialize<'de>>(path: &Path) -> Result<T> {
    let content =
        fs::read_to_string(path).with_context(|| format!("failed to read {}", path.display()))?;
    serde_json::from_str(&content).with_context(|| format!("failed to parse {}", path.display()))
}

fn read_optional_json(path: &Path) -> Option<Value> {
    let content = fs::read_to_string(path).ok()?;
    serde_json::from_str(&content).ok()
}

fn reply(
    source: &crate::conversation_new::ServiceAddr,
    target: &crate::conversation_new::ServiceAddr,
    payload: serde_json::Value,
) -> ServiceCall {
    ServiceCall {
        source: source.clone(),
        target: target.clone(),
        payload,
    }
}
