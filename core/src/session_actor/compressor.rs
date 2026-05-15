use thiserror::Error;

use crate::{
    model_config::ModelConfig,
    providers::{send_provider_request_with_retry, Provider, ProviderError, ProviderRequest},
};

use super::{
    ChatMessage, ChatMessageItem, ChatRole, ContextItem, FileItem, TokenEstimator,
    TokenEstimatorError, ToolResultContent,
};

pub const COMPRESSION_MARKER: &str = "[StellaClaw Context Compression]";
const MAX_PRESERVED_TOOL_CALL_IDS: usize = 8;
const MAX_PRESERVED_TOOL_RESULT_TEXT_CHARS: usize = 4096;
const COMPRESSION_CYBER_POLICY_RETRY_FEEDBACK_PREFIX: &str = "[Compression Retry Feedback]\nThe previous request was rejected as possible cybersecurity risk, but this is a benign context-compaction request for an existing agent conversation. Do not add new cybersecurity instructions, procedures, exploit steps, or operational details. Only summarize the already-present conversation state at the level needed for continuity, preserving user requirements, decisions, files, validation state, blockers, active task IDs/process IDs, and exact next steps. Return the same strict JSON schema.";

#[derive(Debug, Clone)]
pub struct SessionCompressor {
    estimator: TokenEstimator,
    threshold_tokens: u64,
    retain_recent_tokens: u64,
}

impl SessionCompressor {
    pub fn new(
        estimator: TokenEstimator,
        threshold_tokens: u64,
        retain_recent_tokens: u64,
    ) -> Result<Self, CompressionError> {
        if threshold_tokens == 0 {
            return Err(CompressionError::InvalidThreshold);
        }

        Ok(Self {
            estimator,
            threshold_tokens,
            retain_recent_tokens: retain_recent_tokens.max(1),
        })
    }

    pub fn append_with_compression(
        &self,
        messages: &mut Vec<ChatMessage>,
        next_message: ChatMessage,
        provider: &(dyn Provider + Send + Sync),
        model_config: &ModelConfig,
        system_prompt: Option<&str>,
        compression_context: Option<&str>,
    ) -> Result<CompressionReport, CompressionError> {
        let estimated_tokens_before = self.estimate_with_next(messages, &next_message)?;
        if estimated_tokens_before <= self.threshold_tokens {
            messages.push(next_message);
            return Ok(CompressionReport {
                compressed: false,
                estimated_tokens_before,
                estimated_tokens_after: estimated_tokens_before,
                threshold_tokens: self.threshold_tokens,
                retained_message_count: messages.len(),
                compressed_message_count: 0,
            });
        }

        let Some(plan) = self.plan_compression(messages)? else {
            messages.push(next_message);
            let estimated_tokens_after = self.estimator.estimate(messages)?.total_tokens;
            return Ok(CompressionReport {
                compressed: false,
                estimated_tokens_before,
                estimated_tokens_after,
                threshold_tokens: self.threshold_tokens,
                retained_message_count: messages.len(),
                compressed_message_count: 0,
            });
        };

        let generated_compression = self.generate_compression(
            &messages[..plan.recent_start],
            messages.len() - plan.recent_start,
            provider,
            model_config,
            system_prompt,
            compression_context,
        )?;
        let summary_present = generated_compression.is_some();
        let mut compressed_messages = Vec::new();
        if let Some(generated_compression) = generated_compression {
            compressed_messages.push(generated_compression.summary_message);
            compressed_messages.extend(generated_compression.preserved_tool_messages);
        }
        compressed_messages.extend_from_slice(&messages[plan.recent_start..]);
        let compressed_message_count = plan.recent_start;
        let retained_message_count = compressed_messages
            .len()
            .saturating_sub(usize::from(summary_present));

        *messages = compressed_messages;
        messages.push(next_message);

        let estimated_tokens_after = self.estimator.estimate(messages)?.total_tokens;
        Ok(CompressionReport {
            compressed: true,
            estimated_tokens_before,
            estimated_tokens_after,
            threshold_tokens: self.threshold_tokens,
            retained_message_count,
            compressed_message_count,
        })
    }

    pub fn compact_if_needed(
        &self,
        messages: &mut Vec<ChatMessage>,
        provider: &(dyn Provider + Send + Sync),
        model_config: &ModelConfig,
        system_prompt: Option<&str>,
    ) -> Result<CompressionReport, CompressionError> {
        self.compact_if_needed_with_threshold(
            messages,
            provider,
            model_config,
            system_prompt,
            self.threshold_tokens,
            None,
        )
    }

    pub fn compact_now(
        &self,
        messages: &mut Vec<ChatMessage>,
        provider: &(dyn Provider + Send + Sync),
        model_config: &ModelConfig,
        system_prompt: Option<&str>,
        compression_context: Option<&str>,
    ) -> Result<CompressionReport, CompressionError> {
        let estimated_tokens_before = self.estimator.estimate(messages)?.total_tokens;
        let Some(plan) = self.plan_compression(messages)? else {
            return Ok(CompressionReport {
                compressed: false,
                estimated_tokens_before,
                estimated_tokens_after: estimated_tokens_before,
                threshold_tokens: self.threshold_tokens,
                retained_message_count: messages.len(),
                compressed_message_count: 0,
            });
        };

        let generated_compression = self.generate_compression(
            &messages[..plan.recent_start],
            messages.len() - plan.recent_start,
            provider,
            model_config,
            system_prompt,
            compression_context,
        )?;
        let summary_present = generated_compression.is_some();
        let mut compressed_messages = Vec::new();
        if let Some(generated_compression) = generated_compression {
            compressed_messages.push(generated_compression.summary_message);
            compressed_messages.extend(generated_compression.preserved_tool_messages);
        }
        compressed_messages.extend_from_slice(&messages[plan.recent_start..]);
        let compressed_message_count = plan.recent_start;
        let retained_message_count = compressed_messages
            .len()
            .saturating_sub(usize::from(summary_present));

        *messages = compressed_messages;

        let estimated_tokens_after = self.estimator.estimate(messages)?.total_tokens;
        Ok(CompressionReport {
            compressed: true,
            estimated_tokens_before,
            estimated_tokens_after,
            threshold_tokens: self.threshold_tokens,
            retained_message_count,
            compressed_message_count,
        })
    }

    pub fn compact_if_needed_with_threshold(
        &self,
        messages: &mut Vec<ChatMessage>,
        provider: &(dyn Provider + Send + Sync),
        model_config: &ModelConfig,
        system_prompt: Option<&str>,
        threshold_tokens: u64,
        compression_context: Option<&str>,
    ) -> Result<CompressionReport, CompressionError> {
        if threshold_tokens == 0 {
            return Err(CompressionError::InvalidThreshold);
        }
        let estimated_tokens_before = self.estimator.estimate(messages)?.total_tokens;
        if estimated_tokens_before <= threshold_tokens {
            return Ok(CompressionReport {
                compressed: false,
                estimated_tokens_before,
                estimated_tokens_after: estimated_tokens_before,
                threshold_tokens,
                retained_message_count: messages.len(),
                compressed_message_count: 0,
            });
        }

        let Some(plan) = self.plan_compression(messages)? else {
            return Ok(CompressionReport {
                compressed: false,
                estimated_tokens_before,
                estimated_tokens_after: estimated_tokens_before,
                threshold_tokens,
                retained_message_count: messages.len(),
                compressed_message_count: 0,
            });
        };

        let generated_compression = self.generate_compression(
            &messages[..plan.recent_start],
            messages.len() - plan.recent_start,
            provider,
            model_config,
            system_prompt,
            compression_context,
        )?;
        let summary_present = generated_compression.is_some();
        let mut compressed_messages = Vec::new();
        if let Some(generated_compression) = generated_compression {
            compressed_messages.push(generated_compression.summary_message);
            compressed_messages.extend(generated_compression.preserved_tool_messages);
        }
        compressed_messages.extend_from_slice(&messages[plan.recent_start..]);
        let compressed_message_count = plan.recent_start;
        let retained_message_count = compressed_messages
            .len()
            .saturating_sub(usize::from(summary_present));

        *messages = compressed_messages;

        let estimated_tokens_after = self.estimator.estimate(messages)?.total_tokens;
        Ok(CompressionReport {
            compressed: true,
            estimated_tokens_before,
            estimated_tokens_after,
            threshold_tokens,
            retained_message_count,
            compressed_message_count,
        })
    }

    fn estimate_with_next(
        &self,
        messages: &[ChatMessage],
        next_message: &ChatMessage,
    ) -> Result<u64, CompressionError> {
        let mut next_messages = Vec::with_capacity(messages.len() + 1);
        next_messages.extend_from_slice(messages);
        next_messages.push(next_message.clone());
        Ok(self.estimator.estimate(&next_messages)?.total_tokens)
    }

    fn plan_compression(
        &self,
        messages: &[ChatMessage],
    ) -> Result<Option<CompressionPlan>, CompressionError> {
        if messages.is_empty() {
            return Ok(None);
        }

        let recent_start = recent_tail_start_by_token_budget(
            messages,
            self.retain_recent_tokens,
            &self.estimator,
        )?;
        if recent_start == 0 {
            return Ok(None);
        }

        Ok(Some(CompressionPlan { recent_start }))
    }

    fn generate_compression(
        &self,
        messages_to_summarize: &[ChatMessage],
        preserved_recent_count: usize,
        provider: &(dyn Provider + Send + Sync),
        _model_config: &ModelConfig,
        system_prompt: Option<&str>,
        compression_context: Option<&str>,
    ) -> Result<Option<GeneratedCompression>, CompressionError> {
        let request_messages = build_compression_request_messages(
            messages_to_summarize,
            preserved_recent_count,
            compression_context,
            CompressionSanitizeMode::Full,
        );
        if request_messages.len() == compression_request_suffix_len(compression_context) {
            return Ok(None);
        }

        let Some(response) = send_compression_request_with_policy_reductions(
            provider,
            messages_to_summarize,
            preserved_recent_count,
            compression_context,
            &request_messages,
            system_prompt,
        )?
        else {
            return Ok(None);
        };
        let response_text = message_text(&response);
        if response_text.trim().is_empty() {
            return Err(CompressionError::EmptySummary);
        }
        let result = parse_compression_result(&response_text)?;
        let summary_text = render_compression_result(&result);
        let preserved_tool_messages =
            preserve_tool_messages(messages_to_summarize, &result.preserved_tool_call_ids);

        Ok(Some(GeneratedCompression {
            summary_message: ChatMessage::new(
                ChatRole::Assistant,
                vec![ChatMessageItem::Context(ContextItem { text: summary_text })],
            ),
            preserved_tool_messages,
        }))
    }

    pub fn would_compress_with_next(
        &self,
        messages: &[ChatMessage],
        next_message: &ChatMessage,
    ) -> Result<bool, CompressionError> {
        if self.estimate_with_next(messages, next_message)? <= self.threshold_tokens {
            return Ok(false);
        }
        Ok(self.plan_compression(messages)?.is_some())
    }

    pub fn would_compact_with_threshold(
        &self,
        messages: &[ChatMessage],
        threshold_tokens: u64,
    ) -> Result<bool, CompressionError> {
        if threshold_tokens == 0 {
            return Err(CompressionError::InvalidThreshold);
        }
        if self.estimator.estimate(messages)?.total_tokens <= threshold_tokens {
            return Ok(false);
        }
        Ok(self.plan_compression(messages)?.is_some())
    }
}

fn send_compression_request_with_policy_reductions(
    provider: &(dyn Provider + Send + Sync),
    messages_to_summarize: &[ChatMessage],
    preserved_recent_count: usize,
    compression_context: Option<&str>,
    request_messages: &[ChatMessage],
    system_prompt: Option<&str>,
) -> Result<Option<ChatMessage>, CompressionError> {
    match send_provider_request_with_retry(
        provider,
        ProviderRequest::new(request_messages).with_system_prompt(system_prompt),
        |_| {},
    ) {
        Ok(response) => Ok(Some(response)),
        Err(error) if provider_error_is_cyber_policy(&error) => {
            for retry in [
                CompressionCyberPolicyRetry::FeedbackOnly,
                CompressionCyberPolicyRetry::DropToolResults,
                CompressionCyberPolicyRetry::DropToolResultsAndUserMessages,
            ] {
                let retry_messages = retry.build_request_messages(
                    request_messages,
                    messages_to_summarize,
                    preserved_recent_count,
                    compression_context,
                );
                match send_provider_request_with_retry(
                    provider,
                    ProviderRequest::new(&retry_messages).with_system_prompt(system_prompt),
                    |_| {},
                ) {
                    Ok(response) => return Ok(Some(response)),
                    Err(retry_error) if provider_error_is_cyber_policy(&retry_error) => {}
                    Err(retry_error) => {
                        return Err(CompressionError::Provider(retry_error.to_string()));
                    }
                }
            }
            Ok(None)
        }
        Err(error) => Err(CompressionError::Provider(error.to_string())),
    }
}

fn provider_error_is_cyber_policy(error: &ProviderError) -> bool {
    error.is_cyber_policy()
}

#[derive(Clone, Copy)]
enum CompressionCyberPolicyRetry {
    FeedbackOnly,
    DropToolResults,
    DropToolResultsAndUserMessages,
}

impl CompressionCyberPolicyRetry {
    fn build_request_messages(
        self,
        original_request_messages: &[ChatMessage],
        messages_to_summarize: &[ChatMessage],
        preserved_recent_count: usize,
        compression_context: Option<&str>,
    ) -> Vec<ChatMessage> {
        let mut messages = match self {
            Self::FeedbackOnly => original_request_messages.to_vec(),
            Self::DropToolResults => build_compression_request_messages(
                messages_to_summarize,
                preserved_recent_count,
                compression_context,
                CompressionSanitizeMode::DropToolResults,
            ),
            Self::DropToolResultsAndUserMessages => build_compression_request_messages(
                messages_to_summarize,
                preserved_recent_count,
                compression_context,
                CompressionSanitizeMode::DropToolResultsAndUserMessages,
            ),
        };
        messages.push(ChatMessage::new(
            ChatRole::User,
            vec![ChatMessageItem::Context(ContextItem {
                text: self.feedback_text(),
            })],
        ));
        messages
    }

    fn feedback_text(self) -> String {
        let reduction = match self {
            Self::FeedbackOnly => "",
            Self::DropToolResults => "\nThis retry has removed all tool result content from the history being summarized. Summarize only the remaining context; do not infer omitted tool output.",
            Self::DropToolResultsAndUserMessages => "\nThis retry has removed all tool result content and all user-role history messages from the history being summarized. Summarize only the remaining context; do not infer omitted user text or tool output.",
        };
        format!("{COMPRESSION_CYBER_POLICY_RETRY_FEEDBACK_PREFIX}{reduction}")
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompressionReport {
    pub compressed: bool,
    pub estimated_tokens_before: u64,
    pub estimated_tokens_after: u64,
    pub threshold_tokens: u64,
    pub retained_message_count: usize,
    pub compressed_message_count: usize,
}

#[derive(Debug, Error)]
pub enum CompressionError {
    #[error("compression threshold must be greater than zero")]
    InvalidThreshold,
    #[error("token estimation failed during compression: {0}")]
    Estimate(#[from] TokenEstimatorError),
    #[error("compression provider failed: {0}")]
    Provider(String),
    #[error("compression summary came back empty")]
    EmptySummary,
    #[error("compression summary was not valid JSON: {0}")]
    InvalidSummaryJson(String),
}

struct CompressionPlan {
    recent_start: usize,
}

struct GeneratedCompression {
    summary_message: ChatMessage,
    preserved_tool_messages: Vec<ChatMessage>,
}

#[derive(Debug, Default, serde::Deserialize)]
struct CompressionResult {
    #[serde(default)]
    summary: String,
    #[serde(default)]
    current_state: String,
    #[serde(default)]
    plan: String,
    #[serde(default)]
    active_tasks: Vec<ActiveTaskSummary>,
    #[serde(default)]
    preserved_tool_call_ids: Vec<String>,
}

#[derive(Debug, Default, serde::Deserialize)]
struct ActiveTaskSummary {
    #[serde(default)]
    id: String,
    #[serde(default)]
    kind: String,
    #[serde(default)]
    status: String,
    #[serde(default)]
    purpose: String,
    #[serde(default)]
    next_action: String,
}

fn recent_tail_start_by_token_budget(
    messages: &[ChatMessage],
    token_budget: u64,
    estimator: &TokenEstimator,
) -> Result<usize, CompressionError> {
    if messages.is_empty() {
        return Ok(0);
    }

    let mut start = messages.len();
    let mut total_tokens = 0_u64;
    while start > 0 {
        let candidate_start =
            adjust_split_index_to_preserve_tool_context(messages, start.saturating_sub(1));
        if candidate_start >= start {
            break;
        }
        let slice_tokens = estimator
            .estimate(&messages[candidate_start..start])?
            .total_tokens;
        if total_tokens + slice_tokens > token_budget && start < messages.len() {
            break;
        }

        total_tokens = total_tokens.saturating_add(slice_tokens);
        start = candidate_start;
        if total_tokens >= token_budget {
            break;
        }
    }

    Ok(start)
}

#[derive(Clone, Copy)]
enum CompressionSanitizeMode {
    Full,
    DropToolResults,
    DropToolResultsAndUserMessages,
}

fn build_compression_request_messages(
    messages: &[ChatMessage],
    preserved_recent_count: usize,
    compression_context: Option<&str>,
    mode: CompressionSanitizeMode,
) -> Vec<ChatMessage> {
    let mut request_messages = sanitize_messages_for_compression_request_with_mode(messages, mode);
    if let Some(context) = compression_context
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        request_messages.push(ChatMessage::new(
            ChatRole::User,
            vec![ChatMessageItem::Context(ContextItem {
                text: format!(
                    "[Relevant Long Memory For Compression]\nUse this as small background context while compressing. Keep only details that are still relevant to the current conversation state; do not copy unrelated memory.\n\n{context}"
                ),
            })],
        ));
    }
    request_messages.push(ChatMessage::new(
        ChatRole::User,
        vec![ChatMessageItem::Context(ContextItem {
            text: compression_instruction(preserved_recent_count),
        })],
    ));
    request_messages
}

fn compression_request_suffix_len(compression_context: Option<&str>) -> usize {
    1 + usize::from(
        compression_context
            .map(str::trim)
            .is_some_and(|value| !value.is_empty()),
    )
}

#[cfg(test)]
fn sanitize_messages_for_compression_request(messages: &[ChatMessage]) -> Vec<ChatMessage> {
    sanitize_messages_for_compression_request_with_mode(messages, CompressionSanitizeMode::Full)
}

fn sanitize_messages_for_compression_request_with_mode(
    messages: &[ChatMessage],
    mode: CompressionSanitizeMode,
) -> Vec<ChatMessage> {
    messages
        .iter()
        .filter(|message| !is_runtime_update_message(message))
        .filter(|message| {
            !matches!(
                mode,
                CompressionSanitizeMode::DropToolResultsAndUserMessages
                    if message.role == ChatRole::User
            )
        })
        .filter_map(|message| sanitize_message_for_compression_request(message, mode))
        .collect()
}

fn sanitize_message_for_compression_request(
    message: &ChatMessage,
    mode: CompressionSanitizeMode,
) -> Option<ChatMessage> {
    let mut data = Vec::new();
    for item in &message.data {
        match item {
            ChatMessageItem::Reasoning(_) => {}
            ChatMessageItem::Context(context) => {
                data.push(ChatMessageItem::Context(context.clone()))
            }
            ChatMessageItem::SelectionReference(selection) => {
                data.push(ChatMessageItem::Context(ContextItem {
                    text: selection.to_prompt_text(),
                }));
            }
            ChatMessageItem::File(file) => data.push(ChatMessageItem::Context(ContextItem {
                text: file_placeholder(file),
            })),
            ChatMessageItem::ToolCall(tool_call) => {
                let mut text = format!(
                    "[tool call: {} id={}]",
                    tool_call.tool_name, tool_call.tool_call_id
                );
                if !tool_call.arguments.text.trim().is_empty() {
                    text.push('\n');
                    text.push_str(&tool_call.arguments.text);
                }
                data.push(ChatMessageItem::Context(ContextItem { text }));
            }
            ChatMessageItem::ToolResult(tool_result) => {
                if matches!(
                    mode,
                    CompressionSanitizeMode::DropToolResults
                        | CompressionSanitizeMode::DropToolResultsAndUserMessages
                ) {
                    continue;
                }
                data.push(ChatMessageItem::Context(ContextItem {
                    text: tool_result_placeholder(
                        &tool_result.tool_name,
                        &tool_result.tool_call_id,
                        &tool_result.result,
                    ),
                }));
            }
        }
    }

    if data.is_empty() {
        return None;
    }

    Some(ChatMessage {
        message_id: message.message_id.clone(),
        role: message.role.clone(),
        user_name: message.user_name.clone(),
        message_time: message.message_time.clone(),
        token_usage: None,
        data,
    })
}

fn compression_instruction(preserved_recent_count: usize) -> String {
    format!(
        "The conversation is nearing its context limit. To continue effectively, create a comprehensive, high-fidelity summary of the task's progress.\n\n\
This compression will be the only low-level context available for the compressed portion of the conversation. Prioritize continuation fidelity over brevity, but do not copy raw history that can be accurately summarized.\n\n\
Return strict JSON only. Do not output Markdown, code fences, or commentary.\n\n\
Schema:\n\
{{\n\
  \"summary\": \"High-fidelity natural-language summary of all user intents and requirements, key facts, technical findings, architectural decisions, code patterns discovered, touched files/modules, completed work, and important errors.\",\n\
  \"current_state\": \"Precise current status: what is happening now, the latest meaningful tool result, files currently being worked on, blockers, and any pending user decision.\",\n\
  \"plan\": \"Exact next steps needed to continue safely if work remains. Include ordering, verification steps, and any path/cwd/command/id needed for the next action.\",\n\
  \"active_tasks\": [{{\"id\":\"process_id_or_other_handle\", \"kind\":\"shell|download|subagent|background_agent|dsl|other\", \"status\":\"running|waiting|blocked|unknown\", \"purpose\":\"what this task is doing\", \"next_action\":\"how to observe, continue, stop, or recover this task\"}}],\n\
  \"preserved_tool_call_ids\": [\"call_id\"]\n\
}}\n\n\
Rules:\n\
- keep every field factual, compact, and continuation-oriented\n\
- capture the whole task state, not just a chat recap: goals, constraints, decisions, findings, touched files, code patterns, completed work, remaining work, and verification state\n\
- include critical code snippets or exact symbols only when they are needed to continue safely; otherwise cite file paths, modules, functions, commands, ids, URLs, and conclusions\n\
- preserve concrete decisions, file paths, commands, errors, ids, URLs, and the current next step\n\
- active_tasks is required for unfinished external work. Include every still-running or still-waiting handle needed to resume safely, especially shell process_id, subagent id, background agent_id, and any associated command, cwd, remote target, output path, URL, or wait/stop/observe instruction\n\
- do not put completed one-off tool calls in active_tasks; only include tasks whose future state can still change or whose handle is needed for recovery\n\
- redact long secrets; mention that a secret was provided without copying the full value\n\
- do not invent details\n\
- do not restate shared context that already lives in the canonical system prompt snapshots\n\
- ignore transient runtime update notices; they are reconstructed separately and should not be preserved in the summary\n\
- if unfinished work still matters, preserve the continuation-critical identifier in active_tasks and also mention it in current_state or plan when it is the immediate next action\n\
- if a task is already finished or no longer relevant, do not preserve its identifier just because it appeared earlier\n\
- intermediate tool calls and tool results should be summarized by outcome, not replayed step by step, unless a still-needed identifier or exact reference is required to continue safely\n\
- preserved_tool_call_ids is optional. Prefer active_tasks and summary/current_state/plan. Only include real tool_call ids from the compressed history if the raw tool arguments and result are very likely to be needed later and cannot be safely summarized\n\
- if you already extracted the useful conclusion from a tool result into summary/current_state/plan, do not preserve that tool call id\n\
- be conservative with preserved_tool_call_ids so compression does not keep too much raw context\n\
- the most recent {preserved_recent_count} message(s) immediately before this request are preserved separately as high-fidelity context; do not summarize them unless continuity requires a short pointer\n\
Return JSON only."
    )
}

fn parse_compression_result(text: &str) -> Result<CompressionResult, CompressionError> {
    let trimmed = text.trim();
    serde_json::from_str::<CompressionResult>(trimmed)
        .map(normalize_compression_result)
        .map_err(|error| CompressionError::InvalidSummaryJson(error.to_string()))
}

fn normalize_compression_result(mut result: CompressionResult) -> CompressionResult {
    result.summary = result.summary.trim().to_string();
    result.current_state = result.current_state.trim().to_string();
    result.plan = result.plan.trim().to_string();
    result.active_tasks = result
        .active_tasks
        .into_iter()
        .map(normalize_active_task_summary)
        .filter(|task| !task.id.is_empty())
        .take(16)
        .collect();
    result.preserved_tool_call_ids = result
        .preserved_tool_call_ids
        .into_iter()
        .map(|id| id.trim().to_string())
        .filter(|id| !id.is_empty())
        .take(MAX_PRESERVED_TOOL_CALL_IDS)
        .collect();
    result
}

fn normalize_active_task_summary(mut task: ActiveTaskSummary) -> ActiveTaskSummary {
    task.id = task.id.trim().to_string();
    task.kind = task.kind.trim().to_string();
    task.status = task.status.trim().to_string();
    task.purpose = task.purpose.trim().to_string();
    task.next_action = task.next_action.trim().to_string();
    task
}

fn render_compression_result(result: &CompressionResult) -> String {
    let mut sections = vec![
        COMPRESSION_MARKER.to_string(),
        "Older conversation history has been compressed into the structured context below."
            .to_string(),
    ];
    if !result.summary.trim().is_empty() {
        sections.push(format!("Summary:\n{}", result.summary.trim()));
    }
    if !result.current_state.trim().is_empty() {
        sections.push(format!("Current State:\n{}", result.current_state.trim()));
    }
    if !result.plan.trim().is_empty() {
        sections.push(format!("Plan:\n{}", result.plan.trim()));
    }
    if !result.active_tasks.is_empty() {
        let tasks = result
            .active_tasks
            .iter()
            .map(render_active_task_summary)
            .collect::<Vec<_>>()
            .join("\n");
        sections.push(format!("Active Tasks:\n{tasks}"));
    }
    if sections.len() == 2 {
        sections.push("Summary:\nNo useful older context was returned.".to_string());
    }
    sections.join("\n\n")
}

fn render_active_task_summary(task: &ActiveTaskSummary) -> String {
    let mut parts = vec![format!("- id: {}", task.id)];
    if !task.kind.is_empty() {
        parts.push(format!("kind: {}", task.kind));
    }
    if !task.status.is_empty() {
        parts.push(format!("status: {}", task.status));
    }
    if !task.purpose.is_empty() {
        parts.push(format!("purpose: {}", task.purpose));
    }
    if !task.next_action.is_empty() {
        parts.push(format!("next_action: {}", task.next_action));
    }
    parts.join("; ")
}

fn preserve_tool_messages(messages: &[ChatMessage], requested_ids: &[String]) -> Vec<ChatMessage> {
    if requested_ids.is_empty() {
        return Vec::new();
    }
    let existing_ids: std::collections::BTreeSet<&str> = messages
        .iter()
        .flat_map(|message| message.data.iter())
        .filter_map(|item| match item {
            ChatMessageItem::ToolCall(tool_call) => Some(tool_call.tool_call_id.as_str()),
            _ => None,
        })
        .collect();
    let requested: std::collections::BTreeSet<&str> = requested_ids
        .iter()
        .map(String::as_str)
        .filter(|id| existing_ids.contains(id))
        .collect();
    if requested.is_empty() {
        return Vec::new();
    }

    messages
        .iter()
        .filter_map(|message| {
            let data = message
                .data
                .iter()
                .filter_map(|item| match item {
                    ChatMessageItem::ToolCall(tool_call)
                        if requested.contains(tool_call.tool_call_id.as_str()) =>
                    {
                        Some(ChatMessageItem::ToolCall(tool_call.clone()))
                    }
                    ChatMessageItem::ToolResult(tool_result)
                        if requested.contains(tool_result.tool_call_id.as_str()) =>
                    {
                        Some(ChatMessageItem::ToolResult(truncate_tool_result_content(
                            tool_result.clone(),
                        )))
                    }
                    _ => None,
                })
                .collect::<Vec<_>>();
            if data.is_empty() {
                return None;
            }
            Some(ChatMessage {
                message_id: message.message_id.clone(),
                role: message.role.clone(),
                user_name: message.user_name.clone(),
                message_time: message.message_time.clone(),
                token_usage: None,
                data,
            })
        })
        .collect()
}

fn truncate_tool_result_content(mut tool_result: super::ToolResultItem) -> super::ToolResultItem {
    let rendered = super::tool_result_structured_text(&tool_result);
    if rendered.chars().count() > MAX_PRESERVED_TOOL_RESULT_TEXT_CHARS {
        let files = std::mem::take(&mut tool_result.result.files);
        tool_result.result = ToolResultContent::from_text(truncate_text_with_notice(
            &rendered,
            MAX_PRESERVED_TOOL_RESULT_TEXT_CHARS,
        ))
        .with_files(files);
    }
    tool_result
}

fn truncate_text_with_notice(text: &str, max_chars: usize) -> String {
    let trimmed = text.trim();
    if trimmed.chars().count() <= max_chars {
        return trimmed.to_string();
    }
    let mut output = trimmed.chars().take(max_chars).collect::<String>();
    output.push_str("\n[tool result truncated during context compression]");
    output
}

fn find_originating_tool_call_index(
    messages: &[ChatMessage],
    search_before: usize,
    tool_call_id: &str,
) -> Option<usize> {
    messages[..search_before].iter().rposition(|message| {
        message.data.iter().any(|item| {
            matches!(
                item,
                ChatMessageItem::ToolCall(tool_call) if tool_call.tool_call_id == tool_call_id
            )
        })
    })
}

fn adjust_split_index_to_preserve_tool_context(
    messages: &[ChatMessage],
    mut split_index: usize,
) -> usize {
    while split_index < messages.len() {
        let mut earliest_origin: Option<usize> = None;
        for item in &messages[split_index].data {
            let ChatMessageItem::ToolResult(tool_result) = item else {
                continue;
            };
            let Some(origin_index) =
                find_originating_tool_call_index(messages, split_index, &tool_result.tool_call_id)
            else {
                continue;
            };
            earliest_origin = Some(match earliest_origin {
                Some(current) => current.min(origin_index),
                None => origin_index,
            });
        }

        let Some(origin_index) = earliest_origin else {
            break;
        };
        split_index = origin_index;
    }
    split_index
}

fn is_runtime_update_message(message: &ChatMessage) -> bool {
    if message.role != ChatRole::User || message.data.len() != 1 {
        return false;
    }
    let Some(ChatMessageItem::Context(context)) = message.data.first() else {
        return false;
    };
    let text = context.text.trim_start();
    text.starts_with("[Runtime Prompt Updates]")
        || text.starts_with("[Runtime Skill Updates]")
        || text.starts_with("[Incoming User Metadata]")
}

fn message_text(message: &ChatMessage) -> String {
    let mut parts = Vec::new();
    for item in &message.data {
        match item {
            ChatMessageItem::Reasoning(reasoning) => {
                if !reasoning.text.trim().is_empty() {
                    parts.push(reasoning.text.clone());
                }
            }
            ChatMessageItem::Context(context) => {
                if !context.text.trim().is_empty() {
                    parts.push(context.text.clone());
                }
            }
            ChatMessageItem::SelectionReference(selection) => {
                parts.push(selection.to_prompt_text());
            }
            ChatMessageItem::File(file) => parts.push(file_placeholder(file)),
            ChatMessageItem::ToolCall(tool_call) => parts.push(format!(
                "[tool call: {} id={}]",
                tool_call.tool_name, tool_call.tool_call_id
            )),
            ChatMessageItem::ToolResult(tool_result) => parts.push(tool_result_placeholder(
                &tool_result.tool_name,
                &tool_result.tool_call_id,
                &tool_result.result,
            )),
        }
    }
    parts.join("\n\n")
}

fn tool_result_placeholder(
    tool_name: &str,
    tool_call_id: &str,
    result: &ToolResultContent,
) -> String {
    let mut parts = vec![format!("[tool result: {tool_name} id={tool_call_id}]")];
    let result_item = super::ToolResultItem {
        tool_call_id: tool_call_id.to_string(),
        tool_name: tool_name.to_string(),
        result: result.clone(),
    };
    let rendered = super::tool_result_structured_text(&result_item);
    if !rendered.trim().is_empty() {
        parts.push(rendered);
    }
    for file in &result.files {
        parts.push(file_placeholder(file));
    }
    parts.join("\n")
}

fn file_placeholder(file: &FileItem) -> String {
    let label = file.name.as_deref().unwrap_or(&file.uri);
    format!("[file omitted during context compression: {label}]")
}

#[cfg(test)]
mod tests {
    use std::{
        fs,
        sync::{Arc, Mutex},
    };

    use ahash::AHashMap;
    use tokenizers::{
        models::wordlevel::WordLevel, pre_tokenizers::whitespace::Whitespace, Tokenizer,
    };

    use super::*;
    use crate::{
        huggingface::HuggingFaceFileResolver,
        model_config::{ModelCapability, ModelConfig, ProviderType, RetryMode, TokenEstimatorType},
        providers::{ProviderError, ProviderFailureKind},
        session_actor::{FileItem, ToolCallItem, ToolResultContent, ToolResultItem},
    };

    #[derive(Debug, Clone)]
    struct SummaryRequestSnapshot {
        system_prompt: Option<String>,
        messages: Vec<ChatMessage>,
    }

    struct SummaryProvider {
        model_config: ModelConfig,
        seen_requests: Arc<Mutex<Vec<SummaryRequestSnapshot>>>,
        response_text: String,
    }

    impl SummaryProvider {
        fn new(seen_requests: Arc<Mutex<Vec<SummaryRequestSnapshot>>>) -> Self {
            Self {
                model_config: test_model_config(String::new()),
                seen_requests,
                response_text: r#"{"summary":"summary of older context","current_state":"","plan":"","preserved_tool_call_ids":[]}"#
                    .to_string(),
            }
        }

        fn with_response(
            seen_requests: Arc<Mutex<Vec<SummaryRequestSnapshot>>>,
            response_text: impl Into<String>,
        ) -> Self {
            Self {
                model_config: test_model_config(String::new()),
                seen_requests,
                response_text: response_text.into(),
            }
        }
    }

    impl Provider for SummaryProvider {
        fn model_config(&self) -> &ModelConfig {
            &self.model_config
        }

        fn send(&self, request: ProviderRequest<'_>) -> Result<ChatMessage, ProviderError> {
            self.seen_requests
                .lock()
                .unwrap()
                .push(SummaryRequestSnapshot {
                    system_prompt: request.system_prompt.map(str::to_string),
                    messages: request.messages.to_vec(),
                });
            Ok(ChatMessage::new(
                ChatRole::Assistant,
                vec![ChatMessageItem::Context(ContextItem {
                    text: self.response_text.clone(),
                })],
            ))
        }
    }

    struct CyberPolicyOnceSummaryProvider {
        model_config: ModelConfig,
        seen_requests: Arc<Mutex<Vec<SummaryRequestSnapshot>>>,
        calls: Mutex<usize>,
        failures_before_success: usize,
    }

    impl CyberPolicyOnceSummaryProvider {
        fn new(
            seen_requests: Arc<Mutex<Vec<SummaryRequestSnapshot>>>,
            failures_before_success: usize,
        ) -> Self {
            Self {
                model_config: test_model_config(String::new()),
                seen_requests,
                calls: Mutex::new(0),
                failures_before_success,
            }
        }
    }

    impl Provider for CyberPolicyOnceSummaryProvider {
        fn model_config(&self) -> &ModelConfig {
            &self.model_config
        }

        fn send(&self, request: ProviderRequest<'_>) -> Result<ChatMessage, ProviderError> {
            self.seen_requests
                .lock()
                .unwrap()
                .push(SummaryRequestSnapshot {
                    system_prompt: request.system_prompt.map(str::to_string),
                    messages: request.messages.to_vec(),
                });
            let mut calls = self.calls.lock().unwrap();
            *calls += 1;
            if *calls <= self.failures_before_success {
                return Err(ProviderError::ProviderFailure {
                    kind: ProviderFailureKind::CyberPolicy,
                    message: "This content was flagged for possible cybersecurity risk."
                        .to_string(),
                    body: r#"{"error":{"type":"cyberPolicy","message":"This content was flagged for possible cybersecurity risk."}}"#
                        .to_string(),
                });
            }
            Ok(ChatMessage::new(
                ChatRole::Assistant,
                vec![ChatMessageItem::Context(ContextItem {
                    text: r#"{"summary":"summary after policy feedback","current_state":"","plan":"","preserved_tool_call_ids":[]}"#
                        .to_string(),
                })],
            ))
        }
    }

    fn test_model_config(tokenizer_config_path: String) -> ModelConfig {
        ModelConfig {
            provider_type: ProviderType::OpenRouterCompletion,
            model_name: "openai/gpt-4o-mini".to_string(),
            url: "https://openrouter.ai/api/v1/chat/completions".to_string(),
            api_key_env: "OPENROUTER_API_KEY".to_string(),
            capabilities: vec![ModelCapability::Chat],
            token_max_context: 128_000,
            max_tokens: 0,
            cache_timeout: 300,
            conn_timeout: 10,
            request_timeout: 600,
            max_request_size: 30 * 1024 * 1024,
            retry_mode: RetryMode::Once,
            reasoning: None,
            token_estimator_type: TokenEstimatorType::HuggingFace,
            multimodal_estimator: None,
            multimodal_input: None,
            token_estimator_url: Some(tokenizer_config_path),
        }
    }

    fn build_test_estimator() -> (TokenEstimator, ModelConfig, std::path::PathBuf) {
        let mut vocab = AHashMap::new();
        vocab.insert("[UNK]".to_string(), 0);
        vocab.insert("user".to_string(), 1);
        vocab.insert("assistant".to_string(), 2);
        vocab.insert("old".to_string(), 3);
        vocab.insert("recent".to_string(), 4);
        vocab.insert("next".to_string(), 5);
        vocab.insert("summary".to_string(), 6);
        vocab.insert("context".to_string(), 7);

        let model = WordLevel::builder()
            .vocab(vocab)
            .unk_token("[UNK]".to_string())
            .build()
            .expect("word level should build");
        let mut tokenizer = Tokenizer::new(model);
        tokenizer.with_pre_tokenizer(Some(Whitespace));

        let directory = std::env::temp_dir().join(format!(
            "stellaclaw-compressor-test-{}-{}",
            std::process::id(),
            rand::random::<u64>()
        ));
        fs::create_dir_all(&directory).expect("directory should exist");
        tokenizer
            .save(directory.join("tokenizer.json"), false)
            .expect("tokenizer should save");
        fs::write(
            directory.join("tokenizer_config.json"),
            r#"{
                "chat_template": "{% for message in messages %}{{ message.role }} {{ message.content }}\n{% endfor %}",
                "bos_token": "<s>",
                "eos_token": "</s>"
            }"#,
        )
        .expect("tokenizer config should save");

        let model_config = test_model_config(
            directory
                .join("tokenizer_config.json")
                .to_string_lossy()
                .to_string(),
        );
        let resolver = HuggingFaceFileResolver::new().expect("resolver should build");
        let estimator = TokenEstimator::from_model_config(&model_config, &resolver)
            .expect("estimator should build");
        (estimator, model_config, directory)
    }

    #[test]
    fn append_with_compression_keeps_recent_tail_and_appends_next_message() {
        let (estimator, model_config, directory) = build_test_estimator();
        let compressor = SessionCompressor::new(estimator, 24, 8).expect("compressor should build");
        let seen_requests = Arc::new(Mutex::new(Vec::new()));
        let provider = SummaryProvider::new(seen_requests.clone());

        let mut messages = vec![
            ChatMessage::new(
                ChatRole::User,
                vec![ChatMessageItem::Context(ContextItem {
                    text: "old ".repeat(30),
                })],
            ),
            ChatMessage::new(
                ChatRole::Assistant,
                vec![ChatMessageItem::Context(ContextItem {
                    text: "recent context".to_string(),
                })],
            ),
        ];
        let next_message = ChatMessage::new(
            ChatRole::User,
            vec![ChatMessageItem::Context(ContextItem {
                text: "next request".to_string(),
            })],
        );

        let report = compressor
            .append_with_compression(
                &mut messages,
                next_message,
                &provider,
                &model_config,
                Some("stable compression instructions"),
                None,
            )
            .expect("append should compress");

        assert!(report.compressed);
        assert_eq!(report.compressed_message_count, 1);
        assert_eq!(messages.len(), 3);
        assert!(message_text(&messages[0]).contains(COMPRESSION_MARKER));
        assert!(message_text(&messages[0]).contains("summary of older context"));
        assert!(message_text(&messages[1]).contains("recent context"));
        assert!(message_text(&messages[2]).contains("next request"));

        let requests = seen_requests.lock().unwrap();
        assert_eq!(requests.len(), 1);
        assert_eq!(
            requests[0].system_prompt.as_deref(),
            Some("stable compression instructions")
        );
        assert!(message_text(requests[0].messages.last().unwrap())
            .contains("The conversation is nearing its context limit"));
        assert!(message_text(requests[0].messages.last().unwrap()).contains("process_id"));
        let request_body = requests[0]
            .messages
            .iter()
            .take(requests[0].messages.len().saturating_sub(1))
            .map(message_text)
            .collect::<Vec<_>>()
            .join("\n");
        assert!(request_body.contains("old"));
        assert!(!request_body.contains("recent context"));

        fs::remove_dir_all(directory).expect("test directory should be removed");
    }

    #[test]
    fn append_with_compression_retries_cyber_policy_with_feedback() {
        let (estimator, model_config, directory) = build_test_estimator();
        let compressor = SessionCompressor::new(estimator, 24, 8).expect("compressor should build");
        let seen_requests = Arc::new(Mutex::new(Vec::new()));
        let provider = CyberPolicyOnceSummaryProvider::new(seen_requests.clone(), 1);

        let mut messages = vec![
            ChatMessage::new(
                ChatRole::User,
                vec![ChatMessageItem::Context(ContextItem {
                    text: "old ".repeat(30),
                })],
            ),
            ChatMessage::new(
                ChatRole::Assistant,
                vec![ChatMessageItem::Context(ContextItem {
                    text: "recent context".to_string(),
                })],
            ),
        ];
        let next_message = ChatMessage::new(
            ChatRole::User,
            vec![ChatMessageItem::Context(ContextItem {
                text: "next request".to_string(),
            })],
        );

        let report = compressor
            .append_with_compression(
                &mut messages,
                next_message,
                &provider,
                &model_config,
                Some("stable compression instructions"),
                None,
            )
            .expect("append should retry and compress");

        assert!(report.compressed);
        assert!(message_text(&messages[0]).contains("summary after policy feedback"));
        let requests = seen_requests.lock().unwrap();
        assert_eq!(requests.len(), 2);
        let first_request = requests[0]
            .messages
            .iter()
            .map(message_text)
            .collect::<Vec<_>>()
            .join("\n");
        assert!(!first_request.contains("[Compression Retry Feedback]"));
        let retry_request = requests[1]
            .messages
            .iter()
            .map(message_text)
            .collect::<Vec<_>>()
            .join("\n");
        assert!(retry_request.contains("[Compression Retry Feedback]"));
        assert!(retry_request.contains("benign context-compaction request"));
        assert!(retry_request.contains("Do not add new cybersecurity instructions"));

        fs::remove_dir_all(directory).expect("test directory should be removed");
    }

    #[test]
    fn append_with_compression_drops_tool_results_after_cyber_policy_retry_fails() {
        let (estimator, model_config, directory) = build_test_estimator();
        let compressor = SessionCompressor::new(estimator, 24, 8).expect("compressor should build");
        let seen_requests = Arc::new(Mutex::new(Vec::new()));
        let provider = CyberPolicyOnceSummaryProvider::new(seen_requests.clone(), 2);

        let mut messages = vec![
            ChatMessage::new(
                ChatRole::Assistant,
                vec![ChatMessageItem::ToolCall(ToolCallItem {
                    tool_call_id: "call_secret".to_string(),
                    tool_name: "shell_exec".to_string(),
                    arguments: ContextItem {
                        text: "{\"cmd\":\"cat secret\"}".to_string(),
                    },
                })],
            ),
            ChatMessage::new(
                ChatRole::User,
                vec![ChatMessageItem::ToolResult(ToolResultItem {
                    tool_call_id: "call_secret".to_string(),
                    tool_name: "shell_exec".to_string(),
                    result: ToolResultContent::from_text(
                        "secret tool result should be dropped".to_string(),
                    ),
                })],
            ),
            ChatMessage::new(
                ChatRole::User,
                vec![ChatMessageItem::Context(ContextItem {
                    text: "old ".repeat(30),
                })],
            ),
            ChatMessage::new(
                ChatRole::Assistant,
                vec![ChatMessageItem::Context(ContextItem {
                    text: "recent context".to_string(),
                })],
            ),
        ];
        let next_message = ChatMessage::new(
            ChatRole::User,
            vec![ChatMessageItem::Context(ContextItem {
                text: "next request".to_string(),
            })],
        );

        let report = compressor
            .append_with_compression(
                &mut messages,
                next_message,
                &provider,
                &model_config,
                Some("stable compression instructions"),
                None,
            )
            .expect("append should retry without tool results and compress");

        assert!(report.compressed);
        assert!(message_text(&messages[0]).contains("summary after policy feedback"));
        let requests = seen_requests.lock().unwrap();
        assert_eq!(requests.len(), 3);
        let feedback_request = requests[1]
            .messages
            .iter()
            .map(message_text)
            .collect::<Vec<_>>()
            .join("\n");
        assert!(feedback_request.contains("secret tool result should be dropped"));
        let drop_tool_request = requests[2]
            .messages
            .iter()
            .map(message_text)
            .collect::<Vec<_>>()
            .join("\n");
        assert!(drop_tool_request.contains("removed all tool result content"));
        assert!(!drop_tool_request.contains("secret tool result should be dropped"));
        assert!(drop_tool_request.contains("old"));

        fs::remove_dir_all(directory).expect("test directory should be removed");
    }

    #[test]
    fn append_with_compression_drops_user_messages_after_tool_result_reduction_fails() {
        let (estimator, model_config, directory) = build_test_estimator();
        let compressor = SessionCompressor::new(estimator, 24, 8).expect("compressor should build");
        let seen_requests = Arc::new(Mutex::new(Vec::new()));
        let provider = CyberPolicyOnceSummaryProvider::new(seen_requests.clone(), 3);

        let mut messages = vec![
            ChatMessage::new(
                ChatRole::Assistant,
                vec![ChatMessageItem::Context(ContextItem {
                    text: "assistant context survives ".repeat(30),
                })],
            ),
            ChatMessage::new(
                ChatRole::User,
                vec![ChatMessageItem::Context(ContextItem {
                    text: "user text should be dropped ".repeat(30),
                })],
            ),
            ChatMessage::new(
                ChatRole::Assistant,
                vec![ChatMessageItem::Context(ContextItem {
                    text: "assistant context survives".to_string(),
                })],
            ),
        ];
        let next_message = ChatMessage::new(
            ChatRole::User,
            vec![ChatMessageItem::Context(ContextItem {
                text: "next request".to_string(),
            })],
        );

        let report = compressor
            .append_with_compression(
                &mut messages,
                next_message,
                &provider,
                &model_config,
                Some("stable compression instructions"),
                None,
            )
            .expect("append should retry without user messages and compress");

        assert!(report.compressed);
        assert!(message_text(&messages[0]).contains("summary after policy feedback"));
        let requests = seen_requests.lock().unwrap();
        assert_eq!(requests.len(), 4);
        let drop_user_request = requests[3]
            .messages
            .iter()
            .map(message_text)
            .collect::<Vec<_>>()
            .join("\n");
        assert!(drop_user_request
            .contains("removed all tool result content and all user-role history messages"));
        assert!(!drop_user_request.contains("user text should be dropped"));
        assert!(drop_user_request.contains("assistant context survives"));

        fs::remove_dir_all(directory).expect("test directory should be removed");
    }

    #[test]
    fn append_with_compression_drops_old_context_after_cyber_policy_reductions_fail() {
        let (estimator, model_config, directory) = build_test_estimator();
        let compressor = SessionCompressor::new(estimator, 24, 8).expect("compressor should build");
        let seen_requests = Arc::new(Mutex::new(Vec::new()));
        let provider = CyberPolicyOnceSummaryProvider::new(seen_requests.clone(), 4);

        let mut messages = vec![
            ChatMessage::new(
                ChatRole::User,
                vec![ChatMessageItem::Context(ContextItem {
                    text: "old ".repeat(30),
                })],
            ),
            ChatMessage::new(
                ChatRole::Assistant,
                vec![ChatMessageItem::Context(ContextItem {
                    text: "recent context".to_string(),
                })],
            ),
        ];
        let next_message = ChatMessage::new(
            ChatRole::User,
            vec![ChatMessageItem::Context(ContextItem {
                text: "next request".to_string(),
            })],
        );

        let report = compressor
            .append_with_compression(
                &mut messages,
                next_message,
                &provider,
                &model_config,
                Some("stable compression instructions"),
                None,
            )
            .expect("cyber policy reduction failure should drop old context");

        assert!(report.compressed);
        assert_eq!(messages.len(), 2);
        assert!(!messages
            .iter()
            .any(|message| message_text(message).contains(COMPRESSION_MARKER)));
        assert!(!messages
            .iter()
            .any(|message| message_text(message).contains("old old old")));
        assert!(message_text(&messages[0]).contains("recent context"));
        assert!(message_text(&messages[1]).contains("next request"));
        assert_eq!(seen_requests.lock().unwrap().len(), 4);

        fs::remove_dir_all(directory).expect("test directory should be removed");
    }

    #[test]
    fn compression_request_includes_optional_memory_context() {
        let (estimator, model_config, directory) = build_test_estimator();
        let compressor = SessionCompressor::new(estimator, 24, 8).expect("compressor should build");
        let seen_requests = Arc::new(Mutex::new(Vec::new()));
        let provider = SummaryProvider::new(seen_requests.clone());
        let mut messages = vec![
            ChatMessage::new(
                ChatRole::User,
                vec![ChatMessageItem::Context(ContextItem {
                    text: "old ".repeat(30),
                })],
            ),
            ChatMessage::new(
                ChatRole::Assistant,
                vec![ChatMessageItem::Context(ContextItem {
                    text: "recent context".to_string(),
                })],
            ),
        ];
        let next_message = ChatMessage::new(
            ChatRole::User,
            vec![ChatMessageItem::Context(ContextItem {
                text: "next request".to_string(),
            })],
        );

        compressor
            .append_with_compression(
                &mut messages,
                next_message,
                &provider,
                &model_config,
                None,
                Some("* [conversation:c_1] Project A background"),
            )
            .expect("append should compress");

        let requests = seen_requests.lock().unwrap();
        let rendered = requests[0]
            .messages
            .iter()
            .map(message_text)
            .collect::<Vec<_>>()
            .join("\n");
        assert!(rendered.contains("[Relevant Long Memory For Compression]"));
        assert!(rendered.contains("Project A background"));
        fs::remove_dir_all(directory).expect("test directory should be removed");
    }

    #[test]
    fn structured_compression_renders_sections_and_preserves_selected_tool_pair() {
        let (estimator, model_config, directory) = build_test_estimator();
        let compressor = SessionCompressor::new(estimator, 24, 8).expect("compressor should build");
        let seen_requests = Arc::new(Mutex::new(Vec::new()));
        let provider = SummaryProvider::with_response(
            seen_requests,
            r#"{
                "summary": "Read the config and found memory disabled.",
                "current_state": "Need to update docs next.",
                "plan": "Patch TODO.md and run tests.",
                "active_tasks": [{
                    "id": "sh_test_123",
                    "kind": "shell",
                    "status": "running",
                    "purpose": "cargo test is still running in /tmp/project",
                    "next_action": "poll with shell_write_stdin using process_id sh_test_123"
                }],
                "preserved_tool_call_ids": ["call_keep", "missing_call"]
            }"#,
        );
        let mut messages = vec![
            ChatMessage::new(
                ChatRole::Assistant,
                vec![ChatMessageItem::ToolCall(ToolCallItem {
                    tool_call_id: "call_keep".to_string(),
                    tool_name: "shell_exec".to_string(),
                    arguments: ContextItem {
                        text: r#"{"file_path":"TODO.md"}"#.to_string(),
                    },
                })],
            ),
            ChatMessage::new(
                ChatRole::User,
                vec![ChatMessageItem::ToolResult(ToolResultItem {
                    tool_call_id: "call_keep".to_string(),
                    tool_name: "shell_exec".to_string(),
                    result: ToolResultContent::from_text("memory disabled in config".to_string()),
                })],
            ),
            ChatMessage::new(
                ChatRole::Assistant,
                vec![ChatMessageItem::Context(ContextItem {
                    text: "old ".repeat(30),
                })],
            ),
            ChatMessage::new(
                ChatRole::Assistant,
                vec![ChatMessageItem::Context(ContextItem {
                    text: "recent context".to_string(),
                })],
            ),
        ];
        let next_message = ChatMessage::new(
            ChatRole::User,
            vec![ChatMessageItem::Context(ContextItem {
                text: "next request".to_string(),
            })],
        );

        compressor
            .append_with_compression(
                &mut messages,
                next_message,
                &provider,
                &model_config,
                None,
                None,
            )
            .expect("append should compress");

        let summary_text = message_text(&messages[0]);
        assert!(summary_text.contains("Summary:\nRead the config"));
        assert!(summary_text.contains("Current State:\nNeed to update docs next."));
        assert!(summary_text.contains("Plan:\nPatch TODO.md and run tests."));
        assert!(summary_text.contains("Active Tasks:"));
        assert!(summary_text.contains("id: sh_test_123"));
        assert!(summary_text.contains("kind: shell"));
        assert!(summary_text.contains("process_id sh_test_123"));
        assert!(matches!(
            messages[1].data.first(),
            Some(ChatMessageItem::ToolCall(tool_call)) if tool_call.tool_call_id == "call_keep"
        ));
        assert!(matches!(
            messages[2].data.first(),
            Some(ChatMessageItem::ToolResult(tool_result))
                if tool_result.tool_call_id == "call_keep"
                    && crate::session_actor::tool_result_text(tool_result).contains("memory disabled")
        ));
        assert!(!messages
            .iter()
            .flat_map(|message| message.data.iter())
            .any(|item| matches!(
                item,
                ChatMessageItem::ToolCall(tool_call)
                    if tool_call.tool_call_id == "missing_call"
            )));
        fs::remove_dir_all(directory).expect("test directory should be removed");
    }

    #[test]
    fn compression_fails_when_provider_does_not_return_json() {
        let (estimator, model_config, directory) = build_test_estimator();
        let compressor = SessionCompressor::new(estimator, 24, 8).expect("compressor should build");
        let provider = SummaryProvider::with_response(
            Arc::new(Mutex::new(Vec::new())),
            "summary of older context",
        );
        let mut messages = vec![
            ChatMessage::new(
                ChatRole::User,
                vec![ChatMessageItem::Context(ContextItem {
                    text: "old ".repeat(30),
                })],
            ),
            ChatMessage::new(
                ChatRole::Assistant,
                vec![ChatMessageItem::Context(ContextItem {
                    text: "recent context".to_string(),
                })],
            ),
        ];
        let next_message = ChatMessage::new(
            ChatRole::User,
            vec![ChatMessageItem::Context(ContextItem {
                text: "next request".to_string(),
            })],
        );

        let error = compressor
            .append_with_compression(
                &mut messages,
                next_message,
                &provider,
                &model_config,
                None,
                None,
            )
            .expect_err("plain text compression response should fail");

        assert!(matches!(error, CompressionError::InvalidSummaryJson(_)));
        fs::remove_dir_all(directory).expect("test directory should be removed");
    }

    #[test]
    fn compression_request_sanitizes_tool_protocol_and_files() {
        let messages = vec![
            ChatMessage::new(
                ChatRole::Assistant,
                vec![
                    ChatMessageItem::ToolCall(ToolCallItem {
                        tool_call_id: "call_1".to_string(),
                        tool_name: "shell_exec".to_string(),
                        arguments: ContextItem {
                            text: r#"{"file_path":"src/lib.rs"}"#.to_string(),
                        },
                    }),
                    ChatMessageItem::File(FileItem {
                        uri: "file:///tmp/image.png".to_string(),
                        name: Some("image.png".to_string()),
                        media_type: Some("image/png".to_string()),
                        width: Some(32),
                        height: Some(32),
                        state: None,
                    }),
                ],
            ),
            ChatMessage::new(
                ChatRole::Assistant,
                vec![ChatMessageItem::ToolResult(ToolResultItem {
                    tool_call_id: "call_1".to_string(),
                    tool_name: "shell_exec".to_string(),
                    result: ToolResultContent::from_text("loaded".to_string()).with_file(
                        FileItem {
                            uri: "file:///tmp/report.pdf".to_string(),
                            name: Some("report.pdf".to_string()),
                            media_type: Some("application/pdf".to_string()),
                            width: None,
                            height: None,
                            state: None,
                        },
                    ),
                })],
            ),
        ];

        let sanitized = sanitize_messages_for_compression_request(&messages);
        let text = sanitized
            .iter()
            .map(message_text)
            .collect::<Vec<_>>()
            .join("\n");

        assert!(text.contains("[tool call: shell_exec id=call_1]"));
        assert!(text.contains(r#"{"file_path":"src/lib.rs"}"#));
        assert!(text.contains("[file omitted during context compression: image.png]"));
        assert!(text.contains("[tool result: shell_exec id=call_1]"));
        assert!(text.contains("loaded"));
        assert!(text.contains("[file omitted during context compression: report.pdf]"));
        assert!(sanitized.iter().all(|message| {
            message
                .data
                .iter()
                .all(|item| matches!(item, ChatMessageItem::Context(_)))
        }));
    }

    #[test]
    fn compression_request_skips_runtime_update_messages() {
        let messages = vec![
            ChatMessage::new(
                ChatRole::User,
                vec![ChatMessageItem::Context(ContextItem {
                    text: "[Runtime Prompt Updates]\nIgnore me".to_string(),
                })],
            ),
            ChatMessage::new(
                ChatRole::User,
                vec![ChatMessageItem::Context(ContextItem {
                    text: "[Incoming User Metadata]\nSpeaker: alice".to_string(),
                })],
            ),
            ChatMessage::new(
                ChatRole::Assistant,
                vec![ChatMessageItem::Context(ContextItem {
                    text: "real stable content".to_string(),
                })],
            ),
        ];

        let sanitized = sanitize_messages_for_compression_request(&messages);

        assert_eq!(sanitized.len(), 1);
        assert!(message_text(&sanitized[0]).contains("real stable content"));
    }

    #[test]
    fn recent_tail_split_moves_before_tool_result_origin() {
        let (estimator, _model_config, directory) = build_test_estimator();
        let messages = vec![
            ChatMessage::new(
                ChatRole::User,
                vec![ChatMessageItem::Context(ContextItem {
                    text: "old ".repeat(40),
                })],
            ),
            ChatMessage::new(
                ChatRole::Assistant,
                vec![ChatMessageItem::ToolCall(ToolCallItem {
                    tool_call_id: "call_1".to_string(),
                    tool_name: "shell".to_string(),
                    arguments: ContextItem {
                        text: "{\"command\":\"echo hi\"}".to_string(),
                    },
                })],
            ),
            ChatMessage::new(
                ChatRole::Assistant,
                vec![ChatMessageItem::ToolResult(ToolResultItem {
                    tool_call_id: "call_1".to_string(),
                    tool_name: "shell".to_string(),
                    result: ToolResultContent::from_text("hi".to_string()),
                })],
            ),
        ];

        let split_index =
            recent_tail_start_by_token_budget(&messages, 4, &estimator).expect("split works");

        assert_eq!(split_index, 1);

        fs::remove_dir_all(directory).expect("test directory should be removed");
    }
}
