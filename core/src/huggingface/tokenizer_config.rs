use std::{fs, path::PathBuf};

use serde::Deserialize;
use thiserror::Error;
use url::Url;

use super::{
    HuggingFaceFileResolver, HuggingFaceRemoteFile, HuggingFaceRepo, HuggingFaceRepoType,
    ModelFileSource, ResolveModelFileError,
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedTokenizerAssets {
    pub tokenizer_source: ModelFileSource,
    pub chat_template_source: Option<ModelFileSource>,
    pub chat_template: Option<String>,
    pub bos_token: Option<String>,
    pub eos_token: Option<String>,
}

pub fn resolve_tokenizer_assets(
    token_estimator_url: &str,
    file_resolver: &HuggingFaceFileResolver,
) -> Result<ResolvedTokenizerAssets, ResolveTokenizerAssetsError> {
    let tokenizer_config_source = parse_model_file_source(token_estimator_url)?;
    let tokenizer_config_path = file_resolver.resolve(&tokenizer_config_source)?;
    let tokenizer_config = serde_json::from_str::<TokenizerConfigFile>(
        &fs::read_to_string(&tokenizer_config_path).map_err(|source| {
            ResolveTokenizerAssetsError::ReadFile {
                path: tokenizer_config_path.clone(),
                source,
            }
        })?,
    )
    .map_err(ResolveTokenizerAssetsError::ParseJson)?;

    let tokenizer_source =
        sibling_source(&tokenizer_config_source, "tokenizer.json").ok_or_else(|| {
            ResolveTokenizerAssetsError::UnsupportedUrl(token_estimator_url.to_string())
        })?;
    let chat_template_source = sibling_source(&tokenizer_config_source, "chat_template.jinja");
    let chat_template = match &chat_template_source {
        Some(source) => match file_resolver.resolve(source) {
            Ok(path) => Some(
                fs::read_to_string(&path)
                    .map_err(|source| ResolveTokenizerAssetsError::ReadFile { path, source })?,
            ),
            Err(_) => tokenizer_config.chat_template.clone(),
        },
        None => tokenizer_config.chat_template.clone(),
    };

    Ok(ResolvedTokenizerAssets {
        tokenizer_source,
        chat_template_source,
        chat_template,
        bos_token: tokenizer_config
            .bos_token
            .map(SpecialTokenValue::into_content),
        eos_token: tokenizer_config
            .eos_token
            .map(SpecialTokenValue::into_content),
    })
}

fn parse_model_file_source(input: &str) -> Result<ModelFileSource, ResolveTokenizerAssetsError> {
    if let Ok(url) = Url::parse(input) {
        if url.scheme() == "https" && url.domain() == Some("huggingface.co") {
            return parse_huggingface_url(&url);
        }

        return Err(ResolveTokenizerAssetsError::UnsupportedUrl(
            input.to_string(),
        ));
    }

    let path = PathBuf::from(input);
    if path.is_dir() {
        Ok(ModelFileSource::LocalDirectory {
            directory: path,
            filename: "tokenizer_config.json".to_string(),
        })
    } else {
        Ok(ModelFileSource::LocalFile(path))
    }
}

fn parse_huggingface_url(url: &Url) -> Result<ModelFileSource, ResolveTokenizerAssetsError> {
    let segments = url
        .path_segments()
        .ok_or_else(|| ResolveTokenizerAssetsError::UnsupportedUrl(url.to_string()))?
        .collect::<Vec<_>>();

    let raw_index = segments
        .iter()
        .position(|segment| *segment == "raw" || *segment == "resolve")
        .ok_or_else(|| ResolveTokenizerAssetsError::UnsupportedHuggingFaceUrl(url.to_string()))?;

    if raw_index < 1 || raw_index + 2 >= segments.len() {
        return Err(ResolveTokenizerAssetsError::UnsupportedHuggingFaceUrl(
            url.to_string(),
        ));
    }

    let repo_id = segments[..raw_index].join("/");
    let revision = segments[raw_index + 1].to_string();
    let filename = segments[raw_index + 2..].join("/");

    Ok(ModelFileSource::HuggingFace(HuggingFaceRemoteFile {
        repo: HuggingFaceRepo {
            id: repo_id,
            repo_type: HuggingFaceRepoType::Model,
            revision: Some(revision),
        },
        filename,
    }))
}

fn sibling_source(source: &ModelFileSource, sibling_filename: &str) -> Option<ModelFileSource> {
    match source {
        ModelFileSource::LocalFile(path) => {
            path.parent()
                .map(|directory| ModelFileSource::LocalDirectory {
                    directory: directory.to_path_buf(),
                    filename: sibling_filename.to_string(),
                })
        }
        ModelFileSource::LocalDirectory { directory, .. } => {
            Some(ModelFileSource::LocalDirectory {
                directory: directory.clone(),
                filename: sibling_filename.to_string(),
            })
        }
        ModelFileSource::HuggingFace(remote) => {
            Some(ModelFileSource::HuggingFace(HuggingFaceRemoteFile {
                repo: remote.repo.clone(),
                filename: sibling_filename.to_string(),
            }))
        }
    }
}

#[derive(Debug, Deserialize)]
struct TokenizerConfigFile {
    #[serde(default)]
    chat_template: Option<String>,
    #[serde(default)]
    bos_token: Option<SpecialTokenValue>,
    #[serde(default)]
    eos_token: Option<SpecialTokenValue>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
enum SpecialTokenValue {
    Plain(String),
    Detailed { content: String },
}

impl SpecialTokenValue {
    fn into_content(self) -> String {
        match self {
            Self::Plain(value) => value,
            Self::Detailed { content } => content,
        }
    }
}

#[derive(Debug, Error)]
pub enum ResolveTokenizerAssetsError {
    #[error("failed to resolve tokenizer asset source: {0}")]
    ResolveModelFile(#[from] ResolveModelFileError),
    #[error("unsupported tokenizer config url: {0}")]
    UnsupportedUrl(String),
    #[error("unsupported hugging face tokenizer config url: {0}")]
    UnsupportedHuggingFaceUrl(String),
    #[error("failed to read tokenizer asset file {path}: {source}")]
    ReadFile {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("failed to parse tokenizer config json: {0}")]
    ParseJson(serde_json::Error),
}
