use std::path::PathBuf;

use crossbeam_channel::Sender;
use serde::Serialize;
use serde_json::Value;
use stellaclaw_core::session_actor::{ChatMessage, FileItem, SelectionReferenceItem};

use crate::config::SandboxMode;

#[derive(Debug, Clone)]
pub struct IncomingConversationMessage {
    pub remote_message_id: String,
    pub user_name: Option<String>,
    pub message_time: Option<String>,
    pub text: Option<String>,
    pub selection_references: Vec<SelectionReferenceItem>,
    pub files: Vec<FileItem>,
    pub control: Option<ConversationControl>,
}

#[derive(Debug, Clone)]
pub enum ConversationControl {
    Continue,
    Cancel,
    Compact,
    ShowStatus,
    ShowModel,
    SwitchModel { model_name: String },
    ShowReasoning,
    SetReasoning { effort: Option<String> },
    InvalidReasoning { reason: String },
    ShowRemote,
    SetRemote { host: String, path: String },
    DisableRemote,
    InvalidRemote { reason: String },
    ShowSandbox,
    SetSandbox { mode: Option<SandboxMode> },
    InvalidSandbox { reason: String },
}

pub(crate) fn parse_reasoning_control_argument(argument: &str) -> ConversationControl {
    let argument = argument.trim();
    if argument.is_empty() {
        return ConversationControl::ShowReasoning;
    }
    match argument.to_ascii_lowercase().as_str() {
        "default" | "model" | "model_default" | "model-default" | "global" => {
            ConversationControl::SetReasoning { effort: None }
        }
        "minimal" | "low" | "medium" | "high" | "xhigh" => ConversationControl::SetReasoning {
            effort: Some(argument.to_ascii_lowercase()),
        },
        _ => ConversationControl::InvalidReasoning {
            reason: format!("未知 reasoning effort `{argument}`。"),
        },
    }
}

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

#[derive(Debug, Clone)]
pub struct OutgoingMessageAppended {
    pub channel_id: String,
    pub platform_chat_id: String,
    pub conversation_id: String,
    pub session_id: String,
    pub index: usize,
    pub message: ChatMessage,
}

#[derive(Debug, Clone)]
pub struct OutgoingSessionStream {
    pub channel_id: String,
    pub platform_chat_id: String,
    pub conversation_id: String,
    pub session_id: String,
    pub event: Value,
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

#[derive(Debug, Clone)]
pub enum ChannelEvent {
    Delivery(OutgoingDelivery),
    MessageAppended(OutgoingMessageAppended),
    SessionStream(OutgoingSessionStream),
    Processing(OutgoingProcessing),
    Error(OutgoingError),
}

impl ChannelEvent {
    pub fn channel_id(&self) -> &str {
        match self {
            ChannelEvent::Delivery(delivery) => &delivery.channel_id,
            ChannelEvent::MessageAppended(appended) => &appended.channel_id,
            ChannelEvent::SessionStream(stream) => &stream.channel_id,
            ChannelEvent::Processing(processing) => &processing.channel_id,
            ChannelEvent::Error(error) => &error.channel_id,
        }
    }

    pub fn platform_chat_id(&self) -> &str {
        match self {
            ChannelEvent::Delivery(delivery) => &delivery.platform_chat_id,
            ChannelEvent::MessageAppended(appended) => &appended.platform_chat_id,
            ChannelEvent::SessionStream(stream) => &stream.platform_chat_id,
            ChannelEvent::Processing(processing) => &processing.platform_chat_id,
            ChannelEvent::Error(error) => &error.platform_chat_id,
        }
    }
}

#[derive(Debug, Clone)]
pub enum OutgoingDispatch {
    Event(ChannelEvent),
}
