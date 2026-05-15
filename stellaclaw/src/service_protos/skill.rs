#![allow(dead_code)]

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::conversation_new::{ServiceAddr, ServiceCall};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SkillPersistMode {
    Create,
    Update,
    Delete,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SkillInfo {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    pub runtime_path: String,
    pub workspace_path: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum SkillRequest {
    Reconcile,
    Persist {
        skill_name: String,
        mode: SkillPersistMode,
    },
    List,
    Load {
        skill_name: String,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum SkillResponse {
    Reconciled {
        runtime_skills: usize,
        synced_skills: usize,
    },
    Persisted {
        skill_name: String,
        mode: SkillPersistMode,
        synced_workspaces: usize,
    },
    Skills {
        skills: Vec<SkillInfo>,
    },
    Loaded {
        skill_name: String,
        description: String,
        content: String,
    },
    Failure {
        reason: String,
    },
}

pub fn encode_request(request: SkillRequest) -> Result<Value> {
    serde_json::to_value(request).context("failed to encode skill request")
}

pub fn decode_request(payload: Value) -> Result<SkillRequest> {
    serde_json::from_value(payload).context("failed to decode skill request")
}

pub fn encode_response(response: SkillResponse) -> Result<Value> {
    serde_json::to_value(response).context("failed to encode skill response")
}

pub fn decode_response(payload: Value) -> Result<SkillResponse> {
    serde_json::from_value(payload).context("failed to decode skill response")
}

pub fn skill_call(source: ServiceAddr, request: SkillRequest) -> Result<ServiceCall> {
    Ok(ServiceCall {
        source,
        target: ServiceAddr::skill(),
        payload: encode_request(request)?,
    })
}
