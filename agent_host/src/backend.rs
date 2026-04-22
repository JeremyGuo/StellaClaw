use agent_frame::compaction::ContextCompactionReport;
use agent_frame::config::AgentConfig as FrameAgentConfig;
use agent_frame::message::ChatMessage;
use agent_frame::{
    Tool, compact_session_messages_with_report as frame_compact_session_messages_with_report,
};
use anyhow::{Result, anyhow};
use serde::{Deserialize, Deserializer, Serialize};

#[derive(Clone, Copy, Debug, Default, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum AgentBackendKind {
    #[default]
    AgentFrame,
}

impl<'de> Deserialize<'de> for AgentBackendKind {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        match value.as_str() {
            "agent_frame" => Ok(Self::AgentFrame),
            other => Err(serde::de::Error::custom(format!(
                "unsupported agent backend '{}'",
                other
            ))),
        }
    }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct BackendExecutionOptions {}

pub fn backend_supports_native_multimodal_input(_kind: AgentBackendKind) -> bool {
    true
}

pub fn compact_session_messages_with_report(
    backend: AgentBackendKind,
    previous_messages: Vec<ChatMessage>,
    config: FrameAgentConfig,
    extra_tools: Vec<Tool>,
) -> Result<ContextCompactionReport> {
    match backend {
        AgentBackendKind::AgentFrame => {
            frame_compact_session_messages_with_report(previous_messages, config, extra_tools)
        }
    }
}

pub fn ensure_supported_backend(backend: AgentBackendKind) -> Result<()> {
    match backend {
        AgentBackendKind::AgentFrame => Ok(()),
    }
}

pub fn parse_agent_backend_value(value: &str) -> Option<AgentBackendKind> {
    match value.trim() {
        "agent_frame" => Some(AgentBackendKind::AgentFrame),
        _ => None,
    }
}

pub fn render_agent_backend_value(backend: AgentBackendKind) -> &'static str {
    match backend {
        AgentBackendKind::AgentFrame => "agent_frame",
    }
}

pub fn unsupported_backend_error(value: &str) -> anyhow::Error {
    anyhow!(
        "unsupported agent backend '{}'; this branch only supports agent_frame",
        value
    )
}

#[cfg(test)]
mod tests {
    use super::{AgentBackendKind, backend_supports_native_multimodal_input};

    #[test]
    fn only_agent_frame_backend_exists_and_supports_native_multimodal_input() {
        assert!(backend_supports_native_multimodal_input(
            AgentBackendKind::AgentFrame
        ));
    }

    #[test]
    fn unsupported_legacy_zgent_backend_is_rejected() {
        let error = serde_json::from_str::<AgentBackendKind>("\"zgent\"").unwrap_err();
        assert!(error.to_string().contains("unsupported agent backend"));
    }
}
