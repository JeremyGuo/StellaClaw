#![allow(dead_code)]

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::{
    conversation_new::{ServiceAddr, ServiceCall},
    memory::MemoryScope,
};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MemorySourceRef {
    pub conversation_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_addr: Option<ServiceAddr>,
    pub session_type: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum MemoryRequest {
    Search {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        source: Option<MemorySourceRef>,
        query: String,
        #[serde(default)]
        scopes: Vec<MemoryScope>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        limit: Option<usize>,
    },
    Write {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        source: Option<MemorySourceRef>,
        scope: MemoryScope,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        subject: Option<String>,
        text: String,
        #[serde(default)]
        tags: Vec<String>,
    },
    Update {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        source: Option<MemorySourceRef>,
        memory_id: String,
        text: String,
    },
    Delete {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        source: Option<MemorySourceRef>,
        memory_id: String,
    },
    PromptContext {
        scope: MemoryScope,
        max_bytes: usize,
    },
    Maintain,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MemorySearchResult {
    pub id: String,
    pub scope: MemoryScope,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subject: Option<String>,
    pub text: String,
    #[serde(default)]
    pub tags: Vec<String>,
    pub updated_at: String,
    pub score: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum MemoryResponse {
    SearchResults {
        results: Vec<MemorySearchResult>,
        truncated: bool,
    },
    Accepted,
    Failure {
        reason: String,
    },
    PromptContext {
        scope: MemoryScope,
        text: String,
        entries_hash: String,
        rendered_size_bytes: usize,
        truncated: bool,
    },
    MaintenanceCompleted,
}

pub fn encode_request(request: MemoryRequest) -> Result<Value> {
    serde_json::to_value(request).context("failed to encode memory request")
}

pub fn decode_request(payload: Value) -> Result<MemoryRequest> {
    serde_json::from_value(payload).context("failed to decode memory request")
}

pub fn encode_response(response: MemoryResponse) -> Result<Value> {
    serde_json::to_value(response).context("failed to encode memory response")
}

pub fn decode_response(payload: Value) -> Result<MemoryResponse> {
    serde_json::from_value(payload).context("failed to decode memory response")
}

pub fn memory_call(source: ServiceAddr, request: MemoryRequest) -> Result<ServiceCall> {
    Ok(ServiceCall {
        source,
        target: ServiceAddr::memory(),
        payload: encode_request(request)?,
    })
}

pub fn search_call(
    source: ServiceAddr,
    query: impl Into<String>,
    scopes: Vec<MemoryScope>,
    limit: Option<usize>,
) -> Result<ServiceCall> {
    memory_call(
        source,
        MemoryRequest::Search {
            source: None,
            query: query.into(),
            scopes,
            limit,
        },
    )
}
