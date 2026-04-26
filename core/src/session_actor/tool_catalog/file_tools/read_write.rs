use std::{fs, io::Write};

use serde_json::{json, Map, Value};

use crate::session_actor::tool_runtime::{
    resolve_local_path, run_remote_json, string_arg, string_arg_with_alias,
    string_arg_with_default, usize_arg_with_default, ExecutionTarget, LocalToolError,
    ToolExecutionContext,
};

const MAX_FILE_READ_CONTENT_CHARS: usize = 60_000;

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
    match context.execution_target(arguments)? {
        ExecutionTarget::Local => file_read_local(arguments, context.workspace_root),
        ExecutionTarget::RemoteSsh { host, cwd } => {
            file_read_remote(arguments, &host, cwd.as_deref())
        }
    }
}

fn file_write(
    arguments: &Map<String, Value>,
    context: &ToolExecutionContext<'_>,
) -> Result<Value, LocalToolError> {
    match context.execution_target(arguments)? {
        ExecutionTarget::Local => file_write_local(arguments, context.workspace_root),
        ExecutionTarget::RemoteSsh { host, cwd } => {
            file_write_remote(arguments, &host, cwd.as_deref())
        }
    }
}

fn file_read_local(
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
    let offset = usize_arg_with_default(arguments, "offset", 1)?;
    let limit = usize_arg_with_default(arguments, "limit", 200)?;
    let start_line = offset.max(1);
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

    let content = selected.join("\n");
    let (content, content_truncated) = truncate_file_read_content(&content);

    Ok(json!({
        "file_path": path.display().to_string(),
        "start_line": start_line,
        "end_line": end_line,
        "total_lines": total_lines,
        "truncated": end_line < total_lines || content_truncated,
        "content_truncated": content_truncated,
        "content": content,
    }))
}

fn file_write_local(
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
    host: &str,
    cwd: Option<&str>,
) -> Result<Value, LocalToolError> {
    let file_path = string_arg_with_alias(arguments, "file_path", "path")?;
    let offset = usize_arg_with_default(arguments, "offset", 1)?;
    let limit = usize_arg_with_default(arguments, "limit", 200)?;
    let script = format!(
        "python3 - <<'PY'\n{}\nPY\n",
        remote_file_read_script(&file_path, offset, limit)
    );
    run_remote_json(host, cwd, &script)
}

fn file_write_remote(
    arguments: &Map<String, Value>,
    host: &str,
    cwd: Option<&str>,
) -> Result<Value, LocalToolError> {
    let file_path = string_arg_with_alias(arguments, "file_path", "path")?;
    let content = string_arg(arguments, "content")?;
    let mode = string_arg_with_default(arguments, "mode", "overwrite")?;
    let script = format!(
        "python3 - <<'PY'\n{}\nPY\n",
        remote_file_write_script(&file_path, &content, &mode)
    );
    run_remote_json(host, cwd, &script)
}

fn remote_file_read_script(path: &str, offset: usize, limit: usize) -> String {
    format!(
        r#"
import json, pathlib
MAX_CONTENT_CHARS = {max_content_chars}

def truncate_content(value):
    if len(value) <= MAX_CONTENT_CHARS:
        return value, False
    if MAX_CONTENT_CHARS == 0:
        return "", True
    marker_template = f"\n...<{{len(value)}} chars truncated>...\n"
    if len(marker_template) >= MAX_CONTENT_CHARS:
        return value[:MAX_CONTENT_CHARS], True
    available = MAX_CONTENT_CHARS - len(marker_template)
    head_chars = available // 2
    tail_chars = available - head_chars
    omitted = len(value) - head_chars - tail_chars
    marker = f"\n...<{{omitted}} chars truncated>...\n"
    return value[:head_chars] + marker + value[-tail_chars:], True

path = pathlib.Path({path:?}).expanduser()
text = path.read_text(encoding="utf-8")
lines = text.splitlines()
start = max(1, int({offset}))
limit = int({limit})
selected = [f"{{idx + 1}}: {{line}}" for idx, line in list(enumerate(lines))[start - 1:start - 1 + limit]]
end = start + len(selected) - 1 if selected else start - 1
content, content_truncated = truncate_content("\n".join(selected))
print(json.dumps({{
  "file_path": str(path),
  "start_line": start,
  "end_line": end,
  "total_lines": len(lines),
  "truncated": end < len(lines) or content_truncated,
  "content_truncated": content_truncated,
  "content": content,
}}))
"#,
        max_content_chars = MAX_FILE_READ_CONTENT_CHARS
    )
}

fn truncate_file_read_content(value: &str) -> (String, bool) {
    let total_chars = value.chars().count();
    if total_chars <= MAX_FILE_READ_CONTENT_CHARS {
        return (value.to_string(), false);
    }
    if MAX_FILE_READ_CONTENT_CHARS == 0 {
        return (String::new(), true);
    }

    let marker_template = format!("\n...<{total_chars} chars truncated>...\n");
    let marker_chars = marker_template
        .chars()
        .count()
        .min(MAX_FILE_READ_CONTENT_CHARS);
    if marker_chars >= MAX_FILE_READ_CONTENT_CHARS {
        return (
            value.chars().take(MAX_FILE_READ_CONTENT_CHARS).collect(),
            true,
        );
    }

    let available = MAX_FILE_READ_CONTENT_CHARS - marker_chars;
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

fn remote_file_write_script(path: &str, content: &str, mode: &str) -> String {
    format!(
        r#"
import json, pathlib
path = pathlib.Path({path:?}).expanduser()
path.parent.mkdir(parents=True, exist_ok=True)
mode = {mode:?}
content = {content:?}
write_mode = "a" if mode == "append" else "w"
with path.open(write_mode, encoding="utf-8") as f:
    f.write(content)
print(json.dumps({{
  "file_path": str(path),
  "mode": mode,
  "bytes_written": len(content.encode("utf-8")),
}}))
"#
    )
}
