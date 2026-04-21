pub(super) mod claude_code;
pub(super) mod codex_subscription;
pub(super) mod openrouter;
pub(super) mod openrouter_responses;

use super::{ChatCompletionOutcome, ChatCompletionSession};
use crate::config::{UpstreamApiKind, UpstreamAuthKind, UpstreamConfig};
use crate::message::ChatMessage;
use crate::tooling::Tool;
use anyhow::Result;
use serde_json::{Map, Value};

pub(super) trait UpstreamProvider {
    fn start_session(&self, _upstream: &UpstreamConfig) -> Result<Option<ChatCompletionSession>> {
        Ok(None)
    }

    fn create_completion(
        &self,
        upstream: &UpstreamConfig,
        messages: &[ChatMessage],
        tools: &[Tool],
        extra_payload: Option<Map<String, Value>>,
        session: Option<&mut ChatCompletionSession>,
    ) -> Result<ChatCompletionOutcome>;
}

static CODEX_SUBSCRIPTION_PROVIDER: codex_subscription::CodexSubscriptionProvider =
    codex_subscription::CodexSubscriptionProvider;
static CLAUDE_CODE_PROVIDER: claude_code::ClaudeCodeProvider = claude_code::ClaudeCodeProvider;
static OPENROUTER_PROVIDER: openrouter::OpenRouterProvider = openrouter::OpenRouterProvider;
static OPENROUTER_RESPONSES_PROVIDER: openrouter_responses::OpenRouterResponsesProvider =
    openrouter_responses::OpenRouterResponsesProvider;

pub(super) fn provider_for(upstream: &UpstreamConfig) -> &'static dyn UpstreamProvider {
    match (upstream.auth_kind, upstream.api_kind) {
        (UpstreamAuthKind::CodexSubscription, _) => &CODEX_SUBSCRIPTION_PROVIDER,
        (UpstreamAuthKind::ApiKey, UpstreamApiKind::ClaudeMessages) => &CLAUDE_CODE_PROVIDER,
        (UpstreamAuthKind::ApiKey, UpstreamApiKind::Responses) => &OPENROUTER_RESPONSES_PROVIDER,
        (UpstreamAuthKind::ApiKey, UpstreamApiKind::ChatCompletions) => &OPENROUTER_PROVIDER,
    }
}
