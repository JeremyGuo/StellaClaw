use std::{
    cmp::Ordering,
    collections::HashMap,
    fs,
    io::{BufRead, BufReader, ErrorKind, Read, Seek, SeekFrom, Write},
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
    session_actor::{
        ChatMessage, ChatMessageItem, ChatRole, ContextItem, FileItem, SelectionReferenceItem,
        TokenUsage, ToolRemoteMode,
    },
};
use time::{format_description::well_known::Rfc3339, OffsetDateTime};

use crate::{
    cache::{CacheManager, CachedThumbnail},
    config::{ModelSelection, SandboxMode, SessionProfile, StellaclawConfig},
    conversation_host::ConversationHostRuntime,
    conversation_id_manager::ConversationIdManager,
    conversation_metadata::{ConversationMetadata, ConversationMetadataStore, WorkdirLayout},
    conversation_new::ConversationRuntimeConfig,
    logger::StellaclawLogger,
    service_protos::{
        channel::{ChannelEvent as ServiceChannelEvent, ChannelIngress},
        kernel::{KernelMetadataPatch, KernelResponse, KernelRuntimeConfigPatch},
        status::{StatusRequest, StatusResponse},
        terminal::{
            TerminalDataEncoding, TerminalReplaySnapshot, TerminalRequest, TerminalResponse,
        },
        workspace::{WorkspaceFileEncoding, WorkspaceRequest, WorkspaceResponse, WorkspaceTarget},
    },
    workspace::is_sshfs_workspace_entry_name,
};

use super::{
    types::{
        parse_reasoning_control_argument, ConversationControl, IncomingConversationMessage,
        IncomingDispatch, IncomingMessageDispatch, OutgoingAttachment, OutgoingAttachmentKind,
        OutgoingDelivery, OutgoingError, OutgoingMessageAppended, OutgoingProgressFeedback,
        OutgoingStatus, ProcessingState, ProgressFeedbackFinalState,
    },
    web_terminal::{TerminalCreateRequest, TerminalResizeRequest},
    Channel,
};

const MAX_HEADER_BYTES: usize = 64 * 1024;
const MAX_BODY_BYTES: usize = 11 * 1024 * 1024;
const DEFAULT_WORKSPACE_FILE_LIMIT_BYTES: usize = 1024 * 1024;
const MAX_WORKSPACE_FILE_PREVIEW_BYTES: usize = 25 * 1024 * 1024;
const WEBSOCKET_GUID: &str = "258EAFA5-E914-47DA-95CA-C5AB0DC85B11";
const WEBSOCKET_MAX_FRAME_BYTES: usize = 64 * 1024;
const WEBSOCKET_POLL_INTERVAL: Duration = Duration::from_millis(250);
const WEBSOCKET_HEARTBEAT_INTERVAL: Duration = Duration::from_secs(30);
const TERMINAL_WEBSOCKET_WRITE_TIMEOUT: Duration = Duration::from_secs(10);
const TERMINAL_WEBSOCKET_EVENT_CAPACITY: usize = 256;
const WORKSPACE_REQUEST_TIMEOUT: Duration = Duration::from_secs(70);
const WEB_CHANNEL_STATE_FILE: &str = "web_state.json";

pub struct WebChannel {
    id: String,
    bind_addr: String,
    token: String,
    workdir: PathBuf,
    config: Arc<StellaclawConfig>,
    conversation_runtime: Arc<ConversationHostRuntime>,
    logger: Arc<StellaclawLogger>,
    cache_manager: Arc<CacheManager>,
    websocket_subscribers: Arc<Mutex<HashMap<String, Vec<WebSocketSubscriber>>>>,
    conversation_stream_subscribers: Arc<Mutex<Vec<Sender<Value>>>>,
    processing_states: Arc<Mutex<HashMap<String, ProcessingState>>>,
    active_turn_progress: Arc<Mutex<HashMap<String, Value>>>,
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
        conversation_runtime: Arc<ConversationHostRuntime>,
        logger: Arc<StellaclawLogger>,
    ) -> Self {
        let cache_manager = Arc::new(CacheManager::new(workdir.clone()));
        let _ = cache_manager.ensure_layout();
        let channel_state_dir = web_channel_state_dir(&workdir, &id);
        if let Err(error) = fs::create_dir_all(&channel_state_dir) {
            logger.warn(
                "web_channel_state_dir_create_failed",
                json!({"channel_id": &id, "path": channel_state_dir.display().to_string(), "error": error.to_string()}),
            );
        }
        let seen_states = load_web_channel_state(&workdir, &id, &logger).seen;
        Self {
            id,
            bind_addr,
            token,
            workdir: workdir.clone(),
            config,
            conversation_runtime,
            logger,
            cache_manager,
            websocket_subscribers: Arc::new(Mutex::new(HashMap::new())),
            conversation_stream_subscribers: Arc::new(Mutex::new(Vec::new())),
            processing_states: Arc::new(Mutex::new(HashMap::new())),
            active_turn_progress: Arc::new(Mutex::new(HashMap::new())),
            seen_states: Arc::new(Mutex::new(seen_states)),
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
            ("POST", ["conversations"]) => self.create_conversation(&request.body, id_manager),
            ("PATCH", ["conversations", conversation_id]) => {
                self.update_conversation(conversation_id, &request.body)
            }
            ("DELETE", ["conversations", conversation_id]) => {
                self.delete_conversation(conversation_id, dispatch_tx, id_manager)
            }
            ("POST", ["conversations", conversation_id, "seen"]) => {
                self.mark_conversation_seen(conversation_id, &request.body)
            }
            ("GET", ["conversations", conversation_id, "messages"]) => {
                self.list_messages(conversation_id, &request.query)
            }
            ("GET", ["conversations", conversation_id, "messages", message_id]) => {
                self.message_detail(conversation_id, message_id)
            }
            ("POST", ["conversations", conversation_id, "messages"]) => {
                self.enqueue_message(conversation_id, &request.body, dispatch_tx)
            }
            (
                "POST",
                ["conversations", conversation_id, "foreground_sessions", session_id, "messages"],
            ) => {
                self.enqueue_foreground_session_message(conversation_id, session_id, &request.body)
            }
            ("GET", ["conversations", conversation_id, "status"]) => {
                self.conversation_status(conversation_id)
            }
            ("GET", ["conversations", conversation_id, "workspace"]) => {
                self.conversation_workspace(conversation_id, &request.query)
            }
            ("DELETE", ["conversations", conversation_id, "workspace"]) => {
                self.conversation_workspace_delete(conversation_id, &request.query)
            }
            ("PATCH", ["conversations", conversation_id, "workspace"]) => {
                self.conversation_workspace_move(conversation_id, &request.body)
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
        let store = ConversationMetadataStore::new(&self.workdir);
        for path in store.list_metadata_paths().map_err(ApiError::internal)? {
            if path
                .parent()
                .and_then(|path| path.file_name())
                .and_then(|name| name.to_str())
                .is_some_and(is_sshfs_workspace_entry_name)
            {
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
            let metadata: ConversationMetadata = match serde_json::from_str(&raw) {
                Ok(metadata) => metadata,
                Err(error) => {
                    self.logger.warn(
                        "web_conversation_list_parse_failed",
                        json!({"path": path.display().to_string(), "error": error.to_string()}),
                    );
                    continue;
                }
            };
            if metadata.channel_id == self.id {
                let processing_state = self
                    .processing_states
                    .lock()
                    .ok()
                    .and_then(|states| states.get(&metadata.platform_chat_id).copied())
                    .unwrap_or(ProcessingState::Idle);
                let message_summary = conversation_message_summary(&self.workdir, &metadata);
                conversations.push(ConversationSummary::from_metadata(
                    &self.workdir,
                    &metadata,
                    &self.config,
                    load_conversation_runtime_config(&self.workdir, &metadata.conversation_id)
                        .ok()
                        .as_ref(),
                    processing_state,
                    message_summary,
                    self.conversation_seen(&metadata.conversation_id),
                ));
            }
        }
        conversations.sort_by(|left, right| left.conversation_id.cmp(&right.conversation_id));
        Ok(conversations)
    }

    fn mark_conversation_seen(
        &self,
        conversation_id: &str,
        body: &[u8],
    ) -> ApiResult<HttpResponse> {
        let state = self.load_web_state(conversation_id)?;
        let log_path = message_log_path(&self.workdir, &state);
        let request: MarkConversationSeenRequest = parse_json(body)?;
        let (seen, snapshot) = {
            let mut seen_states = self
                .seen_states
                .lock()
                .map_err(|_| ApiError::new(500, "conversation seen lock poisoned"))?;
            let existing = seen_states.get(conversation_id).cloned();
            let seen = match existing {
                Some(existing)
                    if compare_message_ids(
                        &log_path,
                        &existing.last_seen_message_id,
                        &request.last_seen_message_id,
                    )
                    .is_some_and(|ordering| !ordering.is_lt()) =>
                {
                    existing
                }
                _ => ConversationSeen {
                    last_seen_message_id: request.last_seen_message_id,
                    updated_at: now_rfc3339(),
                },
            };
            seen_states.insert(conversation_id.to_string(), seen.clone());
            (seen, seen_states.clone())
        };
        self.persist_seen_states(&snapshot)?;
        self.publish_conversation_stream_event(json!({
            "type": "conversation_seen",
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

    fn persist_seen_states(&self, seen: &HashMap<String, ConversationSeen>) -> ApiResult<()> {
        let state = WebChannelState { seen: seen.clone() };
        let path = self.web_channel_state_path();
        let parent = path
            .parent()
            .ok_or_else(|| ApiError::new(500, "invalid web channel state path"))?;
        fs::create_dir_all(parent).map_err(ApiError::internal)?;
        let raw = serde_json::to_string_pretty(&state).map_err(ApiError::internal)?;
        fs::write(&path, raw).map_err(ApiError::internal)
    }

    fn web_channel_state_path(&self) -> PathBuf {
        web_channel_state_dir(&self.workdir, &self.id).join(WEB_CHANNEL_STATE_FILE)
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
        let store = ConversationMetadataStore::new(&self.workdir);
        let mut metadata = store
            .load_or_create(&conversation_id, &self.id, &platform_chat_id)
            .map_err(ApiError::internal)?;

        let mut runtime_patch = None;
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
            runtime_patch = Some(KernelRuntimeConfigPatch {
                session_profile: Some(Some(crate::config::SessionProfile {
                    main_model: ModelSelection::alias(model),
                })),
                ..Default::default()
            });
            metadata.model_selection_pending = false;
        }
        if let Some(nickname) = request.nickname {
            metadata.nickname =
                normalize_conversation_nickname(&metadata.conversation_id, &nickname);
        }
        store.persist(&metadata).map_err(ApiError::internal)?;
        self.conversation_runtime
            .ensure_conversation_started(&metadata.conversation_id)
            .map_err(ApiError::internal)?;
        if let Some(patch) = runtime_patch {
            self.conversation_runtime
                .send_main_channel_ingress(
                    &metadata.conversation_id,
                    ChannelIngress::UpdateRuntimeConfig { patch },
                )
                .map_err(ApiError::internal)?;
        }
        self.publish_conversation_upserted(&metadata);

        Ok(json_response(
            201,
            json!({
                "conversation_id": conversation_id,
                "nickname": metadata.nickname,
                "channel_id": self.id,
                "platform_chat_id": platform_chat_id,
                "model_selection_pending": metadata.model_selection_pending,
            }),
        ))
    }

    fn update_conversation(&self, conversation_id: &str, body: &[u8]) -> ApiResult<HttpResponse> {
        let request: UpdateConversationRequest = parse_json(body)?;
        self.load_web_state(conversation_id)?;
        let patch = KernelMetadataPatch {
            conversation_nickname: request.nickname,
            ..Default::default()
        };
        let metadata = match self.kernel_metadata_request(
            conversation_id,
            ChannelIngress::UpdateKernelMetadata {
                request_id: String::new(),
                patch,
            },
        )? {
            KernelResponse::MetadataUpdated { metadata } => metadata,
            KernelResponse::Metadata { metadata } => metadata,
            KernelResponse::Error { message, .. } => return Err(ApiError::new(400, message)),
            _ => return Err(ApiError::internal("unexpected kernel metadata response")),
        };
        let processing_state = self
            .processing_states
            .lock()
            .ok()
            .and_then(|states| states.get(&metadata.platform_chat_id).copied())
            .unwrap_or(ProcessingState::Idle);
        let summary = ConversationSummary::from_metadata(
            &self.workdir,
            &metadata,
            &self.config,
            load_conversation_runtime_config(&self.workdir, &metadata.conversation_id)
                .ok()
                .as_ref(),
            processing_state,
            conversation_message_summary(&self.workdir, &metadata),
            self.conversation_seen(&metadata.conversation_id),
        );
        self.publish_conversation_upserted(&metadata);
        Ok(json_response(
            200,
            json!({
                "conversation": summary,
            }),
        ))
    }

    fn delete_conversation(
        &self,
        conversation_id: &str,
        dispatch_tx: Sender<IncomingDispatch>,
        id_manager: Arc<Mutex<ConversationIdManager>>,
    ) -> ApiResult<HttpResponse> {
        let metadata = self.load_web_state(conversation_id)?;
        let (response_tx, response_rx) = bounded(1);
        dispatch_tx
            .send(IncomingDispatch::DeleteConversation {
                channel_id: self.id.clone(),
                platform_chat_id: metadata.platform_chat_id.clone(),
                conversation_id: conversation_id.to_string(),
                response_tx,
            })
            .map_err(|_| ApiError::new(503, "dispatcher is not available"))?;
        response_rx
            .recv_timeout(Duration::from_secs(6))
            .map_err(|_| ApiError::new(503, "conversation shutdown timed out"))?
            .map_err(|error| ApiError::new(500, error))?;

        ConversationMetadataStore::new(&self.workdir)
            .remove(conversation_id)
            .map_err(ApiError::internal)?;

        let removed_seen = {
            let mut seen_states = self
                .seen_states
                .lock()
                .map_err(|_| ApiError::new(500, "conversation seen lock poisoned"))?;
            let removed = seen_states.remove(conversation_id).is_some();
            let snapshot = removed.then(|| seen_states.clone());
            (removed, snapshot)
        };
        if let (true, Some(snapshot)) = removed_seen {
            self.persist_seen_states(&snapshot)?;
        }
        id_manager
            .lock()
            .map_err(|_| ApiError::new(500, "conversation id manager lock poisoned"))?
            .remove_mapping(&self.id, &metadata.platform_chat_id)
            .map_err(|error| ApiError::new(500, error))?;
        if let Ok(mut subscribers) = self.websocket_subscribers.lock() {
            subscribers.remove(&metadata.platform_chat_id);
        }
        if let Ok(mut progress) = self.active_turn_progress.lock() {
            progress.remove(&metadata.platform_chat_id);
        }
        self.publish_conversation_stream_event(json!({
            "type": "conversation_deleted",
            "channel_id": self.id,
            "conversation_id": conversation_id,
            "platform_chat_id": &metadata.platform_chat_id,
        }));
        self.logger.info(
            "web_conversation_deleted",
            json!({
                "channel_id": self.id,
                "conversation_id": conversation_id,
                "platform_chat_id": &metadata.platform_chat_id,
            }),
        );
        Ok(json_response(
            200,
            json!({
                "conversation_id": conversation_id,
                "deleted": true,
            }),
        ))
    }

    fn enqueue_message(
        &self,
        conversation_id: &str,
        body: &[u8],
        dispatch_tx: Sender<IncomingDispatch>,
    ) -> ApiResult<HttpResponse> {
        let metadata = self.load_web_state(conversation_id)?;
        let request: SendMessageRequest = parse_json(body)?;
        let text = request.text.unwrap_or_default();
        let remote_message_id = request
            .remote_message_id
            .unwrap_or_else(generated_message_id);
        let files = materialize_web_file_items(
            &self.workdir,
            conversation_id,
            &remote_message_id,
            request.files.unwrap_or_default(),
        )
        .map_err(ApiError::internal)?;
        let selection_references =
            sanitize_selection_references(request.selection_references.unwrap_or_default());
        if text.trim().is_empty() && files.is_empty() && selection_references.is_empty() {
            return Err(ApiError::new(
                400,
                "message requires text, files, or selection references",
            ));
        }
        let control = text
            .trim()
            .starts_with('/')
            .then(|| parse_web_control(text.trim()))
            .flatten();
        let incoming = IncomingDispatch::Message(IncomingMessageDispatch {
            channel_id: self.id.clone(),
            platform_chat_id: metadata.platform_chat_id.clone(),
            conversation_id: conversation_id.to_string(),
            message: IncomingConversationMessage {
                remote_message_id: remote_message_id.clone(),
                user_name: request.user_name,
                message_time: Some(
                    normalized_client_message_time(request.message_time)
                        .unwrap_or_else(now_rfc3339),
                ),
                text: (!text.is_empty()).then_some(text),
                selection_references,
                files,
                control,
            },
        });
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

    fn enqueue_foreground_session_message(
        &self,
        conversation_id: &str,
        session_id: &str,
        body: &[u8],
    ) -> ApiResult<HttpResponse> {
        if session_id != "main" {
            return Err(ApiError::new(
                404,
                "foreground session is not mounted on this web channel",
            ));
        }
        let request: SendMessageRequest = parse_json(body)?;
        let remote_message_id = request
            .remote_message_id
            .clone()
            .unwrap_or_else(generated_message_id);
        if let Some(control) = request
            .text
            .as_deref()
            .map(str::trim)
            .filter(|text| text.starts_with('/'))
            .and_then(parse_web_control)
        {
            return self.enqueue_foreground_session_control(
                conversation_id,
                session_id,
                remote_message_id,
                control,
            );
        }
        let message = web_send_message_to_chat_message(request)?;
        self.conversation_runtime
            .ensure_conversation_started(conversation_id)
            .map_err(ApiError::internal)?;
        self.conversation_runtime
            .send_main_channel_ingress(
                conversation_id,
                crate::service_protos::channel::ChannelIngress::IncomingMessage {
                    platform_message_id: Some(remote_message_id.clone()),
                    origin: None,
                    message,
                    metadata: json!({
                        "channel_id": self.id,
                        "web_session_id": session_id,
                    }),
                },
            )
            .map_err(ApiError::internal)?;
        Ok(json_response(
            202,
            json!({
                "conversation_id": conversation_id,
                "foreground_session_id": session_id,
                "remote_message_id": remote_message_id,
                "accepted": true,
            }),
        ))
    }

    fn enqueue_foreground_session_control(
        &self,
        conversation_id: &str,
        session_id: &str,
        remote_message_id: String,
        control: ConversationControl,
    ) -> ApiResult<HttpResponse> {
        let ingress = self.web_control_to_channel_ingress(&control)?;
        self.conversation_runtime
            .ensure_conversation_started(conversation_id)
            .map_err(ApiError::internal)?;
        self.conversation_runtime
            .send_main_channel_ingress(conversation_id, ingress)
            .map_err(ApiError::internal)?;
        if matches!(control, ConversationControl::SwitchModel { .. }) {
            let metadata = match self.kernel_metadata_request(
                conversation_id,
                ChannelIngress::UpdateKernelMetadata {
                    request_id: String::new(),
                    patch: KernelMetadataPatch {
                        model_selection_pending: Some(false),
                        ..Default::default()
                    },
                },
            )? {
                KernelResponse::MetadataUpdated { metadata } => metadata,
                KernelResponse::Metadata { metadata } => metadata,
                _ => return Err(ApiError::internal("unexpected kernel metadata response")),
            };
            self.publish_conversation_upserted(&metadata);
        }
        Ok(json_response(
            202,
            json!({
                "conversation_id": conversation_id,
                "foreground_session_id": session_id,
                "remote_message_id": remote_message_id,
                "accepted": true,
                "control": true,
            }),
        ))
    }

    fn web_control_to_channel_ingress(
        &self,
        control: &ConversationControl,
    ) -> ApiResult<ChannelIngress> {
        match control {
            ConversationControl::Continue => Ok(ChannelIngress::ContinueForegroundTurn {
                reason: Some("user requested continue".to_string()),
            }),
            ConversationControl::Cancel => Ok(ChannelIngress::CancelForegroundTurn {
                reason: Some("user requested cancel".to_string()),
            }),
            ConversationControl::Compact => Ok(ChannelIngress::CompactForegroundNow),
            ConversationControl::ShowStatus
            | ConversationControl::ShowModel
            | ConversationControl::ShowReasoning
            | ConversationControl::ShowRemote
            | ConversationControl::ShowSandbox => Ok(ChannelIngress::QueryForegroundStatus),
            ConversationControl::SwitchModel { model_name } => {
                let Some(model_config) = self.config.models.get(model_name) else {
                    return Err(ApiError::new(
                        400,
                        format!("unknown model alias {model_name}"),
                    ));
                };
                if !self.config.is_available_agent_model(model_name) {
                    return Err(ApiError::new(
                        400,
                        format!("model {model_name} is not available for agent selection"),
                    ));
                }
                if !model_config.supports(ModelCapability::Chat) {
                    return Err(ApiError::new(
                        400,
                        format!("model {model_name} is not chat-capable"),
                    ));
                }
                Ok(ChannelIngress::UpdateRuntimeConfig {
                    patch: KernelRuntimeConfigPatch {
                        session_profile: Some(Some(SessionProfile {
                            main_model: ModelSelection::alias(model_name.clone()),
                        })),
                        ..Default::default()
                    },
                })
            }
            ConversationControl::SetReasoning { effort } => {
                Ok(ChannelIngress::UpdateRuntimeConfig {
                    patch: KernelRuntimeConfigPatch {
                        reasoning_effort: Some(effort.clone()),
                        ..Default::default()
                    },
                })
            }
            ConversationControl::SetRemote { host, path } => {
                Ok(ChannelIngress::UpdateRuntimeConfig {
                    patch: KernelRuntimeConfigPatch {
                        tool_remote_mode: Some(ToolRemoteMode::FixedSsh {
                            host: host.clone(),
                            cwd: Some(path.clone()),
                        }),
                        ..Default::default()
                    },
                })
            }
            ConversationControl::DisableRemote => Ok(ChannelIngress::UpdateRuntimeConfig {
                patch: KernelRuntimeConfigPatch {
                    tool_remote_mode: Some(ToolRemoteMode::Selectable),
                    ..Default::default()
                },
            }),
            ConversationControl::SetSandbox { .. } => Err(ApiError::new(
                400,
                "sandbox runtime switching is not exposed through the new channel protocol yet",
            )),
            ConversationControl::InvalidReasoning { reason }
            | ConversationControl::InvalidRemote { reason }
            | ConversationControl::InvalidSandbox { reason } => {
                Err(ApiError::new(400, reason.clone()))
            }
        }
    }

    fn list_messages(
        &self,
        conversation_id: &str,
        query: &HashMap<String, String>,
    ) -> ApiResult<HttpResponse> {
        let offset = query_usize(query, "offset", 0);
        let limit = query_usize(query, "limit", 50).min(200);
        let state = self.load_web_state(conversation_id)?;
        let page = read_message_page(&message_log_path(&self.workdir, &state), offset, limit)
            .map_err(ApiError::internal)?;
        let attachments =
            WebAttachmentContext::new(&self.workdir, &state, self.cache_manager.clone());
        Ok(json_response(
            200,
            message_page_payload(&state, &attachments, &page, offset, limit),
        ))
    }

    fn message_detail(&self, conversation_id: &str, message_id: &str) -> ApiResult<HttpResponse> {
        let state = self.load_web_state(conversation_id)?;
        let Some((index, message)) =
            read_message_by_id(&message_log_path(&self.workdir, &state), message_id)
                .map_err(ApiError::internal)?
        else {
            return Err(ApiError::new(404, "message_not_found"));
        };
        let attachments =
            WebAttachmentContext::new(&self.workdir, &state, self.cache_manager.clone());
        let roots = attachments.roots();
        let rendered = render_web_message(&message, &attachments, &roots);
        Ok(json_response(
            200,
            json!({
                "conversation_id": conversation_id,
                "id": message.message_id.clone(),
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
            ["conversations", "stream"] => {
                write_websocket_handshake(&mut stream, key)?;
                self.run_conversation_stream(stream)
            }
            ["conversations", conversation_id, "foreground", "ws"] => {
                let state = match self.load_web_state(conversation_id) {
                    Ok(state) => state,
                    Err(error) => {
                        write_http_response(&mut stream, json_error(error.status, &error.message))?;
                        return Ok(());
                    }
                };
                write_websocket_handshake(&mut stream, key)?;
                self.run_foreground_websocket_stream(stream, state)
            }
            ["conversations", conversation_id, "terminals", terminal_id, "stream"] => {
                if let Err(error) = self.load_web_state(conversation_id) {
                    write_http_response(&mut stream, json_error(error.status, &error.message))?;
                    return Ok(());
                }
                let terminal = match self.terminal_request(
                    conversation_id,
                    TerminalRequest::Get {
                        terminal_id: terminal_id.to_string(),
                    },
                ) {
                    Ok(TerminalResponse::Terminal { terminal }) => terminal,
                    Ok(_) => {
                        write_http_response(
                            &mut stream,
                            json_error(500, "unexpected_terminal_response"),
                        )?;
                        return Ok(());
                    }
                    Err(error) => {
                        write_http_response(&mut stream, json_error(error.status, &error.message))?;
                        return Ok(());
                    }
                };
                let offset = request
                    .query
                    .get("offset")
                    .and_then(|value| value.parse::<u64>().ok())
                    .unwrap_or(terminal.next_offset);
                let event_rx = match self.subscribe_conversation_events(conversation_id) {
                    Ok(rx) => rx,
                    Err(error) => {
                        write_http_response(&mut stream, json_error(error.status, &error.message))?;
                        return Ok(());
                    }
                };
                let attach_id = web_request_id("terminal-attach");
                if let Err(error) = self.send_terminal_request(
                    conversation_id,
                    attach_id.clone(),
                    TerminalRequest::Attach {
                        terminal_id: terminal_id.to_string(),
                        offset,
                    },
                ) {
                    write_http_response(&mut stream, json_error(error.status, &error.message))?;
                    return Ok(());
                }
                let attach = match wait_terminal_response(&event_rx, &attach_id) {
                    Ok(TerminalResponse::Attached {
                        replay,
                        subscriber_id,
                    }) => (replay, subscriber_id),
                    Ok(TerminalResponse::Error { message, .. }) => {
                        write_http_response(&mut stream, json_error(400, &message))?;
                        return Ok(());
                    }
                    Ok(_) => {
                        write_http_response(
                            &mut stream,
                            json_error(500, "unexpected_terminal_response"),
                        )?;
                        return Ok(());
                    }
                    Err(error) => {
                        write_http_response(&mut stream, json_error(error.status, &error.message))?;
                        return Ok(());
                    }
                };
                write_websocket_handshake(&mut stream, key)?;
                self.run_terminal_websocket_stream(
                    stream,
                    conversation_id.to_string(),
                    terminal_id.to_string(),
                    event_rx,
                    attach.0,
                    attach.1,
                )
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

    fn publish_conversation_upserted(&self, metadata: &ConversationMetadata) {
        let processing_state = self
            .processing_states
            .lock()
            .ok()
            .and_then(|states| states.get(&metadata.platform_chat_id).copied())
            .unwrap_or(ProcessingState::Idle);
        let summary = ConversationSummary::from_metadata(
            &self.workdir,
            metadata,
            &self.config,
            load_conversation_runtime_config(&self.workdir, &metadata.conversation_id)
                .ok()
                .as_ref(),
            processing_state,
            conversation_message_summary(&self.workdir, metadata),
            self.conversation_seen(&metadata.conversation_id),
        );
        self.publish_conversation_stream_event(json!({
            "type": "conversation_upserted",
            "channel_id": self.id,
            "conversation_id": &metadata.conversation_id,
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
            .and_then(|seen| {
                compare_message_ids(
                    &message_log_path(&self.workdir, &state),
                    &last_message_id,
                    &seen.last_seen_message_id,
                )
            })
            .map(|ordering| ordering.is_gt())
            .unwrap_or(true);
        let summary = ConversationSummary::from_metadata(
            &self.workdir,
            &state,
            &self.config,
            load_conversation_runtime_config(&self.workdir, &state.conversation_id)
                .ok()
                .as_ref(),
            ProcessingState::Idle,
            message_summary.clone(),
            seen.clone(),
        );
        self.publish_conversation_stream_event(json!({
            "type": "conversation_turn_completed",
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

    fn run_foreground_websocket_stream(
        &self,
        mut stream: TcpStream,
        mut state: ConversationMetadata,
    ) -> Result<()> {
        let conversation_id = state.conversation_id.clone();
        let mut session_id = state.foreground_session_id.clone();
        let mut message_summary = conversation_message_summary(&self.workdir, &state);
        let (event_tx, event_rx) = unbounded();
        self.register_websocket_subscriber(&state, event_tx);
        write_websocket_json(
            &mut stream,
            &websocket_subscription_ack(
                &state,
                &message_summary,
                "subscribed",
                self.active_turn_progress_for_state(&state),
            ),
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
            state = self.load_web_state(&conversation_id).map_err(api_anyhow)?;
            if state.foreground_session_id != session_id {
                session_id = state.foreground_session_id.clone();
                message_summary = conversation_message_summary(&self.workdir, &state);
                write_websocket_json(
                    &mut stream,
                    &websocket_subscription_ack(
                        &state,
                        &message_summary,
                        "session_changed",
                        self.active_turn_progress_for_state(&state),
                    ),
                )?;
                continue;
            }
        }
    }

    fn register_websocket_subscriber(&self, state: &ConversationMetadata, sender: Sender<Value>) {
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

    fn active_turn_progress_for_state(&self, metadata: &ConversationMetadata) -> Option<Value> {
        let mut value = self
            .active_turn_progress
            .lock()
            .ok()
            .and_then(|progress| progress.get(&metadata.platform_chat_id).cloned())?;
        if let Value::Object(map) = &mut value {
            map.entry("conversation_id".to_string())
                .or_insert_with(|| Value::String(metadata.conversation_id.clone()));
            map.entry("platform_chat_id".to_string())
                .or_insert_with(|| Value::String(metadata.platform_chat_id.clone()));
        }
        Some(value)
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
    ) -> ApiResult<Option<ConversationMetadata>> {
        let store = ConversationMetadataStore::new(&self.workdir);
        for path in store.list_metadata_paths().map_err(ApiError::internal)? {
            if path
                .parent()
                .and_then(|path| path.file_name())
                .and_then(|name| name.to_str())
                .is_some_and(is_sshfs_workspace_entry_name)
            {
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
            let metadata: ConversationMetadata = match serde_json::from_str(&raw) {
                Ok(metadata) => metadata,
                Err(error) => {
                    self.logger.warn(
                        "web_conversation_state_parse_failed",
                        json!({"path": path.display().to_string(), "error": error.to_string()}),
                    );
                    continue;
                }
            };
            if metadata.channel_id == self.id && metadata.platform_chat_id == platform_chat_id {
                return Ok(Some(metadata));
            }
        }
        Ok(None)
    }

    fn run_terminal_websocket_stream(
        &self,
        mut stream: TcpStream,
        conversation_id: String,
        terminal_id: String,
        event_rx: crossbeam_channel::Receiver<ServiceChannelEvent>,
        replay: TerminalReplaySnapshot,
        subscriber_id: Option<u64>,
    ) -> Result<()> {
        let _ = stream.set_write_timeout(Some(TERMINAL_WEBSOCKET_WRITE_TIMEOUT));
        write_terminal_replay(&mut stream, &replay)?;
        if !replay.running {
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
                recv(event_rx) -> message => {
                    match message {
                        Ok(ServiceChannelEvent::Terminal {
                            response: TerminalResponse::Output {
                                terminal_id: output_terminal_id,
                                subscriber_id: output_subscriber_id,
                                encoding,
                                data,
                            },
                            ..
                        }) if output_terminal_id == terminal_id && output_subscriber_id == subscriber_id => {
                            match decode_terminal_data(encoding, &data) {
                                Ok(bytes) => {
                                    if let Err(error) = write_websocket_frame(&mut stream, 0x2, &bytes) {
                                        break Err(error);
                                    }
                                }
                                Err(error) => {
                                    let _ = write_websocket_json(
                                        &mut stream,
                                        &json!({
                                            "type": "error",
                                            "error": "invalid_terminal_output",
                                            "message": format!("{error:#}"),
                                        }),
                                    );
                                }
                            }
                        }
                        Ok(_) => {}
                        Err(_) => {
                            let _ = write_websocket_close(&mut stream);
                            break Ok(());
                        }
                    }
                }
                recv(client_rx) -> message => {
                    match message {
                        Ok(TerminalWebSocketEvent::Input(bytes)) => {
                            if let Err(error) = self.send_terminal_request(
                                &conversation_id,
                                web_request_id("terminal-input"),
                                TerminalRequest::Input {
                                    terminal_id: terminal_id.clone(),
                                    encoding: TerminalDataEncoding::Base64,
                                    data: general_purpose::STANDARD.encode(bytes),
                                },
                            ) {
                                let _ = write_websocket_json(&mut stream, &json!({
                                    "type": "error",
                                    "error": "terminal_input_failed",
                                    "message": error.message,
                                }));
                                let _ = write_websocket_close(&mut stream);
                                break Ok(());
                            }
                        }
                        Ok(TerminalWebSocketEvent::Resize(request)) => {
                            if let Err(error) = self.send_terminal_request(
                                &conversation_id,
                                web_request_id("terminal-resize"),
                                TerminalRequest::Resize {
                                    terminal_id: terminal_id.clone(),
                                    request,
                                },
                            ) {
                                let _ = write_websocket_json(&mut stream, &json!({
                                    "type": "error",
                                    "error": "terminal_resize_failed",
                                    "message": error.message,
                                }));
                                let _ = write_websocket_close(&mut stream);
                                break Ok(());
                            }
                        }
                        Ok(TerminalWebSocketEvent::Attach(offset)) => {
                            let request_id = web_request_id("terminal-replay");
                            if let Err(error) = self.send_terminal_request(
                                &conversation_id,
                                request_id.clone(),
                                TerminalRequest::Replay {
                                    terminal_id: terminal_id.clone(),
                                    offset,
                                },
                            ) {
                                let _ = write_websocket_json(&mut stream, &json!({
                                    "type": "error",
                                    "error": "terminal_replay_failed",
                                    "message": error.message,
                                }));
                                let _ = write_websocket_close(&mut stream);
                                break Ok(());
                            }
                            match wait_terminal_response(&event_rx, &request_id) {
                                Ok(TerminalResponse::Replay { replay }) => {
                                    if let Err(error) = write_terminal_replay(&mut stream, &replay) {
                                        break Err(error);
                                    }
                                }
                                Ok(TerminalResponse::Error { message, .. }) => {
                                    let _ = write_websocket_json(&mut stream, &json!({
                                        "type": "error",
                                        "error": "terminal_replay_failed",
                                        "message": message,
                                    }));
                                    let _ = write_websocket_close(&mut stream);
                                    break Ok(());
                                }
                                Ok(_) => {}
                                Err(error) => {
                                    let _ = write_websocket_json(&mut stream, &json!({
                                        "type": "error",
                                        "error": "terminal_replay_failed",
                                        "message": error.message,
                                    }));
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
            let _ = self.send_terminal_request(
                &conversation_id,
                web_request_id("terminal-detach"),
                TerminalRequest::Detach {
                    terminal_id,
                    subscriber_id,
                },
            );
        }
        result
    }

    fn conversation_status(&self, conversation_id: &str) -> ApiResult<HttpResponse> {
        let status = self.status_request(conversation_id, StatusRequest::Snapshot)?;
        Ok(json_response(200, json!(status)))
    }

    fn conversation_workspace(
        &self,
        conversation_id: &str,
        query: &HashMap<String, String>,
    ) -> ApiResult<HttpResponse> {
        let path = query.get("path").map(String::as_str);
        let limit = query_usize(query, "limit", 200).min(1000);
        let listing = self.workspace_request(
            conversation_id,
            WorkspaceRequest::List {
                path: path.map(str::to_string),
                target: WorkspaceTarget::Auto,
                limit: Some(limit),
            },
        )?;
        Ok(json_response(200, json!(listing)))
    }

    fn conversation_workspace_file(
        &self,
        conversation_id: &str,
        query: &HashMap<String, String>,
    ) -> ApiResult<HttpResponse> {
        let path = query
            .get("path")
            .map(String::as_str)
            .ok_or_else(|| ApiError::new(400, "workspace file path is required"))?;
        let offset = query
            .get("offset")
            .and_then(|value| value.parse::<u64>().ok())
            .unwrap_or(0);
        let full_image = query_bool(query, "full") && is_image_workspace_path(path);
        let limit_bytes = if full_image {
            None
        } else {
            Some(
                query
                    .get("limit_bytes")
                    .and_then(|value| value.parse::<usize>().ok())
                    .unwrap_or(DEFAULT_WORKSPACE_FILE_LIMIT_BYTES)
                    .clamp(1, MAX_WORKSPACE_FILE_PREVIEW_BYTES),
            )
        };
        let file = self.workspace_request(
            conversation_id,
            WorkspaceRequest::ReadFile {
                path: path.to_string(),
                target: WorkspaceTarget::Auto,
                offset: Some(offset),
                limit_bytes,
            },
        )?;
        Ok(json_response(200, json!(file)))
    }

    fn conversation_workspace_delete(
        &self,
        conversation_id: &str,
        query: &HashMap<String, String>,
    ) -> ApiResult<HttpResponse> {
        let path = query
            .get("path")
            .map(String::as_str)
            .ok_or_else(|| ApiError::new(400, "workspace path is required"))?;
        let response = self.workspace_request(
            conversation_id,
            WorkspaceRequest::DeletePath {
                path: path.to_string(),
                target: WorkspaceTarget::Auto,
            },
        )?;
        Ok(json_response(200, json!(response)))
    }

    fn conversation_workspace_move(
        &self,
        conversation_id: &str,
        body: &[u8],
    ) -> ApiResult<HttpResponse> {
        let request: MoveWorkspacePathRequest = parse_json(body)?;
        let response = self.workspace_request(
            conversation_id,
            WorkspaceRequest::MovePath {
                from_path: request.path,
                to_path: request.new_path,
                target: WorkspaceTarget::Auto,
            },
        )?;
        Ok(json_response(200, json!(response)))
    }

    fn conversation_workspace_upload(
        &self,
        conversation_id: &str,
        query: &HashMap<String, String>,
        body: &[u8],
    ) -> ApiResult<HttpResponse> {
        let dir = query.get("path").map(String::as_str).unwrap_or("");
        let response = self.workspace_request(
            conversation_id,
            WorkspaceRequest::UploadArchive {
                dir_path: dir.to_string(),
                target: WorkspaceTarget::Auto,
                encoding: WorkspaceFileEncoding::Base64,
                data: general_purpose::STANDARD.encode(body),
            },
        )?;
        Ok(json_response(200, json!(response)))
    }

    fn conversation_workspace_download(
        &self,
        conversation_id: &str,
        query: &HashMap<String, String>,
    ) -> ApiResult<HttpResponse> {
        let path = query
            .get("path")
            .map(String::as_str)
            .ok_or_else(|| ApiError::new(400, "workspace path is required for download"))?;
        let response = self.workspace_request(
            conversation_id,
            WorkspaceRequest::DownloadArchive {
                paths: vec![path.to_string()],
                target: WorkspaceTarget::Auto,
            },
        )?;
        let WorkspaceResponse::ArchiveDownloaded { encoding, data, .. } = response else {
            return Err(ApiError::internal(
                "workspace service returned unexpected download response",
            ));
        };
        let archive = match encoding {
            WorkspaceFileEncoding::Base64 => general_purpose::STANDARD
                .decode(data)
                .map_err(ApiError::internal)?,
            WorkspaceFileEncoding::Utf8 => data.into_bytes(),
        };
        Ok(HttpResponse {
            status: 200,
            content_type: "application/gzip",
            body: archive,
        })
    }

    fn workspace_request(
        &self,
        conversation_id: &str,
        request: WorkspaceRequest,
    ) -> ApiResult<WorkspaceResponse> {
        self.load_web_state(conversation_id)?;
        self.conversation_runtime
            .ensure_conversation_started(conversation_id)
            .map_err(ApiError::internal)?;
        let event_rx = self
            .conversation_runtime
            .subscribe_main_channel_events(conversation_id)
            .map_err(ApiError::internal)?;
        let request_id = web_request_id("workspace");
        self.conversation_runtime
            .send_main_channel_ingress(
                conversation_id,
                ChannelIngress::Workspace {
                    request_id: request_id.clone(),
                    request,
                },
            )
            .map_err(ApiError::internal)?;
        loop {
            match event_rx.recv_timeout(WORKSPACE_REQUEST_TIMEOUT) {
                Ok(ServiceChannelEvent::Workspace {
                    request_id: event_request_id,
                    response,
                }) if event_request_id == request_id => {
                    return match response {
                        WorkspaceResponse::Error { message } => Err(ApiError::new(400, message)),
                        response => Ok(response),
                    };
                }
                Ok(_) => {}
                Err(RecvTimeoutError::Timeout) => {
                    return Err(ApiError::new(504, "workspace request timed out"));
                }
                Err(RecvTimeoutError::Disconnected) => {
                    return Err(ApiError::internal("workspace response stream disconnected"));
                }
            }
        }
    }

    fn status_request(
        &self,
        conversation_id: &str,
        request: StatusRequest,
    ) -> ApiResult<StatusResponse> {
        self.load_web_state(conversation_id)?;
        self.conversation_runtime
            .ensure_conversation_started(conversation_id)
            .map_err(ApiError::internal)?;
        let event_rx = self
            .conversation_runtime
            .subscribe_main_channel_events(conversation_id)
            .map_err(ApiError::internal)?;
        let request_id = web_request_id("status");
        self.conversation_runtime
            .send_main_channel_ingress(
                conversation_id,
                ChannelIngress::Status {
                    request_id: request_id.clone(),
                    request,
                },
            )
            .map_err(ApiError::internal)?;
        loop {
            match event_rx.recv_timeout(WORKSPACE_REQUEST_TIMEOUT) {
                Ok(ServiceChannelEvent::StatusSnapshot {
                    request_id: event_request_id,
                    response,
                }) if event_request_id == request_id => {
                    return match response {
                        StatusResponse::Error { message } => Err(ApiError::new(400, message)),
                        response => Ok(response),
                    };
                }
                Ok(_) => {}
                Err(RecvTimeoutError::Timeout) => {
                    return Err(ApiError::new(504, "status request timed out"));
                }
                Err(RecvTimeoutError::Disconnected) => {
                    return Err(ApiError::internal("status response stream disconnected"));
                }
            }
        }
    }

    fn kernel_metadata_request(
        &self,
        conversation_id: &str,
        ingress: ChannelIngress,
    ) -> ApiResult<KernelResponse> {
        self.load_web_state(conversation_id)?;
        self.conversation_runtime
            .ensure_conversation_started(conversation_id)
            .map_err(ApiError::internal)?;
        let event_rx = self
            .conversation_runtime
            .subscribe_main_channel_events(conversation_id)
            .map_err(ApiError::internal)?;
        let request_id = web_request_id("kernel-metadata");
        let ingress = match ingress {
            ChannelIngress::QueryKernelMetadata { .. } => ChannelIngress::QueryKernelMetadata {
                request_id: request_id.clone(),
            },
            ChannelIngress::UpdateKernelMetadata { patch, .. } => {
                ChannelIngress::UpdateKernelMetadata {
                    request_id: request_id.clone(),
                    patch,
                }
            }
            _ => return Err(ApiError::internal("invalid kernel metadata request")),
        };
        self.conversation_runtime
            .send_main_channel_ingress(conversation_id, ingress)
            .map_err(ApiError::internal)?;
        loop {
            match event_rx.recv_timeout(WORKSPACE_REQUEST_TIMEOUT) {
                Ok(ServiceChannelEvent::KernelMetadata {
                    request_id: event_request_id,
                    response,
                }) if event_request_id == request_id => return Ok(response),
                Ok(_) => {}
                Err(RecvTimeoutError::Timeout) => {
                    return Err(ApiError::new(504, "kernel metadata request timed out"));
                }
                Err(RecvTimeoutError::Disconnected) => {
                    return Err(ApiError::internal(
                        "kernel metadata response stream disconnected",
                    ));
                }
            }
        }
    }

    fn terminal_request(
        &self,
        conversation_id: &str,
        request: TerminalRequest,
    ) -> ApiResult<TerminalResponse> {
        let event_rx = self.subscribe_conversation_events(conversation_id)?;
        let request_id = web_request_id("terminal");
        self.send_terminal_request(conversation_id, request_id.clone(), request)?;
        wait_terminal_response(&event_rx, &request_id)
    }

    fn subscribe_conversation_events(
        &self,
        conversation_id: &str,
    ) -> ApiResult<crossbeam_channel::Receiver<ServiceChannelEvent>> {
        self.load_web_state(conversation_id)?;
        self.conversation_runtime
            .ensure_conversation_started(conversation_id)
            .map_err(ApiError::internal)?;
        self.conversation_runtime
            .subscribe_main_channel_events(conversation_id)
            .map_err(ApiError::internal)
    }

    fn send_terminal_request(
        &self,
        conversation_id: &str,
        request_id: String,
        request: TerminalRequest,
    ) -> ApiResult<()> {
        self.conversation_runtime
            .send_main_channel_ingress(
                conversation_id,
                ChannelIngress::Terminal {
                    request_id,
                    request,
                },
            )
            .map_err(ApiError::internal)
    }

    fn list_terminals(&self, conversation_id: &str) -> ApiResult<HttpResponse> {
        let response = self.terminal_request(conversation_id, TerminalRequest::List)?;
        Ok(json_response(200, json!(response)))
    }

    fn create_terminal(&self, conversation_id: &str, body: &[u8]) -> ApiResult<HttpResponse> {
        let request: TerminalCreateRequest = parse_optional_json(body)?;
        let response =
            self.terminal_request(conversation_id, TerminalRequest::Create { request })?;
        Ok(json_response(201, json!(response)))
    }

    fn get_terminal(&self, conversation_id: &str, terminal_id: &str) -> ApiResult<HttpResponse> {
        let response = self.terminal_request(
            conversation_id,
            TerminalRequest::Get {
                terminal_id: terminal_id.to_string(),
            },
        )?;
        Ok(json_response(200, json!(response)))
    }

    fn terminate_terminal(
        &self,
        conversation_id: &str,
        terminal_id: &str,
    ) -> ApiResult<HttpResponse> {
        let response = self.terminal_request(
            conversation_id,
            TerminalRequest::Terminate {
                terminal_id: terminal_id.to_string(),
            },
        )?;
        Ok(json_response(200, json!(response)))
    }

    fn load_web_state(&self, conversation_id: &str) -> ApiResult<ConversationMetadata> {
        validate_conversation_id(conversation_id)?;
        let store = ConversationMetadataStore::new(&self.workdir);
        if !store
            .layout()
            .conversation_metadata_path(conversation_id)
            .exists()
        {
            return Err(ApiError::new(404, "conversation_not_found"));
        }
        let metadata = store.load(conversation_id).map_err(ApiError::internal)?;
        if metadata.channel_id != self.id {
            return Err(ApiError::new(404, "conversation_not_found"));
        }
        Ok(metadata)
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
                "conversation_id": delivery.conversation_id,
                "session_id": delivery.session_id,
                "text_len": delivery.text.len(),
                "attachment_count": delivery.attachments.len(),
                "has_message": delivery.message.is_some(),
            }),
        );
        // Foreground web clients receive persisted session messages through
        // `message_appended`. `OutgoingDelivery` with a structured message is
        // still emitted for non-web channels (Telegram, etc.); publishing it
        // here as well duplicates the same message/attachments in the Web UI.
        if delivery.message.is_some() {
            return Ok(());
        }
        Ok(())
    }

    fn message_appended(&self, appended: &OutgoingMessageAppended) -> Result<()> {
        self.logger.info(
            "web_message_appended",
            json!({
                "channel_id": appended.channel_id,
                "platform_chat_id": appended.platform_chat_id,
                "conversation_id": appended.conversation_id,
                "session_id": appended.session_id,
                "index": appended.index,
                "role": appended.message.role,
                "items": appended.message.data.len(),
            }),
        );
        self.publish_foreground_message(
            &appended.platform_chat_id,
            &appended.conversation_id,
            Some(&appended.session_id),
            Some(appended.index),
            &appended.message,
        )
    }

    fn session_stream(&self, stream: &crate::channels::types::OutgoingSessionStream) -> Result<()> {
        self.logger.info(
            "web_session_stream",
            json!({
                "channel_id": stream.channel_id,
                "platform_chat_id": stream.platform_chat_id,
                "conversation_id": stream.conversation_id,
                "session_id": stream.session_id,
                "event_type": stream.event.get("type").and_then(Value::as_str),
            }),
        );
        self.publish_websocket_event(
            &stream.platform_chat_id,
            json!({
                "type": "session_stream",
                "conversation_id": stream.conversation_id,
                "session_id": stream.session_id,
                "event": stream.event,
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
        self.publish_conversation_processing(platform_chat_id, state);
        Ok(())
    }

    fn update_progress_feedback(&self, feedback: &OutgoingProgressFeedback) -> Result<()> {
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
        let payload = turn_progress_payload(feedback);
        if let Ok(mut progress) = self.active_turn_progress.lock() {
            if feedback.final_state.is_some() {
                progress.remove(&feedback.platform_chat_id);
            } else {
                progress.insert(feedback.platform_chat_id.clone(), payload.clone());
            }
        }
        self.publish_websocket_event(&feedback.platform_chat_id, payload);
        if feedback.final_state.is_some() {
            self.publish_conversation_turn_completed(
                &feedback.platform_chat_id,
                Some(&feedback.turn_id),
                feedback.final_state,
            );
        }
        Ok(())
    }

    fn conversation_updated(&self, platform_chat_id: &str, conversation_id: &str) -> Result<()> {
        self.logger.info(
            "web_conversation_updated",
            json!({
                "channel_id": self.id,
                "platform_chat_id": platform_chat_id,
                "conversation_id": conversation_id,
            }),
        );
        self.publish_conversation_upserted_for_platform_chat(platform_chat_id);
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

impl WebChannel {
    fn publish_foreground_message(
        &self,
        platform_chat_id: &str,
        conversation_id: &str,
        session_id: Option<&str>,
        message_index: Option<usize>,
        message: &ChatMessage,
    ) -> Result<()> {
        let Some(metadata) = self
            .conversation_state_for_platform_chat(platform_chat_id)
            .map_err(api_anyhow)?
        else {
            return Ok(());
        };
        if metadata.conversation_id != conversation_id {
            return Ok(());
        }
        if session_id.is_some_and(|session_id| session_id != metadata.foreground_session_id) {
            return Ok(());
        }
        let log_path = message_log_path(&self.workdir, &metadata);
        let total = count_message_lines(&log_path)?;
        let Some(index) = message_index.or_else(|| total.checked_sub(1)) else {
            return Ok(());
        };
        let attachments =
            WebAttachmentContext::new(&self.workdir, &metadata, self.cache_manager.clone());
        let page = MessagePage {
            start: index,
            end: index + 1,
            total,
            messages: vec![message.clone()],
        };
        self.publish_websocket_event(
            platform_chat_id,
            websocket_messages_payload(&metadata, &attachments, &page),
        );
        Ok(())
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
    message_time: Option<String>,
    #[serde(default)]
    text: Option<String>,
    #[serde(default)]
    selection_references: Option<Vec<SelectionReferenceItem>>,
    #[serde(default)]
    files: Option<Vec<WebFileItem>>,
}

#[derive(Debug, Deserialize)]
struct MoveWorkspacePathRequest {
    path: String,
    new_path: String,
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
        let media_type = clean_media_type(value.media_type)
            .or_else(|| infer_file_item_media_type(&value.name, &value.uri));
        FileItem {
            uri: value.uri,
            media_type,
            name: value.name,
            width: None,
            height: None,
            state: None,
        }
    }
}

fn sanitize_selection_references(
    selections: Vec<SelectionReferenceItem>,
) -> Vec<SelectionReferenceItem> {
    const MAX_SELECTIONS: usize = 8;
    const MAX_SELECTED_TEXT_CHARS: usize = 4_000;
    const MAX_CONTEXT_CHARS: usize = 800;
    const MAX_PATH_CHARS: usize = 512;
    const MAX_SMALL_FIELD_CHARS: usize = 180;
    const MAX_RECTS: usize = 16;

    selections
        .into_iter()
        .take(MAX_SELECTIONS)
        .filter_map(|mut selection| {
            selection.file_path = clean_selection_string(selection.file_path, MAX_PATH_CHARS)
                .trim_start_matches('/')
                .to_string();
            selection.source_kind =
                clean_selection_string(selection.source_kind.to_lowercase(), MAX_SMALL_FIELD_CHARS);
            let original_len = selection.original_text_length.unwrap_or_else(|| {
                selection
                    .selected_text
                    .chars()
                    .count()
                    .min(u32::MAX as usize) as u32
            });
            selection.selected_text =
                clean_selection_string(selection.selected_text, MAX_SELECTED_TEXT_CHARS);
            selection.original_text_length = Some(original_len);
            selection.file_name = selection
                .file_name
                .map(|value| clean_selection_string(value, MAX_SMALL_FIELD_CHARS))
                .filter(|value| !value.is_empty());
            selection.media_type = selection
                .media_type
                .map(|value| clean_selection_string(value, MAX_SMALL_FIELD_CHARS))
                .filter(|value| !value.is_empty());
            if let Some(locator) = &mut selection.locator {
                locator.kind =
                    clean_selection_string(locator.kind.to_lowercase(), MAX_SMALL_FIELD_CHARS);
                locator.heading = locator
                    .heading
                    .take()
                    .map(|value| clean_selection_string(value, MAX_SMALL_FIELD_CHARS))
                    .filter(|value| !value.is_empty());
                locator.selector = locator
                    .selector
                    .take()
                    .map(|value| clean_selection_string(value, MAX_SMALL_FIELD_CHARS))
                    .filter(|value| !value.is_empty());
                locator.xpath = locator
                    .xpath
                    .take()
                    .map(|value| clean_selection_string(value, MAX_SMALL_FIELD_CHARS))
                    .filter(|value| !value.is_empty());
                locator.block_id = locator
                    .block_id
                    .take()
                    .map(|value| clean_selection_string(value, MAX_SMALL_FIELD_CHARS))
                    .filter(|value| !value.is_empty());
                locator.anchor_text = locator
                    .anchor_text
                    .take()
                    .map(|value| clean_selection_string(value, MAX_SMALL_FIELD_CHARS))
                    .filter(|value| !value.is_empty());
                locator.rects.truncate(MAX_RECTS);
            }
            if let Some(context) = &mut selection.context {
                context.before = context
                    .before
                    .take()
                    .map(|value| clean_selection_string(value, MAX_CONTEXT_CHARS))
                    .filter(|value| !value.is_empty());
                context.after = context
                    .after
                    .take()
                    .map(|value| clean_selection_string(value, MAX_CONTEXT_CHARS))
                    .filter(|value| !value.is_empty());
            }

            (!selection.file_path.is_empty()
                && !selection.source_kind.is_empty()
                && !selection.selected_text.trim().is_empty())
            .then_some(selection)
        })
        .collect()
}

fn clean_selection_string(value: String, max_chars: usize) -> String {
    let trimmed = value.trim().replace('\0', "");
    trimmed.chars().take(max_chars).collect()
}

fn web_send_message_to_chat_message(request: SendMessageRequest) -> ApiResult<ChatMessage> {
    let text = request.text.unwrap_or_default();
    let files = request
        .files
        .unwrap_or_default()
        .into_iter()
        .map(FileItem::from)
        .collect::<Vec<_>>();
    let selection_references =
        sanitize_selection_references(request.selection_references.unwrap_or_default());
    if text.trim().is_empty() && files.is_empty() && selection_references.is_empty() {
        return Err(ApiError::new(
            400,
            "message requires text, files, or selection references",
        ));
    }

    let mut items = Vec::new();
    if !text.is_empty() {
        items.push(ChatMessageItem::Context(ContextItem { text }));
    }
    items.extend(
        selection_references
            .into_iter()
            .map(ChatMessageItem::SelectionReference),
    );
    items.extend(files.into_iter().map(ChatMessageItem::File));

    Ok(ChatMessage::new(ChatRole::User, items)
        .with_user_name_option(request.user_name)
        .with_message_time_option(Some(
            normalized_client_message_time(request.message_time).unwrap_or_else(now_rfc3339),
        )))
}

fn materialize_web_file_items(
    workdir: &Path,
    conversation_id: &str,
    remote_message_id: &str,
    files: Vec<WebFileItem>,
) -> Result<Vec<FileItem>> {
    let mut materialized = Vec::with_capacity(files.len());
    for (index, file) in files.into_iter().enumerate() {
        if file.uri.starts_with("data:") {
            materialized.push(materialize_web_data_file(
                workdir,
                conversation_id,
                remote_message_id,
                index,
                file,
            )?);
        } else {
            materialized.push(file.into());
        }
    }
    Ok(materialized)
}

fn materialize_web_data_file(
    workdir: &Path,
    conversation_id: &str,
    remote_message_id: &str,
    index: usize,
    file: WebFileItem,
) -> Result<FileItem> {
    let (data_media_type, payload) = parse_base64_data_url(&file.uri)?;
    let media_type = clean_media_type(file.media_type.clone())
        .or(data_media_type)
        .or_else(|| infer_file_item_media_type(&file.name, ""));
    let bytes = general_purpose::STANDARD
        .decode(payload)
        .context("failed to decode web message data URL file")?;
    let dir = WorkdirLayout::new(workdir)
        .conversation_root(conversation_id)
        .join(".stellaclaw")
        .join("attachments")
        .join("incoming");
    fs::create_dir_all(&dir).with_context(|| format!("failed to create {}", dir.display()))?;
    let name = file.name.unwrap_or_else(|| {
        format!(
            "attachment-{}.{}",
            index + 1,
            extension_for_media_type(media_type.as_deref())
        )
    });
    let filename = format!(
        "{}-{}-{}",
        sanitize_filename_component(remote_message_id),
        index + 1,
        sanitize_filename_component(&name)
    );
    let path = dir.join(filename);
    fs::write(&path, bytes).with_context(|| format!("failed to write {}", path.display()))?;
    Ok(FileItem {
        uri: format!("file://{}", path.display()),
        name: path
            .file_name()
            .and_then(|value| value.to_str())
            .map(ToOwned::to_owned),
        media_type,
        width: None,
        height: None,
        state: None,
    })
}

fn clean_media_type(media_type: Option<String>) -> Option<String> {
    media_type
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

fn parse_base64_data_url(uri: &str) -> Result<(Option<String>, &str)> {
    let (metadata, payload) = uri
        .split_once(',')
        .ok_or_else(|| anyhow!("malformed data URL file item"))?;
    let metadata = metadata
        .strip_prefix("data:")
        .ok_or_else(|| anyhow!("malformed data URL file item"))?;
    if !metadata.split(';').any(|part| part == "base64") {
        return Err(anyhow!("data URL file item must be base64 encoded"));
    }
    let media_type = metadata
        .split(';')
        .next()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned);
    Ok((media_type, payload))
}

fn infer_file_item_media_type(name: &Option<String>, uri: &str) -> Option<String> {
    name.as_deref()
        .and_then(infer_media_type_from_path_text)
        .or_else(|| infer_media_type_from_path_text(uri))
}

fn infer_media_type_from_path_text(value: &str) -> Option<String> {
    let path = value.split('?').next().unwrap_or(value);
    match Path::new(path)
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
        "m4a" => Some("audio/mp4".to_string()),
        "flac" => Some("audio/flac".to_string()),
        "mp4" => Some("video/mp4".to_string()),
        "mov" => Some("video/quicktime".to_string()),
        "txt" => Some("text/plain".to_string()),
        _ => None,
    }
}

fn extension_for_media_type(media_type: Option<&str>) -> &'static str {
    match media_type.unwrap_or_default() {
        "image/png" => "png",
        "image/jpeg" => "jpg",
        "image/webp" => "webp",
        "image/gif" => "gif",
        "application/pdf" => "pdf",
        "audio/mpeg" => "mp3",
        "audio/ogg" => "ogg",
        "audio/wav" => "wav",
        "audio/mp4" => "m4a",
        "audio/flac" => "flac",
        "video/mp4" => "mp4",
        "video/quicktime" => "mov",
        "text/plain" => "txt",
        _ => "bin",
    }
}

fn sanitize_filename_component(value: &str) -> String {
    let sanitized = value
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || matches!(ch, '.' | '-' | '_') {
                ch
            } else {
                '_'
            }
        })
        .collect::<String>();
    let trimmed = sanitized.trim_matches('_');
    if trimmed.is_empty() {
        "file".to_string()
    } else {
        trimmed.to_string()
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
    data: Vec<ChatMessageItem>,
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
    Reasoning {
        index: usize,
        text: String,
        summary: Option<String>,
    },
    Text {
        index: usize,
        text: String,
        text_with_attachment_markers: String,
    },
    SelectionReference {
        index: usize,
        selection: SelectionReferenceItem,
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
        structured: Option<Value>,
        file_attachment_indices: Vec<usize>,
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

struct WebAttachmentExtraction {
    clean_text: String,
    marked_text: String,
    attachments: Vec<OutgoingAttachment>,
}

fn extract_attachment_references_with_markers(
    text: &str,
    workspace_root: &Path,
    shared_root: &Path,
    base_attachment_index: usize,
) -> Result<WebAttachmentExtraction> {
    const START: &str = "<attachment>";
    const END: &str = "</attachment>";

    let mut clean = String::with_capacity(text.len());
    let mut marked = String::with_capacity(text.len());
    let mut attachments = Vec::new();
    let mut cursor = 0usize;

    while let Some(start_rel) = text[cursor..].find(START) {
        let start = cursor + start_rel;
        if is_inside_fenced_code_block(text, start) {
            let start_end = start + START.len();
            clean.push_str(&text[cursor..start_end]);
            marked.push_str(&text[cursor..start_end]);
            cursor = start_end;
            continue;
        }
        clean.push_str(&text[cursor..start]);
        marked.push_str(&text[cursor..start]);
        let path_start = start + START.len();
        let Some(end_rel) = text[path_start..].find(END) else {
            clean.push_str(&text[start..]);
            marked.push_str(&text[start..]);
            return Ok(WebAttachmentExtraction {
                clean_text: clean.trim().to_string(),
                marked_text: marked.trim().to_string(),
                attachments,
            });
        };
        let path_end = path_start + end_rel;
        let path_text = text[path_start..path_end].trim();
        if !path_text.is_empty() {
            let marker = attachment_marker(base_attachment_index + attachments.len());
            attachments.push(resolve_outgoing_attachment(
                workspace_root,
                shared_root,
                path_text,
            )?);
            marked.push_str(&marker);
        }
        cursor = path_end + END.len();
    }

    clean.push_str(&text[cursor..]);
    marked.push_str(&text[cursor..]);
    Ok(WebAttachmentExtraction {
        clean_text: clean.trim().to_string(),
        marked_text: marked.trim().to_string(),
        attachments,
    })
}

fn attachment_marker(index: usize) -> String {
    format!("[[attachment:{index}]]")
}

fn strip_attachment_tags(text: &str) -> String {
    const START: &str = "<attachment>";
    const END: &str = "</attachment>";

    let mut clean = String::with_capacity(text.len());
    let mut cursor = 0usize;
    while let Some(start_rel) = text[cursor..].find(START) {
        let start = cursor + start_rel;
        if is_inside_fenced_code_block(text, start) {
            let start_end = start + START.len();
            clean.push_str(&text[cursor..start_end]);
            cursor = start_end;
            continue;
        }
        clean.push_str(&text[cursor..start]);
        let path_start = start + START.len();
        let Some(end_rel) = text[path_start..].find(END) else {
            clean.push_str(&text[start..]);
            return clean;
        };
        let path_end = path_start + end_rel;
        let path_text = text[path_start..path_end].trim();
        if !path_text.is_empty() {
            clean.push_str(path_text);
        }
        cursor = path_end + END.len();
    }
    clean.push_str(&text[cursor..]);
    clean
}

fn is_inside_fenced_code_block(text: &str, byte_index: usize) -> bool {
    let mut inside = false;
    let mut offset = 0usize;
    for line in text.split_inclusive('\n') {
        if offset >= byte_index {
            break;
        }
        let trimmed = line.trim_start();
        if trimmed.starts_with("```") || trimmed.starts_with("~~~") {
            inside = !inside;
        }
        offset += line.len();
    }
    inside
}

fn resolve_outgoing_attachment(
    workspace_root: &Path,
    shared_root: &Path,
    path_text: &str,
) -> Result<OutgoingAttachment> {
    let joined = attachment_candidate_path(workspace_root, path_text);
    let canonical = joined
        .canonicalize()
        .with_context(|| format!("attachment path does not exist: {}", joined.display()))?;
    let root = workspace_root
        .canonicalize()
        .with_context(|| format!("failed to canonicalize {}", workspace_root.display()))?;
    let allowed_runtime_shared = is_shared_attachment_path(path_text);
    let shared_root = shared_root.canonicalize().ok();
    let in_runtime_shared = allowed_runtime_shared
        && shared_root
            .as_ref()
            .is_some_and(|shared_root| canonical.starts_with(shared_root));
    if !canonical.starts_with(&root) && !in_runtime_shared {
        return Err(anyhow!(
            "attachment path escapes conversation root: {}",
            canonical.display()
        ));
    }
    if !canonical.is_file() {
        return Err(anyhow!(
            "attachment path is not a regular file: {}",
            canonical.display()
        ));
    }
    Ok(OutgoingAttachment {
        kind: infer_outgoing_attachment_kind(&canonical),
        path: canonical,
    })
}

fn attachment_candidate_path(workspace_root: &Path, path_text: &str) -> PathBuf {
    let path = Path::new(path_text);
    if !path.is_absolute() {
        return workspace_root.join(path);
    }

    if let Some(remapped) = remap_absolute_conversation_path(workspace_root, path) {
        if remapped.exists() {
            return remapped;
        }
    }
    path.to_path_buf()
}

fn remap_absolute_conversation_path(workspace_root: &Path, path: &Path) -> Option<PathBuf> {
    let conversation_name = workspace_root.file_name()?;
    let mut components = path.components().peekable();
    while let Some(component) = components.next() {
        if component.as_os_str() != "conversations" {
            continue;
        }
        let Some(next) = components.next() else {
            return None;
        };
        if next.as_os_str() != conversation_name {
            continue;
        }
        let mut remapped = workspace_root.to_path_buf();
        for rest in components {
            remapped.push(rest.as_os_str());
        }
        return Some(remapped);
    }
    None
}

fn is_shared_attachment_path(path_text: &str) -> bool {
    if path_text == ".stellaclaw/shared" {
        return true;
    }
    for prefix in [".stellaclaw/shared/", ".stellaclaw/shared\\"] {
        if let Some(relative) = path_text.strip_prefix(prefix) {
            if !relative.trim().is_empty() {
                return true;
            }
        }
    }
    false
}

fn infer_outgoing_attachment_kind(path: &Path) -> OutgoingAttachmentKind {
    match path
        .extension()
        .and_then(|value| value.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase()
        .as_str()
    {
        "png" | "jpg" | "jpeg" | "webp" => OutgoingAttachmentKind::Image,
        "gif" => OutgoingAttachmentKind::Animation,
        "mp3" | "wav" => OutgoingAttachmentKind::Audio,
        "ogg" => OutgoingAttachmentKind::Voice,
        "mp4" | "mov" | "mkv" => OutgoingAttachmentKind::Video,
        _ => OutgoingAttachmentKind::Document,
    }
}

fn reasoning_web_text(reasoning: &stellaclaw_core::session_actor::ReasoningItem) -> Option<String> {
    reasoning
        .codex_summary_text()
        .as_deref()
        .or_else(|| (!reasoning.text.is_empty()).then_some(reasoning.text.as_str()))
        .map(str::trim)
        .filter(|text| !text.is_empty())
        .map(str::to_string)
}

struct WebAttachmentContext {
    conversation_id: String,
    workdir: PathBuf,
    metadata: ConversationMetadata,
    cache_manager: Arc<CacheManager>,
}

struct WebAttachmentRoots {
    workspace_root: PathBuf,
    shared_root: PathBuf,
}

impl WebAttachmentContext {
    fn new(
        workdir: &Path,
        metadata: &ConversationMetadata,
        cache_manager: Arc<CacheManager>,
    ) -> Self {
        Self {
            conversation_id: metadata.conversation_id.clone(),
            workdir: workdir.to_path_buf(),
            metadata: metadata.clone(),
            cache_manager,
        }
    }

    fn roots(&self) -> WebAttachmentRoots {
        let layout = WorkdirLayout::new(&self.workdir);
        let conversation_root = layout.conversation_root(&self.metadata.conversation_id);
        let workspace_root = conversation_root;
        let shared_root = layout.runtime_shared_root();
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
    reasoning: String,
    sandbox: String,
    sandbox_source: String,
    remote: String,
    workspace: String,
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

#[derive(Debug, Clone, Default)]
struct MessagePage {
    start: usize,
    end: usize,
    total: usize,
    messages: Vec<ChatMessage>,
}

#[derive(Debug, Clone, Default, Deserialize)]
struct MessagesIndexFile {
    #[serde(default)]
    message_count: usize,
    #[serde(default)]
    last_message_id: Option<String>,
    #[serde(default)]
    messages: HashMap<String, MessageIndexEntry>,
}

#[derive(Debug, Clone, Deserialize)]
struct MessageIndexEntry {
    index: usize,
    byte_offset: u64,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct ConversationSeen {
    last_seen_message_id: String,
    updated_at: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct WebChannelState {
    #[serde(default)]
    seen: HashMap<String, ConversationSeen>,
}

impl ConversationSummary {
    fn from_metadata(
        workdir: &Path,
        metadata: &ConversationMetadata,
        config: &StellaclawConfig,
        runtime_config: Option<&ConversationRuntimeConfig>,
        processing_state: ProcessingState,
        message_summary: ConversationMessageSummary,
        seen: Option<ConversationSeen>,
    ) -> Self {
        Self {
            conversation_id: metadata.conversation_id.clone(),
            nickname: conversation_nickname(metadata),
            platform_chat_id: metadata.platform_chat_id.clone(),
            model: runtime_config
                .and_then(|config| config.session_profile.as_ref())
                .or(config.default_profile.as_ref())
                .map(|profile| profile.main_model.display_name(&config.models))
                .or_else(|| config.initial_main_model_name())
                .unwrap_or_else(|| "unconfigured".to_string()),
            model_selection_pending: metadata.model_selection_pending,
            reasoning: runtime_config
                .and_then(|config| config.reasoning_effort.as_deref())
                .unwrap_or("model default")
                .to_string(),
            sandbox: conversation_sandbox_name(runtime_config, config).to_string(),
            sandbox_source: if runtime_config
                .and_then(|config| config.sandbox.as_ref())
                .is_some()
            {
                "conversation"
            } else {
                "default"
            }
            .to_string(),
            remote: conversation_remote_name(
                runtime_config
                    .map(|config| &config.tool_remote_mode)
                    .unwrap_or(&ToolRemoteMode::Selectable),
            ),
            workspace: conversation_workspace_root(workdir, metadata)
                .display()
                .to_string(),
            foreground_session_id: metadata.foreground_session_id.clone(),
            total_background: 0,
            total_subagents: 0,
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

fn conversation_sandbox_name(
    runtime_config: Option<&ConversationRuntimeConfig>,
    config: &StellaclawConfig,
) -> &'static str {
    let sandbox = runtime_config
        .and_then(|config| config.sandbox.as_ref())
        .unwrap_or(&config.sandbox);
    match sandbox.mode {
        SandboxMode::Bubblewrap => "bubblewrap",
        SandboxMode::Subprocess => "subprocess",
    }
}

fn conversation_remote_name(tool_remote_mode: &ToolRemoteMode) -> String {
    match tool_remote_mode {
        ToolRemoteMode::Selectable => "selectable".to_string(),
        ToolRemoteMode::FixedSsh { host, cwd } => {
            format!("fixed ssh `{host}` `{}`", cwd.as_deref().unwrap_or(""))
        }
    }
}

fn conversation_workspace_root(workdir: &Path, metadata: &ConversationMetadata) -> PathBuf {
    WorkdirLayout::new(workdir).conversation_root(&metadata.conversation_id)
}

fn conversation_message_summary(
    workdir: &Path,
    metadata: &ConversationMetadata,
) -> ConversationMessageSummary {
    let path = message_log_path(workdir, metadata);
    if let Ok(Some(index)) = read_messages_index(&path) {
        let last_message_time = index
            .last_message_id
            .as_deref()
            .and_then(|message_id| read_message_by_id(&path, message_id).ok().flatten())
            .and_then(|(_, message)| message.message_time);
        return ConversationMessageSummary {
            message_count: index.message_count,
            last_message_id: index.last_message_id,
            last_message_time: last_message_time.or_else(|| {
                fs::metadata(&path)
                    .ok()
                    .and_then(|metadata| metadata.modified().ok())
                    .and_then(system_time_rfc3339)
            }),
        };
    }

    let Ok(Some(reader)) = open_message_reader(&path) else {
        return ConversationMessageSummary::default();
    };
    let mut summary = ConversationMessageSummary::default();
    let mut last_message: Option<ChatMessage> = None;
    for line in reader.lines().map_while(Result::ok) {
        if line.trim().is_empty() {
            continue;
        }
        summary.message_count = summary.message_count.saturating_add(1);
        if let Ok(message) = serde_json::from_str::<ChatMessage>(&line) {
            last_message = Some(message);
        }
    }
    if let Some(message) = last_message {
        summary.last_message_id = (!message.message_id.is_empty()).then_some(message.message_id);
        summary.last_message_time = message.message_time;
        if summary.last_message_time.is_none() {
            summary.last_message_time = fs::metadata(&path)
                .ok()
                .and_then(|metadata| metadata.modified().ok())
                .and_then(system_time_rfc3339);
        }
    }
    summary
}

fn load_conversation_runtime_config(
    workdir: &Path,
    conversation_id: &str,
) -> Result<ConversationRuntimeConfig> {
    let path = WorkdirLayout::new(workdir)
        .conversation_service_root(conversation_id)
        .join("runtime_config.json");
    let raw =
        fs::read_to_string(&path).with_context(|| format!("failed to read {}", path.display()))?;
    serde_json::from_str(&raw).with_context(|| format!("failed to parse {}", path.display()))
}

fn compare_message_ids(path: &Path, left: &str, right: &str) -> Option<Ordering> {
    if left == right {
        return Some(Ordering::Equal);
    }
    match (
        message_index_for_id(path, left),
        message_index_for_id(path, right),
    ) {
        (Some(left), Some(right)) => Some(left.cmp(&right)),
        _ => {
            let left = parse_message_order(left)?;
            let right = parse_message_order(right)?;
            Some(left.cmp(&right))
        }
    }
}

fn parse_message_order(message_id: &str) -> Option<u64> {
    let rest = message_id.strip_prefix("msg_")?;
    let (index, _suffix) = rest.split_once('_')?;
    index.parse::<u64>().ok()
}

fn web_channel_state_dir(workdir: &Path, channel_id: &str) -> PathBuf {
    workdir
        .join(".stellaclaw")
        .join("channels")
        .join(channel_id)
}

fn load_web_channel_state(
    workdir: &Path,
    channel_id: &str,
    logger: &StellaclawLogger,
) -> WebChannelState {
    let path = web_channel_state_dir(workdir, channel_id).join(WEB_CHANNEL_STATE_FILE);
    if !path.exists() {
        return WebChannelState::default();
    }
    let raw = match fs::read_to_string(&path) {
        Ok(raw) => raw,
        Err(error) => {
            logger.warn(
                "web_channel_state_read_failed",
                json!({"channel_id": channel_id, "path": path.display().to_string(), "error": error.to_string()}),
            );
            return WebChannelState::default();
        }
    };
    match serde_json::from_str(&raw) {
        Ok(state) => state,
        Err(error) => {
            logger.warn(
                "web_channel_state_parse_failed",
                json!({"channel_id": channel_id, "path": path.display().to_string(), "error": error.to_string()}),
            );
            WebChannelState::default()
        }
    }
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

fn write_terminal_replay(stream: &mut TcpStream, replay: &TerminalReplaySnapshot) -> Result<()> {
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
        let bytes = decode_terminal_data(chunk.encoding, &chunk.data)?;
        write_websocket_frame(stream, 0x2, &bytes)?;
    }
    Ok(())
}

fn decode_terminal_data(encoding: TerminalDataEncoding, data: &str) -> Result<Vec<u8>> {
    match encoding {
        TerminalDataEncoding::Utf8 => Ok(data.as_bytes().to_vec()),
        TerminalDataEncoding::Base64 => general_purpose::STANDARD
            .decode(data)
            .context("failed to decode terminal base64 payload"),
    }
}

fn wait_terminal_response(
    event_rx: &crossbeam_channel::Receiver<ServiceChannelEvent>,
    request_id: &str,
) -> ApiResult<TerminalResponse> {
    loop {
        match event_rx.recv_timeout(WORKSPACE_REQUEST_TIMEOUT) {
            Ok(ServiceChannelEvent::Terminal {
                request_id: Some(event_request_id),
                response,
            }) if event_request_id == request_id => {
                return match response {
                    TerminalResponse::Error { message, .. } => Err(ApiError::new(400, message)),
                    response => Ok(response),
                };
            }
            Ok(_) => {}
            Err(RecvTimeoutError::Timeout) => {
                return Err(ApiError::new(504, "terminal request timed out"));
            }
            Err(RecvTimeoutError::Disconnected) => {
                return Err(ApiError::internal("terminal response stream disconnected"));
            }
        }
    }
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

fn web_request_id(prefix: &str) -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    format!("{prefix}-{}-{nanos}", std::process::id())
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
    metadata: &ConversationMetadata,
    attachments: &WebAttachmentContext,
    page: &MessagePage,
    offset: usize,
    limit: usize,
) -> Value {
    let roots = attachments.roots();
    json!({
        "conversation_id": &metadata.conversation_id,
        "offset": offset,
        "limit": limit,
        "total": page.total,
        "messages": page.messages
            .iter()
            .enumerate()
            .map(|(relative, message)| message_skeleton(page.start + relative, message, attachments, &roots))
            .collect::<Vec<_>>(),
    })
}

fn websocket_subscription_ack(
    metadata: &ConversationMetadata,
    message_summary: &ConversationMessageSummary,
    reason: &'static str,
    turn_progress: Option<Value>,
) -> Value {
    json!({
        "type": "subscription_ack",
        "reason": reason,
        "conversation_id": &metadata.conversation_id,
        "session_id": &metadata.foreground_session_id,
        "current_message_id": message_summary.last_message_id.clone(),
        "next_message_index": message_summary.message_count,
        "total": message_summary.message_count,
        "turn_progress": turn_progress,
    })
}

fn websocket_messages_payload(
    metadata: &ConversationMetadata,
    attachments: &WebAttachmentContext,
    page: &MessagePage,
) -> Value {
    let roots = attachments.roots();
    json!({
        "type": "messages",
        "conversation_id": &metadata.conversation_id,
        "session_id": &metadata.foreground_session_id,
        "offset": page.start,
        "start_index": page.start,
        "end_index": page.end,
        "total": page.total,
        "messages": page.messages
            .iter()
            .enumerate()
            .map(|(relative, message)| message_skeleton(page.start + relative, message, attachments, &roots))
            .collect::<Vec<_>>(),
    })
}

fn message_skeleton(
    index: usize,
    message: &ChatMessage,
    attachments: &WebAttachmentContext,
    roots: &WebAttachmentRoots,
) -> MessageSkeleton {
    let rendered = render_web_message(message, attachments, roots);
    MessageSkeleton {
        id: message.message_id.clone(),
        index,
        role: message.role.clone(),
        text: rendered.text.clone(),
        text_with_attachment_markers: rendered.text_with_attachment_markers.clone(),
        preview: preview_text(&rendered.text),
        data: web_chat_message_data(message),
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

fn web_chat_message_data(message: &ChatMessage) -> Vec<ChatMessageItem> {
    message
        .data
        .iter()
        .map(|item| match item {
            ChatMessageItem::Reasoning(reasoning) => {
                ChatMessageItem::Reasoning(stellaclaw_core::session_actor::ReasoningItem {
                    text: reasoning.text.clone(),
                    codex_summary: reasoning.codex_summary.clone(),
                    codex_encrypted_content: None,
                })
            }
            other => other.clone(),
        })
        .collect()
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
            ChatMessageItem::Reasoning(reasoning) => {
                if let Some(text) = reasoning_web_text(reasoning) {
                    let summary = reasoning.codex_summary_text();
                    items.push(WebMessageItem::Reasoning {
                        index: item_index,
                        text,
                        summary,
                    });
                }
            }
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
            ChatMessageItem::SelectionReference(selection) => {
                items.push(WebMessageItem::SelectionReference {
                    index: item_index,
                    selection: selection.clone(),
                });
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
                let rendered_tool_result =
                    stellaclaw_core::session_actor::tool_result_structured_text(tool_result);
                let context_text = if rendered_tool_result.trim().is_empty() {
                    None
                } else {
                    let part = render_web_text_part(
                        &rendered_tool_result,
                        context,
                        roots,
                        &mut attachments,
                        &mut attachment_errors,
                    );
                    if part.text.is_empty() && part.text_with_attachment_markers.is_empty() {
                        None
                    } else {
                        context_with_attachment_markers = Some(part.text_with_attachment_markers);
                        Some(part.text)
                    }
                };
                let mut file_attachment_indices = Vec::new();
                for file in &tool_result.result.files {
                    let attachment_index = attachments.len();
                    attachments.push(web_file_item_attachment(
                        attachment_index,
                        "tool_result_file",
                        file,
                        context,
                        roots,
                    ));
                    file_attachment_indices.push(attachment_index);
                }
                items.push(WebMessageItem::ToolResult {
                    index: item_index,
                    tool_call_id: tool_result.tool_call_id.clone(),
                    tool_name: tool_result.tool_name.clone(),
                    context: context_text,
                    context_with_attachment_markers,
                    structured: tool_result.result.structured.clone(),
                    file_attachment_indices,
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
    if let (Ok(canonical_path), Ok(canonical_workspace)) =
        (path.canonicalize(), roots.workspace_root.canonicalize())
    {
        if let Ok(relative) = canonical_path.strip_prefix(canonical_workspace) {
            return Some(path_to_api_string(relative));
        }
    }
    if let Ok(relative) = path.strip_prefix(&roots.shared_root) {
        return Some(format!(
            ".stellaclaw/shared/{}",
            path_to_api_string(relative)
        ));
    }
    if let (Ok(canonical_path), Ok(canonical_shared)) =
        (path.canonicalize(), roots.shared_root.canonicalize())
    {
        if let Ok(relative) = canonical_path.strip_prefix(canonical_shared) {
            return Some(format!(
                ".stellaclaw/shared/{}",
                path_to_api_string(relative)
            ));
        }
    }
    None
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

fn turn_progress_payload(feedback: &OutgoingProgressFeedback) -> Value {
    json!({
        "type": "turn_progress",
        "turn_id": &feedback.turn_id,
        "phase": feedback.progress.phase,
        "model": &feedback.progress.model,
        "activity": &feedback.progress.activity,
        "hint": &feedback.progress.hint,
        "plan": &feedback.progress.plan,
        "error": &feedback.progress.error,
        "progress": &feedback.progress,
        "final_state": feedback.final_state.map(progress_final_state_name),
        "important": feedback.important,
    })
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

fn message_log_path(workdir: &Path, metadata: &ConversationMetadata) -> PathBuf {
    WorkdirLayout::new(workdir)
        .conversation_root(&metadata.conversation_id)
        .join(".stellaclaw")
        .join("log")
        .join(sanitize_session_id_for_log_path(
            &metadata.foreground_session_id,
        ))
        .join("all_messages.jsonl")
}

fn messages_index_path(message_log_path: &Path) -> PathBuf {
    message_log_path.with_file_name("messages_index.json")
}

fn open_message_reader(path: &Path) -> Result<Option<BufReader<fs::File>>> {
    match fs::File::open(path) {
        Ok(file) => Ok(Some(BufReader::new(file))),
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error).with_context(|| format!("failed to open {}", path.display())),
    }
}

fn read_messages_index(path: &Path) -> Result<Option<MessagesIndexFile>> {
    let index_path = messages_index_path(path);
    match fs::read_to_string(&index_path) {
        Ok(raw) => serde_json::from_str::<MessagesIndexFile>(&raw)
            .map(Some)
            .with_context(|| format!("failed to parse {}", index_path.display())),
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(None),
        Err(error) => {
            Err(error).with_context(|| format!("failed to read {}", index_path.display()))
        }
    }
}

fn message_index_for_id(path: &Path, message_id: &str) -> Option<usize> {
    read_messages_index(path)
        .ok()
        .flatten()
        .and_then(|index| index.messages.get(message_id).map(|entry| entry.index))
}

fn count_message_lines(path: &Path) -> Result<usize> {
    if let Some(index) = read_messages_index(path)? {
        return Ok(index.message_count);
    }
    let Some(reader) = open_message_reader(path)? else {
        return Ok(0);
    };
    let mut count = 0usize;
    for line in reader.lines() {
        let line = line.with_context(|| format!("failed to read {}", path.display()))?;
        if !line.trim().is_empty() {
            count = count.saturating_add(1);
        }
    }
    Ok(count)
}

fn read_message_page(path: &Path, offset: usize, limit: usize) -> Result<MessagePage> {
    let Some(reader) = open_message_reader(path)? else {
        return Ok(MessagePage::default());
    };
    let mut total = 0usize;
    let mut messages = Vec::new();
    let end_limit = offset.saturating_add(limit);
    for line in reader.lines() {
        let line = line.with_context(|| format!("failed to read {}", path.display()))?;
        if line.trim().is_empty() {
            continue;
        }
        let index = total;
        total = total.saturating_add(1);
        if index < offset || index >= end_limit {
            continue;
        }
        messages.push(
            serde_json::from_str::<ChatMessage>(&line)
                .with_context(|| format!("failed to parse {}", path.display()))?,
        );
    }
    let start = offset.min(total);
    let end = start.saturating_add(messages.len()).min(total);
    Ok(MessagePage {
        start,
        end,
        total,
        messages,
    })
}

fn read_message_by_id(path: &Path, message_id: &str) -> Result<Option<(usize, ChatMessage)>> {
    if let Some(index) = read_messages_index(path)? {
        if let Some(entry) = index.messages.get(message_id) {
            let mut reader = BufReader::new(
                fs::File::open(path)
                    .with_context(|| format!("failed to open {}", path.display()))?,
            );
            reader
                .seek(SeekFrom::Start(entry.byte_offset))
                .with_context(|| format!("failed to seek {}", path.display()))?;
            let mut line = String::new();
            reader
                .read_line(&mut line)
                .with_context(|| format!("failed to read {}", path.display()))?;
            if !line.trim().is_empty() {
                return serde_json::from_str::<ChatMessage>(&line)
                    .map(|message| Some((entry.index, message)))
                    .with_context(|| format!("failed to parse {}", path.display()));
            }
        }
    }

    let Some(reader) = open_message_reader(path)? else {
        return Ok(None);
    };
    let mut current = 0usize;
    for line in reader.lines() {
        let line = line.with_context(|| format!("failed to read {}", path.display()))?;
        if line.trim().is_empty() {
            continue;
        }
        let message: ChatMessage = serde_json::from_str(&line)
            .with_context(|| format!("failed to parse {}", path.display()))?;
        if message.message_id == message_id {
            return Ok(Some((current, message)));
        }
        current = current.saturating_add(1);
    }
    Ok(None)
}

fn query_usize(query: &HashMap<String, String>, name: &str, default: usize) -> usize {
    query
        .get(name)
        .and_then(|value| value.parse::<usize>().ok())
        .unwrap_or(default)
}

fn query_bool(query: &HashMap<String, String>, name: &str) -> bool {
    query
        .get(name)
        .map(|value| matches!(value.as_str(), "1" | "true" | "yes" | "on"))
        .unwrap_or(false)
}

fn is_image_workspace_path(path: &str) -> bool {
    let extension = Path::new(path)
        .extension()
        .and_then(|value| value.to_str())
        .map(str::to_ascii_lowercase);
    matches!(
        extension.as_deref(),
        Some("png" | "jpg" | "jpeg" | "gif" | "webp" | "heic" | "heif")
    )
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

fn conversation_nickname(metadata: &ConversationMetadata) -> String {
    normalize_conversation_nickname(&metadata.conversation_id, &metadata.nickname)
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

fn normalized_client_message_time(message_time: Option<String>) -> Option<String> {
    let value = message_time?.trim().to_string();
    if value.is_empty() {
        return None;
    }
    chrono::DateTime::parse_from_rfc3339(&value)
        .ok()
        .map(|time| time.to_rfc3339())
}

fn system_time_rfc3339(system_time: SystemTime) -> Option<String> {
    OffsetDateTime::from(system_time).format(&Rfc3339).ok()
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
        "/reasoning" => Some(parse_reasoning_control_argument(argument)),
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

    #[test]
    fn message_log_path_uses_stellaclaw_log_directory() {
        let workdir = test_workdir("message-log-path");
        let state = test_state("web-main-test-message-log-path");

        assert_eq!(
            message_log_path(&workdir, &state),
            workdir
                .join("conversations")
                .join(&state.conversation_id)
                .join(".stellaclaw")
                .join("log")
                .join(&state.foreground_session_id)
                .join("all_messages.jsonl")
        );

        let _ = fs::remove_dir_all(workdir);
    }

    #[test]
    fn web_data_url_file_items_are_materialized_before_chat_storage() {
        let workdir = test_workdir("materialize-data-url");
        let files = materialize_web_file_items(
            &workdir,
            "web-main-test-materialize",
            "web-message-1",
            vec![WebFileItem {
                uri: "data:image/png;base64,aGVsbG8=".to_string(),
                media_type: None,
                name: Some("photo.png".to_string()),
            }],
        )
        .expect("data url should materialize");

        assert_eq!(files.len(), 1);
        let file = &files[0];
        assert!(file.uri.starts_with("file://"));
        assert!(file.uri.contains(".stellaclaw/attachments/incoming"));
        assert_eq!(file.media_type.as_deref(), Some("image/png"));
        assert!(file
            .name
            .as_deref()
            .is_some_and(|name| name.ends_with("photo.png")));
        let path = PathBuf::from(file.uri.strip_prefix("file://").unwrap());
        assert_eq!(fs::read(path).expect("materialized bytes"), b"hello");

        let _ = fs::remove_dir_all(workdir);
    }

    #[test]
    fn web_file_items_infer_media_type_for_materialized_paths() {
        let file: FileItem = WebFileItem {
            uri: "file:///workspace/report.pdf".to_string(),
            media_type: None,
            name: None,
        }
        .into();

        assert_eq!(file.media_type.as_deref(), Some("application/pdf"));
        assert_eq!(file.uri, "file:///workspace/report.pdf");
    }

    use std::{collections::BTreeMap, fs};

    use crate::channels::types::{
        TurnProgress, TurnProgressPhase, TurnProgressPlan, TurnProgressPlanItem,
        TurnProgressPlanItemStatus,
    };
    use stellaclaw_core::session_actor::{ChatMessageItem, ContextItem, TokenUsageCost};

    fn test_workdir(name: &str) -> PathBuf {
        let path = std::env::temp_dir().join(format!(
            "stellaclaw-web-{name}-{}-{}",
            std::process::id(),
            unix_millis()
        ));
        fs::create_dir_all(&path).expect("create temp workdir");
        path
    }

    fn test_state(conversation_id: &str) -> ConversationMetadata {
        ConversationMetadata {
            version: 1,
            conversation_id: conversation_id.to_string(),
            nickname: conversation_id.to_string(),
            channel_id: "web-main".to_string(),
            platform_chat_id: "test-chat".to_string(),
            foreground_session_id: format!("{conversation_id}.foreground"),
            model_selection_pending: false,
            session_nicknames: BTreeMap::new(),
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
            parse_web_control("/reasoning"),
            Some(ConversationControl::ShowReasoning)
        ));
        assert!(matches!(
            parse_web_control("/reasoning high"),
            Some(ConversationControl::SetReasoning { effort: Some(effort) }) if effort == "high"
        ));
        assert!(matches!(
            parse_web_control("/reasoning default"),
            Some(ConversationControl::SetReasoning { effort: None })
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
    fn conversation_message_summary_uses_message_id() {
        let workdir = test_workdir("message-summary-message-id");
        let state = test_state("web-main-000001");
        let message_dir = workdir
            .join("conversations")
            .join(&state.conversation_id)
            .join(".stellaclaw")
            .join("log")
            .join(sanitize_session_id_for_log_path(
                &state.foreground_session_id,
            ));
        fs::create_dir_all(&message_dir).expect("create message log dir");
        fs::write(
            message_dir.join("all_messages.jsonl"),
            concat!(
                r#"{"message_id":"msg_1","role":"user","data":[]}"#,
                "\n",
                r#"{"message_id":"msg_2","role":"assistant","message_time":"2026-04-30T15:30:46Z","data":[]}"#,
                "\n",
            ),
        )
        .expect("write message log");

        let summary = conversation_message_summary(&workdir, &state);

        assert_eq!(summary.message_count, 2);
        assert_eq!(summary.last_message_id.as_deref(), Some("msg_2"));
        assert_eq!(
            summary.last_message_time.as_deref(),
            Some("2026-04-30T15:30:46Z")
        );
        let _ = fs::remove_dir_all(workdir);
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

        let payload = turn_progress_payload(&feedback);

        assert_eq!(payload["type"], "turn_progress");
        assert!(payload.get("subscription").is_none());
        assert_eq!(payload["turn_id"], "turn-1");
        assert_eq!(payload["phase"], "working");
        assert_eq!(payload["model"], "gpt-5.5");
        assert_eq!(payload["activity"], "读取代码");
        assert_eq!(payload["hint"], "发送新消息可打断");
        assert_eq!(payload["final_state"], serde_json::Value::Null);
        assert_eq!(payload["important"], true);
        assert_eq!(payload["plan"]["items"][0]["status"], "in_progress");
        assert_eq!(payload["progress"]["phase"], "working");
        assert!(payload.get("text").is_none());
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
        let page = MessagePage {
            start: 0,
            end: 1,
            total: 1,
            messages: vec![message],
        };
        let payload = message_page_payload(&state, &context, &page, 0, 50);

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
        let message_page = MessagePage {
            start: 0,
            end: 1,
            total: 1,
            messages: vec![message.clone()],
        };
        let page = message_page_payload(&state, &context, &message_page, 0, 50);
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

        let websocket = websocket_messages_payload(&state, &context, &message_page);
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
    fn websocket_ack_reports_current_message_id_and_next_index() {
        let state = test_state("web-main-test-ws-ack");
        let summary = ConversationMessageSummary {
            message_count: 3,
            last_message_id: Some("msg_3".to_string()),
            last_message_time: None,
        };
        let payload = websocket_subscription_ack(
            &state,
            &summary,
            "subscribed",
            Some(json!({"type": "turn_progress", "turn_id": "turn-1"})),
        );

        assert_eq!(payload["type"], "subscription_ack");
        assert!(payload.get("subscription").is_none());
        assert_eq!(payload["reason"], "subscribed");
        assert_eq!(payload["current_message_id"], "msg_3");
        assert!(payload.get("next_message_id").is_none());
        assert_eq!(payload["next_message_index"], 3);
        assert_eq!(payload["session_id"], "web-main-test-ws-ack.foreground");
        assert_eq!(payload["turn_progress"]["turn_id"], "turn-1");

        let empty = websocket_subscription_ack(
            &state,
            &ConversationMessageSummary::default(),
            "subscribed",
            None,
        );
        assert!(empty["current_message_id"].is_null());
        assert!(empty.get("next_message_id").is_none());
        assert_eq!(empty["next_message_index"], 0);
        assert!(empty["turn_progress"].is_null());
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

        let page = MessagePage {
            start: 0,
            end: 1,
            total: 1,
            messages: vec![message],
        };
        let payload = message_page_payload(&state, &context, &page, 0, 50);
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
    fn web_message_rendering_exposes_reasoning_summary_items() {
        let workdir = test_workdir("reasoning-summary");
        let state = test_state("web-main-test-reasoning-summary");
        let message = ChatMessage::new(
            ChatRole::Assistant,
            vec![ChatMessageItem::Reasoning(
                stellaclaw_core::session_actor::ReasoningItem::codex_from_summary_text(
                    Some("checked current patch".to_string()),
                    Some("encrypted-state".to_string()),
                    Some("raw reasoning".to_string()),
                ),
            )],
        );

        let context = test_attachment_context(&workdir, &state);
        let roots = context.roots();
        let rendered = render_web_message(&message, &context, &roots);

        assert_eq!(rendered.text, "");
        assert_eq!(rendered.items.len(), 1);
        assert!(matches!(
            &rendered.items[0],
            WebMessageItem::Reasoning { text, summary, .. }
                if text == "checked current patch"
                    && summary.as_deref() == Some("checked current patch")
        ));
        let skeleton = message_skeleton(3, &message, &context, &roots);
        assert_eq!(skeleton.data.len(), 1);
        assert!(matches!(
            &skeleton.data[0],
            ChatMessageItem::Reasoning(reasoning)
                if reasoning.codex_summary_text().as_deref() == Some("checked current patch")
                    && reasoning.codex_encrypted_content.is_none()
        ));

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
                    tool_name: "shell_exec".to_string(),
                    result: stellaclaw_core::session_actor::ToolResultContent::from_text(
                        "downloaded".to_string(),
                    )
                    .with_file(FileItem {
                        uri: format!("file://{}", file_path.display()),
                        name: Some("result.txt".to_string()),
                        media_type: Some("text/plain".to_string()),
                        width: None,
                        height: None,
                        state: None,
                    }),
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
                file_attachment_indices,
                ..
            } if tool_call_id == "call_1"
                && tool_name == "shell_exec"
                && context == "downloaded"
                && context_with_attachment_markers == "downloaded"
                && file_attachment_indices == &vec![0]
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

    fn test_attachment_context(
        workdir: &Path,
        state: &ConversationMetadata,
    ) -> WebAttachmentContext {
        WebAttachmentContext::new(workdir, state, Arc::new(CacheManager::new(workdir)))
    }

    fn write_test_image(path: &Path) {
        let image = image::RgbImage::from_pixel(800, 600, image::Rgb([80, 120, 200]));
        image.save(path).expect("write test image");
    }
}
