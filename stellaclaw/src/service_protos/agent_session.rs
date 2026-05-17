#![allow(dead_code)]

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use stellaclaw_core::session_actor::{
    ChatMessage, ChatMessageItem, ChatRole, ContextItem, SessionErrorDetail, TaskPlanView,
    ToolResultItem,
};

use crate::conversation_new::{AgentSessionLaunchConfig, ServiceAddr, ServiceCall};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum AgentSessionKind {
    Foreground,
    Background,
    Subagent,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AgentSessionBinding {
    pub event_sink: ServiceAddr,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_addr: Option<ServiceAddr>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum AgentSessionState {
    Idle,
    Running,
    Stopping,
    Stopped,
    Crashed,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum AgentMessageOrigin {
    User,
    Actor,
    System,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentSessionStatus {
    pub kind: AgentSessionKind,
    pub binding: AgentSessionBinding,
    pub state: AgentSessionState,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub active_turn_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub current_plan: Option<TaskPlanView>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_error: Option<String>,
    pub message_count: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentSessionContext {
    pub status: AgentSessionStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_message: Option<ChatMessage>,
    #[serde(default)]
    pub metadata: Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentSessionMessageRecord {
    pub index: usize,
    pub message: ChatMessage,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentSessionMessageHistory {
    pub request_id: String,
    pub offset: usize,
    pub limit: usize,
    pub total: usize,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_message: Option<AgentSessionMessageRecord>,
    #[serde(default)]
    pub messages: Vec<AgentSessionMessageRecord>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AgentSessionRequest {
    EnqueueMessage {
        origin: AgentMessageOrigin,
        message: ChatMessage,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        ingress_id: Option<String>,
    },
    CancelTurn {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reason: Option<String>,
    },
    ContinueTurn {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reason: Option<String>,
    },
    CompactNow,
    ResolveHostCoordination {
        response: Value,
    },
    ChildSessionEvent {
        session_addr: ServiceAddr,
        event: AgentSessionEvent,
    },
    QueryContext {
        query_id: String,
        #[serde(default)]
        payload: Value,
    },
    QueryMessages {
        request_id: String,
        offset: usize,
        limit: usize,
    },
    QueryMessageDetail {
        request_id: String,
        message_id: String,
    },
    QueryStatus,
    UpdateLaunchConfig {
        launch: AgentSessionLaunchConfig,
    },
    Shutdown {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reason: Option<String>,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AgentSessionEvent {
    UserMessageStarted {
        origin: AgentMessageOrigin,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        ingress_id: Option<String>,
        message: ChatMessage,
    },
    UserMessageCommitted {
        index: usize,
        message: ChatMessage,
    },
    MessageAppended {
        index: usize,
        message: ChatMessage,
    },
    TurnStarted {
        turn_id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        plan: Option<TaskPlanView>,
    },
    PlanUpdated {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        plan: Option<TaskPlanView>,
    },
    StreamAssistantMessageDelta {
        message_id: String,
        turn_id: String,
        in_message_index: u64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        item_id: Option<String>,
        delta: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        message_index: Option<usize>,
    },
    StreamToolCallDelta {
        message_id: String,
        turn_id: String,
        in_message_index: u64,
        item_id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        call_id: Option<String>,
        delta: String,
    },
    StreamReasoningSummaryDelta {
        message_id: String,
        turn_id: String,
        in_message_index: u64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        item_id: Option<String>,
        summary_index: i64,
        delta: String,
    },
    StreamReasoningSummaryPartAdded {
        message_id: String,
        turn_id: String,
        in_message_index: u64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        item_id: Option<String>,
        summary_index: i64,
    },
    StreamError {
        message_id: String,
        turn_id: String,
        in_message_index: u64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        item_id: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        message_index: Option<usize>,
        error: String,
        error_detail: SessionErrorDetail,
    },
    StreamToolResultDone {
        turn_id: String,
        batch_id: String,
        tool_result: ToolResultItem,
    },
    TurnCompleted {
        message: ChatMessage,
    },
    TurnFailed {
        error: String,
        error_detail: SessionErrorDetail,
        can_continue: bool,
    },
    Terminated {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reason: Option<String>,
    },
    HostCoordinationRequested {
        request: Value,
    },
    InteractiveOutputRequested {
        payload: Value,
    },
    SessionViewResult {
        query_id: String,
        payload: Value,
    },
    CompactCompleted {
        compressed: bool,
        estimated_tokens_before: u64,
        estimated_tokens_after: u64,
        threshold_tokens: u64,
        retained_message_count: usize,
        compressed_message_count: usize,
    },
    CompactFailed {
        phase: String,
        reason: String,
    },
    ControlRejected {
        reason: String,
        payload: Value,
    },
    RuntimeCrashed {
        error: String,
        error_detail: SessionErrorDetail,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AgentSessionResponse {
    Accepted,
    Status {
        status: AgentSessionStatus,
    },
    Context {
        query_id: String,
        context: AgentSessionContext,
    },
    MessageHistory {
        history: AgentSessionMessageHistory,
    },
    MessageDetail {
        request_id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        record: Option<AgentSessionMessageRecord>,
    },
    Stopped,
    Rejected {
        reason: String,
    },
}

pub fn encode_request(request: AgentSessionRequest) -> Result<Value> {
    serde_json::to_value(request).context("failed to encode agent session request")
}

pub fn decode_request(payload: Value) -> Result<AgentSessionRequest> {
    serde_json::from_value(payload).context("failed to decode agent session request")
}

pub fn encode_response(response: AgentSessionResponse) -> Result<Value> {
    serde_json::to_value(response).context("failed to encode agent session response")
}

pub fn decode_response(payload: Value) -> Result<AgentSessionResponse> {
    serde_json::from_value(payload).context("failed to decode agent session response")
}

pub fn send_message_call(
    source: ServiceAddr,
    target: ServiceAddr,
    text: impl Into<String>,
) -> Result<ServiceCall> {
    enqueue_user_text_call(source, target, text, None)
}

pub fn enqueue_user_text_call(
    source: ServiceAddr,
    target: ServiceAddr,
    text: impl Into<String>,
    ingress_id: Option<String>,
) -> Result<ServiceCall> {
    enqueue_message_call(
        source,
        target,
        AgentMessageOrigin::User,
        text_message(ChatRole::User, text),
        ingress_id,
    )
}

pub fn enqueue_message_call(
    source: ServiceAddr,
    target: ServiceAddr,
    origin: AgentMessageOrigin,
    message: ChatMessage,
    ingress_id: Option<String>,
) -> Result<ServiceCall> {
    Ok(ServiceCall::new(
        source,
        target,
        encode_request(AgentSessionRequest::EnqueueMessage {
            origin,
            message,
            ingress_id,
        })?,
    ))
}

pub fn query_status_call(source: ServiceAddr, target: ServiceAddr) -> Result<ServiceCall> {
    Ok(ServiceCall::new(
        source,
        target,
        encode_request(AgentSessionRequest::QueryStatus)?,
    ))
}

pub fn query_context_call(
    source: ServiceAddr,
    target: ServiceAddr,
    query_id: impl Into<String>,
    payload: Value,
) -> Result<ServiceCall> {
    Ok(ServiceCall::new(
        source,
        target,
        encode_request(AgentSessionRequest::QueryContext {
            query_id: query_id.into(),
            payload,
        })?,
    ))
}

pub fn query_messages_call(
    source: ServiceAddr,
    target: ServiceAddr,
    request_id: impl Into<String>,
    offset: usize,
    limit: usize,
) -> Result<ServiceCall> {
    let request_id = request_id.into();
    Ok(ServiceCall::new(
        source,
        target,
        encode_request(AgentSessionRequest::QueryMessages {
            request_id: request_id.clone(),
            offset,
            limit,
        })?,
    )
    .with_request_id(request_id))
}

pub fn query_message_detail_call(
    source: ServiceAddr,
    target: ServiceAddr,
    request_id: impl Into<String>,
    message_id: impl Into<String>,
) -> Result<ServiceCall> {
    let request_id = request_id.into();
    Ok(ServiceCall::new(
        source,
        target,
        encode_request(AgentSessionRequest::QueryMessageDetail {
            request_id: request_id.clone(),
            message_id: message_id.into(),
        })?,
    )
    .with_request_id(request_id))
}

pub fn cancel_turn_call(
    source: ServiceAddr,
    target: ServiceAddr,
    reason: Option<String>,
) -> Result<ServiceCall> {
    Ok(ServiceCall::new(
        source,
        target,
        encode_request(AgentSessionRequest::CancelTurn { reason })?,
    ))
}

pub fn update_launch_config_call(
    source: ServiceAddr,
    target: ServiceAddr,
    launch: AgentSessionLaunchConfig,
) -> Result<ServiceCall> {
    Ok(ServiceCall::new(
        source,
        target,
        encode_request(AgentSessionRequest::UpdateLaunchConfig { launch })?,
    ))
}

pub fn continue_turn_call(
    source: ServiceAddr,
    target: ServiceAddr,
    reason: Option<String>,
) -> Result<ServiceCall> {
    Ok(ServiceCall::new(
        source,
        target,
        encode_request(AgentSessionRequest::ContinueTurn { reason })?,
    ))
}

pub fn compact_now_call(source: ServiceAddr, target: ServiceAddr) -> Result<ServiceCall> {
    Ok(ServiceCall::new(
        source,
        target,
        encode_request(AgentSessionRequest::CompactNow)?,
    ))
}

pub fn resolve_host_coordination_call(
    source: ServiceAddr,
    target: ServiceAddr,
    response: Value,
) -> Result<ServiceCall> {
    Ok(ServiceCall::new(
        source,
        target,
        encode_request(AgentSessionRequest::ResolveHostCoordination { response })?,
    ))
}

pub fn child_session_event_call(
    source: ServiceAddr,
    target: ServiceAddr,
    event: AgentSessionEvent,
) -> Result<ServiceCall> {
    Ok(ServiceCall::new(
        source.clone(),
        target,
        encode_request(AgentSessionRequest::ChildSessionEvent {
            session_addr: source,
            event,
        })?,
    ))
}

pub fn shutdown_call(
    source: ServiceAddr,
    target: ServiceAddr,
    reason: Option<String>,
) -> Result<ServiceCall> {
    Ok(ServiceCall::new(
        source,
        target,
        encode_request(AgentSessionRequest::Shutdown { reason })?,
    ))
}

pub fn text_message(role: ChatRole, text: impl Into<String>) -> ChatMessage {
    ChatMessage::new(
        role,
        vec![ChatMessageItem::Context(ContextItem { text: text.into() })],
    )
}
