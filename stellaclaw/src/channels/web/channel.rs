use std::{
    collections::HashMap,
    fs,
    net::{TcpListener, TcpStream},
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    thread,
    time::Duration,
};

use anyhow::{Context, Result};
use crossbeam_channel::{Receiver, RecvTimeoutError, Sender};
use serde::Deserialize;
use serde_json::{json, Value};
use stellaclaw_core::session_actor::{
    ChatMessage, ChatMessageItem, ChatRole, ContextItem, FileItem, SelectionReferenceItem,
};

use crate::{
    config::StellaclawConfig,
    conversation_host::ConversationHostRuntime,
    conversation_id_manager::ConversationIdManager,
    conversation_metadata::{ConversationMetadata, ConversationMetadataStore, WorkdirLayout},
    conversation_new::ConversationRuntimeConfig,
    logger::StellaclawLogger,
    service_protos::{
        agent_session::AgentMessageOrigin,
        channel::{ChannelEvent as KernelChannelEvent, ChannelIngress},
    },
};

use super::{
    control::control_ingress_from_text,
    history::{decorate_message, MessageSummary},
    http::{
        parse_json, parse_optional_json, query_usize, read_http_request, split_path,
        write_response, HttpError, HttpRequest, HttpResponse, HttpResult,
    },
    ids::{
        default_foreground_route_id, foreground_route_id_from_storage_id,
        foreground_session_storage_id, processing_state_name, service_addr_storage_component,
    },
    main::{load_seen_state, ChatLiveState, ConversationSeen, WebChannelMainHandle},
    time_utils::{generated_platform_id, generated_request_id, now_rfc3339},
    websocket::{accept_websocket, send_websocket_json, websocket_event_loop},
};
use crate::channels::{
    Channel, IncomingDispatch, OutgoingDelivery, OutgoingError, OutgoingMessageAppended,
    OutgoingSessionStream, ProcessingState,
};

pub struct WebChannel {
    pub(super) id: String,
    pub(super) bind_addr: String,
    pub(super) token: String,
    pub(super) workdir: PathBuf,
    pub(super) config: Arc<StellaclawConfig>,
    pub(super) conversation_runtime: Arc<ConversationHostRuntime>,
    main: WebChannelMainHandle,
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
        let main = WebChannelMainHandle::start(
            id.clone(),
            workdir.clone(),
            load_seen_state(&workdir, &id).unwrap_or_default(),
        );
        Self {
            id,
            bind_addr,
            token,
            workdir,
            config,
            conversation_runtime,
            main,
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
        self.main
            .update_seen(conversation_id, &foreground_session_id, seen.clone())
            .map_err(HttpError::internal)?;
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
        self.conversation_runtime
            .ensure_conversation_started(conversation_id)
            .map_err(HttpError::internal)?;
        let request_id = generated_request_id("messages");
        let rx = self
            .conversation_runtime
            .send_main_channel_ingress_subscribed(
                conversation_id,
                ChannelIngress::QueryMessageHistory {
                    foreground_session_id: Some(foreground_session_id.to_string()),
                    request_id: request_id.clone(),
                    offset,
                    limit,
                },
            )
            .map_err(HttpError::internal)?;
        let history = wait_for_event(&rx, Duration::from_secs(10), |event| match event {
            KernelChannelEvent::MessageHistory { history } if history.request_id == request_id => {
                Some(history)
            }
            _ => None,
        })?;
        let messages = history
            .messages
            .iter()
            .map(|record| decorate_message(&record.message, record.index))
            .collect::<Vec<_>>();
        Ok(HttpResponse::json(
            200,
            json!({
                "conversation_id": conversation_id,
                "foreground_session_id": foreground_session_id,
                "offset": history.offset,
                "limit": history.limit,
                "total": history.total,
                "messages": messages,
            }),
        ))
    }

    fn message_detail(
        &self,
        conversation_id: &str,
        foreground_session_id: &str,
        message_id: &str,
    ) -> HttpResult {
        self.conversation_runtime
            .ensure_conversation_started(conversation_id)
            .map_err(HttpError::internal)?;
        let request_id = generated_request_id("message-detail");
        let rx = self
            .conversation_runtime
            .send_main_channel_ingress_subscribed(
                conversation_id,
                ChannelIngress::QueryMessageDetail {
                    foreground_session_id: Some(foreground_session_id.to_string()),
                    request_id: request_id.clone(),
                    message_id: message_id.to_string(),
                },
            )
            .map_err(HttpError::internal)?;
        let record = wait_for_event(&rx, Duration::from_secs(10), |event| match event {
            KernelChannelEvent::MessageDetail {
                request_id: id,
                record,
            } if id == request_id => Some(record),
            _ => None,
        })?
        .ok_or_else(|| HttpError::new(404, "message_not_found"))?;
        let message = decorate_message(&record.message, record.index);
        Ok(HttpResponse::json(
            200,
            json!({
                "conversation_id": conversation_id,
                "foreground_session_id": foreground_session_id,
                "message": message,
            }),
        ))
    }

    fn query_message_summary(
        &self,
        conversation_id: &str,
        foreground_session_id: &str,
    ) -> HttpResult<MessageSummary> {
        self.conversation_runtime
            .ensure_conversation_started(conversation_id)
            .map_err(HttpError::internal)?;
        let request_id = generated_request_id("message-summary");
        let rx = self
            .conversation_runtime
            .send_main_channel_ingress_subscribed(
                conversation_id,
                ChannelIngress::QueryMessageHistory {
                    foreground_session_id: Some(foreground_session_id.to_string()),
                    request_id: request_id.clone(),
                    offset: 0,
                    limit: 0,
                },
            )
            .map_err(HttpError::internal)?;
        let history = wait_for_event(&rx, Duration::from_secs(10), |event| match event {
            KernelChannelEvent::MessageHistory { history } if history.request_id == request_id => {
                Some(history)
            }
            _ => None,
        })?;
        Ok(MessageSummary {
            message_count: history.total,
            last_message_id: history
                .last_message
                .as_ref()
                .map(|record| record.message.message_id.clone())
                .filter(|id| !id.is_empty()),
            last_message_index: history.last_message.as_ref().map(|record| record.index),
            last_message_time: history
                .last_message
                .and_then(|record| record.message.message_time),
        })
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
                    control_ingress_from_text(&self.config, text, foreground_session_id)?
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
        let summary = self
            .query_message_summary(&metadata.conversation_id, &default_session_id)
            .unwrap_or_default();
        let processing_state = self.main.processing_state(&metadata.platform_chat_id);
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
        let summary = self
            .query_message_summary(&metadata.conversation_id, foreground_session_id)
            .unwrap_or_default();
        let seen = self
            .main
            .seen_state(&metadata.conversation_id, foreground_session_id);
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
        let (rx, seq) = self.main.subscribe_home()?;
        send_websocket_json(
            &mut stream,
            &json!({
                "type": "home.snapshot",
                "seq": seq,
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
        let (rx, live) = self
            .main
            .subscribe_chat(conversation_id, foreground_session_id)?;
        let summary = self
            .query_message_summary(conversation_id, foreground_session_id)
            .unwrap_or_default();
        send_websocket_json(
            &mut stream,
            &json!({
                "type": "chat.snapshot",
                "conversation_id": conversation_id,
                "foreground_session_id": foreground_session_id,
                "total": summary.message_count,
                "next_message_index": summary.message_count,
                "last_committed_message_id": summary.last_message_id,
                "last_committed_message_index": summary.last_message_index,
                "current_turn_state": live.current_turn_state,
                "current_provisional_assistant_message": live.current_provisional_assistant_message,
                "running_tool_results": live.running_tool_results,
                "queued_outbound_messages": live.queued_outbound_messages,
            }),
        )?;
        websocket_event_loop(stream, rx, "chat.heartbeat")
    }

    fn publish_conversation_event(&self, payload: Value) {
        self.main.publish_home(payload);
    }

    fn chat_live_snapshot(
        &self,
        conversation_id: &str,
        foreground_session_id: &str,
    ) -> ChatLiveState {
        self.main.live_state(conversation_id, foreground_session_id)
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
        let _ = delivery;
        Ok(())
    }

    fn message_appended(&self, appended: &OutgoingMessageAppended) -> Result<()> {
        let message = decorate_message(&appended.message, appended.index);
        let conversation_summary = ConversationMetadataStore::new(&self.workdir)
            .load(&appended.conversation_id)
            .ok()
            .and_then(|metadata| self.conversation_summary(&metadata).ok());
        self.main
            .message_appended(appended.clone(), message, conversation_summary);
        Ok(())
    }

    fn session_stream(&self, stream: &OutgoingSessionStream) -> Result<()> {
        self.main.session_stream(stream.clone());
        Ok(())
    }

    fn set_processing(&self, platform_chat_id: &str, state: ProcessingState) -> Result<()> {
        self.main.set_processing(platform_chat_id, state);
        Ok(())
    }

    fn send_error(&self, error: &OutgoingError) -> Result<()> {
        self.main.send_error(error.clone());
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
