#![allow(dead_code)]

use std::collections::HashMap;

use stellaclaw_core::session_actor::{ChatMessage, ToolResultItem};

use super::protocol::{
    ChatProvisionalMessage, ChatToolResultState, ChatTurnState, QueuedOutboundMessage,
};

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ForegroundSessionKey {
    pub conversation_id: String,
    pub foreground_session_id: String,
}

impl ForegroundSessionKey {
    pub fn new(
        conversation_id: impl Into<String>,
        foreground_session_id: impl Into<String>,
    ) -> Self {
        Self {
            conversation_id: conversation_id.into(),
            foreground_session_id: foreground_session_id.into(),
        }
    }
}

#[derive(Debug, Default)]
pub struct WebChannelMainState {
    home_seq: u64,
    foreground: HashMap<ForegroundSessionKey, ForegroundLiveState>,
}

impl WebChannelMainState {
    pub fn next_home_seq(&mut self) -> u64 {
        self.home_seq = self.home_seq.saturating_add(1);
        self.home_seq
    }

    pub fn home_seq(&self) -> u64 {
        self.home_seq
    }

    pub fn foreground_state(&self, key: &ForegroundSessionKey) -> Option<&ForegroundLiveState> {
        self.foreground.get(key)
    }

    pub fn foreground_state_mut(&mut self, key: ForegroundSessionKey) -> &mut ForegroundLiveState {
        self.foreground.entry(key).or_default()
    }

    pub fn remove_foreground_state(&mut self, key: &ForegroundSessionKey) {
        self.foreground.remove(key);
    }
}

#[derive(Debug, Default, Clone)]
pub struct ForegroundLiveState {
    pub current_turn: Option<ChatTurnState>,
    pub provisional_assistant: Option<ChatProvisionalMessage>,
    pub tool_results: Vec<ChatToolResultState>,
    pub queued_outbound: Vec<QueuedOutboundMessage>,
    pub last_committed_message_id: Option<String>,
    pub last_committed_message_index: Option<usize>,
}

impl ForegroundLiveState {
    pub fn queue_outbound(&mut self, message: QueuedOutboundMessage) {
        if self
            .queued_outbound
            .iter()
            .any(|queued| queued.client_message_id == message.client_message_id)
        {
            return;
        }
        self.queued_outbound.push(message);
    }

    pub fn set_turn(&mut self, turn_id: impl Into<String>, message_id: Option<String>) {
        self.current_turn = Some(ChatTurnState {
            turn_id: turn_id.into(),
            message_id,
        });
    }

    pub fn append_provisional_assistant(
        &mut self,
        turn_id: impl Into<String>,
        message_id: impl Into<String>,
        message: ChatMessage,
    ) {
        self.provisional_assistant = Some(ChatProvisionalMessage {
            turn_id: turn_id.into(),
            message_id: message_id.into(),
            message,
        });
    }

    pub fn clear_provisional_assistant(&mut self, message_id: Option<&str>) {
        if message_id.is_some_and(|id| {
            self.provisional_assistant
                .as_ref()
                .is_some_and(|message| message.message_id != id)
        }) {
            return;
        }
        self.provisional_assistant = None;
    }

    pub fn push_tool_result(
        &mut self,
        turn_id: impl Into<String>,
        tool_result: ToolResultItem,
        committed: bool,
    ) {
        let turn_id = turn_id.into();
        if let Some(existing) = self
            .tool_results
            .iter_mut()
            .find(|state| state.tool_result.tool_call_id == tool_result.tool_call_id)
        {
            existing.turn_id = turn_id;
            existing.tool_result = tool_result;
            existing.committed = existing.committed || committed;
            return;
        }
        self.tool_results.push(ChatToolResultState {
            turn_id,
            tool_result,
            committed,
        });
    }

    pub fn commit_message(&mut self, message: &ChatMessage, index: usize) {
        if !message.message_id.is_empty() {
            self.last_committed_message_id = Some(message.message_id.clone());
        }
        self.last_committed_message_index = Some(index);
        if self
            .provisional_assistant
            .as_ref()
            .is_some_and(|provisional| provisional.message_id == message.message_id)
        {
            self.provisional_assistant = None;
        }
    }
}
