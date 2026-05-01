use std::{
    fs::{self, File},
    io::Write,
    path::{Path, PathBuf},
};

use serde::{Deserialize, Serialize};

use super::{runtime_metadata::RuntimeMetadataState, ChatMessage, ChatMessageItem, SessionInitial};

const MAX_PERSISTED_TOOL_CONTEXT_CHARS: usize = 100_000;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct SessionActorPersistedState {
    pub version: u32,
    pub initial: SessionInitial,
    #[serde(default)]
    pub all_messages: Vec<ChatMessage>,
    #[serde(default)]
    pub current_messages: Vec<ChatMessage>,
    pub next_turn_id: u64,
    pub next_batch_id: u64,
    #[serde(default)]
    pub runtime_metadata_state: RuntimeMetadataState,
}

pub(crate) struct SessionStateStore {
    dir: PathBuf,
}

impl SessionStateStore {
    pub(crate) fn open_default(session_id: &str) -> Result<Self, String> {
        Self::open_under(default_session_root()?, session_id)
    }

    pub(crate) fn open_under(root: impl AsRef<Path>, session_id: &str) -> Result<Self, String> {
        let dir = root
            .as_ref()
            .join(".stellaclaw")
            .join("log")
            .join(sanitize_session_id(session_id));
        fs::create_dir_all(&dir)
            .map_err(|error| format!("failed to create {}: {error}", dir.display()))?;
        Ok(Self { dir })
    }

    pub(crate) fn load(&self) -> Result<Option<SessionActorPersistedState>, String> {
        let path = self.session_json_path();
        if !path.exists() {
            return Ok(None);
        }
        let raw = fs::read_to_string(&path)
            .map_err(|error| format!("failed to read {}: {error}", path.display()))?;
        let mut state: SessionActorPersistedState = serde_json::from_str(&raw)
            .map_err(|error| format!("failed to parse {}: {error}", path.display()))?;
        sanitize_persisted_messages(&mut state.all_messages);
        sanitize_persisted_messages(&mut state.current_messages);
        Ok(Some(state))
    }

    pub(crate) fn save(&self, state: &SessionActorPersistedState) -> Result<(), String> {
        let session_json = serde_json::to_string_pretty(state)
            .map_err(|error| format!("failed to serialize session state: {error}"))?;
        fs::write(self.session_json_path(), session_json)
            .map_err(|error| format!("failed to write session.json: {error}"))?;
        write_messages_jsonl(&self.all_messages_path(), &state.all_messages)?;
        write_messages_jsonl(&self.current_messages_path(), &state.current_messages)?;
        Ok(())
    }

    fn session_json_path(&self) -> PathBuf {
        self.dir.join("session.json")
    }

    fn all_messages_path(&self) -> PathBuf {
        self.dir.join("all_messages.jsonl")
    }

    fn current_messages_path(&self) -> PathBuf {
        self.dir.join("current_messages.jsonl")
    }
}

fn default_session_root() -> Result<PathBuf, String> {
    match std::env::var_os("STELLACLAW_SESSION_ROOT") {
        Some(root) => Ok(PathBuf::from(root)),
        None => std::env::current_dir().map_err(|error| format!("failed to resolve cwd: {error}")),
    }
}

fn write_messages_jsonl(path: &Path, messages: &[ChatMessage]) -> Result<(), String> {
    let mut file = File::create(path)
        .map_err(|error| format!("failed to create {}: {error}", path.display()))?;
    for message in messages {
        let line = serde_json::to_string(message)
            .map_err(|error| format!("failed to serialize message: {error}"))?;
        writeln!(file, "{line}")
            .map_err(|error| format!("failed to write {}: {error}", path.display()))?;
    }
    file.flush()
        .map_err(|error| format!("failed to flush {}: {error}", path.display()))
}

fn sanitize_persisted_messages(messages: &mut [ChatMessage]) {
    for message in messages {
        for item in &mut message.data {
            let ChatMessageItem::ToolResult(tool_result) = item else {
                continue;
            };
            let Some(context) = tool_result.result.context.as_mut() else {
                continue;
            };
            let (text, truncated) =
                truncate_context_text(&context.text, MAX_PERSISTED_TOOL_CONTEXT_CHARS);
            if truncated {
                context.text = text;
            }
        }
    }
}

fn truncate_context_text(value: &str, max_chars: usize) -> (String, bool) {
    let total_chars = value.chars().count();
    if total_chars <= max_chars {
        return (value.to_string(), false);
    }
    if max_chars == 0 {
        return (String::new(), true);
    }

    let marker_template = format!("\n...<{total_chars} chars truncated>...\n");
    let marker_chars = marker_template.chars().count().min(max_chars);
    if marker_chars >= max_chars {
        return (value.chars().take(max_chars).collect(), true);
    }

    let available = max_chars - marker_chars;
    let head_chars = available / 2;
    let tail_chars = available - head_chars;
    let omitted = total_chars.saturating_sub(head_chars + tail_chars);
    let marker = format!("\n...<{omitted} chars truncated>...\n");
    let head = value.chars().take(head_chars).collect::<String>();
    let tail = value
        .chars()
        .rev()
        .take(tail_chars)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect::<String>();
    (format!("{head}{marker}{tail}"), true)
}

fn sanitize_session_id(session_id: &str) -> String {
    let safe = session_id
        .chars()
        .map(|ch| match ch {
            'a'..='z' | 'A'..='Z' | '0'..='9' | '_' | '-' | '.' => ch,
            _ => '_',
        })
        .collect::<String>();
    if safe.trim_matches('_').is_empty() || safe == "." || safe == ".." {
        "session".to_string()
    } else {
        safe
    }
}

#[cfg(test)]
mod tests {
    use std::time::{SystemTime, UNIX_EPOCH};

    use super::*;
    use crate::session_actor::{
        ChatMessageItem, ChatRole, ContextItem, SessionType, ToolResultContent, ToolResultItem,
    };

    fn temp_root() -> PathBuf {
        let id = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!("stellaclaw_session_state_{id}"))
    }

    #[test]
    fn saves_and_loads_session_state_and_jsonl() {
        let root = temp_root();
        let store = SessionStateStore::open_under(&root, "session/1").expect("store opens");
        let message = ChatMessage::new(
            ChatRole::User,
            vec![ChatMessageItem::Context(ContextItem {
                text: "hello".to_string(),
            })],
        );
        let state = SessionActorPersistedState {
            version: 1,
            initial: SessionInitial::new("session/1", SessionType::Foreground),
            all_messages: vec![message.clone()],
            current_messages: vec![message],
            next_turn_id: 2,
            next_batch_id: 1,
            runtime_metadata_state: RuntimeMetadataState::default(),
        };

        store.save(&state).expect("state saves");
        let loaded = store.load().expect("state loads").expect("state exists");

        assert_eq!(loaded.next_turn_id, 2);
        assert!(root
            .join(".stellaclaw/log/session_1/all_messages.jsonl")
            .exists());
        assert!(root
            .join(".stellaclaw/log/session_1/current_messages.jsonl")
            .exists());
    }

    #[test]
    fn load_sanitizes_persisted_large_tool_results() {
        let root = temp_root();
        let store = SessionStateStore::open_under(&root, "session/large").expect("store opens");
        let large_content = "x".repeat(MAX_PERSISTED_TOOL_CONTEXT_CHARS + 10_000);
        let message = ChatMessage::new(
            ChatRole::Assistant,
            vec![ChatMessageItem::ToolResult(ToolResultItem {
                tool_call_id: "call_1".to_string(),
                tool_name: "any_tool".to_string(),
                result: ToolResultContent {
                    context: Some(ContextItem {
                        text: large_content,
                    }),
                    file: None,
                },
            })],
        );
        let state = SessionActorPersistedState {
            version: 1,
            initial: SessionInitial::new("session/large", SessionType::Foreground),
            all_messages: vec![message.clone()],
            current_messages: vec![message],
            next_turn_id: 2,
            next_batch_id: 1,
            runtime_metadata_state: RuntimeMetadataState::default(),
        };

        store.save(&state).expect("state saves");
        let loaded = store.load().expect("state loads").expect("state exists");
        let ChatMessageItem::ToolResult(result) = &loaded.current_messages[0].data[0] else {
            panic!("expected tool result");
        };
        let text = &result.result.context.as_ref().expect("context").text;

        assert!(text.contains("chars truncated"));
        assert!(text.len() <= MAX_PERSISTED_TOOL_CONTEXT_CHARS);
    }
}
