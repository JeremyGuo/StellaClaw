#![allow(dead_code)]

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::conversation_new::{ServiceAddr, ServiceCall};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ControlRequest {
    Apply { name: String, value: Value },
    Query { name: String },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ControlResponse {
    Accepted,
    Value { name: String, value: Value },
}

pub fn encode_request(request: ControlRequest) -> Result<Value> {
    serde_json::to_value(request).context("failed to encode control request")
}

pub fn decode_request(payload: Value) -> Result<ControlRequest> {
    serde_json::from_value(payload).context("failed to decode control request")
}

pub fn encode_response(response: ControlResponse) -> Result<Value> {
    serde_json::to_value(response).context("failed to encode control response")
}

pub fn decode_response(payload: Value) -> Result<ControlResponse> {
    serde_json::from_value(payload).context("failed to decode control response")
}

pub fn apply_call(
    source: ServiceAddr,
    target: ServiceAddr,
    name: impl Into<String>,
    value: Value,
) -> Result<ServiceCall> {
    Ok(ServiceCall {
        source,
        target,
        payload: encode_request(ControlRequest::Apply {
            name: name.into(),
            value,
        })?,
    })
}
