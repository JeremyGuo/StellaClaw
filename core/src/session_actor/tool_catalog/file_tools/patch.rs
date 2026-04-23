use std::{
    io::Write,
    process::{Command, Stdio},
};

use serde_json::{json, Map, Value};

use crate::session_actor::tool_runtime::{
    bool_arg_with_default, run_remote_command_with_stdin, shell_quote, string_arg,
    usize_arg_with_default, ExecutionTarget, LocalToolError, ToolExecutionContext,
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
        ExecutionTarget::Local => apply_patch_local(arguments, context.workspace_root)?,
        ExecutionTarget::RemoteSsh { host, cwd } => {
            apply_patch_remote(arguments, &host, cwd.as_deref())?
        }
    };
    Ok(Some(result))
}

fn apply_patch_local(
    arguments: &Map<String, Value>,
    workspace_root: &std::path::Path,
) -> Result<Value, LocalToolError> {
    let patch = string_arg(arguments, "patch")?;
    let strip = usize_arg_with_default(arguments, "strip", 0)?;
    let reverse = bool_arg_with_default(arguments, "reverse", false)?;
    let check = bool_arg_with_default(arguments, "check", false)?;

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

    Ok(json!({
        "applied": output.status.success(),
        "returncode": output.status.code().unwrap_or(-1),
        "stdout": String::from_utf8_lossy(&output.stdout),
        "stderr": String::from_utf8_lossy(&output.stderr),
    }))
}

fn apply_patch_remote(
    arguments: &Map<String, Value>,
    host: &str,
    cwd: Option<&str>,
) -> Result<Value, LocalToolError> {
    let patch = string_arg(arguments, "patch")?;
    let strip = usize_arg_with_default(arguments, "strip", 0)?;
    let reverse = bool_arg_with_default(arguments, "reverse", false)?;
    let check = bool_arg_with_default(arguments, "check", false)?;
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
    Ok(json!({
        "remote": host,
        "applied": output.status.success(),
        "returncode": output.status.code().unwrap_or(-1),
        "stdout": String::from_utf8_lossy(&output.stdout),
        "stderr": String::from_utf8_lossy(&output.stderr),
    }))
}
