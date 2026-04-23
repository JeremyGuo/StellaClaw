use crate::transcript::{TranscriptEntry, TranscriptEntrySkeleton};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

macro_rules! uuid_newtype {
    ($name:ident) => {
        #[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
        pub struct $name(pub Uuid);

        impl $name {
            pub fn new() -> Self {
                Self(Uuid::new_v4())
            }
        }

        impl Default for $name {
            fn default() -> Self {
                Self::new()
            }
        }
    };
}

uuid_newtype!(RequestId);
uuid_newtype!(EventId);
uuid_newtype!(QueryId);
uuid_newtype!(ToolRequestId);
uuid_newtype!(TurnId);
uuid_newtype!(ConversationMessageId);
uuid_newtype!(ActorMessageId);

#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ConversationId(pub String);

#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct SessionId(pub Uuid);

impl SessionId {
    pub fn new() -> Self {
        Self(Uuid::new_v4())
    }
}

impl Default for SessionId {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SessionRequestEnvelope {
    pub request_id: RequestId,
    pub conversation_id: ConversationId,
    pub session_id: SessionId,
    #[serde(flatten)]
    pub body: SessionRequest,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum SessionRequest {
    QuerySessionView(SessionViewQuery),
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SessionEventEnvelope {
    pub event_id: EventId,
    pub conversation_id: ConversationId,
    pub session_id: SessionId,
    #[serde(flatten)]
    pub body: SessionEvent,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum SessionEvent {
    SessionViewResult(SessionViewResult),
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SessionViewQuery {
    pub query_id: QueryId,
    #[serde(flatten)]
    pub kind: SessionViewQueryKind,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum SessionViewQueryKind {
    TranscriptPage(TranscriptPageQuery),
    TranscriptDetail(TranscriptDetailQuery),
    LiveSnapshot(LiveSnapshotQuery),
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TranscriptPageQuery {
    pub offset: usize,
    pub limit: usize,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TranscriptDetailQuery {
    pub seq_start: usize,
    pub seq_end: usize,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct LiveSnapshotQuery {
    #[serde(default)]
    pub include_progress: bool,
    #[serde(default)]
    pub include_pending_mailbox_summary: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SessionViewResult {
    pub query_id: QueryId,
    pub result: Result<SessionViewPayload, QueryError>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum SessionViewPayload {
    TranscriptPage(TranscriptPageView),
    TranscriptDetail(TranscriptDetailView),
    LiveSnapshot(LiveSnapshotView),
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TranscriptPageView {
    pub entries: Vec<TranscriptEntrySkeleton>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TranscriptDetailView {
    pub entries: Vec<TranscriptEntry>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct LiveSnapshotView {
    #[serde(default)]
    pub active_turn_id: Option<TurnId>,
    #[serde(default)]
    pub progress_summary: Option<String>,
    #[serde(default)]
    pub control_mailbox_len: Option<usize>,
    #[serde(default)]
    pub data_mailbox_len: Option<usize>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct QueryError {
    pub code: QueryErrorCode,
    pub message: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum QueryErrorCode {
    NotFound,
    InvalidCursor,
    UnsupportedView,
    SessionUnavailable,
    InternalError,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_request_uses_stable_query_shape() {
        let request = SessionRequestEnvelope {
            request_id: RequestId::new(),
            conversation_id: ConversationId("web-demo".to_string()),
            session_id: SessionId::new(),
            body: SessionRequest::QuerySessionView(SessionViewQuery {
                query_id: QueryId::new(),
                kind: SessionViewQueryKind::TranscriptPage(TranscriptPageQuery {
                    offset: 0,
                    limit: 20,
                }),
            }),
        };
        let json = serde_json::to_value(&request).expect("serialize request");
        assert_eq!(json["type"], "query_session_view");
        assert_eq!(json["kind"], "transcript_page");
        assert_eq!(json["offset"], 0);
        assert_eq!(json["limit"], 20);
    }

    #[test]
    fn session_event_uses_stable_result_shape() {
        let event = SessionEventEnvelope {
            event_id: EventId::new(),
            conversation_id: ConversationId("web-demo".to_string()),
            session_id: SessionId::new(),
            body: SessionEvent::SessionViewResult(SessionViewResult {
                query_id: QueryId::new(),
                result: Err(QueryError {
                    code: QueryErrorCode::SessionUnavailable,
                    message: "session is restarting".to_string(),
                }),
            }),
        };
        let json = serde_json::to_value(&event).expect("serialize event");
        assert_eq!(json["type"], "session_view_result");
        assert_eq!(json["result"]["Err"]["code"], "session_unavailable");
    }
}
