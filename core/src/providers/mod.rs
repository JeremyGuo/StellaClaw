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

use std::{error::Error as StdError, time::Duration};

#[cfg(not(test))]
use rand::Rng;
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::{
    model_config::{ModelConfig, ProviderType, RetryMode},
    session_actor::{ChatMessage, ToolDefinition},
};

pub use brave_search::BraveSearchProvider;
pub use brave_search_image::BraveSearchImageProvider;
pub use brave_search_news::BraveSearchNewsProvider;
pub use brave_search_video::BraveSearchVideoProvider;
pub use claude_code::ClaudeCodeProvider;
pub use codex_subscription::CodexSubscriptionProvider;
pub use error_report::ProviderErrorReport;
pub use forkserver::{
    global_provider_fork_server, init_global_provider_fork_server, ForkServerProvider,
    ProviderRequestAbortHandle, ProviderRequestForkServer, ProviderRequestHandle,
    ProviderRequestOwned,
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

pub trait Provider {
    fn model_config(&self) -> &ModelConfig;

    fn normalize_messages_for_provider(&self, messages: &[ChatMessage]) -> Vec<ChatMessage> {
        messages.to_vec()
    }

    fn filter_tools_for_provider<'a>(
        &self,
        tools: Vec<&'a ToolDefinition>,
    ) -> Vec<&'a ToolDefinition> {
        tools
    }

    fn send(&self, request: ProviderRequest<'_>) -> Result<ChatMessage, ProviderError>;

    fn before_retry(&self, _error: &ProviderError) {}
}

pub(crate) trait ProviderBackend: Send + Sync {
    fn normalize_messages_for_provider(
        &self,
        _model_config: &ModelConfig,
        messages: &[ChatMessage],
    ) -> Vec<ChatMessage> {
        messages.to_vec()
    }

    fn filter_tools_for_provider<'a>(
        &self,
        _model_config: &ModelConfig,
        tools: Vec<&'a ToolDefinition>,
    ) -> Vec<&'a ToolDefinition> {
        tools
    }

    fn send(
        &self,
        model_config: &ModelConfig,
        request: ProviderRequest<'_>,
    ) -> Result<ChatMessage, ProviderError>;

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

    fn normalize_messages_for_provider(&self, messages: &[ChatMessage]) -> Vec<ChatMessage> {
        self.backend
            .normalize_messages_for_provider(&self.model_config, messages)
    }

    fn filter_tools_for_provider<'a>(
        &self,
        tools: Vec<&'a ToolDefinition>,
    ) -> Vec<&'a ToolDefinition> {
        self.backend
            .filter_tools_for_provider(&self.model_config, tools)
    }

    fn send(&self, request: ProviderRequest<'_>) -> Result<ChatMessage, ProviderError> {
        self.backend.send(&self.model_config, request)
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
    mut on_retry: F,
) -> Result<ChatMessage, ProviderError>
where
    F: FnMut(ProviderRetryEvent<'_>),
{
    let mut retries_used = 0_u64;
    loop {
        match provider.send(request.clone()) {
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
