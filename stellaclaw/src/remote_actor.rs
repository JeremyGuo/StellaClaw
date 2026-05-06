use std::{
    fmt, fs,
    io::{Read, Seek, SeekFrom, Write},
    path::{Component, Path, PathBuf},
    process::{Command, Stdio},
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use anyhow::Context;
use base64::{engine::general_purpose, Engine as _};
use flate2::{read::GzDecoder, write::GzEncoder, Compression};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use stellaclaw_core::session_actor::ToolRemoteMode;

use crate::conversation::ConversationState;

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

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
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

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkspaceEntryKind {
    Directory,
    File,
    Symlink,
    Other,
}

#[derive(Debug, Deserialize)]
struct RemoteListPayload {
    entries: Vec<RemoteEntryPayload>,
}

#[derive(Debug, Deserialize)]
struct RemoteEntryPayload {
    name: String,
    kind: WorkspaceEntryKind,
    size_bytes: Option<u64>,
    modified_ms: Option<u128>,
    hidden: bool,
    readonly: bool,
}

#[derive(Debug, Deserialize)]
struct RemoteReadPayload {
    name: String,
    size_bytes: u64,
    modified_ms: Option<u128>,
    offset: u64,
    returned_bytes: usize,
    truncated: bool,
    encoding: WorkspaceFileEncoding,
    data: String,
}

#[derive(Debug, Deserialize)]
struct RemoteCountPayload {
    count: usize,
}

pub fn list_workspace_entries(
    workdir: &Path,
    state: &ConversationState,
    relative_path: Option<&str>,
    limit: usize,
) -> std::result::Result<WorkspaceListing, RemoteActorError> {
    let normalized = normalize_workspace_path(relative_path.unwrap_or_default())?;
    if let ToolRemoteMode::FixedSsh { host, cwd } = &state.tool_remote_mode {
        return list_remote_workspace_entries(host, cwd.as_deref(), state, &normalized, limit);
    }
    let conversation_root = workdir.join("conversations").join(&state.conversation_id);
    let workspace_root = conversation_root;
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
    if let ToolRemoteMode::FixedSsh { host, cwd } = &state.tool_remote_mode {
        return read_remote_workspace_file(
            host,
            cwd.as_deref(),
            state,
            &normalized,
            offset,
            limit_bytes,
        );
    }
    let conversation_root = workdir.join("conversations").join(&state.conversation_id);
    let workspace_root = conversation_root;
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
const REMOTE_WORKSPACE_TIMEOUT: Duration = Duration::from_secs(60);

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
    if let ToolRemoteMode::FixedSsh { host, cwd } = &state.tool_remote_mode {
        return upload_remote_workspace_archive(host, cwd.as_deref(), &normalized, archive_data);
    }
    let conversation_root = workdir.join("conversations").join(&state.conversation_id);
    let workspace_root = conversation_root;
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
    if let ToolRemoteMode::FixedSsh { host, cwd } = &state.tool_remote_mode {
        return delete_remote_workspace_path(host, cwd.as_deref(), &normalized);
    }
    let conversation_root = workdir.join("conversations").join(&state.conversation_id);
    let workspace_root = conversation_root;
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
    if let ToolRemoteMode::FixedSsh { host, cwd } = &state.tool_remote_mode {
        return move_remote_workspace_path(host, cwd.as_deref(), &from, &to);
    }
    let conversation_root = workdir.join("conversations").join(&state.conversation_id);
    let workspace_root = conversation_root;
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
    if let ToolRemoteMode::FixedSsh { host, cwd } = &state.tool_remote_mode {
        let normalized = relative_paths
            .iter()
            .map(|path| normalize_workspace_path(path))
            .collect::<std::result::Result<Vec<_>, _>>()?;
        return download_remote_workspace_archive(host, cwd.as_deref(), &normalized);
    }
    let conversation_root = workdir.join("conversations").join(&state.conversation_id);
    let workspace_root = conversation_root;

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

fn list_remote_workspace_entries(
    host: &str,
    cwd: Option<&str>,
    state: &ConversationState,
    normalized: &Path,
    limit: usize,
) -> std::result::Result<WorkspaceListing, RemoteActorError> {
    let payload: RemoteListPayload = run_remote_json(
        host,
        cwd,
        remote_json_script("list", json!({"path": path_to_api_string(normalized)})),
        None,
    )?;
    let mut entries = payload
        .entries
        .into_iter()
        .map(|entry| {
            let relative_entry_path = normalized.join(&entry.name);
            WorkspaceEntry {
                name: entry.name,
                path: path_to_api_string(&relative_entry_path),
                kind: entry.kind,
                size_bytes: entry.size_bytes,
                modified_ms: entry.modified_ms,
                hidden: entry.hidden,
                readonly: entry.readonly,
            }
        })
        .collect::<Vec<_>>();
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
        mode: WorkspaceMode::FixedSsh,
        remote: fixed_remote(host, cwd),
        workspace_root: cwd.unwrap_or(".").to_string(),
        parent: parent_api_path(normalized),
        path: path_to_api_string(normalized),
        total_entries,
        returned_entries,
        truncated,
        entries,
    })
}

fn read_remote_workspace_file(
    host: &str,
    cwd: Option<&str>,
    state: &ConversationState,
    normalized: &Path,
    offset: u64,
    limit_bytes: Option<usize>,
) -> std::result::Result<WorkspaceFile, RemoteActorError> {
    let payload: RemoteReadPayload = run_remote_json(
        host,
        cwd,
        remote_json_script(
            "read",
            json!({
                "path": path_to_api_string(normalized),
                "offset": offset,
                "limit_bytes": limit_bytes,
            }),
        ),
        None,
    )?;
    Ok(WorkspaceFile {
        conversation_id: state.conversation_id.clone(),
        mode: WorkspaceMode::FixedSsh,
        remote: fixed_remote(host, cwd),
        workspace_root: cwd.unwrap_or(".").to_string(),
        path: path_to_api_string(normalized),
        name: payload.name,
        size_bytes: payload.size_bytes,
        modified_ms: payload.modified_ms,
        offset: payload.offset,
        returned_bytes: payload.returned_bytes,
        truncated: payload.truncated,
        encoding: payload.encoding,
        data: payload.data,
    })
}

fn upload_remote_workspace_archive(
    host: &str,
    cwd: Option<&str>,
    normalized: &Path,
    archive_data: &[u8],
) -> std::result::Result<usize, RemoteActorError> {
    let payload: RemoteCountPayload = run_remote_json(
        host,
        cwd,
        remote_json_script("upload", json!({"path": path_to_api_string(normalized)})),
        Some(archive_data),
    )?;
    Ok(payload.count)
}

fn delete_remote_workspace_path(
    host: &str,
    cwd: Option<&str>,
    normalized: &Path,
) -> std::result::Result<(), RemoteActorError> {
    let _: Value = run_remote_json(
        host,
        cwd,
        remote_json_script("delete", json!({"path": path_to_api_string(normalized)})),
        None,
    )?;
    Ok(())
}

fn move_remote_workspace_path(
    host: &str,
    cwd: Option<&str>,
    from: &Path,
    to: &Path,
) -> std::result::Result<(), RemoteActorError> {
    let _: Value = run_remote_json(
        host,
        cwd,
        remote_json_script(
            "move",
            json!({
                "from": path_to_api_string(from),
                "to": path_to_api_string(to),
            }),
        ),
        None,
    )?;
    Ok(())
}

fn download_remote_workspace_archive(
    host: &str,
    cwd: Option<&str>,
    normalized_paths: &[PathBuf],
) -> std::result::Result<Vec<u8>, RemoteActorError> {
    let payload = json!({
        "operation": "download",
        "paths": normalized_paths.iter().map(|path| path_to_api_string(path)).collect::<Vec<_>>(),
    });
    let output = run_remote_command(
        host,
        cwd,
        &format!(
            "python3 - <<'PY'\n{}\nPY\n",
            remote_workspace_script(&payload)
        ),
        None,
    )?;
    if output.len() > MAX_DOWNLOAD_BYTES {
        return Err(RemoteActorError::InvalidPath(format!(
            "download archive exceeds {} byte limit (got {} bytes)",
            MAX_DOWNLOAD_BYTES,
            output.len()
        )));
    }
    Ok(output)
}

fn fixed_remote(host: &str, cwd: Option<&str>) -> Option<WorkspaceRemote> {
    Some(WorkspaceRemote {
        host: host.to_string(),
        cwd: cwd.map(str::to_string),
    })
}

fn run_remote_json<T: for<'de> Deserialize<'de>>(
    host: &str,
    cwd: Option<&str>,
    script: String,
    stdin: Option<&[u8]>,
) -> std::result::Result<T, RemoteActorError> {
    let stdout = run_remote_command(host, cwd, &script, stdin)?;
    serde_json::from_slice(&stdout).map_err(|error| {
        RemoteActorError::Internal(anyhow::anyhow!(
            "remote output was not JSON: {error}; stdout: {}",
            String::from_utf8_lossy(&stdout)
        ))
    })
}

fn run_remote_command(
    host: &str,
    cwd: Option<&str>,
    command: &str,
    stdin: Option<&[u8]>,
) -> std::result::Result<Vec<u8>, RemoteActorError> {
    let remote_command = match cwd.map(str::trim).filter(|value| !value.is_empty()) {
        Some(cwd) => format!("cd {} && {}", shell_quote(cwd), command),
        None => command.to_string(),
    };
    let mut child = Command::new("ssh")
        .arg("-o")
        .arg("BatchMode=yes")
        .arg("-o")
        .arg("ConnectTimeout=10")
        .arg("-T")
        .arg(host)
        .arg(remote_command)
        .stdin(if stdin.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .with_context(|| format!("failed to spawn ssh for {host}"))?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| anyhow::anyhow!("failed to open ssh stdout"))?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| anyhow::anyhow!("failed to open ssh stderr"))?;
    let stdout_handle = thread::spawn(move || read_pipe(stdout));
    let stderr_handle = thread::spawn(move || read_pipe(stderr));

    if let Some(stdin_bytes) = stdin {
        let write_result = match child.stdin.as_mut() {
            Some(child_stdin) => child_stdin
                .write_all(stdin_bytes)
                .with_context(|| "failed to write ssh stdin"),
            None => Err(anyhow::anyhow!("failed to open ssh stdin")),
        };
        if let Err(error) = write_result {
            let _ = child.kill();
            let _ = child.wait();
            let _ = join_pipe_reader(stdout_handle, "stdout");
            let _ = join_pipe_reader(stderr_handle, "stderr");
            return Err(RemoteActorError::Internal(error));
        }
    }
    let _ = child.stdin.take();

    let deadline = Instant::now() + REMOTE_WORKSPACE_TIMEOUT;
    let status = loop {
        match child
            .try_wait()
            .with_context(|| format!("failed to wait for ssh to {host}"))?
        {
            Some(status) => break status,
            None if Instant::now() >= deadline => {
                let _ = child.kill();
                let _ = child.wait();
                let stderr = join_pipe_reader(stderr_handle, "stderr").unwrap_or_default();
                let _ = join_pipe_reader(stdout_handle, "stdout");
                let stderr = String::from_utf8_lossy(&stderr);
                let stderr = stderr.trim();
                let suffix = if stderr.is_empty() {
                    String::new()
                } else {
                    format!("; stderr: {stderr}")
                };
                return Err(RemoteActorError::Internal(anyhow::anyhow!(
                    "ssh to {host} timed out after {} seconds{suffix}",
                    REMOTE_WORKSPACE_TIMEOUT.as_secs()
                )));
            }
            None => thread::sleep(Duration::from_millis(100)),
        }
    };
    let stdout = join_pipe_reader(stdout_handle, "stdout")?;
    let stderr = join_pipe_reader(stderr_handle, "stderr")?;
    let output = std::process::Output {
        status,
        stdout,
        stderr,
    };
    if !output.status.success() {
        return Err(RemoteActorError::Internal(anyhow::anyhow!(
            "ssh exited with {}; stderr: {}",
            output.status.code().unwrap_or(-1),
            String::from_utf8_lossy(&output.stderr)
        )));
    }
    Ok(output.stdout)
}

fn read_pipe<R: Read>(mut pipe: R) -> std::io::Result<Vec<u8>> {
    let mut buffer = Vec::new();
    pipe.read_to_end(&mut buffer)?;
    Ok(buffer)
}

fn join_pipe_reader(
    handle: thread::JoinHandle<std::io::Result<Vec<u8>>>,
    stream_name: &str,
) -> std::result::Result<Vec<u8>, RemoteActorError> {
    handle
        .join()
        .map_err(|_| RemoteActorError::Internal(anyhow::anyhow!("{stream_name} reader panicked")))?
        .map_err(|error| {
            RemoteActorError::Internal(anyhow::anyhow!("failed to read {stream_name}: {error}"))
        })
}

fn remote_json_script(operation: &str, mut payload: Value) -> String {
    if let Some(object) = payload.as_object_mut() {
        object.insert(
            "operation".to_string(),
            Value::String(operation.to_string()),
        );
    }
    format!(
        "python3 - <<'PY'\n{}\nPY\n",
        remote_workspace_script(&payload)
    )
}

fn remote_workspace_script(payload: &Value) -> String {
    let payload = serde_json::to_string(payload).unwrap_or_else(|_| "{}".to_string());
    format!(
        r#"
import base64, io, json, os, pathlib, shutil, sys, tarfile

payload = json.loads({payload:?})

def resolve(value):
    path = pathlib.Path(value or ".")
    if path.is_absolute() or ".." in path.parts:
        raise ValueError("workspace path must be relative and must not contain ..")
    return path

def ms(path):
    try:
        return int(path.stat().st_mtime * 1000)
    except OSError:
        return None

def kind(path):
    if path.is_symlink():
        return "symlink"
    if path.is_dir():
        return "directory"
    if path.is_file():
        return "file"
    return "other"

def list_entries():
    base = resolve(payload.get("path"))
    if not base.is_dir():
        raise ValueError(f"workspace path {{base}} is not a directory")
    entries = []
    for child in base.iterdir():
        st = child.lstat()
        entries.append({{
            "name": child.name,
            "kind": kind(child),
            "size_bytes": st.st_size if child.is_file() else None,
            "modified_ms": int(st.st_mtime * 1000),
            "hidden": child.name.startswith("."),
            "readonly": not os.access(child, os.W_OK),
        }})
    return {{"entries": entries}}

def read_file():
    path = resolve(payload.get("path"))
    if not path.is_file():
        raise ValueError(f"workspace path {{path}} is not a file")
    size = path.stat().st_size
    offset = min(int(payload.get("offset") or 0), size)
    limit = payload.get("limit_bytes")
    with path.open("rb") as handle:
        handle.seek(offset)
        data = handle.read(None if limit is None else max(1, int(limit)))
    try:
        text = data.decode("utf-8")
        encoding = "utf8"
        encoded = text
    except UnicodeDecodeError:
        encoding = "base64"
        encoded = base64.b64encode(data).decode("ascii")
    return {{
        "name": path.name,
        "size_bytes": size,
        "modified_ms": ms(path),
        "offset": offset,
        "returned_bytes": len(data),
        "truncated": offset + len(data) < size,
        "encoding": encoding,
        "data": encoded,
    }}

def safe_members(archive):
    for member in archive.getmembers():
        member_path = pathlib.PurePosixPath(member.name)
        if member_path.is_absolute() or ".." in member_path.parts:
            continue
        yield member

def upload():
    target = resolve(payload.get("path"))
    target.mkdir(parents=True, exist_ok=True)
    raw = sys.stdin.buffer.read()
    with tarfile.open(fileobj=io.BytesIO(raw), mode="r:gz") as archive:
        members = list(safe_members(archive))
        archive.extractall(target, members=members)
    return {{"count": len(members)}}

def delete():
    path = resolve(payload.get("path"))
    if path.is_dir() and not path.is_symlink():
        shutil.rmtree(path)
    else:
        path.unlink()
    return {{"ok": True}}

def move():
    source = resolve(payload.get("from"))
    target = resolve(payload.get("to"))
    if target.exists():
        raise ValueError(f"workspace path {{target}} already exists")
    target.parent.mkdir(parents=True, exist_ok=True)
    source.rename(target)
    return {{"ok": True}}

def add_path(builder, rel):
    path = resolve(rel)
    archive_name = pathlib.Path("workspace") if str(path) == "." else path
    if path.is_dir():
        builder.add(path, arcname=str(archive_name))
    elif path.is_file():
        builder.add(path, arcname=str(archive_name))
    else:
        raise ValueError(f"workspace path {{path}} is not a file or directory")

def download():
    output = io.BytesIO()
    with tarfile.open(fileobj=output, mode="w:gz") as archive:
        for rel in payload.get("paths") or []:
            add_path(archive, rel)
    sys.stdout.buffer.write(output.getvalue())
    return None

handlers = {{
    "list": list_entries,
    "read": read_file,
    "upload": upload,
    "delete": delete,
    "move": move,
    "download": download,
}}

try:
    result = handlers[payload["operation"]]()
    if result is not None:
        print(json.dumps(result, ensure_ascii=False))
except Exception as error:
    print(str(error), file=sys.stderr)
    raise SystemExit(1)
"#
    )
}

fn shell_quote(value: &str) -> String {
    let escaped = value.replace('\'', "'\\''");
    format!("'{escaped}'")
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
