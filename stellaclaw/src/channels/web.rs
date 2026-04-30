use std::{
    cmp::Ordering,
    collections::HashMap,
    fs,
    io::{Read, Write},
    net::{Shutdown, TcpListener, TcpStream},
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use anyhow::{anyhow, Context, Result};
use base64::{engine::general_purpose, Engine as _};
use crossbeam_channel::{bounded, unbounded, RecvTimeoutError, Sender};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha1::{Digest, Sha1};
use stellaclaw_core::{
    model_config::{ModelCapability, ProviderType},
    session_actor::{ChatMessage, ChatMessageItem, ChatRole, FileItem, TokenUsage},
};
use time::{format_description::well_known::Rfc3339, OffsetDateTime};

use crate::{
    cache::{CacheManager, CachedThumbnail},
    config::{ModelSelection, StellaclawConfig},
    conversation::{
        attachment_marker, extract_attachment_references_with_markers,
        load_conversation_status_snapshot, load_or_create_conversation_state,
        persist_conversation_state, strip_attachment_tags, ConversationControl, ConversationState,
        IncomingConversationMessage,
    },
    conversation_id_manager::ConversationIdManager,
    logger::StellaclawLogger,
    remote_actor::{
        download_workspace_archive, list_workspace_entries, read_workspace_file,
        upload_workspace_archive, RemoteActorError,
    },
    workspace::{is_sshfs_workspace_entry_name, sshfs_workspace_root},
};

use super::{
    types::{
        IncomingDispatch, OutgoingAttachment, OutgoingAttachmentKind, OutgoingDelivery,
        OutgoingError, OutgoingProgressFeedback, OutgoingStatus, ProcessingState,
        ProgressFeedbackFinalState, TurnProgressPhase,
    },
    web_terminal::{
        output_limit, TerminalAttach, TerminalCreateRequest, TerminalInputRequest, TerminalManager,
        TerminalReplay, TerminalResizeRequest, WebTerminalError,
    },
    Channel,
};

const MAX_HEADER_BYTES: usize = 64 * 1024;
const MAX_BODY_BYTES: usize = 11 * 1024 * 1024;
const DEFAULT_WORKSPACE_FILE_LIMIT_BYTES: usize = 1024 * 1024;
const MAX_WORKSPACE_FILE_LIMIT_BYTES: usize = 5 * 1024 * 1024;
const WEBSOCKET_GUID: &str = "258EAFA5-E914-47DA-95CA-C5AB0DC85B11";
const WEBSOCKET_MAX_FRAME_BYTES: usize = 64 * 1024;
const WEBSOCKET_POLL_INTERVAL: Duration = Duration::from_millis(250);
const WEBSOCKET_HEARTBEAT_INTERVAL: Duration = Duration::from_secs(30);
const TERMINAL_WEBSOCKET_WRITE_TIMEOUT: Duration = Duration::from_secs(10);
const TERMINAL_WEBSOCKET_EVENT_CAPACITY: usize = 256;

pub struct WebChannel {
    id: String,
    bind_addr: String,
    token: String,
    workdir: PathBuf,
    config: Arc<StellaclawConfig>,
    logger: Arc<StellaclawLogger>,
    terminal_manager: Arc<TerminalManager>,
    cache_manager: Arc<CacheManager>,
    websocket_subscribers: Arc<Mutex<HashMap<String, Vec<WebSocketSubscriber>>>>,
    conversation_stream_subscribers: Arc<Mutex<Vec<Sender<Value>>>>,
    processing_states: Arc<Mutex<HashMap<String, ProcessingState>>>,
    seen_states: Arc<Mutex<HashMap<String, ConversationSeen>>>,
}

#[derive(Clone)]
struct WebSocketSubscriber {
    conversation_id: String,
    sender: Sender<Value>,
}

impl WebChannel {
    pub fn new(
        id: String,
        bind_addr: String,
        token: String,
        workdir: PathBuf,
        config: Arc<StellaclawConfig>,
        logger: Arc<StellaclawLogger>,
    ) -> Self {
        let cache_manager = Arc::new(CacheManager::new(workdir.clone()));
        let _ = cache_manager.ensure_layout();
        Self {
            id,
            bind_addr,
            token,
            workdir: workdir.clone(),
            config,
            logger,
            terminal_manager: Arc::new(TerminalManager::new()),
            cache_manager,
            websocket_subscribers: Arc::new(Mutex::new(HashMap::new())),
            conversation_stream_subscribers: Arc::new(Mutex::new(Vec::new())),
            processing_states: Arc::new(Mutex::new(HashMap::new())),
            seen_states: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    fn run(
        self: Arc<Self>,
        dispatch_tx: Sender<IncomingDispatch>,
        id_manager: Arc<Mutex<ConversationIdManager>>,
        logger: Arc<StellaclawLogger>,
    ) -> Result<()> {
        let listener = TcpListener::bind(&self.bind_addr)
            .with_context(|| format!("failed to bind web channel {}", self.bind_addr))?;
        logger.info(
            "web_channel_listening",
            json!({"channel_id": self.id, "bind_addr": self.bind_addr}),
        );
        for stream in listener.incoming() {
            match stream {
                Ok(stream) => {
                    let channel = self.clone();
                    let dispatch_tx = dispatch_tx.clone();
                    let id_manager = id_manager.clone();
                    thread::spawn(move || {
                        if let Err(error) = channel.handle_stream(stream, dispatch_tx, id_manager) {
                            channel.logger.warn(
                                "web_request_failed",
                                json!({"channel_id": channel.id, "error": format!("{error:#}")}),
                            );
                        }
                    });
                }
                Err(error) => {
                    logger.warn(
                        "web_accept_failed",
                        json!({"channel_id": self.id, "error": error.to_string()}),
                    );
                }
            }
        }
        Ok(())
    }

    fn handle_stream(
        &self,
        mut stream: TcpStream,
        dispatch_tx: Sender<IncomingDispatch>,
        id_manager: Arc<Mutex<ConversationIdManager>>,
    ) -> Result<()> {
        let request = read_http_request(&mut stream)?;
        if request.is_websocket_upgrade() {
            return self.handle_websocket_stream(stream, request);
        }
        let response = self.handle_request(request, dispatch_tx, id_manager);
        write_http_response(&mut stream, response)
    }

    fn handle_request(
        &self,
        request: HttpRequest,
        dispatch_tx: Sender<IncomingDispatch>,
        id_manager: Arc<Mutex<ConversationIdManager>>,
    ) -> HttpResponse {
        if request.method == "OPTIONS" {
            return HttpResponse::empty(204);
        }
        if !self.authorized(&request) {
            return json_error(401, "unauthorized");
        }
        if !request.path.starts_with("/api/") && request.path != "/api" {
            return json_error(404, "not_found");
        }

        match self.route_request(request, dispatch_tx, id_manager) {
            Ok(response) => response,
            Err(ApiError { status, message }) => json_error(status, &message),
        }
    }

    fn route_request(
        &self,
        request: HttpRequest,
        dispatch_tx: Sender<IncomingDispatch>,
        id_manager: Arc<Mutex<ConversationIdManager>>,
    ) -> ApiResult<HttpResponse> {
        let segments = api_segments(&request.path);
        match (request.method.as_str(), segments.as_slice()) {
            ("GET", ["models"]) => self.list_models(),
            ("GET", ["conversations"]) => self.list_conversations(&request.query),
            ("GET", ["conversations", "seen"]) => self.list_conversation_seen(),
            ("POST", ["conversations"]) => self.create_conversation(&request.body, id_manager),
            ("PATCH", ["conversations", conversation_id]) => {
                self.update_conversation(conversation_id, &request.body)
            }
            ("POST", ["conversations", conversation_id, "seen"]) => {
                self.mark_conversation_seen(conversation_id, &request.body)
            }
            ("GET", ["conversations", conversation_id, "messages"]) => {
                self.list_messages(conversation_id, &request.query)
            }
            ("GET", ["conversations", conversation_id, "messages", "after", message_id]) => {
                self.list_messages_after(conversation_id, message_id, &request.query)
            }
            ("GET", ["conversations", conversation_id, "messages", "range"]) => {
                self.list_messages_range(conversation_id, &request.query)
            }
            ("GET", ["conversations", conversation_id, "messages", message_id]) => {
                self.message_detail(conversation_id, message_id)
            }
            ("POST", ["conversations", conversation_id, "messages"]) => {
                self.enqueue_message(conversation_id, &request.body, dispatch_tx)
            }
            ("GET", ["conversations", conversation_id, "status"]) => {
                self.conversation_status(conversation_id)
            }
            ("GET", ["conversations", conversation_id, "workspace"]) => {
                self.conversation_workspace(conversation_id, &request.query)
            }
            ("GET", ["conversations", conversation_id, "workspace", "file"]) => {
                self.conversation_workspace_file(conversation_id, &request.query)
            }
            ("POST", ["conversations", conversation_id, "workspace", "upload"]) => {
                self.conversation_workspace_upload(conversation_id, &request.query, &request.body)
            }
            ("GET", ["conversations", conversation_id, "workspace", "download"]) => {
                self.conversation_workspace_download(conversation_id, &request.query)
            }
            ("GET", ["conversations", conversation_id, "terminals"]) => {
                self.list_terminals(conversation_id)
            }
            ("POST", ["conversations", conversation_id, "terminals"]) => {
                self.create_terminal(conversation_id, &request.body)
            }
            ("GET", ["conversations", conversation_id, "terminals", terminal_id]) => {
                self.get_terminal(conversation_id, terminal_id)
            }
            ("DELETE", ["conversations", conversation_id, "terminals", terminal_id]) => {
                self.terminate_terminal(conversation_id, terminal_id)
            }
            ("GET", ["conversations", conversation_id, "terminals", terminal_id, "output"]) => {
                self.terminal_output(conversation_id, terminal_id, &request.query)
            }
            ("POST", ["conversations", conversation_id, "terminals", terminal_id, "input"]) => {
                self.terminal_input(conversation_id, terminal_id, &request.body)
            }
            ("POST", ["conversations", conversation_id, "terminals", terminal_id, "resize"]) => {
                self.resize_terminal(conversation_id, terminal_id, &request.body)
            }
            _ => Err(ApiError::new(404, "not_found")),
        }
    }

    fn list_models(&self) -> ApiResult<HttpResponse> {
        Ok(json_response(200, model_listing_payload(&self.config)))
    }

    fn list_conversations(&self, query: &HashMap<String, String>) -> ApiResult<HttpResponse> {
        let offset = query_usize(query, "offset", 0);
        let limit = query_usize(query, "limit", 50).min(200);
        let conversations = self.conversation_summaries()?;
        let total = conversations.len();
        let start = offset.min(total);
        let end = start.saturating_add(limit).min(total);
        Ok(json_response(
            200,
            json!({
                "channel_id": self.id,
                "offset": offset,
                "limit": limit,
                "total": total,
                "conversations": &conversations[start..end],
            }),
        ))
    }

    fn conversation_summaries(&self) -> ApiResult<Vec<ConversationSummary>> {
        let mut conversations = Vec::new();
        let root = self.workdir.join("conversations");
        if root.exists() {
            for entry in fs::read_dir(&root).map_err(ApiError::internal)? {
                let entry = entry.map_err(ApiError::internal)?;
                if entry
                    .file_name()
                    .to_str()
                    .is_some_and(is_sshfs_workspace_entry_name)
                {
                    continue;
                }
                let path = entry.path().join("conversation.json");
                if !path.exists() {
                    continue;
                }
                let raw = match fs::read_to_string(&path) {
                    Ok(raw) => raw,
                    Err(error) => {
                        self.logger.warn(
                            "web_conversation_list_read_failed",
                            json!({"path": path.display().to_string(), "error": error.to_string()}),
                        );
                        continue;
                    }
                };
                let state: ConversationState = match serde_json::from_str(&raw) {
                    Ok(state) => state,
                    Err(error) => {
                        self.logger.warn(
                            "web_conversation_list_parse_failed",
                            json!({"path": path.display().to_string(), "error": error.to_string()}),
                        );
                        continue;
                    }
                };
                if state.channel_id == self.id {
                    let processing_state = self
                        .processing_states
                        .lock()
                        .ok()
                        .and_then(|states| states.get(&state.platform_chat_id).copied())
                        .unwrap_or(ProcessingState::Idle);
                    let message_summary = conversation_message_summary(&self.workdir, &state);
                    conversations.push(ConversationSummary::from_state(
                        &state,
                        &self.config,
                        processing_state,
                        message_summary,
                        self.conversation_seen(&state.conversation_id),
                    ));
                }
            }
        }
        conversations.sort_by(|left, right| left.conversation_id.cmp(&right.conversation_id));
        Ok(conversations)
    }

    fn list_conversation_seen(&self) -> ApiResult<HttpResponse> {
        let seen = self
            .seen_states
            .lock()
            .map_err(|_| ApiError::new(500, "conversation seen lock poisoned"))?;
        Ok(json_response(
            200,
            json!({
                "channel_id": self.id,
                "seen": &*seen,
            }),
        ))
    }

    fn mark_conversation_seen(
        &self,
        conversation_id: &str,
        body: &[u8],
    ) -> ApiResult<HttpResponse> {
        self.load_web_state(conversation_id)?;
        let request: MarkConversationSeenRequest = parse_json(body)?;
        let seen = ConversationSeen {
            last_seen_message_id: request.last_seen_message_id,
            updated_at: now_rfc3339(),
        };
        self.seen_states
            .lock()
            .map_err(|_| ApiError::new(500, "conversation seen lock poisoned"))?
            .insert(conversation_id.to_string(), seen.clone());
        self.publish_conversation_stream_event(json!({
            "type": "conversation_seen",
            "subscription": "conversation_list",
            "channel_id": self.id,
            "conversation_id": conversation_id,
            "seen": &seen,
        }));
        Ok(json_response(
            200,
            json!({
                "conversation_id": conversation_id,
                "seen": seen,
            }),
        ))
    }

    fn authorized(&self, request: &HttpRequest) -> bool {
        let Some(value) = request.headers.get("authorization") else {
            return false;
        };
        value == &self.token
            || value
                .strip_prefix("Bearer ")
                .is_some_and(|token| token == self.token)
    }

    fn create_conversation(
        &self,
        body: &[u8],
        id_manager: Arc<Mutex<ConversationIdManager>>,
    ) -> ApiResult<HttpResponse> {
        let request: CreateConversationRequest = parse_optional_json(body)?;
        let platform_chat_id = request
            .platform_chat_id
            .unwrap_or_else(generated_platform_id);
        let conversation_id = id_manager
            .lock()
            .map_err(|_| ApiError::new(500, "conversation id manager lock poisoned"))?
            .get_or_create(&self.id, &platform_chat_id)
            .map_err(|error| ApiError::new(500, error))?;
        let mut state = load_or_create_conversation_state(
            &self.workdir,
            &conversation_id,
            &self.id,
            &platform_chat_id,
            &self.config,
        )
        .map_err(ApiError::internal)?;

        if let Some(model) = request.model {
            let Some(model_config) = self.config.models.get(&model) else {
                return Err(ApiError::new(400, format!("unknown model alias {model}")));
            };
            if !self.config.is_available_agent_model(&model) {
                return Err(ApiError::new(
                    400,
                    format!("model {model} is not available for agent selection"),
                ));
            }
            if !model_config.supports(stellaclaw_core::model_config::ModelCapability::Chat) {
                return Err(ApiError::new(
                    400,
                    format!("model {model} is not chat-capable"),
                ));
            }
            state.session_profile.main_model = ModelSelection::alias(model);
            state.model_selection_pending = false;
        }
        if let Some(nickname) = request.nickname {
            state.nickname = normalize_conversation_nickname(&state.conversation_id, &nickname);
        }
        persist_conversation_state(&self.workdir, &state).map_err(ApiError::internal)?;
        self.publish_conversation_upserted(&state);

        Ok(json_response(
            201,
            json!({
                "conversation_id": conversation_id,
                "nickname": state.nickname,
                "channel_id": self.id,
                "platform_chat_id": platform_chat_id,
                "model_selection_pending": state.model_selection_pending,
            }),
        ))
    }

    fn update_conversation(&self, conversation_id: &str, body: &[u8]) -> ApiResult<HttpResponse> {
        let request: UpdateConversationRequest = parse_json(body)?;
        let mut state = self.load_web_state(conversation_id)?;
        if let Some(nickname) = request.nickname {
            state.nickname = normalize_conversation_nickname(&state.conversation_id, &nickname);
        }
        persist_conversation_state(&self.workdir, &state).map_err(ApiError::internal)?;
        let processing_state = self
            .processing_states
            .lock()
            .ok()
            .and_then(|states| states.get(&state.platform_chat_id).copied())
            .unwrap_or(ProcessingState::Idle);
        let summary = ConversationSummary::from_state(
            &state,
            &self.config,
            processing_state,
            conversation_message_summary(&self.workdir, &state),
            self.conversation_seen(&state.conversation_id),
        );
        self.publish_conversation_upserted(&state);
        Ok(json_response(
            200,
            json!({
                "conversation": summary,
            }),
        ))
    }

    fn enqueue_message(
        &self,
        conversation_id: &str,
        body: &[u8],
        dispatch_tx: Sender<IncomingDispatch>,
    ) -> ApiResult<HttpResponse> {
        let state = self.load_web_state(conversation_id)?;
        let request: SendMessageRequest = parse_json(body)?;
        let text = request.text.unwrap_or_default();
        let files = request
            .files
            .unwrap_or_default()
            .into_iter()
            .map(Into::into)
            .collect::<Vec<_>>();
        if text.trim().is_empty() && files.is_empty() {
            return Err(ApiError::new(400, "message requires text or files"));
        }
        let remote_message_id = request
            .remote_message_id
            .unwrap_or_else(generated_message_id);
        let control = text
            .trim()
            .starts_with('/')
            .then(|| parse_web_control(text.trim()))
            .flatten();
        let incoming = IncomingDispatch {
            channel_id: self.id.clone(),
            platform_chat_id: state.platform_chat_id.clone(),
            conversation_id: conversation_id.to_string(),
            message: IncomingConversationMessage {
                remote_message_id: remote_message_id.clone(),
                user_name: request.user_name,
                message_time: Some(now_rfc3339()),
                text: (!text.is_empty()).then_some(text),
                files,
                control,
            },
        };
        dispatch_tx
            .send(incoming)
            .map_err(|_| ApiError::new(503, "dispatcher is not available"))?;
        Ok(json_response(
            202,
            json!({
                "conversation_id": conversation_id,
                "remote_message_id": remote_message_id,
                "accepted": true,
            }),
        ))
    }

    fn list_messages(
        &self,
        conversation_id: &str,
        query: &HashMap<String, String>,
    ) -> ApiResult<HttpResponse> {
        let offset = query_usize(query, "offset", 0);
        let limit = query_usize(query, "limit", 50).min(200);
        let state = self.load_web_state(conversation_id)?;
        let messages = self.load_messages_for_state(&state)?;
        let attachments =
            WebAttachmentContext::new(&self.workdir, &state, self.cache_manager.clone());
        Ok(json_response(
            200,
            message_page_payload(&state, &attachments, &messages, offset, limit),
        ))
    }

    fn list_messages_after(
        &self,
        conversation_id: &str,
        message_id: &str,
        query: &HashMap<String, String>,
    ) -> ApiResult<HttpResponse> {
        let index = parse_message_id(message_id)?;
        let limit = query_usize(query, "limit", 50).min(200);
        let state = self.load_web_state(conversation_id)?;
        let messages = self.load_messages_for_state(&state)?;
        let attachments =
            WebAttachmentContext::new(&self.workdir, &state, self.cache_manager.clone());
        Ok(json_response(
            200,
            message_page_payload(
                &state,
                &attachments,
                &messages,
                index.saturating_add(1),
                limit,
            ),
        ))
    }

    fn list_messages_range(
        &self,
        conversation_id: &str,
        query: &HashMap<String, String>,
    ) -> ApiResult<HttpResponse> {
        let anchor_id = query
            .get("anchor_id")
            .or_else(|| query.get("message_id"))
            .ok_or_else(|| ApiError::new(400, "anchor_id is required"))?;
        let anchor = parse_message_id(anchor_id)?;
        let direction = query
            .get("direction")
            .map(|value| MessageRangeDirection::parse(value))
            .transpose()?
            .unwrap_or(MessageRangeDirection::Before);
        let include_anchor = query_bool(query, "include_anchor", true);
        let limit = query_usize(query, "limit", 50).min(200);
        let state = self.load_web_state(conversation_id)?;
        let messages = self.load_messages_for_state(&state)?;
        if anchor >= messages.len() {
            return Err(ApiError::new(404, "message_not_found"));
        }
        let (start, end) =
            message_range_bounds(messages.len(), anchor, direction, include_anchor, limit);
        let attachments =
            WebAttachmentContext::new(&self.workdir, &state, self.cache_manager.clone());
        Ok(json_response(
            200,
            message_range_payload(
                &state,
                &attachments,
                &messages,
                anchor,
                direction,
                include_anchor,
                start,
                end,
            ),
        ))
    }

    fn message_detail(&self, conversation_id: &str, message_id: &str) -> ApiResult<HttpResponse> {
        let index = parse_message_id(message_id)?;
        let state = self.load_web_state(conversation_id)?;
        let messages = self.load_messages_for_state(&state)?;
        let Some(message) = messages.get(index) else {
            return Err(ApiError::new(404, "message_not_found"));
        };
        let attachments =
            WebAttachmentContext::new(&self.workdir, &state, self.cache_manager.clone());
        let roots = attachments.roots();
        let rendered = render_web_message(message, &attachments, &roots);
        Ok(json_response(
            200,
            json!({
                "conversation_id": conversation_id,
                "id": index.to_string(),
                "index": index,
                "message": message,
                "token_usage": message.token_usage.as_ref().map(WebTokenUsage::from),
                "rendered_text": rendered.text,
                "items": rendered.items,
                "attachments": rendered.attachments,
                "attachment_errors": rendered.attachment_errors,
            }),
        ))
    }

    fn handle_websocket_stream(&self, mut stream: TcpStream, request: HttpRequest) -> Result<()> {
        if !request.path.starts_with("/api/") && request.path != "/api" {
            write_http_response(&mut stream, json_error(404, "not_found"))?;
            return Ok(());
        }
        if !self.authorized(&request) && !self.websocket_query_authorized(&request) {
            write_http_response(&mut stream, json_error(401, "unauthorized"))?;
            return Ok(());
        }
        let segments = api_segments(&request.path);
        let Some(key) = request.headers.get("sec-websocket-key") else {
            write_http_response(&mut stream, json_error(400, "missing sec-websocket-key"))?;
            return Ok(());
        };
        match segments.as_slice() {
            ["ws"] => {
                write_websocket_handshake(&mut stream, key)?;
                self.run_websocket_subscription(stream)
            }
            ["conversations", "stream"] => {
                write_websocket_handshake(&mut stream, key)?;
                self.run_conversation_stream(stream)
            }
            ["conversations", conversation_id, "terminals", terminal_id, "stream"] => {
                let state = match self.load_web_state(conversation_id) {
                    Ok(state) => state,
                    Err(error) => {
                        write_http_response(&mut stream, json_error(error.status, &error.message))?;
                        return Ok(());
                    }
                };
                let terminal = match self.terminal_manager.get(&state, terminal_id) {
                    Ok(terminal) => terminal,
                    Err(error) => {
                        let error = terminal_api_error(error);
                        write_http_response(&mut stream, json_error(error.status, &error.message))?;
                        return Ok(());
                    }
                };
                let offset = request
                    .query
                    .get("offset")
                    .and_then(|value| value.parse::<u64>().ok())
                    .unwrap_or(terminal.next_offset);
                write_websocket_handshake(&mut stream, key)?;
                let attach = self
                    .terminal_manager
                    .attach(&state, terminal_id, offset)
                    .map_err(|error| anyhow!("{}", terminal_api_error(error).message))?;
                self.run_terminal_websocket_stream(stream, state, terminal_id.to_string(), attach)
            }
            _ => {
                write_http_response(&mut stream, json_error(404, "not_found"))?;
                Ok(())
            }
        }
    }

    fn run_conversation_stream(&self, mut stream: TcpStream) -> Result<()> {
        let (event_tx, event_rx) = unbounded();
        self.register_conversation_stream_subscriber(event_tx);
        let mut last_signature = String::new();
        self.write_conversation_snapshot_if_changed(&mut stream, &mut last_signature, true)?;
        let mut last_heartbeat = Instant::now();

        loop {
            match event_rx.recv_timeout(WEBSOCKET_POLL_INTERVAL) {
                Ok(event) => {
                    write_websocket_json(&mut stream, &event)?;
                    while let Ok(queued) = event_rx.try_recv() {
                        write_websocket_json(&mut stream, &queued)?;
                    }
                }
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => {}
            }
            if last_heartbeat.elapsed() >= WEBSOCKET_HEARTBEAT_INTERVAL {
                write_websocket_frame(&mut stream, 0x9, &[])?;
                last_heartbeat = Instant::now();
            }
        }
    }

    fn write_conversation_snapshot_if_changed(
        &self,
        stream: &mut TcpStream,
        last_signature: &mut String,
        force: bool,
    ) -> Result<()> {
        let conversations = self.conversation_summaries().map_err(api_anyhow)?;
        let signature = serde_json::to_string(&conversations).unwrap_or_default();
        if !force && signature == *last_signature {
            return Ok(());
        }
        *last_signature = signature;
        write_websocket_json(
            stream,
            &json!({
                "type": "conversation_snapshot",
                "subscription": "conversation_list",
                "channel_id": self.id,
                "conversations": conversations,
            }),
        )
    }

    fn register_conversation_stream_subscriber(&self, sender: Sender<Value>) {
        self.conversation_stream_subscribers
            .lock()
            .expect("conversation stream subscriber registry lock poisoned")
            .push(sender);
    }

    fn publish_conversation_upserted(&self, state: &ConversationState) {
        let processing_state = self
            .processing_states
            .lock()
            .ok()
            .and_then(|states| states.get(&state.platform_chat_id).copied())
            .unwrap_or(ProcessingState::Idle);
        let summary = ConversationSummary::from_state(
            state,
            &self.config,
            processing_state,
            conversation_message_summary(&self.workdir, state),
            self.conversation_seen(&state.conversation_id),
        );
        self.publish_conversation_stream_event(json!({
            "type": "conversation_upserted",
            "subscription": "conversation_list",
            "channel_id": self.id,
            "conversation_id": &state.conversation_id,
            "conversation": summary,
        }));
    }

    fn publish_conversation_upserted_for_platform_chat(&self, platform_chat_id: &str) {
        let Ok(Some(state)) = self.conversation_state_for_platform_chat(platform_chat_id) else {
            return;
        };
        self.publish_conversation_upserted(&state);
    }

    fn publish_conversation_processing(&self, platform_chat_id: &str, state: ProcessingState) {
        let Ok(Some(conversation_state)) =
            self.conversation_state_for_platform_chat(platform_chat_id)
        else {
            return;
        };
        self.publish_conversation_stream_event(json!({
            "type": "conversation_processing",
            "subscription": "conversation_list",
            "channel_id": self.id,
            "conversation_id": &conversation_state.conversation_id,
            "platform_chat_id": platform_chat_id,
            "processing_state": processing_state_name(state),
            "running": state != ProcessingState::Idle,
        }));
    }

    fn publish_conversation_stream_event(&self, event: Value) {
        let mut subscribers = self
            .conversation_stream_subscribers
            .lock()
            .expect("conversation stream subscriber registry lock poisoned");
        subscribers.retain(|sender| sender.send(event.clone()).is_ok());
    }

    fn publish_conversation_turn_completed(
        &self,
        platform_chat_id: &str,
        turn_id: Option<&str>,
        final_state: Option<ProgressFeedbackFinalState>,
    ) {
        let Ok(Some(state)) = self.conversation_state_for_platform_chat(platform_chat_id) else {
            return;
        };
        let message_summary = conversation_message_summary(&self.workdir, &state);
        let Some(last_message_id) = message_summary.last_message_id.clone() else {
            return;
        };
        let seen = self.conversation_seen(&state.conversation_id);
        let unread = seen
            .as_ref()
            .and_then(|seen| compare_message_ids(&last_message_id, &seen.last_seen_message_id))
            .map(|ordering| ordering.is_gt())
            .unwrap_or(true);
        let summary = ConversationSummary::from_state(
            &state,
            &self.config,
            ProcessingState::Idle,
            message_summary.clone(),
            seen.clone(),
        );
        self.publish_conversation_stream_event(json!({
            "type": "conversation_turn_completed",
            "subscription": "conversation_list",
            "channel_id": self.id,
            "conversation_id": &state.conversation_id,
            "platform_chat_id": &state.platform_chat_id,
            "turn_id": turn_id,
            "final_state": final_state.map(progress_final_state_name),
            "message_count": message_summary.message_count,
            "last_message_id": last_message_id,
            "last_message_time": message_summary.last_message_time,
            "last_seen_message_id": seen.as_ref().map(|seen| seen.last_seen_message_id.clone()),
            "last_seen_at": seen.map(|seen| seen.updated_at),
            "unread": unread,
            "conversation": summary,
        }));
    }

    fn websocket_query_authorized(&self, request: &HttpRequest) -> bool {
        request
            .query
            .get("token")
            .is_some_and(|token| token == &self.token)
    }

    fn run_websocket_subscription(&self, mut stream: TcpStream) -> Result<()> {
        let subscribe = loop {
            match read_websocket_frame(&mut stream)? {
                WebSocketFrame::Text(text) => break parse_websocket_subscribe(&text)?,
                WebSocketFrame::Ping(payload) => write_websocket_frame(&mut stream, 0xA, &payload)?,
                WebSocketFrame::Close => return Ok(()),
                WebSocketFrame::Binary(_) | WebSocketFrame::Pong => {}
            }
        };
        if subscribe
            .kind
            .as_deref()
            .is_some_and(|kind| kind != "subscribe_foreground")
        {
            let error = json!({
                "type": "error",
                "error": "unsupported_subscription",
            });
            write_websocket_json(&mut stream, &error)?;
            write_websocket_close(&mut stream)?;
            return Ok(());
        }

        let mut state = self
            .load_web_state(&subscribe.conversation_id)
            .map_err(api_anyhow)?;
        let mut session_id = state.session_binding.foreground_session_id.clone();
        let mut messages = self.load_messages_for_state(&state).map_err(api_anyhow)?;
        let mut next_index = messages.len();
        let (event_tx, event_rx) = unbounded();
        self.register_websocket_subscriber(&state, event_tx);
        write_websocket_json(
            &mut stream,
            &websocket_subscription_ack(&state, next_index, "subscribed"),
        )?;
        let mut last_heartbeat = Instant::now();

        loop {
            match event_rx.recv_timeout(WEBSOCKET_POLL_INTERVAL) {
                Ok(event) => {
                    write_websocket_json(&mut stream, &event)?;
                    while let Ok(queued) = event_rx.try_recv() {
                        write_websocket_json(&mut stream, &queued)?;
                    }
                }
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => {}
            }
            if last_heartbeat.elapsed() >= WEBSOCKET_HEARTBEAT_INTERVAL {
                write_websocket_frame(&mut stream, 0x9, &[])?;
                last_heartbeat = Instant::now();
            }
            state = self
                .load_web_state(&subscribe.conversation_id)
                .map_err(api_anyhow)?;
            if state.session_binding.foreground_session_id != session_id {
                session_id = state.session_binding.foreground_session_id.clone();
                messages = self.load_messages_for_state(&state).map_err(api_anyhow)?;
                next_index = messages.len();
                write_websocket_json(
                    &mut stream,
                    &websocket_subscription_ack(&state, next_index, "session_changed"),
                )?;
                continue;
            }

            messages = self.load_messages_for_state(&state).map_err(api_anyhow)?;
            if messages.len() <= next_index {
                continue;
            }
            let attachments =
                WebAttachmentContext::new(&self.workdir, &state, self.cache_manager.clone());
            let payload = websocket_messages_payload(&state, &attachments, &messages, next_index);
            next_index = messages.len();
            write_websocket_json(&mut stream, &payload)?;
        }
    }

    fn register_websocket_subscriber(&self, state: &ConversationState, sender: Sender<Value>) {
        let subscriber = WebSocketSubscriber {
            conversation_id: state.conversation_id.clone(),
            sender,
        };
        self.websocket_subscribers
            .lock()
            .expect("websocket subscriber registry lock poisoned")
            .entry(state.platform_chat_id.clone())
            .or_default()
            .push(subscriber);
    }

    fn publish_websocket_event(&self, platform_chat_id: &str, event: Value) {
        let mut remove_key = false;
        let mut subscribers = self
            .websocket_subscribers
            .lock()
            .expect("websocket subscriber registry lock poisoned");
        let Some(entries) = subscribers.get_mut(platform_chat_id) else {
            return;
        };
        entries.retain(|subscriber| {
            let event = websocket_event_for_subscriber(&event, platform_chat_id, subscriber);
            subscriber.sender.send(event).is_ok()
        });
        if entries.is_empty() {
            remove_key = true;
        }
        if remove_key {
            subscribers.remove(platform_chat_id);
        }
    }

    fn conversation_seen(&self, conversation_id: &str) -> Option<ConversationSeen> {
        self.seen_states
            .lock()
            .ok()
            .and_then(|seen| seen.get(conversation_id).cloned())
    }

    fn conversation_state_for_platform_chat(
        &self,
        platform_chat_id: &str,
    ) -> ApiResult<Option<ConversationState>> {
        let root = self.workdir.join("conversations");
        if !root.exists() {
            return Ok(None);
        }
        for entry in fs::read_dir(&root).map_err(ApiError::internal)? {
            let entry = entry.map_err(ApiError::internal)?;
            if entry
                .file_name()
                .to_str()
                .is_some_and(is_sshfs_workspace_entry_name)
            {
                continue;
            }
            let path = entry.path().join("conversation.json");
            if !path.exists() {
                continue;
            }
            let raw = match fs::read_to_string(&path) {
                Ok(raw) => raw,
                Err(error) => {
                    self.logger.warn(
                        "web_conversation_state_read_failed",
                        json!({"path": path.display().to_string(), "error": error.to_string()}),
                    );
                    continue;
                }
            };
            let state: ConversationState = match serde_json::from_str(&raw) {
                Ok(state) => state,
                Err(error) => {
                    self.logger.warn(
                        "web_conversation_state_parse_failed",
                        json!({"path": path.display().to_string(), "error": error.to_string()}),
                    );
                    continue;
                }
            };
            if state.channel_id == self.id && state.platform_chat_id == platform_chat_id {
                return Ok(Some(state));
            }
        }
        Ok(None)
    }

    fn run_terminal_websocket_stream(
        &self,
        mut stream: TcpStream,
        state: ConversationState,
        terminal_id: String,
        attach: TerminalAttach,
    ) -> Result<()> {
        let _ = stream.set_write_timeout(Some(TERMINAL_WEBSOCKET_WRITE_TIMEOUT));
        write_terminal_replay(&mut stream, &attach.replay)?;
        if !attach.replay.running {
            write_websocket_json(
                &mut stream,
                &json!({
                    "type": "exit",
                    "terminal_id": &terminal_id,
                }),
            )?;
            write_websocket_close(&mut stream)?;
            return Ok(());
        }

        let subscriber_id = attach.subscriber_id;
        let output_rx = attach.receiver;
        let (client_tx, client_rx) = bounded(TERMINAL_WEBSOCKET_EVENT_CAPACITY);
        let mut read_stream = stream
            .try_clone()
            .context("failed to clone terminal websocket stream")?;
        thread::spawn(move || loop {
            let event = match read_websocket_frame(&mut read_stream) {
                Ok(WebSocketFrame::Text(text)) => parse_terminal_websocket_control(&text)
                    .unwrap_or_else(|error| TerminalWebSocketEvent::Error(error.to_string())),
                Ok(WebSocketFrame::Binary(payload)) => TerminalWebSocketEvent::Input(payload),
                Ok(WebSocketFrame::Ping(payload)) => TerminalWebSocketEvent::WsPing(payload),
                Ok(WebSocketFrame::Pong) => continue,
                Ok(WebSocketFrame::Close) => TerminalWebSocketEvent::Close,
                Err(_) => TerminalWebSocketEvent::Close,
            };
            let should_stop = matches!(event, TerminalWebSocketEvent::Close);
            if client_tx.send(event).is_err() || should_stop {
                break;
            }
        });

        let heartbeat = crossbeam_channel::tick(WEBSOCKET_HEARTBEAT_INTERVAL);
        let result = loop {
            crossbeam_channel::select! {
                recv(output_rx) -> message => {
                    match message {
                        Ok(chunk) => {
                            if let Err(error) = write_websocket_frame(&mut stream, 0x2, &chunk.bytes) {
                                break Err(error);
                            }
                        }
                        Err(_) => {
                            let running = self
                                .terminal_manager
                                .get(&state, &terminal_id)
                                .map(|summary| summary.running)
                                .unwrap_or(false);
                            let event = if running {
                                json!({
                                    "type": "detached",
                                    "terminal_id": &terminal_id,
                                    "reason": "slow_client",
                                })
                            } else {
                                json!({
                                    "type": "exit",
                                    "terminal_id": &terminal_id,
                                })
                            };
                            let _ = write_websocket_json(&mut stream, &event);
                            let _ = write_websocket_close(&mut stream);
                            break Ok(());
                        }
                    }
                }
                recv(client_rx) -> message => {
                    match message {
                        Ok(TerminalWebSocketEvent::Input(bytes)) => {
                            if let Err(error) = self.terminal_manager.input_bytes(&state, &terminal_id, &bytes) {
                                let _ = write_terminal_websocket_error(&mut stream, error);
                                let _ = write_websocket_close(&mut stream);
                                break Ok(());
                            }
                        }
                        Ok(TerminalWebSocketEvent::Resize(request)) => {
                            if let Err(error) = self.terminal_manager.resize(&state, &terminal_id, request) {
                                let _ = write_terminal_websocket_error(&mut stream, error);
                                let _ = write_websocket_close(&mut stream);
                                break Ok(());
                            }
                        }
                        Ok(TerminalWebSocketEvent::Attach(offset)) => {
                            match self.terminal_manager.replay(&state, &terminal_id, offset) {
                                Ok(replay) => {
                                    if let Err(error) = write_terminal_replay(&mut stream, &replay) {
                                        break Err(error);
                                    }
                                }
                                Err(error) => {
                                    let _ = write_terminal_websocket_error(&mut stream, error);
                                    let _ = write_websocket_close(&mut stream);
                                    break Ok(());
                                }
                            }
                        }
                        Ok(TerminalWebSocketEvent::JsonPing) => {
                            if let Err(error) = write_websocket_json(&mut stream, &json!({"type": "pong"})) {
                                break Err(error);
                            }
                        }
                        Ok(TerminalWebSocketEvent::WsPing(payload)) => {
                            if let Err(error) = write_websocket_frame(&mut stream, 0xA, &payload) {
                                break Err(error);
                            }
                        }
                        Ok(TerminalWebSocketEvent::Close) | Err(_) => {
                            let _ = write_websocket_close(&mut stream);
                            break Ok(());
                        }
                        Ok(TerminalWebSocketEvent::Error(message)) => {
                            let _ = write_websocket_json(
                                &mut stream,
                                &json!({
                                    "type": "error",
                                    "error": "invalid_terminal_stream_frame",
                                    "message": message,
                                }),
                            );
                        }
                    }
                }
                recv(heartbeat) -> _ => {
                    if let Err(error) = write_websocket_frame(&mut stream, 0x9, &[]) {
                        break Err(error);
                    }
                }
            }
        };
        if let Some(subscriber_id) = subscriber_id {
            let _ = self
                .terminal_manager
                .detach(&state, &terminal_id, subscriber_id);
        }
        result
    }

    fn conversation_status(&self, conversation_id: &str) -> ApiResult<HttpResponse> {
        self.load_web_state(conversation_id)?;
        let status =
            load_conversation_status_snapshot(&self.workdir, &self.config, conversation_id)
                .map_err(ApiError::internal)?;
        Ok(json_response(200, json!(status)))
    }

    fn conversation_workspace(
        &self,
        conversation_id: &str,
        query: &HashMap<String, String>,
    ) -> ApiResult<HttpResponse> {
        let state = self.load_web_state(conversation_id)?;
        let path = query.get("path").map(String::as_str);
        let limit = query_usize(query, "limit", 200).min(1000);
        let listing = list_workspace_entries(&self.workdir, &state, path, limit).map_err(
            |error| match error {
                RemoteActorError::InvalidPath(message) => ApiError::new(400, message),
                RemoteActorError::Internal(error) => ApiError::internal(error),
            },
        )?;
        Ok(json_response(200, json!(listing)))
    }

    fn conversation_workspace_file(
        &self,
        conversation_id: &str,
        query: &HashMap<String, String>,
    ) -> ApiResult<HttpResponse> {
        let state = self.load_web_state(conversation_id)?;
        let path = query
            .get("path")
            .map(String::as_str)
            .ok_or_else(|| ApiError::new(400, "workspace file path is required"))?;
        let offset = query
            .get("offset")
            .and_then(|value| value.parse::<u64>().ok())
            .unwrap_or(0);
        let limit_bytes = query
            .get("limit_bytes")
            .and_then(|value| value.parse::<usize>().ok())
            .unwrap_or(DEFAULT_WORKSPACE_FILE_LIMIT_BYTES)
            .clamp(1, MAX_WORKSPACE_FILE_LIMIT_BYTES);
        let file = read_workspace_file(&self.workdir, &state, path, offset, limit_bytes).map_err(
            |error| match error {
                RemoteActorError::InvalidPath(message) => ApiError::new(400, message),
                RemoteActorError::Internal(error) => ApiError::internal(error),
            },
        )?;
        Ok(json_response(200, json!(file)))
    }

    fn conversation_workspace_upload(
        &self,
        conversation_id: &str,
        query: &HashMap<String, String>,
        body: &[u8],
    ) -> ApiResult<HttpResponse> {
        let state = self.load_web_state(conversation_id)?;
        let dir = query.get("path").map(String::as_str).unwrap_or("");
        let count = upload_workspace_archive(&self.workdir, &state, dir, body).map_err(
            |error| match error {
                RemoteActorError::InvalidPath(message) => ApiError::new(400, message),
                RemoteActorError::Internal(error) => ApiError::internal(error),
            },
        )?;
        Ok(json_response(
            200,
            json!({"conversation_id": conversation_id, "path": dir, "entries_extracted": count}),
        ))
    }

    fn conversation_workspace_download(
        &self,
        conversation_id: &str,
        query: &HashMap<String, String>,
    ) -> ApiResult<HttpResponse> {
        let state = self.load_web_state(conversation_id)?;
        let path = query
            .get("path")
            .map(String::as_str)
            .ok_or_else(|| ApiError::new(400, "workspace path is required for download"))?;
        let paths = vec![path];
        let archive = download_workspace_archive(&self.workdir, &state, &paths).map_err(
            |error| match error {
                RemoteActorError::InvalidPath(message) => ApiError::new(400, message),
                RemoteActorError::Internal(error) => ApiError::internal(error),
            },
        )?;
        Ok(HttpResponse {
            status: 200,
            content_type: "application/gzip",
            body: archive,
        })
    }

    fn list_terminals(&self, conversation_id: &str) -> ApiResult<HttpResponse> {
        let state = self.load_web_state(conversation_id)?;
        Ok(json_response(
            200,
            json!({
                "conversation_id": conversation_id,
                "terminals": self.terminal_manager.list(&state),
            }),
        ))
    }

    fn create_terminal(&self, conversation_id: &str, body: &[u8]) -> ApiResult<HttpResponse> {
        let state = self.load_web_state(conversation_id)?;
        let request: TerminalCreateRequest = parse_optional_json(body)?;
        let terminal = self
            .terminal_manager
            .create(&self.workdir, &state, request)
            .map_err(terminal_api_error)?;
        Ok(json_response(201, json!(terminal)))
    }

    fn get_terminal(&self, conversation_id: &str, terminal_id: &str) -> ApiResult<HttpResponse> {
        let state = self.load_web_state(conversation_id)?;
        let terminal = self
            .terminal_manager
            .get(&state, terminal_id)
            .map_err(terminal_api_error)?;
        Ok(json_response(200, json!(terminal)))
    }

    fn terminal_output(
        &self,
        conversation_id: &str,
        terminal_id: &str,
        query: &HashMap<String, String>,
    ) -> ApiResult<HttpResponse> {
        let state = self.load_web_state(conversation_id)?;
        let offset = query
            .get("offset")
            .and_then(|value| value.parse::<u64>().ok())
            .unwrap_or(0);
        let limit = output_limit(
            query
                .get("limit_bytes")
                .and_then(|value| value.parse().ok()),
        );
        let output = self
            .terminal_manager
            .output(&state, terminal_id, offset, limit)
            .map_err(terminal_api_error)?;
        Ok(json_response(200, json!(output)))
    }

    fn terminal_input(
        &self,
        conversation_id: &str,
        terminal_id: &str,
        body: &[u8],
    ) -> ApiResult<HttpResponse> {
        let state = self.load_web_state(conversation_id)?;
        let request: TerminalInputRequest = parse_json(body)?;
        let terminal = self
            .terminal_manager
            .input(&state, terminal_id, &request.data)
            .map_err(terminal_api_error)?;
        Ok(json_response(202, json!(terminal)))
    }

    fn resize_terminal(
        &self,
        conversation_id: &str,
        terminal_id: &str,
        body: &[u8],
    ) -> ApiResult<HttpResponse> {
        let state = self.load_web_state(conversation_id)?;
        let request: TerminalResizeRequest = parse_json(body)?;
        let terminal = self
            .terminal_manager
            .resize(&state, terminal_id, request)
            .map_err(terminal_api_error)?;
        Ok(json_response(200, json!(terminal)))
    }

    fn terminate_terminal(
        &self,
        conversation_id: &str,
        terminal_id: &str,
    ) -> ApiResult<HttpResponse> {
        let state = self.load_web_state(conversation_id)?;
        let terminal = self
            .terminal_manager
            .terminate(&state, terminal_id)
            .map_err(terminal_api_error)?;
        Ok(json_response(200, json!(terminal)))
    }

    fn load_web_state(&self, conversation_id: &str) -> ApiResult<ConversationState> {
        validate_conversation_id(conversation_id)?;
        let path = self
            .workdir
            .join("conversations")
            .join(conversation_id)
            .join("conversation.json");
        let raw =
            fs::read_to_string(&path).map_err(|_| ApiError::new(404, "conversation_not_found"))?;
        let state: ConversationState = serde_json::from_str(&raw).map_err(ApiError::internal)?;
        if state.channel_id != self.id {
            return Err(ApiError::new(404, "conversation_not_found"));
        }
        Ok(state)
    }

    fn load_messages_for_state(&self, state: &ConversationState) -> ApiResult<Vec<ChatMessage>> {
        let path = self
            .workdir
            .join("conversations")
            .join(&state.conversation_id)
            .join(".log")
            .join("stellaclaw")
            .join(sanitize_session_id_for_log_path(
                &state.session_binding.foreground_session_id,
            ))
            .join("all_messages.jsonl");
        if !path.exists() {
            return Ok(Vec::new());
        }
        read_messages_jsonl(&path).map_err(ApiError::internal)
    }
}

impl Channel for WebChannel {
    fn id(&self) -> &str {
        &self.id
    }

    fn send_delivery(&self, delivery: &OutgoingDelivery) -> Result<()> {
        self.logger.info(
            "web_delivery",
            json!({
                "channel_id": delivery.channel_id,
                "platform_chat_id": delivery.platform_chat_id,
                "text_len": delivery.text.len(),
                "attachment_count": delivery.attachments.len(),
            }),
        );
        Ok(())
    }

    fn send_status(&self, status: &OutgoingStatus) -> Result<()> {
        self.logger.info(
            "web_status_delivery",
            json!({
                "channel_id": status.channel_id,
                "platform_chat_id": status.platform_chat_id,
                "conversation_id": status.conversation_id,
            }),
        );
        self.publish_websocket_event(
            &status.platform_chat_id,
            json!({
                "type": "status",
                "subscription": "foreground_session_events",
                "conversation_id": &status.conversation_id,
                "status": status,
            }),
        );
        self.publish_conversation_upserted_for_platform_chat(&status.platform_chat_id);
        Ok(())
    }

    fn send_error(&self, error: &OutgoingError) -> Result<()> {
        self.logger.warn(
            "web_error_delivery",
            json!({
                "channel_id": error.channel_id,
                "platform_chat_id": error.platform_chat_id,
                "conversation_id": error.conversation_id,
                "scope": error.scope,
                "code": error.code,
            }),
        );
        self.publish_websocket_event(
            &error.platform_chat_id,
            json!({
                "type": "error",
                "subscription": "foreground_session_events",
                "conversation_id": &error.conversation_id,
                "scope": &error.scope,
                "severity": &error.severity,
                "code": &error.code,
                "message": &error.message,
                "detail": &error.detail,
                "can_continue": error.can_continue,
                "suggested_action": &error.suggested_action,
                "error": error,
            }),
        );
        Ok(())
    }

    fn set_processing(&self, platform_chat_id: &str, state: ProcessingState) -> Result<()> {
        if let Ok(mut states) = self.processing_states.lock() {
            if state == ProcessingState::Idle {
                states.remove(platform_chat_id);
            } else {
                states.insert(platform_chat_id.to_string(), state);
            }
        }
        self.logger.info(
            "web_processing",
            json!({
                "channel_id": self.id,
                "platform_chat_id": platform_chat_id,
                "state": format!("{state:?}"),
            }),
        );
        self.publish_websocket_event(
            platform_chat_id,
            json!({
                "type": "processing",
                "subscription": "foreground_session_events",
                "state": processing_state_name(state),
            }),
        );
        self.publish_conversation_processing(platform_chat_id, state);
        Ok(())
    }

    fn update_progress_feedback(&self, feedback: &OutgoingProgressFeedback) -> Result<()> {
        let text = web_progress_text(feedback);
        self.logger.info(
            "web_progress",
            json!({
                "channel_id": feedback.channel_id,
                "platform_chat_id": feedback.platform_chat_id,
                "turn_id": feedback.turn_id,
                "phase": feedback.progress.phase,
                "final_state": feedback.final_state.map(|state| format!("{state:?}")),
            }),
        );
        self.publish_websocket_event(
            &feedback.platform_chat_id,
            json!({
                "type": "progress_feedback",
                "subscription": "foreground_session_events",
                "turn_id": &feedback.turn_id,
                "text": text,
                "phase": feedback.progress.phase,
                "model": &feedback.progress.model,
                "activity": &feedback.progress.activity,
                "hint": &feedback.progress.hint,
                "plan": &feedback.progress.plan,
                "error": &feedback.progress.error,
                "final_state": feedback.final_state.map(progress_final_state_name),
                "important": feedback.important,
            }),
        );
        self.publish_websocket_event(
            &feedback.platform_chat_id,
            turn_progress_payload(feedback, &text),
        );
        if feedback.final_state.is_some() {
            self.publish_conversation_turn_completed(
                &feedback.platform_chat_id,
                Some(&feedback.turn_id),
                feedback.final_state,
            );
        }
        Ok(())
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
            if let Err(error) = self.run(dispatch_tx, id_manager, logger.clone()) {
                logger.error(
                    "web_channel_stopped",
                    json!({"error": format!("{error:#}")}),
                );
            }
        });
    }
}

#[derive(Debug)]
struct HttpRequest {
    method: String,
    path: String,
    query: HashMap<String, String>,
    headers: HashMap<String, String>,
    body: Vec<u8>,
}

impl HttpRequest {
    fn is_websocket_upgrade(&self) -> bool {
        self.method == "GET"
            && self
                .headers
                .get("upgrade")
                .is_some_and(|value| value.eq_ignore_ascii_case("websocket"))
            && self.headers.get("sec-websocket-key").is_some()
    }
}

struct HttpResponse {
    status: u16,
    content_type: &'static str,
    body: Vec<u8>,
}

impl HttpResponse {
    fn empty(status: u16) -> Self {
        Self {
            status,
            content_type: "application/json",
            body: Vec::new(),
        }
    }
}

#[derive(Debug)]
struct ApiError {
    status: u16,
    message: String,
}

type ApiResult<T> = std::result::Result<T, ApiError>;

impl ApiError {
    fn new(status: u16, message: impl Into<String>) -> Self {
        Self {
            status,
            message: message.into(),
        }
    }

    fn internal(error: impl std::fmt::Display) -> Self {
        Self::new(500, error.to_string())
    }
}

#[derive(Debug, Deserialize)]
struct WebSocketSubscribeRequest {
    #[serde(default, rename = "type")]
    kind: Option<String>,
    conversation_id: String,
}

#[derive(Debug, Deserialize)]
struct TerminalWebSocketControl {
    #[serde(rename = "type")]
    kind: String,
    #[serde(default)]
    offset: Option<u64>,
    #[serde(default)]
    cols: Option<u16>,
    #[serde(default)]
    rows: Option<u16>,
}

#[derive(Debug)]
enum TerminalWebSocketEvent {
    Input(Vec<u8>),
    Resize(TerminalResizeRequest),
    Attach(u64),
    JsonPing,
    WsPing(Vec<u8>),
    Close,
    Error(String),
}

#[derive(Debug)]
enum WebSocketFrame {
    Text(String),
    Binary(Vec<u8>),
    Ping(Vec<u8>),
    Pong,
    Close,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MessageRangeDirection {
    Before,
    After,
}

impl MessageRangeDirection {
    fn parse(value: &str) -> ApiResult<Self> {
        match value {
            "before" | "backward" | "backwards" => Ok(Self::Before),
            "after" | "forward" | "forwards" => Ok(Self::After),
            _ => Err(ApiError::new(
                400,
                "direction must be before/backward or after/forward",
            )),
        }
    }

    fn as_str(self) -> &'static str {
        match self {
            Self::Before => "before",
            Self::After => "after",
        }
    }
}

#[derive(Debug, Default, Deserialize)]
struct CreateConversationRequest {
    platform_chat_id: Option<String>,
    model: Option<String>,
    nickname: Option<String>,
}

#[derive(Debug, Deserialize)]
struct UpdateConversationRequest {
    #[serde(default)]
    nickname: Option<String>,
}

#[derive(Debug, Deserialize)]
struct SendMessageRequest {
    #[serde(default)]
    remote_message_id: Option<String>,
    #[serde(default)]
    user_name: Option<String>,
    #[serde(default)]
    text: Option<String>,
    #[serde(default)]
    files: Option<Vec<WebFileItem>>,
}

#[derive(Debug, Deserialize)]
struct MarkConversationSeenRequest {
    last_seen_message_id: String,
}

#[derive(Debug, Deserialize)]
struct WebFileItem {
    uri: String,
    #[serde(default)]
    media_type: Option<String>,
    #[serde(default)]
    name: Option<String>,
}

impl From<WebFileItem> for FileItem {
    fn from(value: WebFileItem) -> Self {
        FileItem {
            uri: value.uri,
            media_type: value.media_type,
            name: value.name,
            width: None,
            height: None,
            state: None,
        }
    }
}

#[derive(Debug, Serialize)]
struct WebModelSummary {
    alias: String,
    model_name: String,
    provider_type: ProviderType,
    capabilities: Vec<ModelCapability>,
    token_max_context: u64,
    max_tokens: u64,
    effective_max_tokens: u64,
}

impl WebModelSummary {
    fn new(alias: &str, model: &stellaclaw_core::model_config::ModelConfig) -> Self {
        Self {
            alias: alias.to_string(),
            model_name: model.model_name.clone(),
            provider_type: model.provider_type.clone(),
            capabilities: model.capabilities.clone(),
            token_max_context: model.token_max_context,
            max_tokens: model.max_tokens,
            effective_max_tokens: model.effective_max_tokens(),
        }
    }
}

#[derive(Debug, Serialize)]
struct MessageSkeleton {
    id: String,
    index: usize,
    role: ChatRole,
    text: String,
    text_with_attachment_markers: String,
    preview: String,
    items: Vec<WebMessageItem>,
    attachments: Vec<WebMessageAttachment>,
    attachment_count: usize,
    has_attachment_errors: bool,
    user_name: Option<String>,
    message_time: Option<String>,
    has_token_usage: bool,
    token_usage: Option<WebTokenUsage>,
}

#[derive(Debug, Clone, Serialize)]
struct WebTokenUsage {
    cache_read: u64,
    cache_write: u64,
    uncache_input: u64,
    input: u64,
    output: u64,
    total: u64,
    cost_usd: Option<stellaclaw_core::session_actor::TokenUsageCost>,
}

impl From<&TokenUsage> for WebTokenUsage {
    fn from(value: &TokenUsage) -> Self {
        let total = value
            .cache_read
            .saturating_add(value.cache_write)
            .saturating_add(value.uncache_input)
            .saturating_add(value.output);
        Self {
            cache_read: value.cache_read,
            cache_write: value.cache_write,
            uncache_input: value.uncache_input,
            input: value.uncache_input,
            output: value.output,
            total,
            cost_usd: value.cost_usd.clone(),
        }
    }
}

#[derive(Debug, Serialize)]
struct WebMessageAttachment {
    index: usize,
    source: &'static str,
    kind: &'static str,
    path: String,
    uri: String,
    name: String,
    media_type: Option<String>,
    width: Option<u32>,
    height: Option<u32>,
    size_bytes: Option<u64>,
    url: String,
    marker: Option<String>,
    thumbnail: Option<CachedThumbnail>,
}

#[derive(Debug, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum WebMessageItem {
    Text {
        index: usize,
        text: String,
        text_with_attachment_markers: String,
    },
    File {
        index: usize,
        attachment_index: usize,
    },
    ToolCall {
        index: usize,
        tool_call_id: String,
        tool_name: String,
        arguments: Value,
    },
    ToolResult {
        index: usize,
        tool_call_id: String,
        tool_name: String,
        context: Option<String>,
        context_with_attachment_markers: Option<String>,
        file_attachment_index: Option<usize>,
    },
}

#[derive(Debug)]
struct WebRenderedMessage {
    text: String,
    text_with_attachment_markers: String,
    items: Vec<WebMessageItem>,
    attachments: Vec<WebMessageAttachment>,
    attachment_errors: Vec<String>,
}

struct WebRenderedTextPart {
    text: String,
    text_with_attachment_markers: String,
}

struct WebAttachmentContext {
    conversation_id: String,
    workdir: PathBuf,
    state: ConversationState,
    cache_manager: Arc<CacheManager>,
}

struct WebAttachmentRoots {
    workspace_root: PathBuf,
    shared_root: PathBuf,
}

impl WebAttachmentContext {
    fn new(workdir: &Path, state: &ConversationState, cache_manager: Arc<CacheManager>) -> Self {
        Self {
            conversation_id: state.conversation_id.clone(),
            workdir: workdir.to_path_buf(),
            state: state.clone(),
            cache_manager,
        }
    }

    fn roots(&self) -> WebAttachmentRoots {
        let conversation_root = self
            .workdir
            .join("conversations")
            .join(&self.state.conversation_id);
        let workspace_root = match &self.state.tool_remote_mode {
            stellaclaw_core::session_actor::ToolRemoteMode::Selectable => conversation_root,
            stellaclaw_core::session_actor::ToolRemoteMode::FixedSsh { .. } => {
                sshfs_workspace_root(&self.workdir, &self.state.conversation_id)
            }
        };
        let shared_root = self.workdir.join("rundir").join("shared");
        WebAttachmentRoots {
            workspace_root,
            shared_root,
        }
    }
}

#[derive(Debug, Serialize)]
struct ConversationSummary {
    conversation_id: String,
    nickname: String,
    platform_chat_id: String,
    model: String,
    model_selection_pending: bool,
    foreground_session_id: String,
    total_background: usize,
    total_subagents: usize,
    processing_state: String,
    running: bool,
    message_count: usize,
    last_message_id: Option<String>,
    last_message_time: Option<String>,
    last_seen_message_id: Option<String>,
    last_seen_at: Option<String>,
}

#[derive(Debug, Clone, Default)]
struct ConversationMessageSummary {
    message_count: usize,
    last_message_id: Option<String>,
    last_message_time: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize)]
struct ConversationSeen {
    last_seen_message_id: String,
    updated_at: String,
}

impl ConversationSummary {
    fn from_state(
        state: &ConversationState,
        config: &StellaclawConfig,
        processing_state: ProcessingState,
        message_summary: ConversationMessageSummary,
        seen: Option<ConversationSeen>,
    ) -> Self {
        Self {
            conversation_id: state.conversation_id.clone(),
            nickname: conversation_nickname(state),
            platform_chat_id: state.platform_chat_id.clone(),
            model: state
                .session_profile
                .main_model
                .display_name(&config.models),
            model_selection_pending: state.model_selection_pending,
            foreground_session_id: state.session_binding.foreground_session_id.clone(),
            total_background: state.session_binding.background_sessions.len(),
            total_subagents: state.session_binding.subagent_sessions.len(),
            processing_state: processing_state_name(processing_state).to_string(),
            running: processing_state != ProcessingState::Idle,
            message_count: message_summary.message_count,
            last_message_id: message_summary.last_message_id,
            last_message_time: message_summary.last_message_time,
            last_seen_message_id: seen.as_ref().map(|seen| seen.last_seen_message_id.clone()),
            last_seen_at: seen.map(|seen| seen.updated_at),
        }
    }
}

fn conversation_message_summary(
    workdir: &Path,
    state: &ConversationState,
) -> ConversationMessageSummary {
    let path = workdir
        .join("conversations")
        .join(&state.conversation_id)
        .join(".log")
        .join("stellaclaw")
        .join(sanitize_session_id_for_log_path(
            &state.session_binding.foreground_session_id,
        ))
        .join("all_messages.jsonl");
    let Ok(raw) = fs::read_to_string(path) else {
        return ConversationMessageSummary::default();
    };
    let mut summary = ConversationMessageSummary::default();
    let mut last_line = "";
    for line in raw.lines().filter(|line| !line.trim().is_empty()) {
        summary.message_count = summary.message_count.saturating_add(1);
        last_line = line;
    }
    if !last_line.is_empty() {
        if let Ok(value) = serde_json::from_str::<Value>(last_line) {
            summary.last_message_id = value
                .get("id")
                .or_else(|| value.get("message_id"))
                .and_then(|value| {
                    value
                        .as_str()
                        .map(str::to_string)
                        .or_else(|| value.as_u64().map(|id| id.to_string()))
                });
            summary.last_message_time = value
                .get("message_time")
                .and_then(Value::as_str)
                .map(str::to_string);
        }
    }
    summary
}

fn compare_message_ids(left: &str, right: &str) -> Option<Ordering> {
    let left = left.parse::<u64>().ok()?;
    let right = right.parse::<u64>().ok()?;
    Some(left.cmp(&right))
}

fn model_listing_payload(config: &StellaclawConfig) -> Value {
    let models = config
        .available_agent_models()
        .into_iter()
        .map(|(alias, model)| WebModelSummary::new(alias, model))
        .collect::<Vec<_>>();
    json!({
        "default_model": config.initial_main_model_name(),
        "total": models.len(),
        "models": models,
    })
}

fn read_http_request(stream: &mut TcpStream) -> Result<HttpRequest> {
    let mut buffer = Vec::new();
    let header_end = loop {
        let mut chunk = [0_u8; 4096];
        let read = stream.read(&mut chunk).context("failed to read request")?;
        if read == 0 {
            return Err(anyhow!("connection closed before request headers"));
        }
        buffer.extend_from_slice(&chunk[..read]);
        if buffer.len() > MAX_HEADER_BYTES {
            return Err(anyhow!("request headers exceed {MAX_HEADER_BYTES} bytes"));
        }
        if let Some(position) = find_header_end(&buffer) {
            break position;
        }
    };
    let header_bytes = &buffer[..header_end];
    let header_text = String::from_utf8_lossy(header_bytes);
    let mut lines = header_text.split("\r\n");
    let request_line = lines
        .next()
        .ok_or_else(|| anyhow!("missing request line"))?;
    let mut request_parts = request_line.split_whitespace();
    let method = request_parts
        .next()
        .ok_or_else(|| anyhow!("missing request method"))?
        .to_string();
    let target = request_parts
        .next()
        .ok_or_else(|| anyhow!("missing request target"))?;
    let (path, query) = split_target(target);
    let mut headers = HashMap::new();
    for line in lines {
        if line.is_empty() {
            continue;
        }
        if let Some((name, value)) = line.split_once(':') {
            headers.insert(name.trim().to_ascii_lowercase(), value.trim().to_string());
        }
    }
    let content_length = headers
        .get("content-length")
        .and_then(|value| value.parse::<usize>().ok())
        .unwrap_or(0);
    if content_length > MAX_BODY_BYTES {
        return Err(anyhow!("request body exceeds {MAX_BODY_BYTES} bytes"));
    }
    let body_start = header_end + 4;
    let mut body = buffer.get(body_start..).unwrap_or_default().to_vec();
    while body.len() < content_length {
        let mut chunk = vec![0_u8; content_length - body.len()];
        let read = stream.read(&mut chunk).context("failed to read body")?;
        if read == 0 {
            return Err(anyhow!("connection closed before request body completed"));
        }
        body.extend_from_slice(&chunk[..read]);
    }
    body.truncate(content_length);
    Ok(HttpRequest {
        method,
        path,
        query,
        headers,
        body,
    })
}

fn write_http_response(stream: &mut TcpStream, response: HttpResponse) -> Result<()> {
    let reason = status_reason(response.status);
    let headers = format!(
        "HTTP/1.1 {} {}\r\nContent-Type: {}\r\nContent-Length: {}\r\nAccess-Control-Allow-Origin: *\r\nAccess-Control-Allow-Headers: Authorization, Content-Type\r\nAccess-Control-Allow-Methods: GET, POST, PATCH, DELETE, OPTIONS\r\nConnection: close\r\n\r\n",
        response.status,
        reason,
        response.content_type,
        response.body.len(),
    );
    stream
        .write_all(headers.as_bytes())
        .context("failed to write response headers")?;
    stream
        .write_all(&response.body)
        .context("failed to write response body")
}

fn write_websocket_handshake(stream: &mut TcpStream, key: &str) -> Result<()> {
    let mut hasher = Sha1::new();
    hasher.update(key.as_bytes());
    hasher.update(WEBSOCKET_GUID.as_bytes());
    let accept = general_purpose::STANDARD.encode(hasher.finalize());
    let response = format!(
        "HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Accept: {accept}\r\n\r\n"
    );
    stream
        .write_all(response.as_bytes())
        .context("failed to write websocket handshake")
}

fn read_websocket_frame(stream: &mut TcpStream) -> Result<WebSocketFrame> {
    let mut header = [0_u8; 2];
    stream
        .read_exact(&mut header)
        .context("failed to read websocket frame header")?;
    let opcode = header[0] & 0x0f;
    let masked = header[1] & 0x80 != 0;
    let mut len = u64::from(header[1] & 0x7f);
    if len == 126 {
        let mut extended = [0_u8; 2];
        stream
            .read_exact(&mut extended)
            .context("failed to read websocket extended length")?;
        len = u64::from(u16::from_be_bytes(extended));
    } else if len == 127 {
        let mut extended = [0_u8; 8];
        stream
            .read_exact(&mut extended)
            .context("failed to read websocket extended length")?;
        len = u64::from_be_bytes(extended);
    }
    let len_usize =
        usize::try_from(len).map_err(|_| anyhow!("websocket frame length overflows usize"))?;
    if len_usize > WEBSOCKET_MAX_FRAME_BYTES {
        return Err(anyhow!(
            "websocket frame exceeds {WEBSOCKET_MAX_FRAME_BYTES} bytes"
        ));
    }
    let mut mask = [0_u8; 4];
    if masked {
        stream
            .read_exact(&mut mask)
            .context("failed to read websocket mask")?;
    }
    let mut payload = vec![0_u8; len_usize];
    stream
        .read_exact(&mut payload)
        .context("failed to read websocket payload")?;
    if masked {
        for (index, byte) in payload.iter_mut().enumerate() {
            *byte ^= mask[index % 4];
        }
    }
    match opcode {
        0x1 => Ok(WebSocketFrame::Text(
            String::from_utf8(payload).context("websocket text frame is not UTF-8")?,
        )),
        0x2 => Ok(WebSocketFrame::Binary(payload)),
        0x8 => Ok(WebSocketFrame::Close),
        0x9 => Ok(WebSocketFrame::Ping(payload)),
        0xA => Ok(WebSocketFrame::Pong),
        _ => Err(anyhow!("unsupported websocket opcode {opcode}")),
    }
}

fn write_websocket_json(stream: &mut TcpStream, value: &Value) -> Result<()> {
    let payload = serde_json::to_vec(value).context("failed to serialize websocket message")?;
    write_websocket_frame(stream, 0x1, &payload)
}

fn write_websocket_close(stream: &mut TcpStream) -> Result<()> {
    write_websocket_frame(stream, 0x8, &[])?;
    let _ = stream.shutdown(Shutdown::Both);
    Ok(())
}

fn write_websocket_frame(stream: &mut TcpStream, opcode: u8, payload: &[u8]) -> Result<()> {
    let mut frame = Vec::with_capacity(payload.len() + 10);
    frame.push(0x80 | (opcode & 0x0f));
    if payload.len() <= 125 {
        frame.push(payload.len() as u8);
    } else if payload.len() <= u16::MAX as usize {
        frame.push(126);
        frame.extend_from_slice(&(payload.len() as u16).to_be_bytes());
    } else {
        frame.push(127);
        frame.extend_from_slice(&(payload.len() as u64).to_be_bytes());
    }
    frame.extend_from_slice(payload);
    stream
        .write_all(&frame)
        .context("failed to write websocket frame")
}

fn parse_websocket_subscribe(raw: &str) -> Result<WebSocketSubscribeRequest> {
    serde_json::from_str(raw).context("failed to parse websocket subscribe message")
}

fn parse_terminal_websocket_control(raw: &str) -> Result<TerminalWebSocketEvent> {
    let control: TerminalWebSocketControl =
        serde_json::from_str(raw).context("failed to parse terminal websocket control message")?;
    match control.kind.as_str() {
        "resize" => {
            let cols = control
                .cols
                .ok_or_else(|| anyhow!("terminal resize control is missing cols"))?;
            let rows = control
                .rows
                .ok_or_else(|| anyhow!("terminal resize control is missing rows"))?;
            Ok(TerminalWebSocketEvent::Resize(TerminalResizeRequest {
                cols,
                rows,
            }))
        }
        "attach" => Ok(TerminalWebSocketEvent::Attach(
            control.offset.unwrap_or_default(),
        )),
        "ping" => Ok(TerminalWebSocketEvent::JsonPing),
        other => Err(anyhow!(
            "unsupported terminal websocket control type {other}"
        )),
    }
}

fn write_terminal_replay(stream: &mut TcpStream, replay: &TerminalReplay) -> Result<()> {
    write_websocket_json(
        stream,
        &json!({
            "type": "attached",
            "terminal_id": &replay.terminal_id,
            "offset": replay.requested_offset,
            "replay_start_offset": replay.replay_start_offset,
            "buffer_start_offset": replay.buffer_start_offset,
            "next_offset": replay.next_offset,
            "running": replay.running,
        }),
    )?;
    if replay.dropped_bytes > 0 {
        write_websocket_json(
            stream,
            &json!({
                "type": "dropped",
                "terminal_id": &replay.terminal_id,
                "buffer_start_offset": replay.buffer_start_offset,
                "dropped_bytes": replay.dropped_bytes,
            }),
        )?;
    }
    for chunk in &replay.chunks {
        write_websocket_frame(stream, 0x2, &chunk.bytes)?;
    }
    Ok(())
}

fn write_terminal_websocket_error(stream: &mut TcpStream, error: WebTerminalError) -> Result<()> {
    let api_error = terminal_api_error(error);
    write_websocket_json(
        stream,
        &json!({
            "type": "error",
            "error": api_error.message,
            "status": api_error.status,
        }),
    )
}

fn websocket_event_for_subscriber(
    event: &Value,
    platform_chat_id: &str,
    subscriber: &WebSocketSubscriber,
) -> Value {
    let mut event = event.clone();
    if let Value::Object(map) = &mut event {
        map.entry("conversation_id".to_string())
            .or_insert_with(|| Value::String(subscriber.conversation_id.clone()));
        map.entry("platform_chat_id".to_string())
            .or_insert_with(|| Value::String(platform_chat_id.to_string()));
    }
    event
}

fn json_response(status: u16, value: Value) -> HttpResponse {
    HttpResponse {
        status,
        content_type: "application/json",
        body: serde_json::to_vec(&value).unwrap_or_else(|_| b"{}".to_vec()),
    }
}

fn json_error(status: u16, message: &str) -> HttpResponse {
    json_response(status, json!({"error": message}))
}

fn api_anyhow(error: ApiError) -> anyhow::Error {
    anyhow!("{} {}", error.status, error.message)
}

fn parse_json<T: for<'de> Deserialize<'de>>(body: &[u8]) -> ApiResult<T> {
    serde_json::from_slice(body).map_err(|error| ApiError::new(400, error.to_string()))
}

fn parse_optional_json<T: for<'de> Deserialize<'de> + Default>(body: &[u8]) -> ApiResult<T> {
    if body.iter().all(|byte| byte.is_ascii_whitespace()) {
        return Ok(T::default());
    }
    parse_json(body)
}

fn terminal_api_error(error: WebTerminalError) -> ApiError {
    match error {
        WebTerminalError::InvalidRequest(message) => ApiError::new(400, message),
        WebTerminalError::NotFound => ApiError::new(404, "terminal_not_found"),
        WebTerminalError::LimitExceeded(message) => ApiError::new(503, message),
        WebTerminalError::Internal(error) => ApiError::internal(error),
    }
}

fn api_segments(path: &str) -> Vec<&str> {
    path.trim_start_matches("/api")
        .trim_matches('/')
        .split('/')
        .filter(|segment| !segment.is_empty())
        .collect()
}

fn split_target(target: &str) -> (String, HashMap<String, String>) {
    let (path, raw_query) = target.split_once('?').unwrap_or((target, ""));
    let mut query = HashMap::new();
    for pair in raw_query.split('&').filter(|pair| !pair.is_empty()) {
        let (name, value) = pair.split_once('=').unwrap_or((pair, ""));
        query.insert(percent_decode(name), percent_decode(value));
    }
    (percent_decode(path), query)
}

fn percent_decode(value: &str) -> String {
    let bytes = value.as_bytes();
    let mut output = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' && index + 2 < bytes.len() {
            if let Ok(hex) = u8::from_str_radix(&value[index + 1..index + 3], 16) {
                output.push(hex);
                index += 3;
                continue;
            }
        }
        output.push(if bytes[index] == b'+' {
            b' '
        } else {
            bytes[index]
        });
        index += 1;
    }
    String::from_utf8_lossy(&output).to_string()
}

fn percent_encode_query_value(value: &str) -> String {
    let mut encoded = String::new();
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'~') {
            encoded.push(byte as char);
        } else {
            encoded.push_str(&format!("%{byte:02X}"));
        }
    }
    encoded
}

fn find_header_end(buffer: &[u8]) -> Option<usize> {
    buffer.windows(4).position(|window| window == b"\r\n\r\n")
}

fn status_reason(status: u16) -> &'static str {
    match status {
        200 => "OK",
        201 => "Created",
        202 => "Accepted",
        204 => "No Content",
        400 => "Bad Request",
        401 => "Unauthorized",
        404 => "Not Found",
        500 => "Internal Server Error",
        503 => "Service Unavailable",
        _ => "OK",
    }
}

fn message_page_payload(
    state: &ConversationState,
    attachments: &WebAttachmentContext,
    messages: &[ChatMessage],
    offset: usize,
    limit: usize,
) -> Value {
    let total = messages.len();
    let start = offset.min(total);
    let end = start.saturating_add(limit).min(total);
    let roots = attachments.roots();
    json!({
        "conversation_id": &state.conversation_id,
        "offset": offset,
        "limit": limit,
        "total": total,
        "messages": messages[start..end]
            .iter()
            .enumerate()
            .map(|(relative, message)| message_skeleton(start + relative, message, attachments, &roots))
            .collect::<Vec<_>>(),
    })
}

fn message_range_payload(
    state: &ConversationState,
    attachments: &WebAttachmentContext,
    messages: &[ChatMessage],
    anchor: usize,
    direction: MessageRangeDirection,
    include_anchor: bool,
    start: usize,
    end: usize,
) -> Value {
    let roots = attachments.roots();
    json!({
        "conversation_id": &state.conversation_id,
        "anchor_id": anchor.to_string(),
        "anchor_index": anchor,
        "direction": direction.as_str(),
        "include_anchor": include_anchor,
        "offset": start,
        "limit": end.saturating_sub(start),
        "start_index": start,
        "end_index": end,
        "total": messages.len(),
        "has_more_before": start > 0,
        "has_more_after": end < messages.len(),
        "messages": messages[start..end]
            .iter()
            .enumerate()
            .map(|(relative, message)| message_skeleton(start + relative, message, attachments, &roots))
            .collect::<Vec<_>>(),
    })
}

fn websocket_subscription_ack(
    state: &ConversationState,
    next_message_index: usize,
    reason: &'static str,
) -> Value {
    let current_message_id = next_message_index
        .checked_sub(1)
        .map(|index| index.to_string());
    json!({
        "type": "subscription_ack",
        "subscription": "foreground_session_messages",
        "reason": reason,
        "conversation_id": &state.conversation_id,
        "session_id": &state.session_binding.foreground_session_id,
        "current_message_id": current_message_id,
        "next_message_id": next_message_index.to_string(),
        "total": next_message_index,
    })
}

fn websocket_messages_payload(
    state: &ConversationState,
    attachments: &WebAttachmentContext,
    messages: &[ChatMessage],
    start: usize,
) -> Value {
    let total = messages.len();
    let end = total;
    let roots = attachments.roots();
    json!({
        "type": "messages",
        "subscription": "foreground_session_messages",
        "conversation_id": &state.conversation_id,
        "session_id": &state.session_binding.foreground_session_id,
        "offset": start,
        "start_index": start,
        "end_index": end,
        "total": total,
        "messages": messages[start..end]
            .iter()
            .enumerate()
            .map(|(relative, message)| message_skeleton(start + relative, message, attachments, &roots))
            .collect::<Vec<_>>(),
    })
}

fn message_range_bounds(
    total: usize,
    anchor: usize,
    direction: MessageRangeDirection,
    include_anchor: bool,
    limit: usize,
) -> (usize, usize) {
    if limit == 0 || total == 0 {
        return (0, 0);
    }
    match direction {
        MessageRangeDirection::Before => {
            let end = if include_anchor {
                anchor.saturating_add(1).min(total)
            } else {
                anchor.min(total)
            };
            (end.saturating_sub(limit), end)
        }
        MessageRangeDirection::After => {
            let start = if include_anchor {
                anchor.min(total)
            } else {
                anchor.saturating_add(1).min(total)
            };
            (start, start.saturating_add(limit).min(total))
        }
    }
}

fn message_skeleton(
    index: usize,
    message: &ChatMessage,
    attachments: &WebAttachmentContext,
    roots: &WebAttachmentRoots,
) -> MessageSkeleton {
    let rendered = render_web_message(message, attachments, roots);
    MessageSkeleton {
        id: index.to_string(),
        index,
        role: message.role.clone(),
        text: rendered.text.clone(),
        text_with_attachment_markers: rendered.text_with_attachment_markers.clone(),
        preview: preview_text(&rendered.text),
        items: rendered.items,
        attachment_count: rendered.attachments.len(),
        attachments: rendered.attachments,
        has_attachment_errors: !rendered.attachment_errors.is_empty(),
        user_name: message.user_name.clone(),
        message_time: message.message_time.clone(),
        has_token_usage: message.token_usage.is_some(),
        token_usage: message.token_usage.as_ref().map(WebTokenUsage::from),
    }
}

fn render_web_message(
    message: &ChatMessage,
    context: &WebAttachmentContext,
    roots: &WebAttachmentRoots,
) -> WebRenderedMessage {
    let mut parts = Vec::new();
    let mut marked_parts = Vec::new();
    let mut items = Vec::new();
    let mut attachments = Vec::new();
    let mut attachment_errors = Vec::new();

    for (item_index, item) in message.data.iter().enumerate() {
        match item {
            ChatMessageItem::Reasoning(_) => {}
            ChatMessageItem::Context(context_item) => {
                let part = render_web_text_part(
                    &context_item.text,
                    context,
                    roots,
                    &mut attachments,
                    &mut attachment_errors,
                );
                if !part.text.is_empty() || !part.text_with_attachment_markers.is_empty() {
                    parts.push(part.text.clone());
                    marked_parts.push(part.text_with_attachment_markers.clone());
                    items.push(WebMessageItem::Text {
                        index: item_index,
                        text: part.text,
                        text_with_attachment_markers: part.text_with_attachment_markers,
                    });
                }
            }
            ChatMessageItem::File(file) => {
                let attachment_index = attachments.len();
                attachments.push(web_file_item_attachment(
                    attachment_index,
                    "message_file",
                    file,
                    context,
                    roots,
                ));
                items.push(WebMessageItem::File {
                    index: item_index,
                    attachment_index,
                });
            }
            ChatMessageItem::ToolCall(tool_call) => items.push(WebMessageItem::ToolCall {
                index: item_index,
                tool_call_id: tool_call.tool_call_id.clone(),
                tool_name: tool_call.tool_name.clone(),
                arguments: parse_tool_arguments(&tool_call.arguments.text),
            }),
            ChatMessageItem::ToolResult(tool_result) => {
                let mut context_with_attachment_markers = None;
                let context_text = tool_result
                    .result
                    .context
                    .as_ref()
                    .and_then(|context_item| {
                        let part = render_web_text_part(
                            &context_item.text,
                            context,
                            roots,
                            &mut attachments,
                            &mut attachment_errors,
                        );
                        if part.text.is_empty() && part.text_with_attachment_markers.is_empty() {
                            None
                        } else {
                            context_with_attachment_markers =
                                Some(part.text_with_attachment_markers);
                            Some(part.text)
                        }
                    });
                let file_attachment_index = if let Some(file) = &tool_result.result.file {
                    let attachment_index = attachments.len();
                    attachments.push(web_file_item_attachment(
                        attachment_index,
                        "tool_result_file",
                        file,
                        context,
                        roots,
                    ));
                    Some(attachment_index)
                } else {
                    None
                };
                items.push(WebMessageItem::ToolResult {
                    index: item_index,
                    tool_call_id: tool_result.tool_call_id.clone(),
                    tool_name: tool_result.tool_name.clone(),
                    context: context_text,
                    context_with_attachment_markers,
                    file_attachment_index,
                });
            }
        }
    }

    WebRenderedMessage {
        text: parts.join("\n\n"),
        text_with_attachment_markers: marked_parts.join("\n\n"),
        items,
        attachments,
        attachment_errors,
    }
}

fn render_web_text_part(
    raw_text: &str,
    context: &WebAttachmentContext,
    roots: &WebAttachmentRoots,
    attachments: &mut Vec<WebMessageAttachment>,
    attachment_errors: &mut Vec<String>,
) -> WebRenderedTextPart {
    if !raw_text.contains("<attachment>") {
        return WebRenderedTextPart {
            text: raw_text.to_string(),
            text_with_attachment_markers: raw_text.to_string(),
        };
    }

    let base_attachment_index = attachments.len();
    match extract_attachment_references_with_markers(
        raw_text,
        &roots.workspace_root,
        &roots.shared_root,
        base_attachment_index,
    ) {
        Ok(extracted) => {
            for (relative_index, attachment) in extracted.attachments.into_iter().enumerate() {
                let attachment_index = attachments.len();
                attachments.push(web_outgoing_attachment(
                    attachment_index,
                    "attachment_tag",
                    Some(attachment_marker(base_attachment_index + relative_index)),
                    &attachment,
                    context,
                    roots,
                ));
            }
            WebRenderedTextPart {
                text: extracted.clean_text,
                text_with_attachment_markers: extracted.marked_text,
            }
        }
        Err(error) => {
            let clean = strip_attachment_tags(raw_text).trim().to_string();
            attachment_errors.push(format!("{error:#}"));
            WebRenderedTextPart {
                text: clean.clone(),
                text_with_attachment_markers: clean,
            }
        }
    }
}

fn parse_tool_arguments(raw_text: &str) -> Value {
    serde_json::from_str(raw_text).unwrap_or_else(|_| Value::String(raw_text.to_string()))
}

fn web_outgoing_attachment(
    index: usize,
    source: &'static str,
    marker: Option<String>,
    attachment: &OutgoingAttachment,
    context: &WebAttachmentContext,
    roots: &WebAttachmentRoots,
) -> WebMessageAttachment {
    let path = attachment_workspace_path(&attachment.path, roots)
        .unwrap_or_else(|| attachment.path.display().to_string());
    let size_bytes = fs::metadata(&attachment.path)
        .map(|metadata| metadata.len())
        .ok();
    let kind = outgoing_attachment_kind_name(attachment.kind);
    let image_preview = (kind == "image")
        .then(|| {
            context
                .cache_manager
                .image_thumbnail(&context.conversation_id, &attachment.path)
        })
        .flatten();
    WebMessageAttachment {
        index,
        source,
        kind,
        name: attachment
            .path
            .file_name()
            .and_then(|value| value.to_str())
            .unwrap_or("attachment")
            .to_string(),
        uri: format!("file://{}", attachment.path.display()),
        media_type: None,
        width: image_preview.as_ref().map(|preview| preview.original_width),
        height: image_preview
            .as_ref()
            .map(|preview| preview.original_height),
        size_bytes,
        url: format!(
            "/api/conversations/{}/workspace/file?path={}",
            percent_encode_query_value(&context.conversation_id),
            percent_encode_query_value(&path)
        ),
        marker,
        thumbnail: image_preview.map(|preview| preview.thumbnail),
        path,
    }
}

fn web_file_item_attachment(
    index: usize,
    source: &'static str,
    file: &FileItem,
    context: &WebAttachmentContext,
    roots: &WebAttachmentRoots,
) -> WebMessageAttachment {
    let local_path = local_path_from_file_item(file);
    let workspace_path = local_path
        .as_deref()
        .and_then(|path| attachment_workspace_path(path, roots));
    let path = workspace_path.clone().unwrap_or_else(|| file.uri.clone());
    let url = workspace_path
        .as_ref()
        .map(|path| {
            format!(
                "/api/conversations/{}/workspace/file?path={}",
                percent_encode_query_value(&context.conversation_id),
                percent_encode_query_value(path)
            )
        })
        .unwrap_or_default();
    let size_bytes = local_path
        .as_deref()
        .and_then(|path| fs::metadata(path).ok())
        .map(|metadata| metadata.len());
    let kind = file_attachment_kind_name(file);
    let image_preview = (kind == "image")
        .then(|| {
            local_path.as_deref().and_then(|path| {
                context
                    .cache_manager
                    .image_thumbnail(&context.conversation_id, path)
            })
        })
        .flatten();
    WebMessageAttachment {
        index,
        source,
        kind,
        path,
        uri: file.uri.clone(),
        name: file
            .name
            .clone()
            .or_else(|| {
                local_path
                    .as_deref()
                    .and_then(|path| path.file_name())
                    .and_then(|value| value.to_str())
                    .map(ToString::to_string)
            })
            .unwrap_or_else(|| "attachment".to_string()),
        media_type: file.media_type.clone(),
        width: file
            .width
            .or_else(|| image_preview.as_ref().map(|preview| preview.original_width)),
        height: file.height.or_else(|| {
            image_preview
                .as_ref()
                .map(|preview| preview.original_height)
        }),
        size_bytes,
        url,
        marker: None,
        thumbnail: image_preview.map(|preview| preview.thumbnail),
    }
}

fn attachment_workspace_path(path: &Path, roots: &WebAttachmentRoots) -> Option<String> {
    if let Ok(relative) = path.strip_prefix(&roots.workspace_root) {
        return Some(path_to_api_string(relative));
    }
    path.strip_prefix(&roots.shared_root)
        .ok()
        .map(|relative| format!("shared/{}", path_to_api_string(relative)))
}

fn local_path_from_file_item(file: &FileItem) -> Option<PathBuf> {
    if let Some(path) = file.uri.strip_prefix("file://") {
        return Some(PathBuf::from(percent_decode(path)));
    }
    let path = Path::new(&file.uri);
    path.is_absolute().then(|| path.to_path_buf())
}

fn path_to_api_string(path: &Path) -> String {
    path.components()
        .filter_map(|component| match component {
            std::path::Component::Normal(value) => Some(value.to_string_lossy().to_string()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("/")
}

fn outgoing_attachment_kind_name(kind: OutgoingAttachmentKind) -> &'static str {
    match kind {
        OutgoingAttachmentKind::Image => "image",
        OutgoingAttachmentKind::Audio => "audio",
        OutgoingAttachmentKind::Voice => "voice",
        OutgoingAttachmentKind::Video => "video",
        OutgoingAttachmentKind::Animation => "animation",
        OutgoingAttachmentKind::Document => "document",
    }
}

fn processing_state_name(state: ProcessingState) -> &'static str {
    match state {
        ProcessingState::Idle => "idle",
        ProcessingState::Typing => "typing",
    }
}

fn turn_progress_payload(feedback: &OutgoingProgressFeedback, text: &str) -> Value {
    json!({
        "type": "turn_progress",
        "subscription": "foreground_session_events",
        "turn_id": &feedback.turn_id,
        "phase": feedback.progress.phase,
        "model": &feedback.progress.model,
        "activity": &feedback.progress.activity,
        "hint": &feedback.progress.hint,
        "plan": &feedback.progress.plan,
        "error": &feedback.progress.error,
        "progress": &feedback.progress,
        "text": text,
        "final_state": feedback.final_state.map(progress_final_state_name),
        "important": feedback.important,
    })
}

fn web_progress_text(feedback: &OutgoingProgressFeedback) -> String {
    let progress = &feedback.progress;
    let mut text = match progress.phase {
        TurnProgressPhase::Thinking => {
            format!(
                "正在执行\n模型：{}\n状态：{}",
                progress.model, progress.activity
            )
        }
        TurnProgressPhase::Working => {
            format!(
                "正在执行\n模型：{}\n阶段：{}",
                progress.model, progress.activity
            )
        }
        TurnProgressPhase::Done => format!("已完成\n模型：{}", progress.model),
        TurnProgressPhase::Failed => {
            let mut text = format!("本轮失败\n模型：{}", progress.model);
            if let Some(error) = progress
                .error
                .as_deref()
                .map(str::trim)
                .filter(|value| !value.is_empty())
            {
                text.push_str("\n");
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
        text.push_str("\n\n");
        text.push_str(hint);
    }
    text
}

fn progress_final_state_name(state: ProgressFeedbackFinalState) -> &'static str {
    match state {
        ProgressFeedbackFinalState::Done => "done",
        ProgressFeedbackFinalState::Failed => "failed",
    }
}

fn file_attachment_kind_name(file: &FileItem) -> &'static str {
    let media_type = file.media_type.as_deref().unwrap_or_default();
    if media_type.starts_with("image/") {
        return "image";
    }
    if media_type.starts_with("audio/") {
        return "audio";
    }
    if media_type.starts_with("video/") {
        return "video";
    }
    local_path_from_file_item(file)
        .as_deref()
        .map(infer_web_attachment_kind_from_path)
        .unwrap_or("document")
}

fn infer_web_attachment_kind_from_path(path: &Path) -> &'static str {
    match path
        .extension()
        .and_then(|value| value.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase()
        .as_str()
    {
        "png" | "jpg" | "jpeg" | "webp" => "image",
        "gif" => "animation",
        "mp3" | "wav" => "audio",
        "ogg" => "voice",
        "mp4" | "mov" | "mkv" => "video",
        _ => "document",
    }
}

fn preview_text(text: &str) -> String {
    text.trim().to_string()
}

fn read_messages_jsonl(path: &Path) -> Result<Vec<ChatMessage>> {
    let raw =
        fs::read_to_string(path).with_context(|| format!("failed to read {}", path.display()))?;
    let mut messages = Vec::new();
    for line in raw.lines().filter(|line| !line.trim().is_empty()) {
        messages.push(
            serde_json::from_str::<ChatMessage>(line)
                .with_context(|| format!("failed to parse {}", path.display()))?,
        );
    }
    Ok(messages)
}

fn query_usize(query: &HashMap<String, String>, name: &str, default: usize) -> usize {
    query
        .get(name)
        .and_then(|value| value.parse::<usize>().ok())
        .unwrap_or(default)
}

fn query_bool(query: &HashMap<String, String>, name: &str, default: bool) -> bool {
    query
        .get(name)
        .map(|value| matches!(value.as_str(), "1" | "true" | "yes" | "on"))
        .unwrap_or(default)
}

fn parse_message_id(message_id: &str) -> ApiResult<usize> {
    message_id
        .parse::<usize>()
        .map_err(|_| ApiError::new(400, "message id must be a numeric index"))
}

fn validate_conversation_id(conversation_id: &str) -> ApiResult<()> {
    if conversation_id.trim().is_empty()
        || conversation_id.contains('/')
        || conversation_id.contains('\\')
        || conversation_id.contains("..")
    {
        return Err(ApiError::new(400, "invalid conversation id"));
    }
    Ok(())
}

fn sanitize_session_id_for_log_path(session_id: &str) -> String {
    session_id
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '.') {
                ch
            } else {
                '_'
            }
        })
        .collect()
}

fn generated_platform_id() -> String {
    format!("web-{}", unix_millis())
}

fn generated_message_id() -> String {
    format!("web-message-{}", unix_millis())
}

fn normalize_conversation_nickname(conversation_id: &str, nickname: &str) -> String {
    let trimmed = nickname.trim();
    if trimmed.is_empty() {
        conversation_id.to_string()
    } else {
        trimmed.to_string()
    }
}

fn conversation_nickname(state: &ConversationState) -> String {
    normalize_conversation_nickname(&state.conversation_id, &state.nickname)
}

fn unix_millis() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis())
        .unwrap_or_default()
}

fn now_rfc3339() -> String {
    OffsetDateTime::now_utc()
        .format(&Rfc3339)
        .unwrap_or_else(|_| "1970-01-01T00:00:00Z".to_string())
}

fn parse_web_control(text: &str) -> Option<ConversationControl> {
    let (command, argument) = text.split_once(char::is_whitespace).unwrap_or((text, ""));
    let argument = argument.trim();
    match command {
        "/continue" if argument.is_empty() => Some(ConversationControl::Continue),
        "/cancel" if argument.is_empty() => Some(ConversationControl::Cancel),
        "/compact" if argument.is_empty() => Some(ConversationControl::Compact),
        "/status" if argument.is_empty() => Some(ConversationControl::ShowStatus),
        "/model" if argument.is_empty() => Some(ConversationControl::ShowModel),
        "/model" => Some(ConversationControl::SwitchModel {
            model_name: argument.to_string(),
        }),
        "/remote" if argument.is_empty() => Some(ConversationControl::ShowRemote),
        "/remote" if argument.eq_ignore_ascii_case("off") => {
            Some(ConversationControl::DisableRemote)
        }
        "/remote" => Some(parse_web_remote_control(argument)),
        _ => None,
    }
}

fn parse_web_remote_control(argument: &str) -> ConversationControl {
    let mut parts = argument.trim().splitn(2, char::is_whitespace);
    let host = parts.next().unwrap_or_default().trim();
    let path = parts.next().map(str::trim).unwrap_or_default();
    if host.is_empty() || path.is_empty() {
        return ConversationControl::InvalidRemote {
            reason: "remote command requires host and path.".to_string(),
        };
    }
    ConversationControl::SetRemote {
        host: host.to_string(),
        path: path.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{collections::BTreeMap, fs};

    use crate::{
        channels::types::{
            TurnProgress, TurnProgressPlan, TurnProgressPlanItem, TurnProgressPlanItemStatus,
        },
        config::{ModelSelection, SessionProfile},
        conversation::ConversationSessionBinding,
    };
    use stellaclaw_core::session_actor::{
        ChatMessageItem, ContextItem, TokenUsageCost, ToolRemoteMode,
    };

    fn test_workdir(name: &str) -> PathBuf {
        let path = std::env::temp_dir().join(format!(
            "stellaclaw-web-{name}-{}-{}",
            std::process::id(),
            unix_millis()
        ));
        fs::create_dir_all(&path).expect("create temp workdir");
        path
    }

    fn test_state(conversation_id: &str) -> ConversationState {
        ConversationState {
            version: 1,
            conversation_id: conversation_id.to_string(),
            nickname: conversation_id.to_string(),
            channel_id: "web-main".to_string(),
            platform_chat_id: "test-chat".to_string(),
            session_profile: SessionProfile {
                main_model: ModelSelection::alias("main"),
            },
            model_selection_pending: false,
            tool_remote_mode: ToolRemoteMode::Selectable,
            sandbox: None,
            reasoning_effort: None,
            session_binding: ConversationSessionBinding {
                foreground_session_id: format!("{conversation_id}.foreground"),
                next_background_index: 1,
                next_subagent_index: 1,
                background_sessions: BTreeMap::new(),
                subagent_sessions: BTreeMap::new(),
            },
        }
    }

    #[test]
    fn parses_api_segments() {
        assert_eq!(
            api_segments("/api/conversations/abc/messages"),
            vec!["conversations", "abc", "messages"]
        );
    }

    #[test]
    fn parses_web_control_commands() {
        assert!(matches!(
            parse_web_control("/model main"),
            Some(ConversationControl::SwitchModel { model_name }) if model_name == "main"
        ));
        assert!(matches!(
            parse_web_control("/status"),
            Some(ConversationControl::ShowStatus)
        ));
        assert!(matches!(
            parse_web_control("/compact"),
            Some(ConversationControl::Compact)
        ));
        assert!(matches!(
            parse_web_control("/remote demo-host ~/repo"),
            Some(ConversationControl::SetRemote { host, path })
                if host == "demo-host" && path == "~/repo"
        ));
        assert!(matches!(
            parse_web_control("/remote off"),
            Some(ConversationControl::DisableRemote)
        ));
    }

    #[test]
    fn parses_terminal_websocket_control_frames() {
        assert!(matches!(
            parse_terminal_websocket_control(r#"{"type":"attach","offset":123}"#)
                .expect("parse attach"),
            TerminalWebSocketEvent::Attach(123)
        ));
        assert!(matches!(
            parse_terminal_websocket_control(r#"{"type":"resize","cols":120,"rows":32}"#)
                .expect("parse resize"),
            TerminalWebSocketEvent::Resize(TerminalResizeRequest {
                cols: 120,
                rows: 32
            })
        ));
        assert!(matches!(
            parse_terminal_websocket_control(r#"{"type":"ping"}"#).expect("parse ping"),
            TerminalWebSocketEvent::JsonPing
        ));
        assert!(parse_terminal_websocket_control(r#"{"type":"resize","cols":120}"#).is_err());
    }

    #[test]
    fn turn_progress_payload_is_structured() {
        let feedback = OutgoingProgressFeedback {
            channel_id: "web-main".to_string(),
            platform_chat_id: "test-chat".to_string(),
            turn_id: "turn-1".to_string(),
            progress: TurnProgress {
                phase: TurnProgressPhase::Working,
                model: "gpt-5.5".to_string(),
                activity: "读取代码".to_string(),
                hint: Some("发送新消息可打断".to_string()),
                plan: Some(TurnProgressPlan {
                    explanation: Some("先确认链路".to_string()),
                    items: vec![TurnProgressPlanItem {
                        step: "检查 ChannelEvent".to_string(),
                        status: TurnProgressPlanItemStatus::InProgress,
                    }],
                }),
                error: None,
            },
            final_state: None,
            important: true,
        };

        let text = web_progress_text(&feedback);
        let payload = turn_progress_payload(&feedback, &text);

        assert_eq!(payload["type"], "turn_progress");
        assert_eq!(payload["subscription"], "foreground_session_events");
        assert_eq!(payload["turn_id"], "turn-1");
        assert_eq!(payload["phase"], "working");
        assert_eq!(payload["model"], "gpt-5.5");
        assert_eq!(payload["activity"], "读取代码");
        assert_eq!(payload["hint"], "发送新消息可打断");
        assert_eq!(payload["final_state"], serde_json::Value::Null);
        assert_eq!(payload["important"], true);
        assert_eq!(payload["plan"]["items"][0]["status"], "in_progress");
        assert_eq!(payload["progress"]["phase"], "working");
        assert!(payload["text"]
            .as_str()
            .is_some_and(|value| value.contains("读取代码")));
    }

    #[test]
    fn web_message_rendering_extracts_attachment_metadata() {
        let workdir = test_workdir("attachment");
        let state = test_state("web-main-test-attachment");
        let conversation_root = workdir.join("conversations").join(&state.conversation_id);
        fs::create_dir_all(&conversation_root).expect("create conversation root");
        fs::write(conversation_root.join("report.txt"), "hello").expect("write attachment");
        let message = ChatMessage::new(
            ChatRole::Assistant,
            vec![ChatMessageItem::Context(ContextItem {
                text: "done\n<attachment>report.txt</attachment>".to_string(),
            })],
        );

        let context = test_attachment_context(&workdir, &state);
        let roots = context.roots();
        let rendered = render_web_message(&message, &context, &roots);

        assert_eq!(rendered.text, "done");
        assert_eq!(
            rendered.text_with_attachment_markers,
            "done\n[[attachment:0]]"
        );
        assert_eq!(rendered.items.len(), 1);
        assert!(matches!(
            &rendered.items[0],
            WebMessageItem::Text { text, text_with_attachment_markers, .. }
                if text == "done" && text_with_attachment_markers == "done\n[[attachment:0]]"
        ));
        assert!(rendered.attachment_errors.is_empty());
        assert_eq!(rendered.attachments.len(), 1);
        assert_eq!(rendered.attachments[0].kind, "document");
        assert_eq!(rendered.attachments[0].path, "report.txt");
        assert_eq!(rendered.attachments[0].name, "report.txt");
        assert_eq!(rendered.attachments[0].source, "attachment_tag");
        assert_eq!(
            rendered.attachments[0].marker.as_deref(),
            Some("[[attachment:0]]")
        );
        assert_eq!(rendered.attachments[0].size_bytes, Some(5));
        assert_eq!(
            rendered.attachments[0].url,
            "/api/conversations/web-main-test-attachment/workspace/file?path=report.txt"
        );

        let skeleton = message_skeleton(7, &message, &context, &roots);
        assert_eq!(skeleton.preview, "done");
        assert_eq!(skeleton.text, "done");
        assert_eq!(
            skeleton.text_with_attachment_markers,
            "done\n[[attachment:0]]"
        );
        assert_eq!(skeleton.items.len(), 1);
        assert_eq!(skeleton.attachment_count, 1);
        assert_eq!(skeleton.attachments.len(), 1);
        assert!(!skeleton.has_attachment_errors);

        let _ = fs::remove_dir_all(workdir);
    }

    #[test]
    fn web_message_skeleton_returns_tool_calls_as_structured_items() {
        let workdir = test_workdir("tool-call-items");
        let state = test_state("web-main-test-tool-call-items");
        fs::create_dir_all(workdir.join("conversations").join(&state.conversation_id))
            .expect("create conversation root");
        let message = ChatMessage::new(
            ChatRole::Assistant,
            vec![ChatMessageItem::ToolCall(
                stellaclaw_core::session_actor::ToolCallItem {
                    tool_call_id: "call_1".to_string(),
                    tool_name: "shell".to_string(),
                    arguments: ContextItem {
                        text: r#"{"cmd":"ls -la","timeout_seconds":5}"#.to_string(),
                    },
                },
            )],
        );

        let context = test_attachment_context(&workdir, &state);
        let payload = message_page_payload(&state, &context, &[message], 0, 50);

        assert_eq!(payload["messages"][0]["preview"], "");
        assert_eq!(payload["messages"][0]["text"], "");
        assert_eq!(payload["messages"][0]["items"][0]["type"], "tool_call");
        assert_eq!(payload["messages"][0]["items"][0]["tool_call_id"], "call_1");
        assert_eq!(payload["messages"][0]["items"][0]["tool_name"], "shell");
        assert_eq!(
            payload["messages"][0]["items"][0]["arguments"]["cmd"],
            "ls -la"
        );
        assert_eq!(
            payload["messages"][0]["items"][0]["arguments"]["timeout_seconds"],
            5
        );

        let _ = fs::remove_dir_all(workdir);
    }

    #[test]
    fn web_message_payloads_include_token_usage() {
        let workdir = test_workdir("token-usage");
        let state = test_state("web-main-test-token-usage");
        fs::create_dir_all(workdir.join("conversations").join(&state.conversation_id))
            .expect("create conversation root");
        let message = ChatMessage::new(
            ChatRole::Assistant,
            vec![ChatMessageItem::Context(ContextItem {
                text: "done".to_string(),
            })],
        )
        .with_token_usage(TokenUsage {
            cache_read: 11,
            cache_write: 12,
            uncache_input: 13,
            output: 14,
            cost_usd: Some(TokenUsageCost {
                cache_read: 0.001,
                cache_write: 0.002,
                uncache_input: 0.003,
                output: 0.004,
            }),
        });

        let context = test_attachment_context(&workdir, &state);
        let page = message_page_payload(&state, &context, std::slice::from_ref(&message), 0, 50);
        assert_eq!(page["messages"][0]["has_token_usage"], true);
        assert_eq!(page["messages"][0]["token_usage"]["cache_read"], 11);
        assert_eq!(page["messages"][0]["token_usage"]["cache_write"], 12);
        assert_eq!(page["messages"][0]["token_usage"]["uncache_input"], 13);
        assert_eq!(page["messages"][0]["token_usage"]["input"], 13);
        assert_eq!(page["messages"][0]["token_usage"]["output"], 14);
        assert_eq!(page["messages"][0]["token_usage"]["total"], 50);
        assert_eq!(
            page["messages"][0]["token_usage"]["cost_usd"]["output"],
            0.004
        );

        let websocket =
            websocket_messages_payload(&state, &context, std::slice::from_ref(&message), 0);
        assert_eq!(websocket["messages"][0]["has_token_usage"], true);
        assert_eq!(websocket["messages"][0]["token_usage"]["cache_read"], 11);
        assert_eq!(websocket["messages"][0]["token_usage"]["input"], 13);
        assert_eq!(websocket["messages"][0]["token_usage"]["total"], 50);
        assert_eq!(
            websocket["messages"][0]["token_usage"]["cost_usd"]["uncache_input"],
            0.003
        );

        let _ = fs::remove_dir_all(workdir);
    }

    #[test]
    fn conversation_nickname_falls_back_to_conversation_id() {
        let mut state = test_state("web-main-test-nickname");
        state.nickname = " Project Alpha ".to_string();
        assert_eq!(conversation_nickname(&state), "Project Alpha");

        state.nickname.clear();
        assert_eq!(conversation_nickname(&state), state.conversation_id);
    }

    #[test]
    fn web_message_range_payload_returns_backward_window() {
        let workdir = test_workdir("message-range");
        let state = test_state("web-main-test-message-range");
        fs::create_dir_all(workdir.join("conversations").join(&state.conversation_id))
            .expect("create conversation root");
        let messages = (0..5)
            .map(|index| {
                ChatMessage::new(
                    ChatRole::Assistant,
                    vec![ChatMessageItem::Context(ContextItem {
                        text: format!("message {index}"),
                    })],
                )
            })
            .collect::<Vec<_>>();
        let (start, end) =
            message_range_bounds(messages.len(), 3, MessageRangeDirection::Before, true, 2);
        let context = test_attachment_context(&workdir, &state);
        let payload = message_range_payload(
            &state,
            &context,
            &messages,
            3,
            MessageRangeDirection::Before,
            true,
            start,
            end,
        );

        assert_eq!(payload["anchor_id"], "3");
        assert_eq!(payload["direction"], "before");
        assert_eq!(payload["start_index"], 2);
        assert_eq!(payload["end_index"], 4);
        assert_eq!(payload["has_more_before"], true);
        assert_eq!(payload["has_more_after"], true);
        assert_eq!(payload["messages"][0]["id"], "2");
        assert_eq!(payload["messages"][1]["id"], "3");
        assert_eq!(payload["messages"][1]["text"], "message 3");

        let _ = fs::remove_dir_all(workdir);
    }

    #[test]
    fn websocket_ack_reports_current_and_next_message_ids() {
        let state = test_state("web-main-test-ws-ack");
        let payload = websocket_subscription_ack(&state, 3, "subscribed");

        assert_eq!(payload["type"], "subscription_ack");
        assert_eq!(payload["subscription"], "foreground_session_messages");
        assert_eq!(payload["reason"], "subscribed");
        assert_eq!(payload["current_message_id"], "2");
        assert_eq!(payload["next_message_id"], "3");
        assert_eq!(payload["session_id"], "web-main-test-ws-ack.foreground");

        let empty = websocket_subscription_ack(&state, 0, "subscribed");
        assert!(empty["current_message_id"].is_null());
        assert_eq!(empty["next_message_id"], "0");
    }

    #[test]
    fn web_message_skeleton_keeps_full_text_preview() {
        let workdir = test_workdir("full-preview");
        let state = test_state("web-main-test-full-preview");
        fs::create_dir_all(workdir.join("conversations").join(&state.conversation_id))
            .expect("create conversation root");
        let text = "0123456789".repeat(40);
        let message = ChatMessage::new(
            ChatRole::Assistant,
            vec![ChatMessageItem::Context(ContextItem { text: text.clone() })],
        );

        let context = test_attachment_context(&workdir, &state);
        let roots = context.roots();
        let skeleton = message_skeleton(7, &message, &context, &roots);

        assert_eq!(skeleton.preview, text);
        assert!(!skeleton.preview.ends_with("..."));

        let _ = fs::remove_dir_all(workdir);
    }

    #[test]
    fn web_message_rendering_inlines_image_attachment_thumbnail() {
        let workdir = test_workdir("attachment-image-thumbnail");
        let state = test_state("web-main-test-attachment-image-thumbnail");
        let conversation_root = workdir.join("conversations").join(&state.conversation_id);
        fs::create_dir_all(&conversation_root).expect("create conversation root");
        let image_path = conversation_root.join("photo.png");
        write_test_image(&image_path);
        let message = ChatMessage::new(
            ChatRole::Assistant,
            vec![ChatMessageItem::Context(ContextItem {
                text: "photo\n<attachment>photo.png</attachment>".to_string(),
            })],
        );

        let context = test_attachment_context(&workdir, &state);
        let roots = context.roots();
        let rendered = render_web_message(&message, &context, &roots);

        assert_eq!(rendered.text, "photo");
        assert_eq!(
            rendered.text_with_attachment_markers,
            "photo\n[[attachment:0]]"
        );
        assert!(rendered.attachment_errors.is_empty());
        assert_eq!(rendered.attachments.len(), 1);
        assert_eq!(rendered.attachments[0].source, "attachment_tag");
        assert_eq!(
            rendered.attachments[0].marker.as_deref(),
            Some("[[attachment:0]]")
        );
        assert_eq!(rendered.attachments[0].kind, "image");
        assert_eq!(rendered.attachments[0].path, "photo.png");
        assert_eq!(rendered.attachments[0].width, Some(800));
        assert_eq!(rendered.attachments[0].height, Some(600));
        let thumbnail = rendered.attachments[0]
            .thumbnail
            .as_ref()
            .expect("image attachment should include thumbnail");
        assert_eq!(thumbnail.media_type, "image/jpeg");
        assert_eq!(thumbnail.width, 800);
        assert_eq!(thumbnail.height, 600);
        assert!(!thumbnail.data_base64.is_empty());
        assert!(thumbnail.size_bytes <= 256 * 1024);
        assert!(thumbnail.data_url.starts_with("data:image/jpeg;base64,"));

        let payload = message_page_payload(&state, &context, &[message], 0, 50);
        assert_eq!(
            payload["messages"][0]["attachments"][0]["source"],
            "attachment_tag"
        );
        assert_eq!(
            payload["messages"][0]["text_with_attachment_markers"],
            "photo\n[[attachment:0]]"
        );
        assert_eq!(
            payload["messages"][0]["attachments"][0]["marker"],
            "[[attachment:0]]"
        );
        assert_eq!(
            payload["messages"][0]["items"][0]["text_with_attachment_markers"],
            "photo\n[[attachment:0]]"
        );
        assert!(
            payload["messages"][0]["attachments"][0]["thumbnail"]["data_base64"]
                .as_str()
                .unwrap()
                .len()
                > 10
        );

        let _ = fs::remove_dir_all(workdir);
    }

    #[test]
    fn web_message_rendering_reports_attachment_errors_without_raw_tags() {
        let workdir = test_workdir("attachment-missing");
        let state = test_state("web-main-test-attachment-missing");
        fs::create_dir_all(workdir.join("conversations").join(&state.conversation_id))
            .expect("create conversation root");
        let message = ChatMessage::new(
            ChatRole::Assistant,
            vec![ChatMessageItem::Context(ContextItem {
                text: "done\n<attachment>missing.txt</attachment>".to_string(),
            })],
        );

        let context = test_attachment_context(&workdir, &state);
        let roots = context.roots();
        let rendered = render_web_message(&message, &context, &roots);

        assert_eq!(rendered.text, "done\nmissing.txt");
        assert!(rendered.attachments.is_empty());
        assert_eq!(rendered.attachment_errors.len(), 1);
        assert!(!rendered.text.contains("<attachment>"));

        let skeleton = message_skeleton(7, &message, &context, &roots);
        assert_eq!(skeleton.preview, "done\nmissing.txt");
        assert_eq!(skeleton.attachment_count, 0);
        assert!(skeleton.has_attachment_errors);

        let _ = fs::remove_dir_all(workdir);
    }

    #[test]
    fn web_message_rendering_includes_structured_file_items() {
        let workdir = test_workdir("structured-file");
        let state = test_state("web-main-test-structured-file");
        let conversation_root = workdir.join("conversations").join(&state.conversation_id);
        fs::create_dir_all(&conversation_root).expect("create conversation root");
        let file_path = conversation_root.join("image.png");
        write_test_image(&file_path);
        let message = ChatMessage::new(
            ChatRole::Assistant,
            vec![
                ChatMessageItem::Context(ContextItem {
                    text: "here".to_string(),
                }),
                ChatMessageItem::File(FileItem {
                    uri: format!("file://{}", file_path.display()),
                    name: Some("image.png".to_string()),
                    media_type: Some("image/png".to_string()),
                    width: Some(640),
                    height: Some(480),
                    state: None,
                }),
            ],
        );

        let context = test_attachment_context(&workdir, &state);
        let roots = context.roots();
        let rendered = render_web_message(&message, &context, &roots);

        assert_eq!(rendered.text, "here");
        assert!(rendered.attachment_errors.is_empty());
        assert_eq!(rendered.attachments.len(), 1);
        assert_eq!(rendered.attachments[0].source, "message_file");
        assert_eq!(rendered.attachments[0].kind, "image");
        assert_eq!(rendered.attachments[0].path, "image.png");
        assert_eq!(
            rendered.attachments[0].uri,
            format!("file://{}", file_path.display())
        );
        assert_eq!(
            rendered.attachments[0].media_type.as_deref(),
            Some("image/png")
        );
        assert_eq!(rendered.attachments[0].width, Some(640));
        assert_eq!(rendered.attachments[0].height, Some(480));
        assert!(rendered.attachments[0].size_bytes.unwrap_or_default() > 0);
        assert!(rendered.attachments[0].thumbnail.is_some());
        assert_eq!(
            rendered.attachments[0].url,
            "/api/conversations/web-main-test-structured-file/workspace/file?path=image.png"
        );

        let skeleton = message_skeleton(7, &message, &context, &roots);
        assert_eq!(skeleton.preview, "here");
        assert_eq!(skeleton.attachment_count, 1);

        let _ = fs::remove_dir_all(workdir);
    }

    #[test]
    fn web_message_rendering_includes_tool_result_files() {
        let workdir = test_workdir("tool-result-file");
        let state = test_state("web-main-test-tool-result-file");
        let conversation_root = workdir.join("conversations").join(&state.conversation_id);
        fs::create_dir_all(&conversation_root).expect("create conversation root");
        let file_path = conversation_root.join("result.txt");
        fs::write(&file_path, "result bytes").expect("write attachment");
        let message = ChatMessage::new(
            ChatRole::Assistant,
            vec![ChatMessageItem::ToolResult(
                stellaclaw_core::session_actor::ToolResultItem {
                    tool_call_id: "call_1".to_string(),
                    tool_name: "file_download_wait".to_string(),
                    result: stellaclaw_core::session_actor::ToolResultContent {
                        context: Some(ContextItem {
                            text: "downloaded".to_string(),
                        }),
                        file: Some(FileItem {
                            uri: format!("file://{}", file_path.display()),
                            name: Some("result.txt".to_string()),
                            media_type: Some("text/plain".to_string()),
                            width: None,
                            height: None,
                            state: None,
                        }),
                    },
                },
            )],
        );

        let context = test_attachment_context(&workdir, &state);
        let roots = context.roots();
        let rendered = render_web_message(&message, &context, &roots);

        assert_eq!(rendered.text, "");
        assert_eq!(rendered.text_with_attachment_markers, "");
        assert_eq!(rendered.items.len(), 1);
        assert!(matches!(
            &rendered.items[0],
            WebMessageItem::ToolResult {
                tool_call_id,
                tool_name,
                context: Some(context),
                context_with_attachment_markers: Some(context_with_attachment_markers),
                file_attachment_index: Some(0),
                ..
            } if tool_call_id == "call_1"
                && tool_name == "file_download_wait"
                && context == "downloaded"
                && context_with_attachment_markers == "downloaded"
        ));
        assert!(rendered.attachment_errors.is_empty());
        assert_eq!(rendered.attachments.len(), 1);
        assert_eq!(rendered.attachments[0].source, "tool_result_file");
        assert_eq!(rendered.attachments[0].kind, "document");
        assert_eq!(rendered.attachments[0].path, "result.txt");
        assert_eq!(rendered.attachments[0].size_bytes, Some(12));

        let skeleton = message_skeleton(7, &message, &context, &roots);
        assert_eq!(skeleton.preview, "");
        assert_eq!(skeleton.text, "");
        assert_eq!(skeleton.text_with_attachment_markers, "");
        assert_eq!(skeleton.items.len(), 1);
        assert_eq!(skeleton.attachment_count, 1);

        let _ = fs::remove_dir_all(workdir);
    }

    fn test_attachment_context(workdir: &Path, state: &ConversationState) -> WebAttachmentContext {
        WebAttachmentContext::new(workdir, state, Arc::new(CacheManager::new(workdir)))
    }

    fn write_test_image(path: &Path) {
        let image = image::RgbImage::from_pixel(800, 600, image::Rgb([80, 120, 200]));
        image.save(path).expect("write test image");
    }
}
