use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChatRole {
    User,
    Assistant,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct TokenUsage {
    pub cache_read: u64,
    pub cache_write: u64,
    pub uncache_input: u64,
    pub output: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cost_usd: Option<TokenUsageCost>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct TokenUsageCost {
    pub cache_read: f64,
    pub cache_write: f64,
    pub uncache_input: f64,
    pub output: f64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ChatMessage {
    pub role: ChatRole,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message_time: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub token_usage: Option<TokenUsage>,
    pub data: Vec<ChatMessageItem>,
}

impl ChatMessage {
    pub fn new(role: ChatRole, data: Vec<ChatMessageItem>) -> Self {
        Self {
            role,
            user_name: None,
            message_time: None,
            token_usage: None,
            data,
        }
    }

    pub fn with_user_name(mut self, user_name: impl Into<String>) -> Self {
        self.user_name = Some(user_name.into());
        self
    }

    pub fn with_user_name_option(mut self, user_name: Option<String>) -> Self {
        self.user_name = user_name;
        self
    }

    pub fn with_message_time(mut self, message_time: impl Into<String>) -> Self {
        self.message_time = Some(message_time.into());
        self
    }

    pub fn with_message_time_option(mut self, message_time: Option<String>) -> Self {
        self.message_time = message_time;
        self
    }

    pub fn with_token_usage(mut self, token_usage: TokenUsage) -> Self {
        self.token_usage = Some(token_usage);
        self
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", content = "payload", rename_all = "snake_case")]
pub enum ChatMessageItem {
    Reasoning(ReasoningItem),
    Context(ContextItem),
    File(FileItem),
    ToolCall(ToolCallItem),
    ToolResult(ToolResultItem),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReasoningItem {
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub text: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub codex_summary: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub codex_encrypted_content: Option<String>,
}

impl ReasoningItem {
    pub fn from_text(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            codex_summary: None,
            codex_encrypted_content: None,
        }
    }

    pub fn codex(
        codex_summary: Option<String>,
        codex_encrypted_content: Option<String>,
        fallback_text: Option<String>,
    ) -> Self {
        let text = fallback_text.unwrap_or_default();
        Self {
            text,
            codex_summary,
            codex_encrypted_content,
        }
    }

    pub fn has_codex_encrypted_content(&self) -> bool {
        self.codex_encrypted_content
            .as_deref()
            .is_some_and(|content| !content.is_empty())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContextItem {
    pub text: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileItem {
    pub uri: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub media_type: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub width: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub height: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub state: Option<FileState>,
}

impl FileItem {
    pub fn image_dimensions(&self) -> Option<(u32, u32)> {
        match (self.width, self.height) {
            (Some(width), Some(height)) => Some((width, height)),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum FileState {
    Crashed { reason: String },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolCallItem {
    pub tool_call_id: String,
    pub tool_name: String,
    pub arguments: ContextItem,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolResultItem {
    pub tool_call_id: String,
    pub tool_name: String,
    pub result: ToolResultContent,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolResultContent {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub context: Option<ContextItem>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub file: Option<FileItem>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn serializes_assistant_message_with_usage_and_tool_result() {
        let message = ChatMessage::new(
            ChatRole::Assistant,
            vec![
                ChatMessageItem::Reasoning(ReasoningItem::from_text("need to inspect workspace")),
                ChatMessageItem::ToolResult(ToolResultItem {
                    tool_call_id: "call_1".to_string(),
                    tool_name: "read_file".to_string(),
                    result: ToolResultContent {
                        context: Some(ContextItem {
                            text: "file loaded".to_string(),
                        }),
                        file: Some(FileItem {
                            uri: "file:///tmp/demo.png".to_string(),
                            name: Some("demo.png".to_string()),
                            media_type: Some("image/png".to_string()),
                            width: Some(320),
                            height: Some(240),
                            state: None,
                        }),
                    },
                }),
            ],
        )
        .with_token_usage(TokenUsage {
            cache_read: 10,
            cache_write: 2,
            uncache_input: 20,
            output: 30,
            cost_usd: Some(TokenUsageCost {
                cache_read: 0.001,
                cache_write: 0.002,
                uncache_input: 0.003,
                output: 0.004,
            }),
        });

        let json = serde_json::to_value(&message).expect("message should serialize");

        assert_eq!(json["role"], "assistant");
        assert!(json.get("user_name").is_none());
        assert!(json.get("message_time").is_none());
        assert_eq!(json["token_usage"]["cache_read"], 10);
        assert_eq!(json["token_usage"]["cost_usd"]["output"], 0.004);
        assert_eq!(json["data"][1]["type"], "tool_result");
        assert_eq!(
            json["data"][1]["payload"]["result"]["file"]["media_type"],
            "image/png"
        );
    }

    #[test]
    fn deserializes_user_message_without_usage() {
        let raw = r#"
        {
          "role": "user",
          "data": [
            {
              "type": "context",
              "payload": {
                "text": "hello"
              }
            }
          ]
        }
        "#;

        let message: ChatMessage = serde_json::from_str(raw).expect("message should deserialize");

        assert_eq!(message.role, ChatRole::User);
        assert!(message.user_name.is_none());
        assert!(message.message_time.is_none());
        assert!(message.token_usage.is_none());
        assert_eq!(
            message.data,
            vec![ChatMessageItem::Context(ContextItem {
                text: "hello".to_string(),
            })]
        );
    }

    #[test]
    fn serializes_user_message_metadata() {
        let message = ChatMessage::new(
            ChatRole::User,
            vec![ChatMessageItem::Context(ContextItem {
                text: "hello".to_string(),
            })],
        )
        .with_user_name("alice")
        .with_message_time("2026-04-23T10:20:30Z");

        let json = serde_json::to_value(&message).expect("message should serialize");

        assert_eq!(json["user_name"], "alice");
        assert_eq!(json["message_time"], "2026-04-23T10:20:30Z");
    }
}
