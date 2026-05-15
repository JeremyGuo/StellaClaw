#![allow(dead_code)]

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use stellaclaw_core::session_actor::{ChatMessage, FileItem};

use crate::conversation_new::{ConversationRuntimeConfig, ServiceAddr, ServiceCall};

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkspaceTarget {
    #[default]
    Auto,
    LocalWorkspace,
    LocalOverlay,
    Remote,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkspaceEntryKind {
    Directory,
    File,
    Symlink,
    Other,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkspaceFileEncoding {
    Utf8,
    Base64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkspaceEntry {
    pub name: String,
    pub path: String,
    pub kind: WorkspaceEntryKind,
    pub size_bytes: Option<u64>,
    pub modified_ms: Option<u128>,
    pub hidden: bool,
    pub readonly: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkspaceRemote {
    pub host: String,
    pub cwd: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum WorkspaceRequest {
    QueryRoot,
    RegisterAttachment {
        uri: String,
    },
    MaterializeMessage {
        message: ChatMessage,
    },
    MaterializeFiles {
        files: Vec<FileItem>,
    },
    UpdateRuntimeConfig {
        config: ConversationRuntimeConfig,
    },
    List {
        path: Option<String>,
        #[serde(default)]
        target: WorkspaceTarget,
        limit: Option<usize>,
    },
    ReadFile {
        path: String,
        #[serde(default)]
        target: WorkspaceTarget,
        offset: Option<u64>,
        limit_bytes: Option<usize>,
    },
    WriteFile {
        path: String,
        #[serde(default)]
        target: WorkspaceTarget,
        encoding: WorkspaceFileEncoding,
        data: String,
        #[serde(default = "default_true")]
        create_parent_dirs: bool,
        #[serde(default)]
        overwrite: bool,
    },
    DeletePath {
        path: String,
        #[serde(default)]
        target: WorkspaceTarget,
    },
    MovePath {
        from_path: String,
        to_path: String,
        #[serde(default)]
        target: WorkspaceTarget,
    },
    UploadArchive {
        dir_path: String,
        #[serde(default)]
        target: WorkspaceTarget,
        encoding: WorkspaceFileEncoding,
        data: String,
    },
    DownloadArchive {
        paths: Vec<String>,
        #[serde(default)]
        target: WorkspaceTarget,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum WorkspaceResponse {
    Root {
        path: String,
    },
    AttachmentRegistered {
        uri: String,
    },
    MessageMaterialized {
        message: ChatMessage,
    },
    FilesMaterialized {
        files: Vec<FileItem>,
    },
    Listing {
        target: WorkspaceTarget,
        remote: Option<WorkspaceRemote>,
        workspace_root: String,
        path: String,
        parent: Option<String>,
        total_entries: usize,
        returned_entries: usize,
        truncated: bool,
        entries: Vec<WorkspaceEntry>,
    },
    File {
        target: WorkspaceTarget,
        remote: Option<WorkspaceRemote>,
        workspace_root: String,
        path: String,
        name: String,
        size_bytes: u64,
        modified_ms: Option<u128>,
        offset: u64,
        returned_bytes: usize,
        truncated: bool,
        encoding: WorkspaceFileEncoding,
        data: String,
    },
    WriteCompleted {
        target: WorkspaceTarget,
        path: String,
        bytes_written: usize,
    },
    Deleted {
        target: WorkspaceTarget,
        path: String,
    },
    Moved {
        target: WorkspaceTarget,
        from_path: String,
        to_path: String,
    },
    ArchiveUploaded {
        target: WorkspaceTarget,
        path: String,
        entries_extracted: usize,
    },
    ArchiveDownloaded {
        target: WorkspaceTarget,
        paths: Vec<String>,
        encoding: WorkspaceFileEncoding,
        data: String,
        bytes: usize,
    },
    Error {
        message: String,
    },
}

fn default_true() -> bool {
    true
}

pub fn encode_request(request: WorkspaceRequest) -> Result<Value> {
    serde_json::to_value(request).context("failed to encode workspace request")
}

pub fn decode_request(payload: Value) -> Result<WorkspaceRequest> {
    serde_json::from_value(payload).context("failed to decode workspace request")
}

pub fn encode_response(response: WorkspaceResponse) -> Result<Value> {
    serde_json::to_value(response).context("failed to encode workspace response")
}

pub fn decode_response(payload: Value) -> Result<WorkspaceResponse> {
    serde_json::from_value(payload).context("failed to decode workspace response")
}

pub fn workspace_call(source: ServiceAddr, request: WorkspaceRequest) -> Result<ServiceCall> {
    Ok(ServiceCall {
        source,
        target: ServiceAddr::workspace(),
        payload: encode_request(request)?,
    })
}

pub fn update_runtime_config_call(
    source: ServiceAddr,
    target: ServiceAddr,
    config: ConversationRuntimeConfig,
) -> Result<ServiceCall> {
    Ok(ServiceCall {
        source,
        target,
        payload: encode_request(WorkspaceRequest::UpdateRuntimeConfig { config })?,
    })
}
