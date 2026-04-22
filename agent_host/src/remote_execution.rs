use anyhow::{Result, anyhow};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum RemoteExecutionBinding {
    Local { path: PathBuf },
    Ssh { host: String, path: String },
}

impl RemoteExecutionBinding {
    pub fn describe(&self) -> String {
        match self {
            Self::Local { path } => format!("local {}", path.display()),
            Self::Ssh { host, path } => format!("{host} {path}"),
        }
    }
}

pub fn storage_root_for_execution_root(execution_root: &Path) -> PathBuf {
    execution_root.join(".cache").join("partyclaw")
}

pub fn validate_local_execution_path(raw: &str) -> Result<PathBuf> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Err(anyhow!("local remote execution path must not be empty"));
    }
    let path = PathBuf::from(trimmed);
    if !path.is_absolute() {
        return Err(anyhow!(
            "local remote execution path must be an absolute path"
        ));
    }
    Ok(path)
}

pub fn validate_ssh_execution_binding(host: &str, path: &str) -> Result<(String, String)> {
    let host = crate::workpath::validate_remote_workpath_host(host)?;
    let path = path.trim();
    if path.is_empty() {
        return Err(anyhow!("remote execution path must not be empty"));
    }
    Ok((host, path.to_string()))
}
