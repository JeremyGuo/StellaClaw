use crate::channel::{
    AttachmentSource, Channel, ConversationProbe, IncomingControl, IncomingMessage,
    PendingAttachment, ProgressFeedback, ProgressFeedbackFinalState, ProgressFeedbackUpdate,
};
use crate::config::{BotCommandConfig, TelegramChannelConfig, default_telegram_commands};
use crate::domain::{
    AttachmentKind, ChannelAddress, OutgoingAttachment, OutgoingMessage, ProcessingState,
    ShowOptions,
};
use anyhow::{Context, Result, anyhow};
use async_trait::async_trait;
use pulldown_cmark::{CodeBlockKind, Event, Options, Parser, Tag, TagEnd};
use reqwest::Client;
use reqwest::multipart::{Form, Part};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::collections::{HashMap, VecDeque};
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
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
    progress_messages: Mutex<HashMap<String, TelegramProgressMessage>>,
}

#[derive(Clone, Debug)]
struct PendingOutbound {
    address: ChannelAddress,
    message: OutgoingMessage,
}

#[derive(Clone, Debug)]
struct TelegramProgressMessage {
    message_id: i64,
    last_text: String,
    last_update: Instant,
}

impl TelegramChannel {
    const MAX_POLL_BACKOFF_SECONDS: u64 = 30;
    const MAX_SEND_RETRIES: u32 = 3;
    const MAX_MESSAGE_CHARS: usize = 4096;
    const MAX_CAPTION_CHARS: usize = 1024;
    const MIN_PROGRESS_EDIT_INTERVAL: Duration = Duration::from_secs(3);
    const GET_UPDATES_TIMEOUT_GRACE_SECONDS: u64 = 15;
    const DEFAULT_JSON_API_TIMEOUT_SECONDS: u64 = 60;
    const SEND_CHAT_ACTION_TIMEOUT_SECONDS: u64 = 10;
    const MULTIPART_API_TIMEOUT_SECONDS: u64 = 600;

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
            commands: default_telegram_commands(),
            client: Client::new(),
            bot_username: Mutex::new(None),
            bot_user_id: Mutex::new(None),
            chat_member_counts: Mutex::new(HashMap::new()),
            pending_outbound: Mutex::new(VecDeque::new()),
            progress_messages: Mutex::new(HashMap::new()),
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
            let call = async {
                let response = self
                    .client
                    .post(self.method_url(method))
                    .json(&payload)
                    .send()
                    .await
                    .map_err(|error| {
                        anyhow!(
                            "{}",
                            self.redact_sensitive_text(&format!(
                                "telegram API call {} failed: {error:#}",
                                method
                            ))
                        )
                    })?;
                response
                    .json::<TelegramEnvelope<T>>()
                    .await
                    .map_err(|error| {
                        anyhow!(
                            "{}",
                            self.redact_sensitive_text(&format!(
                                "telegram API {} returned invalid JSON: {error:#}",
                                method
                            ))
                        )
                    })
            };
            let envelope = match tokio::time::timeout(self.api_call_timeout(method), call).await {
                Ok(Ok(envelope)) => envelope,
                Ok(Err(error)) => {
                    if attempt < max_attempts && self.should_retry_transport_error(method) {
                        self.log_send_retry(method, attempt, max_attempts, &error);
                        tokio::time::sleep(Duration::from_secs(u64::from(attempt))).await;
                        continue;
                    }
                    return Err(error);
                }
                Err(_) => {
                    let timeout_seconds = self.api_call_timeout(method).as_secs();
                    let error = anyhow!(
                        "{}",
                        self.redact_sensitive_text(&format!(
                            "telegram API call {} timed out after {}s",
                            method, timeout_seconds
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

    fn api_call_timeout(&self, method: &str) -> Duration {
        match method {
            "getUpdates" => Duration::from_secs(
                self.poll_timeout_seconds
                    .saturating_add(Self::GET_UPDATES_TIMEOUT_GRACE_SECONDS)
                    .max(1),
            ),
            "sendChatAction" => Duration::from_secs(Self::SEND_CHAT_ACTION_TIMEOUT_SECONDS),
            _ => Duration::from_secs(Self::DEFAULT_JSON_API_TIMEOUT_SECONDS),
        }
    }

    async fn call_multipart(&self, method: &str, form: Form) -> Result<serde_json::Value> {
        let call = async {
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
            response
                .json::<TelegramEnvelope<serde_json::Value>>()
                .await
                .map_err(|error| {
                    anyhow!(
                        "{}",
                        self.redact_sensitive_text(&format!(
                            "telegram API {} returned invalid JSON: {error:#}",
                            method
                        ))
                    )
                })
        };
        let envelope = match tokio::time::timeout(
            Duration::from_secs(Self::MULTIPART_API_TIMEOUT_SECONDS),
            call,
        )
        .await
        {
            Ok(result) => result?,
            Err(_) => {
                return Err(anyhow!(
                    "{}",
                    self.redact_sensitive_text(&format!(
                        "telegram multipart API call {} timed out after {}s",
                        method,
                        Self::MULTIPART_API_TIMEOUT_SECONDS
                    ))
                ));
            }
        };
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

    fn progress_message_key(address: &ChannelAddress, turn_id: &str) -> String {
        format!("{}:{}", address.session_key(), turn_id)
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

    async fn answer_callback_query(&self, callback_query_id: &str) -> Result<()> {
        self.call_api::<bool>(
            "answerCallbackQuery",
            json!({
                "callback_query_id": callback_query_id,
            }),
        )
        .await?;
        Ok(())
    }

    fn build_address_from_chat_and_user(
        &self,
        chat: &TelegramChat,
        user: Option<&TelegramUser>,
    ) -> ChannelAddress {
        let display_name = user.map(|user| {
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
            conversation_id: chat.id.to_string(),
            user_id: user.map(|user| user.id.to_string()),
            display_name,
        }
    }

    fn build_address(&self, message: &TelegramMessage) -> ChannelAddress {
        self.build_address_from_chat_and_user(&message.chat, message.from.as_ref())
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

    async fn probe_group_member_count(&self, chat_id: i64) -> Result<u64> {
        let count = self
            .call_api::<u64>(
                "getChatMemberCount",
                json!({
                    "chat_id": chat_id,
                }),
            )
            .await?;
        self.chat_member_counts
            .lock()
            .await
            .insert(chat_id, (count, Instant::now()));
        Ok(count)
    }

    async fn bot_was_removed_from_chat(&self, message: &TelegramMessage) -> bool {
        let Some(left_chat_member) = message.left_chat_member.as_ref() else {
            return false;
        };
        let bot_user_id = *self.bot_user_id.lock().await;
        bot_user_id.is_some_and(|bot_id| bot_id == left_chat_member.id)
    }

    fn is_terminal_chat_error(&self, error: &anyhow::Error) -> bool {
        let message = format!("{error:#}").to_ascii_lowercase();
        message.contains("chat not found")
            || message.contains("group chat was deleted")
            || message.contains("group chat was upgraded to a supergroup chat")
            || message.contains("bot was kicked from the group chat")
            || message.contains("bot is not a member of the channel chat")
            || message.contains("forbidden: bot was kicked")
    }

    async fn conversation_is_unavailable(&self, message: &TelegramMessage) -> bool {
        if !matches!(message.chat.kind.as_str(), "group" | "supergroup") {
            return false;
        }
        match self
            .call_api::<serde_json::Value>(
                "getChat",
                json!({
                    "chat_id": message.chat.id,
                }),
            )
            .await
        {
            Ok(_) => false,
            Err(error) => self.is_terminal_chat_error(&error),
        }
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
            let mut iter =
                render_markdown_chunks_to_telegram_entities(&caption, Self::MAX_CAPTION_CHARS)
                    .into_iter();
            let caption = iter.next();
            trailing_text_chunks = iter.collect();
            let rendered = caption.unwrap_or_else(|| TelegramRenderedText {
                text: String::new(),
                entities: Vec::new(),
            });
            let mut form = form.text("caption", rendered.text);
            if !rendered.entities.is_empty() {
                form = form.text(
                    "caption_entities",
                    serde_json::to_string(&rendered.entities)
                        .context("failed to serialize telegram caption entities")?,
                );
            }
            form
        } else {
            form
        };
        self.call_multipart("sendPhoto", form).await?;
        for chunk in trailing_text_chunks {
            self.send_rendered_text_chunk(chat_id, chunk, None).await?;
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
            let mut iter =
                render_markdown_chunks_to_telegram_entities(&caption, Self::MAX_CAPTION_CHARS)
                    .into_iter();
            let caption = iter.next();
            trailing_text_chunks = iter.collect();
            let rendered = caption.unwrap_or_else(|| TelegramRenderedText {
                text: String::new(),
                entities: Vec::new(),
            });
            let mut form = form.text("caption", rendered.text);
            if !rendered.entities.is_empty() {
                form = form.text(
                    "caption_entities",
                    serde_json::to_string(&rendered.entities)
                        .context("failed to serialize telegram caption entities")?,
                );
            }
            form
        } else {
            form
        };
        self.call_multipart("sendDocument", form).await?;
        for chunk in trailing_text_chunks {
            self.send_rendered_text_chunk(chat_id, chunk, None).await?;
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
        let mut trailing_text_chunks = Vec::new();
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
                let mut chunks =
                    render_markdown_chunks_to_telegram_entities(&caption, Self::MAX_CAPTION_CHARS)
                        .into_iter();
                if let Some(rendered_caption) = chunks.next() {
                    object.insert("caption".to_string(), json!(rendered_caption.text));
                    if !rendered_caption.entities.is_empty() {
                        object.insert(
                            "caption_entities".to_string(),
                            serde_json::to_value(rendered_caption.entities)
                                .context("failed to encode telegram caption entities")?,
                        );
                    }
                }
                if index == 0 {
                    trailing_text_chunks.extend(chunks);
                }
            }
            media.push(item);
            form = form.part(field_name, Part::bytes(bytes).file_name(file_name));
        }
        form = form.text(
            "media",
            serde_json::to_string(&media).context("failed to serialize telegram media group")?,
        );
        self.call_multipart("sendMediaGroup", form).await?;
        for chunk in trailing_text_chunks {
            self.send_rendered_text_chunk(chat_id, chunk, None).await?;
        }
        Ok(())
    }

    async fn send_media_group_with_caption(
        &self,
        address: &ChannelAddress,
        images: Vec<OutgoingAttachment>,
        caption: Option<String>,
    ) -> Result<()> {
        let total_chunks = images.len().div_ceil(5);
        for (index, chunk) in images.chunks(5).enumerate() {
            let shared_caption = if index + 1 == total_chunks {
                caption.clone()
            } else {
                None
            };
            self.send_photo_group(&address.conversation_id, chunk.to_vec(), shared_caption)
                .await?;
        }
        Ok(())
    }

    async fn send_text_chunks(
        &self,
        chat_id: &str,
        text: &str,
        options: Option<&ShowOptions>,
    ) -> Result<()> {
        let chunks = render_markdown_chunks_to_telegram_entities(text, Self::MAX_MESSAGE_CHARS);
        let mut message_ids = Vec::new();
        self.upsert_rendered_text_chain(chat_id, chunks, &mut message_ids, options)
            .await?;
        Ok(())
    }

    async fn send_rendered_text_chunk(
        &self,
        chat_id: &str,
        rendered: TelegramRenderedText,
        options: Option<&ShowOptions>,
    ) -> Result<i64> {
        let payload = build_send_text_payload(chat_id, rendered, options)?;
        let message = self
            .call_api::<TelegramMessage>("sendMessage", payload)
            .await?;
        Ok(message.message_id)
    }

    async fn edit_rendered_text_chunk(
        &self,
        chat_id: &str,
        message_id: i64,
        rendered: TelegramRenderedText,
        options: Option<&ShowOptions>,
    ) -> Result<()> {
        let payload = build_edit_text_payload(chat_id, message_id, rendered, options)?;
        self.call_api::<serde_json::Value>("editMessageText", payload)
            .await?;
        Ok(())
    }

    async fn send_progress_text(&self, chat_id: &str, text: &str) -> Result<i64> {
        let rendered = render_markdown_chunks_to_telegram_entities(text, Self::MAX_MESSAGE_CHARS)
            .into_iter()
            .next()
            .unwrap_or_else(|| TelegramRenderedText {
                text: text.to_string(),
                entities: Vec::new(),
            });
        self.send_rendered_text_chunk(chat_id, rendered, None).await
    }

    async fn edit_progress_text(&self, chat_id: &str, message_id: i64, text: &str) -> Result<()> {
        let rendered = render_markdown_chunks_to_telegram_entities(text, Self::MAX_MESSAGE_CHARS)
            .into_iter()
            .next()
            .unwrap_or_else(|| TelegramRenderedText {
                text: text.to_string(),
                entities: Vec::new(),
            });
        self.edit_rendered_text_chunk(chat_id, message_id, rendered, None)
            .await
    }

    async fn delete_message(&self, chat_id: &str, message_id: i64) -> Result<()> {
        self.call_api::<serde_json::Value>(
            "deleteMessage",
            json!({
                "chat_id": chat_id,
                "message_id": message_id,
            }),
        )
        .await?;
        Ok(())
    }

    async fn upsert_rendered_text_chain(
        &self,
        chat_id: &str,
        chunks: Vec<TelegramRenderedText>,
        message_ids: &mut Vec<i64>,
        options: Option<&ShowOptions>,
    ) -> Result<()> {
        for (index, rendered) in chunks.iter().take(message_ids.len()).cloned().enumerate() {
            self.edit_rendered_text_chunk(
                chat_id,
                message_ids[index],
                rendered,
                (index == 0).then_some(options).flatten(),
            )
            .await?;
        }

        for (index, rendered) in chunks.iter().skip(message_ids.len()).cloned().enumerate() {
            let absolute_index = message_ids.len() + index;
            let message_id = self
                .send_rendered_text_chunk(
                    chat_id,
                    rendered,
                    (absolute_index == 0).then_some(options).flatten(),
                )
                .await?;
            message_ids.push(message_id);
        }

        while message_ids.len() > chunks.len() {
            if let Some(message_id) = message_ids.pop() {
                self.delete_message(chat_id, message_id).await?;
            }
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
            options,
        } = message;
        if images.len() >= 2 {
            self.send_media_group_with_caption(address, images, text)
                .await?;
        } else {
            let mut images = images;
            let has_images = !images.is_empty();
            if let Some(text) = text.as_deref()
                && has_images
            {
                if let Some(image) = images.first_mut()
                    && image.caption.is_none()
                {
                    image.caption = Some(text.to_string());
                }
            }
            self.send_photo_group(&address.conversation_id, images, None)
                .await?;
            if let Some(text) = text.as_deref()
                && !has_images
            {
                self.send_text_chunks(&address.conversation_id, text, options.as_ref())
                    .await?;
            }
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
                "allowed_updates": ["message", "callback_query"],
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
                if let Some(callback_query) = update.callback_query {
                    if let Err(error) = self.answer_callback_query(&callback_query.id).await {
                        warn!(
                            log_stream = "channel",
                            log_key = %self.id,
                            kind = "telegram_callback_query_ack_failed",
                            callback_query_id = callback_query.id,
                            error = %format!("{error:#}"),
                            "failed to acknowledge telegram callback query"
                        );
                    }
                    let Some(message) = callback_query.message else {
                        continue;
                    };
                    let Some(text) = callback_query
                        .data
                        .map(|value| value.trim().to_string())
                        .filter(|value| !value.is_empty())
                    else {
                        continue;
                    };
                    let incoming = IncomingMessage {
                        remote_message_id: format!("callback:{}", callback_query.id),
                        address: self.build_address_from_chat_and_user(
                            &message.chat,
                            Some(&callback_query.from),
                        ),
                        text: Some(text),
                        attachments: Vec::new(),
                        stored_attachments: Vec::new(),
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
                    continue;
                }
                let Some(message) = update.message else {
                    continue;
                };
                if self.bot_was_removed_from_chat(&message).await {
                    let incoming = IncomingMessage {
                        remote_message_id: message.message_id.to_string(),
                        address: self.build_address(&message),
                        text: None,
                        attachments: Vec::new(),
                        stored_attachments: Vec::new(),
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
                    if self.conversation_is_unavailable(&message).await {
                        let incoming = IncomingMessage {
                            remote_message_id: message.message_id.to_string(),
                            address: self.build_address(&message),
                            text: None,
                            attachments: Vec::new(),
                            stored_attachments: Vec::new(),
                            control: Some(IncomingControl::ConversationClosed {
                                reason: "telegram chat is no longer available".to_string(),
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
                let receive_lag_seconds = telegram_message_receive_lag_seconds(message.date);
                info!(
                    log_stream = "channel",
                    log_key = %self.id,
                    kind = "telegram_update_accepted",
                    conversation_id = message.chat.id.to_string(),
                    remote_message_id = message.message_id.to_string(),
                    telegram_message_date = ?message.date,
                    telegram_receive_lag_seconds = ?receive_lag_seconds,
                    text_preview = text.as_deref().map(summarize_for_log),
                    attachment_count = attachments.len() as u64,
                    "accepted telegram update"
                );
                let incoming = IncomingMessage {
                    remote_message_id: message.message_id.to_string(),
                    address: self.build_address(&message),
                    text,
                    attachments,
                    stored_attachments: Vec::new(),
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
            options,
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
            has_options = options.is_some(),
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
                    options,
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

    async fn update_progress_feedback(
        &self,
        address: &ChannelAddress,
        feedback: ProgressFeedback,
    ) -> Result<ProgressFeedbackUpdate> {
        let key = Self::progress_message_key(address, &feedback.turn_id);
        let persisted_message_id = feedback
            .message_id
            .as_deref()
            .and_then(|value| value.parse::<i64>().ok());
        if feedback.final_state == Some(ProgressFeedbackFinalState::Done) {
            let existing = self.progress_messages.lock().await.remove(&key);
            let message_id = existing
                .map(|existing| existing.message_id)
                .or(persisted_message_id);
            if let Some(message_id) = message_id
                && let Err(error) = self
                    .delete_message(&address.conversation_id, message_id)
                    .await
            {
                warn!(
                    log_stream = "channel",
                    log_key = %self.id,
                    kind = "telegram_progress_delete_failed",
                    conversation_id = %address.conversation_id,
                    error = %format!("{error:#}"),
                    "failed to delete telegram progress message"
                );
            }
            return Ok(ProgressFeedbackUpdate::ClearMessage);
        }

        let now = Instant::now();
        let existing = self
            .progress_messages
            .lock()
            .await
            .get(&key)
            .cloned()
            .or_else(|| {
                persisted_message_id.map(|message_id| TelegramProgressMessage {
                    message_id,
                    last_text: String::new(),
                    last_update: now
                        .checked_sub(Self::MIN_PROGRESS_EDIT_INTERVAL)
                        .unwrap_or(now),
                })
            });
        let Some(existing) = existing else {
            let message_id = self
                .send_progress_text(&address.conversation_id, &feedback.text)
                .await?;
            self.progress_messages.lock().await.insert(
                key,
                TelegramProgressMessage {
                    message_id,
                    last_text: feedback.text,
                    last_update: now,
                },
            );
            return Ok(ProgressFeedbackUpdate::StoreMessage {
                message_id: message_id.to_string(),
            });
        };

        if existing.last_text == feedback.text {
            return Ok(ProgressFeedbackUpdate::Unchanged);
        }
        let is_final = feedback.final_state.is_some();
        if !feedback.important
            && !is_final
            && now.duration_since(existing.last_update) < Self::MIN_PROGRESS_EDIT_INTERVAL
        {
            return Ok(ProgressFeedbackUpdate::Unchanged);
        }

        self.edit_progress_text(
            &address.conversation_id,
            existing.message_id,
            &feedback.text,
        )
        .await?;
        if is_final {
            self.progress_messages.lock().await.remove(&key);
            return Ok(ProgressFeedbackUpdate::ClearMessage);
        } else {
            self.progress_messages.lock().await.insert(
                key,
                TelegramProgressMessage {
                    message_id: existing.message_id,
                    last_text: feedback.text,
                    last_update: now,
                },
            );
        }
        Ok(ProgressFeedbackUpdate::Unchanged)
    }

    async fn probe_conversation(
        &self,
        address: &ChannelAddress,
    ) -> Result<Option<ConversationProbe>> {
        let Ok(chat_id) = address.conversation_id.parse::<i64>() else {
            return Ok(None);
        };
        if chat_id >= 0 {
            return Ok(None);
        }
        match self.probe_group_member_count(chat_id).await {
            Ok(count) => Ok(Some(ConversationProbe::Available {
                member_count: Some(count),
            })),
            Err(error) if self.is_terminal_chat_error(&error) => {
                Ok(Some(ConversationProbe::Unavailable {
                    reason: format!("{error:#}"),
                }))
            }
            Err(error) => Err(error.context("failed to probe telegram conversation")),
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

fn telegram_message_receive_lag_seconds(message_date: Option<i64>) -> Option<i64> {
    let sent_at = message_date?;
    let now: i64 = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()?
        .as_secs()
        .try_into()
        .ok()?;
    Some(now - sent_at)
}

fn build_inline_keyboard_markup(options: &ShowOptions) -> serde_json::Value {
    let mut rows = Vec::new();
    for chunk in options.options.chunks(2) {
        let row = chunk
            .iter()
            .map(|option| {
                json!({
                    "text": option.label,
                    "callback_data": option.value,
                })
            })
            .collect::<Vec<_>>();
        rows.push(row);
    }
    json!({
        "inline_keyboard": rows,
    })
}

fn build_send_text_payload(
    chat_id: &str,
    rendered: TelegramRenderedText,
    options: Option<&ShowOptions>,
) -> Result<serde_json::Value> {
    let mut payload = json!({
        "chat_id": chat_id,
        "text": rendered.text,
    });
    if !rendered.entities.is_empty()
        && let Some(object) = payload.as_object_mut()
    {
        object.insert(
            "entities".to_string(),
            serde_json::to_value(rendered.entities)
                .context("failed to encode telegram entities")?,
        );
    }
    if let Some(options) = options
        && let Some(object) = payload.as_object_mut()
    {
        object.insert(
            "reply_markup".to_string(),
            build_inline_keyboard_markup(options),
        );
    }
    Ok(payload)
}

fn build_edit_text_payload(
    chat_id: &str,
    message_id: i64,
    rendered: TelegramRenderedText,
    options: Option<&ShowOptions>,
) -> Result<serde_json::Value> {
    let mut payload = json!({
        "chat_id": chat_id,
        "message_id": message_id,
        "text": rendered.text,
    });
    if !rendered.entities.is_empty()
        && let Some(object) = payload.as_object_mut()
    {
        object.insert(
            "entities".to_string(),
            serde_json::to_value(rendered.entities)
                .context("failed to encode telegram entities")?,
        );
    }
    if let Some(options) = options
        && let Some(object) = payload.as_object_mut()
    {
        object.insert(
            "reply_markup".to_string(),
            build_inline_keyboard_markup(options),
        );
    }
    Ok(payload)
}

fn poll_backoff_seconds(consecutive_failures: u32, cap_seconds: u64) -> u64 {
    let exponent = consecutive_failures.saturating_sub(1).min(5);
    2_u64.saturating_pow(exponent).min(cap_seconds).max(1)
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct TelegramFormattedText {
    text: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct TelegramRenderedText {
    text: String,
    entities: Vec<TelegramMessageEntity>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
struct TelegramMessageEntity {
    #[serde(rename = "type")]
    kind: String,
    offset: usize,
    length: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    language: Option<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct RichDocument {
    blocks: Vec<RichBlock>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum RichBlock {
    Paragraph(Vec<RichInline>),
    Heading(Vec<RichInline>),
    BlockQuote(Vec<RichBlock>),
    List {
        start: Option<u64>,
        items: Vec<Vec<RichBlock>>,
    },
    CodeBlock {
        language: Option<String>,
        code: String,
    },
    Table(RichTable),
    ThematicBreak,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct RichTable {
    rows: Vec<Vec<String>>,
    header_rows: usize,
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum RichInline {
    Text(String),
    Emphasis(Vec<RichInline>),
    Strong(Vec<RichInline>),
    Strikethrough(Vec<RichInline>),
    Link {
        url: String,
        content: Vec<RichInline>,
    },
    Code(String),
    LineBreak,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PendingBlockKind {
    Paragraph,
    Heading,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum InlineStyle {
    Emphasis,
    Strong,
    Strikethrough,
    Link,
}

#[derive(Clone, Debug)]
enum InlineWrapper {
    Emphasis,
    Strong,
    Strikethrough,
    Link(String),
}

#[derive(Clone, Debug)]
struct PendingInlineBlock {
    kind: PendingBlockKind,
    inlines: Vec<RichInline>,
}

#[derive(Clone, Debug)]
struct PendingInlineContainer {
    style: InlineStyle,
    url: Option<String>,
    inlines: Vec<RichInline>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct PendingTable {
    rows: Vec<Vec<String>>,
    current_row: Vec<String>,
    current_cell: String,
    header_rows: usize,
}

#[derive(Clone, Debug)]
enum BlockContainer {
    Root(Vec<RichBlock>),
    BlockQuote(Vec<RichBlock>),
    List {
        start: Option<u64>,
        items: Vec<Vec<RichBlock>>,
    },
    ListItem(Vec<RichBlock>),
}

fn parse_markdown_to_rich_document(input: &str) -> RichDocument {
    let parser = Parser::new_ext(input, Options::all());
    let mut block_stack = vec![BlockContainer::Root(Vec::new())];
    let mut pending_inline_block: Option<PendingInlineBlock> = None;
    let mut inline_stack: Vec<PendingInlineContainer> = Vec::new();
    let mut code_block_language: Option<String> = None;
    let mut code_block_buffer: Option<String> = None;
    let mut pending_table: Option<PendingTable> = None;

    for event in parser {
        match event {
            Event::Start(tag) => match tag {
                Tag::Table(_) => {
                    flush_inline_block(
                        &mut pending_inline_block,
                        &mut inline_stack,
                        &mut block_stack,
                    );
                    pending_table = Some(PendingTable::default());
                }
                Tag::TableHead => {}
                Tag::TableRow => {
                    if let Some(table) = pending_table.as_mut() {
                        table.current_row.clear();
                    }
                }
                Tag::TableCell => {
                    if let Some(table) = pending_table.as_mut() {
                        table.current_cell.clear();
                    }
                }
                Tag::Paragraph => {
                    flush_inline_block(
                        &mut pending_inline_block,
                        &mut inline_stack,
                        &mut block_stack,
                    );
                    pending_inline_block = Some(PendingInlineBlock {
                        kind: PendingBlockKind::Paragraph,
                        inlines: Vec::new(),
                    });
                }
                Tag::Heading { .. } => {
                    flush_inline_block(
                        &mut pending_inline_block,
                        &mut inline_stack,
                        &mut block_stack,
                    );
                    pending_inline_block = Some(PendingInlineBlock {
                        kind: PendingBlockKind::Heading,
                        inlines: Vec::new(),
                    });
                }
                Tag::BlockQuote(_) => {
                    flush_inline_block(
                        &mut pending_inline_block,
                        &mut inline_stack,
                        &mut block_stack,
                    );
                    block_stack.push(BlockContainer::BlockQuote(Vec::new()));
                }
                Tag::List(start) => {
                    flush_inline_block(
                        &mut pending_inline_block,
                        &mut inline_stack,
                        &mut block_stack,
                    );
                    block_stack.push(BlockContainer::List {
                        start,
                        items: Vec::new(),
                    });
                }
                Tag::Item => {
                    flush_inline_block(
                        &mut pending_inline_block,
                        &mut inline_stack,
                        &mut block_stack,
                    );
                    block_stack.push(BlockContainer::ListItem(Vec::new()));
                }
                Tag::Emphasis => {
                    ensure_inline_block(&mut pending_inline_block);
                    inline_stack.push(PendingInlineContainer {
                        style: InlineStyle::Emphasis,
                        url: None,
                        inlines: Vec::new(),
                    });
                }
                Tag::Strong => {
                    ensure_inline_block(&mut pending_inline_block);
                    inline_stack.push(PendingInlineContainer {
                        style: InlineStyle::Strong,
                        url: None,
                        inlines: Vec::new(),
                    });
                }
                Tag::Strikethrough => {
                    ensure_inline_block(&mut pending_inline_block);
                    inline_stack.push(PendingInlineContainer {
                        style: InlineStyle::Strikethrough,
                        url: None,
                        inlines: Vec::new(),
                    });
                }
                Tag::Link { dest_url, .. } => {
                    ensure_inline_block(&mut pending_inline_block);
                    inline_stack.push(PendingInlineContainer {
                        style: InlineStyle::Link,
                        url: Some(dest_url.to_string()),
                        inlines: Vec::new(),
                    });
                }
                Tag::CodeBlock(kind) => {
                    flush_inline_block(
                        &mut pending_inline_block,
                        &mut inline_stack,
                        &mut block_stack,
                    );
                    code_block_language = match kind {
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
                    code_block_buffer = Some(String::new());
                }
                _ => {}
            },
            Event::End(tag) => match tag {
                TagEnd::Table => {
                    if let Some(table) = pending_table.take() {
                        push_block(
                            &mut block_stack,
                            RichBlock::Table(RichTable {
                                rows: table.rows,
                                header_rows: table.header_rows,
                            }),
                        );
                    }
                }
                TagEnd::TableHead => {
                    if let Some(table) = pending_table.as_mut() {
                        if !table.current_row.is_empty() {
                            table.rows.push(std::mem::take(&mut table.current_row));
                        }
                        table.header_rows = table.rows.len();
                    }
                }
                TagEnd::TableRow => {
                    if let Some(table) = pending_table.as_mut()
                        && !table.current_row.is_empty()
                    {
                        table.rows.push(std::mem::take(&mut table.current_row));
                    }
                }
                TagEnd::TableCell => {
                    if let Some(table) = pending_table.as_mut() {
                        table
                            .current_row
                            .push(normalize_table_cell(&table.current_cell));
                        table.current_cell.clear();
                    }
                }
                TagEnd::Paragraph | TagEnd::Heading(_) => {
                    flush_inline_block(
                        &mut pending_inline_block,
                        &mut inline_stack,
                        &mut block_stack,
                    );
                }
                TagEnd::BlockQuote(_) => {
                    flush_inline_block(
                        &mut pending_inline_block,
                        &mut inline_stack,
                        &mut block_stack,
                    );
                    if let Some(BlockContainer::BlockQuote(blocks)) = block_stack.pop() {
                        push_block(&mut block_stack, RichBlock::BlockQuote(blocks));
                    }
                }
                TagEnd::List(_) => {
                    flush_inline_block(
                        &mut pending_inline_block,
                        &mut inline_stack,
                        &mut block_stack,
                    );
                    if let Some(BlockContainer::List { start, items }) = block_stack.pop() {
                        push_block(&mut block_stack, RichBlock::List { start, items });
                    }
                }
                TagEnd::Item => {
                    flush_inline_block(
                        &mut pending_inline_block,
                        &mut inline_stack,
                        &mut block_stack,
                    );
                    if let Some(BlockContainer::ListItem(blocks)) = block_stack.pop()
                        && let Some(BlockContainer::List { items, .. }) = block_stack.last_mut()
                    {
                        items.push(blocks);
                    }
                }
                TagEnd::Emphasis | TagEnd::Strong | TagEnd::Strikethrough | TagEnd::Link => {
                    if let Some(container) = inline_stack.pop() {
                        let inline = match container.style {
                            InlineStyle::Emphasis => RichInline::Emphasis(container.inlines),
                            InlineStyle::Strong => RichInline::Strong(container.inlines),
                            InlineStyle::Strikethrough => {
                                RichInline::Strikethrough(container.inlines)
                            }
                            InlineStyle::Link => RichInline::Link {
                                url: container.url.unwrap_or_default(),
                                content: container.inlines,
                            },
                        };
                        push_inline(&mut pending_inline_block, &mut inline_stack, inline);
                    }
                }
                TagEnd::CodeBlock => {
                    let code = code_block_buffer.take().unwrap_or_default();
                    let language = code_block_language.take();
                    push_block(&mut block_stack, RichBlock::CodeBlock { language, code });
                }
                _ => {}
            },
            Event::Text(text) => {
                if let Some(table) = pending_table.as_mut() {
                    table.current_cell.push_str(&text);
                } else if let Some(buffer) = code_block_buffer.as_mut() {
                    buffer.push_str(&text);
                } else {
                    push_inline(
                        &mut pending_inline_block,
                        &mut inline_stack,
                        RichInline::Text(text.to_string()),
                    );
                }
            }
            Event::Code(code) => {
                if let Some(table) = pending_table.as_mut() {
                    table.current_cell.push_str(&code);
                } else {
                    push_inline(
                        &mut pending_inline_block,
                        &mut inline_stack,
                        RichInline::Code(code.to_string()),
                    );
                }
            }
            Event::SoftBreak | Event::HardBreak => {
                if let Some(table) = pending_table.as_mut() {
                    if !table.current_cell.ends_with(' ') {
                        table.current_cell.push(' ');
                    }
                } else if let Some(buffer) = code_block_buffer.as_mut() {
                    buffer.push('\n');
                } else {
                    push_inline(
                        &mut pending_inline_block,
                        &mut inline_stack,
                        RichInline::LineBreak,
                    );
                }
            }
            Event::Rule => {
                flush_inline_block(
                    &mut pending_inline_block,
                    &mut inline_stack,
                    &mut block_stack,
                );
                push_block(&mut block_stack, RichBlock::ThematicBreak);
            }
            Event::Html(html) | Event::InlineHtml(html) => {
                push_inline(
                    &mut pending_inline_block,
                    &mut inline_stack,
                    RichInline::Text(html.to_string()),
                );
            }
            Event::InlineMath(math) => {
                push_inline(
                    &mut pending_inline_block,
                    &mut inline_stack,
                    RichInline::Code(math.to_string()),
                );
            }
            Event::DisplayMath(math) => {
                flush_inline_block(
                    &mut pending_inline_block,
                    &mut inline_stack,
                    &mut block_stack,
                );
                push_block(
                    &mut block_stack,
                    RichBlock::CodeBlock {
                        language: None,
                        code: math.to_string(),
                    },
                );
            }
            Event::FootnoteReference(text) => {
                push_inline(
                    &mut pending_inline_block,
                    &mut inline_stack,
                    RichInline::Text(format!("[{}]", text)),
                );
            }
            Event::TaskListMarker(checked) => {
                push_inline(
                    &mut pending_inline_block,
                    &mut inline_stack,
                    RichInline::Text(if checked {
                        "☑ ".to_string()
                    } else {
                        "☐ ".to_string()
                    }),
                );
            }
        }
    }

    flush_inline_block(
        &mut pending_inline_block,
        &mut inline_stack,
        &mut block_stack,
    );

    let blocks = match block_stack.pop() {
        Some(BlockContainer::Root(blocks)) => blocks,
        _ => Vec::new(),
    };
    RichDocument { blocks }
}

fn translate_markdown_to_telegram_html(input: &str) -> TelegramFormattedText {
    render_rich_document_to_telegram_html(&parse_markdown_to_rich_document(input))
}

fn ensure_inline_block(pending_inline_block: &mut Option<PendingInlineBlock>) {
    if pending_inline_block.is_none() {
        *pending_inline_block = Some(PendingInlineBlock {
            kind: PendingBlockKind::Paragraph,
            inlines: Vec::new(),
        });
    }
}

fn push_inline(
    pending_inline_block: &mut Option<PendingInlineBlock>,
    inline_stack: &mut Vec<PendingInlineContainer>,
    inline: RichInline,
) {
    ensure_inline_block(pending_inline_block);
    if let Some(container) = inline_stack.last_mut() {
        container.inlines.push(inline);
    } else if let Some(block) = pending_inline_block.as_mut() {
        block.inlines.push(inline);
    }
}

fn push_block(block_stack: &mut [BlockContainer], block: RichBlock) {
    if let Some(container) = block_stack.last_mut() {
        match container {
            BlockContainer::Root(blocks)
            | BlockContainer::BlockQuote(blocks)
            | BlockContainer::ListItem(blocks) => blocks.push(block),
            BlockContainer::List { .. } => {}
        }
    }
}

fn flush_inline_block(
    pending_inline_block: &mut Option<PendingInlineBlock>,
    inline_stack: &mut Vec<PendingInlineContainer>,
    block_stack: &mut [BlockContainer],
) {
    while let Some(container) = inline_stack.pop() {
        let inline = match container.style {
            InlineStyle::Emphasis => RichInline::Emphasis(container.inlines),
            InlineStyle::Strong => RichInline::Strong(container.inlines),
            InlineStyle::Strikethrough => RichInline::Strikethrough(container.inlines),
            InlineStyle::Link => RichInline::Link {
                url: container.url.unwrap_or_default(),
                content: container.inlines,
            },
        };
        push_inline(pending_inline_block, inline_stack, inline);
    }

    let Some(block) = pending_inline_block.take() else {
        return;
    };
    if block.inlines.is_empty() {
        return;
    }
    let rich_block = match block.kind {
        PendingBlockKind::Paragraph => RichBlock::Paragraph(block.inlines),
        PendingBlockKind::Heading => RichBlock::Heading(block.inlines),
    };
    push_block(block_stack, rich_block);
}

fn render_rich_document_to_telegram_html(document: &RichDocument) -> TelegramFormattedText {
    let mut output = String::new();
    let mut need_paragraph_break = false;
    render_blocks_for_telegram(&document.blocks, &mut output, &mut need_paragraph_break, 0);
    TelegramFormattedText {
        text: output.trim().to_string(),
    }
}

#[derive(Clone, Copy, Debug)]
struct EntityCursor {
    byte: usize,
    utf16: usize,
}

#[derive(Default)]
struct TelegramEntityBuilder {
    text: String,
    entities: Vec<TelegramMessageEntity>,
}

impl TelegramEntityBuilder {
    fn cursor(&self) -> EntityCursor {
        EntityCursor {
            byte: self.text.len(),
            utf16: utf16_len(&self.text),
        }
    }

    fn push_text(&mut self, value: &str) {
        self.text.push_str(value);
    }

    fn push_entity_trimmed(
        &mut self,
        start: EntityCursor,
        kind: &str,
        url: Option<String>,
        language: Option<String>,
    ) {
        let end = self.cursor();
        let slice = &self.text[start.byte..end.byte];
        let leading_utf16 =
            utf16_len(slice.trim_start_matches(char::is_whitespace)).abs_diff(utf16_len(slice));
        let trailing_utf16 =
            utf16_len(slice.trim_end_matches(char::is_whitespace)).abs_diff(utf16_len(slice));
        let full_length = end.utf16.saturating_sub(start.utf16);
        let length = full_length
            .saturating_sub(leading_utf16)
            .saturating_sub(trailing_utf16);
        if length == 0 {
            return;
        }
        self.entities.push(TelegramMessageEntity {
            kind: kind.to_string(),
            offset: start.utf16 + leading_utf16,
            length,
            url,
            language,
        });
    }

    fn has_entity_of_kinds_in_range(
        &self,
        start_utf16: usize,
        end_utf16: usize,
        kinds: &[&str],
    ) -> bool {
        self.entities.iter().any(|entity| {
            entity.offset >= start_utf16
                && entity.offset + entity.length <= end_utf16
                && kinds.contains(&entity.kind.as_str())
        })
    }

    fn has_any_entity_in_range(&self, start_utf16: usize, end_utf16: usize) -> bool {
        self.entities.iter().any(|entity| {
            entity.offset >= start_utf16 && entity.offset + entity.length <= end_utf16
        })
    }
}

fn render_rich_document_to_telegram_entities(document: &RichDocument) -> TelegramRenderedText {
    let mut builder = TelegramEntityBuilder::default();
    let mut need_paragraph_break = false;
    render_blocks_to_telegram_entities(
        &document.blocks,
        &mut builder,
        &mut need_paragraph_break,
        0,
    );
    builder.entities.sort_by(|left, right| {
        left.offset
            .cmp(&right.offset)
            .then(right.length.cmp(&left.length))
    });
    TelegramRenderedText {
        text: builder.text,
        entities: builder.entities,
    }
}

fn render_blocks_to_telegram_entities(
    blocks: &[RichBlock],
    builder: &mut TelegramEntityBuilder,
    need_paragraph_break: &mut bool,
    quote_depth: usize,
) {
    for block in blocks {
        match block {
            RichBlock::Paragraph(inlines) => {
                ensure_block_break_text(&mut builder.text, need_paragraph_break);
                maybe_render_nested_quote_prefix(builder, quote_depth);
                render_inlines_to_telegram_entities(inlines, builder, quote_depth);
                *need_paragraph_break = true;
            }
            RichBlock::Heading(inlines) => {
                ensure_block_break_text(&mut builder.text, need_paragraph_break);
                maybe_render_nested_quote_prefix(builder, quote_depth);
                let start = builder.cursor();
                render_inlines_to_telegram_entities(inlines, builder, quote_depth);
                maybe_push_wrapping_entity(builder, start, "bold", None, None);
                *need_paragraph_break = true;
            }
            RichBlock::BlockQuote(inner) => {
                ensure_block_break_text(&mut builder.text, need_paragraph_break);
                let start = builder.cursor();
                render_blocks_to_telegram_entities(
                    inner,
                    builder,
                    need_paragraph_break,
                    quote_depth + 1,
                );
                if quote_depth == 0 {
                    builder.push_entity_trimmed(
                        start,
                        classify_blockquote_entity(&builder.text[start.byte..builder.text.len()]),
                        None,
                        None,
                    );
                }
            }
            RichBlock::List { start, items } => {
                ensure_block_break_text(&mut builder.text, need_paragraph_break);
                maybe_render_nested_quote_prefix(builder, quote_depth);
                render_list_to_telegram_entities(*start, items, builder, quote_depth);
                *need_paragraph_break = true;
            }
            RichBlock::CodeBlock { language, code } => {
                ensure_block_break_text(&mut builder.text, need_paragraph_break);
                maybe_render_nested_quote_prefix(builder, quote_depth);
                let start = builder.cursor();
                builder.push_text(code);
                builder.push_entity_trimmed(start, "pre", None, language.clone());
                *need_paragraph_break = true;
            }
            RichBlock::Table(table) => {
                ensure_block_break_text(&mut builder.text, need_paragraph_break);
                maybe_render_nested_quote_prefix(builder, quote_depth);
                let start = builder.cursor();
                builder.push_text(&render_table_text(table));
                builder.push_entity_trimmed(start, "pre", None, None);
                *need_paragraph_break = true;
            }
            RichBlock::ThematicBreak => {
                ensure_block_break_text(&mut builder.text, need_paragraph_break);
                maybe_render_nested_quote_prefix(builder, quote_depth);
                builder.push_text("──────────");
                *need_paragraph_break = true;
            }
        }
    }
}

fn render_list_to_telegram_entities(
    start: Option<u64>,
    items: &[Vec<RichBlock>],
    builder: &mut TelegramEntityBuilder,
    quote_depth: usize,
) {
    let mut next_number = start.unwrap_or(1);
    for (index, item) in items.iter().enumerate() {
        if index > 0 && !builder.text.ends_with('\n') {
            builder.push_text("\n");
        }
        maybe_render_nested_quote_prefix(builder, quote_depth);
        if start.is_some() {
            builder.push_text(&format!("{}. ", next_number));
            next_number += 1;
        } else {
            builder.push_text("• ");
        }
        if let Some((first, rest)) = item.split_first() {
            render_first_list_block_to_telegram_entities(first, builder, quote_depth);
            if !rest.is_empty() {
                let mut nested_break = true;
                render_blocks_to_telegram_entities(rest, builder, &mut nested_break, quote_depth);
            }
        }
    }
}

fn render_first_list_block_to_telegram_entities(
    block: &RichBlock,
    builder: &mut TelegramEntityBuilder,
    quote_depth: usize,
) {
    match block {
        RichBlock::Paragraph(inlines) => {
            render_inlines_to_telegram_entities(inlines, builder, quote_depth)
        }
        RichBlock::Heading(inlines) => {
            let start = builder.cursor();
            render_inlines_to_telegram_entities(inlines, builder, quote_depth);
            maybe_push_wrapping_entity(builder, start, "bold", None, None);
        }
        RichBlock::CodeBlock { language, code } => {
            builder.push_text("\n");
            maybe_render_nested_quote_prefix(builder, quote_depth);
            let start = builder.cursor();
            builder.push_text(code);
            builder.push_entity_trimmed(start, "pre", None, language.clone());
        }
        RichBlock::Table(table) => {
            builder.push_text("\n");
            maybe_render_nested_quote_prefix(builder, quote_depth);
            let start = builder.cursor();
            builder.push_text(&render_table_text(table));
            builder.push_entity_trimmed(start, "pre", None, None);
        }
        RichBlock::ThematicBreak => builder.push_text("──────────"),
        RichBlock::BlockQuote(inner) => {
            let mut nested_break = false;
            render_blocks_to_telegram_entities(inner, builder, &mut nested_break, quote_depth + 1);
        }
        RichBlock::List { start, items } => {
            builder.push_text("\n");
            render_list_to_telegram_entities(*start, items, builder, quote_depth);
        }
    }
}

fn render_inlines_to_telegram_entities(
    inlines: &[RichInline],
    builder: &mut TelegramEntityBuilder,
    quote_depth: usize,
) {
    for inline in inlines {
        match inline {
            RichInline::Text(text) => render_text_to_telegram_entities(text, builder, quote_depth),
            RichInline::Emphasis(children) => {
                let start = builder.cursor();
                render_inlines_to_telegram_entities(children, builder, quote_depth);
                maybe_push_wrapping_entity(builder, start, "italic", None, None);
            }
            RichInline::Strong(children) => {
                let start = builder.cursor();
                render_inlines_to_telegram_entities(children, builder, quote_depth);
                maybe_push_wrapping_entity(builder, start, "bold", None, None);
            }
            RichInline::Strikethrough(children) => {
                let start = builder.cursor();
                render_inlines_to_telegram_entities(children, builder, quote_depth);
                maybe_push_wrapping_entity(builder, start, "strikethrough", None, None);
            }
            RichInline::Link { url, content } => {
                let start = builder.cursor();
                render_inlines_to_telegram_entities(content, builder, quote_depth);
                let end = builder.cursor();
                if !builder.has_any_entity_in_range(start.utf16, end.utf16) {
                    builder.push_entity_trimmed(start, "text_link", Some(url.clone()), None);
                }
            }
            RichInline::Code(code) => {
                let start = builder.cursor();
                builder.push_text(code);
                builder.push_entity_trimmed(start, "code", None, None);
            }
            RichInline::LineBreak => builder.push_text("\n"),
        }
    }
}

fn render_text_to_telegram_entities(
    text: &str,
    builder: &mut TelegramEntityBuilder,
    quote_depth: usize,
) {
    for segment in text.split_inclusive('\n') {
        maybe_render_nested_quote_prefix(builder, quote_depth);
        builder.push_text(segment);
    }
}

fn maybe_render_nested_quote_prefix(builder: &mut TelegramEntityBuilder, quote_depth: usize) {
    if quote_depth > 1 && (builder.text.is_empty() || builder.text.ends_with('\n')) {
        builder.push_text(&"> ".repeat(quote_depth - 1));
    }
}

fn classify_blockquote_entity(text: &str) -> &'static str {
    let line_count = text.lines().count();
    let char_count = text.chars().count();
    if line_count >= 6 || char_count >= 360 {
        "expandable_blockquote"
    } else {
        "blockquote"
    }
}

fn maybe_push_wrapping_entity(
    builder: &mut TelegramEntityBuilder,
    start: EntityCursor,
    kind: &str,
    url: Option<String>,
    language: Option<String>,
) {
    let end = builder.cursor();
    if matches!(kind, "bold" | "italic" | "strikethrough")
        && builder.has_entity_of_kinds_in_range(start.utf16, end.utf16, &["code", "pre"])
    {
        return;
    }
    builder.push_entity_trimmed(start, kind, url, language);
}

fn render_blocks_for_telegram(
    blocks: &[RichBlock],
    output: &mut String,
    need_paragraph_break: &mut bool,
    quote_depth: usize,
) {
    for block in blocks {
        match block {
            RichBlock::Paragraph(inlines) => {
                ensure_block_break(output, need_paragraph_break);
                render_inlines_for_telegram(inlines, output, quote_depth);
                *need_paragraph_break = true;
            }
            RichBlock::Heading(inlines) => {
                ensure_block_break(output, need_paragraph_break);
                render_quote_prefix_if_needed(output, quote_depth);
                output.push_str("<b>");
                render_inlines_for_telegram(inlines, output, quote_depth);
                output.push_str("</b>");
                *need_paragraph_break = true;
            }
            RichBlock::BlockQuote(inner) => {
                render_blocks_for_telegram(inner, output, need_paragraph_break, quote_depth + 1);
            }
            RichBlock::List { start, items } => {
                ensure_block_break(output, need_paragraph_break);
                render_list_for_telegram(items, *start, output, quote_depth);
                *need_paragraph_break = true;
            }
            RichBlock::CodeBlock { language, code } => {
                ensure_block_break(output, need_paragraph_break);
                render_code_block_for_telegram(language.as_deref(), code, output);
                *need_paragraph_break = true;
            }
            RichBlock::Table(table) => {
                ensure_block_break(output, need_paragraph_break);
                output.push_str(&render_table_for_telegram(table));
                *need_paragraph_break = true;
            }
            RichBlock::ThematicBreak => {
                ensure_block_break(output, need_paragraph_break);
                output.push_str("──────────");
                *need_paragraph_break = true;
            }
        }
    }
}

fn render_list_for_telegram(
    items: &[Vec<RichBlock>],
    start: Option<u64>,
    output: &mut String,
    quote_depth: usize,
) {
    let mut next_number = start.unwrap_or(1);
    for (index, item) in items.iter().enumerate() {
        if index > 0 && !output.ends_with('\n') {
            output.push('\n');
        }
        render_quote_prefix_if_needed(output, quote_depth);
        if start.is_some() {
            output.push_str(&format!("{}. ", next_number));
            next_number += 1;
        } else {
            output.push_str("• ");
        }

        if let Some((first, rest)) = item.split_first() {
            render_first_list_block_for_telegram(first, output, quote_depth);
            if !rest.is_empty() {
                let mut nested_break = true;
                render_blocks_for_telegram(rest, output, &mut nested_break, quote_depth);
            }
        }
    }
}

fn render_first_list_block_for_telegram(
    block: &RichBlock,
    output: &mut String,
    quote_depth: usize,
) {
    match block {
        RichBlock::Paragraph(inlines) => render_inlines_for_telegram(inlines, output, quote_depth),
        RichBlock::Heading(inlines) => {
            output.push_str("<b>");
            render_inlines_for_telegram(inlines, output, quote_depth);
            output.push_str("</b>");
        }
        RichBlock::CodeBlock { language, code } => {
            output.push('\n');
            render_code_block_for_telegram(language.as_deref(), code, output);
        }
        RichBlock::Table(table) => {
            output.push('\n');
            output.push_str(&render_table_for_telegram(table));
        }
        RichBlock::ThematicBreak => output.push_str("──────────"),
        RichBlock::BlockQuote(inner) => {
            let mut nested_break = false;
            render_blocks_for_telegram(inner, output, &mut nested_break, quote_depth + 1);
        }
        RichBlock::List { start, items } => {
            output.push('\n');
            render_list_for_telegram(items, *start, output, quote_depth);
        }
    }
}

fn render_inlines_for_telegram(inlines: &[RichInline], output: &mut String, quote_depth: usize) {
    for inline in inlines {
        match inline {
            RichInline::Text(text) => render_text_for_telegram(text, output, quote_depth),
            RichInline::Emphasis(children) => {
                render_quote_prefix_if_needed(output, quote_depth);
                output.push_str("<i>");
                render_inlines_for_telegram(children, output, quote_depth);
                output.push_str("</i>");
            }
            RichInline::Strong(children) => {
                render_quote_prefix_if_needed(output, quote_depth);
                output.push_str("<b>");
                render_inlines_for_telegram(children, output, quote_depth);
                output.push_str("</b>");
            }
            RichInline::Strikethrough(children) => {
                render_quote_prefix_if_needed(output, quote_depth);
                output.push_str("<s>");
                render_inlines_for_telegram(children, output, quote_depth);
                output.push_str("</s>");
            }
            RichInline::Link { url, content } => {
                render_quote_prefix_if_needed(output, quote_depth);
                output.push_str("<a href=\"");
                output.push_str(&escape_html_attribute(url));
                output.push_str("\">");
                render_inlines_for_telegram(content, output, quote_depth);
                output.push_str("</a>");
            }
            RichInline::Code(code) => {
                render_quote_prefix_if_needed(output, quote_depth);
                output.push_str("<code>");
                output.push_str(&escape_html_text(code));
                output.push_str("</code>");
            }
            RichInline::LineBreak => output.push('\n'),
        }
    }
}

fn render_text_for_telegram(text: &str, output: &mut String, quote_depth: usize) {
    for segment in text.split_inclusive('\n') {
        render_quote_prefix_if_needed(output, quote_depth);
        output.push_str(&escape_html_text(segment));
    }
}

fn render_quote_prefix_if_needed(output: &mut String, quote_depth: usize) {
    if quote_depth > 0 && starts_new_block_line(output) {
        output.push_str(&"&gt; ".repeat(quote_depth));
    }
}

fn render_code_block_for_telegram(language: Option<&str>, code: &str, output: &mut String) {
    if let Some(language) = language {
        output.push_str("<pre><code class=\"language-");
        output.push_str(&escape_html_attribute(language));
        output.push_str("\">");
        output.push_str(&escape_html_text(code));
        output.push_str("</code></pre>");
    } else {
        output.push_str("<pre>");
        output.push_str(&escape_html_text(code));
        output.push_str("</pre>");
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

fn normalize_table_cell(value: &str) -> String {
    value.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn pad_table_cell(value: &str, width: usize) -> String {
    let cell_width = value.chars().count();
    if cell_width >= width {
        value.to_string()
    } else {
        format!("{}{}", value, " ".repeat(width - cell_width))
    }
}

fn render_table_for_telegram(table: &RichTable) -> String {
    let text = render_table_text(table);
    if text.is_empty() {
        String::new()
    } else {
        format!("<pre>{}</pre>", escape_html_text(&text))
    }
}

fn render_table_text(table: &RichTable) -> String {
    if table.rows.is_empty() {
        return String::new();
    }

    let column_count = table.rows.iter().map(Vec::len).max().unwrap_or(0);
    if column_count == 0 {
        return String::new();
    }

    let widths = (0..column_count)
        .map(|column| {
            table
                .rows
                .iter()
                .filter_map(|row| row.get(column))
                .map(|cell| cell.chars().count())
                .max()
                .unwrap_or(0)
        })
        .collect::<Vec<_>>();

    let mut lines = Vec::new();
    for (index, row) in table.rows.iter().enumerate() {
        let rendered = (0..column_count)
            .map(|column| {
                let value = row.get(column).cloned().unwrap_or_default();
                pad_table_cell(&value, widths[column])
            })
            .collect::<Vec<_>>()
            .join(" | ");
        lines.push(rendered);
        if table.header_rows > 0 && index + 1 == table.header_rows {
            let separator = widths
                .iter()
                .map(|width| "─".repeat(*width))
                .collect::<Vec<_>>()
                .join("─┼─");
            lines.push(separator);
        }
    }

    lines.join("\n")
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

fn ensure_block_break_text(output: &mut String, need_paragraph_break: &mut bool) {
    ensure_block_break(output, need_paragraph_break)
}

fn utf16_len(value: &str) -> usize {
    value.encode_utf16().count()
}

fn telegram_text_len(value: &str) -> usize {
    utf16_len(value)
}

fn split_markdown_for_telegram_documents(input: &str, max_chars: usize) -> Vec<RichDocument> {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return Vec::new();
    }

    let document = parse_markdown_to_rich_document(trimmed);
    if rendered_length_for_document(&document) <= max_chars {
        return vec![document];
    }

    match split_blocks_to_fit(&document.blocks, max_chars) {
        Some(chunks) => chunks
            .into_iter()
            .map(|blocks| RichDocument { blocks })
            .collect(),
        None => split_markdown_message_legacy(trimmed, max_chars)
            .into_iter()
            .map(|chunk| parse_markdown_to_rich_document(&chunk))
            .collect(),
    }
}

fn render_markdown_chunks_to_telegram_entities(
    input: &str,
    max_chars: usize,
) -> Vec<TelegramRenderedText> {
    split_markdown_for_telegram_documents(input, max_chars)
        .into_iter()
        .map(|document| render_rich_document_to_telegram_entities(&document))
        .collect()
}

fn rendered_length_for_document(document: &RichDocument) -> usize {
    telegram_text_len(&render_rich_document_to_telegram_entities(document).text)
}

fn rendered_length_for_blocks(blocks: &[RichBlock]) -> usize {
    rendered_length_for_document(&RichDocument {
        blocks: blocks.to_vec(),
    })
}

fn rendered_length_for_block(block: &RichBlock) -> usize {
    rendered_length_for_document(&RichDocument {
        blocks: vec![block.clone()],
    })
}

fn split_blocks_to_fit(blocks: &[RichBlock], max_chars: usize) -> Option<Vec<Vec<RichBlock>>> {
    let mut chunks = Vec::new();
    let mut current_blocks = Vec::new();

    for block in blocks {
        let split_parts = split_block_to_fit(block, max_chars)?;
        for part in split_parts {
            let mut candidate_blocks = current_blocks.clone();
            candidate_blocks.extend(part.clone());
            if rendered_length_for_blocks(&candidate_blocks) <= max_chars {
                current_blocks = candidate_blocks;
                continue;
            }
            if !current_blocks.is_empty() {
                chunks.push(std::mem::take(&mut current_blocks));
            }
            if rendered_length_for_blocks(&part) > max_chars {
                return None;
            }
            current_blocks = part;
        }
    }

    if !current_blocks.is_empty() {
        chunks.push(current_blocks);
    }
    Some(chunks)
}

fn split_block_to_fit(block: &RichBlock, max_chars: usize) -> Option<Vec<Vec<RichBlock>>> {
    if rendered_length_for_block(block) <= max_chars {
        return Some(vec![vec![block.clone()]]);
    }

    match block {
        RichBlock::Paragraph(inlines) => {
            split_inline_block_to_fit(PendingBlockKind::Paragraph, inlines, max_chars)
        }
        RichBlock::Heading(inlines) => {
            split_inline_block_to_fit(PendingBlockKind::Heading, inlines, max_chars)
        }
        RichBlock::BlockQuote(blocks) => {
            let chunks = split_blocks_to_fit(blocks, max_chars)?;
            Some(
                chunks
                    .into_iter()
                    .map(|chunk| vec![RichBlock::BlockQuote(chunk)])
                    .collect(),
            )
        }
        RichBlock::List { start, items } => split_list_block_to_fit(*start, items, max_chars),
        RichBlock::CodeBlock { language, code } => {
            split_code_block_to_fit(language.clone(), code, max_chars)
        }
        RichBlock::Table(_) => None,
        RichBlock::ThematicBreak => Some(vec![vec![RichBlock::ThematicBreak]]),
    }
}

fn split_inline_block_to_fit(
    kind: PendingBlockKind,
    inlines: &[RichInline],
    max_chars: usize,
) -> Option<Vec<Vec<RichBlock>>> {
    let mut chunks = Vec::new();
    let mut current = Vec::new();

    for inline in inlines {
        let split_parts = split_inline_to_fit(kind, inline, max_chars)?;
        for part in split_parts {
            let mut candidate = current.clone();
            candidate.push(part.clone());
            if rendered_length_for_inline_block(kind, &candidate) <= max_chars {
                current = candidate;
                continue;
            }
            if !current.is_empty() {
                chunks.push(vec![inline_block_from_parts(
                    kind,
                    std::mem::take(&mut current),
                )]);
            }
            if rendered_length_for_inline_block(kind, std::slice::from_ref(&part)) > max_chars {
                return None;
            }
            current.push(part);
        }
    }

    if !current.is_empty() {
        chunks.push(vec![inline_block_from_parts(kind, current)]);
    }
    Some(chunks)
}

fn split_inline_to_fit(
    block_kind: PendingBlockKind,
    inline: &RichInline,
    max_chars: usize,
) -> Option<Vec<RichInline>> {
    if rendered_length_for_inline_block(block_kind, std::slice::from_ref(inline)) <= max_chars {
        return Some(vec![inline.clone()]);
    }

    match inline {
        RichInline::Text(text) => split_text_inline_to_fit(block_kind, text, max_chars),
        RichInline::Code(code) => {
            split_leaf_inline_text_to_fit(block_kind, code, max_chars, RichInline::Code)
        }
        RichInline::LineBreak => Some(vec![RichInline::LineBreak]),
        RichInline::Emphasis(children) => {
            split_wrapped_inline_to_fit(block_kind, children, max_chars, &InlineWrapper::Emphasis)
        }
        RichInline::Strong(children) => {
            split_wrapped_inline_to_fit(block_kind, children, max_chars, &InlineWrapper::Strong)
        }
        RichInline::Strikethrough(children) => split_wrapped_inline_to_fit(
            block_kind,
            children,
            max_chars,
            &InlineWrapper::Strikethrough,
        ),
        RichInline::Link { url, content } => split_wrapped_inline_to_fit(
            block_kind,
            content,
            max_chars,
            &InlineWrapper::Link(url.clone()),
        ),
    }
}

fn split_text_inline_to_fit(
    block_kind: PendingBlockKind,
    text: &str,
    max_chars: usize,
) -> Option<Vec<RichInline>> {
    split_leaf_inline_text_to_fit(block_kind, text, max_chars, |value| RichInline::Text(value))
}

fn split_leaf_inline_text_to_fit<F>(
    block_kind: PendingBlockKind,
    text: &str,
    max_chars: usize,
    make_inline: F,
) -> Option<Vec<RichInline>>
where
    F: Fn(String) -> RichInline,
{
    split_text_to_fit(
        text,
        |candidate| {
            rendered_length_for_inline_block(block_kind, &[make_inline(candidate.to_string())])
        },
        max_chars,
    )
    .map(|parts| {
        parts
            .into_iter()
            .map(|part| make_inline(part))
            .collect::<Vec<_>>()
    })
    .filter(|parts| {
        !parts.is_empty()
            && parts.iter().all(|inline| {
                rendered_length_for_inline_block(block_kind, std::slice::from_ref(inline))
                    <= max_chars
            })
    })
}

fn split_wrapped_inline_to_fit(
    block_kind: PendingBlockKind,
    children: &[RichInline],
    max_chars: usize,
    wrapper: &InlineWrapper,
) -> Option<Vec<RichInline>> {
    let mut chunks = Vec::new();
    let mut current = Vec::new();

    for child in children {
        let split_parts = split_child_for_wrapped_chunk(block_kind, child, max_chars, wrapper)?;
        for part in split_parts {
            let mut candidate = current.clone();
            candidate.push(part.clone());
            if rendered_length_for_inline_block(
                block_kind,
                &[apply_inline_wrapper(wrapper, candidate.clone())],
            ) <= max_chars
            {
                current = candidate;
                continue;
            }
            if !current.is_empty() {
                chunks.push(apply_inline_wrapper(wrapper, std::mem::take(&mut current)));
            }
            if rendered_length_for_inline_block(
                block_kind,
                &[apply_inline_wrapper(wrapper, vec![part.clone()])],
            ) > max_chars
            {
                return None;
            }
            current.push(part);
        }
    }

    if !current.is_empty() {
        chunks.push(apply_inline_wrapper(wrapper, current));
    }

    Some(chunks)
}

fn split_child_for_wrapped_chunk(
    block_kind: PendingBlockKind,
    child: &RichInline,
    max_chars: usize,
    wrapper: &InlineWrapper,
) -> Option<Vec<RichInline>> {
    if rendered_length_for_inline_block(
        block_kind,
        &[apply_inline_wrapper(wrapper, vec![child.clone()])],
    ) <= max_chars
    {
        return Some(vec![child.clone()]);
    }

    match child {
        RichInline::Text(text) => split_text_to_fit(
            text,
            |candidate| {
                rendered_length_for_inline_block(
                    block_kind,
                    &[apply_inline_wrapper(
                        wrapper,
                        vec![RichInline::Text(candidate.to_string())],
                    )],
                )
            },
            max_chars,
        )
        .map(|parts| parts.into_iter().map(RichInline::Text).collect()),
        RichInline::Code(code) => split_text_to_fit(
            code,
            |candidate| {
                rendered_length_for_inline_block(
                    block_kind,
                    &[apply_inline_wrapper(
                        wrapper,
                        vec![RichInline::Code(candidate.to_string())],
                    )],
                )
            },
            max_chars,
        )
        .map(|parts| parts.into_iter().map(RichInline::Code).collect()),
        RichInline::LineBreak => Some(vec![RichInline::LineBreak]),
        RichInline::Emphasis(children) => {
            split_wrapped_inline_to_fit(block_kind, children, max_chars, &InlineWrapper::Emphasis)
        }
        RichInline::Strong(children) => {
            split_wrapped_inline_to_fit(block_kind, children, max_chars, &InlineWrapper::Strong)
        }
        RichInline::Strikethrough(children) => split_wrapped_inline_to_fit(
            block_kind,
            children,
            max_chars,
            &InlineWrapper::Strikethrough,
        ),
        RichInline::Link { url, content } => split_wrapped_inline_to_fit(
            block_kind,
            content,
            max_chars,
            &InlineWrapper::Link(url.clone()),
        ),
    }
}

fn apply_inline_wrapper(wrapper: &InlineWrapper, children: Vec<RichInline>) -> RichInline {
    match wrapper {
        InlineWrapper::Emphasis => RichInline::Emphasis(children),
        InlineWrapper::Strong => RichInline::Strong(children),
        InlineWrapper::Strikethrough => RichInline::Strikethrough(children),
        InlineWrapper::Link(url) => RichInline::Link {
            url: url.clone(),
            content: children,
        },
    }
}

fn split_list_block_to_fit(
    start: Option<u64>,
    items: &[Vec<RichBlock>],
    max_chars: usize,
) -> Option<Vec<Vec<RichBlock>>> {
    let mut normalized_items = Vec::new();
    for item in items {
        if rendered_length_for_single_item_list(start, item) <= max_chars {
            normalized_items.push(item.clone());
            continue;
        }
        let item_chunks = split_blocks_to_fit(item, max_chars)?;
        for chunk in item_chunks {
            if rendered_length_for_single_item_list(start, &chunk) > max_chars {
                return None;
            }
            normalized_items.push(chunk);
        }
    }

    let mut chunks = Vec::new();
    let mut current_items = Vec::new();
    let mut current_start = start;
    let mut next_number = start.unwrap_or(1);

    for item in normalized_items {
        let candidate_items = {
            let mut items = current_items.clone();
            items.push(item.clone());
            items
        };
        let candidate_block = RichBlock::List {
            start: current_start,
            items: candidate_items.clone(),
        };
        if rendered_length_for_block(&candidate_block) <= max_chars {
            current_items = candidate_items;
        } else {
            if !current_items.is_empty() {
                chunks.push(vec![RichBlock::List {
                    start: current_start,
                    items: std::mem::take(&mut current_items),
                }]);
                current_start = start.map(|_| next_number);
            }
            current_items.push(item.clone());
            let single_block = RichBlock::List {
                start: current_start,
                items: current_items.clone(),
            };
            if rendered_length_for_block(&single_block) > max_chars {
                return None;
            }
        }
        if start.is_some() {
            next_number += 1;
        }
    }

    if !current_items.is_empty() {
        chunks.push(vec![RichBlock::List {
            start: current_start,
            items: current_items,
        }]);
    }

    Some(chunks)
}

fn split_code_block_to_fit(
    language: Option<String>,
    code: &str,
    max_chars: usize,
) -> Option<Vec<Vec<RichBlock>>> {
    let parts = split_text_to_fit(
        code,
        |candidate| {
            rendered_length_for_block(&RichBlock::CodeBlock {
                language: language.clone(),
                code: candidate.to_string(),
            })
        },
        max_chars,
    )?;

    let chunks = parts
        .into_iter()
        .map(|part| {
            vec![RichBlock::CodeBlock {
                language: language.clone(),
                code: part,
            }]
        })
        .collect::<Vec<_>>();

    if chunks
        .iter()
        .all(|chunk| rendered_length_for_blocks(chunk) <= max_chars)
    {
        Some(chunks)
    } else {
        None
    }
}

fn split_text_to_fit<F>(text: &str, measure: F, max_chars: usize) -> Option<Vec<String>>
where
    F: Fn(&str) -> usize,
{
    if text.is_empty() {
        return Some(Vec::new());
    }

    let chars: Vec<char> = text.chars().collect();
    let mut cursor = 0usize;
    let mut chunks = Vec::new();

    while cursor < chars.len() {
        let remaining = chars.len() - cursor;
        let mut low = 1usize;
        let mut high = remaining;
        let mut best = 0usize;
        while low <= high {
            let mid = (low + high) / 2;
            let candidate: String = chars[cursor..cursor + mid].iter().collect();
            if measure(&candidate) <= max_chars {
                best = mid;
                low = mid + 1;
            } else {
                high = mid.saturating_sub(1);
            }
        }
        if best == 0 {
            return None;
        }

        let mut end = cursor + best;
        if end < chars.len()
            && let Some(adjusted) = prefer_split_boundary_with_measure(
                &chars[cursor..end],
                best / 2,
                &measure,
                max_chars,
            )
        {
            end = cursor + adjusted;
        }

        let chunk: String = chars[cursor..end].iter().collect();
        if chunk.is_empty() {
            return None;
        }
        chunks.push(chunk);
        cursor = end;
    }

    Some(chunks)
}

fn prefer_split_boundary_with_measure<F>(
    chars: &[char],
    minimum_index: usize,
    measure: &F,
    max_chars: usize,
) -> Option<usize>
where
    F: Fn(&str) -> usize,
{
    let text: String = chars.iter().collect();
    for needle in ["\n\n", "\n", " "] {
        if let Some(index) = text.rfind(needle) {
            let split_index = index + needle.len();
            if split_index >= minimum_index {
                let candidate = &text[..split_index];
                if measure(candidate) <= max_chars {
                    return Some(candidate.chars().count());
                }
            }
        }
    }
    None
}

fn inline_block_from_parts(kind: PendingBlockKind, inlines: Vec<RichInline>) -> RichBlock {
    match kind {
        PendingBlockKind::Paragraph => RichBlock::Paragraph(inlines),
        PendingBlockKind::Heading => RichBlock::Heading(inlines),
    }
}

fn rendered_length_for_inline_block(kind: PendingBlockKind, inlines: &[RichInline]) -> usize {
    rendered_length_for_block(&inline_block_from_parts(kind, inlines.to_vec()))
}

fn rendered_length_for_single_item_list(start: Option<u64>, item: &[RichBlock]) -> usize {
    rendered_length_for_block(&RichBlock::List {
        start,
        items: vec![item.to_vec()],
    })
}

fn split_markdown_message_legacy(input: &str, max_chars: usize) -> Vec<String> {
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
            let translated_len =
                telegram_text_len(&translate_markdown_to_telegram_html(&candidate).text);
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
        RichBlock, RichInline, TelegramChannel, TelegramChat, TelegramMessage,
        TelegramMessageEntity, TelegramRenderedText, build_edit_text_payload,
        build_send_text_payload, parse_markdown_to_rich_document, poll_backoff_seconds,
        render_markdown_chunks_to_telegram_entities, render_rich_document_to_telegram_entities,
        telegram_text_len, translate_markdown_to_telegram_html,
    };
    use crate::domain::ShowOptions;
    use anyhow::anyhow;
    use reqwest::Client;
    use std::collections::{HashMap, VecDeque};
    use std::time::{Duration, Instant};
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
    fn renders_basic_entities_for_telegram() {
        let document = parse_markdown_to_rich_document(
            "# Title\n\n**bold** and *italic* with [link](https://example.com).\n\n```rust\nlet x = 1;\n```",
        );
        let rendered = render_rich_document_to_telegram_entities(&document);

        assert!(rendered.text.contains("Title"));
        assert!(rendered.text.contains("bold"));
        assert!(rendered.text.contains("italic"));
        assert!(rendered.text.contains("let x = 1;"));
        assert!(rendered.entities.iter().any(|entity| entity.kind == "bold"));
        assert!(
            rendered
                .entities
                .iter()
                .any(|entity| entity.kind == "italic")
        );
        assert!(
            rendered
                .entities
                .iter()
                .any(|entity| entity.kind == "text_link")
        );
        assert!(
            rendered
                .entities
                .iter()
                .any(|entity| entity.kind == "pre" && entity.language.as_deref() == Some("rust"))
        );
    }

    #[test]
    fn renders_blockquote_entities_for_telegram() {
        let document = parse_markdown_to_rich_document("> quoted line\n>\n> second line");
        let rendered = render_rich_document_to_telegram_entities(&document);

        assert!(rendered.text.contains("quoted line"));
        assert!(rendered.text.contains("second line"));
        assert!(
            rendered
                .entities
                .iter()
                .any(|entity| entity.kind == "blockquote")
        );
    }

    #[test]
    fn build_send_text_payload_includes_entities_and_inline_keyboard() {
        let payload = build_send_text_payload(
            "123",
            TelegramRenderedText {
                text: "hello".to_string(),
                entities: vec![TelegramMessageEntity {
                    kind: "bold".to_string(),
                    offset: 0,
                    length: 5,
                    url: None,
                    language: None,
                }],
            },
            Some(&ShowOptions {
                prompt: "Choose".to_string(),
                options: vec![crate::domain::ShowOption {
                    label: "One".to_string(),
                    value: "One".to_string(),
                }],
                one_time: true,
            }),
        )
        .unwrap();

        assert_eq!(payload["chat_id"], "123");
        assert_eq!(payload["text"], "hello");
        assert_eq!(payload["entities"][0]["type"], "bold");
        assert_eq!(
            payload["reply_markup"]["inline_keyboard"][0][0]["text"],
            "One"
        );
        assert_eq!(
            payload["reply_markup"]["inline_keyboard"][0][0]["callback_data"],
            "One"
        );
    }

    #[test]
    fn build_edit_text_payload_includes_message_id_and_entities() {
        let payload = build_edit_text_payload(
            "123",
            42,
            TelegramRenderedText {
                text: "hello".to_string(),
                entities: vec![TelegramMessageEntity {
                    kind: "italic".to_string(),
                    offset: 0,
                    length: 5,
                    url: None,
                    language: None,
                }],
            },
            None,
        )
        .unwrap();

        assert_eq!(payload["chat_id"], "123");
        assert_eq!(payload["message_id"], 42);
        assert_eq!(payload["entities"][0]["type"], "italic");
    }

    #[test]
    fn build_send_text_payload_lays_out_inline_keyboard_two_per_row() {
        let payload = build_send_text_payload(
            "123",
            TelegramRenderedText {
                text: "choose".to_string(),
                entities: Vec::new(),
            },
            Some(&ShowOptions {
                prompt: "Choose a model".to_string(),
                options: vec![
                    crate::domain::ShowOption {
                        label: "One".to_string(),
                        value: "/one".to_string(),
                    },
                    crate::domain::ShowOption {
                        label: "Two".to_string(),
                        value: "/two".to_string(),
                    },
                    crate::domain::ShowOption {
                        label: "Three".to_string(),
                        value: "/three".to_string(),
                    },
                ],
                one_time: true,
            }),
        )
        .unwrap();

        assert_eq!(
            payload["reply_markup"]["inline_keyboard"][0]
                .as_array()
                .unwrap()
                .len(),
            2
        );
        assert_eq!(
            payload["reply_markup"]["inline_keyboard"][1]
                .as_array()
                .unwrap()
                .len(),
            1
        );
    }

    #[test]
    fn renders_long_blockquotes_as_expandable_entities() {
        let document = parse_markdown_to_rich_document(
            &(0..8)
                .map(|index| format!("> quoted line {}", index))
                .collect::<Vec<_>>()
                .join("\n"),
        );
        let rendered = render_rich_document_to_telegram_entities(&document);

        assert!(
            rendered
                .entities
                .iter()
                .any(|entity| entity.kind == "expandable_blockquote")
        );
    }

    #[test]
    fn preserves_nested_blockquote_depth_markers() {
        let document =
            parse_markdown_to_rich_document("> outer line\n> > inner line\n> > second inner line");
        let rendered = render_rich_document_to_telegram_entities(&document);

        assert!(rendered.text.contains("outer line"));
        assert!(rendered.text.contains("> inner line"));
        assert!(rendered.text.contains("> second inner line"));
        assert!(
            rendered
                .entities
                .iter()
                .any(|entity| entity.kind == "blockquote")
        );
    }

    #[test]
    fn parses_markdown_into_rich_blocks_before_rendering() {
        let document =
            parse_markdown_to_rich_document("# Title\n\n**bold** and `code`\n\n- one\n- two");

        assert!(matches!(
            document.blocks.first(),
            Some(RichBlock::Heading(_))
        ));
        assert!(matches!(
            document.blocks.get(1),
            Some(RichBlock::Paragraph(_))
        ));
        assert!(matches!(
            document.blocks.get(2),
            Some(RichBlock::List { items, .. }) if items.len() == 2
        ));

        let Some(RichBlock::Paragraph(inlines)) = document.blocks.get(1) else {
            panic!("expected paragraph block");
        };
        assert!(
            inlines
                .iter()
                .any(|inline| matches!(inline, RichInline::Strong(_)))
        );
        assert!(
            inlines
                .iter()
                .any(|inline| matches!(inline, RichInline::Code(code) if code == "code"))
        );
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
    fn translates_markdown_tables_into_preformatted_text() {
        let translated = translate_markdown_to_telegram_html(
            "| Name | Status | GPU |\n|------|--------|-----|\n| job-a | running | 8xH100 |\n| job-b | failed | 4xH100 |",
        );

        assert!(translated.text.contains("<pre>"));
        assert!(translated.text.contains("Name"));
        assert!(translated.text.contains("Status"));
        assert!(translated.text.contains("GPU"));
        assert!(translated.text.contains("job-a"));
        assert!(translated.text.contains("running"));
        assert!(translated.text.contains("8xH100"));
        assert!(translated.text.contains("┼") || translated.text.contains("─"));
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
        let chunks = render_markdown_chunks_to_telegram_entities(&input, 4096);
        assert!(chunks.len() >= 2);
        assert!(
            chunks
                .iter()
                .all(|chunk| { telegram_text_len(&chunk.text) <= 4096 })
        );
    }

    #[test]
    fn splits_caption_safely_under_caption_limit() {
        let input = format!("**{}**", "x".repeat(1400));
        let chunks = render_markdown_chunks_to_telegram_entities(&input, 1024);
        assert!(chunks.len() >= 2);
        assert!(
            chunks
                .iter()
                .all(|chunk| { telegram_text_len(&chunk.text) <= 1024 })
        );
    }

    #[test]
    fn splits_prefer_rich_block_boundaries_before_falling_back() {
        let input = format!(
            "{}\n\n{}\n\n{}",
            "a".repeat(1000),
            "b".repeat(1000),
            "c".repeat(1000)
        );
        let chunks = render_markdown_chunks_to_telegram_entities(&input, 2200);

        assert_eq!(chunks.len(), 2);
        assert!(chunks[0].text.contains(&"a".repeat(1000)));
        assert!(chunks[0].text.contains(&"b".repeat(1000)));
        assert!(!chunks[0].text.contains(&"c".repeat(1000)));
    }

    #[test]
    fn splits_single_large_paragraph_without_legacy_markdown_boundaries() {
        let input = format!("**{}**", vec!["word"; 900].join(" "));
        let chunks = render_markdown_chunks_to_telegram_entities(&input, 1024);

        assert!(chunks.len() >= 2);
        assert!(
            chunks
                .iter()
                .all(|chunk| telegram_text_len(&chunk.text) <= 1024)
        );
        assert!(
            chunks
                .iter()
                .all(|chunk| chunk.entities.iter().any(|entity| entity.kind == "bold"))
        );
    }

    #[test]
    fn splits_large_code_block_into_multiple_pre_blocks() {
        let input = format!("```rust\n{}\n```", "let x = 42;\n".repeat(300));
        let chunks = render_markdown_chunks_to_telegram_entities(&input, 1024);

        assert!(chunks.len() >= 2);
        assert!(
            chunks
                .iter()
                .all(|chunk| telegram_text_len(&chunk.text) <= 1024)
        );
        assert!(
            chunks
                .iter()
                .all(|chunk| chunk.entities.iter().any(|entity| entity.kind == "pre"))
        );
    }

    #[test]
    fn splits_large_code_block_with_astral_chars_under_utf16_limit() {
        let input = format!("```txt\n{}\n```", "😀".repeat(900));
        let chunks = render_markdown_chunks_to_telegram_entities(&input, 1024);

        assert!(chunks.len() >= 2);
        assert!(
            chunks
                .iter()
                .all(|chunk| telegram_text_len(&chunk.text) <= 1024)
        );
        assert!(
            chunks
                .iter()
                .all(|chunk| chunk.entities.iter().any(|entity| entity.kind == "pre"))
        );
    }

    #[test]
    fn splits_long_list_across_multiple_messages() {
        let input = (1..=40)
            .map(|index| format!("- item {} {}", index, "x".repeat(60)))
            .collect::<Vec<_>>()
            .join("\n");
        let chunks = render_markdown_chunks_to_telegram_entities(&input, 1024);

        assert!(chunks.len() >= 2);
        assert!(
            chunks
                .iter()
                .all(|chunk| telegram_text_len(&chunk.text) <= 1024)
        );
        assert!(chunks.iter().all(|chunk| chunk.text.contains("• ")));
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
            progress_messages: Mutex::new(HashMap::new()),
        };

        let redacted = channel
            .redact_sensitive_text("https://api.telegram.org/botsecret-token/getUpdates failed");
        assert!(!redacted.contains("secret-token"));
        assert!(redacted.contains("[REDACTED_TELEGRAM_BOT_TOKEN]"));
    }

    #[test]
    fn telegram_json_api_timeouts_are_bounded() {
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
            progress_messages: Mutex::new(HashMap::new()),
        };

        assert_eq!(
            channel.api_call_timeout("getUpdates"),
            Duration::from_secs(45)
        );
        assert_eq!(
            channel.api_call_timeout("sendChatAction"),
            Duration::from_secs(10)
        );
        assert_eq!(
            channel.api_call_timeout("sendMessage"),
            Duration::from_secs(60)
        );
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
            progress_messages: Mutex::new(HashMap::new()),
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
            progress_messages: Mutex::new(HashMap::new()),
        };

        let error = anyhow!("telegram API sendMessage failed: Bad Request: chat not found");
        assert!(!channel.should_defer_outbound_message(&error));
    }

    #[test]
    fn detects_terminal_chat_errors() {
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
            progress_messages: Mutex::new(HashMap::new()),
        };

        assert!(channel.is_terminal_chat_error(&anyhow!(
            "telegram API getChat failed: Bad Request: chat not found"
        )));
        assert!(channel.is_terminal_chat_error(&anyhow!(
            "telegram API getChat failed: Bad Request: group chat was deleted"
        )));
        assert!(channel.is_terminal_chat_error(&anyhow!(
            "telegram API getChatMemberCount failed: Bad Request: group chat was upgraded to a supergroup chat"
        )));
        assert!(
            !channel
                .is_terminal_chat_error(&anyhow!("telegram API getChat failed: Too Many Requests"))
        );
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
            progress_messages: Mutex::new(HashMap::new()),
        };
        let message = TelegramMessage {
            message_id: 1,
            date: None,
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
            progress_messages: Mutex::new(HashMap::new()),
        };
        let message = TelegramMessage {
            message_id: 1,
            date: None,
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
            progress_messages: Mutex::new(HashMap::new()),
        };
        let message = TelegramMessage {
            message_id: 1,
            date: None,
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
            progress_messages: Mutex::new(HashMap::new()),
        };
        let message = TelegramMessage {
            message_id: 1,
            date: None,
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
    callback_query: Option<TelegramCallbackQuery>,
}

#[derive(Deserialize)]
struct TelegramCallbackQuery {
    id: String,
    from: TelegramUser,
    message: Option<TelegramMessage>,
    data: Option<String>,
}

#[derive(Deserialize)]
struct TelegramMessage {
    message_id: i64,
    date: Option<i64>,
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
