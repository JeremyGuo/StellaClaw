use serde::{Deserialize, Deserializer, Serialize};
use serde_json::{json, Value};

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
    SelectionReference(SelectionReferenceItem),
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
pub struct SelectionReferenceItem {
    pub file_path: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub file_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub media_type: Option<String>,
    pub source_kind: String,
    pub selected_text: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub locator: Option<SelectionLocator>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context: Option<SelectionContext>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub original_text_length: Option<u32>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SelectionLocator {
    pub kind: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub start_line: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub end_line: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub start_column: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub end_column: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub page: Option<u32>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub rects: Vec<SelectionRect>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub heading: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub selector: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub xpath: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub block_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub block_index: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text_offset: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text_length: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub anchor_text: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SelectionRect {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub page: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub x: Option<i32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub y: Option<i32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub width: Option<i32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub height: Option<i32>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SelectionContext {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub before: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub after: Option<String>,
}

impl SelectionReferenceItem {
    pub fn to_prompt_text(&self) -> String {
        let mut lines = Vec::new();
        lines.push("[Selected Content Reference]".to_string());
        lines.push(format!("File: {}", self.file_path));
        if let Some(name) = self.file_name.as_deref().filter(|value| !value.is_empty()) {
            lines.push(format!("Name: {name}"));
        }
        lines.push(format!("Type: {}", self.source_kind));
        if let Some(media_type) = self.media_type.as_deref().filter(|value| !value.is_empty()) {
            lines.push(format!("Media type: {media_type}"));
        }
        if let Some(locator) = &self.locator {
            let mut parts = vec![format!("kind={}", locator.kind)];
            if let (Some(start), Some(end)) = (locator.start_line, locator.end_line) {
                parts.push(format!("lines={start}-{end}"));
            }
            if let Some(page) = locator.page {
                parts.push(format!("page={page}"));
            }
            if let Some(heading) = locator.heading.as_deref().filter(|value| !value.is_empty()) {
                parts.push(format!("heading={heading}"));
            }
            if let Some(selector) = locator
                .selector
                .as_deref()
                .filter(|value| !value.is_empty())
            {
                parts.push(format!("selector={selector}"));
            }
            if let Some(block_id) = locator
                .block_id
                .as_deref()
                .filter(|value| !value.is_empty())
            {
                parts.push(format!("block_id={block_id}"));
            }
            if let Some(offset) = locator.text_offset {
                parts.push(format!("text_offset={offset}"));
            }
            if let Some(length) = locator.text_length {
                parts.push(format!("text_length={length}"));
            }
            if let Some(anchor) = locator
                .anchor_text
                .as_deref()
                .filter(|value| !value.is_empty())
            {
                parts.push(format!("anchor={anchor}"));
            }
            lines.push(format!("Locator: {}", parts.join("; ")));
        }
        if let Some(original_len) = self.original_text_length {
            lines.push(format!(
                "Original selected text length: {original_len} chars"
            ));
        }
        if let Some(context) = &self.context {
            if let Some(before) = context.before.as_deref().filter(|value| !value.is_empty()) {
                lines.push("Context before:".to_string());
                lines.push(before.to_string());
            }
        }
        lines.push("Selected text:".to_string());
        lines.push(self.selected_text.clone());
        if let Some(context) = &self.context {
            if let Some(after) = context.after.as_deref().filter(|value| !value.is_empty()) {
                lines.push("Context after:".to_string());
                lines.push(after.to_string());
            }
        }
        lines.push("[/Selected Content Reference]".to_string());
        lines.join("\n")
    }
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

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ToolResultContent {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub structured: Option<Value>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub files: Vec<FileItem>,
}

impl<'de> Deserialize<'de> for ToolResultContent {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        #[derive(Deserialize)]
        struct RawToolResultContent {
            #[serde(default)]
            context: Option<ContextItem>,
            #[serde(default)]
            structured: Option<Value>,
            #[serde(default)]
            file: Option<FileItem>,
            #[serde(default)]
            files: Vec<FileItem>,
        }

        let raw = RawToolResultContent::deserialize(deserializer)?;
        let mut structured = raw.structured;
        if structured.is_none() {
            if let Some(context) = raw.context {
                structured = Some(text_tool_result_value(context.text));
            }
        }

        let mut files = raw.files;
        if let Some(file) = raw.file {
            files.push(file);
        }

        Ok(Self { structured, files })
    }
}

impl ToolResultContent {
    pub fn from_tool_value(value: Value) -> Self {
        Self {
            structured: Some(structured_tool_value(value)),
            files: Vec::new(),
        }
    }

    pub fn from_text(text: impl Into<String>) -> Self {
        Self {
            structured: Some(text_tool_result_value(text)),
            files: Vec::new(),
        }
    }

    pub fn from_json(value: Value) -> Self {
        Self {
            structured: Some(json_tool_result_value(value)),
            files: Vec::new(),
        }
    }

    pub fn with_file(mut self, file: FileItem) -> Self {
        self.files.push(file);
        self
    }

    pub fn with_files(mut self, files: impl IntoIterator<Item = FileItem>) -> Self {
        self.files.extend(files);
        self
    }

    pub fn normalize_legacy_context(&mut self) {}
}

pub fn structured_tool_value(value: Value) -> Value {
    if value.get("kind").and_then(Value::as_str).is_some() {
        return value;
    }
    match value {
        Value::String(text) => text_tool_result_value(text),
        other => json_tool_result_value(other),
    }
}

fn text_tool_result_value(text: impl Into<String>) -> Value {
    json!({
        "kind": "text_result",
        "text": text.into(),
    })
}

fn json_tool_result_value(value: Value) -> Value {
    json!({
        "kind": "json_result",
        "value": value,
    })
}

pub fn tool_result_text(tool_result: &ToolResultItem) -> String {
    let mut parts = Vec::new();
    if let Some(text) = structured_tool_result_text(tool_result) {
        if !text.trim().is_empty() {
            parts.push(text);
        }
    }
    for file in &tool_result.result.files {
        parts.push(file.uri.clone());
    }
    parts.join("\n")
}

pub fn tool_result_structured_text(tool_result: &ToolResultItem) -> String {
    structured_tool_result_text(tool_result).unwrap_or_default()
}

fn structured_tool_result_text(tool_result: &ToolResultItem) -> Option<String> {
    let value = tool_result.result.structured.as_ref()?;
    if value.get("kind").and_then(Value::as_str) == Some("shell_result") {
        return Some(shell_structured_result_text(value));
    }
    match value.get("kind").and_then(Value::as_str) {
        Some("text_result") => value
            .get("text")
            .and_then(Value::as_str)
            .map(str::to_string),
        Some("json_result") => value
            .get("value")
            .and_then(|value| serde_json::to_string_pretty(value).ok()),
        _ => serde_json::to_string_pretty(value).ok(),
    }
}

fn shell_structured_result_text(value: &Value) -> String {
    let running = value
        .get("running")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let timed_out = value
        .get("timed_out")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let wall_time_seconds = value
        .get("wall_time_seconds")
        .and_then(Value::as_f64)
        .or_else(|| {
            value
                .get("duration_ms")
                .and_then(Value::as_u64)
                .map(|millis| millis as f64 / 1000.0)
        })
        .unwrap_or_default();
    let exit_code = value.get("exit_code").and_then(Value::as_i64);
    let mut parts = Vec::new();
    if running {
        parts.push(format!(
            "Process running with session ID {}",
            value
                .get("session_id")
                .or_else(|| value.get("process_id"))
                .and_then(Value::as_str)
                .unwrap_or_default()
        ));
    } else if timed_out {
        parts.push("Process timed out".to_string());
    } else if let Some(code) = exit_code {
        parts.push(format!("Process exited with code {code}"));
    } else {
        parts.push("Process exited".to_string());
    }
    parts.push(format!("Wall time: {wall_time_seconds:.4} seconds"));
    if let Some(original_token_count) = value
        .get("output")
        .and_then(|output| output.get("original_token_count"))
        .and_then(Value::as_u64)
    {
        parts.push(format!("Original token count: {original_token_count}"));
    }

    let tty = value.get("tty").and_then(Value::as_bool).unwrap_or(false);
    if tty {
        push_shell_stream(&mut parts, "Output", value.get("output"));
    } else {
        push_shell_stream(&mut parts, "Stdout", value.get("stdout"));
        push_shell_stream(&mut parts, "Stderr", value.get("stderr"));
        if !has_stream_text(value.get("stdout")) && !has_stream_text(value.get("stderr")) {
            push_shell_stream(&mut parts, "Output", value.get("output"));
        }
    }
    if let Some(snapshot) = value.get("terminal_snapshot").and_then(Value::as_object) {
        let visible_text = snapshot
            .get("visible_text")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let alternate_screen = snapshot
            .get("alternate_screen")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let saw_alternate_screen = snapshot
            .get("saw_alternate_screen")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let truncated = snapshot
            .get("truncated")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        parts.push(format!(
            "Terminal snapshot: alternate_screen={alternate_screen}, saw_alternate_screen={saw_alternate_screen}, truncated={truncated}\n{visible_text}"
        ));
    }
    parts.join("\n")
}

fn push_shell_stream(parts: &mut Vec<String>, label: &str, value: Option<&Value>) {
    let Some(value) = value else {
        return;
    };
    let text = value
        .get("text")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let truncated = value
        .get("truncated")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    if text.is_empty() && !truncated {
        return;
    }
    let mut header = label.to_string();
    if truncated {
        header.push_str(" (truncated)");
    }
    if text.is_empty() {
        parts.push(format!("{header}:\n<empty>"));
    } else {
        parts.push(format!("{header}:\n{text}"));
    }
}

fn has_stream_text(value: Option<&Value>) -> bool {
    value
        .and_then(|value| value.get("text"))
        .and_then(Value::as_str)
        .is_some_and(|text| !text.is_empty())
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
                    result: ToolResultContent::from_text("file loaded".to_string()).with_file(
                        FileItem {
                            uri: "file:///tmp/demo.png".to_string(),
                            name: Some("demo.png".to_string()),
                            media_type: Some("image/png".to_string()),
                            width: Some(320),
                            height: Some(240),
                            state: None,
                        },
                    ),
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
            json["data"][1]["payload"]["result"]["files"][0]["media_type"],
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

    #[test]
    fn selection_reference_renders_prompt_block() {
        let selection = SelectionReferenceItem {
            file_path: "src/lib.rs".to_string(),
            file_name: Some("lib.rs".to_string()),
            media_type: Some("text/x-rust".to_string()),
            source_kind: "code".to_string(),
            selected_text: "fn selected() {}".to_string(),
            locator: Some(SelectionLocator {
                kind: "line_range".to_string(),
                start_line: Some(12),
                end_line: Some(12),
                start_column: Some(1),
                end_column: Some(17),
                page: None,
                rects: Vec::new(),
                heading: None,
                selector: None,
                xpath: None,
                block_id: None,
                block_index: None,
                text_offset: Some(128),
                text_length: Some(16),
                anchor_text: None,
            }),
            context: Some(SelectionContext {
                before: Some("mod tests;".to_string()),
                after: Some("fn next() {}".to_string()),
            }),
            original_text_length: Some(16),
        };

        let text = selection.to_prompt_text();

        assert!(text.contains("[Selected Content Reference]"));
        assert!(text.contains("File: src/lib.rs"));
        assert!(text.contains("Locator: kind=line_range; lines=12-12"));
        assert!(text.contains("Selected text:\nfn selected() {}"));
    }
}
