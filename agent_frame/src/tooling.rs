use crate::config::{ExternalWebSearchConfig, UpstreamConfig};
use crate::skills::{
    SkillMetadata, build_skill_index, load_skill_by_name, validate_skill_markdown,
};
use crate::tool_worker::ToolWorkerJob;
use anyhow::{Context, Result, anyhow};
use crossbeam_channel::{self, Receiver};
use globset::{Glob, GlobMatcher};
use regex::Regex;
use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::{Map, Value, json};
use std::collections::BTreeMap;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use uuid::Uuid;
use walkdir::{DirEntry, WalkDir};

type ToolHandler = dyn Fn(Value) -> Result<Value> + Send + Sync + 'static;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ToolExecutionMode {
    Immediate,
    Interruptible,
}

#[derive(Clone)]
pub struct Tool {
    pub name: String,
    pub description: String,
    pub parameters: Value,
    pub execution_mode: ToolExecutionMode,
    handler: Arc<ToolHandler>,
}

impl Tool {
    pub fn new(
        name: impl Into<String>,
        description: impl Into<String>,
        parameters: Value,
        handler: impl Fn(Value) -> Result<Value> + Send + Sync + 'static,
    ) -> Self {
        Self::new_with_mode(
            ToolExecutionMode::Immediate,
            name,
            description,
            parameters,
            handler,
        )
    }

    pub fn new_interruptible(
        name: impl Into<String>,
        description: impl Into<String>,
        parameters: Value,
        handler: impl Fn(Value) -> Result<Value> + Send + Sync + 'static,
    ) -> Self {
        Self::new_with_mode(
            ToolExecutionMode::Interruptible,
            name,
            description,
            parameters,
            handler,
        )
    }

    pub fn new_with_mode(
        execution_mode: ToolExecutionMode,
        name: impl Into<String>,
        description: impl Into<String>,
        parameters: Value,
        handler: impl Fn(Value) -> Result<Value> + Send + Sync + 'static,
    ) -> Self {
        Self {
            name: name.into(),
            description: description.into(),
            parameters,
            execution_mode,
            handler: Arc::new(handler),
        }
    }

    pub fn as_openai_tool(&self) -> Value {
        let execution_guidance = match self.execution_mode {
            ToolExecutionMode::Immediate => {
                "Execution mode: immediate. This tool returns promptly and does not use a top-level timeout parameter."
            }
            ToolExecutionMode::Interruptible => {
                "Execution mode: interruptible. This tool may wait, but the runtime can interrupt it when a newer user message arrives or the turn hits its timeout observation boundary."
            }
        };
        json!({
            "type": "function",
            "function": {
                "name": self.name,
                "description": format!("{} {}", execution_guidance, self.description),
                "parameters": self.parameters,
            }
        })
    }

    pub fn as_responses_tool(&self) -> Value {
        let execution_guidance = match self.execution_mode {
            ToolExecutionMode::Immediate => {
                "Execution mode: immediate. This tool returns promptly and does not use a top-level timeout parameter."
            }
            ToolExecutionMode::Interruptible => {
                "Execution mode: interruptible. This tool may wait, but the runtime can interrupt it when a newer user message arrives or the turn hits its timeout observation boundary."
            }
        };
        json!({
            "type": "function",
            "name": self.name,
            "description": format!("{} {}", execution_guidance, self.description),
            "parameters": self.parameters,
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

fn string_array_arg(arguments: &Map<String, Value>, key: &str) -> Result<Vec<String>> {
    let Some(value) = arguments.get(key) else {
        return Ok(Vec::new());
    };
    let items = value
        .as_array()
        .ok_or_else(|| anyhow!("argument {} must be an array of strings", key))?;
    let mut values = Vec::with_capacity(items.len());
    for item in items {
        values.push(
            item.as_str()
                .ok_or_else(|| anyhow!("argument {} must be an array of strings", key))?
                .to_string(),
        );
    }
    Ok(values)
}

fn validate_skill_name_component(skill_name: &str) -> Result<String> {
    let trimmed = skill_name.trim();
    if trimmed.is_empty() {
        return Err(anyhow!("skill_name must be a non-empty string"));
    }
    if trimmed == "." || trimmed == ".." || trimmed.contains('/') || trimmed.contains('\\') {
        return Err(anyhow!(
            "skill_name must be a single path component without path separators"
        ));
    }
    Ok(trimmed.to_string())
}

fn copy_dir_recursive(source: &Path, target: &Path) -> Result<()> {
    fs::create_dir_all(target).with_context(|| format!("failed to create {}", target.display()))?;
    for entry in
        fs::read_dir(source).with_context(|| format!("failed to read {}", source.display()))?
    {
        let entry = entry.with_context(|| format!("failed to read {}", source.display()))?;
        let source_path = entry.path();
        let target_path = target.join(entry.file_name());
        let file_type = entry
            .file_type()
            .with_context(|| format!("failed to stat {}", source_path.display()))?;
        if file_type.is_dir() {
            copy_dir_recursive(&source_path, &target_path)?;
        } else if file_type.is_file() {
            if let Some(parent) = target_path.parent() {
                fs::create_dir_all(parent)
                    .with_context(|| format!("failed to create {}", parent.display()))?;
            }
            fs::copy(&source_path, &target_path).with_context(|| {
                format!(
                    "failed to copy {} to {}",
                    source_path.display(),
                    target_path.display()
                )
            })?;
        }
    }
    Ok(())
}

fn writable_skill_root(skill_roots: &[PathBuf]) -> Result<PathBuf> {
    skill_roots
        .first()
        .cloned()
        .ok_or_else(|| anyhow!("no writable skills directory is configured"))
}

fn persist_staged_skill_directory(
    workspace_root: &Path,
    skill_roots: &[PathBuf],
    skill_name: &str,
    require_existing: bool,
) -> Result<Value> {
    let skill_name = validate_skill_name_component(skill_name)?;
    let staged_dir = workspace_root.join(".skills").join(&skill_name);
    let staged_skill_md = staged_dir.join("SKILL.md");
    if !staged_skill_md.is_file() {
        return Err(anyhow!(
            "staged skill '{}' must exist at {}",
            skill_name,
            staged_skill_md.display()
        ));
    }
    let content = fs::read_to_string(&staged_skill_md)
        .with_context(|| format!("failed to read {}", staged_skill_md.display()))?;
    let (declared_name, description) = validate_skill_markdown(&content)?;
    if declared_name != skill_name {
        return Err(anyhow!(
            "SKILL.md frontmatter name '{}' must match skill_name '{}'",
            declared_name,
            skill_name
        ));
    }

    let skill_root = writable_skill_root(skill_roots)?;
    fs::create_dir_all(&skill_root)
        .with_context(|| format!("failed to create {}", skill_root.display()))?;
    let target_dir = skill_root.join(&skill_name);
    let target_exists = target_dir.exists();
    if require_existing && !target_exists {
        return Err(anyhow!(
            "cannot update unknown skill '{}'; create it first",
            skill_name
        ));
    }
    if !require_existing && target_exists {
        return Err(anyhow!(
            "skill '{}' already exists; use skill_update instead",
            skill_name
        ));
    }

    let temp_dir = skill_root.join(format!(".tmp-skill-{}-{}", skill_name, Uuid::new_v4()));
    if temp_dir.exists() {
        fs::remove_dir_all(&temp_dir)
            .with_context(|| format!("failed to remove {}", temp_dir.display()))?;
    }
    copy_dir_recursive(&staged_dir, &temp_dir)?;
    if target_exists {
        fs::remove_dir_all(&target_dir)
            .with_context(|| format!("failed to replace {}", target_dir.display()))?;
    }
    fs::rename(&temp_dir, &target_dir).with_context(|| {
        format!(
            "failed to move {} into {}",
            temp_dir.display(),
            target_dir.display()
        )
    })?;

    Ok(json!({
        "name": declared_name,
        "description": description,
        "persisted": true,
        "created": !require_existing,
        "updated": require_existing
    }))
}

fn resolve_path(path: &str, workspace_root: &Path) -> PathBuf {
    let path_buf = PathBuf::from(path);
    if path_buf.is_absolute() {
        path_buf
    } else {
        workspace_root.join(path_buf)
    }
}

fn canonical_tool_name(tool_name: &str) -> &str {
    match tool_name {
        "read_file" => "file_read",
        "write_file" => "file_write",
        _ => tool_name,
    }
}

fn string_arg_with_alias(arguments: &Map<String, Value>, key: &str, alias: &str) -> Result<String> {
    arguments
        .get(key)
        .or_else(|| arguments.get(alias))
        .ok_or_else(|| anyhow!("missing required argument: {}", key))?
        .as_str()
        .map(ToOwned::to_owned)
        .ok_or_else(|| anyhow!("argument {} must be a string", key))
}

fn usize_arg_with_alias(
    arguments: &Map<String, Value>,
    key: &str,
    alias: &str,
) -> Result<Option<usize>> {
    match arguments.get(key).or_else(|| arguments.get(alias)) {
        Some(value) => value
            .as_u64()
            .map(|value| Some(value as usize))
            .ok_or_else(|| anyhow!("argument {} must be an integer", key)),
        None => Ok(None),
    }
}

const FILE_READ_DEFAULT_LIMIT: usize = 2_000;
const FILE_READ_MAX_LINE_LENGTH: usize = 2_000;
const FILE_READ_MAX_OUTPUT_BYTES: usize = 256 * 1024;
const SEARCH_MAX_RESULTS: usize = 100;
const LS_MAX_ENTRIES: usize = 1_000;

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
        lines.push("truncated: false".to_string());
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

#[derive(Clone, Debug, PartialEq, Eq)]
enum ExecutionTarget {
    Local,
    RemoteSsh { host: String },
}

impl ExecutionTarget {
    fn remote_name(&self) -> &str {
        match self {
            Self::Local => "local",
            Self::RemoteSsh { host } => host,
        }
    }
}

fn validate_remote_host(host: &str) -> Result<String> {
    let trimmed = host.trim();
    if trimmed.is_empty() || trimmed == "local" {
        return Ok("local".to_string());
    }
    if matches!(trimmed, "host" | "<host>" | "<host>|local") {
        return Err(anyhow!(
            "remote must be an actual SSH host alias or local; omit remote or use an empty string for local work"
        ));
    }
    if trimmed.starts_with('-') {
        return Err(anyhow!("remote SSH host must not start with '-'"));
    }
    if trimmed.chars().any(char::is_whitespace) {
        return Err(anyhow!("remote SSH host must not contain whitespace"));
    }
    if trimmed.chars().any(|ch| ch.is_control()) {
        return Err(anyhow!(
            "remote SSH host must not contain control characters"
        ));
    }
    if trimmed.chars().any(|ch| {
        matches!(
            ch,
            '\'' | '"' | '`' | '$' | ';' | '&' | '|' | '<' | '>' | '(' | ')'
        )
    }) {
        return Err(anyhow!(
            "remote SSH host must not contain shell metacharacters"
        ));
    }
    if trimmed.contains('/') || trimmed.contains('\\') {
        return Err(anyhow!("remote SSH host must not contain path separators"));
    }
    Ok(trimmed.to_string())
}

fn remote_schema_property() -> Value {
    json!({
        "type": "string",
        "description": "Optional execution target. Format: \"<host>|local\". Omit remote, set remote=\"\", or set remote=\"local\" for local execution. For SSH execution, use an actual SSH alias such as \"wuwen-dev6\"; do not pass the literal placeholder \"host\"."
    })
}

fn execution_target_arg(arguments: &Map<String, Value>) -> Result<ExecutionTarget> {
    let Some(value) = arguments.get("remote") else {
        return Ok(ExecutionTarget::Local);
    };
    let remote = value
        .as_str()
        .ok_or_else(|| anyhow!("argument remote must be a string"))?;
    let host = validate_remote_host(remote)?;
    if host == "local" {
        Ok(ExecutionTarget::Local)
    } else {
        Ok(ExecutionTarget::RemoteSsh { host })
    }
}

fn shell_quote(value: &str) -> String {
    if value.is_empty() {
        return "''".to_string();
    }
    format!("'{}'", value.replace('\'', "'\"'\"'"))
}

fn resolve_ssh_executable() -> String {
    std::env::var("AGENT_FRAME_SSH_BIN").unwrap_or_else(|_| "ssh".to_string())
}

fn ssh_command(host: &str, tty: bool) -> Command {
    let mut command = Command::new(resolve_ssh_executable());
    command
        .arg("-o")
        .arg("BatchMode=yes")
        .arg("-o")
        .arg("ConnectTimeout=10");
    if tty {
        command.arg("-tt");
    } else {
        command.arg("-T");
    }
    command.arg(host);
    command
}

fn remote_workspace_root(workspace_root: &Path) -> String {
    workspace_root.to_string_lossy().to_string()
}

fn remote_python_command(script: &str) -> String {
    format!(
        "if command -v python3 >/dev/null 2>&1; then exec python3 -c {}; elif command -v python >/dev/null 2>&1; then exec python -c {}; else echo 'remote file tools require Python 3 on this host; install python3/python or use exec_start remote=\"<host>\" for shell-only commands' >&2; exit 127; fi",
        shell_quote(script),
        shell_quote(script)
    )
}

fn run_remote_command(
    host: &str,
    remote_cwd: &str,
    command_args: &[String],
    stdin: Option<&[u8]>,
) -> Result<std::process::Output> {
    let mut script = format!("cd {} &&", shell_quote(remote_cwd));
    for arg in command_args {
        script.push(' ');
        script.push_str(&shell_quote(arg));
    }
    let mut command = ssh_command(host, false);
    let remote_command = format!("sh -lc {}", shell_quote(&script));
    command
        .arg(remote_command)
        .stdin(if stdin.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = command
        .spawn()
        .with_context(|| format!("failed to spawn ssh for remote host {}", host))?;
    if let Some(stdin) = stdin {
        child
            .stdin
            .as_mut()
            .ok_or_else(|| anyhow!("failed to open ssh stdin"))?
            .write_all(stdin)
            .context("failed to write ssh stdin")?;
    }
    let _ = child.stdin.take();
    child
        .wait_with_output()
        .with_context(|| format!("failed to wait for ssh host {}", host))
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
    return {
        "file_path": path,
        "start_line": start_line,
        "end_line": end_line,
        "total_lines": len(lines),
        "truncated": truncated_by_lines or truncated_by_bytes or truncated_long_lines,
        "truncated_by_lines": truncated_by_lines,
        "truncated_by_bytes": truncated_by_bytes,
        "truncated_long_lines": truncated_long_lines,
        "content": "\n".join(content),
    }

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
    return {
        "pattern": pattern,
        "path": base,
        "num_files": total,
        "truncated": total > SEARCH_MAX_RESULTS,
        "filenames": [item["path"] for item in matches[:SEARCH_MAX_RESULTS]],
    }

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
    return {
        "pattern": pattern,
        "path": base,
        "include": include,
        "num_files": total,
        "truncated": total > SEARCH_MAX_RESULTS,
        "filenames": [item["path"] for item in matches[:SEARCH_MAX_RESULTS]],
    }

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
        lines.append("truncated: false")
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
    workspace_root: &Path,
    operation: &str,
    arguments: &Map<String, Value>,
) -> Result<Value> {
    let remote_root = remote_workspace_root(workspace_root);
    let payload = json!({
        "operation": operation,
        "workspace_root": remote_root,
        "arguments": arguments,
    });
    let stdin = serde_json::to_vec(&payload).context("failed to serialize remote tool payload")?;
    let output = run_remote_command(
        host,
        &remote_workspace_root(workspace_root),
        &[
            "sh".to_string(),
            "-lc".to_string(),
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

fn run_remote_apply_patch(
    host: &str,
    workspace_root: &Path,
    patch: &str,
    strip: usize,
    reverse: bool,
    check: bool,
) -> Result<Value> {
    let mut args = vec![
        "git".to_string(),
        "apply".to_string(),
        "--recount".to_string(),
        "--whitespace=nowarn".to_string(),
        format!("-p{}", strip),
    ];
    if reverse {
        args.push("--reverse".to_string());
    }
    if check {
        args.push("--check".to_string());
    }
    let output = run_remote_command(
        host,
        &remote_workspace_root(workspace_root),
        &args,
        Some(patch.as_bytes()),
    )?;
    Ok(json!({
        "remote": host,
        "applied": output.status.success(),
        "returncode": output.status.code().unwrap_or(-1),
        "stdout": String::from_utf8_lossy(&output.stdout),
        "stderr": String::from_utf8_lossy(&output.stderr)
    }))
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

fn file_read_tool(workspace_root: PathBuf, _cancel_flag: Option<Arc<InterruptSignal>>) -> Tool {
    Tool::new(
        "file_read",
        "Read a UTF-8 text file. Supports file_path plus optional offset and limit for large files. Optional remote=\"<host>|local\" runs this single tool call over SSH when set to an actual host alias; omit remote or set remote=\"\" for local reads.",
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
                return run_remote_file_tool(&host, &workspace_root, "file_read", arguments);
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

            Ok(json!({
                "file_path": path.display().to_string(),
                "start_line": start_line,
                "end_line": end_line,
                "total_lines": lines.len(),
                "truncated": truncated_by_lines || truncated_by_bytes || truncated_long_lines,
                "truncated_by_lines": truncated_by_lines,
                "truncated_by_bytes": truncated_by_bytes,
                "truncated_long_lines": truncated_long_lines,
                "content": content
            }))
        },
    )
}

fn file_write_tool(workspace_root: PathBuf, _cancel_flag: Option<Arc<InterruptSignal>>) -> Tool {
    Tool::new(
        "file_write",
        "Write a UTF-8 text file. Optional remote=\"<host>|local\" runs this single tool call over SSH when set to an actual host alias; omit remote or set remote=\"\" for local writes.",
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
                return run_remote_file_tool(&host, &workspace_root, "file_write", arguments);
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

fn glob_tool(workspace_root: PathBuf, _cancel_flag: Option<Arc<InterruptSignal>>) -> Tool {
    Tool::new(
        "glob",
        "Fast file pattern matching tool. Supports glob patterns like **/*.rs and src/**/*.ts. Optional remote=\"<host>|local\" runs this single tool call over SSH when set to an actual host alias; omit remote or set remote=\"\" for local matching.",
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
                return run_remote_file_tool(&host, &workspace_root, "glob", arguments);
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
            Ok(json!({
                "pattern": pattern,
                "path": base_path.display().to_string(),
                "num_files": total_matches,
                "truncated": truncated,
                "filenames": filenames
            }))
        },
    )
}

fn grep_tool(workspace_root: PathBuf, _cancel_flag: Option<Arc<InterruptSignal>>) -> Tool {
    Tool::new(
        "grep",
        "Fast content search tool. Searches file contents with a regex pattern and returns matching file paths. Optional remote=\"<host>|local\" runs this single tool call over SSH when set to an actual host alias; omit remote or set remote=\"\" for local search.",
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
                return run_remote_file_tool(&host, &workspace_root, "grep", arguments);
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
            Ok(json!({
                "pattern": pattern,
                "path": base_path.display().to_string(),
                "include": arguments.get("include").and_then(Value::as_str),
                "num_files": total_matches,
                "truncated": truncated,
                "filenames": filenames
            }))
        },
    )
}

fn ls_tool(workspace_root: PathBuf, _cancel_flag: Option<Arc<InterruptSignal>>) -> Tool {
    Tool::new(
        "ls",
        "List a recursive directory tree for non-hidden files and directories under a path. Skips common cache/build directories by default. Large trees are truncated to the first 1000 files and directories; pass a more specific path or use glob/grep when you know what to search for. Optional remote=\"<host>|local\" runs this single tool call over SSH when set to an actual host alias; omit remote or set remote=\"\" for local listing.",
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
                return run_remote_file_tool(&host, &workspace_root, "ls", arguments);
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

fn edit_tool(workspace_root: PathBuf, _cancel_flag: Option<Arc<InterruptSignal>>) -> Tool {
    Tool::new(
        "edit",
        "Edit a UTF-8 text file by replacing old_text with new_text. Optional remote=\"<host>|local\" runs this single tool call over SSH when set to an actual host alias; omit remote or set remote=\"\" for local edits.",
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
                return run_remote_file_tool(&host, &workspace_root, "edit", arguments);
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

const EXEC_START_DEFAULT_WAIT_TIMEOUT_SECONDS: f64 = 270.0;
const EXEC_OUTPUT_MAX_CHARS: usize = 1000;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ExecTimeoutAction {
    Continue,
    Kill,
}

impl ExecTimeoutAction {
    fn as_str(self) -> &'static str {
        match self {
            Self::Continue => "continue",
            Self::Kill => "kill",
        }
    }
}

fn exec_timeout_action_arg(
    arguments: &Map<String, Value>,
    key: &str,
    default: ExecTimeoutAction,
) -> Result<ExecTimeoutAction> {
    let Some(value) = arguments.get(key) else {
        return Ok(default);
    };
    let text = value
        .as_str()
        .ok_or_else(|| anyhow!("argument {} must be a string", key))?
        .trim()
        .to_ascii_lowercase();
    match text.as_str() {
        "continue" => Ok(ExecTimeoutAction::Continue),
        "kill" => Ok(ExecTimeoutAction::Kill),
        _ => Err(anyhow!("argument {} must be one of: continue, kill", key)),
    }
}

fn max_output_chars_arg(arguments: &Map<String, Value>) -> Result<usize> {
    let value = usize_arg_with_default(arguments, "max_output_chars", EXEC_OUTPUT_MAX_CHARS)?;
    if value > EXEC_OUTPUT_MAX_CHARS {
        return Err(anyhow!(
            "argument max_output_chars must be less than or equal to {}",
            EXEC_OUTPUT_MAX_CHARS
        ));
    }
    Ok(value)
}

#[derive(Clone, Serialize, serde::Deserialize)]
struct ProcessMetadata {
    exec_id: String,
    worker_pid: u32,
    #[serde(default)]
    tty: bool,
    #[serde(default = "default_remote_local")]
    remote: String,
    command: String,
    cwd: String,
    stdout_path: String,
    stderr_path: String,
    status_path: String,
    worker_exit_code_path: String,
    requests_dir: String,
}

fn default_remote_local() -> String {
    "local".to_string()
}

#[derive(serde::Deserialize)]
struct LegacyProcessMetadata {
    exec_id: String,
    pid: u32,
    command: String,
    cwd: String,
    stdout_path: String,
    stderr_path: String,
    exit_code_path: String,
}

enum ProcessMetadataRecord {
    Current(ProcessMetadata),
    Legacy(LegacyProcessMetadata),
}

static LIVE_PROCESSES: std::sync::OnceLock<Mutex<BTreeMap<String, ProcessMetadata>>> =
    std::sync::OnceLock::new();

fn live_processes() -> &'static Mutex<BTreeMap<String, ProcessMetadata>> {
    LIVE_PROCESSES.get_or_init(|| Mutex::new(BTreeMap::new()))
}

fn process_meta_path(dir: &Path, exec_id: &str) -> PathBuf {
    dir.join(format!("{}.json", exec_id))
}

fn parse_process_metadata_record(raw: &str) -> Result<ProcessMetadataRecord> {
    match serde_json::from_str::<ProcessMetadata>(raw) {
        Ok(metadata) => Ok(ProcessMetadataRecord::Current(metadata)),
        Err(current_err) => match serde_json::from_str::<LegacyProcessMetadata>(raw) {
            Ok(metadata) => Ok(ProcessMetadataRecord::Legacy(metadata)),
            Err(_) => Err(current_err).context("failed to parse process metadata"),
        },
    }
}

fn legacy_process_metadata_to_current(
    dir: &Path,
    metadata: LegacyProcessMetadata,
) -> ProcessMetadata {
    ProcessMetadata {
        exec_id: metadata.exec_id.clone(),
        worker_pid: metadata.pid,
        tty: false,
        remote: default_remote_local(),
        command: metadata.command,
        cwd: metadata.cwd,
        stdout_path: metadata.stdout_path,
        stderr_path: metadata.stderr_path,
        status_path: dir
            .join(format!("{}.status.json", metadata.exec_id))
            .display()
            .to_string(),
        worker_exit_code_path: metadata.exit_code_path,
        requests_dir: dir
            .join(format!("{}.requests", metadata.exec_id))
            .display()
            .to_string(),
    }
}

fn read_process_metadata(dir: &Path, exec_id: &str) -> Result<ProcessMetadata> {
    let path = process_meta_path(dir, exec_id);
    let raw =
        fs::read_to_string(&path).with_context(|| format!("failed to read {}", path.display()))?;
    match parse_process_metadata_record(&raw)? {
        ProcessMetadataRecord::Current(metadata) => Ok(metadata),
        ProcessMetadataRecord::Legacy(metadata) => {
            Ok(legacy_process_metadata_to_current(dir, metadata))
        }
    }
}

fn write_process_metadata(dir: &Path, metadata: &ProcessMetadata) -> Result<()> {
    let raw =
        serde_json::to_string_pretty(metadata).context("failed to serialize process metadata")?;
    fs::write(process_meta_path(dir, &metadata.exec_id), raw)
        .with_context(|| format!("failed to write process metadata for {}", metadata.exec_id))
}

fn read_file_lines_window(
    path: &Path,
    start: usize,
    limit: usize,
) -> Result<(String, usize, bool)> {
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
    let end = lines.len().saturating_sub(start);
    let begin = end.saturating_sub(limit);
    let line_window_truncated = begin > 0 || start > 0;
    Ok((
        lines[begin..end].join("\n"),
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

fn workspace_output_relative_path(exec_id: &str, stream: &str) -> PathBuf {
    PathBuf::from(".agent_frame")
        .join("exec")
        .join(format!("{exec_id}.{stream}"))
}

fn format_relative_path(path: &Path) -> String {
    path.components()
        .map(|component| component.as_os_str().to_string_lossy())
        .collect::<Vec<_>>()
        .join("/")
}

fn sync_workspace_output_file(
    workspace_root: Option<&Path>,
    metadata: &ProcessMetadata,
    source: &Path,
    stream: &str,
) -> Result<Option<String>> {
    let Some(workspace_root) = workspace_root else {
        return Ok(None);
    };
    let relative_path = workspace_output_relative_path(&metadata.exec_id, stream);
    let destination = workspace_root.join(&relative_path);
    if source != destination {
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
    }
    Ok(Some(format_relative_path(&relative_path)))
}

fn insert_exec_output_fields(
    object: &mut Map<String, Value>,
    metadata: &ProcessMetadata,
    workspace_root: Option<&Path>,
    start: usize,
    limit: usize,
    max_output_chars: usize,
) -> Result<()> {
    let stdout_path = Path::new(&metadata.stdout_path);
    let stderr_path = Path::new(&metadata.stderr_path);
    let (stdout_window, stdout_chars, stdout_line_truncated) =
        read_file_lines_window(stdout_path, start, limit)?;
    let (stderr_window, stderr_chars, stderr_line_truncated) =
        read_file_lines_window(stderr_path, start, limit)?;
    let (stdout, stdout_truncated) = truncate_exec_output(&stdout_window, max_output_chars);
    let (stderr, stderr_truncated) = truncate_exec_output(&stderr_window, max_output_chars);

    object.insert("stdout".to_string(), Value::String(stdout));
    object.insert("stderr".to_string(), Value::String(stderr));
    object.insert(
        "stdout_truncated".to_string(),
        Value::Bool(stdout_truncated || stdout_line_truncated),
    );
    object.insert(
        "stderr_truncated".to_string(),
        Value::Bool(stderr_truncated || stderr_line_truncated),
    );
    object.insert("stdout_chars".to_string(), Value::from(stdout_chars));
    object.insert("stderr_chars".to_string(), Value::from(stderr_chars));
    if let Some(path) = sync_workspace_output_file(workspace_root, metadata, stdout_path, "stdout")?
    {
        object.insert("stdout_path".to_string(), Value::String(path));
    }
    if let Some(path) = sync_workspace_output_file(workspace_root, metadata, stderr_path, "stderr")?
    {
        object.insert("stderr_path".to_string(), Value::String(path));
    }
    Ok(())
}

fn read_exit_code(path: &Path) -> Option<i32> {
    fs::read_to_string(path)
        .ok()
        .and_then(|value| value.trim().parse::<i32>().ok())
}

#[cfg(not(windows))]
fn process_is_running(pid: u32) -> bool {
    Command::new("sh")
        .arg("-c")
        .arg(format!("kill -0 {} 2>/dev/null", pid))
        .status()
        .map(|status| status.success())
        .unwrap_or(false)
}

#[cfg(windows)]
fn process_is_running(pid: u32) -> bool {
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
fn terminate_process_pid(pid: u32) {
    let _ = Command::new("kill")
        .arg("-KILL")
        .arg(pid.to_string())
        .status();
}

#[cfg(windows)]
fn terminate_process_pid(pid: u32) {
    let _ = Command::new("taskkill")
        .args(["/PID", &pid.to_string(), "/T", "/F"])
        .status();
}

#[cfg(not(windows))]
fn terminate_process_group(pid: u32) {
    let _ = Command::new("kill")
        .arg("-KILL")
        .arg(format!("-{}", pid))
        .status();
}

#[cfg(windows)]
fn terminate_process_group(pid: u32) {
    terminate_process_pid(pid);
}

fn record_exit_code(path: &Path, code: i32) -> Result<()> {
    fs::write(path, code.to_string())
        .with_context(|| format!("failed to write exit code to {}", path.display()))
}

fn process_request_path(requests_dir: &Path, request_id: &str) -> PathBuf {
    requests_dir.join(format!("request-{request_id}.json"))
}

fn process_request_result_path(requests_dir: &Path, request_id: &str) -> PathBuf {
    requests_dir.join(format!("request-{request_id}.result.json"))
}

fn queue_exec_input_request(metadata: &ProcessMetadata, input: &str) -> Result<PathBuf> {
    let requests_dir = Path::new(&metadata.requests_dir);
    fs::create_dir_all(requests_dir)
        .with_context(|| format!("failed to create {}", requests_dir.display()))?;
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    let request_id = format!("{timestamp:020}-{}", Uuid::new_v4());
    let request_path = process_request_path(requests_dir, &request_id);
    let temp_path = requests_dir.join(format!("request-{request_id}.tmp"));
    fs::write(
        &temp_path,
        serde_json::to_vec_pretty(&json!({ "input": input }))
            .context("failed to serialize exec input request")?,
    )
    .with_context(|| format!("failed to write {}", temp_path.display()))?;
    fs::rename(&temp_path, &request_path).with_context(|| {
        format!(
            "failed to move {} to {}",
            temp_path.display(),
            request_path.display()
        )
    })?;
    Ok(process_request_result_path(requests_dir, &request_id))
}

fn read_exec_input_result(result_path: &Path) -> Result<Option<Value>> {
    if !result_path.exists() {
        return Ok(None);
    }
    let raw = fs::read_to_string(result_path)
        .with_context(|| format!("failed to read {}", result_path.display()))?;
    let value = serde_json::from_str(&raw).context("failed to parse exec input result")?;
    Ok(Some(value))
}

fn exec_pid_from_snapshot(snapshot: &Value) -> Option<u32> {
    snapshot
        .get("pid")
        .and_then(Value::as_u64)
        .and_then(|pid| u32::try_from(pid).ok())
}

fn kill_exec_processes(metadata: &ProcessMetadata, snapshot: Option<&Value>) {
    if let Some(exec_pid) = snapshot.and_then(exec_pid_from_snapshot)
        && exec_pid != metadata.worker_pid
    {
        if metadata.tty {
            terminate_process_group(exec_pid);
        } else {
            terminate_process_pid(exec_pid);
        }
    }
    terminate_process_pid(metadata.worker_pid);
}

fn write_exec_snapshot(
    metadata: &ProcessMetadata,
    running: bool,
    completed: bool,
    returncode: Value,
    extra: Option<Map<String, Value>>,
) -> Result<()> {
    let pid = read_status_json(Path::new(&metadata.status_path))
        .ok()
        .and_then(|value| value.get("pid").cloned())
        .unwrap_or(Value::Null);
    let stdin_closed = read_status_json(Path::new(&metadata.status_path))
        .ok()
        .and_then(|value| value.get("stdin_closed").cloned())
        .unwrap_or(Value::Bool(true));
    let failed = read_status_json(Path::new(&metadata.status_path))
        .ok()
        .and_then(|value| value.get("failed").cloned())
        .unwrap_or(Value::Bool(false));

    let mut object = Map::from_iter([
        (
            "exec_id".to_string(),
            Value::String(metadata.exec_id.clone()),
        ),
        ("tty".to_string(), Value::Bool(metadata.tty)),
        ("pid".to_string(), pid),
        (
            "command".to_string(),
            Value::String(metadata.command.clone()),
        ),
        ("remote".to_string(), Value::String(metadata.remote.clone())),
        ("cwd".to_string(), Value::String(metadata.cwd.clone())),
        ("running".to_string(), Value::Bool(running)),
        ("completed".to_string(), Value::Bool(completed)),
        ("returncode".to_string(), returncode),
        ("stdin_closed".to_string(), stdin_closed),
        ("failed".to_string(), failed),
    ]);
    if let Some(extra) = extra {
        object.extend(extra);
    }
    fs::write(
        &metadata.status_path,
        serde_json::to_vec_pretty(&Value::Object(object))
            .context("failed to serialize exec status snapshot")?,
    )
    .with_context(|| format!("failed to write {}", metadata.status_path))
}

fn spawn_managed_process(
    runtime_state_root: &Path,
    state_dir: &Path,
    workspace_root: &Path,
    command: &str,
    cwd: &Path,
    tty: bool,
    target: &ExecutionTarget,
) -> Result<ProcessMetadata> {
    let exec_id = Uuid::new_v4().to_string();
    let output_dir = workspace_root.join(".agent_frame").join("exec");
    fs::create_dir_all(&output_dir)
        .with_context(|| format!("failed to create {}", output_dir.display()))?;
    let stdout_path = output_dir.join(format!("{}.stdout", exec_id));
    let stderr_path = output_dir.join(format!("{}.stderr", exec_id));
    let status_path = state_dir.join(format!("{}.status.json", exec_id));
    let requests_dir = state_dir.join(format!("{}.requests", exec_id));
    fs::create_dir_all(&requests_dir)
        .with_context(|| format!("failed to create {}", requests_dir.display()))?;
    fs::write(
        &status_path,
        serde_json::to_vec_pretty(&json!({
            "exec_id": exec_id,
            "tty": tty,
            "pid": Value::Null,
            "command": command,
            "remote": target.remote_name(),
            "cwd": cwd.display().to_string(),
            "running": true,
            "completed": false,
            "returncode": Value::Null,
            "stdin_closed": false,
            "failed": false,
            "error": Value::Null,
        }))
        .context("failed to serialize initial exec status")?,
    )
    .with_context(|| format!("failed to write {}", status_path.display()))?;
    let worker = spawn_background_worker_process(
        runtime_state_root,
        "exec",
        &exec_id,
        &ToolWorkerJob::Exec {
            exec_id: exec_id.clone(),
            tty,
            remote: match target {
                ExecutionTarget::Local => None,
                ExecutionTarget::RemoteSsh { host } => Some(host.clone()),
            },
            command: command.to_string(),
            cwd: cwd.display().to_string(),
            status_path: status_path.display().to_string(),
            stdout_path: stdout_path.display().to_string(),
            stderr_path: stderr_path.display().to_string(),
            requests_dir: requests_dir.display().to_string(),
        },
    )?;
    let metadata = ProcessMetadata {
        exec_id: exec_id.clone(),
        worker_pid: worker.pid,
        tty,
        remote: target.remote_name().to_string(),
        command: command.to_string(),
        cwd: cwd.display().to_string(),
        stdout_path: stdout_path.display().to_string(),
        stderr_path: stderr_path.display().to_string(),
        status_path: status_path.display().to_string(),
        worker_exit_code_path: worker.exit_code_path,
        requests_dir: requests_dir.display().to_string(),
    };
    write_process_metadata(state_dir, &metadata)?;
    live_processes()
        .lock()
        .unwrap()
        .insert(exec_id, metadata.clone());
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
    max_output_chars: usize,
    workspace_root: Option<&Path>,
) -> Result<Value> {
    let metadata = match read_process_metadata(state_dir, exec_id) {
        Ok(metadata) => metadata,
        Err(_) => return Err(process_missing_error(exec_id)),
    };
    let mut snapshot = read_status_json(Path::new(&metadata.status_path))
        .map_err(|_| process_missing_error(exec_id))?;
    let worker_alive = read_exit_code(Path::new(&metadata.worker_exit_code_path)).is_none()
        && process_is_running(metadata.worker_pid);
    let running = snapshot
        .get("running")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    if running && !worker_alive {
        let pid = snapshot.get("pid").cloned().unwrap_or(Value::Null);
        snapshot = json!({
            "exec_id": metadata.exec_id,
            "tty": metadata.tty,
            "pid": pid,
            "command": metadata.command,
            "remote": metadata.remote,
            "cwd": metadata.cwd,
            "running": false,
            "completed": false,
            "returncode": Value::Null,
            "stdin_closed": true,
            "failed": true,
            "error": "exec worker exited unexpectedly",
        });
    }
    let mut object = snapshot
        .as_object()
        .cloned()
        .ok_or_else(|| anyhow!("exec snapshot must be a JSON object"))?;
    object
        .entry("tty".to_string())
        .or_insert_with(|| Value::Bool(metadata.tty));
    insert_exec_output_fields(
        &mut object,
        &metadata,
        workspace_root,
        start,
        limit,
        max_output_chars,
    )?;
    Ok(Value::Object(object))
}

fn list_active_exec_summaries(runtime_state_root: &Path) -> Result<Vec<String>> {
    let Some(state_dir) = process_state_dir_if_exists(runtime_state_root) else {
        return Ok(Vec::new());
    };
    let mut entries = Vec::new();
    for entry in fs::read_dir(&state_dir)
        .with_context(|| format!("failed to read {}", state_dir.display()))?
    {
        let entry = entry?;
        let path = entry.path();
        let Some(file_name) = path.file_name().and_then(|value| value.to_str()) else {
            continue;
        };
        if !file_name.ends_with(".json") || file_name.ends_with(".status.json") {
            continue;
        }
        let raw = fs::read_to_string(&path)
            .with_context(|| format!("failed to read {}", path.display()))?;
        let (metadata, allow_missing_snapshot) = match parse_process_metadata_record(&raw)? {
            ProcessMetadataRecord::Current(metadata) => (metadata, false),
            // Older workdirs can still contain pre-worker exec metadata files.
            // Skip them for runtime summaries so compaction stays best-effort.
            ProcessMetadataRecord::Legacy(metadata) => (
                legacy_process_metadata_to_current(&state_dir, metadata),
                true,
            ),
        };
        let snapshot = match read_process_snapshot(
            &state_dir,
            &metadata.exec_id,
            0,
            0,
            EXEC_OUTPUT_MAX_CHARS,
            None,
        ) {
            Ok(snapshot) => snapshot,
            Err(_) if allow_missing_snapshot => continue,
            Err(err) => return Err(err),
        };
        if !snapshot
            .get("running")
            .and_then(Value::as_bool)
            .unwrap_or(false)
        {
            continue;
        }
        let pid = snapshot
            .get("pid")
            .and_then(Value::as_u64)
            .unwrap_or(metadata.worker_pid as u64);
        entries.push(format!(
            "- exec_id=`{}` pid={} remote=`{}` tty={} cwd=`{}` command=`{}`",
            metadata.exec_id, pid, metadata.remote, metadata.tty, metadata.cwd, metadata.command
        ));
    }
    entries.sort();
    Ok(entries)
}

pub fn terminate_all_managed_processes() -> Result<()> {
    let mut registry = live_processes().lock().unwrap();
    let processes = std::mem::take(&mut *registry)
        .into_values()
        .collect::<Vec<_>>();
    drop(registry);
    for process in processes {
        let state_dir = Path::new(&process.status_path)
            .parent()
            .unwrap_or_else(|| Path::new("."));
        let snapshot = read_process_snapshot(
            state_dir,
            &process.exec_id,
            0,
            0,
            EXEC_OUTPUT_MAX_CHARS,
            None,
        )
        .ok();
        let meta_path = process_meta_path(state_dir, &process.exec_id);
        let _ = fs::remove_file(meta_path);
        let _ = fs::remove_file(&process.worker_exit_code_path);
        let _ = fs::remove_file(&process.status_path);
        let _ = fs::remove_dir_all(&process.requests_dir);
        kill_exec_processes(&process, snapshot.as_ref());
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
    max_output_chars: usize,
    workspace_root: Option<&Path>,
    on_timeout: ExecTimeoutAction,
    cancel_flag: Option<&Arc<InterruptSignal>>,
) -> Result<Value> {
    if !wait_timeout_seconds.is_finite() || wait_timeout_seconds < 0.0 {
        return Err(anyhow!(
            "argument wait_timeout_seconds must be a finite non-negative number"
        ));
    }
    let pending_input_result = if let Some(input) = input {
        let metadata = read_process_metadata(state_dir, exec_id)
            .map_err(|_| process_missing_error(exec_id))?;
        Some(queue_exec_input_request(&metadata, input)?)
    } else {
        None
    };
    let deadline = std::time::Instant::now() + Duration::from_secs_f64(wait_timeout_seconds);
    let cancel_receiver = cancel_flag.map(|signal| signal.subscribe());
    let mut input_acknowledged = pending_input_result.is_none();
    loop {
        let snapshot = read_process_snapshot(
            state_dir,
            exec_id,
            start,
            limit,
            max_output_chars,
            workspace_root,
        )?;
        if !input_acknowledged && let Some(result_path) = pending_input_result.as_ref() {
            if let Some(result) = read_exec_input_result(result_path)? {
                let _ = fs::remove_file(result_path);
                let ok = result.get("ok").and_then(Value::as_bool).unwrap_or(false);
                if !ok {
                    let error = result
                        .get("error")
                        .and_then(Value::as_str)
                        .unwrap_or("stdin is closed for exec process");
                    return Err(anyhow!(error.to_string()));
                }
                input_acknowledged = true;
            } else {
                let running = snapshot
                    .get("running")
                    .and_then(Value::as_bool)
                    .unwrap_or(false);
                let stdin_closed = snapshot
                    .get("stdin_closed")
                    .and_then(Value::as_bool)
                    .unwrap_or(false);
                let failed = snapshot
                    .get("failed")
                    .and_then(Value::as_bool)
                    .unwrap_or(false);
                if stdin_closed || failed || !running {
                    let error = snapshot
                        .get("error")
                        .and_then(Value::as_str)
                        .map(ToOwned::to_owned)
                        .unwrap_or_else(|| format!("stdin is closed for exec process {}", exec_id));
                    return Err(anyhow!(error));
                }
                thread::sleep(Duration::from_millis(50));
                continue;
            }
        }

        let completed = snapshot
            .get("completed")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        if completed {
            live_processes().lock().unwrap().remove(exec_id);
            return Ok(snapshot);
        }
        if let Some(signal) = cancel_flag
            && signal.is_requested()
        {
            return Ok(json!({
                "interrupted": true,
                "reason": "agent_turn_interrupted",
                "process": snapshot
            }));
        }
        if std::time::Instant::now() >= deadline {
            if on_timeout == ExecTimeoutAction::Kill {
                let metadata = read_process_metadata(state_dir, exec_id)
                    .map_err(|_| process_missing_error(exec_id))?;
                kill_exec_processes(&metadata, Some(&snapshot));
                let _ = record_exit_code(Path::new(&metadata.worker_exit_code_path), -9);
                let mut extra = Map::new();
                extra.insert("killed".to_string(), Value::Bool(true));
                extra.insert("wait_timed_out".to_string(), Value::Bool(true));
                extra.insert(
                    "on_timeout".to_string(),
                    Value::String(on_timeout.as_str().to_string()),
                );
                extra.insert("stdin_closed".to_string(), Value::Bool(true));
                extra.insert("failed".to_string(), Value::Bool(false));
                extra.insert("error".to_string(), Value::Null);
                write_exec_snapshot(&metadata, false, true, Value::from(-9), Some(extra))?;
                live_processes().lock().unwrap().remove(exec_id);
                return read_process_snapshot(
                    state_dir,
                    exec_id,
                    start,
                    limit,
                    max_output_chars,
                    workspace_root,
                );
            }

            let mut object = snapshot
                .as_object()
                .cloned()
                .ok_or_else(|| anyhow!("exec snapshot must be a JSON object"))?;
            object.insert("wait_timed_out".to_string(), Value::Bool(true));
            object.insert(
                "on_timeout".to_string(),
                Value::String(on_timeout.as_str().to_string()),
            );
            object.insert("running".to_string(), Value::Bool(true));
            object.insert("completed".to_string(), Value::Bool(false));
            object.insert("returncode".to_string(), Value::Null);
            return Ok(Value::Object(object));
        }
        if let Some(cancel_receiver) = &cancel_receiver {
            crossbeam_channel::select! {
                recv(cancel_receiver) -> _ => {
                    return Ok(json!({
                        "interrupted": true,
                        "reason": "agent_turn_interrupted",
                        "process": snapshot
                    }));
                }
                recv(crossbeam_channel::after(Duration::from_millis(200))) -> _ => {}
            }
        } else {
            thread::sleep(Duration::from_millis(200));
        }
    }
}

fn exec_start_tool(
    workspace_root: PathBuf,
    runtime_state_root: PathBuf,
    cancel_flag: Option<Arc<InterruptSignal>>,
) -> Tool {
    Tool::new_interruptible(
        "exec_start",
        "Start a shell command or executable. By default this waits up to wait_timeout_seconds for completion and returns the final status. Set return_immediate=true for long-running, server, daemon, watch, or interactive commands. If the default wait times out, on_timeout=continue leaves the process running while on_timeout=kill terminates it. Output returned to the model is capped by max_output_chars, which must be 0..1000; complete stdout/stderr are saved at the returned workspace-relative paths. Optional remote=\"<host>|local\" runs this single command over SSH when set to an actual host alias; omit remote or set remote=\"\" for local commands.",
        json!({
            "type": "object",
            "properties": {
                "command": {"type": "string"},
                "cwd": {
                    "type": "string",
                    "description": "Working directory for the command. Prefer relative paths for normal workspace work. When remote is set to an SSH host, cwd is resolved as a directory on that target host."
                },
                "tty": {"type": "boolean"},
                "include_stdout": {"type": "boolean"},
                "start": {"type": "integer"},
                "limit": {"type": "integer"},
                "return_immediate": {"type": "boolean"},
                "wait_timeout_seconds": {"type": "number"},
                "on_timeout": {"type": "string", "enum": ["continue", "kill", "CONTINUE", "KILL"]},
                "max_output_chars": {"type": "integer", "minimum": 0, "maximum": 1000},
                "remote": remote_schema_property()
            },
            "required": ["command"],
            "additionalProperties": false
        }),
        move |arguments| {
            let arguments = arguments
                .as_object()
                .ok_or_else(|| anyhow!("tool arguments must be an object"))?;
            let target = execution_target_arg(arguments)?;
            let command = string_arg(arguments, "command")?;
            let include_stdout = arguments
                .get("include_stdout")
                .and_then(Value::as_bool)
                .unwrap_or(true);
            let tty = arguments
                .get("tty")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            let return_immediate = arguments
                .get("return_immediate")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            let wait_timeout_seconds = arguments
                .get("wait_timeout_seconds")
                .map(|_| f64_arg(arguments, "wait_timeout_seconds"))
                .transpose()?
                .unwrap_or(EXEC_START_DEFAULT_WAIT_TIMEOUT_SECONDS);
            let on_timeout =
                exec_timeout_action_arg(arguments, "on_timeout", ExecTimeoutAction::Continue)?;
            let max_output_chars = max_output_chars_arg(arguments)?;
            let start = usize_arg_with_default(arguments, "start", 0)?;
            let limit = usize_arg_with_default(arguments, "limit", 20)?;
            let cwd = arguments
                .get("cwd")
                .and_then(Value::as_str)
                .map(|value| resolve_path(value, &workspace_root))
                .unwrap_or_else(|| workspace_root.clone());
            let state_dir = ensure_process_state_dir(&runtime_state_root)?;
            let metadata = spawn_managed_process(
                &runtime_state_root,
                &state_dir,
                &workspace_root,
                &command,
                &cwd,
                tty,
                &target,
            )?;
            let mut result = if return_immediate {
                read_process_snapshot(
                    &state_dir,
                    &metadata.exec_id,
                    start,
                    limit,
                    max_output_chars,
                    Some(&workspace_root),
                )?
            } else {
                wait_for_managed_process(
                    &state_dir,
                    &metadata.exec_id,
                    wait_timeout_seconds,
                    None,
                    start,
                    limit,
                    max_output_chars,
                    Some(&workspace_root),
                    on_timeout,
                    cancel_flag.as_ref(),
                )?
            };
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
    workspace_root: PathBuf,
    runtime_state_root: PathBuf,
    _cancel_flag: Option<Arc<InterruptSignal>>,
) -> Tool {
    Tool::new(
        "exec_observe",
        "Observe the latest output of a previously started exec process by exec_id. start=0 and limit=2 means the last two lines. Output returned to the model is capped by max_output_chars, which must be 0..1000; complete stdout/stderr are saved at the returned workspace-relative paths. Remote host is inferred from exec_id.",
        json!({
            "type": "object",
            "properties": {
                "exec_id": {"type": "string"},
                "start": {"type": "integer"},
                "limit": {"type": "integer"},
                "max_output_chars": {"type": "integer", "minimum": 0, "maximum": 1000}
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
            let max_output_chars = max_output_chars_arg(arguments)?;
            let state_dir = ensure_process_state_dir(&runtime_state_root)?;
            read_process_snapshot(
                &state_dir,
                &exec_id,
                start,
                limit,
                max_output_chars,
                Some(&workspace_root),
            )
        },
    )
}

fn exec_wait_tool(
    workspace_root: PathBuf,
    runtime_state_root: PathBuf,
    cancel_flag: Option<Arc<InterruptSignal>>,
) -> Tool {
    Tool::new_interruptible(
        "exec_wait",
        "Wait on a previously started exec process by exec_id. Optionally write input to stdin before waiting. If interrupted by a newer user message or timeout observation, return immediately and leave the process running. If the process does not finish before wait_timeout_seconds, on_timeout=continue leaves it running while on_timeout=kill terminates it. Output returned to the model is capped by max_output_chars, which must be 0..1000; complete stdout/stderr are saved at the returned workspace-relative paths. Remote host is inferred from exec_id.",
        json!({
            "type": "object",
            "properties": {
                "exec_id": {"type": "string"},
                "wait_timeout_seconds": {"type": "number"},
                "input": {"type": "string"},
                "include_stdout": {"type": "boolean"},
                "start": {"type": "integer"},
                "limit": {"type": "integer"},
                "on_timeout": {"type": "string", "enum": ["continue", "kill", "CONTINUE", "KILL"]},
                "max_output_chars": {"type": "integer", "minimum": 0, "maximum": 1000}
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
            let on_timeout =
                exec_timeout_action_arg(arguments, "on_timeout", ExecTimeoutAction::Continue)?;
            let max_output_chars = max_output_chars_arg(arguments)?;
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
                max_output_chars,
                Some(&workspace_root),
                on_timeout,
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

fn exec_kill_tool(runtime_state_root: PathBuf, _cancel_flag: Option<Arc<InterruptSignal>>) -> Tool {
    Tool::new(
        "exec_kill",
        "Immediately stop a previously started exec process by exec_id. Remote host is inferred from exec_id.",
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
            let state_dir = ensure_process_state_dir(&runtime_state_root)?;
            let metadata = read_process_metadata(&state_dir, &exec_id)
                .map_err(|_| process_missing_error(&exec_id))?;
            let previous =
                read_process_snapshot(&state_dir, &exec_id, 0, 0, EXEC_OUTPUT_MAX_CHARS, None).ok();
            kill_exec_processes(&metadata, previous.as_ref());
            let _ = record_exit_code(Path::new(&metadata.worker_exit_code_path), -9);
            let mut extra = Map::new();
            extra.insert("killed".to_string(), Value::Bool(true));
            extra.insert("failed".to_string(), Value::Bool(false));
            extra.insert("stdin_closed".to_string(), Value::Bool(true));
            extra.insert("error".to_string(), Value::Null);
            write_exec_snapshot(&metadata, false, true, Value::from(-9), Some(extra))?;
            live_processes().lock().unwrap().remove(&exec_id);
            Ok(json!({
                "exec_id": metadata.exec_id,
                "pid": previous
                    .as_ref()
                    .and_then(exec_pid_from_snapshot)
                    .map(Value::from)
                    .unwrap_or(Value::Null),
                "command": metadata.command,
                "remote": metadata.remote,
                "cwd": metadata.cwd,
                "running": false,
                "completed": true,
                "killed": true,
                "returncode": -9,
            }))
        },
    )
}

fn apply_patch_tool(workspace_root: PathBuf, _cancel_flag: Option<Arc<InterruptSignal>>) -> Tool {
    Tool::new(
        "apply_patch",
        "Apply a unified diff patch inside the workspace using git apply. The patch must be a valid unified diff. Optional remote=\"<host>|local\" applies this single patch over SSH when set to an actual host alias; omit remote or set remote=\"\" for local patches.",
        json!({
            "type": "object",
            "properties": {
                "patch": {"type": "string"},
                "strip": {"type": "integer"},
                "reverse": {"type": "boolean"},
                "check": {"type": "boolean"},
                "remote": remote_schema_property()
            },
            "required": ["patch"],
            "additionalProperties": false
        }),
        move |arguments| {
            let arguments = arguments
                .as_object()
                .ok_or_else(|| anyhow!("tool arguments must be an object"))?;
            let patch = string_arg(arguments, "patch")?;
            let strip = usize_arg_with_default(arguments, "strip", 0)?;
            let reverse = arguments
                .get("reverse")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            let check = arguments
                .get("check")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            if let ExecutionTarget::RemoteSsh { host } = execution_target_arg(arguments)? {
                return run_remote_apply_patch(
                    &host,
                    &workspace_root,
                    &patch,
                    strip,
                    reverse,
                    check,
                );
            }
            let patch_workspace_root = workspace_root.clone();

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
            let output = child
                .wait_with_output()
                .context("failed to wait for git apply")?;
            Ok(json!({
                "applied": output.status.success(),
                "returncode": output.status.code().unwrap_or(-1),
                "stdout": String::from_utf8_lossy(&output.stdout),
                "stderr": String::from_utf8_lossy(&output.stderr)
            }))
        },
    )
}

#[derive(Clone, Serialize, serde::Deserialize)]
struct BackgroundTaskMetadata {
    task_id: String,
    pid: u32,
    label: String,
    status_path: String,
    stdout_path: String,
    stderr_path: String,
    exit_code_path: String,
}

#[derive(Clone, Debug, Default)]
pub struct RuntimeTaskCleanupReport {
    pub exec_processes_killed: usize,
    pub file_downloads_cancelled: usize,
    pub image_tasks_cancelled: usize,
}

fn tool_worker_state_dir(runtime_state_root: &Path) -> Result<PathBuf> {
    let path = runtime_state_root.join("agent_frame").join("tool_workers");
    fs::create_dir_all(&path).with_context(|| format!("failed to create {}", path.display()))?;
    Ok(path)
}

fn background_task_dir(runtime_state_root: &Path, kind: &str) -> Result<PathBuf> {
    let path = runtime_state_root.join("agent_frame").join(kind);
    fs::create_dir_all(&path).with_context(|| format!("failed to create {}", path.display()))?;
    Ok(path)
}

fn background_task_dir_if_exists(runtime_state_root: &Path, kind: &str) -> Option<PathBuf> {
    let path = runtime_state_root.join("agent_frame").join(kind);
    path.exists().then_some(path)
}

fn background_task_meta_path(dir: &Path, task_id: &str) -> PathBuf {
    dir.join(format!("{}.json", task_id))
}

fn write_background_task_metadata(dir: &Path, metadata: &BackgroundTaskMetadata) -> Result<()> {
    fs::write(
        background_task_meta_path(dir, &metadata.task_id),
        serde_json::to_vec_pretty(metadata).context("failed to serialize background task")?,
    )
    .with_context(|| format!("failed to write metadata for {}", metadata.task_id))
}

fn read_background_task_metadata(dir: &Path, task_id: &str) -> Result<BackgroundTaskMetadata> {
    let path = background_task_meta_path(dir, task_id);
    let raw =
        fs::read_to_string(&path).with_context(|| format!("failed to read {}", path.display()))?;
    serde_json::from_str(&raw).context("failed to parse background task metadata")
}

fn background_task_is_running(metadata: &BackgroundTaskMetadata) -> bool {
    read_exit_code(Path::new(&metadata.exit_code_path)).is_none()
        && process_is_running(metadata.pid)
}

fn resolve_tool_worker_executable() -> Result<PathBuf> {
    if let Ok(path) = std::env::var("AGENT_TOOL_WORKER_BIN") {
        let candidate = PathBuf::from(path);
        if candidate.exists() {
            return Ok(candidate);
        }
    }
    if let Ok(path) = std::env::var("CARGO_BIN_EXE_run_agent") {
        let candidate = PathBuf::from(path);
        if candidate.exists() {
            return Ok(candidate);
        }
    }
    if let Ok(path) = std::env::var("CARGO_BIN_EXE_partyclaw") {
        let candidate = PathBuf::from(path);
        if candidate.exists() {
            return Ok(candidate);
        }
    }
    let current = std::env::current_exe().context("failed to resolve current executable")?;
    if current
        .file_stem()
        .and_then(|value| value.to_str())
        .is_some_and(|name| matches!(name, "partyclaw" | "agent_host" | "run_agent"))
    {
        return Ok(current);
    }
    let mut candidates = Vec::new();
    if let Some(parent) = current.parent() {
        candidates.push(parent.join("run_agent"));
        candidates.push(parent.join("partyclaw"));
        candidates.push(parent.join("agent_host"));
        if parent.file_name().and_then(|value| value.to_str()) == Some("deps")
            && let Some(grandparent) = parent.parent()
        {
            candidates.push(grandparent.join("run_agent"));
            candidates.push(grandparent.join("partyclaw"));
            candidates.push(grandparent.join("agent_host"));
        }
    }
    candidates
        .into_iter()
        .find(|candidate| candidate.exists())
        .ok_or_else(|| {
            anyhow!("failed to locate tool worker executable; set AGENT_TOOL_WORKER_BIN")
        })
}

fn write_tool_worker_job_file(runtime_state_root: &Path, job: &ToolWorkerJob) -> Result<PathBuf> {
    let dir = tool_worker_state_dir(runtime_state_root)?;
    let path = dir.join(format!("job-{}.json", Uuid::new_v4()));
    fs::write(
        &path,
        serde_json::to_vec_pretty(job).context("failed to serialize tool worker job")?,
    )
    .with_context(|| format!("failed to write {}", path.display()))?;
    Ok(path)
}

fn run_interruptible_worker_job(
    runtime_state_root: &Path,
    job: &ToolWorkerJob,
    timeout_seconds: f64,
    cancel_flag: Option<&Arc<InterruptSignal>>,
) -> Result<Value> {
    let job_file = write_tool_worker_job_file(runtime_state_root, job)?;
    let worker_executable = resolve_tool_worker_executable()?;
    let child = Command::new(worker_executable)
        .arg("run-tool-worker")
        .arg("--job-file")
        .arg(&job_file)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("failed to spawn tool worker")?;
    let pid = child.id();
    let (sender, receiver) = crossbeam_channel::bounded(1);
    thread::spawn(move || {
        let _ = sender.send(child.wait_with_output());
    });
    let cancel_receiver = cancel_flag.map(|signal| signal.subscribe());
    let timeout_receiver = crossbeam_channel::after(Duration::from_secs_f64(timeout_seconds));
    let output = match cancel_receiver {
        Some(cancel_receiver) => crossbeam_channel::select! {
            recv(receiver) -> result => Some(result),
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
            recv(receiver) -> result => Some(result),
            recv(timeout_receiver) -> _ => {
                terminate_process_pid(pid);
                None
            }
        },
    };
    let _ = fs::remove_file(&job_file);
    let Some(output) = output else {
        let _ = receiver.recv_timeout(Duration::from_secs(5));
        if cancel_flag.is_some_and(|signal| signal.is_requested()) {
            return Err(anyhow!("operation cancelled"));
        }
        return Err(anyhow!(
            "operation timed out after {} seconds",
            timeout_seconds
        ));
    };
    let output = output
        .context("tool worker completion channel disconnected")?
        .context("failed to wait for tool worker process")?;
    if !output.status.success() {
        return Err(anyhow!(
            "tool worker failed with {}: {}",
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    serde_json::from_slice(&output.stdout).context("failed to parse tool worker output")
}

fn spawn_background_worker_process(
    runtime_state_root: &Path,
    label: &str,
    task_id: &str,
    job: &ToolWorkerJob,
) -> Result<BackgroundTaskMetadata> {
    let worker_dir = tool_worker_state_dir(runtime_state_root)?;
    let job_file = worker_dir.join(format!("{}-{}.job.json", label, task_id));
    fs::write(
        &job_file,
        serde_json::to_vec_pretty(job).context("failed to serialize background worker job")?,
    )
    .with_context(|| format!("failed to write {}", job_file.display()))?;
    let stdout_path = worker_dir.join(format!("{}-{}.stdout", label, task_id));
    let stderr_path = worker_dir.join(format!("{}-{}.stderr", label, task_id));
    let exit_code_path = worker_dir.join(format!("{}-{}.exit", label, task_id));
    let stdout_file = fs::File::create(&stdout_path)
        .with_context(|| format!("failed to create {}", stdout_path.display()))?;
    let stderr_file = fs::File::create(&stderr_path)
        .with_context(|| format!("failed to create {}", stderr_path.display()))?;
    let worker_executable = resolve_tool_worker_executable()?;
    let child_pid = {
        #[cfg(windows)]
        {
            let mut child = Command::new(&worker_executable)
                .arg("run-tool-worker")
                .arg("--job-file")
                .arg(&job_file)
                .current_dir(runtime_state_root)
                .stdin(Stdio::null())
                .stdout(Stdio::from(stdout_file))
                .stderr(Stdio::from(stderr_file))
                .spawn()
                .context("failed to spawn background tool worker")?;
            let child_pid = child.id();
            let exit_code_path = exit_code_path.clone();
            thread::spawn(move || {
                let code = child
                    .wait()
                    .ok()
                    .and_then(|status| status.code())
                    .unwrap_or(-1);
                let _ = record_exit_code(&exit_code_path, code);
            });
            child_pid
        }
        #[cfg(not(windows))]
        {
            let child = Command::new("sh")
                .arg("-c")
                .arg("\"$@\"; code=$?; printf '%s' \"$code\" > \"$AGENT_FRAME_EXIT_CODE_PATH\"; exit $code")
                .arg("sh")
                .arg(&worker_executable)
                .arg("run-tool-worker")
                .arg("--job-file")
                .arg(&job_file)
                .current_dir(runtime_state_root)
                .env("AGENT_FRAME_EXIT_CODE_PATH", &exit_code_path)
                .stdin(Stdio::null())
                .stdout(Stdio::from(stdout_file))
                .stderr(Stdio::from(stderr_file))
                .spawn()
                .context("failed to spawn background tool worker")?;
            child.id()
        }
    };
    Ok(BackgroundTaskMetadata {
        task_id: task_id.to_string(),
        pid: child_pid,
        label: label.to_string(),
        status_path: match job {
            ToolWorkerJob::Image { status_path, .. } => status_path.clone(),
            ToolWorkerJob::FileDownload { status_path, .. } => status_path.clone(),
            _ => String::new(),
        },
        stdout_path: stdout_path.display().to_string(),
        stderr_path: stderr_path.display().to_string(),
        exit_code_path: exit_code_path.display().to_string(),
    })
}

fn read_status_json(path: &Path) -> Result<Value> {
    let raw =
        fs::read_to_string(path).with_context(|| format!("failed to read {}", path.display()))?;
    serde_json::from_str(&raw).context("failed to parse status json")
}

fn read_image_task_snapshot(runtime_state_root: &Path, image_id: &str) -> Result<Value> {
    let metadata = read_background_task_metadata(
        &background_task_dir(runtime_state_root, "image_tasks")?,
        image_id,
    )?;
    let mut snapshot = read_status_json(Path::new(&metadata.status_path))?;
    if snapshot
        .get("running")
        .and_then(Value::as_bool)
        .unwrap_or(false)
        && (read_exit_code(Path::new(&metadata.exit_code_path)).is_some()
            || !process_is_running(metadata.pid))
    {
        snapshot = json!({
            "image_id": image_id,
            "path": snapshot["path"].clone(),
            "question": snapshot["question"].clone(),
            "running": false,
            "completed": false,
            "cancelled": false,
            "failed": true,
            "error": "image worker exited unexpectedly",
        });
    }
    Ok(snapshot)
}

fn read_file_download_snapshot(runtime_state_root: &Path, download_id: &str) -> Result<Value> {
    let metadata = read_background_task_metadata(
        &background_task_dir(runtime_state_root, "file_downloads")?,
        download_id,
    )?;
    let mut snapshot = read_status_json(Path::new(&metadata.status_path))?;
    if snapshot
        .get("running")
        .and_then(Value::as_bool)
        .unwrap_or(false)
        && (read_exit_code(Path::new(&metadata.exit_code_path)).is_some()
            || !process_is_running(metadata.pid))
    {
        snapshot = json!({
            "download_id": download_id,
            "url": snapshot["url"].clone(),
            "path": snapshot["path"].clone(),
            "running": false,
            "completed": false,
            "cancelled": false,
            "failed": true,
            "error": "file download worker exited unexpectedly",
        });
    }
    Ok(snapshot)
}

fn list_active_file_download_summaries(runtime_state_root: &Path) -> Result<Vec<String>> {
    let Some(task_dir) = background_task_dir_if_exists(runtime_state_root, "file_downloads") else {
        return Ok(Vec::new());
    };
    let mut entries = Vec::new();
    for entry in
        fs::read_dir(&task_dir).with_context(|| format!("failed to read {}", task_dir.display()))?
    {
        let entry = entry?;
        let path = entry.path();
        let Some(file_name) = path.file_name().and_then(|value| value.to_str()) else {
            continue;
        };
        if !file_name.ends_with(".json") || file_name.ends_with(".status.json") {
            continue;
        }
        let Some(download_id) = path.file_stem().and_then(|value| value.to_str()) else {
            continue;
        };
        let snapshot = read_file_download_snapshot(runtime_state_root, download_id)?;
        if !snapshot
            .get("running")
            .and_then(Value::as_bool)
            .unwrap_or(false)
        {
            continue;
        }
        let url = snapshot
            .get("url")
            .and_then(Value::as_str)
            .unwrap_or("<unknown>");
        let target_path = snapshot
            .get("path")
            .and_then(Value::as_str)
            .unwrap_or("<unknown>");
        let bytes_downloaded = snapshot
            .get("bytes_downloaded")
            .and_then(Value::as_u64)
            .unwrap_or(0);
        entries.push(format!(
            "- download_id=`{}` path=`{}` bytes_downloaded={} url=`{}`",
            download_id, target_path, bytes_downloaded, url
        ));
    }
    entries.sort();
    Ok(entries)
}

pub(crate) fn active_runtime_state_summary(runtime_state_root: &Path) -> Result<Option<String>> {
    let active_execs = list_active_exec_summaries(runtime_state_root)?;
    let active_downloads = list_active_file_download_summaries(runtime_state_root)?;
    let active_subagents = list_active_subagent_summaries(runtime_state_root)?;
    if active_execs.is_empty() && active_downloads.is_empty() && active_subagents.is_empty() {
        return Ok(None);
    }
    let mut sections = vec![
        "[Active Runtime Tasks]".to_string(),
        "These tasks are still in progress across turns. Reuse their ids with observe/wait/cancel tools instead of starting duplicates.".to_string(),
    ];
    if !active_execs.is_empty() {
        sections.push("Active exec processes:".to_string());
        sections.extend(active_execs);
    }
    if !active_downloads.is_empty() {
        sections.push("Active file downloads:".to_string());
        sections.extend(active_downloads);
    }
    if !active_subagents.is_empty() {
        sections.push("Active subagents:".to_string());
        sections.extend(active_subagents);
    }
    Ok(Some(sections.join("\n")))
}

fn list_active_subagent_summaries(runtime_state_root: &Path) -> Result<Vec<String>> {
    let Some(dir) = background_task_dir_if_exists(runtime_state_root, "subagents") else {
        return Ok(Vec::new());
    };
    let mut entries = Vec::new();
    for path in iter_metadata_json_files(&dir)? {
        let raw = fs::read_to_string(&path)
            .with_context(|| format!("failed to read {}", path.display()))?;
        let value: Value =
            serde_json::from_str(&raw).context("failed to parse subagent state json")?;
        let state = value
            .get("state")
            .and_then(Value::as_str)
            .unwrap_or("unknown");
        if !matches!(state, "running" | "waiting_for_charge" | "ready") {
            continue;
        }
        let id = value
            .get("id")
            .or_else(|| value.get("agent_id"))
            .and_then(Value::as_str)
            .unwrap_or("unknown");
        let description = value
            .get("description")
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim();
        let model = value
            .get("model_key")
            .or_else(|| value.get("model"))
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim();
        let mut line = format!("- id={} state={}", id, state);
        if !description.is_empty() {
            line.push_str(&format!(" description={:?}", description));
        }
        if !model.is_empty() {
            line.push_str(&format!(" model={}", model));
        }
        entries.push(line);
    }
    entries.sort();
    Ok(entries)
}

fn iter_metadata_json_files(dir: &Path) -> Result<Vec<PathBuf>> {
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

fn cleanup_exec_processes(runtime_state_root: &Path) -> Result<usize> {
    let Some(state_dir) = process_state_dir_if_exists(runtime_state_root) else {
        return Ok(0);
    };
    let mut killed = 0usize;
    for path in iter_metadata_json_files(&state_dir)? {
        let raw = fs::read_to_string(&path)
            .with_context(|| format!("failed to read {}", path.display()))?;
        let (metadata, allow_missing_snapshot) = match parse_process_metadata_record(&raw)? {
            ProcessMetadataRecord::Current(metadata) => (metadata, false),
            ProcessMetadataRecord::Legacy(metadata) => (
                legacy_process_metadata_to_current(&state_dir, metadata),
                true,
            ),
        };
        let snapshot = match read_process_snapshot(
            &state_dir,
            &metadata.exec_id,
            0,
            0,
            EXEC_OUTPUT_MAX_CHARS,
            None,
        ) {
            Ok(snapshot) => Some(snapshot),
            Err(_) if allow_missing_snapshot => None,
            Err(err) => return Err(err),
        };
        let running = snapshot
            .as_ref()
            .and_then(|value| value.get("running"))
            .and_then(Value::as_bool)
            .unwrap_or(false);
        live_processes().lock().unwrap().remove(&metadata.exec_id);
        if running
            || (read_exit_code(Path::new(&metadata.worker_exit_code_path)).is_none()
                && process_is_running(metadata.worker_pid))
        {
            kill_exec_processes(&metadata, snapshot.as_ref());
            let _ = record_exit_code(Path::new(&metadata.worker_exit_code_path), -9);
            let mut extra = Map::new();
            extra.insert("cancelled".to_string(), Value::Bool(true));
            extra.insert(
                "reason".to_string(),
                Value::String("session_destroyed".to_string()),
            );
            extra.insert("stdin_closed".to_string(), Value::Bool(true));
            extra.insert("failed".to_string(), Value::Bool(false));
            extra.insert("error".to_string(), Value::Null);
            let _ = write_exec_snapshot(&metadata, false, false, Value::from(-9), Some(extra));
            killed = killed.saturating_add(1);
        }
    }
    Ok(killed)
}

fn cleanup_image_tasks(runtime_state_root: &Path) -> Result<usize> {
    let Some(task_dir) = background_task_dir_if_exists(runtime_state_root, "image_tasks") else {
        return Ok(0);
    };
    let mut cancelled = 0usize;
    for path in iter_metadata_json_files(&task_dir)? {
        let raw = fs::read_to_string(&path)
            .with_context(|| format!("failed to read {}", path.display()))?;
        let metadata: BackgroundTaskMetadata =
            serde_json::from_str(&raw).context("failed to parse background task metadata")?;
        if !background_task_is_running(&metadata) {
            continue;
        }
        let previous = read_status_json(Path::new(&metadata.status_path)).ok();
        terminate_process_pid(metadata.pid);
        let _ = record_exit_code(Path::new(&metadata.exit_code_path), -9);
        let snapshot = json!({
            "image_id": metadata.task_id,
            "path": previous.as_ref().and_then(|value| value.get("path")).cloned().unwrap_or(Value::String(String::new())),
            "question": previous.as_ref().and_then(|value| value.get("question")).cloned().unwrap_or(Value::String(String::new())),
            "running": false,
            "completed": false,
            "cancelled": true,
            "failed": false,
            "reason": "session_destroyed",
        });
        fs::write(
            Path::new(&metadata.status_path),
            serde_json::to_vec_pretty(&snapshot)
                .context("failed to serialize image cleanup snapshot")?,
        )
        .with_context(|| format!("failed to write {}", metadata.status_path))?;
        cancelled = cancelled.saturating_add(1);
    }
    Ok(cancelled)
}

fn cleanup_file_downloads(runtime_state_root: &Path) -> Result<usize> {
    let Some(task_dir) = background_task_dir_if_exists(runtime_state_root, "file_downloads") else {
        return Ok(0);
    };
    let mut cancelled = 0usize;
    for path in iter_metadata_json_files(&task_dir)? {
        let raw = fs::read_to_string(&path)
            .with_context(|| format!("failed to read {}", path.display()))?;
        let metadata: BackgroundTaskMetadata =
            serde_json::from_str(&raw).context("failed to parse background task metadata")?;
        if !background_task_is_running(&metadata) {
            continue;
        }
        let previous = read_status_json(Path::new(&metadata.status_path)).ok();
        terminate_process_pid(metadata.pid);
        let _ = record_exit_code(Path::new(&metadata.exit_code_path), -9);
        let snapshot = json!({
            "download_id": metadata.task_id,
            "url": previous.as_ref().and_then(|value| value.get("url")).cloned().unwrap_or(Value::String(String::new())),
            "path": previous.as_ref().and_then(|value| value.get("path")).cloned().unwrap_or(Value::String(String::new())),
            "running": false,
            "completed": false,
            "cancelled": true,
            "failed": false,
            "bytes_downloaded": previous.as_ref().and_then(|value| value.get("bytes_downloaded")).cloned().unwrap_or(Value::from(0_u64)),
            "total_bytes": previous.as_ref().and_then(|value| value.get("total_bytes")).cloned().unwrap_or(Value::Null),
            "http_status": previous.as_ref().and_then(|value| value.get("http_status")).cloned().unwrap_or(Value::Null),
            "final_url": previous.as_ref().and_then(|value| value.get("final_url")).cloned().unwrap_or(Value::Null),
            "content_type": previous.as_ref().and_then(|value| value.get("content_type")).cloned().unwrap_or(Value::Null),
            "reason": "session_destroyed",
        });
        fs::write(
            Path::new(&metadata.status_path),
            serde_json::to_vec_pretty(&snapshot)
                .context("failed to serialize file download cleanup snapshot")?,
        )
        .with_context(|| format!("failed to write {}", metadata.status_path))?;
        cancelled = cancelled.saturating_add(1);
    }
    Ok(cancelled)
}

pub fn terminate_runtime_state_tasks(
    runtime_state_root: &Path,
) -> Result<RuntimeTaskCleanupReport> {
    Ok(RuntimeTaskCleanupReport {
        exec_processes_killed: cleanup_exec_processes(runtime_state_root)?,
        file_downloads_cancelled: cleanup_file_downloads(runtime_state_root)?,
        image_tasks_cancelled: cleanup_image_tasks(runtime_state_root)?,
    })
}

fn download_temp_path(path: &Path, download_id: &str) -> PathBuf {
    let file_name = path
        .file_name()
        .and_then(|value| value.to_str())
        .unwrap_or("download");
    path.with_file_name(format!(".{}.{}.part", file_name, download_id))
}

fn image_start_tool(
    workspace_root: PathBuf,
    runtime_state_root: PathBuf,
    upstream: UpstreamConfig,
    image_tool_upstream: Option<UpstreamConfig>,
    _cancel_flag: Option<Arc<InterruptSignal>>,
) -> Tool {
    Tool::new(
        "image_start",
        "Start inspecting a local image file with the model's multimodal capability and return immediately with an image_id.",
        json!({
            "type": "object",
            "properties": {
                "path": {"type": "string"},
                "question": {"type": "string"}
            },
            "required": ["path", "question"],
            "additionalProperties": false
        }),
        move |arguments| {
            let arguments = arguments
                .as_object()
                .ok_or_else(|| anyhow!("tool arguments must be an object"))?;
            let path = resolve_path(&string_arg(arguments, "path")?, &workspace_root);
            let question = string_arg(arguments, "question")?;
            let upstream = image_tool_upstream
                .clone()
                .unwrap_or_else(|| upstream.clone());
            let image_id = Uuid::new_v4().to_string();
            let task_dir = background_task_dir(&runtime_state_root, "image_tasks")?;
            let status_path = task_dir.join(format!("{}.status.json", image_id));
            let initial = json!({
                "image_id": image_id,
                "path": path.display().to_string(),
                "question": question,
                "running": true,
                "completed": false,
                "cancelled": false,
                "failed": false,
            });
            fs::write(
                &status_path,
                serde_json::to_vec_pretty(&initial).context("failed to serialize image status")?,
            )
            .with_context(|| format!("failed to write {}", status_path.display()))?;
            let job = ToolWorkerJob::Image {
                image_id: image_id.clone(),
                path: path.display().to_string(),
                question: question.to_string(),
                upstream,
                status_path: status_path.display().to_string(),
            };
            let metadata =
                spawn_background_worker_process(&runtime_state_root, "image", &image_id, &job)?;
            write_background_task_metadata(&task_dir, &metadata)?;
            read_image_task_snapshot(&runtime_state_root, &image_id)
        },
    )
}

fn image_load_tool(
    workspace_root: PathBuf,
    upstream: UpstreamConfig,
    _cancel_flag: Option<Arc<InterruptSignal>>,
) -> Tool {
    Tool::new(
        "image_load",
        "Load a local image file into the next model request for direct multimodal inspection by the current model. Returns immediately. Do not call image_load more than 3 times in the same assistant tool-call batch; excess image_load calls in that batch will fail. Load more images after inspecting the first batch.",
        json!({
            "type": "object",
            "properties": {
                "path": {"type": "string"}
            },
            "required": ["path"],
            "additionalProperties": false
        }),
        move |arguments| {
            let arguments = arguments
                .as_object()
                .ok_or_else(|| anyhow!("tool arguments must be an object"))?;
            if !upstream.native_image_input {
                return Err(anyhow!(
                    "image_load requires a model with native image input support"
                ));
            }
            let path = resolve_path(&string_arg(arguments, "path")?, &workspace_root);
            Ok(json!({
                "loaded": true,
                "kind": "synthetic_user_multimodal",
                "media": [{
                    "type": "input_image",
                    "path": path.display().to_string(),
                }],
                "path": path.display().to_string(),
            }))
        },
    )
}

fn pdf_load_tool(
    workspace_root: PathBuf,
    upstream: UpstreamConfig,
    _cancel_flag: Option<Arc<InterruptSignal>>,
) -> Tool {
    Tool::new(
        "pdf_load",
        "Load a local PDF file into the next model request for direct inspection by the current model. Returns immediately.",
        json!({
            "type": "object",
            "properties": {
                "path": {"type": "string"}
            },
            "required": ["path"],
            "additionalProperties": false
        }),
        move |arguments| {
            let arguments = arguments
                .as_object()
                .ok_or_else(|| anyhow!("tool arguments must be an object"))?;
            if !upstream.native_pdf_input {
                return Err(anyhow!(
                    "pdf_load requires a model with native PDF input support"
                ));
            }
            let path = resolve_path(&string_arg(arguments, "path")?, &workspace_root);
            Ok(json!({
                "loaded": true,
                "kind": "synthetic_user_multimodal",
                "media": [{
                    "type": "input_file",
                    "path": path.display().to_string(),
                    "filename": path.file_name().and_then(|value| value.to_str()).unwrap_or("document.pdf"),
                }],
                "path": path.display().to_string(),
            }))
        },
    )
}

fn audio_load_tool(
    workspace_root: PathBuf,
    upstream: UpstreamConfig,
    _cancel_flag: Option<Arc<InterruptSignal>>,
) -> Tool {
    Tool::new(
        "audio_load",
        "Load a local audio file into the next model request for direct inspection by the current model. Returns immediately.",
        json!({
            "type": "object",
            "properties": {
                "path": {"type": "string"}
            },
            "required": ["path"],
            "additionalProperties": false
        }),
        move |arguments| {
            let arguments = arguments
                .as_object()
                .ok_or_else(|| anyhow!("tool arguments must be an object"))?;
            if !upstream.native_audio_input {
                return Err(anyhow!(
                    "audio_load requires a model with native audio input support"
                ));
            }
            let path = resolve_path(&string_arg(arguments, "path")?, &workspace_root);
            let format = path
                .extension()
                .and_then(|value| value.to_str())
                .unwrap_or_default()
                .to_ascii_lowercase();
            Ok(json!({
                "loaded": true,
                "kind": "synthetic_user_multimodal",
                "media": [{
                    "type": "input_audio",
                    "path": path.display().to_string(),
                    "format": format,
                }],
                "path": path.display().to_string(),
            }))
        },
    )
}

fn maybe_add_images_schema(properties: &mut Map<String, Value>, upstream_supports_vision: bool) {
    if upstream_supports_vision {
        properties.insert(
            "images".to_string(),
            json!({
                "type": "array",
                "items": { "type": "string" }
            }),
        );
    }
}

fn pdf_query_tool(
    workspace_root: PathBuf,
    runtime_state_root: PathBuf,
    upstream: UpstreamConfig,
    cancel_flag: Option<Arc<InterruptSignal>>,
) -> Tool {
    let mut properties = Map::new();
    properties.insert("path".to_string(), json!({"type": "string"}));
    properties.insert("question".to_string(), json!({"type": "string"}));
    maybe_add_images_schema(&mut properties, upstream.supports_vision_input);
    Tool::new_interruptible(
        "pdf_query",
        "Ask a question about a local PDF using a helper model. This can be interrupted and will cancel immediately.",
        Value::Object(Map::from_iter([
            ("type".to_string(), Value::String("object".to_string())),
            ("properties".to_string(), Value::Object(properties)),
            ("required".to_string(), json!(["path", "question"])),
            ("additionalProperties".to_string(), Value::Bool(false)),
        ])),
        move |arguments| {
            let arguments = arguments
                .as_object()
                .ok_or_else(|| anyhow!("tool arguments must be an object"))?;
            let path = resolve_path(&string_arg(arguments, "path")?, &workspace_root);
            let question = string_arg(arguments, "question")?;
            let images = string_array_arg(arguments, "images")?
                .into_iter()
                .map(|path| resolve_path(&path, &workspace_root).display().to_string())
                .collect::<Vec<_>>();
            let result = run_interruptible_worker_job(
                &runtime_state_root,
                &ToolWorkerJob::Pdf {
                    path: path.display().to_string(),
                    question,
                    upstream: upstream.clone(),
                    images,
                },
                upstream.timeout_seconds,
                cancel_flag.as_ref(),
            )?;
            Ok(result)
        },
    )
}

fn audio_transcribe_tool(
    workspace_root: PathBuf,
    runtime_state_root: PathBuf,
    upstream: UpstreamConfig,
    cancel_flag: Option<Arc<InterruptSignal>>,
) -> Tool {
    let mut properties = Map::new();
    properties.insert("path".to_string(), json!({"type": "string"}));
    properties.insert("question".to_string(), json!({"type": "string"}));
    maybe_add_images_schema(&mut properties, upstream.supports_vision_input);
    Tool::new_interruptible(
        "audio_transcribe",
        "Transcribe or inspect a local audio file using a helper model. This can be interrupted and will cancel immediately.",
        Value::Object(Map::from_iter([
            ("type".to_string(), Value::String("object".to_string())),
            ("properties".to_string(), Value::Object(properties)),
            ("required".to_string(), json!(["path"])),
            ("additionalProperties".to_string(), Value::Bool(false)),
        ])),
        move |arguments| {
            let arguments = arguments
                .as_object()
                .ok_or_else(|| anyhow!("tool arguments must be an object"))?;
            let path = resolve_path(&string_arg(arguments, "path")?, &workspace_root);
            let question = string_arg_with_default(
                arguments,
                "question",
                "Transcribe the audio accurately and summarize anything important.",
            )?;
            let images = string_array_arg(arguments, "images")?
                .into_iter()
                .map(|path| resolve_path(&path, &workspace_root).display().to_string())
                .collect::<Vec<_>>();
            let result = run_interruptible_worker_job(
                &runtime_state_root,
                &ToolWorkerJob::Audio {
                    path: path.display().to_string(),
                    question,
                    upstream: upstream.clone(),
                    images,
                },
                upstream.timeout_seconds,
                cancel_flag.as_ref(),
            )?;
            Ok(result)
        },
    )
}

fn image_generate_tool(
    workspace_root: PathBuf,
    runtime_state_root: PathBuf,
    upstream: UpstreamConfig,
    cancel_flag: Option<Arc<InterruptSignal>>,
) -> Tool {
    let mut properties = Map::new();
    properties.insert("prompt".to_string(), json!({"type": "string"}));
    maybe_add_images_schema(&mut properties, upstream.supports_vision_input);
    Tool::new_interruptible(
        "image_generate",
        "Generate a new image with a helper model. Returns a generated file path and attaches the image back into context. This can be interrupted and will cancel immediately.",
        Value::Object(Map::from_iter([
            ("type".to_string(), Value::String("object".to_string())),
            ("properties".to_string(), Value::Object(properties)),
            ("required".to_string(), json!(["prompt"])),
            ("additionalProperties".to_string(), Value::Bool(false)),
        ])),
        move |arguments| {
            let arguments = arguments
                .as_object()
                .ok_or_else(|| anyhow!("tool arguments must be an object"))?;
            let prompt = string_arg(arguments, "prompt")?;
            let images = string_array_arg(arguments, "images")?
                .into_iter()
                .map(|path| resolve_path(&path, &workspace_root).display().to_string())
                .collect::<Vec<_>>();
            let output_dir = workspace_root.join("generated");
            fs::create_dir_all(&output_dir)
                .with_context(|| format!("failed to create {}", output_dir.display()))?;
            let output_path = output_dir.join(format!("generated-{}.png", Uuid::new_v4()));
            let result = run_interruptible_worker_job(
                &runtime_state_root,
                &ToolWorkerJob::ImageGenerate {
                    prompt,
                    upstream: upstream.clone(),
                    output_path: output_path.display().to_string(),
                    images,
                },
                upstream.timeout_seconds,
                cancel_flag.as_ref(),
            )?;
            Ok(result)
        },
    )
}

fn image_wait_tool(runtime_state_root: PathBuf, cancel_flag: Option<Arc<InterruptSignal>>) -> Tool {
    Tool::new_interruptible(
        "image_wait",
        "Wait for a previously started image task by image_id. If interrupted by a newer user message or timeout observation, return immediately without cancelling the image task.",
        json!({
            "type": "object",
            "properties": {
                "image_id": {"type": "string"}
            },
            "required": ["image_id"],
            "additionalProperties": false
        }),
        move |arguments| {
            let arguments = arguments
                .as_object()
                .ok_or_else(|| anyhow!("tool arguments must be an object"))?;
            let image_id = string_arg(arguments, "image_id")?;
            let cancel_receiver = cancel_flag.as_ref().map(|signal| signal.subscribe());
            loop {
                let snapshot = read_image_task_snapshot(&runtime_state_root, &image_id)?;
                let finished = snapshot
                    .get("running")
                    .and_then(Value::as_bool)
                    .is_some_and(|running| !running);
                if finished {
                    return Ok(snapshot);
                }
                if let Some(cancel_receiver) = &cancel_receiver {
                    crossbeam_channel::select! {
                        recv(cancel_receiver) -> _ => {
                            return Ok(json!({
                                "interrupted": true,
                                "image": snapshot,
                            }));
                        }
                        recv(crossbeam_channel::after(Duration::from_millis(200))) -> _ => {}
                    }
                } else {
                    thread::sleep(Duration::from_millis(200));
                }
            }
        },
    )
}

fn image_cancel_tool(
    runtime_state_root: PathBuf,
    _cancel_flag: Option<Arc<InterruptSignal>>,
) -> Tool {
    Tool::new(
        "image_cancel",
        "Cancel a previously started image task by image_id.",
        json!({
            "type": "object",
            "properties": {
                "image_id": {"type": "string"}
            },
            "required": ["image_id"],
            "additionalProperties": false
        }),
        move |arguments| {
            let arguments = arguments
                .as_object()
                .ok_or_else(|| anyhow!("tool arguments must be an object"))?;
            let image_id = string_arg(arguments, "image_id")?;
            let task_dir = background_task_dir(&runtime_state_root, "image_tasks")?;
            let metadata = read_background_task_metadata(&task_dir, &image_id)?;
            terminate_process_pid(metadata.pid);
            let _ = record_exit_code(Path::new(&metadata.exit_code_path), -9);
            let snapshot = json!({
                "image_id": image_id,
                "path": read_image_task_snapshot(&runtime_state_root, &image_id)
                    .ok()
                    .and_then(|value| value.get("path").cloned())
                    .unwrap_or(Value::String(String::new())),
                "question": read_image_task_snapshot(&runtime_state_root, &image_id)
                    .ok()
                    .and_then(|value| value.get("question").cloned())
                    .unwrap_or(Value::String(String::new())),
                "running": false,
                "completed": false,
                "cancelled": true,
                "failed": false,
            });
            fs::write(
                Path::new(&metadata.status_path),
                serde_json::to_vec_pretty(&snapshot)
                    .context("failed to serialize image cancel snapshot")?,
            )
            .with_context(|| format!("failed to write {}", metadata.status_path))?;
            Ok(snapshot)
        },
    )
}

fn web_fetch_tool(runtime_state_root: PathBuf, cancel_flag: Option<Arc<InterruptSignal>>) -> Tool {
    Tool::new_interruptible(
        "web_fetch",
        "Fetch a web page or HTTP resource and return a readable text body. If interrupted by a newer user message or timeout observation, cancel the in-flight fetch. The model must choose timeout_seconds.",
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
            let result = run_interruptible_worker_job(
                &runtime_state_root,
                &ToolWorkerJob::WebFetch {
                    url: url.clone(),
                    max_chars,
                    headers,
                },
                timeout_seconds,
                cancel_flag.as_ref(),
            );
            match result {
                Ok(value) => Ok(value),
                Err(error) if error.to_string() == "operation cancelled" => Ok(json!({
                    "url": url,
                    "interrupted": true,
                    "cancelled": true,
                })),
                Err(error) if error.to_string().contains("timed out") => Ok(json!({
                    "url": url,
                    "timed_out": true,
                    "cancelled": true,
                })),
                Err(error) => Err(error),
            }
        },
    )
}

fn file_download_start_tool(
    workspace_root: PathBuf,
    runtime_state_root: PathBuf,
    _cancel_flag: Option<Arc<InterruptSignal>>,
) -> Tool {
    Tool::new(
        "file_download_start",
        "Start downloading an HTTP resource to a local file and return immediately with a download_id.",
        json!({
            "type": "object",
            "properties": {
                "url": {"type": "string"},
                "path": {"type": "string"},
                "headers": {"type": "object"},
                "overwrite": {"type": "boolean"}
            },
            "required": ["url", "path"],
            "additionalProperties": false
        }),
        move |arguments| {
            let arguments = arguments
                .as_object()
                .ok_or_else(|| anyhow!("tool arguments must be an object"))?;
            let url = string_arg(arguments, "url")?;
            let path = resolve_path(&string_arg(arguments, "path")?, &workspace_root);
            let overwrite = arguments
                .get("overwrite")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            let headers = arguments
                .get("headers")
                .and_then(Value::as_object)
                .cloned()
                .unwrap_or_default();
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
            let download_id = Uuid::new_v4().to_string();
            let temp_path = download_temp_path(&path, &download_id);
            let task_dir = background_task_dir(&runtime_state_root, "file_downloads")?;
            let status_path = task_dir.join(format!("{}.status.json", download_id));
            let initial = json!({
                "download_id": download_id,
                "url": url,
                "path": path.display().to_string(),
                "running": true,
                "completed": false,
                "cancelled": false,
                "failed": false,
                "bytes_downloaded": 0,
                "total_bytes": Value::Null,
                "http_status": Value::Null,
                "final_url": Value::Null,
                "content_type": Value::Null,
            });
            fs::write(
                &status_path,
                serde_json::to_vec_pretty(&initial)
                    .context("failed to serialize file download status")?,
            )
            .with_context(|| format!("failed to write {}", status_path.display()))?;
            let job = ToolWorkerJob::FileDownload {
                download_id: download_id.clone(),
                url: url.clone(),
                path: path.display().to_string(),
                temp_path: temp_path.display().to_string(),
                headers,
                status_path: status_path.display().to_string(),
            };
            let metadata = spawn_background_worker_process(
                &runtime_state_root,
                "file-download",
                &download_id,
                &job,
            )?;
            write_background_task_metadata(&task_dir, &metadata)?;
            read_file_download_snapshot(&runtime_state_root, &download_id)
        },
    )
}

fn file_download_progress_tool(
    runtime_state_root: PathBuf,
    _cancel_flag: Option<Arc<InterruptSignal>>,
) -> Tool {
    Tool::new(
        "file_download_progress",
        "Read the latest progress snapshot for a previously started download by download_id.",
        json!({
            "type": "object",
            "properties": {
                "download_id": {"type": "string"}
            },
            "required": ["download_id"],
            "additionalProperties": false
        }),
        move |arguments| {
            let arguments = arguments
                .as_object()
                .ok_or_else(|| anyhow!("tool arguments must be an object"))?;
            let download_id = string_arg(arguments, "download_id")?;
            read_file_download_snapshot(&runtime_state_root, &download_id)
        },
    )
}

fn file_download_wait_tool(
    runtime_state_root: PathBuf,
    cancel_flag: Option<Arc<InterruptSignal>>,
) -> Tool {
    Tool::new_interruptible(
        "file_download_wait",
        "Wait for a previously started download by download_id. If interrupted by a newer user message or timeout observation, return immediately without cancelling the download.",
        json!({
            "type": "object",
            "properties": {
                "download_id": {"type": "string"}
            },
            "required": ["download_id"],
            "additionalProperties": false
        }),
        move |arguments| {
            let arguments = arguments
                .as_object()
                .ok_or_else(|| anyhow!("tool arguments must be an object"))?;
            let download_id = string_arg(arguments, "download_id")?;
            let cancel_receiver = cancel_flag.as_ref().map(|signal| signal.subscribe());
            loop {
                let snapshot = read_file_download_snapshot(&runtime_state_root, &download_id)?;
                let finished = snapshot
                    .get("running")
                    .and_then(Value::as_bool)
                    .is_some_and(|running| !running);
                if finished {
                    return Ok(snapshot);
                }
                if let Some(cancel_receiver) = &cancel_receiver {
                    crossbeam_channel::select! {
                        recv(cancel_receiver) -> _ => {
                            return Ok(json!({
                                "interrupted": true,
                                "download": snapshot,
                            }));
                        }
                        recv(crossbeam_channel::after(Duration::from_millis(200))) -> _ => {}
                    }
                } else {
                    thread::sleep(Duration::from_millis(200));
                }
            }
        },
    )
}

fn file_download_cancel_tool(
    runtime_state_root: PathBuf,
    _cancel_flag: Option<Arc<InterruptSignal>>,
) -> Tool {
    Tool::new(
        "file_download_cancel",
        "Cancel a previously started download by download_id.",
        json!({
            "type": "object",
            "properties": {
                "download_id": {"type": "string"}
            },
            "required": ["download_id"],
            "additionalProperties": false
        }),
        move |arguments| {
            let arguments = arguments
                .as_object()
                .ok_or_else(|| anyhow!("tool arguments must be an object"))?;
            let download_id = string_arg(arguments, "download_id")?;
            let task_dir = background_task_dir(&runtime_state_root, "file_downloads")?;
            let metadata = read_background_task_metadata(&task_dir, &download_id)?;
            let previous = read_file_download_snapshot(&runtime_state_root, &download_id).ok();
            terminate_process_pid(metadata.pid);
            let _ = record_exit_code(Path::new(&metadata.exit_code_path), -9);
            let snapshot = json!({
                "download_id": download_id,
                "url": previous
                    .as_ref()
                    .and_then(|value| value.get("url").cloned())
                    .unwrap_or(Value::String(String::new())),
                "path": previous
                    .as_ref()
                    .and_then(|value| value.get("path").cloned())
                    .unwrap_or(Value::String(String::new())),
                "running": false,
                "completed": false,
                "cancelled": true,
                "failed": false,
            });
            fs::write(
                Path::new(&metadata.status_path),
                serde_json::to_vec_pretty(&snapshot)
                    .context("failed to serialize file download cancel snapshot")?,
            )
            .with_context(|| format!("failed to write {}", metadata.status_path))?;
            Ok(snapshot)
        },
    )
}

fn web_search_tool(
    runtime_state_root: PathBuf,
    workspace_root: PathBuf,
    search_config: ExternalWebSearchConfig,
    cancel_flag: Option<Arc<InterruptSignal>>,
) -> Tool {
    let mut properties = Map::new();
    properties.insert("query".to_string(), json!({"type": "string"}));
    properties.insert("timeout_seconds".to_string(), json!({"type": "number"}));
    properties.insert("max_results".to_string(), json!({"type": "integer"}));
    maybe_add_images_schema(&mut properties, search_config.supports_vision_input);
    Tool::new_interruptible(
        "web_search",
        "Search the web using the configured search provider and return an answer plus citations. If interrupted by a newer user message or timeout observation, this tool cancels the in-flight search result and returns immediately.",
        Value::Object(Map::from_iter([
            ("type".to_string(), Value::String("object".to_string())),
            ("properties".to_string(), Value::Object(properties)),
            ("required".to_string(), json!(["query", "timeout_seconds"])),
            ("additionalProperties".to_string(), Value::Bool(false)),
        ])),
        move |arguments| {
            let arguments = arguments
                .as_object()
                .ok_or_else(|| anyhow!("tool arguments must be an object"))?;
            let query = string_arg(arguments, "query")?;
            let timeout_seconds = f64_arg(arguments, "timeout_seconds")?;
            let max_results = usize_arg_with_default(arguments, "max_results", 8)?;
            let images = string_array_arg(arguments, "images")?
                .into_iter()
                .map(|path| resolve_path(&path, &workspace_root).display().to_string())
                .collect::<Vec<_>>();
            let mut runtime_search_config = search_config.clone();
            runtime_search_config.timeout_seconds = timeout_seconds;
            let search_result = run_interruptible_worker_job(
                &runtime_state_root,
                &ToolWorkerJob::WebSearch {
                    search_config: runtime_search_config,
                    query: query.clone(),
                    max_results,
                    images,
                },
                timeout_seconds,
                cancel_flag.as_ref(),
            );

            match search_result {
                Ok(value) => Ok(value),
                Err(error) if error.to_string() == "operation cancelled" => Ok(json!({
                    "query": query,
                    "interrupted": true,
                    "cancelled": true,
                })),
                Err(error) if error.to_string().contains("timed out") => Ok(json!({
                    "query": query,
                    "timed_out": true,
                    "cancelled": true,
                })),
                Err(error) => Err(error),
            }
        },
    )
}

fn skill_load_tool(
    skills: &[SkillMetadata],
    _cancel_flag: Option<Arc<InterruptSignal>>,
) -> Result<Tool> {
    let skill_index = build_skill_index(skills)?;
    let available_skills = skill_index.keys().cloned().collect::<Vec<_>>();
    Ok(Tool::new(
        "skill_load",
        "Load the SKILL.md instructions for a named skill. Use exact skill names from the preloaded metadata.",
        json!({
            "type": "object",
            "properties": {
                "skill_name": {"type": "string", "enum": available_skills}
            },
            "required": ["skill_name"],
            "additionalProperties": false
        }),
        move |arguments| {
            let arguments = arguments
                .as_object()
                .ok_or_else(|| anyhow!("tool arguments must be an object"))?;
            let skill_name = string_arg(arguments, "skill_name")?;
            let (skill, content) = load_skill_by_name(&skill_index, &skill_name)?;
            Ok(json!({
                "name": skill.name,
                "description": skill.description,
                "content": content
            }))
        },
    ))
}

fn skill_create_tool(workspace_root: PathBuf, skill_roots: Vec<PathBuf>) -> Tool {
    Tool::new(
        "skill_create",
        "Persist a staged skill directory from .skills/<skill_name>/ in the current workspace into the runtime skills store as a new skill. Validate SKILL.md and fail with the validation reason if invalid.",
        json!({
            "type": "object",
            "properties": {
                "skill_name": {"type": "string"}
            },
            "required": ["skill_name"],
            "additionalProperties": false
        }),
        move |arguments| {
            let arguments = arguments
                .as_object()
                .ok_or_else(|| anyhow!("tool arguments must be an object"))?;
            let skill_name = string_arg(arguments, "skill_name")?;
            persist_staged_skill_directory(&workspace_root, &skill_roots, &skill_name, false)
        },
    )
}

fn skill_update_tool(workspace_root: PathBuf, skill_roots: Vec<PathBuf>) -> Tool {
    Tool::new(
        "skill_update",
        "Persist a staged skill directory from .skills/<skill_name>/ in the current workspace into the runtime skills store as an update to an existing skill. Validate SKILL.md and fail with the validation reason if invalid.",
        json!({
            "type": "object",
            "properties": {
                "skill_name": {"type": "string"}
            },
            "required": ["skill_name"],
            "additionalProperties": false
        }),
        move |arguments| {
            let arguments = arguments
                .as_object()
                .ok_or_else(|| anyhow!("tool arguments must be an object"))?;
            let skill_name = string_arg(arguments, "skill_name")?;
            persist_staged_skill_directory(&workspace_root, &skill_roots, &skill_name, true)
        },
    )
}

pub fn build_tool_registry(
    enabled_tools: &[String],
    workspace_root: &Path,
    runtime_state_root: &Path,
    upstream: &UpstreamConfig,
    image_tool_upstream: Option<&UpstreamConfig>,
    pdf_tool_upstream: Option<&UpstreamConfig>,
    audio_tool_upstream: Option<&UpstreamConfig>,
    image_generation_tool_upstream: Option<&UpstreamConfig>,
    skill_roots: &[PathBuf],
    skills: &[SkillMetadata],
    extra_tools: &[Tool],
) -> Result<BTreeMap<String, Tool>> {
    build_tool_registry_with_cancel(
        enabled_tools,
        workspace_root,
        runtime_state_root,
        upstream,
        image_tool_upstream,
        pdf_tool_upstream,
        audio_tool_upstream,
        image_generation_tool_upstream,
        skill_roots,
        skills,
        extra_tools,
        None,
    )
}

pub fn build_tool_registry_with_cancel(
    _enabled_tools: &[String],
    workspace_root: &Path,
    runtime_state_root: &Path,
    upstream: &UpstreamConfig,
    image_tool_upstream: Option<&UpstreamConfig>,
    pdf_tool_upstream: Option<&UpstreamConfig>,
    audio_tool_upstream: Option<&UpstreamConfig>,
    image_generation_tool_upstream: Option<&UpstreamConfig>,
    skill_roots: &[PathBuf],
    skills: &[SkillMetadata],
    extra_tools: &[Tool],
    cancel_flag: Option<Arc<InterruptSignal>>,
) -> Result<BTreeMap<String, Tool>> {
    let mut registry = BTreeMap::from([
        (
            "file_read".to_string(),
            file_read_tool(workspace_root.to_path_buf(), cancel_flag.clone()),
        ),
        (
            "file_write".to_string(),
            file_write_tool(workspace_root.to_path_buf(), cancel_flag.clone()),
        ),
        (
            "glob".to_string(),
            glob_tool(workspace_root.to_path_buf(), cancel_flag.clone()),
        ),
        (
            "grep".to_string(),
            grep_tool(workspace_root.to_path_buf(), cancel_flag.clone()),
        ),
        (
            "ls".to_string(),
            ls_tool(workspace_root.to_path_buf(), cancel_flag.clone()),
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
            exec_observe_tool(
                workspace_root.to_path_buf(),
                runtime_state_root.to_path_buf(),
                cancel_flag.clone(),
            ),
        ),
        (
            "exec_wait".to_string(),
            exec_wait_tool(
                workspace_root.to_path_buf(),
                runtime_state_root.to_path_buf(),
                cancel_flag.clone(),
            ),
        ),
        (
            "exec_kill".to_string(),
            exec_kill_tool(runtime_state_root.to_path_buf(), cancel_flag.clone()),
        ),
        (
            "apply_patch".to_string(),
            apply_patch_tool(workspace_root.to_path_buf(), cancel_flag.clone()),
        ),
        (
            "file_download_start".to_string(),
            file_download_start_tool(
                workspace_root.to_path_buf(),
                runtime_state_root.to_path_buf(),
                cancel_flag.clone(),
            ),
        ),
        (
            "file_download_progress".to_string(),
            file_download_progress_tool(runtime_state_root.to_path_buf(), cancel_flag.clone()),
        ),
        (
            "file_download_wait".to_string(),
            file_download_wait_tool(runtime_state_root.to_path_buf(), cancel_flag.clone()),
        ),
        (
            "file_download_cancel".to_string(),
            file_download_cancel_tool(runtime_state_root.to_path_buf(), cancel_flag.clone()),
        ),
        (
            "web_fetch".to_string(),
            web_fetch_tool(runtime_state_root.to_path_buf(), cancel_flag.clone()),
        ),
    ]);
    if image_tool_upstream.is_some() {
        registry.insert(
            "image_start".to_string(),
            image_start_tool(
                workspace_root.to_path_buf(),
                runtime_state_root.to_path_buf(),
                upstream.clone(),
                image_tool_upstream.cloned(),
                cancel_flag.clone(),
            ),
        );
        registry.insert(
            "image_wait".to_string(),
            image_wait_tool(runtime_state_root.to_path_buf(), cancel_flag.clone()),
        );
        registry.insert(
            "image_cancel".to_string(),
            image_cancel_tool(runtime_state_root.to_path_buf(), cancel_flag.clone()),
        );
    } else if upstream.native_image_input {
        registry.insert(
            "image_load".to_string(),
            image_load_tool(
                workspace_root.to_path_buf(),
                upstream.clone(),
                cancel_flag.clone(),
            ),
        );
    }
    if let Some(pdf_tool_upstream) = pdf_tool_upstream {
        registry.insert(
            "pdf_query".to_string(),
            pdf_query_tool(
                workspace_root.to_path_buf(),
                runtime_state_root.to_path_buf(),
                pdf_tool_upstream.clone(),
                cancel_flag.clone(),
            ),
        );
    } else if upstream.native_pdf_input {
        registry.insert(
            "pdf_load".to_string(),
            pdf_load_tool(
                workspace_root.to_path_buf(),
                upstream.clone(),
                cancel_flag.clone(),
            ),
        );
    }
    if let Some(audio_tool_upstream) = audio_tool_upstream {
        registry.insert(
            "audio_transcribe".to_string(),
            audio_transcribe_tool(
                workspace_root.to_path_buf(),
                runtime_state_root.to_path_buf(),
                audio_tool_upstream.clone(),
                cancel_flag.clone(),
            ),
        );
    } else if upstream.native_audio_input {
        registry.insert(
            "audio_load".to_string(),
            audio_load_tool(
                workspace_root.to_path_buf(),
                upstream.clone(),
                cancel_flag.clone(),
            ),
        );
    }
    if let Some(image_generation_tool_upstream) = image_generation_tool_upstream {
        registry.insert(
            "image_generate".to_string(),
            image_generate_tool(
                workspace_root.to_path_buf(),
                runtime_state_root.to_path_buf(),
                image_generation_tool_upstream.clone(),
                cancel_flag.clone(),
            ),
        );
    }
    let native_web_search_enabled = upstream
        .native_web_search
        .as_ref()
        .is_some_and(|settings| settings.enabled);
    if !native_web_search_enabled {
        if let Some(web_search_config) = upstream.external_web_search.clone() {
            registry.insert(
                "web_search".to_string(),
                web_search_tool(
                    runtime_state_root.to_path_buf(),
                    workspace_root.to_path_buf(),
                    web_search_config,
                    cancel_flag.clone(),
                ),
            );
        }
    }

    if !skills.is_empty() {
        let skill_tool = skill_load_tool(skills, cancel_flag.clone())?;
        registry.insert(skill_tool.name.clone(), skill_tool);
    }

    if !skill_roots.is_empty() {
        let create_tool = skill_create_tool(workspace_root.to_path_buf(), skill_roots.to_vec());
        registry.insert(create_tool.name.clone(), create_tool);
        let update_tool = skill_update_tool(workspace_root.to_path_buf(), skill_roots.to_vec());
        registry.insert(update_tool.name.clone(), update_tool);
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
    let normalized_name = canonical_tool_name(tool_name);
    let Some(tool) = registry.get(normalized_name) else {
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

#[cfg(test)]
mod tests {
    use super::{
        BackgroundTaskMetadata, ExecutionTarget, FILE_READ_MAX_OUTPUT_BYTES, LS_MAX_ENTRIES,
        ProcessMetadata, Tool, active_runtime_state_summary, build_tool_registry_with_cancel,
        execute_tool_call, execution_target_arg, process_is_running, process_meta_path,
        terminate_runtime_state_tasks, write_background_task_metadata,
    };
    use crate::config::{
        AuthCredentialsStoreMode, UpstreamApiKind, UpstreamAuthKind, UpstreamConfig,
    };
    use serde_json::json;
    use std::ffi::OsString;
    use std::fs;
    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt;
    use std::path::PathBuf;
    use std::process::{Command, Stdio};
    use std::sync::{Mutex, OnceLock};
    use std::thread;
    use std::time::Duration;
    use tempfile::TempDir;

    fn test_upstream() -> UpstreamConfig {
        UpstreamConfig {
            base_url: "https://example.com".to_string(),
            model: "demo".to_string(),
            api_kind: UpstreamApiKind::Responses,
            auth_kind: UpstreamAuthKind::ApiKey,
            supports_vision_input: false,
            supports_pdf_input: false,
            supports_audio_input: false,
            api_key: None,
            api_key_env: "OPENAI_API_KEY".to_string(),
            chat_completions_path: "/responses".to_string(),
            codex_home: None,
            codex_auth: None,
            auth_credentials_store_mode: AuthCredentialsStoreMode::Auto,
            timeout_seconds: 30.0,
            retry_mode: Default::default(),
            context_window_tokens: 200_000,
            cache_control: None,
            prompt_cache_retention: None,
            prompt_cache_key: None,
            reasoning: None,
            headers: serde_json::Map::new(),
            native_web_search: None,
            external_web_search: None,
            native_image_input: false,
            native_pdf_input: false,
            native_audio_input: false,
            native_image_generation: false,
            token_estimation: None,
        }
    }

    fn env_test_lock() -> &'static Mutex<()> {
        static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        LOCK.get_or_init(|| Mutex::new(()))
    }

    struct EnvVarGuard {
        key: &'static str,
        previous: Option<OsString>,
    }

    impl EnvVarGuard {
        fn set(key: &'static str, value: impl AsRef<std::ffi::OsStr>) -> Self {
            let previous = std::env::var_os(key);
            unsafe {
                std::env::set_var(key, value);
            }
            Self { key, previous }
        }
    }

    impl Drop for EnvVarGuard {
        fn drop(&mut self) {
            unsafe {
                if let Some(previous) = &self.previous {
                    std::env::set_var(self.key, previous);
                } else {
                    std::env::remove_var(self.key);
                }
            }
        }
    }

    #[cfg(unix)]
    fn write_fake_ssh(temp_dir: &TempDir) -> PathBuf {
        write_fake_ssh_with_path(temp_dir, "exec sh -c \"$remote_command\"")
    }

    #[cfg(unix)]
    fn write_fake_ssh_with_path(temp_dir: &TempDir, path_line: &str) -> PathBuf {
        let path = temp_dir.path().join("fake-ssh");
        fs::write(
            &path,
            format!(
                r#"#!/bin/sh
while [ "$1" = "-o" ]; do
  shift 2
done
if [ "$1" = "-T" ] || [ "$1" = "-tt" ]; then
  shift
fi
shift
remote_command="$*"
{path_line}
"#
            ),
        )
        .unwrap();
        let mut permissions = fs::metadata(&path).unwrap().permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&path, permissions).unwrap();
        path
    }

    #[test]
    fn openai_tool_description_includes_execution_mode_guidance() {
        let immediate = Tool::new(
            "immediate_demo",
            "Immediate demo tool.",
            json!({"type": "object", "properties": {}, "additionalProperties": false}),
            |_| Ok(json!({"ok": true})),
        );
        let interruptible = Tool::new_interruptible(
            "interruptible_demo",
            "Interruptible demo tool.",
            json!({"type": "object", "properties": {}, "additionalProperties": false}),
            |_| Ok(json!({"ok": true})),
        );

        let immediate_description = immediate
            .as_openai_tool()
            .get("function")
            .and_then(|value| value.get("description"))
            .and_then(|value| value.as_str())
            .unwrap_or_default()
            .to_string();
        let interruptible_description = interruptible
            .as_openai_tool()
            .get("function")
            .and_then(|value| value.get("description"))
            .and_then(|value| value.as_str())
            .unwrap_or_default()
            .to_string();

        assert!(immediate_description.contains("Execution mode: immediate."));
        assert!(interruptible_description.contains("Execution mode: interruptible."));
    }

    #[test]
    fn responses_tool_schema_is_flattened() {
        let tool = Tool::new(
            "demo",
            "A demo tool.",
            json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string" }
                }
            }),
            |_| Ok(json!({"ok": true})),
        );

        let value = tool.as_responses_tool();
        assert_eq!(value["type"], "function");
        assert_eq!(value["name"], "demo");
        assert_eq!(value["parameters"]["type"], "object");
        assert!(value.get("function").is_none());
    }

    #[test]
    fn execute_tool_call_accepts_legacy_file_tool_names() {
        let temp_dir = TempDir::new().unwrap();
        let workspace_root = temp_dir.path().join("workspace");
        fs::create_dir_all(&workspace_root).unwrap();
        let runtime_state_root = temp_dir.path().join("runtime");
        fs::create_dir_all(&runtime_state_root).unwrap();
        let upstream = UpstreamConfig {
            base_url: "https://example.com".to_string(),
            model: "demo".to_string(),
            api_kind: UpstreamApiKind::Responses,
            auth_kind: UpstreamAuthKind::ApiKey,
            supports_vision_input: false,
            supports_pdf_input: false,
            supports_audio_input: false,
            api_key: None,
            api_key_env: "OPENAI_API_KEY".to_string(),
            chat_completions_path: "/responses".to_string(),
            codex_home: None,
            codex_auth: None,
            auth_credentials_store_mode: AuthCredentialsStoreMode::Auto,
            timeout_seconds: 30.0,
            retry_mode: Default::default(),
            context_window_tokens: 200_000,
            cache_control: None,
            prompt_cache_retention: None,
            prompt_cache_key: None,
            reasoning: None,
            headers: serde_json::Map::new(),
            native_web_search: None,
            external_web_search: None,
            native_image_input: false,
            native_pdf_input: false,
            native_audio_input: false,
            native_image_generation: false,
            token_estimation: None,
        };
        let registry = build_tool_registry_with_cancel(
            &[],
            &workspace_root,
            &runtime_state_root,
            &upstream,
            None,
            None,
            None,
            None,
            &Vec::<PathBuf>::new(),
            &[],
            &[],
            None,
        )
        .unwrap();

        let write_result = execute_tool_call(
            &registry,
            "write_file",
            Some(r#"{"path":"legacy.txt","content":"hello"}"#),
        );
        assert!(write_result.contains("legacy.txt"));

        let read_result = execute_tool_call(
            &registry,
            "read_file",
            Some(r#"{"path":"legacy.txt","offset_lines":0,"limit_lines":10}"#),
        );
        assert!(read_result.contains("1: hello"));
    }

    #[test]
    fn glob_grep_and_ls_tools_explore_workspace() {
        let temp_dir = TempDir::new().unwrap();
        let workspace_root = temp_dir.path().join("workspace");
        let src_dir = workspace_root.join("src");
        fs::create_dir_all(&src_dir).unwrap();
        fs::write(
            src_dir.join("main.rs"),
            "fn main() { println!(\"hello\"); }\n",
        )
        .unwrap();
        fs::write(src_dir.join("lib.rs"), "pub fn helper() {}\n").unwrap();
        let runtime_state_root = temp_dir.path().join("runtime");
        fs::create_dir_all(&runtime_state_root).unwrap();
        let upstream = UpstreamConfig {
            base_url: "https://example.com".to_string(),
            model: "demo".to_string(),
            api_kind: UpstreamApiKind::Responses,
            auth_kind: UpstreamAuthKind::ApiKey,
            supports_vision_input: false,
            supports_pdf_input: false,
            supports_audio_input: false,
            api_key: None,
            api_key_env: "OPENAI_API_KEY".to_string(),
            chat_completions_path: "/responses".to_string(),
            codex_home: None,
            codex_auth: None,
            auth_credentials_store_mode: AuthCredentialsStoreMode::Auto,
            timeout_seconds: 30.0,
            retry_mode: Default::default(),
            context_window_tokens: 200_000,
            cache_control: None,
            prompt_cache_retention: None,
            prompt_cache_key: None,
            reasoning: None,
            headers: serde_json::Map::new(),
            native_web_search: None,
            external_web_search: None,
            native_image_input: false,
            native_pdf_input: false,
            native_audio_input: false,
            native_image_generation: false,
            token_estimation: None,
        };
        let registry = build_tool_registry_with_cancel(
            &[],
            &workspace_root,
            &runtime_state_root,
            &upstream,
            None,
            None,
            None,
            None,
            &Vec::<PathBuf>::new(),
            &[],
            &[],
            None,
        )
        .unwrap();

        let glob_result = registry["glob"]
            .invoke(json!({"pattern":"src/**/*.rs"}))
            .unwrap();
        assert_eq!(glob_result["num_files"].as_u64(), Some(2));

        let grep_result = registry["grep"]
            .invoke(json!({"pattern":"println!", "path":"src"}))
            .unwrap();
        assert_eq!(grep_result["num_files"].as_u64(), Some(1));
        assert!(
            grep_result["filenames"][0]
                .as_str()
                .is_some_and(|path| path.ends_with("main.rs"))
        );

        let ls_result = registry["ls"].invoke(json!({"path":"src"})).unwrap();
        let ls_result = ls_result.as_str().unwrap();
        assert!(ls_result.contains("num_entries: 2"));
        assert!(ls_result.contains("truncated: false"));
        assert!(ls_result.contains("- main.rs"));
        assert!(ls_result.contains("- lib.rs"));
        assert!(!ls_result.contains("\"entries\""));
        assert!(!ls_result.contains("\"type\""));
    }

    #[test]
    fn supported_tools_expose_optional_remote_parameter() {
        let temp_dir = TempDir::new().unwrap();
        let workspace_root = temp_dir.path().join("workspace");
        fs::create_dir_all(&workspace_root).unwrap();
        let runtime_state_root = temp_dir.path().join("runtime");
        fs::create_dir_all(&runtime_state_root).unwrap();
        let upstream = test_upstream();
        let registry = build_tool_registry_with_cancel(
            &[],
            &workspace_root,
            &runtime_state_root,
            &upstream,
            None,
            None,
            None,
            None,
            &Vec::<PathBuf>::new(),
            &[],
            &[],
            None,
        )
        .unwrap();

        for name in [
            "file_read",
            "file_write",
            "glob",
            "grep",
            "ls",
            "edit",
            "exec_start",
            "apply_patch",
        ] {
            let tool = registry.get(name).expect("tool should be registered");
            let remote = &tool.parameters["properties"]["remote"];
            assert_eq!(remote["type"], "string");
            assert!(
                remote["description"]
                    .as_str()
                    .is_some_and(|description| description.contains("<host>|local")),
                "remote schema should document the accepted target format for {name}"
            );
            let required = tool.parameters["required"].as_array().unwrap();
            assert!(
                !required.iter().any(|item| item.as_str() == Some("remote")),
                "remote must stay optional for {name}"
            );
        }
    }

    #[test]
    fn exec_followup_tools_do_not_expose_remote_parameter() {
        let temp_dir = TempDir::new().unwrap();
        let workspace_root = temp_dir.path().join("workspace");
        fs::create_dir_all(&workspace_root).unwrap();
        let runtime_state_root = temp_dir.path().join("runtime");
        fs::create_dir_all(&runtime_state_root).unwrap();
        let upstream = test_upstream();
        let registry = build_tool_registry_with_cancel(
            &[],
            &workspace_root,
            &runtime_state_root,
            &upstream,
            None,
            None,
            None,
            None,
            &Vec::<PathBuf>::new(),
            &[],
            &[],
            None,
        )
        .unwrap();

        for name in ["exec_observe", "exec_wait", "exec_kill"] {
            let tool = registry.get(name).expect("tool should be registered");
            assert!(
                tool.parameters["properties"].get("remote").is_none(),
                "{name} should infer remote from exec_id instead of exposing a remote argument"
            );
        }
    }

    #[test]
    fn empty_remote_argument_is_treated_as_local() {
        let arguments = json!({"remote": ""});
        let target = execution_target_arg(arguments.as_object().unwrap()).unwrap();
        assert_eq!(target, ExecutionTarget::Local);
    }

    #[test]
    fn placeholder_remote_argument_is_rejected() {
        let arguments = json!({"remote": "host"});
        let error = execution_target_arg(arguments.as_object().unwrap()).unwrap_err();
        assert!(format!("{error:#}").contains("actual SSH host alias"));
    }

    #[cfg(unix)]
    #[test]
    fn remote_filesystem_tools_use_ssh_and_omit_local_state() {
        let _env_guard = env_test_lock()
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let temp_dir = TempDir::new().unwrap();
        let fake_ssh = write_fake_ssh(&temp_dir);
        let _ssh_guard = EnvVarGuard::set("AGENT_FRAME_SSH_BIN", fake_ssh.as_os_str());
        let workspace_root = temp_dir.path().join("workspace");
        fs::create_dir_all(workspace_root.join("src")).unwrap();
        let runtime_state_root = temp_dir.path().join("runtime");
        fs::create_dir_all(&runtime_state_root).unwrap();
        let upstream = test_upstream();
        let registry = build_tool_registry_with_cancel(
            &[],
            &workspace_root,
            &runtime_state_root,
            &upstream,
            None,
            None,
            None,
            None,
            &Vec::<PathBuf>::new(),
            &[],
            &[],
            None,
        )
        .unwrap();

        let write_result = registry["file_write"]
            .invoke(json!({
                "file_path": "src/remote.txt",
                "content": "alpha\nbeta\n",
                "remote": "fake-host"
            }))
            .unwrap();
        assert_eq!(write_result["remote"].as_str(), Some("fake-host"));
        assert_eq!(
            fs::read_to_string(workspace_root.join("src/remote.txt")).unwrap(),
            "alpha\nbeta\n"
        );

        let read_result = registry["file_read"]
            .invoke(json!({
                "file_path": "src/remote.txt",
                "offset": 1,
                "limit": 10,
                "remote": "fake-host"
            }))
            .unwrap();
        assert_eq!(read_result["remote"].as_str(), Some("fake-host"));
        assert!(
            read_result["content"]
                .as_str()
                .unwrap()
                .contains("1: alpha")
        );

        let grep_result = registry["grep"]
            .invoke(json!({
                "pattern": "beta",
                "path": "src",
                "include": "*.txt",
                "remote": "fake-host"
            }))
            .unwrap();
        assert_eq!(grep_result["num_files"].as_u64(), Some(1));

        let ls_result = registry["ls"]
            .invoke(json!({
                "path": "src",
                "remote": "fake-host"
            }))
            .unwrap();
        let ls_result = ls_result.as_str().unwrap();
        assert!(ls_result.contains("remote: fake-host"));
        assert!(ls_result.contains("num_entries: 1"));
        assert!(ls_result.contains("truncated: false"));
        assert!(ls_result.contains("- remote.txt"));

        let remote_crowded_dir = workspace_root.join("crowded");
        fs::create_dir_all(&remote_crowded_dir).unwrap();
        for index in 0..(LS_MAX_ENTRIES + 5) {
            fs::write(
                remote_crowded_dir.join(format!("file_{index:04}.txt")),
                "data\n",
            )
            .unwrap();
        }
        let remote_crowded_result = registry["ls"]
            .invoke(json!({
                "path": "crowded",
                "remote": "fake-host"
            }))
            .unwrap();
        let remote_crowded_result = remote_crowded_result.as_str().unwrap();
        assert!(remote_crowded_result.contains("remote: fake-host"));
        assert!(remote_crowded_result.contains("num_entries: >1000"));
        assert!(remote_crowded_result.contains("truncated: true"));
        assert!(!remote_crowded_result.contains("\"entries\""));
    }

    #[cfg(unix)]
    #[test]
    fn remote_file_tools_fall_back_to_python_when_python3_is_missing() {
        let _env_guard = env_test_lock()
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let temp_dir = TempDir::new().unwrap();
        let bin_dir = temp_dir.path().join("bin");
        fs::create_dir_all(&bin_dir).unwrap();
        std::os::unix::fs::symlink("/bin/sh", bin_dir.join("sh")).unwrap();
        let python3_path = std::env::var("PATH")
            .unwrap_or_default()
            .split(':')
            .map(|path| std::path::Path::new(path).join("python3"))
            .find(|path| path.is_file())
            .expect("test host must have python3 on PATH");
        std::os::unix::fs::symlink(python3_path, bin_dir.join("python")).unwrap();
        let fake_ssh = write_fake_ssh_with_path(
            &temp_dir,
            &format!(
                "PATH='{}' exec sh -c \"$remote_command\"",
                bin_dir.display()
            ),
        );
        let _ssh_guard = EnvVarGuard::set("AGENT_FRAME_SSH_BIN", fake_ssh.as_os_str());
        let workspace_root = temp_dir.path().join("workspace");
        fs::create_dir_all(workspace_root.join("src")).unwrap();
        fs::write(workspace_root.join("src/fallback.txt"), "ok\n").unwrap();
        let runtime_state_root = temp_dir.path().join("runtime");
        fs::create_dir_all(&runtime_state_root).unwrap();
        let upstream = test_upstream();
        let registry = build_tool_registry_with_cancel(
            &[],
            &workspace_root,
            &runtime_state_root,
            &upstream,
            None,
            None,
            None,
            None,
            &Vec::<PathBuf>::new(),
            &[],
            &[],
            None,
        )
        .unwrap();

        let ls_result = registry["ls"]
            .invoke(json!({"path": "src", "remote": "fake-host"}))
            .unwrap();
        let ls_result = ls_result.as_str().unwrap();
        assert!(ls_result.contains("remote: fake-host"));
        assert!(ls_result.contains("- fallback.txt"));
    }

    #[cfg(unix)]
    #[test]
    fn remote_file_tools_report_missing_python_clearly() {
        let _env_guard = env_test_lock()
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let temp_dir = TempDir::new().unwrap();
        let bin_dir = temp_dir.path().join("bin");
        fs::create_dir_all(&bin_dir).unwrap();
        std::os::unix::fs::symlink("/bin/sh", bin_dir.join("sh")).unwrap();
        let fake_ssh = write_fake_ssh_with_path(
            &temp_dir,
            &format!(
                "PATH='{}' exec sh -c \"$remote_command\"",
                bin_dir.display()
            ),
        );
        let _ssh_guard = EnvVarGuard::set("AGENT_FRAME_SSH_BIN", fake_ssh.as_os_str());
        let workspace_root = temp_dir.path().join("workspace");
        fs::create_dir_all(workspace_root.join("src")).unwrap();
        let runtime_state_root = temp_dir.path().join("runtime");
        fs::create_dir_all(&runtime_state_root).unwrap();
        let upstream = test_upstream();
        let registry = build_tool_registry_with_cancel(
            &[],
            &workspace_root,
            &runtime_state_root,
            &upstream,
            None,
            None,
            None,
            None,
            &Vec::<PathBuf>::new(),
            &[],
            &[],
            None,
        )
        .unwrap();

        let error = registry["ls"]
            .invoke(json!({"path": "src", "remote": "fake-host"}))
            .unwrap_err();
        assert!(format!("{error:#}").contains("remote file tools require Python 3"));
    }

    #[test]
    fn ls_skips_hidden_and_common_cache_directories() {
        let temp_dir = TempDir::new().unwrap();
        let workspace_root = temp_dir.path().join("workspace");
        let visible_dir = workspace_root.join("src");
        let hidden_dir = workspace_root.join(".venv_tools");
        let cache_dir = workspace_root.join("node_modules");
        fs::create_dir_all(&visible_dir).unwrap();
        fs::create_dir_all(&hidden_dir).unwrap();
        fs::create_dir_all(&cache_dir).unwrap();
        fs::write(visible_dir.join("main.rs"), "fn main() {}\n").unwrap();
        fs::write(hidden_dir.join("ignored.py"), "print('ignore')\n").unwrap();
        fs::write(cache_dir.join("ignored.js"), "console.log('ignore')\n").unwrap();
        fs::write(workspace_root.join(".env"), "SECRET=1\n").unwrap();
        let runtime_state_root = temp_dir.path().join("runtime");
        fs::create_dir_all(&runtime_state_root).unwrap();
        let upstream = UpstreamConfig {
            base_url: "https://example.com".to_string(),
            model: "demo".to_string(),
            api_kind: UpstreamApiKind::Responses,
            auth_kind: UpstreamAuthKind::ApiKey,
            supports_vision_input: false,
            supports_pdf_input: false,
            supports_audio_input: false,
            api_key: None,
            api_key_env: "OPENAI_API_KEY".to_string(),
            chat_completions_path: "/responses".to_string(),
            codex_home: None,
            codex_auth: None,
            auth_credentials_store_mode: AuthCredentialsStoreMode::Auto,
            timeout_seconds: 30.0,
            retry_mode: Default::default(),
            context_window_tokens: 200_000,
            cache_control: None,
            prompt_cache_retention: None,
            prompt_cache_key: None,
            reasoning: None,
            headers: serde_json::Map::new(),
            native_web_search: None,
            external_web_search: None,
            native_image_input: false,
            native_pdf_input: false,
            native_audio_input: false,
            native_image_generation: false,
            token_estimation: None,
        };
        let registry = build_tool_registry_with_cancel(
            &[],
            &workspace_root,
            &runtime_state_root,
            &upstream,
            None,
            None,
            None,
            None,
            &Vec::<PathBuf>::new(),
            &[],
            &[],
            None,
        )
        .unwrap();

        let ls_result = registry["ls"].invoke(json!({"path":"."})).unwrap();
        let ls_result = ls_result.as_str().unwrap();
        assert!(ls_result.contains("- src/"));
        assert!(ls_result.contains("  - main.rs"));
        assert!(!ls_result.contains(".venv_tools"));
        assert!(!ls_result.contains("node_modules"));
        assert!(!ls_result.contains("target"));
    }

    #[test]
    fn ls_truncates_when_entry_limit_is_hit() {
        let temp_dir = TempDir::new().unwrap();
        let workspace_root = temp_dir.path().join("workspace");
        let crowded_dir = workspace_root.join("crowded");
        fs::create_dir_all(&crowded_dir).unwrap();
        for index in 0..(LS_MAX_ENTRIES + 25) {
            let filename = format!("file_{index:04}.txt");
            fs::write(crowded_dir.join(filename), "data\n").unwrap();
        }
        let runtime_state_root = temp_dir.path().join("runtime");
        fs::create_dir_all(&runtime_state_root).unwrap();
        let upstream = UpstreamConfig {
            base_url: "https://example.com".to_string(),
            model: "demo".to_string(),
            api_kind: UpstreamApiKind::Responses,
            auth_kind: UpstreamAuthKind::ApiKey,
            supports_vision_input: false,
            supports_pdf_input: false,
            supports_audio_input: false,
            api_key: None,
            api_key_env: "OPENAI_API_KEY".to_string(),
            chat_completions_path: "/responses".to_string(),
            codex_home: None,
            codex_auth: None,
            auth_credentials_store_mode: AuthCredentialsStoreMode::Auto,
            timeout_seconds: 30.0,
            retry_mode: Default::default(),
            context_window_tokens: 200_000,
            cache_control: None,
            prompt_cache_retention: None,
            prompt_cache_key: None,
            reasoning: None,
            headers: serde_json::Map::new(),
            native_web_search: None,
            external_web_search: None,
            native_image_input: false,
            native_pdf_input: false,
            native_audio_input: false,
            native_image_generation: false,
            token_estimation: None,
        };
        let registry = build_tool_registry_with_cancel(
            &[],
            &workspace_root,
            &runtime_state_root,
            &upstream,
            None,
            None,
            None,
            None,
            &Vec::<PathBuf>::new(),
            &[],
            &[],
            None,
        )
        .unwrap();

        let ls_result = registry["ls"].invoke(json!({"path":"crowded"})).unwrap();
        let ls_result = ls_result.as_str().unwrap();
        assert!(ls_result.contains("num_entries: >1000"));
        assert!(ls_result.contains("truncated: true"));
        assert!(ls_result.contains("There are more than 1000 files and directories"));
        let printed_nodes = ls_result
            .lines()
            .filter(|line| line.trim_start().starts_with("- "))
            .count();
        assert!(printed_nodes <= LS_MAX_ENTRIES + 1);
        assert!(!ls_result.contains("\"entries\""));
        assert!(!ls_result.contains("\"type\""));
    }

    #[test]
    fn file_read_rejects_large_files_without_explicit_window() {
        let temp_dir = TempDir::new().unwrap();
        let workspace_root = temp_dir.path().join("workspace");
        fs::create_dir_all(&workspace_root).unwrap();
        let large_file = workspace_root.join("large.txt");
        fs::write(&large_file, "x".repeat(FILE_READ_MAX_OUTPUT_BYTES + 1024)).unwrap();
        let runtime_state_root = temp_dir.path().join("runtime");
        fs::create_dir_all(&runtime_state_root).unwrap();
        let upstream = UpstreamConfig {
            base_url: "https://example.com".to_string(),
            model: "demo".to_string(),
            api_kind: UpstreamApiKind::Responses,
            auth_kind: UpstreamAuthKind::ApiKey,
            supports_vision_input: false,
            supports_pdf_input: false,
            supports_audio_input: false,
            api_key: None,
            api_key_env: "OPENAI_API_KEY".to_string(),
            chat_completions_path: "/responses".to_string(),
            codex_home: None,
            codex_auth: None,
            auth_credentials_store_mode: AuthCredentialsStoreMode::Auto,
            timeout_seconds: 30.0,
            retry_mode: Default::default(),
            context_window_tokens: 200_000,
            cache_control: None,
            prompt_cache_retention: None,
            prompt_cache_key: None,
            reasoning: None,
            headers: serde_json::Map::new(),
            native_web_search: None,
            external_web_search: None,
            native_image_input: false,
            native_pdf_input: false,
            native_audio_input: false,
            native_image_generation: false,
            token_estimation: None,
        };
        let registry = build_tool_registry_with_cancel(
            &[],
            &workspace_root,
            &runtime_state_root,
            &upstream,
            None,
            None,
            None,
            None,
            &Vec::<PathBuf>::new(),
            &[],
            &[],
            None,
        )
        .unwrap();

        let error = registry["file_read"]
            .invoke(json!({"file_path":"large.txt"}))
            .unwrap_err()
            .to_string();
        assert!(error.contains("provide offset and/or limit"));

        let ok = registry["file_read"]
            .invoke(json!({"file_path":"large.txt","offset":1,"limit":10}))
            .unwrap();
        assert_eq!(ok["start_line"].as_u64(), Some(1));
    }

    #[test]
    fn image_load_returns_small_multimodal_marker_payload() {
        let temp_dir = TempDir::new().unwrap();
        let workspace_root = temp_dir.path().join("workspace");
        fs::create_dir_all(&workspace_root).unwrap();
        let image_path = workspace_root.join("demo.png");
        fs::write(&image_path, b"png-bytes").unwrap();
        let runtime_state_root = temp_dir.path().join("runtime");
        fs::create_dir_all(&runtime_state_root).unwrap();
        let upstream = UpstreamConfig {
            base_url: "https://example.com".to_string(),
            model: "demo".to_string(),
            api_kind: UpstreamApiKind::Responses,
            auth_kind: UpstreamAuthKind::CodexSubscription,
            supports_vision_input: true,
            supports_pdf_input: false,
            supports_audio_input: false,
            api_key: None,
            api_key_env: "OPENAI_API_KEY".to_string(),
            chat_completions_path: "/responses".to_string(),
            codex_home: None,
            codex_auth: None,
            auth_credentials_store_mode: AuthCredentialsStoreMode::Auto,
            timeout_seconds: 30.0,
            retry_mode: Default::default(),
            context_window_tokens: 200_000,
            cache_control: None,
            prompt_cache_retention: None,
            prompt_cache_key: None,
            reasoning: None,
            headers: serde_json::Map::new(),
            native_web_search: None,
            external_web_search: None,
            native_image_input: true,
            native_pdf_input: false,
            native_audio_input: false,
            native_image_generation: false,
            token_estimation: None,
        };
        let registry = build_tool_registry_with_cancel(
            &["image_load".to_string()],
            &workspace_root,
            &runtime_state_root,
            &upstream,
            None,
            None,
            None,
            None,
            &Vec::<PathBuf>::new(),
            &[],
            &[],
            None,
        )
        .unwrap();

        let result = registry["image_load"]
            .invoke(json!({
                "path": "demo.png"
            }))
            .unwrap();

        assert_eq!(result["kind"], "synthetic_user_multimodal");
        assert_eq!(result["media"][0]["type"], "input_image");
        assert_eq!(result["media"][0]["path"], image_path.display().to_string());
        assert!(result["media"][0].get("image_url").is_none());
    }

    #[test]
    fn vision_upstream_registers_image_load_instead_of_async_image_tools() {
        let temp_dir = TempDir::new().unwrap();
        let workspace_root = temp_dir.path().join("workspace");
        fs::create_dir_all(&workspace_root).unwrap();
        let runtime_state_root = temp_dir.path().join("runtime");
        fs::create_dir_all(&runtime_state_root).unwrap();
        let upstream = UpstreamConfig {
            base_url: "https://example.com".to_string(),
            model: "demo".to_string(),
            api_kind: UpstreamApiKind::Responses,
            auth_kind: UpstreamAuthKind::CodexSubscription,
            supports_vision_input: true,
            supports_pdf_input: false,
            supports_audio_input: false,
            api_key: None,
            api_key_env: "OPENAI_API_KEY".to_string(),
            chat_completions_path: "/responses".to_string(),
            codex_home: None,
            codex_auth: None,
            auth_credentials_store_mode: AuthCredentialsStoreMode::Auto,
            timeout_seconds: 30.0,
            retry_mode: Default::default(),
            context_window_tokens: 200_000,
            cache_control: None,
            prompt_cache_retention: None,
            prompt_cache_key: None,
            reasoning: None,
            headers: serde_json::Map::new(),
            native_web_search: None,
            external_web_search: None,
            native_image_input: true,
            native_pdf_input: false,
            native_audio_input: false,
            native_image_generation: false,
            token_estimation: None,
        };
        let registry = build_tool_registry_with_cancel(
            &[],
            &workspace_root,
            &runtime_state_root,
            &upstream,
            None,
            None,
            None,
            None,
            &Vec::<PathBuf>::new(),
            &[],
            &[],
            None,
        )
        .unwrap();

        assert!(registry.contains_key("image_load"));
        assert!(!registry.contains_key("image_start"));
        assert!(!registry.contains_key("image_wait"));
        assert!(!registry.contains_key("image_cancel"));
    }

    #[test]
    fn external_image_tool_target_registers_async_image_tools() {
        let temp_dir = TempDir::new().unwrap();
        let workspace_root = temp_dir.path().join("workspace");
        fs::create_dir_all(&workspace_root).unwrap();
        let runtime_state_root = temp_dir.path().join("runtime");
        fs::create_dir_all(&runtime_state_root).unwrap();
        let upstream = UpstreamConfig {
            base_url: "https://example.com".to_string(),
            model: "demo".to_string(),
            api_kind: UpstreamApiKind::Responses,
            auth_kind: UpstreamAuthKind::CodexSubscription,
            supports_vision_input: true,
            supports_pdf_input: false,
            supports_audio_input: false,
            api_key: None,
            api_key_env: "OPENAI_API_KEY".to_string(),
            chat_completions_path: "/responses".to_string(),
            codex_home: None,
            codex_auth: None,
            auth_credentials_store_mode: AuthCredentialsStoreMode::Auto,
            timeout_seconds: 30.0,
            retry_mode: Default::default(),
            context_window_tokens: 200_000,
            cache_control: None,
            prompt_cache_retention: None,
            prompt_cache_key: None,
            reasoning: None,
            headers: serde_json::Map::new(),
            native_web_search: None,
            external_web_search: None,
            native_image_input: false,
            native_pdf_input: false,
            native_audio_input: false,
            native_image_generation: false,
            token_estimation: None,
        };
        let image_helper = UpstreamConfig {
            supports_vision_input: true,
            ..upstream.clone()
        };
        let registry = build_tool_registry_with_cancel(
            &[],
            &workspace_root,
            &runtime_state_root,
            &upstream,
            Some(&image_helper),
            None,
            None,
            None,
            &Vec::<PathBuf>::new(),
            &[],
            &[],
            None,
        )
        .unwrap();

        assert!(!registry.contains_key("image_load"));
        assert!(registry.contains_key("image_start"));
        assert!(registry.contains_key("image_wait"));
        assert!(registry.contains_key("image_cancel"));
    }

    #[test]
    fn native_pdf_input_registers_pdf_load_without_external_helper() {
        let temp_dir = TempDir::new().unwrap();
        let workspace_root = temp_dir.path().join("workspace");
        fs::create_dir_all(&workspace_root).unwrap();
        let runtime_state_root = temp_dir.path().join("runtime");
        fs::create_dir_all(&runtime_state_root).unwrap();
        let upstream = UpstreamConfig {
            base_url: "https://example.com".to_string(),
            model: "demo".to_string(),
            api_kind: UpstreamApiKind::Responses,
            auth_kind: UpstreamAuthKind::ApiKey,
            supports_vision_input: false,
            supports_pdf_input: true,
            supports_audio_input: false,
            api_key: None,
            api_key_env: "OPENAI_API_KEY".to_string(),
            chat_completions_path: "/responses".to_string(),
            codex_home: None,
            codex_auth: None,
            auth_credentials_store_mode: AuthCredentialsStoreMode::Auto,
            timeout_seconds: 30.0,
            retry_mode: Default::default(),
            context_window_tokens: 200_000,
            cache_control: None,
            prompt_cache_retention: None,
            prompt_cache_key: None,
            reasoning: None,
            headers: serde_json::Map::new(),
            native_web_search: None,
            external_web_search: None,
            native_image_input: false,
            native_pdf_input: true,
            native_audio_input: false,
            native_image_generation: false,
            token_estimation: None,
        };
        let registry = build_tool_registry_with_cancel(
            &[],
            &workspace_root,
            &runtime_state_root,
            &upstream,
            None,
            None,
            None,
            None,
            &Vec::<PathBuf>::new(),
            &[],
            &[],
            None,
        )
        .unwrap();

        assert!(registry.contains_key("pdf_load"));
        assert!(!registry.contains_key("pdf_query"));
    }

    #[test]
    fn native_audio_input_registers_audio_load_without_external_helper() {
        let temp_dir = TempDir::new().unwrap();
        let workspace_root = temp_dir.path().join("workspace");
        fs::create_dir_all(&workspace_root).unwrap();
        let runtime_state_root = temp_dir.path().join("runtime");
        fs::create_dir_all(&runtime_state_root).unwrap();
        let upstream = UpstreamConfig {
            base_url: "https://example.com".to_string(),
            model: "demo".to_string(),
            api_kind: UpstreamApiKind::Responses,
            auth_kind: UpstreamAuthKind::ApiKey,
            supports_vision_input: false,
            supports_pdf_input: false,
            supports_audio_input: true,
            api_key: None,
            api_key_env: "OPENAI_API_KEY".to_string(),
            chat_completions_path: "/responses".to_string(),
            codex_home: None,
            codex_auth: None,
            auth_credentials_store_mode: AuthCredentialsStoreMode::Auto,
            timeout_seconds: 30.0,
            retry_mode: Default::default(),
            context_window_tokens: 200_000,
            cache_control: None,
            prompt_cache_retention: None,
            prompt_cache_key: None,
            reasoning: None,
            headers: serde_json::Map::new(),
            native_web_search: None,
            external_web_search: None,
            native_image_input: false,
            native_pdf_input: false,
            native_audio_input: true,
            native_image_generation: false,
            token_estimation: None,
        };
        let registry = build_tool_registry_with_cancel(
            &[],
            &workspace_root,
            &runtime_state_root,
            &upstream,
            None,
            None,
            None,
            None,
            &Vec::<PathBuf>::new(),
            &[],
            &[],
            None,
        )
        .unwrap();

        assert!(registry.contains_key("audio_load"));
        assert!(!registry.contains_key("audio_transcribe"));
    }

    #[test]
    fn active_runtime_state_summary_lists_running_execs_and_downloads() {
        let temp_dir = TempDir::new().unwrap();
        let runtime_state_root = temp_dir.path();
        let processes_dir = runtime_state_root.join("agent_frame").join("processes");
        let downloads_dir = runtime_state_root
            .join("agent_frame")
            .join("file_downloads");
        let subagents_dir = runtime_state_root.join("agent_frame").join("subagents");
        fs::create_dir_all(&processes_dir).unwrap();
        fs::create_dir_all(&downloads_dir).unwrap();
        fs::create_dir_all(&subagents_dir).unwrap();

        let exec_status_path = processes_dir.join("exec-1.status.json");
        fs::write(
            &exec_status_path,
            serde_json::to_vec_pretty(&json!({
                "exec_id": "exec-1",
                "pid": std::process::id(),
                "command": "sleep 10",
                "cwd": "/tmp/demo",
                "running": true,
                "completed": false,
                "returncode": json!(null),
                "stdin_closed": false,
                "failed": false,
                "error": json!(null),
            }))
            .unwrap(),
        )
        .unwrap();
        let exec_metadata = ProcessMetadata {
            exec_id: "exec-1".to_string(),
            worker_pid: std::process::id(),
            tty: false,
            remote: "local".to_string(),
            command: "sleep 10".to_string(),
            cwd: "/tmp/demo".to_string(),
            stdout_path: processes_dir.join("exec-1.stdout").display().to_string(),
            stderr_path: processes_dir.join("exec-1.stderr").display().to_string(),
            status_path: exec_status_path.display().to_string(),
            worker_exit_code_path: processes_dir
                .join("exec-1.worker.exit")
                .display()
                .to_string(),
            requests_dir: processes_dir.join("exec-1.requests").display().to_string(),
        };
        fs::write(
            process_meta_path(&processes_dir, "exec-1"),
            serde_json::to_vec_pretty(&exec_metadata).unwrap(),
        )
        .unwrap();
        fs::write(
            processes_dir.join("exec-1.stdout"),
            b"hello\nnon-utf8:\x9f\nstill visible\n",
        )
        .unwrap();
        fs::write(processes_dir.join("exec-1.stderr"), b"").unwrap();

        let finished_status_path = processes_dir.join("exec-finished.status.json");
        fs::write(
            &finished_status_path,
            serde_json::to_vec_pretty(&json!({
                "exec_id": "exec-finished",
                "pid": std::process::id(),
                "command": "echo done",
                "cwd": "/tmp/finished",
                "running": false,
                "completed": true,
                "returncode": 0,
                "stdin_closed": true,
                "failed": false,
                "error": json!(null),
            }))
            .unwrap(),
        )
        .unwrap();
        let finished_metadata = ProcessMetadata {
            exec_id: "exec-finished".to_string(),
            worker_pid: std::process::id(),
            tty: false,
            remote: "local".to_string(),
            command: "echo done".to_string(),
            cwd: "/tmp/finished".to_string(),
            stdout_path: processes_dir
                .join("exec-finished.stdout")
                .display()
                .to_string(),
            stderr_path: processes_dir
                .join("exec-finished.stderr")
                .display()
                .to_string(),
            status_path: finished_status_path.display().to_string(),
            worker_exit_code_path: processes_dir
                .join("exec-finished.worker.exit")
                .display()
                .to_string(),
            requests_dir: processes_dir
                .join("exec-finished.requests")
                .display()
                .to_string(),
        };
        fs::write(
            process_meta_path(&processes_dir, "exec-finished"),
            serde_json::to_vec_pretty(&finished_metadata).unwrap(),
        )
        .unwrap();

        let download_status_path = downloads_dir.join("download-1.status.json");
        let download_exit_path = downloads_dir.join("download-1.exit");
        fs::write(
            &download_status_path,
            serde_json::to_vec_pretty(&json!({
                "download_id": "download-1",
                "url": "https://example.com/file.bin",
                "path": "/tmp/file.bin",
                "running": true,
                "completed": false,
                "cancelled": false,
                "bytes_downloaded": 128
            }))
            .unwrap(),
        )
        .unwrap();
        write_background_task_metadata(
            &downloads_dir,
            &BackgroundTaskMetadata {
                task_id: "download-1".to_string(),
                pid: std::process::id(),
                label: "file-download".to_string(),
                status_path: download_status_path.display().to_string(),
                stdout_path: downloads_dir
                    .join("download-1.stdout")
                    .display()
                    .to_string(),
                stderr_path: downloads_dir
                    .join("download-1.stderr")
                    .display()
                    .to_string(),
                exit_code_path: download_exit_path.display().to_string(),
            },
        )
        .unwrap();

        fs::write(
            subagents_dir.join("subagent-1.json"),
            serde_json::to_vec_pretty(&json!({
                "id": "subagent-1",
                "description": "inspect logs and summarize the issue",
                "model_key": "main",
                "state": "ready"
            }))
            .unwrap(),
        )
        .unwrap();

        let summary = active_runtime_state_summary(runtime_state_root)
            .unwrap()
            .expect("expected active runtime summary");
        assert!(summary.contains("Active exec processes:"));
        assert!(summary.contains("exec_id=`exec-1`"));
        assert!(!summary.contains("exec-finished"));
        assert!(summary.contains("Active file downloads:"));
        assert!(summary.contains("download_id=`download-1`"));
        assert!(summary.contains("Active subagents:"));
        assert!(summary.contains("subagent-1"));
    }

    #[test]
    fn active_runtime_state_summary_ignores_legacy_exec_metadata_files() {
        let temp_dir = TempDir::new().unwrap();
        let runtime_state_root = temp_dir.path();
        let processes_dir = runtime_state_root.join("agent_frame").join("processes");
        fs::create_dir_all(&processes_dir).unwrap();

        let legacy_exec_id = "legacy-exec";
        fs::write(
            process_meta_path(&processes_dir, legacy_exec_id),
            serde_json::to_vec_pretty(&json!({
                "exec_id": legacy_exec_id,
                "pid": 12345,
                "command": "sleep 10",
                "cwd": "/tmp/legacy",
                "stdout_path": processes_dir.join(format!("{legacy_exec_id}.stdout")).display().to_string(),
                "stderr_path": processes_dir.join(format!("{legacy_exec_id}.stderr")).display().to_string(),
                "exit_code_path": processes_dir.join(format!("{legacy_exec_id}.exit")).display().to_string(),
            }))
            .unwrap(),
        )
        .unwrap();

        let current_exec_id = "current-exec";
        let current_status_path = processes_dir.join(format!("{current_exec_id}.status.json"));
        fs::write(
            &current_status_path,
            serde_json::to_vec_pretty(&json!({
                "exec_id": current_exec_id,
                "pid": std::process::id(),
                "command": "sleep 10",
                "cwd": "/tmp/current",
                "running": true,
                "completed": false,
                "returncode": json!(null),
                "stdin_closed": false,
                "failed": false,
                "error": json!(null),
            }))
            .unwrap(),
        )
        .unwrap();
        let current_metadata = ProcessMetadata {
            exec_id: current_exec_id.to_string(),
            worker_pid: std::process::id(),
            tty: false,
            remote: "local".to_string(),
            command: "sleep 10".to_string(),
            cwd: "/tmp/current".to_string(),
            stdout_path: processes_dir
                .join(format!("{current_exec_id}.stdout"))
                .display()
                .to_string(),
            stderr_path: processes_dir
                .join(format!("{current_exec_id}.stderr"))
                .display()
                .to_string(),
            status_path: current_status_path.display().to_string(),
            worker_exit_code_path: processes_dir
                .join(format!("{current_exec_id}.worker.exit"))
                .display()
                .to_string(),
            requests_dir: processes_dir
                .join(format!("{current_exec_id}.requests"))
                .display()
                .to_string(),
        };
        fs::write(
            process_meta_path(&processes_dir, current_exec_id),
            serde_json::to_vec_pretty(&current_metadata).unwrap(),
        )
        .unwrap();

        let summary = active_runtime_state_summary(runtime_state_root)
            .unwrap()
            .expect("expected active runtime summary");
        assert!(summary.contains("current-exec"));
        assert!(!summary.contains("legacy-exec"));
    }

    #[test]
    fn terminate_runtime_state_tasks_kills_running_tasks() {
        let temp_dir = TempDir::new().unwrap();
        let runtime_state_root = temp_dir.path();
        let processes_dir = runtime_state_root.join("agent_frame").join("processes");
        let downloads_dir = runtime_state_root
            .join("agent_frame")
            .join("file_downloads");
        let images_dir = runtime_state_root.join("agent_frame").join("image_tasks");
        fs::create_dir_all(&processes_dir).unwrap();
        fs::create_dir_all(&downloads_dir).unwrap();
        fs::create_dir_all(&images_dir).unwrap();

        let mut exec_child = Command::new("sh")
            .arg("-c")
            .arg("sleep 30")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let exec_status_path = processes_dir.join("exec-cleanup.status.json");
        fs::write(
            &exec_status_path,
            serde_json::to_vec_pretty(&json!({
                "exec_id": "exec-cleanup",
                "pid": exec_child.id(),
                "command": "sleep 30",
                "cwd": "/tmp",
                "running": true,
                "completed": false,
                "returncode": json!(null),
                "stdin_closed": false,
                "failed": false,
                "error": json!(null),
            }))
            .unwrap(),
        )
        .unwrap();
        let exec_metadata = ProcessMetadata {
            exec_id: "exec-cleanup".to_string(),
            worker_pid: exec_child.id(),
            tty: false,
            remote: "local".to_string(),
            command: "sleep 30".to_string(),
            cwd: "/tmp".to_string(),
            stdout_path: processes_dir
                .join("exec-cleanup.stdout")
                .display()
                .to_string(),
            stderr_path: processes_dir
                .join("exec-cleanup.stderr")
                .display()
                .to_string(),
            status_path: exec_status_path.display().to_string(),
            worker_exit_code_path: processes_dir
                .join("exec-cleanup.worker.exit")
                .display()
                .to_string(),
            requests_dir: processes_dir
                .join("exec-cleanup.requests")
                .display()
                .to_string(),
        };
        fs::write(
            process_meta_path(&processes_dir, "exec-cleanup"),
            serde_json::to_vec_pretty(&exec_metadata).unwrap(),
        )
        .unwrap();

        let mut download_child = Command::new("sh")
            .arg("-c")
            .arg("sleep 30")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let download_status_path = downloads_dir.join("download-cleanup.status.json");
        fs::write(
            &download_status_path,
            serde_json::to_vec_pretty(&json!({
                "download_id": "download-cleanup",
                "url": "https://example.com/archive.tar",
                "path": "/tmp/archive.tar",
                "running": true,
                "completed": false,
                "cancelled": false,
                "failed": false,
                "bytes_downloaded": 64
            }))
            .unwrap(),
        )
        .unwrap();
        write_background_task_metadata(
            &downloads_dir,
            &BackgroundTaskMetadata {
                task_id: "download-cleanup".to_string(),
                pid: download_child.id(),
                label: "file-download".to_string(),
                status_path: download_status_path.display().to_string(),
                stdout_path: downloads_dir
                    .join("download-cleanup.stdout")
                    .display()
                    .to_string(),
                stderr_path: downloads_dir
                    .join("download-cleanup.stderr")
                    .display()
                    .to_string(),
                exit_code_path: downloads_dir
                    .join("download-cleanup.exit")
                    .display()
                    .to_string(),
            },
        )
        .unwrap();

        let mut image_child = Command::new("sh")
            .arg("-c")
            .arg("sleep 30")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let image_status_path = images_dir.join("image-cleanup.status.json");
        fs::write(
            &image_status_path,
            serde_json::to_vec_pretty(&json!({
                "image_id": "image-cleanup",
                "path": "/tmp/demo.png",
                "question": "what is in the image?",
                "running": true,
                "completed": false,
                "cancelled": false,
                "failed": false
            }))
            .unwrap(),
        )
        .unwrap();
        write_background_task_metadata(
            &images_dir,
            &BackgroundTaskMetadata {
                task_id: "image-cleanup".to_string(),
                pid: image_child.id(),
                label: "image".to_string(),
                status_path: image_status_path.display().to_string(),
                stdout_path: images_dir
                    .join("image-cleanup.stdout")
                    .display()
                    .to_string(),
                stderr_path: images_dir
                    .join("image-cleanup.stderr")
                    .display()
                    .to_string(),
                exit_code_path: images_dir.join("image-cleanup.exit").display().to_string(),
            },
        )
        .unwrap();

        let report = terminate_runtime_state_tasks(runtime_state_root).unwrap();
        assert_eq!(report.exec_processes_killed, 1);
        assert_eq!(report.file_downloads_cancelled, 1);
        assert_eq!(report.image_tasks_cancelled, 1);

        let download_snapshot: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(download_status_path).unwrap()).unwrap();
        assert_eq!(download_snapshot["cancelled"], json!(true));
        assert_eq!(download_snapshot["reason"], json!("session_destroyed"));
        let image_snapshot: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(image_status_path).unwrap()).unwrap();
        assert_eq!(image_snapshot["cancelled"], json!(true));
        assert_eq!(image_snapshot["reason"], json!("session_destroyed"));

        let _ = exec_child.wait();
        let _ = download_child.wait();
        let _ = image_child.wait();
        thread::sleep(Duration::from_millis(50));
        assert!(!process_is_running(exec_metadata.worker_pid));
        assert!(!process_is_running(download_child.id()));
        assert!(!process_is_running(image_child.id()));
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
