use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use uuid::Uuid;

#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ChannelAddress {
    pub channel_id: String,
    pub conversation_id: String,
    #[serde(default)]
    pub user_id: Option<String>,
    #[serde(default)]
    pub display_name: Option<String>,
}

impl ChannelAddress {
    pub fn session_key(&self) -> String {
        format!("{}::{}", self.channel_id, self.conversation_id)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AttachmentKind {
    Image,
    File,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct StoredAttachment {
    pub id: Uuid,
    pub kind: AttachmentKind,
    pub original_name: Option<String>,
    pub media_type: Option<String>,
    pub path: PathBuf,
    pub size_bytes: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct OutgoingAttachment {
    pub kind: AttachmentKind,
    pub path: PathBuf,
    #[serde(default)]
    pub caption: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ShowOption {
    pub label: String,
    pub value: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ShowOptions {
    pub prompt: String,
    pub options: Vec<ShowOption>,
    #[serde(default = "default_show_options_one_time")]
    pub one_time: bool,
}

fn default_show_options_one_time() -> bool {
    true
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct OutgoingMessage {
    #[serde(default)]
    pub text: Option<String>,
    #[serde(default)]
    pub images: Vec<OutgoingAttachment>,
    #[serde(default)]
    pub attachments: Vec<OutgoingAttachment>,
    #[serde(default)]
    pub options: Option<ShowOptions>,
    #[serde(default)]
    pub usage_chart: Option<UsageChart>,
}

impl OutgoingMessage {
    pub fn text(text: impl Into<String>) -> Self {
        Self {
            text: Some(text.into()),
            images: Vec::new(),
            attachments: Vec::new(),
            options: None,
            usage_chart: None,
        }
    }

    pub fn with_options(
        text: impl Into<String>,
        prompt: impl Into<String>,
        options: Vec<ShowOption>,
    ) -> Self {
        Self {
            text: Some(text.into()),
            images: Vec::new(),
            attachments: Vec::new(),
            options: Some(ShowOptions {
                prompt: prompt.into(),
                options,
                one_time: true,
            }),
            usage_chart: None,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct UsageChart {
    pub title: String,
    pub y_label: String,
    pub days: Vec<UsageChartDay>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct UsageChartDay {
    pub label: String,
    pub total_usd: f64,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub llm_calls: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProcessingState {
    Idle,
    Typing,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MessageRole {
    User,
    Assistant,
    System,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SessionMessage {
    pub role: MessageRole,
    pub text: Option<String>,
    pub attachments: Vec<StoredAttachment>,
}
