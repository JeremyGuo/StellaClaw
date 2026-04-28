use std::{
    collections::hash_map::DefaultHasher,
    fs,
    hash::{Hash, Hasher},
    io::Cursor,
    path::{Path, PathBuf},
};

use base64::{engine::general_purpose, Engine as _};
use image::{codecs::jpeg::JpegEncoder, imageops::FilterType, GenericImageView, Rgb, RgbImage};
use serde::{Deserialize, Serialize};

const RUNDIR: &str = "rundir";
const CACHE_DIR: &str = "cache";
const CONVERSATIONS_DIR: &str = "conversations";
const THUMBNAILS_DIR: &str = "thumbnails";
const THUMBNAIL_MAX_SOURCE_BYTES: u64 = 16 * 1024 * 1024;
const THUMBNAIL_MAX_DIMENSION: u32 = 360;
const THUMBNAIL_JPEG_QUALITY: u8 = 72;
const THUMBNAIL_VERSION: &str = "web-thumbnail-v1";

#[derive(Debug, Clone)]
pub struct CacheManager {
    workdir: PathBuf,
}

#[derive(Debug, Clone, Serialize)]
pub struct CachedThumbnail {
    pub media_type: String,
    pub data_base64: String,
    pub data_url: String,
    pub width: u32,
    pub height: u32,
    pub size_bytes: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ThumbnailMetadata {
    media_type: String,
    width: u32,
    height: u32,
    original_width: u32,
    original_height: u32,
}

#[derive(Debug, Clone)]
pub struct CachedImagePreview {
    pub original_width: u32,
    pub original_height: u32,
    pub thumbnail: CachedThumbnail,
}

impl CacheManager {
    pub fn new(workdir: impl Into<PathBuf>) -> Self {
        Self {
            workdir: workdir.into(),
        }
    }

    pub fn ensure_layout(&self) -> std::io::Result<()> {
        fs::create_dir_all(
            self.workdir
                .join(RUNDIR)
                .join(CACHE_DIR)
                .join(CONVERSATIONS_DIR),
        )
    }

    pub fn image_thumbnail(
        &self,
        conversation_id: &str,
        source_path: &Path,
    ) -> Option<CachedImagePreview> {
        let key = thumbnail_cache_key(source_path);
        let cache_dir = self.thumbnail_dir(conversation_id);
        let image_path = cache_dir.join(format!("{key}.jpg"));
        let metadata_path = cache_dir.join(format!("{key}.json"));
        if let Some(preview) = read_cached_thumbnail(&image_path, &metadata_path) {
            return Some(preview);
        }

        let source_metadata = fs::metadata(source_path).ok()?;
        let source_size = source_metadata.len();
        if source_size > THUMBNAIL_MAX_SOURCE_BYTES {
            return None;
        }

        let image = image::ImageReader::open(source_path).ok()?.decode().ok()?;
        let (original_width, original_height) = image.dimensions();
        let resized = image.resize(
            THUMBNAIL_MAX_DIMENSION,
            THUMBNAIL_MAX_DIMENSION,
            FilterType::Triangle,
        );
        let rgba = resized.to_rgba8();
        let mut rgb = RgbImage::new(rgba.width(), rgba.height());
        for (x, y, pixel) in rgba.enumerate_pixels() {
            let alpha = pixel[3] as u16;
            let inv_alpha = 255_u16.saturating_sub(alpha);
            let blend = |channel: u8| -> u8 {
                (((channel as u16 * alpha) + (255_u16 * inv_alpha)) / 255) as u8
            };
            rgb.put_pixel(
                x,
                y,
                Rgb([blend(pixel[0]), blend(pixel[1]), blend(pixel[2])]),
            );
        }

        let mut encoded = Cursor::new(Vec::new());
        JpegEncoder::new_with_quality(&mut encoded, THUMBNAIL_JPEG_QUALITY)
            .encode_image(&rgb)
            .ok()?;
        let bytes = encoded.into_inner();
        let metadata = ThumbnailMetadata {
            media_type: "image/jpeg".to_string(),
            width: rgb.width(),
            height: rgb.height(),
            original_width,
            original_height,
        };

        let _ = fs::create_dir_all(&cache_dir);
        let _ = fs::write(&image_path, &bytes);
        if let Ok(raw) = serde_json::to_vec(&metadata) {
            let _ = fs::write(&metadata_path, raw);
        }

        Some(cached_preview_from_parts(metadata, bytes))
    }

    fn thumbnail_dir(&self, conversation_id: &str) -> PathBuf {
        self.workdir
            .join(RUNDIR)
            .join(CACHE_DIR)
            .join(CONVERSATIONS_DIR)
            .join(sanitize_cache_component(conversation_id))
            .join(THUMBNAILS_DIR)
    }
}

fn read_cached_thumbnail(image_path: &Path, metadata_path: &Path) -> Option<CachedImagePreview> {
    let metadata = fs::read(metadata_path)
        .ok()
        .and_then(|raw| serde_json::from_slice::<ThumbnailMetadata>(&raw).ok())?;
    let bytes = fs::read(image_path).ok()?;
    Some(cached_preview_from_parts(metadata, bytes))
}

fn cached_preview_from_parts(metadata: ThumbnailMetadata, bytes: Vec<u8>) -> CachedImagePreview {
    let data_base64 = general_purpose::STANDARD.encode(&bytes);
    CachedImagePreview {
        original_width: metadata.original_width,
        original_height: metadata.original_height,
        thumbnail: CachedThumbnail {
            data_url: format!("data:{};base64,{data_base64}", metadata.media_type),
            data_base64,
            media_type: metadata.media_type,
            width: metadata.width,
            height: metadata.height,
            size_bytes: bytes.len(),
        },
    }
}

fn thumbnail_cache_key(path: &Path) -> String {
    let mut hasher = DefaultHasher::new();
    THUMBNAIL_VERSION.hash(&mut hasher);
    path.to_string_lossy().hash(&mut hasher);
    format!("{:016x}", hasher.finish())
}

fn sanitize_cache_component(value: &str) -> String {
    value
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '.') {
                ch
            } else {
                '_'
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn image_thumbnail_uses_conversation_cache() {
        let root =
            std::env::temp_dir().join(format!("stellaclaw-cache-manager-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let source = root.join("source.png");
        fs::create_dir_all(&root).expect("cache temp dir should exist");
        let image = image::RgbImage::from_pixel(800, 600, image::Rgb([80, 120, 200]));
        image.save(&source).expect("test image should be written");

        let manager = CacheManager::new(&root);
        let first = manager
            .image_thumbnail("web-main-000001", &source)
            .expect("thumbnail should be generated");
        let second = manager
            .image_thumbnail("web-main-000001", &source)
            .expect("thumbnail should be loaded from cache");
        fs::remove_file(&source).expect("source image should be removable after cache fill");
        let cached_without_source = manager
            .image_thumbnail("web-main-000001", &source)
            .expect("cache hit should not require source image");

        assert_eq!(first.original_width, 800);
        assert_eq!(first.original_height, 600);
        assert_eq!(first.thumbnail.width, 360);
        assert_eq!(first.thumbnail.height, 270);
        assert_eq!(first.thumbnail.data_base64, second.thumbnail.data_base64);
        assert_eq!(
            first.thumbnail.data_base64,
            cached_without_source.thumbnail.data_base64
        );
        assert!(root
            .join("rundir")
            .join("cache")
            .join("conversations")
            .join("web-main-000001")
            .join("thumbnails")
            .exists());

        let _ = fs::remove_dir_all(root);
    }
}
