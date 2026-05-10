use std::sync::atomic::{AtomicU64, Ordering};

use serde::{Deserialize, Serialize};

use super::{
    tool_runtime::{LocalToolError, ToolExecutionContext},
    ConversationBridgeRequest,
};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolBinaryEnsureRequest {
    pub tool: String,
    #[serde(default)]
    pub host: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolBinaryEnsureResponse {
    pub status: String,
    pub tool: String,
    pub version: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub platform: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub local_path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub remote_path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path_dir: Option<String>,
}

static TOOL_BINARY_REQUEST_COUNTER: AtomicU64 = AtomicU64::new(1);

pub(super) fn ensure_tool_binary(
    context: &ToolExecutionContext<'_>,
    tool: &str,
    host: Option<&str>,
) -> Result<ToolBinaryEnsureResponse, LocalToolError> {
    let Some(bridge) = context.conversation_bridge else {
        return Err(LocalToolError::Bridge(
            "tool binary manager is not configured".to_string(),
        ));
    };
    let request = ToolBinaryEnsureRequest {
        tool: tool.to_string(),
        host: host.map(str::to_string),
    };
    let request_id = format!(
        "tool_binary_ensure_{tool}_{}",
        TOOL_BINARY_REQUEST_COUNTER.fetch_add(1, Ordering::Relaxed)
    );
    let response = bridge
        .call(ConversationBridgeRequest {
            request_id: request_id.clone(),
            tool_call_id: request_id,
            tool_name: "tool_binary_ensure".to_string(),
            action: "tool_binary_ensure".to_string(),
            payload: serde_json::to_value(request).map_err(|error| {
                LocalToolError::InvalidArguments(format!(
                    "failed to encode tool binary request: {error}"
                ))
            })?,
        })
        .map_err(|error| LocalToolError::Bridge(error.to_string()))?;
    let text = response
        .result
        .result
        .context
        .ok_or_else(|| LocalToolError::Bridge("tool binary response missing context".to_string()))?
        .text;
    let parsed: ToolBinaryEnsureResponse = serde_json::from_str(&text).map_err(|error| {
        LocalToolError::Bridge(format!(
            "failed to parse tool binary response: {error}: {text}"
        ))
    })?;
    if parsed.status == "success" {
        Ok(parsed)
    } else {
        Err(LocalToolError::Bridge(text))
    }
}
