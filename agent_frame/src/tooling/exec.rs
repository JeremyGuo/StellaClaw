use super::args::{string_arg, usize_arg_with_default};
use super::remote::{
    ExecutionTarget, RemoteWorkpathMap, execution_target_arg, remote_schema_property,
    resolve_remote_cwd,
};
use super::runtime_state::{read_status_json, spawn_background_worker_process};
use super::{InterruptSignal, Tool, compact_tool_status_fields_for_model};
use crate::tool_worker::ToolWorkerJob;
use anyhow::{Context, Result, anyhow};
use base64::Engine;
use chrono::Local;
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

const SHELL_DEFAULT_WAIT_MS: usize = 10_000;
const SHELL_OUTPUT_TAIL_LINES: usize = 20;
const SHELL_OUTPUT_MAX_CHARS: usize = 1000;
const SHELL_SESSION_ID_MAX_LEN: usize = 128;
const DIRECT_READ_COMMANDS: &[&str] = &["cat", "grep", "find", "head", "tail", "ls"];

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

fn validate_shell_session_id(session_id: &str) -> Result<()> {
    if session_id.is_empty() {
        return Err(anyhow!("session_id must not be empty"));
    }
    if session_id.len() > SHELL_SESSION_ID_MAX_LEN {
        return Err(anyhow!(
            "session_id must be at most {} characters",
            SHELL_SESSION_ID_MAX_LEN
        ));
    }
    if !session_id
        .bytes()
        .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
    {
        return Err(anyhow!(
            "session_id may contain only ASCII letters, digits, '_' and '-'"
        ));
    }
    Ok(())
}

fn generate_shell_session_id() -> String {
    let date = Local::now().format("%Y%m%d").to_string();
    let random =
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(&Uuid::new_v4().into_bytes()[..5]);
    format!("sh_{}_{:6}", date, &random[..6])
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
        "cat" => Some("Use file_read with file_path instead of shell cat."),
        "grep" => Some("Use grep with pattern/path instead of shell grep."),
        "find" => Some("Use glob or ls instead of shell find."),
        "head" | "tail" => Some("Use file_read with offset/limit instead of shell head/tail."),
        "ls" => Some("Use ls with path instead of shell ls."),
        "ssh" => Some(
            "Manual ssh through shell is rejected. Follow the remote execution policy in the system prompt.",
        ),
        _ if DIRECT_READ_COMMANDS.contains(&head) => {
            Some("Use the dedicated filesystem/search tool instead of shell.")
        }
        _ => None,
    }
}

#[derive(Clone, Serialize, serde::Deserialize)]
pub(super) struct ShellSessionMetadata {
    pub(super) session_id: String,
    pub(super) worker_pid: u32,
    pub(super) interactive: bool,
    #[serde(default = "default_remote_local")]
    pub(super) remote: String,
    pub(super) status_path: String,
    pub(super) worker_exit_code_path: String,
    pub(super) requests_dir: String,
    pub(super) output_root: String,
    #[serde(default)]
    pub(super) delivered_process_id: Option<String>,
}

fn default_remote_local() -> String {
    "local".to_string()
}

static LIVE_SESSIONS: std::sync::OnceLock<Mutex<BTreeMap<String, ShellSessionMetadata>>> =
    std::sync::OnceLock::new();

fn live_sessions() -> &'static Mutex<BTreeMap<String, ShellSessionMetadata>> {
    LIVE_SESSIONS.get_or_init(|| Mutex::new(BTreeMap::new()))
}

pub(super) fn process_meta_path(dir: &Path, session_id: &str) -> PathBuf {
    dir.join(format!("{}.json", session_id))
}

fn read_session_metadata(dir: &Path, session_id: &str) -> Result<ShellSessionMetadata> {
    validate_shell_session_id(session_id)?;
    let path = process_meta_path(dir, session_id);
    let raw =
        fs::read_to_string(&path).with_context(|| format!("failed to read {}", path.display()))?;
    serde_json::from_str(&raw).context("failed to parse shell session metadata")
}

fn write_session_metadata(dir: &Path, metadata: &ShellSessionMetadata) -> Result<()> {
    let raw = serde_json::to_string_pretty(metadata)
        .context("failed to serialize shell session metadata")?;
    fs::write(process_meta_path(dir, &metadata.session_id), raw).with_context(|| {
        format!(
            "failed to write shell session metadata for {}",
            metadata.session_id
        )
    })
}

fn read_file_lines_window(path: &Path, limit: usize) -> Result<(String, usize, bool)> {
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
    let begin = lines.len().saturating_sub(limit);
    let line_window_truncated = begin > 0;
    Ok((
        lines[begin..].join("\n"),
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

fn format_relative_path(path: &Path) -> String {
    path.components()
        .map(|component| component.as_os_str().to_string_lossy())
        .collect::<Vec<_>>()
        .join("/")
}

fn sync_workspace_output_dir(
    workspace_root: &Path,
    session_id: &str,
    process_id: &str,
    source_root: &Path,
) -> Result<String> {
    let relative_root = PathBuf::from(".agent_frame")
        .join("shell")
        .join(session_id)
        .join(process_id);
    let destination_root = workspace_root.join(&relative_root);
    fs::create_dir_all(&destination_root)
        .with_context(|| format!("failed to create {}", destination_root.display()))?;
    for name in ["stdout", "stderr"] {
        let source = source_root.join(name);
        let destination = destination_root.join(name);
        if source.exists() {
            if source != destination {
                fs::copy(&source, &destination).with_context(|| {
                    format!(
                        "failed to copy {} to {}",
                        source.display(),
                        destination.display()
                    )
                })?;
            }
        } else {
            fs::write(&destination, b"")
                .with_context(|| format!("failed to write {}", destination.display()))?;
        }
    }
    Ok(format_relative_path(&relative_root))
}

pub(super) fn read_exit_code(path: &Path) -> Option<i32> {
    fs::read_to_string(path)
        .ok()
        .and_then(|value| value.trim().parse::<i32>().ok())
}

pub(super) fn record_exit_code(path: &Path, code: i32) -> Result<()> {
    fs::write(path, code.to_string())
        .with_context(|| format!("failed to write exit code to {}", path.display()))
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

fn shell_request_path(requests_dir: &Path, request_id: &str) -> PathBuf {
    requests_dir.join(format!("request-{request_id}.json"))
}

fn shell_request_result_path(requests_dir: &Path, request_id: &str) -> PathBuf {
    requests_dir.join(format!("request-{request_id}.result.json"))
}

fn queue_shell_request(metadata: &ShellSessionMetadata, payload: &Value) -> Result<PathBuf> {
    let requests_dir = Path::new(&metadata.requests_dir);
    fs::create_dir_all(requests_dir)
        .with_context(|| format!("failed to create {}", requests_dir.display()))?;
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    let request_id = format!("{timestamp:020}-{}", Uuid::new_v4());
    let request_path = shell_request_path(requests_dir, &request_id);
    let result_path = shell_request_result_path(requests_dir, &request_id);
    let temp_path = requests_dir.join(format!("request-{request_id}.tmp"));
    fs::write(
        &temp_path,
        serde_json::to_vec_pretty(payload).context("failed to serialize shell request")?,
    )
    .with_context(|| format!("failed to write {}", temp_path.display()))?;
    fs::rename(&temp_path, &request_path).with_context(|| {
        format!(
            "failed to move {} to {}",
            temp_path.display(),
            request_path.display()
        )
    })?;
    Ok(result_path)
}

fn read_shell_request_result(result_path: &Path) -> Result<Option<Value>> {
    if !result_path.exists() {
        return Ok(None);
    }
    let raw = fs::read_to_string(result_path)
        .with_context(|| format!("failed to read {}", result_path.display()))?;
    let value = serde_json::from_str(&raw).context("failed to parse shell request result")?;
    Ok(Some(value))
}

fn wait_for_shell_request_ack(
    result_path: &Path,
    wait_timeout_ms: usize,
    cancel_flag: Option<&Arc<InterruptSignal>>,
) -> Result<Value> {
    let deadline = std::time::Instant::now() + Duration::from_millis(wait_timeout_ms as u64);
    loop {
        if let Some(result) = read_shell_request_result(result_path)? {
            let _ = fs::remove_file(result_path);
            return Ok(result);
        }
        if cancel_flag.is_some_and(|signal| signal.is_requested()) {
            return Err(anyhow!("operation cancelled"));
        }
        if std::time::Instant::now() >= deadline {
            return Err(anyhow!("shell request acknowledgement timed out"));
        }
        thread::sleep(Duration::from_millis(25));
    }
}

fn session_missing_error(session_id: &str) -> anyhow::Error {
    anyhow!(
        "shell session {} no longer exists; it may have been closed or terminated when the main runtime shut down",
        session_id
    )
}

fn read_session_status(metadata: &ShellSessionMetadata) -> Result<Value> {
    read_status_json(Path::new(&metadata.status_path))
        .map_err(|_| session_missing_error(&metadata.session_id))
}

fn current_process_id(status: &Value) -> Option<&str> {
    status.get("process_id").and_then(Value::as_str)
}

fn current_process_output_root(status: &Value) -> Option<PathBuf> {
    let stdout = status.get("stdout_path").and_then(Value::as_str)?;
    Path::new(stdout).parent().map(Path::to_path_buf)
}

fn build_current_process_fields(
    object: &mut Map<String, Value>,
    status: &Value,
    workspace_root: &Path,
    session_id: &str,
) -> Result<()> {
    let Some(process_id) = current_process_id(status) else {
        return Ok(());
    };
    let Some(output_root) = current_process_output_root(status) else {
        return Ok(());
    };
    let stdout_path = output_root.join("stdout");
    let stderr_path = output_root.join("stderr");
    let (stdout_window, _, stdout_line_truncated) =
        read_file_lines_window(&stdout_path, SHELL_OUTPUT_TAIL_LINES)?;
    let (stderr_window, _, stderr_line_truncated) =
        read_file_lines_window(&stderr_path, SHELL_OUTPUT_TAIL_LINES)?;
    let (stdout, stdout_truncated) = truncate_exec_output(&stdout_window, SHELL_OUTPUT_MAX_CHARS);
    let (stderr, stderr_truncated) = truncate_exec_output(&stderr_window, SHELL_OUTPUT_MAX_CHARS);
    object.insert(
        "process_id".to_string(),
        Value::String(process_id.to_string()),
    );
    object.insert("stdout".to_string(), Value::String(stdout));
    object.insert("stderr".to_string(), Value::String(stderr));
    if stdout_truncated || stdout_line_truncated {
        object.insert("stdout_truncated".to_string(), Value::Bool(true));
    }
    if stderr_truncated || stderr_line_truncated {
        object.insert("stderr_truncated".to_string(), Value::Bool(true));
    }
    let out_path = sync_workspace_output_dir(workspace_root, session_id, process_id, &output_root)?;
    object.insert("out_path".to_string(), Value::String(out_path));
    Ok(())
}

fn format_shell_response(
    metadata: &ShellSessionMetadata,
    status: &Value,
    workspace_root: &Path,
    include_finished_process: bool,
) -> Result<Value> {
    let running = status
        .get("running")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let interactive = status
        .get("interactive")
        .and_then(Value::as_bool)
        .unwrap_or(metadata.interactive);
    let mut object = Map::from_iter([
        (
            "session_id".to_string(),
            Value::String(metadata.session_id.clone()),
        ),
        ("running".to_string(), Value::Bool(running)),
        ("interactive".to_string(), Value::Bool(interactive)),
    ]);
    let include_process = running || include_finished_process;
    if include_process {
        build_current_process_fields(&mut object, status, workspace_root, &metadata.session_id)?;
    }
    if !running
        && include_finished_process
        && let Some(exit_code) = status.get("exit_code").and_then(Value::as_i64)
    {
        object.insert("exit_code".to_string(), Value::from(exit_code));
    }
    if status
        .get("needs_input")
        .and_then(Value::as_bool)
        .unwrap_or(false)
    {
        object.insert("needs_input".to_string(), Value::Bool(true));
    }
    if let Some(error) = status.get("error").and_then(Value::as_str)
        && !error.trim().is_empty()
    {
        object.insert("error".to_string(), Value::String(error.to_string()));
    }
    Ok(Value::Object(object))
}

fn spawn_shell_session(
    runtime_state_root: &Path,
    state_dir: &Path,
    workspace_root: &Path,
    interactive: bool,
    target: &ExecutionTarget,
    initial_cwd: &Path,
) -> Result<ShellSessionMetadata> {
    let session_id = generate_shell_session_id();
    validate_shell_session_id(&session_id)?;
    let output_root = workspace_root
        .join(".agent_frame")
        .join("shell")
        .join(&session_id);
    fs::create_dir_all(&output_root)
        .with_context(|| format!("failed to create {}", output_root.display()))?;
    let status_path = state_dir.join(format!("{}.status.json", session_id));
    let requests_dir = state_dir.join(format!("{}.requests", session_id));
    fs::create_dir_all(&requests_dir)
        .with_context(|| format!("failed to create {}", requests_dir.display()))?;
    fs::write(
        &status_path,
        serde_json::to_vec_pretty(&json!({
            "session_id": session_id,
            "interactive": interactive,
            "remote": target.remote_name(),
            "pid": Value::Null,
            "running": false,
            "process_id": Value::Null,
            "exit_code": Value::Null,
            "command": Value::Null,
            "stdout_path": Value::Null,
            "stderr_path": Value::Null,
            "error": Value::Null,
        }))
        .context("failed to serialize initial shell session status")?,
    )
    .with_context(|| format!("failed to write {}", status_path.display()))?;
    let worker = spawn_background_worker_process(
        runtime_state_root,
        "shell-session",
        &session_id,
        &ToolWorkerJob::ShellSession {
            session_id: session_id.clone(),
            interactive,
            remote: match target {
                ExecutionTarget::Local => None,
                ExecutionTarget::RemoteSsh { host } => Some(host.clone()),
            },
            initial_cwd: initial_cwd.display().to_string(),
            status_path: status_path.display().to_string(),
            requests_dir: requests_dir.display().to_string(),
            output_root: output_root.display().to_string(),
        },
    )?;
    let metadata = ShellSessionMetadata {
        session_id: session_id.clone(),
        worker_pid: worker.pid,
        interactive,
        remote: target.remote_name().to_string(),
        status_path: status_path.display().to_string(),
        worker_exit_code_path: worker.exit_code_path,
        requests_dir: requests_dir.display().to_string(),
        output_root: output_root.display().to_string(),
        delivered_process_id: None,
    };
    write_session_metadata(state_dir, &metadata)?;
    live_sessions()
        .lock()
        .unwrap()
        .insert(session_id, metadata.clone());
    Ok(metadata)
}

fn current_process_was_delivered(metadata: &ShellSessionMetadata, status: &Value) -> bool {
    metadata.delivered_process_id.as_deref() == current_process_id(status)
}

fn mark_process_delivered(
    state_dir: &Path,
    metadata: &mut ShellSessionMetadata,
    process_id: &str,
) -> Result<()> {
    metadata.delivered_process_id = Some(process_id.to_string());
    write_session_metadata(state_dir, metadata)
}

fn wait_for_shell_session(
    state_dir: &Path,
    metadata: &mut ShellSessionMetadata,
    wait_ms: usize,
    workspace_root: &Path,
    cancel_flag: Option<&Arc<InterruptSignal>>,
    allow_finished_result: bool,
) -> Result<Value> {
    let deadline = std::time::Instant::now() + Duration::from_millis(wait_ms as u64);
    loop {
        let status = read_session_status(metadata)?;
        let running = status
            .get("running")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        if !running {
            let finished_undelivered = current_process_id(&status).is_some()
                && !current_process_was_delivered(metadata, &status);
            if allow_finished_result && finished_undelivered {
                let process_id = current_process_id(&status).unwrap();
                let response = format_shell_response(metadata, &status, workspace_root, true)?;
                mark_process_delivered(state_dir, metadata, process_id)?;
                return Ok(response);
            }
            let response = format_shell_response(metadata, &status, workspace_root, false)?;
            return Ok(response);
        }
        if wait_ms == 0 {
            let response = format_shell_response(metadata, &status, workspace_root, true)?;
            return Ok(response);
        }
        if cancel_flag.is_some_and(|signal| signal.is_requested()) {
            let response = format_shell_response(metadata, &status, workspace_root, true)?;
            return Ok(response);
        }
        if std::time::Instant::now() >= deadline {
            let response = format_shell_response(metadata, &status, workspace_root, true)?;
            return Ok(response);
        }
        thread::sleep(Duration::from_millis(100));
    }
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

fn kill_shell_session_processes(metadata: &ShellSessionMetadata, status: Option<&Value>) {
    if let Some(shell_pid) = status
        .and_then(|value| value.get("pid"))
        .and_then(Value::as_u64)
        .and_then(|pid| u32::try_from(pid).ok())
    {
        terminate_process_pid(shell_pid);
    }
    terminate_process_pid(metadata.worker_pid);
}

pub(super) fn cleanup_exec_processes(runtime_state_root: &Path) -> Result<usize> {
    let Some(state_dir) = process_state_dir_if_exists(runtime_state_root) else {
        return Ok(0);
    };
    let mut killed = 0usize;
    for path in iter_process_metadata_json_files(&state_dir)? {
        let raw = fs::read_to_string(&path)
            .with_context(|| format!("failed to read {}", path.display()))?;
        let Ok(metadata) = serde_json::from_str::<ShellSessionMetadata>(&raw) else {
            continue;
        };
        let status = read_session_status(&metadata).ok();
        live_sessions().lock().unwrap().remove(&metadata.session_id);
        if read_exit_code(Path::new(&metadata.worker_exit_code_path)).is_none()
            || process_is_running(metadata.worker_pid)
        {
            kill_shell_session_processes(&metadata, status.as_ref());
            killed = killed.saturating_add(1);
        }
    }
    Ok(killed)
}

pub(super) fn list_active_exec_summaries(runtime_state_root: &Path) -> Result<Vec<String>> {
    let Some(state_dir) = process_state_dir_if_exists(runtime_state_root) else {
        return Ok(Vec::new());
    };
    let mut entries = Vec::new();
    for path in iter_process_metadata_json_files(&state_dir)? {
        let raw = fs::read_to_string(&path)
            .with_context(|| format!("failed to read {}", path.display()))?;
        let Ok(metadata) = serde_json::from_str::<ShellSessionMetadata>(&raw) else {
            continue;
        };
        let Ok(status) = read_session_status(&metadata) else {
            continue;
        };
        if !status
            .get("running")
            .and_then(Value::as_bool)
            .unwrap_or(false)
        {
            continue;
        }
        let pid = status
            .get("pid")
            .and_then(Value::as_u64)
            .unwrap_or(metadata.worker_pid as u64);
        let process_id = current_process_id(&status).unwrap_or("-");
        let command = status.get("command").and_then(Value::as_str).unwrap_or("-");
        entries.push(format!(
            "- shell session_id=`{}` process_id=`{}` pid={} remote=`{}` interactive={} command=`{}`",
            metadata.session_id, process_id, pid, metadata.remote, metadata.interactive, command
        ));
    }
    entries.sort();
    Ok(entries)
}

pub fn terminate_all_managed_processes() -> Result<()> {
    let mut registry = live_sessions().lock().unwrap();
    let sessions = std::mem::take(&mut *registry)
        .into_values()
        .collect::<Vec<_>>();
    drop(registry);
    for session in sessions {
        let status = read_session_status(&session).ok();
        let state_dir = Path::new(&session.status_path)
            .parent()
            .unwrap_or_else(|| Path::new("."));
        let _ = fs::remove_file(process_meta_path(state_dir, &session.session_id));
        let _ = fs::remove_file(&session.worker_exit_code_path);
        let _ = fs::remove_file(&session.status_path);
        let _ = fs::remove_dir_all(&session.requests_dir);
        kill_shell_session_processes(&session, status.as_ref());
    }
    Ok(())
}

fn shell_wait_ms_arg(arguments: &Map<String, Value>) -> Result<usize> {
    let wait_ms = usize_arg_with_default(arguments, "wait_ms", SHELL_DEFAULT_WAIT_MS)?;
    Ok(wait_ms)
}

fn optional_command_arg(arguments: &Map<String, Value>) -> Result<Option<String>> {
    let Some(value) = arguments.get("command") else {
        return Ok(None);
    };
    let command = value
        .as_str()
        .ok_or_else(|| anyhow!("argument command must be a string"))?
        .trim()
        .to_string();
    if command.is_empty() {
        return Ok(None);
    }
    Ok(Some(command))
}

fn current_status_running(status: &Value) -> bool {
    status
        .get("running")
        .and_then(Value::as_bool)
        .unwrap_or(false)
}

fn ensure_remote_argument_allowed(
    arguments: &Map<String, Value>,
    enable_remote_tools: bool,
) -> Result<()> {
    if !enable_remote_tools && arguments.get("remote").is_some() {
        return Err(anyhow!(
            "argument remote is disabled in the current execution mode"
        ));
    }
    Ok(())
}

pub(super) fn shell_tool(
    workspace_root: PathBuf,
    runtime_state_root: PathBuf,
    remote_workpaths: RemoteWorkpathMap,
    enable_remote_tools: bool,
    cancel_flag: Option<Arc<InterruptSignal>>,
) -> Tool {
    let mut properties = Map::new();
    properties.insert("session_id".to_string(), json!({"type": "string"}));
    properties.insert("command".to_string(), json!({"type": "string"}));
    properties.insert("input".to_string(), json!({"type": "string"}));
    properties.insert("interactive".to_string(), json!({"type": "boolean"}));
    properties.insert("wait_ms".to_string(), json!({"type": "integer"}));
    if enable_remote_tools {
        properties.insert("remote".to_string(), remote_schema_property());
    }
    Tool::new_interruptible(
        "shell",
        "Run or continue a persistent shell session. Pass command to start the next command in a session. Pass no command to only observe or collect the current command result. Pass input to write to the current interactive command. command=\"\" is treated the same as omitting command. session_id only reuses an existing session; omit it when creating a new one. If a finished command result has not been returned yet and you start a new command in the same session, the older unreturned result is discarded. Returned stdout/stderr describe only the current process. Full stdout/stderr are saved under out_path/stdout and out_path/stderr.",
        Value::Object(
            [
                ("type".to_string(), Value::String("object".to_string())),
                ("properties".to_string(), Value::Object(properties)),
                ("additionalProperties".to_string(), Value::Bool(false)),
            ]
            .into_iter()
            .collect(),
        ),
        move |arguments| {
            let arguments = arguments
                .as_object()
                .ok_or_else(|| anyhow!("tool arguments must be an object"))?;
            ensure_remote_argument_allowed(arguments, enable_remote_tools)?;
            let command = optional_command_arg(arguments)?;
            if let Some(command) = command.as_deref()
                && let Some(guidance) = direct_read_command_guidance(command)
            {
                return Err(anyhow!(
                    "direct read/search shell command rejected by tool policy: {guidance}"
                ));
            }
            let input = arguments
                .get("input")
                .and_then(Value::as_str)
                .map(ToOwned::to_owned);
            if command.is_some() && input.is_some() {
                return Err(anyhow!(
                    "arguments command and input are mutually exclusive"
                ));
            }
            let wait_ms = shell_wait_ms_arg(arguments)?;
            let state_dir = ensure_process_state_dir(&runtime_state_root)?;
            let session_id = arguments
                .get("session_id")
                .and_then(Value::as_str)
                .map(ToOwned::to_owned);
            if let Some(session_id) = session_id.as_deref() {
                validate_shell_session_id(session_id)?;
            }

            let mut metadata = if let Some(session_id) = session_id {
                if process_meta_path(&state_dir, &session_id).exists() {
                    let metadata = read_session_metadata(&state_dir, &session_id)
                        .map_err(|_| session_missing_error(&session_id))?;
                    if arguments.get("interactive").is_some() {
                        return Err(anyhow!(
                            "argument interactive is only allowed when creating a new shell session"
                        ));
                    }
                    if arguments.get("remote").is_some() {
                        return Err(anyhow!(
                            "argument remote is only allowed when creating a new shell session"
                        ));
                    }
                    metadata
                } else {
                    return Err(session_missing_error(&session_id));
                }
            } else {
                let Some(command) = command.as_deref() else {
                    return Err(anyhow!(
                        "command is required when creating a new shell session"
                    ));
                };
                let _ = command;
                let target = execution_target_arg(arguments)?;
                let interactive = arguments
                    .get("interactive")
                    .and_then(Value::as_bool)
                    .unwrap_or(false);
                let initial_cwd = match &target {
                    ExecutionTarget::Local => workspace_root.clone(),
                    ExecutionTarget::RemoteSsh { host } => {
                        PathBuf::from(resolve_remote_cwd(host, None, &remote_workpaths)?)
                    }
                };
                spawn_shell_session(
                    &runtime_state_root,
                    &state_dir,
                    &workspace_root,
                    interactive,
                    &target,
                    &initial_cwd,
                )?
            };

            if let Some(input) = input {
                let result_path = queue_shell_request(
                    &metadata,
                    &json!({
                        "action": "input",
                        "input": input,
                    }),
                )?;
                let ack = wait_for_shell_request_ack(
                    &result_path,
                    wait_ms.max(500),
                    cancel_flag.as_ref(),
                )?;
                if !ack.get("ok").and_then(Value::as_bool).unwrap_or(false) {
                    let error = ack
                        .get("error")
                        .and_then(Value::as_str)
                        .unwrap_or("shell input failed");
                    return Err(anyhow!(error.to_string()));
                }
                return wait_for_shell_session(
                    &state_dir,
                    &mut metadata,
                    wait_ms,
                    &workspace_root,
                    cancel_flag.as_ref(),
                    true,
                );
            }

            if let Some(command) = command {
                let status = read_session_status(&metadata)?;
                if current_status_running(&status) {
                    return Err(anyhow!(
                        "shell session {} already has a running command; call shell without command to observe it first",
                        metadata.session_id
                    ));
                }
                if let Some(process_id) = current_process_id(&status)
                    && !current_process_was_delivered(&metadata, &status)
                {
                    mark_process_delivered(&state_dir, &mut metadata, process_id)?;
                }
                let process_id = Uuid::new_v4().to_string();
                let result_path = queue_shell_request(
                    &metadata,
                    &json!({
                        "action": "run",
                        "process_id": process_id,
                        "command": command,
                    }),
                )?;
                let ack = wait_for_shell_request_ack(
                    &result_path,
                    wait_ms.max(500),
                    cancel_flag.as_ref(),
                )?;
                if !ack.get("ok").and_then(Value::as_bool).unwrap_or(false) {
                    let error = ack
                        .get("error")
                        .and_then(Value::as_str)
                        .unwrap_or("shell command failed to start");
                    return Err(anyhow!(error.to_string()));
                }
                metadata.delivered_process_id = None;
                write_session_metadata(&state_dir, &metadata)?;
                return wait_for_shell_session(
                    &state_dir,
                    &mut metadata,
                    wait_ms,
                    &workspace_root,
                    cancel_flag.as_ref(),
                    true,
                );
            }

            wait_for_shell_session(
                &state_dir,
                &mut metadata,
                wait_ms,
                &workspace_root,
                cancel_flag.as_ref(),
                true,
            )
        },
    )
}

pub(super) fn shell_close_tool(
    runtime_state_root: PathBuf,
    _cancel_flag: Option<Arc<InterruptSignal>>,
) -> Tool {
    Tool::new(
        "shell_close",
        "Close a shell session and stop its background worker. If the session has a running command, that command is stopped too.",
        json!({
            "type": "object",
            "properties": {
                "session_id": {"type": "string"}
            },
            "required": ["session_id"],
            "additionalProperties": false
        }),
        move |arguments| {
            let arguments = arguments
                .as_object()
                .ok_or_else(|| anyhow!("tool arguments must be an object"))?;
            let session_id = string_arg(arguments, "session_id")?;
            let state_dir = ensure_process_state_dir(&runtime_state_root)?;
            let metadata = read_session_metadata(&state_dir, &session_id)
                .map_err(|_| session_missing_error(&session_id))?;
            let status = read_session_status(&metadata).ok();
            let killed_running_process = status
                .as_ref()
                .and_then(|value| value.get("running"))
                .and_then(Value::as_bool)
                .unwrap_or(false);
            let close_result_path = queue_shell_request(&metadata, &json!({"action": "close"}))?;
            let _ = wait_for_shell_request_ack(&close_result_path, 500, None);
            kill_shell_session_processes(&metadata, status.as_ref());
            let _ = fs::remove_file(process_meta_path(&state_dir, &metadata.session_id));
            let _ = fs::remove_file(&metadata.worker_exit_code_path);
            let _ = fs::remove_file(&metadata.status_path);
            let _ = fs::remove_dir_all(&metadata.requests_dir);
            live_sessions().lock().unwrap().remove(&metadata.session_id);
            let mut result = json!({
                "session_id": metadata.session_id,
                "closed": true,
                "killed_running_process": killed_running_process,
            });
            compact_tool_status_fields_for_model(&mut result);
            Ok(result)
        },
    )
}

#[cfg(test)]
mod tests {
    use super::{
        direct_read_command_guidance, generate_shell_session_id, validate_shell_session_id,
    };

    #[test]
    fn direct_read_command_guard_catches_simple_shell_reads() {
        assert!(direct_read_command_guidance("cat README.md").is_some());
        assert!(direct_read_command_guidance("grep -n foo src/lib.rs").is_some());
        assert!(direct_read_command_guidance("ls src").is_some());
        assert!(direct_read_command_guidance("ssh dev uptime").is_some());
        assert!(direct_read_command_guidance("printf 123").is_none());
    }

    #[test]
    fn shell_session_id_validation_accepts_safe_names() {
        assert!(validate_shell_session_id("shell_123-test").is_ok());
    }

    #[test]
    fn shell_session_id_validation_rejects_special_characters() {
        assert!(validate_shell_session_id("../oops").is_err());
        assert!(validate_shell_session_id("bad space").is_err());
    }

    #[test]
    fn generated_shell_session_id_uses_short_dated_format() {
        let session_id = generate_shell_session_id();
        assert!(session_id.starts_with("sh_"));
        assert_eq!(session_id.len(), 18, "{session_id}");
        assert_eq!(&session_id[11..12], "_");
        assert!(
            validate_shell_session_id(&session_id).is_ok(),
            "{session_id}"
        );
        assert!(session_id[3..11].bytes().all(|byte| byte.is_ascii_digit()));
    }
}
