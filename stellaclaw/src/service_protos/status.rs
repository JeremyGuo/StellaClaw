#![allow(dead_code)]

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::conversation_new::{ServiceAddr, ServiceCall};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum StatusRequest {
    Snapshot,
    Observe { service: ServiceAddr },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum StatusResponse {
    Snapshot { snapshot: Value },
    Accepted,
    Error { message: String },
}

pub fn encode_request(request: StatusRequest) -> Result<Value> {
    serde_json::to_value(request).context("failed to encode status request")
}

pub fn decode_request(payload: Value) -> Result<StatusRequest> {
    serde_json::from_value(payload).context("failed to decode status request")
}

pub fn encode_response(response: StatusResponse) -> Result<Value> {
    serde_json::to_value(response).context("failed to encode status response")
}

pub fn decode_response(payload: Value) -> Result<StatusResponse> {
    serde_json::from_value(payload).context("failed to decode status response")
}

pub fn status_call(
    source: ServiceAddr,
    target: ServiceAddr,
    request: StatusRequest,
) -> Result<ServiceCall> {
    Ok(ServiceCall {
        source,
        target,
        payload: encode_request(request)?,
    })
}
