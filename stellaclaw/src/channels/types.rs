use std::path::PathBuf;

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

#[derive(Debug, Clone)]
pub enum OutgoingDispatch {
    Delivery(OutgoingDelivery),
    Processing(OutgoingProcessing),
    ProgressFeedback(OutgoingProgressFeedback),
}
