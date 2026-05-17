use std::{
    fs,
    io::{BufRead, BufReader},
    path::{Path, PathBuf},
};

use serde_json::{json, Value};
use stellaclaw_core::session_actor::ChatMessage;

use crate::conversation_metadata::WorkdirLayout;

use super::{
    http::{HttpError, HttpResult},
    ids::foreground_session_storage_id,
};

#[derive(Debug, Default)]
pub(super) struct MessageSummary {
    pub(super) message_count: usize,
    pub(super) last_message_id: Option<String>,
    pub(super) last_message_index: Option<usize>,
    pub(super) last_message_time: Option<String>,
}

pub(super) fn read_messages(path: &Path) -> HttpResult<Vec<Value>> {
    if !path.exists() {
        return Ok(Vec::new());
    }
    let file = fs::File::open(path).map_err(HttpError::internal)?;
    let reader = BufReader::new(file);
    let mut messages = Vec::new();
    for (index, line) in reader.lines().enumerate() {
        let line = line.map_err(HttpError::internal)?;
        if line.trim().is_empty() {
            continue;
        }
        let message: ChatMessage = serde_json::from_str(&line).map_err(HttpError::internal)?;
        messages.push(decorate_message(&message, index));
    }
    Ok(messages)
}

pub(super) fn decorate_message(message: &ChatMessage, index: usize) -> Value {
    let mut value = serde_json::to_value(message).unwrap_or_else(|_| json!({}));
    if let Value::Object(map) = &mut value {
        map.insert("index".to_string(), json!(index));
        if !message.message_id.is_empty() {
            map.insert("id".to_string(), json!(message.message_id));
        }
    }
    value
}

pub(super) fn message_summary(path: &Path) -> MessageSummary {
    let Ok(messages) = read_messages(path) else {
        return MessageSummary::default();
    };
    let mut summary = MessageSummary {
        message_count: messages.len(),
        ..MessageSummary::default()
    };
    if let Some(last) = messages.last() {
        summary.last_message_index = last
            .get("index")
            .and_then(Value::as_u64)
            .and_then(|value| usize::try_from(value).ok());
        summary.last_message_id = last
            .get("message_id")
            .or_else(|| last.get("id"))
            .and_then(Value::as_str)
            .map(str::to_string);
        summary.last_message_time = last
            .get("message_time")
            .and_then(Value::as_str)
            .map(str::to_string);
    }
    summary
}

pub(super) fn message_log_path(
    workdir: &Path,
    conversation_id: &str,
    foreground_session_id: &str,
) -> PathBuf {
    WorkdirLayout::new(workdir)
        .conversation_root(conversation_id)
        .join(".stellaclaw")
        .join("log")
        .join(sanitize_session_id_for_log_path(
            &foreground_session_storage_id(foreground_session_id),
        ))
        .join("all_messages.jsonl")
}

fn sanitize_session_id_for_log_path(session_id: &str) -> String {
    session_id
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '.') {
                ch
            } else {
                '_'
            }
        })
        .collect()
}
