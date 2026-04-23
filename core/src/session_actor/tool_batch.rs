use crossbeam_channel::Sender;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use thiserror::Error;

use crate::model_config::ModelConfig;

use super::{
    ChatMessage, ChatMessageItem, ChatRole, ProviderBackedToolKind, SessionSkillObservation,
    ToolCallItem, ToolResultItem,
};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolBatch {
    pub batch_id: String,
    pub operations: Vec<ToolExecutionOp>,
}

impl ToolBatch {
    pub fn new(batch_id: impl Into<String>, operations: Vec<ToolExecutionOp>) -> Self {
        Self {
            batch_id: batch_id.into(),
            operations,
        }
    }

    pub fn is_empty(&self) -> bool {
        self.operations.is_empty()
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
pub enum ToolExecutionOp {
    LocalTool(ToolCallItem),
    SkillLoad {
        tool_call: ToolCallItem,
        skill: SessionSkillObservation,
    },
    ProviderBacked {
        tool_call: ToolCallItem,
        kind: ProviderBackedToolKind,
        model_config: ModelConfig,
    },
    WebSearch {
        tool_call: ToolCallItem,
        model_config: ModelConfig,
    },
    ConversationBridge(ConversationBridgeRequest),
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
    ) -> Result<ToolBatchHandle, ToolBatchError>;

    fn interrupt(&self, handle: &ToolBatchHandle) -> Result<(), ToolBatchError>;

    fn finish(&self, batch_id: &str) -> Result<(), ToolBatchError>;
}

#[derive(Debug)]
pub struct ToolBatchCompletion {
    pub batch_id: String,
    pub result: Result<ChatMessage, String>,
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
            vec![ToolExecutionOp::LocalTool(ToolCallItem {
                tool_call_id: "call_1".to_string(),
                tool_name: "read_file".to_string(),
                arguments: ContextItem {
                    text: "{\"path\":\"README.md\"}".to_string(),
                },
            })],
        );

        let message = batch.into_result_message(vec![ToolResultItem {
            tool_call_id: "call_1".to_string(),
            tool_name: "read_file".to_string(),
            result: ToolResultContent {
                context: Some(ContextItem {
                    text: "file loaded".to_string(),
                }),
                file: None,
            },
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
    fn supports_conversation_bridge_operations_inside_batch() {
        let batch = ToolBatch::new(
            "batch_3",
            vec![ToolExecutionOp::ConversationBridge(
                ConversationBridgeRequest {
                    request_id: "req_1".to_string(),
                    tool_call_id: "call_2".to_string(),
                    tool_name: "snapshot_save".to_string(),
                    action: "snapshot_save".to_string(),
                    payload: serde_json::json!({
                        "name": "before-edit"
                    }),
                },
            )],
        );

        assert_eq!(batch.operations.len(), 1);
        assert!(matches!(
            &batch.operations[0],
            ToolExecutionOp::ConversationBridge(_)
        ));
    }
}
