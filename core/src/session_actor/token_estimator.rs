use jinja::{context, new_jinja2};
use serde::{Deserialize, Serialize};
use thiserror::Error;
use tiktoken_rs::{
    num_tokens_from_messages, tokenizer::get_tokenizer as get_tiktoken_tokenizer,
    ChatCompletionRequestMessage, FunctionCall,
};
use tokenizers::Tokenizer as HuggingFaceTokenizer;

use crate::{
    huggingface::{
        resolve_tokenizer_assets, HuggingFaceFileResolver, ResolveModelFileError,
        ResolveTokenizerAssetsError,
    },
    model_config::{ModelConfig, TokenEstimatorType},
};

use super::{normalize_messages_for_model, ChatMessage, ChatMessageItem, ChatRole, FileItem};

const FALLBACK_TIKTOKEN_MODEL: &str = "gpt-4o";

#[derive(Debug, Clone)]
pub struct TokenEstimator {
    backend: TokenEstimatorBackend,
    multimodal_strategy: MultimodalTokenStrategy,
    model_config: ModelConfig,
}

impl TokenEstimator {
    fn new(
        backend: TokenEstimatorBackend,
        multimodal_strategy: MultimodalTokenStrategy,
        model_config: ModelConfig,
    ) -> Self {
        Self {
            backend,
            multimodal_strategy,
            model_config,
        }
    }

    pub fn from_model_config(
        model_config: &ModelConfig,
        file_resolver: &HuggingFaceFileResolver,
    ) -> Result<Self, TokenEstimatorError> {
        let multimodal_strategy = model_config
            .multimodal_estimator
            .as_ref()
            .and_then(|config| config.image)
            .unwrap_or_else(default_multimodal_image_token_strategy);

        let backend = match model_config.token_estimator_type {
            TokenEstimatorType::Local => TokenEstimatorBackend::OpenAiTiktoken(
                OpenAiTiktokenEstimator::from_model_name(&model_config.model_name)?,
            ),
            TokenEstimatorType::HuggingFace => TokenEstimatorBackend::HuggingFace(
                build_huggingface_estimator(model_config, file_resolver)?,
            ),
        };

        Ok(Self::new(
            backend,
            multimodal_strategy,
            model_config.clone(),
        ))
    }

    pub fn estimate(&self, messages: &[ChatMessage]) -> Result<TokenEstimate, TokenEstimatorError> {
        let normalized_messages = normalize_messages_for_model(messages, &self.model_config);
        let rendered = self.backend.estimate_text(&normalized_messages)?;
        let text_tokens = rendered.text_tokens;
        let mut multimodal_tokens = 0;
        for file in &rendered.files {
            multimodal_tokens += self.multimodal_strategy.estimate(file)?;
        }
        let reasoning_tokens = estimate_codex_encrypted_reasoning_tokens(&normalized_messages);

        Ok(TokenEstimate {
            text_tokens,
            multimodal_tokens,
            reasoning_tokens,
            total_tokens: text_tokens + multimodal_tokens + reasoning_tokens,
        })
    }
}

#[derive(Debug, Clone)]
enum TokenEstimatorBackend {
    HuggingFace(HuggingFaceEstimator),
    OpenAiTiktoken(OpenAiTiktokenEstimator),
}

impl TokenEstimatorBackend {
    fn estimate_text(
        &self,
        messages: &[ChatMessage],
    ) -> Result<RenderedTextEstimate, TokenEstimatorError> {
        match self {
            Self::HuggingFace(estimator) => estimator.estimate_text(messages),
            Self::OpenAiTiktoken(estimator) => estimator.estimate_text(messages),
        }
    }
}

#[derive(Debug, Clone)]
struct HuggingFaceEstimator {
    tokenizer: HuggingFaceTokenizer,
    template: JinjaChatTemplate,
}

impl HuggingFaceEstimator {
    fn estimate_text(
        &self,
        messages: &[ChatMessage],
    ) -> Result<RenderedTextEstimate, TokenEstimatorError> {
        let rendered = self.template.render(messages)?;
        let encoding = self
            .tokenizer
            .encode(rendered.text, true)
            .map_err(TokenEstimatorError::Tokenize)?;

        Ok(RenderedTextEstimate {
            text_tokens: encoding.len() as u64,
            files: rendered.files,
        })
    }
}

fn build_huggingface_estimator(
    model_config: &ModelConfig,
    file_resolver: &HuggingFaceFileResolver,
) -> Result<HuggingFaceEstimator, TokenEstimatorError> {
    let token_estimator_url = model_config
        .token_estimator_url
        .as_deref()
        .ok_or(TokenEstimatorError::MissingTokenEstimatorUrl)?;
    let resolved_assets = resolve_tokenizer_assets(token_estimator_url, file_resolver)?;

    let template_source = resolved_assets
        .chat_template
        .ok_or(TokenEstimatorError::MissingChatTemplate)?;
    let mut template =
        JinjaChatTemplate::from_source(template_source).with_add_generation_prompt(true);

    if let Some(bos_token) = resolved_assets.bos_token {
        template = template.with_bos_token(bos_token);
    }
    if let Some(eos_token) = resolved_assets.eos_token {
        template = template.with_eos_token(eos_token);
    }

    let tokenizer_path = file_resolver.resolve(&resolved_assets.tokenizer_source)?;
    let tokenizer = HuggingFaceTokenizer::from_file(tokenizer_path)
        .map_err(TokenEstimatorError::LoadTokenizer)?;

    Ok(HuggingFaceEstimator {
        tokenizer,
        template,
    })
}

#[derive(Debug, Clone)]
struct OpenAiTiktokenEstimator {
    model_name: String,
}

impl OpenAiTiktokenEstimator {
    fn from_model_name(model_name: &str) -> Result<Self, TokenEstimatorError> {
        let normalized = normalize_tiktoken_model_name(model_name).ok_or_else(|| {
            TokenEstimatorError::UnsupportedLocalTokenEstimatorModel {
                model_name: model_name.to_string(),
            }
        })?;

        Ok(Self {
            model_name: normalized,
        })
    }

    fn estimate_text(
        &self,
        messages: &[ChatMessage],
    ) -> Result<RenderedTextEstimate, TokenEstimatorError> {
        let chat_messages = messages
            .iter()
            .map(openai_chat_message_from_chat_message)
            .collect::<Vec<_>>();
        let text_tokens = num_tokens_from_messages(&self.model_name, &chat_messages)
            .map_err(|error| TokenEstimatorError::LocalTokenEstimate(error.to_string()))?
            as u64;

        Ok(RenderedTextEstimate {
            text_tokens,
            files: collect_all_files(messages),
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct RenderedTextEstimate {
    text_tokens: u64,
    files: Vec<FileItem>,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MultimodalTokenStrategy {
    Ignore,
    FixedTokens {
        tokens_per_file: u64,
    },
    PatchGrid {
        patch_size: u32,
        patch_budget: u32,
        multiplier: f64,
    },
    TileGrid {
        low_detail_tokens: u64,
        base_tokens: u64,
        tokens_per_tile: u64,
        max_dimension: u32,
        target_shortest_side: u32,
        tile_size: u32,
        detail: VisionDetail,
    },
    AreaApprox {
        pixels_per_token: u64,
        max_tokens: u64,
        max_long_edge: u32,
        pad_to_multiple: u32,
    },
    ProviderDerived,
}

impl MultimodalTokenStrategy {
    pub fn estimate(self, file: &FileItem) -> Result<u64, TokenEstimatorError> {
        if matches!(self, Self::Ignore) {
            return Ok(0);
        }

        if let Some(media_type) = unsupported_token_estimate_media_type(file) {
            return Err(TokenEstimatorError::UnsupportedMediaTokenEstimate {
                media_type: media_type.to_string(),
            });
        }

        let tokens = match self {
            Self::Ignore => unreachable!("ignore strategy returns before media token estimation"),
            Self::FixedTokens { tokens_per_file } => tokens_per_file,
            Self::PatchGrid {
                patch_size,
                patch_budget,
                multiplier,
            } => estimate_patch_tokens(file, patch_size, patch_budget, multiplier),
            Self::TileGrid {
                low_detail_tokens,
                base_tokens,
                tokens_per_tile,
                max_dimension,
                target_shortest_side,
                tile_size,
                detail,
            } => estimate_tile_tokens(
                file,
                low_detail_tokens,
                base_tokens,
                tokens_per_tile,
                max_dimension,
                target_shortest_side,
                tile_size,
                detail,
            ),
            Self::AreaApprox {
                pixels_per_token,
                max_tokens,
                max_long_edge,
                pad_to_multiple,
            } => estimate_anthropic_tokens(
                file,
                pixels_per_token,
                max_tokens,
                max_long_edge,
                pad_to_multiple,
            ),
            Self::ProviderDerived => {
                return Err(TokenEstimatorError::ProviderDerivedStrategyRequiresRemoteCount)
            }
        };

        Ok(tokens)
    }
}

fn unsupported_token_estimate_media_type(file: &FileItem) -> Option<&str> {
    let media_type = file.media_type.as_deref()?;
    if media_type == "application/pdf" || media_type.starts_with("audio/") {
        Some(media_type)
    } else {
        None
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VisionDetail {
    Low,
    High,
}

fn default_multimodal_image_token_strategy() -> MultimodalTokenStrategy {
    MultimodalTokenStrategy::TileGrid {
        low_detail_tokens: 85,
        base_tokens: 85,
        tokens_per_tile: 170,
        max_dimension: 2048,
        target_shortest_side: 768,
        tile_size: 512,
        detail: VisionDetail::High,
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TokenEstimate {
    pub text_tokens: u64,
    pub multimodal_tokens: u64,
    pub reasoning_tokens: u64,
    pub total_tokens: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RenderedChatPrompt {
    pub text: String,
    pub files: Vec<FileItem>,
}

pub trait ChatTemplate {
    fn render(&self, messages: &[ChatMessage]) -> Result<RenderedChatPrompt, ChatTemplateError>;
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JinjaChatTemplate {
    source: String,
    bos_token: Option<String>,
    eos_token: Option<String>,
    add_generation_prompt: bool,
}

impl JinjaChatTemplate {
    pub fn from_source(source: impl Into<String>) -> Self {
        Self {
            source: source.into(),
            bos_token: None,
            eos_token: None,
            add_generation_prompt: false,
        }
    }

    pub fn with_bos_token(mut self, bos_token: impl Into<String>) -> Self {
        self.bos_token = Some(bos_token.into());
        self
    }

    pub fn with_eos_token(mut self, eos_token: impl Into<String>) -> Self {
        self.eos_token = Some(eos_token.into());
        self
    }

    pub fn with_add_generation_prompt(mut self, add_generation_prompt: bool) -> Self {
        self.add_generation_prompt = add_generation_prompt;
        self
    }
}

impl ChatTemplate for JinjaChatTemplate {
    fn render(&self, messages: &[ChatMessage]) -> Result<RenderedChatPrompt, ChatTemplateError> {
        let mut environment = new_jinja2();
        let normalized_source = normalize_huggingface_chat_template(&self.source);
        environment
            .add_template("chat", &normalized_source)
            .map_err(ChatTemplateError::Compile)?;
        let template = environment
            .get_template("chat")
            .map_err(ChatTemplateError::Compile)?;

        let template_messages = messages
            .iter()
            .map(JinjaTemplateMessage::from_chat_message)
            .collect::<Vec<_>>();
        let rendered = template
            .render(context! {
                messages => template_messages,
                bos_token => self.bos_token.as_deref().unwrap_or(""),
                eos_token => self.eos_token.as_deref().unwrap_or(""),
                add_generation_prompt => self.add_generation_prompt,
            })
            .map_err(ChatTemplateError::Render)?;

        Ok(RenderedChatPrompt {
            text: rendered,
            files: collect_all_files(messages),
        })
    }
}

fn normalize_huggingface_chat_template(source: &str) -> String {
    let mut normalized = String::with_capacity(source.len());
    let mut remaining = source;

    while let Some(start) = find_next_jinja_tag_start(remaining) {
        normalized.push_str(&remaining[..start]);
        let tag = &remaining[start..start + 2];
        let end_delimiter = if tag == "{{" { "}}" } else { "%}" };

        if let Some(end) = remaining[start + 2..].find(end_delimiter) {
            let tag_end = start + 2 + end + 2;
            normalized.push_str(&normalize_numeric_attr_lookup_in_jinja_tag(
                &remaining[start..tag_end],
            ));
            remaining = &remaining[tag_end..];
        } else {
            normalized.push_str(&remaining[start..]);
            remaining = "";
        }
    }

    normalized.push_str(remaining);
    normalized
}

fn find_next_jinja_tag_start(input: &str) -> Option<usize> {
    let expression = input.find("{{");
    let block = input.find("{%");
    match (expression, block) {
        (Some(a), Some(b)) => Some(a.min(b)),
        (Some(a), None) => Some(a),
        (None, Some(b)) => Some(b),
        (None, None) => None,
    }
}

fn normalize_numeric_attr_lookup_in_jinja_tag(tag: &str) -> String {
    let chars = tag.chars().collect::<Vec<_>>();
    let mut normalized = String::with_capacity(tag.len());
    let mut index = 0;
    let mut quote: Option<char> = None;

    while index < chars.len() {
        let current = chars[index];
        if let Some(active_quote) = quote {
            normalized.push(current);
            if current == active_quote && !is_escaped(&chars, index) {
                quote = None;
            }
            index += 1;
            continue;
        }

        if current == '\'' || current == '"' {
            quote = Some(current);
            normalized.push(current);
            index += 1;
            continue;
        }

        if current == '.'
            && index > 0
            && is_numeric_attr_lookup_receiver(chars[index - 1])
            && chars
                .get(index + 1)
                .is_some_and(|next| next.is_ascii_digit())
        {
            let mut digit_end = index + 1;
            while digit_end < chars.len() && chars[digit_end].is_ascii_digit() {
                digit_end += 1;
            }
            normalized.push('[');
            for digit in &chars[index + 1..digit_end] {
                normalized.push(*digit);
            }
            normalized.push(']');
            index = digit_end;
            continue;
        }

        normalized.push(current);
        index += 1;
    }

    normalized
}

fn is_numeric_attr_lookup_receiver(ch: char) -> bool {
    ch == '_' || ch == ')' || ch == ']' || ch.is_ascii_alphabetic()
}

fn is_escaped(chars: &[char], index: usize) -> bool {
    let mut slash_count = 0;
    let mut cursor = index;
    while cursor > 0 && chars[cursor - 1] == '\\' {
        slash_count += 1;
        cursor -= 1;
    }
    slash_count % 2 == 1
}

#[derive(Debug, Clone, Serialize)]
struct JinjaTemplateMessage {
    role: String,
    content: String,
    data: Vec<ChatMessageItem>,
    files: Vec<FileItem>,
}

impl JinjaTemplateMessage {
    fn from_chat_message(message: &ChatMessage) -> Self {
        Self {
            role: role_name(&message.role).to_string(),
            content: render_message_content(message),
            data: message.data.clone(),
            files: collect_message_files(message),
        }
    }
}

fn render_message_content(message: &ChatMessage) -> String {
    let mut sections = Vec::new();

    for item in &message.data {
        match item {
            ChatMessageItem::Reasoning(_) => {}
            ChatMessageItem::Context(context) => {
                sections.push(context.text.clone());
            }
            ChatMessageItem::File(_) => {}
            ChatMessageItem::ToolCall(tool_call) => {
                sections.push(format!(
                    "<tool_call name=\"{}\">{}</tool_call>",
                    tool_call.tool_name, tool_call.arguments.text
                ));
            }
            ChatMessageItem::ToolResult(tool_result) => {
                if let Some(context) = &tool_result.result.context {
                    sections.push(context.text.clone());
                }
            }
        }
    }

    sections.join("\n")
}

fn render_openai_message_content(message: &ChatMessage) -> String {
    let mut sections = Vec::new();

    for item in &message.data {
        match item {
            ChatMessageItem::Reasoning(_)
            | ChatMessageItem::File(_)
            | ChatMessageItem::ToolCall(_) => {}
            ChatMessageItem::Context(context) => {
                sections.push(context.text.clone());
            }
            ChatMessageItem::ToolResult(tool_result) => {
                if let Some(context) = &tool_result.result.context {
                    sections.push(context.text.clone());
                }
            }
        }
    }

    sections.join("\n")
}

fn openai_chat_message_from_chat_message(message: &ChatMessage) -> ChatCompletionRequestMessage {
    let content = render_openai_message_content(message);
    let tool_calls = message
        .data
        .iter()
        .filter_map(|item| match item {
            ChatMessageItem::ToolCall(tool_call) => Some(FunctionCall {
                name: tool_call.tool_name.clone(),
                arguments: tool_call.arguments.text.clone(),
            }),
            _ => None,
        })
        .collect();

    ChatCompletionRequestMessage {
        role: role_name(&message.role).to_string(),
        content: if content.is_empty() {
            None
        } else {
            Some(content)
        },
        tool_calls,
        ..Default::default()
    }
}

fn normalize_tiktoken_model_name(model_name: &str) -> Option<String> {
    for candidate in tiktoken_model_name_candidates(model_name) {
        if get_tiktoken_tokenizer(&candidate).is_some() {
            return Some(candidate);
        }
    }
    Some(FALLBACK_TIKTOKEN_MODEL.to_string())
}

fn tiktoken_model_name_candidates(model_name: &str) -> Vec<String> {
    let mut candidates = Vec::new();
    push_unique_candidate(&mut candidates, model_name);

    if let Some((_, model_without_provider)) = model_name.rsplit_once('/') {
        push_unique_candidate(&mut candidates, model_without_provider);
    }

    if let Some(model_without_suffix) = model_name.strip_suffix(":free") {
        push_unique_candidate(&mut candidates, model_without_suffix);
        if let Some((_, model_without_provider)) = model_without_suffix.rsplit_once('/') {
            push_unique_candidate(&mut candidates, model_without_provider);
        }
    }

    candidates
}

fn push_unique_candidate(candidates: &mut Vec<String>, candidate: &str) {
    if !candidate.is_empty() && !candidates.iter().any(|existing| existing == candidate) {
        candidates.push(candidate.to_string());
    }
}

fn collect_message_files(message: &ChatMessage) -> Vec<FileItem> {
    let mut files = Vec::new();

    for item in &message.data {
        match item {
            ChatMessageItem::File(file) => files.push(file.clone()),
            ChatMessageItem::ToolResult(tool_result) => {
                if let Some(file) = &tool_result.result.file {
                    files.push(file.clone());
                }
            }
            ChatMessageItem::Reasoning(_)
            | ChatMessageItem::Context(_)
            | ChatMessageItem::ToolCall(_) => {}
        }
    }

    files
}

fn collect_all_files(messages: &[ChatMessage]) -> Vec<FileItem> {
    let mut files = Vec::new();
    for message in messages {
        files.extend(collect_message_files(message));
    }
    files
}

fn estimate_codex_encrypted_reasoning_tokens(messages: &[ChatMessage]) -> u64 {
    messages
        .iter()
        .flat_map(|message| &message.data)
        .filter_map(|item| match item {
            ChatMessageItem::Reasoning(reasoning) => reasoning.codex_encrypted_content.as_deref(),
            _ => None,
        })
        .map(estimate_codex_encrypted_reasoning_item_tokens)
        .sum()
}

fn estimate_codex_encrypted_reasoning_item_tokens(encrypted_content: &str) -> u64 {
    let visible_bytes = encrypted_content
        .len()
        .saturating_mul(3)
        .checked_div(4)
        .unwrap_or(0)
        .saturating_sub(650);
    (visible_bytes as u64).div_ceil(4)
}

fn role_name(role: &ChatRole) -> &'static str {
    match role {
        ChatRole::User => "user",
        ChatRole::Assistant => "assistant",
    }
}

#[derive(Debug, Error)]
pub enum ChatTemplateError {
    #[error("failed to compile jinja chat template: {0}")]
    Compile(jinja::Error),
    #[error("failed to render jinja chat template: {0}")]
    Render(jinja::Error),
}

fn estimate_patch_tokens(
    file: &FileItem,
    patch_size: u32,
    patch_budget: u32,
    multiplier: f64,
) -> u64 {
    let Some((width, height)) = file.image_dimensions() else {
        return 0;
    };

    let original_patch_count = ceil_div(width, patch_size) * ceil_div(height, patch_size);
    let resized_patch_count = if original_patch_count <= patch_budget as u64 {
        original_patch_count
    } else {
        let width_f64 = width as f64;
        let height_f64 = height as f64;
        let patch_size_f64 = patch_size as f64;
        let budget_f64 = patch_budget as f64;
        let shrink_factor =
            ((patch_size_f64 * patch_size_f64 * budget_f64) / (width_f64 * height_f64)).sqrt();
        let adjusted_shrink_factor = shrink_factor
            * f64::min(
                (width_f64 * shrink_factor / patch_size_f64).floor()
                    / (width_f64 * shrink_factor / patch_size_f64),
                (height_f64 * shrink_factor / patch_size_f64).floor()
                    / (height_f64 * shrink_factor / patch_size_f64),
            );

        let resized_width = (width_f64 * adjusted_shrink_factor).floor().max(1.0) as u32;
        let resized_height = (height_f64 * adjusted_shrink_factor).floor().max(1.0) as u32;
        u64::min(
            ceil_div(resized_width, patch_size) * ceil_div(resized_height, patch_size),
            patch_budget as u64,
        )
    };

    (resized_patch_count as f64 * multiplier).ceil() as u64
}

#[allow(clippy::too_many_arguments)]
fn estimate_tile_tokens(
    file: &FileItem,
    low_detail_tokens: u64,
    base_tokens: u64,
    tokens_per_tile: u64,
    max_dimension: u32,
    target_shortest_side: u32,
    tile_size: u32,
    detail: VisionDetail,
) -> u64 {
    if matches!(detail, VisionDetail::Low) {
        return low_detail_tokens;
    }

    let Some((mut width, mut height)) = file.image_dimensions() else {
        return 0;
    };

    let max_side = width.max(height);
    if max_side > max_dimension {
        let scale = max_dimension as f64 / max_side as f64;
        width = (width as f64 * scale).round().max(1.0) as u32;
        height = (height as f64 * scale).round().max(1.0) as u32;
    }

    let shortest = width.min(height);
    let scale = target_shortest_side as f64 / shortest as f64;
    width = (width as f64 * scale).round().max(1.0) as u32;
    height = (height as f64 * scale).round().max(1.0) as u32;

    let tiles = ceil_div(width, tile_size) * ceil_div(height, tile_size);
    base_tokens + tiles * tokens_per_tile
}

fn estimate_anthropic_tokens(
    file: &FileItem,
    pixels_per_token: u64,
    max_tokens: u64,
    max_long_edge: u32,
    pad_to_multiple: u32,
) -> u64 {
    let Some((mut width, mut height)) = file.image_dimensions() else {
        return 0;
    };

    let max_side = width.max(height);
    if max_side > max_long_edge {
        let scale = max_long_edge as f64 / max_side as f64;
        width = (width as f64 * scale).round().max(1.0) as u32;
        height = (height as f64 * scale).round().max(1.0) as u32;
    }

    width = round_up_to_multiple(width, pad_to_multiple);
    height = round_up_to_multiple(height, pad_to_multiple);

    let estimate = ((width as u64 * height as u64) as f64 / pixels_per_token as f64).round() as u64;
    estimate.min(max_tokens)
}

fn ceil_div(value: u32, divisor: u32) -> u64 {
    value.div_ceil(divisor) as u64
}

fn round_up_to_multiple(value: u32, multiple: u32) -> u32 {
    if multiple == 0 {
        return value;
    }

    value.div_ceil(multiple) * multiple
}

#[derive(Debug, Error)]
pub enum TokenEstimatorError {
    #[error("token estimator url is required in model config")]
    MissingTokenEstimatorUrl,
    #[error("chat template is missing from tokenizer assets")]
    MissingChatTemplate,
    #[error("failed to resolve tokenizer assets: {0}")]
    ResolveTokenizerAssets(#[from] ResolveTokenizerAssetsError),
    #[error("failed to resolve tokenizer source: {0}")]
    ResolveTokenizerSource(#[from] ResolveModelFileError),
    #[error("failed to render chat template: {0}")]
    RenderChatTemplate(#[from] ChatTemplateError),
    #[error("failed to load tokenizer: {0}")]
    LoadTokenizer(tokenizers::Error),
    #[error("failed to tokenize rendered prompt: {0}")]
    Tokenize(tokenizers::Error),
    #[error("local tiktoken estimator does not support model {model_name}")]
    UnsupportedLocalTokenEstimatorModel { model_name: String },
    #[error("failed to estimate local tiktoken prompt: {0}")]
    LocalTokenEstimate(String),
    #[error("token estimation for media type {media_type} is not supported yet")]
    UnsupportedMediaTokenEstimate { media_type: String },
    #[error("the selected multimodal strategy requires provider-side token counting")]
    ProviderDerivedStrategyRequiresRemoteCount,
}

#[cfg(test)]
mod tests {
    use std::fs;

    use ahash::AHashMap;
    use tokenizers::{models::wordlevel::WordLevel, pre_tokenizers::whitespace::Whitespace};

    use super::*;
    use crate::{
        model_config::{
            MediaInputConfig, MediaInputTransport, ModelCapability, ModelConfig,
            MultimodalEstimatorConfig, MultimodalInputConfig, ProviderType, RetryMode,
            TokenEstimatorType,
        },
        session_actor::{
            ContextItem, ReasoningItem, TokenUsage, ToolResultContent, ToolResultItem,
        },
    };

    #[test]
    fn ignores_reasoning_when_estimating_tokens() {
        let estimator = build_test_estimator(
            "{% for message in messages %}{{ message.role }} {{ message.content }}\n{% endfor %}",
            MultimodalTokenStrategy::Ignore,
        );
        let messages = vec![ChatMessage::new(
            ChatRole::Assistant,
            vec![
                ChatMessageItem::Reasoning(ReasoningItem::from_text("hidden chain of thought")),
                ChatMessageItem::Context(ContextItem {
                    text: "hello world".to_string(),
                }),
            ],
        )
        .with_token_usage(TokenUsage {
            cache_read: 0,
            cache_write: 0,
            uncache_input: 0,
            output: 2,
            cost_usd: None,
        })];

        let estimate = estimator.estimate(&messages).expect("estimate should work");

        assert!(estimate.text_tokens > 0);
        assert_eq!(estimate.multimodal_tokens, 0);
        assert_eq!(estimate.reasoning_tokens, 0);
    }

    #[test]
    fn estimates_codex_encrypted_reasoning_tokens() {
        let estimator = build_test_estimator(
            "{% for message in messages %}{{ message.role }} {{ message.content }}\n{% endfor %}",
            MultimodalTokenStrategy::Ignore,
        );
        let messages = vec![ChatMessage::new(
            ChatRole::Assistant,
            vec![ChatMessageItem::Reasoning(ReasoningItem::codex(
                None,
                Some("a".repeat(1_000)),
                None,
            ))],
        )];

        let estimate = estimator.estimate(&messages).expect("estimate should work");

        assert_eq!(estimate.reasoning_tokens, 25);
        assert_eq!(
            estimate.total_tokens,
            estimate.text_tokens + estimate.reasoning_tokens
        );
    }

    #[test]
    fn estimates_multimodal_tokens_with_patch_grid_strategy() {
        let estimator = build_test_estimator(
            "{% for message in messages %}{{ message.role }} {{ message.content }}\n{% endfor %}",
            MultimodalTokenStrategy::PatchGrid {
                patch_size: 32,
                patch_budget: 1536,
                multiplier: 1.62,
            },
        );
        let messages = vec![ChatMessage::new(
            ChatRole::User,
            vec![ChatMessageItem::File(FileItem {
                uri: "file:///tmp/cat.png".to_string(),
                name: Some("cat.png".to_string()),
                media_type: Some("image/png".to_string()),
                width: Some(1024),
                height: Some(1024),
                state: None,
            })],
        )];

        let estimate = estimator.estimate(&messages).expect("estimate should work");

        assert_eq!(estimate.multimodal_tokens, 1659);
        assert!(estimate.total_tokens >= estimate.multimodal_tokens);
    }

    #[test]
    fn ignores_pdf_and_audio_files_when_multimodal_strategy_is_ignore() {
        let estimator = build_test_estimator(
            "{% for message in messages %}{{ message.role }} {{ message.content }}\n{% endfor %}",
            MultimodalTokenStrategy::Ignore,
        );

        for (media_type, name) in [
            ("application/pdf", "report.pdf"),
            ("audio/mpeg", "voice.mp3"),
        ] {
            let estimate = estimator
                .estimate(&[ChatMessage::new(
                    ChatRole::User,
                    vec![ChatMessageItem::File(FileItem {
                        uri: format!("file:///tmp/{name}"),
                        name: Some(name.to_string()),
                        media_type: Some(media_type.to_string()),
                        width: None,
                        height: None,
                        state: None,
                    })],
                )])
                .expect("pdf/audio should be ignored by the multimodal estimator");

            assert_eq!(estimate.multimodal_tokens, 0);
        }
    }

    #[test]
    fn media_strategy_rejects_pdf_and_audio_files_directly() {
        let strategy = MultimodalTokenStrategy::FixedTokens {
            tokens_per_file: 100,
        };

        for (media_type, name) in [
            ("application/pdf", "report.pdf"),
            ("audio/mpeg", "voice.mp3"),
        ] {
            let error = strategy
                .estimate(&FileItem {
                    uri: format!("file:///tmp/{name}"),
                    name: Some(name.to_string()),
                    media_type: Some(media_type.to_string()),
                    width: None,
                    height: None,
                    state: None,
                })
                .expect_err("pdf/audio token estimation should fail");

            assert!(matches!(
                error,
                TokenEstimatorError::UnsupportedMediaTokenEstimate {
                    media_type: returned
                } if returned == media_type
            ));
        }
    }

    #[test]
    fn estimates_downgraded_pdf_reference_as_text() {
        let estimator = build_test_estimator(
            "{% for message in messages %}{{ message.role }} {{ message.content }}\n{% endfor %}",
            MultimodalTokenStrategy::FixedTokens {
                tokens_per_file: 100,
            },
        );

        let estimate = estimator
            .estimate(&[ChatMessage::new(
                ChatRole::User,
                vec![ChatMessageItem::File(FileItem {
                    uri: "file:///tmp/report.pdf".to_string(),
                    name: Some("report.pdf".to_string()),
                    media_type: Some("application/pdf".to_string()),
                    width: None,
                    height: None,
                    state: None,
                })],
            )])
            .expect("unsupported pdf should be downgraded to text before token estimation");

        assert!(estimate.text_tokens > 0);
        assert_eq!(estimate.multimodal_tokens, 0);
    }

    #[test]
    fn loads_tokenizer_from_local_directory_source() {
        let estimator = build_test_estimator(
            "{% for message in messages %}{{ message.content }}{% endfor %}",
            MultimodalTokenStrategy::Ignore,
        );

        let estimate = estimator
            .estimate(&[ChatMessage::new(
                ChatRole::User,
                vec![ChatMessageItem::Context(ContextItem {
                    text: "hello world".to_string(),
                })],
            )])
            .expect("estimate should work");

        assert!(estimate.text_tokens > 0);
    }

    #[test]
    fn renders_tool_result_context_and_file() {
        let prompt = JinjaChatTemplate::from_source(
            "{{ bos_token }}{% for message in messages %}<|{{ message.role }}|>\n{{ message.content }}{{ eos_token }}{% endfor %}{% if add_generation_prompt %}<|assistant|>\n{% endif %}",
        )
        .with_bos_token("<s>")
        .with_eos_token("</s>")
        .with_add_generation_prompt(true)
        .render(&[ChatMessage::new(
            ChatRole::Assistant,
            vec![ChatMessageItem::ToolResult(ToolResultItem {
                tool_call_id: "call_1".to_string(),
                tool_name: "read_file".to_string(),
                result: ToolResultContent {
                    context: Some(ContextItem {
                        text: "loaded".to_string(),
                    }),
                    file: Some(FileItem {
                        uri: "file:///tmp/out.png".to_string(),
                        name: None,
                        media_type: Some("image/png".to_string()),
                        width: Some(640),
                        height: Some(480),
                        state: None,
                    }),
                },
            })],
        )])
        .expect("template should render");

        assert!(prompt.text.contains("<s><|assistant|>"));
        assert!(prompt.text.contains("loaded</s><|assistant|>"));
        assert_eq!(prompt.files.len(), 1);
    }

    #[test]
    fn renders_huggingface_template_with_numeric_attr_lookup() {
        let prompt = JinjaChatTemplate::from_source(
            r#"version 1.0
{% for m in messages %}
{%- if m.role == 'tool' and m.content is iterable and m.content is not mapping and m.content and m.content.0.type == "tool_reference" -%}
tool reference
{%- elif m.content is string -%}
{{ m.content }}
{%- endif -%}
{% endfor %}
{{ "keep m.content.0.type inside strings" }}"#,
        )
        .render(&[ChatMessage::new(
            ChatRole::User,
            vec![ChatMessageItem::Context(ContextItem {
                text: "hello world".to_string(),
            })],
        )])
        .expect("template should render");

        assert!(prompt.text.contains("version 1.0"));
        assert!(prompt.text.contains("hello world"));
        assert!(prompt.text.contains("keep m.content.0.type inside strings"));
    }

    #[test]
    fn builds_estimator_and_template_from_model_config() {
        let estimator = build_test_estimator(
            "{% for message in messages %}{{ message.role }}: {{ message.content }}\n{% endfor %}",
            MultimodalTokenStrategy::Ignore,
        );
        let estimate = estimator
            .estimate(&[ChatMessage::new(
                ChatRole::User,
                vec![ChatMessageItem::Context(ContextItem {
                    text: "hello world".to_string(),
                })],
            )])
            .expect("estimate should work");

        assert!(estimate.text_tokens > 0);
    }

    #[test]
    fn builds_local_tiktoken_estimator_without_tokenizer_config_url() {
        let resolver = HuggingFaceFileResolver::new().expect("resolver should build");
        let estimator = TokenEstimator::from_model_config(
            &local_tiktoken_model_config("openai/gpt-4o-mini", MultimodalTokenStrategy::Ignore),
            &resolver,
        )
        .expect("openai local estimator should build");

        let estimate = estimator
            .estimate(&[ChatMessage::new(
                ChatRole::User,
                vec![
                    ChatMessageItem::Reasoning(ReasoningItem::from_text("do not count this")),
                    ChatMessageItem::Context(ContextItem {
                        text: "hello from local tiktoken".to_string(),
                    }),
                ],
            )])
            .expect("local estimate should work");

        assert!(estimate.text_tokens > 0);
        assert_eq!(estimate.multimodal_tokens, 0);
    }

    #[test]
    fn local_tiktoken_estimator_counts_files_with_configured_multimodal_strategy() {
        let resolver = HuggingFaceFileResolver::new().expect("resolver should build");
        let estimator = TokenEstimator::from_model_config(
            &local_tiktoken_model_config(
                "gpt-4o",
                MultimodalTokenStrategy::FixedTokens {
                    tokens_per_file: 85,
                },
            ),
            &resolver,
        )
        .expect("openai local estimator should build");

        let estimate = estimator
            .estimate(&[ChatMessage::new(
                ChatRole::User,
                vec![ChatMessageItem::File(FileItem {
                    uri: "file:///tmp/cat.png".to_string(),
                    name: Some("cat.png".to_string()),
                    media_type: Some("image/png".to_string()),
                    width: Some(128),
                    height: Some(128),
                    state: None,
                })],
            )])
            .expect("local estimate should work");

        assert_eq!(estimate.multimodal_tokens, 85);
        assert!(estimate.total_tokens >= estimate.multimodal_tokens);
    }

    #[test]
    fn local_tiktoken_estimator_defaults_images_to_openai_tile_grid() {
        let resolver = HuggingFaceFileResolver::new().expect("resolver should build");
        let mut config = local_tiktoken_model_config("gpt-4o", MultimodalTokenStrategy::Ignore);
        config.multimodal_estimator = None;
        let estimator = TokenEstimator::from_model_config(&config, &resolver)
            .expect("openai local estimator should build");

        let estimate = estimator
            .estimate(&[ChatMessage::new(
                ChatRole::User,
                vec![ChatMessageItem::File(FileItem {
                    uri: "file:///tmp/cat.png".to_string(),
                    name: Some("cat.png".to_string()),
                    media_type: Some("image/png".to_string()),
                    width: Some(1024),
                    height: Some(1024),
                    state: None,
                })],
            )])
            .expect("local estimate should work");

        assert_eq!(estimate.multimodal_tokens, 765);
        assert!(estimate.total_tokens >= estimate.multimodal_tokens);
    }

    #[test]
    fn local_tiktoken_estimator_falls_back_for_unknown_models() {
        let resolver = HuggingFaceFileResolver::new().expect("resolver should build");
        let estimator = TokenEstimator::from_model_config(
            &local_tiktoken_model_config(
                "anthropic/claude-3-5-sonnet",
                MultimodalTokenStrategy::Ignore,
            ),
            &resolver,
        )
        .expect("unknown local model should use fallback tokenizer");

        let estimate = estimator
            .estimate(&[ChatMessage::new(
                ChatRole::User,
                vec![ChatMessageItem::Context(ContextItem {
                    text: "hello from fallback tokenizer".to_string(),
                })],
            )])
            .expect("fallback estimate should work");

        assert!(estimate.text_tokens > 0);
    }

    fn test_tokenizer() -> HuggingFaceTokenizer {
        let vocab = AHashMap::from([
            ("[UNK]".to_string(), 0),
            ("<".to_string(), 1),
            (">".to_string(), 2),
            ("message".to_string(), 3),
            ("role".to_string(), 4),
            ("user".to_string(), 5),
            ("assistant".to_string(), 6),
            ("hello".to_string(), 7),
            ("world".to_string(), 8),
            ("/message".to_string(), 9),
            ("file".to_string(), 10),
            ("tool_result".to_string(), 11),
            ("loaded".to_string(), 12),
        ]);
        let model = WordLevel::builder()
            .vocab(vocab)
            .unk_token("[UNK]".to_string())
            .build()
            .expect("word level should build");
        let mut tokenizer = HuggingFaceTokenizer::new(model);
        tokenizer.with_pre_tokenizer(Some(Whitespace));
        tokenizer
    }

    fn build_test_estimator(
        chat_template: &str,
        multimodal_strategy: MultimodalTokenStrategy,
    ) -> TokenEstimator {
        let tokenizer = test_tokenizer();
        let directory = std::env::temp_dir().join(format!(
            "stellaclaw-token-estimator-{}-{}",
            std::process::id(),
            rand::random::<u64>()
        ));
        fs::create_dir_all(&directory).expect("directory should exist");
        tokenizer
            .save(directory.join("tokenizer.json"), false)
            .expect("tokenizer should save");
        fs::write(
            directory.join("tokenizer_config.json"),
            format!(
                r#"{{
                    "chat_template": {chat_template:?},
                    "bos_token": "<s>",
                    "eos_token": "</s>"
                }}"#
            ),
        )
        .expect("tokenizer config should save");

        let model_config = ModelConfig {
            provider_type: ProviderType::OpenRouterCompletion,
            model_name: "openai/gpt-4o-mini".to_string(),
            url: "https://openrouter.ai/api/v1/chat/completions".to_string(),
            api_key_env: "OPENROUTER_API_KEY".to_string(),
            capabilities: vec![ModelCapability::Chat, ModelCapability::ImageIn],
            token_max_context: 128_000,
            cache_timeout: 300,
            conn_timeout: 10,
            retry_mode: RetryMode::Once,
            reasoning: None,
            token_estimator_type: TokenEstimatorType::HuggingFace,
            multimodal_estimator: Some(MultimodalEstimatorConfig {
                image: Some(multimodal_strategy),
            }),
            multimodal_input: Some(MultimodalInputConfig {
                image: Some(MediaInputConfig {
                    transport: MediaInputTransport::FileReference,
                    supported_media_types: vec!["image/png".to_string(), "image/jpeg".to_string()],
                    max_width: None,
                    max_height: None,
                }),
                pdf: None,
                audio: None,
            }),
            token_estimator_url: Some(
                directory
                    .join("tokenizer_config.json")
                    .to_string_lossy()
                    .to_string(),
            ),
        };

        let resolver = HuggingFaceFileResolver::new().expect("resolver should build");
        let estimator =
            TokenEstimator::from_model_config(&model_config, &resolver).expect("should build");

        fs::remove_dir_all(directory).expect("directory should be removed");
        estimator
    }

    fn local_tiktoken_model_config(
        model_name: &str,
        multimodal_strategy: MultimodalTokenStrategy,
    ) -> ModelConfig {
        ModelConfig {
            provider_type: ProviderType::OpenRouterCompletion,
            model_name: model_name.to_string(),
            url: "https://openrouter.ai/api/v1/chat/completions".to_string(),
            api_key_env: "OPENROUTER_API_KEY".to_string(),
            capabilities: vec![ModelCapability::Chat, ModelCapability::ImageIn],
            token_max_context: 128_000,
            cache_timeout: 300,
            conn_timeout: 10,
            retry_mode: RetryMode::Once,
            reasoning: None,
            token_estimator_type: TokenEstimatorType::Local,
            multimodal_estimator: Some(MultimodalEstimatorConfig {
                image: Some(multimodal_strategy),
            }),
            multimodal_input: Some(MultimodalInputConfig {
                image: Some(MediaInputConfig {
                    transport: MediaInputTransport::FileReference,
                    supported_media_types: vec!["image/png".to_string(), "image/jpeg".to_string()],
                    max_width: None,
                    max_height: None,
                }),
                pdf: None,
                audio: None,
            }),
            token_estimator_url: None,
        }
    }
}
