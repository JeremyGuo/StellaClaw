use crate::config::AgentConfig;
use crate::llm::TokenUsage;
use crate::llm::create_chat_completion;
use crate::message::ChatMessage;
use crate::tooling::Tool;
use anyhow::{Result, anyhow};
use serde_json::{Map, Value};

pub const COMPACTION_MARKER: &str = "[AgentFrame Context Compression]";

#[derive(Clone, Debug, Default)]
pub struct ContextCompactionReport {
    pub messages: Vec<ChatMessage>,
    pub usage: TokenUsage,
    pub compacted: bool,
    pub estimated_tokens_before: usize,
    pub estimated_tokens_after: usize,
    pub token_limit: usize,
}

const SUMMARY_SYSTEM_PROMPT: &str = "You compress older conversation history for an agent runtime.

Produce durable memory that lets the next assistant continue work without re-reading the old turns.

Preserve:
- the user's goals, constraints, and preferences
- exact file paths, commands, URLs, identifiers, config values, and decisions that still matter
- work already completed, including edits, tool results, and failures to avoid repeating
- open questions, pending tasks, and next recommended actions

Rules:
- keep it concise and factual
- redact long secrets; mention that a secret or cookie was provided without copying the full value
- do not invent details
- prefer short markdown bullets under these headings:
  Goals
  Constraints
  Work Completed
  Important Facts
  Pending";

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
    config.auto_compact_token_limit.unwrap_or_else(|| {
        (config.upstream.context_window_tokens as f64 * config.effective_context_window_percent)
            .floor() as usize
    })
}

fn render_message_for_summary(message: &ChatMessage) -> String {
    let mut parts = vec![format!("role: {}", message.role)];
    if let Some(name) = &message.name {
        parts.push(format!("name: {}", name));
    }
    if let Some(tool_call_id) = &message.tool_call_id {
        parts.push(format!("tool_call_id: {}", tool_call_id));
    }
    if let Some(tool_calls) = &message.tool_calls {
        parts.push(format!(
            "tool_calls: {}",
            serde_json::to_string(tool_calls).unwrap_or_default()
        ));
    }
    let content = content_to_text(&message.content);
    if !content.is_empty() {
        let truncated = content.chars().take(4_000).collect::<String>();
        let suffix = if content.chars().count() > 4_000 {
            " ...[truncated]"
        } else {
            ""
        };
        parts.push(format!("content:\n{}{}", truncated, suffix));
    }
    parts.join("\n")
}

fn build_summary_request(messages_to_summarize: &[ChatMessage]) -> Vec<ChatMessage> {
    let transcript = messages_to_summarize
        .iter()
        .enumerate()
        .map(|(index, message)| {
            format!(
                "Message {}\n{}",
                index + 1,
                render_message_for_summary(message)
            )
        })
        .collect::<Vec<_>>()
        .join("\n\n");
    vec![
        ChatMessage::text("system", SUMMARY_SYSTEM_PROMPT),
        ChatMessage::text(
            "user",
            format!(
                "Summarize the following earlier conversation history for future turns.\n\n{}",
                transcript
            ),
        ),
    ]
}

fn generate_summary(
    config: &AgentConfig,
    messages_to_summarize: &[ChatMessage],
) -> Result<(String, TokenUsage)> {
    let mut extra_payload = Map::new();
    extra_payload.insert("max_completion_tokens".to_string(), Value::from(1_200_u64));
    let summary_message = create_chat_completion(
        &config.upstream,
        &build_summary_request(messages_to_summarize),
        &[],
        Some(extra_payload),
    )?;
    let summary_text = content_to_text(&summary_message.message.content);
    if summary_text.trim().is_empty() {
        return Err(anyhow!("context compression summary came back empty"));
    }
    Ok((summary_text, summary_message.usage))
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

fn compact_history_once(
    config: &AgentConfig,
    messages: &[ChatMessage],
    retain_recent: usize,
) -> Result<(Vec<ChatMessage>, TokenUsage)> {
    if messages.is_empty() {
        return Ok((messages.to_vec(), TokenUsage::default()));
    }

    let has_system = messages.first().map(|message| message.role.as_str()) == Some("system");
    let system_prefix = if has_system {
        vec![messages[0].clone()]
    } else {
        Vec::new()
    };
    let body = if has_system { &messages[1..] } else { messages };
    if body.len() <= retain_recent + 1 {
        return Ok((messages.to_vec(), TokenUsage::default()));
    }

    let split_index = adjust_split_index_to_preserve_tool_context(body, body.len() - retain_recent);
    if split_index == 0 {
        return Ok((messages.to_vec(), TokenUsage::default()));
    }
    let messages_to_summarize = &body[..split_index];
    let recent_messages = &body[split_index..];
    let (summary_text, usage) = generate_summary(config, messages_to_summarize)?;
    let summary_message = ChatMessage::text(
        "assistant",
        format!(
            "{COMPACTION_MARKER}\n\nOlder conversation history has been compacted into the summary below.\n\n{}",
            summary_text
        ),
    );
    let mut compacted = system_prefix;
    compacted.push(summary_message);
    compacted.extend_from_slice(recent_messages);
    Ok((compacted, usage))
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
            estimated_tokens_before,
            estimated_tokens_after: estimated_tokens_before,
            token_limit: limit,
            ..ContextCompactionReport::default()
        });
    }

    let mut compacted = messages.to_vec();
    let mut usage = TokenUsage::default();
    let mut retain_recent = config.retain_recent_messages.max(2);
    for _ in 0..3 {
        let (next, step_usage) = compact_history_once(config, &compacted, retain_recent)?;
        if next == compacted {
            break;
        }
        usage.add_assign(&step_usage);
        compacted = next;
        if estimate_session_tokens(&compacted, tools, pending_user_prompt) < limit {
            break;
        }
        retain_recent = (retain_recent / 2).max(2);
    }
    let estimated_tokens_after = estimate_session_tokens(&compacted, tools, pending_user_prompt);
    Ok(ContextCompactionReport {
        compacted: compacted != messages,
        messages: compacted,
        usage,
        estimated_tokens_before,
        estimated_tokens_after,
        token_limit: limit,
    })
}

#[cfg(test)]
mod tests {
    use super::{
        adjust_split_index_to_preserve_tool_context, content_to_text,
        find_originating_tool_use_index,
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
}
