use crate::config::{ExternalWebSearchConfig, UpstreamConfig};
use crate::llm::create_chat_completion;
use crate::message::ChatMessage;
use crate::skills::{SkillMetadata, build_skill_index, load_skill_by_name};
use anyhow::{Context, Result, anyhow};
use base64::Engine;
use crossbeam_channel::{self, Receiver};
use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::{Map, Value, json};
use std::collections::BTreeMap;
use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Output, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::thread;
use std::time::Duration;
use uuid::Uuid;

type ToolHandler = dyn Fn(Value) -> Result<Value> + Send + Sync + 'static;

#[derive(Clone)]
pub struct Tool {
    pub name: String,
    pub description: String,
    pub parameters: Value,
    handler: Arc<ToolHandler>,
}

impl Tool {
    pub fn new(
        name: impl Into<String>,
        description: impl Into<String>,
        parameters: Value,
        handler: impl Fn(Value) -> Result<Value> + Send + Sync + 'static,
    ) -> Self {
        Self {
            name: name.into(),
            description: description.into(),
            parameters,
            handler: Arc::new(handler),
        }
    }

    pub fn as_openai_tool(&self) -> Value {
        json!({
            "type": "function",
            "function": {
                "name": self.name,
                "description": self.description,
                "parameters": self.parameters,
            }
        })
    }

    pub fn invoke(&self, arguments: Value) -> Result<Value> {
        (self.handler)(arguments)
    }
}

fn normalize_tool_result(result: Value) -> String {
    match result {
        Value::String(text) => text,
        other => serde_json::to_string_pretty(&other).unwrap_or_else(|_| "{}".to_string()),
    }
}

fn object_arg<'a>(arguments: &'a Map<String, Value>, key: &str) -> Result<&'a Value> {
    arguments
        .get(key)
        .ok_or_else(|| anyhow!("missing required argument: {}", key))
}

fn string_arg(arguments: &Map<String, Value>, key: &str) -> Result<String> {
    object_arg(arguments, key)?
        .as_str()
        .map(ToOwned::to_owned)
        .ok_or_else(|| anyhow!("argument {} must be a string", key))
}

fn f64_arg(arguments: &Map<String, Value>, key: &str) -> Result<f64> {
    object_arg(arguments, key)?
        .as_f64()
        .ok_or_else(|| anyhow!("argument {} must be a number", key))
}

fn usize_arg_with_default(
    arguments: &Map<String, Value>,
    key: &str,
    default: usize,
) -> Result<usize> {
    match arguments.get(key) {
        Some(value) => value
            .as_u64()
            .map(|value| value as usize)
            .ok_or_else(|| anyhow!("argument {} must be an integer", key)),
        None => Ok(default),
    }
}

fn string_arg_with_default(
    arguments: &Map<String, Value>,
    key: &str,
    default: &str,
) -> Result<String> {
    match arguments.get(key) {
        Some(value) => value
            .as_str()
            .map(ToOwned::to_owned)
            .ok_or_else(|| anyhow!("argument {} must be a string", key)),
        None => Ok(default.to_string()),
    }
}

fn resolve_path(path: &str, workspace_root: &Path) -> PathBuf {
    let path_buf = PathBuf::from(path);
    if path_buf.is_absolute() {
        path_buf
    } else {
        workspace_root.join(path_buf)
    }
}

#[derive(Default)]
pub struct InterruptSignal {
    flag: AtomicBool,
    subscribers: Mutex<Vec<crossbeam_channel::Sender<()>>>,
}

impl InterruptSignal {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn request(&self) {
        self.flag.store(true, Ordering::SeqCst);
        let mut subscribers = self.subscribers.lock().unwrap();
        subscribers.retain(|subscriber| subscriber.try_send(()).is_ok());
    }

    pub fn clear(&self) {
        self.flag.store(false, Ordering::SeqCst);
    }

    pub fn is_requested(&self) -> bool {
        self.flag.load(Ordering::SeqCst)
    }

    pub fn subscribe(&self) -> Receiver<()> {
        let (sender, receiver) = crossbeam_channel::bounded(1);
        if self.is_requested() {
            let _ = sender.try_send(());
        } else {
            self.subscribers.lock().unwrap().push(sender);
        }
        receiver
    }
}

fn with_timeout_and_cancel<T: Send + 'static>(
    timeout_seconds: f64,
    cancel_flag: Option<Arc<InterruptSignal>>,
    operation: impl FnOnce() -> Result<T> + Send + 'static,
) -> Result<T> {
    enum OperationEvent<T> {
        Completed(Result<T>),
    }

    let (sender, receiver) = crossbeam_channel::bounded(1);
    thread::spawn(move || {
        let _ = sender.send(OperationEvent::Completed(operation()));
    });

    let cancel_receiver = cancel_flag.as_ref().map(|signal| signal.subscribe());
    let timeout_receiver = crossbeam_channel::after(Duration::from_secs_f64(timeout_seconds));
    match cancel_receiver {
        Some(cancel_receiver) => {
            crossbeam_channel::select! {
                recv(receiver) -> result => match result {
                    Ok(OperationEvent::Completed(result)) => result,
                    Err(_) => Err(anyhow!("operation worker disconnected")),
                },
                recv(cancel_receiver) -> _ => Err(anyhow!("operation cancelled")),
                recv(timeout_receiver) -> _ => Err(anyhow!(
                    "operation timed out after {} seconds",
                    timeout_seconds
                )),
            }
        }
        None => {
            crossbeam_channel::select! {
                recv(receiver) -> result => match result {
                    Ok(OperationEvent::Completed(result)) => result,
                    Err(_) => Err(anyhow!("operation worker disconnected")),
                },
                recv(timeout_receiver) -> _ => Err(anyhow!(
                    "operation timed out after {} seconds",
                    timeout_seconds
                )),
            }
        }
    }
}

fn wait_for_child_with_timeout(
    child: Child,
    timeout_seconds: f64,
    timeout_label: &str,
    cancel_flag: Option<&Arc<InterruptSignal>>,
) -> Result<Output> {
    let pid = child.id();
    let (result_sender, result_receiver) = crossbeam_channel::bounded(1);
    thread::spawn(move || {
        let mut child = child;
        let result = (|| -> Result<Output> {
            let status = child.wait().context("failed to finalize child process")?;
            let mut stdout = Vec::new();
            let mut stderr = Vec::new();
            if let Some(pipe) = child.stdout.as_mut() {
                pipe.read_to_end(&mut stdout)
                    .context("failed to read child stdout")?;
            }
            if let Some(pipe) = child.stderr.as_mut() {
                pipe.read_to_end(&mut stderr)
                    .context("failed to read child stderr")?;
            }
            Ok(Output {
                status,
                stdout,
                stderr,
            })
        })();
        let _ = result_sender.send(result);
    });

    let cancel_receiver = cancel_flag.map(|signal| signal.subscribe());
    let timeout_receiver = crossbeam_channel::after(Duration::from_secs_f64(timeout_seconds));
    let outcome = match cancel_receiver {
        Some(cancel_receiver) => crossbeam_channel::select! {
            recv(result_receiver) -> result => Some(result),
            recv(cancel_receiver) -> _ => {
                terminate_process_pid(pid);
                None
            }
            recv(timeout_receiver) -> _ => {
                terminate_process_pid(pid);
                None
            }
        },
        None => crossbeam_channel::select! {
            recv(result_receiver) -> result => Some(result),
            recv(timeout_receiver) -> _ => {
                terminate_process_pid(pid);
                None
            }
        },
    };

    if let Some(result) = outcome {
        return result.context("child process worker disconnected")?;
    }

    let _ = result_receiver.recv_timeout(Duration::from_secs(5));
    if cancel_flag.is_some_and(|signal| signal.is_requested()) {
        Err(anyhow!("{} cancelled", timeout_label))
    } else {
        Err(anyhow!(
            "{} timed out after {} seconds",
            timeout_label,
            timeout_seconds
        ))
    }
}

fn read_file_tool(workspace_root: PathBuf, cancel_flag: Option<Arc<InterruptSignal>>) -> Tool {
    Tool::new(
        "read_file",
        "Read a UTF-8 text file. The model must choose timeout_seconds.",
        json!({
            "type": "object",
            "properties": {
                "path": {"type": "string"},
                "timeout_seconds": {"type": "number"},
                "offset_lines": {"type": "integer"},
                "limit_lines": {"type": "integer"},
                "encoding": {"type": "string"}
            },
            "required": ["path", "timeout_seconds"],
            "additionalProperties": false
        }),
        move |arguments| {
            let arguments = arguments
                .as_object()
                .ok_or_else(|| anyhow!("tool arguments must be an object"))?;
            let path = resolve_path(&string_arg(arguments, "path")?, &workspace_root);
            let timeout_seconds = f64_arg(arguments, "timeout_seconds")?;
            let offset_lines = usize_arg_with_default(arguments, "offset_lines", 0)?;
            let limit_lines = usize_arg_with_default(arguments, "limit_lines", 200)?;
            let encoding = string_arg_with_default(arguments, "encoding", "utf-8")?;

            with_timeout_and_cancel(timeout_seconds, cancel_flag.clone(), move || {
                if encoding.to_lowercase() != "utf-8" {
                    return Err(anyhow!("only utf-8 encoding is supported"));
                }
                let text = fs::read_to_string(&path)
                    .with_context(|| format!("failed to read {}", path.display()))?;
                let lines: Vec<&str> = text.lines().collect();
                let selected = lines
                    .iter()
                    .skip(offset_lines)
                    .take(limit_lines)
                    .enumerate()
                    .map(|(index, line)| format!("{}: {}", offset_lines + index + 1, line))
                    .collect::<Vec<_>>()
                    .join("\n");
                Ok(json!({
                    "path": path.display().to_string(),
                    "start_line": offset_lines + 1,
                    "end_line": offset_lines + lines.iter().skip(offset_lines).take(limit_lines).count(),
                    "total_lines": lines.len(),
                    "truncated": offset_lines + limit_lines < lines.len(),
                    "content": selected
                }))
            })
        },
    )
}

fn write_file_tool(workspace_root: PathBuf, cancel_flag: Option<Arc<InterruptSignal>>) -> Tool {
    Tool::new(
        "write_file",
        "Write a UTF-8 text file. The model must choose timeout_seconds.",
        json!({
            "type": "object",
            "properties": {
                "path": {"type": "string"},
                "content": {"type": "string"},
                "mode": {"type": "string", "enum": ["overwrite", "append"]},
                "timeout_seconds": {"type": "number"},
                "encoding": {"type": "string"}
            },
            "required": ["path", "content", "timeout_seconds"],
            "additionalProperties": false
        }),
        move |arguments| {
            let arguments = arguments
                .as_object()
                .ok_or_else(|| anyhow!("tool arguments must be an object"))?;
            let path = resolve_path(&string_arg(arguments, "path")?, &workspace_root);
            let content = string_arg(arguments, "content")?;
            let mode = string_arg_with_default(arguments, "mode", "overwrite")?;
            let timeout_seconds = f64_arg(arguments, "timeout_seconds")?;
            let encoding = string_arg_with_default(arguments, "encoding", "utf-8")?;

            with_timeout_and_cancel(timeout_seconds, cancel_flag.clone(), move || {
                if encoding.to_lowercase() != "utf-8" {
                    return Err(anyhow!("only utf-8 encoding is supported"));
                }
                if let Some(parent) = path.parent() {
                    fs::create_dir_all(parent)
                        .with_context(|| format!("failed to create {}", parent.display()))?;
                }
                let mut options = fs::OpenOptions::new();
                options.create(true).write(true);
                if mode == "append" {
                    options.append(true);
                } else {
                    options.truncate(true);
                }
                use std::io::Write;
                let mut file = options
                    .open(&path)
                    .with_context(|| format!("failed to open {}", path.display()))?;
                file.write_all(content.as_bytes())
                    .with_context(|| format!("failed to write {}", path.display()))?;
                Ok(json!({
                    "path": path.display().to_string(),
                    "mode": mode,
                    "bytes_written": content.len()
                }))
            })
        },
    )
}

fn edit_tool(workspace_root: PathBuf, cancel_flag: Option<Arc<InterruptSignal>>) -> Tool {
    Tool::new(
        "edit",
        "Edit a UTF-8 text file by replacing old_text with new_text. The model must choose timeout_seconds.",
        json!({
            "type": "object",
            "properties": {
                "path": {"type": "string"},
                "old_text": {"type": "string"},
                "new_text": {"type": "string"},
                "replace_all": {"type": "boolean"},
                "create_if_missing": {"type": "boolean"},
                "timeout_seconds": {"type": "number"},
                "encoding": {"type": "string"}
            },
            "required": ["path", "old_text", "new_text", "timeout_seconds"],
            "additionalProperties": false
        }),
        move |arguments| {
            let arguments = arguments
                .as_object()
                .ok_or_else(|| anyhow!("tool arguments must be an object"))?;
            let path = resolve_path(&string_arg(arguments, "path")?, &workspace_root);
            let old_text = string_arg(arguments, "old_text")?;
            let new_text = string_arg(arguments, "new_text")?;
            let replace_all = arguments
                .get("replace_all")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            let create_if_missing = arguments
                .get("create_if_missing")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            let timeout_seconds = f64_arg(arguments, "timeout_seconds")?;
            let encoding = string_arg_with_default(arguments, "encoding", "utf-8")?;

            with_timeout_and_cancel(timeout_seconds, cancel_flag.clone(), move || {
                if encoding.to_lowercase() != "utf-8" {
                    return Err(anyhow!("only utf-8 encoding is supported"));
                }
                if !path.exists() && create_if_missing {
                    if let Some(parent) = path.parent() {
                        fs::create_dir_all(parent)
                            .with_context(|| format!("failed to create {}", parent.display()))?;
                    }
                    fs::write(&path, &new_text)
                        .with_context(|| format!("failed to write {}", path.display()))?;
                    return Ok(json!({
                        "path": path.display().to_string(),
                        "created": true,
                        "replacements": 1,
                        "bytes_written": new_text.len()
                    }));
                }
                let content = fs::read_to_string(&path)
                    .with_context(|| format!("failed to read {}", path.display()))?;
                let replacements = content.matches(&old_text).count();
                if replacements == 0 {
                    return Err(anyhow!("old_text was not found in {}", path.display()));
                }
                let updated = if replace_all {
                    content.replace(&old_text, &new_text)
                } else {
                    content.replacen(&old_text, &new_text, 1)
                };
                fs::write(&path, updated.as_bytes())
                    .with_context(|| format!("failed to write {}", path.display()))?;
                Ok(json!({
                    "path": path.display().to_string(),
                    "created": false,
                    "replacements": if replace_all { replacements } else { 1 },
                    "bytes_written": updated.len()
                }))
            })
        },
    )
}

fn process_state_dir(runtime_state_root: &Path) -> PathBuf {
    runtime_state_root.join("agent_frame").join("processes")
}

fn ensure_process_state_dir(runtime_state_root: &Path) -> Result<PathBuf> {
    let path = process_state_dir(runtime_state_root);
    fs::create_dir_all(&path).with_context(|| format!("failed to create {}", path.display()))?;
    Ok(path)
}

#[derive(Serialize, serde::Deserialize)]
struct ProcessMetadata {
    exec_id: String,
    pid: u32,
    command: String,
    cwd: String,
    stdout_path: String,
    stderr_path: String,
    exit_code_path: String,
}

struct LiveManagedProcess {
    stdin: Option<ChildStdin>,
    metadata: ProcessMetadata,
    completion_receiver: Option<Receiver<i32>>,
}

static LIVE_PROCESSES: OnceLock<Mutex<BTreeMap<String, LiveManagedProcess>>> = OnceLock::new();

fn live_processes() -> &'static Mutex<BTreeMap<String, LiveManagedProcess>> {
    LIVE_PROCESSES.get_or_init(|| Mutex::new(BTreeMap::new()))
}

fn process_meta_path(dir: &Path, exec_id: &str) -> PathBuf {
    dir.join(format!("{}.json", exec_id))
}

fn read_process_metadata(dir: &Path, exec_id: &str) -> Result<ProcessMetadata> {
    let path = process_meta_path(dir, exec_id);
    let raw =
        fs::read_to_string(&path).with_context(|| format!("failed to read {}", path.display()))?;
    serde_json::from_str(&raw).context("failed to parse process metadata")
}

fn write_process_metadata(dir: &Path, metadata: &ProcessMetadata) -> Result<()> {
    let raw =
        serde_json::to_string_pretty(metadata).context("failed to serialize process metadata")?;
    fs::write(process_meta_path(dir, &metadata.exec_id), raw)
        .with_context(|| format!("failed to write process metadata for {}", metadata.exec_id))
}

fn read_file_lines_window(path: &Path, start: usize, limit: usize) -> Result<String> {
    if !path.exists() || limit == 0 {
        return Ok(String::new());
    }
    let text =
        fs::read_to_string(path).with_context(|| format!("failed to read {}", path.display()))?;
    let lines = text.lines().map(ToOwned::to_owned).collect::<Vec<_>>();
    let end = lines.len().saturating_sub(start);
    let begin = end.saturating_sub(limit);
    Ok(lines[begin..end].join("\n"))
}

fn read_exit_code(path: &Path) -> Option<i32> {
    fs::read_to_string(path)
        .ok()
        .and_then(|value| value.trim().parse::<i32>().ok())
}

fn process_is_running(pid: u32) -> bool {
    Command::new("sh")
        .arg("-c")
        .arg(format!("kill -0 {} 2>/dev/null", pid))
        .status()
        .map(|status| status.success())
        .unwrap_or(false)
}

fn terminate_process_pid(pid: u32) {
    let _ = Command::new("kill")
        .arg("-KILL")
        .arg(pid.to_string())
        .status();
}

fn spawn_pipe_copy_thread<R>(mut reader: R, path: PathBuf)
where
    R: Read + Send + 'static,
{
    thread::spawn(move || {
        if let Ok(mut file) = fs::File::create(&path) {
            let _ = std::io::copy(&mut reader, &mut file);
            let _ = file.flush();
        }
    });
}

fn record_exit_code(path: &Path, code: i32) -> Result<()> {
    fs::write(path, code.to_string())
        .with_context(|| format!("failed to write exit code to {}", path.display()))
}

fn spawn_managed_process(state_dir: &Path, command: &str, cwd: &Path) -> Result<ProcessMetadata> {
    let exec_id = Uuid::new_v4().to_string();
    let stdout_path = state_dir.join(format!("{}.stdout", exec_id));
    let stderr_path = state_dir.join(format!("{}.stderr", exec_id));
    let exit_code_path = state_dir.join(format!("{}.exit", exec_id));
    let mut child = Command::new("sh")
        .arg("-c")
        .arg(command)
        .current_dir(cwd)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .with_context(|| format!("failed to spawn shell in {}", cwd.display()))?;
    let stdin = child.stdin.take();
    let pid = child.id();
    if let Some(stdout) = child.stdout.take() {
        spawn_pipe_copy_thread(stdout, stdout_path.clone());
    }
    if let Some(stderr) = child.stderr.take() {
        spawn_pipe_copy_thread(stderr, stderr_path.clone());
    }
    let metadata = ProcessMetadata {
        exec_id: exec_id.clone(),
        pid,
        command: command.to_string(),
        cwd: cwd.display().to_string(),
        stdout_path: stdout_path.display().to_string(),
        stderr_path: stderr_path.display().to_string(),
        exit_code_path: exit_code_path.display().to_string(),
    };
    write_process_metadata(state_dir, &metadata)?;
    let completion_receiver = {
        let (completion_sender, completion_receiver) = crossbeam_channel::bounded(1);
        let exec_id = metadata.exec_id.clone();
        let exit_code_path = metadata.exit_code_path.clone();
        thread::spawn(move || {
            let code = child
                .wait()
                .ok()
                .and_then(|status| status.code())
                .unwrap_or(-1);
            let _ = record_exit_code(Path::new(&exit_code_path), code);
            let _ = completion_sender.send(code);
            let mut registry = live_processes().lock().unwrap();
            if let Some(process) = registry.get_mut(&exec_id) {
                process.completion_receiver = None;
            }
            registry.remove(&exec_id);
        });
        completion_receiver
    };
    live_processes().lock().unwrap().insert(
        exec_id,
        LiveManagedProcess {
            stdin,
            metadata: ProcessMetadata {
                exec_id: metadata.exec_id.clone(),
                pid: metadata.pid,
                command: metadata.command.clone(),
                cwd: metadata.cwd.clone(),
                stdout_path: metadata.stdout_path.clone(),
                stderr_path: metadata.stderr_path.clone(),
                exit_code_path: metadata.exit_code_path.clone(),
            },
            completion_receiver: Some(completion_receiver),
        },
    );
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
) -> Result<Value> {
    let metadata = match read_process_metadata(state_dir, exec_id) {
        Ok(metadata) => metadata,
        Err(_) => return Err(process_missing_error(exec_id)),
    };
    let exit_code = read_exit_code(Path::new(&metadata.exit_code_path));
    let running = if exit_code.is_some() {
        false
    } else {
        process_is_running(metadata.pid)
    };
    if !running && exit_code.is_none() {
        return Err(process_missing_error(exec_id));
    }
    Ok(json!({
        "exec_id": metadata.exec_id,
        "pid": metadata.pid,
        "command": metadata.command,
        "cwd": metadata.cwd,
        "running": running,
        "completed": !running,
        "returncode": exit_code,
        "stdout": read_file_lines_window(Path::new(&metadata.stdout_path), start, limit)?,
        "stderr": read_file_lines_window(Path::new(&metadata.stderr_path), start, limit)?,
    }))
}

pub fn terminate_all_managed_processes() -> Result<()> {
    let mut registry = live_processes().lock().unwrap();
    let processes = std::mem::take(&mut *registry)
        .into_values()
        .collect::<Vec<_>>();
    drop(registry);
    for process in processes {
        let meta_path = process_meta_path(
            Path::new(&process.metadata.stdout_path)
                .parent()
                .unwrap_or_else(|| Path::new(".")),
            &process.metadata.exec_id,
        );
        let _ = fs::remove_file(meta_path);
        let _ = fs::remove_file(&process.metadata.exit_code_path);
        let _ = fs::remove_file(&process.metadata.stdout_path);
        let _ = fs::remove_file(&process.metadata.stderr_path);
        terminate_process_pid(process.metadata.pid);
    }
    Ok(())
}

fn wait_for_managed_process(
    state_dir: &Path,
    exec_id: &str,
    wait_timeout_seconds: f64,
    input: Option<&str>,
    start: usize,
    limit: usize,
    cancel_flag: Option<&Arc<InterruptSignal>>,
) -> Result<Value> {
    let mut pending_input = input.map(ToOwned::to_owned);
    if let Some(cancel_flag) = cancel_flag
        && cancel_flag.is_requested()
    {
        let snapshot = read_process_snapshot(state_dir, exec_id, start, limit)?;
        return Ok(json!({
            "interrupted": true,
            "reason": "agent_turn_timeout_observation_requested",
            "process": snapshot
        }));
    }

    let (metadata, completion_receiver) = {
        let mut registry = live_processes().lock().unwrap();
        let Some(process) = registry.get_mut(exec_id) else {
            drop(registry);
            return read_process_snapshot(state_dir, exec_id, start, limit)
                .or_else(|_| Err(process_missing_error(exec_id)));
        };
        if let Some(input) = pending_input.take()
            && let Some(stdin) = process.stdin.as_mut()
        {
            stdin
                .write_all(input.as_bytes())
                .with_context(|| format!("failed to write stdin for exec process {}", exec_id))?;
            stdin
                .flush()
                .with_context(|| format!("failed to flush stdin for exec process {}", exec_id))?;
        }
        let metadata = ProcessMetadata {
            exec_id: process.metadata.exec_id.clone(),
            pid: process.metadata.pid,
            command: process.metadata.command.clone(),
            cwd: process.metadata.cwd.clone(),
            stdout_path: process.metadata.stdout_path.clone(),
            stderr_path: process.metadata.stderr_path.clone(),
            exit_code_path: process.metadata.exit_code_path.clone(),
        };
        let completion_receiver = process
            .completion_receiver
            .take()
            .ok_or_else(|| anyhow!("exec process {} is already being awaited", exec_id))?;
        (metadata, completion_receiver)
    };

    let timeout_receiver = crossbeam_channel::after(Duration::from_secs_f64(wait_timeout_seconds));
    let cancel_receiver = cancel_flag.map(|signal| signal.subscribe());
    let completed = match cancel_receiver {
        Some(cancel_receiver) => crossbeam_channel::select! {
            recv(completion_receiver) -> exit_code => Some(exit_code),
            recv(cancel_receiver) -> _ => None,
            recv(timeout_receiver) -> _ => None,
        },
        None => crossbeam_channel::select! {
            recv(completion_receiver) -> exit_code => Some(exit_code),
            recv(timeout_receiver) -> _ => None,
        },
    };

    if let Some(exit_code) = completed {
        let code = exit_code.context("exec process worker disconnected")?;
        return Ok(json!({
            "exec_id": metadata.exec_id,
            "pid": metadata.pid,
            "command": metadata.command,
            "cwd": metadata.cwd,
            "running": false,
            "completed": true,
            "returncode": code,
            "stdout": read_file_lines_window(Path::new(&metadata.stdout_path), start, limit)?,
            "stderr": read_file_lines_window(Path::new(&metadata.stderr_path), start, limit)?,
        }));
    }

    if let Ok(code) = completion_receiver.try_recv() {
        return Ok(json!({
            "exec_id": metadata.exec_id,
            "pid": metadata.pid,
            "command": metadata.command,
            "cwd": metadata.cwd,
            "running": false,
            "completed": true,
            "returncode": code,
            "stdout": read_file_lines_window(Path::new(&metadata.stdout_path), start, limit)?,
            "stderr": read_file_lines_window(Path::new(&metadata.stderr_path), start, limit)?,
        }));
    }

    {
        let mut registry = live_processes().lock().unwrap();
        if let Some(process) = registry.get_mut(exec_id) {
            process.completion_receiver = Some(completion_receiver);
        }
    }

    let snapshot = json!({
        "exec_id": metadata.exec_id,
        "pid": metadata.pid,
        "command": metadata.command,
        "cwd": metadata.cwd,
        "running": true,
        "completed": false,
        "returncode": Value::Null,
        "stdout": read_file_lines_window(Path::new(&metadata.stdout_path), start, limit)?,
        "stderr": read_file_lines_window(Path::new(&metadata.stderr_path), start, limit)?,
    });

    if cancel_flag.is_some_and(|signal| signal.is_requested()) {
        return Ok(json!({
            "interrupted": true,
            "reason": "agent_turn_timeout_observation_requested",
            "process": snapshot
        }));
    }

    Ok(json!({
        "exec_id": metadata.exec_id,
        "pid": metadata.pid,
        "command": metadata.command,
        "cwd": metadata.cwd,
        "running": true,
        "completed": false,
        "returncode": Value::Null,
        "wait_timed_out": true,
        "stdout": read_file_lines_window(Path::new(&metadata.stdout_path), start, limit)?,
        "stderr": read_file_lines_window(Path::new(&metadata.stderr_path), start, limit)?,
    }))
}

fn exec_start_tool(
    workspace_root: PathBuf,
    runtime_state_root: PathBuf,
    cancel_flag: Option<Arc<InterruptSignal>>,
) -> Tool {
    Tool::new(
        "exec_start",
        "Start a shell command. Wait for at most wait_timeout_seconds before returning. If it finishes in time, return the result immediately. Otherwise keep it running and return an exec_id.",
        json!({
            "type": "object",
            "properties": {
                "command": {"type": "string"},
                "wait_timeout_seconds": {"type": "number"},
                "cwd": {"type": "string"},
                "include_stdout": {"type": "boolean"},
                "start": {"type": "integer"},
                "limit": {"type": "integer"}
            },
            "required": ["command", "wait_timeout_seconds"],
            "additionalProperties": false
        }),
        move |arguments| {
            let arguments = arguments
                .as_object()
                .ok_or_else(|| anyhow!("tool arguments must be an object"))?;
            let command = string_arg(arguments, "command")?;
            let wait_timeout_seconds = f64_arg(arguments, "wait_timeout_seconds")?;
            let include_stdout = arguments
                .get("include_stdout")
                .and_then(Value::as_bool)
                .unwrap_or(true);
            let start = usize_arg_with_default(arguments, "start", 0)?;
            let limit = usize_arg_with_default(arguments, "limit", 20)?;
            let cwd = arguments
                .get("cwd")
                .and_then(Value::as_str)
                .map(|value| resolve_path(value, &workspace_root))
                .unwrap_or_else(|| workspace_root.clone());
            let state_dir = ensure_process_state_dir(&runtime_state_root)?;
            let metadata = spawn_managed_process(&state_dir, &command, &cwd)?;
            let mut result = wait_for_managed_process(
                &state_dir,
                &metadata.exec_id,
                wait_timeout_seconds,
                None,
                start,
                limit,
                cancel_flag.as_ref(),
            )?;
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

fn exec_observe_tool(
    runtime_state_root: PathBuf,
    cancel_flag: Option<Arc<InterruptSignal>>,
) -> Tool {
    Tool::new(
        "exec_observe",
        "Observe the latest output of a previously started exec process by exec_id. start=0 and limit=2 means the last two lines.",
        json!({
            "type": "object",
            "properties": {
                "exec_id": {"type": "string"},
                "start": {"type": "integer"},
                "limit": {"type": "integer"}
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
            let state_dir = ensure_process_state_dir(&runtime_state_root)?;
            let _ = &cancel_flag;
            read_process_snapshot(&state_dir, &exec_id, start, limit)
        },
    )
}

fn exec_wait_tool(runtime_state_root: PathBuf, cancel_flag: Option<Arc<InterruptSignal>>) -> Tool {
    Tool::new(
        "exec_wait",
        "Wait on a previously started exec process by exec_id. Optionally write input to stdin before waiting. If the process does not finish before wait_timeout_seconds, return immediately and leave it running.",
        json!({
            "type": "object",
            "properties": {
                "exec_id": {"type": "string"},
                "wait_timeout_seconds": {"type": "number"},
                "input": {"type": "string"},
                "include_stdout": {"type": "boolean"},
                "start": {"type": "integer"},
                "limit": {"type": "integer"}
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
                cancel_flag.as_ref(),
            )?;
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

fn exec_kill_tool(runtime_state_root: PathBuf, cancel_flag: Option<Arc<InterruptSignal>>) -> Tool {
    Tool::new(
        "exec_kill",
        "Immediately stop a previously started exec process by exec_id.",
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
            let _state_dir = ensure_process_state_dir(&runtime_state_root)?;
            let _ = &cancel_flag;
            let metadata = {
                let mut registry = live_processes().lock().unwrap();
                let Some(process) = registry.remove(&exec_id) else {
                    return Err(process_missing_error(&exec_id));
                };
                ProcessMetadata {
                    exec_id: process.metadata.exec_id,
                    pid: process.metadata.pid,
                    command: process.metadata.command,
                    cwd: process.metadata.cwd,
                    stdout_path: process.metadata.stdout_path,
                    stderr_path: process.metadata.stderr_path,
                    exit_code_path: process.metadata.exit_code_path,
                }
            };
            terminate_process_pid(metadata.pid);
            record_exit_code(Path::new(&metadata.exit_code_path), -9)?;
            Ok(json!({
                "exec_id": metadata.exec_id,
                "pid": metadata.pid,
                "command": metadata.command,
                "cwd": metadata.cwd,
                "running": false,
                "completed": true,
                "killed": true,
                "returncode": -9,
            }))
        },
    )
}

fn exec_tool(
    workspace_root: PathBuf,
    runtime_state_root: PathBuf,
    cancel_flag: Option<Arc<InterruptSignal>>,
) -> Tool {
    let exec_start = exec_start_tool(workspace_root, runtime_state_root, cancel_flag);
    Tool::new(
        "exec",
        "Deprecated alias for exec_start. Start a shell command and optionally wait for completion.",
        exec_start.parameters.clone(),
        move |arguments| exec_start.invoke(arguments),
    )
}

fn process_tool(runtime_state_root: PathBuf, cancel_flag: Option<Arc<InterruptSignal>>) -> Tool {
    Tool::new(
        "process",
        "Deprecated compatibility wrapper around exec_observe and exec_kill.",
        json!({
            "type": "object",
            "properties": {
                "action": {"type": "string", "enum": ["list", "inspect", "terminate"]},
                "exec_id": {"type": "string"},
                "process_id": {"type": "string"},
                "start": {"type": "integer"},
                "limit": {"type": "integer"}
            },
            "required": ["action"],
            "additionalProperties": false
        }),
        move |arguments| {
            let arguments = arguments
                .as_object()
                .ok_or_else(|| anyhow!("tool arguments must be an object"))?;
            let action = string_arg(arguments, "action")?;
            let exec_id = arguments
                .get("exec_id")
                .and_then(Value::as_str)
                .or_else(|| arguments.get("process_id").and_then(Value::as_str))
                .map(ToOwned::to_owned);
            let state_dir = ensure_process_state_dir(&runtime_state_root)?;
            match action.as_str() {
                "list" => {
                    let mut items = Vec::new();
                    for entry in fs::read_dir(&state_dir)
                        .with_context(|| format!("failed to read {}", state_dir.display()))?
                    {
                        let entry = entry?;
                        let path = entry.path();
                        if path.extension().and_then(|value| value.to_str()) != Some("json") {
                            continue;
                        }
                        let metadata = read_process_metadata(
                            &state_dir,
                            path.file_stem()
                                .and_then(|value| value.to_str())
                                .unwrap_or_default(),
                        )?;
                        let exit_code = read_exit_code(Path::new(&metadata.exit_code_path));
                        let running = if exit_code.is_some() {
                            false
                        } else {
                            process_is_running(metadata.pid)
                        };
                        items.push(json!({
                            "exec_id": metadata.exec_id,
                            "pid": metadata.pid,
                            "command": metadata.command,
                            "cwd": metadata.cwd,
                            "running": running,
                            "completed": !running,
                            "returncode": exit_code
                        }));
                    }
                    Ok(json!({ "processes": items }))
                }
                "inspect" => {
                    let exec_id = exec_id.ok_or_else(|| anyhow!("missing exec_id"))?;
                    exec_observe_tool(runtime_state_root.clone(), cancel_flag.clone()).invoke(
                        json!({
                            "exec_id": exec_id,
                            "start": usize_arg_with_default(arguments, "start", 0)?,
                            "limit": usize_arg_with_default(arguments, "limit", 20)?,
                        }),
                    )
                }
                "terminate" => {
                    let exec_id = exec_id.ok_or_else(|| anyhow!("missing exec_id"))?;
                    exec_kill_tool(runtime_state_root.clone(), cancel_flag.clone()).invoke(json!({
                        "exec_id": exec_id,
                    }))
                }
                _ => Err(anyhow!("unsupported process action {}", action)),
            }
        },
    )
}

fn apply_patch_tool(workspace_root: PathBuf, cancel_flag: Option<Arc<InterruptSignal>>) -> Tool {
    Tool::new(
        "apply_patch",
        "Apply a unified diff patch inside the workspace using git apply. The patch must be a valid unified diff. The model must choose timeout_seconds.",
        json!({
            "type": "object",
            "properties": {
                "patch": {"type": "string"},
                "timeout_seconds": {"type": "number"},
                "strip": {"type": "integer"},
                "reverse": {"type": "boolean"},
                "check": {"type": "boolean"}
            },
            "required": ["patch", "timeout_seconds"],
            "additionalProperties": false
        }),
        move |arguments| {
            let arguments = arguments
                .as_object()
                .ok_or_else(|| anyhow!("tool arguments must be an object"))?;
            let patch = string_arg(arguments, "patch")?;
            let timeout_seconds = f64_arg(arguments, "timeout_seconds")?;
            let strip = usize_arg_with_default(arguments, "strip", 0)?;
            let reverse = arguments
                .get("reverse")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            let check = arguments
                .get("check")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            let patch_workspace_root = workspace_root.clone();

            let patch_cancel_flag = cancel_flag.clone();
            with_timeout_and_cancel(timeout_seconds + 1.0, cancel_flag.clone(), move || {
                let mut command = Command::new("git");
                command
                    .arg("apply")
                    .arg("--recount")
                    .arg("--whitespace=nowarn")
                    .arg(format!("-p{}", strip))
                    .current_dir(&patch_workspace_root)
                    .stdin(Stdio::piped())
                    .stdout(Stdio::piped())
                    .stderr(Stdio::piped());
                if reverse {
                    command.arg("--reverse");
                }
                if check {
                    command.arg("--check");
                }
                let mut child = command.spawn().context("failed to spawn git apply")?;
                child
                    .stdin
                    .as_mut()
                    .ok_or_else(|| anyhow!("failed to open git apply stdin"))?
                    .write_all(patch.as_bytes())
                    .context("failed to write patch to git apply stdin")?;
                let _ = child.stdin.take();
                let output = wait_for_child_with_timeout(
                    child,
                    timeout_seconds,
                    "git apply",
                    patch_cancel_flag.as_ref(),
                )?;
                Ok(json!({
                    "applied": output.status.success(),
                    "returncode": output.status.code().unwrap_or(-1),
                    "stdout": String::from_utf8_lossy(&output.stdout),
                    "stderr": String::from_utf8_lossy(&output.stderr)
                }))
            })
        },
    )
}

fn strip_html_tags(body: &str) -> String {
    let mut output = String::with_capacity(body.len());
    let mut in_tag = false;
    for ch in body.chars() {
        match ch {
            '<' => in_tag = true,
            '>' => {
                in_tag = false;
                output.push(' ');
            }
            _ if !in_tag => output.push(ch),
            _ => {}
        }
    }
    output.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn infer_image_media_type(path: &Path) -> String {
    match path
        .extension()
        .and_then(|value| value.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase()
        .as_str()
    {
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "bmp" => "image/bmp",
        _ => "image/png",
    }
    .to_string()
}

fn image_to_data_url(path: &Path) -> Result<String> {
    let bytes = fs::read(path).with_context(|| format!("failed to read {}", path.display()))?;
    let encoded = base64::engine::general_purpose::STANDARD.encode(bytes);
    Ok(format!(
        "data:{};base64,{}",
        infer_image_media_type(path),
        encoded
    ))
}

fn chat_message_text(message: &ChatMessage) -> String {
    match &message.content {
        Some(Value::String(text)) => text.clone(),
        Some(Value::Array(items)) => items
            .iter()
            .filter_map(|item| {
                let object = item.as_object()?;
                let item_type = object.get("type")?.as_str()?;
                match item_type {
                    "text" | "input_text" | "output_text" => {
                        object.get("text")?.as_str().map(ToOwned::to_owned)
                    }
                    _ => None,
                }
            })
            .collect::<Vec<_>>()
            .join("\n"),
        Some(other) => other.to_string(),
        None => String::new(),
    }
}

fn image_tool(
    workspace_root: PathBuf,
    upstream: UpstreamConfig,
    image_tool_upstream: Option<UpstreamConfig>,
    cancel_flag: Option<Arc<InterruptSignal>>,
) -> Tool {
    Tool::new(
        "image",
        "Inspect a local image file with the model's multimodal capability and answer a focused question about it. Use this for local images that are not already directly visible in the current user turn. The model must choose timeout_seconds.",
        json!({
            "type": "object",
            "properties": {
                "path": {"type": "string"},
                "question": {"type": "string"},
                "timeout_seconds": {"type": "number"}
            },
            "required": ["path", "question", "timeout_seconds"],
            "additionalProperties": false
        }),
        move |arguments| {
            let arguments = arguments
                .as_object()
                .ok_or_else(|| anyhow!("tool arguments must be an object"))?;
            let path = resolve_path(&string_arg(arguments, "path")?, &workspace_root);
            let question = string_arg(arguments, "question")?;
            let timeout_seconds = f64_arg(arguments, "timeout_seconds")?;
            let upstream = image_tool_upstream
                .clone()
                .unwrap_or_else(|| upstream.clone());

            with_timeout_and_cancel(timeout_seconds, cancel_flag.clone(), move || {
                if !upstream.supports_vision_input {
                    return Err(anyhow!(
                        "the configured upstream model does not support multimodal image input"
                    ));
                }
                let data_url = image_to_data_url(&path)?;
                let outcome = create_chat_completion(
                    &UpstreamConfig {
                        timeout_seconds,
                        ..upstream.clone()
                    },
                    &[
                        ChatMessage::text(
                            "system",
                            "You inspect a local image for an agent runtime. Answer the user's question about the image directly and concisely. If relevant visible text appears in the image, quote or transcribe it accurately.",
                        ),
                        ChatMessage {
                            role: "user".to_string(),
                            content: Some(Value::Array(vec![
                                json!({
                                    "type": "text",
                                    "text": question
                                }),
                                json!({
                                    "type": "image_url",
                                    "image_url": {
                                        "url": data_url
                                    }
                                }),
                            ])),
                            name: None,
                            tool_call_id: None,
                            tool_calls: None,
                        },
                    ],
                    &[],
                    Some(Map::from_iter([(
                        "max_completion_tokens".to_string(),
                        Value::from(800_u64),
                    )])),
                )?;
                Ok(json!({
                    "path": path.display().to_string(),
                    "answer": chat_message_text(&outcome.message),
                }))
            })
        },
    )
}

fn web_fetch_tool() -> Tool {
    Tool::new(
        "web_fetch",
        "Fetch a web page or HTTP resource and return a readable text body. The model must choose timeout_seconds.",
        json!({
            "type": "object",
            "properties": {
                "url": {"type": "string"},
                "timeout_seconds": {"type": "number"},
                "max_chars": {"type": "integer"},
                "headers": {"type": "object"}
            },
            "required": ["url", "timeout_seconds"],
            "additionalProperties": false
        }),
        move |arguments| {
            let arguments = arguments
                .as_object()
                .ok_or_else(|| anyhow!("tool arguments must be an object"))?;
            let url = string_arg(arguments, "url")?;
            let timeout_seconds = f64_arg(arguments, "timeout_seconds")?;
            let max_chars = usize_arg_with_default(arguments, "max_chars", 20_000)?;
            let headers = arguments
                .get("headers")
                .and_then(Value::as_object)
                .cloned()
                .unwrap_or_default();

            let client = reqwest::blocking::Client::builder()
                .timeout(Duration::from_secs_f64(timeout_seconds))
                .build()
                .context("failed to construct http client")?;
            let mut request = client.get(&url);
            for (key, value) in headers {
                if let Some(value) = value.as_str() {
                    request = request.header(&key, value);
                }
            }
            let response = request.send().context("web fetch failed")?;
            let status = response.status().as_u16();
            let final_url = response.url().to_string();
            let content_type = response
                .headers()
                .get(reqwest::header::CONTENT_TYPE)
                .and_then(|value| value.to_str().ok())
                .unwrap_or("")
                .to_string();
            let body = response.text().context("failed to read fetched body")?;
            let cleaned = if content_type.contains("html") {
                strip_html_tags(&body)
            } else {
                body
            };
            let truncated = cleaned.chars().count() > max_chars;
            let content = cleaned.chars().take(max_chars).collect::<String>();
            Ok(json!({
                "status": status,
                "url": final_url,
                "content_type": content_type,
                "content": content,
                "truncated": truncated
            }))
        },
    )
}

fn download_file_tool(workspace_root: PathBuf, cancel_flag: Option<Arc<InterruptSignal>>) -> Tool {
    Tool::new(
        "download_file",
        "Download an HTTP resource and save it to a local file. Use this for binary files such as images or PDFs. The model must choose timeout_seconds.",
        json!({
            "type": "object",
            "properties": {
                "url": {"type": "string"},
                "path": {"type": "string"},
                "timeout_seconds": {"type": "number"},
                "headers": {"type": "object"},
                "overwrite": {"type": "boolean"}
            },
            "required": ["url", "path", "timeout_seconds"],
            "additionalProperties": false
        }),
        move |arguments| {
            let arguments = arguments
                .as_object()
                .ok_or_else(|| anyhow!("tool arguments must be an object"))?;
            let url = string_arg(arguments, "url")?;
            let path = resolve_path(&string_arg(arguments, "path")?, &workspace_root);
            let timeout_seconds = f64_arg(arguments, "timeout_seconds")?;
            let overwrite = arguments
                .get("overwrite")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            let headers = arguments
                .get("headers")
                .and_then(Value::as_object)
                .cloned()
                .unwrap_or_default();

            with_timeout_and_cancel(timeout_seconds, cancel_flag.clone(), move || {
                if path.exists() && !overwrite {
                    return Err(anyhow!(
                        "destination already exists and overwrite=false: {}",
                        path.display()
                    ));
                }
                if let Some(parent) = path.parent() {
                    fs::create_dir_all(parent).with_context(|| {
                        format!("failed to create parent directory {}", parent.display())
                    })?;
                }

                let client = reqwest::blocking::Client::builder()
                    .timeout(Duration::from_secs_f64(timeout_seconds))
                    .build()
                    .context("failed to construct http client")?;
                let mut request = client.get(&url);
                for (key, value) in headers {
                    if let Some(value) = value.as_str() {
                        request = request.header(&key, value);
                    }
                }
                let response = request.send().context("download request failed")?;
                let status = response.status();
                let final_url = response.url().to_string();
                if !status.is_success() {
                    let status_text = status.to_string();
                    let body = response
                        .text()
                        .unwrap_or_else(|_| "<unreadable error body>".to_string());
                    return Err(anyhow!("download failed with {}: {}", status_text, body));
                }
                let content_type = response
                    .headers()
                    .get(reqwest::header::CONTENT_TYPE)
                    .and_then(|value| value.to_str().ok())
                    .unwrap_or("")
                    .to_string();
                let bytes = response.bytes().context("failed to read downloaded body")?;
                fs::write(&path, &bytes).with_context(|| {
                    format!("failed to write downloaded file {}", path.display())
                })?;
                Ok(json!({
                    "status": status.as_u16(),
                    "url": final_url,
                    "content_type": content_type,
                    "path": path.display().to_string(),
                    "size_bytes": bytes.len(),
                }))
            })
        },
    )
}

fn default_external_web_search_config() -> ExternalWebSearchConfig {
    ExternalWebSearchConfig {
        base_url: "https://openrouter.ai/api/v1".to_string(),
        model: "perplexity/sonar".to_string(),
        api_key: None,
        api_key_env: "OPENROUTER_API_KEY".to_string(),
        chat_completions_path: "/chat/completions".to_string(),
        timeout_seconds: 60.0,
        headers: Map::new(),
    }
}

fn extract_text_content(value: &Value) -> String {
    value
        .get("choices")
        .and_then(Value::as_array)
        .and_then(|choices| choices.first())
        .and_then(|choice| choice.get("message"))
        .and_then(|message| message.get("content"))
        .map(|content| match content {
            Value::String(text) => text.clone(),
            Value::Array(items) => items
                .iter()
                .filter_map(|item| {
                    let object = item.as_object()?;
                    let item_type = object.get("type")?.as_str()?;
                    match item_type {
                        "text" | "input_text" | "output_text" => {
                            object.get("text")?.as_str().map(ToOwned::to_owned)
                        }
                        _ => None,
                    }
                })
                .collect::<Vec<_>>()
                .join("\n"),
            other => other.to_string(),
        })
        .unwrap_or_default()
}

fn web_search_tool(search_config: ExternalWebSearchConfig) -> Tool {
    Tool::new(
        "web_search",
        "Search the web using the configured search provider and return an answer plus citations. The model must choose timeout_seconds.",
        json!({
            "type": "object",
            "properties": {
                "query": {"type": "string"},
                "timeout_seconds": {"type": "number"},
                "max_results": {"type": "integer"}
            },
            "required": ["query", "timeout_seconds"],
            "additionalProperties": false
        }),
        move |arguments| {
            let arguments = arguments
                .as_object()
                .ok_or_else(|| anyhow!("tool arguments must be an object"))?;
            let query = string_arg(arguments, "query")?;
            let timeout_seconds = f64_arg(arguments, "timeout_seconds")?;
            let max_results = usize_arg_with_default(arguments, "max_results", 8)?;
            let config = UpstreamConfig {
                base_url: search_config.base_url.clone(),
                model: search_config.model.clone(),
                supports_vision_input: false,
                api_key: search_config.api_key.clone(),
                api_key_env: search_config.api_key_env.clone(),
                chat_completions_path: search_config.chat_completions_path.clone(),
                timeout_seconds,
                context_window_tokens: 32_000,
                cache_control: None,
                reasoning: None,
                headers: search_config.headers.clone(),
                native_web_search: None,
                external_web_search: None,
            };
            let client = reqwest::blocking::Client::builder()
                .timeout(Duration::from_secs_f64(config.timeout_seconds))
                .build()
                .context("failed to construct web search client")?;
            let mut payload = Map::new();
            payload.insert("model".to_string(), Value::String(config.model.clone()));
            payload.insert(
                "messages".to_string(),
                json!([
                    {
                        "role": "system",
                        "content": "Search the web and answer the query. Include source URLs in the answer when available."
                    },
                    {
                        "role": "user",
                        "content": query
                    }
                ]),
            );
            let mut request = client
                .post(format!(
                    "{}{}",
                    config.base_url.trim_end_matches('/'),
                    if config.chat_completions_path.starts_with('/') {
                        config.chat_completions_path.clone()
                    } else {
                        format!("/{}", config.chat_completions_path)
                    }
                ))
                .json(&Value::Object(payload));
            if let Some(api_key) = config
                .api_key
                .clone()
                .or_else(|| std::env::var(&config.api_key_env).ok())
            {
                request = request.bearer_auth(api_key);
            }
            for (key, value) in &config.headers {
                if let Some(value) = value.as_str() {
                    request = request.header(key, value);
                }
            }
            let response = request.send().context("web search request failed")?;
            let status = response.status();
            let body = response
                .text()
                .context("failed to read web search response")?;
            if !status.is_success() {
                return Err(anyhow!(
                    "web search upstream failed with {}: {}",
                    status,
                    body
                ));
            }
            let value: Value =
                serde_json::from_str(&body).context("failed to parse web search response")?;
            let citations = value
                .get("citations")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default()
                .into_iter()
                .take(max_results)
                .collect::<Vec<_>>();
            Ok(json!({
                "query": arguments.get("query").and_then(Value::as_str).unwrap_or_default(),
                "answer": extract_text_content(&value),
                "citations": citations
            }))
        },
    )
}

fn run_shell_tool(
    workspace_root: PathBuf,
    runtime_state_root: PathBuf,
    cancel_flag: Option<Arc<InterruptSignal>>,
) -> Tool {
    let exec = exec_tool(workspace_root, runtime_state_root, cancel_flag);
    Tool::new(
        "run_shell",
        "Deprecated alias for exec. Execute a shell command. The model must choose timeout_seconds.",
        exec.parameters.clone(),
        move |arguments| exec.invoke(arguments),
    )
}

fn http_request_tool() -> Tool {
    let fetch = web_fetch_tool();
    Tool::new(
        "http_request",
        "Deprecated alias for web_fetch. Fetch an HTTP resource. The model must choose timeout_seconds.",
        fetch.parameters.clone(),
        move |arguments| fetch.invoke(arguments),
    )
}

fn load_skill_tool(
    skills: &[SkillMetadata],
    cancel_flag: Option<Arc<InterruptSignal>>,
) -> Result<Tool> {
    let skill_index = build_skill_index(skills)?;
    let available_skills = skill_index.keys().cloned().collect::<Vec<_>>();
    Ok(Tool::new(
        "load_skill",
        "Load the SKILL.md instructions for a named skill. Use exact skill names from the preloaded metadata and choose timeout_seconds yourself.",
        json!({
            "type": "object",
            "properties": {
                "skill_name": {"type": "string", "enum": available_skills},
                "timeout_seconds": {"type": "number"}
            },
            "required": ["skill_name", "timeout_seconds"],
            "additionalProperties": false
        }),
        move |arguments| {
            let arguments = arguments
                .as_object()
                .ok_or_else(|| anyhow!("tool arguments must be an object"))?;
            let skill_name = string_arg(arguments, "skill_name")?;
            let timeout_seconds = f64_arg(arguments, "timeout_seconds")?;
            let skill_index = skill_index.clone();
            with_timeout_and_cancel(timeout_seconds, cancel_flag.clone(), move || {
                let (skill, content) = load_skill_by_name(&skill_index, &skill_name)?;
                Ok(json!({
                    "name": skill.name,
                    "description": skill.description,
                    "content": content
                }))
            })
        },
    ))
}

pub fn build_tool_registry(
    enabled_tools: &[String],
    workspace_root: &Path,
    runtime_state_root: &Path,
    upstream: &UpstreamConfig,
    image_tool_upstream: Option<&UpstreamConfig>,
    skills: &[SkillMetadata],
    extra_tools: &[Tool],
) -> Result<BTreeMap<String, Tool>> {
    build_tool_registry_with_cancel(
        enabled_tools,
        workspace_root,
        runtime_state_root,
        upstream,
        image_tool_upstream,
        skills,
        extra_tools,
        None,
    )
}

pub fn build_tool_registry_with_cancel(
    enabled_tools: &[String],
    workspace_root: &Path,
    runtime_state_root: &Path,
    upstream: &UpstreamConfig,
    image_tool_upstream: Option<&UpstreamConfig>,
    skills: &[SkillMetadata],
    extra_tools: &[Tool],
    cancel_flag: Option<Arc<InterruptSignal>>,
) -> Result<BTreeMap<String, Tool>> {
    let mut builtins = BTreeMap::from([
        (
            "read_file".to_string(),
            read_file_tool(workspace_root.to_path_buf(), cancel_flag.clone()),
        ),
        (
            "write_file".to_string(),
            write_file_tool(workspace_root.to_path_buf(), cancel_flag.clone()),
        ),
        (
            "run_shell".to_string(),
            run_shell_tool(
                workspace_root.to_path_buf(),
                runtime_state_root.to_path_buf(),
                cancel_flag.clone(),
            ),
        ),
        (
            "edit".to_string(),
            edit_tool(workspace_root.to_path_buf(), cancel_flag.clone()),
        ),
        (
            "exec_start".to_string(),
            exec_start_tool(
                workspace_root.to_path_buf(),
                runtime_state_root.to_path_buf(),
                cancel_flag.clone(),
            ),
        ),
        (
            "exec_observe".to_string(),
            exec_observe_tool(runtime_state_root.to_path_buf(), cancel_flag.clone()),
        ),
        (
            "exec_wait".to_string(),
            exec_wait_tool(runtime_state_root.to_path_buf(), cancel_flag.clone()),
        ),
        (
            "exec_kill".to_string(),
            exec_kill_tool(runtime_state_root.to_path_buf(), cancel_flag.clone()),
        ),
        (
            "exec".to_string(),
            exec_tool(
                workspace_root.to_path_buf(),
                runtime_state_root.to_path_buf(),
                cancel_flag.clone(),
            ),
        ),
        (
            "process".to_string(),
            process_tool(runtime_state_root.to_path_buf(), cancel_flag.clone()),
        ),
        (
            "apply_patch".to_string(),
            apply_patch_tool(workspace_root.to_path_buf(), cancel_flag.clone()),
        ),
        (
            "image".to_string(),
            image_tool(
                workspace_root.to_path_buf(),
                upstream.clone(),
                image_tool_upstream.cloned(),
                cancel_flag.clone(),
            ),
        ),
        (
            "download_file".to_string(),
            download_file_tool(workspace_root.to_path_buf(), cancel_flag.clone()),
        ),
        ("web_fetch".to_string(), web_fetch_tool()),
        ("http_request".to_string(), http_request_tool()),
    ]);
    let native_web_search_enabled = upstream
        .native_web_search
        .as_ref()
        .is_some_and(|settings| settings.enabled);
    if !native_web_search_enabled {
        let web_search_config = upstream
            .external_web_search
            .clone()
            .unwrap_or_else(default_external_web_search_config);
        builtins.insert("web_search".to_string(), web_search_tool(web_search_config));
    }

    let mut registry = BTreeMap::new();
    for tool_name in enabled_tools {
        if native_web_search_enabled && tool_name == "web_search" {
            continue;
        }
        let tool = builtins
            .get(tool_name)
            .cloned()
            .ok_or_else(|| anyhow!("unknown built-in tool: {}", tool_name))?;
        registry.insert(tool.name.clone(), tool);
    }

    if !skills.is_empty() {
        let skill_tool = load_skill_tool(skills, cancel_flag)?;
        registry.insert(skill_tool.name.clone(), skill_tool);
    }

    for tool in extra_tools {
        if registry.contains_key(&tool.name) {
            return Err(anyhow!("tool name collision: {}", tool.name));
        }
        registry.insert(tool.name.clone(), tool.clone());
    }
    Ok(registry)
}

pub fn execute_tool_call(
    registry: &BTreeMap<String, Tool>,
    tool_name: &str,
    raw_arguments: Option<&str>,
) -> String {
    let Some(tool) = registry.get(tool_name) else {
        return normalize_tool_result(json!({"error": format!("unknown tool: {}", tool_name)}));
    };

    execute_tool(tool, raw_arguments)
}

pub fn execute_tool(tool: &Tool, raw_arguments: Option<&str>) -> String {
    let arguments = match raw_arguments {
        Some(text) if !text.trim().is_empty() => match serde_json::from_str::<Value>(text) {
            Ok(value) => value,
            Err(error) => {
                return normalize_tool_result(
                    json!({"error": format!("invalid tool arguments: {}", error)}),
                );
            }
        },
        _ => Value::Object(Map::new()),
    };

    match tool.invoke(arguments) {
        Ok(result) => normalize_tool_result(result),
        Err(error) => normalize_tool_result(json!({"error": error.to_string(), "tool": tool.name})),
    }
}

pub mod macro_support {
    use super::*;

    pub fn normalize_type_name(type_name: &str) -> String {
        type_name.chars().filter(|ch| !ch.is_whitespace()).collect()
    }

    pub fn schema_for_type_name(type_name: &str) -> Value {
        let normalized = normalize_type_name(type_name);
        let normalized = if normalized.starts_with("Option<") && normalized.ends_with('>') {
            &normalized["Option<".len()..normalized.len() - 1]
        } else {
            normalized.as_str()
        };

        match normalized {
            "i8" | "i16" | "i32" | "i64" | "i128" | "isize" | "u8" | "u16" | "u32" | "u64"
            | "u128" | "usize" => json!({"type": "integer"}),
            "f32" | "f64" => json!({"type": "number"}),
            "bool" => json!({"type": "boolean"}),
            "String" | "&str" | "str" => json!({"type": "string"}),
            _ if normalized.starts_with("Vec<") => json!({"type": "array"}),
            _ if normalized.starts_with("HashMap<")
                || normalized.starts_with("BTreeMap<")
                || normalized.starts_with("serde_json::Map<") =>
            {
                json!({"type": "object"})
            }
            _ if normalized == "Value" || normalized.ends_with("::Value") => json!({}),
            _ => json!({}),
        }
    }

    pub fn type_is_optional(type_name: &str) -> bool {
        let normalized = normalize_type_name(type_name);
        normalized.starts_with("Option<") && normalized.ends_with('>')
    }

    pub fn parse_argument<T: DeserializeOwned>(
        arguments: &Map<String, Value>,
        key: &str,
        optional: bool,
    ) -> Result<T> {
        let value = match arguments.get(key) {
            Some(value) => value.clone(),
            None if optional => Value::Null,
            None => return Err(anyhow!("missing required argument: {}", key)),
        };
        serde_json::from_value(value)
            .with_context(|| format!("failed to parse argument {} from JSON", key))
    }

    pub fn result_to_value<T: Serialize>(value: T) -> Result<Value> {
        serde_json::to_value(value).context("failed to serialize tool result")
    }

    pub fn arguments_object(arguments: &Value) -> Result<&Map<String, Value>> {
        arguments
            .as_object()
            .ok_or_else(|| anyhow!("tool arguments must be an object"))
    }
}

#[macro_export]
macro_rules! __agent_frame_build_tool_schema {
    ($( $arg:ident : $arg_ty:ty ),* $(,)?) => {{
        let mut properties = $crate::serde_json::Map::new();
        let mut required = Vec::<String>::new();
        $(
            properties.insert(
                stringify!($arg).to_string(),
                $crate::tooling::macro_support::schema_for_type_name(stringify!($arg_ty)),
            );
            if !$crate::tooling::macro_support::type_is_optional(stringify!($arg_ty)) {
                required.push(stringify!($arg).to_string());
            }
        )*
        $crate::serde_json::json!({
            "type": "object",
            "properties": properties,
            "required": required,
            "additionalProperties": false
        })
    }};
}

#[macro_export]
macro_rules! tool {
    (
        description: $description:expr,
        fn $fn_name:ident ( $( $arg:ident : $arg_ty:ty ),* $(,)? ) -> $ret:ty $body:block
    ) => {
        $crate::tool! {
            name: stringify!($fn_name),
            description: $description,
            fn $fn_name( $( $arg : $arg_ty ),* ) -> $ret $body
        }
    };
    (
        name: $name:expr,
        description: $description:expr,
        fn $fn_name:ident ( $( $arg:ident : $arg_ty:ty ),* $(,)? ) -> $ret:ty $body:block
    ) => {{
        $crate::tooling::Tool::new(
            $name,
            $description,
            $crate::__agent_frame_build_tool_schema!($( $arg : $arg_ty ),*),
            move |__tool_arguments| {
                let __tool_arguments = $crate::tooling::macro_support::arguments_object(&__tool_arguments)?;
                $(
                    let $arg: $arg_ty = $crate::tooling::macro_support::parse_argument::<$arg_ty>(
                        __tool_arguments,
                        stringify!($arg),
                        $crate::tooling::macro_support::type_is_optional(stringify!($arg_ty)),
                    )?;
                )*
                let __tool_result: $ret = { $body };
                $crate::tooling::macro_support::result_to_value(__tool_result)
            },
        )
    }};
}
