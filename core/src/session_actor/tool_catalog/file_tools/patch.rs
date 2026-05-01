use std::{
    fs,
    io::Write,
    path::PathBuf,
    process::{Command, Stdio},
    time::{SystemTime, UNIX_EPOCH},
};

use serde_json::{Map, Value};

use crate::session_actor::tool_runtime::{
    bool_arg_with_default, clamp_tool_output_chars, run_remote_command_with_stdin, shell_quote,
    string_arg, truncate_tool_text, usize_arg_with_default, ExecutionTarget, LocalToolError,
    ToolExecutionContext,
};

pub(super) fn execute_patch_tool(
    tool_name: &str,
    arguments: &Map<String, Value>,
    context: &ToolExecutionContext<'_>,
) -> Result<Option<Value>, LocalToolError> {
    if tool_name != "apply_patch" {
        return Ok(None);
    }

    let result = match context.execution_target(arguments)? {
        ExecutionTarget::Local => apply_patch_local(arguments, context.workspace_root, context.data_root)?,
        ExecutionTarget::RemoteSsh { host, cwd } => {
            apply_patch_remote(arguments, context.workspace_root, context.data_root, &host, cwd.as_deref())?
        }
    };
    Ok(Some(result))
}

fn apply_patch_local(
    arguments: &Map<String, Value>,
    workspace_root: &std::path::Path,
    data_root: &std::path::Path,
) -> Result<Value, LocalToolError> {
    let patch = string_arg(arguments, "patch")?;
    let strip = usize_arg_with_default(arguments, "strip", 0)?;
    let reverse = bool_arg_with_default(arguments, "reverse", false)?;
    let check = bool_arg_with_default(arguments, "check", false)?;
    let max_output_chars =
        clamp_tool_output_chars(usize_arg_with_default(arguments, "max_output_chars", 1000)?);
    let out_path = make_patch_output_dir(data_root)?;

    let mut command = Command::new("git");
    command
        .arg("apply")
        .arg("--recount")
        .arg("--whitespace=nowarn")
        .arg(format!("-p{strip}"))
        .current_dir(workspace_root)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if reverse {
        command.arg("--reverse");
    }
    if check {
        command.arg("--check");
    }

    let mut child = command
        .spawn()
        .map_err(|error| LocalToolError::Io(format!("failed to spawn git apply: {error}")))?;
    child
        .stdin
        .as_mut()
        .ok_or_else(|| LocalToolError::Io("failed to open git apply stdin".to_string()))?
        .write_all(patch.as_bytes())
        .map_err(|error| LocalToolError::Io(format!("failed to write patch: {error}")))?;
    let _ = child.stdin.take();
    let output = child
        .wait_with_output()
        .map_err(|error| LocalToolError::Io(format!("failed to wait for git apply: {error}")))?;
    patch_result(output, out_path, None, max_output_chars)
}

fn apply_patch_remote(
    arguments: &Map<String, Value>,
    _workspace_root: &std::path::Path,
    data_root: &std::path::Path,
    host: &str,
    cwd: Option<&str>,
) -> Result<Value, LocalToolError> {
    let patch = string_arg(arguments, "patch")?;
    let strip = usize_arg_with_default(arguments, "strip", 0)?;
    let reverse = bool_arg_with_default(arguments, "reverse", false)?;
    let check = bool_arg_with_default(arguments, "check", false)?;
    let max_output_chars =
        clamp_tool_output_chars(usize_arg_with_default(arguments, "max_output_chars", 1000)?);
    let mut args = vec![
        "git".to_string(),
        "apply".to_string(),
        "--recount".to_string(),
        "--whitespace=nowarn".to_string(),
        format!("-p{strip}"),
    ];
    if reverse {
        args.push("--reverse".to_string());
    }
    if check {
        args.push("--check".to_string());
    }
    let remote_command = args
        .iter()
        .map(|arg| shell_quote(arg))
        .collect::<Vec<_>>()
        .join(" ");
    let remote_command = match cwd {
        Some(cwd) => format!("cd {} && {}", shell_quote(cwd), remote_command),
        None => remote_command,
    };
    let output = run_remote_command_with_stdin(host, &remote_command, patch.as_bytes())?;
    let out_path = make_patch_output_dir(data_root)?;
    patch_result(output, out_path, Some(host), max_output_chars)
}

fn patch_result(
    output: std::process::Output,
    out_path: PathBuf,
    remote: Option<&str>,
    max_output_chars: usize,
) -> Result<Value, LocalToolError> {
    fs::write(out_path.join("stdout"), &output.stdout).map_err(|error| {
        LocalToolError::Io(format!("failed to write patch stdout artifact: {error}"))
    })?;
    fs::write(out_path.join("stderr"), &output.stderr).map_err(|error| {
        LocalToolError::Io(format!("failed to write patch stderr artifact: {error}"))
    })?;

    let stdout_text = String::from_utf8_lossy(&output.stdout).to_string();
    let stderr_text = String::from_utf8_lossy(&output.stderr).to_string();
    let (stdout, stdout_truncated) = truncate_tool_text(&stdout_text, max_output_chars);
    let (stderr, stderr_truncated) = truncate_tool_text(&stderr_text, max_output_chars);

    let mut result = Map::new();
    result.insert("applied".to_string(), Value::Bool(output.status.success()));
    result.insert(
        "out_path".to_string(),
        Value::String(out_path.display().to_string()),
    );
    if let Some(returncode) = output.status.code() {
        result.insert("returncode".to_string(), Value::from(returncode));
    }
    if let Some(remote) = remote {
        result.insert("remote".to_string(), Value::String(remote.to_string()));
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
    Ok(Value::Object(result))
}

fn make_patch_output_dir(data_root: &std::path::Path) -> Result<PathBuf, LocalToolError> {
    let out_path = data_root
        .join(".stellaclaw")
        .join("output")
        .join("apply_patch")
        .join(nonce());
    fs::create_dir_all(&out_path).map_err(|error| {
        LocalToolError::Io(format!("failed to create {}: {error}", out_path.display()))
    })?;
    Ok(out_path)
}

fn nonce() -> String {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos().to_string())
        .unwrap_or_else(|_| "0".to_string())
}
