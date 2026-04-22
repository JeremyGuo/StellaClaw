use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

pub const TOOL_RESULT_BLOCK_TYPE: &str = "tool_result";
pub const CONTEXT_BLOCK_TYPE: &str = "context";

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct FunctionCall {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub arguments: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct ToolCall {
    pub id: String,
    #[serde(rename = "type")]
    pub kind: String,
    pub function: FunctionCall,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct ToolResultBlock {
    pub tool_call_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content: Option<Value>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct ContextBlock {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub payload: Option<Value>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct ChatMessage {
    pub role: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_calls: Option<Vec<ToolCall>>,
}

impl ChatMessage {
    pub fn text(role: impl Into<String>, text: impl Into<String>) -> Self {
        Self {
            role: role.into(),
            content: Some(Value::String(text.into())),
            reasoning: None,
            name: None,
            tool_call_id: None,
            tool_calls: None,
        }
    }

    pub fn tool_output(
        tool_call_id: impl Into<String>,
        name: impl Into<String>,
        content: impl Into<String>,
    ) -> Self {
        Self {
            role: "tool".to_string(),
            content: Some(Value::String(content.into())),
            reasoning: None,
            name: Some(name.into()),
            tool_call_id: Some(tool_call_id.into()),
            tool_calls: None,
        }
    }
}

pub fn tool_result_content_block(
    tool_call_id: impl Into<String>,
    name: impl Into<String>,
    content: Value,
) -> Value {
    json!({
        "type": TOOL_RESULT_BLOCK_TYPE,
        "tool_call_id": tool_call_id.into(),
        "name": name.into(),
        "content": content,
    })
}

pub fn context_content_block(
    name: Option<&str>,
    text: Option<&str>,
    payload: Option<Value>,
) -> Value {
    let mut object = serde_json::Map::from_iter([(
        "type".to_string(),
        Value::String(CONTEXT_BLOCK_TYPE.to_string()),
    )]);
    if let Some(name) = name {
        object.insert("name".to_string(), Value::String(name.to_string()));
    }
    if let Some(text) = text {
        object.insert("text".to_string(), Value::String(text.to_string()));
    }
    if let Some(payload) = payload {
        object.insert("payload".to_string(), payload);
    }
    Value::Object(object)
}

pub fn parse_tool_result_block(item: &Value) -> Option<ToolResultBlock> {
    (item.get("type").and_then(Value::as_str) == Some(TOOL_RESULT_BLOCK_TYPE))
        .then(|| serde_json::from_value(item.clone()).ok())
        .flatten()
}

pub fn collect_tool_result_blocks(content: Option<&Value>) -> Vec<ToolResultBlock> {
    match content {
        Some(Value::Array(items)) => items.iter().filter_map(parse_tool_result_block).collect(),
        _ => Vec::new(),
    }
}

pub fn content_without_tool_result_blocks(content: Option<&Value>) -> Option<Value> {
    match content {
        None => None,
        Some(Value::Array(items)) => {
            let filtered = items
                .iter()
                .filter(|item| parse_tool_result_block(item).is_none())
                .cloned()
                .collect::<Vec<_>>();
            if filtered.is_empty() {
                None
            } else {
                Some(Value::Array(filtered))
            }
        }
        Some(other) => Some(other.clone()),
    }
}

pub fn content_item_text(item: &Value) -> Option<String> {
    let Some(object) = item.as_object() else {
        return value_text(item);
    };
    let item_type = object.get("type").and_then(Value::as_str)?;
    match item_type {
        "text" | "input_text" | "output_text" => object
            .get("text")
            .and_then(Value::as_str)
            .map(ToOwned::to_owned),
        TOOL_RESULT_BLOCK_TYPE => {
            let result = parse_tool_result_block(item)?;
            let label = result
                .name
                .as_deref()
                .filter(|name| !name.trim().is_empty())
                .unwrap_or("tool");
            let body = result
                .content
                .as_ref()
                .and_then(value_text)
                .unwrap_or_default();
            if body.trim().is_empty() {
                Some(format!("[tool result: {label} id={}]", result.tool_call_id))
            } else {
                Some(format!(
                    "[tool result: {label} id={}]\n{}",
                    result.tool_call_id, body
                ))
            }
        }
        CONTEXT_BLOCK_TYPE => {
            let context: ContextBlock = serde_json::from_value(item.clone()).ok()?;
            let mut parts = Vec::new();
            let label = context
                .name
                .as_deref()
                .filter(|name| !name.trim().is_empty())
                .map(|name| format!("[context: {name}]"))
                .unwrap_or_else(|| "[context]".to_string());
            parts.push(label);
            if let Some(text) = context.text.as_deref()
                && !text.trim().is_empty()
            {
                parts.push(text.to_string());
            }
            if let Some(payload) = context.payload.as_ref()
                && let Some(payload_text) = value_text(payload)
                && !payload_text.trim().is_empty()
            {
                parts.push(payload_text);
            }
            Some(parts.join("\n"))
        }
        _ => None,
    }
}

pub fn content_has_nonempty_visible_parts(content: Option<&Value>) -> bool {
    match content {
        None | Some(Value::Null) => false,
        Some(Value::String(text)) => !text.trim().is_empty(),
        Some(Value::Array(items)) => items.iter().any(|item| match item {
            Value::String(text) => !text.trim().is_empty(),
            Value::Object(_) => {
                content_item_text(item).is_some_and(|text| !text.trim().is_empty())
                    || !matches!(item.get("type").and_then(Value::as_str), Some("text"))
                    || item.as_object().is_some_and(|object| {
                        !matches!(
                            object.get("type").and_then(Value::as_str),
                            Some("text" | "input_text" | "output_text")
                        )
                    })
            }
            Value::Null => false,
            _ => true,
        }),
        Some(_) => true,
    }
}

pub fn value_text(value: &Value) -> Option<String> {
    match value {
        Value::Null => None,
        Value::String(text) => Some(text.clone()),
        Value::Array(items) => {
            let parts = items
                .iter()
                .filter_map(|item| {
                    content_item_text(item)
                        .or_else(|| value_text(item))
                        .filter(|text| !text.trim().is_empty())
                })
                .collect::<Vec<_>>();
            (!parts.is_empty()).then(|| parts.join("\n\n"))
        }
        Value::Object(object) => object
            .get("text")
            .and_then(Value::as_str)
            .map(ToOwned::to_owned)
            .or_else(|| Some(value.to_string())),
        _ => Some(value.to_string()),
    }
}
