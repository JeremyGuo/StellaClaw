use std::{
    collections::HashMap,
    fs,
    io::{Read, Write},
    net::{TcpListener, TcpStream},
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    thread,
    time::{SystemTime, UNIX_EPOCH},
};

use anyhow::{anyhow, Context, Result};
use crossbeam_channel::Sender;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use stellaclaw_core::session_actor::{ChatMessage, ChatRole, FileItem};
use time::{format_description::well_known::Rfc3339, OffsetDateTime};

use crate::{
    config::{ModelSelection, StellaclawConfig},
    conversation::{
        load_conversation_status_snapshot, load_or_create_conversation_state,
        persist_conversation_state, render_chat_message, ConversationControl, ConversationState,
        IncomingConversationMessage,
    },
    conversation_id_manager::ConversationIdManager,
    logger::StellaclawLogger,
};

use super::{
    types::{
        IncomingDispatch, OutgoingDelivery, OutgoingProgressFeedback, OutgoingStatus,
        ProcessingState,
    },
    Channel,
};

const MAX_HEADER_BYTES: usize = 64 * 1024;
const MAX_BODY_BYTES: usize = 2 * 1024 * 1024;

pub struct WebChannel {
    id: String,
    bind_addr: String,
    token: String,
    workdir: PathBuf,
    config: Arc<StellaclawConfig>,
    logger: Arc<StellaclawLogger>,
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
        Self {
            id,
            bind_addr,
            token,
            workdir,
            config,
            logger,
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
            ("GET", ["conversations"]) => self.list_conversations(&request.query),
            ("POST", ["conversations"]) => self.create_conversation(&request.body, id_manager),
            ("GET", ["conversations", conversation_id, "messages"]) => {
                self.list_messages(conversation_id, &request.query)
            }
            ("GET", ["conversations", conversation_id, "messages", "after", message_id]) => {
                self.list_messages_after(conversation_id, message_id, &request.query)
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
            _ => Err(ApiError::new(404, "not_found")),
        }
    }

    fn list_conversations(&self, query: &HashMap<String, String>) -> ApiResult<HttpResponse> {
        let offset = query_usize(query, "offset", 0);
        let limit = query_usize(query, "limit", 50).min(200);
        let mut conversations = Vec::new();
        let root = self.workdir.join("conversations");
        if root.exists() {
            for entry in fs::read_dir(&root).map_err(ApiError::internal)? {
                let entry = entry.map_err(ApiError::internal)?;
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
                    conversations.push(ConversationSummary::from_state(&state, &self.config));
                }
            }
        }
        conversations.sort_by(|left, right| left.conversation_id.cmp(&right.conversation_id));
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
            if !model_config.supports(stellaclaw_core::model_config::ModelCapability::Chat) {
                return Err(ApiError::new(
                    400,
                    format!("model {model} is not chat-capable"),
                ));
            }
            state.session_profile.main_model = ModelSelection::alias(model);
            state.model_selection_pending = false;
        }
        persist_conversation_state(&self.workdir, &state).map_err(ApiError::internal)?;

        Ok(json_response(
            201,
            json!({
                "conversation_id": conversation_id,
                "channel_id": self.id,
                "platform_chat_id": platform_chat_id,
                "model_selection_pending": state.model_selection_pending,
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
        let messages = self.load_messages(conversation_id)?;
        Ok(json_response(
            200,
            message_page_payload(conversation_id, &messages, offset, limit),
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
        let messages = self.load_messages(conversation_id)?;
        Ok(json_response(
            200,
            message_page_payload(conversation_id, &messages, index.saturating_add(1), limit),
        ))
    }

    fn message_detail(&self, conversation_id: &str, message_id: &str) -> ApiResult<HttpResponse> {
        let index = parse_message_id(message_id)?;
        let messages = self.load_messages(conversation_id)?;
        let Some(message) = messages.get(index) else {
            return Err(ApiError::new(404, "message_not_found"));
        };
        Ok(json_response(
            200,
            json!({
                "conversation_id": conversation_id,
                "id": index.to_string(),
                "index": index,
                "message": message,
                "rendered_text": render_chat_message(message),
            }),
        ))
    }

    fn conversation_status(&self, conversation_id: &str) -> ApiResult<HttpResponse> {
        self.load_web_state(conversation_id)?;
        let status =
            load_conversation_status_snapshot(&self.workdir, &self.config, conversation_id)
                .map_err(ApiError::internal)?;
        Ok(json_response(200, json!(status)))
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

    fn load_messages(&self, conversation_id: &str) -> ApiResult<Vec<ChatMessage>> {
        let state = self.load_web_state(conversation_id)?;
        let path = self
            .workdir
            .join("conversations")
            .join(conversation_id)
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
        Ok(())
    }

    fn set_processing(&self, platform_chat_id: &str, state: ProcessingState) -> Result<()> {
        self.logger.info(
            "web_processing",
            json!({
                "channel_id": self.id,
                "platform_chat_id": platform_chat_id,
                "state": format!("{state:?}"),
            }),
        );
        Ok(())
    }

    fn update_progress_feedback(&self, feedback: &OutgoingProgressFeedback) -> Result<()> {
        self.logger.info(
            "web_progress",
            json!({
                "channel_id": feedback.channel_id,
                "platform_chat_id": feedback.platform_chat_id,
                "turn_id": feedback.turn_id,
                "final_state": feedback.final_state.map(|state| format!("{state:?}")),
            }),
        );
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

#[derive(Debug, Default, Deserialize)]
struct CreateConversationRequest {
    platform_chat_id: Option<String>,
    model: Option<String>,
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
struct MessageSkeleton {
    id: String,
    index: usize,
    role: ChatRole,
    preview: String,
    user_name: Option<String>,
    message_time: Option<String>,
    has_token_usage: bool,
}

#[derive(Debug, Serialize)]
struct ConversationSummary {
    conversation_id: String,
    platform_chat_id: String,
    model: String,
    model_selection_pending: bool,
    foreground_session_id: String,
    total_background: usize,
    total_subagents: usize,
}

impl ConversationSummary {
    fn from_state(state: &ConversationState, config: &StellaclawConfig) -> Self {
        Self {
            conversation_id: state.conversation_id.clone(),
            platform_chat_id: state.platform_chat_id.clone(),
            model: state
                .session_profile
                .main_model
                .display_name(&config.models),
            model_selection_pending: state.model_selection_pending,
            foreground_session_id: state.session_binding.foreground_session_id.clone(),
            total_background: state.session_binding.background_sessions.len(),
            total_subagents: state.session_binding.subagent_sessions.len(),
        }
    }
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
        "HTTP/1.1 {} {}\r\nContent-Type: {}\r\nContent-Length: {}\r\nAccess-Control-Allow-Origin: *\r\nAccess-Control-Allow-Headers: Authorization, Content-Type\r\nAccess-Control-Allow-Methods: GET, POST, OPTIONS\r\nConnection: close\r\n\r\n",
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
    conversation_id: &str,
    messages: &[ChatMessage],
    offset: usize,
    limit: usize,
) -> Value {
    let total = messages.len();
    let start = offset.min(total);
    let end = start.saturating_add(limit).min(total);
    json!({
        "conversation_id": conversation_id,
        "offset": offset,
        "limit": limit,
        "total": total,
        "messages": messages[start..end]
            .iter()
            .enumerate()
            .map(|(relative, message)| message_skeleton(start + relative, message))
            .collect::<Vec<_>>(),
    })
}

fn message_skeleton(index: usize, message: &ChatMessage) -> MessageSkeleton {
    MessageSkeleton {
        id: index.to_string(),
        index,
        role: message.role.clone(),
        preview: preview_text(&render_chat_message(message)),
        user_name: message.user_name.clone(),
        message_time: message.message_time.clone(),
        has_token_usage: message.token_usage.is_some(),
    }
}

fn preview_text(text: &str) -> String {
    const LIMIT: usize = 240;
    let mut preview = text.trim().chars().take(LIMIT).collect::<String>();
    if text.trim().chars().count() > LIMIT {
        preview.push_str("...");
    }
    preview
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
            parse_web_control("/remote demo-host ~/repo"),
            Some(ConversationControl::SetRemote { host, path })
                if host == "demo-host" && path == "~/repo"
        ));
        assert!(matches!(
            parse_web_control("/remote off"),
            Some(ConversationControl::DisableRemote)
        ));
    }
}
