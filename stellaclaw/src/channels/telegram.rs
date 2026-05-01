use std::{
    collections::{BTreeMap, HashMap},
    fs,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    thread,
    time::{Duration, Instant},
};

use anyhow::{anyhow, Context, Result};
use crossbeam_channel::Sender;
use pulldown_cmark::{CodeBlockKind, Event, Options, Parser, Tag, TagEnd};
use reqwest::blocking::Client;
use serde::{Deserialize, Serialize};
use serde_json::json;
use stellaclaw_core::session_actor::{FileItem, FileState};
use time::{format_description::well_known::Rfc3339, OffsetDateTime};

use crate::{
    conversation::{
        parse_reasoning_control_argument, render_chat_message, ConversationControl,
        IncomingConversationMessage,
    },
    conversation_id_manager::ConversationIdManager,
    logger::StellaclawLogger,
};

use super::{
    types::{
        IncomingDispatch, IncomingMessageDispatch, OutgoingAttachment, OutgoingAttachmentKind,
        OutgoingDelivery, OutgoingError, OutgoingOptions, OutgoingProgressFeedback, OutgoingStatus,
        OutgoingUsageSummary, OutgoingUsageTotals, ProcessingState, ProgressFeedbackFinalState,
        TurnProgressPhase, TurnProgressPlanItemStatus,
    },
    Channel,
};

#[derive(Debug, Clone, Serialize, Deserialize)]
enum AuthorizationState {
    Pending,
    Approved,
    Rejected,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ChatAuthorization {
    state: AuthorizationState,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    last_title: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    last_user: Option<String>,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct SecurityState {
    #[serde(default)]
    admin_user_ids: Vec<i64>,
    #[serde(default)]
    chats: BTreeMap<String, ChatAuthorization>,
}

pub struct TelegramChannel {
    id: String,
    bot_token: String,
    api_base_url: String,
    poll_timeout_seconds: u64,
    poll_interval_ms: u64,
    client: Client,
    workdir: PathBuf,
    security_path: PathBuf,
    security: Mutex<SecurityState>,
    progress_messages: Mutex<HashMap<String, TelegramProgressMessage>>,
}

#[derive(Debug, Clone)]
struct TelegramProgressMessage {
    message_id: i64,
    last_text: String,
    last_update: Instant,
    started_at: Instant,
}

impl TelegramChannel {
    const MAX_MESSAGE_CHARS: usize = 4096;
    const MAX_CAPTION_CHARS: usize = 1024;
    const MIN_PROGRESS_EDIT_INTERVAL: Duration = Duration::from_secs(1);

    pub fn new(
        id: String,
        bot_token: String,
        api_base_url: String,
        poll_timeout_seconds: u64,
        poll_interval_ms: u64,
        admin_user_ids: Vec<i64>,
        workdir: &Path,
    ) -> Result<Self> {
        let dir = workdir
            .join(".stellaclaw")
            .join("channels")
            .join(&id);
        fs::create_dir_all(&dir).with_context(|| format!("failed to create {}", dir.display()))?;
        let security_path = dir.join("security.json");
        let security = if security_path.exists() {
            let raw = fs::read_to_string(&security_path)
                .with_context(|| format!("failed to read {}", security_path.display()))?;
            serde_json::from_str(&raw)
                .with_context(|| format!("failed to parse {}", security_path.display()))?
        } else {
            SecurityState::default()
        };
        let mut security = security;
        for admin_user_id in &admin_user_ids {
            if !security.admin_user_ids.contains(admin_user_id) {
                security.admin_user_ids.push(*admin_user_id);
            }
        }
        security.admin_user_ids.sort_unstable();
        security.admin_user_ids.dedup();

        let request_timeout = Duration::from_secs(poll_timeout_seconds.saturating_add(15).max(30));
        let client = Client::builder()
            .timeout(request_timeout)
            .connect_timeout(Duration::from_secs(10))
            .build()
            .context("failed to build telegram HTTP client")?;

        let instance = Self {
            id,
            bot_token,
            api_base_url: api_base_url.trim_end_matches('/').to_string(),
            poll_timeout_seconds,
            poll_interval_ms,
            client,
            workdir: workdir.to_path_buf(),
            security_path,
            security: Mutex::new(security),
            progress_messages: Mutex::new(HashMap::new()),
        };
        instance.save_security_state()?;
        Ok(instance)
    }

    fn progress_message_key(platform_chat_id: &str, turn_id: &str) -> String {
        format!("{platform_chat_id}:{}", turn_id)
    }

    fn run_loop(
        &self,
        dispatch_tx: Sender<IncomingDispatch>,
        id_manager: Arc<Mutex<ConversationIdManager>>,
        logger: Arc<StellaclawLogger>,
    ) -> Result<()> {
        let mut offset = 0_i64;
        loop {
            let payload = json!({
                "offset": offset,
                "timeout": self.poll_timeout_seconds,
                "allowed_updates": ["message", "callback_query"],
            });
            let updates: Vec<TelegramUpdate> = match self.call_api("getUpdates", &payload) {
                Ok(updates) => updates,
                Err(error) => {
                    logger.warn(
                        "telegram_poll_failed",
                        json!({"channel_id": self.id, "error": format!("{error:#}")}),
                    );
                    thread::sleep(Duration::from_millis(self.poll_interval_ms.max(1000)));
                    continue;
                }
            };
            for update in updates {
                offset = update.update_id.saturating_add(1);
                if let Some(message) = update.message {
                    if let Err(error) =
                        self.handle_message(message, &dispatch_tx, &id_manager, &logger)
                    {
                        logger.warn(
                            "telegram_update_failed",
                            json!({"channel_id": self.id, "update_id": update.update_id, "error": format!("{error:#}")}),
                        );
                    }
                } else if let Some(callback_query) = update.callback_query {
                    if let Err(error) = self.answer_callback_query(&callback_query.id) {
                        logger.warn(
                            "telegram_callback_query_ack_failed",
                            json!({
                                "channel_id": self.id,
                                "callback_query_id": callback_query.id,
                                "error": format!("{error:#}"),
                            }),
                        );
                    }
                    if let Err(error) = self.handle_callback_query(
                        callback_query,
                        &dispatch_tx,
                        &id_manager,
                        &logger,
                    ) {
                        logger.warn(
                            "telegram_update_failed",
                            json!({"channel_id": self.id, "update_id": update.update_id, "error": format!("{error:#}")}),
                        );
                    }
                }
            }
            thread::sleep(Duration::from_millis(self.poll_interval_ms));
        }
    }

    fn handle_message(
        &self,
        message: TelegramMessage,
        dispatch_tx: &Sender<IncomingDispatch>,
        id_manager: &Arc<Mutex<ConversationIdManager>>,
        logger: &Arc<StellaclawLogger>,
    ) -> Result<()> {
        let chat_id = message.chat.id.to_string();
        let from_user_id = message
            .from
            .as_ref()
            .map(|user| user.id)
            .unwrap_or_default();
        let text = message
            .text
            .clone()
            .or_else(|| message.caption.clone())
            .unwrap_or_default();
        self.bootstrap_first_private_admin(&message, from_user_id)?;

        if self.is_admin_private_chat(&message, from_user_id) && self.handle_admin_command(&text)? {
            return Ok(());
        }

        if !self.authorize_chat(&message, from_user_id, &text)? {
            return Ok(());
        }

        let conversation_id = id_manager
            .lock()
            .map_err(|_| anyhow!("conversation id manager lock poisoned"))?
            .get_or_create(&self.id, &chat_id)
            .map_err(anyhow::Error::msg)?;

        let control = parse_conversation_control(&text);
        let files = self.collect_incoming_files(&conversation_id, &message, logger)?;
        if control.is_none() && text.trim().is_empty() && files.is_empty() {
            return Ok(());
        }
        let incoming = IncomingDispatch::Message(IncomingMessageDispatch {
            channel_id: self.id.clone(),
            platform_chat_id: chat_id,
            conversation_id,
            message: IncomingConversationMessage {
                remote_message_id: message.message_id.to_string(),
                user_name: message.from.as_ref().map(render_user),
                message_time: message.date.and_then(render_message_time),
                text: if control.is_some() || text.trim().is_empty() {
                    None
                } else {
                    Some(text)
                },
                files,
                control,
            },
        });
        dispatch_tx
            .send(incoming)
            .map_err(|_| anyhow!("dispatcher channel closed"))
    }

    fn handle_callback_query(
        &self,
        callback_query: TelegramCallbackQuery,
        dispatch_tx: &Sender<IncomingDispatch>,
        id_manager: &Arc<Mutex<ConversationIdManager>>,
        logger: &Arc<StellaclawLogger>,
    ) -> Result<()> {
        let Some(message) = callback_query.message else {
            return Ok(());
        };
        let text = callback_query
            .data
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(ToOwned::to_owned)
            .unwrap_or_default();
        let chat_id = message.chat.id.to_string();
        let from_user_id = callback_query.from.id;
        self.bootstrap_first_private_admin(&message, from_user_id)?;

        if self.is_admin_private_chat(&message, from_user_id) && self.handle_admin_command(&text)? {
            return Ok(());
        }

        if !self.authorize_chat(&message, from_user_id, &text)? {
            return Ok(());
        }

        let conversation_id = id_manager
            .lock()
            .map_err(|_| anyhow!("conversation id manager lock poisoned"))?
            .get_or_create(&self.id, &chat_id)
            .map_err(anyhow::Error::msg)?;

        let control = parse_conversation_control(&text);
        let files = self.collect_incoming_files(&conversation_id, &message, logger)?;
        if control.is_none() && text.trim().is_empty() && files.is_empty() {
            return Ok(());
        }
        let incoming = IncomingDispatch::Message(IncomingMessageDispatch {
            channel_id: self.id.clone(),
            platform_chat_id: chat_id,
            conversation_id,
            message: IncomingConversationMessage {
                remote_message_id: format!("callback:{}", callback_query.id),
                user_name: Some(render_user(&callback_query.from)),
                message_time: message.date.and_then(render_message_time),
                text: if control.is_some() || text.trim().is_empty() {
                    None
                } else {
                    Some(text)
                },
                files,
                control,
            },
        });
        dispatch_tx
            .send(incoming)
            .map_err(|_| anyhow!("dispatcher channel closed"))
    }

    fn handle_admin_command(&self, text: &str) -> Result<bool> {
        let command = text.trim();
        if command == "/admin_chat_list" {
            let security = self
                .security
                .lock()
                .map_err(|_| anyhow!("telegram security lock poisoned"))?;
            let mut lines = vec![format!("channel `{}` 当前 chat 审批状态:", self.id)];
            for (chat_id, state) in &security.chats {
                lines.push(format!(
                    "- {}: {:?} {} {}",
                    chat_id,
                    state.state,
                    state.last_title.as_deref().unwrap_or(""),
                    state.last_user.as_deref().unwrap_or("")
                ));
            }
            drop(security);
            self.send_text_to_admins(&lines.join("\n"))?;
            return Ok(true);
        }
        if let Some(chat_id) = command.strip_prefix("/admin_chat_approve ").map(str::trim) {
            self.update_chat_state(chat_id, AuthorizationState::Approved)?;
            self.send_text(chat_id, "此 chat 已批准，可以开始使用。", None)?;
            self.send_text_to_admins(&format!("已批准 chat {chat_id}"))?;
            return Ok(true);
        }
        if let Some(chat_id) = command.strip_prefix("/admin_chat_reject ").map(str::trim) {
            self.update_chat_state(chat_id, AuthorizationState::Rejected)?;
            self.send_text(chat_id, "此 chat 已被拒绝。", None)?;
            self.send_text_to_admins(&format!("已拒绝 chat {chat_id}"))?;
            return Ok(true);
        }
        Ok(false)
    }

    fn authorize_chat(
        &self,
        message: &TelegramMessage,
        from_user_id: i64,
        text: &str,
    ) -> Result<bool> {
        if self.is_admin_private_chat(message, from_user_id) {
            return Ok(true);
        }
        let chat_id = message.chat.id.to_string();
        let mut security = self
            .security
            .lock()
            .map_err(|_| anyhow!("telegram security lock poisoned"))?;
        let entry = security
            .chats
            .entry(chat_id.clone())
            .or_insert(ChatAuthorization {
                state: AuthorizationState::Pending,
                last_title: message.chat.title.clone(),
                last_user: message.from.as_ref().map(render_user),
            });
        entry.last_title = message.chat.title.clone();
        entry.last_user = message.from.as_ref().map(render_user);
        let state = entry.state.clone();
        drop(security);
        self.save_security_state()?;
        match state {
            AuthorizationState::Approved => Ok(true),
            AuthorizationState::Rejected => {
                self.send_text(&chat_id, "当前 chat 未获批准，无法使用。", None)?;
                Ok(false)
            }
            AuthorizationState::Pending => {
                self.send_text(
                    &chat_id,
                    "当前 chat 正在等待管理员批准，请联系管理员处理。",
                    None,
                )?;
                self.send_text_to_admins(&format!(
                    "待审批 chat: {}\n标题: {}\n用户: {}\n最近消息: {}",
                    chat_id,
                    message.chat.title.as_deref().unwrap_or(""),
                    message.from.as_ref().map(render_user).unwrap_or_default(),
                    text
                ))?;
                Ok(false)
            }
        }
    }

    fn collect_incoming_files(
        &self,
        conversation_id: &str,
        message: &TelegramMessage,
        logger: &StellaclawLogger,
    ) -> Result<Vec<FileItem>> {
        let attachments = self.collect_attachment_descriptors(message);
        if attachments.is_empty() {
            return Ok(Vec::new());
        }
        let dir = self
            .workdir
            .join("conversations")
            .join(conversation_id)
            .join("attachments")
            .join("incoming");
        fs::create_dir_all(&dir).with_context(|| format!("failed to create {}", dir.display()))?;

        let mut files = Vec::with_capacity(attachments.len());
        for attachment in attachments {
            let target = dir.join(attachment.file_name());
            match self.download_attachment(&attachment, &target) {
                Ok(file) => files.push(file),
                Err(error) => {
                    logger.warn(
                        "telegram_attachment_download_failed",
                        json!({
                            "channel_id": self.id,
                            "conversation_id": conversation_id,
                            "file_id": attachment.file_id,
                            "target": target.display().to_string(),
                            "error": format!("{error:#}"),
                        }),
                    );
                    files.push(FileItem {
                        uri: format!("file://{}", target.display()),
                        name: target
                            .file_name()
                            .and_then(|value| value.to_str())
                            .map(ToOwned::to_owned),
                        media_type: attachment.media_type.clone(),
                        width: None,
                        height: None,
                        state: Some(FileState::Crashed {
                            reason: format!("{error:#}"),
                        }),
                    });
                }
            }
        }
        Ok(files)
    }

    fn collect_attachment_descriptors(&self, message: &TelegramMessage) -> Vec<TelegramAttachment> {
        let mut attachments = Vec::new();
        if let Some(photo) = message.photo.as_ref().and_then(|items| items.last()) {
            attachments.push(TelegramAttachment {
                kind: OutgoingAttachmentKind::Image,
                file_id: photo.file_id.clone(),
                file_unique_id: photo.file_unique_id.clone(),
                file_name: None,
                media_type: Some("image/jpeg".to_string()),
            });
        }
        if let Some(document) = &message.document {
            attachments.push(TelegramAttachment {
                kind: classify_document_kind(document),
                file_id: document.file_id.clone(),
                file_unique_id: document.file_unique_id.clone(),
                file_name: document.file_name.clone(),
                media_type: document.mime_type.clone(),
            });
        }
        if let Some(audio) = &message.audio {
            attachments.push(TelegramAttachment {
                kind: OutgoingAttachmentKind::Audio,
                file_id: audio.file_id.clone(),
                file_unique_id: audio.file_unique_id.clone(),
                file_name: audio.file_name.clone(),
                media_type: audio.mime_type.clone(),
            });
        }
        if let Some(voice) = &message.voice {
            attachments.push(TelegramAttachment {
                kind: OutgoingAttachmentKind::Voice,
                file_id: voice.file_id.clone(),
                file_unique_id: voice.file_unique_id.clone(),
                file_name: voice.file_name.clone(),
                media_type: voice.mime_type.clone(),
            });
        }
        if let Some(video) = &message.video {
            attachments.push(TelegramAttachment {
                kind: OutgoingAttachmentKind::Video,
                file_id: video.file_id.clone(),
                file_unique_id: video.file_unique_id.clone(),
                file_name: video.file_name.clone(),
                media_type: video.mime_type.clone(),
            });
        }
        if let Some(animation) = &message.animation {
            attachments.push(TelegramAttachment {
                kind: OutgoingAttachmentKind::Animation,
                file_id: animation.file_id.clone(),
                file_unique_id: animation.file_unique_id.clone(),
                file_name: animation.file_name.clone(),
                media_type: animation.mime_type.clone(),
            });
        }
        attachments
    }

    fn download_attachment(
        &self,
        attachment: &TelegramAttachment,
        target: &Path,
    ) -> Result<FileItem> {
        let metadata: TelegramFile = self.call_api(
            "getFile",
            &json!({
                "file_id": attachment.file_id,
            }),
        )?;
        let file_path = metadata
            .file_path
            .context("telegram getFile returned no file_path")?;
        let url = format!(
            "{}/file/bot{}/{}",
            self.api_base_url, self.bot_token, file_path
        );
        let bytes = self
            .client
            .get(url)
            .send()
            .context("telegram attachment download request failed")?
            .bytes()
            .context("failed to read telegram attachment bytes")?;
        fs::write(target, &bytes)
            .with_context(|| format!("failed to write attachment {}", target.display()))?;
        Ok(FileItem {
            uri: format!("file://{}", target.display()),
            name: target
                .file_name()
                .and_then(|value| value.to_str())
                .map(ToOwned::to_owned),
            media_type: attachment
                .media_type
                .clone()
                .or_else(|| infer_media_type(target)),
            width: None,
            height: None,
            state: None,
        })
    }

    fn answer_callback_query(&self, callback_query_id: &str) -> Result<()> {
        let _: serde_json::Value = self.call_api(
            "answerCallbackQuery",
            &json!({
                "callback_query_id": callback_query_id,
            }),
        )?;
        Ok(())
    }

    fn send_text(
        &self,
        platform_chat_id: &str,
        text: &str,
        options: Option<&OutgoingOptions>,
    ) -> Result<()> {
        let chat_id = platform_chat_id.to_string();
        let chunks = render_markdown_chunks_to_telegram_entities(text, Self::MAX_MESSAGE_CHARS);
        let last_index = chunks.len().saturating_sub(1);
        for (index, rendered) in chunks.into_iter().enumerate() {
            let payload = build_send_text_payload(
                &chat_id,
                rendered,
                (index == last_index).then_some(options).flatten(),
            )?;
            let _: serde_json::Value = self.call_api("sendMessage", &payload)?;
        }
        Ok(())
    }

    fn send_progress_text(&self, platform_chat_id: &str, text: &str) -> Result<i64> {
        let rendered = render_markdown_chunks_to_telegram_entities(text, Self::MAX_MESSAGE_CHARS)
            .into_iter()
            .next()
            .unwrap_or_else(|| TelegramRenderedText {
                text: text.to_string(),
                entities: Vec::new(),
            });
        let payload = build_send_text_payload(platform_chat_id, rendered, None)?;
        let message: TelegramMessage = self.call_api("sendMessage", &payload)?;
        Ok(message.message_id)
    }

    fn edit_progress_text(
        &self,
        platform_chat_id: &str,
        message_id: i64,
        text: &str,
    ) -> Result<()> {
        let rendered = render_markdown_chunks_to_telegram_entities(text, Self::MAX_MESSAGE_CHARS)
            .into_iter()
            .next()
            .unwrap_or_else(|| TelegramRenderedText {
                text: text.to_string(),
                entities: Vec::new(),
            });
        let payload = build_edit_text_payload(platform_chat_id, message_id, rendered)?;
        let _: serde_json::Value = self.call_api("editMessageText", &payload)?;
        Ok(())
    }

    fn send_attachment(
        &self,
        platform_chat_id: &str,
        attachment: &OutgoingAttachment,
        caption: Option<&str>,
    ) -> Result<()> {
        let chat_id = platform_chat_id.to_string();
        let bytes = fs::read(&attachment.path)
            .with_context(|| format!("failed to read {}", attachment.path.display()))?;
        let file_name = attachment
            .path
            .file_name()
            .and_then(|value| value.to_str())
            .unwrap_or("attachment.bin")
            .to_string();
        let part = reqwest::blocking::multipart::Part::bytes(bytes).file_name(file_name);
        let field = match attachment.kind {
            OutgoingAttachmentKind::Image => "photo",
            OutgoingAttachmentKind::Audio => "audio",
            OutgoingAttachmentKind::Voice => "voice",
            OutgoingAttachmentKind::Video => "video",
            OutgoingAttachmentKind::Animation => "animation",
            OutgoingAttachmentKind::Document => "document",
        };
        let method = match attachment.kind {
            OutgoingAttachmentKind::Image => "sendPhoto",
            OutgoingAttachmentKind::Audio => "sendAudio",
            OutgoingAttachmentKind::Voice => "sendVoice",
            OutgoingAttachmentKind::Video => "sendVideo",
            OutgoingAttachmentKind::Animation => "sendAnimation",
            OutgoingAttachmentKind::Document => "sendDocument",
        };
        let mut trailing_text_chunks = Vec::new();
        let form = reqwest::blocking::multipart::Form::new()
            .text("chat_id", chat_id.clone())
            .part(field.to_string(), part);
        let form = if let Some(caption) = caption.filter(|value| !value.trim().is_empty()) {
            let mut rendered =
                render_markdown_chunks_to_telegram_entities(caption, Self::MAX_CAPTION_CHARS)
                    .into_iter();
            let first = rendered.next().unwrap_or_else(|| TelegramRenderedText {
                text: String::new(),
                entities: Vec::new(),
            });
            trailing_text_chunks = rendered.collect();
            let mut form = form.text("caption", first.text);
            if !first.entities.is_empty() {
                form = form.text(
                    "caption_entities",
                    serde_json::to_string(&first.entities)
                        .context("failed to encode telegram caption entities")?,
                );
            }
            form
        } else {
            form
        };
        let response = self
            .client
            .post(self.method_url(method))
            .multipart(form)
            .send()
            .with_context(|| format!("telegram API call {method} failed"))?;
        let envelope = response
            .json::<TelegramEnvelope<serde_json::Value>>()
            .with_context(|| format!("telegram API {method} returned invalid JSON"))?;
        if !envelope.ok {
            return Err(anyhow!(
                "telegram API {} failed: {}",
                method,
                envelope
                    .description
                    .unwrap_or_else(|| "unknown".to_string())
            ));
        }
        for chunk in trailing_text_chunks {
            let payload = build_send_text_payload(&chat_id, chunk, None)?;
            let _: serde_json::Value = self.call_api("sendMessage", &payload)?;
        }
        Ok(())
    }

    fn save_security_state(&self) -> Result<()> {
        let security = self
            .security
            .lock()
            .map_err(|_| anyhow!("telegram security lock poisoned"))?;
        let raw = serde_json::to_string_pretty(&*security)
            .context("failed to serialize telegram security state")?;
        fs::write(&self.security_path, raw)
            .with_context(|| format!("failed to write {}", self.security_path.display()))
    }

    fn send_text_to_admins(&self, text: &str) -> Result<()> {
        for admin in self.effective_admin_user_ids()? {
            let _ = self.send_text(&admin.to_string(), text, None);
        }
        Ok(())
    }

    fn update_chat_state(&self, chat_id: &str, new_state: AuthorizationState) -> Result<()> {
        let mut security = self
            .security
            .lock()
            .map_err(|_| anyhow!("telegram security lock poisoned"))?;
        let entry = security
            .chats
            .entry(chat_id.to_string())
            .or_insert(ChatAuthorization {
                state: AuthorizationState::Pending,
                last_title: None,
                last_user: None,
            });
        entry.state = new_state;
        drop(security);
        self.save_security_state()
    }

    fn is_admin_private_chat(&self, message: &TelegramMessage, from_user_id: i64) -> bool {
        message.chat.chat_type == "private"
            && self
                .effective_admin_user_ids()
                .map(|admins| admins.contains(&from_user_id))
                .unwrap_or(false)
            && message.chat.id == from_user_id
    }

    fn effective_admin_user_ids(&self) -> Result<Vec<i64>> {
        let security = self
            .security
            .lock()
            .map_err(|_| anyhow!("telegram security lock poisoned"))?;
        let mut admin_user_ids = security.admin_user_ids.clone();
        admin_user_ids.sort_unstable();
        admin_user_ids.dedup();
        Ok(admin_user_ids)
    }

    fn bootstrap_first_private_admin(
        &self,
        message: &TelegramMessage,
        from_user_id: i64,
    ) -> Result<()> {
        if self.bootstrap_first_private_admin_in_memory(message, from_user_id)? {
            self.save_security_state()?;
            self.send_text(
                &from_user_id.to_string(),
                "已将你注册为此 Telegram channel 的管理员。",
                None,
            )?;
        }
        Ok(())
    }

    fn bootstrap_first_private_admin_in_memory(
        &self,
        message: &TelegramMessage,
        from_user_id: i64,
    ) -> Result<bool> {
        if from_user_id == 0
            || message.chat.chat_type != "private"
            || message.chat.id != from_user_id
        {
            return Ok(false);
        }

        let mut security = self
            .security
            .lock()
            .map_err(|_| anyhow!("telegram security lock poisoned"))?;
        if !security.admin_user_ids.is_empty() {
            return Ok(false);
        }
        security.admin_user_ids.push(from_user_id);
        security.chats.insert(
            message.chat.id.to_string(),
            ChatAuthorization {
                state: AuthorizationState::Approved,
                last_title: message.chat.title.clone(),
                last_user: message.from.as_ref().map(render_user),
            },
        );
        Ok(true)
    }

    fn call_api<T: serde::de::DeserializeOwned>(
        &self,
        method: &str,
        payload: &serde_json::Value,
    ) -> Result<T> {
        let response = self
            .client
            .post(self.method_url(method))
            .json(payload)
            .send()
            .with_context(|| format!("telegram API call {method} failed"))?;
        let envelope = response
            .json::<TelegramEnvelope<T>>()
            .with_context(|| format!("telegram API {method} returned invalid JSON"))?;
        if !envelope.ok {
            return Err(anyhow!(
                "telegram API {} failed: {}",
                method,
                envelope
                    .description
                    .unwrap_or_else(|| "unknown".to_string())
            ));
        }
        envelope
            .result
            .ok_or_else(|| anyhow!("telegram API {} returned no result", method))
    }

    fn method_url(&self, method: &str) -> String {
        format!("{}/bot{}/{}", self.api_base_url, self.bot_token, method)
    }
}

fn telegram_progress_text(feedback: &OutgoingProgressFeedback) -> String {
    let progress = &feedback.progress;
    let mut text = match progress.phase {
        TurnProgressPhase::Thinking => format!(
            "⚙️ 正在执行\n🤖 模型：{}\n🧠 状态：{}",
            progress.model, progress.activity
        ),
        TurnProgressPhase::Working => format!(
            "⚙️ 正在执行\n🤖 模型：{}\n📌 阶段：{}",
            progress.model, progress.activity
        ),
        TurnProgressPhase::Done => {
            format!("✅ 已完成\n🤖 模型：{}", progress.model)
        }
        TurnProgressPhase::Failed => {
            let mut text = format!("❌ 本轮失败\n🤖 模型：{}", progress.model);
            if let Some(error) = progress
                .error
                .as_deref()
                .map(str::trim)
                .filter(|value| !value.is_empty())
            {
                text.push_str("\n📌 ");
                text.push_str(error);
            }
            text
        }
    };
    if let Some(hint) = progress
        .hint
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        text.push_str("\n\n💡 ");
        text.push_str(hint);
    }
    append_telegram_progress_plan(&mut text, feedback);
    text
}

fn append_telegram_progress_plan(text: &mut String, feedback: &OutgoingProgressFeedback) {
    let Some(plan) = &feedback.progress.plan else {
        return;
    };
    if plan.explanation.is_none() && plan.items.is_empty() {
        return;
    }
    text.push_str("\n\n计划");
    if let Some(explanation) = plan
        .explanation
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        text.push('\n');
        text.push_str(explanation);
    }
    for item in &plan.items {
        let step = item.step.trim();
        if step.is_empty() {
            continue;
        }
        text.push('\n');
        text.push_str(telegram_progress_plan_status_marker(item.status));
        text.push(' ');
        text.push_str(step);
    }
}

fn telegram_progress_plan_status_marker(status: TurnProgressPlanItemStatus) -> &'static str {
    match status {
        TurnProgressPlanItemStatus::Pending => "☐",
        TurnProgressPlanItemStatus::InProgress => "◐",
        TurnProgressPlanItemStatus::Completed => "☑",
    }
}

impl Channel for TelegramChannel {
    fn id(&self) -> &str {
        &self.id
    }

    fn set_processing(&self, platform_chat_id: &str, state: ProcessingState) -> Result<()> {
        if state == ProcessingState::Typing {
            let _: serde_json::Value = self.call_api(
                "sendChatAction",
                &json!({
                    "chat_id": platform_chat_id,
                    "action": "typing",
                }),
            )?;
        }
        Ok(())
    }

    fn update_progress_feedback(&self, feedback: &OutgoingProgressFeedback) -> Result<()> {
        let key = Self::progress_message_key(&feedback.platform_chat_id, &feedback.turn_id);
        let text = telegram_progress_text(feedback);
        if feedback.final_state == Some(ProgressFeedbackFinalState::Done) {
            let existing = self.progress_messages.lock().unwrap().remove(&key);
            if let Some(existing) = existing {
                let elapsed = Instant::now().duration_since(existing.started_at);
                let summary = format!("{}\n⏱️ 用时：{}", text, format_duration(elapsed));
                if let Err(error) = self.edit_progress_text(
                    &feedback.platform_chat_id,
                    existing.message_id,
                    &summary,
                ) {
                    eprintln!("telegram progress completion edit failed: {error:#}");
                }
            }
            return Ok(());
        }

        let now = Instant::now();
        let is_final = feedback.final_state.is_some();
        let existing = self.progress_messages.lock().unwrap().get(&key).cloned();
        let Some(existing) = existing else {
            let message_id = self.send_progress_text(&feedback.platform_chat_id, &text)?;
            if is_final {
                return Ok(());
            }
            self.progress_messages.lock().unwrap().insert(
                key,
                TelegramProgressMessage {
                    message_id,
                    last_text: text,
                    last_update: now,
                    started_at: now,
                },
            );
            return Ok(());
        };

        if existing.last_text == text && !is_final {
            return Ok(());
        }
        if !feedback.important
            && !is_final
            && now.duration_since(existing.last_update) < Self::MIN_PROGRESS_EDIT_INTERVAL
        {
            return Ok(());
        }

        if let Err(error) =
            self.edit_progress_text(&feedback.platform_chat_id, existing.message_id, &text)
        {
            eprintln!("telegram progress edit failed: {error:#}");
            if is_final {
                self.progress_messages.lock().unwrap().remove(&key);
            }
            return Ok(());
        }

        if is_final {
            self.progress_messages.lock().unwrap().remove(&key);
        } else {
            self.progress_messages.lock().unwrap().insert(
                key,
                TelegramProgressMessage {
                    message_id: existing.message_id,
                    last_text: text,
                    last_update: now,
                    started_at: existing.started_at,
                },
            );
        }
        Ok(())
    }

    fn send_delivery(&self, delivery: &OutgoingDelivery) -> Result<()> {
        let options = delivery.options.as_ref();
        let rendered_message = delivery.message.as_ref().map(render_chat_message);
        let text = if !delivery.text.trim().is_empty() {
            delivery.text.as_str()
        } else if let Some(rendered) = rendered_message
            .as_deref()
            .filter(|text| !text.trim().is_empty())
        {
            rendered
        } else if let Some(options) = options {
            options.prompt.as_str()
        } else {
            ""
        };
        if !text.trim().is_empty() && (delivery.attachments.is_empty() || options.is_some()) {
            self.send_text(&delivery.platform_chat_id, text, options)?;
        }
        let mut attachment_caption = (!text.trim().is_empty() && options.is_none()).then_some(text);
        for attachment in &delivery.attachments {
            self.send_attachment(
                &delivery.platform_chat_id,
                attachment,
                attachment_caption.take(),
            )?;
        }
        Ok(())
    }

    fn send_status(&self, status: &OutgoingStatus) -> Result<()> {
        self.send_text(&status.platform_chat_id, &render_status_text(status), None)
    }

    fn send_error(&self, error: &OutgoingError) -> Result<()> {
        let mut text = error.message.clone();
        if let Some(action) = error
            .suggested_action
            .as_deref()
            .filter(|action| !action.trim().is_empty())
        {
            text.push('\n');
            text.push_str(action);
        }
        self.send_text(&error.platform_chat_id, &text, None)
    }

    fn spawn_ingress(
        self: Arc<Self>,
        dispatch_tx: Sender<IncomingDispatch>,
        id_manager: Arc<Mutex<ConversationIdManager>>,
        logger: Arc<StellaclawLogger>,
    ) where
        Self: Sized,
    {
        thread::spawn(move || {
            if let Err(error) = self.run_loop(dispatch_tx, id_manager, logger.clone()) {
                logger.error(
                    "telegram_channel_failed",
                    json!({"channel_id": self.id, "error": format!("{error:#}")}),
                );
            }
        });
    }
}

#[derive(Debug, Clone)]
struct TelegramAttachment {
    kind: OutgoingAttachmentKind,
    file_id: String,
    file_unique_id: String,
    file_name: Option<String>,
    media_type: Option<String>,
}

impl TelegramAttachment {
    fn file_name(&self) -> String {
        if let Some(name) = self
            .file_name
            .as_deref()
            .filter(|name| !name.trim().is_empty())
        {
            return sanitize_file_name(name);
        }
        let ext = infer_extension(self.media_type.as_deref(), self.kind);
        format!(
            "telegram-{}-{}.{}",
            kind_label(self.kind),
            self.file_unique_id,
            ext
        )
    }
}

#[derive(Debug, Deserialize)]
struct TelegramEnvelope<T> {
    ok: bool,
    result: Option<T>,
    #[serde(default)]
    description: Option<String>,
}

#[derive(Debug, Deserialize)]
struct TelegramUpdate {
    update_id: i64,
    #[serde(default)]
    message: Option<TelegramMessage>,
    #[serde(default)]
    callback_query: Option<TelegramCallbackQuery>,
}

#[derive(Debug, Deserialize)]
struct TelegramCallbackQuery {
    id: String,
    from: TelegramUser,
    #[serde(default)]
    message: Option<TelegramMessage>,
    #[serde(default)]
    data: Option<String>,
}

#[derive(Debug, Deserialize)]
struct TelegramMessage {
    message_id: i64,
    #[serde(default)]
    date: Option<i64>,
    chat: TelegramChat,
    #[serde(default)]
    from: Option<TelegramUser>,
    #[serde(default)]
    text: Option<String>,
    #[serde(default)]
    caption: Option<String>,
    #[serde(default)]
    photo: Option<Vec<TelegramPhotoSize>>,
    #[serde(default)]
    document: Option<TelegramMedia>,
    #[serde(default)]
    audio: Option<TelegramMedia>,
    #[serde(default)]
    voice: Option<TelegramMedia>,
    #[serde(default)]
    video: Option<TelegramMedia>,
    #[serde(default)]
    animation: Option<TelegramMedia>,
}

#[derive(Debug, Deserialize)]
struct TelegramChat {
    id: i64,
    #[serde(rename = "type")]
    chat_type: String,
    #[serde(default)]
    title: Option<String>,
}

#[derive(Debug, Deserialize)]
struct TelegramUser {
    id: i64,
    first_name: String,
    #[serde(default)]
    last_name: Option<String>,
    #[serde(default)]
    username: Option<String>,
}

#[derive(Debug, Deserialize)]
struct TelegramPhotoSize {
    file_id: String,
    file_unique_id: String,
}

#[derive(Debug, Deserialize)]
struct TelegramMedia {
    file_id: String,
    file_unique_id: String,
    #[serde(default)]
    file_name: Option<String>,
    #[serde(default)]
    mime_type: Option<String>,
}

#[derive(Debug, Deserialize)]
struct TelegramFile {
    #[serde(default)]
    file_path: Option<String>,
}

fn classify_document_kind(document: &TelegramMedia) -> OutgoingAttachmentKind {
    match document.mime_type.as_deref() {
        Some(value) if value.starts_with("image/") => OutgoingAttachmentKind::Image,
        Some(value) if value.starts_with("audio/") => OutgoingAttachmentKind::Audio,
        Some(value) if value.starts_with("video/") => OutgoingAttachmentKind::Video,
        _ => OutgoingAttachmentKind::Document,
    }
}

fn render_user(user: &TelegramUser) -> String {
    user.username.clone().unwrap_or_else(|| {
        format!(
            "{} {}",
            user.first_name,
            user.last_name.clone().unwrap_or_default()
        )
        .trim()
        .to_string()
    })
}

fn render_message_time(unix_timestamp: i64) -> Option<String> {
    OffsetDateTime::from_unix_timestamp(unix_timestamp)
        .ok()?
        .format(&Rfc3339)
        .ok()
}

fn render_status_text(status: &OutgoingStatus) -> String {
    format!(
        "当前状态\nconversation: `{}`\nmodel: `{}`\nreasoning: `{}`\nsandbox: `{}` ({})\nremote: {}\nworkspace: `{}`\nbackground: {} running / {} total\nsubagents: {} running / {} total\n\n{}",
        status.conversation_id,
        status.model,
        status.reasoning,
        status.sandbox,
        status.sandbox_source,
        status.remote,
        status.workspace,
        status.running_background,
        status.total_background,
        status.running_subagents,
        status.total_subagents,
        render_usage_summary(&status.usage),
    )
}

fn render_usage_summary(summary: &OutgoingUsageSummary) -> String {
    let mut total = OutgoingUsageTotals::default();
    add_usage_totals(&mut total, &summary.foreground);
    add_usage_totals(&mut total, &summary.background);
    add_usage_totals(&mut total, &summary.subagents);
    add_usage_totals(&mut total, &summary.media_tools);

    format!(
        "token usage\n{}\n{}\n{}\n{}\n{}",
        render_usage_line("total", &total),
        render_usage_line("foreground", &summary.foreground),
        render_usage_line("background", &summary.background),
        render_usage_line("subagents", &summary.subagents),
        render_usage_line("media tools", &summary.media_tools),
    )
}

fn render_usage_line(label: &str, totals: &OutgoingUsageTotals) -> String {
    format!(
        "- {label}: read {} (${:.3}), write {} (${:.3}), input {} (${:.3}), output {} (${:.3}), total ${:.3}",
        totals.cache_read,
        totals.cost.cache_read,
        totals.cache_write,
        totals.cost.cache_write,
        totals.uncache_input,
        totals.cost.uncache_input,
        totals.output,
        totals.cost.output,
        totals.cost.cache_read + totals.cost.cache_write + totals.cost.uncache_input + totals.cost.output,
    )
}

fn add_usage_totals(target: &mut OutgoingUsageTotals, source: &OutgoingUsageTotals) {
    target.cache_read = target.cache_read.saturating_add(source.cache_read);
    target.cache_write = target.cache_write.saturating_add(source.cache_write);
    target.uncache_input = target.uncache_input.saturating_add(source.uncache_input);
    target.output = target.output.saturating_add(source.output);
    target.cost.cache_read += source.cost.cache_read;
    target.cost.cache_write += source.cost.cache_write;
    target.cost.uncache_input += source.cost.uncache_input;
    target.cost.output += source.cost.output;
}

fn parse_conversation_control(text: &str) -> Option<ConversationControl> {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return None;
    }

    let mut parts = trimmed.splitn(2, char::is_whitespace);
    let first = parts.next()?;
    let argument = parts.next().map(str::trim).unwrap_or("");
    let command = first.split_once('@').map_or(first, |(base, _)| base);

    match command {
        "/continue" if argument.is_empty() => Some(ConversationControl::Continue),
        "/cancel" if argument.is_empty() => Some(ConversationControl::Cancel),
        "/compact" if argument.is_empty() => Some(ConversationControl::Compact),
        "/status" if argument.is_empty() => Some(ConversationControl::ShowStatus),
        "/model" if argument.is_empty() => Some(ConversationControl::ShowModel),
        "/model" => Some(ConversationControl::SwitchModel {
            model_name: argument.to_string(),
        }),
        "/reasoning" => Some(parse_reasoning_control_argument(argument)),
        "/remote" if argument.is_empty() => Some(ConversationControl::ShowRemote),
        "/remote" if argument.eq_ignore_ascii_case("off") => {
            Some(ConversationControl::DisableRemote)
        }
        "/remote" => parse_remote_control(argument),
        "/sandbox" if argument.is_empty() => Some(ConversationControl::ShowSandbox),
        "/sandbox" => parse_sandbox_control(argument),
        _ => None,
    }
}

fn parse_remote_control(argument: &str) -> Option<ConversationControl> {
    let mut parts = argument.trim().splitn(2, char::is_whitespace);
    let host = parts.next().unwrap_or_default().trim();
    let path = parts.next().map(str::trim).unwrap_or_default();
    if host.is_empty() || path.is_empty() {
        return Some(ConversationControl::InvalidRemote {
            reason: "remote 命令缺少 host 或 path。".to_string(),
        });
    }
    Some(ConversationControl::SetRemote {
        host: host.to_string(),
        path: path.to_string(),
    })
}

fn parse_sandbox_control(argument: &str) -> Option<ConversationControl> {
    let argument = argument.trim();
    if argument.eq_ignore_ascii_case("default") || argument.eq_ignore_ascii_case("global") {
        return Some(ConversationControl::SetSandbox { mode: None });
    }
    if argument.eq_ignore_ascii_case("subprocess")
        || argument.eq_ignore_ascii_case("off")
        || argument.eq_ignore_ascii_case("none")
        || argument.eq_ignore_ascii_case("disabled")
    {
        return Some(ConversationControl::SetSandbox {
            mode: Some(crate::config::SandboxMode::Subprocess),
        });
    }
    if argument.eq_ignore_ascii_case("bubblewrap") || argument.eq_ignore_ascii_case("bwrap") {
        return Some(ConversationControl::SetSandbox {
            mode: Some(crate::config::SandboxMode::Bubblewrap),
        });
    }
    Some(ConversationControl::InvalidSandbox {
        reason: format!("未知 sandbox 模式 `{argument}`。"),
    })
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

fn build_inline_keyboard_markup(options: &OutgoingOptions) -> serde_json::Value {
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
    options: Option<&OutgoingOptions>,
) -> Result<serde_json::Value> {
    let mut payload = json!({
        "chat_id": chat_id,
        "text": rendered.text,
        "disable_web_page_preview": true,
    });
    if !rendered.entities.is_empty() {
        if let Some(object) = payload.as_object_mut() {
            object.insert(
                "entities".to_string(),
                serde_json::to_value(rendered.entities)
                    .context("failed to encode telegram entities")?,
            );
        }
    }
    if let Some(options) = options.filter(|value| !value.options.is_empty()) {
        if let Some(object) = payload.as_object_mut() {
            object.insert(
                "reply_markup".to_string(),
                build_inline_keyboard_markup(options),
            );
        }
    }
    Ok(payload)
}

fn build_edit_text_payload(
    chat_id: &str,
    message_id: i64,
    rendered: TelegramRenderedText,
) -> Result<serde_json::Value> {
    let mut payload = json!({
        "chat_id": chat_id,
        "message_id": message_id,
        "text": rendered.text,
        "disable_web_page_preview": true,
    });
    if !rendered.entities.is_empty() {
        if let Some(object) = payload.as_object_mut() {
            object.insert(
                "entities".to_string(),
                serde_json::to_value(rendered.entities)
                    .context("failed to encode telegram entities")?,
            );
        }
    }
    Ok(payload)
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
                    if let Some(table) = pending_table.as_mut() {
                        if !table.current_row.is_empty() {
                            table.rows.push(std::mem::take(&mut table.current_row));
                        }
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
                    if let Some(BlockContainer::ListItem(blocks)) = block_stack.pop() {
                        if let Some(BlockContainer::List { items, .. }) = block_stack.last_mut() {
                            items.push(blocks);
                        }
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

fn render_markdown_chunks_to_telegram_entities(
    input: &str,
    max_chars: usize,
) -> Vec<TelegramRenderedText> {
    split_markdown_for_telegram_documents(input, max_chars)
        .into_iter()
        .map(|document| render_rich_document_to_telegram_entities(&document))
        .collect()
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
                let collapse = quote_depth == 0 && should_collapse_code_block(code);
                let outer_start = builder.cursor();
                let start = builder.cursor();
                builder.push_text(code);
                builder.push_entity_trimmed(start, "pre", None, language.clone());
                if collapse {
                    builder.push_entity_trimmed(outer_start, "expandable_blockquote", None, None);
                }
                *need_paragraph_break = true;
            }
            RichBlock::Table(table) => {
                ensure_block_break_text(&mut builder.text, need_paragraph_break);
                maybe_render_nested_quote_prefix(builder, quote_depth);
                let table_text = render_table_text(table);
                let collapse = quote_depth == 0
                    && (table_text.lines().count() >= 15 || table_text.chars().count() >= 600);
                let outer_start = builder.cursor();
                let start = builder.cursor();
                builder.push_text(&table_text);
                builder.push_entity_trimmed(start, "pre", None, None);
                if collapse {
                    builder.push_entity_trimmed(outer_start, "expandable_blockquote", None, None);
                }
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

fn should_collapse_code_block(code: &str) -> bool {
    let line_count = code.lines().count();
    let char_count = code.chars().count();
    line_count >= 15 || char_count >= 600
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
        RichInline::Text(text) => {
            split_leaf_inline_text_to_fit(block_kind, text, max_chars, |value| {
                RichInline::Text(value)
            })
        }
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
    .map(|parts| parts.into_iter().map(make_inline).collect::<Vec<_>>())
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
        if end < chars.len() {
            if let Some(adjusted) = prefer_split_boundary_with_measure(
                &chars[cursor..end],
                best / 2,
                &measure,
                max_chars,
            ) {
                end = cursor + adjusted;
            }
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
            let translated_len = telegram_text_len(
                &render_rich_document_to_telegram_entities(&parse_markdown_to_rich_document(
                    &candidate,
                ))
                .text,
            );
            if translated_len <= max_chars {
                best = mid;
                low = mid + 1;
            } else {
                high = mid.saturating_sub(1);
            }
        }

        let mut end = cursor + best;
        if end < chars.len() {
            if let Some(adjusted) = prefer_split_boundary(&chars[cursor..end], best / 2) {
                end = cursor + adjusted;
            }
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

fn ensure_block_break_text(output: &mut String, need_paragraph_break: &mut bool) {
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

fn utf16_len(value: &str) -> usize {
    value.encode_utf16().count()
}

fn telegram_text_len(value: &str) -> usize {
    utf16_len(value)
}

fn sanitize_file_name(name: &str) -> String {
    let mut sanitized = name
        .chars()
        .map(|ch| match ch {
            '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|' => '_',
            _ => ch,
        })
        .collect::<String>();
    if sanitized.is_empty() {
        sanitized = "attachment.bin".to_string();
    }
    sanitized
}

fn infer_extension(media_type: Option<&str>, kind: OutgoingAttachmentKind) -> &'static str {
    match (media_type.unwrap_or_default(), kind) {
        ("image/png", _) => "png",
        ("image/webp", _) => "webp",
        ("image/gif", _) => "gif",
        ("audio/mpeg", _) => "mp3",
        ("audio/ogg", _) => "ogg",
        ("audio/wav", _) => "wav",
        ("video/mp4", _) => "mp4",
        ("application/pdf", _) => "pdf",
        (_, OutgoingAttachmentKind::Image) => "jpg",
        (_, OutgoingAttachmentKind::Audio) => "mp3",
        (_, OutgoingAttachmentKind::Voice) => "ogg",
        (_, OutgoingAttachmentKind::Video) => "mp4",
        (_, OutgoingAttachmentKind::Animation) => "gif",
        (_, OutgoingAttachmentKind::Document) => "bin",
    }
}

fn infer_media_type(path: &Path) -> Option<String> {
    match path
        .extension()
        .and_then(|value| value.to_str())?
        .to_ascii_lowercase()
        .as_str()
    {
        "png" => Some("image/png".to_string()),
        "jpg" | "jpeg" => Some("image/jpeg".to_string()),
        "webp" => Some("image/webp".to_string()),
        "gif" => Some("image/gif".to_string()),
        "pdf" => Some("application/pdf".to_string()),
        "mp3" => Some("audio/mpeg".to_string()),
        "ogg" => Some("audio/ogg".to_string()),
        "wav" => Some("audio/wav".to_string()),
        "mp4" => Some("video/mp4".to_string()),
        _ => None,
    }
}

fn format_duration(duration: Duration) -> String {
    let total_secs = duration.as_secs();
    let minutes = total_secs / 60;
    let seconds = total_secs % 60;
    if minutes > 0 {
        format!("{minutes}m{seconds:02}s")
    } else {
        format!("{seconds}s")
    }
}

fn kind_label(kind: OutgoingAttachmentKind) -> &'static str {
    match kind {
        OutgoingAttachmentKind::Image => "image",
        OutgoingAttachmentKind::Audio => "audio",
        OutgoingAttachmentKind::Voice => "voice",
        OutgoingAttachmentKind::Video => "video",
        OutgoingAttachmentKind::Animation => "animation",
        OutgoingAttachmentKind::Document => "document",
    }
}

#[cfg(test)]
mod tests {
    use super::{
        build_send_text_payload, parse_conversation_control, parse_markdown_to_rich_document,
        render_markdown_chunks_to_telegram_entities, render_rich_document_to_telegram_entities,
        telegram_text_len, ChatAuthorization, SecurityState, TelegramChannel, TelegramChat,
        TelegramMessage, TelegramMessageEntity, TelegramRenderedText, TelegramUser,
    };
    use crate::channels::types::{OutgoingOption, OutgoingOptions};
    use crate::config::SandboxMode;
    use crate::conversation::ConversationControl;
    use std::{
        collections::{BTreeMap, HashMap},
        path::PathBuf,
        sync::Mutex,
    };

    #[test]
    fn parses_model_control_commands() {
        assert!(matches!(
            parse_conversation_control("/model"),
            Some(ConversationControl::ShowModel)
        ));
        assert!(matches!(
            parse_conversation_control("/model gpt54"),
            Some(ConversationControl::SwitchModel { model_name }) if model_name == "gpt54"
        ));
        assert!(matches!(
            parse_conversation_control("/model@stellaclaw_bot gpt54"),
            Some(ConversationControl::SwitchModel { model_name }) if model_name == "gpt54"
        ));
        assert!(matches!(
            parse_conversation_control("/remote"),
            Some(ConversationControl::ShowRemote)
        ));
        assert!(matches!(
            parse_conversation_control("/remote demo-host ~/repo"),
            Some(ConversationControl::SetRemote { host, path }) if host == "demo-host" && path == "~/repo"
        ));
        assert!(matches!(
            parse_conversation_control("/remote off"),
            Some(ConversationControl::DisableRemote)
        ));
        assert!(matches!(
            parse_conversation_control("/status"),
            Some(ConversationControl::ShowStatus)
        ));
        assert!(matches!(
            parse_conversation_control("/compact"),
            Some(ConversationControl::Compact)
        ));
        assert!(matches!(
            parse_conversation_control("/reasoning"),
            Some(ConversationControl::ShowReasoning)
        ));
        assert!(matches!(
            parse_conversation_control("/reasoning high"),
            Some(ConversationControl::SetReasoning { effort: Some(effort) }) if effort == "high"
        ));
        assert!(matches!(
            parse_conversation_control("/reasoning default"),
            Some(ConversationControl::SetReasoning { effort: None })
        ));
        assert!(matches!(
            parse_conversation_control("/sandbox"),
            Some(ConversationControl::ShowSandbox)
        ));
        assert!(matches!(
            parse_conversation_control("/sandbox bubblewrap"),
            Some(ConversationControl::SetSandbox {
                mode: Some(SandboxMode::Bubblewrap)
            })
        ));
        assert!(matches!(
            parse_conversation_control("/sandbox subprocess"),
            Some(ConversationControl::SetSandbox {
                mode: Some(SandboxMode::Subprocess)
            })
        ));
        assert!(matches!(
            parse_conversation_control("/sandbox default"),
            Some(ConversationControl::SetSandbox { mode: None })
        ));
    }

    #[test]
    fn first_private_user_bootstraps_as_admin_in_security_state() {
        let channel = TelegramChannel {
            id: "telegram-main".to_string(),
            bot_token: "token".to_string(),
            api_base_url: "https://api.telegram.org".to_string(),
            poll_timeout_seconds: 30,
            poll_interval_ms: 250,
            client: reqwest::blocking::Client::new(),
            workdir: PathBuf::from("."),
            security_path: PathBuf::from("/tmp/unused-security.json"),
            security: Mutex::new(SecurityState {
                admin_user_ids: Vec::new(),
                chats: BTreeMap::<String, ChatAuthorization>::new(),
            }),
            progress_messages: Mutex::new(HashMap::new()),
        };
        let message = TelegramMessage {
            message_id: 1,
            date: None,
            chat: TelegramChat {
                id: 42,
                chat_type: "private".to_string(),
                title: None,
            },
            from: Some(TelegramUser {
                id: 42,
                first_name: "Alice".to_string(),
                last_name: None,
                username: Some("alice".to_string()),
            }),
            text: Some("/start".to_string()),
            caption: None,
            photo: None,
            document: None,
            audio: None,
            voice: None,
            video: None,
            animation: None,
        };

        let bootstrapped = channel
            .bootstrap_first_private_admin_in_memory(&message, 42)
            .expect("bootstrap should work");

        assert!(bootstrapped);
        assert_eq!(channel.effective_admin_user_ids().unwrap(), vec![42]);
        assert!(channel.is_admin_private_chat(&message, 42));
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
        assert!(rendered
            .entities
            .iter()
            .any(|entity| entity.kind == "italic"));
        assert!(rendered
            .entities
            .iter()
            .any(|entity| entity.kind == "text_link"));
        assert!(rendered
            .entities
            .iter()
            .any(|entity| entity.kind == "pre" && entity.language.as_deref() == Some("rust")));
    }

    #[test]
    fn renders_blockquote_entities_for_telegram() {
        let document = parse_markdown_to_rich_document("> quoted line\n>\n> second line");
        let rendered = render_rich_document_to_telegram_entities(&document);

        assert!(rendered.text.contains("quoted line"));
        assert!(rendered.text.contains("second line"));
        assert!(rendered
            .entities
            .iter()
            .any(|entity| entity.kind == "blockquote"));
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
            Some(&OutgoingOptions {
                prompt: "Choose".to_string(),
                options: vec![
                    OutgoingOption {
                        label: "One".to_string(),
                        value: "/one".to_string(),
                    },
                    OutgoingOption {
                        label: "Two".to_string(),
                        value: "/two".to_string(),
                    },
                    OutgoingOption {
                        label: "Three".to_string(),
                        value: "/three".to_string(),
                    },
                ],
            }),
        )
        .unwrap();

        assert_eq!(payload["chat_id"], "123");
        assert_eq!(payload["text"], "hello");
        assert_eq!(payload["entities"][0]["type"], "bold");
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
        assert_eq!(
            payload["reply_markup"]["inline_keyboard"][0][0]["text"],
            "One"
        );
        assert_eq!(
            payload["reply_markup"]["inline_keyboard"][0][0]["callback_data"],
            "/one"
        );
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
        assert!(chunks
            .iter()
            .all(|chunk| telegram_text_len(&chunk.text) <= 4096));
    }

    #[test]
    fn splits_large_code_block_into_multiple_pre_blocks() {
        let input = format!("```rust\n{}\n```", "let x = 42;\n".repeat(300));
        let chunks = render_markdown_chunks_to_telegram_entities(&input, 1024);

        assert!(chunks.len() >= 2);
        assert!(chunks
            .iter()
            .all(|chunk| telegram_text_len(&chunk.text) <= 1024));
        assert!(chunks
            .iter()
            .all(|chunk| chunk.entities.iter().any(|entity| entity.kind == "pre")));
    }
}
