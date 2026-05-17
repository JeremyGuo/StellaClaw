#![allow(dead_code)]

use serde::{Deserialize, Serialize};
use serde_json::Value;
use stellaclaw_core::session_actor::{
    ChatMessage, SessionErrorDetail, TaskPlanView, ToolResultItem,
};

use crate::service_protos::agent_session::{AgentMessageOrigin, AgentSessionState};

pub const HOME_WS_PATH: &str = "/api/ws/home";
pub const HEARTBEAT_INTERVAL_SECS: u64 = 30;

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
