use crate::config::AgentConfig;
use crate::llm::TokenUsage;
use crate::llm::create_chat_completion;
use crate::message::ChatMessage;
use crate::tooling::Tool;
use crate::tooling::active_runtime_state_summary;
use anyhow::{Context, Result, anyhow};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

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
                    _ => None,
                }
            })
            .collect::<Vec<_>>()
            .join("\n"),
        Some(other) => serde_json::to_string(other).unwrap_or_default(),
        None => String::new(),
    }
}

fn estimate_text_tokens(text: &str) -> usize {
    if text.is_empty() {
        return 0;
    }
    let char_estimate = (text.chars().count() as f64 * 0.75).ceil() as usize;
    let byte_estimate = text.len().div_ceil(4);
    char_estimate.max(byte_estimate).max(1)
}

pub fn estimate_session_tokens(
    messages: &[ChatMessage],
    tools: &[Tool],
    pending_user_prompt: &str,
) -> usize {
    let message_tokens = messages
        .iter()
        .map(|message| serde_json::to_string(message).unwrap_or_default())
        .map(|serialized| estimate_text_tokens(&serialized) + 6)
        .sum::<usize>();
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
    let summary_start = after_marker
        .find("## Old Summary")
        .or_else(|| after_marker.find("Older conversation history has been compacted into the summary below."));
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

fn build_summary_request(messages: &[ChatMessage], preserved_recent_count: usize) -> Vec<ChatMessage> {
    let mut request_messages = messages.to_vec();
    request_messages.push(ChatMessage::text(
        "user",
        format!(
            "Compress the older conversation history in this same transcript.\n\nReturn a JSON object with exactly these top-level keys:\n- old_summary\n- new_summary\n- keywords\n- important_refs\n- memory_hints\n- next_step\n\nMeaning:\n- old_summary: further compress the previously compacted old history. If there is no previous compacted old history, return an empty string.\n- new_summary: summarize only the older conversation history that appears before the most recent {preserved_recent_count} message(s) immediately preceding this request.\n- keywords: short retrieval keywords.\n- important_refs: object with arrays paths, commands, errors, urls, ids.\n- memory_hints: array of {{ group, conclusions }} for higher-level memory grouping.\n- next_step: one short recommended next step.\n\nRules:\n- keep summaries concise and factual\n- redact long secrets; mention that a secret or cookie was provided without copying the full value\n- do not invent details\n- do not restate or summarize shared context content from the system prompt, skills metadata, USER, or IDENTITY\n- the most recent {preserved_recent_count} message(s) immediately preceding this request are preserved separately as the recent high-fidelity zone; do not summarize them except for a tiny pointer when continuity absolutely requires it\n- if an earlier assistant message beginning with {COMPACTION_MARKER} exists in the older history, further compress its Old Summary into old_summary\n- preserve continuation-critical identifiers when present in the older history, especially exec_id, download_id, subagent ids, and any path, cwd, or url needed to continue unfinished work safely\n- old_summary and new_summary should each be markdown bullet summaries\n- if a field has no content, use an empty string or empty array\n- return JSON only"
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

fn generate_summary(
    config: &AgentConfig,
    messages: &[ChatMessage],
    preserved_recent_count: usize,
) -> Result<(StructuredCompactionOutput, TokenUsage)> {
    let mut extra_payload = Map::new();
    extra_payload.insert("max_completion_tokens".to_string(), Value::from(1_200_u64));
    let summary_message = create_chat_completion(
        &config.upstream,
        &build_summary_request(messages, preserved_recent_count),
        &[],
        Some(extra_payload),
    )?;
    let summary_text = content_to_text(&summary_message.message.content);
    if summary_text.trim().is_empty() {
        return Err(anyhow!("context compression summary came back empty"));
    }
    Ok((parse_structured_summary(&summary_text)?, summary_message.usage))
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

fn estimate_message_tokens(message: &ChatMessage) -> usize {
    let serialized = serde_json::to_string(message).unwrap_or_default();
    estimate_text_tokens(&serialized) + 6
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
    let (structured_output, usage) =
        generate_summary(config, messages, recent_messages.len())?;
    let runtime_state = active_runtime_state_summary(&config.runtime_state_root)?;
    let summary_message = ChatMessage::text(
        "assistant",
        render_structured_summary(&structured_output),
    );
    let mut compacted = system_prefix;
    compacted.push(summary_message);
    if let Some(runtime_state) = runtime_state.filter(|value| !value.trim().is_empty()) {
        compacted.push(ChatMessage::text("system", runtime_state));
    }
    compacted.extend_from_slice(recent_messages);
    Ok((
        compacted,
        usage,
        Some(structured_output),
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
            compact_history_once(config, &compacted, retain_recent_token_budget)?;
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
        estimate_message_tokens, extract_previous_compaction_summary,
        find_originating_tool_use_index,
        parse_structured_summary, split_compaction_inputs,
        recent_tail_start_by_token_budget,
    };
    use crate::message::{ChatMessage, ToolCall};
    use serde_json::json;

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
                        name: "read_file".to_string(),
                        arguments: Some("{}".to_string()),
                    },
                }]),
            },
            ChatMessage::tool_output("call_1", "read_file", "{\"ok\":true}"),
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
                        name: "read_file".to_string(),
                        arguments: Some("{}".to_string()),
                    },
                }]),
            },
            ChatMessage::tool_output("call_1", "read_file", "{\"ok\":true}"),
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
            ChatMessage::tool_output("call_1", "exec_wait", "{\"exec_id\":\"abc\",\"status\":\"running\"}"),
        ];

        let budget = estimate_message_tokens(&messages[3]);
        let start = recent_tail_start_by_token_budget(&messages, budget);
        assert_eq!(start, 2);
    }

    #[test]
    fn extracts_previous_compaction_summary_from_marker_message() {
        let message = ChatMessage::text(
            "assistant",
            format!(
                "{COMPACTION_MARKER}\n\n## Old Summary\n- older\n\n## New Summary\n- newer"
            ),
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

        assert_eq!(old_summary.as_deref(), Some("## Old Summary\n- old\n\n## New Summary\n- new"));
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
