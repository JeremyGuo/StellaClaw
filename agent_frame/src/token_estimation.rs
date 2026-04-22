use crate::config::{
    TokenEstimationConfig, TokenEstimationSource, TokenEstimationTemplateConfig,
    TokenEstimationTiktokenEncoding, TokenEstimationTokenizerConfig, UpstreamConfig,
};
use crate::message::{ChatMessage, ToolCall, content_item_text};
use crate::tooling::Tool;
use base64::Engine;
use hf_hub::api::sync::ApiBuilder;
use hf_hub::{Repo, RepoType};
use image::ImageReader;
use minijinja::{Environment, context};
use serde_json::Value;
use std::collections::HashMap;
use std::fs;
use std::io::Cursor;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use tiktoken_rs::{cl100k_base_singleton, o200k_base_singleton, o200k_harmony_singleton};
use tokenizers::Tokenizer;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TokenEstimator {
    TiktokenO200k,
    TiktokenCl100k,
    TiktokenO200kHarmony,
}

impl TokenEstimator {
    fn encode_len(self, text: &str) -> usize {
        match self {
            Self::TiktokenO200k => o200k_base_singleton()
                .encode_with_special_tokens(text)
                .len(),
            Self::TiktokenCl100k => cl100k_base_singleton()
                .encode_with_special_tokens(text)
                .len(),
            Self::TiktokenO200kHarmony => o200k_harmony_singleton()
                .encode_with_special_tokens(text)
                .len(),
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::TiktokenO200k => "o200k_base",
            Self::TiktokenCl100k => "cl100k_base approx",
            Self::TiktokenO200kHarmony => "o200k_harmony",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RenderedTokenEstimatePrompt {
    pub text: String,
    pub inline_payload_tokens: usize,
    pub template_label: String,
}

#[derive(Clone, Copy)]
pub struct TokenEstimateInput<'a> {
    pub messages: &'a [ChatMessage],
    pub tools: &'a [Tool],
    pub pending_user_prompt: &'a str,
}

#[derive(Clone, Copy, Debug)]
pub struct TokenEstimateModel<'a> {
    pub model: &'a str,
    pub token_estimation: Option<&'a TokenEstimationConfig>,
}

pub fn token_estimator_for_model(model: &str) -> TokenEstimator {
    let normalized = model.trim().to_ascii_lowercase();
    let leaf = normalized
        .rsplit('/')
        .next()
        .filter(|value| !value.is_empty())
        .unwrap_or(normalized.as_str());

    if leaf.starts_with("gpt-oss") {
        return TokenEstimator::TiktokenO200kHarmony;
    }

    if leaf.starts_with("gpt-5")
        || leaf.starts_with("gpt-4.1")
        || leaf.starts_with("gpt-4o")
        || leaf.starts_with("chatgpt-4o")
        || leaf.starts_with("o1")
        || leaf.starts_with("o3")
        || leaf.starts_with("o4")
        || leaf.starts_with("codex-mini")
    {
        return TokenEstimator::TiktokenO200k;
    }

    if leaf.starts_with("gpt-4")
        || leaf.starts_with("gpt-3.5")
        || leaf.starts_with("gpt-35")
        || leaf.starts_with("text-embedding-ada-002")
    {
        return TokenEstimator::TiktokenCl100k;
    }

    if normalized.starts_with("anthropic/")
        || leaf.starts_with("claude")
        || normalized.starts_with("google/")
        || leaf.starts_with("gemini")
        || normalized.starts_with("qwen/")
        || leaf.starts_with("qwen")
        || normalized.starts_with("deepseek/")
        || leaf.starts_with("deepseek")
        || normalized.starts_with("mistralai/")
        || leaf.starts_with("mistral")
        || normalized.starts_with("meta-llama/")
        || leaf.starts_with("llama")
        || normalized.starts_with("z-ai/")
        || leaf.starts_with("glm")
        || normalized.starts_with("moonshotai/")
        || leaf.starts_with("kimi")
        || normalized.starts_with("x-ai/")
        || leaf.starts_with("grok")
    {
        return TokenEstimator::TiktokenCl100k;
    }

    TokenEstimator::TiktokenO200k
}

pub fn token_estimator_label_for_model(model: &str) -> &'static str {
    token_estimator_for_model(model).label()
}

fn tiktoken_encoding_to_estimator(
    encoding: TokenEstimationTiktokenEncoding,
    model: &str,
) -> TokenEstimator {
    match encoding {
        TokenEstimationTiktokenEncoding::Auto => token_estimator_for_model(model),
        TokenEstimationTiktokenEncoding::O200kBase => TokenEstimator::TiktokenO200k,
        TokenEstimationTiktokenEncoding::Cl100kBase => TokenEstimator::TiktokenCl100k,
        TokenEstimationTiktokenEncoding::O200kHarmony => TokenEstimator::TiktokenO200kHarmony,
    }
}

pub fn estimate_text_tokens_for_estimator(text: &str, estimator: TokenEstimator) -> usize {
    if text.is_empty() {
        return 0;
    }
    estimator.encode_len(text).max(1)
}

// Mirrors Codex's approach: do not estimate inline base64 media payloads as
// raw text. Replace them with a conservative side estimate before tokenizing
// the rendered prompt. Image estimates are model-aware: GPT-family models use
// OpenAI's documented image-token sizing rules, while other models use the
// Anthropic-style vision-area approximation. Invalid payloads still fall back
// to a coarse bytes/4 estimate so malformed inline media never collapses to 0.
const INLINE_FILE_BYTES_ESTIMATE: usize = 12_000;
const INLINE_AUDIO_BYTES_ESTIMATE: usize = 16_000;
const CLAUDE_IMAGE_MAX_EDGE_PX: f64 = 1_568.0;
const CLAUDE_IMAGE_MAX_TOKENS: usize = 1_568;
const OPENAI_IMAGE_MAX_DIMENSION_PX: f64 = 2_048.0;
const OPENAI_IMAGE_TILE_SIZE_PX: f64 = 512.0;
const OPENAI_IMAGE_HIGH_DETAIL_SHORT_SIDE_PX: f64 = 768.0;
const OPENAI_IMAGE_PATCH_SIZE_PX: f64 = 32.0;

#[derive(Clone, Copy, Debug, PartialEq)]
enum ImageTokenEstimatorKind {
    OpenAiTiles {
        base_tokens: usize,
        tile_tokens: usize,
    },
    OpenAiPatches {
        patch_budget: usize,
        max_dimension_px: f64,
        multiplier: f64,
    },
    Anthropic,
}

fn estimate_payload_bytes_as_tokens(bytes: usize) -> usize {
    bytes.div_ceil(4).max(1)
}

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

fn image_dimensions_from_data_url(image_url: &str) -> Option<(u32, u32)> {
    let payload = parse_base64_image_data_url(image_url)?;
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(payload)
        .ok()?;
    ImageReader::new(Cursor::new(bytes))
        .with_guessed_format()
        .ok()?
        .into_dimensions()
        .ok()
}

fn image_token_estimator_kind_for_model(model_id: Option<&str>) -> ImageTokenEstimatorKind {
    let normalized = model_id.unwrap_or_default().trim().to_ascii_lowercase();
    if !normalized.contains("gpt") {
        return ImageTokenEstimatorKind::Anthropic;
    }

    if normalized.contains("gpt-5.4-mini")
        || normalized.contains("gpt-5-mini")
        || normalized.contains("gpt-4.1-mini")
    {
        return ImageTokenEstimatorKind::OpenAiPatches {
            patch_budget: 1_536,
            max_dimension_px: OPENAI_IMAGE_MAX_DIMENSION_PX,
            multiplier: 1.62,
        };
    }

    if normalized.contains("gpt-5.4-nano")
        || normalized.contains("gpt-5-nano")
        || normalized.contains("gpt-4.1-nano")
    {
        return ImageTokenEstimatorKind::OpenAiPatches {
            patch_budget: 1_536,
            max_dimension_px: OPENAI_IMAGE_MAX_DIMENSION_PX,
            multiplier: 2.46,
        };
    }

    if normalized.contains("gpt-5.4") {
        return ImageTokenEstimatorKind::OpenAiPatches {
            patch_budget: 2_500,
            max_dimension_px: OPENAI_IMAGE_MAX_DIMENSION_PX,
            multiplier: 1.0,
        };
    }

    if normalized.contains("gpt-5.3-codex")
        || normalized.contains("gpt-5-codex")
        || normalized.contains("gpt-5.1-codex-mini")
        || normalized.contains("gpt-5.2-codex")
        || normalized.contains("gpt-5.2-chat-latest")
        || normalized.contains("gpt-5.2")
    {
        return ImageTokenEstimatorKind::OpenAiPatches {
            patch_budget: 1_536,
            max_dimension_px: OPENAI_IMAGE_MAX_DIMENSION_PX,
            multiplier: 1.0,
        };
    }

    if normalized.contains("gpt-4o-mini") {
        return ImageTokenEstimatorKind::OpenAiTiles {
            base_tokens: 2_833,
            tile_tokens: 5_667,
        };
    }

    if normalized.contains("gpt-5") {
        return ImageTokenEstimatorKind::OpenAiTiles {
            base_tokens: 70,
            tile_tokens: 140,
        };
    }

    if normalized.contains("gpt-4o")
        || normalized.contains("gpt-4.1")
        || normalized.contains("gpt-4.5")
    {
        return ImageTokenEstimatorKind::OpenAiTiles {
            base_tokens: 85,
            tile_tokens: 170,
        };
    }

    ImageTokenEstimatorKind::OpenAiTiles {
        base_tokens: 70,
        tile_tokens: 140,
    }
}

fn estimate_inline_image_tokens_for_model(model_id: Option<&str>, image_url: &str) -> usize {
    let Some((width, height)) = image_dimensions_from_data_url(image_url) else {
        return estimate_payload_bytes_as_tokens(image_url.len());
    };
    match image_token_estimator_kind_for_model(model_id) {
        ImageTokenEstimatorKind::OpenAiTiles {
            base_tokens,
            tile_tokens,
        } => estimate_openai_tiled_image_tokens(width, height, base_tokens, tile_tokens),
        ImageTokenEstimatorKind::OpenAiPatches {
            patch_budget,
            max_dimension_px,
            multiplier,
        } => estimate_openai_patch_image_tokens(
            width,
            height,
            patch_budget,
            max_dimension_px,
            multiplier,
        ),
        ImageTokenEstimatorKind::Anthropic => {
            estimate_claude_image_tokens_from_dimensions(width, height)
        }
    }
}

fn estimate_openai_tiled_image_tokens(
    width: u32,
    height: u32,
    base_tokens: usize,
    tile_tokens: usize,
) -> usize {
    if width == 0 || height == 0 {
        return base_tokens.max(1);
    }

    let mut width = width as f64;
    let mut height = height as f64;
    let fit_scale = (OPENAI_IMAGE_MAX_DIMENSION_PX / width.max(height)).min(1.0);
    width *= fit_scale;
    height *= fit_scale;

    let shortest_side = width.min(height);
    if shortest_side > 0.0 {
        let detail_scale = OPENAI_IMAGE_HIGH_DETAIL_SHORT_SIDE_PX / shortest_side;
        width *= detail_scale;
        height *= detail_scale;
    }

    let tiles_w = (width / OPENAI_IMAGE_TILE_SIZE_PX).ceil().max(1.0) as usize;
    let tiles_h = (height / OPENAI_IMAGE_TILE_SIZE_PX).ceil().max(1.0) as usize;
    base_tokens.saturating_add(tile_tokens.saturating_mul(tiles_w.saturating_mul(tiles_h)))
}

fn estimate_openai_patch_image_tokens(
    width: u32,
    height: u32,
    patch_budget: usize,
    max_dimension_px: f64,
    multiplier: f64,
) -> usize {
    if width == 0 || height == 0 {
        return 1;
    }

    let width = width as f64;
    let height = height as f64;
    let original_patch_count =
        (width / OPENAI_IMAGE_PATCH_SIZE_PX).ceil() * (height / OPENAI_IMAGE_PATCH_SIZE_PX).ceil();
    let dimension_scale = (max_dimension_px / width.max(height)).min(1.0);
    let budget_scale =
        ((OPENAI_IMAGE_PATCH_SIZE_PX.powi(2) * patch_budget as f64) / (width * height)).sqrt();
    let shrink_factor = dimension_scale.min(budget_scale).min(1.0);

    let resized_patch_count = if original_patch_count <= patch_budget as f64
        && width.max(height) <= max_dimension_px
    {
        original_patch_count
    } else {
        let scaled_width_in_patches = (width * shrink_factor) / OPENAI_IMAGE_PATCH_SIZE_PX;
        let scaled_height_in_patches = (height * shrink_factor) / OPENAI_IMAGE_PATCH_SIZE_PX;
        let width_adjust = if scaled_width_in_patches > 0.0 {
            scaled_width_in_patches.floor().max(1.0) / scaled_width_in_patches
        } else {
            1.0
        };
        let height_adjust = if scaled_height_in_patches > 0.0 {
            scaled_height_in_patches.floor().max(1.0) / scaled_height_in_patches
        } else {
            1.0
        };
        let adjusted_shrink_factor = shrink_factor * width_adjust.min(height_adjust);
        let resized_patches_w = ((width * adjusted_shrink_factor) / OPENAI_IMAGE_PATCH_SIZE_PX)
            .ceil()
            .max(1.0);
        let resized_patches_h = ((height * adjusted_shrink_factor) / OPENAI_IMAGE_PATCH_SIZE_PX)
            .ceil()
            .max(1.0);
        (resized_patches_w * resized_patches_h).min(patch_budget as f64)
    };

    ((resized_patch_count * multiplier).ceil() as usize).max(1)
}

fn estimate_claude_image_tokens_from_dimensions(width: u32, height: u32) -> usize {
    if width == 0 || height == 0 {
        return 1;
    }

    let width = width as f64;
    let height = height as f64;
    let scale = (CLAUDE_IMAGE_MAX_EDGE_PX / width.max(height)).min(1.0);
    let scaled_width = (width * scale).round().max(1.0);
    let scaled_height = (height * scale).round().max(1.0);
    let area = scaled_width * scaled_height;
    let estimate = (area / 750.0).ceil() as usize;
    estimate.clamp(1, CLAUDE_IMAGE_MAX_TOKENS)
}

fn value_text(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        other => serde_json::to_string(other).unwrap_or_default(),
    }
}

fn image_url_value(item_type: &str, item: &Value) -> Option<String> {
    if let Some(path) = item.get("path").and_then(Value::as_str) {
        return Some(path.to_string());
    }
    if item_type == "image_url" {
        item.get("image_url").and_then(|value| match value {
            Value::String(url) => Some(url.clone()),
            Value::Object(map) => map
                .get("url")
                .and_then(Value::as_str)
                .map(ToOwned::to_owned),
            _ => None,
        })
    } else {
        item.get("image_url")
            .and_then(Value::as_str)
            .map(ToOwned::to_owned)
    }
}

fn render_file_item(item_type: &str, item: &Value, extra_tokens: &mut usize) -> String {
    let file_value = if item_type == "file" {
        item.get("file").unwrap_or(item)
    } else {
        item
    };
    if let Some(path) = file_value.get("path").and_then(Value::as_str) {
        return format!("[file: {path}]");
    }
    let filename = file_value
        .get("filename")
        .and_then(Value::as_str)
        .unwrap_or("document");
    if file_value
        .get("file_data")
        .and_then(Value::as_str)
        .is_some()
    {
        *extra_tokens = extra_tokens
            .saturating_add(estimate_payload_bytes_as_tokens(INLINE_FILE_BYTES_ESTIMATE));
        format!("[inline file payload omitted for token estimate: {filename}]")
    } else {
        format!("[file: {filename}]")
    }
}

fn render_content_item(item: &Value, extra_tokens: &mut usize, model_id: Option<&str>) -> String {
    let Some(object) = item.as_object() else {
        return value_text(item);
    };
    let Some(item_type) = object.get("type").and_then(Value::as_str) else {
        return value_text(item);
    };
    match item_type {
        "text" | "input_text" | "output_text" => object
            .get("text")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
        "image_url" | "input_image" | "output_image" => {
            let image_url = image_url_value(item_type, item).unwrap_or_default();
            if parse_base64_image_data_url(&image_url).is_some() {
                *extra_tokens = extra_tokens
                    .saturating_add(estimate_inline_image_tokens_for_model(model_id, &image_url));
                "[inline image payload omitted for token estimate]".to_string()
            } else if image_url.is_empty() {
                "[image]".to_string()
            } else {
                format!("[image: {image_url}]")
            }
        }
        "file" | "input_file" | "output_file" => render_file_item(item_type, item, extra_tokens),
        "input_audio" | "output_audio" => {
            let format = object
                .get("format")
                .and_then(Value::as_str)
                .or_else(|| {
                    object
                        .get("input_audio")
                        .and_then(Value::as_object)
                        .and_then(|audio| audio.get("format"))
                        .and_then(Value::as_str)
                })
                .unwrap_or("audio");
            if object.get("path").and_then(Value::as_str).is_some() {
                format!("[audio: {format}]")
            } else if object
                .get("input_audio")
                .and_then(Value::as_object)
                .and_then(|audio| audio.get("data"))
                .and_then(Value::as_str)
                .is_some()
            {
                *extra_tokens = extra_tokens.saturating_add(estimate_payload_bytes_as_tokens(
                    INLINE_AUDIO_BYTES_ESTIMATE,
                ));
                format!("[inline audio payload omitted for token estimate: {format}]")
            } else {
                format!("[audio: {format}]")
            }
        }
        _ => content_item_text(item).unwrap_or_else(|| value_text(item)),
    }
}

fn render_message_content(
    content: &Option<Value>,
    extra_tokens: &mut usize,
    model_id: Option<&str>,
) -> String {
    match content {
        Some(Value::String(text)) => text.clone(),
        Some(Value::Array(items)) => items
            .iter()
            .map(|item| render_content_item(item, extra_tokens, model_id))
            .filter(|text| !text.is_empty())
            .collect::<Vec<_>>()
            .join("\n"),
        Some(other) => value_text(other),
        None => String::new(),
    }
}

fn escaped_attr(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('"', "&quot;")
        .replace('\n', "\\n")
}

fn render_tool_call(tool_call: &ToolCall) -> String {
    let arguments = tool_call.function.arguments.as_deref().unwrap_or_default();
    if arguments.trim().is_empty() {
        format!(
            "<|tool_call id=\"{}\" name=\"{}\" type=\"{}\"|>",
            escaped_attr(&tool_call.id),
            escaped_attr(&tool_call.function.name),
            escaped_attr(&tool_call.kind)
        )
    } else {
        format!(
            "<|tool_call id=\"{}\" name=\"{}\" type=\"{}\"|>\n{}",
            escaped_attr(&tool_call.id),
            escaped_attr(&tool_call.function.name),
            escaped_attr(&tool_call.kind),
            arguments
        )
    }
}

fn render_message(
    message: &ChatMessage,
    extra_tokens: &mut usize,
    model_id: Option<&str>,
) -> String {
    let mut attrs = String::new();
    if let Some(name) = message.name.as_deref() {
        attrs.push_str(" name=\"");
        attrs.push_str(&escaped_attr(name));
        attrs.push('"');
    }
    if let Some(tool_call_id) = message.tool_call_id.as_deref() {
        attrs.push_str(" tool_call_id=\"");
        attrs.push_str(&escaped_attr(tool_call_id));
        attrs.push('"');
    }

    let mut parts = vec![format!("<|{}{}|>", message.role, attrs)];
    let content = render_message_content(&message.content, extra_tokens, model_id);
    if !content.trim().is_empty() {
        parts.push(content);
    }
    if let Some(tool_calls) = &message.tool_calls {
        parts.extend(tool_calls.iter().map(render_tool_call));
    }
    parts.join("\n")
}

fn render_builtin_prompt_for_estimate_with_model(
    messages: &[ChatMessage],
    tools: &[Tool],
    pending_user_prompt: &str,
    model_id: Option<&str>,
) -> RenderedTokenEstimatePrompt {
    let mut inline_payload_tokens = 0usize;
    let mut sections = Vec::new();
    if !tools.is_empty() {
        let tools_json =
            serde_json::to_string(&tools.iter().map(Tool::as_openai_tool).collect::<Vec<_>>())
                .unwrap_or_default();
        sections.push(format!("<|tools|>\n{tools_json}"));
    }
    sections.extend(
        messages
            .iter()
            .map(|message| render_message(message, &mut inline_payload_tokens, model_id)),
    );
    if !pending_user_prompt.is_empty() {
        sections.push(format!("<|user|>\n{pending_user_prompt}"));
    }
    sections.push("<|assistant|>".to_string());
    RenderedTokenEstimatePrompt {
        text: sections.join("\n\n"),
        inline_payload_tokens,
        template_label: "builtin".to_string(),
    }
}

pub fn render_builtin_prompt_for_estimate(
    messages: &[ChatMessage],
    tools: &[Tool],
    pending_user_prompt: &str,
) -> RenderedTokenEstimatePrompt {
    render_builtin_prompt_for_estimate_with_model(messages, tools, pending_user_prompt, None)
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct LocalFileCacheKey {
    path: PathBuf,
    modified_nanos: u128,
    len: u64,
    field: Option<String>,
}

fn local_file_cache_key(path: &Path, field: Option<&str>) -> LocalFileCacheKey {
    let metadata = fs::metadata(path).ok();
    let modified_nanos = metadata
        .as_ref()
        .and_then(|metadata| metadata.modified().ok())
        .and_then(|modified| modified.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|duration| duration.as_nanos())
        .unwrap_or(0);
    LocalFileCacheKey {
        path: path.to_path_buf(),
        modified_nanos,
        len: metadata.map(|metadata| metadata.len()).unwrap_or(0),
        field: field.map(ToOwned::to_owned),
    }
}

static LOCAL_TEMPLATE_CACHE: OnceLock<Mutex<HashMap<LocalFileCacheKey, String>>> = OnceLock::new();
static LOCAL_TOKENIZER_CACHE: OnceLock<Mutex<HashMap<LocalFileCacheKey, Tokenizer>>> =
    OnceLock::new();
static HUGGINGFACE_FILE_CACHE: OnceLock<Mutex<HashMap<HuggingFaceFileCacheKey, PathBuf>>> =
    OnceLock::new();
static PROMPT_TOKEN_CALIBRATION: OnceLock<Mutex<HashMap<String, PromptTokenCalibration>>> =
    OnceLock::new();

#[derive(Clone, Copy, Debug)]
struct PromptTokenCalibration {
    ratio: f64,
    samples: u64,
}

fn calibration_key_for_model(model: &str) -> String {
    model.trim().to_ascii_lowercase()
}

fn clamp_calibration_ratio(ratio: f64) -> f64 {
    if ratio.is_finite() {
        ratio.clamp(0.25, 4.0)
    } else {
        1.0
    }
}

fn apply_prompt_token_calibration(model: &str, estimated_tokens: usize) -> usize {
    if estimated_tokens == 0 {
        return 0;
    }
    let Some((ratio, _samples)) = prompt_token_calibration_for_model(model) else {
        return estimated_tokens;
    };
    ((estimated_tokens as f64) * ratio).round().max(1.0) as usize
}

pub fn observe_prompt_token_estimate(model: &str, estimated_tokens: usize, actual_tokens: u64) {
    if estimated_tokens == 0 || actual_tokens == 0 {
        return;
    }
    let observed_ratio = clamp_calibration_ratio(actual_tokens as f64 / estimated_tokens as f64);
    let key = calibration_key_for_model(model);
    let cache = PROMPT_TOKEN_CALIBRATION.get_or_init(|| Mutex::new(HashMap::new()));
    let Ok(mut cache) = cache.lock() else {
        return;
    };
    cache
        .entry(key)
        .and_modify(|calibration| {
            let alpha = if calibration.samples < 5 { 0.35 } else { 0.15 };
            calibration.ratio =
                clamp_calibration_ratio(calibration.ratio * (1.0 - alpha) + observed_ratio * alpha);
            calibration.samples = calibration.samples.saturating_add(1);
        })
        .or_insert(PromptTokenCalibration {
            ratio: observed_ratio,
            samples: 1,
        });
}

pub fn prompt_token_calibration_for_model(model: &str) -> Option<(f64, u64)> {
    let cache = PROMPT_TOKEN_CALIBRATION.get_or_init(|| Mutex::new(HashMap::new()));
    let cache = cache.lock().ok()?;
    let calibration = cache.get(&calibration_key_for_model(model))?;
    Some((calibration.ratio, calibration.samples))
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct HuggingFaceFileCacheKey {
    repo: String,
    revision: String,
    file: String,
    cache_dir: Option<PathBuf>,
}

fn is_allowed_huggingface_token_file(file: &str) -> bool {
    matches!(
        file,
        "tokenizer.json"
            | "tokenizer_config.json"
            | "chat_template.jinja"
            | "chat_template.json"
            | "processor_config.json"
            | "special_tokens_map.json"
    )
}

fn download_huggingface_token_file(
    repo: &str,
    revision: &str,
    file: &str,
    cache_dir: Option<&Path>,
) -> Option<PathBuf> {
    if !is_allowed_huggingface_token_file(file) {
        return None;
    }

    let key = HuggingFaceFileCacheKey {
        repo: repo.to_string(),
        revision: revision.to_string(),
        file: file.to_string(),
        cache_dir: cache_dir.map(Path::to_path_buf),
    };
    let cache = HUGGINGFACE_FILE_CACHE.get_or_init(|| Mutex::new(HashMap::new()));
    if let Ok(cache) = cache.lock()
        && let Some(path) = cache.get(&key)
    {
        return Some(path.clone());
    }

    let mut builder = ApiBuilder::from_env().with_progress(false);
    if let Some(cache_dir) = cache_dir {
        builder = builder.with_cache_dir(cache_dir.to_path_buf());
    }
    let api = builder.build().ok()?;
    let repo = Repo::with_revision(repo.to_string(), RepoType::Model, revision.to_string());
    let path = api.repo(repo).get(file).ok()?;
    if let Ok(mut cache) = cache.lock() {
        cache.insert(key, path.clone());
    }
    Some(path)
}

fn read_local_template(path: &Path, field: &str) -> Option<String> {
    let key = local_file_cache_key(path, Some(field));
    let cache = LOCAL_TEMPLATE_CACHE.get_or_init(|| Mutex::new(HashMap::new()));
    if let Ok(cache) = cache.lock()
        && let Some(template) = cache.get(&key)
    {
        return Some(template.clone());
    }

    let raw = fs::read_to_string(path).ok()?;
    let template = if path
        .extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| extension.eq_ignore_ascii_case("json"))
    {
        let value = serde_json::from_str::<Value>(&raw).ok()?;
        value
            .get(field)
            .and_then(Value::as_str)
            .map(ToOwned::to_owned)?
    } else {
        raw
    };

    if let Ok(mut cache) = cache.lock() {
        cache.insert(key, template.clone());
    }
    Some(template)
}

fn load_local_tokenizer(path: &Path) -> Option<Tokenizer> {
    let key = local_file_cache_key(path, None);
    let cache = LOCAL_TOKENIZER_CACHE.get_or_init(|| Mutex::new(HashMap::new()));
    if let Ok(cache) = cache.lock()
        && let Some(tokenizer) = cache.get(&key)
    {
        return Some(tokenizer.clone());
    }

    let tokenizer = Tokenizer::from_file(path).ok()?;
    if let Ok(mut cache) = cache.lock() {
        cache.insert(key, tokenizer.clone());
    }
    Some(tokenizer)
}

fn message_to_template_value(
    message: &ChatMessage,
    extra_tokens: &mut usize,
    model_id: Option<&str>,
) -> Value {
    let mut object = serde_json::Map::new();
    object.insert("role".to_string(), Value::String(message.role.clone()));
    object.insert(
        "content".to_string(),
        Value::String(render_message_content(
            &message.content,
            extra_tokens,
            model_id,
        )),
    );
    if let Some(name) = &message.name {
        object.insert("name".to_string(), Value::String(name.clone()));
    }
    if let Some(tool_call_id) = &message.tool_call_id {
        object.insert(
            "tool_call_id".to_string(),
            Value::String(tool_call_id.clone()),
        );
    }
    if let Some(tool_calls) = &message.tool_calls {
        let calls = tool_calls
            .iter()
            .map(|call| {
                serde_json::json!({
                    "id": call.id,
                    "type": call.kind,
                    "function": {
                        "name": call.function.name,
                        "arguments": call.function.arguments.clone().unwrap_or_default()
                    }
                })
            })
            .collect::<Vec<_>>();
        object.insert("tool_calls".to_string(), Value::Array(calls));
    }
    Value::Object(object)
}

fn render_local_template_prompt_for_estimate(
    template: &str,
    messages: &[ChatMessage],
    tools: &[Tool],
    pending_user_prompt: &str,
    label: String,
    model_id: Option<&str>,
) -> Option<RenderedTokenEstimatePrompt> {
    let mut inline_payload_tokens = 0usize;
    let mut template_messages = messages
        .iter()
        .map(|message| message_to_template_value(message, &mut inline_payload_tokens, model_id))
        .collect::<Vec<_>>();
    if !pending_user_prompt.is_empty() {
        template_messages.push(serde_json::json!({
            "role": "user",
            "content": pending_user_prompt
        }));
    }

    let template_tools = tools.iter().map(Tool::as_openai_tool).collect::<Vec<_>>();
    let mut env = Environment::new();
    env.add_template("chat_template", template).ok()?;
    let rendered = env
        .get_template("chat_template")
        .ok()?
        .render(context! {
            messages => template_messages,
            tools => template_tools,
            add_generation_prompt => true,
        })
        .ok()?;

    let text = if tools.is_empty() || template.contains("tools") {
        rendered
    } else {
        let tools_json =
            serde_json::to_string(&tools.iter().map(Tool::as_openai_tool).collect::<Vec<_>>())
                .unwrap_or_default();
        format!("<|tools|>\n{tools_json}\n\n{rendered}")
    };

    Some(RenderedTokenEstimatePrompt {
        text,
        inline_payload_tokens,
        template_label: label,
    })
}

fn render_prompt_for_token_estimate_with_model(
    input: TokenEstimateInput<'_>,
    token_estimation: Option<&TokenEstimationConfig>,
    model_id: Option<&str>,
) -> RenderedTokenEstimatePrompt {
    if let Some(config) = token_estimation {
        match config.template.as_ref() {
            Some(TokenEstimationTemplateConfig::Builtin) => {}
            Some(TokenEstimationTemplateConfig::Local { path, field }) => {
                if let Some(template) = read_local_template(path, field)
                    && let Some(rendered) = render_local_template_prompt_for_estimate(
                        &template,
                        input.messages,
                        input.tools,
                        input.pending_user_prompt,
                        format!("local:{}", path.display()),
                        model_id,
                    )
                {
                    return rendered;
                }
            }
            Some(TokenEstimationTemplateConfig::Huggingface {
                repo,
                revision,
                file,
                field,
                cache_dir,
            }) => {
                if let Some(path) =
                    download_huggingface_token_file(repo, revision, file, cache_dir.as_deref())
                    && let Some(template) = read_local_template(&path, field)
                    && let Some(rendered) = render_local_template_prompt_for_estimate(
                        &template,
                        input.messages,
                        input.tools,
                        input.pending_user_prompt,
                        format!("huggingface:{repo}@{revision}:{file}"),
                        model_id,
                    )
                {
                    return rendered;
                }
            }
            None => {
                if config.source == Some(TokenEstimationSource::Huggingface)
                    && let Some(repo) = config.repo.as_deref()
                {
                    let revision = config.revision.as_deref().unwrap_or("main");
                    let file = "tokenizer_config.json";
                    if let Some(path) = download_huggingface_token_file(
                        repo,
                        revision,
                        file,
                        config.cache_dir.as_deref(),
                    ) && let Some(template) = read_local_template(&path, "chat_template")
                        && let Some(rendered) = render_local_template_prompt_for_estimate(
                            &template,
                            input.messages,
                            input.tools,
                            input.pending_user_prompt,
                            format!("huggingface:{repo}@{revision}:{file}"),
                            model_id,
                        )
                    {
                        return rendered;
                    }
                }
            }
        }
    }
    render_builtin_prompt_for_estimate_with_model(
        input.messages,
        input.tools,
        input.pending_user_prompt,
        model_id,
    )
}

pub fn render_prompt_for_token_estimate(
    input: TokenEstimateInput<'_>,
    token_estimation: Option<&TokenEstimationConfig>,
) -> RenderedTokenEstimatePrompt {
    render_prompt_for_token_estimate_with_model(input, token_estimation, None)
}

pub fn estimate_rendered_tokens_for_model(
    rendered: &RenderedTokenEstimatePrompt,
    model: TokenEstimateModel<'_>,
) -> usize {
    if let Some(config) = model.token_estimation {
        let local_tokenizer_path = match config.tokenizer.as_ref() {
            Some(TokenEstimationTokenizerConfig::Local { path }) => Some(path.clone()),
            Some(TokenEstimationTokenizerConfig::Huggingface {
                repo,
                revision,
                file,
                cache_dir,
            }) => download_huggingface_token_file(repo, revision, file, cache_dir.as_deref()),
            None if config.source == Some(TokenEstimationSource::Huggingface) => {
                config.repo.as_deref().and_then(|repo| {
                    download_huggingface_token_file(
                        repo,
                        config.revision.as_deref().unwrap_or("main"),
                        "tokenizer.json",
                        config.cache_dir.as_deref(),
                    )
                })
            }
            _ => None,
        };
        if let Some(path) = local_tokenizer_path
            && let Some(tokenizer) = load_local_tokenizer(&path)
            && let Ok(encoding) = tokenizer.encode(rendered.text.as_str(), true)
        {
            return encoding
                .len()
                .max(1)
                .saturating_add(rendered.inline_payload_tokens);
        }
    }

    let estimator = match model
        .token_estimation
        .and_then(|config| config.tokenizer.as_ref())
    {
        Some(TokenEstimationTokenizerConfig::Tiktoken { encoding }) => {
            tiktoken_encoding_to_estimator(*encoding, model.model)
        }
        _ => token_estimator_for_model(model.model),
    };
    estimate_text_tokens_for_estimator(&rendered.text, estimator)
        .saturating_add(rendered.inline_payload_tokens)
}

pub fn estimate_session_tokens_for_request(
    input: TokenEstimateInput<'_>,
    model: TokenEstimateModel<'_>,
) -> usize {
    apply_prompt_token_calibration(
        model.model,
        estimate_session_tokens_for_request_uncalibrated(input, model),
    )
}

pub fn estimate_session_tokens_for_request_uncalibrated(
    input: TokenEstimateInput<'_>,
    model: TokenEstimateModel<'_>,
) -> usize {
    let rendered = render_prompt_for_token_estimate_with_model(
        input,
        model.token_estimation,
        Some(model.model),
    );
    estimate_rendered_tokens_for_model(&rendered, model)
}

pub fn estimate_session_tokens_for_estimator(
    messages: &[ChatMessage],
    tools: &[Tool],
    pending_user_prompt: &str,
    estimator: TokenEstimator,
) -> usize {
    let rendered = render_builtin_prompt_for_estimate(messages, tools, pending_user_prompt);
    estimate_text_tokens_for_estimator(&rendered.text, estimator)
        .saturating_add(rendered.inline_payload_tokens)
}

pub fn estimate_message_tokens_for_estimator(
    message: &ChatMessage,
    estimator: TokenEstimator,
) -> usize {
    estimate_session_tokens_for_estimator(std::slice::from_ref(message), &[], "", estimator)
}

pub fn estimate_session_tokens_for_model(
    messages: &[ChatMessage],
    tools: &[Tool],
    pending_user_prompt: &str,
    model: &str,
) -> usize {
    estimate_session_tokens_for_model_with_config(messages, tools, pending_user_prompt, model, None)
}

pub fn estimate_session_tokens_for_model_with_config(
    messages: &[ChatMessage],
    tools: &[Tool],
    pending_user_prompt: &str,
    model: &str,
    token_estimation: Option<&TokenEstimationConfig>,
) -> usize {
    estimate_session_tokens_for_request(
        TokenEstimateInput {
            messages,
            tools,
            pending_user_prompt,
        },
        TokenEstimateModel {
            model,
            token_estimation,
        },
    )
}

pub fn estimate_session_tokens_for_model_with_config_uncalibrated(
    messages: &[ChatMessage],
    tools: &[Tool],
    pending_user_prompt: &str,
    model: &str,
    token_estimation: Option<&TokenEstimationConfig>,
) -> usize {
    estimate_session_tokens_for_request_uncalibrated(
        TokenEstimateInput {
            messages,
            tools,
            pending_user_prompt,
        },
        TokenEstimateModel {
            model,
            token_estimation,
        },
    )
}

pub fn estimate_session_tokens_for_upstream(
    messages: &[ChatMessage],
    tools: &[Tool],
    pending_user_prompt: &str,
    upstream: &UpstreamConfig,
) -> usize {
    estimate_session_tokens_for_model_with_config(
        messages,
        tools,
        pending_user_prompt,
        &upstream.model,
        upstream.token_estimation.as_ref(),
    )
}

pub fn estimate_session_tokens_for_upstream_uncalibrated(
    messages: &[ChatMessage],
    tools: &[Tool],
    pending_user_prompt: &str,
    upstream: &UpstreamConfig,
) -> usize {
    estimate_session_tokens_for_model_with_config_uncalibrated(
        messages,
        tools,
        pending_user_prompt,
        &upstream.model,
        upstream.token_estimation.as_ref(),
    )
}

pub fn observe_prompt_tokens_for_upstream(
    upstream: &UpstreamConfig,
    messages: &[ChatMessage],
    tools: &[Tool],
    pending_user_prompt: &str,
    actual_prompt_tokens: u64,
) {
    let estimated_tokens = estimate_session_tokens_for_upstream_uncalibrated(
        messages,
        tools,
        pending_user_prompt,
        upstream,
    );
    observe_prompt_token_estimate(&upstream.model, estimated_tokens, actual_prompt_tokens);
}

pub fn estimate_session_tokens(
    messages: &[ChatMessage],
    tools: &[Tool],
    pending_user_prompt: &str,
) -> usize {
    estimate_session_tokens_for_estimator(
        messages,
        tools,
        pending_user_prompt,
        TokenEstimator::TiktokenO200k,
    )
}

#[cfg(test)]
mod tests {
    use super::{
        TokenEstimateInput, TokenEstimateModel, TokenEstimator,
        estimate_claude_image_tokens_from_dimensions, estimate_inline_image_tokens_for_model,
        estimate_rendered_tokens_for_model, estimate_session_tokens_for_estimator,
        estimate_session_tokens_for_model_with_config, estimate_session_tokens_for_request,
        estimate_session_tokens_for_request_uncalibrated, estimate_session_tokens_for_upstream,
        observe_prompt_token_estimate, prompt_token_calibration_for_model,
        render_builtin_prompt_for_estimate, render_builtin_prompt_for_estimate_with_model,
        render_prompt_for_token_estimate, token_estimator_for_model,
    };
    use crate::config::{
        AuthCredentialsStoreMode, RetryModeConfig, TokenEstimationConfig,
        TokenEstimationTemplateConfig, TokenEstimationTokenizerConfig, UpstreamApiKind,
        UpstreamAuthKind, UpstreamConfig,
    };
    use crate::message::{
        ChatMessage, FunctionCall, ToolCall, context_content_block, tool_result_content_block,
    };
    use crate::tooling::Tool;
    use base64::Engine as _;
    use image::{ImageBuffer, ImageFormat, Rgba};
    use serde_json::json;
    use std::fs;
    use std::io::Cursor;
    use tempfile::TempDir;

    fn synthetic_base64_payload(len: usize) -> String {
        let alphabet = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
        (0..len)
            .map(|index| alphabet[(index * 37 + index / 7) % alphabet.len()] as char)
            .collect()
    }

    fn png_data_url(width: u32, height: u32) -> String {
        let image = ImageBuffer::from_pixel(width, height, Rgba([12, 34, 56, 255]));
        let mut bytes = Vec::new();
        image::DynamicImage::ImageRgba8(image)
            .write_to(&mut Cursor::new(&mut bytes), ImageFormat::Png)
            .unwrap();
        let encoded = base64::engine::general_purpose::STANDARD.encode(bytes);
        format!("data:image/png;base64,{encoded}")
    }

    #[test]
    fn builtin_template_renders_messages_tools_and_pending_prompt() {
        let mut assistant = ChatMessage::text("assistant", "I will inspect.");
        assistant.tool_calls = Some(vec![ToolCall {
            id: "call_1".to_string(),
            kind: "function".to_string(),
            function: FunctionCall {
                name: "ls".to_string(),
                arguments: Some("{\"path\":\".\"}".to_string()),
            },
        }]);
        let tool = Tool::new(
            "ls",
            "List files.",
            json!({
                "type": "object",
                "properties": {"path": {"type": "string"}},
                "required": ["path"],
            }),
            |_| Ok(json!({})),
        );
        let rendered = render_builtin_prompt_for_estimate(
            &[
                ChatMessage::text("system", "You are helpful."),
                ChatMessage::text("user", "List files."),
                assistant,
                ChatMessage::tool_output("call_1", "ls", "- src/\n- Cargo.toml"),
            ],
            &[tool],
            "continue",
        );

        assert!(rendered.text.contains("<|tools|>"));
        assert!(rendered.text.contains("<|system|>\nYou are helpful."));
        assert!(rendered.text.contains("<|user|>\nList files."));
        assert!(
            rendered
                .text
                .contains("<|tool_call id=\"call_1\" name=\"ls\"")
        );
        assert!(
            rendered
                .text
                .contains("<|tool name=\"ls\" tool_call_id=\"call_1\"|>")
        );
        assert!(rendered.text.contains("<|user|>\ncontinue"));
        assert!(rendered.text.contains("<|assistant|>"));
        assert!(!rendered.text.contains("\"content\":null"));
        assert!(!rendered.text.contains("\"tool_calls\":null"));
    }

    #[test]
    fn builtin_template_discounts_inline_media_payloads() {
        let image_payload = synthetic_base64_payload(20_000);
        let file_payload = synthetic_base64_payload(16_000);
        let message = ChatMessage {
            role: "user".to_string(),
            content: Some(json!([
                {"type": "input_image", "image_url": format!("data:image/png;base64,{image_payload}")},
                {"type": "input_file", "filename": "notes.txt", "file_data": file_payload}
            ])),
            reasoning: None,
            name: None,
            tool_call_id: None,
            tool_calls: None,
        };
        let rendered = render_builtin_prompt_for_estimate(&[message], &[], "");

        assert!(rendered.inline_payload_tokens > 0);
        assert!(!rendered.text.contains(&image_payload));
        assert!(!rendered.text.contains(&file_payload));
        assert!(rendered.text.contains("inline image payload omitted"));
        assert!(rendered.text.contains("inline file payload omitted"));
    }

    #[test]
    fn gpt_models_use_openai_tile_estimator() {
        let image_url = png_data_url(1_000, 1_000);
        assert_eq!(
            estimate_inline_image_tokens_for_model(Some("openai/gpt-5"), &image_url),
            630
        );

        let message = ChatMessage {
            role: "user".to_string(),
            content: Some(json!([
                {"type": "input_image", "image_url": image_url}
            ])),
            reasoning: None,
            name: None,
            tool_call_id: None,
            tool_calls: None,
        };
        let rendered =
            render_builtin_prompt_for_estimate_with_model(&[message], &[], "", Some("gpt-5"));

        assert_eq!(rendered.inline_payload_tokens, 630);
        assert!(rendered.text.contains("inline image payload omitted"));
    }

    #[test]
    fn gpt_patch_models_use_openai_patch_estimator() {
        let image_url = png_data_url(1_024, 1_024);
        assert_eq!(
            estimate_inline_image_tokens_for_model(Some("gpt-5.3-codex"), &image_url),
            1_024
        );
    }

    #[test]
    fn non_gpt_models_use_anthropic_estimator() {
        let image_url = png_data_url(1_000, 1_000);
        assert_eq!(
            estimate_inline_image_tokens_for_model(Some("anthropic/claude-opus-4.6"), &image_url),
            1_334
        );
    }

    #[test]
    fn anthropic_image_estimate_scales_large_dimensions_before_capping() {
        assert_eq!(
            estimate_claude_image_tokens_from_dimensions(3_136, 1_568),
            1_568
        );
        assert_eq!(
            estimate_claude_image_tokens_from_dimensions(10_000, 100),
            34
        );
    }

    #[test]
    fn builtin_template_renders_context_and_tool_result_blocks() {
        let message = ChatMessage {
            role: "assistant".to_string(),
            content: Some(json!([
                {"type": "output_text", "text": "Need two preserved observations."},
                context_content_block(
                    Some("cache"),
                    Some("prefix was restored"),
                    Some(json!({"ttl": "5m"}))
                ),
                tool_result_content_block("call_1", "fetch", json!("first result"))
            ])),
            reasoning: None,
            name: None,
            tool_call_id: None,
            tool_calls: None,
        };

        let rendered = render_builtin_prompt_for_estimate(&[message], &[], "");
        assert!(rendered.text.contains("Need two preserved observations."));
        assert!(rendered.text.contains("[context: cache]"));
        assert!(rendered.text.contains("\"ttl\":\"5m\""));
        assert!(rendered.text.contains("[tool result: fetch id=call_1]"));
        assert!(rendered.text.contains("first result"));
    }

    #[test]
    fn token_estimator_follows_model_family() {
        assert_eq!(
            token_estimator_for_model("openai/gpt-5.4"),
            TokenEstimator::TiktokenO200k
        );
        assert_eq!(
            token_estimator_for_model("gpt-oss-120b"),
            TokenEstimator::TiktokenO200kHarmony
        );
        assert_eq!(
            token_estimator_for_model("anthropic/claude-opus-4.6"),
            TokenEstimator::TiktokenCl100k
        );
        assert_eq!(
            token_estimator_for_model("google/gemini-2.5-pro"),
            TokenEstimator::TiktokenCl100k
        );
    }

    #[test]
    fn builtin_estimator_counts_rendered_template() {
        let estimate = estimate_session_tokens_for_estimator(
            &[ChatMessage::text("user", "hello")],
            &[],
            "",
            TokenEstimator::TiktokenO200k,
        );

        assert!(estimate > 0);
    }

    fn test_upstream(
        model: &str,
        token_estimation: Option<TokenEstimationConfig>,
    ) -> UpstreamConfig {
        UpstreamConfig {
            base_url: "https://example.com/v1".to_string(),
            model: model.to_string(),
            api_kind: UpstreamApiKind::ChatCompletions,
            auth_kind: UpstreamAuthKind::ApiKey,
            supports_vision_input: false,
            supports_pdf_input: false,
            supports_audio_input: false,
            api_key: None,
            api_key_env: "OPENAI_API_KEY".to_string(),
            chat_completions_path: "/chat/completions".to_string(),
            codex_home: None,
            codex_auth: None,
            auth_credentials_store_mode: AuthCredentialsStoreMode::Auto,
            timeout_seconds: 120.0,
            retry_mode: RetryModeConfig::No,
            context_window_tokens: 128_000,
            cache_control: None,
            prompt_cache_retention: None,
            prompt_cache_key: None,
            reasoning: None,
            headers: serde_json::Map::new(),
            native_web_search: None,
            external_web_search: None,
            native_image_input: false,
            native_pdf_input: false,
            native_audio_input: false,
            native_image_generation: false,
            token_estimation,
        }
    }

    #[test]
    fn local_template_config_renders_chat_template() {
        let temp_dir = TempDir::new().unwrap();
        let template_path = temp_dir.path().join("tokenizer_config.json");
        fs::write(
            &template_path,
            serde_json::to_string(&json!({
                "chat_template": "{% for message in messages %}[{{ message.role }}] {{ message.content }}\n{% endfor %}[assistant]"
            }))
            .unwrap(),
        )
        .unwrap();

        let upstream = test_upstream(
            "qwen/qwen3",
            Some(TokenEstimationConfig {
                template: Some(TokenEstimationTemplateConfig::Local {
                    path: template_path,
                    field: "chat_template".to_string(),
                }),
                tokenizer: None,
                ..TokenEstimationConfig::default()
            }),
        );

        let message = ChatMessage::text("user", "Hello template");
        let local = estimate_session_tokens_for_upstream(
            std::slice::from_ref(&message),
            &[],
            "",
            &upstream,
        );
        let builtin = estimate_session_tokens_for_estimator(
            std::slice::from_ref(&message),
            &[],
            "",
            TokenEstimator::TiktokenCl100k,
        );
        assert_ne!(local, builtin);
    }

    #[test]
    fn explicit_request_render_matches_legacy_builtin_rendering() {
        let message = ChatMessage::text("user", "Hello template");
        let legacy = render_builtin_prompt_for_estimate(std::slice::from_ref(&message), &[], "");
        let explicit = render_prompt_for_token_estimate(
            TokenEstimateInput {
                messages: std::slice::from_ref(&message),
                tools: &[],
                pending_user_prompt: "",
            },
            None,
        );

        assert_eq!(explicit, legacy);
    }

    #[test]
    fn local_tokenizer_config_uses_tokenizer_json() {
        let temp_dir = TempDir::new().unwrap();
        let tokenizer_path = temp_dir.path().join("tokenizer.json");
        fs::write(
            &tokenizer_path,
            serde_json::to_string(&json!({
                "version": "1.0",
                "truncation": null,
                "padding": null,
                "added_tokens": [],
                "normalizer": null,
                "pre_tokenizer": {"type": "Whitespace"},
                "post_processor": null,
                "decoder": null,
                "model": {
                    "type": "WordLevel",
                    "vocab": {"[UNK]": 0, "<|user|>": 1, "Hello": 2, "<|assistant|>": 3},
                    "unk_token": "[UNK]"
                }
            }))
            .unwrap(),
        )
        .unwrap();

        let upstream = test_upstream(
            "local-model",
            Some(TokenEstimationConfig {
                template: None,
                tokenizer: Some(TokenEstimationTokenizerConfig::Local {
                    path: tokenizer_path,
                }),
                ..TokenEstimationConfig::default()
            }),
        );

        let estimated = estimate_session_tokens_for_upstream(
            &[ChatMessage::text("user", "Hello")],
            &[],
            "",
            &upstream,
        );
        assert!(
            estimated < 10,
            "local tokenizer should produce a tiny count, got {estimated}"
        );
    }

    #[test]
    fn explicit_request_estimator_matches_legacy_wrapper() {
        let upstream = test_upstream("anthropic/claude-opus-4.6", None);
        let message = ChatMessage::text("user", "count me");

        let legacy = estimate_session_tokens_for_model_with_config(
            std::slice::from_ref(&message),
            &[],
            "",
            &upstream.model,
            upstream.token_estimation.as_ref(),
        );
        let explicit = estimate_session_tokens_for_request(
            TokenEstimateInput {
                messages: std::slice::from_ref(&message),
                tools: &[],
                pending_user_prompt: "",
            },
            TokenEstimateModel {
                model: &upstream.model,
                token_estimation: upstream.token_estimation.as_ref(),
            },
        );

        assert_eq!(explicit, legacy);
    }

    #[test]
    fn rendered_prompt_estimate_respects_explicit_tiktoken_override() {
        let rendered =
            render_builtin_prompt_for_estimate(&[ChatMessage::text("user", "hello")], &[], "");
        let config = TokenEstimationConfig {
            tokenizer: Some(TokenEstimationTokenizerConfig::Tiktoken {
                encoding: crate::config::TokenEstimationTiktokenEncoding::Cl100kBase,
            }),
            ..TokenEstimationConfig::default()
        };

        let explicit = estimate_rendered_tokens_for_model(
            &rendered,
            TokenEstimateModel {
                model: "any-model",
                token_estimation: Some(&config),
            },
        );
        let wrapped = estimate_session_tokens_for_request_uncalibrated(
            TokenEstimateInput {
                messages: &[ChatMessage::text("user", "hello")],
                tools: &[],
                pending_user_prompt: "",
            },
            TokenEstimateModel {
                model: "any-model",
                token_estimation: Some(&config),
            },
        );

        assert_eq!(explicit, wrapped);
    }

    #[test]
    fn huggingface_file_filter_rejects_weight_files() {
        assert!(super::is_allowed_huggingface_token_file("tokenizer.json"));
        assert!(super::is_allowed_huggingface_token_file(
            "tokenizer_config.json"
        ));
        assert!(super::is_allowed_huggingface_token_file(
            "chat_template.jinja"
        ));
        assert!(!super::is_allowed_huggingface_token_file(
            "model.safetensors"
        ));
        assert!(!super::is_allowed_huggingface_token_file(
            "pytorch_model.bin"
        ));
        assert!(!super::is_allowed_huggingface_token_file("model.gguf"));
    }

    #[test]
    fn observed_prompt_usage_calibrates_future_estimates() {
        let model = format!("test-calibration-{}", uuid::Uuid::new_v4());
        let message = ChatMessage::text("user", "calibrate me");
        let before = estimate_session_tokens_for_model_with_config(
            std::slice::from_ref(&message),
            &[],
            "",
            &model,
            None,
        );
        observe_prompt_token_estimate(&model, before, (before as u64) * 2);
        let after = estimate_session_tokens_for_model_with_config(
            std::slice::from_ref(&message),
            &[],
            "",
            &model,
            None,
        );
        let (ratio, samples) = prompt_token_calibration_for_model(&model).unwrap();
        assert_eq!(samples, 1);
        assert!(ratio > 1.9);
        assert!(after > before);
    }
}
