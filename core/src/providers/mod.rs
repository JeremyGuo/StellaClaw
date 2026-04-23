mod brave_search;
mod claude_code;
mod codex_subscription;
mod common;
mod forkserver;
mod openrouter_completion;
mod openrouter_responses;
mod output_persistor;

use thiserror::Error;

use crate::{
    model_config::{ModelConfig, ProviderType},
    session_actor::{ChatMessage, ToolDefinition},
};

pub use brave_search::BraveSearchProvider;
pub use claude_code::ClaudeCodeProvider;
pub use codex_subscription::CodexSubscriptionProvider;
pub use forkserver::{
    global_provider_fork_server, init_global_provider_fork_server, ForkServerProvider,
    ProviderRequestAbortHandle, ProviderRequestForkServer, ProviderRequestHandle,
    ProviderRequestOwned,
};
pub use openrouter_completion::OpenRouterCompletionProvider;
pub use openrouter_responses::OpenRouterResponsesProvider;
pub use output_persistor::{OutputPersistor, OutputPersistorError};

pub fn provider_from_model_config(model_config: &ModelConfig) -> Box<dyn Provider + Send + Sync> {
    match model_config.provider_type {
        ProviderType::OpenRouterCompletion => Box::new(OpenRouterCompletionProvider::new()),
        ProviderType::OpenRouterResponses => Box::new(OpenRouterResponsesProvider::new()),
        ProviderType::ClaudeCode => Box::new(ClaudeCodeProvider::new()),
        ProviderType::CodexSubscription => Box::new(CodexSubscriptionProvider::new()),
        ProviderType::BraveSearch => Box::new(BraveSearchProvider::new()),
    }
}

pub trait Provider {
    fn send(
        &self,
        model_config: &ModelConfig,
        request: ProviderRequest<'_>,
    ) -> Result<ChatMessage, ProviderError>;
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
    Request(reqwest::Error),
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
    #[error("websocket provider request failed: {0}")]
    WebSocket(String),
    #[error("failed to persist provider output: {0}")]
    PersistOutput(#[from] OutputPersistorError),
    #[error("provider response did not include any completion choices")]
    EmptyChoices,
    #[error("provider request isolation failed: {0}")]
    Subprocess(String),
}
