use crate::backend::{AgentBackendKind, BackendExecutionOptions};
use agent_frame::config::AgentConfig as FrameAgentConfig;
use agent_frame::{ChatMessage, SessionEvent, SessionRunReport};
use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RemoteToolDefinition {
    pub name: String,
    pub description: String,
    pub parameters: Value,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ChildInitPayload {
    pub backend: AgentBackendKind,
    pub previous_messages: Vec<ChatMessage>,
    pub prompt: String,
    pub config: FrameAgentConfig,
    pub extra_tools: Vec<RemoteToolDefinition>,
    #[serde(default)]
    pub execution_options: BackendExecutionOptions,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "type", content = "payload", rename_all = "snake_case")]
pub enum ParentToChildMessage {
    Init(ChildInitPayload),
    ToolResponse {
        request_id: String,
        ok: bool,
        result: Option<Value>,
        error: Option<String>,
    },
    SoftTimeout,
    Yield,
    Cancel,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "type", content = "payload", rename_all = "snake_case")]
pub enum ChildToParentMessage {
    Started,
    SessionEvent(SessionEvent),
    Checkpoint(SessionRunReport),
    StableReport(SessionRunReport),
    ToolRequest {
        request_id: String,
        tool_name: String,
        arguments: Value,
    },
    Completed(SessionRunReport),
    Failed {
        error: String,
    },
}
