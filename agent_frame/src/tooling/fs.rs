use super::args::{
    string_arg, string_arg_with_alias, string_arg_with_default, usize_arg_with_alias,
};
use super::remote::{
    ExecutionTarget, RemoteWorkpathMap, execution_target_arg, remote_file_root,
    remote_python_command, remote_schema_property, run_remote_command,
};
use super::{InterruptSignal, Tool, resolve_path};
use anyhow::{Context, Result, anyhow};
use globset::{Glob, GlobMatcher};
use regex::Regex;
use serde_json::{Map, Value, json};
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::UNIX_EPOCH;
use walkdir::{DirEntry, WalkDir};

const FILE_READ_DEFAULT_LIMIT: usize = 2_000;
const FILE_READ_MAX_LINE_LENGTH: usize = 2_000;
pub(super) const FILE_READ_MAX_OUTPUT_BYTES: usize = 256 * 1024;
const SEARCH_MAX_RESULTS: usize = 100;
pub(super) const LS_MAX_ENTRIES: usize = 1_000;

#[derive(Clone)]
struct SearchMatch {
    path: String,
    mtime_ms: u128,
}

#[derive(Clone)]
struct LsEntry {
    path: String,
    is_dir: bool,
}

fn truncate_line_for_file_read(line: &str) -> (String, bool) {
    let count = line.chars().count();
    if count <= FILE_READ_MAX_LINE_LENGTH {
        return (line.to_string(), false);
    }
    let truncated = line
        .chars()
        .take(FILE_READ_MAX_LINE_LENGTH)
        .collect::<String>();
    (format!("{}...", truncated), true)
}

fn sort_search_matches(matches: &mut [SearchMatch]) {
    matches.sort_by(|left, right| {
        right
            .mtime_ms
            .cmp(&left.mtime_ms)
            .then_with(|| left.path.cmp(&right.path))
    });
}

fn file_mtime_ms(path: &Path) -> u128 {
    fs::metadata(path)
        .ok()
        .and_then(|metadata| metadata.modified().ok())
        .and_then(|modified| modified.duration_since(UNIX_EPOCH).ok())
        .map(|duration| duration.as_millis())
        .unwrap_or(0)
}

fn relative_display_path(path: &Path, base: &Path) -> String {
    path.strip_prefix(base)
        .unwrap_or(path)
        .to_string_lossy()
        .replace('\\', "/")
}

fn build_optional_glob_matcher(pattern: Option<&str>) -> Result<Option<GlobMatcher>> {
    match pattern {
        Some(pattern) if !pattern.trim().is_empty() => Ok(Some(
            Glob::new(pattern)
                .with_context(|| format!("invalid glob pattern: {}", pattern))?
                .compile_matcher(),
        )),
        _ => Ok(None),
    }
}

fn collect_walk_paths(base_path: &Path, include_directories: bool) -> Vec<PathBuf> {
    WalkDir::new(base_path)
        .follow_links(false)
        .into_iter()
        .filter_map(|entry| entry.ok())
        .filter_map(|entry| {
            let path = entry.into_path();
            if path == base_path {
                return None;
            }
            if path.is_dir() {
                include_directories.then_some(path)
            } else if path.is_file() {
                Some(path)
            } else {
                None
            }
        })
        .collect()
}

fn is_common_ls_skip_dir_name(name: &str) -> bool {
    matches!(
        name,
        "__pycache__" | "node_modules" | "target" | "dist" | "build" | "coverage" | "venv"
    )
}

fn should_skip_ls_entry(entry: &DirEntry, base_path: &Path) -> bool {
    let path = entry.path();
    if path == base_path {
        return false;
    }

    let name = entry.file_name().to_string_lossy();
    if name.starts_with('.') {
        return true;
    }

    entry.file_type().is_dir() && is_common_ls_skip_dir_name(&name)
}

fn collect_ls_paths(base_path: &Path, max_entries: usize) -> (Vec<PathBuf>, bool) {
    let mut paths = Vec::new();
    for entry in WalkDir::new(base_path)
        .follow_links(false)
        .into_iter()
        .filter_entry(|entry| !should_skip_ls_entry(entry, base_path))
    {
        let Ok(entry) = entry else {
            continue;
        };
        let path = entry.into_path();
        if path == base_path || !(path.is_dir() || path.is_file()) {
            continue;
        }
        if paths.len() >= max_entries {
            return (paths, true);
        }
        paths.push(path);
    }
    (paths, false)
}

fn path_with_trailing_slash(path: &Path) -> String {
    let mut display = path.to_string_lossy().replace('\\', "/");
    if !display.ends_with('/') {
        display.push('/');
    }
    display
}

fn render_ls_tree(base_path: &Path, mut entries: Vec<LsEntry>, truncated: bool) -> String {
    entries.sort_by(|left, right| left.path.cmp(&right.path));
    let mut lines = Vec::new();
    if truncated {
        lines.push(format!("num_entries: >{LS_MAX_ENTRIES}"));
        lines.push("truncated: true".to_string());
        lines.push(format!(
            "There are more than {LS_MAX_ENTRIES} files and directories under {}. Use ls with a more specific path, or use glob/grep to narrow the search. The first {LS_MAX_ENTRIES} files and directories are included below:",
            base_path.display()
        ));
        lines.push(String::new());
    } else {
        lines.push(format!("num_entries: {}", entries.len()));
        lines.push(String::new());
    }
    lines.push(format!("- {}", path_with_trailing_slash(base_path)));
    for entry in entries {
        let components = entry
            .path
            .split('/')
            .filter(|part| !part.is_empty())
            .collect::<Vec<_>>();
        let Some(name) = components.last() else {
            continue;
        };
        let indent = "  ".repeat(components.len());
        let suffix = if entry.is_dir { "/" } else { "" };
        lines.push(format!("{indent}- {name}{suffix}"));
    }
    lines.join("\n")
}

const REMOTE_FILE_TOOL_PY: &str = r#"
import fnmatch
import json
import os
import re
import sys

FILE_READ_DEFAULT_LIMIT = 2000
FILE_READ_MAX_LINE_LENGTH = 2000
FILE_READ_MAX_OUTPUT_BYTES = 256 * 1024
SEARCH_MAX_RESULTS = 100
LS_MAX_ENTRIES = 1000
COMMON_LS_SKIP_DIRS = {"__pycache__", "node_modules", "target", "dist", "build", "coverage", "venv"}

def resolve(path, root):
    if os.path.isabs(path):
        return os.path.abspath(path)
    return os.path.abspath(os.path.join(root, path))

def truncate_line(line):
    if len(line) <= FILE_READ_MAX_LINE_LENGTH:
        return line, False
    return line[:FILE_READ_MAX_LINE_LENGTH] + "...", True

def file_mtime_ms(path):
    try:
        return int(os.path.getmtime(path) * 1000)
    except OSError:
        return 0

def walk_files(base):
    paths = []
    for root, dirs, files in os.walk(base, followlinks=False):
        for name in files:
            path = os.path.join(root, name)
            if os.path.isfile(path):
                paths.append(path)
    return paths

def handle_file_read(args, workspace_root):
    file_path = args.get("file_path", args.get("path"))
    if not isinstance(file_path, str):
        raise ValueError("missing required argument: file_path")
    path = resolve(file_path, workspace_root)
    if os.path.isdir(path):
        raise ValueError(f"{path} is a directory, not a file")
    has_explicit_window = any(key in args for key in ("offset", "offset_lines", "limit", "limit_lines"))
    file_size = os.path.getsize(path)
    if file_size > FILE_READ_MAX_OUTPUT_BYTES and not has_explicit_window:
        raise ValueError(f"{path} is {file_size} bytes, which exceeds the direct-read limit of {FILE_READ_MAX_OUTPUT_BYTES} bytes; provide offset and/or limit")
    encoding = args.get("encoding", "utf-8")
    if str(encoding).lower() != "utf-8":
        raise ValueError("only utf-8 encoding is supported")
    if "offset_lines" in args and "offset" not in args:
        offset = int(args["offset_lines"]) + 1
    else:
        offset = int(args.get("offset", 1))
    if "limit_lines" in args and "limit" not in args:
        limit = int(args["limit_lines"])
    else:
        limit = int(args.get("limit", FILE_READ_DEFAULT_LIMIT))
    start_line = 0 if offset == 0 else max(offset, 1)
    line_offset = 0 if start_line == 0 else start_line - 1
    with open(path, "r", encoding="utf-8") as handle:
        lines = handle.read().splitlines()
    content = []
    content_bytes = 0
    truncated_long_lines = False
    truncated_by_bytes = False
    for index, line in list(enumerate(lines))[line_offset:line_offset + limit]:
        display_line, was_truncated = truncate_line(line)
        truncated_long_lines = truncated_long_lines or was_truncated
        next_line = f"{index + 1}: {display_line}"
        separator = 0 if not content else 1
        if content_bytes + separator + len(next_line.encode("utf-8")) > FILE_READ_MAX_OUTPUT_BYTES:
            truncated_by_bytes = True
            break
        content.append(next_line)
        content_bytes += separator + len(next_line.encode("utf-8"))
    selected_line_count = len(content)
    if selected_line_count == 0:
        end_line = max(start_line - 1, 0)
    elif start_line == 0:
        end_line = selected_line_count - 1
    else:
        end_line = start_line + selected_line_count - 1
    truncated_by_lines = line_offset + selected_line_count < len(lines)
    result = {
        "file_path": path,
        "start_line": start_line,
        "end_line": end_line,
        "total_lines": len(lines),
        "content": "\n".join(content),
    }
    if truncated_by_lines or truncated_by_bytes or truncated_long_lines:
        result["truncated"] = True
    if truncated_by_lines:
        result["truncated_by_lines"] = True
    if truncated_by_bytes:
        result["truncated_by_bytes"] = True
    if truncated_long_lines:
        result["truncated_long_lines"] = True
    return result

def handle_file_write(args, workspace_root):
    file_path = args.get("file_path", args.get("path"))
    if not isinstance(file_path, str):
        raise ValueError("missing required argument: file_path")
    content = args.get("content")
    if not isinstance(content, str):
        raise ValueError("argument content must be a string")
    mode = args.get("mode", "overwrite")
    encoding = args.get("encoding", "utf-8")
    if str(encoding).lower() != "utf-8":
        raise ValueError("only utf-8 encoding is supported")
    path = resolve(file_path, workspace_root)
    parent = os.path.dirname(path)
    if parent:
        os.makedirs(parent, exist_ok=True)
    open_mode = "a" if mode == "append" else "w"
    with open(path, open_mode, encoding="utf-8") as handle:
        handle.write(content)
    return {"file_path": path, "mode": mode, "bytes_written": len(content.encode("utf-8"))}

def handle_glob(args, workspace_root):
    pattern = args.get("pattern")
    if not isinstance(pattern, str):
        raise ValueError("missing required argument: pattern")
    base = resolve(args.get("path", "."), workspace_root)
    if not os.path.exists(base):
        raise ValueError(f"{base} does not exist")
    matches = []
    for path in walk_files(base):
        rel = os.path.relpath(path, base).replace(os.sep, "/")
        if fnmatch.fnmatch(rel, pattern):
            matches.append({"path": path, "mtime_ms": file_mtime_ms(path)})
    matches.sort(key=lambda item: (-item["mtime_ms"], item["path"]))
    total = len(matches)
    result = {
        "pattern": pattern,
        "path": base,
        "num_files": total,
        "filenames": [item["path"] for item in matches[:SEARCH_MAX_RESULTS]],
    }
    if total > SEARCH_MAX_RESULTS:
        result["truncated"] = True
    return result

def handle_grep(args, workspace_root):
    pattern = args.get("pattern")
    if not isinstance(pattern, str):
        raise ValueError("missing required argument: pattern")
    base = resolve(args.get("path", "."), workspace_root)
    if not os.path.exists(base):
        raise ValueError(f"{base} does not exist")
    include = args.get("include")
    regex = re.compile(pattern)
    matches = []
    for path in walk_files(base):
        rel = os.path.relpath(path, base).replace(os.sep, "/")
        if include and not fnmatch.fnmatch(rel, include):
            continue
        try:
            with open(path, "r", encoding="utf-8") as handle:
                text = handle.read()
        except (UnicodeDecodeError, OSError):
            continue
        if regex.search(text):
            matches.append({"path": path, "mtime_ms": file_mtime_ms(path)})
    matches.sort(key=lambda item: (-item["mtime_ms"], item["path"]))
    total = len(matches)
    result = {
        "pattern": pattern,
        "path": base,
        "num_files": total,
        "filenames": [item["path"] for item in matches[:SEARCH_MAX_RESULTS]],
    }
    if include:
        result["include"] = include
    if total > SEARCH_MAX_RESULTS:
        result["truncated"] = True
    return result

def should_skip_ls(root, name, is_dir, base):
    path = os.path.join(root, name)
    if path == base:
        return False
    if name.startswith("."):
        return True
    return is_dir and name in COMMON_LS_SKIP_DIRS

def handle_ls(args, workspace_root):
    path_arg = args.get("path")
    if not isinstance(path_arg, str):
        raise ValueError("missing required argument: path")
    base = resolve(path_arg, workspace_root)
    if not os.path.exists(base):
        raise ValueError(f"{base} does not exist")
    if not os.path.isdir(base):
        raise ValueError(f"{base} is not a directory")
    entries = []
    truncated = False
    for root, dirs, files in os.walk(base, followlinks=False):
        dirs[:] = [name for name in dirs if not should_skip_ls(root, name, True, base)]
        for name in dirs:
            if len(entries) >= LS_MAX_ENTRIES:
                truncated = True
                break
            path = os.path.join(root, name)
            entries.append((os.path.relpath(path, base).replace(os.sep, "/"), True))
        if truncated:
            break
        for name in files:
            if should_skip_ls(root, name, False, base):
                continue
            if len(entries) >= LS_MAX_ENTRIES:
                truncated = True
                break
            path = os.path.join(root, name)
            entries.append((os.path.relpath(path, base).replace(os.sep, "/"), False))
        if truncated:
            break
    entries.sort(key=lambda item: item[0])
    lines = []
    if truncated:
        lines.append(f"num_entries: >{LS_MAX_ENTRIES}")
        lines.append("truncated: true")
        lines.append(f"There are more than {LS_MAX_ENTRIES} files and directories under {base}. Use ls with a more specific path, or use glob/grep to narrow the search. The first {LS_MAX_ENTRIES} files and directories are included below:")
        lines.append("")
    else:
        lines.append(f"num_entries: {len(entries)}")
        lines.append("")
    display_base = base.replace(os.sep, "/")
    if not display_base.endswith("/"):
        display_base += "/"
    lines.append(f"- {display_base}")
    for rel_path, is_dir in entries:
        parts = [part for part in rel_path.split("/") if part]
        if not parts:
            continue
        indent = "  " * len(parts)
        suffix = "/" if is_dir else ""
        lines.append(f"{indent}- {parts[-1]}{suffix}")
    return "\n".join(lines)

def handle_edit(args, workspace_root):
    path_arg = args.get("path")
    if not isinstance(path_arg, str):
        raise ValueError("missing required argument: path")
    old_text = args.get("old_text")
    new_text = args.get("new_text")
    if not isinstance(old_text, str):
        raise ValueError("argument old_text must be a string")
    if not isinstance(new_text, str):
        raise ValueError("argument new_text must be a string")
    encoding = args.get("encoding", "utf-8")
    if str(encoding).lower() != "utf-8":
        raise ValueError("only utf-8 encoding is supported")
    path = resolve(path_arg, workspace_root)
    replace_all = bool(args.get("replace_all", False))
    create_if_missing = bool(args.get("create_if_missing", False))
    if not os.path.exists(path) and create_if_missing:
        parent = os.path.dirname(path)
        if parent:
            os.makedirs(parent, exist_ok=True)
        with open(path, "w", encoding="utf-8") as handle:
            handle.write(new_text)
        return {"path": path, "created": True, "replacements": 1, "bytes_written": len(new_text.encode("utf-8"))}
    with open(path, "r", encoding="utf-8") as handle:
        content = handle.read()
    replacements = content.count(old_text)
    if replacements == 0:
        raise ValueError(f"old_text was not found in {path}")
    if replacements > 1 and not replace_all:
        raise ValueError(
            f"old_text matched {replacements} locations in {path}; "
            "include more surrounding context or set replace_all=true"
        )
    updated = content.replace(old_text, new_text) if replace_all else content.replace(old_text, new_text, 1)
    with open(path, "w", encoding="utf-8") as handle:
        handle.write(updated)
    return {"path": path, "created": False, "replacements": replacements if replace_all else 1, "bytes_written": len(updated.encode("utf-8"))}

handlers = {
    "file_read": handle_file_read,
    "file_write": handle_file_write,
    "glob": handle_glob,
    "grep": handle_grep,
    "ls": handle_ls,
    "edit": handle_edit,
}

try:
    payload = json.load(sys.stdin)
    operation = payload["operation"]
    workspace_root = payload["workspace_root"]
    result = handlers[operation](payload.get("arguments", {}), workspace_root)
    print(json.dumps({"ok": True, "result": result}, ensure_ascii=False))
except Exception as error:
    print(json.dumps({"ok": False, "error": str(error)}, ensure_ascii=False))
"#;

fn run_remote_file_tool(
    host: &str,
    operation: &str,
    arguments: &Map<String, Value>,
    remote_workpaths: &BTreeMap<String, String>,
) -> Result<Value> {
    let (remote_cwd, remote_root) = remote_file_root(host, operation, arguments, remote_workpaths)?;
    let payload = json!({
        "operation": operation,
        "workspace_root": remote_root,
        "arguments": arguments,
    });
    let stdin = serde_json::to_vec(&payload).context("failed to serialize remote tool payload")?;
    let output = run_remote_command(
        host,
        &remote_cwd,
        &[
            "sh".to_string(),
            "-c".to_string(),
            remote_python_command(REMOTE_FILE_TOOL_PY),
        ],
        Some(&stdin),
    )?;
    if !output.status.success() {
        return Err(anyhow!(
            "remote {} tool failed on {} with {}: {}",
            operation,
            host,
            output.status.code().unwrap_or(-1),
            String::from_utf8_lossy(&output.stderr)
        ));
    }
    let wrapper: Value = serde_json::from_slice(&output.stdout).with_context(|| {
        format!(
            "failed to parse remote {} JSON from {}: {}",
            operation,
            host,
            String::from_utf8_lossy(&output.stdout)
        )
    })?;
    let ok = wrapper.get("ok").and_then(Value::as_bool).unwrap_or(false);
    if !ok {
        let error = wrapper
            .get("error")
            .and_then(Value::as_str)
            .unwrap_or("remote tool failed");
        return Err(anyhow!(
            "remote {} on {} failed: {}",
            operation,
            host,
            error
        ));
    }
    let mut result = wrapper.get("result").cloned().unwrap_or(Value::Null);
    if let Some(text) = result.as_str() {
        return Ok(Value::String(format!("remote: {host}\n{text}")));
    }
    if let Some(object) = result.as_object_mut() {
        object.insert("remote".to_string(), Value::String(host.to_string()));
    }
    Ok(result)
}

pub(super) fn file_read_tool(
    workspace_root: PathBuf,
    remote_workpaths: RemoteWorkpathMap,
    _cancel_flag: Option<Arc<InterruptSignal>>,
) -> Tool {
    Tool::new(
        "file_read",
        "Read a UTF-8 text file. Supports file_path plus optional offset and limit for large files.",
        json!({
            "type": "object",
            "properties": {
                "file_path": {"type": "string"},
                "offset": {"type": "integer"},
                "limit": {"type": "integer"},
                "remote": remote_schema_property()
            },
            "required": ["file_path"],
            "additionalProperties": false
        }),
        move |arguments| {
            let arguments = arguments
                .as_object()
                .ok_or_else(|| anyhow!("tool arguments must be an object"))?;
            if let ExecutionTarget::RemoteSsh { host } = execution_target_arg(arguments)? {
                return run_remote_file_tool(&host, "file_read", arguments, &remote_workpaths);
            }
            let file_path = string_arg_with_alias(arguments, "file_path", "path")?;
            let path = resolve_path(&file_path, &workspace_root);
            if path.is_dir() {
                return Err(anyhow!("{} is a directory, not a file", path.display()));
            }

            let has_explicit_window = arguments.contains_key("offset")
                || arguments.contains_key("offset_lines")
                || arguments.contains_key("limit")
                || arguments.contains_key("limit_lines");
            let file_size = fs::metadata(&path)
                .with_context(|| format!("failed to stat {}", path.display()))?
                .len();
            if file_size > FILE_READ_MAX_OUTPUT_BYTES as u64 && !has_explicit_window {
                return Err(anyhow!(
                    "{} is {} bytes, which exceeds the direct-read limit of {} bytes; provide offset and/or limit",
                    path.display(),
                    file_size,
                    FILE_READ_MAX_OUTPUT_BYTES
                ));
            }

            let encoding = string_arg_with_default(arguments, "encoding", "utf-8")?;
            if encoding.to_lowercase() != "utf-8" {
                return Err(anyhow!("only utf-8 encoding is supported"));
            }

            let offset = match usize_arg_with_alias(arguments, "offset", "offset_lines")? {
                Some(value)
                    if arguments.contains_key("offset_lines")
                        && !arguments.contains_key("offset") =>
                {
                    value.saturating_add(1)
                }
                Some(value) => value,
                None => 1,
            };
            let limit = match usize_arg_with_alias(arguments, "limit", "limit_lines")? {
                Some(value) => value,
                None => FILE_READ_DEFAULT_LIMIT,
            };
            let start_line = if offset == 0 { 0 } else { offset.max(1) };
            let line_offset = if start_line == 0 { 0 } else { start_line - 1 };

            let text = fs::read_to_string(&path)
                .with_context(|| format!("failed to read {}", path.display()))?;
            let lines: Vec<&str> = text.lines().collect();

            let mut content = String::new();
            let mut selected_line_count = 0usize;
            let mut truncated_long_lines = false;
            let mut truncated_by_bytes = false;

            for (index, line) in lines.iter().enumerate().skip(line_offset).take(limit) {
                let (display_line, was_truncated) = truncate_line_for_file_read(line);
                truncated_long_lines |= was_truncated;
                let next = format!("{}: {}", index + 1, display_line);
                let separator = if content.is_empty() { 0 } else { 1 };
                if content.len() + separator + next.len() > FILE_READ_MAX_OUTPUT_BYTES {
                    truncated_by_bytes = true;
                    break;
                }
                if !content.is_empty() {
                    content.push('\n');
                }
                content.push_str(&next);
                selected_line_count += 1;
            }

            let end_line = if selected_line_count == 0 {
                start_line.saturating_sub(1)
            } else if start_line == 0 {
                selected_line_count - 1
            } else {
                start_line + selected_line_count - 1
            };
            let truncated_by_lines = line_offset.saturating_add(selected_line_count) < lines.len();

            let mut result = Map::new();
            result.insert(
                "file_path".to_string(),
                Value::String(path.display().to_string()),
            );
            result.insert("start_line".to_string(), Value::from(start_line));
            result.insert("end_line".to_string(), Value::from(end_line));
            result.insert("total_lines".to_string(), Value::from(lines.len()));
            if truncated_by_lines || truncated_by_bytes || truncated_long_lines {
                result.insert("truncated".to_string(), Value::Bool(true));
            }
            if truncated_by_lines {
                result.insert("truncated_by_lines".to_string(), Value::Bool(true));
            }
            if truncated_by_bytes {
                result.insert("truncated_by_bytes".to_string(), Value::Bool(true));
            }
            if truncated_long_lines {
                result.insert("truncated_long_lines".to_string(), Value::Bool(true));
            }
            result.insert("content".to_string(), Value::String(content));
            Ok(Value::Object(result))
        },
    )
}

pub(super) fn file_write_tool(
    workspace_root: PathBuf,
    remote_workpaths: RemoteWorkpathMap,
    _cancel_flag: Option<Arc<InterruptSignal>>,
) -> Tool {
    Tool::new(
        "file_write",
        "Write a UTF-8 text file.",
        json!({
            "type": "object",
            "properties": {
                "file_path": {"type": "string"},
                "content": {"type": "string"},
                "mode": {"type": "string", "enum": ["overwrite", "append"]},
                "encoding": {"type": "string"},
                "remote": remote_schema_property()
            },
            "required": ["file_path", "content"],
            "additionalProperties": false
        }),
        move |arguments| {
            let arguments = arguments
                .as_object()
                .ok_or_else(|| anyhow!("tool arguments must be an object"))?;
            if let ExecutionTarget::RemoteSsh { host } = execution_target_arg(arguments)? {
                return run_remote_file_tool(&host, "file_write", arguments, &remote_workpaths);
            }
            let file_path = string_arg_with_alias(arguments, "file_path", "path")?;
            let path = resolve_path(&file_path, &workspace_root);
            let content = string_arg(arguments, "content")?;
            let mode = string_arg_with_default(arguments, "mode", "overwrite")?;
            let encoding = string_arg_with_default(arguments, "encoding", "utf-8")?;

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
                "file_path": path.display().to_string(),
                "mode": mode,
                "bytes_written": content.len()
            }))
        },
    )
}

pub(super) fn glob_tool(
    workspace_root: PathBuf,
    remote_workpaths: RemoteWorkpathMap,
    _cancel_flag: Option<Arc<InterruptSignal>>,
) -> Tool {
    Tool::new(
        "glob",
        "Fast file pattern matching tool. Supports glob patterns like **/*.rs and src/**/*.ts.",
        json!({
            "type": "object",
            "properties": {
                "pattern": {"type": "string"},
                "path": {"type": "string"},
                "remote": remote_schema_property()
            },
            "required": ["pattern"],
            "additionalProperties": false
        }),
        move |arguments| {
            let arguments = arguments
                .as_object()
                .ok_or_else(|| anyhow!("tool arguments must be an object"))?;
            if let ExecutionTarget::RemoteSsh { host } = execution_target_arg(arguments)? {
                return run_remote_file_tool(&host, "glob", arguments, &remote_workpaths);
            }
            let pattern = string_arg(arguments, "pattern")?;
            let base_path = resolve_path(
                &string_arg_with_default(arguments, "path", ".")?,
                &workspace_root,
            );
            if !base_path.exists() {
                return Err(anyhow!("{} does not exist", base_path.display()));
            }
            let matcher = Glob::new(&pattern)
                .with_context(|| format!("invalid glob pattern: {}", pattern))?
                .compile_matcher();
            let mut matches = collect_walk_paths(&base_path, false)
                .into_iter()
                .filter_map(|path| {
                    let relative = relative_display_path(&path, &base_path);
                    matcher.is_match(&relative).then(|| SearchMatch {
                        path: path.display().to_string(),
                        mtime_ms: file_mtime_ms(&path),
                    })
                })
                .collect::<Vec<_>>();
            sort_search_matches(&mut matches);
            let total_matches = matches.len();
            let truncated = total_matches > SEARCH_MAX_RESULTS;
            let filenames = matches
                .into_iter()
                .take(SEARCH_MAX_RESULTS)
                .map(|entry| entry.path)
                .collect::<Vec<_>>();
            let mut result = Map::new();
            result.insert("pattern".to_string(), Value::String(pattern));
            result.insert(
                "path".to_string(),
                Value::String(base_path.display().to_string()),
            );
            result.insert("num_files".to_string(), Value::from(total_matches));
            if truncated {
                result.insert("truncated".to_string(), Value::Bool(true));
            }
            result.insert("filenames".to_string(), json!(filenames));
            Ok(Value::Object(result))
        },
    )
}

pub(super) fn grep_tool(
    workspace_root: PathBuf,
    remote_workpaths: RemoteWorkpathMap,
    _cancel_flag: Option<Arc<InterruptSignal>>,
) -> Tool {
    Tool::new(
        "grep",
        "Fast content search tool. Searches file contents with a regex pattern and returns matching file paths.",
        json!({
            "type": "object",
            "properties": {
                "pattern": {"type": "string"},
                "path": {"type": "string"},
                "include": {"type": "string"},
                "remote": remote_schema_property()
            },
            "required": ["pattern"],
            "additionalProperties": false
        }),
        move |arguments| {
            let arguments = arguments
                .as_object()
                .ok_or_else(|| anyhow!("tool arguments must be an object"))?;
            if let ExecutionTarget::RemoteSsh { host } = execution_target_arg(arguments)? {
                return run_remote_file_tool(&host, "grep", arguments, &remote_workpaths);
            }
            let pattern = string_arg(arguments, "pattern")?;
            let base_path = resolve_path(
                &string_arg_with_default(arguments, "path", ".")?,
                &workspace_root,
            );
            if !base_path.exists() {
                return Err(anyhow!("{} does not exist", base_path.display()));
            }
            let regex = Regex::new(&pattern)
                .with_context(|| format!("invalid regex pattern: {}", pattern))?;
            let include =
                build_optional_glob_matcher(arguments.get("include").and_then(Value::as_str))?;

            let mut matches = Vec::new();
            for path in collect_walk_paths(&base_path, false) {
                let relative = relative_display_path(&path, &base_path);
                if let Some(include) = &include
                    && !include.is_match(&relative)
                {
                    continue;
                }
                let Ok(text) = fs::read_to_string(&path) else {
                    continue;
                };
                if regex.is_match(&text) {
                    matches.push(SearchMatch {
                        path: path.display().to_string(),
                        mtime_ms: file_mtime_ms(&path),
                    });
                }
            }
            sort_search_matches(&mut matches);
            let total_matches = matches.len();
            let truncated = total_matches > SEARCH_MAX_RESULTS;
            let filenames = matches
                .into_iter()
                .take(SEARCH_MAX_RESULTS)
                .map(|entry| entry.path)
                .collect::<Vec<_>>();
            let mut result = Map::new();
            result.insert("pattern".to_string(), Value::String(pattern));
            result.insert(
                "path".to_string(),
                Value::String(base_path.display().to_string()),
            );
            if let Some(include) = arguments.get("include").and_then(Value::as_str) {
                result.insert("include".to_string(), Value::String(include.to_string()));
            }
            result.insert("num_files".to_string(), Value::from(total_matches));
            if truncated {
                result.insert("truncated".to_string(), Value::Bool(true));
            }
            result.insert("filenames".to_string(), json!(filenames));
            Ok(Value::Object(result))
        },
    )
}

pub(super) fn ls_tool(
    workspace_root: PathBuf,
    remote_workpaths: RemoteWorkpathMap,
    _cancel_flag: Option<Arc<InterruptSignal>>,
) -> Tool {
    Tool::new(
        "ls",
        "List a recursive directory tree for non-hidden files and directories under a path. Skips common cache/build directories by default. Large trees are truncated to the first 1000 files and directories; pass a more specific path or use glob/grep when you know what to search for.",
        json!({
            "type": "object",
            "properties": {
                "path": {"type": "string"},
                "remote": remote_schema_property()
            },
            "required": ["path"],
            "additionalProperties": false
        }),
        move |arguments| {
            let arguments = arguments
                .as_object()
                .ok_or_else(|| anyhow!("tool arguments must be an object"))?;
            if let ExecutionTarget::RemoteSsh { host } = execution_target_arg(arguments)? {
                return run_remote_file_tool(&host, "ls", arguments, &remote_workpaths);
            }
            let base_path = resolve_path(&string_arg(arguments, "path")?, &workspace_root);
            if !base_path.exists() {
                return Err(anyhow!("{} does not exist", base_path.display()));
            }
            if !base_path.is_dir() {
                return Err(anyhow!("{} is not a directory", base_path.display()));
            }

            let (paths, truncated) = collect_ls_paths(&base_path, LS_MAX_ENTRIES);
            let mut entries = Vec::new();
            for path in paths {
                entries.push(LsEntry {
                    path: relative_display_path(&path, &base_path),
                    is_dir: path.is_dir(),
                });
            }
            Ok(Value::String(render_ls_tree(
                &base_path, entries, truncated,
            )))
        },
    )
}

pub(super) fn edit_tool(
    workspace_root: PathBuf,
    remote_workpaths: RemoteWorkpathMap,
    _cancel_flag: Option<Arc<InterruptSignal>>,
) -> Tool {
    Tool::new(
        "edit",
        "Edit a UTF-8 text file by replacing old_text with new_text. When replace_all=false, old_text must match exactly one location; if it matches multiple locations, include more surrounding context.",
        json!({
            "type": "object",
            "properties": {
                "path": {"type": "string"},
                "old_text": {"type": "string"},
                "new_text": {"type": "string"},
                "replace_all": {"type": "boolean"},
                "create_if_missing": {"type": "boolean"},
                "encoding": {"type": "string"},
                "remote": remote_schema_property()
            },
            "required": ["path", "old_text", "new_text"],
            "additionalProperties": false
        }),
        move |arguments| {
            let arguments = arguments
                .as_object()
                .ok_or_else(|| anyhow!("tool arguments must be an object"))?;
            if let ExecutionTarget::RemoteSsh { host } = execution_target_arg(arguments)? {
                return run_remote_file_tool(&host, "edit", arguments, &remote_workpaths);
            }
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
            let encoding = string_arg_with_default(arguments, "encoding", "utf-8")?;

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
            if replacements > 1 && !replace_all {
                return Err(anyhow!(
                    "old_text matched {} locations in {}; include more surrounding context or set replace_all=true",
                    replacements,
                    path.display()
                ));
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
        },
    )
}
