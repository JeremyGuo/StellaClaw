#![allow(dead_code)]

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::conversation_new::{ServiceAddr, ServiceCall};
use crate::service_protos::agent_session::AgentSessionEvent;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CronTaskRegistration {
    pub task_id: String,
    pub registered_by: ServiceAddr,
    pub channel_addr: ServiceAddr,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub foreground_session_addr: Option<ServiceAddr>,
    #[serde(default)]
    pub schedule: CronSchedule,
    pub payload: CronTaskPayload,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum CronSchedule {
    Manual,
    IntervalSeconds {
        seconds: f64,
    },
    CronExpression {
        expression: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        timezone: Option<String>,
    },
}

impl Default for CronSchedule {
    fn default() -> Self {
        Self::Manual
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum CronTaskPayload {
    Prompt {
        prompt: String,
        #[serde(default)]
        output_policy: CronTaskOutputPolicy,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum CronTaskOutputPolicy {
    ForwardResultToForeground,
    StoreOnly,
}

impl Default for CronTaskOutputPolicy {
    fn default() -> Self {
        Self::ForwardResultToForeground
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum CronRunStatus {
    Running,
    Completed,
    Failed,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CronTaskStatus {
    pub registration: CronTaskRegistration,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub active_background_addr: Option<ServiceAddr>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_due_in_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_run_status: Option<CronRunStatus>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_result_summary: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_error: Option<String>,
    pub consecutive_failures: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum CronRequest {
    RegisterTask {
        task: CronTaskRegistration,
    },
    UpdateTask {
        task_id: String,
        patch: CronTaskPatch,
    },
    RemoveTask {
        task_id: String,
    },
    DisableTasksForOwner {
        owner: ServiceAddr,
        reason: String,
    },
    ListTasks {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        owner: Option<ServiceAddr>,
    },
    GetTaskStatus {
        task_id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        owner: Option<ServiceAddr>,
    },
    TriggerTaskNow {
        task_id: String,
    },
    AgentSessionEvent {
        session_addr: ServiceAddr,
        event: AgentSessionEvent,
    },
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CronTaskPatch {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<Option<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<Option<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enabled: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub schedule: Option<CronSchedule>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub payload: Option<CronTaskPayload>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum CronResponse {
    Tasks { tasks: Vec<CronTaskRegistration> },
    TaskStatus { status: Option<CronTaskStatus> },
    Task { task: Option<CronTaskRegistration> },
    Accepted,
    Rejected { reason: String },
}

pub fn encode_request(request: CronRequest) -> Result<Value> {
    serde_json::to_value(request).context("failed to encode cron request")
}

pub fn decode_request(payload: Value) -> Result<CronRequest> {
    serde_json::from_value(payload).context("failed to decode cron request")
}

pub fn decode_response(payload: Value) -> Result<CronResponse> {
    serde_json::from_value(payload).context("failed to decode cron response")
}

pub fn encode_response(response: CronResponse) -> Result<Value> {
    serde_json::to_value(response).context("failed to encode cron response")
}

pub fn list_tasks_call(source: ServiceAddr, target: ServiceAddr) -> Result<ServiceCall> {
    Ok(ServiceCall::new(
        source,
        target,
        encode_request(CronRequest::ListTasks { owner: None })?,
    ))
}

pub fn register_task_call(
    source: ServiceAddr,
    target: ServiceAddr,
    task: CronTaskRegistration,
) -> Result<ServiceCall> {
    Ok(ServiceCall::new(
        source,
        target,
        encode_request(CronRequest::RegisterTask { task })?,
    ))
}

pub fn trigger_task_now_call(
    source: ServiceAddr,
    target: ServiceAddr,
    task_id: impl Into<String>,
) -> Result<ServiceCall> {
    Ok(ServiceCall::new(
        source,
        target,
        encode_request(CronRequest::TriggerTaskNow {
            task_id: task_id.into(),
        })?,
    ))
}

pub fn get_task_status_call(
    source: ServiceAddr,
    target: ServiceAddr,
    task_id: impl Into<String>,
) -> Result<ServiceCall> {
    Ok(ServiceCall::new(
        source,
        target,
        encode_request(CronRequest::GetTaskStatus {
            task_id: task_id.into(),
            owner: None,
        })?,
    ))
}

pub fn update_task_call(
    source: ServiceAddr,
    target: ServiceAddr,
    task_id: impl Into<String>,
    patch: CronTaskPatch,
) -> Result<ServiceCall> {
    Ok(ServiceCall::new(
        source,
        target,
        encode_request(CronRequest::UpdateTask {
            task_id: task_id.into(),
            patch,
        })?,
    ))
}

pub fn remove_task_call(
    source: ServiceAddr,
    target: ServiceAddr,
    task_id: impl Into<String>,
) -> Result<ServiceCall> {
    Ok(ServiceCall::new(
        source,
        target,
        encode_request(CronRequest::RemoveTask {
            task_id: task_id.into(),
        })?,
    ))
}

pub fn disable_tasks_for_owner_call(
    source: ServiceAddr,
    target: ServiceAddr,
    owner: ServiceAddr,
    reason: impl Into<String>,
) -> Result<ServiceCall> {
    Ok(ServiceCall::new(
        source,
        target,
        encode_request(CronRequest::DisableTasksForOwner {
            owner,
            reason: reason.into(),
        })?,
    ))
}

fn default_true() -> bool {
    true
}

pub fn agent_session_event_call(
    source: ServiceAddr,
    target: ServiceAddr,
    event: AgentSessionEvent,
) -> Result<ServiceCall> {
    Ok(ServiceCall::new(
        source.clone(),
        target,
        encode_request(CronRequest::AgentSessionEvent {
            session_addr: source,
            event,
        })?,
    ))
}
