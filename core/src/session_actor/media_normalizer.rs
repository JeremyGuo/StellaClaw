use std::{fs, io::Cursor, path::PathBuf};

use base64::{engine::general_purpose::STANDARD, Engine as _};
use image::{imageops::FilterType, ImageFormat, ImageReader};

use crate::model_config::{MediaInputConfig, MediaInputTransport, ModelCapability, ModelConfig};

use super::{ChatMessage, ChatMessageItem, ChatRole, ContextItem, FileItem, FileState};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MediaKind {
    Image,
    Pdf,
    Audio,
}

pub(crate) fn normalize_messages_for_model(
    messages: &[ChatMessage],
    model_config: &ModelConfig,
) -> Vec<ChatMessage> {
    messages
        .iter()
        .map(|message| normalize_message_for_model(message, model_config))
        .collect()
}

fn normalize_message_for_model(message: &ChatMessage, model_config: &ModelConfig) -> ChatMessage {
    let mut normalized = message.clone();
    normalized.data = message
        .data
        .iter()
        .flat_map(|item| normalize_item_for_model(item, &message.role, model_config))
        .collect();
    normalized
}

fn normalize_item_for_model(
    item: &ChatMessageItem,
    role: &ChatRole,
    model_config: &ModelConfig,
) -> Vec<ChatMessageItem> {
    if let ChatMessageItem::Reasoning(reasoning) = item {
        if reasoning.has_codex_encrypted_content() {
            return vec![item.clone()];
        }
        return Vec::new();
    }

    let ChatMessageItem::File(file) = item else {
        return vec![item.clone()];
    };
    if !matches!(role, ChatRole::User) {
        return vec![normalize_assistant_file_for_model(file, model_config)];
    }

    match normalize_file_for_model(file, model_config) {
        Ok(NormalizedFile::File(file)) => vec![ChatMessageItem::File(file)],
        Ok(NormalizedFile::Text(text)) => vec![ChatMessageItem::Context(ContextItem { text })],
        Ok(NormalizedFile::Unchanged) => vec![item.clone()],
        Err(reason) => vec![ChatMessageItem::Context(ContextItem {
            text: crashed_file_prompt(file, &reason),
        })],
    }
}

fn normalize_assistant_file_for_model(
    file: &FileItem,
    model_config: &ModelConfig,
) -> ChatMessageItem {
    match media_kind(file) {
        Some(MediaKind::Image) if model_config.supports(ModelCapability::ImageOut) => {
            ChatMessageItem::File(file.clone())
        }
        Some(MediaKind::Image) => ChatMessageItem::Context(ContextItem {
            text: file_reference_prompt(
                file,
                "model does not support image output in assistant history",
            ),
        }),
        Some(MediaKind::Pdf | MediaKind::Audio) => ChatMessageItem::Context(ContextItem {
            text: file_reference_prompt(
                file,
                "model does not support this media type as assistant output",
            ),
        }),
        None if model_config.supports(ModelCapability::FileIn) => {
            ChatMessageItem::File(file.clone())
        }
        None => ChatMessageItem::Context(ContextItem {
            text: file_reference_prompt(
                file,
                "model does not support generic file output in assistant history",
            ),
        }),
    }
}

enum NormalizedFile {
    File(FileItem),
    Text(String),
    Unchanged,
}

fn normalize_file_for_model(
    file: &FileItem,
    model_config: &ModelConfig,
) -> Result<NormalizedFile, String> {
    if let Some(FileState::Crashed { reason }) = &file.state {
        return Err(reason.clone());
    }

    let Some(kind) = media_kind(file) else {
        if model_config.supports(ModelCapability::FileIn) {
            return Ok(NormalizedFile::Unchanged);
        }
        return Ok(NormalizedFile::Text(file_reference_prompt(
            file,
            "model does not support generic file input",
        )));
    };

    if !model_supports_media_kind(model_config, kind) {
        return Ok(NormalizedFile::Text(file_reference_prompt(
            file,
            "model does not support this media type as direct input",
        )));
    }

    let Some(config) = media_input_config(model_config, kind) else {
        return Ok(NormalizedFile::Text(file_reference_prompt(
            file,
            "model has no direct input transport configured for this media type",
        )));
    };
    if matches!(config.transport, MediaInputTransport::FileReference) {
        return Ok(NormalizedFile::Unchanged);
    }

    match kind {
        MediaKind::Image => normalize_image_inline(file, config).map(NormalizedFile::File),
        MediaKind::Pdf | MediaKind::Audio => {
            normalize_binary_inline(file, config, kind).map(NormalizedFile::File)
        }
    }
}

fn normalize_image_inline(file: &FileItem, config: &MediaInputConfig) -> Result<FileItem, String> {
    let bytes = read_file_bytes(file)?;
    let reader = ImageReader::new(Cursor::new(bytes))
        .with_guessed_format()
        .map_err(|error| format!("failed to detect image format: {error}"))?;
    let source_format = reader
        .format()
        .ok_or_else(|| "failed to detect image format".to_string())?;
    let mut image = reader
        .decode()
        .map_err(|error| format!("failed to decode image: {error}"))?;

    if let (Some(max_width), Some(max_height)) = (config.max_width, config.max_height) {
        if image.width() > max_width || image.height() > max_height {
            image = image.resize(max_width, max_height, FilterType::Triangle);
        }
    } else if let Some(max_width) = config.max_width {
        if image.width() > max_width {
            image = image.resize(max_width, u32::MAX, FilterType::Triangle);
        }
    } else if let Some(max_height) = config.max_height {
        if image.height() > max_height {
            image = image.resize(u32::MAX, max_height, FilterType::Triangle);
        }
    }

    let source_media_type = image_format_media_type(source_format).unwrap_or("image/png");
    let target_media_type = if supports_media_type(config, source_media_type) {
        source_media_type
    } else {
        preferred_image_media_type(config)
    };
    let target_format = image_format_for_media_type(target_media_type)
        .ok_or_else(|| format!("unsupported target image media type {target_media_type}"))?;

    let mut cursor = Cursor::new(Vec::new());
    image
        .write_to(&mut cursor, target_format)
        .map_err(|error| format!("failed to encode image as {target_media_type}: {error}"))?;
    let encoded = STANDARD.encode(cursor.into_inner());

    Ok(FileItem {
        uri: format!("data:{target_media_type};base64,{encoded}"),
        name: file.name.clone(),
        media_type: Some(target_media_type.to_string()),
        width: Some(image.width()),
        height: Some(image.height()),
        state: None,
    })
}

fn normalize_binary_inline(
    file: &FileItem,
    config: &MediaInputConfig,
    kind: MediaKind,
) -> Result<FileItem, String> {
    let bytes = read_file_bytes(file)?;
    let detected = detect_binary_media_type(&bytes, kind)
        .ok_or_else(|| format!("failed to validate {:?} file signature", kind))?;
    if !supports_media_type(config, detected) {
        return Err(format!(
            "media type {detected} is not supported by this model"
        ));
    }
    Ok(FileItem {
        uri: format!("data:{detected};base64,{}", STANDARD.encode(bytes)),
        name: file.name.clone(),
        media_type: Some(detected.to_string()),
        width: None,
        height: None,
        state: None,
    })
}

fn media_input_config<'a>(
    model_config: &'a ModelConfig,
    kind: MediaKind,
) -> Option<&'a MediaInputConfig> {
    let multimodal = model_config.multimodal_input.as_ref()?;
    match kind {
        MediaKind::Image => multimodal.image.as_ref(),
        MediaKind::Pdf => multimodal.pdf.as_ref(),
        MediaKind::Audio => multimodal.audio.as_ref(),
    }
}

fn model_supports_media_kind(model_config: &ModelConfig, kind: MediaKind) -> bool {
    match kind {
        MediaKind::Image => model_config.supports(ModelCapability::ImageIn),
        MediaKind::Pdf => model_config.supports(ModelCapability::PdfIn),
        MediaKind::Audio => model_config.supports(ModelCapability::AudioIn),
    }
}

fn media_kind(file: &FileItem) -> Option<MediaKind> {
    let media_type = file.media_type.as_deref()?;
    if media_type.starts_with("image/") {
        Some(MediaKind::Image)
    } else if media_type == "application/pdf" {
        Some(MediaKind::Pdf)
    } else if media_type.starts_with("audio/") {
        Some(MediaKind::Audio)
    } else {
        None
    }
}

fn read_file_bytes(file: &FileItem) -> Result<Vec<u8>, String> {
    if file.uri.starts_with("data:") {
        return data_url_bytes(&file.uri);
    }
    let path = local_file_path(file)?;
    fs::read(&path).map_err(|error| format!("failed to read {}: {error}", path.display()))
}

fn local_file_path(file: &FileItem) -> Result<PathBuf, String> {
    file.uri
        .strip_prefix("file://")
        .map(PathBuf::from)
        .ok_or_else(|| {
            format!(
                "file must be local file:// URI for inline transport: {}",
                file.uri
            )
        })
}

fn data_url_bytes(uri: &str) -> Result<Vec<u8>, String> {
    let (_, payload) = uri
        .split_once(',')
        .ok_or_else(|| "malformed data URL".to_string())?;
    STANDARD
        .decode(payload)
        .map_err(|error| format!("failed to decode data URL: {error}"))
}

fn supports_media_type(config: &MediaInputConfig, media_type: &str) -> bool {
    config.supported_media_types.is_empty()
        || config
            .supported_media_types
            .iter()
            .any(|supported| supported == media_type)
}

fn preferred_image_media_type(config: &MediaInputConfig) -> &'static str {
    for media_type in &config.supported_media_types {
        if image_format_for_media_type(media_type).is_some() {
            return match media_type.as_str() {
                "image/jpeg" => "image/jpeg",
                "image/webp" => "image/webp",
                _ => "image/png",
            };
        }
    }
    "image/png"
}

fn image_format_media_type(format: ImageFormat) -> Option<&'static str> {
    match format {
        ImageFormat::Png => Some("image/png"),
        ImageFormat::Jpeg => Some("image/jpeg"),
        ImageFormat::WebP => Some("image/webp"),
        ImageFormat::Gif => Some("image/gif"),
        ImageFormat::Bmp => Some("image/bmp"),
        ImageFormat::Tiff => Some("image/tiff"),
        _ => None,
    }
}

fn image_format_for_media_type(media_type: &str) -> Option<ImageFormat> {
    match media_type {
        "image/png" => Some(ImageFormat::Png),
        "image/jpeg" => Some(ImageFormat::Jpeg),
        "image/webp" => Some(ImageFormat::WebP),
        "image/gif" => Some(ImageFormat::Gif),
        "image/bmp" => Some(ImageFormat::Bmp),
        "image/tiff" => Some(ImageFormat::Tiff),
        _ => None,
    }
}

fn detect_binary_media_type(bytes: &[u8], kind: MediaKind) -> Option<&'static str> {
    match kind {
        MediaKind::Pdf if bytes.starts_with(b"%PDF-") => Some("application/pdf"),
        MediaKind::Audio if bytes.starts_with(b"ID3") || is_mp3_frame(bytes) => Some("audio/mpeg"),
        MediaKind::Audio if bytes.starts_with(b"fLaC") => Some("audio/flac"),
        MediaKind::Audio if bytes.starts_with(b"OggS") => Some("audio/ogg"),
        MediaKind::Audio
            if bytes.len() >= 12 && &bytes[0..4] == b"RIFF" && &bytes[8..12] == b"WAVE" =>
        {
            Some("audio/wav")
        }
        MediaKind::Audio if bytes.len() >= 12 && &bytes[4..8] == b"ftyp" => Some("audio/mp4"),
        _ => None,
    }
}

fn is_mp3_frame(bytes: &[u8]) -> bool {
    bytes.len() >= 2 && bytes[0] == 0xFF && (bytes[1] & 0xE0) == 0xE0
}

fn crashed_file_prompt(file: &FileItem, reason: &str) -> String {
    format!(
        "[Crashed file omitted from multimodal input]\nuri: {}\nname: {}\nmedia_type: {}\nreason: {}",
        file.uri,
        file.name.as_deref().unwrap_or("<unknown>"),
        file.media_type.as_deref().unwrap_or("<unknown>"),
        reason
    )
}

fn file_reference_prompt(file: &FileItem, reason: &str) -> String {
    format!(
        "[File attached as text reference]\nuri: {}\nname: {}\nmedia_type: {}\nreason: {}",
        file.uri,
        file.name.as_deref().unwrap_or("<unknown>"),
        file.media_type.as_deref().unwrap_or("<unknown>"),
        reason
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        model_config::{
            MediaInputConfig, MediaInputTransport, ModelCapability, MultimodalInputConfig,
        },
        session_actor::{ChatMessage, ChatRole, ReasoningItem},
    };

    #[test]
    fn crashed_file_becomes_text_prompt_before_provider_request() {
        let config = ModelConfig {
            provider_type: crate::model_config::ProviderType::OpenRouterCompletion,
            model_name: "test".to_string(),
            url: "http://localhost".to_string(),
            api_key_env: "TEST".to_string(),
            capabilities: Vec::new(),
            token_max_context: 1,
            max_tokens: 0,
            cache_timeout: 0,
            conn_timeout: 1,
            request_timeout: 600,
            max_request_size: 30 * 1024 * 1024,
            retry_mode: crate::model_config::RetryMode::Once,
            reasoning: None,
            token_estimator_type: crate::model_config::TokenEstimatorType::Local,
            multimodal_estimator: None,
            multimodal_input: Some(MultimodalInputConfig {
                image: Some(MediaInputConfig {
                    transport: MediaInputTransport::InlineBase64,
                    supported_media_types: vec!["image/png".to_string()],
                    max_width: None,
                    max_height: None,
                }),
                pdf: None,
                audio: None,
            }),
            token_estimator_url: None,
        };
        let messages = vec![ChatMessage::new(
            ChatRole::User,
            vec![ChatMessageItem::File(FileItem {
                uri: "file:///tmp/missing.png".to_string(),
                name: Some("missing.png".to_string()),
                media_type: Some("image/png".to_string()),
                width: None,
                height: None,
                state: Some(FileState::Crashed {
                    reason: "decode failed".to_string(),
                }),
            })],
        )];

        let normalized = normalize_messages_for_model(&messages, &config);

        assert!(matches!(
            &normalized[0].data[0],
            ChatMessageItem::Context(context) if context.text.contains("decode failed")
        ));
    }

    #[test]
    fn reasoning_is_removed_during_model_normalization() {
        let config = ModelConfig {
            provider_type: crate::model_config::ProviderType::OpenRouterCompletion,
            model_name: "test".to_string(),
            url: "http://localhost".to_string(),
            api_key_env: "TEST".to_string(),
            capabilities: Vec::new(),
            token_max_context: 1,
            max_tokens: 0,
            cache_timeout: 0,
            conn_timeout: 1,
            request_timeout: 600,
            max_request_size: 30 * 1024 * 1024,
            retry_mode: crate::model_config::RetryMode::Once,
            reasoning: None,
            token_estimator_type: crate::model_config::TokenEstimatorType::Local,
            multimodal_estimator: None,
            multimodal_input: None,
            token_estimator_url: None,
        };
        let messages = vec![ChatMessage::new(
            ChatRole::Assistant,
            vec![
                ChatMessageItem::Reasoning(ReasoningItem::from_text("hidden")),
                ChatMessageItem::Context(ContextItem {
                    text: "visible".to_string(),
                }),
            ],
        )];

        let normalized = normalize_messages_for_model(&messages, &config);

        assert_eq!(normalized.len(), 1);
        assert_eq!(normalized[0].data.len(), 1);
        assert!(matches!(
            &normalized[0].data[0],
            ChatMessageItem::Context(context) if context.text == "visible"
        ));
    }

    #[test]
    fn codex_encrypted_reasoning_is_retained_during_model_normalization() {
        let config = ModelConfig {
            provider_type: crate::model_config::ProviderType::CodexSubscription,
            model_name: "gpt-5.5".to_string(),
            url: "http://localhost".to_string(),
            api_key_env: "TEST".to_string(),
            capabilities: Vec::new(),
            token_max_context: 1,
            max_tokens: 0,
            cache_timeout: 0,
            conn_timeout: 1,
            request_timeout: 600,
            max_request_size: 30 * 1024 * 1024,
            retry_mode: crate::model_config::RetryMode::Once,
            reasoning: None,
            token_estimator_type: crate::model_config::TokenEstimatorType::Local,
            multimodal_estimator: None,
            multimodal_input: None,
            token_estimator_url: None,
        };
        let messages = vec![ChatMessage::new(
            ChatRole::Assistant,
            vec![ChatMessageItem::Reasoning(ReasoningItem::codex(
                Some("summary".to_string()),
                Some("encrypted".to_string()),
                Some("raw text".to_string()),
            ))],
        )];

        let normalized = normalize_messages_for_model(&messages, &config);

        assert_eq!(normalized.len(), 1);
        assert_eq!(normalized[0].data.len(), 1);
        assert!(matches!(
            &normalized[0].data[0],
            ChatMessageItem::Reasoning(reasoning)
                if reasoning.codex_encrypted_content.as_deref() == Some("encrypted")
        ));
    }

    #[test]
    fn unsupported_pdf_is_downgraded_to_text_reference() {
        let config = ModelConfig {
            provider_type: crate::model_config::ProviderType::ClaudeCode,
            model_name: "claude-opus-4-6".to_string(),
            url: "http://localhost".to_string(),
            api_key_env: "TEST".to_string(),
            capabilities: vec![ModelCapability::Chat, ModelCapability::ImageIn],
            token_max_context: 1,
            max_tokens: 0,
            cache_timeout: 0,
            conn_timeout: 1,
            request_timeout: 600,
            max_request_size: 30 * 1024 * 1024,
            retry_mode: crate::model_config::RetryMode::Once,
            reasoning: None,
            token_estimator_type: crate::model_config::TokenEstimatorType::Local,
            multimodal_estimator: None,
            multimodal_input: Some(MultimodalInputConfig {
                image: Some(MediaInputConfig {
                    transport: MediaInputTransport::InlineBase64,
                    supported_media_types: vec!["image/png".to_string()],
                    max_width: None,
                    max_height: None,
                }),
                pdf: None,
                audio: None,
            }),
            token_estimator_url: None,
        };
        let messages = vec![ChatMessage::new(
            ChatRole::User,
            vec![ChatMessageItem::File(FileItem {
                uri: "file:///tmp/report.pdf".to_string(),
                name: Some("report.pdf".to_string()),
                media_type: Some("application/pdf".to_string()),
                width: None,
                height: None,
                state: None,
            })],
        )];

        let normalized = normalize_messages_for_model(&messages, &config);

        assert!(matches!(
            &normalized[0].data[0],
            ChatMessageItem::Context(context)
                if context.text.contains("file:///tmp/report.pdf")
                    && context.text.contains("model does not support")
        ));
    }

    #[test]
    fn unsupported_image_is_downgraded_to_text_reference() {
        let config = ModelConfig {
            provider_type: crate::model_config::ProviderType::ClaudeCode,
            model_name: "text-only".to_string(),
            url: "http://localhost".to_string(),
            api_key_env: "TEST".to_string(),
            capabilities: vec![ModelCapability::Chat],
            token_max_context: 1,
            max_tokens: 0,
            cache_timeout: 0,
            conn_timeout: 1,
            request_timeout: 600,
            max_request_size: 30 * 1024 * 1024,
            retry_mode: crate::model_config::RetryMode::Once,
            reasoning: None,
            token_estimator_type: crate::model_config::TokenEstimatorType::Local,
            multimodal_estimator: None,
            multimodal_input: None,
            token_estimator_url: None,
        };
        let messages = vec![ChatMessage::new(
            ChatRole::User,
            vec![ChatMessageItem::File(FileItem {
                uri: "file:///tmp/cat.png".to_string(),
                name: Some("cat.png".to_string()),
                media_type: Some("image/png".to_string()),
                width: Some(640),
                height: Some(480),
                state: None,
            })],
        )];

        let normalized = normalize_messages_for_model(&messages, &config);

        assert!(matches!(
            &normalized[0].data[0],
            ChatMessageItem::Context(context)
                if context.text.contains("file:///tmp/cat.png")
                    && context.text.contains("model does not support")
        ));
    }

    #[test]
    fn assistant_image_without_image_out_is_downgraded_to_text_reference() {
        let config = ModelConfig {
            provider_type: crate::model_config::ProviderType::ClaudeCode,
            model_name: "claude-opus-4-6".to_string(),
            url: "http://localhost".to_string(),
            api_key_env: "TEST".to_string(),
            capabilities: vec![ModelCapability::Chat, ModelCapability::ImageIn],
            token_max_context: 1,
            max_tokens: 0,
            cache_timeout: 0,
            conn_timeout: 1,
            request_timeout: 600,
            max_request_size: 30 * 1024 * 1024,
            retry_mode: crate::model_config::RetryMode::Once,
            reasoning: None,
            token_estimator_type: crate::model_config::TokenEstimatorType::Local,
            multimodal_estimator: None,
            multimodal_input: Some(MultimodalInputConfig {
                image: Some(MediaInputConfig {
                    transport: MediaInputTransport::InlineBase64,
                    supported_media_types: vec!["image/png".to_string()],
                    max_width: Some(2000),
                    max_height: Some(2000),
                }),
                pdf: None,
                audio: None,
            }),
            token_estimator_url: None,
        };
        let messages = vec![ChatMessage::new(
            ChatRole::Assistant,
            vec![ChatMessageItem::File(FileItem {
                uri: "file:///tmp/generated.png".to_string(),
                name: Some("generated.png".to_string()),
                media_type: Some("image/png".to_string()),
                width: Some(4096),
                height: Some(4096),
                state: None,
            })],
        )];

        let normalized = normalize_messages_for_model(&messages, &config);

        assert!(matches!(
            &normalized[0].data[0],
            ChatMessageItem::Context(context)
                if context.text.contains("file:///tmp/generated.png")
                    && context.text.contains("image output")
        ));
    }

    #[test]
    fn assistant_image_with_image_out_is_preserved() {
        let config = ModelConfig {
            provider_type: crate::model_config::ProviderType::OpenRouterCompletion,
            model_name: "image-output-model".to_string(),
            url: "http://localhost".to_string(),
            api_key_env: "TEST".to_string(),
            capabilities: vec![ModelCapability::Chat, ModelCapability::ImageOut],
            token_max_context: 1,
            max_tokens: 0,
            cache_timeout: 0,
            conn_timeout: 1,
            request_timeout: 600,
            max_request_size: 30 * 1024 * 1024,
            retry_mode: crate::model_config::RetryMode::Once,
            reasoning: None,
            token_estimator_type: crate::model_config::TokenEstimatorType::Local,
            multimodal_estimator: None,
            multimodal_input: None,
            token_estimator_url: None,
        };
        let file = FileItem {
            uri: "file:///tmp/generated.png".to_string(),
            name: Some("generated.png".to_string()),
            media_type: Some("image/png".to_string()),
            width: Some(4096),
            height: Some(4096),
            state: None,
        };
        let messages = vec![ChatMessage::new(
            ChatRole::Assistant,
            vec![ChatMessageItem::File(file.clone())],
        )];

        let normalized = normalize_messages_for_model(&messages, &config);

        assert_eq!(normalized[0].data, vec![ChatMessageItem::File(file)]);
    }

    #[test]
    fn unsupported_audio_is_downgraded_to_text_reference() {
        let config = ModelConfig {
            provider_type: crate::model_config::ProviderType::ClaudeCode,
            model_name: "claude-opus-4-6".to_string(),
            url: "http://localhost".to_string(),
            api_key_env: "TEST".to_string(),
            capabilities: vec![ModelCapability::Chat, ModelCapability::ImageIn],
            token_max_context: 1,
            max_tokens: 0,
            cache_timeout: 0,
            conn_timeout: 1,
            request_timeout: 600,
            max_request_size: 30 * 1024 * 1024,
            retry_mode: crate::model_config::RetryMode::Once,
            reasoning: None,
            token_estimator_type: crate::model_config::TokenEstimatorType::Local,
            multimodal_estimator: None,
            multimodal_input: Some(MultimodalInputConfig {
                image: Some(MediaInputConfig {
                    transport: MediaInputTransport::InlineBase64,
                    supported_media_types: vec!["image/png".to_string()],
                    max_width: None,
                    max_height: None,
                }),
                pdf: None,
                audio: None,
            }),
            token_estimator_url: None,
        };
        let messages = vec![ChatMessage::new(
            ChatRole::User,
            vec![ChatMessageItem::File(FileItem {
                uri: "file:///tmp/voice.mp3".to_string(),
                name: Some("voice.mp3".to_string()),
                media_type: Some("audio/mpeg".to_string()),
                width: None,
                height: None,
                state: None,
            })],
        )];

        let normalized = normalize_messages_for_model(&messages, &config);

        assert!(matches!(
            &normalized[0].data[0],
            ChatMessageItem::Context(context)
                if context.text.contains("file:///tmp/voice.mp3")
                    && context.text.contains("model does not support")
        ));
    }

    #[test]
    fn unsupported_generic_file_is_downgraded_to_text_reference() {
        let config = ModelConfig {
            provider_type: crate::model_config::ProviderType::OpenRouterCompletion,
            model_name: "text-only".to_string(),
            url: "http://localhost".to_string(),
            api_key_env: "TEST".to_string(),
            capabilities: vec![ModelCapability::Chat],
            token_max_context: 1,
            max_tokens: 0,
            cache_timeout: 0,
            conn_timeout: 1,
            request_timeout: 600,
            max_request_size: 30 * 1024 * 1024,
            retry_mode: crate::model_config::RetryMode::Once,
            reasoning: None,
            token_estimator_type: crate::model_config::TokenEstimatorType::Local,
            multimodal_estimator: None,
            multimodal_input: None,
            token_estimator_url: None,
        };
        let messages = vec![ChatMessage::new(
            ChatRole::User,
            vec![ChatMessageItem::File(FileItem {
                uri: "file:///tmp/notes.txt".to_string(),
                name: Some("notes.txt".to_string()),
                media_type: Some("text/plain".to_string()),
                width: None,
                height: None,
                state: None,
            })],
        )];

        let normalized = normalize_messages_for_model(&messages, &config);

        assert!(matches!(
            &normalized[0].data[0],
            ChatMessageItem::Context(context)
                if context.text.contains("file:///tmp/notes.txt")
                    && context.text.contains("generic file input")
        ));
    }

    #[test]
    fn supported_generic_file_reference_is_preserved() {
        let config = ModelConfig {
            provider_type: crate::model_config::ProviderType::CodexSubscription,
            model_name: "gpt-5.5".to_string(),
            url: "http://localhost".to_string(),
            api_key_env: "TEST".to_string(),
            capabilities: vec![ModelCapability::Chat, ModelCapability::FileIn],
            token_max_context: 1,
            max_tokens: 0,
            cache_timeout: 0,
            conn_timeout: 1,
            request_timeout: 600,
            max_request_size: 30 * 1024 * 1024,
            retry_mode: crate::model_config::RetryMode::Once,
            reasoning: None,
            token_estimator_type: crate::model_config::TokenEstimatorType::Local,
            multimodal_estimator: None,
            multimodal_input: None,
            token_estimator_url: None,
        };
        let file = FileItem {
            uri: "file:///tmp/notes.txt".to_string(),
            name: Some("notes.txt".to_string()),
            media_type: Some("text/plain".to_string()),
            width: None,
            height: None,
            state: None,
        };
        let messages = vec![ChatMessage::new(
            ChatRole::User,
            vec![ChatMessageItem::File(file.clone())],
        )];

        let normalized = normalize_messages_for_model(&messages, &config);

        assert_eq!(normalized[0].data, vec![ChatMessageItem::File(file)]);
    }

    #[test]
    fn supported_pdf_file_reference_is_preserved() {
        let config = ModelConfig {
            provider_type: crate::model_config::ProviderType::OpenRouterResponses,
            model_name: "pdf-model".to_string(),
            url: "http://localhost".to_string(),
            api_key_env: "TEST".to_string(),
            capabilities: vec![ModelCapability::Chat, ModelCapability::PdfIn],
            token_max_context: 1,
            max_tokens: 0,
            cache_timeout: 0,
            conn_timeout: 1,
            request_timeout: 600,
            max_request_size: 30 * 1024 * 1024,
            retry_mode: crate::model_config::RetryMode::Once,
            reasoning: None,
            token_estimator_type: crate::model_config::TokenEstimatorType::Local,
            multimodal_estimator: None,
            multimodal_input: Some(MultimodalInputConfig {
                image: None,
                pdf: Some(MediaInputConfig {
                    transport: MediaInputTransport::FileReference,
                    supported_media_types: vec!["application/pdf".to_string()],
                    max_width: None,
                    max_height: None,
                }),
                audio: None,
            }),
            token_estimator_url: None,
        };
        let file = FileItem {
            uri: "file:///tmp/report.pdf".to_string(),
            name: Some("report.pdf".to_string()),
            media_type: Some("application/pdf".to_string()),
            width: None,
            height: None,
            state: None,
        };
        let messages = vec![ChatMessage::new(
            ChatRole::User,
            vec![ChatMessageItem::File(file.clone())],
        )];

        let normalized = normalize_messages_for_model(&messages, &config);

        assert_eq!(normalized[0].data, vec![ChatMessageItem::File(file)]);
    }

    #[test]
    fn supported_audio_file_reference_is_preserved() {
        let config = ModelConfig {
            provider_type: crate::model_config::ProviderType::OpenRouterResponses,
            model_name: "audio-model".to_string(),
            url: "http://localhost".to_string(),
            api_key_env: "TEST".to_string(),
            capabilities: vec![ModelCapability::Chat, ModelCapability::AudioIn],
            token_max_context: 1,
            max_tokens: 0,
            cache_timeout: 0,
            conn_timeout: 1,
            request_timeout: 600,
            max_request_size: 30 * 1024 * 1024,
            retry_mode: crate::model_config::RetryMode::Once,
            reasoning: None,
            token_estimator_type: crate::model_config::TokenEstimatorType::Local,
            multimodal_estimator: None,
            multimodal_input: Some(MultimodalInputConfig {
                image: None,
                pdf: None,
                audio: Some(MediaInputConfig {
                    transport: MediaInputTransport::FileReference,
                    supported_media_types: vec!["audio/mpeg".to_string()],
                    max_width: None,
                    max_height: None,
                }),
            }),
            token_estimator_url: None,
        };
        let file = FileItem {
            uri: "file:///tmp/voice.mp3".to_string(),
            name: Some("voice.mp3".to_string()),
            media_type: Some("audio/mpeg".to_string()),
            width: None,
            height: None,
            state: None,
        };
        let messages = vec![ChatMessage::new(
            ChatRole::User,
            vec![ChatMessageItem::File(file.clone())],
        )];

        let normalized = normalize_messages_for_model(&messages, &config);

        assert_eq!(normalized[0].data, vec![ChatMessageItem::File(file)]);
    }
}
