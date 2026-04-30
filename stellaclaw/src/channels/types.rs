use std::path::PathBuf;

use crossbeam_channel::Sender;
use serde::Serialize;
use serde_json::Value;
use stellaclaw_core::session_actor::ChatMessage;

use crate::conversation::IncomingConversationMessage;

#[derive(Debug, Clone)]
pub enum IncomingDispatch {
    Message(IncomingMessageDispatch),
    DeleteConversation {
        channel_id: String,
        platform_chat_id: String,
        conversation_id: String,
        response_tx: Sender<Result<(), String>>,
    },
}

#[derive(Debug, Clone)]
pub struct IncomingMessageDispatch {
    pub channel_id: String,
    pub platform_chat_id: String,
    pub conversation_id: String,
    pub message: IncomingConversationMessage,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutgoingAttachmentKind {
    Image,
    Audio,
    Voice,
    Video,
    Animation,
    Document,
}

#[derive(Debug, Clone)]
pub struct OutgoingAttachment {
    pub path: PathBuf,
    pub kind: OutgoingAttachmentKind,
}

#[derive(Debug, Clone)]
pub struct OutgoingOption {
    pub label: String,
    pub value: String,
}

#[derive(Debug, Clone)]
pub struct OutgoingOptions {
    pub prompt: String,
    pub options: Vec<OutgoingOption>,
}

#[derive(Debug, Clone)]
pub struct OutgoingDelivery {
    pub channel_id: String,
    pub platform_chat_id: String,
    pub conversation_id: String,
    pub session_id: Option<String>,
    pub message: Option<ChatMessage>,
    pub text: String,
    pub attachments: Vec<OutgoingAttachment>,
    pub options: Option<OutgoingOptions>,
}

#[allow(dead_code)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum OutgoingErrorScope {
    Turn,
    Runtime,
    Control,
    Configuration,
    RemoteWorkspace,
    Sandbox,
    Attachment,
    Delivery,
    BackgroundSession,
    Subagent,
}

#[allow(dead_code)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum OutgoingErrorSeverity {
    Info,
    Warning,
    Error,
}

#[derive(Debug, Clone, Serialize)]
pub struct OutgoingError {
    pub channel_id: String,
    pub platform_chat_id: String,
    pub conversation_id: String,
    pub scope: OutgoingErrorScope,
    pub severity: OutgoingErrorSeverity,
    pub code: String,
    pub message: String,
    pub detail: Option<Value>,
    pub can_continue: bool,
    pub suggested_action: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProcessingState {
    Idle,
    Typing,
}

#[derive(Debug, Clone)]
pub struct OutgoingProcessing {
    pub channel_id: String,
    pub platform_chat_id: String,
    pub state: ProcessingState,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProgressFeedbackFinalState {
    Done,
    Failed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TurnProgressPhase {
    Thinking,
    Working,
    Done,
    Failed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TurnProgressPlanItemStatus {
    Pending,
    InProgress,
    Completed,
}

#[derive(Debug, Clone, Serialize)]
pub struct TurnProgressPlanItem {
    pub step: String,
    pub status: TurnProgressPlanItemStatus,
}

#[derive(Debug, Clone, Serialize)]
pub struct TurnProgressPlan {
    pub explanation: Option<String>,
    pub items: Vec<TurnProgressPlanItem>,
}

#[derive(Debug, Clone, Serialize)]
pub struct TurnProgress {
    pub phase: TurnProgressPhase,
    pub model: String,
    pub activity: String,
    pub hint: Option<String>,
    pub plan: Option<TurnProgressPlan>,
    pub error: Option<String>,
}

#[derive(Debug, Clone)]
pub struct OutgoingProgressFeedback {
    pub channel_id: String,
    pub platform_chat_id: String,
    pub turn_id: String,
    pub progress: TurnProgress,
    pub final_state: Option<ProgressFeedbackFinalState>,
    pub important: bool,
}

#[derive(Debug, Clone)]
pub struct OutgoingConversationUpdated {
    pub channel_id: String,
    pub platform_chat_id: String,
    pub conversation_id: String,
}

#[derive(Debug, Clone)]
pub enum ChannelEvent {
    Delivery(OutgoingDelivery),
    Processing(OutgoingProcessing),
    ProgressFeedback(OutgoingProgressFeedback),
    ConversationUpdated(OutgoingConversationUpdated),
    Status(OutgoingStatus),
    Error(OutgoingError),
}

impl ChannelEvent {
    pub fn channel_id(&self) -> &str {
        match self {
            ChannelEvent::Delivery(delivery) => &delivery.channel_id,
            ChannelEvent::Processing(processing) => &processing.channel_id,
            ChannelEvent::ProgressFeedback(feedback) => &feedback.channel_id,
            ChannelEvent::ConversationUpdated(updated) => &updated.channel_id,
            ChannelEvent::Status(status) => &status.channel_id,
            ChannelEvent::Error(error) => &error.channel_id,
        }
    }

    pub fn platform_chat_id(&self) -> &str {
        match self {
            ChannelEvent::Delivery(delivery) => &delivery.platform_chat_id,
            ChannelEvent::Processing(processing) => &processing.platform_chat_id,
            ChannelEvent::ProgressFeedback(feedback) => &feedback.platform_chat_id,
            ChannelEvent::ConversationUpdated(updated) => &updated.platform_chat_id,
            ChannelEvent::Status(status) => &status.platform_chat_id,
            ChannelEvent::Error(error) => &error.platform_chat_id,
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct OutgoingStatus {
    pub channel_id: String,
    pub platform_chat_id: String,
    pub conversation_id: String,
    pub model: String,
    pub reasoning: String,
    pub sandbox: String,
    pub sandbox_source: String,
    pub remote: String,
    pub workspace: String,
    pub running_background: usize,
    pub total_background: usize,
    pub running_subagents: usize,
    pub total_subagents: usize,
    pub usage: OutgoingUsageSummary,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct OutgoingUsageSummary {
    pub foreground: OutgoingUsageTotals,
    pub background: OutgoingUsageTotals,
    pub subagents: OutgoingUsageTotals,
    pub media_tools: OutgoingUsageTotals,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct OutgoingUsageTotals {
    pub cache_read: u64,
    pub cache_write: u64,
    pub uncache_input: u64,
    pub output: u64,
    pub cost: OutgoingUsageCost,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct OutgoingUsageCost {
    pub cache_read: f64,
    pub cache_write: f64,
    pub uncache_input: f64,
    pub output: f64,
}

#[derive(Debug, Clone)]
pub enum OutgoingDispatch {
    Event(ChannelEvent),
}
