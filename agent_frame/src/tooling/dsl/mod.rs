use super::args::{f64_arg, string_arg, usize_arg_with_default};
use super::download::file_download_cancel_tool;
use super::exec::{record_exit_code, terminate_process_pid};
use super::media::image_cancel_tool;
use super::runtime_state::{
    BackgroundTaskMetadata, background_task_dir, background_task_is_running,
    read_background_task_metadata, read_status_json, spawn_background_worker_process,
    write_background_task_metadata,
};
use super::{
    InterruptSignal, Tool, build_tool_registry_with_cancel, exec_kill_tool, execute_tool_call,
};
use crate::config::{RemoteWorkpathConfig, UpstreamConfig};
use crate::llm::create_chat_completion;
use crate::message::ChatMessage;
use crate::tool_worker::ToolWorkerJob;
use anyhow::{Context, Result, anyhow};
use crossbeam_channel as channel;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use std::collections::BTreeMap;
use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use uuid::Uuid;

const DSL_WORKER_SOURCE: &str = include_str!("worker.py");
const DSL_DEFAULT_WAIT_TIMEOUT_SECONDS: f64 = 270.0;
const DSL_OUTPUT_MAX_CHARS: usize = 1000;
const DSL_DEFAULT_MAX_RUNTIME_SECONDS: u64 = 600;
const DSL_DEFAULT_MAX_LLM_CALLS: u64 = 20;
const DSL_DEFAULT_MAX_TOOL_CALLS: u64 = 50;
const DSL_DEFAULT_MAX_EMIT_CALLS: u64 = 20;
const DSL_MAX_CODE_CHARS: usize = 20_000;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum DslTimeoutAction {
    Continue,
    Kill,
}

impl DslTimeoutAction {
    fn as_str(self) -> &'static str {
        match self {
            DslTimeoutAction::Continue => "continue",
            DslTimeoutAction::Kill => "kill",
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct DslMetadata {
    dsl_id: String,
    worker_pid: u32,
    label: Option<String>,
    code_path: String,
    status_path: String,
    stdout_path: String,
    stderr_path: String,
    result_path: String,
    worker_exit_code_path: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct DslChildTask {
    tool: String,
    id_field: String,
    id: String,
    status: String,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
struct DslUsage {
    llm_calls: u64,
    tool_calls: u64,
    emit_calls: u64,
    runtime_seconds: f64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct DslLimits {
    max_llm_calls: u64,
    max_tool_calls: u64,
    max_emit_calls: u64,
    max_runtime_seconds: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct DslStatus {
    dsl_id: String,
    status: String,
    label: Option<String>,
    started_at: String,
    finished_at: Option<String>,
    summary: String,
    diagnostics: Vec<Value>,
    usage: DslUsage,
    limits: DslLimits,
    children: Vec<DslChildTask>,
}

struct DslRuntime {
    upstream: UpstreamConfig,
    available_upstreams: BTreeMap<String, UpstreamConfig>,
    workspace_root: PathBuf,
    runtime_state_root: PathBuf,
    remote_workpaths: Vec<RemoteWorkpathConfig>,
    status_path: PathBuf,
    result_path: PathBuf,
    worker_script_path: PathBuf,
    started: Instant,
    status: DslStatus,
}

fn now_timestamp_string() -> String {
    match SystemTime::now().duration_since(UNIX_EPOCH) {
        Ok(duration) => duration.as_secs().to_string(),
        Err(_) => "0".to_string(),
    }
}

fn diagnostic(level: &str, message: impl Into<String>) -> Value {
    json!({
        "level": level,
        "message": message.into(),
    })
}

fn dsl_timeout_action_arg(
    arguments: &Map<String, Value>,
    key: &str,
    default: DslTimeoutAction,
) -> Result<DslTimeoutAction> {
    let Some(value) = arguments.get(key) else {
        return Ok(default);
    };
    let text = value
        .as_str()
        .ok_or_else(|| anyhow!("argument {} must be a string", key))?
        .trim()
        .to_ascii_lowercase();
    match text.as_str() {
        "continue" => Ok(DslTimeoutAction::Continue),
        "kill" => Ok(DslTimeoutAction::Kill),
        _ => Err(anyhow!("argument {} must be one of: continue, kill", key)),
    }
}

fn max_output_chars_arg(arguments: &Map<String, Value>) -> Result<usize> {
    let value = usize_arg_with_default(arguments, "max_output_chars", DSL_OUTPUT_MAX_CHARS)?;
    if value > DSL_OUTPUT_MAX_CHARS {
        return Err(anyhow!(
            "argument max_output_chars must be less than or equal to {}",
            DSL_OUTPUT_MAX_CHARS
        ));
    }
    Ok(value)
}

fn optional_u64_arg(arguments: &Map<String, Value>, key: &str, default: u64) -> Result<u64> {
    let Some(value) = arguments.get(key) else {
        return Ok(default);
    };
    let Some(number) = value.as_u64() else {
        return Err(anyhow!("argument {} must be a non-negative integer", key));
    };
    Ok(number)
}

fn truncate_output(text: &str, max_chars: usize) -> (String, bool) {
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

fn workspace_dsl_relative_path(dsl_id: &str, stream: &str) -> PathBuf {
    PathBuf::from(".agent_frame")
        .join("dsl")
        .join(format!("{dsl_id}.{stream}"))
}

fn sync_workspace_file(
    workspace_root: &Path,
    dsl_id: &str,
    source: &Path,
    stream: &str,
) -> Result<String> {
    let relative_path = workspace_dsl_relative_path(dsl_id, stream);
    let destination = workspace_root.join(&relative_path);
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
    Ok(format_relative_path(&relative_path))
}

fn read_lossy(path: &Path) -> Result<String> {
    if !path.exists() {
        return Ok(String::new());
    }
    let raw = fs::read(path).with_context(|| format!("failed to read {}", path.display()))?;
    Ok(String::from_utf8_lossy(&raw).to_string())
}

fn dsl_result_text(result_full: &str, stdout_full: &str) -> String {
    let trimmed = result_full.trim();
    let parsed = if trimmed.is_empty() {
        Value::Null
    } else {
        serde_json::from_str::<Value>(trimmed)
            .unwrap_or_else(|_| Value::String(trimmed.to_string()))
    };
    match parsed {
        Value::Null => stdout_full.trim().to_string(),
        Value::String(text) => text,
        Value::Bool(value) => value.to_string(),
        Value::Number(value) => value.to_string(),
        Value::Object(object) => {
            if let Some(error) = object.get("error").and_then(Value::as_str) {
                error.to_string()
            } else {
                serde_json::to_string_pretty(&Value::Object(object)).unwrap_or_default()
            }
        }
        other => serde_json::to_string_pretty(&other).unwrap_or_else(|_| other.to_string()),
    }
}

fn dsl_state_dir(runtime_state_root: &Path) -> Result<PathBuf> {
    background_task_dir(runtime_state_root, "dsl")
}

fn dsl_status_path(dir: &Path, dsl_id: &str) -> PathBuf {
    dir.join(format!("{dsl_id}.status.json"))
}

fn dsl_result_path(dir: &Path, dsl_id: &str) -> PathBuf {
    dir.join(format!("{dsl_id}.result.json"))
}

fn dsl_code_path(dir: &Path, dsl_id: &str) -> PathBuf {
    dir.join(format!("{dsl_id}.py"))
}

fn dsl_worker_script_path(dir: &Path, dsl_id: &str) -> PathBuf {
    dir.join(format!("{dsl_id}.worker.py"))
}

fn is_dsl_metadata_file_name(file_name: &str) -> bool {
    file_name.ends_with(".json")
        && !file_name.ends_with(".status.json")
        && !file_name.ends_with(".result.json")
}

fn write_dsl_status(path: &Path, status: &DslStatus) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }
    fs::write(
        path,
        serde_json::to_vec_pretty(status).context("failed to serialize DSL status")?,
    )
    .with_context(|| format!("failed to write {}", path.display()))
}

fn read_dsl_metadata(runtime_state_root: &Path, dsl_id: &str) -> Result<DslMetadata> {
    let dir = dsl_state_dir(runtime_state_root)?;
    let metadata: BackgroundTaskMetadata = read_background_task_metadata(&dir, dsl_id)?;
    let status_path = Path::new(&metadata.status_path);
    let status = read_status_json(status_path).unwrap_or_else(|_| json!({}));
    Ok(DslMetadata {
        dsl_id: metadata.task_id,
        worker_pid: metadata.pid,
        label: status
            .get("label")
            .and_then(Value::as_str)
            .map(ToOwned::to_owned),
        code_path: dir.join(format!("{dsl_id}.py")).display().to_string(),
        status_path: metadata.status_path,
        stdout_path: metadata.stdout_path,
        stderr_path: metadata.stderr_path,
        result_path: dsl_result_path(&dir, dsl_id).display().to_string(),
        worker_exit_code_path: metadata.exit_code_path,
    })
}

fn render_dsl_snapshot(
    metadata: &DslMetadata,
    workspace_root: &Path,
    max_output_chars: usize,
    interrupted: bool,
    timed_out: bool,
) -> Result<Value> {
    let status = read_status_json(Path::new(&metadata.status_path)).unwrap_or_else(|_| {
        json!({
            "dsl_id": metadata.dsl_id,
            "status": "running",
            "summary": "DSL status is not available yet.",
            "diagnostics": [],
            "usage": {},
            "limits": {},
            "children": [],
        })
    });

    let stdout_full = read_lossy(Path::new(&metadata.stdout_path))?;
    let stderr_full = read_lossy(Path::new(&metadata.stderr_path))?;
    let result_full = read_lossy(Path::new(&metadata.result_path))?;
    let result_display = dsl_result_text(&result_full, &stdout_full);
    let (stdout, stdout_truncated) = truncate_output(&stdout_full, max_output_chars);
    let (stderr, stderr_truncated) = truncate_output(&stderr_full, max_output_chars);
    let (result_text, result_truncated) = truncate_output(&result_display, max_output_chars);

    let mut object = Map::new();
    object.insert("dsl_id".to_string(), Value::String(metadata.dsl_id.clone()));
    object.insert(
        "status".to_string(),
        status
            .get("status")
            .cloned()
            .unwrap_or_else(|| Value::String("unknown".to_string())),
    );
    if let Some(label) = status.get("label").filter(|value| !value.is_null()) {
        object.insert("label".to_string(), label.clone());
    }
    if let Some(summary) = status
        .get("summary")
        .filter(|value| value.as_str().is_some_and(|text| !text.trim().is_empty()))
    {
        object.insert("summary".to_string(), summary.clone());
    }
    if !result_text.is_empty() {
        object.insert("result_text".to_string(), Value::String(result_text));
    }
    if let Some(usage) = status.get("usage") {
        object.insert("usage".to_string(), usage.clone());
    }
    if let Some(diagnostics) = status.get("diagnostics").and_then(Value::as_array)
        && !diagnostics.is_empty()
    {
        object.insert(
            "diagnostics".to_string(),
            Value::Array(diagnostics.to_vec()),
        );
    }
    if let Some(children) = status.get("children").and_then(Value::as_array)
        && !children.is_empty()
    {
        object.insert("children".to_string(), Value::Array(children.to_vec()));
    }
    if interrupted {
        object.insert("interrupted".to_string(), Value::Bool(true));
    }
    if timed_out {
        object.insert("timed_out".to_string(), Value::Bool(true));
    }
    if result_truncated {
        let result_path = sync_workspace_file(
            workspace_root,
            &metadata.dsl_id,
            Path::new(&metadata.result_path),
            "result.json",
        )?;
        object.insert("result_truncated".to_string(), Value::Bool(true));
        object.insert("result_path".to_string(), Value::String(result_path));
    }
    if stderr_truncated || !stderr.trim().is_empty() {
        object.insert("stderr".to_string(), Value::String(stderr));
        if stderr_truncated {
            let stderr_path = sync_workspace_file(
                workspace_root,
                &metadata.dsl_id,
                Path::new(&metadata.stderr_path),
                "stderr",
            )?;
            object.insert("stderr_truncated".to_string(), Value::Bool(true));
            object.insert("stderr_path".to_string(), Value::String(stderr_path));
        }
    }
    if stdout_truncated || (stdout.trim() != result_display.trim() && !stdout.trim().is_empty()) {
        object.insert("stdout".to_string(), Value::String(stdout));
        if stdout_truncated {
            let stdout_path = sync_workspace_file(
                workspace_root,
                &metadata.dsl_id,
                Path::new(&metadata.stdout_path),
                "stdout",
            )?;
            object.insert("stdout_truncated".to_string(), Value::Bool(true));
            object.insert("stdout_path".to_string(), Value::String(stdout_path));
        }
    }
    Ok(Value::Object(object))
}

pub(super) fn list_active_dsl_summaries(runtime_state_root: &Path) -> Result<Vec<String>> {
    let Ok(dir) = dsl_state_dir(runtime_state_root) else {
        return Ok(Vec::new());
    };
    let mut entries = Vec::new();
    for entry in fs::read_dir(&dir).with_context(|| format!("failed to read {}", dir.display()))? {
        let entry = entry?;
        let path = entry.path();
        let Some(file_name) = path.file_name().and_then(|value| value.to_str()) else {
            continue;
        };
        if !is_dsl_metadata_file_name(file_name) {
            continue;
        }
        let Some(dsl_id) = path.file_stem().and_then(|value| value.to_str()) else {
            continue;
        };
        let metadata: BackgroundTaskMetadata =
            serde_json::from_slice(&fs::read(&path)?).context("failed to parse DSL metadata")?;
        if !background_task_is_running(&metadata) {
            continue;
        }
        let status =
            read_status_json(Path::new(&metadata.status_path)).unwrap_or_else(|_| json!({}));
        let label = status.get("label").and_then(Value::as_str).unwrap_or("");
        let summary = status.get("summary").and_then(Value::as_str).unwrap_or("");
        let mut line = format!("- dsl_id={} state=running", dsl_id);
        if !label.is_empty() {
            line.push_str(&format!(" label={:?}", label));
        }
        if !summary.is_empty() {
            line.push_str(&format!(" summary={:?}", summary));
        }
        entries.push(line);
    }
    entries.sort();
    Ok(entries)
}

pub(super) fn cleanup_dsl_tasks(runtime_state_root: &Path) -> Result<usize> {
    let Ok(dir) = dsl_state_dir(runtime_state_root) else {
        return Ok(0);
    };
    let mut killed = 0usize;
    for entry in fs::read_dir(&dir).with_context(|| format!("failed to read {}", dir.display()))? {
        let entry = entry?;
        let path = entry.path();
        let Some(file_name) = path.file_name().and_then(|value| value.to_str()) else {
            continue;
        };
        if !is_dsl_metadata_file_name(file_name) {
            continue;
        }
        let raw = fs::read_to_string(&path)
            .with_context(|| format!("failed to read {}", path.display()))?;
        let metadata: BackgroundTaskMetadata =
            serde_json::from_str(&raw).context("failed to parse DSL metadata")?;
        if !background_task_is_running(&metadata) {
            continue;
        }
        terminate_dsl_process_tree(metadata.pid);
        let _ = record_exit_code(Path::new(&metadata.exit_code_path), -9);
        let previous = read_status_json(Path::new(&metadata.status_path)).ok();
        let mut status = previous
            .and_then(|value| serde_json::from_value::<DslStatus>(value).ok())
            .unwrap_or_else(|| initial_status(&metadata.task_id, None, default_limits()));
        status.status = "killed".to_string();
        status.finished_at = Some(now_timestamp_string());
        status.summary =
            "DSL job was terminated because the runtime restarted or session was destroyed."
                .to_string();
        status.diagnostics.push(diagnostic(
            "warning",
            "DSL job terminated by runtime cleanup",
        ));
        let _ = write_dsl_status(Path::new(&metadata.status_path), &status);
        killed = killed.saturating_add(1);
    }
    Ok(killed)
}

fn default_limits() -> DslLimits {
    DslLimits {
        max_llm_calls: DSL_DEFAULT_MAX_LLM_CALLS,
        max_tool_calls: DSL_DEFAULT_MAX_TOOL_CALLS,
        max_emit_calls: DSL_DEFAULT_MAX_EMIT_CALLS,
        max_runtime_seconds: DSL_DEFAULT_MAX_RUNTIME_SECONDS,
    }
}

fn initial_status(dsl_id: &str, label: Option<String>, limits: DslLimits) -> DslStatus {
    DslStatus {
        dsl_id: dsl_id.to_string(),
        status: "running".to_string(),
        label,
        started_at: now_timestamp_string(),
        finished_at: None,
        summary: "DSL job is running.".to_string(),
        diagnostics: Vec::new(),
        usage: DslUsage::default(),
        limits,
        children: Vec::new(),
    }
}

fn assistant_text(message: &ChatMessage) -> String {
    match &message.content {
        Some(Value::String(text)) => text.clone(),
        Some(value) => value.to_string(),
        None => String::new(),
    }
}

fn write_json_line<W: Write>(writer: &mut W, value: &Value) -> Result<()> {
    serde_json::to_writer(&mut *writer, value).context("failed to serialize DSL RPC message")?;
    writer
        .write_all(b"\n")
        .context("failed to write DSL RPC newline")?;
    writer.flush().context("failed to flush DSL RPC message")
}

fn spawn_python_worker(script_path: &Path, workspace_root: &Path) -> Result<Child> {
    let mut last_error = None;
    for executable in ["python3", "python"] {
        let mut command = Command::new(executable);
        command
            .arg("-u")
            .arg(script_path)
            .current_dir(workspace_root)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit());
        match command.spawn() {
            Ok(child) => return Ok(child),
            Err(error) => last_error = Some(error),
        }
    }
    Err(anyhow!(
        "failed to spawn CPython DSL worker with python3 or python: {}",
        last_error
            .map(|error| error.to_string())
            .unwrap_or_else(|| "unknown error".to_string())
    ))
}

fn chat_messages_from_value(value: &Value) -> Result<Vec<ChatMessage>> {
    let array = value
        .as_array()
        .ok_or_else(|| anyhow!("llm_call messages must be an array"))?;
    let mut messages = Vec::new();
    for item in array {
        let object = item
            .as_object()
            .ok_or_else(|| anyhow!("llm_call message must be an object"))?;
        let role = object
            .get("role")
            .and_then(Value::as_str)
            .ok_or_else(|| anyhow!("llm_call message role must be a string"))?;
        let content = object
            .get("content")
            .and_then(Value::as_str)
            .ok_or_else(|| anyhow!("llm_call message content must be a string"))?;
        messages.push(ChatMessage::text(role, content.to_string()));
    }
    Ok(messages)
}

#[cfg(not(windows))]
fn child_process_pids(pid: u32) -> Vec<u32> {
    let Ok(output) = Command::new("pgrep")
        .arg("-P")
        .arg(pid.to_string())
        .output()
    else {
        return Vec::new();
    };
    if !output.status.success() {
        return Vec::new();
    }
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter_map(|line| line.trim().parse::<u32>().ok())
        .collect()
}

#[cfg(not(windows))]
fn terminate_dsl_process_tree(pid: u32) {
    for child_pid in child_process_pids(pid) {
        terminate_dsl_process_tree(child_pid);
    }
    terminate_process_pid(pid);
}

#[cfg(windows)]
fn terminate_dsl_process_tree(pid: u32) {
    terminate_process_pid(pid);
}

impl DslRuntime {
    fn new(
        dsl_id: &str,
        upstream: UpstreamConfig,
        available_upstreams: BTreeMap<String, UpstreamConfig>,
        workspace_root: &Path,
        runtime_state_root: &Path,
        remote_workpaths: &[RemoteWorkpathConfig],
        status_path: &Path,
        result_path: &Path,
        worker_script_path: &Path,
        label: Option<String>,
        max_runtime_seconds: u64,
        max_llm_calls: u64,
        max_tool_calls: u64,
        max_emit_calls: u64,
    ) -> Self {
        let limits = DslLimits {
            max_llm_calls,
            max_tool_calls,
            max_emit_calls,
            max_runtime_seconds,
        };
        Self {
            upstream,
            available_upstreams,
            workspace_root: workspace_root.to_path_buf(),
            runtime_state_root: runtime_state_root.to_path_buf(),
            remote_workpaths: remote_workpaths.to_vec(),
            status_path: status_path.to_path_buf(),
            result_path: result_path.to_path_buf(),
            worker_script_path: worker_script_path.to_path_buf(),
            started: Instant::now(),
            status: initial_status(dsl_id, label, limits),
        }
    }

    fn persist_status(&mut self) -> Result<()> {
        self.status.usage.runtime_seconds = self.started.elapsed().as_secs_f64();
        write_dsl_status(&self.status_path, &self.status)
    }

    fn ensure_llm_budget(&self) -> Result<()> {
        if self.status.usage.llm_calls >= self.status.limits.max_llm_calls {
            return Err(anyhow!("DSL exceeded max_llm_calls"));
        }
        Ok(())
    }

    fn ensure_tool_budget(&self) -> Result<()> {
        if self.status.usage.tool_calls >= self.status.limits.max_tool_calls {
            return Err(anyhow!("DSL exceeded max_tool_calls"));
        }
        Ok(())
    }

    fn ensure_emit_budget(&self) -> Result<()> {
        if self.status.usage.emit_calls >= self.status.limits.max_emit_calls {
            return Err(anyhow!("DSL exceeded max_emit_calls"));
        }
        Ok(())
    }

    fn ensure_runtime_budget(&self) -> Result<()> {
        if self.started.elapsed().as_secs_f64() > self.status.limits.max_runtime_seconds as f64 {
            return Err(anyhow!("DSL runtime exceeded max_runtime_seconds"));
        }
        Ok(())
    }

    fn finish(&mut self, result: Value) -> Result<()> {
        self.status.status = "completed".to_string();
        self.status.finished_at = Some(now_timestamp_string());
        self.status.summary = "DSL job completed.".to_string();
        fs::write(
            &self.result_path,
            serde_json::to_vec_pretty(&result).context("failed to serialize DSL result")?,
        )
        .with_context(|| format!("failed to write {}", self.result_path.display()))?;
        self.persist_status()
    }

    fn fail(&mut self, error: anyhow::Error) -> Result<()> {
        self.status.status = "failed".to_string();
        self.status.finished_at = Some(now_timestamp_string());
        self.status.summary = "DSL job failed.".to_string();
        self.status
            .diagnostics
            .push(diagnostic("error", error.to_string()));
        fs::write(
            &self.result_path,
            serde_json::to_vec_pretty(&json!({"error": error.to_string()}))
                .context("failed to serialize DSL error result")?,
        )
        .with_context(|| format!("failed to write {}", self.result_path.display()))?;
        self.persist_status()
    }

    fn execute(&mut self, code: &str) -> Result<()> {
        if code.chars().count() > DSL_MAX_CODE_CHARS {
            return Err(anyhow!(
                "DSL code is too large; maximum is {} characters",
                DSL_MAX_CODE_CHARS
            ));
        }
        if let Some(parent) = self.worker_script_path.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("failed to create {}", parent.display()))?;
        }
        fs::write(&self.worker_script_path, DSL_WORKER_SOURCE)
            .with_context(|| format!("failed to write {}", self.worker_script_path.display()))?;
        self.persist_status()?;

        let mut child = spawn_python_worker(&self.worker_script_path, &self.workspace_root)?;
        self.status.summary = "DSL CPython worker is running.".to_string();
        self.persist_status()?;

        let mut child_stdin = child
            .stdin
            .take()
            .ok_or_else(|| anyhow!("DSL worker stdin was not piped"))?;
        let child_stdout = child
            .stdout
            .take()
            .ok_or_else(|| anyhow!("DSL worker stdout was not piped"))?;
        let (line_tx, line_rx) = channel::unbounded::<Result<String, String>>();
        thread::spawn(move || {
            let reader = BufReader::new(child_stdout);
            for line in reader.lines() {
                match line {
                    Ok(line) => {
                        if line_tx.send(Ok(line)).is_err() {
                            return;
                        }
                    }
                    Err(error) => {
                        let _ = line_tx.send(Err(error.to_string()));
                        return;
                    }
                }
            }
        });

        write_json_line(
            &mut child_stdin,
            &json!({
                "id": 0,
                "method": "exec",
                "params": {"code": code},
            }),
        )?;

        loop {
            channel::select! {
                recv(line_rx) -> received => {
                    let line = match received {
                        Ok(Ok(line)) => line,
                        Ok(Err(error)) => return Err(anyhow!("failed to read DSL worker protocol: {}", error)),
                        Err(_) => return Err(anyhow!("DSL worker exited before returning a result")),
                    };
                    if line.trim().is_empty() {
                        continue;
                    }
                    let message: Value = serde_json::from_str(&line)
                        .with_context(|| format!("invalid DSL worker protocol line: {}", line))?;
                    if message.get("id") == Some(&Value::from(0)) && message.get("method").is_none() {
                        let result = self.handle_exec_response(&message)?;
                        let _ = child.wait();
                        return self.finish(result);
                    }
                    let response = self.handle_worker_request(&message);
                    write_json_line(&mut child_stdin, &response)?;
                }
                recv(channel::after(Duration::from_millis(200))) -> _ => {
                    if let Err(error) = self.ensure_runtime_budget() {
                        let _ = child.kill();
                        let _ = child.wait();
                        return Err(error);
                    }
                }
            }
        }
    }

    fn handle_exec_response(&mut self, message: &Value) -> Result<Value> {
        if let Some(error) = message.get("error") {
            return Err(anyhow!("DSL worker error: {}", error));
        }
        let result = message
            .get("result")
            .ok_or_else(|| anyhow!("DSL worker exec response missing result"))?;
        if result.get("ok").and_then(Value::as_bool) == Some(true) {
            return Ok(result.get("result").cloned().unwrap_or(Value::Null));
        }
        let error = result
            .get("error")
            .and_then(Value::as_str)
            .unwrap_or("unknown DSL error");
        if let Some(traceback) = result.get("traceback").and_then(Value::as_str) {
            return Err(anyhow!("{error}\n{traceback}"));
        }
        Err(anyhow!(error.to_string()))
    }

    fn handle_worker_request(&mut self, message: &Value) -> Value {
        let id = message.get("id").cloned().unwrap_or(Value::Null);
        let result = (|| {
            let method = message
                .get("method")
                .and_then(Value::as_str)
                .ok_or_else(|| anyhow!("DSL worker request missing method"))?;
            let params = message.get("params").cloned().unwrap_or_else(|| json!({}));
            match method {
                "emit" => self.handle_emit(&params),
                "llm_call" => self.handle_llm_call(&params),
                "tool_call" => self.handle_tool_call(&params),
                _ => Err(anyhow!("unknown DSL worker request method: {}", method)),
            }
        })();
        match result {
            Ok(value) => json!({"id": id, "result": value}),
            Err(error) => json!({"id": id, "error": error.to_string()}),
        }
    }

    fn handle_emit(&mut self, params: &Value) -> Result<Value> {
        self.ensure_emit_budget()?;
        let text = params
            .get("text")
            .and_then(Value::as_str)
            .ok_or_else(|| anyhow!("emit text must be a string"))?;
        println!("{text}");
        self.status.usage.emit_calls += 1;
        self.status.summary = "DSL emitted output.".to_string();
        self.persist_status()?;
        Ok(json!({"ok": true}))
    }

    fn handle_llm_call(&mut self, params: &Value) -> Result<Value> {
        self.ensure_runtime_budget()?;
        self.ensure_llm_budget()?;
        let (upstream, extra_payload) = self.llm_request_upstream_and_payload(params)?;
        let mut messages = chat_messages_from_value(
            params
                .get("messages")
                .ok_or_else(|| anyhow!("llm_call missing messages"))?,
        )?;
        let prompt = params
            .get("prompt")
            .and_then(Value::as_str)
            .ok_or_else(|| anyhow!("llm_call prompt must be a string"))?;
        messages.push(ChatMessage::text("user", prompt.to_string()));
        self.status.usage.llm_calls += 1;
        self.status.summary = "DSL is calling an LLM.".to_string();
        self.persist_status()?;
        let outcome = create_chat_completion(&upstream, &messages, &[], Some(extra_payload), None)?;
        let text = assistant_text(&outcome.message);
        self.status.summary = "DSL job is running.".to_string();
        self.persist_status()?;
        Ok(json!({"text": text}))
    }

    fn llm_request_upstream_and_payload(
        &self,
        params: &Value,
    ) -> Result<(UpstreamConfig, Map<String, Value>)> {
        if params.get("model").is_some() {
            return Err(anyhow!(
                "DSL only supports LLM(); model switching is not allowed"
            ));
        }
        let extra_payload = params
            .get("extra_payload")
            .and_then(Value::as_object)
            .cloned()
            .unwrap_or_default();
        if extra_payload.get("model").is_some() {
            return Err(anyhow!(
                "DSL only supports LLM(); handle.config(model=...) is not allowed"
            ));
        }
        Ok((self.upstream.clone(), extra_payload))
    }

    fn handle_tool_call(&mut self, params: &Value) -> Result<Value> {
        self.ensure_runtime_budget()?;
        self.ensure_tool_budget()?;
        let tool_name = params
            .get("name")
            .and_then(Value::as_str)
            .ok_or_else(|| anyhow!("tool name must be a string"))?;
        if tool_name.starts_with("dsl_") {
            return Err(anyhow!("DSL cannot recursively call DSL lifecycle tools"));
        }
        let arguments = params
            .get("args")
            .and_then(Value::as_object)
            .cloned()
            .unwrap_or_default();
        let mut registry = build_tool_registry_with_cancel(
            &[],
            &self.workspace_root,
            &self.runtime_state_root,
            &self.upstream,
            &self.available_upstreams,
            None,
            None,
            None,
            None,
            &[],
            &[],
            &[],
            &self.remote_workpaths,
            None,
        )?;
        registry.remove("dsl_start");
        registry.remove("dsl_wait");
        registry.remove("dsl_kill");
        let raw_arguments = serde_json::to_string(&Value::Object(arguments))
            .context("failed to serialize DSL tool arguments")?;
        self.status.usage.tool_calls += 1;
        self.status.summary = format!("DSL is calling tool {}.", tool_name);
        self.persist_status()?;
        let output = execute_tool_call(&registry, tool_name, Some(&raw_arguments));
        let value = serde_json::from_str::<Value>(&output).unwrap_or(Value::String(output));
        self.record_child_task(tool_name, &value);
        self.status.summary = "DSL job is running.".to_string();
        self.persist_status()?;
        Ok(value)
    }

    fn record_child_task(&mut self, tool_name: &str, value: &Value) {
        for id_field in ["exec_id", "download_id", "image_id"] {
            if let Some(id) = value.get(id_field).and_then(Value::as_str) {
                self.status.children.push(DslChildTask {
                    tool: tool_name.to_string(),
                    id_field: id_field.to_string(),
                    id: id.to_string(),
                    status: "started".to_string(),
                });
            }
        }
    }
}

pub(crate) fn run_dsl_worker_job(
    dsl_id: &str,
    label: Option<String>,
    code: &str,
    upstream: UpstreamConfig,
    available_upstreams: BTreeMap<String, UpstreamConfig>,
    workspace_root: &Path,
    runtime_state_root: &Path,
    remote_workpaths: &[RemoteWorkpathConfig],
    status_path: &Path,
    result_path: &Path,
    max_runtime_seconds: u64,
    max_llm_calls: u64,
    max_tool_calls: u64,
    max_emit_calls: u64,
) -> Result<()> {
    let state_dir = dsl_state_dir(runtime_state_root)?;
    let worker_script_path = dsl_worker_script_path(&state_dir, dsl_id);
    let mut runtime = DslRuntime::new(
        dsl_id,
        upstream,
        available_upstreams,
        workspace_root,
        runtime_state_root,
        remote_workpaths,
        status_path,
        result_path,
        &worker_script_path,
        label,
        max_runtime_seconds,
        max_llm_calls,
        max_tool_calls,
        max_emit_calls,
    );
    match runtime.execute(code) {
        Ok(()) => Ok(()),
        Err(error) => {
            runtime.fail(error)?;
            Ok(())
        }
    }
}

fn wait_for_dsl_job(
    runtime_state_root: &Path,
    workspace_root: &Path,
    dsl_id: &str,
    wait_timeout_seconds: f64,
    max_output_chars: usize,
    on_timeout: DslTimeoutAction,
    cancel_flag: Option<&Arc<InterruptSignal>>,
) -> Result<Value> {
    let metadata = read_dsl_metadata(runtime_state_root, dsl_id)?;
    let start = Instant::now();
    let cancel_receiver = cancel_flag.map(|signal| signal.subscribe());
    loop {
        let background_metadata = BackgroundTaskMetadata {
            task_id: metadata.dsl_id.clone(),
            pid: metadata.worker_pid,
            label: "dsl".to_string(),
            status_path: metadata.status_path.clone(),
            stdout_path: metadata.stdout_path.clone(),
            stderr_path: metadata.stderr_path.clone(),
            exit_code_path: metadata.worker_exit_code_path.clone(),
        };
        if !background_task_is_running(&background_metadata) {
            return render_dsl_snapshot(&metadata, workspace_root, max_output_chars, false, false);
        }
        if wait_timeout_seconds == 0.0 || start.elapsed().as_secs_f64() >= wait_timeout_seconds {
            if on_timeout == DslTimeoutAction::Kill {
                terminate_dsl_process_tree(metadata.worker_pid);
                let _ = record_exit_code(Path::new(&metadata.worker_exit_code_path), -9);
                mark_dsl_killed(&metadata, "dsl_wait timeout killed the DSL job")?;
                return render_dsl_snapshot(
                    &metadata,
                    workspace_root,
                    max_output_chars,
                    false,
                    true,
                );
            }
            let mut snapshot =
                render_dsl_snapshot(&metadata, workspace_root, max_output_chars, false, true)?;
            if let Some(object) = snapshot.as_object_mut() {
                object.insert("status".to_string(), Value::String("running".to_string()));
                object.insert(
                    "summary".to_string(),
                    Value::String("DSL job is still running.".to_string()),
                );
                object.insert(
                    "on_timeout".to_string(),
                    Value::String(on_timeout.as_str().to_string()),
                );
            }
            return Ok(snapshot);
        }
        if let Some(cancel_receiver) = &cancel_receiver {
            channel::select! {
                recv(cancel_receiver) -> _ => {
                    let mut snapshot = render_dsl_snapshot(&metadata, workspace_root, max_output_chars, true, false)?;
                    if let Some(object) = snapshot.as_object_mut() {
                        object.insert("status".to_string(), Value::String("running".to_string()));
                        object.insert("summary".to_string(), Value::String("DSL job is still running after outer wait interruption.".to_string()));
                        object.insert("reason".to_string(), Value::String("agent_turn_interrupted".to_string()));
                    }
                    return Ok(snapshot);
                }
                recv(channel::after(Duration::from_millis(200))) -> _ => {}
            }
        } else {
            thread::sleep(Duration::from_millis(200));
        }
    }
}

fn mark_dsl_killed(metadata: &DslMetadata, summary: &str) -> Result<()> {
    let value = read_status_json(Path::new(&metadata.status_path)).unwrap_or_else(|_| json!({}));
    let mut status = serde_json::from_value::<DslStatus>(value).unwrap_or_else(|_| {
        initial_status(&metadata.dsl_id, metadata.label.clone(), default_limits())
    });
    status.status = "killed".to_string();
    status.finished_at = Some(now_timestamp_string());
    status.summary = summary.to_string();
    status
        .diagnostics
        .push(diagnostic("warning", summary.to_string()));
    write_dsl_status(Path::new(&metadata.status_path), &status)
}

pub(super) fn dsl_start_tool(
    workspace_root: PathBuf,
    runtime_state_root: PathBuf,
    upstream: UpstreamConfig,
    available_upstreams: BTreeMap<String, UpstreamConfig>,
    remote_workpaths: Vec<RemoteWorkpathConfig>,
    cancel_flag: Option<Arc<InterruptSignal>>,
) -> Tool {
    Tool::new_interruptible(
        "dsl_start",
        "Start an exec-like DSL orchestration job backed by an isolated CPython worker. See the system prompt for DSL syntax and lifecycle rules. Output is capped by max_output_chars 0..1000 with complete files saved at returned paths.",
        json!({
            "type": "object",
            "properties": {
                "code": {"type": "string", "description": "DSL code executed inside a restricted CPython worker. See the system prompt for supported syntax and restrictions."},
                "label": {"type": "string"},
                "return_immediate": {"type": "boolean"},
                "wait_timeout_seconds": {"type": "number", "minimum": 0},
                "on_timeout": {"type": "string", "enum": ["continue", "kill", "CONTINUE", "KILL"]},
                "max_output_chars": {"type": "integer", "minimum": 0, "maximum": 1000},
                "max_runtime_seconds": {"type": "integer", "minimum": 1},
                "max_llm_calls": {"type": "integer", "minimum": 0},
                "max_tool_calls": {"type": "integer", "minimum": 0},
                "max_emit_calls": {"type": "integer", "minimum": 0}
            },
            "required": ["code"],
            "additionalProperties": false
        }),
        move |arguments| {
            let arguments = arguments
                .as_object()
                .ok_or_else(|| anyhow!("tool arguments must be an object"))?;
            let code = string_arg(arguments, "code")?;
            if code.chars().count() > DSL_MAX_CODE_CHARS {
                return Err(anyhow!(
                    "DSL code is too large; maximum is {} characters",
                    DSL_MAX_CODE_CHARS
                ));
            }
            let label = arguments
                .get("label")
                .and_then(Value::as_str)
                .map(ToOwned::to_owned);
            let return_immediate = arguments
                .get("return_immediate")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            let wait_timeout_seconds = arguments
                .get("wait_timeout_seconds")
                .map(|_| f64_arg(arguments, "wait_timeout_seconds"))
                .transpose()?
                .unwrap_or(DSL_DEFAULT_WAIT_TIMEOUT_SECONDS);
            let on_timeout =
                dsl_timeout_action_arg(arguments, "on_timeout", DslTimeoutAction::Continue)?;
            let max_output_chars = max_output_chars_arg(arguments)?;
            let max_runtime_seconds = optional_u64_arg(
                arguments,
                "max_runtime_seconds",
                DSL_DEFAULT_MAX_RUNTIME_SECONDS,
            )?;
            let max_llm_calls =
                optional_u64_arg(arguments, "max_llm_calls", DSL_DEFAULT_MAX_LLM_CALLS)?;
            let max_tool_calls =
                optional_u64_arg(arguments, "max_tool_calls", DSL_DEFAULT_MAX_TOOL_CALLS)?;
            let max_emit_calls =
                optional_u64_arg(arguments, "max_emit_calls", DSL_DEFAULT_MAX_EMIT_CALLS)?;

            let dsl_id = Uuid::new_v4().to_string();
            let state_dir = dsl_state_dir(&runtime_state_root)?;
            let status_path = dsl_status_path(&state_dir, &dsl_id);
            let result_path = dsl_result_path(&state_dir, &dsl_id);
            let code_path = dsl_code_path(&state_dir, &dsl_id);
            fs::write(&code_path, &code)
                .with_context(|| format!("failed to write {}", code_path.display()))?;
            let mut initial = initial_status(
                &dsl_id,
                label.clone(),
                DslLimits {
                    max_llm_calls,
                    max_tool_calls,
                    max_emit_calls,
                    max_runtime_seconds,
                },
            );
            initial.summary = "DSL job has been started.".to_string();
            write_dsl_status(&status_path, &initial)?;
            fs::write(&result_path, b"null")
                .with_context(|| format!("failed to write {}", result_path.display()))?;

            let job = ToolWorkerJob::Dsl {
                dsl_id: dsl_id.clone(),
                label: label.clone(),
                code,
                upstream: upstream.clone(),
                available_upstreams: available_upstreams.clone(),
                workspace_root: workspace_root.display().to_string(),
                runtime_state_root: runtime_state_root.display().to_string(),
                remote_workpaths: remote_workpaths.clone(),
                status_path: status_path.display().to_string(),
                result_path: result_path.display().to_string(),
                max_runtime_seconds,
                max_llm_calls,
                max_tool_calls,
                max_emit_calls,
            };
            let background =
                spawn_background_worker_process(&runtime_state_root, "dsl", &dsl_id, &job)?;
            write_background_task_metadata(&state_dir, &background)?;
            let metadata = DslMetadata {
                dsl_id: dsl_id.clone(),
                worker_pid: background.pid,
                label,
                code_path: code_path.display().to_string(),
                status_path: background.status_path,
                stdout_path: background.stdout_path,
                stderr_path: background.stderr_path,
                result_path: result_path.display().to_string(),
                worker_exit_code_path: background.exit_code_path,
            };
            if return_immediate {
                return render_dsl_snapshot(
                    &metadata,
                    &workspace_root,
                    max_output_chars,
                    false,
                    false,
                );
            }
            wait_for_dsl_job(
                &runtime_state_root,
                &workspace_root,
                &dsl_id,
                wait_timeout_seconds,
                max_output_chars,
                on_timeout,
                cancel_flag.as_ref(),
            )
        },
    )
}

pub(super) fn dsl_wait_tool(
    workspace_root: PathBuf,
    runtime_state_root: PathBuf,
    cancel_flag: Option<Arc<InterruptSignal>>,
) -> Tool {
    Tool::new_interruptible(
        "dsl_wait",
        "Wait for or observe a previously started DSL job by dsl_id. Interrupting dsl_wait only interrupts the outer wait and returns running; the CPython DSL worker and any LLM/tool call it is currently performing continue in the background. Use wait_timeout_seconds=0 to observe without waiting. on_timeout=kill terminates the DSL job; otherwise timeout leaves it running.",
        json!({
            "type": "object",
            "properties": {
                "dsl_id": {"type": "string"},
                "wait_timeout_seconds": {"type": "number", "minimum": 0},
                "on_timeout": {"type": "string", "enum": ["continue", "kill", "CONTINUE", "KILL"]},
                "max_output_chars": {"type": "integer", "minimum": 0, "maximum": 1000}
            },
            "required": ["dsl_id"],
            "additionalProperties": false
        }),
        move |arguments| {
            let arguments = arguments
                .as_object()
                .ok_or_else(|| anyhow!("tool arguments must be an object"))?;
            let dsl_id = string_arg(arguments, "dsl_id")?;
            let wait_timeout_seconds = arguments
                .get("wait_timeout_seconds")
                .map(|_| f64_arg(arguments, "wait_timeout_seconds"))
                .transpose()?
                .unwrap_or(DSL_DEFAULT_WAIT_TIMEOUT_SECONDS);
            let on_timeout =
                dsl_timeout_action_arg(arguments, "on_timeout", DslTimeoutAction::Continue)?;
            let max_output_chars = max_output_chars_arg(arguments)?;
            wait_for_dsl_job(
                &runtime_state_root,
                &workspace_root,
                &dsl_id,
                wait_timeout_seconds,
                max_output_chars,
                on_timeout,
                cancel_flag.as_ref(),
            )
        },
    )
}

pub(super) fn dsl_kill_tool(
    workspace_root: PathBuf,
    runtime_state_root: PathBuf,
    _cancel_flag: Option<Arc<InterruptSignal>>,
) -> Tool {
    Tool::new(
        "dsl_kill",
        "Terminate a DSL job by dsl_id. By default this kills only the DSL CPython worker/job; child exec/download/image jobs continue unless kill_children=true is set.",
        json!({
            "type": "object",
            "properties": {
                "dsl_id": {"type": "string"},
                "kill_children": {"type": "boolean"},
                "max_output_chars": {"type": "integer", "minimum": 0, "maximum": 1000}
            },
            "required": ["dsl_id"],
            "additionalProperties": false
        }),
        move |arguments| {
            let arguments = arguments
                .as_object()
                .ok_or_else(|| anyhow!("tool arguments must be an object"))?;
            let dsl_id = string_arg(arguments, "dsl_id")?;
            let kill_children = arguments
                .get("kill_children")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            let max_output_chars = max_output_chars_arg(arguments)?;
            let metadata = read_dsl_metadata(&runtime_state_root, &dsl_id)?;
            let background = BackgroundTaskMetadata {
                task_id: metadata.dsl_id.clone(),
                pid: metadata.worker_pid,
                label: "dsl".to_string(),
                status_path: metadata.status_path.clone(),
                stdout_path: metadata.stdout_path.clone(),
                stderr_path: metadata.stderr_path.clone(),
                exit_code_path: metadata.worker_exit_code_path.clone(),
            };
            let was_running = background_task_is_running(&background);
            if was_running {
                terminate_dsl_process_tree(metadata.worker_pid);
                let _ = record_exit_code(Path::new(&metadata.worker_exit_code_path), -9);
                mark_dsl_killed(&metadata, "DSL job was killed by dsl_kill")?;
            }
            let mut child_results = Vec::new();
            if kill_children {
                let snapshot = read_status_json(Path::new(&metadata.status_path))
                    .unwrap_or_else(|_| json!({}));
                if let Some(children) = snapshot.get("children").and_then(Value::as_array) {
                    for child in children {
                        child_results.push(kill_child_task(&runtime_state_root, child));
                    }
                }
            }
            let mut snapshot =
                render_dsl_snapshot(&metadata, &workspace_root, max_output_chars, false, false)?;
            if let Some(object) = snapshot.as_object_mut() {
                let status = object
                    .get("status")
                    .and_then(Value::as_str)
                    .unwrap_or("unknown")
                    .to_string();
                object.insert("killed".to_string(), Value::Bool(was_running));
                object.insert(
                    "status".to_string(),
                    Value::String(if was_running {
                        "killed".to_string()
                    } else {
                        status
                    }),
                );
                if kill_children {
                    object.insert(
                        "children_kill_results".to_string(),
                        Value::Array(child_results),
                    );
                }
            }
            Ok(snapshot)
        },
    )
}

fn kill_child_task(runtime_state_root: &Path, child: &Value) -> Value {
    let tool = child
        .get("tool")
        .and_then(Value::as_str)
        .unwrap_or("unknown");
    let id_field = child.get("id_field").and_then(Value::as_str).unwrap_or("");
    let id = child.get("id").and_then(Value::as_str).unwrap_or("");
    let result = match id_field {
        "exec_id" => {
            exec_kill_tool(runtime_state_root.to_path_buf(), None).invoke(json!({"exec_id": id}))
        }
        "download_id" => file_download_cancel_tool(runtime_state_root.to_path_buf(), None)
            .invoke(json!({"download_id": id})),
        "image_id" => image_cancel_tool(runtime_state_root.to_path_buf(), None)
            .invoke(json!({"image_id": id})),
        _ => Err(anyhow!("unknown child id field")),
    };
    json!({
        "tool": tool,
        "id": id,
        "status": if result.is_ok() { "killed" } else { "failed_to_kill" },
        "error": result.err().map(|error| error.to_string()),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn test_upstream() -> UpstreamConfig {
        serde_json::from_value(json!({"base_url": "http://localhost", "model": "test"})).unwrap()
    }

    fn run_test_dsl(
        code: &str,
        max_emit_calls: u64,
        max_tool_calls: u64,
    ) -> (TempDir, PathBuf, PathBuf) {
        let temp_dir = TempDir::new().unwrap();
        let workspace = temp_dir.path().join("workspace");
        let runtime_root = temp_dir.path().join("runtime");
        fs::create_dir_all(&workspace).unwrap();
        fs::create_dir_all(&runtime_root).unwrap();
        let status_path = runtime_root.join("status.json");
        let result_path = runtime_root.join("result.json");
        let worker_script_path = runtime_root.join("worker.py");
        let mut runtime = DslRuntime::new(
            "test-dsl",
            test_upstream(),
            BTreeMap::new(),
            &workspace,
            &runtime_root,
            &[],
            &status_path,
            &result_path,
            &worker_script_path,
            None,
            60,
            0,
            max_tool_calls,
            max_emit_calls,
        );
        runtime.execute(code).unwrap();
        (temp_dir, status_path, result_path)
    }

    #[test]
    fn dsl_tools_have_exec_like_lifecycle_schema() {
        let temp_dir = TempDir::new().unwrap();
        let workspace = temp_dir.path().join("workspace");
        let runtime = temp_dir.path().join("runtime");
        fs::create_dir_all(&workspace).unwrap();
        fs::create_dir_all(&runtime).unwrap();
        let start = dsl_start_tool(
            workspace.clone(),
            runtime.clone(),
            test_upstream(),
            BTreeMap::new(),
            Vec::new(),
            None,
        );
        assert_eq!(
            start.execution_mode,
            super::super::ToolExecutionMode::Interruptible
        );
        assert!(start.description.contains("CPython worker"));
        assert!(start.description.contains("See the system prompt"));
        assert!(!start.description.contains("tool({\"name\""));
        assert!(!start.description.contains("same caller model"));
        assert!(start.parameters["properties"].get("code").is_some());
        let code_description = start.parameters["properties"]["code"]["description"]
            .as_str()
            .unwrap();
        assert!(code_description.contains("See the system prompt"));
        assert!(!code_description.contains("LLM()"));
        assert!(!code_description.contains("tool({\"name\""));
        assert!(
            start.parameters["properties"]
                .get("return_immediate")
                .is_some()
        );
        assert!(
            start.parameters["properties"]
                .get("wait_timeout_seconds")
                .is_some()
        );
        assert!(start.parameters["properties"].get("on_timeout").is_some());
        assert_eq!(
            start.parameters["properties"]["max_output_chars"]["maximum"],
            Value::from(1000)
        );

        let wait = dsl_wait_tool(workspace.clone(), runtime.clone(), None);
        assert_eq!(
            wait.execution_mode,
            super::super::ToolExecutionMode::Interruptible
        );
        assert!(wait.description.contains("CPython DSL worker"));
        assert!(wait.parameters["properties"].get("dsl_id").is_some());

        let kill = dsl_kill_tool(workspace, runtime, None);
        assert_eq!(
            kill.execution_mode,
            super::super::ToolExecutionMode::Immediate
        );
        assert!(kill.parameters["properties"].get("kill_children").is_some());
    }

    #[test]
    fn dsl_active_summary_ignores_status_and_result_json_files() {
        let temp_dir = TempDir::new().unwrap();
        let runtime_root = temp_dir.path().join("runtime");
        let dir = dsl_state_dir(&runtime_root).unwrap();
        let dsl_id = "test-dsl";
        let status_path = dsl_status_path(&dir, dsl_id);
        let result_path = dsl_result_path(&dir, dsl_id);
        let stdout_path = dir.join(format!("{dsl_id}.stdout"));
        let stderr_path = dir.join(format!("{dsl_id}.stderr"));
        let exit_code_path = dir.join(format!("{dsl_id}.exit"));
        let status = initial_status(dsl_id, Some("summary-test".to_string()), default_limits());
        write_dsl_status(&status_path, &status).unwrap();
        fs::write(&result_path, serde_json::to_vec(&json!("hello")).unwrap()).unwrap();
        fs::write(&stdout_path, "").unwrap();
        fs::write(&stderr_path, "").unwrap();
        fs::write(&exit_code_path, "").unwrap();
        write_background_task_metadata(
            &dir,
            &BackgroundTaskMetadata {
                task_id: dsl_id.to_string(),
                pid: std::process::id(),
                label: "dsl".to_string(),
                status_path: status_path.display().to_string(),
                stdout_path: stdout_path.display().to_string(),
                stderr_path: stderr_path.display().to_string(),
                exit_code_path: exit_code_path.display().to_string(),
            },
        )
        .unwrap();

        let summaries = list_active_dsl_summaries(&runtime_root).unwrap();

        assert_eq!(summaries.len(), 1);
        assert!(summaries[0].contains("test-dsl"));
    }

    #[test]
    fn dsl_cpython_worker_executes_emit_and_quit_script() {
        let (_temp_dir, status_path, result_path) =
            run_test_dsl("emit('hello')\nquit({'ok': True})", 2, 0);
        let status: Value = serde_json::from_slice(&fs::read(&status_path).unwrap()).unwrap();
        assert_eq!(status["status"], json!("completed"));
        assert_eq!(status["usage"]["emit_calls"], json!(1));
        let result: Value = serde_json::from_slice(&fs::read(&result_path).unwrap()).unwrap();
        assert_eq!(result["ok"], json!(true));
    }

    #[test]
    fn dsl_cpython_worker_uses_emit_output_as_default_result() {
        let (_temp_dir, _status_path, result_path) =
            run_test_dsl("emit('hello')\nemit('world')", 2, 0);
        let result: Value = serde_json::from_slice(&fs::read(&result_path).unwrap()).unwrap();
        assert_eq!(result, json!("hello\nworld"));
    }

    #[test]
    fn dsl_cpython_worker_defaults_to_zero_without_emit_or_quit() {
        let (_temp_dir, _status_path, result_path) = run_test_dsl("x = 1 + 1", 0, 0);
        let result: Value = serde_json::from_slice(&fs::read(&result_path).unwrap()).unwrap();
        assert_eq!(result, json!(0));
    }

    #[test]
    fn dsl_cpython_worker_supports_real_python_expressions_and_if() {
        let code = "items = [1, 2, 3, 4]\nword = 'hello'\nflag = 1 < 2 <= 2 and not False\nif flag:\n    label = 'yes'\nelse:\n    label = 'no'\nemit(f\"{label}:{len(items)}:{items[1:3]}:{word.upper()}:{'A' * 3}\")";
        let (_temp_dir, _status_path, result_path) = run_test_dsl(code, 1, 0);
        let result: Value = serde_json::from_slice(&fs::read(&result_path).unwrap()).unwrap();
        assert_eq!(result, json!("yes:4:[2, 3]:HELLO:AAA"));
    }

    #[test]
    fn dsl_cpython_worker_rejects_unbounded_or_unsafe_constructs() {
        let temp_dir = TempDir::new().unwrap();
        let workspace = temp_dir.path().join("workspace");
        let runtime_root = temp_dir.path().join("runtime");
        fs::create_dir_all(&workspace).unwrap();
        fs::create_dir_all(&runtime_root).unwrap();
        for (index, code) in [
            "for x in [1, 2]:\n    emit(x)",
            "while True:\n    emit('x')",
            "items = [x for x in [1, 2]]",
            "import os",
            "def f():\n    pass",
            "class X:\n    pass",
            "await tool('dsl_start', code='quit(None)')",
            "await tool({'name': 'dsl_start', 'args': {'code': 'quit(None)'}})",
        ]
        .iter()
        .enumerate()
        {
            let status_path = runtime_root.join(format!("status-{index}.json"));
            let result_path = runtime_root.join(format!("result-{index}.json"));
            let worker_script_path = runtime_root.join(format!("worker-{index}.py"));
            let mut runtime = DslRuntime::new(
                "test-dsl",
                test_upstream(),
                BTreeMap::new(),
                &workspace,
                &runtime_root,
                &[],
                &status_path,
                &result_path,
                &worker_script_path,
                None,
                60,
                0,
                0,
                1,
            );
            runtime.execute(code).unwrap_err();
        }
    }

    #[test]
    fn dsl_cpython_worker_allows_type_builtin() {
        let (_temp_dir, _status_path, result_path) =
            run_test_dsl("value = str(type(1))\nemit(value)", 1, 0);
        let result: Value = serde_json::from_slice(&fs::read(&result_path).unwrap()).unwrap();
        assert_eq!(result, json!("<class 'int'>"));
    }

    #[test]
    fn dsl_cpython_worker_calls_existing_tools_through_registry() {
        let temp_dir = TempDir::new().unwrap();
        let workspace = temp_dir.path().join("workspace");
        let runtime_root = temp_dir.path().join("runtime");
        fs::create_dir_all(&workspace).unwrap();
        fs::create_dir_all(&runtime_root).unwrap();
        fs::write(workspace.join("README.md"), "hello from the workspace").unwrap();
        let status_path = runtime_root.join("status.json");
        let result_path = runtime_root.join("result.json");
        let worker_script_path = runtime_root.join("worker.py");
        let mut runtime = DslRuntime::new(
            "test-dsl",
            test_upstream(),
            BTreeMap::new(),
            &workspace,
            &runtime_root,
            &[],
            &status_path,
            &result_path,
            &worker_script_path,
            None,
            60,
            0,
            2,
            1,
        );
        runtime
            .execute(
                "content = await tool({'name': 'file_read', 'args': {'path': 'README.md'}})\nemit(content)",
            )
            .unwrap();
        let status: Value = serde_json::from_slice(&fs::read(&status_path).unwrap()).unwrap();
        assert_eq!(status["usage"]["tool_calls"], json!(1));
        let result: Value = serde_json::from_slice(&fs::read(&result_path).unwrap()).unwrap();
        assert!(
            result
                .as_str()
                .unwrap()
                .contains("hello from the workspace")
        );
    }

    #[test]
    fn dsl_cpython_worker_rejects_legacy_tool_kwargs_shape() {
        let temp_dir = TempDir::new().unwrap();
        let workspace = temp_dir.path().join("workspace");
        let runtime_root = temp_dir.path().join("runtime");
        fs::create_dir_all(&workspace).unwrap();
        fs::create_dir_all(&runtime_root).unwrap();
        let status_path = runtime_root.join("status.json");
        let result_path = runtime_root.join("result.json");
        let worker_script_path = runtime_root.join("worker.py");
        let mut runtime = DslRuntime::new(
            "test-dsl",
            test_upstream(),
            BTreeMap::new(),
            &workspace,
            &runtime_root,
            &[],
            &status_path,
            &result_path,
            &worker_script_path,
            None,
            60,
            0,
            2,
            1,
        );

        let error = runtime
            .execute("content = await tool('file_read', path='README.md')")
            .unwrap_err();

        assert!(
            error
                .to_string()
                .contains("tool() requires a single dict argument")
        );
    }

    #[test]
    fn dsl_model_switching_is_rejected() {
        let temp_dir = TempDir::new().unwrap();
        let workspace = temp_dir.path().join("workspace");
        let runtime_root = temp_dir.path().join("runtime");
        fs::create_dir_all(&workspace).unwrap();
        fs::create_dir_all(&runtime_root).unwrap();
        let status_path = runtime_root.join("status.json");
        let result_path = runtime_root.join("result.json");
        let worker_script_path = runtime_root.join("worker.py");
        let upstream = test_upstream();
        let runtime = DslRuntime::new(
            "test-dsl",
            upstream.clone(),
            BTreeMap::new(),
            &workspace,
            &runtime_root,
            &[],
            &status_path,
            &result_path,
            &worker_script_path,
            None,
            60,
            0,
            0,
            0,
        );

        let model_param = json!({
            "model": "opus-4.6",
            "messages": [],
            "prompt": "hello"
        });
        let error = runtime
            .llm_request_upstream_and_payload(&model_param)
            .unwrap_err();
        assert!(error.to_string().contains("model switching is not allowed"));

        let config_model = json!({
            "messages": [],
            "prompt": "hello",
            "extra_payload": {
                "model": "opus-4.6",
                "temperature": 0.2
            }
        });
        let error = runtime
            .llm_request_upstream_and_payload(&config_model)
            .unwrap_err();
        assert!(
            error
                .to_string()
                .contains("handle.config(model=...) is not allowed")
        );

        let current_model = json!({
            "messages": [],
            "prompt": "hello",
            "extra_payload": {
                "temperature": 0.2
            }
        });
        let (selected_upstream, payload) = runtime
            .llm_request_upstream_and_payload(&current_model)
            .unwrap();
        assert_eq!(selected_upstream.model, upstream.model);
        assert_eq!(payload.get("temperature"), Some(&json!(0.2)));
    }

    #[test]
    fn dsl_snapshot_is_compact_for_successful_results() {
        let temp_dir = TempDir::new().unwrap();
        let workspace = temp_dir.path().join("workspace");
        let runtime_root = temp_dir.path().join("runtime");
        fs::create_dir_all(&workspace).unwrap();
        fs::create_dir_all(&runtime_root).unwrap();
        let status_path = runtime_root.join("status.json");
        let stdout_path = runtime_root.join("stdout.txt");
        let stderr_path = runtime_root.join("stderr.txt");
        let result_path = runtime_root.join("result.json");
        let exit_path = runtime_root.join("exit.txt");
        let status = initial_status("test-dsl", None, default_limits());
        write_dsl_status(&status_path, &status).unwrap();
        fs::write(&stdout_path, "").unwrap();
        fs::write(&stderr_path, "").unwrap();
        fs::write(&result_path, serde_json::to_vec(&json!("hello")).unwrap()).unwrap();
        fs::write(&exit_path, "0").unwrap();
        let metadata = DslMetadata {
            dsl_id: "test-dsl".to_string(),
            worker_pid: 1,
            label: None,
            code_path: runtime_root.join("test-dsl.py").display().to_string(),
            status_path: status_path.display().to_string(),
            stdout_path: stdout_path.display().to_string(),
            stderr_path: stderr_path.display().to_string(),
            result_path: result_path.display().to_string(),
            worker_exit_code_path: exit_path.display().to_string(),
        };

        let snapshot = render_dsl_snapshot(&metadata, &workspace, 1000, false, false).unwrap();

        assert_eq!(snapshot["result_text"], json!("hello"));
        assert_eq!(snapshot["status"], json!("running"));
        assert!(snapshot.get("started_at").is_none());
        assert!(snapshot.get("finished_at").is_none());
        assert!(snapshot.get("limits").is_none());
        assert!(snapshot.get("stdout").is_none());
        assert!(snapshot.get("stderr").is_none());
        assert!(snapshot.get("stdout_path").is_none());
        assert!(snapshot.get("stderr_path").is_none());
        assert!(snapshot.get("result_path").is_none());
        assert!(snapshot.get("interrupted").is_none());
        assert!(snapshot.get("timed_out").is_none());
        assert!(snapshot.get("result_truncated").is_none());
    }

    #[test]
    fn dsl_snapshot_includes_paths_only_when_truncated_or_stderr_present() {
        let temp_dir = TempDir::new().unwrap();
        let workspace = temp_dir.path().join("workspace");
        let runtime_root = temp_dir.path().join("runtime");
        fs::create_dir_all(&workspace).unwrap();
        fs::create_dir_all(&runtime_root).unwrap();
        let status_path = runtime_root.join("status.json");
        let stdout_path = runtime_root.join("stdout.txt");
        let stderr_path = runtime_root.join("stderr.txt");
        let result_path = runtime_root.join("result.json");
        let exit_path = runtime_root.join("exit.txt");
        let status = initial_status("test-dsl", None, default_limits());
        write_dsl_status(&status_path, &status).unwrap();
        fs::write(&stdout_path, "side channel output").unwrap();
        fs::write(&stderr_path, "warning text").unwrap();
        fs::write(
            &result_path,
            serde_json::to_vec(&json!("abcdefghijklmnopqrstuvwxyz")).unwrap(),
        )
        .unwrap();
        fs::write(&exit_path, "0").unwrap();
        let metadata = DslMetadata {
            dsl_id: "test-dsl".to_string(),
            worker_pid: 1,
            label: None,
            code_path: runtime_root.join("test-dsl.py").display().to_string(),
            status_path: status_path.display().to_string(),
            stdout_path: stdout_path.display().to_string(),
            stderr_path: stderr_path.display().to_string(),
            result_path: result_path.display().to_string(),
            worker_exit_code_path: exit_path.display().to_string(),
        };

        let snapshot = render_dsl_snapshot(&metadata, &workspace, 10, true, true).unwrap();

        assert_eq!(snapshot["interrupted"], json!(true));
        assert_eq!(snapshot["timed_out"], json!(true));
        assert_eq!(snapshot["result_truncated"], json!(true));
        assert!(
            snapshot["result_path"]
                .as_str()
                .unwrap()
                .ends_with(".result.json")
        );
        assert_eq!(snapshot["stderr_truncated"], json!(true));
        assert!(
            snapshot["stderr_path"]
                .as_str()
                .unwrap()
                .ends_with(".stderr")
        );
        assert_eq!(snapshot["stdout_truncated"], json!(true));
        assert!(
            snapshot["stdout_path"]
                .as_str()
                .unwrap()
                .ends_with(".stdout")
        );
    }
}
