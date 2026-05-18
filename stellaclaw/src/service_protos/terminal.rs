#![allow(dead_code)]

use anyhow::{Context, Result};
use base64::{engine::general_purpose, Engine as _};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::{
    conversation_new::{ConversationRuntimeConfig, ServiceAddr, ServiceCall},
    services::terminal_runtime::{
        TerminalCreateRequest, TerminalOutputChunk, TerminalReplay, TerminalResizeRequest,
        TerminalSummary,
    },
};

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TerminalDataEncoding {
    Utf8,
    Base64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum TerminalRequest {
    UpdateRuntimeConfig {
        config: ConversationRuntimeConfig,
    },
    List,
    Get {
        terminal_id: String,
    },
    Create {
        request: TerminalCreateRequest,
    },
    Terminate {
        terminal_id: String,
    },
    Input {
        terminal_id: String,
        encoding: TerminalDataEncoding,
        data: String,
    },
    Resize {
        terminal_id: String,
        request: TerminalResizeRequest,
    },
    Replay {
        terminal_id: String,
        offset: u64,
    },
    Attach {
        terminal_id: String,
        offset: u64,
    },
    Detach {
        terminal_id: String,
        subscriber_id: u64,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum TerminalResponse {
    RuntimeConfigUpdated,
    Terminals {
        terminals: Vec<TerminalSummary>,
    },
    Terminal {
        terminal: TerminalSummary,
    },
    Replay {
        replay: TerminalReplaySnapshot,
    },
    Attached {
        replay: TerminalReplaySnapshot,
        subscriber_id: Option<u64>,
    },
    Output {
        terminal_id: String,
        subscriber_id: Option<u64>,
        encoding: TerminalDataEncoding,
        data: String,
    },
    Detached {
        terminal_id: String,
        subscriber_id: u64,
    },
    Error {
        code: String,
        message: String,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TerminalReplaySnapshot {
    pub terminal_id: String,
    pub requested_offset: u64,
    pub replay_start_offset: u64,
    pub buffer_start_offset: u64,
    pub next_offset: u64,
    pub dropped_bytes: u64,
    pub chunks: Vec<TerminalChunkSnapshot>,
    pub running: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TerminalChunkSnapshot {
    pub encoding: TerminalDataEncoding,
    pub data: String,
}

impl TerminalDataEncoding {
    pub fn decode(self, data: &str) -> Result<Vec<u8>> {
        match self {
            Self::Utf8 => Ok(data.as_bytes().to_vec()),
            Self::Base64 => general_purpose::STANDARD
                .decode(data)
                .context("failed to decode terminal base64 payload"),
        }
    }
}

pub fn replay_snapshot(replay: TerminalReplay) -> TerminalReplaySnapshot {
    TerminalReplaySnapshot {
        terminal_id: replay.terminal_id,
        requested_offset: replay.requested_offset,
        replay_start_offset: replay.replay_start_offset,
        buffer_start_offset: replay.buffer_start_offset,
        next_offset: replay.next_offset,
        dropped_bytes: replay.dropped_bytes,
        chunks: replay
            .chunks
            .into_iter()
            .map(chunk_snapshot)
            .collect::<Vec<_>>(),
        running: replay.running,
    }
}

pub fn chunk_snapshot(chunk: TerminalOutputChunk) -> TerminalChunkSnapshot {
    TerminalChunkSnapshot {
        encoding: TerminalDataEncoding::Base64,
        data: general_purpose::STANDARD.encode(chunk.bytes),
    }
}

pub fn encode_request(request: TerminalRequest) -> Result<Value> {
    serde_json::to_value(request).context("failed to encode terminal request")
}

pub fn decode_request(payload: Value) -> Result<TerminalRequest> {
    serde_json::from_value(payload).context("failed to decode terminal request")
}

pub fn encode_response(response: TerminalResponse) -> Result<Value> {
    serde_json::to_value(response).context("failed to encode terminal response")
}

pub fn decode_response(payload: Value) -> Result<TerminalResponse> {
    serde_json::from_value(payload).context("failed to decode terminal response")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_terminal_summary_payload() {
        let payload = serde_json::json!({
            "type": "terminal",
            "terminal": {
                "terminal_id": "terminal_0000",
                "conversation_id": "web-main-000016",
                "mode": "fixed_ssh",
                "remote": { "host": "cpu001", "cwd": "/home/guojunyi/" },
                "shell": "${SHELL:-sh}",
                "cwd": "/home/guojunyi/",
                "cols": 120,
                "rows": 30,
                "running": true,
                "created_ms": 1779133185061u64,
                "updated_ms": 1779133185061u64,
                "next_offset": 0,
            }
        });
        decode_response(payload).expect("terminal payload decodes");
    }
}

pub fn update_runtime_config_call(
    source: ServiceAddr,
    target: ServiceAddr,
    config: ConversationRuntimeConfig,
) -> Result<ServiceCall> {
    Ok(ServiceCall::new(
        source,
        target,
        encode_request(TerminalRequest::UpdateRuntimeConfig { config })?,
    ))
}

pub fn terminal_call(
    source: ServiceAddr,
    target: ServiceAddr,
    request: TerminalRequest,
) -> Result<ServiceCall> {
    Ok(ServiceCall::new(source, target, encode_request(request)?))
}
