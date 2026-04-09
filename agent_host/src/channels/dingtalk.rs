use crate::channel::{Channel, IncomingMessage};
use crate::config::DingtalkChannelConfig;
use crate::domain::{ChannelAddress, OutgoingMessage, ProcessingState};
use anyhow::{Context, Result, anyhow};
use async_trait::async_trait;
use futures_util::{SinkExt, StreamExt};
use reqwest::Client;
use serde::Deserialize;
use serde_json::{Value, json};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::sync::{Mutex, mpsc};
use tokio_tungstenite::{connect_async, tungstenite::Message};
use tracing::{info, warn};

const BOT_MESSAGE_TOPIC: &str = "/v1.0/im/bot/messages/get";
const MAX_STREAM_BACKOFF_SECONDS: u64 = 30;

pub struct DingtalkChannel {
    id: String,
    client_id: String,
    client_secret: String,
    api_base_url: String,
    client: Client,
    session_routes: Mutex<HashMap<String, SessionRoute>>,
}

#[derive(Clone, Debug)]
struct SessionRoute {
    session_webhook: String,
    session_webhook_expires_at_ms: Option<i64>,
}

#[derive(Debug, Deserialize)]
struct StreamOpenResponse {
    endpoint: String,
    ticket: String,
}

#[derive(Debug, Deserialize)]
struct StreamEnvelope {
    #[serde(rename = "type")]
    message_type: String,
    headers: StreamHeaders,
    data: Option<String>,
}

#[derive(Debug, Deserialize)]
struct StreamHeaders {
    #[serde(rename = "messageId")]
    message_id: String,
    topic: String,
}

#[derive(Clone, Debug, Deserialize)]
struct DingtalkBotCallbackData {
    #[serde(rename = "conversationId")]
    conversation_id: String,
    #[serde(rename = "conversationType")]
    conversation_type: String,
    #[serde(rename = "senderStaffId")]
    sender_staff_id: Option<String>,
    #[serde(rename = "senderId")]
    sender_id: Option<String>,
    #[serde(rename = "senderNick")]
    sender_nick: Option<String>,
    #[serde(rename = "msgId")]
    msg_id: String,
    #[serde(rename = "msgtype")]
    msg_type: String,
    #[serde(rename = "sessionWebhook")]
    session_webhook: Option<String>,
    #[serde(rename = "sessionWebhookExpiredTime")]
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
}

#[derive(Debug, Deserialize)]
struct DingtalkWebhookResponse {
    errcode: Option<i64>,
    errmsg: Option<String>,
}

impl DingtalkChannel {
    pub fn from_config(config: DingtalkChannelConfig) -> Result<Self> {
        let client_id = match config.client_id {
            Some(value) if !value.trim().is_empty() => value,
            _ => std::env::var(&config.client_id_env).with_context(|| {
                format!(
                    "dingtalk channel {} requires client_id or env {}",
                    config.id, config.client_id_env
                )
            })?,
        };
        let client_secret = match config.client_secret {
            Some(value) if !value.trim().is_empty() => value,
            _ => std::env::var(&config.client_secret_env).with_context(|| {
                format!(
                    "dingtalk channel {} requires client_secret or env {}",
                    config.id, config.client_secret_env
                )
            })?,
        };

        Ok(Self {
            id: config.id,
            client_id,
            client_secret,
            api_base_url: config.api_base_url.trim_end_matches('/').to_string(),
            client: Client::new(),
            session_routes: Mutex::new(HashMap::new()),
        })
    }

    fn stream_open_url(&self) -> String {
        format!("{}/v1.0/gateway/connections/open", self.api_base_url)
    }

    async fn open_stream_connection(&self) -> Result<StreamOpenResponse> {
        let response = self
            .client
            .post(self.stream_open_url())
            .json(&json!({
                "clientId": self.client_id,
                "clientSecret": self.client_secret,
                "subscriptions": [
                    {
                        "topic": BOT_MESSAGE_TOPIC,
                        "type": "CALLBACK"
                    }
                ],
                "ua": "partyclaw-agent-host/0.11"
            }))
            .send()
            .await
            .context("failed to open dingtalk stream connection")?;
        if !response.status().is_success() {
            let status = response.status();
            let body = response
                .text()
                .await
                .unwrap_or_else(|_| "<unreadable body>".to_string());
            return Err(anyhow!(
                "dingtalk stream open failed with status {}: {}",
                status,
                body
            ));
        }
        response
            .json::<StreamOpenResponse>()
            .await
            .context("dingtalk stream open returned invalid JSON")
    }

    fn ws_url(endpoint: &str, ticket: &str) -> String {
        let delimiter = if endpoint.contains('?') { '&' } else { '?' };
        format!("{endpoint}{delimiter}ticket={ticket}")
    }

    async fn run_stream_once(&self, sender: &mpsc::Sender<IncomingMessage>) -> Result<()> {
        let opened = self.open_stream_connection().await?;
        let ws_url = Self::ws_url(&opened.endpoint, &opened.ticket);
        let (mut stream, _) = connect_async(&ws_url).await.with_context(|| {
            format!(
                "failed to connect DingTalk stream websocket {}",
                opened.endpoint
            )
        })?;
        info!(
            log_stream = "channel",
            log_key = %self.id,
            kind = "dingtalk_stream_connected",
            endpoint = %opened.endpoint,
            "connected to dingtalk stream websocket"
        );

        while let Some(frame) = stream.next().await {
            let frame = frame.context("dingtalk websocket read failed")?;
            match frame {
                Message::Text(text) => {
                    if let Some(response) = self.handle_stream_text(sender, &text).await? {
                        stream
                            .send(Message::Text(response.into()))
                            .await
                            .context("failed to send dingtalk stream ack")?;
                    }
                }
                Message::Ping(payload) => {
                    stream
                        .send(Message::Pong(payload))
                        .await
                        .context("failed to send websocket pong")?;
                }
                Message::Close(_) => break,
                Message::Binary(_) | Message::Pong(_) | Message::Frame(_) => {}
            }
        }

        Ok(())
    }

    async fn handle_stream_text(
        &self,
        sender: &mpsc::Sender<IncomingMessage>,
        text: &str,
    ) -> Result<Option<String>> {
        let envelope: StreamEnvelope =
            serde_json::from_str(text).context("failed to parse dingtalk stream envelope")?;
        match envelope.headers.topic.as_str() {
            "ping" => Ok(Some(Self::ping_ack(
                &envelope.headers.message_id,
                envelope.data.as_deref(),
            ))),
            "disconnect" => {
                let reason = envelope
                    .data
                    .as_deref()
                    .and_then(|raw| serde_json::from_str::<Value>(raw).ok())
                    .and_then(|value| {
                        value
                            .get("reason")
                            .and_then(Value::as_str)
                            .map(str::to_string)
                    })
                    .unwrap_or_else(|| "connection is expired".to_string());
                Err(anyhow!("dingtalk stream requested disconnect: {reason}"))
            }
            BOT_MESSAGE_TOPIC if envelope.message_type == "CALLBACK" => {
                let Some(raw_data) = envelope.data.as_deref() else {
                    return Ok(Some(Self::callback_error_ack(
                        &envelope.headers.message_id,
                        "missing callback data",
                    )));
                };
                let payload: DingtalkBotCallbackData = match serde_json::from_str(raw_data) {
                    Ok(value) => value,
                    Err(error) => {
                        warn!(
                            log_stream = "channel",
                            log_key = %self.id,
                            kind = "dingtalk_callback_parse_failed",
                            error = %format!("{error:#}"),
                            "failed to parse dingtalk callback payload"
                        );
                        return Ok(Some(Self::callback_error_ack(
                            &envelope.headers.message_id,
                            "invalid callback data",
                        )));
                    }
                };
                self.cache_session_route(&payload).await;
                let incoming = IncomingMessage {
                    remote_message_id: payload.msg_id.clone(),
                    address: self.build_address(&payload),
                    text: extract_callback_text(&payload),
                    attachments: Vec::new(),
                    control: None,
                };
                sender
                    .send(incoming)
                    .await
                    .map_err(|_| anyhow!("dingtalk receiver closed; stopping stream loop"))?;
                Ok(Some(Self::callback_ok_ack(&envelope.headers.message_id)))
            }
            _ => Ok(Some(Self::callback_ok_ack(&envelope.headers.message_id))),
        }
    }

    async fn cache_session_route(&self, payload: &DingtalkBotCallbackData) {
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

    fn build_address(&self, payload: &DingtalkBotCallbackData) -> ChannelAddress {
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

    async fn active_session_webhook(&self, address: &ChannelAddress) -> Option<String> {
        let now_ms = current_time_millis();
        let key = address.session_key();
        let mut routes = self.session_routes.lock().await;
        let route = routes.get(&key)?.clone();
        if let Some(expires_at) = route.session_webhook_expires_at_ms
            && expires_at <= now_ms
        {
            routes.remove(&key);
            return None;
        }
        Some(route.session_webhook)
    }

    async fn send_via_session_webhook(&self, webhook: &str, text: &str) -> Result<()> {
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
            .context("failed to send DingTalk session webhook message")?;
        if !response.status().is_success() {
            let status = response.status();
            let body = response
                .text()
                .await
                .unwrap_or_else(|_| "<unreadable body>".to_string());
            return Err(anyhow!(
                "dingtalk session webhook returned status {}: {}",
                status,
                body
            ));
        }
        let payload = response
            .json::<DingtalkWebhookResponse>()
            .await
            .context("dingtalk session webhook returned invalid JSON")?;
        if payload.errcode.unwrap_or_default() != 0 {
            return Err(anyhow!(
                "dingtalk session webhook failed: {}",
                payload
                    .errmsg
                    .unwrap_or_else(|| "unknown error".to_string())
            ));
        }
        Ok(())
    }

    fn callback_ok_ack(message_id: &str) -> String {
        json!({
            "code": 200,
            "message": "OK",
            "headers": {
                "messageId": message_id,
                "contentType": "application/json"
            },
            "data": "{\"response\":null}"
        })
        .to_string()
    }

    fn callback_error_ack(message_id: &str, message: &str) -> String {
        json!({
            "code": 500,
            "message": "internal error",
            "headers": {
                "messageId": message_id,
                "contentType": "application/json"
            },
            "data": json!({
                "message": message
            }).to_string()
        })
        .to_string()
    }

    fn ping_ack(message_id: &str, raw_data: Option<&str>) -> String {
        let opaque = raw_data
            .and_then(|value| serde_json::from_str::<Value>(value).ok())
            .and_then(|value| value.get("opaque").cloned())
            .unwrap_or(Value::Null);
        json!({
            "code": 200,
            "message": "OK",
            "headers": {
                "messageId": message_id,
                "contentType": "application/json"
            },
            "data": json!({
                "opaque": opaque
            }).to_string()
        })
        .to_string()
    }
}

#[async_trait]
impl Channel for DingtalkChannel {
    fn id(&self) -> &str {
        &self.id
    }

    async fn run(self: Arc<Self>, sender: mpsc::Sender<IncomingMessage>) -> Result<()> {
        let mut backoff_seconds = 1;
        loop {
            match self.run_stream_once(&sender).await {
                Ok(()) => {
                    warn!(
                        log_stream = "channel",
                        log_key = %self.id,
                        kind = "dingtalk_stream_closed",
                        "dingtalk stream closed; reconnecting"
                    );
                    backoff_seconds = 1;
                }
                Err(error) => {
                    warn!(
                        log_stream = "channel",
                        log_key = %self.id,
                        kind = "dingtalk_stream_failed",
                        error = %format!("{error:#}"),
                        backoff_seconds = backoff_seconds,
                        "dingtalk stream failed; retrying"
                    );
                }
            }
            tokio::time::sleep(Duration::from_secs(backoff_seconds)).await;
            backoff_seconds = (backoff_seconds * 2).min(MAX_STREAM_BACKOFF_SECONDS);
        }
    }

    async fn send_media_group(
        &self,
        address: &ChannelAddress,
        images: Vec<crate::domain::OutgoingAttachment>,
    ) -> Result<()> {
        let names = images
            .iter()
            .map(|attachment| attachment.path.display().to_string())
            .collect::<Vec<_>>();
        self.send(
            address,
            OutgoingMessage::text(format!(
                "This response generated {} image attachment(s), but DingTalk attachment upload is not implemented yet.\n{}",
                names.len(),
                names.join("\n")
            )),
        )
        .await
    }

    async fn send(&self, address: &ChannelAddress, message: OutgoingMessage) -> Result<()> {
        let webhook = self.active_session_webhook(address).await.ok_or_else(|| {
            anyhow!(
                "no active DingTalk session webhook cached for conversation {}",
                address.conversation_id
            )
        })?;
        let text = render_outgoing_text(message);
        self.send_via_session_webhook(&webhook, &text).await
    }

    async fn set_processing(
        &self,
        _address: &ChannelAddress,
        _state: ProcessingState,
    ) -> Result<()> {
        Ok(())
    }
}

fn extract_callback_text(payload: &DingtalkBotCallbackData) -> Option<String> {
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
        "picture" => Some("[DingTalk picture message]".to_string()),
        "video" => Some("[DingTalk video message]".to_string()),
        "file" => Some(format!(
            "[DingTalk file message: {}]",
            payload
                .content
                .as_ref()
                .and_then(|value| value.file_name.as_deref())
                .unwrap_or("unnamed file")
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

fn render_outgoing_text(message: OutgoingMessage) -> String {
    let mut parts = Vec::new();
    if let Some(text) = message.text
        && !text.trim().is_empty()
    {
        parts.push(text.trim().to_string());
    }
    if let Some(options) = message.options {
        if !options.prompt.trim().is_empty() {
            parts.push(options.prompt.trim().to_string());
        }
        if !options.options.is_empty() {
            let rendered = options
                .options
                .iter()
                .enumerate()
                .map(|(index, option)| format!("{}. {}", index + 1, option.label))
                .collect::<Vec<_>>()
                .join("\n");
            if !rendered.is_empty() {
                parts.push(rendered);
            }
        }
    }
    let unsupported_paths = message
        .images
        .into_iter()
        .chain(message.attachments)
        .map(|attachment| attachment.path.display().to_string())
        .collect::<Vec<_>>();
    if !unsupported_paths.is_empty() {
        parts.push(format!(
            "Generated attachments are not uploaded to DingTalk yet:\n{}",
            unsupported_paths.join("\n")
        ));
    }
    if parts.is_empty() {
        " ".to_string()
    } else {
        parts.join("\n\n")
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
    use super::{DingtalkBotCallbackData, extract_callback_text};

    #[test]
    fn private_callback_uses_sender_as_conversation_id() {
        let payload: DingtalkBotCallbackData = serde_json::from_str(
            r#"{
              "conversationId":"cid-group",
              "conversationType":"1",
              "senderStaffId":"user-42",
              "senderNick":"Alice",
              "msgId":"msg-1",
              "msgtype":"text",
              "sessionWebhook":"https://example.com/webhook",
              "sessionWebhookExpiredTime":4102444800000,
              "text":{"content":"hello"}
            }"#,
        )
        .unwrap();
        let channel = super::DingtalkChannel::from_config(crate::config::DingtalkChannelConfig {
            id: "dingtalk-main".to_string(),
            client_id: Some("client".to_string()),
            client_id_env: "DINGTALK_CLIENT_ID".to_string(),
            client_secret: Some("secret".to_string()),
            client_secret_env: "DINGTALK_CLIENT_SECRET".to_string(),
            api_base_url: "https://api.dingtalk.com".to_string(),
        })
        .unwrap();
        let address = channel.build_address(&payload);
        assert_eq!(address.conversation_id, "user-42");
        assert_eq!(address.user_id.as_deref(), Some("user-42"));
    }

    #[test]
    fn audio_callback_prefers_recognition_text() {
        let payload: DingtalkBotCallbackData = serde_json::from_str(
            r#"{
              "conversationId":"cid-group",
              "conversationType":"2",
              "senderStaffId":"user-42",
              "msgId":"msg-1",
              "msgtype":"audio",
              "content":{"recognition":"hello from audio"}
            }"#,
        )
        .unwrap();
        assert_eq!(
            extract_callback_text(&payload).as_deref(),
            Some("hello from audio")
        );
    }
}
