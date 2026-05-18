use std::{
    collections::{HashMap, HashSet},
    path::{Path, PathBuf},
    thread,
    time::Duration,
};

use anyhow::{Context, Result};
use crossbeam_channel::{bounded, unbounded, Receiver, Sender};
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};
use stellaclaw_core::session_actor::{ChatMessage, ChatMessageItem, ChatRole};

use crate::channels::{
    OutgoingError, OutgoingMessageAppended, OutgoingSessionStream, ProcessingState,
};

use super::{
    ids::{foreground_route_id_from_storage_id, foreground_session_storage_id},
    protocol,
    time_utils::now_rfc3339,
};

const SEEN_STATE_FILE: &str = "seen_state.json";

#[derive(Clone)]
pub(super) struct WebChannelMainHandle {
    tx: Sender<WebMainCommand>,
}

impl WebChannelMainHandle {
    pub(super) fn start(channel_id: String, workdir: PathBuf, seen: WebSeenState) -> Self {
        let (tx, rx) = unbounded();
        thread::spawn(move || {
            let mut state = WebChannelMain::new(channel_id, workdir, seen);
            state.run(rx);
        });
        Self { tx }
    }

    pub(super) fn subscribe_home(&self) -> Result<(Receiver<Value>, u64)> {
        let (reply_tx, reply_rx) = bounded(1);
        self.tx
            .send(WebMainCommand::SubscribeHome { reply_tx })
            .context("failed to subscribe home websocket")?;
        reply_rx.recv().context("home websocket subscribe failed")
    }

    pub(super) fn subscribe_chat(
        &self,
        conversation_id: &str,
        foreground_session_id: &str,
    ) -> Result<(Receiver<Value>, ChatLiveState)> {
        let (reply_tx, reply_rx) = bounded(1);
        self.tx
            .send(WebMainCommand::SubscribeChat {
                key: WebSessionKey::new(conversation_id, foreground_session_id),
                reply_tx,
            })
            .context("failed to subscribe chat websocket")?;
        reply_rx.recv().context("chat websocket subscribe failed")
    }

    pub(super) fn live_state(
        &self,
        conversation_id: &str,
        foreground_session_id: &str,
    ) -> ChatLiveState {
        let (reply_tx, reply_rx) = bounded(1);
        if self
            .tx
            .send(WebMainCommand::GetLiveState {
                key: WebSessionKey::new(conversation_id, foreground_session_id),
                reply_tx,
            })
            .is_err()
        {
            return ChatLiveState::default();
        }
        reply_rx
            .recv_timeout(Duration::from_secs(2))
            .unwrap_or_default()
    }

    pub(super) fn seen_state(
        &self,
        conversation_id: &str,
        foreground_session_id: &str,
    ) -> Option<ConversationSeen> {
        let (reply_tx, reply_rx) = bounded(1);
        if self
            .tx
            .send(WebMainCommand::GetSeenState {
                key: conversation_seen_key(conversation_id, foreground_session_id),
                reply_tx,
            })
            .is_err()
        {
            return None;
        }
        reply_rx.recv_timeout(Duration::from_secs(2)).ok().flatten()
    }

    pub(super) fn processing_state(&self, platform_chat_id: &str) -> ProcessingState {
        let (reply_tx, reply_rx) = bounded(1);
        if self
            .tx
            .send(WebMainCommand::GetProcessing {
                platform_chat_id: platform_chat_id.to_string(),
                reply_tx,
            })
            .is_err()
        {
            return ProcessingState::Idle;
        }
        reply_rx
            .recv_timeout(Duration::from_secs(2))
            .unwrap_or(ProcessingState::Idle)
    }

    pub(super) fn publish_home(&self, payload: Value) {
        let _ = self.tx.send(WebMainCommand::PublishHome { payload });
    }

    pub(super) fn message_appended(
        &self,
        appended: OutgoingMessageAppended,
        decorated_message: Value,
        conversation_summary: Option<Value>,
    ) {
        let _ = self.tx.send(WebMainCommand::MessageAppended {
            appended,
            decorated_message,
            conversation_summary,
        });
    }

    pub(super) fn session_stream(&self, stream: OutgoingSessionStream) {
        let _ = self.tx.send(WebMainCommand::SessionStream { stream });
    }

    pub(super) fn set_processing(&self, platform_chat_id: &str, state: ProcessingState) {
        let _ = self.tx.send(WebMainCommand::SetProcessing {
            platform_chat_id: platform_chat_id.to_string(),
            state,
        });
    }

    pub(super) fn update_seen(
        &self,
        conversation_id: &str,
        foreground_session_id: &str,
        seen: ConversationSeen,
    ) -> Result<()> {
        let (reply_tx, reply_rx) = bounded(1);
        self.tx
            .send(WebMainCommand::UpdateSeen {
                key: conversation_seen_key(conversation_id, foreground_session_id),
                conversation_id: conversation_id.to_string(),
                foreground_session_id: foreground_session_id.to_string(),
                seen,
                reply_tx,
            })
            .context("failed to update seen state")?;
        reply_rx.recv().context("seen state update failed")?
    }

    pub(super) fn send_error(&self, error: OutgoingError) {
        let _ = self.tx.send(WebMainCommand::SendError { error });
    }
}

enum WebMainCommand {
    SubscribeHome {
        reply_tx: Sender<(Receiver<Value>, u64)>,
    },
    SubscribeChat {
        key: WebSessionKey,
        reply_tx: Sender<(Receiver<Value>, ChatLiveState)>,
    },
    GetLiveState {
        key: WebSessionKey,
        reply_tx: Sender<ChatLiveState>,
    },
    GetSeenState {
        key: String,
        reply_tx: Sender<Option<ConversationSeen>>,
    },
    GetProcessing {
        platform_chat_id: String,
        reply_tx: Sender<ProcessingState>,
    },
    PublishHome {
        payload: Value,
    },
    MessageAppended {
        appended: OutgoingMessageAppended,
        decorated_message: Value,
        conversation_summary: Option<Value>,
    },
    SessionStream {
        stream: OutgoingSessionStream,
    },
    SetProcessing {
        platform_chat_id: String,
        state: ProcessingState,
    },
    UpdateSeen {
        key: String,
        conversation_id: String,
        foreground_session_id: String,
        seen: ConversationSeen,
        reply_tx: Sender<Result<()>>,
    },
    SendError {
        error: OutgoingError,
    },
}

struct WebChannelMain {
    channel_id: String,
    workdir: PathBuf,
    home_seq: u64,
    home_subscribers: Vec<Sender<Value>>,
    chat_subscribers: HashMap<WebSessionKey, Vec<Sender<Value>>>,
    live_states: HashMap<WebSessionKey, ChatLiveState>,
    seen_states: HashMap<String, ConversationSeen>,
    processing_states: HashMap<String, ProcessingState>,
}

impl WebChannelMain {
    fn new(channel_id: String, workdir: PathBuf, seen: WebSeenState) -> Self {
        Self {
            channel_id,
            workdir,
            home_seq: 0,
            home_subscribers: Vec::new(),
            chat_subscribers: HashMap::new(),
            live_states: HashMap::new(),
            seen_states: seen.seen,
            processing_states: HashMap::new(),
        }
    }

    fn run(&mut self, rx: Receiver<WebMainCommand>) {
        while let Ok(command) = rx.recv() {
            self.handle(command);
        }
    }

    fn handle(&mut self, command: WebMainCommand) {
        match command {
            WebMainCommand::SubscribeHome { reply_tx } => {
                let (tx, rx) = unbounded();
                self.home_subscribers.push(tx);
                let _ = reply_tx.send((rx, self.home_seq));
            }
            WebMainCommand::SubscribeChat { key, reply_tx } => {
                let (tx, rx) = unbounded();
                self.chat_subscribers
                    .entry(key.clone())
                    .or_default()
                    .push(tx);
                let state = self.live_states.get(&key).cloned().unwrap_or_default();
                let _ = reply_tx.send((rx, state));
            }
            WebMainCommand::GetLiveState { key, reply_tx } => {
                let _ = reply_tx.send(self.live_states.get(&key).cloned().unwrap_or_default());
            }
            WebMainCommand::GetSeenState { key, reply_tx } => {
                let _ = reply_tx.send(self.seen_states.get(&key).cloned());
            }
            WebMainCommand::GetProcessing {
                platform_chat_id,
                reply_tx,
            } => {
                let _ = reply_tx.send(
                    self.processing_states
                        .get(&platform_chat_id)
                        .copied()
                        .unwrap_or(ProcessingState::Idle),
                );
            }
            WebMainCommand::PublishHome { payload } => {
                self.publish_home(payload);
            }
            WebMainCommand::MessageAppended {
                appended,
                decorated_message,
                conversation_summary,
            } => self.message_appended(appended, decorated_message, conversation_summary),
            WebMainCommand::SessionStream { stream } => self.session_stream(stream),
            WebMainCommand::SetProcessing {
                platform_chat_id,
                state,
            } => {
                self.processing_states.insert(platform_chat_id, state);
            }
            WebMainCommand::UpdateSeen {
                key,
                conversation_id,
                foreground_session_id,
                seen,
                reply_tx,
            } => {
                self.seen_states.insert(key, seen.clone());
                let result = persist_seen_state(
                    &self.workdir,
                    &self.channel_id,
                    &WebSeenState {
                        seen: self.seen_states.clone(),
                    },
                );
                if result.is_ok() {
                    self.publish_home(protocol::home_foreground_session_seen_state_updated(
                        &conversation_id,
                        &foreground_session_id,
                        &seen.last_seen_message_id,
                        &seen.updated_at,
                    ));
                }
                let _ = reply_tx.send(result);
            }
            WebMainCommand::SendError { error } => {
                self.publish_chat(
                    &WebSessionKey::new(&error.conversation_id, "main"),
                    protocol::chat_stream_error(
                        &error.conversation_id,
                        "main",
                        "",
                        "",
                        "",
                        0,
                        &error.code,
                        &error.message,
                    ),
                );
            }
        }
    }

    fn publish_home(&mut self, mut payload: Value) {
        self.home_seq = self.home_seq.saturating_add(1);
        if let Value::Object(map) = &mut payload {
            map.insert("seq".to_string(), json!(self.home_seq));
        }
        self.home_subscribers
            .retain(|sender| sender.send(payload.clone()).is_ok());
    }

    fn publish_chat(&mut self, key: &WebSessionKey, payload: Value) {
        if let Some(list) = self.chat_subscribers.get_mut(key) {
            list.retain(|sender| sender.send(payload.clone()).is_ok());
        }
    }

    fn publish_foreground_session_state_event(&mut self, key: &WebSessionKey) {
        let live = self.live_states.get(key).cloned().unwrap_or_default();
        self.publish_home(protocol::home_foreground_session_state_updated(
            &key.conversation_id,
            &key.foreground_session_id,
            live.summary_state(),
            live.active_turn_id(),
            live.last_error,
        ));
    }

    fn message_appended(
        &mut self,
        appended: OutgoingMessageAppended,
        decorated_message: Value,
        conversation_summary: Option<Value>,
    ) {
        let key =
            WebSessionKey::from_storage_session_id(&appended.conversation_id, &appended.session_id);
        let consistency_error = {
            let state = self.live_states.entry(key.clone()).or_default();
            state.commit_consistency_error(&appended.message)
        };
        if let Some(error) = consistency_error {
            self.publish_chat(
                &key,
                protocol::chat_stream_error(
                    &appended.conversation_id,
                    &key.foreground_session_id,
                    &appended.message.message_id,
                    &appended.message.message_id,
                    error.turn_id.as_deref().unwrap_or_default(),
                    error.expected_index.unwrap_or_default(),
                    "stream_commit_mismatch",
                    &error.reason,
                ),
            );
        }
        {
            let state = self.live_states.entry(key.clone()).or_default();
            state.record_message_appended(&appended, &decorated_message);
        }
        self.publish_chat(
            &key,
            protocol::chat_message_appended(
                &appended.conversation_id,
                &key.foreground_session_id,
                appended.index,
                &appended.message.message_id,
                decorated_message,
            ),
        );
        self.publish_foreground_session_state_event(&key);
        if let Some(conversation) = conversation_summary {
            self.publish_home(protocol::home_conversation_upserted(conversation));
        }
    }

    fn session_stream(&mut self, stream: OutgoingSessionStream) {
        let key =
            WebSessionKey::from_storage_session_id(&stream.conversation_id, &stream.session_id);
        let event_type = stream
            .event
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or("stream_event")
            .to_string();
        if let Some(error) = {
            let state = self.live_states.entry(key.clone()).or_default();
            state.validate_stream_event(&stream.event, &event_type)
        } {
            self.publish_chat(
                &key,
                protocol::chat_stream_error(
                    &stream.conversation_id,
                    &key.foreground_session_id,
                    error.message_id.as_deref().unwrap_or_default(),
                    error.message_id.as_deref().unwrap_or_default(),
                    error.turn_id.as_deref().unwrap_or_default(),
                    error.received_index.unwrap_or_default(),
                    "non_contiguous_stream_event",
                    &error.reason,
                ),
            );
            self.publish_foreground_session_state_event(&key);
            return;
        }
        {
            let state = self.live_states.entry(key.clone()).or_default();
            state.record_session_stream(&stream.event, &event_type);
        }
        if matches!(
            event_type.as_str(),
            "user_message_queued"
                | "user_message_started"
                | "user_message_committed"
                | "turn_started"
                | "turn_completed"
                | "stream_error"
        ) {
            self.publish_foreground_session_state_event(&key);
        }
        let payload = protocol::public_chat_stream_payload(
            &stream.conversation_id,
            &key.foreground_session_id,
            &event_type,
            &stream.event,
        );
        self.publish_chat(&key, payload);
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(super) struct WebSessionKey {
    conversation_id: String,
    foreground_session_id: String,
}

impl WebSessionKey {
    fn new(conversation_id: &str, foreground_session_id: &str) -> Self {
        Self {
            conversation_id: conversation_id.to_string(),
            foreground_session_id: foreground_session_id.to_string(),
        }
    }

    fn from_storage_session_id(conversation_id: &str, session_id: &str) -> Self {
        Self::new(
            conversation_id,
            &foreground_route_id_from_storage_id(session_id)
                .unwrap_or_else(|| session_id.to_string()),
        )
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub(super) struct WebSeenState {
    #[serde(default)]
    pub(super) seen: HashMap<String, ConversationSeen>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub(super) struct ConversationSeen {
    pub(super) last_seen_message_id: String,
    pub(super) updated_at: String,
}

#[derive(Debug, Clone, Default)]
pub(super) struct ChatLiveState {
    pub(super) current_turn_state: Option<Value>,
    pub(super) current_provisional_assistant_message: Option<Value>,
    pub(super) running_tool_results: Vec<Value>,
    pub(super) queued_outbound_messages: Vec<Value>,
    pub(super) last_committed_message_id: Option<String>,
    pub(super) last_committed_message_index: Option<usize>,
    pub(super) last_error: Option<String>,
    stream_next_indices: HashMap<String, u64>,
    invalid_stream_messages: HashSet<String>,
}

struct StreamValidationError {
    message_id: Option<String>,
    turn_id: Option<String>,
    received_index: Option<u64>,
    reason: String,
}

struct StreamConsistencyError {
    turn_id: Option<String>,
    expected_index: Option<u64>,
    reason: String,
}

impl ChatLiveState {
    pub(super) fn summary_state(&self) -> &'static str {
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

    pub(super) fn active_turn_id(&self) -> Option<String> {
        self.current_turn_state
            .as_ref()
            .and_then(|turn| turn.get("turn_id"))
            .and_then(Value::as_str)
            .map(str::to_string)
    }

    fn validate_stream_event(
        &mut self,
        event: &Value,
        event_type: &str,
    ) -> Option<StreamValidationError> {
        if event_type == "stream_error" {
            return None;
        }
        let Some(message_id) = event.get("message_id").and_then(Value::as_str) else {
            return None;
        };
        let Some(received_index) = event.get("in_message_index").and_then(Value::as_u64) else {
            return None;
        };
        let turn_id = event
            .get("turn_id")
            .and_then(Value::as_str)
            .map(str::to_string);
        if self.invalid_stream_messages.contains(message_id) {
            return Some(StreamValidationError {
                message_id: Some(message_id.to_string()),
                turn_id,
                received_index: Some(received_index),
                reason: "stream message was already marked invalid".to_string(),
            });
        }
        let expected = self
            .stream_next_indices
            .get(message_id)
            .copied()
            .unwrap_or(0);
        if received_index != expected {
            self.invalid_stream_messages.insert(message_id.to_string());
            self.current_provisional_assistant_message = self
                .current_provisional_assistant_message
                .take()
                .filter(|message| {
                    !message
                        .get("message_id")
                        .and_then(Value::as_str)
                        .is_some_and(|id| id == message_id)
                });
            return Some(StreamValidationError {
                message_id: Some(message_id.to_string()),
                turn_id,
                received_index: Some(received_index),
                reason: format!(
                    "non-contiguous stream event for {message_id}: expected index {expected}, received {received_index}"
                ),
            });
        }
        self.stream_next_indices
            .insert(message_id.to_string(), expected.saturating_add(1));
        None
    }

    fn commit_consistency_error(&self, message: &ChatMessage) -> Option<StreamConsistencyError> {
        if message.role != ChatRole::Assistant || message.message_id.is_empty() {
            return None;
        }
        let provisional =
            self.current_provisional_assistant_message
                .as_ref()
                .filter(|provisional| {
                    provisional
                        .get("message_id")
                        .and_then(Value::as_str)
                        .is_some_and(|id| id == message.message_id)
                })?;
        if !provisional_has_stream_content(provisional) {
            return None;
        }
        let Some(provisional_message) = provisional.get("message") else {
            return None;
        };
        let expected = self.stream_next_indices.get(&message.message_id).copied();
        let turn_id = provisional
            .get("turn_id")
            .and_then(Value::as_str)
            .map(str::to_string);
        let provisional_fingerprint = stream_message_fingerprint(provisional_message);
        let committed_fingerprint = committed_message_fingerprint(message);
        if provisional_fingerprint == committed_fingerprint {
            return None;
        }
        Some(StreamConsistencyError {
            turn_id,
            expected_index: expected,
            reason: format!(
                "stream provisional message {} did not match committed assistant message",
                message.message_id
            ),
        })
    }

    fn record_message_appended(&mut self, appended: &OutgoingMessageAppended, message: &Value) {
        self.last_committed_message_id = message
            .get("message_id")
            .or_else(|| message.get("id"))
            .and_then(Value::as_str)
            .map(str::to_string);
        self.last_committed_message_index = Some(appended.index);
        if let Some(message_id) = self.last_committed_message_id.as_deref() {
            self.queued_outbound_messages.retain(|queued| {
                match queued.get("client_message_id").and_then(Value::as_str) {
                    Some(id) => id != message_id,
                    None => true,
                }
            });
            if self
                .current_provisional_assistant_message
                .as_ref()
                .and_then(|provisional| provisional.get("message_id"))
                .and_then(Value::as_str)
                .is_some_and(|id| id == message_id)
            {
                self.current_provisional_assistant_message = None;
            }
            self.stream_next_indices.remove(message_id);
            self.invalid_stream_messages.remove(message_id);
        }
        if appended.message.role == stellaclaw_core::session_actor::ChatRole::User {
            if let Some(index) = self.queued_outbound_messages.iter().position(|queued| {
                queued
                    .get("conversation_id")
                    .and_then(Value::as_str)
                    .is_some_and(|id| id == appended.conversation_id)
            }) {
                self.queued_outbound_messages.remove(index);
            }
        }
        for tool_call_id in tool_result_call_ids(message) {
            for tool_state in &mut self.running_tool_results {
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

    fn record_session_stream(&mut self, event: &Value, event_type: &str) {
        match event_type {
            "user_message_queued" => {
                let client_message_id = event
                    .get("client_message_id")
                    .and_then(Value::as_str)
                    .map(str::to_string);
                let message_id = event
                    .get("message_id")
                    .and_then(Value::as_str)
                    .map(str::to_string);
                if let Some(dedupe_id) = client_message_id.as_deref().or(message_id.as_deref()) {
                    let is_queued = self.queued_outbound_messages.iter().any(|message| {
                        message
                            .get("client_message_id")
                            .and_then(Value::as_str)
                            .or_else(|| message.get("message_id").and_then(Value::as_str))
                            .is_some_and(|id| id == dedupe_id)
                    });
                    if !is_queued {
                        self.queued_outbound_messages.push(json!({
                            "client_message_id": client_message_id,
                            "message_id": message_id,
                        }));
                    }
                }
                self.last_error = None;
            }
            "user_message_started" => {
                self.last_error = None;
            }
            "user_message_committed" => {
                let message_id = event.get("message_id").and_then(Value::as_str).or_else(|| {
                    event
                        .get("message")
                        .and_then(|message| message.get("message_id"))
                        .and_then(Value::as_str)
                });
                if let Some(message_id) = message_id {
                    self.queued_outbound_messages.retain(|queued| {
                        let queued_message_id = queued.get("message_id").and_then(Value::as_str);
                        let queued_client_message_id =
                            queued.get("client_message_id").and_then(Value::as_str);
                        queued_message_id != Some(message_id)
                            && queued_client_message_id != Some(message_id)
                    });
                } else {
                    self.queued_outbound_messages.clear();
                }
                self.last_error = None;
            }
            "turn_started" => {
                self.last_error = None;
                self.current_turn_state =
                    event.get("turn_id").and_then(Value::as_str).map(|turn_id| {
                        json!({
                            "turn_id": turn_id,
                            "message_id": Value::Null,
                        })
                    });
                self.current_provisional_assistant_message = None;
                self.running_tool_results.clear();
                self.stream_next_indices.clear();
                self.invalid_stream_messages.clear();
            }
            "stream_assistant_message_delta" => self.apply_assistant_delta(event),
            "stream_tool_call_delta" => self.apply_tool_call_delta(event),
            "stream_reasoning_summary_part_added" => self.apply_reasoning_summary_part(event),
            "stream_reasoning_summary_delta" => self.apply_reasoning_summary_delta(event),
            "stream_tool_result_done" => self.apply_tool_result_done(event),
            "stream_error" => {
                let message_id = event
                    .get("message_id")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                if message_id.is_empty() {
                    self.current_provisional_assistant_message = None;
                } else {
                    self.current_provisional_assistant_message = self
                        .current_provisional_assistant_message
                        .take()
                        .filter(|message| {
                            !message
                                .get("message_id")
                                .and_then(Value::as_str)
                                .is_some_and(|id| id == message_id)
                        });
                }
                self.current_turn_state = None;
                self.running_tool_results.clear();
                if !message_id.is_empty() {
                    self.stream_next_indices.remove(message_id);
                    self.invalid_stream_messages.insert(message_id.to_string());
                }
                self.last_error = event
                    .get("error")
                    .and_then(Value::as_str)
                    .map(str::to_string);
            }
            "turn_completed" => {
                self.current_turn_state = None;
                self.current_provisional_assistant_message = None;
                self.running_tool_results.clear();
                self.stream_next_indices.clear();
                self.invalid_stream_messages.clear();
                self.queued_outbound_messages.clear();
                self.last_error = None;
            }
            _ => {}
        }
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

fn provisional_has_stream_content(provisional: &Value) -> bool {
    let Some(message) = provisional.get("message") else {
        return false;
    };
    message
        .get("text")
        .and_then(Value::as_str)
        .is_some_and(|text| !text.is_empty())
        || message
            .get("items")
            .and_then(Value::as_array)
            .is_some_and(|items| !items.is_empty())
}

fn stream_message_fingerprint(message: &Value) -> Value {
    let text = message
        .get("text")
        .or_else(|| message.get("content"))
        .and_then(Value::as_str)
        .unwrap_or_default();
    let mut tool_calls = Vec::new();
    let mut reasoning = Vec::new();
    if let Some(items) = message.get("items").and_then(Value::as_array) {
        for item in items {
            match item.get("type").and_then(Value::as_str) {
                Some("tool_call") => {
                    tool_calls.push(json!({
                        "tool_call_id": item.get("tool_call_id").and_then(Value::as_str).unwrap_or_default(),
                        "tool_name": item.get("tool_name").and_then(Value::as_str).unwrap_or_default(),
                        "arguments": item.get("arguments").and_then(Value::as_str).unwrap_or_default(),
                    }));
                }
                Some("reasoning") => {
                    reasoning.push(json!({
                        "summary_index": item.get("_summaryIndex").and_then(Value::as_i64).unwrap_or_default(),
                        "text": item.get("text").or_else(|| item.get("summary")).and_then(Value::as_str).unwrap_or_default(),
                    }));
                }
                _ => {}
            }
        }
    }
    sort_fingerprint_items(&mut tool_calls, "tool_call_id");
    sort_fingerprint_items(&mut reasoning, "summary_index");
    json!({
        "text": text,
        "tool_calls": tool_calls,
        "reasoning": reasoning,
    })
}

fn committed_message_fingerprint(message: &ChatMessage) -> Value {
    let mut text = String::new();
    let mut tool_calls = Vec::new();
    let mut reasoning = Vec::new();
    for item in &message.data {
        match item {
            ChatMessageItem::Context(context) => text.push_str(&context.text),
            ChatMessageItem::ToolCall(tool) => tool_calls.push(json!({
                "tool_call_id": tool.tool_call_id,
                "tool_name": tool.tool_name,
                "arguments": tool.arguments.text,
            })),
            ChatMessageItem::Reasoning(reasoning_item) => {
                if reasoning_item.codex_summary.is_empty() {
                    if !reasoning_item.text.is_empty() {
                        reasoning.push(json!({
                            "summary_index": reasoning.len() as i64,
                            "text": reasoning_item.text,
                        }));
                    }
                } else {
                    for part in &reasoning_item.codex_summary {
                        reasoning.push(json!({
                            "summary_index": reasoning.len() as i64,
                            "text": part.text,
                        }));
                    }
                }
            }
            _ => {}
        }
    }
    sort_fingerprint_items(&mut tool_calls, "tool_call_id");
    sort_fingerprint_items(&mut reasoning, "summary_index");
    json!({
        "text": text,
        "tool_calls": tool_calls,
        "reasoning": reasoning,
    })
}

fn sort_fingerprint_items(items: &mut [Value], key: &str) {
    items.sort_by(|left, right| {
        let left_key = left
            .get(key)
            .map(|value| {
                value
                    .as_str()
                    .map(str::to_string)
                    .unwrap_or_else(|| value.to_string())
            })
            .unwrap_or_default();
        let right_key = right
            .get(key)
            .map(|value| {
                value
                    .as_str()
                    .map(str::to_string)
                    .unwrap_or_else(|| value.to_string())
            })
            .unwrap_or_default();
        left_key.cmp(&right_key)
    });
}

pub(super) fn load_seen_state(workdir: &Path, channel_id: &str) -> Result<WebSeenState> {
    let path = seen_state_path(workdir, channel_id);
    if !path.exists() {
        return Ok(WebSeenState::default());
    }
    let raw = std::fs::read_to_string(&path)
        .with_context(|| format!("failed to read {}", path.display()))?;
    serde_json::from_str(&raw).with_context(|| format!("failed to parse {}", path.display()))
}

fn persist_seen_state(workdir: &Path, channel_id: &str, state: &WebSeenState) -> Result<()> {
    let path = seen_state_path(workdir, channel_id);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }
    std::fs::write(&path, serde_json::to_string_pretty(state)?)
        .with_context(|| format!("failed to write {}", path.display()))
}

fn seen_state_path(workdir: &Path, channel_id: &str) -> PathBuf {
    workdir
        .join(".stellaclaw")
        .join("web")
        .join(channel_id)
        .join(SEEN_STATE_FILE)
}

fn conversation_seen_key(conversation_id: &str, foreground_session_id: &str) -> String {
    format!(
        "{conversation_id}:{}",
        foreground_session_storage_id(foreground_session_id)
    )
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

#[cfg(test)]
mod tests {
    use serde_json::json;
    use stellaclaw_core::session_actor::{ChatMessage, ChatMessageItem, ChatRole, ContextItem};

    use super::{protocol::public_chat_stream_payload, ChatLiveState};

    #[test]
    fn stream_payload_is_flat_typed_chat_event() {
        let payload = public_chat_stream_payload(
            "conversation-1",
            "main",
            "stream_assistant_message_delta",
            &json!({
                "type": "stream_assistant_message_delta",
                "message_id": "msg-1",
                "turn_id": "turn-1",
                "in_message_index": 0,
                "delta": "hello",
            }),
        );

        assert_eq!(
            payload.get("type").and_then(|value| value.as_str()),
            Some("chat.stream_assistant_message_delta")
        );
        assert_eq!(
            payload
                .get("conversation_id")
                .and_then(|value| value.as_str()),
            Some("conversation-1")
        );
        assert_eq!(
            payload
                .get("foreground_session_id")
                .and_then(|value| value.as_str()),
            Some("main")
        );
        assert_eq!(payload.get("event"), None);
        assert_eq!(
            payload.get("delta").and_then(|value| value.as_str()),
            Some("hello")
        );
        assert_eq!(
            payload
                .get("next_message_id")
                .and_then(|value| value.as_str()),
            Some("msg-1")
        );
        assert_eq!(
            payload
                .get("in_message_index")
                .and_then(|value| value.as_u64()),
            Some(0)
        );
    }

    #[test]
    fn turn_lifecycle_events_use_public_names() {
        let start = public_chat_stream_payload(
            "conversation-1",
            "main",
            "turn_started",
            &json!({"type": "turn_started", "turn_id": "turn-1"}),
        );
        let done = public_chat_stream_payload(
            "conversation-1",
            "main",
            "turn_completed",
            &json!({"type": "turn_completed", "message": {"role": "assistant"}}),
        );

        assert_eq!(
            start.get("type").and_then(|value| value.as_str()),
            Some("chat.stream_turn_start")
        );
        assert_eq!(
            done.get("type").and_then(|value| value.as_str()),
            Some("chat.stream_turn_done")
        );
    }

    #[test]
    fn live_state_rejects_non_contiguous_stream_index() {
        let mut state = ChatLiveState::default();
        let first = json!({
            "type": "stream_assistant_message_delta",
            "message_id": "msg-1",
            "turn_id": "turn-1",
            "in_message_index": 0,
            "delta": "hello",
        });
        assert!(state
            .validate_stream_event(&first, "stream_assistant_message_delta")
            .is_none());
        state.record_session_stream(&first, "stream_assistant_message_delta");

        let skipped = json!({
            "type": "stream_assistant_message_delta",
            "message_id": "msg-1",
            "turn_id": "turn-1",
            "in_message_index": 2,
            "delta": " world",
        });
        let error = state
            .validate_stream_event(&skipped, "stream_assistant_message_delta")
            .expect("gap should invalidate stream");
        assert!(error.reason.contains("expected index 1"));
        assert!(state.current_provisional_assistant_message.is_none());
    }

    #[test]
    fn live_state_accepts_matching_committed_assistant_message() {
        let mut state = ChatLiveState::default();
        let stream = json!({
            "type": "stream_assistant_message_delta",
            "message_id": "msg-1",
            "turn_id": "turn-1",
            "in_message_index": 0,
            "delta": "hello",
        });
        assert!(state
            .validate_stream_event(&stream, "stream_assistant_message_delta")
            .is_none());
        state.record_session_stream(&stream, "stream_assistant_message_delta");

        let message = ChatMessage::new(
            ChatRole::Assistant,
            vec![ChatMessageItem::Context(ContextItem {
                text: "hello".to_string(),
            })],
        )
        .with_message_id("msg-1");
        assert!(state.commit_consistency_error(&message).is_none());
    }

    #[test]
    fn live_state_reports_committed_assistant_mismatch() {
        let mut state = ChatLiveState::default();
        let stream = json!({
            "type": "stream_assistant_message_delta",
            "message_id": "msg-1",
            "turn_id": "turn-1",
            "in_message_index": 0,
            "delta": "hello",
        });
        assert!(state
            .validate_stream_event(&stream, "stream_assistant_message_delta")
            .is_none());
        state.record_session_stream(&stream, "stream_assistant_message_delta");

        let message = ChatMessage::new(
            ChatRole::Assistant,
            vec![ChatMessageItem::Context(ContextItem {
                text: "goodbye".to_string(),
            })],
        )
        .with_message_id("msg-1");
        assert!(state.commit_consistency_error(&message).is_some());
    }
}
