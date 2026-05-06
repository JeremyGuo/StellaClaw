use std::{
    io::{Read, Write},
    path::{Component, Path, PathBuf},
    process::{Command, Output, Stdio},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    thread,
    time::{Duration, Instant},
};

use serde_json::{Map, Value};
use thiserror::Error;

use super::ToolRemoteMode;

const REMOTE_JSON_TIMEOUT: Duration = Duration::from_secs(60);
const REMOTE_STDIN_TIMEOUT: Duration = Duration::from_secs(300);

#[derive(Clone, Default)]
pub(super) struct ToolCancellationToken {
    state: Arc<ToolCancellationState>,
}

#[derive(Default)]
struct ToolCancellationState {
    cancelled: AtomicBool,
}

impl ToolCancellationToken {
    pub(super) fn cancel(&self) {
        self.state.cancelled.store(true, Ordering::SeqCst);
    }

    pub(super) fn is_cancelled(&self) -> bool {
        self.state.cancelled.load(Ordering::SeqCst)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum ExecutionTarget {
    Local,
    RemoteSsh { host: String, cwd: Option<String> },
}

#[derive(Debug, Error)]
pub(super) enum LocalToolError {
    #[error("tool batch {0} is empty")]
    EmptyBatch(String),
    #[error("invalid tool arguments: {0}")]
    InvalidArguments(String),
    #[error("unsupported tool: {0}")]
    UnsupportedTool(String),
    #[error("io failed: {0}")]
    Io(String),
    #[error("remote command failed: {0}")]
    Remote(String),
    #[error("conversation bridge failed: {0}")]
    Bridge(String),
}

pub(super) struct ToolExecutionContext<'a> {
    pub workspace_root: &'a Path,
    pub data_root: &'a Path,
    pub remote_mode: &'a ToolRemoteMode,
    pub cancel_token: ToolCancellationToken,
}

impl ToolExecutionContext<'_> {
    pub(super) fn execution_target(
        &self,
        arguments: &Map<String, Value>,
    ) -> Result<ExecutionTarget, LocalToolError> {
        match self.remote_mode {
            ToolRemoteMode::Selectable => match arguments.get("remote") {
                Some(value) => {
                    let host = value
                        .as_str()
                        .ok_or_else(|| {
                            LocalToolError::InvalidArguments(
                                "argument remote must be a string".to_string(),
                            )
                        })?
                        .trim();
                    if host.is_empty() {
                        Ok(ExecutionTarget::Local)
                    } else {
                        validate_remote_host(host)?;
                        Ok(ExecutionTarget::RemoteSsh {
                            host: host.to_string(),
                            cwd: None,
                        })
                    }
                }
                None => Ok(ExecutionTarget::Local),
            },
            ToolRemoteMode::FixedSsh { host, cwd } => {
                validate_remote_host(host)?;
                Ok(ExecutionTarget::RemoteSsh {
                    host: host.clone(),
                    cwd: normalize_optional_cwd(cwd.as_deref()),
                })
            }
        }
    }

    pub(super) fn execution_target_for_path(
        &self,
        arguments: &Map<String, Value>,
        path_keys: &[&str],
    ) -> Result<ExecutionTarget, LocalToolError> {
        match self.remote_mode {
            ToolRemoteMode::FixedSsh { host, cwd } => {
                for key in path_keys {
                    let Some(path) = arguments.get(*key).and_then(Value::as_str) else {
                        continue;
                    };
                    if self.is_local_special_workspace_path(path) {
                        return Ok(ExecutionTarget::Local);
                    }
                }
                validate_remote_host(host)?;
                Ok(ExecutionTarget::RemoteSsh {
                    host: host.clone(),
                    cwd: normalize_optional_cwd(cwd.as_deref()),
                })
            }
            ToolRemoteMode::Selectable => self.execution_target(arguments),
        }
    }

    fn is_local_special_workspace_path(&self, raw_path: &str) -> bool {
        let path = raw_path.strip_prefix("file://").unwrap_or(raw_path).trim();
        if path.is_empty() || path == "." {
            return false;
        }
        let path = PathBuf::from(path);
        if path.is_absolute() {
            return path.starts_with(self.workspace_root);
        }
        let mut components = path.components();
        let Some(Component::Normal(first)) = components.next() else {
            return false;
        };
        first.to_string_lossy() == ".stellaclaw"
    }
}

fn normalize_optional_cwd(cwd: Option<&str>) -> Option<String> {
    cwd.and_then(|cwd| {
        let cwd = cwd.trim();
        (!cwd.is_empty()).then(|| cwd.to_string())
    })
}

pub(super) fn parse_arguments(text: &str) -> Result<Map<String, Value>, LocalToolError> {
    let value: Value = serde_json::from_str(text).map_err(|error| {
        LocalToolError::InvalidArguments(format!("tool arguments must be JSON: {error}"))
    })?;
    value.as_object().cloned().ok_or_else(|| {
        LocalToolError::InvalidArguments("tool arguments must be an object".to_string())
    })
}

pub(super) fn normalize_tool_value(value: Value) -> String {
    match value {
        Value::String(text) => text,
        other => serde_json::to_string_pretty(&other).unwrap_or_else(|_| "{}".to_string()),
    }
}

pub(super) fn clamp_tool_output_chars(requested: usize) -> usize {
    requested.min(1000)
}

pub(super) fn truncate_tool_text(value: &str, max_chars: usize) -> (String, bool) {
    let max_chars = clamp_tool_output_chars(max_chars);
    let total_chars = value.chars().count();
    if total_chars <= max_chars {
        return (value.to_string(), false);
    }
    if max_chars == 0 {
        return (String::new(), true);
    }

    let marker_template = format!("\n...<{} chars truncated>...\n", total_chars);
    let marker_chars = marker_template.chars().count().min(max_chars);
    if marker_chars >= max_chars {
        return (value.chars().take(max_chars).collect::<String>(), true);
    }

    let available = max_chars - marker_chars;
    let head_chars = available / 2;
    let tail_chars = available - head_chars;
    let omitted = total_chars.saturating_sub(head_chars + tail_chars);
    let marker = format!("\n...<{omitted} chars truncated>...\n");
    let head = value.chars().take(head_chars).collect::<String>();
    let tail = value
        .chars()
        .rev()
        .take(tail_chars)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect::<String>();
    (format!("{head}{marker}{tail}"), true)
}

pub(super) fn resolve_local_path(workspace_root: &Path, path: &str) -> PathBuf {
    if path.is_empty() {
        return workspace_root.to_path_buf();
    }
    let path = PathBuf::from(path);
    if path.is_absolute() {
        path
    } else {
        workspace_root.join(path)
    }
}

pub(super) fn string_arg(
    arguments: &Map<String, Value>,
    key: &str,
) -> Result<String, LocalToolError> {
    arguments
        .get(key)
        .and_then(Value::as_str)
        .map(ToOwned::to_owned)
        .ok_or_else(|| LocalToolError::InvalidArguments(format!("missing string argument {key}")))
}

pub(super) fn string_arg_with_alias(
    arguments: &Map<String, Value>,
    key: &str,
    alias: &str,
) -> Result<String, LocalToolError> {
    arguments
        .get(key)
        .or_else(|| arguments.get(alias))
        .and_then(Value::as_str)
        .map(ToOwned::to_owned)
        .ok_or_else(|| LocalToolError::InvalidArguments(format!("missing string argument {key}")))
}

pub(super) fn string_arg_with_default(
    arguments: &Map<String, Value>,
    key: &str,
    default: &str,
) -> Result<String, LocalToolError> {
    match arguments.get(key) {
        Some(value) => value.as_str().map(ToOwned::to_owned).ok_or_else(|| {
            LocalToolError::InvalidArguments(format!("argument {key} must be a string"))
        }),
        None => Ok(default.to_string()),
    }
}

pub(super) fn usize_arg_with_default(
    arguments: &Map<String, Value>,
    key: &str,
    default: usize,
) -> Result<usize, LocalToolError> {
    match arguments.get(key) {
        Some(value) => value.as_u64().map(|value| value as usize).ok_or_else(|| {
            LocalToolError::InvalidArguments(format!("argument {key} must be an integer"))
        }),
        None => Ok(default),
    }
}

pub(super) fn bool_arg_with_default(
    arguments: &Map<String, Value>,
    key: &str,
    default: bool,
) -> Result<bool, LocalToolError> {
    match arguments.get(key) {
        Some(value) => value.as_bool().ok_or_else(|| {
            LocalToolError::InvalidArguments(format!("argument {key} must be a boolean"))
        }),
        None => Ok(default),
    }
}

pub(super) fn f64_arg_with_default(
    arguments: &Map<String, Value>,
    key: &str,
    default: f64,
) -> Result<f64, LocalToolError> {
    match arguments.get(key) {
        Some(value) => value.as_f64().ok_or_else(|| {
            LocalToolError::InvalidArguments(format!("argument {key} must be a number"))
        }),
        None => Ok(default),
    }
}

pub(super) fn validate_remote_host(host: &str) -> Result<(), LocalToolError> {
    let trimmed = host.trim();
    if trimmed.is_empty() || trimmed == "local" {
        return Err(LocalToolError::InvalidArguments(
            "remote host must be a non-empty ~/.ssh/config Host alias".to_string(),
        ));
    }
    if trimmed.starts_with('-')
        || trimmed.chars().any(char::is_whitespace)
        || trimmed.chars().any(|ch| ch.is_control())
        || trimmed.contains('/')
        || trimmed.contains('\\')
        || trimmed.chars().any(|ch| {
            matches!(
                ch,
                '\'' | '"' | '`' | '$' | ';' | '&' | '|' | '<' | '>' | '(' | ')'
            )
        })
    {
        return Err(LocalToolError::InvalidArguments(
            "remote host must be a safe ~/.ssh/config Host alias".to_string(),
        ));
    }
    Ok(())
}

pub(super) fn run_remote_json(
    host: &str,
    cwd: Option<&str>,
    script: &str,
) -> Result<Value, LocalToolError> {
    let remote_command = match cwd {
        Some(cwd) => format!("cd {} && {}", shell_quote(cwd), script),
        None => script.to_string(),
    };
    let mut command = Command::new("ssh");
    command
        .arg("-o")
        .arg("BatchMode=yes")
        .arg("-o")
        .arg("ConnectTimeout=10")
        .arg("-T")
        .arg(host)
        .arg(remote_command);
    let output = run_command_with_timeout(command, REMOTE_JSON_TIMEOUT, None, "ssh")?;

    if !output.status.success() {
        return Err(LocalToolError::Remote(format!(
            "ssh exited with {}; stderr: {}",
            output.status.code().unwrap_or(-1),
            String::from_utf8_lossy(&output.stderr)
        )));
    }

    serde_json::from_slice(&output.stdout).map_err(|error| {
        LocalToolError::Remote(format!(
            "remote output was not JSON: {error}; stdout: {}",
            String::from_utf8_lossy(&output.stdout)
        ))
    })
}

pub(super) fn run_remote_command_with_stdin(
    host: &str,
    remote_command: &str,
    stdin: &[u8],
) -> Result<Output, LocalToolError> {
    let mut command = Command::new("ssh");
    command
        .arg("-o")
        .arg("BatchMode=yes")
        .arg("-o")
        .arg("ConnectTimeout=10")
        .arg("-T")
        .arg(host)
        .arg(remote_command);
    run_command_with_timeout(command, REMOTE_STDIN_TIMEOUT, Some(stdin), "ssh")
}

fn run_command_with_timeout(
    mut command: Command,
    timeout: Duration,
    stdin: Option<&[u8]>,
    command_label: &str,
) -> Result<Output, LocalToolError> {
    let mut child = command
        .stdin(if stdin.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| {
            LocalToolError::Remote(format!("failed to spawn {command_label}: {error}"))
        })?;

    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| LocalToolError::Remote(format!("failed to open {command_label} stdout")))?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| LocalToolError::Remote(format!("failed to open {command_label} stderr")))?;
    let stdout_handle = thread::spawn(move || read_pipe(stdout));
    let stderr_handle = thread::spawn(move || read_pipe(stderr));

    if let Some(stdin_bytes) = stdin {
        let write_result = match child.stdin.as_mut() {
            Some(child_stdin) => child_stdin.write_all(stdin_bytes).map_err(|error| {
                LocalToolError::Remote(format!("failed to write {command_label} stdin: {error}"))
            }),
            None => Err(LocalToolError::Remote(format!(
                "failed to open {command_label} stdin"
            ))),
        };
        if let Err(error) = write_result {
            let _ = child.kill();
            let _ = child.wait();
            let _ = join_pipe_reader(stdout_handle, command_label, "stdout");
            let _ = join_pipe_reader(stderr_handle, command_label, "stderr");
            return Err(error);
        }
    }
    let _ = child.stdin.take();

    let deadline = Instant::now() + timeout;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                let stdout = join_pipe_reader(stdout_handle, command_label, "stdout")?;
                let stderr = join_pipe_reader(stderr_handle, command_label, "stderr")?;
                return Ok(Output {
                    status,
                    stdout,
                    stderr,
                });
            }
            Ok(None) => {
                if Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    let stderr = join_pipe_reader(stderr_handle, command_label, "stderr")
                        .unwrap_or_default();
                    let _ = join_pipe_reader(stdout_handle, command_label, "stdout");
                    let stderr = String::from_utf8_lossy(&stderr);
                    let stderr = stderr.trim();
                    let suffix = if stderr.is_empty() {
                        String::new()
                    } else {
                        format!("; stderr: {stderr}")
                    };
                    return Err(LocalToolError::Remote(format!(
                        "{command_label} timed out after {} seconds{suffix}",
                        timeout.as_secs()
                    )));
                }
                thread::sleep(Duration::from_millis(100));
            }
            Err(error) => {
                return Err(LocalToolError::Remote(format!(
                    "failed to wait for {command_label}: {error}"
                )));
            }
        }
    }
}

fn read_pipe<R: Read>(mut pipe: R) -> Result<Vec<u8>, std::io::Error> {
    let mut buffer = Vec::new();
    pipe.read_to_end(&mut buffer)?;
    Ok(buffer)
}

fn join_pipe_reader(
    handle: thread::JoinHandle<Result<Vec<u8>, std::io::Error>>,
    command_label: &str,
    stream_name: &str,
) -> Result<Vec<u8>, LocalToolError> {
    handle
        .join()
        .map_err(|_| {
            LocalToolError::Remote(format!("{command_label} {stream_name} reader panicked"))
        })?
        .map_err(|error| {
            LocalToolError::Remote(format!(
                "failed to read {command_label} {stream_name}: {error}"
            ))
        })
}

pub(super) fn shell_quote(value: &str) -> String {
    if value.is_empty() {
        return "''".to_string();
    }
    format!("'{}'", value.replace('\'', "'\"'\"'"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    #[test]
    fn run_command_with_timeout_collects_output() {
        let mut command = Command::new("sh");
        command.arg("-c").arg("printf ok && printf err >&2");

        let output =
            run_command_with_timeout(command, Duration::from_secs(5), None, "test command")
                .expect("command should complete");

        assert!(output.status.success());
        assert_eq!(String::from_utf8_lossy(&output.stdout), "ok");
        assert_eq!(String::from_utf8_lossy(&output.stderr), "err");
    }

    #[cfg(unix)]
    #[test]
    fn run_command_with_timeout_kills_slow_process() {
        let mut command = Command::new("sh");
        command.arg("-c").arg("sleep 5");

        let started = Instant::now();
        let error =
            run_command_with_timeout(command, Duration::from_millis(100), None, "test command")
                .expect_err("command should time out");

        assert!(started.elapsed() < Duration::from_secs(2));
        assert!(error.to_string().contains("timed out"));
    }
}
