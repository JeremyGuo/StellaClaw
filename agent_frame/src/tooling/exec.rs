use super::args::{f64_arg, string_arg, usize_arg_with_default};
use super::remote::{
    ExecutionTarget, RemoteWorkpathMap, execution_target_arg, remote_schema_property,
    resolve_remote_cwd,
};
use super::runtime_state::{read_status_json, spawn_background_worker_process};
use super::{InterruptSignal, Tool, compact_tool_status_fields_for_model, resolve_path};
use crate::tool_worker::ToolWorkerJob;
use anyhow::{Context, Result, anyhow};
use serde::Serialize;
use serde_json::{Map, Value, json};
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use uuid::Uuid;

fn process_state_dir(runtime_state_root: &Path) -> PathBuf {
    runtime_state_root.join("agent_frame").join("processes")
}

fn process_state_dir_if_exists(runtime_state_root: &Path) -> Option<PathBuf> {
    let path = process_state_dir(runtime_state_root);
    path.exists().then_some(path)
}

fn ensure_process_state_dir(runtime_state_root: &Path) -> Result<PathBuf> {
    let path = process_state_dir(runtime_state_root);
    fs::create_dir_all(&path).with_context(|| format!("failed to create {}", path.display()))?;
    Ok(path)
}

const EXEC_START_DEFAULT_WAIT_TIMEOUT_SECONDS: f64 = 270.0;
const EXEC_OUTPUT_MAX_CHARS: usize = 1000;
const DIRECT_READ_COMMANDS: &[&str] = &["cat", "grep", "find", "head", "tail", "ls"];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ExecTimeoutAction {
    Continue,
    Kill,
}

impl ExecTimeoutAction {
    fn as_str(self) -> &'static str {
        match self {
            Self::Continue => "continue",
            Self::Kill => "kill",
        }
    }
}

fn exec_timeout_action_arg(
    arguments: &Map<String, Value>,
    key: &str,
    default: ExecTimeoutAction,
) -> Result<ExecTimeoutAction> {
    let Some(value) = arguments.get(key) else {
        return Ok(default);
    };
    let text = value
        .as_str()
        .ok_or_else(|| anyhow!("argument {} must be a string", key))?
        .trim()
        .to_ascii_lowercase();
    match text.as_str() {
        "continue" => Ok(ExecTimeoutAction::Continue),
        "kill" => Ok(ExecTimeoutAction::Kill),
        _ => Err(anyhow!("argument {} must be one of: continue, kill", key)),
    }
}

fn max_output_chars_arg(arguments: &Map<String, Value>) -> Result<usize> {
    let value = usize_arg_with_default(arguments, "max_output_chars", EXEC_OUTPUT_MAX_CHARS)?;
    if value > EXEC_OUTPUT_MAX_CHARS {
        return Err(anyhow!(
            "argument max_output_chars must be less than or equal to {}",
            EXEC_OUTPUT_MAX_CHARS
        ));
    }
    Ok(value)
}

fn shell_command_head(command: &str) -> Option<&str> {
    command
        .trim_start()
        .split_whitespace()
        .next()
        .map(|head| head.trim_matches(|ch| ch == '\'' || ch == '"'))
}

fn direct_read_command_guidance(command: &str) -> Option<&'static str> {
    let head = shell_command_head(command)?;
    let head = Path::new(head)
        .file_name()
        .and_then(|value| value.to_str())
        .unwrap_or(head);
    match head {
        "cat" => Some("Use file_read with file_path instead of exec_start cat."),
        "grep" => Some("Use grep with pattern/path instead of exec_start grep."),
        "find" => Some("Use glob or ls instead of exec_start find."),
        "head" | "tail" => Some("Use file_read with offset/limit instead of exec_start head/tail."),
        "ls" => Some("Use ls with path instead of exec_start ls."),
        "ssh" => Some(
            "Manual ssh through exec_start is rejected. Follow the remote execution policy in the system prompt.",
        ),
        _ if DIRECT_READ_COMMANDS.contains(&head) => {
            Some("Use the dedicated filesystem/search tool instead of exec_start.")
        }
        _ => None,
    }
}

#[derive(Clone, Serialize, serde::Deserialize)]
pub(super) struct ProcessMetadata {
    pub(super) exec_id: String,
    pub(super) worker_pid: u32,
    #[serde(default)]
    pub(super) tty: bool,
    #[serde(default = "default_remote_local")]
    pub(super) remote: String,
    pub(super) command: String,
    pub(super) cwd: String,
    pub(super) stdout_path: String,
    pub(super) stderr_path: String,
    pub(super) status_path: String,
    pub(super) worker_exit_code_path: String,
    pub(super) requests_dir: String,
}

fn default_remote_local() -> String {
    "local".to_string()
}

#[derive(serde::Deserialize)]
struct LegacyProcessMetadata {
    exec_id: String,
    pid: u32,
    command: String,
    cwd: String,
    stdout_path: String,
    stderr_path: String,
    exit_code_path: String,
}

enum ProcessMetadataRecord {
    Current(ProcessMetadata),
    Legacy(LegacyProcessMetadata),
}

static LIVE_PROCESSES: std::sync::OnceLock<Mutex<BTreeMap<String, ProcessMetadata>>> =
    std::sync::OnceLock::new();

fn live_processes() -> &'static Mutex<BTreeMap<String, ProcessMetadata>> {
    LIVE_PROCESSES.get_or_init(|| Mutex::new(BTreeMap::new()))
}

pub(super) fn process_meta_path(dir: &Path, exec_id: &str) -> PathBuf {
    dir.join(format!("{}.json", exec_id))
}

fn parse_process_metadata_record(raw: &str) -> Result<ProcessMetadataRecord> {
    match serde_json::from_str::<ProcessMetadata>(raw) {
        Ok(metadata) => Ok(ProcessMetadataRecord::Current(metadata)),
        Err(current_err) => match serde_json::from_str::<LegacyProcessMetadata>(raw) {
            Ok(metadata) => Ok(ProcessMetadataRecord::Legacy(metadata)),
            Err(_) => Err(current_err).context("failed to parse process metadata"),
        },
    }
}

fn legacy_process_metadata_to_current(
    dir: &Path,
    metadata: LegacyProcessMetadata,
) -> ProcessMetadata {
    ProcessMetadata {
        exec_id: metadata.exec_id.clone(),
        worker_pid: metadata.pid,
        tty: false,
        remote: default_remote_local(),
        command: metadata.command,
        cwd: metadata.cwd,
        stdout_path: metadata.stdout_path,
        stderr_path: metadata.stderr_path,
        status_path: dir
            .join(format!("{}.status.json", metadata.exec_id))
            .display()
            .to_string(),
        worker_exit_code_path: metadata.exit_code_path,
        requests_dir: dir
            .join(format!("{}.requests", metadata.exec_id))
            .display()
            .to_string(),
    }
}

fn read_process_metadata(dir: &Path, exec_id: &str) -> Result<ProcessMetadata> {
    let path = process_meta_path(dir, exec_id);
    let raw =
        fs::read_to_string(&path).with_context(|| format!("failed to read {}", path.display()))?;
    match parse_process_metadata_record(&raw)? {
        ProcessMetadataRecord::Current(metadata) => Ok(metadata),
        ProcessMetadataRecord::Legacy(metadata) => {
            Ok(legacy_process_metadata_to_current(dir, metadata))
        }
    }
}

fn write_process_metadata(dir: &Path, metadata: &ProcessMetadata) -> Result<()> {
    let raw =
        serde_json::to_string_pretty(metadata).context("failed to serialize process metadata")?;
    fs::write(process_meta_path(dir, &metadata.exec_id), raw)
        .with_context(|| format!("failed to write process metadata for {}", metadata.exec_id))
}

fn read_file_lines_window(
    path: &Path,
    start: usize,
    limit: usize,
) -> Result<(String, usize, bool)> {
    if !path.exists() {
        return Ok((String::new(), 0, false));
    }
    let raw = fs::read(path).with_context(|| format!("failed to read {}", path.display()))?;
    let text = String::from_utf8_lossy(&raw);
    let total_chars = text.chars().count();
    if limit == 0 {
        return Ok((String::new(), total_chars, total_chars > 0));
    }
    let lines = text.lines().map(ToOwned::to_owned).collect::<Vec<_>>();
    let end = lines.len().saturating_sub(start);
    let begin = end.saturating_sub(limit);
    let line_window_truncated = begin > 0 || start > 0;
    Ok((
        lines[begin..end].join("\n"),
        total_chars,
        line_window_truncated,
    ))
}

fn truncate_exec_output(text: &str, max_chars: usize) -> (String, bool) {
    let char_count = text.chars().count();
    if char_count <= max_chars {
        return (text.to_string(), false);
    }
    if max_chars == 0 {
        return (String::new(), true);
    }

    let head_chars = max_chars.saturating_mul(4) / 10;
    let tail_chars = max_chars.saturating_sub(head_chars);
    let head = text.chars().take(head_chars).collect::<String>();
    let tail = text
        .chars()
        .rev()
        .take(tail_chars)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect::<String>();
    (format!("{head}{tail}"), true)
}

fn workspace_output_relative_path(exec_id: &str, stream: &str) -> PathBuf {
    PathBuf::from(".agent_frame")
        .join("exec")
        .join(format!("{exec_id}.{stream}"))
}

fn format_relative_path(path: &Path) -> String {
    path.components()
        .map(|component| component.as_os_str().to_string_lossy())
        .collect::<Vec<_>>()
        .join("/")
}

fn sync_workspace_output_file(
    workspace_root: Option<&Path>,
    metadata: &ProcessMetadata,
    source: &Path,
    stream: &str,
) -> Result<Option<String>> {
    let Some(workspace_root) = workspace_root else {
        return Ok(None);
    };
    let relative_path = workspace_output_relative_path(&metadata.exec_id, stream);
    let destination = workspace_root.join(&relative_path);
    if source != destination {
        if let Some(parent) = destination.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("failed to create {}", parent.display()))?;
        }
        if source.exists() {
            fs::copy(source, &destination).with_context(|| {
                format!(
                    "failed to copy {} to {}",
                    source.display(),
                    destination.display()
                )
            })?;
        } else {
            fs::write(&destination, b"")
                .with_context(|| format!("failed to write {}", destination.display()))?;
        }
    }
    Ok(Some(format_relative_path(&relative_path)))
}

fn insert_exec_output_fields(
    object: &mut Map<String, Value>,
    metadata: &ProcessMetadata,
    workspace_root: Option<&Path>,
    start: usize,
    limit: usize,
    max_output_chars: usize,
) -> Result<()> {
    let stdout_path = Path::new(&metadata.stdout_path);
    let stderr_path = Path::new(&metadata.stderr_path);
    let (stdout_window, stdout_chars, stdout_line_truncated) =
        read_file_lines_window(stdout_path, start, limit)?;
    let (stderr_window, stderr_chars, stderr_line_truncated) =
        read_file_lines_window(stderr_path, start, limit)?;
    let (stdout, stdout_truncated) = truncate_exec_output(&stdout_window, max_output_chars);
    let (stderr, stderr_truncated) = truncate_exec_output(&stderr_window, max_output_chars);
    let stdout_truncated = stdout_truncated || stdout_line_truncated;
    let stderr_truncated = stderr_truncated || stderr_line_truncated;

    object.insert("stdout".to_string(), Value::String(stdout));
    if stdout_truncated {
        object.insert("stdout_truncated".to_string(), Value::Bool(true));
    }
    if stderr_truncated || !stderr.is_empty() {
        object.insert("stderr".to_string(), Value::String(stderr));
    }
    if stderr_truncated {
        object.insert("stderr_truncated".to_string(), Value::Bool(true));
    }
    object.insert("stdout_chars".to_string(), Value::from(stdout_chars));
    if stderr_chars > 0 || stderr_truncated {
        object.insert("stderr_chars".to_string(), Value::from(stderr_chars));
    }
    if let Some(path) = sync_workspace_output_file(workspace_root, metadata, stdout_path, "stdout")?
    {
        object.insert("stdout_path".to_string(), Value::String(path));
    }
    if let Some(path) = sync_workspace_output_file(workspace_root, metadata, stderr_path, "stderr")?
    {
        object.insert("stderr_path".to_string(), Value::String(path));
    }
    Ok(())
}

pub(super) fn read_exit_code(path: &Path) -> Option<i32> {
    fs::read_to_string(path)
        .ok()
        .and_then(|value| value.trim().parse::<i32>().ok())
}

#[cfg(not(windows))]
pub(super) fn process_is_running(pid: u32) -> bool {
    Command::new("sh")
        .arg("-c")
        .arg(format!("kill -0 {} 2>/dev/null", pid))
        .status()
        .map(|status| status.success())
        .unwrap_or(false)
}

#[cfg(windows)]
pub(super) fn process_is_running(pid: u32) -> bool {
    let Ok(output) = Command::new("tasklist")
        .args(["/FI", &format!("PID eq {pid}"), "/NH"])
        .output()
    else {
        return false;
    };
    if !output.status.success() {
        return false;
    }
    let pid_text = pid.to_string();
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .any(|line| line.split_whitespace().any(|part| part == pid_text))
}

#[cfg(not(windows))]
pub(super) fn terminate_process_pid(pid: u32) {
    let _ = Command::new("kill")
        .arg("-KILL")
        .arg(pid.to_string())
        .status();
}

#[cfg(windows)]
pub(super) fn terminate_process_pid(pid: u32) {
    let _ = Command::new("taskkill")
        .args(["/PID", &pid.to_string(), "/T", "/F"])
        .status();
}

#[cfg(not(windows))]
fn terminate_process_group(pid: u32) {
    let _ = Command::new("kill")
        .arg("-KILL")
        .arg(format!("-{}", pid))
        .status();
}

#[cfg(windows)]
fn terminate_process_group(pid: u32) {
    terminate_process_pid(pid);
}

pub(super) fn record_exit_code(path: &Path, code: i32) -> Result<()> {
    fs::write(path, code.to_string())
        .with_context(|| format!("failed to write exit code to {}", path.display()))
}

fn process_request_path(requests_dir: &Path, request_id: &str) -> PathBuf {
    requests_dir.join(format!("request-{request_id}.json"))
}

fn process_request_result_path(requests_dir: &Path, request_id: &str) -> PathBuf {
    requests_dir.join(format!("request-{request_id}.result.json"))
}

fn queue_exec_input_request(metadata: &ProcessMetadata, input: &str) -> Result<PathBuf> {
    let requests_dir = Path::new(&metadata.requests_dir);
    fs::create_dir_all(requests_dir)
        .with_context(|| format!("failed to create {}", requests_dir.display()))?;
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    let request_id = format!("{timestamp:020}-{}", Uuid::new_v4());
    let request_path = process_request_path(requests_dir, &request_id);
    let temp_path = requests_dir.join(format!("request-{request_id}.tmp"));
    fs::write(
        &temp_path,
        serde_json::to_vec_pretty(&json!({ "input": input }))
            .context("failed to serialize exec input request")?,
    )
    .with_context(|| format!("failed to write {}", temp_path.display()))?;
    fs::rename(&temp_path, &request_path).with_context(|| {
        format!(
            "failed to move {} to {}",
            temp_path.display(),
            request_path.display()
        )
    })?;
    Ok(process_request_result_path(requests_dir, &request_id))
}

fn read_exec_input_result(result_path: &Path) -> Result<Option<Value>> {
    if !result_path.exists() {
        return Ok(None);
    }
    let raw = fs::read_to_string(result_path)
        .with_context(|| format!("failed to read {}", result_path.display()))?;
    let value = serde_json::from_str(&raw).context("failed to parse exec input result")?;
    Ok(Some(value))
}

fn exec_pid_from_snapshot(snapshot: &Value) -> Option<u32> {
    snapshot
        .get("pid")
        .and_then(Value::as_u64)
        .and_then(|pid| u32::try_from(pid).ok())
}

fn kill_exec_processes(metadata: &ProcessMetadata, snapshot: Option<&Value>) {
    if let Some(exec_pid) = snapshot.and_then(exec_pid_from_snapshot)
        && exec_pid != metadata.worker_pid
    {
        if metadata.tty {
            terminate_process_group(exec_pid);
        } else {
            terminate_process_pid(exec_pid);
        }
    }
    terminate_process_pid(metadata.worker_pid);
}

fn write_exec_snapshot(
    metadata: &ProcessMetadata,
    running: bool,
    completed: bool,
    returncode: Value,
    extra: Option<Map<String, Value>>,
) -> Result<()> {
    let pid = read_status_json(Path::new(&metadata.status_path))
        .ok()
        .and_then(|value| value.get("pid").cloned())
        .unwrap_or(Value::Null);
    let stdin_closed = read_status_json(Path::new(&metadata.status_path))
        .ok()
        .and_then(|value| value.get("stdin_closed").cloned())
        .unwrap_or(Value::Bool(true));
    let failed = read_status_json(Path::new(&metadata.status_path))
        .ok()
        .and_then(|value| value.get("failed").cloned())
        .unwrap_or(Value::Bool(false));

    let mut object = Map::from_iter([
        (
            "exec_id".to_string(),
            Value::String(metadata.exec_id.clone()),
        ),
        ("tty".to_string(), Value::Bool(metadata.tty)),
        ("pid".to_string(), pid),
        (
            "command".to_string(),
            Value::String(metadata.command.clone()),
        ),
        ("remote".to_string(), Value::String(metadata.remote.clone())),
        ("cwd".to_string(), Value::String(metadata.cwd.clone())),
        ("running".to_string(), Value::Bool(running)),
        ("completed".to_string(), Value::Bool(completed)),
        ("returncode".to_string(), returncode),
        ("stdin_closed".to_string(), stdin_closed),
        ("failed".to_string(), failed),
    ]);
    if let Some(extra) = extra {
        object.extend(extra);
    }
    fs::write(
        &metadata.status_path,
        serde_json::to_vec_pretty(&Value::Object(object))
            .context("failed to serialize exec status snapshot")?,
    )
    .with_context(|| format!("failed to write {}", metadata.status_path))
}

fn spawn_managed_process(
    runtime_state_root: &Path,
    state_dir: &Path,
    workspace_root: &Path,
    command: &str,
    cwd: &Path,
    tty: bool,
    target: &ExecutionTarget,
) -> Result<ProcessMetadata> {
    let exec_id = Uuid::new_v4().to_string();
    let output_dir = workspace_root.join(".agent_frame").join("exec");
    fs::create_dir_all(&output_dir)
        .with_context(|| format!("failed to create {}", output_dir.display()))?;
    let stdout_path = output_dir.join(format!("{}.stdout", exec_id));
    let stderr_path = output_dir.join(format!("{}.stderr", exec_id));
    let status_path = state_dir.join(format!("{}.status.json", exec_id));
    let requests_dir = state_dir.join(format!("{}.requests", exec_id));
    fs::create_dir_all(&requests_dir)
        .with_context(|| format!("failed to create {}", requests_dir.display()))?;
    fs::write(
        &status_path,
        serde_json::to_vec_pretty(&json!({
            "exec_id": exec_id,
            "tty": tty,
            "pid": Value::Null,
            "command": command,
            "remote": target.remote_name(),
            "cwd": cwd.display().to_string(),
            "running": true,
            "completed": false,
            "returncode": Value::Null,
            "stdin_closed": false,
            "failed": false,
            "error": Value::Null,
        }))
        .context("failed to serialize initial exec status")?,
    )
    .with_context(|| format!("failed to write {}", status_path.display()))?;
    let worker = spawn_background_worker_process(
        runtime_state_root,
        "exec",
        &exec_id,
        &ToolWorkerJob::Exec {
            exec_id: exec_id.clone(),
            tty,
            remote: match target {
                ExecutionTarget::Local => None,
                ExecutionTarget::RemoteSsh { host } => Some(host.clone()),
            },
            command: command.to_string(),
            cwd: cwd.display().to_string(),
            status_path: status_path.display().to_string(),
            stdout_path: stdout_path.display().to_string(),
            stderr_path: stderr_path.display().to_string(),
            requests_dir: requests_dir.display().to_string(),
        },
    )?;
    let metadata = ProcessMetadata {
        exec_id: exec_id.clone(),
        worker_pid: worker.pid,
        tty,
        remote: target.remote_name().to_string(),
        command: command.to_string(),
        cwd: cwd.display().to_string(),
        stdout_path: stdout_path.display().to_string(),
        stderr_path: stderr_path.display().to_string(),
        status_path: status_path.display().to_string(),
        worker_exit_code_path: worker.exit_code_path,
        requests_dir: requests_dir.display().to_string(),
    };
    write_process_metadata(state_dir, &metadata)?;
    live_processes()
        .lock()
        .unwrap()
        .insert(exec_id, metadata.clone());
    Ok(metadata)
}

fn process_missing_error(exec_id: &str) -> anyhow::Error {
    anyhow!(
        "exec process {} no longer exists; it may have already finished, been killed, or been terminated when the main runtime shut down",
        exec_id
    )
}

fn read_process_snapshot(
    state_dir: &Path,
    exec_id: &str,
    start: usize,
    limit: usize,
    max_output_chars: usize,
    workspace_root: Option<&Path>,
) -> Result<Value> {
    let metadata = match read_process_metadata(state_dir, exec_id) {
        Ok(metadata) => metadata,
        Err(_) => return Err(process_missing_error(exec_id)),
    };
    let mut snapshot = read_status_json(Path::new(&metadata.status_path))
        .map_err(|_| process_missing_error(exec_id))?;
    let worker_alive = read_exit_code(Path::new(&metadata.worker_exit_code_path)).is_none()
        && process_is_running(metadata.worker_pid);
    let running = snapshot
        .get("running")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    if running && !worker_alive {
        let pid = snapshot.get("pid").cloned().unwrap_or(Value::Null);
        snapshot = json!({
            "exec_id": metadata.exec_id,
            "tty": metadata.tty,
            "pid": pid,
            "command": metadata.command,
            "remote": metadata.remote,
            "cwd": metadata.cwd,
            "running": false,
            "completed": false,
            "returncode": Value::Null,
            "stdin_closed": true,
            "failed": true,
            "error": "exec worker exited unexpectedly",
        });
    }
    let mut object = snapshot
        .as_object()
        .cloned()
        .ok_or_else(|| anyhow!("exec snapshot must be a JSON object"))?;
    object
        .entry("tty".to_string())
        .or_insert_with(|| Value::Bool(metadata.tty));
    insert_exec_output_fields(
        &mut object,
        &metadata,
        workspace_root,
        start,
        limit,
        max_output_chars,
    )?;
    Ok(Value::Object(object))
}

pub(super) fn list_active_exec_summaries(runtime_state_root: &Path) -> Result<Vec<String>> {
    let Some(state_dir) = process_state_dir_if_exists(runtime_state_root) else {
        return Ok(Vec::new());
    };
    let mut entries = Vec::new();
    for entry in fs::read_dir(&state_dir)
        .with_context(|| format!("failed to read {}", state_dir.display()))?
    {
        let entry = entry?;
        let path = entry.path();
        let Some(file_name) = path.file_name().and_then(|value| value.to_str()) else {
            continue;
        };
        if !file_name.ends_with(".json") || file_name.ends_with(".status.json") {
            continue;
        }
        let raw = fs::read_to_string(&path)
            .with_context(|| format!("failed to read {}", path.display()))?;
        let (metadata, allow_missing_snapshot) = match parse_process_metadata_record(&raw)? {
            ProcessMetadataRecord::Current(metadata) => (metadata, false),
            // Older workdirs can still contain pre-worker exec metadata files.
            // Skip them for runtime summaries so compaction stays best-effort.
            ProcessMetadataRecord::Legacy(metadata) => (
                legacy_process_metadata_to_current(&state_dir, metadata),
                true,
            ),
        };
        let snapshot = match read_process_snapshot(
            &state_dir,
            &metadata.exec_id,
            0,
            0,
            EXEC_OUTPUT_MAX_CHARS,
            None,
        ) {
            Ok(snapshot) => snapshot,
            Err(_) if allow_missing_snapshot => continue,
            Err(err) => return Err(err),
        };
        if !snapshot
            .get("running")
            .and_then(Value::as_bool)
            .unwrap_or(false)
        {
            continue;
        }
        let pid = snapshot
            .get("pid")
            .and_then(Value::as_u64)
            .unwrap_or(metadata.worker_pid as u64);
        entries.push(format!(
            "- exec_id=`{}` pid={} remote=`{}` tty={} cwd=`{}` command=`{}`",
            metadata.exec_id, pid, metadata.remote, metadata.tty, metadata.cwd, metadata.command
        ));
    }
    entries.sort();
    Ok(entries)
}

pub fn terminate_all_managed_processes() -> Result<()> {
    let mut registry = live_processes().lock().unwrap();
    let processes = std::mem::take(&mut *registry)
        .into_values()
        .collect::<Vec<_>>();
    drop(registry);
    for process in processes {
        let state_dir = Path::new(&process.status_path)
            .parent()
            .unwrap_or_else(|| Path::new("."));
        let snapshot = read_process_snapshot(
            state_dir,
            &process.exec_id,
            0,
            0,
            EXEC_OUTPUT_MAX_CHARS,
            None,
        )
        .ok();
        let meta_path = process_meta_path(state_dir, &process.exec_id);
        let _ = fs::remove_file(meta_path);
        let _ = fs::remove_file(&process.worker_exit_code_path);
        let _ = fs::remove_file(&process.status_path);
        let _ = fs::remove_dir_all(&process.requests_dir);
        kill_exec_processes(&process, snapshot.as_ref());
    }
    Ok(())
}

fn iter_process_metadata_json_files(dir: &Path) -> Result<Vec<PathBuf>> {
    let mut paths = Vec::new();
    for entry in fs::read_dir(dir).with_context(|| format!("failed to read {}", dir.display()))? {
        let entry = entry?;
        let path = entry.path();
        let Some(file_name) = path.file_name().and_then(|value| value.to_str()) else {
            continue;
        };
        if !file_name.ends_with(".json") || file_name.ends_with(".status.json") {
            continue;
        }
        paths.push(path);
    }
    paths.sort();
    Ok(paths)
}

pub(super) fn cleanup_exec_processes(runtime_state_root: &Path) -> Result<usize> {
    let Some(state_dir) = process_state_dir_if_exists(runtime_state_root) else {
        return Ok(0);
    };
    let mut killed = 0usize;
    for path in iter_process_metadata_json_files(&state_dir)? {
        let raw = fs::read_to_string(&path)
            .with_context(|| format!("failed to read {}", path.display()))?;
        let (metadata, allow_missing_snapshot) = match parse_process_metadata_record(&raw)? {
            ProcessMetadataRecord::Current(metadata) => (metadata, false),
            ProcessMetadataRecord::Legacy(metadata) => (
                legacy_process_metadata_to_current(&state_dir, metadata),
                true,
            ),
        };
        let snapshot = match read_process_snapshot(
            &state_dir,
            &metadata.exec_id,
            0,
            0,
            EXEC_OUTPUT_MAX_CHARS,
            None,
        ) {
            Ok(snapshot) => Some(snapshot),
            Err(_) if allow_missing_snapshot => None,
            Err(err) => return Err(err),
        };
        let running = snapshot
            .as_ref()
            .and_then(|value| value.get("running"))
            .and_then(Value::as_bool)
            .unwrap_or(false);
        live_processes().lock().unwrap().remove(&metadata.exec_id);
        if running
            || (read_exit_code(Path::new(&metadata.worker_exit_code_path)).is_none()
                && process_is_running(metadata.worker_pid))
        {
            kill_exec_processes(&metadata, snapshot.as_ref());
            let _ = record_exit_code(Path::new(&metadata.worker_exit_code_path), -9);
            let mut extra = Map::new();
            extra.insert("cancelled".to_string(), Value::Bool(true));
            extra.insert(
                "reason".to_string(),
                Value::String("session_destroyed".to_string()),
            );
            extra.insert("stdin_closed".to_string(), Value::Bool(true));
            extra.insert("failed".to_string(), Value::Bool(false));
            extra.insert("error".to_string(), Value::Null);
            let _ = write_exec_snapshot(&metadata, false, false, Value::from(-9), Some(extra));
            killed = killed.saturating_add(1);
        }
    }
    Ok(killed)
}

fn wait_for_managed_process(
    state_dir: &Path,
    exec_id: &str,
    wait_timeout_seconds: f64,
    input: Option<&str>,
    start: usize,
    limit: usize,
    max_output_chars: usize,
    workspace_root: Option<&Path>,
    on_timeout: ExecTimeoutAction,
    cancel_flag: Option<&Arc<InterruptSignal>>,
) -> Result<Value> {
    if !wait_timeout_seconds.is_finite() || wait_timeout_seconds < 0.0 {
        return Err(anyhow!(
            "argument wait_timeout_seconds must be a finite non-negative number"
        ));
    }
    let pending_input_result = if let Some(input) = input {
        let metadata = read_process_metadata(state_dir, exec_id)
            .map_err(|_| process_missing_error(exec_id))?;
        Some(queue_exec_input_request(&metadata, input)?)
    } else {
        None
    };
    let deadline = std::time::Instant::now() + Duration::from_secs_f64(wait_timeout_seconds);
    let cancel_receiver = cancel_flag.map(|signal| signal.subscribe());
    let mut input_acknowledged = pending_input_result.is_none();
    loop {
        let snapshot = read_process_snapshot(
            state_dir,
            exec_id,
            start,
            limit,
            max_output_chars,
            workspace_root,
        )?;
        if !input_acknowledged && let Some(result_path) = pending_input_result.as_ref() {
            if let Some(result) = read_exec_input_result(result_path)? {
                let _ = fs::remove_file(result_path);
                let ok = result.get("ok").and_then(Value::as_bool).unwrap_or(false);
                if !ok {
                    let error = result
                        .get("error")
                        .and_then(Value::as_str)
                        .unwrap_or("stdin is closed for exec process");
                    return Err(anyhow!(error.to_string()));
                }
                input_acknowledged = true;
            } else {
                let running = snapshot
                    .get("running")
                    .and_then(Value::as_bool)
                    .unwrap_or(false);
                let stdin_closed = snapshot
                    .get("stdin_closed")
                    .and_then(Value::as_bool)
                    .unwrap_or(false);
                let failed = snapshot
                    .get("failed")
                    .and_then(Value::as_bool)
                    .unwrap_or(false);
                if stdin_closed || failed || !running {
                    let error = snapshot
                        .get("error")
                        .and_then(Value::as_str)
                        .map(ToOwned::to_owned)
                        .unwrap_or_else(|| format!("stdin is closed for exec process {}", exec_id));
                    return Err(anyhow!(error));
                }
                thread::sleep(Duration::from_millis(50));
                continue;
            }
        }

        let completed = snapshot
            .get("completed")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        if completed {
            live_processes().lock().unwrap().remove(exec_id);
            return Ok(snapshot);
        }
        if let Some(signal) = cancel_flag
            && signal.is_requested()
        {
            return Ok(json!({
                "interrupted": true,
                "reason": "agent_turn_interrupted",
                "process": snapshot
            }));
        }
        if std::time::Instant::now() >= deadline {
            if on_timeout == ExecTimeoutAction::Kill {
                let metadata = read_process_metadata(state_dir, exec_id)
                    .map_err(|_| process_missing_error(exec_id))?;
                kill_exec_processes(&metadata, Some(&snapshot));
                let _ = record_exit_code(Path::new(&metadata.worker_exit_code_path), -9);
                let mut extra = Map::new();
                extra.insert("killed".to_string(), Value::Bool(true));
                extra.insert("wait_timed_out".to_string(), Value::Bool(true));
                extra.insert(
                    "on_timeout".to_string(),
                    Value::String(on_timeout.as_str().to_string()),
                );
                extra.insert("stdin_closed".to_string(), Value::Bool(true));
                extra.insert("failed".to_string(), Value::Bool(false));
                extra.insert("error".to_string(), Value::Null);
                write_exec_snapshot(&metadata, false, true, Value::from(-9), Some(extra))?;
                live_processes().lock().unwrap().remove(exec_id);
                let mut object = snapshot
                    .as_object()
                    .cloned()
                    .ok_or_else(|| anyhow!("exec snapshot must be a JSON object"))?;
                object.insert("running".to_string(), Value::Bool(false));
                object.insert("completed".to_string(), Value::Bool(true));
                object.insert("returncode".to_string(), Value::from(-9));
                object.insert("wait_timed_out".to_string(), Value::Bool(true));
                object.insert(
                    "on_timeout".to_string(),
                    Value::String(on_timeout.as_str().to_string()),
                );
                object.insert("killed".to_string(), Value::Bool(true));
                object.insert("stdin_closed".to_string(), Value::Bool(true));
                object.insert("failed".to_string(), Value::Bool(false));
                object.insert("error".to_string(), Value::Null);
                return Ok(Value::Object(object));
            }

            let mut object = snapshot
                .as_object()
                .cloned()
                .ok_or_else(|| anyhow!("exec snapshot must be a JSON object"))?;
            object.insert("wait_timed_out".to_string(), Value::Bool(true));
            object.insert(
                "on_timeout".to_string(),
                Value::String(on_timeout.as_str().to_string()),
            );
            object.insert("running".to_string(), Value::Bool(true));
            object.insert("completed".to_string(), Value::Bool(false));
            object.insert("returncode".to_string(), Value::Null);
            return Ok(Value::Object(object));
        }
        if let Some(cancel_receiver) = &cancel_receiver {
            crossbeam_channel::select! {
                recv(cancel_receiver) -> _ => {
                    return Ok(json!({
                        "interrupted": true,
                        "reason": "agent_turn_interrupted",
                        "process": snapshot
                    }));
                }
                recv(crossbeam_channel::after(Duration::from_millis(200))) -> _ => {}
            }
        } else {
            thread::sleep(Duration::from_millis(200));
        }
    }
}

pub(super) fn exec_start_tool(
    workspace_root: PathBuf,
    runtime_state_root: PathBuf,
    remote_workpaths: RemoteWorkpathMap,
    cancel_flag: Option<Arc<InterruptSignal>>,
) -> Tool {
    Tool::new_interruptible(
        "exec_start",
        "Start a shell command or executable. Output returned to the model is capped by max_output_chars, which must be 0..1000; complete stdout/stderr are saved at the returned workspace-relative paths.",
        json!({
            "type": "object",
            "properties": {
                "command": {"type": "string"},
                "cwd": {
                    "type": "string",
                    "description": "Working directory for the command. Prefer relative paths for normal workspace work."
                },
                "tty": {"type": "boolean"},
                "include_stdout": {"type": "boolean"},
                "start": {"type": "integer"},
                "limit": {"type": "integer"},
                "return_immediate": {"type": "boolean"},
                "wait_timeout_seconds": {"type": "number"},
                "on_timeout": {"type": "string", "enum": ["continue", "kill", "CONTINUE", "KILL"]},
                "max_output_chars": {"type": "integer", "minimum": 0, "maximum": 1000},
                "remote": remote_schema_property()
            },
            "required": ["command"],
            "additionalProperties": false
        }),
        move |arguments| {
            let arguments = arguments
                .as_object()
                .ok_or_else(|| anyhow!("tool arguments must be an object"))?;
            let target = execution_target_arg(arguments)?;
            let command = string_arg(arguments, "command")?;
            if let Some(guidance) = direct_read_command_guidance(&command) {
                return Err(anyhow!(
                    "direct read/search shell command rejected by tool policy: {guidance}"
                ));
            }
            let include_stdout = arguments
                .get("include_stdout")
                .and_then(Value::as_bool)
                .unwrap_or(true);
            let tty = arguments
                .get("tty")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            let return_immediate = arguments
                .get("return_immediate")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            let wait_timeout_seconds = arguments
                .get("wait_timeout_seconds")
                .map(|_| f64_arg(arguments, "wait_timeout_seconds"))
                .transpose()?
                .unwrap_or(EXEC_START_DEFAULT_WAIT_TIMEOUT_SECONDS);
            let on_timeout =
                exec_timeout_action_arg(arguments, "on_timeout", ExecTimeoutAction::Continue)?;
            let max_output_chars = max_output_chars_arg(arguments)?;
            let start = usize_arg_with_default(arguments, "start", 0)?;
            let limit = usize_arg_with_default(arguments, "limit", 20)?;
            let cwd = match &target {
                ExecutionTarget::Local => arguments
                    .get("cwd")
                    .and_then(Value::as_str)
                    .map(|value| resolve_path(value, &workspace_root))
                    .unwrap_or_else(|| workspace_root.clone()),
                ExecutionTarget::RemoteSsh { host } => PathBuf::from(resolve_remote_cwd(
                    host,
                    arguments.get("cwd").and_then(Value::as_str),
                    &remote_workpaths,
                )?),
            };
            let state_dir = ensure_process_state_dir(&runtime_state_root)?;
            let metadata = spawn_managed_process(
                &runtime_state_root,
                &state_dir,
                &workspace_root,
                &command,
                &cwd,
                tty,
                &target,
            )?;
            let mut result = if return_immediate {
                read_process_snapshot(
                    &state_dir,
                    &metadata.exec_id,
                    start,
                    limit,
                    max_output_chars,
                    Some(&workspace_root),
                )?
            } else {
                wait_for_managed_process(
                    &state_dir,
                    &metadata.exec_id,
                    wait_timeout_seconds,
                    None,
                    start,
                    limit,
                    max_output_chars,
                    Some(&workspace_root),
                    on_timeout,
                    cancel_flag.as_ref(),
                )?
            };
            compact_tool_status_fields_for_model(&mut result);
            if !include_stdout && let Some(object) = result.as_object_mut() {
                object.remove("stdout");
                object.remove("stderr");
                if let Some(process) = object.get_mut("process").and_then(Value::as_object_mut) {
                    process.remove("stdout");
                    process.remove("stderr");
                }
            }
            Ok(result)
        },
    )
}

pub(super) fn exec_observe_tool(
    workspace_root: PathBuf,
    runtime_state_root: PathBuf,
    _cancel_flag: Option<Arc<InterruptSignal>>,
) -> Tool {
    Tool::new(
        "exec_observe",
        "Observe the latest output of a previously started exec process by exec_id. start=0 and limit=2 means the last two lines. Output returned to the model is capped by max_output_chars, which must be 0..1000; complete stdout/stderr are saved at the returned workspace-relative paths. Remote host is inferred from exec_id.",
        json!({
            "type": "object",
            "properties": {
                "exec_id": {"type": "string"},
                "start": {"type": "integer"},
                "limit": {"type": "integer"},
                "max_output_chars": {"type": "integer", "minimum": 0, "maximum": 1000}
            },
            "required": ["exec_id"],
            "additionalProperties": false
        }),
        move |arguments| {
            let arguments = arguments
                .as_object()
                .ok_or_else(|| anyhow!("tool arguments must be an object"))?;
            let exec_id = string_arg(arguments, "exec_id")?;
            let start = usize_arg_with_default(arguments, "start", 0)?;
            let limit = usize_arg_with_default(arguments, "limit", 20)?;
            let max_output_chars = max_output_chars_arg(arguments)?;
            let state_dir = ensure_process_state_dir(&runtime_state_root)?;
            let mut result = read_process_snapshot(
                &state_dir,
                &exec_id,
                start,
                limit,
                max_output_chars,
                Some(&workspace_root),
            )?;
            compact_tool_status_fields_for_model(&mut result);
            Ok(result)
        },
    )
}

pub(super) fn exec_wait_tool(
    workspace_root: PathBuf,
    runtime_state_root: PathBuf,
    cancel_flag: Option<Arc<InterruptSignal>>,
) -> Tool {
    Tool::new_interruptible(
        "exec_wait",
        "Wait on a previously started exec process by exec_id. Optionally write input to stdin before waiting. If interrupted by a newer user message or timeout observation, return immediately and leave the process running. If the process does not finish before wait_timeout_seconds, on_timeout=continue leaves it running while on_timeout=kill terminates it. Output returned to the model is capped by max_output_chars, which must be 0..1000; complete stdout/stderr are saved at the returned workspace-relative paths. Remote host is inferred from exec_id.",
        json!({
            "type": "object",
            "properties": {
                "exec_id": {"type": "string"},
                "wait_timeout_seconds": {"type": "number"},
                "input": {"type": "string"},
                "include_stdout": {"type": "boolean"},
                "start": {"type": "integer"},
                "limit": {"type": "integer"},
                "on_timeout": {"type": "string", "enum": ["continue", "kill", "CONTINUE", "KILL"]},
                "max_output_chars": {"type": "integer", "minimum": 0, "maximum": 1000}
            },
            "required": ["exec_id", "wait_timeout_seconds"],
            "additionalProperties": false
        }),
        move |arguments| {
            let arguments = arguments
                .as_object()
                .ok_or_else(|| anyhow!("tool arguments must be an object"))?;
            let exec_id = string_arg(arguments, "exec_id")?;
            let wait_timeout_seconds = f64_arg(arguments, "wait_timeout_seconds")?;
            let include_stdout = arguments
                .get("include_stdout")
                .and_then(Value::as_bool)
                .unwrap_or(true);
            let on_timeout =
                exec_timeout_action_arg(arguments, "on_timeout", ExecTimeoutAction::Continue)?;
            let max_output_chars = max_output_chars_arg(arguments)?;
            let start = usize_arg_with_default(arguments, "start", 0)?;
            let limit = usize_arg_with_default(arguments, "limit", 20)?;
            let input = arguments
                .get("input")
                .and_then(Value::as_str)
                .map(ToOwned::to_owned);
            let mut result = wait_for_managed_process(
                &ensure_process_state_dir(&runtime_state_root)?,
                &exec_id,
                wait_timeout_seconds,
                input.as_deref(),
                start,
                limit,
                max_output_chars,
                Some(&workspace_root),
                on_timeout,
                cancel_flag.as_ref(),
            )?;
            compact_tool_status_fields_for_model(&mut result);
            if !include_stdout && let Some(object) = result.as_object_mut() {
                object.remove("stdout");
                object.remove("stderr");
                if let Some(process) = object.get_mut("process").and_then(Value::as_object_mut) {
                    process.remove("stdout");
                    process.remove("stderr");
                }
            }
            Ok(result)
        },
    )
}

pub(super) fn exec_kill_tool(
    runtime_state_root: PathBuf,
    _cancel_flag: Option<Arc<InterruptSignal>>,
) -> Tool {
    Tool::new(
        "exec_kill",
        "Immediately stop a previously started exec process by exec_id. Remote host is inferred from exec_id.",
        json!({
            "type": "object",
            "properties": {
                "exec_id": {"type": "string"}
            },
            "required": ["exec_id"],
            "additionalProperties": false
        }),
        move |arguments| {
            let arguments = arguments
                .as_object()
                .ok_or_else(|| anyhow!("tool arguments must be an object"))?;
            let exec_id = string_arg(arguments, "exec_id")?;
            let state_dir = ensure_process_state_dir(&runtime_state_root)?;
            let metadata = read_process_metadata(&state_dir, &exec_id)
                .map_err(|_| process_missing_error(&exec_id))?;
            let previous =
                read_process_snapshot(&state_dir, &exec_id, 0, 0, EXEC_OUTPUT_MAX_CHARS, None).ok();
            kill_exec_processes(&metadata, previous.as_ref());
            let _ = record_exit_code(Path::new(&metadata.worker_exit_code_path), -9);
            let mut extra = Map::new();
            extra.insert("killed".to_string(), Value::Bool(true));
            extra.insert("failed".to_string(), Value::Bool(false));
            extra.insert("stdin_closed".to_string(), Value::Bool(true));
            extra.insert("error".to_string(), Value::Null);
            write_exec_snapshot(&metadata, false, true, Value::from(-9), Some(extra))?;
            live_processes().lock().unwrap().remove(&exec_id);
            let mut result = json!({
                "exec_id": metadata.exec_id,
                "pid": previous
                    .as_ref()
                    .and_then(exec_pid_from_snapshot)
                    .map(Value::from)
                    .unwrap_or(Value::Null),
                "command": metadata.command,
                "remote": metadata.remote,
                "cwd": metadata.cwd,
                "running": false,
                "completed": true,
                "killed": true,
                "returncode": -9,
            });
            compact_tool_status_fields_for_model(&mut result);
            Ok(result)
        },
    )
}

#[cfg(test)]
mod tests {
    use super::direct_read_command_guidance;

    #[test]
    fn direct_read_command_guard_catches_simple_shell_reads() {
        assert!(direct_read_command_guidance("cat src/main.rs").is_some());
        assert!(direct_read_command_guidance("/usr/bin/grep foo src/main.rs").is_some());
        assert!(direct_read_command_guidance("find . -name '*.rs'").is_some());
        assert!(direct_read_command_guidance("ssh dev 'cat src/main.rs'").is_some());
        assert!(direct_read_command_guidance("cargo test").is_none());
        assert!(direct_read_command_guidance("git grep foo").is_none());
    }
}
