#![allow(dead_code)]

use std::collections::BTreeMap;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::{
    conversation_metadata::ConversationMetadata,
    conversation_new::{ConversationRuntimeConfig, ServiceAddr, ServiceCall},
    service_protos::agent_session::{AgentSessionBinding, AgentSessionKind},
};

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct KernelRuntimeConfigPatch {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_server_path: Option<Option<std::path::PathBuf>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_profile: Option<Option<crate::config::SessionProfile>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_defaults: Option<crate::config::SessionDefaults>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub memory_enabled: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_remote_mode: Option<stellaclaw_core::session_actor::ToolRemoteMode>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sandbox: Option<Option<crate::config::SandboxConfig>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_effort: Option<Option<String>>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct KernelMetadataPatch {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub conversation_nickname: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model_selection_pending: Option<bool>,
    #[serde(default)]
    pub session_nicknames: BTreeMap<String, Option<String>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum KernelRequest {
    CreateAgentSession {
        kind: AgentSessionKind,
        id: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        binding: Option<AgentSessionBinding>,
    },
    StopService {
        addr: ServiceAddr,
        reason: Option<String>,
    },
    UpdateRuntimeConfig {
        patch: KernelRuntimeConfigPatch,
    },
    QueryMetadata,
    UpdateMetadata {
        patch: KernelMetadataPatch,
    },
    ListServices,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum KernelResponse {
    AgentSessionCreated {
        addr: ServiceAddr,
    },
    ServiceStopped {
        addr: ServiceAddr,
    },
    RuntimeConfigUpdated {
        config: ConversationRuntimeConfig,
        #[serde(default)]
        updated_services: Vec<ServiceAddr>,
    },
    Metadata {
        metadata: ConversationMetadata,
    },
    MetadataUpdated {
        metadata: ConversationMetadata,
    },
    Services {
        addrs: Vec<ServiceAddr>,
    },
    Error {
        code: String,
        message: String,
    },
}

pub fn encode_request(request: KernelRequest) -> Result<Value> {
    serde_json::to_value(request).context("failed to encode kernel request")
}

pub fn decode_request(payload: Value) -> Result<KernelRequest> {
    serde_json::from_value(payload).context("failed to decode kernel request")
}

pub fn encode_response(response: KernelResponse) -> Result<Value> {
    serde_json::to_value(response).context("failed to encode kernel response")
}

pub fn decode_response(payload: Value) -> Result<KernelResponse> {
    serde_json::from_value(payload).context("failed to decode kernel response")
}

pub fn create_agent_session_call(
    source: ServiceAddr,
    kind: AgentSessionKind,
    id: Option<String>,
) -> Result<ServiceCall> {
    Ok(ServiceCall {
        source,
        target: ServiceAddr::kernel(),
        payload: encode_request(KernelRequest::CreateAgentSession {
            kind,
            id,
            binding: None,
        })?,
    })
}

pub fn create_agent_session_with_binding_call(
    source: ServiceAddr,
    kind: AgentSessionKind,
    id: Option<String>,
    binding: AgentSessionBinding,
) -> Result<ServiceCall> {
    Ok(ServiceCall {
        source,
        target: ServiceAddr::kernel(),
        payload: encode_request(KernelRequest::CreateAgentSession {
            kind,
            id,
            binding: Some(binding),
        })?,
    })
}

pub fn stop_service_call(
    source: ServiceAddr,
    addr: ServiceAddr,
    reason: Option<String>,
) -> Result<ServiceCall> {
    Ok(ServiceCall {
        source,
        target: ServiceAddr::kernel(),
        payload: encode_request(KernelRequest::StopService { addr, reason })?,
    })
}

pub fn update_runtime_config_call(
    source: ServiceAddr,
    patch: KernelRuntimeConfigPatch,
) -> Result<ServiceCall> {
    Ok(ServiceCall {
        source,
        target: ServiceAddr::kernel(),
        payload: encode_request(KernelRequest::UpdateRuntimeConfig { patch })?,
    })
}

pub fn query_metadata_call(source: ServiceAddr) -> Result<ServiceCall> {
    Ok(ServiceCall {
        source,
        target: ServiceAddr::kernel(),
        payload: encode_request(KernelRequest::QueryMetadata)?,
    })
}

pub fn update_metadata_call(
    source: ServiceAddr,
    patch: KernelMetadataPatch,
) -> Result<ServiceCall> {
    Ok(ServiceCall {
        source,
        target: ServiceAddr::kernel(),
        payload: encode_request(KernelRequest::UpdateMetadata { patch })?,
    })
}
