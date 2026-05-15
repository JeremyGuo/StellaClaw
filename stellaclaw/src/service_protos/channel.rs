#![allow(dead_code)]

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use stellaclaw_core::session_actor::ChatMessage;

use crate::conversation_new::{ServiceAddr, ServiceCall};
use crate::service_protos::agent_session::{
    AgentMessageOrigin, AgentSessionContext, AgentSessionEvent, AgentSessionStatus,
};
use crate::service_protos::kernel::{
    KernelMetadataPatch, KernelResponse, KernelRuntimeConfigPatch,
};
use crate::service_protos::status::{StatusRequest, StatusResponse};
use crate::service_protos::terminal::{TerminalRequest, TerminalResponse};
use crate::service_protos::workspace::{WorkspaceRequest, WorkspaceResponse};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ChannelIngress {
    IncomingMessage {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        platform_message_id: Option<String>,
        #[serde(default)]
        origin: Option<AgentMessageOrigin>,
        message: ChatMessage,
        #[serde(default)]
        metadata: Value,
    },
    QueryForegroundContext {
        query_id: String,
        #[serde(default)]
        payload: Value,
    },
    QueryForegroundStatus,
    CreateForegroundSession {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        requested_id: Option<String>,
    },
    CancelForegroundTurn {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reason: Option<String>,
    },
    ContinueForegroundTurn {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reason: Option<String>,
    },
    CompactForegroundNow,
    DeleteForegroundSession {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reason: Option<String>,
    },
    ResolveHostCoordination {
        response: Value,
    },
    UpdateRuntimeConfig {
        patch: KernelRuntimeConfigPatch,
    },
    QueryKernelMetadata {
        request_id: String,
    },
    UpdateKernelMetadata {
        request_id: String,
        patch: KernelMetadataPatch,
    },
    Workspace {
        request_id: String,
        request: WorkspaceRequest,
    },
    Status {
        request_id: String,
        request: StatusRequest,
    },
    Terminal {
        request_id: String,
        request: TerminalRequest,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChannelDelivery {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_addr: Option<ServiceAddr>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<ChatMessage>,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub text: String,
    #[serde(default)]
    pub attachments: Vec<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub options: Option<Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ChannelRequest {
    Deliver {
        delivery: ChannelDelivery,
    },
    SessionEvent {
        session_addr: ServiceAddr,
        event: AgentSessionEvent,
    },
    Status {
        label: String,
        #[serde(default)]
        detail: Value,
    },
    Error {
        code: String,
        message: String,
        detail: Option<String>,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ChannelResponse {
    Accepted,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ChannelEvent {
    AgentSessionAccepted,
    AgentSessionStopped,
    AgentSessionRejected {
        reason: String,
    },
    Delivery {
        delivery: ChannelDelivery,
        text: String,
    },
    SessionEvent {
        session_addr: ServiceAddr,
        event: AgentSessionEvent,
    },
    ForegroundContext {
        query_id: String,
        context: AgentSessionContext,
    },
    AgentSessionStatus {
        status: AgentSessionStatus,
    },
    AgentSessionCreated {
        addr: ServiceAddr,
    },
    Status {
        label: String,
        #[serde(default)]
        detail: Value,
    },
    Workspace {
        request_id: String,
        response: WorkspaceResponse,
    },
    StatusSnapshot {
        request_id: String,
        response: StatusResponse,
    },
    KernelMetadata {
        request_id: String,
        response: KernelResponse,
    },
    Terminal {
        request_id: Option<String>,
        response: TerminalResponse,
    },
    Error {
        code: String,
        message: String,
        detail: Option<String>,
    },
}

pub fn encode_request(request: ChannelRequest) -> Result<Value> {
    serde_json::to_value(request).context("failed to encode channel request")
}

pub fn decode_request(payload: Value) -> Result<ChannelRequest> {
    serde_json::from_value(payload).context("failed to decode channel request")
}

pub fn encode_response(response: ChannelResponse) -> Result<Value> {
    serde_json::to_value(response).context("failed to encode channel response")
}

pub fn deliver_text_call(
    source: ServiceAddr,
    channel: ServiceAddr,
    text: impl Into<String>,
) -> Result<ServiceCall> {
    deliver_call(
        source,
        channel,
        ChannelDelivery {
            session_addr: None,
            message: None,
            text: text.into(),
            attachments: Vec::new(),
            options: None,
        },
    )
}

pub fn deliver_call(
    source: ServiceAddr,
    channel: ServiceAddr,
    delivery: ChannelDelivery,
) -> Result<ServiceCall> {
    Ok(ServiceCall {
        source,
        target: channel,
        payload: encode_request(ChannelRequest::Deliver { delivery })?,
    })
}

pub fn session_event_call(
    source: ServiceAddr,
    channel: ServiceAddr,
    event: AgentSessionEvent,
) -> Result<ServiceCall> {
    Ok(ServiceCall {
        source: source.clone(),
        target: channel,
        payload: encode_request(ChannelRequest::SessionEvent {
            session_addr: source,
            event,
        })?,
    })
}

pub fn error_call(
    source: ServiceAddr,
    channel: ServiceAddr,
    code: impl Into<String>,
    message: impl Into<String>,
    detail: Option<String>,
) -> Result<ServiceCall> {
    Ok(ServiceCall {
        source,
        target: channel,
        payload: encode_request(ChannelRequest::Error {
            code: code.into(),
            message: message.into(),
            detail,
        })?,
    })
}
