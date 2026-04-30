use std::{
    collections::HashMap,
    sync::{mpsc, Arc},
    thread::{self, JoinHandle},
};

use serde::{Deserialize, Serialize};
use serde_json::Value;
use thiserror::Error;

use crate::model_config::ModelConfig;

use super::{
    ChatMessage, ConversationBridge, ConversationBridgeRequest, ConversationBridgeResponse,
    ToolBatchError,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionType {
    Foreground,
    Background,
    Subagent,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ToolRemoteMode {
    Selectable,
    FixedSsh {
        host: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        cwd: Option<String>,
    },
}

impl Default for ToolRemoteMode {
    fn default() -> Self {
        Self::Selectable
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SessionInitial {
    pub session_id: String,
    pub session_type: SessionType,
    #[serde(default)]
    pub tool_remote_mode: ToolRemoteMode,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub compression_threshold_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub compression_retain_recent_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub image_tool_model: Option<ModelConfig>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pdf_tool_model: Option<ModelConfig>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub audio_tool_model: Option<ModelConfig>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub image_generation_tool_model: Option<ModelConfig>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub search_tool_model: Option<ModelConfig>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub search_image_tool_model: Option<ModelConfig>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub search_video_tool_model: Option<ModelConfig>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub search_news_tool_model: Option<ModelConfig>,
}

impl SessionInitial {
    pub fn new(session_id: impl Into<String>, session_type: SessionType) -> Self {
        Self {
            session_id: session_id.into(),
            session_type,
            tool_remote_mode: ToolRemoteMode::Selectable,
            compression_threshold_tokens: None,
            compression_retain_recent_tokens: None,
            image_tool_model: None,
            pdf_tool_model: None,
            audio_tool_model: None,
            image_generation_tool_model: None,
            search_tool_model: None,
            search_image_tool_model: None,
            search_video_tool_model: None,
            search_news_tool_model: None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionMailboxKind {
    Control,
    Data,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", content = "payload", rename_all = "snake_case")]
pub enum SessionRequest {
    Initial {
        initial: SessionInitial,
    },
    EnqueueUserMessage {
        message: ChatMessage,
    },
    EnqueueActorMessage {
        message: ChatMessage,
    },
    CancelTurn {
        reason: Option<String>,
    },
    ContinueTurn {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reason: Option<String>,
    },
    CompactNow,
    ResolveHostCoordination {
        response: ConversationBridgeResponse,
    },
    QuerySessionView {
        query_id: String,
        payload: Value,
    },
    Shutdown,
}

impl SessionRequest {
    pub fn mailbox_kind(&self) -> SessionMailboxKind {
        match self {
            SessionRequest::EnqueueUserMessage { .. }
            | SessionRequest::EnqueueActorMessage { .. } => SessionMailboxKind::Data,
            SessionRequest::Initial { .. }
            | SessionRequest::CancelTurn { .. }
            | SessionRequest::ContinueTurn { .. }
            | SessionRequest::CompactNow
            | SessionRequest::ResolveHostCoordination { .. }
            | SessionRequest::QuerySessionView { .. }
            | SessionRequest::Shutdown => SessionMailboxKind::Control,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", content = "payload", rename_all = "snake_case")]
pub enum SessionEvent {
    MessageAppended {
        index: usize,
        message: ChatMessage,
    },
    TurnStarted {
        turn_id: String,
    },
    Progress {
        message: String,
    },
    TurnCompleted {
        message: ChatMessage,
    },
    TurnFailed {
        error: String,
        error_detail: SessionErrorDetail,
        can_continue: bool,
    },
    HostCoordinationRequested {
        request: ConversationBridgeRequest,
    },
    InteractiveOutputRequested {
        payload: Value,
    },
    SessionViewResult {
        query_id: String,
        payload: Value,
    },
    CompactCompleted {
        compressed: bool,
        estimated_tokens_before: u64,
        estimated_tokens_after: u64,
        threshold_tokens: u64,
        retained_message_count: usize,
        compressed_message_count: usize,
    },
    ControlRejected {
        reason: String,
        payload: Value,
    },
    RuntimeCrashed {
        error: String,
        error_detail: SessionErrorDetail,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SessionErrorDetail {
    pub module: String,
    pub kind: String,
    pub reason: String,
}

impl SessionErrorDetail {
    pub fn new(
        module: impl Into<String>,
        kind: impl Into<String>,
        reason: impl Into<String>,
    ) -> Self {
        Self {
            module: module.into(),
            kind: kind.into(),
            reason: reason.into(),
        }
    }

    pub fn summary(&self) -> String {
        format!("{} [{}]: {}", self.module, self.kind, self.reason)
    }
}

pub trait SessionMailbox: Send + Sync + 'static {
    fn append(&self, kind: SessionMailboxKind, request: SessionRequest) -> Result<(), String>;
}

pub trait ConversationTransport: Send + Sync + 'static {
    fn send_event(&self, event: SessionEvent) -> Result<(), String>;
}

enum SessionRpcCommand {
    FromConversation(SessionRequest),
    FromSession(SessionEvent),
    ConversationBridgeCall {
        request: ConversationBridgeRequest,
        response_tx: mpsc::Sender<Result<ConversationBridgeResponse, SessionRpcError>>,
    },
    Shutdown,
}

pub struct SessionRpcThread {
    command_tx: mpsc::Sender<SessionRpcCommand>,
    join_handle: Option<JoinHandle<()>>,
}

impl SessionRpcThread {
    pub fn spawn(
        mailbox: Arc<dyn SessionMailbox>,
        conversation_transport: Arc<dyn ConversationTransport>,
    ) -> Self {
        let (command_tx, command_rx) = mpsc::channel();
        let join_handle = thread::spawn(move || {
            run_session_rpc_loop(command_rx, mailbox, conversation_transport)
        });

        Self {
            command_tx,
            join_handle: Some(join_handle),
        }
    }

    pub fn enqueue_from_conversation(
        &self,
        request: SessionRequest,
    ) -> Result<(), SessionRpcError> {
        self.command_tx
            .send(SessionRpcCommand::FromConversation(request))
            .map_err(|_| SessionRpcError::ThreadStopped)
    }

    pub fn emit_event(&self, event: SessionEvent) -> Result<(), SessionRpcError> {
        self.command_tx
            .send(SessionRpcCommand::FromSession(event))
            .map_err(|_| SessionRpcError::ThreadStopped)
    }

    pub fn conversation_bridge(&self) -> SessionRpcConversationBridge {
        SessionRpcConversationBridge {
            command_tx: self.command_tx.clone(),
        }
    }

    pub fn shutdown(mut self) -> Result<(), SessionRpcError> {
        let _ = self.command_tx.send(SessionRpcCommand::Shutdown);
        if let Some(join_handle) = self.join_handle.take() {
            join_handle
                .join()
                .map_err(|_| SessionRpcError::ThreadPanicked)?;
        }
        Ok(())
    }
}

#[derive(Clone)]
pub struct SessionRpcConversationBridge {
    command_tx: mpsc::Sender<SessionRpcCommand>,
}

impl ConversationBridge for SessionRpcConversationBridge {
    fn call(
        &self,
        request: ConversationBridgeRequest,
    ) -> Result<ConversationBridgeResponse, ToolBatchError> {
        self.call_session_rpc(request)
            .map_err(|error| ToolBatchError::Bridge(error.to_string()))
    }
}

impl SessionRpcConversationBridge {
    pub fn call_session_rpc(
        &self,
        request: ConversationBridgeRequest,
    ) -> Result<ConversationBridgeResponse, SessionRpcError> {
        let (response_tx, response_rx) = mpsc::channel();
        self.command_tx
            .send(SessionRpcCommand::ConversationBridgeCall {
                request,
                response_tx,
            })
            .map_err(|_| SessionRpcError::ThreadStopped)?;

        response_rx
            .recv()
            .map_err(|_| SessionRpcError::ThreadStopped)?
    }
}

fn run_session_rpc_loop(
    command_rx: mpsc::Receiver<SessionRpcCommand>,
    mailbox: Arc<dyn SessionMailbox>,
    conversation_transport: Arc<dyn ConversationTransport>,
) {
    let mut pending_bridge_calls = HashMap::new();

    while let Ok(command) = command_rx.recv() {
        match command {
            SessionRpcCommand::FromConversation(request) => {
                handle_conversation_request(request, &mailbox, &mut pending_bridge_calls);
            }
            SessionRpcCommand::FromSession(event) => {
                let _ = conversation_transport.send_event(event);
            }
            SessionRpcCommand::ConversationBridgeCall {
                request,
                response_tx,
            } => {
                let request_id = request.request_id.clone();
                match conversation_transport
                    .send_event(SessionEvent::HostCoordinationRequested { request })
                {
                    Ok(()) => {
                        pending_bridge_calls.insert(request_id, response_tx);
                    }
                    Err(error) => {
                        let _ = response_tx.send(Err(SessionRpcError::Transport(error)));
                    }
                }
            }
            SessionRpcCommand::Shutdown => break,
        }
    }
}

fn handle_conversation_request(
    request: SessionRequest,
    mailbox: &Arc<dyn SessionMailbox>,
    pending_bridge_calls: &mut HashMap<
        String,
        mpsc::Sender<Result<ConversationBridgeResponse, SessionRpcError>>,
    >,
) {
    if let SessionRequest::ResolveHostCoordination { response } = &request {
        if let Some(response_tx) = pending_bridge_calls.remove(&response.request_id) {
            let _ = response_tx.send(Ok(response.clone()));
            return;
        }
    }

    let kind = request.mailbox_kind();
    let _ = mailbox.append(kind, request);
}

#[derive(Debug, Error)]
pub enum SessionRpcError {
    #[error("session rpc thread stopped")]
    ThreadStopped,
    #[error("session rpc thread panicked")]
    ThreadPanicked,
    #[error("conversation transport failed: {0}")]
    Transport(String),
    #[error("session mailbox append failed: {0}")]
    Mailbox(String),
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use crate::session_actor::{ContextItem, ToolResultContent, ToolResultItem};

    use super::*;

    #[derive(Default)]
    struct MemoryMailbox {
        entries: Mutex<Vec<(SessionMailboxKind, SessionRequest)>>,
    }

    impl SessionMailbox for MemoryMailbox {
        fn append(&self, kind: SessionMailboxKind, request: SessionRequest) -> Result<(), String> {
            self.entries.lock().unwrap().push((kind, request));
            Ok(())
        }
    }

    #[derive(Default)]
    struct MemoryTransport {
        events: Mutex<Vec<SessionEvent>>,
    }

    impl ConversationTransport for MemoryTransport {
        fn send_event(&self, event: SessionEvent) -> Result<(), String> {
            self.events.lock().unwrap().push(event);
            Ok(())
        }
    }

    #[test]
    fn appends_conversation_requests_to_durable_mailbox() {
        let mailbox = Arc::new(MemoryMailbox::default());
        let transport = Arc::new(MemoryTransport::default());
        let rpc = SessionRpcThread::spawn(mailbox.clone(), transport);

        rpc.enqueue_from_conversation(SessionRequest::CancelTurn {
            reason: Some("user_cancelled".to_string()),
        })
        .expect("request should enqueue");

        rpc.shutdown().expect("rpc should shut down");

        let entries = mailbox.entries.lock().unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].0, SessionMailboxKind::Control);
    }

    #[test]
    fn initial_request_goes_to_control_mailbox() {
        let mailbox = Arc::new(MemoryMailbox::default());
        let transport = Arc::new(MemoryTransport::default());
        let rpc = SessionRpcThread::spawn(mailbox.clone(), transport);

        rpc.enqueue_from_conversation(SessionRequest::Initial {
            initial: SessionInitial::new("session_1", SessionType::Foreground),
        })
        .expect("initial should enqueue");

        rpc.shutdown().expect("rpc should shut down");

        let entries = mailbox.entries.lock().unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].0, SessionMailboxKind::Control);
        assert!(matches!(
            &entries[0].1,
            SessionRequest::Initial { initial }
                if initial.session_type == SessionType::Foreground
        ));
    }

    #[test]
    fn bridge_call_round_trips_through_session_rpc() {
        let mailbox = Arc::new(MemoryMailbox::default());
        let transport = Arc::new(MemoryTransport::default());
        let rpc = SessionRpcThread::spawn(mailbox, transport.clone());
        let bridge = rpc.conversation_bridge();

        let request = ConversationBridgeRequest {
            request_id: "req_1".to_string(),
            tool_call_id: "call_1".to_string(),
            tool_name: "user_tell".to_string(),
            action: "user_tell".to_string(),
            payload: serde_json::json!({"text": "working"}),
        };

        let bridge_thread = thread::spawn(move || bridge.call_session_rpc(request));

        loop {
            let events = transport.events.lock().unwrap();
            if events.len() == 1 {
                assert!(matches!(
                    &events[0],
                    SessionEvent::HostCoordinationRequested { request }
                        if request.request_id == "req_1"
                ));
                break;
            }
            drop(events);
            thread::yield_now();
        }

        rpc.enqueue_from_conversation(SessionRequest::ResolveHostCoordination {
            response: ConversationBridgeResponse {
                request_id: "req_1".to_string(),
                tool_call_id: "call_1".to_string(),
                tool_name: "user_tell".to_string(),
                result: ToolResultItem {
                    tool_call_id: "call_1".to_string(),
                    tool_name: "user_tell".to_string(),
                    result: ToolResultContent {
                        context: Some(ContextItem {
                            text: "sent".to_string(),
                        }),
                        file: None,
                    },
                },
            },
        })
        .expect("response should enqueue");

        let response = bridge_thread
            .join()
            .expect("bridge thread should join")
            .expect("bridge should return response");
        assert_eq!(response.request_id, "req_1");

        rpc.shutdown().expect("rpc should shut down");
    }
}
