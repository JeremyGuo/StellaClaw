use thiserror::Error;

use crate::{
    model_config::ModelConfig,
    providers::{Provider, ProviderRequest},
};

use super::{
    ChatMessage, ChatMessageItem, ChatRole, ContextItem, FileItem, TokenEstimator,
    TokenEstimatorError, ToolResultContent,
};

pub const COMPRESSION_MARKER: &str = "[PartyClaw Context Compression]";

#[derive(Debug)]
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

        let summary_message =
            self.generate_summary_message(messages, &plan, provider, model_config)?;
        let mut compressed_messages = vec![summary_message];
        compressed_messages.extend_from_slice(&messages[plan.recent_start..]);
        let compressed_message_count = plan.recent_start;
        let retained_message_count = compressed_messages.len().saturating_sub(1);

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

    fn generate_summary_message(
        &self,
        messages: &[ChatMessage],
        plan: &CompressionPlan,
        provider: &(dyn Provider + Send + Sync),
        model_config: &ModelConfig,
    ) -> Result<ChatMessage, CompressionError> {
        let mut request_messages = sanitize_messages_for_compression_request(messages);
        request_messages.push(ChatMessage::new(
            ChatRole::User,
            vec![ChatMessageItem::Context(ContextItem {
                text: compression_instruction(messages.len() - plan.recent_start),
            })],
        ));

        let summary = provider
            .send(model_config, ProviderRequest::new(&request_messages))
            .map_err(|error| CompressionError::Provider(error.to_string()))?;
        let summary_text = message_text(&summary);
        if summary_text.trim().is_empty() {
            return Err(CompressionError::EmptySummary);
        }

        Ok(ChatMessage::new(
            ChatRole::Assistant,
            vec![ChatMessageItem::Context(ContextItem {
                text: format!(
                    "{COMPRESSION_MARKER}\n\nOlder conversation history has been compressed into the summary below.\n\n{}",
                    summary_text.trim()
                ),
            })],
        ))
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
}

struct CompressionPlan {
    recent_start: usize,
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
        let candidate_start = start - 1;
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

fn sanitize_messages_for_compression_request(messages: &[ChatMessage]) -> Vec<ChatMessage> {
    messages
        .iter()
        .map(sanitize_message_for_compression_request)
        .collect()
}

fn sanitize_message_for_compression_request(message: &ChatMessage) -> ChatMessage {
    let mut data = Vec::new();
    for item in &message.data {
        match item {
            ChatMessageItem::Reasoning(_) => {}
            ChatMessageItem::Context(context) => {
                data.push(ChatMessageItem::Context(context.clone()))
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

    ChatMessage {
        role: message.role.clone(),
        token_usage: None,
        data,
    }
}

fn compression_instruction(preserved_recent_count: usize) -> String {
    format!(
        "Compress the older conversation history in this same transcript.\n\n\
Return a concise, factual summary that is useful for continuing the session.\n\n\
Rules:\n\
- preserve concrete decisions, file paths, commands, errors, ids, URLs, and current next steps\n\
- redact long secrets; mention that a secret was provided without copying the full value\n\
- do not invent details\n\
- intermediate tool calls and tool results should be summarized by outcome, not replayed step by step\n\
- the most recent {preserved_recent_count} message(s) immediately before this request are preserved separately as high-fidelity context; do not summarize them unless continuity requires a short pointer\n\
- return plain text only"
    )
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
    if let Some(context) = &result.context {
        if !context.text.trim().is_empty() {
            parts.push(context.text.clone());
        }
    }
    if let Some(file) = &result.file {
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
        providers::ProviderError,
        session_actor::{FileItem, ToolCallItem, ToolResultContent, ToolResultItem},
    };

    struct SummaryProvider {
        seen_requests: Arc<Mutex<Vec<Vec<ChatMessage>>>>,
    }

    impl SummaryProvider {
        fn new(seen_requests: Arc<Mutex<Vec<Vec<ChatMessage>>>>) -> Self {
            Self { seen_requests }
        }
    }

    impl Provider for SummaryProvider {
        fn send(
            &self,
            _model_config: &ModelConfig,
            request: ProviderRequest<'_>,
        ) -> Result<ChatMessage, ProviderError> {
            self.seen_requests
                .lock()
                .unwrap()
                .push(request.messages.to_vec());
            Ok(ChatMessage::new(
                ChatRole::Assistant,
                vec![ChatMessageItem::Context(ContextItem {
                    text: "summary of older context".to_string(),
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
            cache_timeout: 300,
            conn_timeout: 10,
            retry_mode: RetryMode::Once,
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
            "claw-party-compressor-test-{}-{}",
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
            .append_with_compression(&mut messages, next_message, &provider, &model_config)
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
        assert!(message_text(requests[0].last().unwrap()).contains("Compress the older"));

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
                        tool_name: "file_read".to_string(),
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
                    tool_name: "file_read".to_string(),
                    result: ToolResultContent {
                        context: Some(ContextItem {
                            text: "loaded".to_string(),
                        }),
                        file: Some(FileItem {
                            uri: "file:///tmp/report.pdf".to_string(),
                            name: Some("report.pdf".to_string()),
                            media_type: Some("application/pdf".to_string()),
                            width: None,
                            height: None,
                            state: None,
                        }),
                    },
                })],
            ),
        ];

        let sanitized = sanitize_messages_for_compression_request(&messages);
        let text = sanitized
            .iter()
            .map(message_text)
            .collect::<Vec<_>>()
            .join("\n");

        assert!(text.contains("[tool call: file_read id=call_1]"));
        assert!(text.contains(r#"{"file_path":"src/lib.rs"}"#));
        assert!(text.contains("[file omitted during context compression: image.png]"));
        assert!(text.contains("[tool result: file_read id=call_1]"));
        assert!(text.contains("loaded"));
        assert!(text.contains("[file omitted during context compression: report.pdf]"));
        assert!(sanitized.iter().all(|message| {
            message
                .data
                .iter()
                .all(|item| matches!(item, ChatMessageItem::Context(_)))
        }));
    }
}
