use agent_frame::compaction::ContextCompactionReport;
use agent_frame::config::AgentConfig as FrameAgentConfig;
use agent_frame::message::ChatMessage;
use agent_frame::{TokenUsage, Tool};
use anyhow::Result;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ZgentCompactionRequest {
    pub prompt: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ZgentCompactionResponse {
    pub output: String,
}

pub trait ZgentCompactionAdapter {
    fn run_compaction(&self, request: &ZgentCompactionRequest) -> Result<ZgentCompactionResponse>;
}

pub fn compact_session_messages_with_report(
    previous_messages: Vec<ChatMessage>,
    config: &FrameAgentConfig,
    _extra_tools: &[Tool],
) -> Result<ContextCompactionReport> {
    Ok(ContextCompactionReport {
        messages: previous_messages,
        compacted_messages: Vec::new(),
        usage: TokenUsage::default(),
        compacted: false,
        token_limit: config
            .context_compaction
            .token_limit_override
            .unwrap_or_default(),
        estimated_tokens_before: 0,
        estimated_tokens_after: 0,
        structured_output: None,
    })
}
