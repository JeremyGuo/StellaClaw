use std::{
    collections::HashMap,
    fs,
    io::{BufRead, BufReader},
    net::{TcpListener, TcpStream},
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    thread,
    time::Duration,
};

use anyhow::{anyhow, Context, Result};
use crossbeam_channel::{unbounded, Receiver, RecvTimeoutError, Sender};
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};
use stellaclaw_core::session_actor::{
    ChatMessage, ChatMessageItem, ChatRole, ContextItem, FileItem, SelectionReferenceItem,
    ToolRemoteMode,
};

use crate::{
    config::{ModelSelection, SessionProfile, StellaclawConfig},
    conversation_host::ConversationHostRuntime,
    conversation_id_manager::ConversationIdManager,
    conversation_metadata::{ConversationMetadata, ConversationMetadataStore, WorkdirLayout},
    conversation_new::ConversationRuntimeConfig,
    conversation_new::{ServiceAddr, ServiceScope},
    logger::StellaclawLogger,
    service_protos::{
        agent_session::AgentMessageOrigin,
        channel::{ChannelEvent as KernelChannelEvent, ChannelIngress},
        kernel::KernelRuntimeConfigPatch,
    },
};

use super::{
    http::{
        parse_json, parse_optional_json, query_usize, read_http_request, split_path,
        write_response, HttpError, HttpRequest, HttpResponse, HttpResult,
    },
    time_utils::{generated_platform_id, generated_request_id, now_rfc3339},
    websocket::{accept_websocket, send_websocket_json, websocket_event_loop},
};
use crate::channels::{
    Channel, IncomingDispatch, OutgoingDelivery, OutgoingError, OutgoingMessageAppended,
    OutgoingSessionStream, ProcessingState,
};

const SEEN_STATE_FILE: &str = "seen_state.json";

pub struct WebChannel {
    pub(super) id: String,
    pub(super) bind_addr: String,
    pub(super) token: String,
    pub(super) workdir: PathBuf,
    pub(super) config: Arc<StellaclawConfig>,
    pub(super) conversation_runtime: Arc<ConversationHostRuntime>,
    pub(super) websocket_subscribers: Arc<Mutex<HashMap<String, Vec<Sender<Value>>>>>,
    pub(super) conversation_stream_subscribers: Arc<Mutex<Vec<Sender<Value>>>>,
    pub(super) processing_states: Arc<Mutex<HashMap<String, ProcessingState>>>,
    pub(super) seen_states: Arc<Mutex<HashMap<String, ConversationSeen>>>,
    pub(super) home_seq: Arc<Mutex<u64>>,
    pub(super) live_states: Arc<Mutex<HashMap<String, ChatLiveState>>>,
}

impl WebChannel {
    pub fn new(
        id: String,
        bind_addr: String,
        token: String,
        workdir: PathBuf,
        config: Arc<StellaclawConfig>,
        conversation_runtime: Arc<ConversationHostRuntime>,
        _logger: Arc<StellaclawLogger>,
    ) -> Self {
        let seen_states = load_seen_state(&workdir, &id).unwrap_or_default().seen;
        Self {
            id,
            bind_addr,
            token,
            workdir,
            config,
            conversation_runtime,
            websocket_subscribers: Arc::new(Mutex::new(HashMap::new())),
            conversation_stream_subscribers: Arc::new(Mutex::new(Vec::new())),
            processing_states: Arc::new(Mutex::new(HashMap::new())),
            seen_states: Arc::new(Mutex::new(seen_states)),
            home_seq: Arc::new(Mutex::new(0)),
            live_states: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    fn handle_request(
        &self,
        request: HttpRequest,
        id_manager: Arc<Mutex<ConversationIdManager>>,
    ) -> HttpResult {
        if request.method == "OPTIONS" {
            return Ok(HttpResponse::empty(204));
        }
        if request.is_websocket() {
            return Err(HttpError::upgrade_required());
        }
        if !self.authorized(&request) {
            return Err(HttpError::new(401, "unauthorized"));
        }
        let path = split_path(&request.path);
        match (request.method.as_str(), path.as_slice()) {
            ("GET", ["api", "health"]) => Ok(HttpResponse::json(200, json!({"ok": true}))),
            ("GET", ["api", "models"]) => self.list_models(),
            ("GET", ["api", "conversations"]) => self.list_conversations(&request.query),
            ("POST", ["api", "conversations"]) => self.create_conversation(&request, id_manager),
            ("PATCH", ["api", "conversations", conversation_id]) => {
                self.rename_conversation(conversation_id, &request.body)
            }
            ("DELETE", ["api", "conversations", conversation_id]) => {
                self.delete_conversation(conversation_id)
            }
            ("POST", ["api", "conversations", conversation_id, "seen"]) => {
                self.mark_seen(conversation_id, &request.body)
            }
            ("GET", ["api", "conversations", conversation_id, "foreground_sessions"]) => {
                self.list_foreground_sessions(conversation_id)
            }
            ("POST", ["api", "conversations", conversation_id, "foreground_sessions"]) => {
                self.create_foreground_session(conversation_id, &request.body)
            }
            (
                "PATCH",
                ["api", "conversations", conversation_id, "foreground_sessions", foreground_session_id],
            ) => self.rename_foreground_session(
                conversation_id,
                foreground_session_id,
                &request.body,
            ),
            (
                "DELETE",
                ["api", "conversations", conversation_id, "foreground_sessions", foreground_session_id],
            ) => self.delete_foreground_session(conversation_id, foreground_session_id),
            (
                "GET",
                ["api", "conversations", conversation_id, "foreground_sessions", foreground_session_id, "messages"],
            ) => self.list_messages(conversation_id, foreground_session_id, &request.query),
            (
                "GET",
                ["api", "conversations", conversation_id, "foreground_sessions", foreground_session_id, "messages", message_id],
            ) => self.message_detail(conversation_id, foreground_session_id, message_id),
            (
                "POST",
                ["api", "conversations", conversation_id, "foreground_sessions", foreground_session_id, "messages"],
            ) => self.post_message(conversation_id, foreground_session_id, &request.body),
            ("GET", ["api", "conversations", conversation_id, "status"]) => {
                self.status_snapshot(conversation_id)
            }
            ("GET", ["api", "conversations", conversation_id, "workspace"]) => {
                self.list_workspace(conversation_id, &request.query)
            }
            ("GET", ["api", "conversations", conversation_id, "workspace", "file"]) => {
                self.read_workspace_file(conversation_id, &request.query)
            }
            ("DELETE", ["api", "conversations", conversation_id, "workspace"]) => {
                self.delete_workspace_path(conversation_id, &request.query)
            }
            ("PATCH", ["api", "conversations", conversation_id, "workspace"]) => {
                self.move_workspace_path(conversation_id, &request.body)
            }
            ("POST", ["api", "conversations", conversation_id, "workspace", "upload"]) => {
                self.upload_workspace_archive(conversation_id, &request.query, &request.body)
            }
            ("GET", ["api", "conversations", conversation_id, "workspace", "download"]) => {
                self.download_workspace_archive(conversation_id, &request.query)
            }
            ("GET", ["api", "conversations", conversation_id, "terminals"]) => {
                self.list_terminals(conversation_id)
            }
            ("POST", ["api", "conversations", conversation_id, "terminals"]) => {
                self.create_terminal(conversation_id, &request.body)
            }
            ("GET", ["api", "conversations", conversation_id, "terminals", terminal_id]) => {
                self.get_terminal(conversation_id, terminal_id)
            }
            ("DELETE", ["api", "conversations", conversation_id, "terminals", terminal_id]) => {
                self.terminate_terminal(conversation_id, terminal_id)
            }
            _ => Err(HttpError::new(404, "not_found")),
        }
    }

    fn handle_websocket(
        self: Arc<Self>,
        mut stream: TcpStream,
        request: HttpRequest,
    ) -> Result<()> {
        if !self.authorized(&request) {
            write_response(
                &mut stream,
                &HttpResponse::json(401, json!({"error": "unauthorized"})),
            )?;
            return Ok(());
        }
        let path = split_path(&request.path);
        match path.as_slice() {
            ["api", "ws", "home"] => self.accept_home_stream(stream, &request),
            ["api", "conversations", conversation_id, "foreground_sessions", foreground_session_id, "ws"] => {
                self.accept_session_stream(stream, &request, conversation_id, foreground_session_id)
            }
            ["api", "conversations", conversation_id, "terminals", terminal_id, "ws"] => {
                self.accept_terminal_stream(stream, &request, conversation_id, terminal_id)
            }
            _ => {
                write_response(
                    &mut stream,
                    &HttpResponse::json(404, json!({"error": "not_found"})),
                )?;
                Ok(())
            }
        }
    }

    fn list_models(&self) -> HttpResult {
        let models = self
            .config
            .available_agent_models()
            .into_iter()
            .map(|(alias, model)| {
                json!({
                    "alias": alias,
                    "name": alias,
                    "provider": format!("{:?}", model.provider_type),
                    "display_name": alias,
                })
            })
            .collect::<Vec<_>>();
        Ok(HttpResponse::json(
            200,
            json!({
                "default_model": self.config.initial_main_model_name(),
                "total": models.len(),
                "models": models,
            }),
        ))
    }

    fn list_conversations(&self, query: &HashMap<String, String>) -> HttpResult {
        let offset = query_usize(query, "offset", 0);
        let limit = query_usize(query, "limit", 80).min(200);
        let mut conversations = self.conversation_summaries()?;
        conversations.sort_by(|left, right| {
            left["conversation_id"]
                .as_str()
                .unwrap_or_default()
                .cmp(right["conversation_id"].as_str().unwrap_or_default())
        });
        let total = conversations.len();
        let start = offset.min(total);
        let end = start.saturating_add(limit).min(total);
        Ok(HttpResponse::json(
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

    fn create_conversation(
        &self,
        request: &HttpRequest,
        id_manager: Arc<Mutex<ConversationIdManager>>,
    ) -> HttpResult {
        let body: CreateConversationRequest = parse_optional_json(&request.body)?;
        let platform_chat_id = body.platform_chat_id.unwrap_or_else(generated_platform_id);
        let conversation_id = id_manager
            .lock()
            .map_err(|_| HttpError::new(500, "conversation id manager lock poisoned"))?
            .get_or_create(&self.id, &platform_chat_id)
            .map_err(|error| HttpError::new(500, error))?;
        let store = ConversationMetadataStore::new(&self.workdir);
        let mut metadata = store
            .load_or_create(&conversation_id, &self.id, &platform_chat_id)
            .map_err(HttpError::internal)?;
        if let Some(nickname) = body.nickname.filter(|value| !value.trim().is_empty()) {
            metadata.nickname = nickname;
        }
        store.persist(&metadata).map_err(HttpError::internal)?;
        self.conversation_runtime
            .ensure_conversation_started(&conversation_id)
            .map_err(HttpError::internal)?;
        let summary = self.conversation_summary(&metadata)?;
        self.publish_conversation_event(json!({
            "type": "home.conversation_upserted",
            "conversation": summary,
        }));
        Ok(HttpResponse::json(
            201,
            json!({
                "conversation_id": conversation_id,
                "conversation": summary,
            }),
        ))
    }

    fn rename_conversation(&self, conversation_id: &str, body: &[u8]) -> HttpResult {
        let body: RenameRequest = parse_json(body)?;
        let store = ConversationMetadataStore::new(&self.workdir);
        let mut metadata = store
            .load(conversation_id)
            .map_err(|_| HttpError::new(404, "conversation_not_found"))?;
        metadata.nickname = body
            .nickname
            .unwrap_or_else(|| metadata.conversation_id.clone());
        store.persist(&metadata).map_err(HttpError::internal)?;
        let summary = self.conversation_summary(&metadata)?;
        self.publish_conversation_event(json!({
            "type": "home.conversation_upserted",
            "conversation": summary,
        }));
        Ok(HttpResponse::json(200, json!({"conversation": summary})))
    }

    fn delete_conversation(&self, conversation_id: &str) -> HttpResult {
        let _ = self
            .conversation_runtime
            .stop_conversation(conversation_id, "web deleted conversation");
        ConversationMetadataStore::new(&self.workdir)
            .remove(conversation_id)
            .map_err(HttpError::internal)?;
        self.publish_conversation_event(json!({
            "type": "home.conversation_deleted",
            "conversation_id": conversation_id,
        }));
        Ok(HttpResponse::json(200, json!({"deleted": true})))
    }

    fn mark_seen(&self, conversation_id: &str, body: &[u8]) -> HttpResult {
        let request: MarkSeenRequest = parse_json(body)?;
        let foreground_session_id = request
            .foreground_session_id
            .unwrap_or_else(|| "main".to_string());
        let seen = ConversationSeen {
            last_seen_message_id: request.last_seen_message_id,
            updated_at: now_rfc3339(),
        };
        let key = conversation_seen_key(conversation_id, &foreground_session_id);
        let snapshot = {
            let mut states = self
                .seen_states
                .lock()
                .map_err(|_| HttpError::new(500, "seen state lock poisoned"))?;
            states.insert(key, seen.clone());
            states.clone()
        };
        persist_seen_state(&self.workdir, &self.id, &WebSeenState { seen: snapshot })
            .map_err(HttpError::internal)?;
        self.publish_conversation_event(json!({
            "type": "home.foreground_session_seen_state_updated",
            "conversation_id": conversation_id,
            "foreground_session_id": foreground_session_id,
            "seen": seen,
        }));
        Ok(HttpResponse::json(200, json!({"seen": seen})))
    }

    fn list_foreground_sessions(&self, conversation_id: &str) -> HttpResult {
        let metadata = ConversationMetadataStore::new(&self.workdir)
            .load(conversation_id)
            .map_err(|_| HttpError::new(404, "conversation_not_found"))?;
        Ok(HttpResponse::json(
            200,
            json!({
                "conversation_id": conversation_id,
                "foreground_sessions": self.foreground_session_summaries(&metadata),
            }),
        ))
    }

    fn create_foreground_session(&self, conversation_id: &str, body: &[u8]) -> HttpResult {
        let request: CreateForegroundSessionRequest = parse_optional_json(body)?;
        self.conversation_runtime
            .ensure_conversation_started(conversation_id)
            .map_err(HttpError::internal)?;
        let rx = self
            .conversation_runtime
            .send_main_channel_ingress_subscribed(
                conversation_id,
                ChannelIngress::CreateForegroundSession {
                    requested_id: request.session_id.clone(),
                },
            )
            .map_err(HttpError::internal)?;
        let storage_id = wait_agent_session_created(&rx).unwrap_or_else(|_| {
            foreground_session_storage_id(request.session_id.as_deref().unwrap_or("main"))
        });
        let route_id =
            foreground_route_id_from_storage_id(&storage_id).unwrap_or(storage_id.clone());
        if let Some(nickname) = request.nickname {
            self.set_session_nickname(conversation_id, &route_id, Some(nickname))?;
        }
        let metadata = ConversationMetadataStore::new(&self.workdir)
            .load(conversation_id)
            .map_err(HttpError::internal)?;
        let session = self.foreground_session_summary(&metadata, &route_id);
        self.publish_conversation_event(json!({
            "type": "home.conversation_upserted",
            "conversation": self.conversation_summary(&metadata)?,
        }));
        Ok(HttpResponse::json(
            201,
            json!({
                "conversation_id": conversation_id,
                "foreground_session": session,
                "foreground_sessions": self.foreground_session_summaries(&metadata),
            }),
        ))
    }

    fn rename_foreground_session(
        &self,
        conversation_id: &str,
        foreground_session_id: &str,
        body: &[u8],
    ) -> HttpResult {
        let request: RenameRequest = parse_json(body)?;
        let metadata =
            self.set_session_nickname(conversation_id, foreground_session_id, request.nickname)?;
        let session = self.foreground_session_summary(&metadata, foreground_session_id);
        self.publish_conversation_event(json!({
            "type": "home.conversation_upserted",
            "conversation": self.conversation_summary(&metadata)?,
        }));
        Ok(HttpResponse::json(
            200,
            json!({"foreground_session": session}),
        ))
    }

    fn delete_foreground_session(
        &self,
        conversation_id: &str,
        foreground_session_id: &str,
    ) -> HttpResult {
        if foreground_session_id == "main" {
            return Err(HttpError::new(
                400,
                "main foreground session cannot be deleted",
            ));
        }
        self.conversation_runtime
            .send_main_channel_ingress(
                conversation_id,
                ChannelIngress::DeleteForegroundSession {
                    foreground_session_id: Some(foreground_session_id.to_string()),
                    reason: Some("web deleted foreground session".to_string()),
                },
            )
            .map_err(HttpError::internal)?;
        let metadata = self.set_session_nickname(conversation_id, foreground_session_id, None)?;
        self.publish_conversation_event(json!({
            "type": "home.conversation_upserted",
            "conversation": self.conversation_summary(&metadata)?,
        }));
        Ok(HttpResponse::json(
            200,
            json!({
                "conversation_id": conversation_id,
                "foreground_session_id": foreground_session_id,
                "deleted": true,
            }),
        ))
    }

    fn list_messages(
        &self,
        conversation_id: &str,
        foreground_session_id: &str,
        query: &HashMap<String, String>,
    ) -> HttpResult {
        let offset = query_usize(query, "offset", 0);
        let limit = query_usize(query, "limit", 80).min(200);
        let messages = read_messages(&message_log_path(
            &self.workdir,
            conversation_id,
            foreground_session_id,
        ))?;
        let total = messages.len();
        let start = offset.min(total);
        let end = start.saturating_add(limit).min(total);
        Ok(HttpResponse::json(
            200,
            json!({
                "conversation_id": conversation_id,
                "foreground_session_id": foreground_session_id,
                "offset": offset,
                "limit": limit,
                "total": total,
                "messages": &messages[start..end],
            }),
        ))
    }

    fn message_detail(
        &self,
        conversation_id: &str,
        foreground_session_id: &str,
        message_id: &str,
    ) -> HttpResult {
        let messages = read_messages(&message_log_path(
            &self.workdir,
            conversation_id,
            foreground_session_id,
        ))?;
        let message = messages
            .into_iter()
            .find(|message| {
                message
                    .get("message_id")
                    .or_else(|| message.get("id"))
                    .and_then(Value::as_str)
                    .is_some_and(|id| id == message_id)
                    || message
                        .get("index")
                        .and_then(Value::as_u64)
                        .is_some_and(|index| index.to_string() == message_id)
            })
            .ok_or_else(|| HttpError::new(404, "message_not_found"))?;
        Ok(HttpResponse::json(
            200,
            json!({
                "conversation_id": conversation_id,
                "foreground_session_id": foreground_session_id,
                "message": message,
            }),
        ))
    }

    fn post_message(
        &self,
        conversation_id: &str,
        foreground_session_id: &str,
        body: &[u8],
    ) -> HttpResult {
        let request: PostMessageRequest = parse_json(body)?;
        if request.selection_references.is_empty() && request.files.is_empty() {
            if let Some(text) = request.text.as_deref() {
                if let Some(ingress) =
                    self.control_ingress_from_text(text, foreground_session_id)?
                {
                    self.conversation_runtime
                        .ensure_conversation_started(conversation_id)
                        .map_err(HttpError::internal)?;
                    self.conversation_runtime
                        .send_main_channel_ingress(conversation_id, ingress)
                        .map_err(HttpError::internal)?;
                    return Ok(HttpResponse::json(
                        202,
                        json!({
                            "accepted": true,
                            "control": true,
                            "conversation_id": conversation_id,
                            "foreground_session_id": foreground_session_id,
                        }),
                    ));
                }
            }
        }
        let mut items = Vec::new();
        if let Some(text) = request.text.filter(|text| !text.trim().is_empty()) {
            items.push(ChatMessageItem::Context(ContextItem { text }));
        }
        for selection in request.selection_references {
            items.push(ChatMessageItem::SelectionReference(selection));
        }
        for file in request.files {
            items.push(ChatMessageItem::File(file));
        }
        if items.is_empty() {
            return Err(HttpError::new(400, "message body is empty"));
        }
        let client_message_id = request
            .client_message_id
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_string);
        let message = ChatMessage::new(ChatRole::User, items)
            .with_user_name_option(request.user_name)
            .with_message_time(now_rfc3339());
        let client_message_id = client_message_id.unwrap_or_else(|| message.message_id.clone());
        self.conversation_runtime
            .ensure_conversation_started(conversation_id)
            .map_err(HttpError::internal)?;
        self.conversation_runtime
            .send_main_channel_ingress(
                conversation_id,
                ChannelIngress::IncomingMessage {
                    foreground_session_id: Some(foreground_session_id.to_string()),
                    platform_message_id: Some(message.message_id.clone()),
                    origin: Some(AgentMessageOrigin::User),
                    message,
                    metadata: json!({
                        "source": "web",
                        "client_message_id": client_message_id,
                    }),
                },
            )
            .map_err(HttpError::internal)?;
        self.record_user_message_queued(conversation_id, foreground_session_id, &client_message_id);
        self.publish_foreground_session_state_event(conversation_id, foreground_session_id);
        self.publish_websocket_event(
            conversation_id,
            foreground_session_id,
            json!({
                "type": "chat.user_message_queued",
                "client_message_id": client_message_id,
                "conversation_id": conversation_id,
                "foreground_session_id": foreground_session_id,
            }),
        );
        Ok(HttpResponse::json(
            202,
            json!({
                "accepted": true,
                "conversation_id": conversation_id,
                "foreground_session_id": foreground_session_id,
                "client_message_id": client_message_id,
            }),
        ))
    }

    fn control_ingress_from_text(
        &self,
        text: &str,
        foreground_session_id: &str,
    ) -> HttpResult<Option<ChannelIngress>> {
        let Some((command, argument)) = parse_web_control_command(text) else {
            return Ok(None);
        };
        let foreground_session_id = Some(foreground_session_id.to_string());
        let ingress = match command {
            "/continue" if argument.is_empty() => ChannelIngress::ContinueForegroundTurn {
                foreground_session_id,
                reason: Some("web requested continue".to_string()),
            },
            "/cancel" if argument.is_empty() => ChannelIngress::CancelForegroundTurn {
                foreground_session_id,
                reason: Some("web requested cancel".to_string()),
            },
            "/compact" if argument.is_empty() => ChannelIngress::CompactForegroundNow {
                foreground_session_id,
            },
            "/status" if argument.is_empty() => ChannelIngress::QueryForegroundStatus {
                foreground_session_id,
            },
            "/model" if argument.is_empty() => ChannelIngress::QueryForegroundStatus {
                foreground_session_id,
            },
            "/model" => {
                if !self.config.models.contains_key(argument) {
                    return Err(HttpError::new(
                        400,
                        format!("unknown model alias {argument}"),
                    ));
                }
                ChannelIngress::UpdateRuntimeConfig {
                    patch: KernelRuntimeConfigPatch {
                        session_profile: Some(Some(SessionProfile {
                            main_model: ModelSelection::alias(argument.to_string()),
                        })),
                        ..Default::default()
                    },
                }
            }
            "/reasoning" => {
                let effort = parse_reasoning_effort_argument(argument)?;
                ChannelIngress::UpdateRuntimeConfig {
                    patch: KernelRuntimeConfigPatch {
                        reasoning_effort: Some(effort),
                        ..Default::default()
                    },
                }
            }
            "/remote" if argument.is_empty() => ChannelIngress::QueryForegroundStatus {
                foreground_session_id,
            },
            "/remote" if matches!(argument, "off" | "disable" | "disabled" | "local") => {
                ChannelIngress::UpdateRuntimeConfig {
                    patch: KernelRuntimeConfigPatch {
                        tool_remote_mode: Some(ToolRemoteMode::Selectable),
                        ..Default::default()
                    },
                }
            }
            "/remote" => {
                let mut parts = argument.split_whitespace();
                let host = parts.next().unwrap_or_default();
                let path = parts.next().unwrap_or_default();
                if host.is_empty() || path.is_empty() || parts.next().is_some() {
                    return Err(HttpError::new(400, "usage: /remote <host> <path>"));
                }
                ChannelIngress::UpdateRuntimeConfig {
                    patch: KernelRuntimeConfigPatch {
                        tool_remote_mode: Some(ToolRemoteMode::FixedSsh {
                            host: host.to_string(),
                            cwd: Some(path.to_string()),
                        }),
                        ..Default::default()
                    },
                }
            }
            "/sandbox" => {
                return Err(HttpError::new(
                    400,
                    "sandbox runtime switching is not exposed through web yet",
                ));
            }
            _ => return Ok(None),
        };
        Ok(Some(ingress))
    }

    fn status_snapshot(&self, conversation_id: &str) -> HttpResult {
        self.conversation_runtime
            .ensure_conversation_started(conversation_id)
            .map_err(HttpError::internal)?;
        let request_id = generated_request_id("status");
        let rx = self
            .conversation_runtime
            .send_main_channel_ingress_subscribed(
                conversation_id,
                ChannelIngress::Status {
                    request_id: request_id.clone(),
                    request: crate::service_protos::status::StatusRequest::Snapshot,
                },
            )
            .map_err(HttpError::internal)?;
        let response = wait_for_event(&rx, Duration::from_secs(10), |event| match event {
            KernelChannelEvent::StatusSnapshot {
                request_id: id,
                response,
            } if id == request_id => Some(serde_json::to_value(response).ok()?),
            _ => None,
        })?;
        Ok(HttpResponse::json(200, response))
    }

    fn set_session_nickname(
        &self,
        conversation_id: &str,
        foreground_session_id: &str,
        nickname: Option<String>,
    ) -> HttpResult<ConversationMetadata> {
        let store = ConversationMetadataStore::new(&self.workdir);
        let mut metadata = store.load(conversation_id).map_err(HttpError::internal)?;
        let storage_id = foreground_session_storage_id(foreground_session_id);
        match nickname {
            Some(nickname) if !nickname.trim().is_empty() => {
                metadata.session_nicknames.insert(storage_id, nickname);
            }
            _ => {
                metadata.session_nicknames.remove(&storage_id);
            }
        }
        store.persist(&metadata).map_err(HttpError::internal)?;
        Ok(metadata)
    }

    fn conversation_summaries(&self) -> HttpResult<Vec<Value>> {
        let store = ConversationMetadataStore::new(&self.workdir);
        let mut summaries = Vec::new();
        for conversation_id in self.conversation_runtime.conversation_ids() {
            let Ok(metadata) = store.load(&conversation_id) else {
                continue;
            };
            summaries.push(self.conversation_summary(&metadata)?);
        }
        Ok(summaries)
    }

    fn conversation_summary(&self, metadata: &ConversationMetadata) -> HttpResult<Value> {
        let default_session_id = default_foreground_route_id(metadata);
        let summary = message_summary(&message_log_path(
            &self.workdir,
            &metadata.conversation_id,
            &default_session_id,
        ));
        let processing_state = self
            .processing_states
            .lock()
            .ok()
            .and_then(|states| states.get(&metadata.platform_chat_id).copied())
            .unwrap_or(ProcessingState::Idle);
        let runtime_config = load_runtime_config(&self.workdir, &metadata.conversation_id).ok();
        Ok(json!({
            "conversation_id": metadata.conversation_id,
            "conversation_name": if metadata.nickname.trim().is_empty() { &metadata.conversation_id } else { &metadata.nickname },
            "nickname": if metadata.nickname.trim().is_empty() { &metadata.conversation_id } else { &metadata.nickname },
            "platform_chat_id": metadata.platform_chat_id,
            "foreground_session_id": metadata.foreground_session_id,
            "model": conversation_model_label(runtime_config.as_ref(), &self.config),
            "reasoning": runtime_config.as_ref().and_then(|config| config.reasoning_effort.as_deref()).unwrap_or("model default"),
            "sandbox": runtime_config.as_ref().and_then(|config| config.sandbox.as_ref()).map(|sandbox| format!("{:?}", sandbox.mode)).unwrap_or_else(|| "default".to_string()),
            "remote": runtime_config.as_ref().map(|config| format!("{:?}", config.tool_remote_mode)).unwrap_or_else(|| "selectable".to_string()),
            "workspace": WorkdirLayout::new(&self.workdir).conversation_root(&metadata.conversation_id).display().to_string(),
            "processing_state": processing_state_name(processing_state),
            "running": processing_state != ProcessingState::Idle,
            "message_count": summary.message_count,
            "last_message_id": summary.last_message_id.clone(),
            "last_message_time": summary.last_message_time.clone(),
            "last_committed_message_id": summary.last_message_id.clone(),
            "last_committed_message_index": summary.last_message_index,
            "updated_at": summary.last_message_time,
            "foreground_sessions": self.foreground_session_summaries(metadata),
        }))
    }

    fn foreground_session_summaries(&self, metadata: &ConversationMetadata) -> Vec<Value> {
        let mut ids = metadata
            .session_nicknames
            .keys()
            .filter_map(|storage| foreground_route_id_from_storage_id(storage))
            .collect::<Vec<_>>();
        let default_id = default_foreground_route_id(metadata);
        if !ids.iter().any(|id| id == &default_id) {
            ids.push(default_id);
        }
        ids.sort();
        ids.dedup();
        ids.into_iter()
            .map(|id| self.foreground_session_summary(metadata, &id))
            .collect()
    }

    fn foreground_session_summary(
        &self,
        metadata: &ConversationMetadata,
        foreground_session_id: &str,
    ) -> Value {
        let storage_id = foreground_session_storage_id(foreground_session_id);
        let summary = message_summary(&message_log_path(
            &self.workdir,
            &metadata.conversation_id,
            foreground_session_id,
        ));
        let seen = self.seen_states.lock().ok().and_then(|states| {
            states
                .get(&conversation_seen_key(
                    &metadata.conversation_id,
                    foreground_session_id,
                ))
                .cloned()
        });
        let live = self.chat_live_snapshot(&metadata.conversation_id, foreground_session_id);
        json!({
            "id": foreground_session_id,
            "foreground_session_id": foreground_session_id,
            "session_id": storage_id,
            "nickname": metadata.session_nicknames.get(&storage_id).cloned().unwrap_or_else(|| {
                if foreground_session_id == "main" {
                    if metadata.nickname.trim().is_empty() { metadata.conversation_id.clone() } else { metadata.nickname.clone() }
                } else {
                    foreground_session_id.to_string()
                }
            }),
            "session_name": metadata.session_nicknames.get(&storage_id).cloned().unwrap_or_else(|| {
                if foreground_session_id == "main" {
                    if metadata.nickname.trim().is_empty() { metadata.conversation_id.clone() } else { metadata.nickname.clone() }
                } else {
                    foreground_session_id.to_string()
                }
            }),
            "state": live.summary_state(),
            "active_turn_id": live.active_turn_id(),
            "is_main": foreground_session_id == "main",
            "message_count": summary.message_count,
            "last_message_id": summary.last_message_id.clone(),
            "last_message_time": summary.last_message_time.clone(),
            "last_committed_message_id": summary.last_message_id.clone(),
            "last_committed_message_index": summary.last_message_index,
            "last_activity_at": summary.last_message_time,
            "last_seen_message_id": seen.as_ref().map(|seen| seen.last_seen_message_id.clone()),
            "last_seen_at": seen.map(|seen| seen.updated_at),
        })
    }

    fn accept_home_stream(&self, mut stream: TcpStream, request: &HttpRequest) -> Result<()> {
        accept_websocket(&mut stream, request)?;
        let (tx, rx) = unbounded();
        self.conversation_stream_subscribers
            .lock()
            .map_err(|_| anyhow!("conversation stream subscriber lock poisoned"))?
            .push(tx);
        send_websocket_json(
            &mut stream,
            &json!({
                "type": "home.snapshot",
                "seq": self.current_home_seq(),
                "conversations": self.conversation_summaries().unwrap_or_default(),
                "server_time": now_rfc3339(),
            }),
        )?;
        websocket_event_loop(stream, rx, "home.heartbeat")
    }

    fn accept_session_stream(
        &self,
        mut stream: TcpStream,
        request: &HttpRequest,
        conversation_id: &str,
        foreground_session_id: &str,
    ) -> Result<()> {
        accept_websocket(&mut stream, request)?;
        self.conversation_runtime
            .ensure_conversation_started(conversation_id)?;
        let (tx, rx) = unbounded();
        self.websocket_subscribers
            .lock()
            .map_err(|_| anyhow!("websocket subscriber lock poisoned"))?
            .entry(websocket_key(conversation_id, foreground_session_id))
            .or_default()
            .push(tx);
        let messages = read_messages(&message_log_path(
            &self.workdir,
            conversation_id,
            foreground_session_id,
        ))
        .unwrap_or_default();
        let live = self.chat_live_snapshot(conversation_id, foreground_session_id);
        send_websocket_json(
            &mut stream,
            &json!({
                "type": "chat.snapshot",
                "conversation_id": conversation_id,
                "foreground_session_id": foreground_session_id,
                "total": messages.len(),
                "next_message_index": messages.len(),
                "last_committed_message_id": messages.last().and_then(|message| {
                    message.get("message_id").or_else(|| message.get("id")).and_then(Value::as_str)
                }),
                "last_committed_message_index": messages.last().and_then(|message| message.get("index")).and_then(Value::as_u64),
                "current_turn_state": live.current_turn_state,
                "current_provisional_assistant_message": live.current_provisional_assistant_message,
                "running_tool_results": live.running_tool_results,
                "queued_outbound_messages": live.queued_outbound_messages,
            }),
        )?;
        websocket_event_loop(stream, rx, "chat.heartbeat")
    }

    fn publish_websocket_event(&self, conversation_id: &str, session_id: &str, payload: Value) {
        let key = websocket_key(
            conversation_id,
            &foreground_route_id_from_storage_id(session_id)
                .unwrap_or_else(|| session_id.to_string()),
        );
        let Ok(mut subscribers) = self.websocket_subscribers.lock() else {
            return;
        };
        if let Some(list) = subscribers.get_mut(&key) {
            list.retain(|sender| sender.send(payload.clone()).is_ok());
        }
    }

    fn publish_conversation_event(&self, mut payload: Value) {
        let seq = self.next_home_seq();
        if let Value::Object(map) = &mut payload {
            map.insert("seq".to_string(), json!(seq));
        }
        let Ok(mut subscribers) = self.conversation_stream_subscribers.lock() else {
            return;
        };
        subscribers.retain(|sender| sender.send(payload.clone()).is_ok());
    }

    fn publish_foreground_session_state_event(
        &self,
        conversation_id: &str,
        foreground_session_id: &str,
    ) {
        let live = self.chat_live_snapshot(conversation_id, foreground_session_id);
        self.publish_conversation_event(json!({
            "type": "home.foreground_session_state_updated",
            "conversation_id": conversation_id,
            "foreground_session_id": foreground_session_id,
            "state": live.summary_state(),
            "active_turn_id": live.active_turn_id(),
            "last_error": live.last_error,
        }));
    }

    fn current_home_seq(&self) -> u64 {
        self.home_seq.lock().map(|seq| *seq).unwrap_or(0)
    }

    fn next_home_seq(&self) -> u64 {
        let Ok(mut seq) = self.home_seq.lock() else {
            return 0;
        };
        *seq = seq.saturating_add(1);
        *seq
    }

    fn chat_live_snapshot(
        &self,
        conversation_id: &str,
        foreground_session_id: &str,
    ) -> ChatLiveState {
        self.live_states
            .lock()
            .ok()
            .and_then(|states| {
                states
                    .get(&websocket_key(conversation_id, foreground_session_id))
                    .cloned()
            })
            .unwrap_or_default()
    }

    fn record_user_message_queued(
        &self,
        conversation_id: &str,
        foreground_session_id: &str,
        client_message_id: &str,
    ) {
        let Ok(mut states) = self.live_states.lock() else {
            return;
        };
        let state = states
            .entry(websocket_key(conversation_id, foreground_session_id))
            .or_default();
        if state.queued_outbound_messages.iter().any(|message| {
            message
                .get("client_message_id")
                .and_then(Value::as_str)
                .is_some_and(|id| id == client_message_id)
        }) {
            return;
        }
        state.queued_outbound_messages.push(json!({
            "client_message_id": client_message_id,
            "conversation_id": conversation_id,
            "foreground_session_id": foreground_session_id,
        }));
        state.last_error = None;
    }

    fn record_message_appended(&self, appended: &OutgoingMessageAppended, message: &Value) {
        let foreground_session_id = foreground_route_id_from_storage_id(&appended.session_id)
            .unwrap_or_else(|| appended.session_id.clone());
        let Ok(mut states) = self.live_states.lock() else {
            return;
        };
        let state = states
            .entry(websocket_key(
                &appended.conversation_id,
                &foreground_session_id,
            ))
            .or_default();
        state.last_committed_message_id = message
            .get("message_id")
            .or_else(|| message.get("id"))
            .and_then(Value::as_str)
            .map(str::to_string);
        state.last_committed_message_index = Some(appended.index);
        if let Some(message_id) = state.last_committed_message_id.as_deref() {
            state.queued_outbound_messages.retain(|queued| {
                match queued.get("client_message_id").and_then(Value::as_str) {
                    Some(id) => id != message_id,
                    None => true,
                }
            });
            if state
                .current_provisional_assistant_message
                .as_ref()
                .and_then(|provisional| provisional.get("message_id"))
                .and_then(Value::as_str)
                .is_some_and(|id| id == message_id)
            {
                state.current_provisional_assistant_message = None;
            }
        }
        if appended.message.role == ChatRole::User {
            if let Some(index) = state.queued_outbound_messages.iter().position(|queued| {
                queued
                    .get("conversation_id")
                    .and_then(Value::as_str)
                    .is_some_and(|id| id == appended.conversation_id)
            }) {
                state.queued_outbound_messages.remove(index);
            }
        }
        for tool_call_id in tool_result_call_ids(message) {
            for tool_state in &mut state.running_tool_results {
                if tool_state
                    .get("tool_result")
                    .and_then(|tool_result| tool_result.get("tool_call_id"))
                    .and_then(Value::as_str)
                    .is_some_and(|id| id == tool_call_id)
                {
                    if let Value::Object(map) = tool_state {
                        map.insert("committed".to_string(), json!(true));
                    }
                }
            }
        }
    }

    fn record_session_stream(&self, stream: &OutgoingSessionStream, event_type: &str) {
        let foreground_session_id = foreground_route_id_from_storage_id(&stream.session_id)
            .unwrap_or_else(|| stream.session_id.clone());
        let Ok(mut states) = self.live_states.lock() else {
            return;
        };
        let state = states
            .entry(websocket_key(
                &stream.conversation_id,
                &foreground_session_id,
            ))
            .or_default();
        match event_type {
            "turn_started" => {
                state.last_error = None;
                state.current_turn_state =
                    stream
                        .event
                        .get("turn_id")
                        .and_then(Value::as_str)
                        .map(|turn_id| {
                            json!({
                                "turn_id": turn_id,
                                "message_id": Value::Null,
                            })
                        });
                state.current_provisional_assistant_message = None;
                state.running_tool_results.clear();
            }
            "stream_assistant_message_delta" => {
                state.apply_assistant_delta(&stream.event);
            }
            "stream_tool_call_delta" => {
                state.apply_tool_call_delta(&stream.event);
            }
            "stream_reasoning_summary_part_added" => {
                state.apply_reasoning_summary_part(&stream.event);
            }
            "stream_reasoning_summary_delta" => {
                state.apply_reasoning_summary_delta(&stream.event);
            }
            "stream_tool_result_done" => {
                state.apply_tool_result_done(&stream.event);
            }
            "stream_error" => {
                let message_id = stream
                    .event
                    .get("message_id")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                if message_id.is_empty() {
                    state.current_provisional_assistant_message = None;
                } else {
                    state.current_provisional_assistant_message = state
                        .current_provisional_assistant_message
                        .take()
                        .filter(|message| {
                            !message
                                .get("message_id")
                                .and_then(Value::as_str)
                                .is_some_and(|id| id == message_id)
                        });
                }
                state.current_turn_state = None;
                state.running_tool_results.clear();
                state.last_error = stream
                    .event
                    .get("error")
                    .and_then(Value::as_str)
                    .map(str::to_string);
            }
            "turn_completed" => {
                state.current_turn_state = None;
                state.current_provisional_assistant_message = None;
                state.running_tool_results.clear();
                state.queued_outbound_messages.clear();
                state.last_error = None;
            }
            _ => {}
        }
    }

    fn authorized(&self, request: &HttpRequest) -> bool {
        request.headers.get("authorization").is_some_and(|value| {
            value == &self.token
                || value
                    .strip_prefix("Bearer ")
                    .is_some_and(|token| token == self.token)
        }) || request
            .query
            .get("token")
            .is_some_and(|token| token == &self.token)
    }
}

impl Channel for WebChannel {
    fn id(&self) -> &str {
        &self.id
    }

    fn send_delivery(&self, delivery: &OutgoingDelivery) -> Result<()> {
        if delivery.message.is_none() && !delivery.text.trim().is_empty() {
            self.publish_websocket_event(
                &delivery.conversation_id,
                delivery.session_id.as_deref().unwrap_or("main"),
                json!({
                    "type": "delivery",
                    "text": delivery.text,
                    "conversation_id": delivery.conversation_id,
                    "session_id": delivery.session_id,
                }),
            );
        }
        Ok(())
    }

    fn message_appended(&self, appended: &OutgoingMessageAppended) -> Result<()> {
        let message = decorate_message(&appended.message, appended.index);
        self.record_message_appended(appended, &message);
        let foreground_session_id = foreground_route_id_from_storage_id(&appended.session_id)
            .unwrap_or_else(|| appended.session_id.clone());
        self.publish_websocket_event(
            &appended.conversation_id,
            &appended.session_id,
            json!({
                "type": "chat.message_appended",
                "conversation_id": appended.conversation_id,
                "session_id": appended.session_id,
                "foreground_session_id": foreground_session_id,
                "message_index": appended.index,
                "message_id": appended.message.message_id,
                "message": message,
            }),
        );
        self.publish_foreground_session_state_event(
            &appended.conversation_id,
            &foreground_session_id,
        );
        if let Ok(metadata) =
            ConversationMetadataStore::new(&self.workdir).load(&appended.conversation_id)
        {
            if let Ok(summary) = self.conversation_summary(&metadata) {
                self.publish_conversation_event(json!({
                    "type": "home.conversation_upserted",
                    "conversation": summary,
                }));
            }
        }
        Ok(())
    }

    fn session_stream(&self, stream: &OutgoingSessionStream) -> Result<()> {
        let event_type = stream
            .event
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or("stream_event");
        self.record_session_stream(stream, event_type);
        if matches!(
            event_type,
            "turn_started" | "turn_completed" | "stream_error"
        ) {
            self.publish_foreground_session_state_event(
                &stream.conversation_id,
                &foreground_route_id_from_storage_id(&stream.session_id)
                    .unwrap_or_else(|| stream.session_id.clone()),
            );
        }
        let public_type = public_chat_stream_type(event_type);
        self.publish_websocket_event(
            &stream.conversation_id,
            &stream.session_id,
            json!({
                "type": public_type,
                "conversation_id": stream.conversation_id,
                "session_id": stream.session_id,
                "event": stream.event,
            }),
        );
        Ok(())
    }

    fn set_processing(&self, platform_chat_id: &str, state: ProcessingState) -> Result<()> {
        if let Ok(mut states) = self.processing_states.lock() {
            states.insert(platform_chat_id.to_string(), state);
        }
        Ok(())
    }

    fn send_error(&self, error: &OutgoingError) -> Result<()> {
        self.publish_websocket_event(
            &error.conversation_id,
            "main",
            json!({
                "type": "error",
                "code": error.code,
                "message": error.message,
                "detail": error.detail,
                "can_continue": error.can_continue,
            }),
        );
        Ok(())
    }

    fn spawn_ingress(
        self: Arc<Self>,
        _dispatch_tx: Sender<IncomingDispatch>,
        id_manager: Arc<Mutex<ConversationIdManager>>,
        logger: Arc<StellaclawLogger>,
    ) where
        Self: Sized,
    {
        thread::spawn(move || {
            let listener = match TcpListener::bind(&self.bind_addr) {
                Ok(listener) => listener,
                Err(error) => {
                    logger.error(
                        "web_channel_bind_failed",
                        json!({"channel_id": self.id, "bind_addr": self.bind_addr, "error": error.to_string()}),
                    );
                    return;
                }
            };
            logger.info(
                "web_channel_listening",
                json!({"channel_id": self.id, "bind_addr": self.bind_addr}),
            );
            for incoming in listener.incoming() {
                let Ok(stream) = incoming else {
                    continue;
                };
                let channel = self.clone();
                let id_manager = id_manager.clone();
                thread::spawn(move || {
                    if let Err(error) = handle_connection(channel, stream, id_manager) {
                        eprintln!("stellaclaw web request failed: {error:#}");
                    }
                });
            }
        });
    }
}

fn handle_connection(
    channel: Arc<WebChannel>,
    mut stream: TcpStream,
    id_manager: Arc<Mutex<ConversationIdManager>>,
) -> Result<()> {
    let request = read_http_request(&mut stream)?;
    if request.is_websocket() {
        return channel.handle_websocket(stream, request);
    }
    let response = match channel.handle_request(request, id_manager) {
        Ok(response) => response,
        Err(error) => error.into_response(),
    };
    write_response(&mut stream, &response)
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct WebSeenState {
    #[serde(default)]
    seen: HashMap<String, ConversationSeen>,
}

#[derive(Debug, Clone, Default)]
pub(super) struct ChatLiveState {
    current_turn_state: Option<Value>,
    current_provisional_assistant_message: Option<Value>,
    running_tool_results: Vec<Value>,
    queued_outbound_messages: Vec<Value>,
    last_committed_message_id: Option<String>,
    last_committed_message_index: Option<usize>,
    last_error: Option<String>,
}

impl ChatLiveState {
    fn summary_state(&self) -> &'static str {
        if self.current_turn_state.is_some() {
            "running"
        } else if !self.queued_outbound_messages.is_empty() {
            "queued"
        } else if self.last_error.is_some() {
            "failed"
        } else {
            "idle"
        }
    }

    fn active_turn_id(&self) -> Option<String> {
        self.current_turn_state
            .as_ref()
            .and_then(|turn| turn.get("turn_id"))
            .and_then(Value::as_str)
            .map(str::to_string)
    }

    fn set_turn_from_event(&mut self, event: &Value) {
        let Some(turn_id) = event.get("turn_id").and_then(Value::as_str) else {
            return;
        };
        self.current_turn_state = Some(json!({
            "turn_id": turn_id,
            "message_id": event.get("message_id").and_then(Value::as_str),
        }));
    }

    fn ensure_provisional_message(
        &mut self,
        message_id: &str,
        turn_id: &str,
        message_index: Option<u64>,
    ) -> Option<&mut Map<String, Value>> {
        let needs_new = self
            .current_provisional_assistant_message
            .as_ref()
            .and_then(|provisional| provisional.get("message_id"))
            .and_then(Value::as_str)
            .is_none_or(|id| id != message_id);
        if needs_new {
            self.current_provisional_assistant_message = Some(json!({
                "turn_id": turn_id,
                "message_id": message_id,
                "message": {
                    "id": message_id,
                    "message_id": message_id,
                    "index": message_index,
                    "role": "assistant",
                    "text": "",
                    "preview": "",
                    "content": "",
                    "text_with_attachment_markers": "",
                    "items": [],
                    "attachments": [],
                    "attachment_count": 0,
                    "message_time": now_rfc3339(),
                    "_streamTurnId": turn_id,
                    "_streaming": true,
                },
            }));
        }
        let provisional = self
            .current_provisional_assistant_message
            .as_mut()?
            .as_object_mut()?;
        provisional.insert("turn_id".to_string(), json!(turn_id));
        provisional.insert("message_id".to_string(), json!(message_id));
        let message = provisional.get_mut("message")?.as_object_mut()?;
        message.insert("id".to_string(), json!(message_id));
        message.insert("message_id".to_string(), json!(message_id));
        message.insert("role".to_string(), json!("assistant"));
        message.insert("_streamTurnId".to_string(), json!(turn_id));
        message.insert("_streaming".to_string(), json!(true));
        if let Some(message_index) = message_index {
            message.insert("index".to_string(), json!(message_index));
        }
        if !message.get("items").is_some_and(Value::is_array) {
            message.insert("items".to_string(), json!([]));
        }
        Some(message)
    }

    fn message_items_mut(message: &mut Map<String, Value>) -> Option<&mut Vec<Value>> {
        if !message.get("items").is_some_and(Value::is_array) {
            message.insert("items".to_string(), json!([]));
        }
        message.get_mut("items").and_then(Value::as_array_mut)
    }

    fn apply_assistant_delta(&mut self, event: &Value) {
        self.set_turn_from_event(event);
        let Some(message_id) = event.get("message_id").and_then(Value::as_str) else {
            return;
        };
        let delta = event
            .get("delta")
            .and_then(Value::as_str)
            .unwrap_or_default();
        if delta.is_empty() {
            return;
        }
        let turn_id = event
            .get("turn_id")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let message_index = event.get("message_index").and_then(Value::as_u64);
        let Some(message) = self.ensure_provisional_message(message_id, turn_id, message_index)
        else {
            return;
        };
        let existing_text = message
            .get("text")
            .or_else(|| message.get("preview"))
            .and_then(Value::as_str)
            .unwrap_or_default();
        let text = append_text_delta(existing_text, delta);
        message.insert("text".to_string(), json!(text));
        message.insert("preview".to_string(), json!(text));
        message.insert("content".to_string(), json!(text));
        message.insert("text_with_attachment_markers".to_string(), json!(text));
        if let Some(items) = Self::message_items_mut(message) {
            if let Some(item) = items
                .iter_mut()
                .find(|item| item.get("type").and_then(Value::as_str) == Some("text"))
            {
                if let Value::Object(map) = item {
                    map.insert("text".to_string(), json!(text));
                    map.insert("text_with_attachment_markers".to_string(), json!(text));
                }
            } else {
                items.push(json!({
                    "type": "text",
                    "index": items.len(),
                    "text": text,
                    "text_with_attachment_markers": text,
                }));
            }
        }
    }

    fn apply_tool_call_delta(&mut self, event: &Value) {
        self.set_turn_from_event(event);
        let Some(message_id) = event.get("message_id").and_then(Value::as_str) else {
            return;
        };
        let item_id = event
            .get("item_id")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let call_id = event
            .get("call_id")
            .and_then(Value::as_str)
            .filter(|value| !value.is_empty())
            .unwrap_or(item_id);
        if call_id.is_empty() {
            return;
        }
        let delta = event
            .get("delta")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let turn_id = event
            .get("turn_id")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let message_index = event.get("message_index").and_then(Value::as_u64);
        let Some(message) = self.ensure_provisional_message(message_id, turn_id, message_index)
        else {
            return;
        };
        if let Some(items) = Self::message_items_mut(message) {
            let next_index = items.len();
            if let Some(item) = items.iter_mut().find(|item| {
                item.get("type").and_then(Value::as_str) == Some("tool_call")
                    && item
                        .get("tool_call_id")
                        .and_then(Value::as_str)
                        .is_some_and(|id| id == call_id)
            }) {
                if let Value::Object(map) = item {
                    let existing = map
                        .get("arguments")
                        .and_then(Value::as_str)
                        .unwrap_or_default();
                    map.insert(
                        "arguments".to_string(),
                        json!(append_text_delta(existing, delta)),
                    );
                }
            } else {
                items.push(json!({
                    "type": "tool_call",
                    "index": next_index,
                    "tool_call_id": call_id,
                    "tool_name": item_id_if_readable(item_id).unwrap_or("tool"),
                    "arguments": delta,
                }));
            }
        }
    }

    fn apply_reasoning_summary_part(&mut self, event: &Value) {
        self.set_turn_from_event(event);
    }

    fn apply_reasoning_summary_delta(&mut self, event: &Value) {
        self.set_turn_from_event(event);
        let Some(message_id) = event.get("message_id").and_then(Value::as_str) else {
            return;
        };
        let turn_id = event
            .get("turn_id")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let summary_index = event
            .get("summary_index")
            .and_then(Value::as_i64)
            .unwrap_or_default();
        let delta = event
            .get("delta")
            .and_then(Value::as_str)
            .unwrap_or_default();
        if delta.is_empty() {
            return;
        }
        let message_index = event.get("message_index").and_then(Value::as_u64);
        let Some(message) = self.ensure_provisional_message(message_id, turn_id, message_index)
        else {
            return;
        };
        let Some(items) = Self::message_items_mut(message) else {
            return;
        };
        if !items.iter().any(|item| {
            item.get("type").and_then(Value::as_str) == Some("reasoning")
                && item
                    .get("_summaryIndex")
                    .and_then(Value::as_i64)
                    .is_some_and(|index| index == summary_index)
        }) {
            items.push(json!({
                "type": "reasoning",
                "index": items.len(),
                "text": "",
                "summary": "",
                "_summaryIndex": summary_index,
            }));
        }
        let Some(message) = self
            .current_provisional_assistant_message
            .as_mut()
            .filter(|provisional| {
                provisional
                    .get("message_id")
                    .and_then(Value::as_str)
                    .is_some_and(|id| id == message_id)
            })
            .and_then(|provisional| provisional.get_mut("message"))
            .and_then(Value::as_object_mut)
        else {
            return;
        };
        if let Some(items) = Self::message_items_mut(message) {
            if let Some(item) = items.iter_mut().find(|item| {
                item.get("type").and_then(Value::as_str) == Some("reasoning")
                    && item
                        .get("_summaryIndex")
                        .and_then(Value::as_i64)
                        .is_some_and(|index| index == summary_index)
            }) {
                if let Value::Object(map) = item {
                    let existing = map
                        .get("text")
                        .or_else(|| map.get("summary"))
                        .and_then(Value::as_str)
                        .unwrap_or_default();
                    let text = append_text_delta(existing, delta);
                    map.insert("text".to_string(), json!(text));
                    map.insert("summary".to_string(), json!(text));
                }
            }
        }
    }

    fn apply_tool_result_done(&mut self, event: &Value) {
        let Some(tool_result) = event.get("tool_result").cloned() else {
            return;
        };
        let tool_call_id = tool_result
            .get("tool_call_id")
            .and_then(Value::as_str)
            .map(str::to_string);
        let turn_id = event
            .get("turn_id")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let next = json!({
            "turn_id": turn_id,
            "tool_result": tool_result,
            "committed": false,
        });
        if let Some(tool_call_id) = tool_call_id {
            if let Some(existing) = self.running_tool_results.iter_mut().find(|item| {
                item.get("tool_result")
                    .and_then(|tool_result| tool_result.get("tool_call_id"))
                    .and_then(Value::as_str)
                    .is_some_and(|id| id == tool_call_id)
            }) {
                *existing = next;
                return;
            }
        }
        self.running_tool_results.push(next);
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub(super) struct ConversationSeen {
    last_seen_message_id: String,
    updated_at: String,
}

#[derive(Debug, Deserialize, Default)]
struct CreateConversationRequest {
    nickname: Option<String>,
    platform_chat_id: Option<String>,
}

#[derive(Debug, Deserialize, Default)]
struct CreateForegroundSessionRequest {
    session_id: Option<String>,
    nickname: Option<String>,
}

#[derive(Debug, Deserialize)]
struct RenameRequest {
    nickname: Option<String>,
}

#[derive(Debug, Deserialize)]
struct MarkSeenRequest {
    last_seen_message_id: String,
    foreground_session_id: Option<String>,
}

#[derive(Debug, Deserialize, Default)]
struct PostMessageRequest {
    client_message_id: Option<String>,
    text: Option<String>,
    user_name: Option<String>,
    #[serde(default)]
    selection_references: Vec<SelectionReferenceItem>,
    #[serde(default)]
    files: Vec<FileItem>,
}

#[derive(Debug, Deserialize)]
pub(super) struct MoveWorkspacePathRequest {
    pub(super) path: String,
    pub(super) new_path: String,
}

#[derive(Debug, Default)]
struct MessageSummary {
    message_count: usize,
    last_message_id: Option<String>,
    last_message_index: Option<usize>,
    last_message_time: Option<String>,
}

fn read_messages(path: &Path) -> HttpResult<Vec<Value>> {
    if !path.exists() {
        return Ok(Vec::new());
    }
    let file = fs::File::open(path).map_err(HttpError::internal)?;
    let reader = BufReader::new(file);
    let mut messages = Vec::new();
    for (index, line) in reader.lines().enumerate() {
        let line = line.map_err(HttpError::internal)?;
        if line.trim().is_empty() {
            continue;
        }
        let message: ChatMessage = serde_json::from_str(&line).map_err(HttpError::internal)?;
        messages.push(decorate_message(&message, index));
    }
    Ok(messages)
}

fn decorate_message(message: &ChatMessage, index: usize) -> Value {
    let mut value = serde_json::to_value(message).unwrap_or_else(|_| json!({}));
    if let Value::Object(map) = &mut value {
        map.insert("index".to_string(), json!(index));
        if !message.message_id.is_empty() {
            map.insert("id".to_string(), json!(message.message_id));
        }
    }
    value
}

fn message_summary(path: &Path) -> MessageSummary {
    let Ok(messages) = read_messages(path) else {
        return MessageSummary::default();
    };
    let mut summary = MessageSummary {
        message_count: messages.len(),
        ..MessageSummary::default()
    };
    if let Some(last) = messages.last() {
        summary.last_message_index = last
            .get("index")
            .and_then(Value::as_u64)
            .and_then(|value| usize::try_from(value).ok());
        summary.last_message_id = last
            .get("message_id")
            .or_else(|| last.get("id"))
            .and_then(Value::as_str)
            .map(str::to_string);
        summary.last_message_time = last
            .get("message_time")
            .and_then(Value::as_str)
            .map(str::to_string);
    }
    summary
}

fn tool_result_call_ids(message: &Value) -> Vec<&str> {
    let mut ids = Vec::new();
    if let Some(items) = message
        .get("items")
        .or_else(|| message.get("data"))
        .and_then(Value::as_array)
    {
        for item in items {
            if item.get("type").and_then(Value::as_str) == Some("tool_result") {
                if let Some(id) = item.get("tool_call_id").and_then(Value::as_str) {
                    ids.push(id);
                }
            }
        }
    }
    ids
}

fn append_text_delta(existing_text: &str, delta: &str) -> String {
    if existing_text.is_empty() {
        return delta.to_string();
    }
    if delta.is_empty() || existing_text.ends_with(delta) {
        return existing_text.to_string();
    }
    if delta.starts_with(existing_text) {
        return delta.to_string();
    }
    let max_overlap = existing_text.len().min(delta.len());
    for length in (1..=max_overlap).rev() {
        if existing_text.is_char_boundary(existing_text.len() - length)
            && delta.is_char_boundary(length)
            && existing_text[existing_text.len() - length..] == delta[..length]
        {
            return format!("{}{}", existing_text, &delta[length..]);
        }
    }
    format!("{existing_text}{delta}")
}

fn item_id_if_readable(item_id: &str) -> Option<&str> {
    let trimmed = item_id.trim();
    if trimmed.is_empty()
        || trimmed.starts_with("item_")
        || trimmed.starts_with("fc_")
        || trimmed.starts_with("call_")
    {
        return None;
    }
    Some(trimmed)
}

fn wait_agent_session_created(rx: &Receiver<KernelChannelEvent>) -> HttpResult<String> {
    wait_for_event(rx, Duration::from_secs(10), |event| match event {
        KernelChannelEvent::AgentSessionCreated { addr } => {
            Some(service_addr_storage_component(&addr))
        }
        _ => None,
    })
}

pub(super) fn wait_for_event<T>(
    rx: &Receiver<KernelChannelEvent>,
    timeout: Duration,
    mut matcher: impl FnMut(KernelChannelEvent) -> Option<T>,
) -> HttpResult<T> {
    let deadline = std::time::Instant::now() + timeout;
    loop {
        let now = std::time::Instant::now();
        if now >= deadline {
            return Err(HttpError::new(504, "request timed out"));
        }
        match rx.recv_timeout(deadline.saturating_duration_since(now)) {
            Ok(event) => {
                if let Some(value) = matcher(event) {
                    return Ok(value);
                }
            }
            Err(RecvTimeoutError::Timeout) => return Err(HttpError::new(504, "request timed out")),
            Err(RecvTimeoutError::Disconnected) => {
                return Err(HttpError::new(503, "conversation event stream closed"));
            }
        }
    }
}

fn load_seen_state(workdir: &Path, channel_id: &str) -> Result<WebSeenState> {
    let path = seen_state_path(workdir, channel_id);
    if !path.exists() {
        return Ok(WebSeenState::default());
    }
    let raw =
        fs::read_to_string(&path).with_context(|| format!("failed to read {}", path.display()))?;
    serde_json::from_str(&raw).with_context(|| format!("failed to parse {}", path.display()))
}

fn persist_seen_state(workdir: &Path, channel_id: &str, state: &WebSeenState) -> Result<()> {
    let path = seen_state_path(workdir, channel_id);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }
    fs::write(&path, serde_json::to_string_pretty(state)?)
        .with_context(|| format!("failed to write {}", path.display()))
}

fn seen_state_path(workdir: &Path, channel_id: &str) -> PathBuf {
    workdir
        .join(".stellaclaw")
        .join("web")
        .join(channel_id)
        .join(SEEN_STATE_FILE)
}

fn load_runtime_config(workdir: &Path, conversation_id: &str) -> Result<ConversationRuntimeConfig> {
    let path = WorkdirLayout::new(workdir)
        .conversation_service_root(conversation_id)
        .join("runtime_config.json");
    let raw =
        fs::read_to_string(&path).with_context(|| format!("failed to read {}", path.display()))?;
    serde_json::from_str(&raw).with_context(|| format!("failed to parse {}", path.display()))
}

fn conversation_model_label(
    runtime_config: Option<&ConversationRuntimeConfig>,
    config: &StellaclawConfig,
) -> String {
    runtime_config
        .and_then(|runtime_config| runtime_config.session_profile.as_ref())
        .or(config.default_profile.as_ref())
        .map(|profile| profile.main_model.display_name(&config.models))
        .or_else(|| config.initial_main_model_name())
        .unwrap_or_else(|| "unconfigured".to_string())
}

fn service_addr_storage_component(addr: &ServiceAddr) -> String {
    let scope = match &addr.scope {
        ServiceScope::Local => "local".to_string(),
        ServiceScope::Conversation(conversation_id) => format!("conversation_{conversation_id}"),
    };
    format!("{scope}__{}", addr.path.join("__"))
}

fn message_log_path(workdir: &Path, conversation_id: &str, foreground_session_id: &str) -> PathBuf {
    WorkdirLayout::new(workdir)
        .conversation_root(conversation_id)
        .join(".stellaclaw")
        .join("log")
        .join(sanitize_session_id_for_log_path(
            &foreground_session_storage_id(foreground_session_id),
        ))
        .join("all_messages.jsonl")
}

fn parse_web_control_command(text: &str) -> Option<(&str, &str)> {
    let trimmed = text.trim();
    if !trimmed.starts_with('/') {
        return None;
    }
    let mut parts = trimmed.splitn(2, char::is_whitespace);
    let command = parts.next()?.split('@').next()?.trim();
    let argument = parts.next().unwrap_or_default().trim();
    Some((command, argument))
}

fn parse_reasoning_effort_argument(argument: &str) -> HttpResult<Option<String>> {
    match argument.trim().to_ascii_lowercase().as_str() {
        "" | "show" => Ok(None),
        "default" | "model" | "model_default" | "model-default" | "global" => Ok(None),
        "minimal" | "low" | "medium" | "high" | "xhigh" => {
            Ok(Some(argument.trim().to_ascii_lowercase()))
        }
        other => Err(HttpError::new(
            400,
            format!("unknown reasoning effort {other}"),
        )),
    }
}

fn foreground_session_storage_id(foreground_session_id: &str) -> String {
    if foreground_session_id.starts_with("local__agent__foreground__") {
        foreground_session_id.to_string()
    } else {
        format!("local__agent__foreground__{foreground_session_id}")
    }
}

fn foreground_route_id_from_storage_id(storage_id: &str) -> Option<String> {
    storage_id
        .strip_prefix("local__agent__foreground__")
        .map(str::to_string)
}

fn default_foreground_route_id(metadata: &ConversationMetadata) -> String {
    foreground_route_id_from_storage_id(&metadata.foreground_session_id)
        .unwrap_or_else(|| "main".to_string())
}

fn conversation_seen_key(conversation_id: &str, foreground_session_id: &str) -> String {
    format!(
        "{conversation_id}:{}",
        foreground_session_storage_id(foreground_session_id)
    )
}

fn websocket_key(conversation_id: &str, foreground_session_id: &str) -> String {
    format!("{conversation_id}:{foreground_session_id}")
}

fn public_chat_stream_type(event_type: &str) -> String {
    let suffix = match event_type {
        "turn_started" => "stream_turn_start",
        "turn_completed" => "stream_turn_done",
        "plan_updated" => "plan_updated",
        other => other,
    };
    format!("chat.{suffix}")
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

fn processing_state_name(state: ProcessingState) -> &'static str {
    match state {
        ProcessingState::Idle => "idle",
        ProcessingState::Typing => "typing",
    }
}
