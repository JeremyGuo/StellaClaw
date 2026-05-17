#![allow(dead_code)]

use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};
use stellaclaw_core::session_actor::{
    ChatMessage, SessionErrorDetail, TaskPlanView, ToolResultItem,
};

use crate::service_protos::agent_session::{AgentMessageOrigin, AgentSessionState};

pub const HOME_WS_PATH: &str = "/api/ws/home";
pub const HEARTBEAT_INTERVAL_SECS: u64 = 30;

pub fn home_snapshot(seq: u64, conversations: Vec<Value>, server_time: String) -> Value {
    json!({
        "type": "home.snapshot",
        "seq": seq,
        "server_time": server_time,
        "conversations": conversations,
    })
}

pub fn home_conversation_upserted(conversation: Value) -> Value {
    json!({
        "type": "home.conversation_upserted",
        "conversation": conversation,
    })
}

pub fn home_conversation_deleted(conversation_id: &str) -> Value {
    json!({
        "type": "home.conversation_deleted",
        "conversation_id": conversation_id,
    })
}

pub fn home_foreground_session_upserted(conversation_id: &str, foreground_session: Value) -> Value {
    json!({
        "type": "home.foreground_session_upserted",
        "conversation_id": conversation_id,
        "foreground_session": foreground_session,
    })
}

pub fn home_foreground_session_updated(
    conversation_id: &str,
    foreground_session_id: &str,
    patch: Value,
) -> Value {
    json!({
        "type": "home.foreground_session_updated",
        "conversation_id": conversation_id,
        "foreground_session_id": foreground_session_id,
        "patch": patch,
    })
}

pub fn home_foreground_session_deleted(
    conversation_id: &str,
    foreground_session_id: &str,
) -> Value {
    json!({
        "type": "home.foreground_session_deleted",
        "conversation_id": conversation_id,
        "foreground_session_id": foreground_session_id,
    })
}

pub fn home_foreground_session_state_updated(
    conversation_id: &str,
    foreground_session_id: &str,
    state: &str,
    active_turn_id: Option<String>,
    last_error: Option<String>,
) -> Value {
    json!({
        "type": "home.foreground_session_state_updated",
        "conversation_id": conversation_id,
        "foreground_session_id": foreground_session_id,
        "state": state,
        "active_turn_id": active_turn_id,
        "last_error": last_error,
    })
}

pub fn home_foreground_session_seen_state_updated(
    conversation_id: &str,
    foreground_session_id: &str,
    last_seen_message_id: &str,
    last_seen_at: &str,
) -> Value {
    json!({
        "type": "home.foreground_session_seen_state_updated",
        "conversation_id": conversation_id,
        "foreground_session_id": foreground_session_id,
        "last_seen_message_id": last_seen_message_id,
        "last_seen_at": last_seen_at,
    })
}

pub fn chat_snapshot(
    conversation_id: &str,
    foreground_session_id: &str,
    total: usize,
    last_committed_message_id: Option<String>,
    last_committed_message_index: Option<usize>,
    current_turn_state: Option<Value>,
    current_provisional_assistant_message: Option<Value>,
    running_tool_results: Vec<Value>,
    queued_outbound_messages: Vec<Value>,
) -> Value {
    json!({
        "type": "chat.snapshot",
        "conversation_id": conversation_id,
        "foreground_session_id": foreground_session_id,
        "total": total,
        "next_message_index": total,
        "last_committed_message_id": last_committed_message_id,
        "last_committed_message_index": last_committed_message_index,
        "current_turn_state": current_turn_state,
        "current_provisional_assistant_message": current_provisional_assistant_message,
        "running_tool_results": running_tool_results,
        "queued_outbound_messages": queued_outbound_messages,
    })
}

pub fn chat_message_appended(
    conversation_id: &str,
    foreground_session_id: &str,
    message_index: usize,
    message_id: &str,
    message: Value,
) -> Value {
    json!({
        "type": "chat.message_appended",
        "conversation_id": conversation_id,
        "foreground_session_id": foreground_session_id,
        "message_index": message_index,
        "message_id": message_id,
        "committed": true,
        "message": message,
    })
}

pub fn chat_stream_error(
    conversation_id: &str,
    foreground_session_id: &str,
    message_id: &str,
    next_message_id: &str,
    turn_id: &str,
    in_message_index: u64,
    error: &str,
    reason: &str,
) -> Value {
    json!({
        "type": "chat.stream_error",
        "conversation_id": conversation_id,
        "foreground_session_id": foreground_session_id,
        "message_id": message_id,
        "next_message_id": next_message_id,
        "turn_id": turn_id,
        "in_message_index": in_message_index,
        "error": error,
        "error_detail": {
            "module": "web_channel",
            "kind": error,
            "reason": reason,
        },
    })
}

pub fn public_chat_stream_payload(
    conversation_id: &str,
    foreground_session_id: &str,
    event_type: &str,
    event: &Value,
) -> Value {
    let mut payload = match event {
        Value::Object(map) => map.clone(),
        _ => Map::new(),
    };
    payload.insert(
        "type".to_string(),
        json!(public_chat_stream_type(event_type)),
    );
    payload.insert("conversation_id".to_string(), json!(conversation_id));
    payload.insert(
        "foreground_session_id".to_string(),
        json!(foreground_session_id),
    );
    if event_type == "user_message_committed" {
        if let Some(index) = payload.get("index").cloned() {
            payload.insert("message_index".to_string(), index);
        }
        if let Some(message_id) = payload
            .get("message")
            .and_then(|message| message.get("message_id"))
            .cloned()
        {
            payload.insert("message_id".to_string(), message_id);
        }
        payload.insert("committed".to_string(), json!(true));
    }
    if !payload.contains_key("next_message_id") {
        if let Some(message_id) = payload.get("message_id").cloned() {
            payload.insert("next_message_id".to_string(), message_id);
        }
    }
    Value::Object(payload)
}

fn public_chat_stream_type(event_type: &str) -> String {
    let suffix = match event_type {
        "turn_started" => "stream_turn_start",
        "turn_completed" => "stream_turn_done",
        "plan_updated" => "plan_updated",
        other => other,
    };
    format!("chat.{suffix}")
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WebHeartbeat {
    pub server_time: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HomeSnapshot {
    pub r#type: String,
    pub seq: u64,
    pub server_time: String,
    pub conversations: Vec<HomeConversationSummary>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HomeConversationSummary {
    pub conversation_id: String,
    pub conversation_name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub updated_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_committed_message_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_committed_message_index: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_message_preview: Option<String>,
    #[serde(default)]
    pub foreground_sessions: Vec<HomeForegroundSessionSummary>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HomeForegroundSessionSummary {
    pub foreground_session_id: String,
    pub session_name: String,
    pub state: HomeForegroundSessionState,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub active_turn_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_committed_message_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_committed_message_index: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_activity_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_seen_message_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_seen_at: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HomeForegroundSessionState {
    Idle,
    Queued,
    Running,
    Failed,
}

impl From<AgentSessionState> for HomeForegroundSessionState {
    fn from(value: AgentSessionState) -> Self {
        match value {
            AgentSessionState::Running | AgentSessionState::Stopping => Self::Running,
            AgentSessionState::Crashed => Self::Failed,
            AgentSessionState::Idle | AgentSessionState::Stopped => Self::Idle,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum HomeEvent {
    #[serde(rename = "home.conversation_upserted")]
    ConversationUpserted {
        seq: u64,
        conversation: HomeConversationSummary,
    },
    #[serde(rename = "home.conversation_updated")]
    ConversationUpdated {
        seq: u64,
        conversation_id: String,
        patch: Value,
    },
    #[serde(rename = "home.conversation_deleted")]
    ConversationDeleted { seq: u64, conversation_id: String },
    #[serde(rename = "home.foreground_session_upserted")]
    ForegroundSessionUpserted {
        seq: u64,
        conversation_id: String,
        foreground_session: HomeForegroundSessionSummary,
    },
    #[serde(rename = "home.foreground_session_updated")]
    ForegroundSessionUpdated {
        seq: u64,
        conversation_id: String,
        foreground_session_id: String,
        patch: Value,
    },
    #[serde(rename = "home.foreground_session_deleted")]
    ForegroundSessionDeleted {
        seq: u64,
        conversation_id: String,
        foreground_session_id: String,
    },
    #[serde(rename = "home.foreground_session_state_updated")]
    ForegroundSessionStateUpdated {
        seq: u64,
        conversation_id: String,
        foreground_session_id: String,
        state: HomeForegroundSessionState,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        active_turn_id: Option<String>,
    },
    #[serde(rename = "home.foreground_session_seen_state_updated")]
    ForegroundSessionSeenStateUpdated {
        seq: u64,
        conversation_id: String,
        foreground_session_id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        last_seen_message_id: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        last_seen_at: Option<String>,
    },
    #[serde(rename = "home.heartbeat")]
    Heartbeat(WebHeartbeat),
    #[serde(rename = "home.error")]
    Error {
        seq: u64,
        code: String,
        message: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        detail: Option<Value>,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatSnapshot {
    pub r#type: String,
    pub conversation_id: String,
    pub foreground_session_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_committed_message_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_committed_message_index: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub current_turn_state: Option<ChatTurnState>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub current_provisional_assistant_message: Option<ChatProvisionalMessage>,
    #[serde(default)]
    pub running_tool_results: Vec<ChatToolResultState>,
    #[serde(default)]
    pub queued_outbound_messages: Vec<QueuedOutboundMessage>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatTurnState {
    pub turn_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatProvisionalMessage {
    pub turn_id: String,
    pub message_id: String,
    pub message: ChatMessage,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatToolResultState {
    pub turn_id: String,
    pub tool_result: ToolResultItem,
    pub committed: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QueuedOutboundMessage {
    pub client_message_id: String,
    pub conversation_id: String,
    pub foreground_session_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ChatEvent {
    #[serde(rename = "chat.user_message_queued")]
    UserMessageQueued {
        client_message_id: String,
        conversation_id: String,
        foreground_session_id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        message_id: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        message: Option<ChatMessage>,
    },
    #[serde(rename = "chat.user_message_started")]
    UserMessageStarted {
        conversation_id: String,
        foreground_session_id: String,
        origin: AgentMessageOrigin,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        ingress_id: Option<String>,
        message: ChatMessage,
    },
    #[serde(rename = "chat.user_message_committed")]
    UserMessageCommitted {
        conversation_id: String,
        foreground_session_id: String,
        message_index: usize,
        message_id: String,
        committed: bool,
        message: ChatMessage,
    },
    #[serde(rename = "chat.message_appended")]
    MessageAppended {
        conversation_id: String,
        foreground_session_id: String,
        message_index: usize,
        message_id: String,
        committed: bool,
        message: ChatMessage,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        turn_id: Option<String>,
    },
    #[serde(rename = "chat.stream_tool_result_done")]
    StreamToolResultDone {
        conversation_id: String,
        foreground_session_id: String,
        turn_id: String,
        batch_id: String,
        tool_result: ToolResultItem,
    },
    #[serde(rename = "chat.stream_turn_start")]
    StreamTurnStart {
        conversation_id: String,
        foreground_session_id: String,
        turn_id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        plan: Option<TaskPlanView>,
    },
    #[serde(rename = "chat.stream_assistant_message_delta")]
    StreamAssistantMessageDelta {
        conversation_id: String,
        foreground_session_id: String,
        message_id: String,
        next_message_id: String,
        turn_id: String,
        in_message_index: u64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        item_id: Option<String>,
        delta: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        message_index: Option<usize>,
    },
    #[serde(rename = "chat.stream_tool_call_delta")]
    StreamToolCallDelta {
        conversation_id: String,
        foreground_session_id: String,
        message_id: String,
        next_message_id: String,
        turn_id: String,
        in_message_index: u64,
        item_id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        call_id: Option<String>,
        delta: String,
    },
    #[serde(rename = "chat.stream_reasoning_summary_part_added")]
    StreamReasoningSummaryPartAdded {
        conversation_id: String,
        foreground_session_id: String,
        message_id: String,
        next_message_id: String,
        turn_id: String,
        in_message_index: u64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        item_id: Option<String>,
        summary_index: i64,
    },
    #[serde(rename = "chat.stream_reasoning_summary_delta")]
    StreamReasoningSummaryDelta {
        conversation_id: String,
        foreground_session_id: String,
        message_id: String,
        next_message_id: String,
        turn_id: String,
        in_message_index: u64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        item_id: Option<String>,
        summary_index: i64,
        delta: String,
    },
    #[serde(rename = "chat.stream_error")]
    StreamError {
        conversation_id: String,
        foreground_session_id: String,
        message_id: String,
        next_message_id: String,
        turn_id: String,
        in_message_index: u64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        item_id: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        message_index: Option<usize>,
        error: String,
        error_detail: SessionErrorDetail,
    },
    #[serde(rename = "chat.stream_turn_done")]
    StreamTurnDone {
        conversation_id: String,
        foreground_session_id: String,
        message: ChatMessage,
    },
    #[serde(rename = "chat.plan_updated")]
    PlanUpdated {
        conversation_id: String,
        foreground_session_id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        plan: Option<TaskPlanView>,
    },
    #[serde(rename = "chat.heartbeat")]
    Heartbeat(WebHeartbeat),
}
