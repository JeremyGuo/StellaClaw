use crate::config::{AgentConfig, MemorySystem};
use crate::llm::{ChatCompletionSession, TokenUsage, create_chat_completion};
use crate::message::{ChatMessage, ToolCall};
use crate::tooling::Tool;
use crate::tooling::active_runtime_state_summary;
use anyhow::{Context, Result, anyhow};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use tiktoken_rs::o200k_base_singleton;

pub const COMPACTION_MARKER: &str = "[AgentFrame Context Compression]";

#[derive(Clone, Debug, Default)]
pub struct ContextCompactionReport {
    pub messages: Vec<ChatMessage>,
    pub compacted_messages: Vec<ChatMessage>,
    pub usage: TokenUsage,
    pub compacted: bool,
    pub estimated_tokens_before: usize,
    pub estimated_tokens_after: usize,
    pub token_limit: usize,
    pub structured_output: Option<StructuredCompactionOutput>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct StructuredCompactionRefs {
    #[serde(default)]
    pub paths: Vec<String>,
    #[serde(default)]
    pub commands: Vec<String>,
    #[serde(default)]
    pub errors: Vec<String>,
    #[serde(default)]
    pub urls: Vec<String>,
    #[serde(default)]
    pub ids: Vec<String>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct StructuredCompactionMemoryHint {
    #[serde(default)]
    pub group: String,
    #[serde(default)]
    pub conclusions: Vec<String>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct StructuredCompactionOutput {
    #[serde(default)]
    pub old_summary: String,
    #[serde(default)]
    pub new_summary: String,
    #[serde(default)]
    pub keywords: Vec<String>,
    #[serde(default)]
    pub important_refs: StructuredCompactionRefs,
    #[serde(default)]
    pub memory_hints: Vec<StructuredCompactionMemoryHint>,
    #[serde(default)]
    pub next_step: String,
}

fn content_to_text(content: &Option<Value>) -> String {
    match content {
        Some(Value::String(text)) => text.clone(),
        Some(Value::Array(items)) => items
            .iter()
            .filter_map(|item| {
                let object = item.as_object()?;
                let item_type = object.get("type")?.as_str()?;
                match item_type {
                    "text" | "input_text" | "output_text" => {
                        object.get("text")?.as_str().map(ToOwned::to_owned)
                    }
                    "input_image" | "image_url" => object
                        .get("image_url")
                        .and_then(|value| match value {
                            Value::String(url) => Some(url.clone()),
                            Value::Object(map) => map
                                .get("url")
                                .and_then(Value::as_str)
                                .map(ToOwned::to_owned),
                            _ => None,
                        })
                        .map(|value| format!("[image] {}", value)),
                    "file" | "input_file" => {
                        let file_value = if item_type == "file" {
                            object.get("file")?
                        } else {
                            item
                        };
                        let filename = file_value
                            .get("filename")
                            .and_then(Value::as_str)
                            .unwrap_or("document");
                        Some(format!("[file] {}", filename))
                    }
                    "input_audio" => {
                        let format = object
                            .get("input_audio")
                            .and_then(Value::as_object)
                            .and_then(|audio| audio.get("format"))
                            .and_then(Value::as_str)
                            .unwrap_or("audio");
                        Some(format!("[audio] {}", format))
                    }
                    _ => None,
                }
            })
            .collect::<Vec<_>>()
            .join("\n"),
        Some(other) => serde_json::to_string(other).unwrap_or_default(),
        None => String::new(),
    }
}

fn compaction_text_item(text: impl Into<String>) -> Value {
    json!({
        "type": "text",
        "text": text.into(),
    })
}

fn image_url_from_content_item(object: &Map<String, Value>) -> Option<&str> {
    object.get("image_url").and_then(|value| match value {
        Value::String(url) => Some(url.as_str()),
        Value::Object(map) => map.get("url").and_then(Value::as_str),
        _ => None,
    })
}

fn image_placeholder_for_compaction(image_url: Option<&str>) -> String {
    let Some(image_url) = image_url else {
        return "[image omitted during context compression]".to_string();
    };
    if image_url
        .get(.."data:".len())
        .is_some_and(|prefix| prefix.eq_ignore_ascii_case("data:"))
    {
        let media_type = image_url
            .strip_prefix("data:")
            .and_then(|rest| rest.split(';').next())
            .filter(|value| !value.trim().is_empty())
            .unwrap_or("image");
        format!("[image omitted during context compression: inline {media_type}]")
    } else {
        format!("[image omitted during context compression: {image_url}]")
    }
}

fn file_placeholder_for_compaction(item_type: &str, item: &Value) -> String {
    let file_value = if item_type == "file" {
        item.get("file").unwrap_or(item)
    } else {
        item
    };
    let filename = file_value
        .get("filename")
        .and_then(Value::as_str)
        .unwrap_or("document");
    format!("[file omitted during context compression: {filename}]")
}

fn audio_placeholder_for_compaction(object: &Map<String, Value>) -> String {
    let format = object
        .get("input_audio")
        .and_then(Value::as_object)
        .and_then(|audio| audio.get("format"))
        .and_then(Value::as_str)
        .unwrap_or("audio");
    format!("[audio omitted during context compression: {format}]")
}

fn sanitize_content_item_for_compaction_request(item: &Value) -> Value {
    let Some(object) = item.as_object() else {
        return item.clone();
    };
    let Some(item_type) = object.get("type").and_then(Value::as_str) else {
        return item.clone();
    };
    match item_type {
        "input_image" | "image_url" => compaction_text_item(image_placeholder_for_compaction(
            image_url_from_content_item(object),
        )),
        "file" | "input_file" => {
            compaction_text_item(file_placeholder_for_compaction(item_type, item))
        }
        "input_audio" => compaction_text_item(audio_placeholder_for_compaction(object)),
        _ => item.clone(),
    }
}

fn sanitize_content_for_compaction_request(content: &Option<Value>) -> Option<Value> {
    match content {
        Some(Value::Array(items)) => Some(Value::Array(
            items
                .iter()
                .map(sanitize_content_item_for_compaction_request)
                .collect(),
        )),
        _ => content.clone(),
    }
}

fn sanitized_content_text_for_compaction_request(content: &Option<Value>) -> String {
    content_to_text(&sanitize_content_for_compaction_request(content))
}

fn render_tool_call_for_compaction_request(tool_call: &ToolCall) -> String {
    let arguments = tool_call.function.arguments.as_deref().unwrap_or_default();
    if arguments.trim().is_empty() {
        format!(
            "[tool call: {} id={}]",
            tool_call.function.name, tool_call.id
        )
    } else {
        format!(
            "[tool call: {} id={}]\n{}",
            tool_call.function.name, tool_call.id, arguments
        )
    }
}

fn render_tool_result_for_compaction_request(message: &ChatMessage) -> String {
    let mut label = message.name.as_deref().unwrap_or("tool").to_string();
    if let Some(tool_call_id) = message.tool_call_id.as_deref() {
        label.push_str(" id=");
        label.push_str(tool_call_id);
    }
    let content = sanitized_content_text_for_compaction_request(&message.content);
    if content.trim().is_empty() {
        format!("[tool result: {label}]")
    } else {
        format!("[tool result: {label}]\n{content}")
    }
}

fn sanitize_messages_for_compaction_request(messages: &[ChatMessage]) -> Vec<ChatMessage> {
    messages
        .iter()
        .map(|message| {
            let role = if message.role == "tool" {
                "user".to_string()
            } else {
                message.role.clone()
            };
            let content = if message.role == "tool" {
                render_tool_result_for_compaction_request(message)
            } else {
                let mut parts = Vec::new();
                let content = sanitized_content_text_for_compaction_request(&message.content);
                if !content.trim().is_empty() {
                    parts.push(content);
                }
                if let Some(tool_calls) = &message.tool_calls {
                    parts.extend(
                        tool_calls
                            .iter()
                            .map(render_tool_call_for_compaction_request),
                    );
                }
                parts.join("\n\n")
            };
            ChatMessage {
                role,
                content: Some(Value::String(content)),
                name: None,
                tool_call_id: None,
                tool_calls: None,
            }
        })
        .collect()
}

fn estimate_text_tokens(text: &str) -> usize {
    if text.is_empty() {
        return 0;
    }
    o200k_base_singleton()
        .encode_with_special_tokens(text)
        .len()
        .max(1)
}

// Mirrors Codex's approach: do not estimate inline base64 image payloads as
// raw text. Replace them with a fixed per-image byte estimate before turning
// bytes into tokens. 7,373 bytes is approximately 1,844 tokens at 4 bytes/token.
const RESIZED_IMAGE_BYTES_ESTIMATE: usize = 7_373;
const INLINE_FILE_BYTES_ESTIMATE: usize = 12_000;
const INLINE_AUDIO_BYTES_ESTIMATE: usize = 16_000;

fn parse_base64_image_data_url(url: &str) -> Option<&str> {
    if !url
        .get(.."data:".len())
        .is_some_and(|prefix| prefix.eq_ignore_ascii_case("data:"))
    {
        return None;
    }
    let comma_index = url.find(',')?;
    let metadata = &url[..comma_index];
    let payload = &url[comma_index + 1..];
    let metadata_without_scheme = &metadata["data:".len()..];
    let mut metadata_parts = metadata_without_scheme.split(';');
    let mime_type = metadata_parts.next().unwrap_or_default();
    let has_base64_marker = metadata_parts.any(|part| part.eq_ignore_ascii_case("base64"));
    if !mime_type
        .get(.."image/".len())
        .is_some_and(|prefix| prefix.eq_ignore_ascii_case("image/"))
    {
        return None;
    }
    if !has_base64_marker {
        return None;
    }
    Some(payload)
}

fn estimate_payload_bytes_as_tokens(bytes: usize) -> usize {
    bytes.div_ceil(4).max(1)
}

fn replace_inline_payloads_for_token_estimate(content: Option<&mut Value>) -> usize {
    let Some(Value::Array(items)) = content else {
        return 0;
    };

    let mut extra_tokens = 0usize;
    for item in items {
        let Some(object) = item.as_object_mut() else {
            continue;
        };
        let kind = object
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        if (kind.as_str() == "input_image" || kind.as_str() == "image_url")
            && let Some(image_url) = object.get_mut("image_url")
        {
            let url = match image_url {
                Value::String(url) => Some(url.as_str()),
                Value::Object(map) => map.get("url").and_then(Value::as_str),
                _ => None,
            };
            if url.and_then(parse_base64_image_data_url).is_some() {
                match image_url {
                    Value::String(url) => {
                        *url = "[inline image payload omitted for token estimate]".to_string();
                    }
                    Value::Object(map) => {
                        map.insert(
                            "url".to_string(),
                            Value::String(
                                "[inline image payload omitted for token estimate]".to_string(),
                            ),
                        );
                    }
                    _ => {}
                }
                extra_tokens = extra_tokens.saturating_add(estimate_payload_bytes_as_tokens(
                    RESIZED_IMAGE_BYTES_ESTIMATE,
                ));
            }
            continue;
        }

        if kind == "file"
            && let Some(file_object) = object.get_mut("file").and_then(Value::as_object_mut)
            && file_object
                .get("file_data")
                .and_then(Value::as_str)
                .is_some()
        {
            file_object.insert(
                "file_data".to_string(),
                Value::String("[inline file payload omitted for token estimate]".to_string()),
            );
            extra_tokens = extra_tokens
                .saturating_add(estimate_payload_bytes_as_tokens(INLINE_FILE_BYTES_ESTIMATE));
            continue;
        }

        if kind == "input_file" && object.get("file_data").and_then(Value::as_str).is_some() {
            object.insert(
                "file_data".to_string(),
                Value::String("[inline file payload omitted for token estimate]".to_string()),
            );
            extra_tokens = extra_tokens
                .saturating_add(estimate_payload_bytes_as_tokens(INLINE_FILE_BYTES_ESTIMATE));
            continue;
        }

        if kind == "input_audio"
            && let Some(audio) = object.get_mut("input_audio").and_then(Value::as_object_mut)
            && audio.get("data").and_then(Value::as_str).is_some()
        {
            audio.insert(
                "data".to_string(),
                Value::String("[inline audio payload omitted for token estimate]".to_string()),
            );
            extra_tokens = extra_tokens.saturating_add(estimate_payload_bytes_as_tokens(
                INLINE_AUDIO_BYTES_ESTIMATE,
            ));
        }
    }

    extra_tokens
}

fn estimate_message_tokens(message: &ChatMessage) -> usize {
    let mut value = serde_json::to_value(message).unwrap_or_default();
    let inline_payload_tokens = replace_inline_payloads_for_token_estimate(
        value
            .as_object_mut()
            .and_then(|object| object.get_mut("content")),
    );
    let serialized = serde_json::to_string(&value).unwrap_or_default();
    estimate_text_tokens(&serialized) + inline_payload_tokens + 6
}

pub fn estimate_session_tokens(
    messages: &[ChatMessage],
    tools: &[Tool],
    pending_user_prompt: &str,
) -> usize {
    let message_tokens = messages.iter().map(estimate_message_tokens).sum::<usize>();
    let tool_tokens = if tools.is_empty() {
        0
    } else {
        estimate_text_tokens(
            &serde_json::to_string(&tools.iter().map(Tool::as_openai_tool).collect::<Vec<_>>())
                .unwrap_or_default(),
        )
    };
    message_tokens + tool_tokens + estimate_text_tokens(pending_user_prompt)
}

fn auto_compact_token_limit(config: &AgentConfig) -> usize {
    config
        .context_compaction
        .token_limit_override
        .unwrap_or_else(|| {
            (config.upstream.context_window_tokens as f64 * config.context_compaction.trigger_ratio)
                .floor() as usize
        })
}

fn recent_fidelity_token_budget(config: &AgentConfig) -> usize {
    ((config.upstream.context_window_tokens as f64
        * config.context_compaction.recent_fidelity_target_ratio)
        .floor() as usize)
        .max(1_024)
}

#[cfg(test)]
fn extract_previous_compaction_summary(message: &ChatMessage) -> Option<String> {
    if message.role != "assistant" {
        return None;
    }
    let content = content_to_text(&message.content);
    let marker_index = content.find(COMPACTION_MARKER)?;
    let after_marker = content[marker_index + COMPACTION_MARKER.len()..].trim();
    if after_marker.is_empty() {
        return None;
    }
    let summary_start = after_marker.find("## Old Summary").or_else(|| {
        after_marker.find("Older conversation history has been compacted into the summary below.")
    });
    let summary = summary_start
        .map(|index| &after_marker[index..])
        .unwrap_or(after_marker)
        .trim();
    if summary.is_empty() {
        None
    } else {
        Some(summary.to_string())
    }
}

#[cfg(test)]
fn split_compaction_inputs(
    messages_to_summarize: &[ChatMessage],
) -> (Option<String>, Vec<ChatMessage>) {
    let previous_summary_index = messages_to_summarize
        .iter()
        .rposition(|message| extract_previous_compaction_summary(message).is_some());
    let previous_old_summary = previous_summary_index
        .and_then(|index| extract_previous_compaction_summary(&messages_to_summarize[index]));
    let new_messages = messages_to_summarize
        .iter()
        .enumerate()
        .filter_map(|(index, message)| {
            if previous_summary_index == Some(index) {
                None
            } else {
                Some(message.clone())
            }
        })
        .collect::<Vec<_>>();
    (previous_old_summary, new_messages)
}

fn build_summary_request(
    messages: &[ChatMessage],
    preserved_recent_count: usize,
) -> Vec<ChatMessage> {
    let mut request_messages = messages.to_vec();
    request_messages.push(ChatMessage::text(
        "user",
        format!(
            "Compress the older conversation history in this same transcript.\n\nReturn a JSON object with exactly these top-level keys:\n- old_summary\n- new_summary\n- keywords\n- important_refs\n- memory_hints\n- next_step\n\nMeaning:\n- old_summary: further compress the previously compacted old history. If there is no previous compacted old history, return an empty string.\n- new_summary: summarize only the older conversation history that appears before the most recent {preserved_recent_count} message(s) immediately preceding this request.\n- keywords: short retrieval keywords.\n- important_refs: object with arrays paths, commands, errors, urls, ids.\n- memory_hints: array of {{ group, conclusions }} for higher-level memory grouping.\n- next_step: one short recommended next step.\n\nRules:\n- keep summaries concise and factual\n- redact long secrets; mention that a secret or cookie was provided without copying the full value\n- do not invent details\n- do not restate or summarize shared context content from the system prompt, skills metadata, USER, IDENTITY, remote workpath host/path/description entries, or remote AGENTS.md content\n- ignore transient runtime system messages that only announce refreshed shared profiles, runtime skill updates, or available model catalog changes; those are reconstructed separately and should not be preserved in the compaction summary\n- the most recent {preserved_recent_count} message(s) immediately preceding this request are preserved separately as the recent high-fidelity zone; do not summarize them except for a tiny pointer when continuity absolutely requires it\n- if an earlier assistant message beginning with {COMPACTION_MARKER} exists in the older history, further compress its Old Summary into old_summary\n- if an older start-type task is still active, preserve the continuation-critical identifier needed to resume it safely, especially exec_id, download_id, or subagent id, plus any path, cwd, or url needed to continue unfinished work\n- if that start-type task has already finished or is no longer active, you do not need to preserve its identifier just because it appeared earlier\n- intermediate tool calls and tool results do not need to be reproduced step by step; summarize their outcome compactly unless a still-active task needs a specific identifier or reference to continue safely\n- old_summary and new_summary should each be markdown bullet summaries\n- if a field has no content, use an empty string or empty array\n- return JSON only"
        ),
    ));
    request_messages
}

fn build_claude_code_summary_request(
    messages: &[ChatMessage],
    preserved_recent_count: usize,
) -> Vec<ChatMessage> {
    let mut request_messages = messages.to_vec();
    request_messages.push(ChatMessage::text(
        "user",
        format!(
            "Provide a detailed but concise summary of our conversation above.\n\nFocus on information that would be helpful for continuing the conversation, including what we did, what we're doing, which files we're working on, and what we're going to do next.\n\nRules:\n- keep the summary factual, compact, and continuation-oriented\n- redact long secrets; mention that a secret or cookie was provided without copying the full value\n- do not invent details\n- do not restate shared context content from the system prompt, skills metadata, USER, IDENTITY, PARTCLAW, remote workpath host/path/description entries, or remote AGENTS.md content\n- ignore transient runtime system messages that only announce refreshed shared profiles, runtime skill updates, or available model catalog changes; those are reconstructed separately and should not be preserved in the summary\n- the most recent {preserved_recent_count} message(s) immediately preceding this request are preserved separately as the recent high-fidelity zone; do not summarize them except for a tiny pointer when continuity absolutely requires it\n- if an unfinished long-running task still matters, preserve the continuation-critical identifier needed to resume or write back safely, especially exec_id, download_id, file_download id, subagent id, plus any path, cwd, url, or pending destination needed to finish the task\n- never drop the identifiers for background work that is still running or waiting to be observed; losing those ids can prevent safe completion\n- if a task is already finished or no longer relevant, you do not need to preserve its identifier just because it appeared earlier\n- intermediate tool calls and tool results do not need to be reproduced step by step; summarize the outcome compactly unless an unfinished task needs exact references\n- return plain text only"
        ),
    ));
    request_messages
}

fn parse_structured_summary(summary_text: &str) -> Result<StructuredCompactionOutput> {
    let trimmed = summary_text.trim();
    let json_text = if trimmed.starts_with("```") {
        trimmed
            .trim_start_matches("```json")
            .trim_start_matches("```")
            .trim_end_matches("```")
            .trim()
    } else {
        trimmed
    };
    let parsed: StructuredCompactionOutput =
        serde_json::from_str(json_text).context("failed to parse structured compaction JSON")?;
    Ok(parsed)
}

fn render_structured_summary(output: &StructuredCompactionOutput) -> String {
    let mut sections = vec![COMPACTION_MARKER.to_string()];
    if !output.old_summary.trim().is_empty() {
        sections.push(format!("## Old Summary\n{}", output.old_summary.trim()));
    }
    if !output.new_summary.trim().is_empty() {
        sections.push(format!("## New Summary\n{}", output.new_summary.trim()));
    }
    if !output.next_step.trim().is_empty() {
        sections.push(format!("## Next Step\n- {}", output.next_step.trim()));
    }
    sections.join("\n\n")
}

fn render_claude_code_summary(summary_text: &str) -> String {
    format!(
        "{COMPACTION_MARKER}\n\nOlder conversation history has been compacted into the summary below.\n\n{}",
        summary_text.trim()
    )
}

enum GeneratedCompactionSummary {
    Structured(StructuredCompactionOutput),
    ClaudeCode(String),
}

fn generate_summary(
    config: &AgentConfig,
    messages: &[ChatMessage],
    preserved_recent_count: usize,
    session: Option<&mut ChatCompletionSession>,
) -> Result<(GeneratedCompactionSummary, TokenUsage)> {
    let mut extra_payload = Map::new();
    extra_payload.insert("max_completion_tokens".to_string(), Value::from(1_200_u64));
    let request_messages = match config.memory_system {
        MemorySystem::Layered => build_summary_request(messages, preserved_recent_count),
        MemorySystem::ClaudeCode => {
            build_claude_code_summary_request(messages, preserved_recent_count)
        }
    };
    let request_messages = sanitize_messages_for_compaction_request(&request_messages);
    let summary_message = create_chat_completion(
        &config.upstream,
        &request_messages,
        &[],
        Some(extra_payload),
        session,
    )?;
    let summary_text = content_to_text(&summary_message.message.content);
    if summary_text.trim().is_empty() {
        return Err(anyhow!("context compression summary came back empty"));
    }
    let generated = match config.memory_system {
        MemorySystem::Layered => {
            GeneratedCompactionSummary::Structured(parse_structured_summary(&summary_text)?)
        }
        MemorySystem::ClaudeCode => GeneratedCompactionSummary::ClaudeCode(summary_text),
    };
    Ok((generated, summary_message.usage))
}

fn find_originating_tool_use_index(
    messages: &[ChatMessage],
    search_before: usize,
    tool_call_id: &str,
) -> Option<usize> {
    messages[..search_before].iter().rposition(|message| {
        message.role == "assistant"
            && message.tool_calls.as_ref().is_some_and(|tool_calls| {
                tool_calls
                    .iter()
                    .any(|tool_call| tool_call.id == tool_call_id)
            })
    })
}

fn adjust_split_index_to_preserve_tool_context(
    messages: &[ChatMessage],
    mut split_index: usize,
) -> usize {
    while split_index < messages.len() && messages[split_index].role == "tool" {
        let Some(tool_call_id) = messages[split_index].tool_call_id.as_deref() else {
            break;
        };
        let Some(origin_index) =
            find_originating_tool_use_index(messages, split_index, tool_call_id)
        else {
            break;
        };
        split_index = origin_index;
    }
    split_index
}

fn recent_tail_start_by_token_budget(messages: &[ChatMessage], token_budget: usize) -> usize {
    if messages.is_empty() {
        return 0;
    }

    let mut start = messages.len();
    let mut total_tokens = 0usize;

    while start > 0 {
        let candidate_start =
            adjust_split_index_to_preserve_tool_context(messages, start.saturating_sub(1));
        if candidate_start >= start {
            break;
        }
        let slice_tokens = messages[candidate_start..start]
            .iter()
            .map(estimate_message_tokens)
            .sum::<usize>();
        if total_tokens + slice_tokens > token_budget && start < messages.len() {
            break;
        }
        total_tokens = total_tokens.saturating_add(slice_tokens);
        start = candidate_start;
        if total_tokens >= token_budget {
            break;
        }
    }

    start
}

fn compact_history_once(
    config: &AgentConfig,
    messages: &[ChatMessage],
    retain_recent_token_budget: usize,
    session: Option<&mut ChatCompletionSession>,
) -> Result<(
    Vec<ChatMessage>,
    TokenUsage,
    Option<StructuredCompactionOutput>,
    Vec<ChatMessage>,
)> {
    if messages.is_empty() {
        return Ok((messages.to_vec(), TokenUsage::default(), None, Vec::new()));
    }

    let has_system = messages.first().map(|message| message.role.as_str()) == Some("system");
    let system_prefix = if has_system {
        vec![ChatMessage::text("system", config.system_prompt.clone())]
    } else {
        Vec::new()
    };
    let body = if has_system { &messages[1..] } else { messages };
    if body.len() <= 1 {
        return Ok((messages.to_vec(), TokenUsage::default(), None, Vec::new()));
    }

    let split_index = recent_tail_start_by_token_budget(body, retain_recent_token_budget);
    if split_index == 0 {
        return Ok((messages.to_vec(), TokenUsage::default(), None, Vec::new()));
    }
    let messages_to_summarize = &body[..split_index];
    let recent_messages = &body[split_index..];
    let (generated_summary, usage) =
        generate_summary(config, messages, recent_messages.len(), session)?;
    let runtime_state = active_runtime_state_summary(&config.runtime_state_root)?;
    let (summary_message, structured_output) = match generated_summary {
        GeneratedCompactionSummary::Structured(structured_output) => (
            ChatMessage::text("assistant", render_structured_summary(&structured_output)),
            Some(structured_output),
        ),
        GeneratedCompactionSummary::ClaudeCode(summary_text) => (
            ChatMessage::text("assistant", render_claude_code_summary(&summary_text)),
            None,
        ),
    };
    let mut compacted = system_prefix;
    compacted.push(summary_message);
    if let Some(runtime_state) = runtime_state.filter(|value| !value.trim().is_empty()) {
        compacted.push(ChatMessage::text("system", runtime_state));
    }
    compacted.extend_from_slice(recent_messages);
    Ok((
        compacted,
        usage,
        structured_output,
        messages_to_summarize.to_vec(),
    ))
}

pub fn maybe_compact_messages(
    config: &AgentConfig,
    messages: &[ChatMessage],
    tools: &[Tool],
    pending_user_prompt: &str,
) -> Result<Vec<ChatMessage>> {
    Ok(maybe_compact_messages_with_report(config, messages, tools, pending_user_prompt)?.messages)
}

pub fn maybe_compact_messages_with_report(
    config: &AgentConfig,
    messages: &[ChatMessage],
    tools: &[Tool],
    pending_user_prompt: &str,
) -> Result<ContextCompactionReport> {
    maybe_compact_messages_with_report_with_session(
        config,
        messages,
        tools,
        pending_user_prompt,
        None,
    )
}

pub(crate) fn maybe_compact_messages_with_report_with_session(
    config: &AgentConfig,
    messages: &[ChatMessage],
    tools: &[Tool],
    pending_user_prompt: &str,
    mut session: Option<&mut ChatCompletionSession>,
) -> Result<ContextCompactionReport> {
    let estimated_tokens_before = estimate_session_tokens(messages, tools, pending_user_prompt);
    if !config.enable_context_compression {
        return Ok(ContextCompactionReport {
            messages: messages.to_vec(),
            compacted_messages: Vec::new(),
            estimated_tokens_before,
            estimated_tokens_after: estimated_tokens_before,
            token_limit: auto_compact_token_limit(config),
            ..ContextCompactionReport::default()
        });
    }
    let limit = auto_compact_token_limit(config);
    if estimated_tokens_before < limit {
        return Ok(ContextCompactionReport {
            messages: messages.to_vec(),
            compacted_messages: Vec::new(),
            estimated_tokens_before,
            estimated_tokens_after: estimated_tokens_before,
            token_limit: limit,
            ..ContextCompactionReport::default()
        });
    }

    let mut compacted = messages.to_vec();
    let mut usage = TokenUsage::default();
    let mut retain_recent_token_budget = recent_fidelity_token_budget(config);
    let mut structured_output = None;
    let mut compacted_messages = Vec::new();
    for _ in 0..3 {
        let (next, step_usage, step_structured_output, step_compacted_messages) =
            compact_history_once(
                config,
                &compacted,
                retain_recent_token_budget,
                session.as_deref_mut(),
            )?;
        if next == compacted {
            break;
        }
        usage.add_assign(&step_usage);
        compacted = next;
        if step_structured_output.is_some() {
            structured_output = step_structured_output;
            compacted_messages = step_compacted_messages;
        }
        if estimate_session_tokens(&compacted, tools, pending_user_prompt) < limit {
            break;
        }
        retain_recent_token_budget = (retain_recent_token_budget / 2).max(512);
    }
    let estimated_tokens_after = estimate_session_tokens(&compacted, tools, pending_user_prompt);
    Ok(ContextCompactionReport {
        compacted: compacted != messages,
        messages: compacted,
        compacted_messages,
        usage,
        estimated_tokens_before,
        estimated_tokens_after,
        token_limit: limit,
        structured_output,
    })
}

#[cfg(test)]
mod tests {
    use super::{
        COMPACTION_MARKER, adjust_split_index_to_preserve_tool_context, content_to_text,
        estimate_message_tokens, estimate_text_tokens, extract_previous_compaction_summary,
        find_originating_tool_use_index, parse_structured_summary,
        recent_tail_start_by_token_budget, sanitize_messages_for_compaction_request,
        split_compaction_inputs,
    };
    use crate::message::{ChatMessage, ToolCall};
    use serde_json::json;

    fn synthetic_base64_payload(len: usize) -> String {
        let alphabet = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
        (0..len)
            .map(|index| alphabet[(index * 37 + index / 7) % alphabet.len()] as char)
            .collect()
    }

    #[test]
    fn content_to_text_reads_image_url_objects() {
        let content = Some(json!([
            {
                "type": "text",
                "text": "Please inspect this."
            },
            {
                "type": "image_url",
                "image_url": {
                    "url": "data:image/png;base64,abc123"
                }
            }
        ]));

        let text = content_to_text(&content);
        assert!(text.contains("Please inspect this."));
        assert!(text.contains("[image] data:image/png;base64,abc123"));
    }

    #[test]
    fn estimate_message_tokens_discounts_inline_image_data_urls() {
        let base64_payload = synthetic_base64_payload(20_000);
        let message = ChatMessage {
            role: "user".to_string(),
            content: Some(json!([
                {
                    "type": "text",
                    "text": "Please inspect this image."
                },
                {
                    "type": "input_image",
                    "image_url": format!("data:image/png;base64,{base64_payload}")
                }
            ])),
            name: None,
            tool_call_id: None,
            tool_calls: None,
        };

        let discounted = estimate_message_tokens(&message);
        let naive = estimate_text_tokens(&serde_json::to_string(&message).unwrap()) + 6;

        assert!(naive > 10_000);
        assert!(discounted < naive / 2);
        assert!(discounted < 8_000);
    }

    #[test]
    fn estimate_message_tokens_discounts_inline_file_payloads() {
        let base64_payload = synthetic_base64_payload(32_000);
        let message = ChatMessage {
            role: "user".to_string(),
            content: Some(json!([
                {
                    "type": "text",
                    "text": "Inspect this PDF."
                },
                {
                    "type": "file",
                    "file": {
                        "file_data": base64_payload,
                        "filename": "report.pdf"
                    }
                }
            ])),
            name: None,
            tool_call_id: None,
            tool_calls: None,
        };

        let discounted = estimate_message_tokens(&message);
        let naive = estimate_text_tokens(&serde_json::to_string(&message).unwrap()) + 6;

        assert!(naive > 10_000);
        assert!(discounted < naive);
        assert!(discounted < naive.saturating_sub(2_000));
    }

    #[test]
    fn estimate_message_tokens_discounts_inline_audio_payloads() {
        let base64_payload = synthetic_base64_payload(32_000);
        let message = ChatMessage {
            role: "user".to_string(),
            content: Some(json!([
                {
                    "type": "text",
                    "text": "Inspect this audio."
                },
                {
                    "type": "input_audio",
                    "input_audio": {
                        "data": base64_payload,
                        "format": "wav"
                    }
                }
            ])),
            name: None,
            tool_call_id: None,
            tool_calls: None,
        };

        let discounted = estimate_message_tokens(&message);
        let naive = estimate_text_tokens(&serde_json::to_string(&message).unwrap()) + 6;

        assert!(naive > 10_000);
        assert!(discounted < naive);
        assert!(discounted < naive.saturating_sub(2_000));
    }

    #[test]
    fn compaction_request_replaces_inline_media_payloads_with_placeholders() {
        let image_payload = "UNIQUE_IMAGE_PAYLOAD".repeat(512);
        let file_payload = "UNIQUE_FILE_PAYLOAD".repeat(512);
        let audio_payload = "UNIQUE_AUDIO_PAYLOAD".repeat(512);
        let messages = vec![ChatMessage {
            role: "user".to_string(),
            content: Some(json!([
                {
                    "type": "text",
                    "text": "Keep the textual instruction."
                },
                {
                    "type": "input_image",
                    "image_url": format!("data:image/png;base64,{image_payload}")
                },
                {
                    "type": "image_url",
                    "image_url": {
                        "url": "https://example.com/screenshot.png"
                    }
                },
                {
                    "type": "file",
                    "file": {
                        "filename": "report.pdf",
                        "file_data": file_payload
                    }
                },
                {
                    "type": "input_audio",
                    "input_audio": {
                        "format": "wav",
                        "data": audio_payload
                    }
                }
            ])),
            name: None,
            tool_call_id: None,
            tool_calls: None,
        }];

        let sanitized = sanitize_messages_for_compaction_request(&messages);
        let serialized = serde_json::to_string(&sanitized).unwrap();

        assert!(serialized.contains("Keep the textual instruction."));
        assert!(
            serialized.contains("[image omitted during context compression: inline image/png]")
        );
        assert!(serialized.contains(
            "[image omitted during context compression: https://example.com/screenshot.png]"
        ));
        assert!(serialized.contains("[file omitted during context compression: report.pdf]"));
        assert!(serialized.contains("[audio omitted during context compression: wav]"));
        assert!(!serialized.contains("UNIQUE_IMAGE_PAYLOAD"));
        assert!(!serialized.contains("UNIQUE_FILE_PAYLOAD"));
        assert!(!serialized.contains("UNIQUE_AUDIO_PAYLOAD"));
        assert!(!serialized.contains("data:image/png;base64"));
    }

    #[test]
    fn compaction_request_renders_tool_protocol_as_plain_text() {
        let messages = vec![
            ChatMessage {
                role: "assistant".to_string(),
                content: Some(json!("I will inspect the file.")),
                name: None,
                tool_call_id: None,
                tool_calls: Some(vec![ToolCall {
                    id: "call_1".to_string(),
                    kind: "function".to_string(),
                    function: crate::message::FunctionCall {
                        name: "file_read".to_string(),
                        arguments: Some("{\"file_path\":\"src/lib.rs\"}".to_string()),
                    },
                }]),
            },
            ChatMessage::tool_output("call_1", "file_read", "{\"content\":\"hello\"}"),
        ];

        let sanitized = sanitize_messages_for_compaction_request(&messages);

        assert_eq!(sanitized[0].role, "assistant");
        assert_eq!(sanitized[0].tool_calls, None);
        assert_eq!(sanitized[0].tool_call_id, None);
        assert_eq!(sanitized[0].name, None);
        assert_eq!(sanitized[1].role, "user");
        assert_eq!(sanitized[1].tool_calls, None);
        assert_eq!(sanitized[1].tool_call_id, None);
        assert_eq!(sanitized[1].name, None);

        let serialized = serde_json::to_string(&sanitized).unwrap();
        assert!(serialized.contains("I will inspect the file."));
        assert!(serialized.contains("[tool call: file_read id=call_1]"));
        assert!(serialized.contains("{\\\"file_path\\\":\\\"src/lib.rs\\\"}"));
        assert!(serialized.contains("[tool result: file_read id=call_1]"));
        assert!(serialized.contains("{\\\"content\\\":\\\"hello\\\"}"));
        assert!(!serialized.contains("\"tool_calls\""));
        assert!(!serialized.contains("\"tool_call_id\""));
        assert!(!serialized.contains("\"name\""));
        assert!(!serialized.contains("\"role\":\"tool\""));
    }

    #[test]
    fn finds_originating_tool_use_for_tool_result() {
        let messages = vec![
            ChatMessage {
                role: "assistant".to_string(),
                content: None,
                name: None,
                tool_call_id: None,
                tool_calls: Some(vec![ToolCall {
                    id: "call_1".to_string(),
                    kind: "function".to_string(),
                    function: crate::message::FunctionCall {
                        name: "file_read".to_string(),
                        arguments: Some("{}".to_string()),
                    },
                }]),
            },
            ChatMessage::tool_output("call_1", "file_read", "{\"ok\":true}"),
            ChatMessage::text("assistant", "done"),
        ];

        assert_eq!(
            find_originating_tool_use_index(&messages, 1, "call_1"),
            Some(0)
        );
    }

    #[test]
    fn split_index_moves_back_to_preserve_tool_transaction() {
        let messages = vec![
            ChatMessage::text("user", "start"),
            ChatMessage {
                role: "assistant".to_string(),
                content: None,
                name: None,
                tool_call_id: None,
                tool_calls: Some(vec![ToolCall {
                    id: "call_1".to_string(),
                    kind: "function".to_string(),
                    function: crate::message::FunctionCall {
                        name: "file_read".to_string(),
                        arguments: Some("{}".to_string()),
                    },
                }]),
            },
            ChatMessage::tool_output("call_1", "file_read", "{\"ok\":true}"),
            ChatMessage::text("assistant", "done"),
        ];

        assert_eq!(adjust_split_index_to_preserve_tool_context(&messages, 2), 1);
    }

    #[test]
    fn recent_tail_budget_moves_back_to_keep_tool_pair_together() {
        let messages = vec![
            ChatMessage::text("user", "older context"),
            ChatMessage::text("assistant", "prelude"),
            ChatMessage {
                role: "assistant".to_string(),
                content: None,
                name: None,
                tool_call_id: None,
                tool_calls: Some(vec![ToolCall {
                    id: "call_1".to_string(),
                    kind: "function".to_string(),
                    function: crate::message::FunctionCall {
                        name: "exec_wait".to_string(),
                        arguments: Some("{\"exec_id\":\"abc\"}".to_string()),
                    },
                }]),
            },
            ChatMessage::tool_output(
                "call_1",
                "exec_wait",
                "{\"exec_id\":\"abc\",\"status\":\"running\"}",
            ),
        ];

        let budget = estimate_message_tokens(&messages[3]);
        let start = recent_tail_start_by_token_budget(&messages, budget);
        assert_eq!(start, 2);
    }

    #[test]
    fn extracts_previous_compaction_summary_from_marker_message() {
        let message = ChatMessage::text(
            "assistant",
            format!("{COMPACTION_MARKER}\n\n## Old Summary\n- older\n\n## New Summary\n- newer"),
        );

        let summary = extract_previous_compaction_summary(&message).unwrap();
        assert!(summary.contains("## Old Summary"));
        assert!(summary.contains("- older"));
    }

    #[test]
    fn splits_compaction_inputs_into_old_and_new_sections() {
        let prior_summary = ChatMessage::text(
            "assistant",
            format!("{COMPACTION_MARKER}\n\n## Old Summary\n- old\n\n## New Summary\n- new"),
        );
        let next_user = ChatMessage::text("user", "continue");
        let next_assistant = ChatMessage::text("assistant", "working");

        let (old_summary, new_messages) =
            split_compaction_inputs(&[prior_summary, next_user.clone(), next_assistant.clone()]);

        assert_eq!(
            old_summary.as_deref(),
            Some("## Old Summary\n- old\n\n## New Summary\n- new")
        );
        assert_eq!(new_messages, vec![next_user, next_assistant]);
    }

    #[test]
    fn parses_structured_summary_from_fenced_json() {
        let parsed = parse_structured_summary(
            r#"```json
{
  "old_summary": "- old",
  "new_summary": "- new",
  "keywords": ["memory"],
  "important_refs": {
    "paths": ["NEW_MEMORY_SYSTEM.md"],
    "commands": [],
    "errors": [],
    "urls": [],
    "ids": []
  },
  "memory_hints": [
    {
      "group": "Memory System",
      "conclusions": ["Use structured compaction output."]
    }
  ],
  "next_step": "Persist rollout artifacts."
}
```"#,
        )
        .unwrap();

        assert_eq!(parsed.old_summary, "- old");
        assert_eq!(parsed.new_summary, "- new");
        assert_eq!(parsed.keywords, vec!["memory"]);
        assert_eq!(parsed.important_refs.paths, vec!["NEW_MEMORY_SYSTEM.md"]);
        assert_eq!(parsed.memory_hints[0].group, "Memory System");
        assert_eq!(parsed.next_step, "Persist rollout artifacts.");
    }
}
