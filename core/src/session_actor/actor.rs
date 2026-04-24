use std::{
    collections::VecDeque,
    env,
    path::PathBuf,
    sync::Arc,
    time::{Duration, Instant},
};

use crossbeam_channel::{select, Receiver, Sender};
use thiserror::Error;

use crate::{
    huggingface::HuggingFaceFileResolver,
    model_config::ModelConfig,
    providers::{request_too_large_text, Provider, ProviderRequest},
    session_actor::tool_catalog::ToolBackend,
};

use super::{
    logger::SessionActorLogger,
    normalize_messages_for_model,
    runtime_metadata::{remote_aliases_prompt_for_mode, RuntimeMetadataState},
    session_state::{SessionActorPersistedState, SessionStateStore},
    system_prompt_for_initial, ChatMessage, ChatMessageItem, ChatRole, CompressionError,
    CompressionReport, ContextItem, ConversationBridgeRequest, SessionCompressor, SessionEvent,
    SessionInitial, SessionMailbox, SessionMailboxKind, SessionRequest, TokenEstimator, ToolBatch,
    ToolBatchCompletion, ToolBatchExecutor, ToolCatalog, ToolExecutionOp,
};

const DEFAULT_MAX_MODEL_STEPS_PER_TURN: usize = 200;
const ACTIVE_COMPRESSION_THRESHOLD_RATIO: f64 = 0.9;
const IDLE_COMPACTION_MIN_RATIO: f64 = 0.4;
const REQUEST_TOO_LARGE_PRUNE_MAX_ATTEMPTS: usize = 8;

pub struct SessionActor {
    model_config: ModelConfig,
    provider: Arc<dyn Provider + Send + Sync>,
    tool_executor: Arc<dyn ToolBatchExecutor + Send + Sync>,
    request_rx: Receiver<SessionRequest>,
    tool_completion_tx: Sender<ToolBatchCompletion>,
    tool_completion_rx: Receiver<ToolBatchCompletion>,
    pending_control: VecDeque<SessionRequest>,
    pending_data: VecDeque<SessionRequest>,
    pending_tool_completions: VecDeque<ToolBatchCompletion>,
    event_sink: Arc<dyn SessionActorEventSink>,
    tool_catalog: ToolCatalog,
    history: Vec<ChatMessage>,
    all_messages: Vec<ChatMessage>,
    initial: Option<SessionInitial>,
    active_tool_batch: Option<ActiveToolBatch>,
    runtime_metadata_state: RuntimeMetadataState,
    next_turn_id: u64,
    next_batch_id: u64,
    max_model_steps_per_turn: usize,
    shutdown: bool,
    logger: Option<SessionActorLogger>,
    state_store: Option<SessionStateStore>,
    compressor: Option<SessionCompressor>,
    token_estimator: Option<TokenEstimator>,
    pending_continuation: Option<PendingContinuation>,
    last_agent_returned_at: Option<Instant>,
    last_completed_turn_number: u64,
    last_idle_compaction_turn_number: u64,
}

#[derive(Debug, Clone)]
struct ActiveToolBatch {
    turn_id: String,
    turn_number: u64,
    step_index: usize,
    handle: super::ToolBatchHandle,
    interrupt: Option<ToolBatchInterrupt>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ToolBatchInterrupt {
    Cancel,
    SupersededByUserMessage,
}

#[derive(Debug, Clone)]
enum PendingContinuation {
    CurrentHistory,
    DataRequest(SessionRequest),
}

pub struct SessionActorInbox {
    request_rx: Receiver<SessionRequest>,
    tool_completion_tx: Sender<ToolBatchCompletion>,
    tool_completion_rx: Receiver<ToolBatchCompletion>,
}

#[derive(Clone)]
pub struct SessionActorRequestSender {
    request_tx: Sender<SessionRequest>,
}

impl SessionActorInbox {
    pub fn channel() -> (Self, SessionActorRequestSender) {
        let (request_tx, request_rx) = crossbeam_channel::unbounded();
        let (tool_completion_tx, tool_completion_rx) = crossbeam_channel::unbounded();
        (
            Self {
                request_rx,
                tool_completion_tx,
                tool_completion_rx,
            },
            SessionActorRequestSender { request_tx },
        )
    }
}

impl SessionActorRequestSender {
    pub fn send(&self, request: SessionRequest) -> Result<(), String> {
        self.request_tx
            .send(request)
            .map_err(|_| "session actor request channel closed".to_string())
    }
}

impl SessionMailbox for SessionActorRequestSender {
    fn append(&self, kind: SessionMailboxKind, request: SessionRequest) -> Result<(), String> {
        if kind != request.mailbox_kind() {
            return Err(format!(
                "request kind mismatch: envelope={kind:?}, request={:?}",
                request.mailbox_kind()
            ));
        }
        self.send(request)
    }
}

impl SessionActor {
    pub fn new(
        model_config: ModelConfig,
        provider: Arc<dyn Provider + Send + Sync>,
        tool_executor: Arc<dyn ToolBatchExecutor + Send + Sync>,
        inbox: SessionActorInbox,
        event_sink: Arc<dyn SessionActorEventSink>,
        tool_catalog: ToolCatalog,
    ) -> Self {
        Self {
            model_config,
            provider,
            tool_executor,
            request_rx: inbox.request_rx,
            tool_completion_tx: inbox.tool_completion_tx,
            tool_completion_rx: inbox.tool_completion_rx,
            pending_control: VecDeque::new(),
            pending_data: VecDeque::new(),
            pending_tool_completions: VecDeque::new(),
            event_sink,
            tool_catalog,
            history: Vec::new(),
            all_messages: Vec::new(),
            initial: None,
            active_tool_batch: None,
            runtime_metadata_state: RuntimeMetadataState::default(),
            next_turn_id: 1,
            next_batch_id: 1,
            max_model_steps_per_turn: DEFAULT_MAX_MODEL_STEPS_PER_TURN,
            shutdown: false,
            logger: None,
            state_store: None,
            compressor: None,
            token_estimator: None,
            pending_continuation: None,
            last_agent_returned_at: None,
            last_completed_turn_number: 0,
            last_idle_compaction_turn_number: 0,
        }
    }

    pub fn history(&self) -> &[ChatMessage] {
        &self.history
    }

    pub fn initial(&self) -> Option<&SessionInitial> {
        self.initial.as_ref()
    }

    pub fn tool_catalog(&self) -> &ToolCatalog {
        &self.tool_catalog
    }

    pub fn set_max_model_steps_per_turn(&mut self, max_steps: usize) {
        self.max_model_steps_per_turn = max_steps.max(1);
    }

    pub fn step(&mut self) -> Result<SessionActorStep, SessionActorError> {
        self.drain_ready_events();
        self.process_ready_step()
    }

    pub fn recv_step(&mut self) -> Result<SessionActorStep, SessionActorError> {
        self.drain_ready_events();
        if self.has_ready_work() {
            return self.process_ready_step();
        }

        if let Some(delay) = self.idle_compaction_delay() {
            if delay.is_zero() {
                if self.try_run_idle_compaction()? {
                    return Ok(SessionActorStep::ProcessedIdleCompaction);
                }
            } else {
                let idle_timer = crossbeam_channel::after(delay);
                select! {
                    recv(self.request_rx) -> request => {
                        let request = request.map_err(|_| SessionActorError::Mailbox("session actor request channel closed".to_string()))?;
                        self.enqueue_request(request);
                    }
                    recv(self.tool_completion_rx) -> completion => {
                        let completion = completion.map_err(|_| SessionActorError::Tool("tool completion channel disconnected".to_string()))?;
                        self.pending_tool_completions.push_back(completion);
                    }
                    recv(idle_timer) -> _ => {
                        if self.try_run_idle_compaction()? {
                            return Ok(SessionActorStep::ProcessedIdleCompaction);
                        }
                    }
                }
            }
        } else {
            select! {
                recv(self.request_rx) -> request => {
                    let request = request.map_err(|_| SessionActorError::Mailbox("session actor request channel closed".to_string()))?;
                    self.enqueue_request(request);
                }
                recv(self.tool_completion_rx) -> completion => {
                    let completion = completion.map_err(|_| SessionActorError::Tool("tool completion channel disconnected".to_string()))?;
                    self.pending_tool_completions.push_back(completion);
                }
            }
        }
        self.process_ready_step()
    }

    fn process_ready_step(&mut self) -> Result<SessionActorStep, SessionActorError> {
        if self.shutdown {
            return Ok(SessionActorStep::Shutdown);
        }

        if let Some(control) = self.pending_control.pop_front() {
            self.log_info(
                "control_request",
                serde_json::json!({"request": session_request_kind(&control)}),
            );
            self.handle_control(control)?;
            return Ok(if self.shutdown {
                SessionActorStep::Shutdown
            } else {
                SessionActorStep::ProcessedControl
            });
        }

        if self.active_tool_batch.is_some() {
            if self.has_pending_user_message_interrupt() {
                self.request_active_tool_interrupt(
                    ToolBatchInterrupt::SupersededByUserMessage,
                    "newer user message arrived".to_string(),
                )?;
            }
            return self.handle_ready_tool_completion();
        }

        let Some(data) = self.pending_data.pop_front() else {
            return Ok(SessionActorStep::Idle);
        };

        if !matches!(
            data,
            SessionRequest::EnqueueUserMessage { .. } | SessionRequest::EnqueueActorMessage { .. }
        ) {
            return Err(SessionActorError::UnexpectedDataRequest);
        }

        self.log_info(
            "data_request",
            serde_json::json!({"request": session_request_kind(&data)}),
        );
        self.run_turn_from_data_request(data)?;
        Ok(if self.active_tool_batch.is_some() {
            SessionActorStep::WaitingToolBatch
        } else {
            SessionActorStep::ProcessedData
        })
    }

    fn drain_ready_events(&mut self) {
        while let Ok(request) = self.request_rx.try_recv() {
            self.enqueue_request(request);
        }
        while let Ok(completion) = self.tool_completion_rx.try_recv() {
            self.pending_tool_completions.push_back(completion);
        }
    }

    fn enqueue_request(&mut self, request: SessionRequest) {
        match request.mailbox_kind() {
            SessionMailboxKind::Control => self.pending_control.push_back(request),
            SessionMailboxKind::Data => self.pending_data.push_back(request),
        }
    }

    fn has_ready_work(&self) -> bool {
        self.shutdown
            || !self.pending_control.is_empty()
            || (!self.pending_data.is_empty() && self.active_tool_batch.is_none())
            || (self.active_tool_batch.is_some() && !self.pending_tool_completions.is_empty())
            || (self.active_tool_batch.is_some() && self.has_pending_user_message_interrupt())
    }

    pub fn run_until_idle(
        &mut self,
        max_steps: usize,
    ) -> Result<SessionActorStep, SessionActorError> {
        for _ in 0..max_steps {
            let step = self.step()?;
            if matches!(step, SessionActorStep::Idle | SessionActorStep::Shutdown) {
                return Ok(step);
            }
        }

        Err(SessionActorError::StepLimitExceeded(max_steps))
    }

    fn handle_control(&mut self, control: SessionRequest) -> Result<(), SessionActorError> {
        match control {
            SessionRequest::Initial { initial } => {
                if self.initial.is_some() {
                    self.log_warn(
                        "initial_rejected",
                        serde_json::json!({"reason": "session initial has already been applied"}),
                    );
                    return self.emit(SessionEvent::ControlRejected {
                        reason: "session initial has already been applied".to_string(),
                        payload: serde_json::to_value(initial).unwrap_or_else(
                            |_| serde_json::json!({"error": "serialize initial failed"}),
                        ),
                    });
                }

                let logger = SessionActorLogger::open_default(&initial.session_id)
                    .map_err(SessionActorError::Logging)?;
                let state_store = SessionStateStore::open_default(&initial.session_id)
                    .map_err(SessionActorError::Persistence)?;
                self.tool_catalog =
                    ToolCatalog::from_model_config_and_initial(&self.model_config, &initial)
                        .map_err(|error| SessionActorError::ToolCatalog(error.to_string()))?;
                let active_compression_threshold_tokens =
                    active_compression_threshold_tokens(&self.model_config, &initial);
                let token_estimator = match build_session_token_estimator(&self.model_config) {
                    Ok(estimator) => Some(estimator),
                    Err(error) if active_compression_threshold_tokens.is_none() => {
                        logger.warn(
                            "token_estimator_unavailable",
                            serde_json::json!({
                                "error": error.to_string(),
                                "preflight_context_check": false,
                            }),
                        );
                        None
                    }
                    Err(error) => {
                        return Err(SessionActorError::Compression(error.to_string()));
                    }
                };
                let compressor = build_session_compressor(
                    active_compression_threshold_tokens,
                    &initial,
                    token_estimator.as_ref(),
                )?;
                let loaded = state_store.load().map_err(SessionActorError::Persistence)?;
                if let Some(saved) = loaded {
                    self.restore_persisted_state(saved, initial.clone())?;
                    logger.info(
                        "session_state_restored",
                        serde_json::json!({
                            "session_id": &initial.session_id,
                            "history_len": self.history.len(),
                            "all_messages_len": self.all_messages.len(),
                            "next_turn_id": self.next_turn_id,
                            "next_batch_id": self.next_batch_id,
                        }),
                    );
                } else {
                    self.runtime_metadata_state
                        .initialize_from_workspace(
                            &self
                                .workspace_root()
                                .map_err(SessionActorError::RuntimeMetadata)?,
                            remote_aliases_prompt_for_mode(&initial.tool_remote_mode),
                        )
                        .map_err(SessionActorError::RuntimeMetadata)?;
                }
                logger.info(
                    "initial_applied",
                    serde_json::json!({
                        "session_id": &initial.session_id,
                        "session_type": &initial.session_type,
                        "tool_remote_mode": &initial.tool_remote_mode,
                        "compression_threshold_tokens": initial.compression_threshold_tokens,
                        "active_compression_threshold_tokens": active_compression_threshold_tokens,
                        "compression_retain_recent_tokens": initial.compression_retain_recent_tokens,
                        "tool_count": self.tool_catalog.len(),
                        "log_path": logger.path(),
                    }),
                );
                self.logger = Some(logger);
                self.state_store = Some(state_store);
                self.compressor = compressor;
                self.token_estimator = token_estimator;
                self.initial = Some(initial);
                self.persist_state()?;
                if restored_history_needs_continuation(&self.history) {
                    self.pending_continuation = Some(PendingContinuation::CurrentHistory);
                    self.log_warn(
                        "restored_unfinished_turn",
                        serde_json::json!({
                            "history_len": self.history.len(),
                            "all_messages_len": self.all_messages.len(),
                        }),
                    );
                    self.emit_turn_failed(
                        "session restored with an unfinished turn; ask the user whether to continue processing".to_string(),
                        true,
                    )?;
                }
                Ok(())
            }
            SessionRequest::Shutdown => {
                self.log_info("shutdown_requested", serde_json::json!({}));
                self.shutdown = true;
                Ok(())
            }
            SessionRequest::CancelTurn { reason } => self.handle_cancel_turn(reason),
            SessionRequest::ContinueTurn { reason } => self.handle_continue_turn(reason),
            SessionRequest::QuerySessionView { query_id, payload } => {
                self.handle_query_session_view(query_id, payload)
            }
            other => self.emit(SessionEvent::ControlRejected {
                reason: "control command is not implemented by SessionActor yet".to_string(),
                payload: serde_json::to_value(other)
                    .unwrap_or_else(|_| serde_json::json!({"error": "serialize control failed"})),
            }),
        }
    }

    fn handle_cancel_turn(&mut self, reason: Option<String>) -> Result<(), SessionActorError> {
        self.request_active_tool_interrupt(
            ToolBatchInterrupt::Cancel,
            reason.unwrap_or_else(|| "user_cancelled".to_string()),
        )
    }

    fn request_active_tool_interrupt(
        &mut self,
        interrupt: ToolBatchInterrupt,
        reason: String,
    ) -> Result<(), SessionActorError> {
        let Some(active) = self.active_tool_batch.as_mut() else {
            return self.emit(SessionEvent::ControlRejected {
                reason: "no active interruptible turn".to_string(),
                payload: serde_json::json!({"command": "cancel_turn"}),
            });
        };
        if active.interrupt.is_some() {
            return Ok(());
        }

        self.tool_executor
            .interrupt(&active.handle)
            .map_err(|error| SessionActorError::Tool(error.to_string()))?;
        active.interrupt = Some(interrupt);
        let turn_id = active.turn_id.clone();
        let batch_id = active.handle.batch_id.clone();
        self.log_info(
            "tool_batch_interrupt_requested",
            serde_json::json!({
                "turn_id": turn_id,
                "batch_id": batch_id,
                "reason": match interrupt {
                    ToolBatchInterrupt::Cancel => "cancel",
                    ToolBatchInterrupt::SupersededByUserMessage => "superseded_by_user_message",
                },
                "detail": reason,
            }),
        );
        if interrupt == ToolBatchInterrupt::SupersededByUserMessage {
            return Ok(());
        }

        self.emit(SessionEvent::Progress {
            message: format!("interrupt requested for tool batch {batch_id}"),
        })
    }

    fn has_pending_user_message_interrupt(&self) -> bool {
        self.active_tool_batch
            .as_ref()
            .is_some_and(|active| active.interrupt.is_none())
            && self
                .pending_data
                .iter()
                .any(|request| matches!(request, SessionRequest::EnqueueUserMessage { .. }))
    }

    fn handle_query_session_view(
        &self,
        query_id: String,
        payload: serde_json::Value,
    ) -> Result<(), SessionActorError> {
        self.emit(SessionEvent::SessionViewResult {
            query_id,
            payload: self.session_view_payload(payload),
        })
    }

    fn session_view_payload(&self, payload: serde_json::Value) -> serde_json::Value {
        match payload.get("type").and_then(serde_json::Value::as_str) {
            Some("transcript_page") => self.transcript_page_payload(&payload),
            Some("message_detail") => self.message_detail_payload(&payload),
            Some("live_state") => self.live_state_payload(),
            Some(query_type) => serde_json::json!({
                "type": query_type,
                "error": format!("unsupported session view query type {query_type}"),
            }),
            None => serde_json::json!({
                "type": "error",
                "error": "missing session view query type",
                "query": payload,
            }),
        }
    }

    fn transcript_page_payload(&self, payload: &serde_json::Value) -> serde_json::Value {
        let (source, messages) = self.messages_for_view_payload(payload);
        let Some(messages) = messages else {
            return invalid_source_payload(source);
        };
        let offset = usize_field(payload, "offset", 0);
        let limit = usize_field(payload, "limit", 50).min(200);
        let total = messages.len();
        let start = offset.min(total);
        let end = start.saturating_add(limit).min(total);
        serde_json::json!({
            "type": "transcript_page",
            "source": source,
            "offset": offset,
            "limit": limit,
            "total": total,
            "messages": messages[start..end],
        })
    }

    fn message_detail_payload(&self, payload: &serde_json::Value) -> serde_json::Value {
        let (source, messages) = self.messages_for_view_payload(payload);
        let Some(messages) = messages else {
            return invalid_source_payload(source);
        };
        let Some(index) = payload
            .get("index")
            .and_then(serde_json::Value::as_u64)
            .map(|value| value as usize)
        else {
            return serde_json::json!({
                "type": "message_detail",
                "source": source,
                "error": "missing numeric message index",
                "total": messages.len(),
            });
        };
        let Some(message) = messages.get(index) else {
            return serde_json::json!({
                "type": "message_detail",
                "source": source,
                "index": index,
                "error": "message index out of range",
                "total": messages.len(),
            });
        };
        serde_json::json!({
            "type": "message_detail",
            "source": source,
            "index": index,
            "message": message,
        })
    }

    fn live_state_payload(&self) -> serde_json::Value {
        let initial = self.initial.as_ref();
        serde_json::json!({
            "type": "live_state",
            "initialized": initial.is_some(),
            "session_id": initial.map(|initial| initial.session_id.as_str()),
            "session_type": initial.map(|initial| initial.session_type),
            "shutdown": self.shutdown,
            "history_len": self.history.len(),
            "all_messages_len": self.all_messages.len(),
            "pending_control_len": self.pending_control.len(),
            "pending_data_len": self.pending_data.len(),
            "pending_tool_completion_len": self.pending_tool_completions.len(),
            "active_tool_batch": self.active_tool_batch.as_ref().map(|active| serde_json::json!({
                "turn_id": active.turn_id,
                "batch_id": active.handle.batch_id,
                "step_index": active.step_index,
            })),
            "can_continue": self.pending_continuation.is_some(),
        })
    }

    fn messages_for_view_payload<'a, 'b>(
        &'a self,
        payload: &'b serde_json::Value,
    ) -> (&'b str, Option<&'a [ChatMessage]>) {
        let source = payload
            .get("source")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("current");
        let messages = match source {
            "current" => Some(self.history.as_slice()),
            "all" => Some(self.all_messages.as_slice()),
            _ => None,
        };
        (source, messages)
    }

    fn handle_continue_turn(&mut self, reason: Option<String>) -> Result<(), SessionActorError> {
        if self.initial.is_none() {
            return Err(SessionActorError::MissingInitial);
        }
        if self.active_tool_batch.is_some() {
            return self.emit(SessionEvent::ControlRejected {
                reason: "cannot continue while a tool batch is running".to_string(),
                payload: serde_json::json!({"command": "continue_turn", "reason": reason}),
            });
        }

        let Some(continuation) = self.pending_continuation.take() else {
            return self.emit(SessionEvent::ControlRejected {
                reason: "no recoverable failed turn is pending".to_string(),
                payload: serde_json::json!({"command": "continue_turn", "reason": reason}),
            });
        };

        self.log_info(
            "continue_turn_requested",
            serde_json::json!({
                "reason": reason,
                "mode": pending_continuation_kind(&continuation),
            }),
        );

        match continuation {
            PendingContinuation::CurrentHistory => self.continue_turn_from_history(),
            PendingContinuation::DataRequest(request) => self.run_turn_from_data_request(request),
        }
    }

    fn run_turn_from_data_request(
        &mut self,
        request: SessionRequest,
    ) -> Result<(), SessionActorError> {
        if self.initial.is_none() {
            self.log_error(
                "turn_rejected",
                serde_json::json!({"reason": "missing_initial"}),
            );
            return Err(SessionActorError::MissingInitial);
        }

        self.pending_continuation = None;
        let retry_request = request.clone();
        let input_message = match request {
            SessionRequest::EnqueueUserMessage { message } => {
                if let Err(error) = self.append_runtime_synthetic_messages(&message) {
                    return self.finish_turn_error(
                        "pre_turn",
                        error,
                        Some(PendingContinuation::DataRequest(retry_request)),
                    );
                }
                message
            }
            SessionRequest::EnqueueActorMessage { message } => message,
            _ => return Err(SessionActorError::UnexpectedDataRequest),
        };

        let turn_id = self.allocate_turn_id();
        let turn_number = self.next_turn_id.saturating_sub(1);
        self.log_info(
            "turn_started",
            serde_json::json!({
                "turn_id": &turn_id,
                "input_role": &input_message.role,
                "input_items": input_message.data.len(),
            }),
        );
        self.emit(SessionEvent::TurnStarted {
            turn_id: turn_id.clone(),
        })?;
        if let Err(error) = self.append_history_message("input", input_message) {
            return self.finish_turn_error(
                &turn_id,
                error,
                Some(PendingContinuation::DataRequest(retry_request)),
            );
        }

        if let Err(error) = self.run_model_tool_loop(&turn_id, turn_number, 0) {
            return self.finish_turn_error(
                &turn_id,
                error,
                Some(PendingContinuation::CurrentHistory),
            );
        }

        Ok(())
    }

    fn continue_turn_from_history(&mut self) -> Result<(), SessionActorError> {
        if self.history.is_empty() {
            return self.emit(SessionEvent::ControlRejected {
                reason: "cannot continue without existing session history".to_string(),
                payload: serde_json::json!({"command": "continue_turn"}),
            });
        }

        let turn_id = self.allocate_turn_id();
        let turn_number = self.next_turn_id.saturating_sub(1);
        self.log_info(
            "turn_continued",
            serde_json::json!({
                "turn_id": &turn_id,
                "history_len": self.history.len(),
            }),
        );
        self.emit(SessionEvent::TurnStarted {
            turn_id: turn_id.clone(),
        })?;

        if let Err(error) = self.run_model_tool_loop(&turn_id, turn_number, 0) {
            return self.finish_turn_error(
                &turn_id,
                error,
                Some(PendingContinuation::CurrentHistory),
            );
        }

        Ok(())
    }

    fn finish_turn_error(
        &mut self,
        turn_id: &str,
        error: SessionActorError,
        continuation: Option<PendingContinuation>,
    ) -> Result<(), SessionActorError> {
        self.log_error(
            "turn_failed",
            serde_json::json!({"turn_id": turn_id, "error": error.to_string()}),
        );
        let can_continue = continuation.is_some() && error.is_recoverable_turn_error();
        if can_continue {
            self.pending_continuation = continuation;
        }
        self.emit_turn_failed(error.to_string(), can_continue)?;
        if can_continue {
            Ok(())
        } else {
            Err(error)
        }
    }

    fn run_model_tool_loop(
        &mut self,
        turn_id: &str,
        turn_number: u64,
        start_step_index: usize,
    ) -> Result<(), SessionActorError> {
        for step_index in start_step_index..self.max_model_steps_per_turn {
            self.log_info(
                "provider_request_started",
                serde_json::json!({
                    "turn_id": turn_id,
                    "step_index": step_index,
                    "history_len": self.history.len(),
                    "provider_type": &self.model_config.provider_type,
                    "model_name": &self.model_config.model_name,
                }),
            );
            let system_prompt = self
                .initial
                .as_ref()
                .map(|initial| system_prompt_for_initial(initial, &self.runtime_metadata_state))
                .ok_or(SessionActorError::MissingInitial)?;
            let model_message = {
                let mut request_too_large_attempts = 0usize;
                loop {
                    let normalized_history =
                        normalize_messages_for_model(&self.history, &self.model_config);
                    let provider_history = self
                        .provider
                        .normalize_messages_for_provider(&self.model_config, &normalized_history);
                    if let Some(estimated_tokens) =
                        self.provider_request_exceeds_context(&provider_history)?
                    {
                        if request_too_large_attempts >= REQUEST_TOO_LARGE_PRUNE_MAX_ATTEMPTS {
                            return Err(SessionActorError::Provider(format!(
                                "estimated provider request tokens {estimated_tokens} exceed model context {} after {REQUEST_TOO_LARGE_PRUNE_MAX_ATTEMPTS} prune attempts",
                                self.model_config.token_max_context
                            )));
                        }
                        request_too_large_attempts += 1;
                        let error = format!(
                            "estimated provider request tokens {estimated_tokens} exceed model context {}",
                            self.model_config.token_max_context
                        );
                        if self.prune_history_after_request_too_large(
                            "provider_request_preflight",
                            Some(turn_id),
                            Some(step_index),
                            &error,
                        )? {
                            continue;
                        }
                        return Err(SessionActorError::Provider(error));
                    }

                    let send_result = {
                        let tools = self
                            .tool_catalog
                            .iter()
                            .map(|(_, tool)| tool)
                            .collect::<Vec<_>>();
                        let tools = self
                            .provider
                            .filter_tools_for_provider(&self.model_config, tools);
                        let request = ProviderRequest::new(&provider_history)
                            .with_system_prompt(Some(system_prompt.as_str()))
                            .with_tools(tools);
                        self.provider.send(&self.model_config, request)
                    };
                    match send_result {
                        Ok(message) => break message,
                        Err(error)
                            if error.is_request_too_large()
                                && request_too_large_attempts
                                    < REQUEST_TOO_LARGE_PRUNE_MAX_ATTEMPTS =>
                        {
                            request_too_large_attempts += 1;
                            if self.prune_history_after_request_too_large(
                                "provider_request",
                                Some(turn_id),
                                Some(step_index),
                                &error.to_string(),
                            )? {
                                continue;
                            }
                            return Err(SessionActorError::Provider(error.to_string()));
                        }
                        Err(error) => return Err(SessionActorError::Provider(error.to_string())),
                    }
                }
            };

            let tool_calls = collect_tool_calls(&model_message);
            self.log_info(
                "provider_response_received",
                serde_json::json!({
                    "turn_id": turn_id,
                    "step_index": step_index,
                    "message_items": model_message.data.len(),
                    "tool_calls": tool_calls.iter().map(|tool| &tool.tool_name).collect::<Vec<_>>(),
                }),
            );
            self.append_history_message("model_response", model_message.clone())?;

            if tool_calls.is_empty() {
                self.log_info(
                    "turn_completed",
                    serde_json::json!({
                        "turn_id": turn_id,
                        "final_items": model_message.data.len(),
                    }),
                );
                self.emit(SessionEvent::TurnCompleted {
                    message: model_message,
                })?;
                self.mark_turn_returned(turn_number);
                return Ok(());
            }

            let batch = self.build_tool_batch(turn_id, tool_calls)?;
            self.log_info(
                "tool_batch_started",
                serde_json::json!({
                    "turn_id": turn_id,
                    "batch_id": &batch.batch_id,
                    "operations": batch.operations.len(),
                }),
            );
            self.emit(SessionEvent::Progress {
                message: format!("running tool batch {}", batch.batch_id),
            })?;

            let handle = self
                .tool_executor
                .start(batch, self.tool_completion_tx.clone())
                .map_err(|error| SessionActorError::Tool(error.to_string()))?;
            self.active_tool_batch = Some(ActiveToolBatch {
                turn_id: turn_id.to_string(),
                turn_number,
                step_index,
                handle,
                interrupt: None,
            });
            return Ok(());
        }

        Err(SessionActorError::ModelStepLimitExceeded(
            self.max_model_steps_per_turn,
        ))
    }

    fn build_tool_batch(
        &mut self,
        turn_id: &str,
        tool_calls: Vec<super::ToolCallItem>,
    ) -> Result<ToolBatch, SessionActorError> {
        let batch_id = self.allocate_batch_id(turn_id);
        let mut operations = Vec::with_capacity(tool_calls.len());

        for tool_call in tool_calls {
            let operation = match self.tool_catalog.get(&tool_call.tool_name) {
                Some(definition) => match &definition.backend {
                    ToolBackend::ConversationBridge { action } => {
                        ToolExecutionOp::ConversationBridge(ConversationBridgeRequest {
                            request_id: format!("{}_{}", batch_id, tool_call.tool_call_id),
                            tool_call_id: tool_call.tool_call_id.clone(),
                            tool_name: tool_call.tool_name.clone(),
                            action: action.clone(),
                            payload: parse_tool_arguments(&tool_call.arguments.text),
                        })
                    }
                    ToolBackend::Local if tool_call.tool_name == "skill_load" => self
                        .build_skill_load_operation(tool_call.clone())
                        .unwrap_or(ToolExecutionOp::LocalTool(tool_call)),
                    ToolBackend::ProviderBacked { kind } => self
                        .build_provider_backed_operation(tool_call.clone(), *kind)
                        .unwrap_or(ToolExecutionOp::LocalTool(tool_call)),
                    ToolBackend::Local if tool_call.tool_name == "web_search" => self
                        .build_web_search_operation(tool_call.clone())
                        .unwrap_or(ToolExecutionOp::LocalTool(tool_call)),
                    ToolBackend::Local => ToolExecutionOp::LocalTool(tool_call),
                },
                None => ToolExecutionOp::LocalTool(tool_call),
            };
            operations.push(operation);
        }

        Ok(ToolBatch::new(batch_id, operations))
    }

    fn build_web_search_operation(
        &self,
        tool_call: super::ToolCallItem,
    ) -> Option<ToolExecutionOp> {
        let search_tool_model = self.initial.as_ref()?.search_tool_model.clone()?;
        Some(ToolExecutionOp::WebSearch {
            tool_call,
            model_config: search_tool_model,
        })
    }

    fn build_provider_backed_operation(
        &self,
        tool_call: super::ToolCallItem,
        kind: super::ProviderBackedToolKind,
    ) -> Option<ToolExecutionOp> {
        let initial = self.initial.as_ref()?;
        let model_config = match kind {
            super::ProviderBackedToolKind::ImageAnalysis => initial.image_tool_model.clone()?,
            super::ProviderBackedToolKind::PdfAnalysis => initial.pdf_tool_model.clone()?,
            super::ProviderBackedToolKind::AudioAnalysis => initial.audio_tool_model.clone()?,
            super::ProviderBackedToolKind::ImageGeneration => initial
                .image_generation_tool_model
                .clone()
                .unwrap_or_else(|| self.model_config.clone()),
        };
        Some(ToolExecutionOp::ProviderBacked {
            tool_call,
            kind,
            model_config,
        })
    }

    fn build_skill_load_operation(
        &self,
        tool_call: super::ToolCallItem,
    ) -> Option<ToolExecutionOp> {
        let arguments =
            serde_json::from_str::<serde_json::Value>(&tool_call.arguments.text).ok()?;
        let skill_name = arguments
            .get("skill_name")
            .or_else(|| arguments.get("name"))
            .and_then(serde_json::Value::as_str)?;
        let skill = self.runtime_metadata_state.skill_observation(skill_name)?;
        Some(ToolExecutionOp::SkillLoad { tool_call, skill })
    }

    fn handle_ready_tool_completion(&mut self) -> Result<SessionActorStep, SessionActorError> {
        let Some(active) = self.active_tool_batch.as_ref() else {
            return Ok(SessionActorStep::Idle);
        };
        let Some(completion) = self.pending_tool_completions.pop_front() else {
            return Ok(SessionActorStep::WaitingToolBatch);
        };
        if completion.batch_id != active.handle.batch_id {
            return Err(SessionActorError::Tool(format!(
                "unexpected tool batch completion {}, expected {}",
                completion.batch_id, active.handle.batch_id
            )));
        }

        let active = self
            .active_tool_batch
            .take()
            .expect("active tool batch should still exist");
        self.tool_executor
            .finish(&active.handle.batch_id)
            .map_err(|error| SessionActorError::Tool(error.to_string()))?;
        let tool_message = completion
            .result
            .map_err(|error| SessionActorError::Tool(error.to_string()))?;
        self.log_info(
            "tool_batch_completed",
            serde_json::json!({
                "turn_id": &active.turn_id,
                "batch_id": &active.handle.batch_id,
                "result_items": tool_message.data.len(),
            }),
        );
        self.mark_loaded_skills_from_message(&tool_message, active.turn_number)?;
        let synthetic_media_message = synthetic_media_message_from_tool_results(&tool_message);
        self.append_history_message("tool_result", tool_message)?;
        if let Some(message) = synthetic_media_message {
            self.append_history_message("tool_media_context", message)?;
        }
        if active.interrupt == Some(ToolBatchInterrupt::SupersededByUserMessage) {
            self.log_info(
                "tool_batch_superseded_by_user_message",
                serde_json::json!({
                    "turn_id": &active.turn_id,
                    "batch_id": &active.handle.batch_id,
                }),
            );
            self.mark_turn_returned(active.turn_number);
            return Ok(SessionActorStep::ProcessedData);
        }
        if let Err(error) =
            self.run_model_tool_loop(&active.turn_id, active.turn_number, active.step_index + 1)
        {
            self.finish_turn_error(
                &active.turn_id,
                error,
                Some(PendingContinuation::CurrentHistory),
            )?;
        }
        Ok(if self.active_tool_batch.is_some() {
            SessionActorStep::WaitingToolBatch
        } else {
            SessionActorStep::ProcessedData
        })
    }

    fn emit(&self, event: SessionEvent) -> Result<(), SessionActorError> {
        self.log_info("event_emitted", session_event_summary(&event));
        self.event_sink
            .emit(event)
            .map_err(SessionActorError::Event)
    }

    fn emit_turn_failed(&self, error: String, can_continue: bool) -> Result<(), SessionActorError> {
        self.emit(SessionEvent::TurnFailed {
            error,
            can_continue,
        })
    }

    fn append_history_message(
        &mut self,
        phase: &str,
        message: ChatMessage,
    ) -> Result<(), SessionActorError> {
        let Some(compressor) = self.compressor.clone() else {
            self.all_messages.push(message.clone());
            self.history.push(message);
            self.persist_state_if_history_closed(phase)?;
            return Ok(());
        };

        self.all_messages.push(message.clone());
        let system_prompt = self
            .initial
            .as_ref()
            .map(|initial| system_prompt_for_initial(initial, &self.runtime_metadata_state));
        let report = {
            let mut request_too_large_attempts = 0usize;
            loop {
                match compressor.append_with_compression(
                    &mut self.history,
                    message.clone(),
                    self.provider.as_ref(),
                    &self.model_config,
                    system_prompt.as_deref(),
                ) {
                    Ok(report) => break report,
                    Err(error)
                        if compression_error_is_request_too_large(&error)
                            && request_too_large_attempts
                                < REQUEST_TOO_LARGE_PRUNE_MAX_ATTEMPTS =>
                    {
                        request_too_large_attempts += 1;
                        if self.prune_history_after_request_too_large(
                            phase,
                            None,
                            None,
                            &error.to_string(),
                        )? {
                            continue;
                        }
                        return Err(SessionActorError::Compression(error.to_string()));
                    }
                    Err(error) => return Err(SessionActorError::Compression(error.to_string())),
                }
            }
        };
        self.log_compression_report(phase, &report);
        if report.compressed {
            self.runtime_metadata_state
                .promote_notified_components_to_system_snapshot();
        }
        self.persist_state_if_history_closed(phase)?;
        Ok(())
    }

    fn append_runtime_synthetic_messages(
        &mut self,
        input_message: &ChatMessage,
    ) -> Result<(), SessionActorError> {
        let notices = self
            .runtime_metadata_state
            .observe_for_user_turn_from_workspace(
                &self
                    .workspace_root()
                    .map_err(SessionActorError::RuntimeMetadata)?,
                self.initial
                    .as_ref()
                    .map(|initial| remote_aliases_prompt_for_mode(&initial.tool_remote_mode))
                    .unwrap_or_default(),
            )
            .map_err(SessionActorError::RuntimeMetadata)?;
        for notice in notices {
            self.append_history_message(
                "runtime_synthetic",
                ChatMessage::new(
                    ChatRole::User,
                    vec![ChatMessageItem::Context(ContextItem { text: notice })],
                ),
            )?;
        }
        if let Some(notice) = render_incoming_user_metadata_notice(input_message) {
            self.append_history_message(
                "runtime_synthetic",
                ChatMessage::new(
                    ChatRole::User,
                    vec![ChatMessageItem::Context(ContextItem { text: notice })],
                ),
            )?;
        }
        Ok(())
    }

    fn mark_loaded_skills_from_message(
        &mut self,
        message: &ChatMessage,
        turn_number: u64,
    ) -> Result<(), SessionActorError> {
        let skill_names = loaded_skill_names_from_message(message);
        if skill_names.is_empty() {
            return Ok(());
        }
        self.runtime_metadata_state
            .mark_loaded_skills(&skill_names, turn_number);
        Ok(())
    }

    fn restore_persisted_state(
        &mut self,
        saved: SessionActorPersistedState,
        incoming_initial: SessionInitial,
    ) -> Result<(), SessionActorError> {
        self.history = saved.current_messages;
        self.all_messages = saved.all_messages;
        self.next_turn_id = saved.next_turn_id.max(1);
        self.next_batch_id = saved.next_batch_id.max(1);
        self.runtime_metadata_state = saved.runtime_metadata_state;
        if self.runtime_metadata_state.prompt_components.is_empty() {
            self.runtime_metadata_state
                .initialize_from_workspace(
                    &self
                        .workspace_root()
                        .map_err(SessionActorError::RuntimeMetadata)?,
                    remote_aliases_prompt_for_mode(&incoming_initial.tool_remote_mode),
                )
                .map_err(SessionActorError::RuntimeMetadata)?;
        }
        Ok(())
    }

    fn persist_state(&self) -> Result<(), SessionActorError> {
        let (Some(store), Some(initial)) = (&self.state_store, &self.initial) else {
            return Ok(());
        };
        store
            .save(&SessionActorPersistedState {
                version: 1,
                initial: initial.clone(),
                all_messages: self.all_messages.clone(),
                current_messages: self.history.clone(),
                next_turn_id: self.next_turn_id,
                next_batch_id: self.next_batch_id,
                runtime_metadata_state: self.runtime_metadata_state.clone(),
            })
            .map_err(SessionActorError::Persistence)
    }

    fn persist_state_if_history_closed(&self, phase: &str) -> Result<(), SessionActorError> {
        let open_tool_call_count = count_unclosed_tool_calls(&self.history);
        if open_tool_call_count > 0 {
            self.log_info(
                "session_state_persist_skipped",
                serde_json::json!({
                    "phase": phase,
                    "reason": "unclosed_tool_calls",
                    "open_tool_call_count": open_tool_call_count,
                    "history_len": self.history.len(),
                    "all_messages_len": self.all_messages.len(),
                }),
            );
            return Ok(());
        }
        self.persist_state()
    }

    fn log_compression_report(&self, phase: &str, report: &CompressionReport) {
        if !report.compressed {
            return;
        }

        self.log_info(
            "history_compressed",
            serde_json::json!({
                "phase": phase,
                "estimated_tokens_before": report.estimated_tokens_before,
                "estimated_tokens_after": report.estimated_tokens_after,
                "threshold_tokens": report.threshold_tokens,
                "retained_message_count": report.retained_message_count,
                "compressed_message_count": report.compressed_message_count,
                "history_len": self.history.len(),
            }),
        );
    }

    fn mark_turn_returned(&mut self, turn_number: u64) {
        self.last_agent_returned_at = Some(Instant::now());
        self.last_completed_turn_number = self.last_completed_turn_number.max(turn_number);
    }

    fn idle_compaction_delay(&self) -> Option<Duration> {
        self.initial.as_ref()?;
        self.compressor.as_ref()?;
        if self.shutdown
            || self.active_tool_batch.is_some()
            || !self.pending_control.is_empty()
            || !self.pending_data.is_empty()
            || !self.pending_tool_completions.is_empty()
            || self.last_completed_turn_number <= self.last_idle_compaction_turn_number
            || count_unclosed_tool_calls(&self.history) > 0
        {
            return None;
        }

        let returned_at = self.last_agent_returned_at?;
        let threshold = idle_compaction_threshold(&self.model_config)?;
        let elapsed = returned_at.elapsed();
        Some(threshold.saturating_sub(elapsed))
    }

    fn try_run_idle_compaction(&mut self) -> Result<bool, SessionActorError> {
        if !matches!(self.idle_compaction_delay(), Some(delay) if delay.is_zero()) {
            return Ok(false);
        }

        let Some(compressor) = self.compressor.clone() else {
            return Ok(false);
        };

        self.log_info(
            "idle_compaction_started",
            serde_json::json!({
                "history_len": self.history.len(),
                "all_messages_len": self.all_messages.len(),
                "last_completed_turn_number": self.last_completed_turn_number,
            }),
        );

        let threshold_tokens =
            idle_compaction_token_threshold(&self.model_config, self.initial.as_ref());
        let system_prompt = self
            .initial
            .as_ref()
            .map(|initial| system_prompt_for_initial(initial, &self.runtime_metadata_state));
        let mut request_too_large_attempts = 0usize;
        let report = loop {
            match compressor.compact_if_needed_with_threshold(
                &mut self.history,
                self.provider.as_ref(),
                &self.model_config,
                system_prompt.as_deref(),
                threshold_tokens,
            ) {
                Ok(report) => break Ok(report),
                Err(error)
                    if compression_error_is_request_too_large(&error)
                        && request_too_large_attempts < REQUEST_TOO_LARGE_PRUNE_MAX_ATTEMPTS =>
                {
                    request_too_large_attempts += 1;
                    if self.prune_history_after_request_too_large(
                        "idle_compaction",
                        None,
                        None,
                        &error.to_string(),
                    )? {
                        continue;
                    }
                    break Err(error);
                }
                Err(error) => break Err(error),
            }
        };

        match report {
            Ok(report) => {
                self.log_compression_report("idle", &report);
                if report.compressed {
                    self.runtime_metadata_state
                        .promote_notified_components_to_system_snapshot();
                    self.persist_state_if_history_closed("idle_compaction")?;
                }
                self.last_idle_compaction_turn_number = self.last_completed_turn_number;
                self.log_info(
                    "idle_compaction_finished",
                    serde_json::json!({
                        "compressed": report.compressed,
                        "estimated_tokens_before": report.estimated_tokens_before,
                        "estimated_tokens_after": report.estimated_tokens_after,
                        "threshold_tokens": report.threshold_tokens,
                        "history_len": self.history.len(),
                    }),
                );
                Ok(report.compressed)
            }
            Err(error) => {
                self.last_idle_compaction_turn_number = self.last_completed_turn_number;
                self.log_error(
                    "idle_compaction_failed",
                    serde_json::json!({"error": error.to_string()}),
                );
                self.emit(SessionEvent::Progress {
                    message: format!("idle context compression failed: {error}"),
                })?;
                Ok(false)
            }
        }
    }

    fn prune_history_after_request_too_large(
        &mut self,
        phase: &str,
        turn_id: Option<&str>,
        step_index: Option<usize>,
        error: &str,
    ) -> Result<bool, SessionActorError> {
        let before_len = self.history.len();
        let Some(prune_start) = request_too_large_prune_start(&self.history) else {
            self.log_warn(
                "request_too_large_history_prune_failed",
                serde_json::json!({
                    "phase": phase,
                    "turn_id": turn_id,
                    "step_index": step_index,
                    "history_len": before_len,
                    "error": error,
                }),
            );
            return Ok(false);
        };

        self.history.drain(..prune_start);
        let retained_len = self.history.len();
        self.log_warn(
            "request_too_large_history_pruned",
            serde_json::json!({
                "phase": phase,
                "turn_id": turn_id,
                "step_index": step_index,
                "dropped_message_count": prune_start,
                "retained_message_count": retained_len,
                "history_len_before": before_len,
                "history_len_after": retained_len,
                "data_loss": true,
                "bug": true,
                "error": error,
            }),
        );
        self.emit(SessionEvent::Progress {
            message: format!(
                "警告：上游拒绝了本轮请求，原因是请求体过大。系统已从当前上下文中丢弃较早的 {prune_start} 条消息并自动重试；这代表发生了上下文数据丢失，应按 bug 处理并排查。"
            ),
        })?;
        self.persist_state_if_history_closed("request_too_large_prune")?;
        Ok(true)
    }

    fn provider_request_exceeds_context(
        &self,
        messages: &[ChatMessage],
    ) -> Result<Option<u64>, SessionActorError> {
        if self.model_config.token_max_context == 0 {
            return Ok(None);
        }
        let Some(estimator) = self.token_estimator.as_ref() else {
            return Ok(None);
        };
        let estimate = estimator
            .estimate(messages)
            .map_err(|error| SessionActorError::Compression(error.to_string()))?;
        if estimate.total_tokens >= self.model_config.token_max_context {
            return Ok(Some(estimate.total_tokens));
        }
        Ok(None)
    }

    fn log_info(&self, event: &str, data: serde_json::Value) {
        if let Some(logger) = &self.logger {
            logger.info(event, data);
        }
    }

    fn log_warn(&self, event: &str, data: serde_json::Value) {
        if let Some(logger) = &self.logger {
            logger.warn(event, data);
        }
    }

    fn log_error(&self, event: &str, data: serde_json::Value) {
        if let Some(logger) = &self.logger {
            logger.error(event, data);
        }
    }

    fn allocate_turn_id(&mut self) -> String {
        let id = format!("turn_{}", self.next_turn_id);
        self.next_turn_id = self.next_turn_id.saturating_add(1);
        id
    }

    fn allocate_batch_id(&mut self, turn_id: &str) -> String {
        let id = format!("{}_batch_{}", turn_id, self.next_batch_id);
        self.next_batch_id = self.next_batch_id.saturating_add(1);
        id
    }

    fn workspace_root(&self) -> Result<PathBuf, String> {
        env::current_dir().map_err(|error| format!("failed to resolve cwd: {error}"))
    }
}

pub trait SessionActorEventSink: Send + Sync + 'static {
    fn emit(&self, event: SessionEvent) -> Result<(), String>;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionActorStep {
    Idle,
    ProcessedControl,
    ProcessedData,
    ProcessedIdleCompaction,
    WaitingToolBatch,
    Shutdown,
}

#[derive(Debug, Error)]
pub enum SessionActorError {
    #[error("session actor mailbox failed: {0}")]
    Mailbox(String),
    #[error("session actor event sink failed: {0}")]
    Event(String),
    #[error("provider failed: {0}")]
    Provider(String),
    #[error("tool batch failed: {0}")]
    Tool(String),
    #[error("tool catalog failed: {0}")]
    ToolCatalog(String),
    #[error("session actor logging failed: {0}")]
    Logging(String),
    #[error("session actor compression failed: {0}")]
    Compression(String),
    #[error("session actor persistence failed: {0}")]
    Persistence(String),
    #[error("session actor runtime metadata failed: {0}")]
    RuntimeMetadata(String),
    #[error("data mailbox head changed while starting a turn")]
    DataHeadChanged,
    #[error("unexpected request in data mailbox")]
    UnexpectedDataRequest,
    #[error("session initial message has not been applied")]
    MissingInitial,
    #[error("session actor exceeded step limit {0}")]
    StepLimitExceeded(usize),
    #[error("model/tool loop exceeded max model steps {0}")]
    ModelStepLimitExceeded(usize),
}

impl SessionActorError {
    fn is_recoverable_turn_error(&self) -> bool {
        matches!(
            self,
            Self::Provider(_)
                | Self::Compression(_)
                | Self::Tool(_)
                | Self::RuntimeMetadata(_)
                | Self::ModelStepLimitExceeded(_)
        )
    }
}

fn build_session_token_estimator(model_config: &ModelConfig) -> Result<TokenEstimator, String> {
    let file_resolver = HuggingFaceFileResolver::new().map_err(|error| error.to_string())?;
    TokenEstimator::from_model_config(model_config, &file_resolver)
        .map_err(|error| error.to_string())
}

fn build_session_compressor(
    threshold_tokens: Option<u64>,
    initial: &SessionInitial,
    token_estimator: Option<&TokenEstimator>,
) -> Result<Option<SessionCompressor>, SessionActorError> {
    let Some(threshold_tokens) = threshold_tokens else {
        return Ok(None);
    };

    let retain_recent_tokens = initial
        .compression_retain_recent_tokens
        .unwrap_or_else(|| default_retain_recent_tokens(threshold_tokens));
    let estimator = token_estimator.cloned().ok_or_else(|| {
        SessionActorError::Compression("token estimator is unavailable".to_string())
    })?;
    let compressor = SessionCompressor::new(estimator, threshold_tokens, retain_recent_tokens)
        .map_err(|error| SessionActorError::Compression(error.to_string()))?;
    Ok(Some(compressor))
}

fn active_compression_threshold_tokens(
    model_config: &ModelConfig,
    initial: &SessionInitial,
) -> Option<u64> {
    let configured_threshold = initial.compression_threshold_tokens?;
    Some(
        configured_threshold
            .min(model_context_ratio_threshold(
                model_config,
                ACTIVE_COMPRESSION_THRESHOLD_RATIO,
            ))
            .max(1),
    )
}

fn model_context_ratio_threshold(model_config: &ModelConfig, ratio: f64) -> u64 {
    ((model_config.token_max_context as f64) * ratio)
        .floor()
        .max(1.0) as u64
}

fn default_retain_recent_tokens(threshold_tokens: u64) -> u64 {
    if threshold_tokens <= 2 {
        return 1;
    }
    (threshold_tokens / 4).max(512).min(threshold_tokens - 1)
}

fn idle_compaction_threshold(model_config: &ModelConfig) -> Option<Duration> {
    const CACHE_EXPIRY_LEAD_TIME_SECS: u64 = 30;
    if model_config.cache_timeout <= CACHE_EXPIRY_LEAD_TIME_SECS {
        return None;
    }
    Some(Duration::from_secs(
        model_config.cache_timeout - CACHE_EXPIRY_LEAD_TIME_SECS,
    ))
}

fn idle_compaction_token_threshold(
    model_config: &ModelConfig,
    initial: Option<&SessionInitial>,
) -> u64 {
    let idle_threshold = model_context_ratio_threshold(model_config, IDLE_COMPACTION_MIN_RATIO);
    initial
        .and_then(|initial| initial.compression_threshold_tokens)
        .map(|active_threshold| active_threshold.min(idle_threshold).max(1))
        .unwrap_or(idle_threshold)
}

fn collect_tool_calls(message: &ChatMessage) -> Vec<super::ToolCallItem> {
    message
        .data
        .iter()
        .filter_map(|item| match item {
            ChatMessageItem::ToolCall(tool_call) => Some(tool_call.clone()),
            _ => None,
        })
        .collect()
}

fn loaded_skill_names_from_message(message: &ChatMessage) -> Vec<String> {
    let mut names = std::collections::BTreeSet::new();
    for item in &message.data {
        let ChatMessageItem::ToolResult(result) = item else {
            continue;
        };
        if result.tool_name != "skill_load" {
            continue;
        }
        let Some(context) = result.result.context.as_ref() else {
            continue;
        };
        if let Ok(value) = serde_json::from_str::<serde_json::Value>(&context.text) {
            if let Some(name) = value
                .get("name")
                .or_else(|| value.get("skill_name"))
                .and_then(serde_json::Value::as_str)
            {
                names.insert(name.to_string());
            }
        }
    }
    names.into_iter().collect()
}

fn count_unclosed_tool_calls(messages: &[ChatMessage]) -> usize {
    let mut open = std::collections::BTreeSet::new();
    for message in messages {
        for item in &message.data {
            match item {
                ChatMessageItem::ToolCall(tool_call) => {
                    open.insert(tool_call.tool_call_id.clone());
                }
                ChatMessageItem::ToolResult(tool_result) => {
                    open.remove(&tool_result.tool_call_id);
                }
                _ => {}
            }
        }
    }
    open.len()
}

fn compression_error_is_request_too_large(error: &CompressionError) -> bool {
    request_too_large_text(&error.to_string())
}

fn request_too_large_prune_start(messages: &[ChatMessage]) -> Option<usize> {
    if messages.len() <= 1 {
        return None;
    }

    let target = (messages.len() / 2).max(1);
    (target..messages.len()).find(|&start| is_tool_protocol_closed_suffix(&messages[start..]))
}

fn is_tool_protocol_closed_suffix(messages: &[ChatMessage]) -> bool {
    let mut open = std::collections::BTreeSet::new();
    for message in messages {
        for item in &message.data {
            match item {
                ChatMessageItem::ToolCall(tool_call) => {
                    open.insert(tool_call.tool_call_id.clone());
                }
                ChatMessageItem::ToolResult(tool_result) => {
                    if !open.remove(&tool_result.tool_call_id) {
                        return false;
                    }
                }
                _ => {}
            }
        }
    }
    open.is_empty()
}

fn synthetic_media_message_from_tool_results(message: &ChatMessage) -> Option<ChatMessage> {
    let mut files = Vec::new();
    for item in &message.data {
        let ChatMessageItem::ToolResult(tool_result) = item else {
            continue;
        };
        if let Some(file) = &tool_result.result.file {
            files.push(ChatMessageItem::File(file.clone()));
        }
    }
    if files.is_empty() {
        return None;
    }

    let mut data = vec![ChatMessageItem::Context(ContextItem {
        text: "Tool returned media files. Use the attached files as current context.".to_string(),
    })];
    data.extend(files);
    Some(ChatMessage::new(ChatRole::User, data))
}

fn render_incoming_user_metadata_notice(message: &ChatMessage) -> Option<String> {
    if message.role != ChatRole::User {
        return None;
    }
    let mut lines = vec!["[Incoming User Metadata]".to_string()];
    let mut has_metadata = false;
    if let Some(user_name) = message.user_name.as_deref().map(str::trim) {
        if !user_name.is_empty() {
            lines.push(format!("Speaker: {user_name}"));
            has_metadata = true;
        }
    }
    if let Some(message_time) = message.message_time.as_deref().map(str::trim) {
        if !message_time.is_empty() {
            lines.push(format!("Message time: {message_time}"));
            has_metadata = true;
        }
    }
    if !has_metadata {
        return None;
    }
    lines.push(
        "Treat this metadata as context for the immediately following user message only."
            .to_string(),
    );
    Some(lines.join("\n"))
}

fn session_request_kind(request: &SessionRequest) -> &'static str {
    match request {
        SessionRequest::Initial { .. } => "initial",
        SessionRequest::EnqueueUserMessage { .. } => "enqueue_user_message",
        SessionRequest::EnqueueActorMessage { .. } => "enqueue_actor_message",
        SessionRequest::CancelTurn { .. } => "cancel_turn",
        SessionRequest::ContinueTurn { .. } => "continue_turn",
        SessionRequest::ResolveHostCoordination { .. } => "resolve_host_coordination",
        SessionRequest::QuerySessionView { .. } => "query_session_view",
        SessionRequest::Shutdown => "shutdown",
    }
}

fn session_event_summary(event: &SessionEvent) -> serde_json::Value {
    match event {
        SessionEvent::TurnStarted { turn_id } => {
            serde_json::json!({"event": "turn_started", "turn_id": turn_id})
        }
        SessionEvent::Progress { message } => {
            serde_json::json!({"event": "progress", "message": message})
        }
        SessionEvent::TurnCompleted { message } => serde_json::json!({
            "event": "turn_completed",
            "message_items": message.data.len(),
        }),
        SessionEvent::TurnFailed {
            error,
            can_continue,
        } => {
            serde_json::json!({"event": "turn_failed", "error": error, "can_continue": can_continue})
        }
        SessionEvent::HostCoordinationRequested { request } => serde_json::json!({
            "event": "host_coordination_requested",
            "request_id": request.request_id,
            "tool_call_id": request.tool_call_id,
            "tool_name": request.tool_name,
            "action": request.action,
        }),
        SessionEvent::InteractiveOutputRequested { payload } => serde_json::json!({
            "event": "interactive_output_requested",
            "payload": payload,
        }),
        SessionEvent::SessionViewResult { query_id, payload } => serde_json::json!({
            "event": "session_view_result",
            "query_id": query_id,
            "payload": payload,
        }),
        SessionEvent::ControlRejected { reason, payload } => serde_json::json!({
            "event": "control_rejected",
            "reason": reason,
            "payload": payload,
        }),
        SessionEvent::RuntimeCrashed { error } => {
            serde_json::json!({"event": "runtime_crashed", "error": error})
        }
    }
}

fn parse_tool_arguments(text: &str) -> serde_json::Value {
    serde_json::from_str(text).unwrap_or_else(|_| serde_json::Value::String(text.to_string()))
}

fn restored_history_needs_continuation(history: &[ChatMessage]) -> bool {
    let Some(last) = history.last() else {
        return false;
    };
    if matches!(last.role, ChatRole::User) {
        return true;
    }
    last.data.iter().any(|item| {
        matches!(
            item,
            ChatMessageItem::ToolCall(_) | ChatMessageItem::ToolResult(_)
        )
    })
}

fn pending_continuation_kind(continuation: &PendingContinuation) -> &'static str {
    match continuation {
        PendingContinuation::CurrentHistory => "current_history",
        PendingContinuation::DataRequest(_) => "data_request",
    }
}

fn usize_field(payload: &serde_json::Value, key: &str, default: usize) -> usize {
    payload
        .get(key)
        .and_then(serde_json::Value::as_u64)
        .map(|value| value as usize)
        .unwrap_or(default)
}

fn invalid_source_payload(source: &str) -> serde_json::Value {
    serde_json::json!({
        "type": "error",
        "source": source,
        "error": "source must be current or all",
    })
}

#[cfg(test)]
mod tests {
    use std::{
        collections::VecDeque,
        fs,
        sync::{mpsc, Mutex},
        time::{Duration, Instant},
    };

    use ahash::AHashMap;
    use tokenizers::{
        models::wordlevel::WordLevel, pre_tokenizers::whitespace::Whitespace, Tokenizer,
    };

    use crate::{
        huggingface::HuggingFaceFileResolver,
        model_config::{
            MediaInputConfig, MediaInputTransport, ModelCapability, MultimodalInputConfig,
            ProviderType, RetryMode, TokenEstimatorType,
        },
        providers::{Provider, ProviderError},
        session_actor::{
            builtin_tool_catalog, BuiltinToolCatalogOptions, ChatRole, ContextItem, FileItem,
            HostToolScope, SessionMailboxKind, ToolBatchError, ToolBatchHandle, ToolCallItem,
            ToolResultContent, ToolResultItem, COMPRESSION_MARKER,
        },
        test_support::temp_cwd,
    };

    use super::*;

    struct MemoryActorMailbox {
        sender: SessionActorRequestSender,
    }

    impl MemoryActorMailbox {
        fn append(&self, kind: SessionMailboxKind, request: SessionRequest) {
            assert_eq!(kind, request.mailbox_kind());
            self.sender
                .send(request)
                .expect("test request channel should be open");
        }
    }

    fn test_inbox() -> (SessionActorInbox, MemoryActorMailbox) {
        let (inbox, sender) = SessionActorInbox::channel();
        (inbox, MemoryActorMailbox { sender })
    }

    #[derive(Default)]
    struct MemoryEventSink {
        events: Mutex<Vec<SessionEvent>>,
    }

    impl SessionActorEventSink for MemoryEventSink {
        fn emit(&self, event: SessionEvent) -> Result<(), String> {
            self.events.lock().unwrap().push(event);
            Ok(())
        }
    }

    struct ScriptedProvider {
        responses: Mutex<VecDeque<ChatMessage>>,
        seen_requests: Mutex<Vec<ProviderRequestSnapshot>>,
    }

    impl ScriptedProvider {
        fn new(responses: Vec<ChatMessage>) -> Self {
            Self {
                responses: Mutex::new(VecDeque::from(responses)),
                seen_requests: Mutex::new(Vec::new()),
            }
        }
    }

    #[derive(Debug, Clone)]
    struct ProviderRequestSnapshot {
        system_prompt: Option<String>,
        tool_names: Vec<String>,
        message_count: usize,
    }

    impl Provider for ScriptedProvider {
        fn send(
            &self,
            _model_config: &ModelConfig,
            request: ProviderRequest<'_>,
        ) -> Result<ChatMessage, ProviderError> {
            self.seen_requests
                .lock()
                .unwrap()
                .push(ProviderRequestSnapshot {
                    system_prompt: request.system_prompt.map(str::to_string),
                    tool_names: request.tools.iter().map(|tool| tool.name.clone()).collect(),
                    message_count: request.messages.len(),
                });
            self.responses
                .lock()
                .unwrap()
                .pop_front()
                .ok_or(ProviderError::EmptyChoices)
        }
    }

    struct RequestTooLargeThenOkProvider {
        calls: Mutex<usize>,
        seen_message_counts: Mutex<Vec<usize>>,
    }

    impl RequestTooLargeThenOkProvider {
        fn new() -> Self {
            Self {
                calls: Mutex::new(0),
                seen_message_counts: Mutex::new(Vec::new()),
            }
        }
    }

    impl Provider for RequestTooLargeThenOkProvider {
        fn send(
            &self,
            _model_config: &ModelConfig,
            request: ProviderRequest<'_>,
        ) -> Result<ChatMessage, ProviderError> {
            self.seen_message_counts
                .lock()
                .unwrap()
                .push(request.messages.len());
            let mut calls = self.calls.lock().unwrap();
            *calls += 1;
            if *calls == 1 {
                return Err(ProviderError::HttpStatus {
                    url: "https://example.invalid/chat".to_string(),
                    status: 413,
                    body: r#"{"error":{"type":"request_too_large","message":"Request exceeds the maximum size"}}"#.to_string(),
                });
            }
            Ok(ChatMessage::new(
                ChatRole::Assistant,
                vec![ChatMessageItem::Context(ContextItem {
                    text: "recovered".to_string(),
                })],
            ))
        }
    }

    struct EchoToolExecutor {
        batches: Mutex<Vec<ToolBatch>>,
    }

    impl EchoToolExecutor {
        fn new() -> Self {
            Self {
                batches: Mutex::new(Vec::new()),
            }
        }
    }

    impl ToolBatchExecutor for EchoToolExecutor {
        fn start(
            &self,
            batch: ToolBatch,
            completion_tx: Sender<ToolBatchCompletion>,
        ) -> Result<ToolBatchHandle, ToolBatchError> {
            let handle = ToolBatchHandle::new(batch.batch_id.clone());
            self.batches.lock().unwrap().push(batch);
            let message = ChatMessage::new(
                ChatRole::Assistant,
                vec![ChatMessageItem::ToolResult(ToolResultItem {
                    tool_call_id: "call_1".to_string(),
                    tool_name: "user_tell".to_string(),
                    result: ToolResultContent {
                        context: Some(ContextItem {
                            text: format!("tool batch {} done", handle.batch_id),
                        }),
                        file: None,
                    },
                })],
            );
            let _ = completion_tx.send(ToolBatchCompletion {
                batch_id: handle.batch_id.clone(),
                result: Ok(message),
            });
            Ok(handle)
        }

        fn interrupt(&self, _handle: &ToolBatchHandle) -> Result<(), ToolBatchError> {
            Ok(())
        }

        fn finish(&self, _batch_id: &str) -> Result<(), ToolBatchError> {
            Ok(())
        }
    }

    struct MediaFileToolExecutor;

    impl ToolBatchExecutor for MediaFileToolExecutor {
        fn start(
            &self,
            batch: ToolBatch,
            completion_tx: Sender<ToolBatchCompletion>,
        ) -> Result<ToolBatchHandle, ToolBatchError> {
            let handle = ToolBatchHandle::new(batch.batch_id);
            let _ = completion_tx.send(ToolBatchCompletion {
                batch_id: handle.batch_id.clone(),
                result: Ok(ChatMessage::new(
                    ChatRole::Assistant,
                    vec![ChatMessageItem::ToolResult(ToolResultItem {
                        tool_call_id: "call_1".to_string(),
                        tool_name: "image_load".to_string(),
                        result: ToolResultContent {
                            context: Some(ContextItem {
                                text: "loaded image".to_string(),
                            }),
                            file: Some(FileItem {
                                uri: "file:///tmp/test.png".to_string(),
                                name: Some("test.png".to_string()),
                                media_type: Some("image/png".to_string()),
                                width: None,
                                height: None,
                                state: None,
                            }),
                        },
                    })],
                )),
            });
            Ok(handle)
        }

        fn interrupt(&self, _handle: &ToolBatchHandle) -> Result<(), ToolBatchError> {
            Ok(())
        }

        fn finish(&self, _batch_id: &str) -> Result<(), ToolBatchError> {
            Ok(())
        }
    }

    struct BlockingToolExecutor {
        started_tx: Mutex<Option<mpsc::Sender<()>>>,
        release_rx: Mutex<Option<mpsc::Receiver<()>>>,
        interrupt_tx: Mutex<Option<mpsc::Sender<()>>>,
    }

    impl BlockingToolExecutor {
        fn new(started_tx: mpsc::Sender<()>, release_rx: mpsc::Receiver<()>) -> Self {
            Self::with_interrupt_tx(started_tx, release_rx, None)
        }

        fn with_interrupt_tx(
            started_tx: mpsc::Sender<()>,
            release_rx: mpsc::Receiver<()>,
            interrupt_tx: Option<mpsc::Sender<()>>,
        ) -> Self {
            Self {
                started_tx: Mutex::new(Some(started_tx)),
                release_rx: Mutex::new(Some(release_rx)),
                interrupt_tx: Mutex::new(interrupt_tx),
            }
        }
    }

    impl ToolBatchExecutor for BlockingToolExecutor {
        fn start(
            &self,
            batch: ToolBatch,
            completion_tx: Sender<ToolBatchCompletion>,
        ) -> Result<ToolBatchHandle, ToolBatchError> {
            let handle = ToolBatchHandle::new(batch.batch_id);
            if let Some(started_tx) = self.started_tx.lock().unwrap().take() {
                let _ = started_tx.send(());
            }
            let release_rx = self.release_rx.lock().unwrap().take().ok_or_else(|| {
                ToolBatchError::Start("blocking executor already started".to_string())
            })?;
            let batch_id = handle.batch_id.clone();
            std::thread::spawn(move || {
                if release_rx.recv().is_ok() {
                    let _ = completion_tx.send(ToolBatchCompletion {
                        batch_id,
                        result: Ok(ChatMessage::new(
                            ChatRole::Assistant,
                            vec![ChatMessageItem::ToolResult(ToolResultItem {
                                tool_call_id: "call_1".to_string(),
                                tool_name: "user_tell".to_string(),
                                result: ToolResultContent {
                                    context: Some(ContextItem {
                                        text: "tool result".to_string(),
                                    }),
                                    file: None,
                                },
                            })],
                        )),
                    });
                }
            });
            Ok(handle)
        }

        fn interrupt(&self, _handle: &ToolBatchHandle) -> Result<(), ToolBatchError> {
            if let Some(interrupt_tx) = self.interrupt_tx.lock().unwrap().take() {
                let _ = interrupt_tx.send(());
            }
            Ok(())
        }

        fn finish(&self, _batch_id: &str) -> Result<(), ToolBatchError> {
            Ok(())
        }
    }

    fn test_session_id(prefix: &str) -> String {
        format!(
            "{}_{}_{}",
            prefix,
            std::process::id(),
            rand::random::<u64>()
        )
    }

    fn test_model_config() -> ModelConfig {
        ModelConfig {
            provider_type: ProviderType::OpenRouterCompletion,
            model_name: "openai/gpt-4o-mini".to_string(),
            url: "https://openrouter.ai/api/v1/chat/completions".to_string(),
            api_key_env: "OPENROUTER_API_KEY".to_string(),
            capabilities: vec![ModelCapability::Chat],
            token_max_context: 128_000,
            cache_timeout: 300,
            conn_timeout: 10,
            request_timeout: 600,
            max_request_size: 30 * 1024 * 1024,
            retry_mode: RetryMode::Once,
            reasoning: None,
            token_estimator_type: TokenEstimatorType::Local,
            multimodal_estimator: None,
            multimodal_input: None,
            token_estimator_url: None,
        }
    }

    fn test_model_config_with_tokenizer() -> (ModelConfig, std::path::PathBuf) {
        let mut vocab = AHashMap::new();
        vocab.insert("[UNK]".to_string(), 0);
        vocab.insert("user".to_string(), 1);
        vocab.insert("assistant".to_string(), 2);
        vocab.insert("old".to_string(), 3);
        vocab.insert("first".to_string(), 4);
        vocab.insert("second".to_string(), 5);
        vocab.insert("final".to_string(), 6);
        vocab.insert("summary".to_string(), 7);

        let model = WordLevel::builder()
            .vocab(vocab)
            .unk_token("[UNK]".to_string())
            .build()
            .expect("word level should build");
        let mut tokenizer = Tokenizer::new(model);
        tokenizer.with_pre_tokenizer(Some(Whitespace));

        let directory = std::env::temp_dir().join(format!(
            "stellaclaw-actor-compression-test-{}-{}",
            std::process::id(),
            rand::random::<u64>()
        ));
        fs::create_dir_all(&directory).expect("directory should exist");
        tokenizer
            .save(directory.join("tokenizer.json"), false)
            .expect("tokenizer should save");
        fs::write(
            directory.join("tokenizer_config.json"),
            r#"{
                "chat_template": "{% for message in messages %}{{ message.role }} {{ message.content }}\n{% endfor %}",
                "bos_token": "<s>",
                "eos_token": "</s>"
            }"#,
        )
        .expect("tokenizer config should save");

        let mut config = test_model_config();
        config.token_estimator_type = TokenEstimatorType::HuggingFace;
        config.token_estimator_url = Some(
            directory
                .join("tokenizer_config.json")
                .to_string_lossy()
                .to_string(),
        );

        let resolver = HuggingFaceFileResolver::new().expect("resolver should build");
        TokenEstimator::from_model_config(&config, &resolver).expect("tokenizer should load");

        (config, directory)
    }

    #[test]
    fn active_compression_threshold_is_capped_by_model_context() {
        let mut model_config = test_model_config();
        model_config.token_max_context = 200_000;
        let mut initial = SessionInitial::new(
            test_session_id("session_compression_threshold"),
            super::super::SessionType::Foreground,
        );
        initial.compression_threshold_tokens = Some(235_929);

        assert_eq!(
            active_compression_threshold_tokens(&model_config, &initial),
            Some(180_000)
        );

        initial.compression_threshold_tokens = Some(120_000);
        assert_eq!(
            active_compression_threshold_tokens(&model_config, &initial),
            Some(120_000)
        );

        initial.compression_threshold_tokens = None;
        assert_eq!(
            active_compression_threshold_tokens(&model_config, &initial),
            None
        );
    }

    #[test]
    fn request_too_large_prune_start_preserves_tool_call_result_pairs() {
        let messages = vec![
            text_message(ChatRole::User, "old user"),
            text_message(ChatRole::Assistant, "old assistant"),
            ChatMessage::new(
                ChatRole::Assistant,
                vec![ChatMessageItem::ToolCall(ToolCallItem {
                    tool_call_id: "call_1".to_string(),
                    tool_name: "file_read".to_string(),
                    arguments: ContextItem {
                        text: r#"{"path":"README.md"}"#.to_string(),
                    },
                })],
            ),
            ChatMessage::new(
                ChatRole::Assistant,
                vec![ChatMessageItem::ToolResult(ToolResultItem {
                    tool_call_id: "call_1".to_string(),
                    tool_name: "file_read".to_string(),
                    result: ToolResultContent {
                        context: Some(ContextItem {
                            text: "contents".to_string(),
                        }),
                        file: None,
                    },
                })],
            ),
            text_message(ChatRole::Assistant, "after tool"),
            text_message(ChatRole::User, "new user"),
        ];

        assert_eq!(request_too_large_prune_start(&messages), Some(4));
    }

    #[test]
    fn runs_user_turn_without_tools() {
        let _cwd = temp_cwd("actor-runs-user-turn");
        let (inbox, mailbox) = test_inbox();
        let session_id = test_session_id("session_runs_user_turn");
        mailbox.append(
            SessionMailboxKind::Control,
            SessionRequest::Initial {
                initial: SessionInitial::new(
                    session_id.clone(),
                    super::super::SessionType::Foreground,
                ),
            },
        );
        mailbox.append(
            SessionMailboxKind::Data,
            SessionRequest::EnqueueUserMessage {
                message: ChatMessage::new(
                    ChatRole::User,
                    vec![ChatMessageItem::Context(ContextItem {
                        text: "hello".to_string(),
                    })],
                ),
            },
        );
        let events = Arc::new(MemoryEventSink::default());
        let provider = Arc::new(ScriptedProvider::new(vec![ChatMessage::new(
            ChatRole::Assistant,
            vec![ChatMessageItem::Context(ContextItem {
                text: "hi".to_string(),
            })],
        )]));
        let tools = Arc::new(EchoToolExecutor::new());
        let catalog = builtin_tool_catalog(BuiltinToolCatalogOptions::default()).unwrap();
        let mut actor = SessionActor::new(
            test_model_config(),
            provider.clone(),
            tools,
            inbox,
            events.clone(),
            catalog,
        );

        let step = actor.run_until_idle(4).expect("actor should run");

        assert_eq!(step, SessionActorStep::Idle);
        assert_eq!(actor.initial().unwrap().session_id, session_id);
        assert_eq!(actor.history().len(), 2);
        assert!(matches!(
            events.events.lock().unwrap().last(),
            Some(SessionEvent::TurnCompleted { .. })
        ));
        let seen_requests = provider.seen_requests.lock().unwrap();
        assert_eq!(seen_requests.len(), 1);
        assert!(seen_requests[0]
            .system_prompt
            .as_ref()
            .unwrap()
            .contains("Session kind: foreground"));
        assert!(seen_requests[0]
            .tool_names
            .contains(&"file_read".to_string()));
        assert_eq!(seen_requests[0].message_count, 1);
    }

    #[test]
    fn provider_error_keeps_actor_alive_and_continue_retries_current_history() {
        let _cwd = temp_cwd("actor-continue-after-provider-error");
        let (inbox, mailbox) = test_inbox();
        mailbox.append(
            SessionMailboxKind::Control,
            SessionRequest::Initial {
                initial: SessionInitial::new(
                    test_session_id("session_continue_error"),
                    super::super::SessionType::Foreground,
                ),
            },
        );
        mailbox.append(
            SessionMailboxKind::Data,
            SessionRequest::EnqueueUserMessage {
                message: ChatMessage::new(
                    ChatRole::User,
                    vec![ChatMessageItem::Context(ContextItem {
                        text: "retry me".to_string(),
                    })],
                ),
            },
        );
        let events = Arc::new(MemoryEventSink::default());
        let provider = Arc::new(ScriptedProvider::new(Vec::new()));
        let tools = Arc::new(EchoToolExecutor::new());
        let catalog = builtin_tool_catalog(BuiltinToolCatalogOptions::default()).unwrap();
        let mut actor = SessionActor::new(
            test_model_config(),
            provider.clone(),
            tools,
            inbox,
            events.clone(),
            catalog,
        );

        let step = actor
            .run_until_idle(4)
            .expect("recoverable error should not crash actor");

        assert_eq!(step, SessionActorStep::Idle);
        assert!(matches!(
            events.events.lock().unwrap().last(),
            Some(SessionEvent::TurnFailed {
                can_continue: true,
                ..
            })
        ));
        assert_eq!(actor.history().len(), 1);
        mailbox.append(
            SessionMailboxKind::Control,
            SessionRequest::QuerySessionView {
                query_id: "live_state".to_string(),
                payload: serde_json::json!({"type": "live_state"}),
            },
        );
        actor.step().expect("live state query should run");
        let live_state = session_view_payload_for_test(&events, "live_state");
        assert_eq!(live_state["type"], "live_state");
        assert_eq!(live_state["initialized"], true);
        assert_eq!(live_state["history_len"], 1);
        assert_eq!(live_state["can_continue"], true);

        provider
            .responses
            .lock()
            .unwrap()
            .push_back(ChatMessage::new(
                ChatRole::Assistant,
                vec![ChatMessageItem::Context(ContextItem {
                    text: "continued".to_string(),
                })],
            ));
        mailbox.append(
            SessionMailboxKind::Control,
            SessionRequest::ContinueTurn {
                reason: Some("user confirmed".to_string()),
            },
        );

        actor
            .run_until_idle(4)
            .expect("continue should retry current history");

        assert!(matches!(
            events.events.lock().unwrap().last(),
            Some(SessionEvent::TurnCompleted { .. })
        ));
        assert_eq!(actor.history().len(), 2);
        assert_eq!(provider.seen_requests.lock().unwrap().len(), 2);
    }

    #[test]
    fn request_too_large_provider_error_prunes_history_and_retries() {
        let _cwd = temp_cwd("actor-request-too-large-retry");
        let (inbox, mailbox) = test_inbox();
        mailbox.append(
            SessionMailboxKind::Control,
            SessionRequest::Initial {
                initial: SessionInitial::new(
                    test_session_id("session_request_too_large"),
                    super::super::SessionType::Foreground,
                ),
            },
        );
        let events = Arc::new(MemoryEventSink::default());
        let provider = Arc::new(RequestTooLargeThenOkProvider::new());
        let tools = Arc::new(EchoToolExecutor::new());
        let catalog = builtin_tool_catalog(BuiltinToolCatalogOptions::default()).unwrap();
        let mut actor = SessionActor::new(
            test_model_config(),
            provider.clone(),
            tools,
            inbox,
            events.clone(),
            catalog,
        );
        actor.run_until_idle(2).expect("initial should apply");
        actor.history = vec![
            text_message(ChatRole::User, "old 1"),
            text_message(ChatRole::Assistant, "old 2"),
            text_message(ChatRole::User, "old 3"),
            text_message(ChatRole::Assistant, "new 1"),
            text_message(ChatRole::User, "new 2"),
            text_message(ChatRole::Assistant, "new 3"),
        ];
        actor.all_messages = actor.history.clone();

        actor
            .run_model_tool_loop("turn_retry", 1, 0)
            .expect("request too large should recover by pruning history");

        assert_eq!(*provider.calls.lock().unwrap(), 2);
        assert_eq!(
            provider.seen_message_counts.lock().unwrap().as_slice(),
            &[6, 3]
        );
        assert_eq!(actor.history().len(), 4);
        assert!(events.events.lock().unwrap().iter().any(|event| matches!(
            event,
            SessionEvent::Progress { message } if message.contains("数据丢失")
        )));
        assert!(matches!(
            events.events.lock().unwrap().last(),
            Some(SessionEvent::TurnCompleted { .. })
        ));
    }

    #[test]
    fn preflight_prunes_when_estimate_already_exceeds_context() {
        let _cwd = temp_cwd("actor-request-too-large-preflight");
        let (inbox, mailbox) = test_inbox();
        mailbox.append(
            SessionMailboxKind::Control,
            SessionRequest::Initial {
                initial: SessionInitial::new(
                    test_session_id("session_preflight_too_large"),
                    super::super::SessionType::Foreground,
                ),
            },
        );
        let events = Arc::new(MemoryEventSink::default());
        let provider = Arc::new(ScriptedProvider::new(vec![text_message(
            ChatRole::Assistant,
            "recovered after preflight prune",
        )]));
        let tools = Arc::new(EchoToolExecutor::new());
        let catalog = builtin_tool_catalog(BuiltinToolCatalogOptions::default()).unwrap();
        let mut model_config = test_model_config();
        model_config.capabilities.push(ModelCapability::ImageIn);
        model_config.multimodal_input = Some(MultimodalInputConfig {
            image: Some(MediaInputConfig {
                transport: MediaInputTransport::FileReference,
                supported_media_types: vec!["image/png".to_string()],
                max_width: None,
                max_height: None,
            }),
            pdf: None,
            audio: None,
        });
        model_config.token_max_context = 1_000;
        let mut actor = SessionActor::new(
            model_config,
            provider.clone(),
            tools,
            inbox,
            events.clone(),
            catalog,
        );
        actor.run_until_idle(2).expect("initial should apply");
        let image_message = || {
            ChatMessage::new(
                ChatRole::User,
                vec![ChatMessageItem::File(FileItem {
                    uri: "file:///tmp/large.png".to_string(),
                    name: Some("large.png".to_string()),
                    media_type: Some("image/png".to_string()),
                    width: Some(1024),
                    height: Some(1024),
                    state: None,
                })],
            )
        };
        actor.history = vec![image_message(), image_message(), image_message()];
        actor.all_messages = actor.history.clone();

        actor
            .run_model_tool_loop("turn_preflight", 1, 0)
            .expect("oversized local estimate should recover by pruning history before send");

        let seen_requests = provider.seen_requests.lock().unwrap();
        assert_eq!(seen_requests.len(), 1);
        assert_eq!(seen_requests[0].message_count, 1);
        assert!(events.events.lock().unwrap().iter().any(|event| matches!(
            event,
            SessionEvent::Progress { message } if message.contains("数据丢失")
        )));
    }

    #[test]
    fn query_session_view_returns_transcript_page_and_message_detail() {
        let _cwd = temp_cwd("actor-query-session-view");
        let (inbox, mailbox) = test_inbox();
        mailbox.append(
            SessionMailboxKind::Control,
            SessionRequest::Initial {
                initial: SessionInitial::new(
                    test_session_id("session_query_view"),
                    super::super::SessionType::Foreground,
                ),
            },
        );
        mailbox.append(
            SessionMailboxKind::Data,
            SessionRequest::EnqueueUserMessage {
                message: ChatMessage::new(
                    ChatRole::User,
                    vec![ChatMessageItem::Context(ContextItem {
                        text: "show transcript".to_string(),
                    })],
                ),
            },
        );
        let events = Arc::new(MemoryEventSink::default());
        let provider = Arc::new(ScriptedProvider::new(vec![ChatMessage::new(
            ChatRole::Assistant,
            vec![ChatMessageItem::Context(ContextItem {
                text: "transcript response".to_string(),
            })],
        )]));
        let tools = Arc::new(EchoToolExecutor::new());
        let catalog = builtin_tool_catalog(BuiltinToolCatalogOptions::default()).unwrap();
        let mut actor = SessionActor::new(
            test_model_config(),
            provider,
            tools,
            inbox,
            events.clone(),
            catalog,
        );
        actor.run_until_idle(4).expect("actor should run");

        mailbox.append(
            SessionMailboxKind::Control,
            SessionRequest::QuerySessionView {
                query_id: "page".to_string(),
                payload: serde_json::json!({
                    "type": "transcript_page",
                    "source": "current",
                    "offset": 0,
                    "limit": 1,
                }),
            },
        );
        actor.step().expect("page query should run");
        let page = session_view_payload_for_test(&events, "page");
        assert_eq!(page["type"], "transcript_page");
        assert_eq!(page["source"], "current");
        assert_eq!(page["total"], 2);
        assert_eq!(page["messages"].as_array().unwrap().len(), 1);

        mailbox.append(
            SessionMailboxKind::Control,
            SessionRequest::QuerySessionView {
                query_id: "detail".to_string(),
                payload: serde_json::json!({
                    "type": "message_detail",
                    "source": "current",
                    "index": 1,
                }),
            },
        );
        actor.step().expect("detail query should run");
        let detail = session_view_payload_for_test(&events, "detail");
        assert_eq!(detail["type"], "message_detail");
        assert_eq!(detail["index"], 1);
        assert_eq!(detail["message"]["role"], "assistant");

        mailbox.append(
            SessionMailboxKind::Control,
            SessionRequest::QuerySessionView {
                query_id: "missing".to_string(),
                payload: serde_json::json!({
                    "type": "message_detail",
                    "source": "all",
                    "index": 99,
                }),
            },
        );
        actor.step().expect("missing detail query should run");
        let missing = session_view_payload_for_test(&events, "missing");
        assert_eq!(missing["type"], "message_detail");
        assert_eq!(missing["error"], "message index out of range");
        assert_eq!(missing["total"], 2);
    }

    #[test]
    fn runs_model_tool_model_loop_and_routes_bridge_tools() {
        let _cwd = temp_cwd("actor-tool-loop");
        let (inbox, mailbox) = test_inbox();
        let session_id = test_session_id("session_tool_loop");
        mailbox.append(
            SessionMailboxKind::Control,
            SessionRequest::Initial {
                initial: SessionInitial::new(session_id, super::super::SessionType::Foreground),
            },
        );
        mailbox.append(
            SessionMailboxKind::Data,
            SessionRequest::EnqueueUserMessage {
                message: ChatMessage::new(
                    ChatRole::User,
                    vec![ChatMessageItem::Context(ContextItem {
                        text: "tell user".to_string(),
                    })],
                ),
            },
        );
        let events = Arc::new(MemoryEventSink::default());
        let provider = Arc::new(ScriptedProvider::new(vec![
            ChatMessage::new(
                ChatRole::Assistant,
                vec![ChatMessageItem::ToolCall(ToolCallItem {
                    tool_call_id: "call_1".to_string(),
                    tool_name: "user_tell".to_string(),
                    arguments: ContextItem {
                        text: r#"{"text":"working"}"#.to_string(),
                    },
                })],
            ),
            ChatMessage::new(
                ChatRole::Assistant,
                vec![ChatMessageItem::Context(ContextItem {
                    text: "done".to_string(),
                })],
            ),
        ]));
        let tools = Arc::new(EchoToolExecutor::new());
        let catalog = builtin_tool_catalog(BuiltinToolCatalogOptions {
            host_tool_scope: Some(HostToolScope::MainForeground),
            ..BuiltinToolCatalogOptions::default()
        })
        .unwrap();
        let mut actor = SessionActor::new(
            test_model_config(),
            provider,
            tools.clone(),
            inbox,
            events,
            catalog,
        );

        actor.run_until_idle(4).expect("actor should run");

        assert_eq!(actor.history().len(), 4);
        let batches = tools.batches.lock().unwrap();
        assert_eq!(batches.len(), 1);
        assert!(matches!(
            batches[0].operations[0],
            ToolExecutionOp::ConversationBridge(_)
        ));
    }

    #[test]
    fn tool_result_file_is_added_as_synthetic_user_media_context() {
        let _cwd = temp_cwd("actor-media-context");
        let (inbox, mailbox) = test_inbox();
        let session_id = test_session_id("session_media_context");
        mailbox.append(
            SessionMailboxKind::Control,
            SessionRequest::Initial {
                initial: SessionInitial::new(session_id, super::super::SessionType::Foreground),
            },
        );
        mailbox.append(
            SessionMailboxKind::Data,
            SessionRequest::EnqueueUserMessage {
                message: ChatMessage::new(
                    ChatRole::User,
                    vec![ChatMessageItem::Context(ContextItem {
                        text: "load image".to_string(),
                    })],
                ),
            },
        );
        let events = Arc::new(MemoryEventSink::default());
        let provider = Arc::new(ScriptedProvider::new(vec![
            ChatMessage::new(
                ChatRole::Assistant,
                vec![ChatMessageItem::ToolCall(ToolCallItem {
                    tool_call_id: "call_1".to_string(),
                    tool_name: "image_load".to_string(),
                    arguments: ContextItem {
                        text: r#"{"path":"test.png"}"#.to_string(),
                    },
                })],
            ),
            ChatMessage::new(
                ChatRole::Assistant,
                vec![ChatMessageItem::Context(ContextItem {
                    text: "saw image".to_string(),
                })],
            ),
        ]));
        let tools = Arc::new(MediaFileToolExecutor);
        let catalog = builtin_tool_catalog(BuiltinToolCatalogOptions {
            enable_native_image_load: true,
            ..BuiltinToolCatalogOptions::default()
        })
        .unwrap();
        let mut actor = SessionActor::new(
            test_model_config(),
            provider.clone(),
            tools,
            inbox,
            events,
            catalog,
        );

        actor.run_until_idle(6).expect("actor should run");

        assert_eq!(actor.history().len(), 5);
        assert!(matches!(
            actor.history()[3].data[1],
            ChatMessageItem::File(_)
        ));
        let seen_requests = provider.seen_requests.lock().unwrap();
        assert_eq!(seen_requests.len(), 2);
        assert_eq!(seen_requests[1].message_count, 4);
    }

    #[test]
    fn rejects_data_before_initial_message() {
        let (inbox, mailbox) = test_inbox();
        mailbox.append(
            SessionMailboxKind::Data,
            SessionRequest::EnqueueUserMessage {
                message: ChatMessage::new(
                    ChatRole::User,
                    vec![ChatMessageItem::Context(ContextItem {
                        text: "hello".to_string(),
                    })],
                ),
            },
        );
        let events = Arc::new(MemoryEventSink::default());
        let provider = Arc::new(ScriptedProvider::new(Vec::new()));
        let tools = Arc::new(EchoToolExecutor::new());
        let catalog = builtin_tool_catalog(BuiltinToolCatalogOptions::default()).unwrap();
        let mut actor =
            SessionActor::new(test_model_config(), provider, tools, inbox, events, catalog);

        let error = actor.step().expect_err("data before initial should fail");

        assert!(matches!(error, SessionActorError::MissingInitial));
    }

    #[test]
    fn initial_enables_remote_tools_for_actor_catalog() {
        let _cwd = temp_cwd("actor-remote-tools");
        let (inbox, mailbox) = test_inbox();
        let mut initial = SessionInitial::new(
            test_session_id("session_remote_tools"),
            super::super::SessionType::Foreground,
        );
        initial.tool_remote_mode = super::super::ToolRemoteMode::Selectable;
        mailbox.append(
            SessionMailboxKind::Control,
            SessionRequest::Initial { initial },
        );
        let events = Arc::new(MemoryEventSink::default());
        let provider = Arc::new(ScriptedProvider::new(Vec::new()));
        let tools = Arc::new(EchoToolExecutor::new());
        let catalog = builtin_tool_catalog(BuiltinToolCatalogOptions::default()).unwrap();
        let mut actor =
            SessionActor::new(test_model_config(), provider, tools, inbox, events, catalog);

        actor.step().expect("initial should apply");

        let remote =
            &actor.tool_catalog().get("file_read").unwrap().parameters["properties"]["remote"];
        assert_eq!(remote["type"], "string");
    }

    #[test]
    fn injects_runtime_metadata_updates_before_user_message() {
        let _cwd = temp_cwd("actor-runtime-meta");
        fs::create_dir_all(".stellaclaw").expect("metadata dir should exist");
        fs::create_dir_all(".skill/demo").expect("skill dir should exist");
        fs::write(".stellaclaw/USER.md", "tier: old").expect("user metadata should seed");
        fs::write(".skill/demo/SKILL.md", "# Demo\n\nold desc\n\nold body")
            .expect("skill should seed");
        let (inbox, mailbox) = test_inbox();
        let events = Arc::new(MemoryEventSink::default());
        let provider = Arc::new(ScriptedProvider::new(vec![ChatMessage::new(
            ChatRole::Assistant,
            vec![ChatMessageItem::Context(ContextItem {
                text: "done".to_string(),
            })],
        )]));
        let tools = Arc::new(EchoToolExecutor::new());
        let catalog = builtin_tool_catalog(BuiltinToolCatalogOptions::default()).unwrap();
        let mut actor =
            SessionActor::new(test_model_config(), provider, tools, inbox, events, catalog);

        mailbox.append(
            SessionMailboxKind::Control,
            SessionRequest::Initial {
                initial: SessionInitial::new(
                    test_session_id("session_runtime_meta"),
                    super::super::SessionType::Foreground,
                ),
            },
        );
        actor.step().expect("initial should apply");

        fs::write(".stellaclaw/USER.md", "tier: new").expect("user metadata should update");
        fs::write(".skill/demo/SKILL.md", "# Demo\n\nnew desc\n\nold body")
            .expect("skill should update");
        mailbox.append(
            SessionMailboxKind::Data,
            SessionRequest::EnqueueUserMessage {
                message: ChatMessage::new(
                    ChatRole::User,
                    vec![ChatMessageItem::Context(ContextItem {
                        text: "real user request".to_string(),
                    })],
                ),
            },
        );

        actor.run_until_idle(4).expect("actor should run");

        assert!(message_text_for_test(&actor.history()[0]).contains("[Runtime Prompt Updates]"));
        assert!(message_text_for_test(&actor.history()[0]).contains("tier: new"));
        assert!(message_text_for_test(&actor.history()[1]).contains("[Runtime Skill Updates]"));
        assert!(message_text_for_test(&actor.history()[2]).contains("real user request"));
    }

    #[test]
    fn user_message_metadata_inserts_synthetic_notice_before_user_input() {
        let _cwd = temp_cwd("actor-user-message-metadata");
        let (inbox, mailbox) = test_inbox();
        mailbox.append(
            SessionMailboxKind::Control,
            SessionRequest::Initial {
                initial: SessionInitial::new(
                    test_session_id("session_user_message_metadata"),
                    super::super::SessionType::Foreground,
                ),
            },
        );
        mailbox.append(
            SessionMailboxKind::Data,
            SessionRequest::EnqueueUserMessage {
                message: ChatMessage::new(
                    ChatRole::User,
                    vec![ChatMessageItem::Context(ContextItem {
                        text: "hello".to_string(),
                    })],
                )
                .with_user_name("alice")
                .with_message_time("2026-04-23T10:20:30Z"),
            },
        );
        let events = Arc::new(MemoryEventSink::default());
        let provider = Arc::new(ScriptedProvider::new(vec![ChatMessage::new(
            ChatRole::Assistant,
            vec![ChatMessageItem::Context(ContextItem {
                text: "done".to_string(),
            })],
        )]));
        let tools = Arc::new(EchoToolExecutor::new());
        let catalog = builtin_tool_catalog(BuiltinToolCatalogOptions::default()).unwrap();
        let mut actor =
            SessionActor::new(test_model_config(), provider, tools, inbox, events, catalog);

        actor.run_until_idle(4).expect("actor should run");

        assert!(message_text_for_test(&actor.history()[0]).contains("[Incoming User Metadata]"));
        assert!(message_text_for_test(&actor.history()[0]).contains("Speaker: alice"));
        assert!(message_text_for_test(&actor.history()[0])
            .contains("Message time: 2026-04-23T10:20:30Z"));
        assert_eq!(actor.history()[1].user_name.as_deref(), Some("alice"));
        assert_eq!(
            actor.history()[1].message_time.as_deref(),
            Some("2026-04-23T10:20:30Z")
        );
    }

    #[test]
    fn restores_session_state_on_initial() {
        let _cwd = temp_cwd("actor-restore");
        let session_id = test_session_id("session_restore");
        let (inbox, mailbox) = test_inbox();
        mailbox.append(
            SessionMailboxKind::Control,
            SessionRequest::Initial {
                initial: SessionInitial::new(
                    session_id.clone(),
                    super::super::SessionType::Foreground,
                ),
            },
        );
        mailbox.append(
            SessionMailboxKind::Data,
            SessionRequest::EnqueueUserMessage {
                message: ChatMessage::new(
                    ChatRole::User,
                    vec![ChatMessageItem::Context(ContextItem {
                        text: "persist me".to_string(),
                    })],
                ),
            },
        );
        let events = Arc::new(MemoryEventSink::default());
        let provider = Arc::new(ScriptedProvider::new(vec![ChatMessage::new(
            ChatRole::Assistant,
            vec![ChatMessageItem::Context(ContextItem {
                text: "persisted reply".to_string(),
            })],
        )]));
        let tools = Arc::new(EchoToolExecutor::new());
        let catalog = builtin_tool_catalog(BuiltinToolCatalogOptions::default()).unwrap();
        let mut actor =
            SessionActor::new(test_model_config(), provider, tools, inbox, events, catalog);
        actor.run_until_idle(4).expect("first actor should run");

        let (inbox, mailbox) = test_inbox();
        mailbox.append(
            SessionMailboxKind::Control,
            SessionRequest::Initial {
                initial: SessionInitial::new(session_id, super::super::SessionType::Foreground),
            },
        );
        let events = Arc::new(MemoryEventSink::default());
        let provider = Arc::new(ScriptedProvider::new(Vec::new()));
        let tools = Arc::new(EchoToolExecutor::new());
        let catalog = builtin_tool_catalog(BuiltinToolCatalogOptions::default()).unwrap();
        let mut restored =
            SessionActor::new(test_model_config(), provider, tools, inbox, events, catalog);

        restored.step().expect("initial should restore state");

        assert_eq!(restored.history().len(), 2);
        assert!(message_text_for_test(&restored.history()[0]).contains("persist me"));
        assert!(message_text_for_test(&restored.history()[1]).contains("persisted reply"));
    }

    #[test]
    fn restored_unfinished_history_requests_continue_confirmation() {
        let _cwd = temp_cwd("actor-restore-unfinished");
        let session_id = test_session_id("session_restore_unfinished");
        let (inbox, mailbox) = test_inbox();
        mailbox.append(
            SessionMailboxKind::Control,
            SessionRequest::Initial {
                initial: SessionInitial::new(
                    session_id.clone(),
                    super::super::SessionType::Foreground,
                ),
            },
        );
        mailbox.append(
            SessionMailboxKind::Data,
            SessionRequest::EnqueueUserMessage {
                message: ChatMessage::new(
                    ChatRole::User,
                    vec![ChatMessageItem::Context(ContextItem {
                        text: "unfinished request".to_string(),
                    })],
                ),
            },
        );
        let events = Arc::new(MemoryEventSink::default());
        let provider = Arc::new(ScriptedProvider::new(Vec::new()));
        let tools = Arc::new(EchoToolExecutor::new());
        let catalog = builtin_tool_catalog(BuiltinToolCatalogOptions::default()).unwrap();
        let mut actor =
            SessionActor::new(test_model_config(), provider, tools, inbox, events, catalog);
        actor
            .run_until_idle(4)
            .expect("provider error should leave recoverable state");

        let (inbox, mailbox) = test_inbox();
        mailbox.append(
            SessionMailboxKind::Control,
            SessionRequest::Initial {
                initial: SessionInitial::new(session_id, super::super::SessionType::Foreground),
            },
        );
        let events = Arc::new(MemoryEventSink::default());
        let provider = Arc::new(ScriptedProvider::new(Vec::new()));
        let tools = Arc::new(EchoToolExecutor::new());
        let catalog = builtin_tool_catalog(BuiltinToolCatalogOptions::default()).unwrap();
        let mut restored = SessionActor::new(
            test_model_config(),
            provider,
            tools,
            inbox,
            events.clone(),
            catalog,
        );

        restored
            .step()
            .expect("initial should restore unfinished state");

        assert_eq!(restored.history().len(), 1);
        assert!(matches!(
            events.events.lock().unwrap().last(),
            Some(SessionEvent::TurnFailed {
                can_continue: true,
                ..
            })
        ));
    }

    #[test]
    fn does_not_persist_history_with_unclosed_tool_call() {
        let _cwd = temp_cwd("actor-unclosed-tool");
        let session_id = test_session_id("session_unclosed_tool");
        let (inbox, mailbox) = test_inbox();
        mailbox.append(
            SessionMailboxKind::Control,
            SessionRequest::Initial {
                initial: SessionInitial::new(
                    session_id.clone(),
                    super::super::SessionType::Foreground,
                ),
            },
        );
        mailbox.append(
            SessionMailboxKind::Data,
            SessionRequest::EnqueueUserMessage {
                message: ChatMessage::new(
                    ChatRole::User,
                    vec![ChatMessageItem::Context(ContextItem {
                        text: "run a tool".to_string(),
                    })],
                ),
            },
        );
        let events = Arc::new(MemoryEventSink::default());
        let provider = Arc::new(ScriptedProvider::new(vec![
            ChatMessage::new(
                ChatRole::Assistant,
                vec![ChatMessageItem::ToolCall(ToolCallItem {
                    tool_call_id: "call_1".to_string(),
                    tool_name: "user_tell".to_string(),
                    arguments: ContextItem {
                        text: r#"{"text":"working"}"#.to_string(),
                    },
                })],
            ),
            ChatMessage::new(
                ChatRole::Assistant,
                vec![ChatMessageItem::Context(ContextItem {
                    text: "done".to_string(),
                })],
            ),
        ]));
        let (started_tx, started_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let tools = Arc::new(BlockingToolExecutor::new(started_tx, release_rx));
        let catalog = builtin_tool_catalog(BuiltinToolCatalogOptions {
            host_tool_scope: Some(HostToolScope::MainForeground),
            ..BuiltinToolCatalogOptions::default()
        })
        .unwrap();
        let mut actor =
            SessionActor::new(test_model_config(), provider, tools, inbox, events, catalog);

        assert_eq!(
            actor.step().expect("initial should apply"),
            SessionActorStep::ProcessedControl
        );
        assert_eq!(
            actor.step().expect("tool batch should start"),
            SessionActorStep::WaitingToolBatch
        );
        started_rx
            .recv()
            .expect("tool batch should start after model tool call");

        let state_path = std::env::current_dir()
            .unwrap()
            .join(".log")
            .join("stellaclaw")
            .join(&session_id)
            .join("session.json");
        let state_before_release: SessionActorPersistedState = serde_json::from_str(
            &fs::read_to_string(&state_path).expect("safe state should exist"),
        )
        .expect("safe state should parse");
        assert_eq!(state_before_release.current_messages.len(), 1);
        assert_eq!(
            count_unclosed_tool_calls(&state_before_release.current_messages),
            0
        );

        release_tx.send(()).expect("tool wait should release");
        let mut reached_idle = false;
        for _ in 0..20 {
            if actor.step().expect("actor should advance") == SessionActorStep::Idle {
                reached_idle = true;
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert!(
            reached_idle,
            "actor should reach idle after tool completion"
        );

        let state_after_release: SessionActorPersistedState = serde_json::from_str(
            &fs::read_to_string(&state_path).expect("closed state should exist"),
        )
        .expect("closed state should parse");
        assert!(state_after_release.current_messages.len() >= 4);
        assert_eq!(
            count_unclosed_tool_calls(&state_after_release.current_messages),
            0
        );
        assert!(state_after_release.current_messages.iter().any(|message| {
            message
                .data
                .iter()
                .any(|item| matches!(item, ChatMessageItem::ToolCall(_)))
        }));
        assert!(state_after_release.current_messages.iter().any(|message| {
            message
                .data
                .iter()
                .any(|item| matches!(item, ChatMessageItem::ToolResult(_)))
        }));
    }

    #[test]
    fn newer_user_message_interrupts_active_tool_batch_and_runs_next_turn() {
        let _cwd = temp_cwd("actor-user-interrupt");
        let session_id = test_session_id("session_user_interrupt");
        let (inbox, mailbox) = test_inbox();
        mailbox.append(
            SessionMailboxKind::Control,
            SessionRequest::Initial {
                initial: SessionInitial::new(session_id, super::super::SessionType::Foreground),
            },
        );
        mailbox.append(
            SessionMailboxKind::Data,
            SessionRequest::EnqueueUserMessage {
                message: ChatMessage::new(
                    ChatRole::User,
                    vec![ChatMessageItem::Context(ContextItem {
                        text: "run a slow tool".to_string(),
                    })],
                ),
            },
        );
        let events = Arc::new(MemoryEventSink::default());
        let provider = Arc::new(ScriptedProvider::new(vec![
            ChatMessage::new(
                ChatRole::Assistant,
                vec![ChatMessageItem::ToolCall(ToolCallItem {
                    tool_call_id: "call_1".to_string(),
                    tool_name: "user_tell".to_string(),
                    arguments: ContextItem {
                        text: r#"{"text":"working"}"#.to_string(),
                    },
                })],
            ),
            ChatMessage::new(
                ChatRole::Assistant,
                vec![ChatMessageItem::Context(ContextItem {
                    text: "handled newer message".to_string(),
                })],
            ),
        ]));
        let (started_tx, started_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let (interrupt_tx, interrupt_rx) = mpsc::channel();
        let tools = Arc::new(BlockingToolExecutor::with_interrupt_tx(
            started_tx,
            release_rx,
            Some(interrupt_tx),
        ));
        let catalog = builtin_tool_catalog(BuiltinToolCatalogOptions {
            host_tool_scope: Some(HostToolScope::MainForeground),
            ..BuiltinToolCatalogOptions::default()
        })
        .unwrap();
        let mut actor = SessionActor::new(
            test_model_config(),
            provider,
            tools,
            inbox,
            events.clone(),
            catalog,
        );

        assert_eq!(
            actor.step().expect("initial should apply"),
            SessionActorStep::ProcessedControl
        );
        assert_eq!(
            actor.step().expect("tool batch should start"),
            SessionActorStep::WaitingToolBatch
        );
        started_rx
            .recv()
            .expect("tool batch should start after model tool call");

        mailbox.append(
            SessionMailboxKind::Data,
            SessionRequest::EnqueueUserMessage {
                message: ChatMessage::new(
                    ChatRole::User,
                    vec![ChatMessageItem::Context(ContextItem {
                        text: "new instruction".to_string(),
                    })],
                ),
            },
        );
        assert_eq!(
            actor
                .step()
                .expect("new user message should request interrupt"),
            SessionActorStep::WaitingToolBatch
        );
        interrupt_rx
            .recv()
            .expect("new user message should interrupt active tool batch");

        release_tx.send(()).expect("tool wait should release");
        let mut yielded = false;
        for _ in 0..20 {
            if actor.step().expect("interrupted batch should yield")
                == SessionActorStep::ProcessedData
            {
                yielded = true;
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert!(yielded, "interrupted batch should yield after completion");
        assert_eq!(
            actor.step().expect("new user message should run next"),
            SessionActorStep::ProcessedData
        );

        let completed = events
            .events
            .lock()
            .unwrap()
            .iter()
            .filter_map(|event| match event {
                SessionEvent::TurnCompleted { message } => Some(message_text_for_test(message)),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(completed, vec!["handled newer message".to_string()]);
        let progress = events
            .events
            .lock()
            .unwrap()
            .iter()
            .filter_map(|event| match event {
                SessionEvent::Progress { message } => Some(message.clone()),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert!(progress
            .iter()
            .all(|message| !message.contains("newer user message")));
    }

    #[test]
    fn compresses_history_before_appending_next_data_message_when_threshold_is_exceeded() {
        let _cwd = temp_cwd("actor-compression");
        let (inbox, mailbox) = test_inbox();
        let mut initial = SessionInitial::new(
            test_session_id("session_compression"),
            super::super::SessionType::Foreground,
        );
        initial.compression_threshold_tokens = Some(32);
        initial.compression_retain_recent_tokens = Some(12);
        mailbox.append(
            SessionMailboxKind::Control,
            SessionRequest::Initial { initial },
        );
        mailbox.append(
            SessionMailboxKind::Data,
            SessionRequest::EnqueueUserMessage {
                message: ChatMessage::new(
                    ChatRole::User,
                    vec![ChatMessageItem::Context(ContextItem {
                        text: "old ".repeat(50),
                    })],
                ),
            },
        );
        mailbox.append(
            SessionMailboxKind::Data,
            SessionRequest::EnqueueUserMessage {
                message: ChatMessage::new(
                    ChatRole::User,
                    vec![ChatMessageItem::Context(ContextItem {
                        text: "second request".to_string(),
                    })],
                ),
            },
        );
        let events = Arc::new(MemoryEventSink::default());
        let provider = Arc::new(ScriptedProvider::new(vec![
            ChatMessage::new(
                ChatRole::Assistant,
                vec![ChatMessageItem::Context(ContextItem {
                    text: "first final".to_string(),
                })],
            ),
            ChatMessage::new(
                ChatRole::Assistant,
                vec![ChatMessageItem::Context(ContextItem {
                    text: "summary".to_string(),
                })],
            ),
            ChatMessage::new(
                ChatRole::Assistant,
                vec![ChatMessageItem::Context(ContextItem {
                    text: "second final".to_string(),
                })],
            ),
        ]));
        let tools = Arc::new(EchoToolExecutor::new());
        let catalog = builtin_tool_catalog(BuiltinToolCatalogOptions::default()).unwrap();
        let (model_config, tokenizer_dir) = test_model_config_with_tokenizer();
        let mut actor = SessionActor::new(model_config, provider, tools, inbox, events, catalog);

        actor.run_until_idle(8).expect("actor should run");

        assert!(message_text_for_test(&actor.history()[0]).contains(COMPRESSION_MARKER));
        assert!(message_text_for_test(&actor.history()[0]).contains("summary"));
        assert!(actor
            .history()
            .iter()
            .any(|message| message_text_for_test(message).contains("second final")));

        fs::remove_dir_all(tokenizer_dir).expect("tokenizer dir should be removed");
    }

    #[test]
    fn idle_compaction_runs_after_cache_lead_time() {
        let _cwd = temp_cwd("actor-idle-compression");
        let (inbox, mailbox) = test_inbox();
        let mut initial = SessionInitial::new(
            test_session_id("session_idle_compression"),
            super::super::SessionType::Foreground,
        );
        initial.compression_threshold_tokens = Some(1_000);
        initial.compression_retain_recent_tokens = Some(12);
        mailbox.append(
            SessionMailboxKind::Control,
            SessionRequest::Initial { initial },
        );
        mailbox.append(
            SessionMailboxKind::Data,
            SessionRequest::EnqueueUserMessage {
                message: ChatMessage::new(
                    ChatRole::User,
                    vec![ChatMessageItem::Context(ContextItem {
                        text: "old ".repeat(50),
                    })],
                ),
            },
        );
        let events = Arc::new(MemoryEventSink::default());
        let provider = Arc::new(ScriptedProvider::new(vec![
            ChatMessage::new(
                ChatRole::Assistant,
                vec![ChatMessageItem::Context(ContextItem {
                    text: "first final".to_string(),
                })],
            ),
            ChatMessage::new(
                ChatRole::Assistant,
                vec![ChatMessageItem::Context(ContextItem {
                    text: "summary".to_string(),
                })],
            ),
        ]));
        let tools = Arc::new(EchoToolExecutor::new());
        let catalog = builtin_tool_catalog(BuiltinToolCatalogOptions::default()).unwrap();
        let (mut model_config, tokenizer_dir) = test_model_config_with_tokenizer();
        model_config.token_max_context = 64;
        model_config.cache_timeout = 300;
        let mut actor = SessionActor::new(
            model_config,
            provider.clone(),
            tools,
            inbox,
            events,
            catalog,
        );

        actor.run_until_idle(4).expect("actor should run");
        assert_eq!(actor.history().len(), 2);
        assert!(!message_text_for_test(&actor.history()[0]).contains(COMPRESSION_MARKER));

        actor.last_agent_returned_at = Some(Instant::now() - Duration::from_secs(271));
        let compacted = actor
            .try_run_idle_compaction()
            .expect("idle compaction should not fail");

        assert!(compacted);
        assert!(message_text_for_test(&actor.history()[0]).contains(COMPRESSION_MARKER));
        assert!(message_text_for_test(&actor.history()[0]).contains("summary"));
        assert_eq!(provider.seen_requests.lock().unwrap().len(), 2);

        fs::remove_dir_all(tokenizer_dir).expect("tokenizer dir should be removed");
    }

    fn message_text_for_test(message: &ChatMessage) -> String {
        message
            .data
            .iter()
            .filter_map(|item| match item {
                ChatMessageItem::Context(context) => Some(context.text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn text_message(role: ChatRole, text: &str) -> ChatMessage {
        ChatMessage::new(
            role,
            vec![ChatMessageItem::Context(ContextItem {
                text: text.to_string(),
            })],
        )
    }

    fn session_view_payload_for_test(
        events: &MemoryEventSink,
        expected_query_id: &str,
    ) -> serde_json::Value {
        events
            .events
            .lock()
            .unwrap()
            .iter()
            .rev()
            .find_map(|event| match event {
                SessionEvent::SessionViewResult { query_id, payload }
                    if query_id == expected_query_id =>
                {
                    Some(payload.clone())
                }
                _ => None,
            })
            .expect("session view result should exist")
    }
}
