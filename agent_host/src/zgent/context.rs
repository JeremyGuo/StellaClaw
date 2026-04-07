use anyhow::Result;
use serde_json::Value;

use crate::zgent::client::{ZgentRpcClient, ZgentSharedRpcClient};

#[derive(Clone, Debug, PartialEq)]
pub struct ZgentConversationSnapshot {
    pub messages: Value,
    pub hash: String,
}

pub trait ZgentContextBridge {
    fn get_conversation(&mut self, session_id: &str) -> Result<ZgentConversationSnapshot>;
    fn set_conversation(
        &mut self,
        session_id: &str,
        snapshot: &ZgentConversationSnapshot,
        if_hash: Option<&str>,
    ) -> Result<String>;
}

impl ZgentContextBridge for ZgentRpcClient {
    fn get_conversation(&mut self, session_id: &str) -> Result<ZgentConversationSnapshot> {
        let result = self.request_value(
            "ctx/getConversation",
            serde_json::json!({
                "session_id": session_id,
            }),
        )?;
        Ok(ZgentConversationSnapshot {
            messages: result
                .get("messages")
                .cloned()
                .unwrap_or(Value::Array(Vec::new())),
            hash: result
                .get("hash")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
        })
    }

    fn set_conversation(
        &mut self,
        session_id: &str,
        snapshot: &ZgentConversationSnapshot,
        if_hash: Option<&str>,
    ) -> Result<String> {
        let result = self.request_value(
            "ctx/setConversation",
            serde_json::json!({
                "session_id": session_id,
                "messages": snapshot.messages,
                "if_hash": if_hash,
            }),
        )?;
        Ok(result
            .get("hash")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string())
    }
}

impl ZgentContextBridge for ZgentSharedRpcClient {
    fn get_conversation(&mut self, session_id: &str) -> Result<ZgentConversationSnapshot> {
        let result = self.request_value(
            "ctx/getConversation",
            serde_json::json!({
                "session_id": session_id,
            }),
        )?;
        Ok(ZgentConversationSnapshot {
            messages: result
                .get("messages")
                .cloned()
                .unwrap_or(Value::Array(Vec::new())),
            hash: result
                .get("hash")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
        })
    }

    fn set_conversation(
        &mut self,
        session_id: &str,
        snapshot: &ZgentConversationSnapshot,
        if_hash: Option<&str>,
    ) -> Result<String> {
        let result = self.request_value(
            "ctx/setConversation",
            serde_json::json!({
                "session_id": session_id,
                "messages": snapshot.messages,
                "if_hash": if_hash,
            }),
        )?;
        Ok(result
            .get("hash")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string())
    }
}
