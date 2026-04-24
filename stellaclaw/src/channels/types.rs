use std::path::PathBuf;

use serde::Serialize;

use crate::conversation::IncomingConversationMessage;

#[derive(Debug, Clone)]
pub struct IncomingDispatch {
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
    pub text: String,
    pub attachments: Vec<OutgoingAttachment>,
    pub options: Option<OutgoingOptions>,
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

#[derive(Debug, Clone)]
pub struct OutgoingProgressFeedback {
    pub channel_id: String,
    pub platform_chat_id: String,
    pub turn_id: String,
    pub text: String,
    pub final_state: Option<ProgressFeedbackFinalState>,
    pub important: bool,
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
    Delivery(OutgoingDelivery),
    Processing(OutgoingProcessing),
    ProgressFeedback(OutgoingProgressFeedback),
    Status(OutgoingStatus),
}
