use crate::domain::ShowOptions;
use agent_frame::{ChatMessage, SessionEvent, content_item_text};
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};

static TRANSCRIPT_LOCKS: OnceLock<Mutex<std::collections::HashMap<PathBuf, Arc<Mutex<()>>>>> =
    OnceLock::new();

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TranscriptEntry {
    pub seq: usize,
    pub ts: String,
    #[serde(rename = "type")]
    pub entry_type: TranscriptEntryType,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    #[serde(default, skip_serializing_if = "is_zero_usize")]
    pub attachment_count: usize,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub options: Option<ShowOptions>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub round: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub completion_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub total_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub assistant_message: Option<ChatMessage>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_len: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub errored: Option<bool>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tokens_before: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tokens_after: Option<usize>,
}

fn is_zero_usize(value: &usize) -> bool {
    *value == 0
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum TranscriptEntryType {
    UserMessage,
    AssistantMessage,
    ModelCall,
    ToolResult,
    Compaction,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TranscriptEntrySkeleton {
    pub seq: usize,
    pub ts: String,
    #[serde(rename = "type")]
    pub entry_type: TranscriptEntryType,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    #[serde(default, skip_serializing_if = "is_zero_usize")]
    pub attachment_count: usize,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub options: Option<ShowOptions>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub round: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub completion_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub total_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub assistant_text_preview: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user_tell_text_preview: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tool_call_names: Vec<String>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_len: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub errored: Option<bool>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tokens_before: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tokens_after: Option<usize>,
}

const ASSISTANT_PREVIEW_LIMIT: usize = 200;

impl TranscriptEntry {
    pub fn user_message(text: Option<String>, attachment_count: usize) -> Self {
        Self {
            seq: 0,
            ts: String::new(),
            entry_type: TranscriptEntryType::UserMessage,
            text,
            attachment_count,
            options: None,
            round: None,
            prompt_tokens: None,
            completion_tokens: None,
            total_tokens: None,
            assistant_message: None,
            tool_call_id: None,
            tool_name: None,
            output: None,
            output_len: None,
            errored: None,
            tokens_before: None,
            tokens_after: None,
        }
    }

    pub fn assistant_message(text: Option<String>, options: Option<ShowOptions>) -> Self {
        Self {
            seq: 0,
            ts: String::new(),
            entry_type: TranscriptEntryType::AssistantMessage,
            text,
            attachment_count: 0,
            options,
            round: None,
            prompt_tokens: None,
            completion_tokens: None,
            total_tokens: None,
            assistant_message: None,
            tool_call_id: None,
            tool_name: None,
            output: None,
            output_len: None,
            errored: None,
            tokens_before: None,
            tokens_after: None,
        }
    }

    pub fn from_session_event(event: &SessionEvent) -> Option<Self> {
        match event {
            SessionEvent::ModelCallCompleted {
                round_index,
                prompt_tokens,
                completion_tokens,
                total_tokens,
                assistant_message,
                ..
            } => Some(TranscriptEntry {
                seq: 0,
                ts: String::new(),
                entry_type: TranscriptEntryType::ModelCall,
                text: None,
                attachment_count: 0,
                options: None,
                round: Some(*round_index),
                prompt_tokens: Some(*prompt_tokens),
                completion_tokens: Some(*completion_tokens),
                total_tokens: Some(*total_tokens),
                assistant_message: assistant_message.clone(),
                tool_call_id: None,
                tool_name: None,
                output: None,
                output_len: None,
                errored: None,
                tokens_before: None,
                tokens_after: None,
            }),
            SessionEvent::ToolCallCompleted {
                round_index,
                tool_name,
                tool_call_id,
                output_len,
                errored,
                output,
                ..
            } => Some(TranscriptEntry {
                seq: 0,
                ts: String::new(),
                entry_type: TranscriptEntryType::ToolResult,
                text: None,
                attachment_count: 0,
                options: None,
                round: Some(*round_index),
                prompt_tokens: None,
                completion_tokens: None,
                total_tokens: None,
                assistant_message: None,
                tool_call_id: Some(tool_call_id.clone()),
                tool_name: Some(tool_name.clone()),
                output: output.clone(),
                output_len: Some(*output_len),
                errored: Some(*errored),
                tokens_before: None,
                tokens_after: None,
            }),
            SessionEvent::CompactionCompleted {
                compacted: true,
                estimated_tokens_before,
                estimated_tokens_after,
                ..
            }
            | SessionEvent::ToolWaitCompactionCompleted {
                compacted: true,
                estimated_tokens_before,
                estimated_tokens_after,
                ..
            } => Some(TranscriptEntry {
                seq: 0,
                ts: String::new(),
                entry_type: TranscriptEntryType::Compaction,
                text: None,
                attachment_count: 0,
                options: None,
                round: None,
                prompt_tokens: None,
                completion_tokens: None,
                total_tokens: None,
                assistant_message: None,
                tool_call_id: None,
                tool_name: None,
                output: None,
                output_len: None,
                errored: None,
                tokens_before: Some(*estimated_tokens_before),
                tokens_after: Some(*estimated_tokens_after),
            }),
            _ => None,
        }
    }

    pub fn to_skeleton(&self) -> TranscriptEntrySkeleton {
        let (assistant_text_preview, user_tell_text_preview, tool_call_names) =
            if let Some(message) = &self.assistant_message {
                let preview = extract_text_preview(message, ASSISTANT_PREVIEW_LIMIT);
                let user_tell_preview = extract_user_tell_preview(message, ASSISTANT_PREVIEW_LIMIT);
                let tool_names = message
                    .tool_calls
                    .as_ref()
                    .map(|calls| {
                        calls
                            .iter()
                            .map(|tool_call| tool_call.function.name.clone())
                            .collect()
                    })
                    .unwrap_or_default();
                (preview, user_tell_preview, tool_names)
            } else {
                (None, None, Vec::new())
            };

        TranscriptEntrySkeleton {
            seq: self.seq,
            ts: self.ts.clone(),
            entry_type: self.entry_type.clone(),
            text: self.text.clone(),
            attachment_count: self.attachment_count,
            options: self.options.clone(),
            round: self.round,
            prompt_tokens: self.prompt_tokens,
            completion_tokens: self.completion_tokens,
            total_tokens: self.total_tokens,
            assistant_text_preview,
            user_tell_text_preview,
            tool_call_names,
            tool_call_id: self.tool_call_id.clone(),
            tool_name: self.tool_name.clone(),
            output_len: self.output_len,
            errored: self.errored,
            tokens_before: self.tokens_before,
            tokens_after: self.tokens_after,
        }
    }
}

fn extract_user_tell_preview(message: &ChatMessage, limit: usize) -> Option<String> {
    let tool_calls = message.tool_calls.as_ref()?;
    for tool_call in tool_calls {
        if tool_call.function.name != "user_tell" {
            continue;
        }
        let raw_arguments = tool_call.function.arguments.as_deref()?;
        let arguments: Value = serde_json::from_str(raw_arguments).ok()?;
        let text = arguments.get("text").and_then(Value::as_str)?;
        if text.is_empty() {
            return None;
        }
        if text.len() <= limit {
            return Some(text.to_string());
        }
        let truncated = text.chars().take(limit).collect::<String>();
        return Some(format!("{truncated}..."));
    }
    None
}

fn extract_text_preview(message: &ChatMessage, limit: usize) -> Option<String> {
    let content = message.content.as_ref()?;
    let text = match content {
        Value::String(text) => text.clone(),
        Value::Array(parts) => {
            let mut combined = String::new();
            for part in parts {
                if let Some(text) = content_item_text(part) {
                    if !combined.is_empty() {
                        combined.push(' ');
                    }
                    combined.push_str(&text);
                }
            }
            combined
        }
        _ => return None,
    };
    if text.is_empty() {
        None
    } else if text.len() <= limit {
        Some(text)
    } else {
        let truncated = text.chars().take(limit).collect::<String>();
        Some(format!("{truncated}..."))
    }
}

#[derive(Debug)]
pub struct SessionTranscript {
    path: PathBuf,
    next_seq: usize,
}

impl SessionTranscript {
    pub fn open(session_root: &Path) -> Result<Self> {
        let path = session_root.join("transcript.jsonl");
        let next_seq = if path.exists() {
            count_lines(&path)?
        } else {
            0
        };
        Ok(Self { path, next_seq })
    }

    pub fn record_user_message(
        &mut self,
        text: Option<String>,
        attachment_count: usize,
    ) -> Result<TranscriptEntry> {
        let entry = TranscriptEntry::user_message(text, attachment_count);
        self.append_next(entry)
    }

    pub fn record_assistant_message(
        &mut self,
        text: Option<String>,
        options: Option<ShowOptions>,
    ) -> Result<TranscriptEntry> {
        let entry = TranscriptEntry::assistant_message(text, options);
        self.append_next(entry)
    }

    pub fn record_event(&mut self, event: &SessionEvent) -> Result<Option<TranscriptEntry>> {
        let Some(entry) = TranscriptEntry::from_session_event(event) else {
            return Ok(None);
        };
        self.append_next(entry).map(Some)
    }

    pub fn append_entry(&mut self, entry: TranscriptEntry) -> Result<TranscriptEntry> {
        self.append_next(entry)
    }

    pub fn len(&self) -> usize {
        self.next_seq
    }

    pub fn list(&self, offset: usize, limit: usize) -> Result<Vec<TranscriptEntrySkeleton>> {
        if limit == 0 || self.next_seq == 0 || offset >= self.next_seq {
            return Ok(Vec::new());
        }
        let newest_seq = self.next_seq.saturating_sub(1);
        let start_seq = newest_seq.saturating_sub(offset + limit - 1);
        let end_seq = newest_seq.saturating_sub(offset);
        let mut entries = self
            .read_range(start_seq, end_seq + 1)?
            .into_iter()
            .map(|entry| entry.to_skeleton())
            .collect::<Vec<_>>();
        entries.reverse();
        Ok(entries)
    }

    pub fn get_detail(&self, seq_start: usize, seq_end: usize) -> Result<Vec<TranscriptEntry>> {
        self.read_range(seq_start, seq_end)
    }

    fn append_next(&mut self, mut entry: TranscriptEntry) -> Result<TranscriptEntry> {
        let lock = transcript_lock_for_path(&self.path)?;
        let _guard = lock
            .lock()
            .map_err(|_| anyhow::anyhow!("transcript file lock poisoned"))?;
        self.next_seq = if self.path.exists() {
            count_lines(&self.path)?
        } else {
            0
        };
        entry.seq = self.next_seq;
        entry.ts = chrono::Utc::now().to_rfc3339();
        self.append(&entry)?;
        self.next_seq += 1;
        Ok(entry)
    }

    fn append(&self, entry: &TranscriptEntry) -> Result<()> {
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("failed to create {}", parent.display()))?;
        }
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)
            .with_context(|| format!("failed to open transcript {}", self.path.display()))?;
        let line = serde_json::to_string(entry).context("failed to serialize transcript entry")?;
        writeln!(file, "{line}")
            .with_context(|| format!("failed to write {}", self.path.display()))?;
        Ok(())
    }

    fn read_range(&self, seq_start: usize, seq_end: usize) -> Result<Vec<TranscriptEntry>> {
        if !self.path.exists() || seq_start >= seq_end {
            return Ok(Vec::new());
        }
        let file = File::open(&self.path)
            .with_context(|| format!("failed to open {}", self.path.display()))?;
        let reader = BufReader::new(file);
        let mut entries = Vec::new();
        for (line_index, line) in reader.lines().enumerate() {
            if line_index < seq_start {
                continue;
            }
            if line_index >= seq_end {
                break;
            }
            let line = line.with_context(|| {
                format!(
                    "failed to read line {} of {}",
                    line_index,
                    self.path.display()
                )
            })?;
            if line.trim().is_empty() {
                continue;
            }
            match serde_json::from_str::<TranscriptEntry>(&line) {
                Ok(entry) => entries.push(entry),
                Err(error) => {
                    tracing::warn!(
                        path = %self.path.display(),
                        line_index,
                        error = %error,
                        "skipping malformed transcript entry"
                    );
                }
            }
        }
        Ok(entries)
    }
}

fn transcript_lock_for_path(path: &Path) -> Result<Arc<Mutex<()>>> {
    let locks = TRANSCRIPT_LOCKS.get_or_init(|| Mutex::new(std::collections::HashMap::new()));
    let mut locks = locks
        .lock()
        .map_err(|_| anyhow::anyhow!("transcript lock registry poisoned"))?;
    Ok(locks
        .entry(path.to_path_buf())
        .or_insert_with(|| Arc::new(Mutex::new(())))
        .clone())
}

fn count_lines(path: &Path) -> Result<usize> {
    let file = File::open(path).with_context(|| format!("failed to open {}", path.display()))?;
    let reader = BufReader::new(file);
    let mut count = 0usize;
    for line in reader.lines() {
        let _ = line?;
        count += 1;
    }
    Ok(count)
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_frame::message::{FunctionCall, ToolCall};
    use tempfile::TempDir;

    #[test]
    fn transcript_append_and_list() {
        let temp_dir = TempDir::new().unwrap();
        let mut transcript = SessionTranscript::open(temp_dir.path()).unwrap();
        transcript
            .record_user_message(Some("hello".to_string()), 0)
            .unwrap();
        transcript
            .record_event(&SessionEvent::ModelCallCompleted {
                round_index: 0,
                tool_call_count: 0,
                api_request_id: None,
                request_cache_control_type: None,
                request_cache_control_ttl: None,
                request_has_cache_breakpoint: false,
                request_cache_breakpoint_count: 0,
                prompt_tokens: 10,
                completion_tokens: 5,
                total_tokens: 15,
                cache_hit_tokens: 0,
                cache_miss_tokens: 10,
                cache_read_tokens: 0,
                cache_write_tokens: 0,
                assistant_message: Some(ChatMessage::text("assistant", "hi")),
            })
            .unwrap();

        let list = transcript.list(0, 10).unwrap();
        assert_eq!(list.len(), 2);
        assert_eq!(list[0].entry_type, TranscriptEntryType::ModelCall);
        assert_eq!(list[0].assistant_text_preview.as_deref(), Some("hi"));
        assert_eq!(list[1].entry_type, TranscriptEntryType::UserMessage);

        let detail = transcript.get_detail(0, 2).unwrap();
        assert_eq!(detail.len(), 2);
        assert_eq!(detail[0].text.as_deref(), Some("hello"));
        assert!(detail[1].assistant_message.is_some());
    }

    #[test]
    fn transcript_reopen_preserves_seq() {
        let temp_dir = TempDir::new().unwrap();
        {
            let mut transcript = SessionTranscript::open(temp_dir.path()).unwrap();
            transcript
                .record_user_message(Some("one".to_string()), 0)
                .unwrap();
        }
        let mut transcript = SessionTranscript::open(temp_dir.path()).unwrap();
        let entry = transcript
            .record_user_message(Some("two".to_string()), 0)
            .unwrap();
        assert_eq!(entry.seq, 1);
    }

    #[test]
    fn transcript_concurrent_appends_allocate_unique_seq() {
        let temp_dir = TempDir::new().unwrap();
        let root = temp_dir.path().to_path_buf();
        let writers = (0..16)
            .map(|index| {
                let root = root.clone();
                std::thread::spawn(move || {
                    let mut transcript = SessionTranscript::open(&root).unwrap();
                    transcript
                        .record_user_message(Some(format!("message-{index}")), 0)
                        .unwrap();
                })
            })
            .collect::<Vec<_>>();
        for writer in writers {
            writer.join().unwrap();
        }

        let transcript = SessionTranscript::open(temp_dir.path()).unwrap();
        let mut seqs = transcript
            .get_detail(0, 16)
            .unwrap()
            .into_iter()
            .map(|entry| entry.seq)
            .collect::<Vec<_>>();
        seqs.sort_unstable();
        assert_eq!(seqs, (0..16).collect::<Vec<_>>());
    }

    #[test]
    fn assistant_message_skeleton_preserves_options_without_detail_payloads() {
        let temp_dir = TempDir::new().unwrap();
        let mut transcript = SessionTranscript::open(temp_dir.path()).unwrap();
        transcript
            .record_assistant_message(
                Some("Choose a model".to_string()),
                Some(ShowOptions {
                    prompt: "Choose".to_string(),
                    options: vec![crate::domain::ShowOption {
                        label: "gpt54".to_string(),
                        value: "/agent gpt54".to_string(),
                    }],
                    one_time: true,
                }),
            )
            .unwrap();

        let list = transcript.list(0, 10).unwrap();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].entry_type, TranscriptEntryType::AssistantMessage);
        assert_eq!(list[0].text.as_deref(), Some("Choose a model"));
        assert_eq!(
            list[0].options.as_ref().unwrap().options[0].value,
            "/agent gpt54"
        );
    }

    #[test]
    fn model_call_skeleton_extracts_user_tell_text() {
        let temp_dir = TempDir::new().unwrap();
        let mut transcript = SessionTranscript::open(temp_dir.path()).unwrap();
        transcript
            .record_event(&SessionEvent::ModelCallCompleted {
                round_index: 0,
                tool_call_count: 1,
                api_request_id: None,
                request_cache_control_type: None,
                request_cache_control_ttl: None,
                request_has_cache_breakpoint: false,
                request_cache_breakpoint_count: 0,
                prompt_tokens: 10,
                completion_tokens: 5,
                total_tokens: 15,
                cache_hit_tokens: 0,
                cache_miss_tokens: 10,
                cache_read_tokens: 0,
                cache_write_tokens: 0,
                assistant_message: Some(ChatMessage {
                    role: "assistant".to_string(),
                    content: None,
                    reasoning: None,
                    tool_calls: Some(vec![ToolCall {
                        id: "call-1".to_string(),
                        kind: "function".to_string(),
                        function: FunctionCall {
                            name: "user_tell".to_string(),
                            arguments: Some(r#"{"text":"正在处理图片"}"#.to_string()),
                        },
                    }]),
                    name: None,
                    tool_call_id: None,
                }),
            })
            .unwrap();

        let list = transcript.list(0, 10).unwrap();
        assert_eq!(
            list[0].user_tell_text_preview.as_deref(),
            Some("正在处理图片")
        );
        assert_eq!(list[0].tool_call_names, vec!["user_tell"]);
    }
}
