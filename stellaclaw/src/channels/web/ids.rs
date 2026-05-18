use crate::{
    channels::ProcessingState,
    conversation_metadata::ConversationMetadata,
    conversation_new::{ServiceAddr, ServiceScope},
};

pub(super) fn service_addr_storage_component(addr: &ServiceAddr) -> String {
    let scope = match &addr.scope {
        ServiceScope::Local => "local".to_string(),
        ServiceScope::Conversation(conversation_id) => format!("conversation_{conversation_id}"),
    };
    format!("{scope}__{}", addr.path.join("__"))
}

pub(super) fn foreground_session_storage_id(foreground_session_id: &str) -> String {
    if foreground_session_id.starts_with("local__agent__foreground__") {
        foreground_session_id.to_string()
    } else {
        format!("local__agent__foreground__{foreground_session_id}")
    }
}

pub(super) fn foreground_route_id_from_storage_id(storage_id: &str) -> Option<String> {
    storage_id
        .strip_prefix("local__agent__foreground__")
        .map(str::to_string)
}

pub(super) fn default_foreground_route_id(metadata: &ConversationMetadata) -> String {
    foreground_route_id_from_storage_id(&metadata.foreground_session_id)
        .unwrap_or_else(|| "main".to_string())
}

pub(super) fn processing_state_name(state: ProcessingState) -> &'static str {
    match state {
        ProcessingState::Idle => "idle",
        ProcessingState::Typing => "typing",
    }
}
