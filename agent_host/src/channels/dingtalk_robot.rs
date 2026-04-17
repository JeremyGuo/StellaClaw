use crate::channel::{
    AttachmentSource, Channel, ConversationProbe, IncomingMessage, PendingAttachment,
    ProgressFeedback, ProgressFeedbackUpdate,
};
use crate::config::DingtalkRobotChannelConfig;
use crate::domain::{
    AttachmentKind, ChannelAddress, OutgoingAttachment, OutgoingMessage, ProcessingState,
};
use anyhow::{Context, Result, anyhow, bail};
use async_trait::async_trait;
use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64_STANDARD};
use hmac::{Hmac, Mac};
use reqwest::Client;
use serde::Deserialize;
use serde_json::json;
use sha2::Sha256;
use std::collections::HashMap;
use std::future::pending;
use std::path::Path;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::fs;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{Mutex, mpsc};
use tracing::{info, warn};
use uuid::Uuid;

const MAX_HTTP_BODY_BYTES: usize = 1024 * 1024;
const MAX_SIGNATURE_SKEW_MS: i64 = 60 * 60 * 1000;

type HmacSha256 = Hmac<Sha256>;

pub struct DingtalkRobotChannel {
    id: String,
    webhook_url: String,
    app_key: Option<String>,
    app_secret: Option<String>,
    api_base_url: String,
    http_listen_addr: String,
    http_callback_path: String,
    client: Client,
    session_routes: Mutex<HashMap<String, SessionRoute>>,
    access_token: Arc<Mutex<Option<CachedAccessToken>>>,
}

#[derive(Clone, Debug)]
struct SessionRoute {
    session_webhook: String,
    session_webhook_expires_at_ms: Option<i64>,
}

#[derive(Clone, Debug)]
struct CachedAccessToken {
    token: String,
    expires_at_ms: i64,
}

#[derive(Clone, Debug, Deserialize)]
struct DingtalkRobotCallbackData {
    #[serde(rename = "conversationId", default)]
    conversation_id: String,
    #[serde(rename = "conversationType", default)]
    conversation_type: String,
    #[serde(rename = "senderStaffId", default)]
    sender_staff_id: Option<String>,
    #[serde(rename = "senderId", default)]
    sender_id: Option<String>,
    #[serde(rename = "senderNick", default)]
    sender_nick: Option<String>,
    #[serde(rename = "robotCode", default)]
    robot_code: Option<String>,
    #[serde(rename = "msgId", default)]
    msg_id: Option<String>,
    #[serde(rename = "msgtype", default)]
    msg_type: String,
    #[serde(rename = "createAt", default)]
    create_at: Option<String>,
    #[serde(rename = "sessionWebhook", default)]
    session_webhook: Option<String>,
    #[serde(rename = "sessionWebhookExpiredTime", default)]
    session_webhook_expired_time: Option<i64>,
    #[serde(default)]
    text: Option<DingtalkTextContent>,
    #[serde(default)]
    content: Option<DingtalkContent>,
}

#[derive(Clone, Debug, Deserialize)]
struct DingtalkTextContent {
    content: String,
}

#[derive(Clone, Debug, Deserialize)]
struct DingtalkContent {
    #[serde(default)]
    recognition: Option<String>,
    #[serde(rename = "fileName", default)]
    file_name: Option<String>,
    #[serde(rename = "downloadCode", default)]
    download_code: Option<String>,
    #[serde(rename = "videoType", default)]
    video_type: Option<String>,
    #[serde(rename = "unknownMsgType", default)]
    unknown_msg_type: Option<String>,
    #[serde(rename = "richText", default)]
    rich_text: Vec<DingtalkRichTextItem>,
}

#[derive(Clone, Debug, Deserialize)]
struct DingtalkRichTextItem {
    #[serde(default)]
    text: Option<String>,
    #[serde(rename = "type", default)]
    item_type: Option<String>,
    #[serde(rename = "downloadCode", default)]
    download_code: Option<String>,
    #[serde(rename = "pictureDownloadCode", default)]
    picture_download_code: Option<String>,
}

#[derive(Debug, Deserialize)]
struct DingtalkRobotResponse {
    errcode: Option<i64>,
    errmsg: Option<String>,
}

#[derive(Debug, Deserialize)]
struct DingtalkAccessTokenResponse {
    #[serde(rename = "accessToken")]
    access_token: String,
    #[serde(rename = "expireIn")]
    expire_in: i64,
}

#[derive(Debug, Deserialize)]
struct DingtalkFileDownloadResponse {
    #[serde(rename = "downloadUrl")]
    download_url: String,
}

impl DingtalkRobotChannel {
    pub fn from_config(config: DingtalkRobotChannelConfig) -> Result<Self> {
        let webhook_url = match config.webhook_url {
            Some(value) if !value.trim().is_empty() => value,
            _ => std::env::var(&config.webhook_url_env).with_context(|| {
                format!(
                    "dingtalk robot channel {} requires webhook_url or env {}",
                    config.id, config.webhook_url_env
                )
            })?,
        };
        let app_key = match config.app_key {
            Some(value) if !value.trim().is_empty() => Some(value),
            _ => std::env::var(&config.app_key_env)
                .ok()
                .filter(|value| !value.trim().is_empty()),
        };
        let app_secret = match config.app_secret {
            Some(value) if !value.trim().is_empty() => Some(value),
            _ => std::env::var(&config.app_secret_env)
                .ok()
                .filter(|value| !value.trim().is_empty()),
        };
        let http_callback_path = normalize_callback_path(&config.http_callback_path);

        Ok(Self {
            id: config.id,
            webhook_url,
            app_key,
            app_secret,
            api_base_url: config.api_base_url.trim_end_matches('/').to_string(),
            http_listen_addr: config.http_listen_addr,
            http_callback_path,
            client: Client::new(),
            session_routes: Mutex::new(HashMap::new()),
            access_token: Arc::new(Mutex::new(None)),
        })
    }

    async fn run_http_receiver(
        self: Arc<Self>,
        sender: mpsc::Sender<IncomingMessage>,
    ) -> Result<()> {
        let app_secret = self.app_secret.as_deref().ok_or_else(|| {
            anyhow!("dingtalk robot HTTP receiver requires app_secret or app_secret_env")
        })?;
        let listener = TcpListener::bind(&self.http_listen_addr)
            .await
            .with_context(|| {
                format!(
                    "failed to bind dingtalk robot HTTP receiver {}",
                    self.http_listen_addr
                )
            })?;
        info!(
            log_stream = "channel",
            log_key = %self.id,
            kind = "dingtalk_robot_http_ready",
            listen_addr = %self.http_listen_addr,
            callback_path = %self.http_callback_path,
            "dingtalk robot HTTP callback receiver is ready"
        );

        loop {
            let (stream, peer_addr) = listener
                .accept()
                .await
                .context("failed to accept dingtalk robot HTTP connection")?;
            let channel = Arc::clone(&self);
            let sender = sender.clone();
            let app_secret = app_secret.to_string();
            tokio::spawn(async move {
                if let Err(error) = channel
                    .handle_http_connection(stream, sender, &app_secret)
                    .await
                {
                    warn!(
                        log_stream = "channel",
                        log_key = %channel.id,
                        kind = "dingtalk_robot_http_request_failed",
                        peer = %peer_addr,
                        error = %format!("{error:#}"),
                        "failed to process dingtalk robot HTTP callback"
                    );
                }
            });
        }
    }

    async fn handle_http_connection(
        &self,
        mut stream: TcpStream,
        sender: mpsc::Sender<IncomingMessage>,
        app_secret: &str,
    ) -> Result<()> {
        let request = match read_http_request(&mut stream).await {
            Ok(request) => request,
            Err(error) => {
                write_http_response(&mut stream, 400, "Bad Request", "invalid request").await?;
                return Err(error);
            }
        };

        if request.method != "POST" || request.path != self.http_callback_path {
            write_http_response(&mut stream, 404, "Not Found", "not found").await?;
            return Ok(());
        }

        if let Err(error) = verify_dingtalk_robot_signature(
            request.header("timestamp"),
            request.header("sign"),
            app_secret,
            current_time_millis(),
        ) {
            write_http_response(&mut stream, 403, "Forbidden", "invalid signature").await?;
            return Err(error);
        }

        let payload: DingtalkRobotCallbackData = match serde_json::from_slice(&request.body) {
            Ok(value) => value,
            Err(error) => {
                write_http_response(&mut stream, 400, "Bad Request", "invalid json").await?;
                return Err(anyhow!("invalid dingtalk robot callback JSON: {error}"));
            }
        };

        self.cache_session_route(&payload).await;
        let address = self.build_address(&payload);
        let attachments = self.collect_attachments(&payload);
        let incoming = IncomingMessage {
            remote_message_id: remote_message_id(&payload),
            address,
            text: extract_callback_text(&payload),
            attachments,
            stored_attachments: Vec::new(),
            control: None,
        };
        sender
            .send(incoming)
            .await
            .map_err(|_| anyhow!("dingtalk robot receiver closed; stopping HTTP receiver"))?;

        write_http_json_response(
            &mut stream,
            200,
            "OK",
            &json!({
                "success": true
            })
            .to_string(),
        )
        .await?;
        Ok(())
    }

    async fn cache_session_route(&self, payload: &DingtalkRobotCallbackData) {
        let Some(session_webhook) = payload
            .session_webhook
            .as_deref()
            .filter(|value| !value.trim().is_empty())
        else {
            return;
        };
        let key = self.build_address(payload).session_key();
        self.session_routes.lock().await.insert(
            key,
            SessionRoute {
                session_webhook: session_webhook.to_string(),
                session_webhook_expires_at_ms: payload.session_webhook_expired_time,
            },
        );
    }

    fn build_address(&self, payload: &DingtalkRobotCallbackData) -> ChannelAddress {
        let user_id = payload
            .sender_staff_id
            .clone()
            .or_else(|| payload.sender_id.clone());
        let conversation_id = if payload.conversation_type == "1" {
            user_id
                .clone()
                .unwrap_or_else(|| payload.conversation_id.clone())
        } else {
            payload.conversation_id.clone()
        };
        ChannelAddress {
            channel_id: self.id.clone(),
            conversation_id,
            user_id,
            display_name: payload.sender_nick.clone(),
        }
    }

    fn collect_attachments(&self, payload: &DingtalkRobotCallbackData) -> Vec<PendingAttachment> {
        let Some(robot_code) = payload
            .robot_code
            .as_deref()
            .filter(|value| !value.trim().is_empty())
        else {
            return Vec::new();
        };
        let Some(app_key) = self.app_key.as_ref() else {
            return Vec::new();
        };
        let Some(app_secret) = self.app_secret.as_ref() else {
            return Vec::new();
        };
        let mut attachments = Vec::new();
        let Some(content) = payload.content.as_ref() else {
            return attachments;
        };

        match payload.msg_type.as_str() {
            "picture" | "image" => {
                if let Some(download_code) = non_empty(content.download_code.as_deref()) {
                    attachments.push(self.pending_download_attachment(
                        AttachmentKind::Image,
                        Some("dingtalk-image.bin".to_string()),
                        None,
                        download_code,
                        robot_code,
                        app_key,
                        app_secret,
                    ));
                }
            }
            "audio" => {
                if let Some(download_code) = non_empty(content.download_code.as_deref()) {
                    attachments.push(self.pending_download_attachment(
                        AttachmentKind::File,
                        Some("dingtalk-audio.bin".to_string()),
                        None,
                        download_code,
                        robot_code,
                        app_key,
                        app_secret,
                    ));
                }
            }
            "video" => {
                if let Some(download_code) = non_empty(content.download_code.as_deref()) {
                    let extension = content
                        .video_type
                        .as_deref()
                        .map(str::trim)
                        .filter(|value| !value.is_empty())
                        .unwrap_or("bin");
                    attachments.push(self.pending_download_attachment(
                        AttachmentKind::File,
                        Some(format!("dingtalk-video.{extension}")),
                        None,
                        download_code,
                        robot_code,
                        app_key,
                        app_secret,
                    ));
                }
            }
            "file" => {
                if let Some(download_code) = non_empty(content.download_code.as_deref()) {
                    attachments.push(self.pending_download_attachment(
                        AttachmentKind::File,
                        content.file_name.clone(),
                        None,
                        download_code,
                        robot_code,
                        app_key,
                        app_secret,
                    ));
                }
            }
            "richText" => {
                for (index, item) in content.rich_text.iter().enumerate() {
                    let Some(download_code) = non_empty(
                        item.download_code
                            .as_deref()
                            .or(item.picture_download_code.as_deref()),
                    ) else {
                        continue;
                    };
                    let kind = if item.item_type.as_deref() == Some("picture") {
                        AttachmentKind::Image
                    } else {
                        AttachmentKind::File
                    };
                    let name = match kind {
                        AttachmentKind::Image => format!("dingtalk-richtext-image-{index}.bin"),
                        AttachmentKind::File => format!("dingtalk-richtext-file-{index}.bin"),
                    };
                    attachments.push(self.pending_download_attachment(
                        kind,
                        Some(name),
                        None,
                        download_code,
                        robot_code,
                        app_key,
                        app_secret,
                    ));
                }
            }
            _ => {}
        }

        attachments
    }

    fn pending_download_attachment(
        &self,
        kind: AttachmentKind,
        original_name: Option<String>,
        media_type: Option<String>,
        download_code: &str,
        robot_code: &str,
        app_key: &str,
        app_secret: &str,
    ) -> PendingAttachment {
        PendingAttachment::new(
            kind,
            original_name,
            media_type,
            None,
            Arc::new(DingtalkRobotAttachmentSource {
                client: self.client.clone(),
                api_base_url: self.api_base_url.clone(),
                app_key: app_key.to_string(),
                app_secret: app_secret.to_string(),
                robot_code: robot_code.to_string(),
                download_code: download_code.to_string(),
                access_token: Arc::clone(&self.access_token),
            }),
        )
    }

    async fn active_webhook(&self, address: &ChannelAddress) -> String {
        let now_ms = current_time_millis();
        let key = address.session_key();
        let mut routes = self.session_routes.lock().await;
        let route = routes.get(&key).cloned();
        if let Some(route) = route {
            if let Some(expires_at) = route.session_webhook_expires_at_ms
                && expires_at <= now_ms
            {
                routes.remove(&key);
            } else {
                return route.session_webhook;
            }
        }
        self.webhook_url.clone()
    }

    async fn send_text_to_webhook(&self, webhook: &str, text: &str) -> Result<()> {
        let response = self
            .client
            .post(webhook)
            .json(&json!({
                "msgtype": "text",
                "text": {
                    "content": text
                }
            }))
            .send()
            .await
            .map_err(|_| anyhow!("failed to send DingTalk robot webhook message"))?;

        if !response.status().is_success() {
            let status = response.status();
            let body = response
                .text()
                .await
                .unwrap_or_else(|_| "<unreadable body>".to_string());
            return Err(anyhow!(
                "dingtalk robot webhook returned status {}: {}",
                status,
                body
            ));
        }

        let payload = response
            .json::<DingtalkRobotResponse>()
            .await
            .context("dingtalk robot webhook returned invalid JSON")?;
        if payload.errcode.unwrap_or_default() != 0 {
            return Err(anyhow!(
                "dingtalk robot webhook failed: {}",
                payload
                    .errmsg
                    .unwrap_or_else(|| "unknown error".to_string())
            ));
        }
        Ok(())
    }

    async fn send_text(&self, address: &ChannelAddress, text: &str) -> Result<()> {
        let webhook = self.active_webhook(address).await;
        self.send_text_to_webhook(&webhook, text).await
    }
}

#[async_trait]
impl Channel for DingtalkRobotChannel {
    fn id(&self) -> &str {
        &self.id
    }

    async fn run(self: Arc<Self>, sender: mpsc::Sender<IncomingMessage>) -> Result<()> {
        if self.app_secret.is_some() {
            self.run_http_receiver(sender).await?;
        } else {
            info!(
                log_stream = "channel",
                log_key = %self.id,
                kind = "dingtalk_robot_ready",
                "dingtalk robot channel is ready for outbound webhook sends; set app_secret_env to enable HTTP callbacks"
            );
            pending::<()>().await;
        }
        Ok(())
    }

    async fn send_media_group(
        &self,
        address: &ChannelAddress,
        images: Vec<OutgoingAttachment>,
    ) -> Result<()> {
        if images.is_empty() {
            return Ok(());
        }
        let details = images
            .into_iter()
            .map(|image| {
                image
                    .caption
                    .filter(|caption| !caption.trim().is_empty())
                    .unwrap_or_else(|| image.path.display().to_string())
            })
            .collect::<Vec<_>>()
            .join("\n");
        self.send_text(
            address,
            &format!(
                "Generated images are not uploaded to DingTalk robot webhooks yet:\n{}",
                details
            ),
        )
        .await
    }

    async fn send(&self, address: &ChannelAddress, message: OutgoingMessage) -> Result<()> {
        info!(
            log_stream = "channel",
            log_key = %self.id,
            kind = "dingtalk_robot_send",
            has_text = message.text.is_some(),
            image_count = message.images.len() as u64,
            attachment_count = message.attachments.len() as u64,
            has_options = message.options.is_some(),
            "sending message through DingTalk robot webhook"
        );

        self.send_media_group(address, message.images).await?;

        let mut chunks = Vec::new();
        if let Some(text) = message.text
            && !text.trim().is_empty()
        {
            chunks.push(text);
        }
        if let Some(options) = message.options {
            chunks.push(options.prompt);
            chunks.extend(
                options
                    .options
                    .into_iter()
                    .map(|option| format!("[option] {} -> {}", option.label, option.value)),
            );
        }
        if !message.attachments.is_empty() {
            let details = message
                .attachments
                .into_iter()
                .map(|attachment| attachment.path.display().to_string())
                .collect::<Vec<_>>()
                .join("\n");
            chunks.push(format!(
                "Generated attachments are not uploaded to DingTalk robot webhooks yet:\n{}",
                details
            ));
        }

        let text = chunks.join("\n");
        if text.trim().is_empty() {
            warn!(
                log_stream = "channel",
                log_key = %self.id,
                kind = "dingtalk_robot_empty_send_skipped",
                "skipped empty DingTalk robot webhook message"
            );
            return Ok(());
        }
        self.send_text(address, &text).await
    }

    async fn set_processing(
        &self,
        _address: &ChannelAddress,
        _state: ProcessingState,
    ) -> Result<()> {
        Ok(())
    }

    async fn probe_conversation(
        &self,
        _address: &ChannelAddress,
    ) -> Result<Option<ConversationProbe>> {
        Ok(Some(ConversationProbe::Available { member_count: None }))
    }

    async fn update_progress_feedback(
        &self,
        _address: &ChannelAddress,
        _feedback: ProgressFeedback,
    ) -> Result<ProgressFeedbackUpdate> {
        Ok(ProgressFeedbackUpdate::Unchanged)
    }
}

struct DingtalkRobotAttachmentSource {
    client: Client,
    api_base_url: String,
    app_key: String,
    app_secret: String,
    robot_code: String,
    download_code: String,
    access_token: Arc<Mutex<Option<CachedAccessToken>>>,
}

#[async_trait]
impl AttachmentSource for DingtalkRobotAttachmentSource {
    async fn save_to(&self, destination: &Path) -> Result<u64> {
        let access_token = self.access_token().await?;
        let response = self
            .client
            .post(format!(
                "{}/v1.0/robot/messageFiles/download",
                self.api_base_url
            ))
            .header("x-acs-dingtalk-access-token", access_token)
            .json(&json!({
                "downloadCode": self.download_code,
                "robotCode": self.robot_code,
            }))
            .send()
            .await
            .context("failed to request DingTalk robot file download URL")?;
        if !response.status().is_success() {
            let status = response.status();
            let body = response
                .text()
                .await
                .unwrap_or_else(|_| "<unreadable body>".to_string());
            return Err(anyhow!(
                "dingtalk robot file download URL request returned status {}: {}",
                status,
                body
            ));
        }

        let download = response
            .json::<DingtalkFileDownloadResponse>()
            .await
            .context("DingTalk robot file download URL response was invalid JSON")?;
        let bytes = self
            .client
            .get(download.download_url)
            .send()
            .await
            .map_err(|_| anyhow!("failed to download DingTalk robot attachment bytes"))?
            .bytes()
            .await
            .context("failed to read DingTalk robot attachment bytes")?;
        if let Some(parent) = destination.parent() {
            fs::create_dir_all(parent).await?;
        }
        fs::write(destination, &bytes).await?;
        Ok(bytes.len() as u64)
    }
}

impl DingtalkRobotAttachmentSource {
    async fn access_token(&self) -> Result<String> {
        let now_ms = current_time_millis();
        if let Some(cached) = self.access_token.lock().await.as_ref()
            && cached.expires_at_ms > now_ms + 60_000
        {
            return Ok(cached.token.clone());
        }

        let response = self
            .client
            .post(format!("{}/v1.0/oauth2/accessToken", self.api_base_url))
            .json(&json!({
                "appKey": self.app_key,
                "appSecret": self.app_secret,
            }))
            .send()
            .await
            .context("failed to request DingTalk accessToken")?;
        if !response.status().is_success() {
            let status = response.status();
            let body = response
                .text()
                .await
                .unwrap_or_else(|_| "<unreadable body>".to_string());
            return Err(anyhow!(
                "dingtalk accessToken request returned status {}: {}",
                status,
                body
            ));
        }
        let token = response
            .json::<DingtalkAccessTokenResponse>()
            .await
            .context("DingTalk accessToken response was invalid JSON")?;
        let expires_at_ms = current_time_millis() + token.expire_in.max(60) * 1000;
        let mut cached = self.access_token.lock().await;
        *cached = Some(CachedAccessToken {
            token: token.access_token.clone(),
            expires_at_ms,
        });
        Ok(token.access_token)
    }
}

struct HttpRequest {
    method: String,
    path: String,
    headers: HashMap<String, String>,
    body: Vec<u8>,
}

impl HttpRequest {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .get(&name.to_ascii_lowercase())
            .map(String::as_str)
    }
}

async fn read_http_request(stream: &mut TcpStream) -> Result<HttpRequest> {
    let mut buffer = Vec::new();
    let header_end = loop {
        let mut chunk = [0_u8; 4096];
        let read = stream
            .read(&mut chunk)
            .await
            .context("failed to read HTTP request")?;
        if read == 0 {
            bail!("connection closed before HTTP request was complete");
        }
        buffer.extend_from_slice(&chunk[..read]);
        if buffer.len() > MAX_HTTP_BODY_BYTES {
            bail!("HTTP request exceeded maximum size");
        }
        if let Some(position) = find_header_end(&buffer) {
            break position;
        }
    };

    let header_bytes = &buffer[..header_end];
    let header_text = std::str::from_utf8(header_bytes).context("HTTP headers are not UTF-8")?;
    let mut lines = header_text.split("\r\n");
    let request_line = lines
        .next()
        .ok_or_else(|| anyhow!("missing request line"))?;
    let mut request_parts = request_line.split_whitespace();
    let method = request_parts
        .next()
        .ok_or_else(|| anyhow!("missing HTTP method"))?
        .to_string();
    let raw_path = request_parts
        .next()
        .ok_or_else(|| anyhow!("missing HTTP path"))?;
    let path = raw_path.split('?').next().unwrap_or(raw_path).to_string();

    let mut headers = HashMap::new();
    for line in lines {
        if line.trim().is_empty() {
            continue;
        }
        let Some((name, value)) = line.split_once(':') else {
            continue;
        };
        headers.insert(name.trim().to_ascii_lowercase(), value.trim().to_string());
    }

    let content_length = headers
        .get("content-length")
        .and_then(|value| value.parse::<usize>().ok())
        .unwrap_or(0);
    if content_length > MAX_HTTP_BODY_BYTES {
        bail!("HTTP body exceeded maximum size");
    }

    let body_start = header_end + 4;
    while buffer.len() < body_start + content_length {
        let mut chunk = vec![0_u8; body_start + content_length - buffer.len()];
        let read = stream
            .read(&mut chunk)
            .await
            .context("failed to read HTTP body")?;
        if read == 0 {
            bail!("connection closed before HTTP body was complete");
        }
        buffer.extend_from_slice(&chunk[..read]);
    }

    Ok(HttpRequest {
        method,
        path,
        headers,
        body: buffer[body_start..body_start + content_length].to_vec(),
    })
}

fn find_header_end(buffer: &[u8]) -> Option<usize> {
    buffer.windows(4).position(|window| window == b"\r\n\r\n")
}

async fn write_http_response(
    stream: &mut TcpStream,
    status_code: u16,
    reason: &str,
    body: &str,
) -> Result<()> {
    let response = format!(
        "HTTP/1.1 {} {}\r\nContent-Type: text/plain; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        status_code,
        reason,
        body.len(),
        body
    );
    stream
        .write_all(response.as_bytes())
        .await
        .context("failed to write HTTP response")
}

async fn write_http_json_response(
    stream: &mut TcpStream,
    status_code: u16,
    reason: &str,
    body: &str,
) -> Result<()> {
    let response = format!(
        "HTTP/1.1 {} {}\r\nContent-Type: application/json; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        status_code,
        reason,
        body.len(),
        body
    );
    stream
        .write_all(response.as_bytes())
        .await
        .context("failed to write HTTP JSON response")
}

fn verify_dingtalk_robot_signature(
    timestamp: Option<&str>,
    sign: Option<&str>,
    app_secret: &str,
    now_ms: i64,
) -> Result<()> {
    let timestamp = timestamp
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| anyhow!("missing dingtalk timestamp header"))?;
    let sign = sign
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| anyhow!("missing dingtalk sign header"))?;
    let timestamp_ms = timestamp
        .parse::<i64>()
        .context("invalid dingtalk timestamp header")?;
    if (now_ms - timestamp_ms).abs() > MAX_SIGNATURE_SKEW_MS {
        bail!("dingtalk timestamp is outside allowed skew");
    }

    let expected = calculate_dingtalk_robot_signature(timestamp, app_secret)?;
    if sign != expected {
        bail!("invalid dingtalk callback signature");
    }
    Ok(())
}

fn calculate_dingtalk_robot_signature(timestamp: &str, app_secret: &str) -> Result<String> {
    let string_to_sign = format!("{timestamp}\n{app_secret}");
    let mut mac = HmacSha256::new_from_slice(app_secret.as_bytes())
        .map_err(|_| anyhow!("invalid dingtalk app secret"))?;
    mac.update(string_to_sign.as_bytes());
    Ok(BASE64_STANDARD.encode(mac.finalize().into_bytes()))
}

fn extract_callback_text(payload: &DingtalkRobotCallbackData) -> Option<String> {
    let text = match payload.msg_type.as_str() {
        "text" => payload
            .text
            .as_ref()
            .map(|value| value.content.trim().to_string()),
        "audio" => payload
            .content
            .as_ref()
            .and_then(|value| value.recognition.as_deref())
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(|value| value.to_string()),
        "image" | "picture" => Some(attachment_notice(
            "DingTalk image message",
            payload.content.as_ref(),
        )),
        "video" => Some(attachment_notice(
            "DingTalk video message",
            payload.content.as_ref(),
        )),
        "file" => Some(format!(
            "[DingTalk file message: {}{}]",
            payload
                .content
                .as_ref()
                .and_then(|value| value.file_name.as_deref())
                .unwrap_or("unnamed file"),
            payload
                .content
                .as_ref()
                .and_then(|value| value.download_code.as_deref())
                .map(|_| "; attachment download queued when credentials are configured")
                .unwrap_or("")
        )),
        "richText" => payload.content.as_ref().map(render_rich_text),
        _ => payload
            .content
            .as_ref()
            .and_then(|value| value.unknown_msg_type.clone())
            .or_else(|| {
                Some(format!(
                    "[Unsupported DingTalk message type: {}]",
                    payload.msg_type
                ))
            }),
    };
    text.and_then(|value| {
        let trimmed = value.trim();
        (!trimmed.is_empty()).then(|| trimmed.to_string())
    })
}

fn attachment_notice(kind: &str, content: Option<&DingtalkContent>) -> String {
    if content
        .and_then(|value| value.download_code.as_deref())
        .is_some()
    {
        format!("[{kind}; attachment download queued when credentials are configured]")
    } else {
        format!("[{kind}]")
    }
}

fn non_empty(value: Option<&str>) -> Option<&str> {
    value.map(str::trim).filter(|value| !value.is_empty())
}

fn render_rich_text(content: &DingtalkContent) -> String {
    let mut parts = Vec::new();
    for item in &content.rich_text {
        if let Some(text) = item.text.as_deref()
            && !text.trim().is_empty()
        {
            parts.push(text.trim().to_string());
            continue;
        }
        if item.item_type.as_deref() == Some("picture") {
            parts.push("[image]".to_string());
        }
    }
    parts.join("\n")
}

fn remote_message_id(payload: &DingtalkRobotCallbackData) -> String {
    payload
        .msg_id
        .clone()
        .or_else(|| payload.create_at.clone())
        .map(|value| format!("dingtalk-robot-{value}"))
        .unwrap_or_else(|| format!("dingtalk-robot-{}", Uuid::new_v4()))
}

fn normalize_callback_path(path: &str) -> String {
    let trimmed = path.trim();
    if trimmed.is_empty() {
        "/dingtalk/robot".to_string()
    } else if trimmed.starts_with('/') {
        trimmed.to_string()
    } else {
        format!("/{trimmed}")
    }
}

fn current_time_millis() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as i64
}

#[cfg(test)]
mod tests {
    use super::*;

    fn robot_config() -> DingtalkRobotChannelConfig {
        DingtalkRobotChannelConfig {
            id: "dingtalk-robot-main".to_string(),
            webhook_url: Some("https://example.com/robot/send".to_string()),
            webhook_url_env: "DINGTALK_ROBOT_WEBHOOK_URL".to_string(),
            app_key: Some("app-key".to_string()),
            app_key_env: "DINGTALK_ROBOT_APP_KEY".to_string(),
            app_secret: Some("secret".to_string()),
            app_secret_env: "DINGTALK_ROBOT_APP_SECRET".to_string(),
            http_listen_addr: "127.0.0.1:0".to_string(),
            http_callback_path: "/dingtalk/robot".to_string(),
            api_base_url: "https://api.dingtalk.com".to_string(),
        }
    }

    fn robot_channel() -> DingtalkRobotChannel {
        DingtalkRobotChannel::from_config(robot_config()).unwrap()
    }

    fn callback_payload(msg_type: &str, content: DingtalkContent) -> DingtalkRobotCallbackData {
        DingtalkRobotCallbackData {
            conversation_id: "cid".to_string(),
            conversation_type: "2".to_string(),
            sender_staff_id: Some("user-1".to_string()),
            sender_id: None,
            sender_nick: Some("Alice".to_string()),
            robot_code: Some("robot-code".to_string()),
            msg_id: Some("msg-1".to_string()),
            msg_type: msg_type.to_string(),
            create_at: None,
            session_webhook: None,
            session_webhook_expired_time: None,
            text: None,
            content: Some(content),
        }
    }

    fn empty_content() -> DingtalkContent {
        DingtalkContent {
            recognition: None,
            file_name: None,
            download_code: None,
            video_type: None,
            unknown_msg_type: None,
            rich_text: Vec::new(),
        }
    }

    #[test]
    fn verifies_valid_dingtalk_robot_signature() {
        let timestamp = "1678888888000";
        let app_secret = "secret";
        let sign = calculate_dingtalk_robot_signature(timestamp, app_secret).unwrap();

        verify_dingtalk_robot_signature(Some(timestamp), Some(&sign), app_secret, 1678888888000)
            .unwrap();
    }

    #[test]
    fn rejects_expired_dingtalk_robot_signature() {
        let timestamp = "1678888888000";
        let app_secret = "secret";
        let sign = calculate_dingtalk_robot_signature(timestamp, app_secret).unwrap();

        assert!(
            verify_dingtalk_robot_signature(
                Some(timestamp),
                Some(&sign),
                app_secret,
                1678888888000 + MAX_SIGNATURE_SKEW_MS + 1,
            )
            .is_err()
        );
    }

    #[test]
    fn rejects_invalid_dingtalk_robot_signature() {
        assert!(
            verify_dingtalk_robot_signature(
                Some("1678888888000"),
                Some("invalid"),
                "secret",
                1678888888000,
            )
            .is_err()
        );
    }

    #[test]
    fn picture_callback_creates_pending_image_attachment() {
        let channel = robot_channel();
        let mut content = empty_content();
        content.download_code = Some("download-code".to_string());
        let payload = callback_payload("picture", content);

        let attachments = channel.collect_attachments(&payload);

        assert_eq!(attachments.len(), 1);
        assert_eq!(attachments[0].kind, AttachmentKind::Image);
        assert_eq!(
            attachments[0].original_name.as_deref(),
            Some("dingtalk-image.bin")
        );
    }

    #[test]
    fn rich_text_picture_callback_preserves_text_and_attachment() {
        let channel = robot_channel();
        let mut content = empty_content();
        content.rich_text = vec![
            DingtalkRichTextItem {
                text: Some("hello".to_string()),
                item_type: None,
                download_code: None,
                picture_download_code: None,
            },
            DingtalkRichTextItem {
                text: None,
                item_type: Some("picture".to_string()),
                download_code: Some("download-code".to_string()),
                picture_download_code: None,
            },
        ];
        let payload = callback_payload("richText", content);

        let attachments = channel.collect_attachments(&payload);

        assert_eq!(
            extract_callback_text(&payload).as_deref(),
            Some("hello\n[image]")
        );
        assert_eq!(attachments.len(), 1);
        assert_eq!(attachments[0].kind, AttachmentKind::Image);
    }

    #[test]
    fn missing_app_key_leaves_download_code_unmaterialized() {
        let mut config = robot_config();
        config.app_key = None;
        config.app_key_env = "MISSING_DINGTALK_ROBOT_APP_KEY_FOR_TEST".to_string();
        let channel = DingtalkRobotChannel::from_config(config).unwrap();
        let mut content = empty_content();
        content.download_code = Some("download-code".to_string());
        let payload = callback_payload("picture", content);

        assert!(channel.collect_attachments(&payload).is_empty());
    }
}
