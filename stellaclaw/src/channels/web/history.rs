use serde_json::{json, Value};
use stellaclaw_core::session_actor::ChatMessage;

#[derive(Debug, Default)]
pub(super) struct MessageSummary {
    pub(super) message_count: usize,
    pub(super) last_message_id: Option<String>,
    pub(super) last_message_index: Option<usize>,
    pub(super) last_message_time: Option<String>,
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
