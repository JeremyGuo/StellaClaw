use std::{
    collections::HashMap,
    fs::{self, File},
    io::Write,
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::{Mutex, OnceLock},
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use serde_json::{json, Map, Value};

use super::{
    schema::{add_remote_property, object_schema, properties},
    ToolBackend, ToolDefinition, ToolExecutionMode, ToolRemoteMode,
};
use crate::session_actor::tool_runtime::{
    clamp_tool_output_chars, shell_quote, truncate_tool_text, usize_arg_with_default,
    ExecutionTarget, LocalToolError, ToolExecutionContext,
};

const SHELL_DEFAULT_WAIT_MS: usize = 10_000;
const SHELL_OUTPUT_TAIL_BYTES: usize = 16 * 1024;

static SHELL_SESSIONS: OnceLock<Mutex<HashMap<String, ShellSession>>> = OnceLock::new();

struct ShellSession {
    child: Child,
    stdout_path: PathBuf,
    stderr_path: PathBuf,
    command: String,
    out_path: PathBuf,
}

fn shell_sessions() -> &'static Mutex<HashMap<String, ShellSession>> {
    SHELL_SESSIONS.get_or_init(|| Mutex::new(HashMap::new()))
}

pub fn process_tool_definitions(remote_mode: &ToolRemoteMode) -> Vec<ToolDefinition> {
    let mut shell_properties = properties([
        ("session_id", json!({"type": "string"})),
        ("command", json!({"type": "string"})),
        ("input", json!({"type": "string"})),
        ("interactive", json!({"type": "boolean"})),
        ("wait_ms", json!({"type": "integer"})),
        (
            "max_output_chars",
            json!({"type": "integer", "minimum": 0, "maximum": 1000}),
        ),
    ]);
    add_remote_property(&mut shell_properties, remote_mode);

    vec![
        ToolDefinition::new(
            "shell",
            "Run or continue a persistent shell session. Pass command to start the next command in a session. Pass no command to only observe or collect the current command result. Pass input to write to the current interactive command. command=\"\" is treated the same as omitting command. session_id only reuses an existing session; omit it when creating a new one. If a finished command result has not been returned yet and you start a new command in the same session, the older unreturned result is discarded. Returned stdout/stderr describe only the current process and are capped by max_output_chars; full stdout/stderr are saved under out_path/stdout and out_path/stderr.",
            object_schema(shell_properties, &[]),
            ToolExecutionMode::Interruptible,
            ToolBackend::Local,
        ),
        ToolDefinition::new(
            "shell_close",
            "Close a shell session and stop its background worker. If the session has a running command, that command is stopped too.",
            object_schema(
                properties([("session_id", json!({"type": "string"}))]),
                &["session_id"],
            ),
            ToolExecutionMode::Immediate,
            ToolBackend::Local,
        ),
    ]
}

pub(crate) fn execute_process_tool(
    tool_name: &str,
    arguments: &Map<String, Value>,
    context: &ToolExecutionContext<'_>,
) -> Result<Option<Value>, LocalToolError> {
    let result = match tool_name {
        "shell" => shell(arguments, context)?,
        "shell_close" => shell_close(arguments)?,
        _ => return Ok(None),
    };
    Ok(Some(result))
}

fn shell(
    arguments: &Map<String, Value>,
    context: &ToolExecutionContext<'_>,
) -> Result<Value, LocalToolError> {
    let explicit_session_id =
        optional_string(arguments, "session_id").filter(|value| !value.trim().is_empty());
    let command = optional_string(arguments, "command").filter(|value| !value.trim().is_empty());
    if explicit_session_id.is_none() && command.is_none() {
        return Err(LocalToolError::InvalidArguments(
            "missing command; omit session_id only when starting a new shell command".to_string(),
        ));
    }
    let session_id = explicit_session_id.unwrap_or_else(generate_shell_session_id);
    validate_session_id(&session_id)?;

    if let Some(command) = command {
        start_shell_command(&session_id, &command, arguments, context)?;
    }

    if let Some(input) = optional_string(arguments, "input") {
        write_shell_input(&session_id, input.as_bytes())?;
    }

    wait_or_snapshot_shell(
        &session_id,
        usize_arg_with_default(arguments, "wait_ms", SHELL_DEFAULT_WAIT_MS)?,
        clamp_tool_output_chars(usize_arg_with_default(arguments, "max_output_chars", 1000)?),
    )
}

fn shell_close(arguments: &Map<String, Value>) -> Result<Value, LocalToolError> {
    let session_id = optional_string(arguments, "session_id")
        .ok_or_else(|| LocalToolError::InvalidArguments("missing session_id".to_string()))?;
    validate_session_id(&session_id)?;
    let mut sessions = shell_sessions().lock().expect("mutex poisoned");
    let Some(mut session) = sessions.remove(&session_id) else {
        return Ok(json!({
            "session_id": session_id,
            "closed": false,
            "reason": "unknown_session",
        }));
    };
    let _ = session.child.kill();
    let _ = session.child.wait();
    Ok(json!({
        "session_id": session_id,
        "closed": true,
        "out_path": session.out_path.display().to_string(),
    }))
}

fn start_shell_command(
    session_id: &str,
    command: &str,
    arguments: &Map<String, Value>,
    context: &ToolExecutionContext<'_>,
) -> Result<(), LocalToolError> {
    let mut sessions = shell_sessions().lock().expect("mutex poisoned");
    if let Some(mut previous) = sessions.remove(session_id) {
        let _ = previous.child.kill();
        let _ = previous.child.wait();
    }

    let out_path = context
        .workspace_root
        .join(".output")
        .join("shell")
        .join(session_id)
        .join(nonce());
    fs::create_dir_all(&out_path).map_err(|error| {
        LocalToolError::Io(format!("failed to create {}: {error}", out_path.display()))
    })?;
    let stdout_path = out_path.join("stdout");
    let stderr_path = out_path.join("stderr");
    let stdout = File::create(&stdout_path).map_err(|error| {
        LocalToolError::Io(format!(
            "failed to create {}: {error}",
            stdout_path.display()
        ))
    })?;
    let stderr = File::create(&stderr_path).map_err(|error| {
        LocalToolError::Io(format!(
            "failed to create {}: {error}",
            stderr_path.display()
        ))
    })?;

    let child = match context.execution_target(arguments)? {
        ExecutionTarget::Local => Command::new("sh")
            .arg("-lc")
            .arg(command)
            .current_dir(context.workspace_root)
            .stdin(Stdio::piped())
            .stdout(Stdio::from(stdout))
            .stderr(Stdio::from(stderr))
            .spawn()
            .map_err(|error| LocalToolError::Io(format!("failed to spawn shell: {error}")))?,
        ExecutionTarget::RemoteSsh { host, cwd } => {
            let remote_command = match cwd {
                Some(cwd) => format!("cd {} && {}", shell_quote(&cwd), command),
                None => command.to_string(),
            };
            Command::new("ssh")
                .arg("-o")
                .arg("BatchMode=yes")
                .arg("-T")
                .arg(host)
                .arg(remote_command)
                .stdin(Stdio::piped())
                .stdout(Stdio::from(stdout))
                .stderr(Stdio::from(stderr))
                .spawn()
                .map_err(|error| LocalToolError::Remote(format!("failed to spawn ssh: {error}")))?
        }
    };

    sessions.insert(
        session_id.to_string(),
        ShellSession {
            child,
            stdout_path,
            stderr_path,
            command: command.to_string(),
            out_path,
        },
    );
    Ok(())
}

fn wait_or_snapshot_shell(
    session_id: &str,
    wait_ms: usize,
    max_output_chars: usize,
) -> Result<Value, LocalToolError> {
    let deadline = Instant::now() + Duration::from_millis(wait_ms as u64);
    loop {
        let snapshot = {
            let mut sessions = shell_sessions().lock().expect("mutex poisoned");
            let session = sessions.get_mut(session_id).ok_or_else(|| {
                LocalToolError::InvalidArguments(format!("unknown shell session {session_id}"))
            })?;
            match session.child.try_wait().map_err(|error| {
                LocalToolError::Io(format!(
                    "failed to poll shell session {session_id}: {error}"
                ))
            })? {
                Some(status) => {
                    let (stdout, stdout_truncated) =
                        read_tail_preview(&session.stdout_path, max_output_chars)?;
                    let (stderr, stderr_truncated) =
                        read_tail_preview(&session.stderr_path, max_output_chars)?;
                    let out_path = session.out_path.clone();
                    let command = session.command.clone();
                    sessions.remove(session_id);
                    let mut result = Map::new();
                    result.insert(
                        "session_id".to_string(),
                        Value::String(session_id.to_string()),
                    );
                    result.insert("running".to_string(), Value::Bool(false));
                    result.insert("success".to_string(), Value::Bool(status.success()));
                    result.insert("command".to_string(), Value::String(command));
                    result.insert(
                        "out_path".to_string(),
                        Value::String(out_path.display().to_string()),
                    );
                    if let Some(exit_code) = status.code() {
                        result.insert("exit_code".to_string(), Value::from(exit_code));
                    }
                    if !stdout.is_empty() {
                        result.insert("stdout".to_string(), Value::String(stdout));
                    }
                    if !stderr.is_empty() {
                        result.insert("stderr".to_string(), Value::String(stderr));
                    }
                    if stdout_truncated {
                        result.insert("stdout_truncated".to_string(), Value::Bool(true));
                    }
                    if stderr_truncated {
                        result.insert("stderr_truncated".to_string(), Value::Bool(true));
                    }
                    return Ok(Value::Object(result));
                }
                None => {
                    let (stdout, stdout_truncated) =
                        read_tail_preview(&session.stdout_path, max_output_chars)?;
                    let (stderr, stderr_truncated) =
                        read_tail_preview(&session.stderr_path, max_output_chars)?;
                    let mut result = Map::new();
                    result.insert(
                        "session_id".to_string(),
                        Value::String(session_id.to_string()),
                    );
                    result.insert("running".to_string(), Value::Bool(true));
                    result.insert(
                        "command".to_string(),
                        Value::String(session.command.clone()),
                    );
                    result.insert(
                        "out_path".to_string(),
                        Value::String(session.out_path.display().to_string()),
                    );
                    if !stdout.is_empty() {
                        result.insert("stdout".to_string(), Value::String(stdout));
                    }
                    if !stderr.is_empty() {
                        result.insert("stderr".to_string(), Value::String(stderr));
                    }
                    if stdout_truncated {
                        result.insert("stdout_truncated".to_string(), Value::Bool(true));
                    }
                    if stderr_truncated {
                        result.insert("stderr_truncated".to_string(), Value::Bool(true));
                    }
                    Value::Object(result)
                }
            }
        };

        if Instant::now() >= deadline {
            return Ok(snapshot);
        }
        thread::sleep(Duration::from_millis(20));
    }
}

fn write_shell_input(session_id: &str, input: &[u8]) -> Result<(), LocalToolError> {
    let mut sessions = shell_sessions().lock().expect("mutex poisoned");
    let session = sessions.get_mut(session_id).ok_or_else(|| {
        LocalToolError::InvalidArguments(format!("unknown shell session {session_id}"))
    })?;
    let Some(stdin) = session.child.stdin.as_mut() else {
        return Err(LocalToolError::Io(format!(
            "shell session {session_id} has no open stdin"
        )));
    };
    stdin
        .write_all(input)
        .map_err(|error| LocalToolError::Io(format!("failed to write shell stdin: {error}")))?;
    Ok(())
}

fn optional_string(arguments: &Map<String, Value>, key: &str) -> Option<String> {
    arguments
        .get(key)
        .and_then(Value::as_str)
        .map(ToOwned::to_owned)
}

fn validate_session_id(session_id: &str) -> Result<(), LocalToolError> {
    if session_id.is_empty()
        || session_id.len() > 128
        || !session_id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
    {
        return Err(LocalToolError::InvalidArguments(
            "session_id must contain only ASCII letters, digits, '_' and '-'".to_string(),
        ));
    }
    Ok(())
}

fn generate_shell_session_id() -> String {
    format!("sh_{}", nonce())
}

fn nonce() -> String {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos().to_string())
        .unwrap_or_else(|_| "0".to_string())
}

fn read_tail_preview(
    path: &Path,
    max_output_chars: usize,
) -> Result<(String, bool), LocalToolError> {
    let bytes = fs::read(path).unwrap_or_default();
    let start = bytes.len().saturating_sub(SHELL_OUTPUT_TAIL_BYTES);
    let tail = String::from_utf8_lossy(&bytes[start..]).to_string();
    let (preview, chars_truncated) = truncate_tool_text(&tail, max_output_chars);
    Ok((preview, start > 0 || chars_truncated))
}
