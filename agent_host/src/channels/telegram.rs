use crate::channel::{
    AttachmentSource, Channel, IncomingControl, IncomingMessage, PendingAttachment,
};
use crate::config::{BotCommandConfig, TelegramChannelConfig};
use crate::domain::{
    AttachmentKind, ChannelAddress, OutgoingAttachment, OutgoingMessage, ProcessingState,
};
use anyhow::{Context, Result, anyhow};
use async_trait::async_trait;
use pulldown_cmark::{CodeBlockKind, Event, Options, Parser, Tag, TagEnd};
use reqwest::Client;
use reqwest::multipart::{Form, Part};
use serde::Deserialize;
use serde_json::json;
use std::collections::{HashMap, VecDeque};
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::fs;
use tokio::sync::{Mutex, mpsc};
use tracing::{info, warn};

pub struct TelegramChannel {
    id: String,
    bot_token: String,
    api_base_url: String,
    poll_timeout_seconds: u64,
    poll_interval_ms: u64,
    commands: Vec<BotCommandConfig>,
    client: Client,
    bot_username: Mutex<Option<String>>,
    bot_user_id: Mutex<Option<i64>>,
    chat_member_counts: Mutex<HashMap<i64, (u64, Instant)>>,
    pending_outbound: Mutex<VecDeque<PendingOutbound>>,
}

#[derive(Clone, Debug)]
struct PendingOutbound {
    address: ChannelAddress,
    message: OutgoingMessage,
}

impl TelegramChannel {
    const MAX_POLL_BACKOFF_SECONDS: u64 = 30;
    const MAX_SEND_RETRIES: u32 = 3;
    const MAX_MESSAGE_CHARS: usize = 4096;
    const MAX_CAPTION_CHARS: usize = 1024;

    pub fn from_config(config: TelegramChannelConfig) -> Result<Self> {
        let bot_token = match config.bot_token {
            Some(token) if !token.trim().is_empty() => token,
            _ => std::env::var(&config.bot_token_env).with_context(|| {
                format!(
                    "telegram channel {} requires bot_token or env {}",
                    config.id, config.bot_token_env
                )
            })?,
        };

        Ok(Self {
            id: config.id,
            bot_token,
            api_base_url: config.api_base_url.trim_end_matches('/').to_string(),
            poll_timeout_seconds: config.poll_timeout_seconds,
            poll_interval_ms: config.poll_interval_ms,
            commands: config.commands,
            client: Client::new(),
            bot_username: Mutex::new(None),
            bot_user_id: Mutex::new(None),
            chat_member_counts: Mutex::new(HashMap::new()),
            pending_outbound: Mutex::new(VecDeque::new()),
        })
    }

    fn method_url(&self, method: &str) -> String {
        format!("{}/bot{}/{}", self.api_base_url, self.bot_token, method)
    }

    fn redact_sensitive_text(&self, text: &str) -> String {
        text.replace(&self.bot_token, "[REDACTED_TELEGRAM_BOT_TOKEN]")
    }

    async fn call_api<T: for<'de> Deserialize<'de>>(
        &self,
        method: &str,
        payload: serde_json::Value,
    ) -> Result<T> {
        let max_attempts = self.max_send_attempts(method);
        for attempt in 1..=max_attempts {
            let response = match self
                .client
                .post(self.method_url(method))
                .json(&payload)
                .send()
                .await
            {
                Ok(response) => response,
                Err(error) => {
                    let error = anyhow!(
                        "{}",
                        self.redact_sensitive_text(&format!(
                            "telegram API call {} failed: {error:#}",
                            method
                        ))
                    );
                    if attempt < max_attempts && self.should_retry_transport_error(method) {
                        self.log_send_retry(method, attempt, max_attempts, &error);
                        tokio::time::sleep(Duration::from_secs(u64::from(attempt))).await;
                        continue;
                    }
                    return Err(error);
                }
            };
            let envelope: TelegramEnvelope<T> = response.json().await.map_err(|error| {
                anyhow!(
                    "{}",
                    self.redact_sensitive_text(&format!(
                        "telegram API {} returned invalid JSON: {error:#}",
                        method
                    ))
                )
            })?;
            if !envelope.ok {
                let error = anyhow!(
                    "telegram API {} failed: {}",
                    method,
                    self.redact_sensitive_text(
                        &envelope
                            .description
                            .unwrap_or_else(|| "unknown error".to_string()),
                    )
                );
                if attempt < max_attempts && self.should_retry_api_error(method, &error) {
                    self.log_send_retry(method, attempt, max_attempts, &error);
                    tokio::time::sleep(Duration::from_secs(u64::from(attempt))).await;
                    continue;
                }
                return Err(error);
            }
            return envelope
                .result
                .ok_or_else(|| anyhow!("telegram API {} returned no result", method));
        }
        unreachable!("telegram API retry loop exhausted without returning")
    }

    async fn call_multipart(&self, method: &str, form: Form) -> Result<serde_json::Value> {
        let response = self
            .client
            .post(self.method_url(method))
            .multipart(form)
            .send()
            .await
            .map_err(|error| {
                anyhow!(
                    "{}",
                    self.redact_sensitive_text(&format!(
                        "telegram multipart API call {} failed: {error:#}",
                        method
                    ))
                )
            })?;
        let envelope: TelegramEnvelope<serde_json::Value> =
            response.json().await.map_err(|error| {
                anyhow!(
                    "{}",
                    self.redact_sensitive_text(&format!(
                        "telegram API {} returned invalid JSON: {error:#}",
                        method
                    ))
                )
            })?;
        if !envelope.ok {
            return Err(anyhow!(
                "telegram API {} failed: {}",
                method,
                self.redact_sensitive_text(
                    &envelope
                        .description
                        .unwrap_or_else(|| "unknown error".to_string()),
                )
            ));
        }
        envelope
            .result
            .ok_or_else(|| anyhow!("telegram API {} returned no result", method))
    }

    fn max_send_attempts(&self, method: &str) -> u32 {
        match method {
            "sendMessage" | "sendPhoto" | "sendDocument" | "sendMediaGroup" | "sendChatAction" => {
                Self::MAX_SEND_RETRIES
            }
            _ => 1,
        }
    }

    fn should_retry_transport_error(&self, method: &str) -> bool {
        self.max_send_attempts(method) > 1
    }

    fn should_retry_api_error(&self, method: &str, error: &anyhow::Error) -> bool {
        if self.max_send_attempts(method) <= 1 {
            return false;
        }
        let message = format!("{error:#}").to_ascii_lowercase();
        message.contains("too many requests")
            || message.contains("retry after")
            || message.contains("timed out")
            || message.contains("timeout")
            || message.contains("internal server error")
            || message.contains("bad gateway")
            || message.contains("gateway timeout")
    }

    fn log_send_retry(&self, method: &str, attempt: u32, max_attempts: u32, error: &anyhow::Error) {
        warn!(
            log_stream = "channel",
            log_key = %self.id,
            kind = "telegram_send_retry",
            method = method,
            attempt = attempt,
            max_attempts = max_attempts,
            error = %format!("{error:#}"),
            "telegram send failed; retrying"
        );
    }

    fn should_defer_outbound_message(&self, error: &anyhow::Error) -> bool {
        let message = format!("{error:#}").to_ascii_lowercase();
        message.contains("telegram api call send")
            || message.contains("telegram multipart api call send")
            || message.contains("too many requests")
            || message.contains("retry after")
            || message.contains("timed out")
            || message.contains("timeout")
            || message.contains("temporary failure in name resolution")
            || message.contains("dns error")
            || message.contains("internal server error")
            || message.contains("bad gateway")
            || message.contains("gateway timeout")
            || message.contains("service unavailable")
            || message.contains("connection reset")
            || message.contains("connection refused")
            || message.contains("broken pipe")
    }

    async fn flush_pending_outbound_queue(&self, trigger: &str) {
        let mut pending_outbound = self.pending_outbound.lock().await;
        if pending_outbound.is_empty() {
            return;
        }
        info!(
            log_stream = "channel",
            log_key = %self.id,
            kind = "telegram_send_queue_flush_started",
            trigger = trigger,
            pending_count = pending_outbound.len() as u64,
            "flushing queued telegram messages"
        );
        while let Some(next) = pending_outbound.front().cloned() {
            match self
                .deliver_outgoing_message(&next.address, next.message.clone())
                .await
            {
                Ok(()) => {
                    pending_outbound.pop_front();
                    info!(
                        log_stream = "channel",
                        log_key = %self.id,
                        kind = "telegram_send_queue_item_delivered",
                        trigger = trigger,
                        conversation_id = %next.address.conversation_id,
                        pending_count = pending_outbound.len() as u64,
                        "delivered queued telegram message"
                    );
                }
                Err(error) if self.should_defer_outbound_message(&error) => {
                    warn!(
                        log_stream = "channel",
                        log_key = %self.id,
                        kind = "telegram_send_queue_blocked",
                        trigger = trigger,
                        conversation_id = %next.address.conversation_id,
                        pending_count = pending_outbound.len() as u64,
                        error = %format!("{error:#}"),
                        "telegram send queue is still blocked"
                    );
                    return;
                }
                Err(error) => {
                    pending_outbound.pop_front();
                    warn!(
                        log_stream = "channel",
                        log_key = %self.id,
                        kind = "telegram_send_queue_item_dropped",
                        trigger = trigger,
                        conversation_id = %next.address.conversation_id,
                        pending_count = pending_outbound.len() as u64,
                        error = %format!("{error:#}"),
                        "dropping queued telegram message after non-retryable failure"
                    );
                }
            }
        }
        info!(
            log_stream = "channel",
            log_key = %self.id,
            kind = "telegram_send_queue_flushed",
            trigger = trigger,
            "finished flushing queued telegram messages"
        );
    }

    async fn set_my_commands_for_scope(
        &self,
        scope: serde_json::Value,
        commands: &[serde_json::Value],
    ) -> Result<()> {
        self.call_api::<bool>(
            "setMyCommands",
            json!({
                "scope": scope,
                "commands": commands,
            }),
        )
        .await?;
        Ok(())
    }

    async fn set_my_commands(&self) -> Result<()> {
        let commands = self
            .commands
            .iter()
            .map(|command| {
                json!({
                    "command": command.command,
                    "description": command.description,
                })
            })
            .collect::<Vec<_>>();
        self.set_my_commands_for_scope(json!({ "type": "all_private_chats" }), &commands)
            .await?;
        self.set_my_commands_for_scope(json!({ "type": "all_group_chats" }), &commands)
            .await?;
        Ok(())
    }

    async fn refresh_bot_identity(&self) {
        match self.call_api::<TelegramUser>("getMe", json!({})).await {
            Ok(user) => {
                let username = user.username.clone();
                *self.bot_user_id.lock().await = Some(user.id);
                *self.bot_username.lock().await = username.clone();
                info!(
                    log_stream = "channel",
                    log_key = %self.id,
                    kind = "telegram_bot_identity",
                    bot_username = username.as_deref().unwrap_or(""),
                    "telegram bot identity loaded"
                );
            }
            Err(error) => {
                warn!(
                    log_stream = "channel",
                    log_key = %self.id,
                    kind = "telegram_bot_identity_failed",
                    error = %format!("{error:#}"),
                    "failed to fetch telegram bot identity"
                );
            }
        }
    }

    fn build_address(&self, message: &TelegramMessage) -> ChannelAddress {
        let display_name = message.from.as_ref().map(|user| {
            let mut pieces = Vec::new();
            if !user.first_name.trim().is_empty() {
                pieces.push(user.first_name.trim());
            }
            if let Some(last_name) = user.last_name.as_deref() {
                if !last_name.trim().is_empty() {
                    pieces.push(last_name.trim());
                }
            }
            if pieces.is_empty() {
                user.username.clone().unwrap_or_else(|| user.id.to_string())
            } else {
                pieces.join(" ")
            }
        });

        ChannelAddress {
            channel_id: self.id.clone(),
            conversation_id: message.chat.id.to_string(),
            user_id: message.from.as_ref().map(|user| user.id.to_string()),
            display_name,
        }
    }

    fn collect_attachments(&self, message: &TelegramMessage) -> Vec<PendingAttachment> {
        let mut attachments = Vec::new();

        if let Some(photo) = message.photo.as_ref().and_then(|sizes| sizes.last()) {
            let file_name = format!("photo_{}.jpg", photo.file_unique_id);
            attachments.push(PendingAttachment::new(
                AttachmentKind::Image,
                Some(file_name),
                Some("image/jpeg".to_string()),
                photo.file_size,
                Arc::new(TelegramAttachmentSource::new(
                    self.client.clone(),
                    self.api_base_url.clone(),
                    self.bot_token.clone(),
                    photo.file_id.clone(),
                )),
            ));
        }

        if let Some(document) = message.document.as_ref() {
            attachments.push(PendingAttachment::new(
                if document
                    .mime_type
                    .as_deref()
                    .unwrap_or_default()
                    .starts_with("image/")
                {
                    AttachmentKind::Image
                } else {
                    AttachmentKind::File
                },
                document.file_name.clone(),
                document.mime_type.clone(),
                document.file_size,
                Arc::new(TelegramAttachmentSource::new(
                    self.client.clone(),
                    self.api_base_url.clone(),
                    self.bot_token.clone(),
                    document.file_id.clone(),
                )),
            ));
        }

        if let Some(video) = message.video.as_ref() {
            attachments.push(PendingAttachment::new(
                AttachmentKind::File,
                video
                    .file_name
                    .clone()
                    .or_else(|| Some(format!("video_{}.bin", video.file_unique_id))),
                video.mime_type.clone(),
                video.file_size,
                Arc::new(TelegramAttachmentSource::new(
                    self.client.clone(),
                    self.api_base_url.clone(),
                    self.bot_token.clone(),
                    video.file_id.clone(),
                )),
            ));
        }

        if let Some(audio) = message.audio.as_ref() {
            attachments.push(PendingAttachment::new(
                AttachmentKind::File,
                audio
                    .file_name
                    .clone()
                    .or_else(|| Some(format!("audio_{}.bin", audio.file_unique_id))),
                audio.mime_type.clone(),
                audio.file_size,
                Arc::new(TelegramAttachmentSource::new(
                    self.client.clone(),
                    self.api_base_url.clone(),
                    self.bot_token.clone(),
                    audio.file_id.clone(),
                )),
            ));
        }

        attachments
    }

    async fn should_accept_message(
        &self,
        message: &TelegramMessage,
        text: Option<&str>,
        attachments_count: usize,
    ) -> bool {
        if message.chat.kind == "private" {
            return true;
        }
        if self.group_behaves_like_direct_chat(message).await {
            return text.map(str::trim).is_some_and(|value| !value.is_empty())
                || attachments_count > 0;
        }
        let Some(text) = text.map(str::trim).filter(|value| !value.is_empty()) else {
            return false;
        };
        let bot_username = self.bot_username.lock().await.clone();
        let Some(bot_username) = bot_username else {
            return false;
        };
        let mention = format!("@{}", bot_username.to_ascii_lowercase());
        text.to_ascii_lowercase().contains(&mention)
    }

    async fn group_behaves_like_direct_chat(&self, message: &TelegramMessage) -> bool {
        if !matches!(message.chat.kind.as_str(), "group" | "supergroup") {
            return false;
        }
        let now = Instant::now();
        if let Some((count, observed_at)) = self
            .chat_member_counts
            .lock()
            .await
            .get(&message.chat.id)
            .copied()
            && now.duration_since(observed_at) <= Duration::from_secs(60)
        {
            return count <= 2;
        }
        match self
            .call_api::<u64>(
                "getChatMemberCount",
                json!({
                    "chat_id": message.chat.id,
                }),
            )
            .await
        {
            Ok(count) => {
                self.chat_member_counts
                    .lock()
                    .await
                    .insert(message.chat.id, (count, now));
                count <= 2
            }
            Err(error) => {
                warn!(
                    log_stream = "channel",
                    log_key = %self.id,
                    kind = "telegram_chat_member_count_failed",
                    conversation_id = message.chat.id.to_string(),
                    error = %format!("{error:#}"),
                    "failed to fetch telegram chat member count"
                );
                false
            }
        }
    }

    async fn bot_was_removed_from_chat(&self, message: &TelegramMessage) -> bool {
        let Some(left_chat_member) = message.left_chat_member.as_ref() else {
            return false;
        };
        let bot_user_id = *self.bot_user_id.lock().await;
        bot_user_id.is_some_and(|bot_id| bot_id == left_chat_member.id)
    }

    async fn send_photo(&self, chat_id: &str, attachment: OutgoingAttachment) -> Result<()> {
        let file_name = attachment
            .path
            .file_name()
            .map(|value| value.to_string_lossy().to_string())
            .unwrap_or_else(|| "image.bin".to_string());
        let bytes = fs::read(&attachment.path)
            .await
            .with_context(|| format!("failed to read image {}", attachment.path.display()))?;
        let mut trailing_text_chunks = Vec::new();
        let form = Form::new()
            .text("chat_id", chat_id.to_string())
            .part("photo", Part::bytes(bytes).file_name(file_name));
        let form = if let Some(caption) = attachment.caption {
            let mut iter = split_markdown_message(&caption, Self::MAX_CAPTION_CHARS).into_iter();
            let caption = iter.next();
            trailing_text_chunks = iter.collect();
            let translated = translate_markdown_to_telegram_html(caption.as_deref().unwrap_or(""));
            form.text("caption", translated.text)
                .text("parse_mode", "HTML".to_string())
        } else {
            form
        };
        self.call_multipart("sendPhoto", form).await?;
        for chunk in trailing_text_chunks {
            self.send_text_chunks(chat_id, &chunk).await?;
        }
        Ok(())
    }

    async fn send_document(&self, chat_id: &str, attachment: OutgoingAttachment) -> Result<()> {
        let file_name = attachment
            .path
            .file_name()
            .map(|value| value.to_string_lossy().to_string())
            .unwrap_or_else(|| "attachment.bin".to_string());
        let bytes = fs::read(&attachment.path)
            .await
            .with_context(|| format!("failed to read attachment {}", attachment.path.display()))?;
        let mut trailing_text_chunks = Vec::new();
        let form = Form::new()
            .text("chat_id", chat_id.to_string())
            .part("document", Part::bytes(bytes).file_name(file_name));
        let form = if let Some(caption) = attachment.caption {
            let mut iter = split_markdown_message(&caption, Self::MAX_CAPTION_CHARS).into_iter();
            let caption = iter.next();
            trailing_text_chunks = iter.collect();
            let translated = translate_markdown_to_telegram_html(caption.as_deref().unwrap_or(""));
            form.text("caption", translated.text)
                .text("parse_mode", "HTML".to_string())
        } else {
            form
        };
        self.call_multipart("sendDocument", form).await?;
        for chunk in trailing_text_chunks {
            self.send_text_chunks(chat_id, &chunk).await?;
        }
        Ok(())
    }

    async fn send_photo_group(
        &self,
        chat_id: &str,
        images: Vec<OutgoingAttachment>,
        shared_caption: Option<String>,
    ) -> Result<()> {
        if images.len() <= 1 {
            if let Some(mut image) = images.into_iter().next() {
                if image.caption.is_none() {
                    image.caption = shared_caption;
                }
                self.send_photo(chat_id, image).await?;
            }
            return Ok(());
        }

        let mut form = Form::new().text("chat_id", chat_id.to_string());
        let mut media = Vec::new();
        for (index, image) in images.into_iter().enumerate() {
            let file_name = image
                .path
                .file_name()
                .map(|value| value.to_string_lossy().to_string())
                .unwrap_or_else(|| format!("image-{}.bin", index));
            let bytes = fs::read(&image.path)
                .await
                .with_context(|| format!("failed to read image {}", image.path.display()))?;
            let field_name = format!("file{}", index);
            let mut item = json!({
                "type": "photo",
                "media": format!("attach://{}", field_name),
            });
            let caption = if image.caption.is_some() {
                image.caption
            } else if index == 0 {
                shared_caption.clone()
            } else {
                None
            };
            if let Some(caption) = caption
                && let Some(object) = item.as_object_mut()
            {
                let translated = translate_markdown_to_telegram_html(&caption);
                object.insert("caption".to_string(), json!(translated.text));
                object.insert("parse_mode".to_string(), json!("HTML"));
            }
            media.push(item);
            form = form.part(field_name, Part::bytes(bytes).file_name(file_name));
        }
        form = form.text(
            "media",
            serde_json::to_string(&media).context("failed to serialize telegram media group")?,
        );
        self.call_multipart("sendMediaGroup", form).await?;
        Ok(())
    }

    async fn send_media_group_with_caption(
        &self,
        address: &ChannelAddress,
        images: Vec<OutgoingAttachment>,
        caption: Option<String>,
    ) -> Result<()> {
        self.send_photo_group(&address.conversation_id, images, caption)
            .await
    }

    async fn send_text_chunks(&self, chat_id: &str, text: &str) -> Result<()> {
        for chunk in split_markdown_message(text, Self::MAX_MESSAGE_CHARS) {
            let translated = translate_markdown_to_telegram_html(&chunk);
            self.call_api::<serde_json::Value>(
                "sendMessage",
                json!({
                    "chat_id": chat_id,
                    "text": translated.text,
                    "parse_mode": "HTML",
                }),
            )
            .await?;
        }
        Ok(())
    }

    async fn deliver_outgoing_message(
        &self,
        address: &ChannelAddress,
        message: OutgoingMessage,
    ) -> Result<()> {
        let OutgoingMessage {
            text,
            images,
            attachments,
        } = message;
        let mut trailing_text_chunks = Vec::new();
        if images.len() >= 2 {
            let (caption, trailing) = text
                .as_deref()
                .map(|value| {
                    let chunks = split_markdown_message(value, Self::MAX_CAPTION_CHARS);
                    let mut iter = chunks.into_iter();
                    let caption = iter.next();
                    let trailing = iter.collect::<Vec<_>>();
                    (caption, trailing)
                })
                .unwrap_or((None, Vec::new()));
            trailing_text_chunks = trailing;
            self.send_media_group_with_caption(address, images, caption)
                .await?;
        } else {
            let mut images = images;
            let has_images = !images.is_empty();
            if let Some(text) = text.as_deref()
                && has_images
            {
                let chunks = split_markdown_message(text, Self::MAX_CAPTION_CHARS);
                let mut iter = chunks.into_iter();
                let caption = iter.next();
                trailing_text_chunks = iter.collect();
                if let Some(image) = images.first_mut()
                    && image.caption.is_none()
                {
                    image.caption = caption;
                }
            }
            self.send_photo_group(&address.conversation_id, images, None)
                .await?;
            if let Some(text) = text.as_deref()
                && !has_images
            {
                self.send_text_chunks(&address.conversation_id, text)
                    .await?;
            }
        }
        for chunk in trailing_text_chunks {
            self.send_text_chunks(&address.conversation_id, &chunk)
                .await?;
        }
        for attachment in attachments {
            self.send_document(&address.conversation_id, attachment)
                .await?;
        }

        Ok(())
    }
}

#[async_trait]
impl Channel for TelegramChannel {
    fn id(&self) -> &str {
        &self.id
    }

    async fn run(self: Arc<Self>, sender: mpsc::Sender<IncomingMessage>) -> Result<()> {
        let mut offset = None::<i64>;
        self.refresh_bot_identity().await;
        self.set_my_commands().await?;
        info!(
            log_stream = "channel",
            log_key = %self.id,
            commands_count = self.commands.len() as u64,
            commands = ?self.commands,
            kind = "telegram_commands_registered",
            "telegram commands registered"
        );
        info!(
            log_stream = "channel",
            log_key = %self.id,
            kind = "telegram_polling_started",
            poll_timeout_seconds = self.poll_timeout_seconds,
            poll_interval_ms = self.poll_interval_ms,
            "telegram polling loop started"
        );
        let mut consecutive_failures = 0u32;
        loop {
            let payload = json!({
                "timeout": self.poll_timeout_seconds,
                "offset": offset,
                "allowed_updates": ["message"],
            });
            let updates: Vec<TelegramUpdate> = match self.call_api("getUpdates", payload).await {
                Ok(updates) => {
                    consecutive_failures = 0;
                    self.flush_pending_outbound_queue("get_updates").await;
                    updates
                }
                Err(error) => {
                    consecutive_failures = consecutive_failures.saturating_add(1);
                    let backoff_seconds =
                        poll_backoff_seconds(consecutive_failures, Self::MAX_POLL_BACKOFF_SECONDS);
                    warn!(
                        log_stream = "channel",
                        log_key = %self.id,
                        kind = "telegram_polling_retry",
                        consecutive_failures = consecutive_failures,
                        backoff_seconds = backoff_seconds,
                        error = %format!("{error:#}"),
                        "telegram getUpdates failed; retrying"
                    );
                    tokio::time::sleep(Duration::from_secs(backoff_seconds)).await;
                    continue;
                }
            };
            for update in updates {
                offset = Some(update.update_id + 1);
                let Some(message) = update.message else {
                    continue;
                };
                if self.bot_was_removed_from_chat(&message).await {
                    let incoming = IncomingMessage {
                        remote_message_id: message.message_id.to_string(),
                        address: self.build_address(&message),
                        text: None,
                        attachments: Vec::new(),
                        control: Some(IncomingControl::ConversationClosed {
                            reason: "telegram bot was removed from the chat".to_string(),
                        }),
                    };
                    if sender.send(incoming).await.is_err() {
                        warn!(
                            log_stream = "channel",
                            log_key = %self.id,
                            kind = "telegram_receiver_closed",
                            "telegram receiver closed; stopping polling loop"
                        );
                        return Ok(());
                    }
                    continue;
                }
                let text = message.text.clone().or_else(|| message.caption.clone());
                let attachments = self.collect_attachments(&message);
                if text.as_deref().is_none_or(|value| value.trim().is_empty())
                    && attachments.is_empty()
                {
                    info!(
                        log_stream = "channel",
                        log_key = %self.id,
                        kind = "telegram_ignored_empty_message",
                        conversation_id = message.chat.id.to_string(),
                        remote_message_id = message.message_id.to_string(),
                        "ignoring telegram service/empty message without text or attachments"
                    );
                    continue;
                }
                if !self
                    .should_accept_message(&message, text.as_deref(), attachments.len())
                    .await
                {
                    info!(
                        log_stream = "channel",
                        log_key = %self.id,
                        kind = "telegram_ignored_group_message",
                        conversation_id = message.chat.id.to_string(),
                        remote_message_id = message.message_id.to_string(),
                        text_preview = text.as_deref().map(summarize_for_log),
                        "ignoring group message without bot mention or command"
                    );
                    continue;
                }
                let incoming = IncomingMessage {
                    remote_message_id: message.message_id.to_string(),
                    address: self.build_address(&message),
                    text,
                    attachments,
                    control: None,
                };
                if sender.send(incoming).await.is_err() {
                    warn!(
                        log_stream = "channel",
                        log_key = %self.id,
                        kind = "telegram_receiver_closed",
                        "telegram receiver closed; stopping polling loop"
                    );
                    return Ok(());
                }
            }
            tokio::time::sleep(tokio::time::Duration::from_millis(self.poll_interval_ms)).await;
        }
    }

    async fn send(&self, address: &ChannelAddress, message: OutgoingMessage) -> Result<()> {
        let pending = PendingOutbound {
            address: address.clone(),
            message: message.clone(),
        };
        let OutgoingMessage {
            text,
            images,
            attachments,
        } = message;
        info!(
            log_stream = "channel",
            log_key = %self.id,
            kind = "telegram_send",
            conversation_id = %address.conversation_id,
            has_text = text.is_some(),
            text_preview = text.as_deref().map(summarize_for_log),
            image_count = images.len() as u64,
            attachment_count = attachments.len() as u64,
            attachment_names = ?attachments
                .iter()
                .filter_map(|item| item.path.file_name().map(|name| name.to_string_lossy().to_string()))
                .collect::<Vec<_>>(),
            "sending message to telegram user"
        );
        let queue_was_blocked = {
            let mut pending_outbound = self.pending_outbound.lock().await;
            let queue_was_blocked = !pending_outbound.is_empty();
            if queue_was_blocked {
                pending_outbound.push_back(pending.clone());
                info!(
                    log_stream = "channel",
                    log_key = %self.id,
                    kind = "telegram_send_enqueued",
                    conversation_id = %address.conversation_id,
                    pending_count = pending_outbound.len() as u64,
                    "queued telegram message behind blocked send queue"
                );
            }
            queue_was_blocked
        };
        if queue_was_blocked {
            self.flush_pending_outbound_queue("send").await;
            return Ok(());
        }
        match self
            .deliver_outgoing_message(
                address,
                OutgoingMessage {
                    text,
                    images,
                    attachments,
                },
            )
            .await
        {
            Ok(()) => Ok(()),
            Err(error) if self.should_defer_outbound_message(&error) => {
                let pending_count = {
                    let mut pending_outbound = self.pending_outbound.lock().await;
                    pending_outbound.push_back(pending);
                    pending_outbound.len() as u64
                };
                warn!(
                    log_stream = "channel",
                    log_key = %self.id,
                    kind = "telegram_send_deferred",
                    conversation_id = %address.conversation_id,
                    pending_count = pending_count,
                    error = %format!("{error:#}"),
                    "telegram send failed after retries; message queued for FIFO retry"
                );
                Ok(())
            }
            Err(error) => Err(error),
        }
    }

    async fn send_media_group(
        &self,
        address: &ChannelAddress,
        images: Vec<OutgoingAttachment>,
    ) -> Result<()> {
        self.send_photo_group(&address.conversation_id, images, None)
            .await
    }

    async fn set_processing(&self, address: &ChannelAddress, state: ProcessingState) -> Result<()> {
        if state == ProcessingState::Typing {
            info!(
                log_stream = "channel",
                log_key = %self.id,
                kind = "typing",
                conversation_id = %address.conversation_id,
                "telegram channel set to typing"
            );
            self.call_api::<serde_json::Value>(
                "sendChatAction",
                json!({
                    "chat_id": address.conversation_id,
                    "action": "typing",
                }),
            )
            .await?;
        }
        Ok(())
    }

    fn processing_keepalive_interval(&self, state: ProcessingState) -> Option<Duration> {
        if state == ProcessingState::Typing {
            Some(Duration::from_secs(4))
        } else {
            None
        }
    }
}

fn summarize_for_log(text: &str) -> String {
    const LIMIT: usize = 160;
    let compact = text.split_whitespace().collect::<Vec<_>>().join(" ");
    let mut chars = compact.chars();
    let summary: String = chars.by_ref().take(LIMIT).collect();
    if chars.next().is_some() {
        format!("{}...", summary)
    } else {
        summary
    }
}

fn poll_backoff_seconds(consecutive_failures: u32, cap_seconds: u64) -> u64 {
    let exponent = consecutive_failures.saturating_sub(1).min(5);
    2_u64.saturating_pow(exponent).min(cap_seconds).max(1)
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct TelegramFormattedText {
    text: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum TextContainer {
    Paragraph,
    Heading,
    BlockQuote,
}

fn translate_markdown_to_telegram_html(input: &str) -> TelegramFormattedText {
    let parser = Parser::new_ext(input, Options::all());
    let mut output = String::new();
    let mut list_stack: Vec<Option<u64>> = Vec::new();
    let mut blockquote_depth = 0usize;
    let mut pending_list_item = false;
    let mut text_stack: Vec<TextContainer> = Vec::new();
    let mut code_block_language: Option<String> = None;
    let mut code_block_buffer: Option<String> = None;
    let mut need_paragraph_break = false;

    for event in parser {
        match event {
            Event::Start(tag) => match tag {
                Tag::Paragraph => {
                    ensure_block_break(&mut output, &mut need_paragraph_break);
                    text_stack.push(TextContainer::Paragraph);
                }
                Tag::Heading { level, .. } => {
                    ensure_block_break(&mut output, &mut need_paragraph_break);
                    let _ = level;
                    output.push_str("<b>");
                    text_stack.push(TextContainer::Heading);
                }
                Tag::BlockQuote(_) => {
                    ensure_block_break(&mut output, &mut need_paragraph_break);
                    blockquote_depth += 1;
                    text_stack.push(TextContainer::BlockQuote);
                }
                Tag::List(start) => {
                    ensure_block_break(&mut output, &mut need_paragraph_break);
                    list_stack.push(start);
                }
                Tag::Item => {
                    if !output.is_empty() && !output.ends_with('\n') {
                        output.push('\n');
                    }
                    let prefix = if let Some(Some(next_number)) = list_stack.last_mut() {
                        let prefix = format!("{}. ", *next_number);
                        *next_number += 1;
                        prefix
                    } else {
                        "• ".to_string()
                    };
                    if blockquote_depth > 0 {
                        output.push_str(&"&gt; ".repeat(blockquote_depth));
                    }
                    output.push_str(&prefix);
                    pending_list_item = false;
                }
                Tag::Emphasis => output.push_str("<i>"),
                Tag::Strong => output.push_str("<b>"),
                Tag::Strikethrough => output.push_str("<s>"),
                Tag::Link { dest_url, .. } => {
                    output.push_str("<a href=\"");
                    output.push_str(&escape_html_attribute(&dest_url));
                    output.push_str("\">");
                }
                Tag::CodeBlock(kind) => {
                    ensure_block_break(&mut output, &mut need_paragraph_break);
                    let language = match kind {
                        CodeBlockKind::Indented => None,
                        CodeBlockKind::Fenced(language) => {
                            let trimmed = language.trim();
                            if trimmed.is_empty() {
                                None
                            } else {
                                Some(trimmed.to_string())
                            }
                        }
                    };
                    code_block_language = language;
                    code_block_buffer = Some(String::new());
                }
                _ => {}
            },
            Event::End(tag) => match tag {
                TagEnd::Paragraph => {
                    let _ = text_stack.pop();
                    need_paragraph_break = true;
                }
                TagEnd::Heading(_) => {
                    output.push_str("</b>");
                    let _ = text_stack.pop();
                    need_paragraph_break = true;
                }
                TagEnd::BlockQuote(_) => {
                    blockquote_depth = blockquote_depth.saturating_sub(1);
                    let _ = text_stack.pop();
                    need_paragraph_break = true;
                }
                TagEnd::List(_) => {
                    let _ = list_stack.pop();
                    need_paragraph_break = true;
                }
                TagEnd::Item => pending_list_item = false,
                TagEnd::Emphasis => output.push_str("</i>"),
                TagEnd::Strong => output.push_str("</b>"),
                TagEnd::Strikethrough => output.push_str("</s>"),
                TagEnd::Link => output.push_str("</a>"),
                TagEnd::CodeBlock => {
                    let code = code_block_buffer.take().unwrap_or_default();
                    if let Some(language) = code_block_language.take() {
                        output.push_str("<pre><code class=\"language-");
                        output.push_str(&escape_html_attribute(&language));
                        output.push_str("\">");
                        output.push_str(&escape_html_text(&code));
                        output.push_str("</code></pre>");
                    } else {
                        output.push_str("<pre>");
                        output.push_str(&escape_html_text(&code));
                        output.push_str("</pre>");
                    }
                    need_paragraph_break = true;
                }
                _ => {}
            },
            Event::Text(text) => {
                if let Some(buffer) = code_block_buffer.as_mut() {
                    buffer.push_str(&text);
                } else {
                    if blockquote_depth > 0 && starts_new_block_line(&output) && !pending_list_item
                    {
                        output.push_str(&"&gt; ".repeat(blockquote_depth));
                    }
                    output.push_str(&escape_html_text(&text));
                }
            }
            Event::Code(code) => {
                output.push_str("<code>");
                output.push_str(&escape_html_text(&code));
                output.push_str("</code>");
            }
            Event::SoftBreak => {
                if code_block_buffer.is_some() {
                    if let Some(buffer) = code_block_buffer.as_mut() {
                        buffer.push('\n');
                    }
                } else {
                    output.push('\n');
                    pending_list_item = false;
                }
            }
            Event::HardBreak => {
                output.push('\n');
                pending_list_item = false;
            }
            Event::Rule => {
                ensure_block_break(&mut output, &mut need_paragraph_break);
                output.push_str("──────────");
                need_paragraph_break = true;
            }
            Event::Html(html) | Event::InlineHtml(html) => {
                output.push_str(&escape_html_text(&html));
            }
            Event::InlineMath(math) => {
                output.push_str("<code>");
                output.push_str(&escape_html_text(&math));
                output.push_str("</code>");
            }
            Event::DisplayMath(math) => {
                ensure_block_break(&mut output, &mut need_paragraph_break);
                output.push_str("<pre>");
                output.push_str(&escape_html_text(&math));
                output.push_str("</pre>");
                need_paragraph_break = true;
            }
            Event::FootnoteReference(text) => {
                output.push('[');
                output.push_str(&escape_html_text(&text));
                output.push(']');
            }
            Event::TaskListMarker(checked) => {
                output.push_str(if checked { "☑ " } else { "☐ " });
            }
        }
    }

    TelegramFormattedText {
        text: output.trim().to_string(),
    }
}

fn ensure_block_break(output: &mut String, need_paragraph_break: &mut bool) {
    if output.is_empty() {
        *need_paragraph_break = false;
        return;
    }
    if *need_paragraph_break {
        if !output.ends_with("\n\n") {
            if output.ends_with('\n') {
                output.push('\n');
            } else {
                output.push_str("\n\n");
            }
        }
        *need_paragraph_break = false;
    } else if !output.ends_with('\n') {
        output.push('\n');
    }
}

fn starts_new_block_line(output: &str) -> bool {
    output.is_empty() || output.ends_with('\n')
}

fn escape_html_text(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

fn escape_html_attribute(value: &str) -> String {
    escape_html_text(value).replace('"', "&quot;")
}

fn split_markdown_message(input: &str, max_chars: usize) -> Vec<String> {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return Vec::new();
    }

    let chars: Vec<char> = trimmed.chars().collect();
    let mut cursor = 0usize;
    let mut chunks = Vec::new();
    while cursor < chars.len() {
        let remaining = chars.len() - cursor;
        let mut low = 1usize;
        let mut high = remaining;
        let mut best = 1usize;
        while low <= high {
            let mid = (low + high) / 2;
            let candidate: String = chars[cursor..cursor + mid].iter().collect();
            let translated_len = translate_markdown_to_telegram_html(&candidate)
                .text
                .chars()
                .count();
            if translated_len <= max_chars {
                best = mid;
                low = mid + 1;
            } else {
                high = mid.saturating_sub(1);
            }
        }

        let mut end = cursor + best;
        if end < chars.len()
            && let Some(adjusted) = prefer_split_boundary(&chars[cursor..end], best / 2)
        {
            end = cursor + adjusted;
        }

        let chunk: String = chars[cursor..end].iter().collect();
        let chunk = chunk.trim();
        if !chunk.is_empty() {
            chunks.push(chunk.to_string());
        }
        cursor = end;
        while cursor < chars.len() && chars[cursor].is_whitespace() {
            cursor += 1;
        }
    }
    chunks
}

fn prefer_split_boundary(chars: &[char], minimum_index: usize) -> Option<usize> {
    let text: String = chars.iter().collect();
    for needle in ["\n\n", "\n", " "] {
        if let Some(index) = text.rfind(needle) {
            let split_index = index + needle.len();
            if split_index >= minimum_index {
                return Some(text[..split_index].chars().count());
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::{
        TelegramChannel, TelegramChat, TelegramMessage, poll_backoff_seconds,
        split_markdown_message, translate_markdown_to_telegram_html,
    };
    use anyhow::anyhow;
    use reqwest::Client;
    use std::collections::{HashMap, VecDeque};
    use std::time::Instant;
    use tokio::sync::Mutex;

    #[test]
    fn translates_basic_markdown_to_telegram_html() {
        let translated = translate_markdown_to_telegram_html(
            "# Title\n\n**bold** and *italic* with [link](https://example.com).\n\n- one\n- two",
        );

        assert!(translated.text.contains("<b>Title</b>"));
        assert!(translated.text.contains("<b>bold</b>"));
        assert!(translated.text.contains("<i>italic</i>"));
        assert!(
            translated
                .text
                .contains("<a href=\"https://example.com\">link</a>")
        );
        assert!(translated.text.contains("• one"));
        assert!(translated.text.contains("• two"));
    }

    #[test]
    fn translates_code_blocks_and_escapes_html() {
        let translated =
            translate_markdown_to_telegram_html("```rust\nlet x = 1 < 2;\n```\n\n`inline <tag>`");

        assert!(
            translated
                .text
                .contains("<pre><code class=\"language-rust\">")
        );
        assert!(translated.text.contains("let x = 1 &lt; 2;"));
        assert!(translated.text.contains("<code>inline &lt;tag&gt;</code>"));
    }

    #[test]
    fn does_not_emit_unsupported_br_tags() {
        let translated = translate_markdown_to_telegram_html("line one  \nline two");

        assert!(translated.text.contains("line one\nline two"));
        assert!(!translated.text.contains("<br/>"));
        assert!(!translated.text.contains("<br>"));
    }

    #[test]
    fn backoff_grows_and_caps() {
        assert_eq!(poll_backoff_seconds(1, 30), 1);
        assert_eq!(poll_backoff_seconds(2, 30), 2);
        assert_eq!(poll_backoff_seconds(3, 30), 4);
        assert_eq!(poll_backoff_seconds(10, 30), 30);
    }

    #[test]
    fn splits_long_markdown_messages_into_multiple_chunks() {
        let input = format!(
            "{}\n\n{}\n\n{}",
            "a".repeat(2200),
            "b".repeat(2200),
            "c".repeat(2200)
        );
        let chunks = split_markdown_message(&input, 4096);
        assert!(chunks.len() >= 2);
        assert!(chunks.iter().all(|chunk| {
            translate_markdown_to_telegram_html(chunk)
                .text
                .chars()
                .count()
                <= 4096
        }));
    }

    #[test]
    fn splits_caption_safely_under_caption_limit() {
        let input = format!("**{}**", "x".repeat(1400));
        let chunks = split_markdown_message(&input, 1024);
        assert!(chunks.len() >= 2);
        assert!(chunks.iter().all(|chunk| {
            translate_markdown_to_telegram_html(chunk)
                .text
                .chars()
                .count()
                <= 1024
        }));
    }

    #[test]
    fn redacts_bot_token_from_errors() {
        let channel = TelegramChannel {
            id: "telegram-main".to_string(),
            bot_token: "secret-token".to_string(),
            api_base_url: "https://api.telegram.org".to_string(),
            poll_timeout_seconds: 30,
            poll_interval_ms: 250,
            commands: Vec::new(),
            client: Client::new(),
            bot_username: Mutex::new(None),
            bot_user_id: Mutex::new(None),
            chat_member_counts: Mutex::new(HashMap::new()),
            pending_outbound: Mutex::new(VecDeque::new()),
        };

        let redacted = channel
            .redact_sensitive_text("https://api.telegram.org/botsecret-token/getUpdates failed");
        assert!(!redacted.contains("secret-token"));
        assert!(redacted.contains("[REDACTED_TELEGRAM_BOT_TOKEN]"));
    }

    #[test]
    fn defers_retryable_send_failures() {
        let channel = TelegramChannel {
            id: "telegram-main".to_string(),
            bot_token: "secret-token".to_string(),
            api_base_url: "https://api.telegram.org".to_string(),
            poll_timeout_seconds: 30,
            poll_interval_ms: 250,
            commands: Vec::new(),
            client: Client::new(),
            bot_username: Mutex::new(None),
            bot_user_id: Mutex::new(None),
            chat_member_counts: Mutex::new(HashMap::new()),
            pending_outbound: Mutex::new(VecDeque::new()),
        };

        let error = anyhow!("telegram API sendMessage failed: Too Many Requests");
        assert!(channel.should_defer_outbound_message(&error));
    }

    #[test]
    fn does_not_defer_permanent_send_failures() {
        let channel = TelegramChannel {
            id: "telegram-main".to_string(),
            bot_token: "secret-token".to_string(),
            api_base_url: "https://api.telegram.org".to_string(),
            poll_timeout_seconds: 30,
            poll_interval_ms: 250,
            commands: Vec::new(),
            client: Client::new(),
            bot_username: Mutex::new(None),
            bot_user_id: Mutex::new(None),
            chat_member_counts: Mutex::new(HashMap::new()),
            pending_outbound: Mutex::new(VecDeque::new()),
        };

        let error = anyhow!("telegram API sendMessage failed: Bad Request: chat not found");
        assert!(!channel.should_defer_outbound_message(&error));
    }

    #[tokio::test]
    async fn accepts_private_messages_without_mention() {
        let channel = TelegramChannel {
            id: "telegram-main".to_string(),
            bot_token: "secret-token".to_string(),
            api_base_url: "https://api.telegram.org".to_string(),
            poll_timeout_seconds: 30,
            poll_interval_ms: 250,
            commands: Vec::new(),
            client: Client::new(),
            bot_username: Mutex::new(Some("party_claw_bot".to_string())),
            bot_user_id: Mutex::new(Some(42)),
            chat_member_counts: Mutex::new(HashMap::new()),
            pending_outbound: Mutex::new(VecDeque::new()),
        };
        let message = TelegramMessage {
            message_id: 1,
            chat: TelegramChat {
                id: 1,
                kind: "private".to_string(),
            },
            from: None,
            left_chat_member: None,
            text: Some("介绍一下你自己".to_string()),
            caption: None,
            photo: None,
            document: None,
            video: None,
            audio: None,
        };
        assert!(
            channel
                .should_accept_message(&message, message.text.as_deref(), 0)
                .await
        );
    }

    #[tokio::test]
    async fn ignores_group_messages_without_mention_even_if_command() {
        let channel = TelegramChannel {
            id: "telegram-main".to_string(),
            bot_token: "secret-token".to_string(),
            api_base_url: "https://api.telegram.org".to_string(),
            poll_timeout_seconds: 30,
            poll_interval_ms: 250,
            commands: Vec::new(),
            client: Client::new(),
            bot_username: Mutex::new(Some("party_claw_bot".to_string())),
            bot_user_id: Mutex::new(Some(42)),
            chat_member_counts: Mutex::new(HashMap::new()),
            pending_outbound: Mutex::new(VecDeque::new()),
        };
        let message = TelegramMessage {
            message_id: 1,
            chat: TelegramChat {
                id: -1,
                kind: "group".to_string(),
            },
            from: None,
            left_chat_member: None,
            text: Some("/status".to_string()),
            caption: None,
            photo: None,
            document: None,
            video: None,
            audio: None,
        };
        assert!(
            !channel
                .should_accept_message(&message, message.text.as_deref(), 0)
                .await
        );
    }

    #[tokio::test]
    async fn accepts_group_messages_with_mention() {
        let channel = TelegramChannel {
            id: "telegram-main".to_string(),
            bot_token: "secret-token".to_string(),
            api_base_url: "https://api.telegram.org".to_string(),
            poll_timeout_seconds: 30,
            poll_interval_ms: 250,
            commands: Vec::new(),
            client: Client::new(),
            bot_username: Mutex::new(Some("party_claw_bot".to_string())),
            bot_user_id: Mutex::new(Some(42)),
            chat_member_counts: Mutex::new(HashMap::new()),
            pending_outbound: Mutex::new(VecDeque::new()),
        };
        let message = TelegramMessage {
            message_id: 1,
            chat: TelegramChat {
                id: -1,
                kind: "supergroup".to_string(),
            },
            from: None,
            left_chat_member: None,
            text: Some("@party_claw_bot 你好".to_string()),
            caption: None,
            photo: None,
            document: None,
            video: None,
            audio: None,
        };
        assert!(
            channel
                .should_accept_message(&message, message.text.as_deref(), 0)
                .await
        );
    }

    #[tokio::test]
    async fn accepts_two_person_group_messages_without_mention() {
        let channel = TelegramChannel {
            id: "telegram-main".to_string(),
            bot_token: "secret-token".to_string(),
            api_base_url: "https://api.telegram.org".to_string(),
            poll_timeout_seconds: 30,
            poll_interval_ms: 250,
            commands: Vec::new(),
            client: Client::new(),
            bot_username: Mutex::new(Some("party_claw_bot".to_string())),
            bot_user_id: Mutex::new(Some(42)),
            chat_member_counts: Mutex::new(HashMap::from([(-5158767783, (2, Instant::now()))])),
            pending_outbound: Mutex::new(VecDeque::new()),
        };
        let message = TelegramMessage {
            message_id: 1,
            chat: TelegramChat {
                id: -5158767783,
                kind: "group".to_string(),
            },
            from: None,
            left_chat_member: None,
            text: Some("在吗".to_string()),
            caption: None,
            photo: None,
            document: None,
            video: None,
            audio: None,
        };
        assert!(
            channel
                .should_accept_message(&message, message.text.as_deref(), 0)
                .await
        );
    }
}

struct TelegramAttachmentSource {
    client: Client,
    api_base_url: String,
    bot_token: String,
    file_id: String,
}

impl TelegramAttachmentSource {
    fn new(client: Client, api_base_url: String, bot_token: String, file_id: String) -> Self {
        Self {
            client,
            api_base_url,
            bot_token,
            file_id,
        }
    }
}

#[async_trait]
impl AttachmentSource for TelegramAttachmentSource {
    async fn save_to(&self, destination: &Path) -> Result<u64> {
        let response = self
            .client
            .post(format!(
                "{}/bot{}/getFile",
                self.api_base_url, self.bot_token
            ))
            .json(&json!({ "file_id": self.file_id }))
            .send()
            .await
            .context("telegram getFile request failed")?;
        let envelope: TelegramEnvelope<TelegramFile> = response
            .json()
            .await
            .context("telegram getFile response was not valid JSON")?;
        if !envelope.ok {
            return Err(anyhow!(
                "telegram getFile failed: {}",
                envelope
                    .description
                    .unwrap_or_else(|| "unknown error".to_string())
            ));
        }
        let file = envelope
            .result
            .ok_or_else(|| anyhow!("telegram getFile returned no file metadata"))?;
        let url = format!(
            "{}/file/bot{}/{}",
            self.api_base_url, self.bot_token, file.file_path
        );
        let bytes = self
            .client
            .get(url)
            .send()
            .await
            .context("telegram file download failed")?
            .bytes()
            .await
            .context("telegram file payload read failed")?;
        if let Some(parent) = destination.parent() {
            fs::create_dir_all(parent).await?;
        }
        fs::write(destination, &bytes).await?;
        Ok(bytes.len() as u64)
    }
}

#[derive(Deserialize)]
struct TelegramEnvelope<T> {
    ok: bool,
    result: Option<T>,
    description: Option<String>,
}

#[derive(Deserialize)]
struct TelegramUpdate {
    update_id: i64,
    message: Option<TelegramMessage>,
}

#[derive(Deserialize)]
struct TelegramMessage {
    message_id: i64,
    chat: TelegramChat,
    from: Option<TelegramUser>,
    left_chat_member: Option<TelegramUser>,
    text: Option<String>,
    caption: Option<String>,
    photo: Option<Vec<TelegramPhotoSize>>,
    document: Option<TelegramMedia>,
    video: Option<TelegramMedia>,
    audio: Option<TelegramMedia>,
}

#[derive(Deserialize)]
struct TelegramChat {
    id: i64,
    #[serde(rename = "type")]
    kind: String,
}

#[derive(Deserialize)]
struct TelegramUser {
    id: i64,
    first_name: String,
    last_name: Option<String>,
    username: Option<String>,
}

#[derive(Deserialize)]
struct TelegramPhotoSize {
    file_id: String,
    file_unique_id: String,
    file_size: Option<u64>,
}

#[derive(Deserialize)]
struct TelegramMedia {
    file_id: String,
    file_unique_id: String,
    file_name: Option<String>,
    mime_type: Option<String>,
    file_size: Option<u64>,
}

#[derive(Deserialize)]
struct TelegramFile {
    file_path: String,
}
