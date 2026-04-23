use crate::channel::{
    Channel, ConversationProbe, IncomingMessage, ProgressFeedback, ProgressFeedbackUpdate,
};
use crate::config::WebChannelConfig;
use crate::domain::{
    ChannelAddress, OutgoingAttachment, OutgoingMessage, ProcessingState, ShowOptions,
    validate_conversation_id,
};
use crate::remote_execution::storage_root_for_execution_root;
use crate::remote_execution::{
    RemoteExecutionBinding, validate_local_execution_path, validate_ssh_execution_binding,
};
use crate::transcript::{TranscriptEntry, TranscriptEntrySkeleton, TranscriptEntryType};
use anyhow::{Context, Result};
use async_trait::async_trait;
use axum::{
    Router,
    extract::{Query, State, WebSocketUpgrade, ws},
    http::{HeaderMap, Method, StatusCode, header},
    response::{Html, IntoResponse, Json, Response},
    routing::{get, post},
};
use futures_util::SinkExt;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock as StdRwLock};
use tokio::sync::{RwLock, broadcast, mpsc};
use tower_http::cors::{Any, CorsLayer};

struct WebChannelInner {
    id: String,
    listen_addr: String,
    auth_token: Option<String>,
    auth_token_env: String,
    workdir: PathBuf,
    host: StdRwLock<Option<Arc<dyn WebChannelHost>>>,
    incoming_sender: RwLock<Option<mpsc::Sender<IncomingMessage>>>,
    event_bus: broadcast::Sender<WebSocketEvent>,
}

pub struct WebChannel {
    inner: Arc<WebChannelInner>,
}

#[async_trait]
pub trait WebChannelHost: Send + Sync {
    async fn list_web_conversations(&self, channel_id: &str)
    -> Result<Vec<WebConversationSummary>>;
    async fn get_web_conversation(
        &self,
        address: &ChannelAddress,
    ) -> Result<Option<WebConversationSummary>>;
    async fn create_web_conversation(
        &self,
        address: &ChannelAddress,
        remote_execution: RemoteExecutionBinding,
    ) -> Result<WebConversationSummary>;
    async fn update_web_conversation_remote_execution(
        &self,
        address: &ChannelAddress,
        remote_execution: RemoteExecutionBinding,
    ) -> Result<WebConversationSummary>;
    async fn delete_web_conversation(&self, address: &ChannelAddress) -> Result<bool>;
    async fn list_web_transcript(
        &self,
        address: &ChannelAddress,
        offset: usize,
        limit: usize,
    ) -> Result<Vec<TranscriptEntrySkeleton>>;
    async fn get_web_transcript_detail(
        &self,
        address: &ChannelAddress,
        seq_start: usize,
        seq_end: usize,
    ) -> Result<Option<Vec<TranscriptEntry>>>;
}

impl WebChannel {
    pub(crate) fn resolve_auth_token(config: &WebChannelConfig) -> Option<String> {
        config
            .auth_token
            .as_deref()
            .map(str::trim)
            .filter(|token| !token.is_empty())
            .map(ToOwned::to_owned)
            .or_else(|| {
                let env_name = config.auth_token_env.trim();
                if env_name.is_empty() {
                    return None;
                }
                std::env::var(env_name)
                    .ok()
                    .map(|token| token.trim().to_string())
                    .filter(|token| !token.is_empty())
            })
    }

    pub fn from_config(config: WebChannelConfig, workdir: impl Into<PathBuf>) -> Result<Self> {
        let auth_token = Self::resolve_auth_token(&config);
        let (event_bus, _) = broadcast::channel(256);
        Ok(Self {
            inner: Arc::new(WebChannelInner {
                id: config.id,
                listen_addr: config.listen_addr,
                auth_token,
                auth_token_env: config.auth_token_env,
                workdir: workdir.into(),
                host: StdRwLock::new(None),
                incoming_sender: RwLock::new(None),
                event_bus,
            }),
        })
    }

    pub fn set_host(&self, host: Arc<dyn WebChannelHost>) -> Result<()> {
        let mut guard = self
            .inner
            .host
            .write()
            .map_err(|_| anyhow::anyhow!("web channel host lock poisoned"))?;
        *guard = Some(host);
        Ok(())
    }

    pub fn publish_transcript_append(
        &self,
        address: &ChannelAddress,
        entry: TranscriptEntrySkeleton,
    ) {
        let _ = self.inner.event_bus.send(WebSocketEvent::TranscriptAppend {
            conversation_key: address.conversation_id.clone(),
            entry,
        });
    }
}

#[async_trait]
impl Channel for WebChannel {
    fn id(&self) -> &str {
        &self.inner.id
    }

    async fn run(self: Arc<Self>, sender: mpsc::Sender<IncomingMessage>) -> Result<()> {
        if self.inner.auth_token.is_none() {
            tracing::warn!(
                log_stream = "channel",
                log_key = %self.inner.id,
                kind = "web_channel_disabled_missing_auth",
                auth_token_env = %self.inner.auth_token_env,
                "web channel disabled because no auth token is configured; set auth_token or auth_token_env before enabling the Web channel"
            );
            std::future::pending::<()>().await;
        }

        {
            let mut incoming_sender = self.inner.incoming_sender.write().await;
            *incoming_sender = Some(sender);
        }

        let listen_addr = self.inner.listen_addr.clone();
        tracing::info!(
            log_stream = "channel",
            log_key = %self.inner.id,
            kind = "web_channel_starting",
            listen_addr = %listen_addr,
            has_auth = self.inner.auth_token.is_some(),
            "web channel starting"
        );

        let listener = tokio::net::TcpListener::bind(&listen_addr)
            .await
            .with_context(|| format!("failed to bind web channel to {listen_addr}"))?;
        tracing::info!(
            log_stream = "channel",
            log_key = %self.inner.id,
            kind = "web_channel_listening",
            listen_addr = %listen_addr,
            "web channel listening"
        );

        axum::serve(listener, build_router(self.inner.clone()))
            .await
            .context("web channel server error")?;
        Ok(())
    }

    async fn send_media_group(
        &self,
        address: &ChannelAddress,
        images: Vec<OutgoingAttachment>,
    ) -> Result<()> {
        let _ = self.inner.event_bus.send(WebSocketEvent::MediaGroup {
            conversation_key: address.conversation_id.clone(),
            count: images.len(),
        });
        Ok(())
    }

    async fn send(&self, address: &ChannelAddress, message: OutgoingMessage) -> Result<()> {
        let images = message
            .images
            .iter()
            .filter_map(|attachment| web_attachment_ref(&self.inner, address, attachment).ok())
            .collect::<Vec<_>>();
        let attachments = message
            .attachments
            .iter()
            .filter_map(|attachment| web_attachment_ref(&self.inner, address, attachment).ok())
            .collect::<Vec<_>>();
        let option_count = message
            .options
            .as_ref()
            .map(|options| options.options.len())
            .unwrap_or(0);
        let text = message.text.unwrap_or_default();
        let options = message.options;
        let has_usage_chart = message.usage_chart.is_some();
        let _ = self.inner.event_bus.send(WebSocketEvent::OutgoingMessage {
            conversation_key: address.conversation_id.clone(),
            text,
            image_count: message.images.len(),
            attachment_count: message.attachments.len(),
            images,
            attachments,
            option_count,
            options,
            has_usage_chart,
        });
        Ok(())
    }

    async fn set_processing(&self, address: &ChannelAddress, state: ProcessingState) -> Result<()> {
        let _ = self.inner.event_bus.send(WebSocketEvent::Processing {
            conversation_key: address.conversation_id.clone(),
            state: match state {
                ProcessingState::Idle => "idle",
                ProcessingState::Typing => "typing",
            },
        });
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
        address: &ChannelAddress,
        feedback: ProgressFeedback,
    ) -> Result<ProgressFeedbackUpdate> {
        let _ = self.inner.event_bus.send(WebSocketEvent::Progress {
            conversation_key: address.conversation_id.clone(),
            turn_id: feedback.turn_id,
            text: feedback.text,
            important: feedback.important,
            final_state: feedback.final_state.map(|state| format!("{state:?}")),
        });
        Ok(ProgressFeedbackUpdate::Unchanged)
    }
}

#[derive(Clone, Debug, Serialize)]
#[serde(tag = "type")]
enum WebSocketEvent {
    #[serde(rename = "outgoing_message")]
    OutgoingMessage {
        conversation_key: String,
        text: String,
        image_count: usize,
        attachment_count: usize,
        images: Vec<WebAttachmentRef>,
        attachments: Vec<WebAttachmentRef>,
        option_count: usize,
        options: Option<ShowOptions>,
        has_usage_chart: bool,
    },
    #[serde(rename = "media_group")]
    MediaGroup {
        conversation_key: String,
        count: usize,
    },
    #[serde(rename = "processing")]
    Processing {
        conversation_key: String,
        state: &'static str,
    },
    #[serde(rename = "progress")]
    Progress {
        conversation_key: String,
        turn_id: String,
        text: String,
        important: bool,
        final_state: Option<String>,
    },
    #[serde(rename = "transcript_append")]
    TranscriptAppend {
        conversation_key: String,
        entry: TranscriptEntrySkeleton,
    },
    #[serde(rename = "transcript_detail")]
    TranscriptDetail {
        request_id: Option<String>,
        conversation_key: String,
        entries: Vec<TranscriptEntry>,
    },
    #[serde(rename = "transcript_error")]
    TranscriptError {
        request_id: Option<String>,
        conversation_key: Option<String>,
        message: String,
    },
}

#[derive(Clone, Debug, Serialize)]
struct WebAttachmentRef {
    source: &'static str,
    path: String,
    kind: String,
    caption: Option<String>,
}

fn build_router(state: Arc<WebChannelInner>) -> Router {
    Router::new()
        .route("/", get(serve_index))
        .route("/api/health", get(health))
        .route("/api/conversations", get(list_conversations))
        .route(
            "/api/conversation",
            get(get_conversation)
                .post(create_conversation)
                .put(update_conversation)
                .delete(delete_conversation),
        )
        .route("/api/attachment", get(get_attachment))
        .route("/api/send", post(send_message))
        .route("/api/transcript", get(list_transcript))
        .route("/api/transcript/detail", get(get_transcript_detail))
        .route("/ws", get(ws_handler))
        .route("/assets/app.js", get(serve_app_js))
        .route("/assets/style.css", get(serve_style_css))
        .layer(
            CorsLayer::new()
                .allow_origin(Any)
                .allow_methods([
                    Method::GET,
                    Method::POST,
                    Method::PUT,
                    Method::DELETE,
                    Method::OPTIONS,
                ])
                .allow_headers([header::AUTHORIZATION, header::CONTENT_TYPE]),
        )
        .with_state(state)
}

fn check_auth(
    state: &WebChannelInner,
    headers: &HeaderMap,
    query_token: Option<&str>,
) -> Result<(), StatusCode> {
    let Some(expected_token) = state.auth_token.as_deref() else {
        return Err(StatusCode::SERVICE_UNAVAILABLE);
    };

    let bearer_token = headers
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "));
    if bearer_token == Some(expected_token) || query_token == Some(expected_token) {
        Ok(())
    } else {
        Err(StatusCode::UNAUTHORIZED)
    }
}

async fn health() -> &'static str {
    "ok"
}

#[derive(Serialize)]
pub struct WebConversationSummary {
    pub conversation_key: String,
    pub entry_count: usize,
    pub latest_ts: Option<String>,
    pub latest_type: Option<TranscriptEntryType>,
    pub latest_summary: Option<String>,
    pub remote_execution: Option<RemoteExecutionBinding>,
    pub remote_execution_label: Option<String>,
}

#[derive(Deserialize)]
struct ConversationMutationRequest {
    conversation_key: Option<String>,
    remote_execution: Option<RemoteExecutionBinding>,
}

#[derive(Deserialize)]
struct ConversationLookupQuery {
    conversation_key: Option<String>,
}

#[derive(Serialize)]
struct DeleteConversationResponse {
    conversation_key: String,
    deleted: bool,
}

async fn list_conversations(
    State(state): State<Arc<WebChannelInner>>,
    headers: HeaderMap,
) -> Result<Json<Vec<WebConversationSummary>>, StatusCode> {
    check_auth(&state, &headers, None)?;
    let host = host_for_state(&state)?;
    let mut conversations = host
        .list_web_conversations(&state.id)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    conversations.sort_by(|left, right| right.latest_ts.cmp(&left.latest_ts));
    Ok(Json(conversations))
}

async fn get_conversation(
    State(state): State<Arc<WebChannelInner>>,
    Query(query): Query<ConversationLookupQuery>,
    headers: HeaderMap,
) -> Result<Json<WebConversationSummary>, StatusCode> {
    check_auth(&state, &headers, None)?;
    let conversation_key = normalize_required_conversation_key(query.conversation_key)
        .map_err(|_| StatusCode::BAD_REQUEST)?;
    let address = web_channel_address(&state, &conversation_key);
    let summary = host_for_state(&state)?
        .get_web_conversation(&address)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
        .ok_or(StatusCode::NOT_FOUND)?;
    Ok(Json(summary))
}

async fn create_conversation(
    State(state): State<Arc<WebChannelInner>>,
    headers: HeaderMap,
    Json(body): Json<ConversationMutationRequest>,
) -> Result<Json<WebConversationSummary>, (StatusCode, String)> {
    check_auth(&state, &headers, None).map_err(status_text)?;
    let conversation_key = normalize_or_generate_conversation_key(body.conversation_key)
        .map_err(error_text(StatusCode::BAD_REQUEST))?;
    let remote_execution = validate_requested_remote_execution(body.remote_execution)
        .map_err(error_text(StatusCode::BAD_REQUEST))?;
    let address = web_channel_address(&state, &conversation_key);
    let summary = host_for_state(&state)
        .map_err(status_text)?
        .create_web_conversation(&address, remote_execution)
        .await
        .map_err(error_text(StatusCode::INTERNAL_SERVER_ERROR))?;
    Ok(Json(summary))
}

async fn update_conversation(
    State(state): State<Arc<WebChannelInner>>,
    headers: HeaderMap,
    Json(body): Json<ConversationMutationRequest>,
) -> Result<Json<WebConversationSummary>, (StatusCode, String)> {
    check_auth(&state, &headers, None).map_err(status_text)?;
    let conversation_key = normalize_required_conversation_key(body.conversation_key)
        .map_err(error_text(StatusCode::BAD_REQUEST))?;
    let remote_execution = validate_requested_remote_execution(body.remote_execution)
        .map_err(error_text(StatusCode::BAD_REQUEST))?;
    let address = web_channel_address(&state, &conversation_key);
    let summary = host_for_state(&state)
        .map_err(status_text)?
        .update_web_conversation_remote_execution(&address, remote_execution)
        .await
        .map_err(error_text(StatusCode::INTERNAL_SERVER_ERROR))?;
    Ok(Json(summary))
}

async fn delete_conversation(
    State(state): State<Arc<WebChannelInner>>,
    headers: HeaderMap,
    Json(body): Json<ConversationMutationRequest>,
) -> Result<Json<DeleteConversationResponse>, (StatusCode, String)> {
    check_auth(&state, &headers, None).map_err(status_text)?;
    let conversation_key = normalize_required_conversation_key(body.conversation_key)
        .map_err(error_text(StatusCode::BAD_REQUEST))?;
    let address = web_channel_address(&state, &conversation_key);
    let deleted = host_for_state(&state)
        .map_err(status_text)?
        .delete_web_conversation(&address)
        .await
        .map_err(error_text(StatusCode::INTERNAL_SERVER_ERROR))?;
    Ok(Json(DeleteConversationResponse {
        conversation_key,
        deleted,
    }))
}

#[derive(Deserialize)]
struct AttachmentQuery {
    conversation_key: Option<String>,
    source: Option<String>,
    path: String,
    token: Option<String>,
}

async fn get_attachment(
    State(state): State<Arc<WebChannelInner>>,
    Query(query): Query<AttachmentQuery>,
    headers: HeaderMap,
) -> Result<Response, StatusCode> {
    check_auth(&state, &headers, query.token.as_deref())?;
    let path = resolve_attachment_path(
        &state,
        &query.conversation_key,
        query.source.as_deref().unwrap_or("workspace"),
        &query.path,
    )
    .map_err(|_| StatusCode::NOT_FOUND)?;
    let bytes = std::fs::read(&path).map_err(|_| StatusCode::NOT_FOUND)?;
    let content_type = infer_static_content_type(&path);
    Ok(([(header::CONTENT_TYPE, content_type)], bytes).into_response())
}

#[derive(Deserialize)]
struct SendMessageRequest {
    text: String,
    conversation_key: Option<String>,
}

async fn send_message(
    State(state): State<Arc<WebChannelInner>>,
    headers: HeaderMap,
    Json(body): Json<SendMessageRequest>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    check_auth(&state, &headers, None).map_err(status_text)?;
    let text = body.text.trim();
    if text.is_empty() {
        return Err((
            StatusCode::BAD_REQUEST,
            "text must not be empty".to_string(),
        ));
    }

    let incoming_sender = state.incoming_sender.read().await;
    let sender = incoming_sender.as_ref().ok_or((
        StatusCode::SERVICE_UNAVAILABLE,
        "web channel is not ready to accept incoming messages".to_string(),
    ))?;
    let conversation_key = normalize_or_default_conversation_key(body.conversation_key.as_deref())
        .map_err(error_text(StatusCode::BAD_REQUEST))?;
    let address = ChannelAddress {
        channel_id: state.id.clone(),
        conversation_id: conversation_key.clone(),
        user_id: Some("web-user".to_string()),
        display_name: Some("Web User".to_string()),
    };
    let conversation = host_for_state(&state)
        .map_err(status_text)?
        .get_web_conversation(&address)
        .await
        .map_err(error_text(StatusCode::INTERNAL_SERVER_ERROR))?;
    let Some(conversation) = conversation else {
        return Err((
            StatusCode::PRECONDITION_FAILED,
            "conversation is not configured yet; create it and bind a remote workspace first"
                .to_string(),
        ));
    };
    if conversation.remote_execution.is_none() {
        return Err((
            StatusCode::PRECONDITION_FAILED,
            "conversation is missing remote execution; bind a workspace before sending messages"
                .to_string(),
        ));
    }

    sender
        .send(IncomingMessage {
            remote_message_id: uuid::Uuid::new_v4().to_string(),
            address,
            text: Some(text.to_string()),
            attachments: Vec::new(),
            stored_attachments: Vec::new(),
            control: None,
        })
        .await
        .map_err(|error| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("failed to enqueue incoming message: {error}"),
            )
        })?;

    Ok(Json(serde_json::json!({
        "status": "sent",
        "conversation_key": conversation_key
    })))
}

#[derive(Deserialize)]
struct WebSocketQuery {
    token: Option<String>,
}

#[derive(Deserialize)]
#[serde(tag = "type")]
enum WebSocketClientRequest {
    #[serde(rename = "auth")]
    Auth { token: String },
    #[serde(rename = "transcript_detail")]
    TranscriptDetail {
        request_id: Option<String>,
        conversation_key: Option<String>,
        seq_start: usize,
        seq_end: usize,
    },
}

async fn ws_handler(
    State(state): State<Arc<WebChannelInner>>,
    Query(query): Query<WebSocketQuery>,
    headers: HeaderMap,
    ws: WebSocketUpgrade,
) -> Result<Response, StatusCode> {
    let query_token = query.token.as_deref();
    let pre_authenticated = match state.auth_token.as_deref() {
        Some(expected_token) if query_token == Some(expected_token) => true,
        Some(_) if query_token.is_some() => {
            check_auth(&state, &headers, query_token)?;
            true
        }
        Some(_) => false,
        None => return Err(StatusCode::SERVICE_UNAVAILABLE),
    };
    Ok(ws.on_upgrade(move |socket| handle_ws(socket, state, pre_authenticated)))
}

async fn handle_ws(
    mut socket: ws::WebSocket,
    state: Arc<WebChannelInner>,
    pre_authenticated: bool,
) {
    let Some(expected_token) = state.auth_token.as_deref() else {
        let _ = socket.close().await;
        return;
    };

    if !pre_authenticated {
        let authed = match socket.recv().await {
            Some(Ok(ws::Message::Text(payload))) => {
                serde_json::from_str::<Value>(&payload)
                    .ok()
                    .and_then(|value| {
                        (value.get("type").and_then(Value::as_str) == Some("auth"))
                            .then(|| {
                                value
                                    .get("token")
                                    .and_then(Value::as_str)
                                    .map(str::to_string)
                            })
                            .flatten()
                    })
                    .as_deref()
                    == Some(expected_token)
            }
            _ => false,
        };
        if !authed {
            let _ = socket.close().await;
            return;
        }
    }

    let mut events = state.event_bus.subscribe();
    loop {
        tokio::select! {
            event = events.recv() => {
                match event {
                    Ok(event) => {
                        let Ok(payload) = serde_json::to_string(&event) else {
                            continue;
                        };
                        if socket.send(ws::Message::Text(payload.into())).await.is_err() {
                            break;
                        }
                    }
                    Err(broadcast::error::RecvError::Lagged(count)) => {
                        tracing::warn!(
                            log_stream = "channel",
                            log_key = %state.id,
                            kind = "web_channel_ws_lagged",
                            lagged_count = count,
                            "web channel websocket client lagged"
                        );
                    }
                    Err(broadcast::error::RecvError::Closed) => break,
                }
            }
            message = socket.recv() => match message {
                Some(Ok(ws::Message::Text(payload))) => {
                    if handle_ws_client_text(&mut socket, &state, &payload).await.is_err() {
                        break;
                    }
                }
                Some(Ok(ws::Message::Ping(payload))) => {
                    let _ = socket.send(ws::Message::Pong(payload)).await;
                }
                Some(Ok(ws::Message::Close(_))) | None => break,
                _ => {}
            }
        }
    }
}

async fn handle_ws_client_text(
    socket: &mut ws::WebSocket,
    state: &Arc<WebChannelInner>,
    payload: &str,
) -> Result<(), ()> {
    let request = match serde_json::from_str::<WebSocketClientRequest>(payload) {
        Ok(request) => request,
        Err(error) => {
            let event = WebSocketEvent::TranscriptError {
                request_id: None,
                conversation_key: None,
                message: format!("bad websocket request: {error}"),
            };
            send_ws_event(socket, &event).await?;
            return Ok(());
        }
    };

    match request {
        WebSocketClientRequest::Auth { token } => {
            let _ = token.len();
            Ok(())
        }
        WebSocketClientRequest::TranscriptDetail {
            request_id,
            conversation_key,
            seq_start,
            seq_end,
        } => {
            let conversation_key =
                match normalize_or_default_conversation_key(conversation_key.as_deref()) {
                    Ok(value) => value,
                    Err(error) => {
                        let event = WebSocketEvent::TranscriptError {
                            request_id,
                            conversation_key: conversation_key.map(|value| value.to_string()),
                            message: format!("{error:#}"),
                        };
                        return send_ws_event(socket, &event).await;
                    }
                };
            let address = web_channel_address(state, &conversation_key);
            let event = match host_for_state(state) {
                Ok(host) => match host
                    .get_web_transcript_detail(&address, seq_start, seq_end)
                    .await
                {
                    Ok(Some(entries)) => WebSocketEvent::TranscriptDetail {
                        request_id,
                        conversation_key,
                        entries,
                    },
                    Ok(None) => WebSocketEvent::TranscriptError {
                        request_id,
                        conversation_key: Some(conversation_key),
                        message: "conversation transcript not found".to_string(),
                    },
                    Err(error) => WebSocketEvent::TranscriptError {
                        request_id,
                        conversation_key: Some(conversation_key),
                        message: format!("{error:#}"),
                    },
                },
                Err(status) => WebSocketEvent::TranscriptError {
                    request_id,
                    conversation_key: Some(conversation_key),
                    message: format!("web host unavailable: {status}"),
                },
            };
            send_ws_event(socket, &event).await
        }
    }
}

async fn send_ws_event(socket: &mut ws::WebSocket, event: &WebSocketEvent) -> Result<(), ()> {
    let payload = serde_json::to_string(event).map_err(|_| ())?;
    socket
        .send(ws::Message::Text(payload.into()))
        .await
        .map_err(|_| ())
}

const INDEX_HTML: &str = include_str!("web_static/index.html");
const APP_JS: &str = include_str!("web_static/app.js");
const STYLE_CSS: &str = include_str!("web_static/style.css");

async fn serve_index() -> Html<&'static str> {
    Html(INDEX_HTML)
}

async fn serve_app_js() -> Response {
    ([(header::CONTENT_TYPE, "application/javascript")], APP_JS).into_response()
}

async fn serve_style_css() -> Response {
    ([(header::CONTENT_TYPE, "text/css")], STYLE_CSS).into_response()
}

#[derive(Deserialize)]
struct TranscriptListQuery {
    conversation_key: Option<String>,
    offset: Option<usize>,
    limit: Option<usize>,
}

async fn list_transcript(
    State(state): State<Arc<WebChannelInner>>,
    Query(query): Query<TranscriptListQuery>,
    headers: HeaderMap,
) -> Result<Json<Vec<TranscriptEntrySkeleton>>, StatusCode> {
    check_auth(&state, &headers, query_token(&query.conversation_key))?;
    let conversation_key = normalize_or_default_conversation_key(query.conversation_key.as_deref())
        .map_err(|_| StatusCode::BAD_REQUEST)?;
    let address = web_channel_address(&state, &conversation_key);
    let entries = host_for_state(&state)?
        .list_web_transcript(
            &address,
            query.offset.unwrap_or(0),
            query.limit.unwrap_or(50).min(200),
        )
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    Ok(Json(entries))
}

#[derive(Deserialize)]
struct TranscriptDetailQuery {
    conversation_key: Option<String>,
    seq_start: usize,
    seq_end: usize,
}

async fn get_transcript_detail(
    State(state): State<Arc<WebChannelInner>>,
    Query(query): Query<TranscriptDetailQuery>,
    headers: HeaderMap,
) -> Result<Json<Vec<TranscriptEntry>>, StatusCode> {
    check_auth(&state, &headers, query_token(&query.conversation_key))?;
    let conversation_key = normalize_or_default_conversation_key(query.conversation_key.as_deref())
        .map_err(|_| StatusCode::BAD_REQUEST)?;
    let address = web_channel_address(&state, &conversation_key);
    let entries = host_for_state(&state)?
        .get_web_transcript_detail(&address, query.seq_start, query.seq_end)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
        .ok_or(StatusCode::NOT_FOUND)?;
    Ok(Json(entries))
}

fn query_token(_conversation_key: &Option<String>) -> Option<&str> {
    None
}

fn host_for_state(state: &WebChannelInner) -> Result<Arc<dyn WebChannelHost>, StatusCode> {
    state
        .host
        .read()
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
        .clone()
        .ok_or(StatusCode::SERVICE_UNAVAILABLE)
}

fn web_channel_address(state: &WebChannelInner, conversation_key: &str) -> ChannelAddress {
    ChannelAddress {
        channel_id: state.id.clone(),
        conversation_id: conversation_key.to_string(),
        user_id: Some("web-user".to_string()),
        display_name: Some("Web User".to_string()),
    }
}

fn normalize_or_generate_conversation_key(value: Option<String>) -> Result<String> {
    match value {
        Some(value) if !value.trim().is_empty() => normalize_conversation_key(&value),
        _ => Ok(format!("web-{}", uuid::Uuid::new_v4().simple())),
    }
}

fn validate_requested_remote_execution(
    remote_execution: Option<RemoteExecutionBinding>,
) -> Result<RemoteExecutionBinding> {
    let remote_execution =
        remote_execution.ok_or_else(|| anyhow::anyhow!("remote_execution is required"))?;
    match remote_execution {
        RemoteExecutionBinding::Local { path } => {
            let validated = validate_local_execution_path(&path.to_string_lossy())?;
            Ok(RemoteExecutionBinding::Local { path: validated })
        }
        RemoteExecutionBinding::Ssh { host, path } => {
            let (host, path) = validate_ssh_execution_binding(&host, &path)?;
            Ok(RemoteExecutionBinding::Ssh { host, path })
        }
    }
}

fn error_text(
    status: StatusCode,
) -> impl FnOnce(anyhow::Error) -> (StatusCode, String) + Copy + Send + Sync + 'static {
    move |error| (status, format!("{error:#}"))
}

fn status_text(status: StatusCode) -> (StatusCode, String) {
    (
        status,
        status
            .canonical_reason()
            .unwrap_or("request failed")
            .to_string(),
    )
}

fn normalize_required_conversation_key(value: Option<String>) -> Result<String> {
    let Some(value) = value else {
        anyhow::bail!("conversation_key is required");
    };
    normalize_conversation_key(&value)
}

fn normalize_or_default_conversation_key(value: Option<&str>) -> Result<String> {
    match value {
        Some(value) if !value.trim().is_empty() => normalize_conversation_key(value),
        _ => Ok("web-default".to_string()),
    }
}

fn normalize_conversation_key(value: &str) -> Result<String> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        anyhow::bail!("conversation_key is empty");
    }
    validate_conversation_id(trimmed)
        .map_err(|error| anyhow::anyhow!("invalid conversation_key: {error}"))?;
    Ok(trimmed.to_string())
}

fn web_attachment_ref(
    state: &WebChannelInner,
    address: &ChannelAddress,
    attachment: &OutgoingAttachment,
) -> Result<WebAttachmentRef> {
    let path = attachment.path.canonicalize().with_context(|| {
        format!(
            "failed to canonicalize web attachment {}",
            attachment.path.display()
        )
    })?;
    let conversation_key = Some(address.conversation_id.clone());
    if let Some(conversation_root) =
        find_conversation_root(&state.workdir, &state.id, &conversation_key)?
        && let Some(workspace_root) =
            conversation_workspace_root(&state.workdir, &conversation_root)?
        && let Ok(relative) = path.strip_prefix(workspace_root.canonicalize()?)
    {
        return Ok(WebAttachmentRef {
            source: "workspace",
            path: relative.to_string_lossy().to_string(),
            kind: format!("{:?}", attachment.kind),
            caption: attachment.caption.clone(),
        });
    }
    if let Some(session_root) = find_session_root(&state.workdir, &state.id, &conversation_key)?
        && let Ok(relative) = path.strip_prefix(session_root.canonicalize()?)
    {
        return Ok(WebAttachmentRef {
            source: "session",
            path: relative.to_string_lossy().to_string(),
            kind: format!("{:?}", attachment.kind),
            caption: attachment.caption.clone(),
        });
    }
    anyhow::bail!("web attachment is outside known conversation roots");
}

fn resolve_attachment_path(
    state: &WebChannelInner,
    conversation_key: &Option<String>,
    source: &str,
    raw_path: &str,
) -> Result<PathBuf> {
    let safe_path = safe_relative_path(raw_path)?;
    let root = match source {
        "workspace" => {
            let conversation_root =
                find_conversation_root(&state.workdir, &state.id, conversation_key)?
                    .context("conversation not found")?;
            conversation_workspace_root(&state.workdir, &conversation_root)?
                .context("conversation workspace not found")?
        }
        "session" => find_session_root(&state.workdir, &state.id, conversation_key)?
            .context("session not found")?,
        _ => anyhow::bail!("unknown attachment source"),
    };
    let canonical_root = root
        .canonicalize()
        .with_context(|| format!("failed to canonicalize {}", root.display()))?;
    let candidate = canonical_root.join(safe_path);
    let canonical_candidate = candidate
        .canonicalize()
        .with_context(|| format!("attachment path does not exist: {}", candidate.display()))?;
    if !canonical_candidate.starts_with(&canonical_root) {
        anyhow::bail!("attachment path escapes root");
    }
    Ok(canonical_candidate)
}

fn safe_relative_path(raw_path: &str) -> Result<PathBuf> {
    let path = Path::new(raw_path);
    if path.is_absolute() {
        anyhow::bail!("absolute attachment paths are not allowed");
    }
    for component in path.components() {
        match component {
            std::path::Component::Normal(_) => {}
            _ => anyhow::bail!("unsafe attachment path"),
        }
    }
    Ok(path.to_path_buf())
}

fn conversation_workspace_root(
    workdir: &Path,
    conversation_root: &Path,
) -> Result<Option<PathBuf>> {
    let value = read_conversation_state(conversation_root)?;
    if let Some(remote_execution) = value
        .get("settings")
        .and_then(|settings| settings.get("remote_execution"))
        .and_then(Value::as_object)
    {
        let conversation_id = conversation_root
            .file_name()
            .map(|value| value.to_string_lossy().to_string())
            .context("conversation root is missing a directory name")?;
        let kind = remote_execution
            .get("kind")
            .and_then(Value::as_str)
            .unwrap_or_default();
        match kind {
            "local" => {
                if let Some(path) = remote_execution.get("path").and_then(Value::as_str) {
                    return Ok(Some(PathBuf::from(path)));
                }
            }
            "ssh" => {
                return Ok(Some(
                    workdir
                        .join("remote_mounts")
                        .join(conversation_id)
                        .join("workspace"),
                ));
            }
            _ => {}
        }
    }
    let workspace_id = value
        .get("settings")
        .and_then(|settings| settings.get("workspace_id"))
        .and_then(Value::as_str)
        .filter(|value| !value.trim().is_empty());
    Ok(
        workspace_id
            .map(|workspace_id| workdir.join("workspaces").join(workspace_id).join("files")),
    )
}

fn conversation_sessions_root(workdir: &Path, conversation_root: &Path) -> Result<Option<PathBuf>> {
    let value = read_conversation_state(conversation_root)?;
    let remote_execution_active = value
        .get("settings")
        .and_then(|settings| settings.get("remote_execution"))
        .is_some_and(|value| !value.is_null());
    if !remote_execution_active {
        return Ok(Some(workdir.join("sessions")));
    }
    let Some(workspace_root) = conversation_workspace_root(workdir, conversation_root)? else {
        return Ok(None);
    };
    Ok(Some(
        storage_root_for_execution_root(&workspace_root).join("sessions"),
    ))
}

fn read_conversation_state(conversation_root: &Path) -> Result<Value> {
    let state_path = conversation_root.join("conversation.json");
    let raw = std::fs::read_to_string(&state_path)
        .with_context(|| format!("failed to read {}", state_path.display()))?;
    serde_json::from_str(&raw).with_context(|| format!("failed to parse {}", state_path.display()))
}

fn infer_static_content_type(path: &Path) -> &'static str {
    match path.extension().and_then(OsStr::to_str).unwrap_or("") {
        "apng" => "image/apng",
        "avif" => "image/avif",
        "gif" => "image/gif",
        "jpg" | "jpeg" => "image/jpeg",
        "png" => "image/png",
        "svg" => "image/svg+xml",
        "webp" => "image/webp",
        "pdf" => "application/pdf",
        "txt" | "log" | "md" => "text/plain; charset=utf-8",
        "json" => "application/json",
        _ => "application/octet-stream",
    }
}

pub(crate) fn summarize_skeleton(entry: &TranscriptEntrySkeleton) -> Option<String> {
    match entry.entry_type {
        TranscriptEntryType::UserMessage | TranscriptEntryType::AssistantMessage => {
            entry.text.clone()
        }
        TranscriptEntryType::ModelCall => {
            let tools = if entry.tool_call_names.is_empty() {
                "no tools".to_string()
            } else {
                entry.tool_call_names.join(", ")
            };
            Some(format!(
                "API round {} ({tools})",
                entry.round.unwrap_or_default()
            ))
        }
        TranscriptEntryType::ToolResult => Some(format!(
            "tool {} ({} bytes)",
            entry.tool_name.as_deref().unwrap_or("unknown"),
            entry.output_len.unwrap_or_default()
        )),
        TranscriptEntryType::Compaction => Some("compaction".to_string()),
    }
}

fn find_conversation_root(
    workdir: &Path,
    channel_id: &str,
    conversation_key: &Option<String>,
) -> Result<Option<PathBuf>> {
    let conversation_key = normalize_or_default_conversation_key(conversation_key.as_deref())?;
    let conversations_root = workdir.join("conversations");
    if !conversations_root.is_dir() {
        return Ok(None);
    }
    for entry in std::fs::read_dir(&conversations_root)
        .with_context(|| format!("failed to read {}", conversations_root.display()))?
    {
        let root = entry?.path();
        let state_path = root.join("conversation.json");
        if !state_path.is_file() {
            continue;
        }
        let raw = std::fs::read_to_string(&state_path)
            .with_context(|| format!("failed to read {}", state_path.display()))?;
        let value: Value = serde_json::from_str(&raw)
            .with_context(|| format!("failed to parse {}", state_path.display()))?;
        let address = value.get("address").and_then(Value::as_object);
        let matches_channel = address
            .and_then(|address| address.get("channel_id"))
            .and_then(Value::as_str)
            == Some(channel_id);
        let matches_conversation = address
            .and_then(|address| address.get("conversation_id"))
            .and_then(Value::as_str)
            == Some(conversation_key.as_str());
        if matches_channel && matches_conversation {
            return Ok(Some(root));
        }
    }
    Ok(None)
}

fn find_session_root(
    workdir: &Path,
    channel_id: &str,
    conversation_key: &Option<String>,
) -> Result<Option<PathBuf>> {
    let conversation_key = normalize_or_default_conversation_key(conversation_key.as_deref())?;
    let sessions_root = if let Some(conversation_root) =
        find_conversation_root(workdir, channel_id, &Some(conversation_key.clone()))?
    {
        conversation_sessions_root(workdir, &conversation_root)?
            .unwrap_or_else(|| workdir.join("sessions"))
    } else {
        workdir.join("sessions")
    };
    if !sessions_root.is_dir() {
        return Ok(None);
    }
    for session_root in crate::session::find_session_roots(&sessions_root)? {
        let session_path = session_root.join("session.json");
        let raw = std::fs::read_to_string(&session_path)
            .with_context(|| format!("failed to read {}", session_path.display()))?;
        let value: Value = serde_json::from_str(&raw)
            .with_context(|| format!("failed to parse {}", session_path.display()))?;
        let address = value.get("address").and_then(Value::as_object);
        let matches_channel = address
            .and_then(|address| address.get("channel_id"))
            .and_then(Value::as_str)
            == Some(channel_id);
        let matches_conversation = address
            .and_then(|address| address.get("conversation_id"))
            .and_then(Value::as_str)
            == Some(conversation_key.as_str());
        if matches_channel && matches_conversation {
            return Ok(Some(session_root));
        }
    }
    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn state_with_token(token: Option<&str>) -> WebChannelInner {
        state_with_workdir(token, PathBuf::new())
    }

    fn state_with_workdir(token: Option<&str>, workdir: PathBuf) -> WebChannelInner {
        let (event_bus, _) = broadcast::channel(1);
        WebChannelInner {
            id: "web".to_string(),
            listen_addr: "127.0.0.1:0".to_string(),
            auth_token: token.map(ToOwned::to_owned),
            auth_token_env: "CLAWPARTY_WEB_AUTH_TOKEN".to_string(),
            workdir,
            host: StdRwLock::new(None),
            incoming_sender: RwLock::new(None),
            event_bus,
        }
    }

    #[test]
    fn auth_rejects_missing_token_configuration() {
        let headers = HeaderMap::new();
        assert_eq!(
            check_auth(&state_with_token(None), &headers, None).unwrap_err(),
            StatusCode::SERVICE_UNAVAILABLE
        );
    }

    #[test]
    fn auth_accepts_bearer_and_query_token() {
        let state = state_with_token(Some("secret"));
        let mut headers = HeaderMap::new();
        headers.insert(header::AUTHORIZATION, "Bearer secret".parse().unwrap());
        assert!(check_auth(&state, &headers, None).is_ok());

        let headers = HeaderMap::new();
        assert!(check_auth(&state, &headers, Some("secret")).is_ok());
        assert_eq!(
            check_auth(&state, &headers, Some("wrong")).unwrap_err(),
            StatusCode::UNAUTHORIZED
        );
    }

    #[test]
    fn web_channel_resolves_literal_auth_and_ignores_blank_values() {
        let config = WebChannelConfig {
            id: "web".to_string(),
            listen_addr: "127.0.0.1:0".to_string(),
            auth_token: Some("  secret  ".to_string()),
            auth_token_env: String::new(),
        };
        assert_eq!(
            WebChannel::resolve_auth_token(&config).as_deref(),
            Some("secret")
        );

        let config = WebChannelConfig {
            id: "web".to_string(),
            listen_addr: "127.0.0.1:0".to_string(),
            auth_token: Some("   ".to_string()),
            auth_token_env: String::new(),
        };
        assert!(WebChannel::resolve_auth_token(&config).is_none());
    }

    #[test]
    fn outgoing_message_serializes_show_options() {
        let event = WebSocketEvent::OutgoingMessage {
            conversation_key: "web-default".to_string(),
            text: "This conversation has no model yet.".to_string(),
            image_count: 0,
            attachment_count: 0,
            images: Vec::new(),
            attachments: Vec::new(),
            option_count: 2,
            options: Some(ShowOptions {
                prompt: "Choose a model below or send /agent <model>.".to_string(),
                options: vec![
                    crate::domain::ShowOption {
                        label: "opus-4.6".to_string(),
                        value: "/agent opus-4.6".to_string(),
                    },
                    crate::domain::ShowOption {
                        label: "gpt54".to_string(),
                        value: "/agent gpt54".to_string(),
                    },
                ],
                one_time: true,
            }),
            has_usage_chart: false,
        };

        let payload = serde_json::to_value(event).unwrap();
        assert_eq!(payload["type"], "outgoing_message");
        assert_eq!(payload["option_count"], 2);
        assert_eq!(
            payload["options"]["prompt"],
            "Choose a model below or send /agent <model>."
        );
        assert_eq!(payload["options"]["options"][0]["label"], "opus-4.6");
        assert_eq!(payload["options"]["options"][0]["value"], "/agent opus-4.6");
    }

    #[test]
    fn bundled_web_client_supports_markdown_tables() {
        assert!(APP_JS.contains("function renderMarkdownTable"));
        assert!(APP_JS.contains("function isMarkdownTableStart"));
        assert!(APP_JS.contains("markdown-table-wrap"));
        assert!(STYLE_CSS.contains(".markdown table"));
        assert!(STYLE_CSS.contains(".markdown th"));
        assert!(STYLE_CSS.contains(".markdown td"));
    }

    #[test]
    fn bundled_web_client_supports_markdown_horizontal_rules() {
        assert!(APP_JS.contains("function isMarkdownHorizontalRule"));
        assert!(APP_JS.contains("document.createElement('hr')"));
        assert!(STYLE_CSS.contains(".markdown hr"));
    }

    #[test]
    fn bundled_web_client_handles_ime_and_authoritative_user_echo() {
        assert!(APP_JS.contains("compositionstart"));
        assert!(APP_JS.contains("compositionend"));
        assert!(APP_JS.contains("e.isComposing || composingInput || e.keyCode === 229"));
        assert!(!APP_JS.contains("appendMessage('user', text);"));
        assert!(APP_JS.contains("loadTranscriptPage('latest');"));
    }

    #[test]
    fn bundled_web_client_preserves_manual_history_scroll_position() {
        assert!(APP_JS.contains("const shouldStick = autoStickToBottom || isNearBottom(160);"));
        assert!(APP_JS.contains("if (shouldStick) scrollToBottom();"));
    }

    #[test]
    fn bundled_web_client_routes_main_panel_wheel_scroll_into_messages() {
        assert!(APP_JS.contains("mainEl.addEventListener('wheel'"));
        assert!(APP_JS.contains("nearestNestedScrollable"));
        assert!(STYLE_CSS.contains("#chat-shell {"));
        assert!(STYLE_CSS.contains("height: 100vh;"));
        assert!(STYLE_CSS.contains("overscroll-behavior: contain;"));
    }

    #[test]
    fn missing_host_returns_service_unavailable() {
        let state = state_with_token(None);
        let result = host_for_state(&state);
        assert!(result.is_err());
        assert_eq!(result.err(), Some(StatusCode::SERVICE_UNAVAILABLE));
    }

    #[test]
    fn normalize_conversation_key_rejects_special_characters() {
        for value in ["..", ".", "web/default", "web default", "abc@def"] {
            assert!(
                normalize_conversation_key(value).is_err(),
                "{value} should be rejected"
            );
        }
        assert_eq!(
            normalize_conversation_key("web-default_123").unwrap(),
            "web-default_123"
        );
    }

    #[test]
    fn requested_remote_execution_validates_local_and_ssh_bindings() {
        let local = validate_requested_remote_execution(Some(RemoteExecutionBinding::Local {
            path: PathBuf::from("/srv/project"),
        }))
        .unwrap();
        assert_eq!(
            local,
            RemoteExecutionBinding::Local {
                path: PathBuf::from("/srv/project"),
            }
        );

        let ssh = validate_requested_remote_execution(Some(RemoteExecutionBinding::Ssh {
            host: "demo-host".to_string(),
            path: "~/repo".to_string(),
        }))
        .unwrap();
        assert_eq!(
            ssh,
            RemoteExecutionBinding::Ssh {
                host: "demo-host".to_string(),
                path: "~/repo".to_string(),
            }
        );

        assert!(validate_requested_remote_execution(None).is_err());
        assert!(
            validate_requested_remote_execution(Some(RemoteExecutionBinding::Local {
                path: PathBuf::from("relative/path"),
            }))
            .is_err()
        );
        assert!(
            validate_requested_remote_execution(Some(RemoteExecutionBinding::Ssh {
                host: "".to_string(),
                path: "".to_string(),
            }))
            .is_err()
        );
    }

    #[test]
    fn bundled_web_client_exposes_remote_workspace_controls() {
        assert!(INDEX_HTML.contains("workspace-kind-input"));
        assert!(INDEX_HTML.contains("bind-workspace-btn"));
        assert!(APP_JS.contains("bindCurrentConversation"));
        assert!(APP_JS.contains("/api/conversation"));
        assert!(APP_JS.contains("remote_execution"));
        assert!(APP_JS.contains("http://127.0.0.1:8080"));
    }

    #[test]
    fn bundled_web_client_persists_server_settings_draft_on_input() {
        assert!(APP_JS.contains("function persistServerSettingsDraft"));
        assert!(
            APP_JS.contains(
                "serverUrlInputEl.addEventListener('input', persistServerSettingsDraft);"
            )
        );
        assert!(
            APP_JS.contains(
                "authTokenInputEl.addEventListener('input', persistServerSettingsDraft);"
            )
        );
        assert!(APP_JS.contains("localStorage.setItem(SERVER_KEY, draftServerBase);"));
        assert!(APP_JS.contains("localStorage.setItem(AUTH_KEY, draftToken);"));
    }

    #[test]
    fn partx_electron_shell_is_scaffolded() {
        let package = std::fs::read_to_string(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../apps/partx/package.json"
        ))
        .unwrap();
        let main = std::fs::read_to_string(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../apps/partx/main.js"
        ))
        .unwrap();
        let renderer = std::fs::read_to_string(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../apps/partx/renderer/index.html"
        ))
        .unwrap();

        assert!(package.contains("\"electron\""));
        assert!(main.contains("BrowserWindow"));
        assert!(renderer.contains("agent_host/src/channels/web_static/app.js"));
    }
}
