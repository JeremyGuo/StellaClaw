mod brave_search;
mod brave_search_image;
mod brave_search_news;
mod brave_search_video;
mod claude_code;
mod codex_subscription;
mod common;
mod error_report;
mod forkserver;
mod openai_image_edit;
mod openrouter_completion;
mod openrouter_responses;
mod output_persistor;
mod pricing;

use std::{
    error::Error as StdError,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    thread,
    time::Duration,
};

use crossbeam_channel::{select, Receiver, Sender};
#[cfg(not(test))]
use rand::Rng;
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::{
    model_config::{ModelConfig, ProviderType, RetryMode},
    session_actor::{ChatMessage, ToolDefinition, ToolSet},
};

pub use brave_search::BraveSearchProvider;
pub use brave_search_image::BraveSearchImageProvider;
pub use brave_search_news::BraveSearchNewsProvider;
pub use brave_search_video::BraveSearchVideoProvider;
pub use claude_code::ClaudeCodeProvider;
pub use codex_subscription::CodexSubscriptionProvider;
pub use error_report::ProviderErrorReport;
pub use forkserver::{
    global_provider_fork_server, init_global_provider_fork_server, ForkServerProviderFactory,
    ProviderForkServerEvent, ProviderRequestAbortHandle, ProviderRequestForkServer,
    ProviderRequestHandle,
};
pub use openai_image_edit::OpenAiImageEditProvider;
pub use openrouter_completion::OpenRouterCompletionProvider;
pub use openrouter_responses::OpenRouterResponsesProvider;
pub use output_persistor::{OutputPersistor, OutputPersistorError};

pub fn provider_from_model_config(model_config: ModelConfig) -> Box<dyn Provider + Send + Sync> {
    let backend = provider_backend_from_model_config(&model_config);
    Box::new(ModelBoundProvider {
        model_config,
        backend,
    })
}

pub fn provider_system_prompt_from_model_config(
    model_config: &ModelConfig,
) -> Result<Option<String>, ProviderError> {
    provider_backend_from_model_config(model_config).system_prompt_for_model(model_config)
}

pub trait Provider {
    fn model_config(&self) -> &ModelConfig;

    fn system_prompt_for_model(
        &self,
        _model_config: &ModelConfig,
    ) -> Result<Option<String>, ProviderError> {
        Ok(None)
    }

    fn system_prompt(&self) -> Result<Option<String>, ProviderError> {
        self.system_prompt_for_model(self.model_config())
    }

    fn normalize_messages_for_provider(&self, messages: &[ChatMessage]) -> Vec<ChatMessage> {
        messages.to_vec()
    }

    fn tool_set(&self) -> Option<Arc<dyn ToolSet>> {
        None
    }

    fn send(&self, request: ProviderRequest<'_>) -> Result<ChatMessage, ProviderError>;

    fn send_with_stream(
        &self,
        request: ProviderRequest<'_>,
        _on_stream: &mut dyn FnMut(ProviderStreamEvent),
    ) -> Result<ChatMessage, ProviderError> {
        self.send(request)
    }

    fn before_retry(&self, _error: &ProviderError) {}

    fn start_worker_request(
        &self,
        _request: ProviderRequestOwned,
    ) -> Result<Option<ProviderRequestHandle>, ProviderError> {
        Ok(None)
    }
}

pub(crate) trait ProviderBackend: Send + Sync {
    fn system_prompt_for_model(
        &self,
        _model_config: &ModelConfig,
    ) -> Result<Option<String>, ProviderError> {
        Ok(None)
    }

    fn normalize_messages_for_provider(
        &self,
        _model_config: &ModelConfig,
        messages: &[ChatMessage],
    ) -> Vec<ChatMessage> {
        messages.to_vec()
    }

    fn tool_set(&self, _model_config: &ModelConfig) -> Option<Arc<dyn ToolSet>> {
        None
    }

    fn send(
        &self,
        model_config: &ModelConfig,
        request: ProviderRequest<'_>,
    ) -> Result<ChatMessage, ProviderError>;

    fn send_with_stream(
        &self,
        model_config: &ModelConfig,
        request: ProviderRequest<'_>,
        _on_stream: &mut dyn FnMut(ProviderStreamEvent),
    ) -> Result<ChatMessage, ProviderError> {
        self.send(model_config, request)
    }

    fn before_retry(&self, _model_config: &ModelConfig, _error: &ProviderError) {}
}

struct ModelBoundProvider {
    model_config: ModelConfig,
    backend: Box<dyn ProviderBackend>,
}

impl Provider for ModelBoundProvider {
    fn model_config(&self) -> &ModelConfig {
        &self.model_config
    }

    fn system_prompt(&self) -> Result<Option<String>, ProviderError> {
        self.system_prompt_for_model(&self.model_config)
    }

    fn system_prompt_for_model(
        &self,
        model_config: &ModelConfig,
    ) -> Result<Option<String>, ProviderError> {
        self.backend.system_prompt_for_model(model_config)
    }

    fn normalize_messages_for_provider(&self, messages: &[ChatMessage]) -> Vec<ChatMessage> {
        self.backend
            .normalize_messages_for_provider(&self.model_config, messages)
    }

    fn tool_set(&self) -> Option<Arc<dyn ToolSet>> {
        self.backend.tool_set(&self.model_config)
    }

    fn send(&self, request: ProviderRequest<'_>) -> Result<ChatMessage, ProviderError> {
        self.backend.send(&self.model_config, request)
    }

    fn send_with_stream(
        &self,
        request: ProviderRequest<'_>,
        on_stream: &mut dyn FnMut(ProviderStreamEvent),
    ) -> Result<ChatMessage, ProviderError> {
        self.backend
            .send_with_stream(&self.model_config, request, on_stream)
    }

    fn before_retry(&self, error: &ProviderError) {
        self.backend.before_retry(&self.model_config, error);
    }
}

pub struct ProviderRetryEvent<'a> {
    pub retry: u64,
    pub max_retries: u64,
    pub delay: Duration,
    pub error: &'a ProviderError,
}

pub fn send_provider_request_with_retry<F>(
    provider: &(dyn Provider + Send + Sync),
    request: ProviderRequest<'_>,
    on_retry: F,
) -> Result<ChatMessage, ProviderError>
where
    F: FnMut(ProviderRetryEvent<'_>),
{
    send_provider_request_with_retry_and_stream(provider, request, on_retry, |_| {})
}

pub fn send_provider_request_with_retry_and_stream<F, S>(
    provider: &(dyn Provider + Send + Sync),
    request: ProviderRequest<'_>,
    mut on_retry: F,
    mut on_stream: S,
) -> Result<ChatMessage, ProviderError>
where
    F: FnMut(ProviderRetryEvent<'_>),
    S: FnMut(ProviderStreamEvent),
{
    let mut retries_used = 0_u64;
    loop {
        match provider.send_with_stream(request.clone(), &mut on_stream) {
            Ok(response) => return Ok(response),
            Err(error) if error.is_transient() => {
                let Some(delay) = transient_provider_retry_delay(
                    &provider.model_config().retry_mode,
                    retries_used,
                ) else {
                    return Err(error);
                };
                retries_used = retries_used.saturating_add(1);
                provider.before_retry(&error);
                on_retry(ProviderRetryEvent {
                    retry: retries_used,
                    max_retries: retry_mode_max_retries(&provider.model_config().retry_mode),
                    delay,
                    error: &error,
                });
                if !delay.is_zero() {
                    std::thread::sleep(delay);
                }
            }
            Err(error) => return Err(error),
        }
    }
}

pub fn retry_mode_max_retries(retry_mode: &RetryMode) -> u64 {
    match retry_mode {
        RetryMode::Once => 0,
        RetryMode::RandomInterval { max_retries, .. } => *max_retries,
    }
}

fn transient_provider_retry_delay(retry_mode: &RetryMode, retries_used: u64) -> Option<Duration> {
    match retry_mode {
        RetryMode::Once => None,
        RetryMode::RandomInterval {
            max_interval_secs,
            max_retries,
        } => {
            if retries_used >= *max_retries {
                return None;
            }
            #[cfg(test)]
            {
                let _ = max_interval_secs;
                Some(Duration::ZERO)
            }
            #[cfg(not(test))]
            {
                let sleep_secs = if *max_interval_secs == 0 {
                    0
                } else {
                    rand::rng().random_range(1..=*max_interval_secs)
                };
                Some(Duration::from_secs(sleep_secs))
            }
        }
    }
}

fn provider_backend_from_model_config(
    model_config: &ModelConfig,
) -> Box<dyn ProviderBackend + Send + Sync> {
    match model_config.provider_type {
        ProviderType::OpenRouterCompletion => Box::new(OpenRouterCompletionProvider::new()),
        ProviderType::OpenRouterResponses => Box::new(OpenRouterResponsesProvider::new()),
        ProviderType::OpenAiImageEdit => Box::new(OpenAiImageEditProvider::new()),
        ProviderType::ClaudeCode => Box::new(ClaudeCodeProvider::new()),
        ProviderType::CodexSubscription => Box::new(CodexSubscriptionProvider::new()),
        ProviderType::BraveSearch => Box::new(BraveSearchProvider::new()),
        ProviderType::BraveSearchImage => Box::new(BraveSearchImageProvider::new()),
        ProviderType::BraveSearchVideo => Box::new(BraveSearchVideoProvider::new()),
        ProviderType::BraveSearchNews => Box::new(BraveSearchNewsProvider::new()),
    }
}

#[derive(Debug, Clone)]
pub struct ProviderRequest<'a> {
    pub system_prompt: Option<&'a str>,
    pub messages: &'a [ChatMessage],
    pub tools: Vec<&'a ToolDefinition>,
}

impl<'a> ProviderRequest<'a> {
    pub fn new(messages: &'a [ChatMessage]) -> Self {
        Self {
            system_prompt: None,
            messages,
            tools: Vec::new(),
        }
    }

    pub fn with_system_prompt(mut self, system_prompt: Option<&'a str>) -> Self {
        self.system_prompt = system_prompt;
        self
    }

    pub fn with_tools(mut self, tools: Vec<&'a ToolDefinition>) -> Self {
        self.tools = tools;
        self
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProviderRequestOwned {
    pub system_prompt: Option<String>,
    pub messages: Vec<ChatMessage>,
    pub tools: Vec<ToolDefinition>,
}

impl ProviderRequestOwned {
    pub fn new(messages: Vec<ChatMessage>) -> Self {
        Self {
            system_prompt: None,
            messages,
            tools: Vec::new(),
        }
    }

    pub fn from_provider_request(request: &ProviderRequest<'_>) -> Self {
        Self {
            system_prompt: request.system_prompt.map(str::to_string),
            messages: request.messages.to_vec(),
            tools: request.tools.iter().map(|tool| (*tool).clone()).collect(),
        }
    }

    pub fn as_provider_request(&self) -> ProviderRequest<'_> {
        ProviderRequest {
            system_prompt: self.system_prompt.as_deref(),
            messages: &self.messages,
            tools: self.tools.iter().collect(),
        }
    }
}

#[derive(Debug)]
pub enum ProviderEvent {
    Stream {
        request_id: String,
        event: ProviderStreamEvent,
    },
    Retry {
        request_id: String,
        retry: u64,
        max_retries: u64,
        delay_ms: u128,
        error: String,
    },
    Result {
        request_id: String,
        result: Result<ChatMessage, ProviderError>,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ProviderStreamEvent {
    KeepAlive,
    OutputTextDelta {
        item_id: Option<String>,
        delta: String,
    },
    ToolCallInputDelta {
        item_id: String,
        call_id: Option<String>,
        delta: String,
    },
    ReasoningSummaryDelta {
        item_id: Option<String>,
        delta: String,
        summary_index: i64,
    },
    ReasoningSummaryPartAdded {
        item_id: Option<String>,
        summary_index: i64,
    },
    RawJson {
        value: serde_json::Value,
    },
}

#[derive(Debug)]
enum ProviderCommand {
    Start {
        request_id: String,
        request: ProviderRequestOwned,
    },
    Abort,
    Shutdown,
}

#[derive(Debug)]
struct ProviderCompletion {
    request_id: String,
    result: Result<ChatMessage, ProviderError>,
}

pub struct ProviderSession {
    kind: ProviderSessionKind,
    command_tx: Sender<ProviderCommand>,
    event_rx: Receiver<ProviderEvent>,
}

#[derive(Clone)]
enum ProviderSessionKind {
    Direct {
        provider: Arc<dyn Provider + Send + Sync>,
    },
    ForkServer {
        model_config: ModelConfig,
        fork_server: Arc<ProviderRequestForkServer>,
    },
}

impl ProviderSession {
    pub fn new(provider: Arc<dyn Provider + Send + Sync>) -> Self {
        let (command_tx, command_rx) = crossbeam_channel::unbounded();
        let (event_tx, event_rx) = crossbeam_channel::unbounded();
        let provider_for_thread = provider.clone();
        thread::spawn(move || {
            direct_provider_session_loop(provider_for_thread, command_rx, event_tx)
        });
        Self {
            kind: ProviderSessionKind::Direct { provider },
            command_tx,
            event_rx,
        }
    }

    pub fn fork_server(
        model_config: ModelConfig,
        fork_server: Arc<ProviderRequestForkServer>,
    ) -> Self {
        let (command_tx, command_rx) = crossbeam_channel::unbounded();
        let (event_tx, event_rx) = crossbeam_channel::unbounded();
        let runtime_model_config = model_config.clone();
        let runtime_fork_server = fork_server.clone();
        thread::spawn(move || {
            fork_server_provider_session_loop(
                runtime_model_config,
                runtime_fork_server,
                command_rx,
                event_tx,
            )
        });
        Self {
            kind: ProviderSessionKind::ForkServer {
                model_config,
                fork_server,
            },
            command_tx,
            event_rx,
        }
    }

    pub fn model_config(&self) -> &ModelConfig {
        match &self.kind {
            ProviderSessionKind::Direct { provider } => provider.model_config(),
            ProviderSessionKind::ForkServer { model_config, .. } => model_config,
        }
    }

    pub fn system_prompt(&self) -> Result<Option<String>, ProviderError> {
        self.system_prompt_for_model(self.model_config())
    }

    pub fn system_prompt_for_model(
        &self,
        model_config: &ModelConfig,
    ) -> Result<Option<String>, ProviderError> {
        match &self.kind {
            ProviderSessionKind::Direct { provider } => {
                provider.system_prompt_for_model(model_config)
            }
            ProviderSessionKind::ForkServer { .. } => {
                provider_backend_from_model_config(model_config)
                    .system_prompt_for_model(model_config)
            }
        }
    }

    pub fn normalize_messages_for_provider(&self, messages: &[ChatMessage]) -> Vec<ChatMessage> {
        match &self.kind {
            ProviderSessionKind::Direct { provider } => {
                provider.normalize_messages_for_provider(messages)
            }
            ProviderSessionKind::ForkServer { .. } => messages.to_vec(),
        }
    }

    pub fn tool_set(&self) -> Option<Arc<dyn ToolSet>> {
        match &self.kind {
            ProviderSessionKind::Direct { provider } => provider.tool_set(),
            ProviderSessionKind::ForkServer { model_config, .. } => {
                provider_backend_from_model_config(model_config).tool_set(model_config)
            }
        }
    }

    pub fn start(
        &self,
        request_id: String,
        request: ProviderRequestOwned,
    ) -> Result<(), ProviderError> {
        self.command_tx
            .send(ProviderCommand::Start {
                request_id,
                request,
            })
            .map_err(|_| ProviderError::Subprocess("provider session stopped".to_string()))
    }

    pub fn abort(&self) -> Result<(), ProviderError> {
        self.command_tx
            .send(ProviderCommand::Abort)
            .map_err(|_| ProviderError::Subprocess("provider session stopped".to_string()))
    }

    pub fn event_rx(&self) -> Receiver<ProviderEvent> {
        self.event_rx.clone()
    }
}

impl Provider for ProviderSession {
    fn model_config(&self) -> &ModelConfig {
        self.model_config()
    }

    fn system_prompt(&self) -> Result<Option<String>, ProviderError> {
        ProviderSession::system_prompt(self)
    }

    fn system_prompt_for_model(
        &self,
        model_config: &ModelConfig,
    ) -> Result<Option<String>, ProviderError> {
        ProviderSession::system_prompt_for_model(self, model_config)
    }

    fn normalize_messages_for_provider(&self, messages: &[ChatMessage]) -> Vec<ChatMessage> {
        self.normalize_messages_for_provider(messages)
    }

    fn tool_set(&self) -> Option<Arc<dyn ToolSet>> {
        ProviderSession::tool_set(self)
    }

    fn send(&self, request: ProviderRequest<'_>) -> Result<ChatMessage, ProviderError> {
        match &self.kind {
            ProviderSessionKind::Direct { provider } => provider.send(request),
            ProviderSessionKind::ForkServer {
                model_config,
                fork_server,
            } => fork_server
                .start(
                    model_config.clone(),
                    ProviderRequestOwned::from_provider_request(&request),
                )?
                .wait(),
        }
    }

    fn before_retry(&self, error: &ProviderError) {
        if let ProviderSessionKind::Direct { provider } = &self.kind {
            provider.before_retry(error);
        }
    }

    fn start_worker_request(
        &self,
        request: ProviderRequestOwned,
    ) -> Result<Option<ProviderRequestHandle>, ProviderError> {
        match &self.kind {
            ProviderSessionKind::Direct { provider } => provider.start_worker_request(request),
            ProviderSessionKind::ForkServer {
                model_config,
                fork_server,
            } => Ok(Some(fork_server.start(model_config.clone(), request)?)),
        }
    }
}

impl Drop for ProviderSession {
    fn drop(&mut self) {
        let _ = self.command_tx.send(ProviderCommand::Shutdown);
    }
}

pub trait ProviderFactory: Send + Sync {
    fn create(&self, model_config: ModelConfig) -> Result<ProviderSession, ProviderError>;
}

pub struct DirectProviderFactory;

impl ProviderFactory for DirectProviderFactory {
    fn create(&self, model_config: ModelConfig) -> Result<ProviderSession, ProviderError> {
        Ok(ProviderSession::new(Arc::from(provider_from_model_config(
            model_config,
        ))))
    }
}

fn direct_provider_session_loop(
    provider: Arc<dyn Provider + Send + Sync>,
    command_rx: Receiver<ProviderCommand>,
    event_tx: Sender<ProviderEvent>,
) {
    let (completion_tx, completion_rx) = crossbeam_channel::unbounded();
    let mut active: Option<ProviderActiveRequest> = None;

    loop {
        select! {
            recv(command_rx) -> command => {
                let Ok(command) = command else {
                    break;
                };
                match command {
                    ProviderCommand::Start { request_id, request } => {
                        if active.is_some() {
                            let _ = event_tx.send(ProviderEvent::Result {
                                request_id,
                                result: Err(ProviderError::Subprocess(
                                    "provider session is busy".to_string(),
                                )),
                            });
                            continue;
                        }
                        match start_provider_session_request(
                            provider.clone(),
                            request_id.clone(),
                            request,
                            event_tx.clone(),
                            completion_tx.clone(),
                        ) {
                            Ok(active_request) => active = Some(active_request),
                            Err(error) => {
                                let _ = event_tx.send(ProviderEvent::Result {
                                    request_id,
                                    result: Err(error),
                                });
                            }
                        }
                    }
                    ProviderCommand::Abort => {
                        if let Some(active_request) = active.take() {
                            active_request.cancel();
                            let _ = event_tx.send(ProviderEvent::Result {
                                request_id: active_request.request_id,
                                result: Err(ProviderError::Subprocess(
                                    "provider request cancelled".to_string(),
                                )),
                            });
                        }
                    }
                    ProviderCommand::Shutdown => {
                        if let Some(active_request) = active.take() {
                            active_request.cancel();
                        }
                        break;
                    }
                }
            }
            recv(completion_rx) -> completion => {
                let Ok(completion) = completion else {
                    break;
                };
                if active
                    .as_ref()
                    .is_some_and(|active| active.request_id == completion.request_id)
                {
                    active = None;
                    let _ = event_tx.send(ProviderEvent::Result {
                        request_id: completion.request_id,
                        result: completion.result,
                    });
                }
            }
        }
    }
}

fn fork_server_provider_session_loop(
    model_config: ModelConfig,
    fork_server: Arc<ProviderRequestForkServer>,
    command_rx: Receiver<ProviderCommand>,
    event_tx: Sender<ProviderEvent>,
) {
    let (fork_server_event_tx, fork_server_event_rx) = crossbeam_channel::unbounded();
    let mut worker: Option<ProviderWorkerBinding> = None;
    let mut active: Option<ProviderActiveRequest> = None;

    loop {
        select! {
            recv(command_rx) -> command => {
                let Ok(command) = command else {
                    break;
                };
                match command {
                    ProviderCommand::Start { request_id, request } => {
                        if active.is_some() {
                            let _ = event_tx.send(ProviderEvent::Result {
                                request_id,
                                result: Err(ProviderError::Subprocess(
                                    "provider session is busy".to_string(),
                                )),
                            });
                            continue;
                        }
                        match start_fork_server_session_request(
                            &model_config,
                            &fork_server,
                            &mut worker,
                            request_id.clone(),
                            request,
                            fork_server_event_tx.clone(),
                        ) {
                            Ok(active_request) => active = Some(active_request),
                            Err(error) => {
                                let _ = event_tx.send(ProviderEvent::Result {
                                    request_id,
                                    result: Err(error),
                                });
                            }
                        }
                    }
                    ProviderCommand::Abort => {
                        if let Some(active_request) = active.take() {
                            active_request.cancel();
                            let _ = event_tx.send(ProviderEvent::Result {
                                request_id: active_request.request_id,
                                result: Err(ProviderError::Subprocess(
                                    "provider request cancelled".to_string(),
                                )),
                            });
                        }
                    }
                    ProviderCommand::Shutdown => {
                        if let Some(active_request) = active.take() {
                            active_request.cancel();
                        }
                        if let Some(worker) = worker.take() {
                            let _ = fork_server.shutdown_worker(&worker.worker_id);
                        }
                        break;
                    }
                }
            }
            recv(fork_server_event_rx) -> event => {
                let Ok(event) = event else {
                    if let Some(active_request) = active.take() {
                        let _ = event_tx.send(ProviderEvent::Result {
                            request_id: active_request.request_id,
                            result: Err(ProviderError::Subprocess(
                                "provider request runtime disconnected".to_string(),
                            )),
                        });
                    }
                    break;
                };
                match event {
                    ProviderForkServerEvent::Stream { request_id, event } => {
                        if active
                            .as_ref()
                            .is_some_and(|active| active.request_id == request_id)
                        {
                            let _ = event_tx.send(ProviderEvent::Stream {
                                request_id,
                                event,
                            });
                        }
                    }
                    ProviderForkServerEvent::Completed { request_id, result } => {
                        if !active
                            .as_ref()
                            .is_some_and(|active| active.request_id == request_id)
                        {
                            continue;
                        }
                        active = None;
                        if should_recreate_provider_worker(&result) {
                            worker = None;
                        }
                        let _ = event_tx.send(ProviderEvent::Result {
                            request_id,
                            result,
                        });
                    }
                }
            }
        }
    }
}

#[derive(Debug, Clone)]
struct ProviderWorkerBinding {
    worker_id: String,
    signature: String,
}

fn start_fork_server_session_request(
    model_config: &ModelConfig,
    fork_server: &ProviderRequestForkServer,
    worker: &mut Option<ProviderWorkerBinding>,
    request_id: String,
    request: ProviderRequestOwned,
    event_tx: Sender<ProviderForkServerEvent>,
) -> Result<ProviderActiveRequest, ProviderError> {
    let abort_handle = start_fork_server_request(
        model_config,
        fork_server,
        worker,
        &request_id,
        request,
        event_tx,
    )?;
    Ok(ProviderActiveRequest {
        request_id,
        cancelled: Arc::new(AtomicBool::new(false)),
        abort_handle: Some(abort_handle),
    })
}

#[cfg(unix)]
fn start_fork_server_request(
    model_config: &ModelConfig,
    fork_server: &ProviderRequestForkServer,
    worker: &mut Option<ProviderWorkerBinding>,
    request_id: &str,
    request: ProviderRequestOwned,
    event_tx: Sender<ProviderForkServerEvent>,
) -> Result<ProviderRequestAbortHandle, ProviderError> {
    let worker_id = ensure_fork_server_worker(model_config, fork_server, worker)?;
    fork_server.start_on_worker_event(worker_id, request_id.to_string(), request, event_tx)
}

#[cfg(not(unix))]
fn start_fork_server_request(
    model_config: &ModelConfig,
    fork_server: &ProviderRequestForkServer,
    _worker: &mut Option<ProviderWorkerBinding>,
    request_id: &str,
    request: ProviderRequestOwned,
    event_tx: Sender<ProviderForkServerEvent>,
) -> Result<ProviderRequestAbortHandle, ProviderError> {
    fork_server.start_event(
        model_config.clone(),
        request_id.to_string(),
        request,
        event_tx,
    )
}

#[cfg(unix)]
fn ensure_fork_server_worker(
    model_config: &ModelConfig,
    fork_server: &ProviderRequestForkServer,
    worker: &mut Option<ProviderWorkerBinding>,
) -> Result<String, ProviderError> {
    let signature = provider_worker_signature(model_config);
    if let Some(binding) = worker.as_ref() {
        if binding.signature == signature {
            return Ok(binding.worker_id.clone());
        }
        let _ = fork_server.shutdown_worker(&binding.worker_id);
    }

    let worker_id = fork_server.start_worker(model_config.clone())?;
    *worker = Some(ProviderWorkerBinding {
        worker_id: worker_id.clone(),
        signature,
    });
    Ok(worker_id)
}

fn provider_worker_signature(model_config: &ModelConfig) -> String {
    serde_json::to_string(model_config).unwrap_or_else(|_| {
        format!(
            "{:?}:{}:{}",
            model_config.provider_type, model_config.model_name, model_config.url
        )
    })
}

fn should_recreate_provider_worker(result: &Result<ChatMessage, ProviderError>) -> bool {
    matches!(
        result,
        Err(ProviderError::Subprocess(message))
            if message.contains("unknown provider worker")
                || message.contains("provider worker")
                    && message.contains("exited before completing request")
    )
}

struct ProviderActiveRequest {
    request_id: String,
    cancelled: Arc<AtomicBool>,
    abort_handle: Option<ProviderRequestAbortHandle>,
}

impl ProviderActiveRequest {
    fn cancel(&self) {
        self.cancelled.store(true, Ordering::SeqCst);
        if let Some(abort_handle) = &self.abort_handle {
            let _ = abort_handle.abort();
        }
    }
}

fn start_provider_session_request(
    provider: Arc<dyn Provider + Send + Sync>,
    request_id: String,
    request: ProviderRequestOwned,
    event_tx: Sender<ProviderEvent>,
    completion_tx: Sender<ProviderCompletion>,
) -> Result<ProviderActiveRequest, ProviderError> {
    if let Some(handle) = provider.start_worker_request(request.clone())? {
        let abort_handle = handle.abort_handle();
        let thread_request_id = request_id.clone();
        thread::spawn(move || {
            let result = handle.wait();
            let _ = completion_tx.send(ProviderCompletion {
                request_id: thread_request_id,
                result,
            });
        });
        return Ok(ProviderActiveRequest {
            request_id,
            cancelled: Arc::new(AtomicBool::new(false)),
            abort_handle: Some(abort_handle),
        });
    }

    let cancelled = Arc::new(AtomicBool::new(false));
    let thread_cancelled = cancelled.clone();
    let thread_request_id = request_id.clone();
    thread::spawn(move || {
        let stream_event_tx = event_tx.clone();
        let result = send_provider_request_with_retry_and_stream(
            provider.as_ref(),
            request.as_provider_request(),
            |retry| {
                let _ = event_tx.send(ProviderEvent::Retry {
                    request_id: thread_request_id.clone(),
                    retry: retry.retry,
                    max_retries: retry.max_retries,
                    delay_ms: retry.delay.as_millis(),
                    error: retry.error.to_string(),
                });
            },
            |event| {
                let _ = stream_event_tx.send(ProviderEvent::Stream {
                    request_id: thread_request_id.clone(),
                    event,
                });
            },
        );
        if !thread_cancelled.load(Ordering::SeqCst) {
            let _ = completion_tx.send(ProviderCompletion {
                request_id: thread_request_id,
                result,
            });
        }
    });

    Ok(ProviderActiveRequest {
        request_id,
        cancelled,
        abort_handle: None,
    })
}

#[derive(Debug, Error)]
pub enum ProviderError {
    #[error("missing api key in environment variable {0}")]
    MissingApiKeyEnv(String),
    #[error("http client build failed: {0}")]
    BuildHttpClient(reqwest::Error),
    #[error("request failed: {0}")]
    Request(String),
    #[error("request to {url} failed with status {status}: {body}")]
    HttpStatus {
        url: String,
        status: u16,
        body: String,
    },
    #[error("response body parse failed: {0}")]
    DecodeResponse(reqwest::Error),
    #[error("response json parse failed: {0}")]
    DecodeJson(serde_json::Error),
    #[error("invalid provider response: {0}")]
    InvalidResponse(String),
    #[error("provider rejected request as {kind}: {message}")]
    ProviderFailure {
        kind: ProviderFailureKind,
        message: String,
        body: String,
    },
    #[error("websocket provider request failed: {0}")]
    WebSocket(String),
    #[error("failed to persist provider output: {0}")]
    PersistOutput(#[from] OutputPersistorError),
    #[error("provider response did not include any completion choices")]
    EmptyChoices,
    #[error("provider request isolation failed: {0}")]
    Subprocess(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderFailureKind {
    RequestTooLarge,
    RateLimited,
    Authentication,
    Permission,
    CyberPolicy,
    ProviderUnavailable,
    Unknown,
}

impl std::fmt::Display for ProviderFailureKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let label = match self {
            Self::RequestTooLarge => "request_too_large",
            Self::RateLimited => "rate_limited",
            Self::Authentication => "authentication",
            Self::Permission => "permission",
            Self::CyberPolicy => "cyber_policy",
            Self::ProviderUnavailable => "provider_unavailable",
            Self::Unknown => "unknown",
        };
        f.write_str(label)
    }
}

impl ProviderError {
    pub fn request(error: reqwest::Error) -> Self {
        Self::Request(error_chain_message(&error))
    }

    pub fn is_transient(&self) -> bool {
        match self {
            Self::Request(_) | Self::WebSocket(_) | Self::Subprocess(_) => true,
            Self::HttpStatus { status, .. } => *status == 429 || (500..=599).contains(status),
            Self::ProviderFailure { kind, .. } => matches!(
                kind,
                ProviderFailureKind::RateLimited
                    | ProviderFailureKind::ProviderUnavailable
                    | ProviderFailureKind::Unknown
            ),
            Self::MissingApiKeyEnv(_)
            | Self::BuildHttpClient(_)
            | Self::DecodeResponse(_)
            | Self::DecodeJson(_)
            | Self::InvalidResponse(_)
            | Self::PersistOutput(_)
            | Self::EmptyChoices => false,
        }
    }

    pub fn is_request_too_large(&self) -> bool {
        match self {
            Self::ProviderFailure {
                kind: ProviderFailureKind::RequestTooLarge,
                ..
            } => true,
            Self::HttpStatus { status, body, .. } => *status == 413 || request_too_large_text(body),
            Self::Request(message)
            | Self::InvalidResponse(message)
            | Self::WebSocket(message)
            | Self::Subprocess(message) => request_too_large_text(message),
            Self::MissingApiKeyEnv(_)
            | Self::BuildHttpClient(_)
            | Self::DecodeResponse(_)
            | Self::DecodeJson(_)
            | Self::PersistOutput(_)
            | Self::ProviderFailure { .. }
            | Self::EmptyChoices => false,
        }
    }

    pub fn is_cyber_policy(&self) -> bool {
        match self {
            Self::ProviderFailure {
                kind: ProviderFailureKind::CyberPolicy,
                ..
            } => true,
            Self::HttpStatus { body, .. }
            | Self::Request(body)
            | Self::InvalidResponse(body)
            | Self::WebSocket(body)
            | Self::Subprocess(body) => cyber_policy_text(body),
            Self::MissingApiKeyEnv(_)
            | Self::BuildHttpClient(_)
            | Self::DecodeResponse(_)
            | Self::DecodeJson(_)
            | Self::PersistOutput(_)
            | Self::ProviderFailure { .. }
            | Self::EmptyChoices => false,
        }
    }
}

pub(crate) fn error_chain_message(error: &dyn StdError) -> String {
    let mut message = error.to_string();
    let mut source = error.source();
    while let Some(error) = source {
        let source_message = error.to_string();
        if !source_message.is_empty() && !message.contains(&source_message) {
            if !message.is_empty() {
                message.push_str(": ");
            }
            message.push_str(&source_message);
        }
        source = error.source();
    }
    message
}

pub(crate) fn request_too_large_text(message: &str) -> bool {
    let message = message.to_ascii_lowercase();
    message.contains("request_too_large")
        || message.contains("request too large")
        || message.contains("request exceeds the maximum size")
        || message.contains("exceeds the maximum size")
        || message.contains("context_length_exceeded")
        || message.contains("maximum context length")
        || message.contains("context window")
        || message.contains("prompt is too long")
        || message.contains("input is too long")
        || message.contains("code\":413")
        || message.contains("code\": 413")
        || message.contains("code:413")
        || message.contains("code: 413")
        || message.contains("status 413")
}

pub(crate) fn cyber_policy_text(message: &str) -> bool {
    let message = message.to_ascii_lowercase();
    message.contains("cyberpolicy")
        || message.contains("cyber_policy")
        || message.contains("cyber policy")
        || message.contains("possible cybersecurity risk")
        || message.contains("cybersecurity risk")
        || message.contains("trusted access for cyber")
        || message.contains("chatgpt.com/cyber")
}
