use crossbeam_channel::Sender;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use thiserror::Error;

use crate::model_config::ModelConfig;

use super::{
    ChatMessage, ChatMessageItem, ChatRole, ToolCallItem, ToolConcurrency, ToolResultItem,
};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolBatch {
    pub batch_id: String,
    pub operations: Vec<ToolBatchOperation>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct SearchToolModels {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub web: Option<ModelConfig>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub image: Option<ModelConfig>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub video: Option<ModelConfig>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub news: Option<ModelConfig>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ProviderBackedToolModels {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub image: Option<ModelConfig>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pdf: Option<ModelConfig>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub audio: Option<ModelConfig>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub image_generation: Option<ModelConfig>,
}

impl ToolBatch {
    pub fn new(batch_id: impl Into<String>, operations: Vec<ToolBatchItem>) -> Self {
        Self {
            batch_id: batch_id.into(),
            operations: operations.into_iter().map(Into::into).collect(),
        }
    }

    pub fn new_scheduled(batch_id: impl Into<String>, operations: Vec<ToolBatchOperation>) -> Self {
        Self {
            batch_id: batch_id.into(),
            operations,
        }
    }

    pub fn is_empty(&self) -> bool {
        self.operations.is_empty()
    }

    pub fn progress_summary(&self) -> String {
        let labels = self
            .operations
            .iter()
            .map(ToolBatchOperation::progress_label)
            .collect::<Vec<_>>();
        let mut summary = labels
            .iter()
            .take(4)
            .cloned()
            .collect::<Vec<_>>()
            .join("; ");
        if labels.len() > 4 {
            summary.push_str(&format!("; +{} more", labels.len() - 4));
        }
        summary
    }

    pub fn into_result_message(self, tool_results: Vec<ToolResultItem>) -> ChatMessage {
        ChatMessage::new(
            ChatRole::Assistant,
            tool_results
                .into_iter()
                .map(ChatMessageItem::ToolResult)
                .collect(),
        )
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolBatchOperation {
    pub item: ToolBatchItem,
    pub concurrency: ToolConcurrency,
}

impl ToolBatchOperation {
    pub fn new(item: ToolBatchItem, concurrency: ToolConcurrency) -> Self {
        Self { item, concurrency }
    }

    pub fn progress_label(&self) -> String {
        self.item.progress_label()
    }
}

impl From<ToolBatchItem> for ToolBatchOperation {
    fn from(item: ToolBatchItem) -> Self {
        let concurrency = item.default_concurrency();
        Self { item, concurrency }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum ToolBatchItem {
    RegisteredTool(ToolCallItem),
    UnsupportedTool {
        tool_call: ToolCallItem,
        reason: String,
    },
}

impl ToolBatchItem {
    pub fn default_concurrency(&self) -> ToolConcurrency {
        match self {
            Self::RegisteredTool(tool_call) => match tool_call.tool_name.as_str() {
                "apply_patch"
                | "shell_make_visible"
                | "attachment_make_visible"
                | "shell_exec"
                | "shell_write_stdin"
                | "shell_stop"
                | "image_stop"
                | "pdf_stop"
                | "audio_stop"
                | "image_generation_stop" => ToolConcurrency::Serial,
                _ => ToolConcurrency::Parallel,
            },
            Self::UnsupportedTool { .. } => ToolConcurrency::Parallel,
        }
    }

    pub fn progress_label(&self) -> String {
        match self {
            Self::RegisteredTool(tool_call) | Self::UnsupportedTool { tool_call, .. } => {
                tool_call_progress_label(tool_call)
            }
        }
    }
}

fn tool_call_progress_label(tool_call: &ToolCallItem) -> String {
    let Some(hint) = tool_call_argument_hint(&tool_call.arguments.text) else {
        return tool_call.tool_name.clone();
    };
    format!("{} {}", tool_call.tool_name, hint)
}

fn tool_call_argument_hint(arguments: &str) -> Option<String> {
    let value = serde_json::from_str::<Value>(arguments).ok()?;
    let object = value.as_object()?;
    for key in [
        "path",
        "file_path",
        "query",
        "url",
        "command",
        "skill_name",
        "prompt",
    ] {
        if let Some(value) = object.get(key).and_then(Value::as_str) {
            let value = value.trim();
            if !value.is_empty() {
                return Some(truncate_progress_hint(value));
            }
        }
    }
    None
}

fn truncate_progress_hint(value: &str) -> String {
    const MAX_CHARS: usize = 80;
    let mut hint = String::new();
    for (index, ch) in value.chars().enumerate() {
        if index >= MAX_CHARS {
            hint.push_str("...");
            return hint;
        }
        hint.push(ch);
    }
    hint
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConversationBridgeRequest {
    pub request_id: String,
    pub tool_call_id: String,
    pub tool_name: String,
    pub action: String,
    pub payload: Value,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConversationBridgeResponse {
    pub request_id: String,
    pub tool_call_id: String,
    pub tool_name: String,
    pub result: ToolResultItem,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolBatchHandle {
    pub batch_id: String,
}

impl ToolBatchHandle {
    pub fn new(batch_id: impl Into<String>) -> Self {
        Self {
            batch_id: batch_id.into(),
        }
    }
}

pub trait ToolBatchExecutor {
    fn start(
        &self,
        batch: ToolBatch,
        completion_tx: Sender<ToolBatchCompletion>,
        progress_tx: Sender<ToolBatchProgress>,
    ) -> Result<ToolBatchHandle, ToolBatchError>;

    fn interrupt(&self, handle: &ToolBatchHandle) -> Result<(), ToolBatchError>;

    fn finish(&self, batch_id: &str) -> Result<(), ToolBatchError>;
}

#[derive(Debug)]
pub struct ToolBatchCompletion {
    pub batch_id: String,
    pub result: Result<ChatMessage, String>,
}

#[derive(Debug, Clone)]
pub struct ToolBatchProgress {
    pub batch_id: String,
    pub result: ToolResultItem,
}

pub trait ConversationBridge {
    fn call(
        &self,
        request: ConversationBridgeRequest,
    ) -> Result<ConversationBridgeResponse, ToolBatchError>;
}

#[derive(Debug, Error)]
pub enum ToolBatchError {
    #[error("tool batch {0} is empty")]
    EmptyBatch(String),
    #[error("tool batch start failed: {0}")]
    Start(String),
    #[error("tool batch interrupt failed: {0}")]
    Interrupt(String),
    #[error("tool batch finish failed: {0}")]
    Finish(String),
    #[error("conversation bridge failed: {0}")]
    Bridge(String),
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session_actor::{ContextItem, ToolResultContent};

    #[test]
    fn builds_result_message_from_tool_results() {
        let batch = ToolBatch::new(
            "batch_1",
            vec![ToolBatchItem::RegisteredTool(ToolCallItem {
                tool_call_id: "call_1".to_string(),
                tool_name: "apply_patch".to_string(),
                arguments: ContextItem {
                    text: "{}".to_string(),
                },
            })],
        );

        let message = batch.into_result_message(vec![ToolResultItem {
            tool_call_id: "call_1".to_string(),
            tool_name: "apply_patch".to_string(),
            result: ToolResultContent::from_text("file loaded".to_string()),
        }]);

        assert_eq!(message.role, ChatRole::Assistant);
        assert_eq!(message.data.len(), 1);
        assert!(matches!(message.data[0], ChatMessageItem::ToolResult(_)));
    }

    #[test]
    fn reports_empty_batch() {
        let batch = ToolBatch::new("batch_2", Vec::new());

        assert!(batch.is_empty());
    }

    #[test]
    fn supports_registered_service_tools_inside_batch() {
        let batch = ToolBatch::new(
            "batch_3",
            vec![ToolBatchItem::RegisteredTool(ToolCallItem {
                tool_call_id: "call_2".to_string(),
                tool_name: "cron_tasks_list".to_string(),
                arguments: ContextItem {
                    text: "{}".to_string(),
                },
            })],
        );

        assert_eq!(batch.operations.len(), 1);
        assert!(matches!(
            &batch.operations[0].item,
            ToolBatchItem::RegisteredTool(_)
        ));
    }
}
