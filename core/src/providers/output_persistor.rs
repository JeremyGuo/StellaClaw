use std::{fs, path::PathBuf};

use base64::{engine::general_purpose::STANDARD, Engine};
use thiserror::Error;
use time::{format_description::BorrowedFormatItem, macros::format_description, OffsetDateTime};

use crate::session_actor::FileItem;

const DATE_FORMAT: &[BorrowedFormatItem<'static>] = format_description!("[year]-[month]-[day]");
const TIME_FORMAT: &[BorrowedFormatItem<'static>] =
    format_description!("[hour][minute][second]-[subsecond digits:3]");

#[derive(Debug, Default)]
pub struct OutputPersistor;

impl OutputPersistor {
    pub fn persist_image_data_url(
        &self,
        image_data_url: &str,
    ) -> Result<FileItem, OutputPersistorError> {
        let (media_type, payload) = parse_data_url(image_data_url)?;
        let bytes = STANDARD
            .decode(payload)
            .map_err(OutputPersistorError::DecodeBase64)?;
        let output_path = build_output_file_path(media_type)?;

        if let Some(parent) = output_path.parent() {
            fs::create_dir_all(parent).map_err(|source| OutputPersistorError::CreateDirectory {
                path: parent.to_path_buf(),
                source,
            })?;
        }

        fs::write(&output_path, bytes).map_err(|source| OutputPersistorError::WriteFile {
            path: output_path.clone(),
            source,
        })?;

        Ok(FileItem {
            uri: format!(
                "file://{}",
                output_path
                    .canonicalize()
                    .unwrap_or(output_path.clone())
                    .display()
            ),
            name: output_path
                .file_name()
                .map(|name| name.to_string_lossy().to_string()),
            media_type: Some(media_type.to_string()),
            width: None,
            height: None,
            state: None,
        })
    }
}

fn build_output_file_path(media_type: &str) -> Result<PathBuf, OutputPersistorError> {
    let cwd = std::env::current_dir().map_err(OutputPersistorError::CurrentDirectory)?;
    let now = OffsetDateTime::now_local().unwrap_or_else(|_| OffsetDateTime::now_utc());
    let date = now
        .format(DATE_FORMAT)
        .map_err(OutputPersistorError::FormatTimestamp)?;
    let time = now
        .format(TIME_FORMAT)
        .map_err(OutputPersistorError::FormatTimestamp)?;
    let extension = media_type_to_extension(media_type);

    Ok(cwd
        .join(".output")
        .join(date)
        .join(format!("output.{time}.{extension}")))
}

fn parse_data_url(input: &str) -> Result<(&str, &str), OutputPersistorError> {
    let Some(rest) = input.strip_prefix("data:") else {
        return Err(OutputPersistorError::UnsupportedImageUrl(input.to_string()));
    };
    let Some((metadata, payload)) = rest.split_once(',') else {
        return Err(OutputPersistorError::MalformedDataUrl);
    };
    let Some((media_type, encoding)) = metadata.split_once(';') else {
        return Err(OutputPersistorError::MalformedDataUrl);
    };
    if encoding != "base64" {
        return Err(OutputPersistorError::UnsupportedEncoding(
            encoding.to_string(),
        ));
    }

    Ok((media_type, payload))
}

fn media_type_to_extension(media_type: &str) -> &'static str {
    match media_type {
        "image/png" => "png",
        "image/jpeg" => "jpg",
        "image/webp" => "webp",
        "image/gif" => "gif",
        _ => "bin",
    }
}

#[derive(Debug, Error)]
pub enum OutputPersistorError {
    #[error("failed to determine current working directory: {0}")]
    CurrentDirectory(std::io::Error),
    #[error("failed to format output timestamp: {0}")]
    FormatTimestamp(time::error::Format),
    #[error("unsupported image url for persistence: {0}")]
    UnsupportedImageUrl(String),
    #[error("malformed data url")]
    MalformedDataUrl,
    #[error("unsupported image data encoding: {0}")]
    UnsupportedEncoding(String),
    #[error("failed to decode base64 image payload: {0}")]
    DecodeBase64(base64::DecodeError),
    #[error("failed to create output directory {path}: {source}")]
    CreateDirectory {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("failed to write output file {path}: {source}")]
    WriteFile {
        path: PathBuf,
        source: std::io::Error,
    },
}
