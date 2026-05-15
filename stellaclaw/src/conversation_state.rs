use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use stellaclaw_core::session_actor::{ChatMessage, ToolRemoteMode};

use crate::config::{SandboxConfig, SessionProfile};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConversationState {
    pub version: u32,
    pub conversation_id: String,
    #[serde(default)]
    pub nickname: String,
    pub channel_id: String,
    pub platform_chat_id: String,
    pub session_profile: SessionProfile,
    #[serde(default)]
    pub model_selection_pending: bool,
    #[serde(default)]
    pub tool_remote_mode: ToolRemoteMode,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sandbox: Option<SandboxConfig>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_effort: Option<String>,
    pub session_binding: ConversationSessionBinding,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConversationSessionBinding {
    pub foreground_session_id: String,
    #[serde(default = "default_index")]
    pub next_background_index: u64,
    #[serde(default = "default_index")]
    pub next_subagent_index: u64,
    #[serde(default)]
    pub background_sessions: BTreeMap<String, ManagedSessionRecord>,
    #[serde(default)]
    pub subagent_sessions: BTreeMap<String, ManagedSessionRecord>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ManagedSessionRecord {
    pub agent_id: String,
    pub session_id: String,
    pub session_type: ManagedSessionType,
    pub status: ManagedSessionStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_message: Option<ChatMessage>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_error: Option<String>,
    #[serde(default)]
    pub suppress_output: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model_override: Option<String>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ManagedSessionType {
    Background,
    Subagent,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ManagedSessionStatus {
    Running,
    Completed,
    Failed,
    Killed,
}

pub(crate) fn default_index() -> u64 {
    1
}
