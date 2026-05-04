use std::{
    fmt, fs,
    io::{Read, Seek, SeekFrom},
    path::{Component, Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

use anyhow::Context;
use base64::{engine::general_purpose, Engine as _};
use flate2::{read::GzDecoder, write::GzEncoder, Compression};
use serde::Serialize;
use stellaclaw_core::session_actor::ToolRemoteMode;

use crate::{conversation::ConversationState, workspace::ensure_workspace_for_remote_mode};

#[derive(Debug)]
pub enum RemoteActorError {
    InvalidPath(String),
    Internal(anyhow::Error),
}

impl fmt::Display for RemoteActorError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidPath(message) => formatter.write_str(message),
            Self::Internal(error) => write!(formatter, "{error:#}"),
        }
    }
}

impl std::error::Error for RemoteActorError {}

impl From<anyhow::Error> for RemoteActorError {
    fn from(error: anyhow::Error) -> Self {
        Self::Internal(error)
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct WorkspaceListing {
    pub conversation_id: String,
    pub mode: WorkspaceMode,
    pub remote: Option<WorkspaceRemote>,
    pub workspace_root: String,
    pub path: String,
    pub parent: Option<String>,
    pub total_entries: usize,
    pub returned_entries: usize,
    pub truncated: bool,
    pub entries: Vec<WorkspaceEntry>,
}

#[derive(Debug, Clone, Serialize)]
pub struct WorkspaceFile {
    pub conversation_id: String,
    pub mode: WorkspaceMode,
    pub remote: Option<WorkspaceRemote>,
    pub workspace_root: String,
    pub path: String,
    pub name: String,
    pub size_bytes: u64,
    pub modified_ms: Option<u128>,
    pub offset: u64,
    pub returned_bytes: usize,
    pub truncated: bool,
    pub encoding: WorkspaceFileEncoding,
    pub data: String,
}

#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkspaceFileEncoding {
    Utf8,
    Base64,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkspaceMode {
    Local,
    FixedSsh,
}

#[derive(Debug, Clone, Serialize)]
pub struct WorkspaceRemote {
    pub host: String,
    pub cwd: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct WorkspaceEntry {
    pub name: String,
    pub path: String,
    pub kind: WorkspaceEntryKind,
    pub size_bytes: Option<u64>,
    pub modified_ms: Option<u128>,
    pub hidden: bool,
    pub readonly: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkspaceEntryKind {
    Directory,
    File,
    Symlink,
    Other,
}

pub fn list_workspace_entries(
    workdir: &Path,
    state: &ConversationState,
    relative_path: Option<&str>,
    limit: usize,
) -> std::result::Result<WorkspaceListing, RemoteActorError> {
    let normalized = normalize_workspace_path(relative_path.unwrap_or_default())?;
    let conversation_root = workdir.join("conversations").join(&state.conversation_id);
    let workspace_root = ensure_workspace_for_remote_mode(
        workdir,
        &conversation_root,
        &state.conversation_id,
        &state.tool_remote_mode,
    )?;
    let target = workspace_root.join(&normalized);
    let metadata = fs::metadata(&target).with_context(|| {
        format!(
            "failed to inspect workspace path {}",
            display_workspace_path(&normalized)
        )
    })?;
    if !metadata.is_dir() {
        return Err(RemoteActorError::InvalidPath(format!(
            "workspace path {} is not a directory",
            display_workspace_path(&normalized)
        )));
    }

    let mut entries = Vec::new();
    for entry in fs::read_dir(&target)
        .with_context(|| format!("failed to read workspace path {}", target.display()))?
    {
        let entry = entry.with_context(|| format!("failed to enumerate {}", target.display()))?;
        let name = entry.file_name().to_string_lossy().to_string();
        let relative_entry_path = normalized.join(&name);
        let metadata = fs::symlink_metadata(entry.path())
            .with_context(|| format!("failed to inspect {}", entry.path().display()))?;
        entries.push(WorkspaceEntry {
            hidden: name.starts_with('.'),
            path: path_to_api_string(&relative_entry_path),
            name,
            kind: entry_kind(&metadata),
            size_bytes: metadata.is_file().then_some(metadata.len()),
            modified_ms: metadata.modified().ok().and_then(system_time_ms),
            readonly: metadata.permissions().readonly(),
        });
    }
    entries.sort_by(|left, right| {
        left.kind
            .cmp(&right.kind)
            .then_with(|| left.name.to_lowercase().cmp(&right.name.to_lowercase()))
            .then_with(|| left.name.cmp(&right.name))
    });

    let total_entries = entries.len();
    let effective_limit = limit.max(1);
    let truncated = total_entries > effective_limit;
    entries.truncate(effective_limit);
    let returned_entries = entries.len();

    Ok(WorkspaceListing {
        conversation_id: state.conversation_id.clone(),
        mode: match state.tool_remote_mode {
            ToolRemoteMode::Selectable => WorkspaceMode::Local,
            ToolRemoteMode::FixedSsh { .. } => WorkspaceMode::FixedSsh,
        },
        remote: match &state.tool_remote_mode {
            ToolRemoteMode::Selectable => None,
            ToolRemoteMode::FixedSsh { host, cwd } => Some(WorkspaceRemote {
                host: host.clone(),
                cwd: cwd.clone(),
            }),
        },
        workspace_root: workspace_root.display().to_string(),
        parent: parent_api_path(&normalized),
        path: path_to_api_string(&normalized),
        total_entries,
        returned_entries,
        truncated,
        entries,
    })
}

pub fn read_workspace_file(
    workdir: &Path,
    state: &ConversationState,
    relative_path: &str,
    offset: u64,
    limit_bytes: Option<usize>,
) -> std::result::Result<WorkspaceFile, RemoteActorError> {
    let normalized = normalize_workspace_path(relative_path)?;
    if normalized.as_os_str().is_empty() {
        return Err(RemoteActorError::InvalidPath(
            "workspace file path must not be empty".to_string(),
        ));
    }
    let conversation_root = workdir.join("conversations").join(&state.conversation_id);
    let workspace_root = ensure_workspace_for_remote_mode(
        workdir,
        &conversation_root,
        &state.conversation_id,
        &state.tool_remote_mode,
    )?;
    let target = workspace_root.join(&normalized);
    let metadata = fs::metadata(&target).with_context(|| {
        format!(
            "failed to inspect workspace file {}",
            display_workspace_path(&normalized)
        )
    })?;
    if !metadata.is_file() {
        return Err(RemoteActorError::InvalidPath(format!(
            "workspace path {} is not a file",
            display_workspace_path(&normalized)
        )));
    }

    let file_size = metadata.len();
    let mut file = fs::File::open(&target)
        .with_context(|| format!("failed to open workspace file {}", target.display()))?;
    let start = offset.min(file_size);
    file.seek(SeekFrom::Start(start))
        .with_context(|| format!("failed to seek workspace file {}", target.display()))?;
    let mut data = Vec::new();
    let read = if let Some(limit_bytes) = limit_bytes {
        let read_limit = limit_bytes.max(1);
        data.resize(read_limit, 0);
        let read = file
            .read(&mut data)
            .with_context(|| format!("failed to read workspace file {}", target.display()))?;
        data.truncate(read);
        read
    } else {
        file.read_to_end(&mut data)
            .with_context(|| format!("failed to read workspace file {}", target.display()))?
    };
    let (encoding, data) = match String::from_utf8(data) {
        Ok(text) => (WorkspaceFileEncoding::Utf8, text),
        Err(error) => (
            WorkspaceFileEncoding::Base64,
            general_purpose::STANDARD.encode(error.into_bytes()),
        ),
    };

    Ok(WorkspaceFile {
        conversation_id: state.conversation_id.clone(),
        mode: match state.tool_remote_mode {
            ToolRemoteMode::Selectable => WorkspaceMode::Local,
            ToolRemoteMode::FixedSsh { .. } => WorkspaceMode::FixedSsh,
        },
        remote: match &state.tool_remote_mode {
            ToolRemoteMode::Selectable => None,
            ToolRemoteMode::FixedSsh { host, cwd } => Some(WorkspaceRemote {
                host: host.clone(),
                cwd: cwd.clone(),
            }),
        },
        workspace_root: workspace_root.display().to_string(),
        path: path_to_api_string(&normalized),
        name: normalized
            .file_name()
            .map(|value| value.to_string_lossy().to_string())
            .unwrap_or_default(),
        size_bytes: file_size,
        modified_ms: metadata.modified().ok().and_then(system_time_ms),
        offset: start,
        returned_bytes: read,
        truncated: start.saturating_add(read as u64) < file_size,
        encoding,
        data,
    })
}

/// Maximum compressed upload size: 10 MiB.
const MAX_UPLOAD_BYTES: usize = 10 * 1024 * 1024;
/// Maximum compressed download archive size: 50 MiB.
const MAX_DOWNLOAD_BYTES: usize = 50 * 1024 * 1024;

/// Upload a tar.gz archive and extract it into the workspace directory at `relative_dir`.
pub fn upload_workspace_archive(
    workdir: &Path,
    state: &ConversationState,
    relative_dir: &str,
    archive_data: &[u8],
) -> std::result::Result<usize, RemoteActorError> {
    if archive_data.len() > MAX_UPLOAD_BYTES {
        return Err(RemoteActorError::InvalidPath(format!(
            "upload exceeds {} byte limit (got {} bytes)",
            MAX_UPLOAD_BYTES,
            archive_data.len()
        )));
    }
    let normalized = normalize_workspace_path(relative_dir)?;
    let conversation_root = workdir.join("conversations").join(&state.conversation_id);
    let workspace_root = ensure_workspace_for_remote_mode(
        workdir,
        &conversation_root,
        &state.conversation_id,
        &state.tool_remote_mode,
    )?;
    let target_dir = workspace_root.join(&normalized);
    fs::create_dir_all(&target_dir)
        .with_context(|| format!("failed to create {}", target_dir.display()))?;

    let decoder = GzDecoder::new(archive_data);
    let mut archive = tar::Archive::new(decoder);
    archive.set_overwrite(true);
    let mut count = 0_usize;
    for entry in archive
        .entries()
        .with_context(|| "failed to read tar entries")?
    {
        let mut entry = entry.with_context(|| "failed to read tar entry")?;
        let entry_path = entry
            .path()
            .with_context(|| "failed to read entry path")?
            .into_owned();
        // Security: reject absolute paths and parent directory traversal.
        if entry_path.is_absolute() {
            continue;
        }
        let has_parent_traversal = entry_path
            .components()
            .any(|c| matches!(c, Component::ParentDir));
        if has_parent_traversal {
            continue;
        }
        entry
            .unpack_in(&target_dir)
            .with_context(|| format!("failed to unpack {}", entry_path.display()))?;
        count += 1;
    }
    Ok(count)
}

pub fn delete_workspace_path(
    workdir: &Path,
    state: &ConversationState,
    relative_path: &str,
) -> std::result::Result<(), RemoteActorError> {
    let normalized = normalize_workspace_path(relative_path)?;
    if normalized.as_os_str().is_empty() {
        return Err(RemoteActorError::InvalidPath(
            "workspace path must not be empty".to_string(),
        ));
    }
    let conversation_root = workdir.join("conversations").join(&state.conversation_id);
    let workspace_root = ensure_workspace_for_remote_mode(
        workdir,
        &conversation_root,
        &state.conversation_id,
        &state.tool_remote_mode,
    )?;
    let target = workspace_root.join(&normalized);
    let metadata = fs::symlink_metadata(&target).with_context(|| {
        format!(
            "failed to inspect workspace path {}",
            display_workspace_path(&normalized)
        )
    })?;
    if metadata.is_dir() {
        fs::remove_dir_all(&target)
            .with_context(|| format!("failed to delete directory {}", target.display()))?;
    } else {
        fs::remove_file(&target)
            .with_context(|| format!("failed to delete file {}", target.display()))?;
    }
    Ok(())
}

pub fn move_workspace_path(
    workdir: &Path,
    state: &ConversationState,
    from_path: &str,
    to_path: &str,
) -> std::result::Result<(), RemoteActorError> {
    let from = normalize_workspace_path(from_path)?;
    let to = normalize_workspace_path(to_path)?;
    if from.as_os_str().is_empty() || to.as_os_str().is_empty() {
        return Err(RemoteActorError::InvalidPath(
            "workspace source and destination paths must not be empty".to_string(),
        ));
    }
    let conversation_root = workdir.join("conversations").join(&state.conversation_id);
    let workspace_root = ensure_workspace_for_remote_mode(
        workdir,
        &conversation_root,
        &state.conversation_id,
        &state.tool_remote_mode,
    )?;
    let source = workspace_root.join(&from);
    let target = workspace_root.join(&to);
    if !source.exists() {
        return Err(RemoteActorError::InvalidPath(format!(
            "workspace path {} does not exist",
            display_workspace_path(&from)
        )));
    }
    if target.exists() {
        return Err(RemoteActorError::InvalidPath(format!(
            "workspace path {} already exists",
            display_workspace_path(&to)
        )));
    }
    if let Some(parent) = target.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }
    fs::rename(&source, &target).with_context(|| {
        format!(
            "failed to move workspace path {} to {}",
            display_workspace_path(&from),
            display_workspace_path(&to)
        )
    })?;
    Ok(())
}

/// Download one or more workspace paths as a tar.gz archive.
/// Returns the compressed bytes.
pub fn download_workspace_archive(
    workdir: &Path,
    state: &ConversationState,
    relative_paths: &[&str],
) -> std::result::Result<Vec<u8>, RemoteActorError> {
    if relative_paths.is_empty() {
        return Err(RemoteActorError::InvalidPath(
            "at least one path is required for download".to_string(),
        ));
    }
    let conversation_root = workdir.join("conversations").join(&state.conversation_id);
    let workspace_root = ensure_workspace_for_remote_mode(
        workdir,
        &conversation_root,
        &state.conversation_id,
        &state.tool_remote_mode,
    )?;

    let mut output = Vec::new();
    {
        let encoder = GzEncoder::new(&mut output, Compression::fast());
        let mut builder = tar::Builder::new(encoder);
        for relative_path in relative_paths {
            let normalized = normalize_workspace_path(relative_path)?;
            let target = workspace_root.join(&normalized);
            let metadata = fs::metadata(&target).with_context(|| {
                format!(
                    "failed to inspect workspace path {}",
                    display_workspace_path(&normalized)
                )
            })?;
            let archive_name = if normalized.as_os_str().is_empty() {
                PathBuf::from("workspace")
            } else {
                normalized.clone()
            };
            if metadata.is_dir() {
                builder
                    .append_dir_all(&archive_name, &target)
                    .with_context(|| format!("failed to archive directory {}", target.display()))?;
            } else if metadata.is_file() {
                let mut file = fs::File::open(&target)
                    .with_context(|| format!("failed to open {}", target.display()))?;
                builder
                    .append_file(&archive_name, &mut file)
                    .with_context(|| format!("failed to archive file {}", target.display()))?;
            }
        }
        builder
            .finish()
            .with_context(|| "failed to finalize tar archive")?;
    }

    if output.len() > MAX_DOWNLOAD_BYTES {
        return Err(RemoteActorError::InvalidPath(format!(
            "download archive exceeds {} byte limit (got {} bytes)",
            MAX_DOWNLOAD_BYTES,
            output.len()
        )));
    }
    Ok(output)
}

fn normalize_workspace_path(value: &str) -> std::result::Result<PathBuf, RemoteActorError> {
    let trimmed = value.trim();
    if trimmed.is_empty() || trimmed == "." {
        return Ok(PathBuf::new());
    }
    let path = Path::new(trimmed);
    if path.is_absolute() {
        return Err(RemoteActorError::InvalidPath(
            "workspace path must be relative".to_string(),
        ));
    }
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::Normal(value) => normalized.push(value),
            Component::ParentDir => {
                return Err(RemoteActorError::InvalidPath(
                    "workspace path must not contain parent components".to_string(),
                ));
            }
            Component::RootDir | Component::Prefix(_) => {
                return Err(RemoteActorError::InvalidPath(
                    "workspace path must be relative".to_string(),
                ));
            }
        }
    }
    Ok(normalized)
}

fn entry_kind(metadata: &fs::Metadata) -> WorkspaceEntryKind {
    let file_type = metadata.file_type();
    if file_type.is_symlink() {
        WorkspaceEntryKind::Symlink
    } else if file_type.is_dir() {
        WorkspaceEntryKind::Directory
    } else if file_type.is_file() {
        WorkspaceEntryKind::File
    } else {
        WorkspaceEntryKind::Other
    }
}

fn parent_api_path(path: &Path) -> Option<String> {
    let parent = path.parent()?;
    Some(path_to_api_string(parent))
}

fn path_to_api_string(path: &Path) -> String {
    path.components()
        .filter_map(|component| match component {
            Component::Normal(value) => Some(value.to_string_lossy().to_string()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("/")
}

fn display_workspace_path(path: &Path) -> String {
    let value = path_to_api_string(path);
    if value.is_empty() {
        ".".to_string()
    } else {
        value
    }
}

fn system_time_ms(value: SystemTime) -> Option<u128> {
    value
        .duration_since(UNIX_EPOCH)
        .ok()
        .map(|duration| duration.as_millis())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalizes_relative_workspace_paths() {
        assert_eq!(
            normalize_workspace_path("./src/../bad")
                .unwrap_err()
                .to_string(),
            "workspace path must not contain parent components"
        );
        assert_eq!(
            path_to_api_string(&normalize_workspace_path("./src/bin").unwrap()),
            "src/bin"
        );
        assert_eq!(
            path_to_api_string(&normalize_workspace_path("").unwrap()),
            ""
        );
    }

    #[test]
    fn parent_path_uses_api_separators() {
        let path = normalize_workspace_path("src/bin").unwrap();
        assert_eq!(parent_api_path(&path).as_deref(), Some("src"));
        let root = normalize_workspace_path("").unwrap();
        assert_eq!(parent_api_path(&root), None);
    }
}
