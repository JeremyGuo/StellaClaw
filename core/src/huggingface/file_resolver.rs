use std::path::{Path, PathBuf};

use hf_hub::{
    api::sync::{Api, ApiBuilder},
    Repo, RepoType,
};
use thiserror::Error;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ModelFileSource {
    LocalFile(PathBuf),
    LocalDirectory {
        directory: PathBuf,
        filename: String,
    },
    HuggingFace(HuggingFaceRemoteFile),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HuggingFaceRemoteFile {
    pub repo: HuggingFaceRepo,
    pub filename: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HuggingFaceRepo {
    pub id: String,
    pub repo_type: HuggingFaceRepoType,
    pub revision: Option<String>,
}

impl HuggingFaceRepo {
    fn to_hf_repo(&self) -> Repo {
        match &self.revision {
            Some(revision) => Repo::with_revision(
                self.id.clone(),
                self.repo_type.to_hf_repo_type(),
                revision.clone(),
            ),
            None => Repo::new(self.id.clone(), self.repo_type.to_hf_repo_type()),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HuggingFaceRepoType {
    Model,
    Dataset,
    Space,
}

impl HuggingFaceRepoType {
    fn to_hf_repo_type(self) -> RepoType {
        match self {
            Self::Model => RepoType::Model,
            Self::Dataset => RepoType::Dataset,
            Self::Space => RepoType::Space,
        }
    }
}

#[derive(Debug, Clone)]
pub struct HuggingFaceFileResolver {
    api: Api,
    cache_dir: PathBuf,
}

impl HuggingFaceFileResolver {
    pub fn new() -> Result<Self, ResolveModelFileError> {
        let cache_dir = default_local_cache_dir()?;
        Self::with_cache_dir(cache_dir)
    }

    pub fn with_cache_dir(cache_dir: impl Into<PathBuf>) -> Result<Self, ResolveModelFileError> {
        let cache_dir = cache_dir.into();
        std::fs::create_dir_all(&cache_dir).map_err(|source| {
            ResolveModelFileError::CreateCacheDirectory {
                path: cache_dir.clone(),
                source,
            }
        })?;

        let api = ApiBuilder::new()
            .with_cache_dir(cache_dir.clone())
            .with_progress(false)
            .build()
            .map_err(ResolveModelFileError::ApiInitialization)?;

        Ok(Self { api, cache_dir })
    }

    pub fn cache_dir(&self) -> &Path {
        &self.cache_dir
    }

    pub fn resolve(&self, source: &ModelFileSource) -> Result<PathBuf, ResolveModelFileError> {
        match source {
            ModelFileSource::LocalFile(path) => ensure_file_exists(path),
            ModelFileSource::LocalDirectory {
                directory,
                filename,
            } => ensure_file_exists(&directory.join(filename)),
            ModelFileSource::HuggingFace(remote) => {
                let repo = self.api.repo(remote.repo.to_hf_repo());
                repo.get(&remote.filename)
                    .map_err(ResolveModelFileError::RemoteFile)
            }
        }
    }
}

fn default_local_cache_dir() -> Result<PathBuf, ResolveModelFileError> {
    let base = match std::env::var_os("STELLACLAW_DATA_ROOT") {
        Some(value) => PathBuf::from(value),
        None => std::env::current_dir().map_err(ResolveModelFileError::CurrentDirectory)?,
    };
    Ok(base
        .join(".stellaclaw")
        .join("cache")
        .join("huggingface")
        .join("hub"))
}

fn ensure_file_exists(path: &Path) -> Result<PathBuf, ResolveModelFileError> {
    if path.is_file() {
        Ok(path.to_path_buf())
    } else {
        Err(ResolveModelFileError::LocalFileNotFound(path.to_path_buf()))
    }
}

#[derive(Debug, Error)]
pub enum ResolveModelFileError {
    #[error("failed to resolve current working directory: {0}")]
    CurrentDirectory(std::io::Error),
    #[error("failed to create hugging face cache directory {path}: {source}")]
    CreateCacheDirectory {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("failed to initialize hugging face api: {0}")]
    ApiInitialization(hf_hub::api::sync::ApiError),
    #[error("local file not found: {0}")]
    LocalFileNotFound(PathBuf),
    #[error("failed to resolve remote file from hugging face: {0}")]
    RemoteFile(hf_hub::api::sync::ApiError),
}
