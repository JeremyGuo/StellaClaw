#![allow(dead_code)]

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::conversation_new::{ServiceAddr, ServiceCall};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ToolBinaryRequest {
    Ensure {
        #[serde(alias = "name")]
        tool: String,
        #[serde(default, alias = "remote_host")]
        host: Option<String>,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ToolBinaryResponse {
    Ready {
        tool: String,
        version: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        platform: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        local_path: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        remote_path: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        path_dir: Option<String>,
    },
    Failure {
        reason: String,
    },
}

pub fn encode_request(request: ToolBinaryRequest) -> Result<Value> {
    serde_json::to_value(request).context("failed to encode tool binary request")
}

pub fn decode_request(payload: Value) -> Result<ToolBinaryRequest> {
    serde_json::from_value(payload).context("failed to decode tool binary request")
}

pub fn encode_response(response: ToolBinaryResponse) -> Result<Value> {
    serde_json::to_value(response).context("failed to encode tool binary response")
}

pub fn decode_response(payload: Value) -> Result<ToolBinaryResponse> {
    serde_json::from_value(payload).context("failed to decode tool binary response")
}

pub fn tool_binary_call(source: ServiceAddr, request: ToolBinaryRequest) -> Result<ServiceCall> {
    Ok(ServiceCall {
        source,
        target: ServiceAddr::tool_binary(),
        payload: encode_request(request)?,
    })
}
