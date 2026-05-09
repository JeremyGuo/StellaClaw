use std::{path::Path, process::Command, time::Duration};

use serde_json::{Map, Value};

#[cfg(test)]
use serde_json::json;
#[cfg(test)]
use std::{fs, io::Write};

#[cfg(test)]
use crate::session_actor::tool_runtime::resolve_local_path;
use crate::session_actor::tool_runtime::{
    run_remote_command_with_stdin, shell_quote, string_arg, string_arg_with_alias,
    string_arg_with_default, ExecutionTarget, LocalToolError, ToolExecutionContext,
};

use super::patch::{ensure_fs_tool_local, ensure_fs_tool_remote, parse_fs_tool_json, FS_TOOL_NAME};

pub(super) fn execute_read_write_tool(
    tool_name: &str,
    arguments: &Map<String, Value>,
    context: &ToolExecutionContext<'_>,
) -> Result<Option<Value>, LocalToolError> {
    let result = match tool_name {
        "file_read" | "read_file" => file_read(arguments, context)?,
        "file_write" | "write_file" => file_write(arguments, context)?,
        _ => return Ok(None),
    };
    Ok(Some(result))
}

fn file_read(
    arguments: &Map<String, Value>,
    context: &ToolExecutionContext<'_>,
) -> Result<Value, LocalToolError> {
    match context.execution_target_for_path(arguments, &["file_path", "path"])? {
        ExecutionTarget::Local => file_read_local(arguments, context),
        ExecutionTarget::RemoteSsh { host, cwd } => {
            file_read_remote(arguments, context, &host, cwd.as_deref())
        }
    }
}

fn file_write(
    arguments: &Map<String, Value>,
    context: &ToolExecutionContext<'_>,
) -> Result<Value, LocalToolError> {
    match context.execution_target_for_path(arguments, &["file_path", "path"])? {
        ExecutionTarget::Local => file_write_local(arguments, context),
        ExecutionTarget::RemoteSsh { host, cwd } => {
            file_write_remote(arguments, context, &host, cwd.as_deref())
        }
    }
}

fn file_read_local(
    arguments: &Map<String, Value>,
    context: &ToolExecutionContext<'_>,
) -> Result<Value, LocalToolError> {
    #[cfg(test)]
    if std::env::var_os("STELLACLAW_FS_TOOL_PATH").is_none() {
        return file_read_local_direct(arguments, context.workspace_root);
    }
    file_read_via_fs_tool(arguments, context.workspace_root, context, None)
}

#[cfg(test)]
fn file_read_local_direct(
    arguments: &Map<String, Value>,
    workspace_root: &std::path::Path,
) -> Result<Value, LocalToolError> {
    let file_path = string_arg_with_alias(arguments, "file_path", "path")?;
    let path = resolve_local_path(workspace_root, &file_path);
    if path.is_dir() {
        return Err(LocalToolError::InvalidArguments(format!(
            "{} is a directory, not a file",
            path.display()
        )));
    }

    let text = fs::read_to_string(&path).map_err(|error| {
        LocalToolError::Io(format!("failed to read {}: {error}", path.display()))
    })?;
    let total_lines = text.lines().count();
    let (start_line, limit) = file_read_range(arguments)?;
    let selected = text
        .lines()
        .enumerate()
        .skip(start_line.saturating_sub(1))
        .take(limit)
        .map(|(index, line)| format!("{}: {}", index + 1, line))
        .collect::<Vec<_>>();
    let end_line = if selected.is_empty() {
        start_line.saturating_sub(1)
    } else {
        start_line + selected.len() - 1
    };

    Ok(json!({
        "file_path": path.display().to_string(),
        "start_line": start_line,
        "end_line": end_line,
        "total_lines": total_lines,
        "truncated": end_line < total_lines,
        "content": selected.join("\n"),
    }))
}

fn file_read_range(arguments: &Map<String, Value>) -> Result<(usize, usize), LocalToolError> {
    let start_line = usize_arg_with_alias_default(arguments, "start_line", "offset", 1)?.max(1);
    let limit = match usize_arg_optional(arguments, "end_line")? {
        Some(end_line) => {
            if end_line < start_line {
                return Err(LocalToolError::InvalidArguments(
                    "argument end_line must be greater than or equal to start_line".to_string(),
                ));
            }
            end_line - start_line + 1
        }
        None => usize_arg_optional(arguments, "limit")?.unwrap_or(200),
    };
    Ok((start_line, limit))
}

fn usize_arg_with_alias_default(
    arguments: &Map<String, Value>,
    key: &str,
    alias: &str,
    default: usize,
) -> Result<usize, LocalToolError> {
    match usize_arg_optional(arguments, key)? {
        Some(value) => Ok(value),
        None => Ok(usize_arg_optional(arguments, alias)?.unwrap_or(default)),
    }
}

fn usize_arg_optional(
    arguments: &Map<String, Value>,
    key: &str,
) -> Result<Option<usize>, LocalToolError> {
    match arguments.get(key) {
        Some(value) => value
            .as_u64()
            .map(|value| value as usize)
            .ok_or_else(|| {
                LocalToolError::InvalidArguments(format!("argument {key} must be an integer"))
            })
            .map(Some),
        None => Ok(None),
    }
}

fn file_write_local(
    arguments: &Map<String, Value>,
    context: &ToolExecutionContext<'_>,
) -> Result<Value, LocalToolError> {
    #[cfg(test)]
    if std::env::var_os("STELLACLAW_FS_TOOL_PATH").is_none() {
        return file_write_local_direct(arguments, context.workspace_root);
    }
    file_write_via_fs_tool(arguments, context.workspace_root, context, None)
}

#[cfg(test)]
fn file_write_local_direct(
    arguments: &Map<String, Value>,
    workspace_root: &std::path::Path,
) -> Result<Value, LocalToolError> {
    let file_path = string_arg_with_alias(arguments, "file_path", "path")?;
    let path = resolve_local_path(workspace_root, &file_path);
    let content = string_arg(arguments, "content")?;
    let mode = string_arg_with_default(arguments, "mode", "overwrite")?;

    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|error| {
            LocalToolError::Io(format!("failed to create {}: {error}", parent.display()))
        })?;
    }

    let mut options = fs::OpenOptions::new();
    options.create(true).write(true);
    if mode == "append" {
        options.append(true);
    } else {
        options.truncate(true);
    }

    let mut file = options.open(&path).map_err(|error| {
        LocalToolError::Io(format!("failed to open {}: {error}", path.display()))
    })?;
    file.write_all(content.as_bytes()).map_err(|error| {
        LocalToolError::Io(format!("failed to write {}: {error}", path.display()))
    })?;

    Ok(json!({
        "file_path": path.display().to_string(),
        "mode": mode,
        "bytes_written": content.len()
    }))
}

fn file_read_remote(
    arguments: &Map<String, Value>,
    context: &ToolExecutionContext<'_>,
    host: &str,
    cwd: Option<&str>,
) -> Result<Value, LocalToolError> {
    file_read_via_fs_tool(arguments, Path::new("."), context, Some((host, cwd)))
}

fn file_write_remote(
    arguments: &Map<String, Value>,
    context: &ToolExecutionContext<'_>,
    host: &str,
    cwd: Option<&str>,
) -> Result<Value, LocalToolError> {
    file_write_via_fs_tool(arguments, Path::new("."), context, Some((host, cwd)))
}

fn file_read_via_fs_tool(
    arguments: &Map<String, Value>,
    workspace_root: &Path,
    context: &ToolExecutionContext<'_>,
    remote: Option<(&str, Option<&str>)>,
) -> Result<Value, LocalToolError> {
    let file_path = string_arg_with_alias(arguments, "file_path", "path")?;
    let (start_line, limit) = file_read_range(arguments)?;
    let mut args = vec![
        "file-read".to_string(),
        "--workspace".to_string(),
        workspace_root.display().to_string(),
        "--file-path".to_string(),
        file_path,
        "--start-line".to_string(),
        start_line.to_string(),
        "--limit".to_string(),
        limit.to_string(),
    ];
    if let Some(end_line) = usize_arg_optional(arguments, "end_line")? {
        args.push("--end-line".to_string());
        args.push(end_line.to_string());
    }
    run_fs_tool(args, context, remote, &[])
}

fn file_write_via_fs_tool(
    arguments: &Map<String, Value>,
    workspace_root: &Path,
    context: &ToolExecutionContext<'_>,
    remote: Option<(&str, Option<&str>)>,
) -> Result<Value, LocalToolError> {
    let file_path = string_arg_with_alias(arguments, "file_path", "path")?;
    let content = string_arg(arguments, "content")?;
    let mode = string_arg_with_default(arguments, "mode", "overwrite")?;
    let args = vec![
        "file-write".to_string(),
        "--workspace".to_string(),
        workspace_root.display().to_string(),
        "--file-path".to_string(),
        file_path,
        "--mode".to_string(),
        mode,
    ];
    run_fs_tool(args, context, remote, content.as_bytes())
}

fn run_fs_tool(
    mut args: Vec<String>,
    context: &ToolExecutionContext<'_>,
    remote: Option<(&str, Option<&str>)>,
    stdin: &[u8],
) -> Result<Value, LocalToolError> {
    match remote {
        Some((host, cwd)) => {
            let binary = ensure_fs_tool_remote(context, host)?;
            args.insert(0, binary);
            let remote_command = args
                .iter()
                .map(|arg| shell_quote(arg))
                .collect::<Vec<_>>()
                .join(" ");
            let remote_command = match cwd {
                Some(cwd) => format!("cd {} && {}", shell_quote(cwd), remote_command),
                None => remote_command,
            };
            let output = run_remote_command_with_stdin(host, &remote_command, stdin)?;
            parse_fs_tool_json(&output, Some(host)).ok_or_else(|| {
                LocalToolError::Remote(format!(
                    "{FS_TOOL_NAME} output was not JSON: stdout: {}; stderr: {}",
                    String::from_utf8_lossy(&output.stdout),
                    String::from_utf8_lossy(&output.stderr)
                ))
            })
        }
        None => {
            let binary = ensure_fs_tool_local(context)?;
            let mut command = Command::new(binary);
            command.args(args);
            let output = crate::session_actor::tool_runtime::run_command_with_timeout(
                command,
                Duration::from_secs(300),
                Some(stdin),
                FS_TOOL_NAME,
            )?;
            parse_fs_tool_json(&output, None).ok_or_else(|| {
                LocalToolError::Io(format!(
                    "{FS_TOOL_NAME} output was not JSON: stdout: {}; stderr: {}",
                    String::from_utf8_lossy(&output.stdout),
                    String::from_utf8_lossy(&output.stderr)
                ))
            })
        }
    }
}

#[cfg(test)]
fn remote_file_read_script(path: &str, offset: usize, limit: usize) -> String {
    format!(
        r#"
import json, pathlib, signal, sys, time

class OperationTimeout(TimeoutError):
    pass

def timeout_handler(signum, frame):
    raise OperationTimeout("operation timed out")

def run_with_timeout(label, timeout_seconds, fn):
    if timeout_seconds <= 0 or not hasattr(signal, "setitimer"):
        return fn()
    previous_handler = signal.getsignal(signal.SIGALRM)
    previous_timer = signal.setitimer(signal.ITIMER_REAL, 0)
    signal.signal(signal.SIGALRM, timeout_handler)
    signal.setitimer(signal.ITIMER_REAL, timeout_seconds)
    try:
        return fn()
    except OperationTimeout:
        raise OperationTimeout(f"{{label}} timed out after {{timeout_seconds}} seconds")
    finally:
        signal.setitimer(signal.ITIMER_REAL, 0)
        signal.signal(signal.SIGALRM, previous_handler)
        if previous_timer[0] > 0:
            signal.setitimer(signal.ITIMER_REAL, *previous_timer)

def run_with_retry(label, attempts, timeout_seconds, fn):
    last_error = None
    for attempt in range(1, attempts + 1):
        try:
            return run_with_timeout(label, timeout_seconds, fn)
        except (OperationTimeout, OSError) as error:
            last_error = error
            if attempt >= attempts:
                break
            time.sleep(0.2 * attempt)
    raise RuntimeError(f"{{label}} failed after {{attempts}} attempt(s): {{last_error}}")

path = pathlib.Path({path:?}).expanduser()
try:
    text = run_with_retry("file_read", 2, 30, lambda: path.read_text(encoding="utf-8"))
    lines = text.splitlines()
    start = max(1, int({offset}))
    limit = int({limit})
    selected = [f"{{idx + 1}}: {{line}}" for idx, line in list(enumerate(lines))[start - 1:start - 1 + limit]]
    end = start + len(selected) - 1 if selected else start - 1
    print(json.dumps({{
      "file_path": str(path),
      "start_line": start,
      "end_line": end,
      "total_lines": len(lines),
      "truncated": end < len(lines),
      "content": "\n".join(selected),
    }}))
except Exception as error:
    print(str(error), file=sys.stderr)
    raise SystemExit(1)
"#
    )
}

#[cfg(test)]
fn remote_file_write_script(path: &str, content: &str, mode: &str) -> String {
    format!(
        r#"
import json, pathlib, signal, sys

class OperationTimeout(TimeoutError):
    pass

def timeout_handler(signum, frame):
    raise OperationTimeout("operation timed out")

def run_with_timeout(label, timeout_seconds, fn):
    if timeout_seconds <= 0 or not hasattr(signal, "setitimer"):
        return fn()
    previous_handler = signal.getsignal(signal.SIGALRM)
    previous_timer = signal.setitimer(signal.ITIMER_REAL, 0)
    signal.signal(signal.SIGALRM, timeout_handler)
    signal.setitimer(signal.ITIMER_REAL, timeout_seconds)
    try:
        return fn()
    except OperationTimeout:
        raise OperationTimeout(f"{{label}} timed out after {{timeout_seconds}} seconds")
    finally:
        signal.setitimer(signal.ITIMER_REAL, 0)
        signal.signal(signal.SIGALRM, previous_handler)
        if previous_timer[0] > 0:
            signal.setitimer(signal.ITIMER_REAL, *previous_timer)

path = pathlib.Path({path:?}).expanduser()
mode = {mode:?}
content = {content:?}
write_mode = "a" if mode == "append" else "w"
try:
    def write_file():
        path.parent.mkdir(parents=True, exist_ok=True)
        with path.open(write_mode, encoding="utf-8") as f:
            f.write(content)
    run_with_timeout("file_write", 30, write_file)
    print(json.dumps({{
      "file_path": str(path),
      "mode": mode,
      "bytes_written": len(content.encode("utf-8")),
    }}))
except Exception as error:
    print(str(error), file=sys.stderr)
    raise SystemExit(1)
"#
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::{
        io::Write,
        process::{Command, Stdio},
    };

    #[test]
    fn remote_read_write_scripts_are_valid_python() {
        assert_python_compiles(&remote_file_read_script("README.md", 1, 20));
        assert_python_compiles(&remote_file_write_script(
            "tmp/test.txt",
            "hello",
            "overwrite",
        ));
    }

    #[test]
    fn file_read_accepts_start_and_end_line() {
        let root =
            std::env::temp_dir().join(format!("stellaclaw-file-read-range-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).expect("workspace should be created");
        std::fs::write(root.join("demo.txt"), "one\ntwo\nthree\nfour\n")
            .expect("file should be written");
        let remote_mode = crate::session_actor::ToolRemoteMode::Selectable;
        let context = ToolExecutionContext {
            workspace_root: &root,
            data_root: &root,
            remote_mode: &remote_mode,
            conversation_bridge: None,
            cancel_token: crate::session_actor::tool_runtime::ToolCancellationToken::default(),
        };

        let result = file_read_local(
            &serde_json::Map::from_iter([
                ("file_path".to_string(), json!("demo.txt")),
                ("start_line".to_string(), json!(2)),
                ("end_line".to_string(), json!(3)),
            ]),
            &context,
        )
        .expect("file_read should succeed");

        assert_eq!(result["start_line"], json!(2));
        assert_eq!(result["end_line"], json!(3));
        assert_eq!(result["content"], json!("2: two\n3: three"));
        std::fs::remove_dir_all(&root).expect("temp dir should be cleaned");
    }

    fn assert_python_compiles(script: &str) {
        let mut child = match Command::new("python3")
            .arg("-c")
            .arg("import sys; compile(sys.stdin.read(), '<remote-read-write-tool>', 'exec')")
            .stdin(Stdio::piped())
            .spawn()
        {
            Ok(child) => child,
            Err(_) => return,
        };
        child
            .stdin
            .as_mut()
            .expect("stdin should be piped")
            .write_all(script.as_bytes())
            .expect("script should be written");
        let status = child.wait().expect("python3 should exit");
        assert!(status.success(), "generated Python script did not compile");
    }
}
