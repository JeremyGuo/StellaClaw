use crate::domain::{
    AttachmentKind, ChannelAddress, OutgoingMessage, ProcessingState, StoredAttachment,
};
use anyhow::{Context, Result, anyhow};
use async_trait::async_trait;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;
use tokio::fs;
use tokio::sync::mpsc;
use uuid::Uuid;

#[async_trait]
pub trait AttachmentSource: Send + Sync {
    async fn save_to(&self, destination: &Path) -> Result<u64>;
}

#[derive(Clone)]
pub struct LocalFileAttachmentSource {
    source_path: PathBuf,
}

impl LocalFileAttachmentSource {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self {
            source_path: path.into(),
        }
    }
}

#[async_trait]
impl AttachmentSource for LocalFileAttachmentSource {
    async fn save_to(&self, destination: &Path) -> Result<u64> {
        if let Some(parent) = destination.parent() {
            fs::create_dir_all(parent).await?;
        }
        let copied = fs::copy(&self.source_path, destination)
            .await
            .with_context(|| format!("failed to copy attachment {}", self.source_path.display()))?;
        Ok(copied)
    }
}

pub struct PendingAttachment {
    pub kind: AttachmentKind,
    pub original_name: Option<String>,
    pub media_type: Option<String>,
    pub size_bytes: Option<u64>,
    pub source: Arc<dyn AttachmentSource>,
}

impl PendingAttachment {
    pub fn new(
        kind: AttachmentKind,
        original_name: Option<String>,
        media_type: Option<String>,
        size_bytes: Option<u64>,
        source: Arc<dyn AttachmentSource>,
    ) -> Self {
        Self {
            kind,
            original_name,
            media_type,
            size_bytes,
            source,
        }
    }

    pub async fn materialize(self, attachments_dir: &Path) -> Result<StoredAttachment> {
        fs::create_dir_all(attachments_dir).await?;
        let attachment_id = Uuid::new_v4();
        let file_name = match self.original_name.as_deref() {
            Some(name) if !name.trim().is_empty() => sanitize_filename(name),
            _ => default_attachment_name(self.kind, attachment_id),
        };
        let destination = attachments_dir.join(format!("{}-{}", attachment_id, file_name));
        let size_bytes =
            self.source.save_to(&destination).await.with_context(|| {
                format!("failed to persist attachment {}", destination.display())
            })?;
        let size_bytes = if size_bytes == 0 {
            fs::metadata(&destination)
                .await
                .map(|metadata| metadata.len())
                .unwrap_or(0)
        } else {
            size_bytes
        };

        Ok(StoredAttachment {
            id: attachment_id,
            kind: self.kind,
            original_name: self.original_name,
            media_type: self.media_type,
            path: destination,
            size_bytes: self.size_bytes.unwrap_or(size_bytes),
        })
    }
}

pub struct IncomingMessage {
    pub remote_message_id: String,
    pub address: ChannelAddress,
    pub text: Option<String>,
    pub attachments: Vec<PendingAttachment>,
    pub stored_attachments: Vec<StoredAttachment>,
    pub control: Option<IncomingControl>,
}

#[derive(Clone, Debug)]
pub enum IncomingControl {
    ConversationClosed { reason: String },
}

#[derive(Clone, Debug)]
pub enum ConversationProbe {
    Available { member_count: Option<u64> },
    Unavailable { reason: String },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ProgressFeedbackFinalState {
    Done,
    Failed,
}

#[derive(Clone, Debug)]
pub struct ProgressFeedback {
    pub turn_id: String,
    pub text: String,
    pub important: bool,
    pub final_state: Option<ProgressFeedbackFinalState>,
    pub message_id: Option<String>,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub enum ProgressFeedbackUpdate {
    #[default]
    Unchanged,
    StoreMessage {
        message_id: String,
    },
    ClearMessage,
}

#[async_trait]
pub trait Channel: Send + Sync {
    fn id(&self) -> &str;

    async fn run(self: Arc<Self>, sender: mpsc::Sender<IncomingMessage>) -> Result<()>;

    /// Send a platform-native media group.
    ///
    /// Channel implementations may translate rich text or Markdown-like agent output
    /// into the provider's supported formatting at send time. That translation is a
    /// channel responsibility rather than a prompt contract.
    async fn send_media_group(
        &self,
        address: &ChannelAddress,
        images: Vec<crate::domain::OutgoingAttachment>,
    ) -> Result<()>;

    /// Send a message to the user on this channel.
    ///
    /// Channel implementations may translate rich text or Markdown-like agent output
    /// into the provider's supported formatting at send time. That translation is a
    /// channel responsibility rather than a prompt contract.
    async fn send(&self, address: &ChannelAddress, message: OutgoingMessage) -> Result<()>;

    async fn set_processing(&self, address: &ChannelAddress, state: ProcessingState) -> Result<()>;

    async fn probe_conversation(
        &self,
        _address: &ChannelAddress,
    ) -> Result<Option<ConversationProbe>> {
        Ok(None)
    }

    async fn update_progress_feedback(
        &self,
        _address: &ChannelAddress,
        _feedback: ProgressFeedback,
    ) -> Result<ProgressFeedbackUpdate> {
        Ok(ProgressFeedbackUpdate::Unchanged)
    }

    fn processing_keepalive_interval(&self, _state: ProcessingState) -> Option<Duration> {
        None
    }
}

fn default_attachment_name(kind: AttachmentKind, id: Uuid) -> String {
    let suffix = match kind {
        AttachmentKind::Image => "image.bin",
        AttachmentKind::File => "file.bin",
    };
    format!("{}-{}", id.simple(), suffix)
}

fn sanitize_filename(input: &str) -> String {
    let sanitized: String = input
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || matches!(ch, '.' | '_' | '-') {
                ch
            } else {
                '_'
            }
        })
        .collect();

    let sanitized = sanitized.trim_matches('_');
    if sanitized.is_empty() {
        "attachment.bin".to_string()
    } else if sanitized.len() > 120 {
        sanitized[..120].to_string()
    } else {
        sanitized.to_string()
    }
}

pub fn ensure_existing_file(path: &Path) -> Result<()> {
    if !path.exists() {
        return Err(anyhow!(
            "attachment path does not exist: {}",
            path.display()
        ));
    }
    if !path.is_file() {
        return Err(anyhow!(
            "attachment path is not a regular file: {}",
            path.display()
        ));
    }
    Ok(())
}
