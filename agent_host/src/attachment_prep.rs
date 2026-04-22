use crate::domain::StoredAttachment;
use anyhow::{Context, Result, anyhow};
use base64::Engine;
use image::{ImageFormat, ImageReader};
use serde_json::{Value, json};
use std::io::Cursor;
use std::path::Path;

pub fn build_image_data_url(attachment: &StoredAttachment) -> Result<String> {
    let bytes = std::fs::read(&attachment.path).with_context(|| {
        format!(
            "failed to read image attachment {}",
            attachment.path.display()
        )
    })?;
    if let Some(media_type) = image_media_type_hint(attachment) {
        let encoded = base64::engine::general_purpose::STANDARD.encode(&bytes);
        let original_url = format!("data:{media_type};base64,{encoded}");
        return match normalize_inline_image_url("image_url", &original_url)? {
            InlineImageUrlNormalization::Unchanged => Ok(original_url),
            InlineImageUrlNormalization::Rewritten(url) => Ok(url),
        };
    }

    transcode_inline_image_bytes_to_png_data_url(&bytes, &attachment.path.display().to_string())
}

pub fn normalize_stored_attachment_for_persistence(
    mut attachment: StoredAttachment,
) -> Result<StoredAttachment> {
    if !attachment.kind.is_image() {
        return Ok(attachment);
    }

    let Some(media_type) = image_media_type_hint(&attachment) else {
        return Ok(attachment);
    };

    if let Some(canonical) = canonical_inline_image_media_type(&media_type) {
        if attachment.media_type.as_deref() != Some(canonical) {
            attachment.media_type = Some(canonical.to_string());
        }
        return Ok(attachment);
    }

    let original_path = attachment.path.clone();
    let bytes = std::fs::read(&original_path).with_context(|| {
        format!(
            "failed to read image attachment {} for persistence normalization",
            original_path.display()
        )
    })?;
    let output =
        match transcode_inline_image_bytes_to_png(&bytes, &original_path.display().to_string()) {
            Ok(output) => output,
            Err(_) => return Ok(attachment),
        };

    let target_path = original_path.with_extension("png");
    std::fs::write(&target_path, &output)
        .with_context(|| format!("failed to write normalized image {}", target_path.display()))?;
    if target_path != original_path {
        std::fs::remove_file(&original_path).with_context(|| {
            format!(
                "failed to remove superseded image attachment {}",
                original_path.display()
            )
        })?;
        attachment.path = target_path;
    }
    attachment.media_type = Some("image/png".to_string());
    attachment.size_bytes = output.len() as u64;
    Ok(attachment)
}

pub fn build_pdf_content_item(attachment: &StoredAttachment) -> Result<Value> {
    let encoded = file_to_base64(attachment)?;
    Ok(json!({
        "type": "file",
        "file": {
            "file_data": encoded,
            "filename": attachment_filename(attachment, "document.pdf"),
        }
    }))
}

pub fn build_audio_content_item(attachment: &StoredAttachment) -> Result<Value> {
    let encoded = file_to_base64(attachment)?;
    let format = infer_audio_format_for_attachment(attachment).ok_or_else(|| {
        anyhow!(
            "unsupported audio attachment format for {}",
            attachment.path.display()
        )
    })?;
    Ok(json!({
        "type": "input_audio",
        "input_audio": {
            "data": encoded,
            "format": format,
        }
    }))
}

pub fn infer_audio_format_for_attachment(attachment: &StoredAttachment) -> Option<&'static str> {
    if let Some(media_type) = attachment.media_type.as_deref() {
        match media_type.to_ascii_lowercase().as_str() {
            "audio/wav" | "audio/wave" | "audio/x-wav" => return Some("wav"),
            "audio/mpeg" | "audio/mp3" | "audio/mpga" => return Some("mp3"),
            "audio/ogg" | "audio/opus" => return Some("ogg"),
            "audio/webm" => return Some("webm"),
            "audio/mp4" | "audio/aac" | "audio/m4a" => return Some("m4a"),
            "audio/flac" => return Some("flac"),
            _ => {}
        }
    }
    match attachment
        .path
        .extension()
        .and_then(|value| value.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase()
        .as_str()
    {
        "wav" => Some("wav"),
        "mp3" | "mpeg" | "mpga" => Some("mp3"),
        "ogg" | "opus" => Some("ogg"),
        "webm" => Some("webm"),
        "m4a" | "mp4" | "aac" => Some("m4a"),
        "flac" => Some("flac"),
        _ => None,
    }
}

pub fn unsupported_inline_image_placeholder_text() -> &'static str {
    "[Earlier image omitted because it could not be converted into a supported inline image format (JPEG, PNG, GIF, or WebP).]"
}

pub fn sanitize_inline_image_item(item_type: &str, item: &Value) -> Result<Value> {
    let Some(url) = inline_image_url_from_item(item_type, item) else {
        return Ok(item.clone());
    };
    match normalize_inline_image_url(item_type, url)? {
        InlineImageUrlNormalization::Unchanged => Ok(item.clone()),
        InlineImageUrlNormalization::Rewritten(url) => {
            Ok(rebuild_inline_image_item(item_type, item, url))
        }
    }
}

pub fn normalize_inline_image_content_for_persistence(
    content: &Option<Value>,
) -> Result<Option<Value>> {
    let Some(Value::Array(items)) = content else {
        return Ok(content.clone());
    };

    let mut rewritten = Vec::with_capacity(items.len());
    let mut changed = false;
    for item in items {
        let Some(item_type) = item.get("type").and_then(Value::as_str) else {
            rewritten.push(item.clone());
            continue;
        };
        if !matches!(item_type, "image_url" | "input_image") {
            rewritten.push(item.clone());
            continue;
        }

        match sanitize_inline_image_item(item_type, item) {
            Ok(value) => {
                changed |= value != *item;
                rewritten.push(value);
            }
            Err(_) => {
                changed = true;
                rewritten.push(json!({
                    "type": "text",
                    "text": unsupported_inline_image_placeholder_text(),
                }));
            }
        }
    }

    if changed {
        Ok(Some(Value::Array(rewritten)))
    } else {
        Ok(content.clone())
    }
}

enum InlineImageUrlNormalization {
    Unchanged,
    Rewritten(String),
}

fn file_to_base64(attachment: &StoredAttachment) -> Result<String> {
    let bytes = std::fs::read(&attachment.path)
        .with_context(|| format!("failed to read attachment {}", attachment.path.display()))?;
    Ok(base64::engine::general_purpose::STANDARD.encode(bytes))
}

fn attachment_filename(attachment: &StoredAttachment, fallback: &str) -> String {
    attachment
        .original_name
        .as_deref()
        .filter(|value| !value.trim().is_empty())
        .map(ToOwned::to_owned)
        .or_else(|| {
            attachment
                .path
                .file_name()
                .map(|value| value.to_string_lossy().to_string())
        })
        .unwrap_or_else(|| fallback.to_string())
}

fn inline_image_url_from_item<'a>(item_type: &str, item: &'a Value) -> Option<&'a str> {
    if item_type == "image_url" {
        return item.get("image_url").and_then(|value| match value {
            Value::String(url) => Some(url.as_str()),
            Value::Object(object) => object.get("url").and_then(Value::as_str),
            _ => None,
        });
    }
    item.get("image_url").and_then(Value::as_str)
}

fn rebuild_inline_image_item(item_type: &str, item: &Value, url: String) -> Value {
    let mut rebuilt = item.clone();
    let Some(object) = rebuilt.as_object_mut() else {
        return item.clone();
    };
    if item_type == "image_url" {
        object.insert("image_url".to_string(), json!({ "url": url }));
    } else {
        object.insert("image_url".to_string(), Value::String(url));
    }
    rebuilt
}

fn normalize_inline_image_url(item_type: &str, url: &str) -> Result<InlineImageUrlNormalization> {
    let Some((media_type, encoded)) = parse_inline_data_url(url) else {
        return Ok(InlineImageUrlNormalization::Unchanged);
    };
    let media_type = media_type.trim();
    let is_image_data_url = media_type.starts_with("image/");
    if is_image_data_url && let Some(canonical) = canonical_inline_image_media_type(media_type) {
        if media_type.eq_ignore_ascii_case(canonical) {
            return Ok(InlineImageUrlNormalization::Unchanged);
        }
        return Ok(InlineImageUrlNormalization::Rewritten(format!(
            "data:{canonical};base64,{encoded}"
        )));
    }
    if !matches!(item_type, "image_url" | "input_image") && !is_image_data_url {
        return Ok(InlineImageUrlNormalization::Unchanged);
    }

    let bytes = base64::engine::general_purpose::STANDARD
        .decode(encoded)
        .context("failed to decode unsupported inline image data")?;
    let rewritten = transcode_inline_image_bytes_to_png_data_url(&bytes, media_type)?;
    Ok(InlineImageUrlNormalization::Rewritten(rewritten))
}

fn parse_inline_data_url(url: &str) -> Option<(&str, &str)> {
    let (metadata, encoded) = url.strip_prefix("data:")?.split_once(',')?;
    let mut parts = metadata.split(';');
    let media_type = parts.next()?.trim();
    if !parts.any(|part| part.eq_ignore_ascii_case("base64")) {
        return None;
    }
    Some((media_type, encoded))
}

fn canonical_inline_image_media_type(media_type: &str) -> Option<&'static str> {
    match media_type.to_ascii_lowercase().as_str() {
        "image/png" => Some("image/png"),
        "image/jpeg" | "image/jpg" => Some("image/jpeg"),
        "image/gif" => Some("image/gif"),
        "image/webp" => Some("image/webp"),
        _ => None,
    }
}

fn image_media_type_hint(attachment: &StoredAttachment) -> Option<String> {
    attachment
        .media_type
        .as_deref()
        .filter(|value| value.starts_with("image/"))
        .map(ToOwned::to_owned)
        .or_else(|| infer_image_media_type(&attachment.path).map(ToOwned::to_owned))
}

fn infer_image_media_type(path: &Path) -> Option<&'static str> {
    match path
        .extension()
        .and_then(|value| value.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase()
        .as_str()
    {
        "png" => Some("image/png"),
        "jpg" | "jpeg" => Some("image/jpeg"),
        "gif" => Some("image/gif"),
        "webp" => Some("image/webp"),
        "bmp" => Some("image/bmp"),
        "tif" | "tiff" => Some("image/tiff"),
        "svg" => Some("image/svg+xml"),
        _ => None,
    }
}

fn transcode_inline_image_bytes_to_png_data_url(bytes: &[u8], label: &str) -> Result<String> {
    let output = transcode_inline_image_bytes_to_png(bytes, label)?;
    let encoded = base64::engine::general_purpose::STANDARD.encode(output);
    Ok(format!("data:image/png;base64,{encoded}"))
}

fn transcode_inline_image_bytes_to_png(bytes: &[u8], label: &str) -> Result<Vec<u8>> {
    let image = ImageReader::new(Cursor::new(bytes))
        .with_guessed_format()
        .with_context(|| format!("failed to guess image format for {label}"))?
        .decode()
        .with_context(|| format!("failed to decode image data for {label}"))?;
    let mut output = Vec::new();
    image
        .write_to(&mut Cursor::new(&mut output), ImageFormat::Png)
        .with_context(|| format!("failed to transcode image data for {label} to PNG"))?;
    Ok(output)
}

#[cfg(test)]
mod tests {
    use super::{
        build_image_data_url, normalize_inline_image_content_for_persistence,
        normalize_stored_attachment_for_persistence,
    };
    use crate::domain::{AttachmentKind, StoredAttachment};
    use base64::Engine;
    use serde_json::json;
    use tempfile::TempDir;
    use uuid::Uuid;

    fn tiny_tiff_bytes() -> Vec<u8> {
        let image = image::ImageBuffer::from_pixel(1, 1, image::Rgba([12, 34, 56, 255]));
        let mut bytes = Vec::new();
        image::DynamicImage::ImageRgba8(image)
            .write_to(
                &mut std::io::Cursor::new(&mut bytes),
                image::ImageFormat::Tiff,
            )
            .unwrap();
        bytes
    }

    #[test]
    fn build_image_data_url_transcodes_tiff_attachments_to_png() {
        let temp_dir = TempDir::new().unwrap();
        let image_path = temp_dir.path().join("photo.tiff");
        std::fs::write(&image_path, tiny_tiff_bytes()).unwrap();
        let attachment = StoredAttachment {
            id: Uuid::new_v4(),
            kind: AttachmentKind::Image,
            original_name: Some("photo.tiff".to_string()),
            media_type: Some("image/tiff".to_string()),
            path: image_path,
            size_bytes: 4,
        };

        let data_url = build_image_data_url(&attachment).unwrap();
        assert!(data_url.starts_with("data:image/png;base64,"));
    }

    #[test]
    fn normalize_stored_attachment_for_persistence_transcodes_tiff_to_png_file() {
        let temp_dir = TempDir::new().unwrap();
        let image_path = temp_dir.path().join("photo.tiff");
        std::fs::write(&image_path, tiny_tiff_bytes()).unwrap();
        let attachment = StoredAttachment {
            id: Uuid::new_v4(),
            kind: AttachmentKind::Image,
            original_name: Some("photo.tiff".to_string()),
            media_type: Some("image/tiff".to_string()),
            path: image_path.clone(),
            size_bytes: 4,
        };

        let normalized = normalize_stored_attachment_for_persistence(attachment).unwrap();

        assert_eq!(normalized.media_type.as_deref(), Some("image/png"));
        assert_eq!(
            normalized.path.extension().and_then(|value| value.to_str()),
            Some("png")
        );
        assert!(!image_path.exists());
        assert!(normalized.path.is_file());
    }

    #[test]
    fn normalize_inline_image_content_for_persistence_rewrites_tiff_and_bad_payloads() {
        let encoded = base64::engine::general_purpose::STANDARD.encode(tiny_tiff_bytes());
        let content = Some(json!([
            {
                "type": "text",
                "text": "look"
            },
            {
                "type": "image_url",
                "image_url": {
                    "url": format!("data:image/tiff;base64,{encoded}")
                }
            },
            {
                "type": "image_url",
                "image_url": {
                    "url": "data:image/tiff;base64,AAAA"
                }
            }
        ]));

        let normalized = normalize_inline_image_content_for_persistence(&content).unwrap();
        let items = normalized.unwrap().as_array().unwrap().clone();
        assert_eq!(items[0]["type"], "text");
        assert_eq!(items[1]["type"], "image_url");
        assert!(
            items[1]["image_url"]["url"]
                .as_str()
                .is_some_and(|url| url.starts_with("data:image/png;base64,"))
        );
        assert_eq!(items[2]["type"], "text");
        assert!(
            items[2]["text"]
                .as_str()
                .is_some_and(|text| text.contains("could not be converted"))
        );
    }

    #[test]
    fn normalize_inline_image_content_for_persistence_rewrites_octet_stream_images() {
        let encoded = base64::engine::general_purpose::STANDARD.encode(tiny_tiff_bytes());
        let content = Some(json!([
            {
                "type": "input_image",
                "image_url": format!("data:application/octet-stream;base64,{encoded}")
            }
        ]));

        let normalized = normalize_inline_image_content_for_persistence(&content).unwrap();
        let items = normalized.unwrap().as_array().unwrap().clone();
        assert_eq!(items[0]["type"], "input_image");
        assert!(
            items[0]["image_url"]
                .as_str()
                .is_some_and(|url| url.starts_with("data:image/png;base64,"))
        );
    }

    #[test]
    fn normalize_inline_image_content_for_persistence_replaces_non_image_octet_stream_payloads() {
        let pdf_header = base64::engine::general_purpose::STANDARD.encode(b"%PDF-1.4\n");
        let content = Some(json!([
            {
                "type": "input_image",
                "image_url": format!("data:application/octet-stream;base64,{pdf_header}")
            }
        ]));

        let normalized = normalize_inline_image_content_for_persistence(&content).unwrap();
        let items = normalized.unwrap().as_array().unwrap().clone();
        assert_eq!(items[0]["type"], "text");
        assert!(
            items[0]["text"]
                .as_str()
                .is_some_and(|text| text.contains("could not be converted"))
        );
    }
}
