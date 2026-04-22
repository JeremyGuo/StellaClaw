use crate::channel::{
    Channel, IncomingMessage, LocalFileAttachmentSource, PendingAttachment, ensure_existing_file,
};
use crate::config::CommandLineChannelConfig;
use crate::domain::{AttachmentKind, ChannelAddress, OutgoingMessage, ProcessingState};
use anyhow::Result;
use async_trait::async_trait;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::io::{self, AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::mpsc;
use tracing::info;

pub struct CommandLineChannel {
    config: CommandLineChannelConfig,
}

impl CommandLineChannel {
    pub fn new(config: CommandLineChannelConfig) -> Self {
        Self { config }
    }

    fn address(&self) -> ChannelAddress {
        ChannelAddress {
            channel_id: self.config.id.clone(),
            conversation_id: "stdin".to_string(),
            user_id: Some("local-user".to_string()),
            display_name: Some("Local CLI".to_string()),
        }
    }

    async fn print_prompt(&self) -> Result<()> {
        let mut stdout = io::stdout();
        stdout.write_all(self.config.prompt.as_bytes()).await?;
        stdout.flush().await?;
        Ok(())
    }

    fn parse_input_line(&self, line: &str) -> Result<IncomingMessage> {
        let trimmed = line.trim();
        let address = self.address();

        if let Some(path) = trimmed.strip_prefix("/file ") {
            let file_path = PathBuf::from(path.trim());
            ensure_existing_file(&file_path)?;
            return Ok(IncomingMessage {
                remote_message_id: uuid::Uuid::new_v4().to_string(),
                address,
                text: None,
                attachments: vec![PendingAttachment::new(
                    AttachmentKind::File,
                    file_path
                        .file_name()
                        .map(|name| name.to_string_lossy().to_string()),
                    None,
                    None,
                    Arc::new(LocalFileAttachmentSource::new(file_path)),
                )],
                stored_attachments: Vec::new(),
                control: None,
            });
        }

        if let Some(path) = trimmed.strip_prefix("/image ") {
            let file_path = PathBuf::from(path.trim());
            ensure_existing_file(&file_path)?;
            return Ok(IncomingMessage {
                remote_message_id: uuid::Uuid::new_v4().to_string(),
                address,
                text: None,
                attachments: vec![PendingAttachment::new(
                    AttachmentKind::Image,
                    file_path
                        .file_name()
                        .map(|name| name.to_string_lossy().to_string()),
                    None,
                    None,
                    Arc::new(LocalFileAttachmentSource::new(file_path)),
                )],
                stored_attachments: Vec::new(),
                control: None,
            });
        }

        Ok(IncomingMessage {
            remote_message_id: uuid::Uuid::new_v4().to_string(),
            address,
            text: Some(line.to_string()),
            attachments: Vec::new(),
            stored_attachments: Vec::new(),
            control: None,
        })
    }
}

#[async_trait]
impl Channel for CommandLineChannel {
    fn id(&self) -> &str {
        &self.config.id
    }

    async fn run(self: Arc<Self>, sender: mpsc::Sender<IncomingMessage>) -> Result<()> {
        let stdin = BufReader::new(io::stdin());
        let mut lines = stdin.lines();
        info!(
            log_stream = "channel",
            log_key = %self.config.id,
            kind = "cli_ready",
            "command line channel is ready"
        );
        self.print_prompt().await?;
        while let Some(line) = lines.next_line().await? {
            let message = self.parse_input_line(&line)?;
            sender.send(message).await.ok();
            self.print_prompt().await?;
        }
        Ok(())
    }

    async fn send(&self, _address: &ChannelAddress, message: OutgoingMessage) -> Result<()> {
        tracing::debug!(
            log_stream = "channel",
            log_key = %self.config.id,
            kind = "cli_send",
            has_text = message.text.is_some(),
            image_count = message.images.len() as u64,
            attachment_count = message.attachments.len() as u64,
            has_options = message.options.is_some(),
            "sending message to CLI user"
        );
        let mut stdout = io::stdout();
        self.send_media_group(_address, message.images).await?;
        if let Some(text) = message.text {
            stdout
                .write_all(format!("\nagent> {}\n", text).as_bytes())
                .await?;
        }
        for attachment in message.attachments {
            stdout
                .write_all(format!("agent> [file] {}\n", attachment.path.display()).as_bytes())
                .await?;
        }
        if let Some(options) = message.options {
            stdout
                .write_all(format!("agent> {}\n", options.prompt).as_bytes())
                .await?;
            for option in options.options {
                stdout
                    .write_all(
                        format!("agent> [option] {} -> {}\n", option.label, option.value)
                            .as_bytes(),
                    )
                    .await?;
            }
        }
        stdout.flush().await?;
        self.print_prompt().await?;
        Ok(())
    }

    async fn send_media_group(
        &self,
        _address: &ChannelAddress,
        images: Vec<crate::domain::OutgoingAttachment>,
    ) -> Result<()> {
        let mut stdout = io::stdout();
        for image in images {
            stdout
                .write_all(format!("agent> [image] {}\n", image.path.display()).as_bytes())
                .await?;
        }
        Ok(())
    }

    async fn set_processing(
        &self,
        _address: &ChannelAddress,
        state: ProcessingState,
    ) -> Result<()> {
        if state == ProcessingState::Typing {
            tracing::debug!(
                log_stream = "channel",
                log_key = %self.config.id,
                kind = "typing",
                "CLI channel set to typing"
            );
            let mut stdout = io::stdout();
            stdout.write_all(b"\nagent> [typing]\n").await?;
            stdout.flush().await?;
        }
        Ok(())
    }
}
