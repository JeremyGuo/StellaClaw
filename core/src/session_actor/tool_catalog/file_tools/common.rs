use std::{
    fs,
    path::{Path, PathBuf},
    time::SystemTime,
};

use globset::{Glob, GlobMatcher};
use serde_json::{json, Map, Value};

use crate::session_actor::tool_runtime::{run_remote_json, LocalToolError};

pub(super) const SEARCH_MAX_RESULTS: usize = 100;
pub(super) const LS_MAX_ENTRIES: usize = 1000;
pub(super) const COMMON_LS_SKIP_DIRS: &[&str] = &[
    "__pycache__",
    "node_modules",
    "target",
    "dist",
    "build",
    "coverage",
    "venv",
];

const SLOW_MOUNT_FS_TYPES: &[&str] = &[
    "fuse.sshfs",
    "sshfs",
    "nfs",
    "nfs4",
    "cifs",
    "smb3",
    "9p",
    "davfs",
    "fuse.rclone",
    "fuse.s3fs",
    "fuse.gcsfuse",
];

#[derive(Debug, Default)]
pub(super) struct SlowMountTable {
    mount_points: Vec<PathBuf>,
}

impl SlowMountTable {
    pub(super) fn load() -> Self {
        let Ok(raw) = fs::read_to_string("/proc/self/mountinfo") else {
            return Self::default();
        };
        let mut mount_points = Vec::new();
        for line in raw.lines() {
            let Some((before_sep, after_sep)) = line.split_once(" - ") else {
                continue;
            };
            let mut before_fields = before_sep.split_whitespace();
            let mount_point = before_fields.nth(4);
            let fs_type = after_sep.split_whitespace().next();
            let (Some(mount_point), Some(fs_type)) = (mount_point, fs_type) else {
                continue;
            };
            if slow_mount_fs_type(fs_type) {
                mount_points.push(PathBuf::from(decode_mountinfo_path(mount_point)));
            }
        }
        mount_points
            .sort_by(|left, right| right.components().count().cmp(&left.components().count()));
        Self { mount_points }
    }

    pub(super) fn contains(&self, path: &Path) -> bool {
        let path = if path.is_absolute() {
            path.to_path_buf()
        } else {
            std::env::current_dir()
                .map(|cwd| cwd.join(path))
                .unwrap_or_else(|_| path.to_path_buf())
        };
        self.mount_points
            .iter()
            .any(|mount_point| path.starts_with(mount_point))
    }
}

pub(super) struct SearchMatch {
    pub path: String,
    pub mtime_ms: u128,
}

pub(super) struct LsEntry {
    pub path: String,
    pub is_dir: bool,
}

pub(super) fn build_glob_matcher(pattern: &str) -> Result<GlobMatcher, LocalToolError> {
    Glob::new(pattern)
        .map(|glob| glob.compile_matcher())
        .map_err(|error| LocalToolError::InvalidArguments(format!("invalid glob pattern: {error}")))
}

pub(super) fn collect_walk_paths(
    base: &Path,
    files_only: bool,
) -> Result<Vec<PathBuf>, LocalToolError> {
    let mut paths = Vec::new();
    let slow_mounts = SlowMountTable::load();
    collect_walk_paths_inner(base, files_only, &slow_mounts, &mut paths)?;
    Ok(paths)
}

fn collect_walk_paths_inner(
    path: &Path,
    files_only: bool,
    slow_mounts: &SlowMountTable,
    paths: &mut Vec<PathBuf>,
) -> Result<(), LocalToolError> {
    if slow_mounts.contains(path) {
        return Ok(());
    }
    let metadata = fs::metadata(path).map_err(|error| {
        LocalToolError::Io(format!("failed to stat {}: {error}", path.display()))
    })?;
    if metadata.is_file() {
        paths.push(path.to_path_buf());
        return Ok(());
    }
    if !metadata.is_dir() {
        return Ok(());
    }
    if !files_only {
        paths.push(path.to_path_buf());
    }
    for entry in fs::read_dir(path).map_err(|error| {
        LocalToolError::Io(format!("failed to read {}: {error}", path.display()))
    })? {
        let entry = entry
            .map_err(|error| LocalToolError::Io(format!("failed to read dir entry: {error}")))?;
        let entry_path = entry.path();
        if entry
            .file_type()
            .map(|kind| kind.is_symlink())
            .unwrap_or(false)
        {
            continue;
        }
        collect_walk_paths_inner(&entry_path, files_only, slow_mounts, paths)?;
    }
    Ok(())
}

fn slow_mount_fs_type(fs_type: &str) -> bool {
    SLOW_MOUNT_FS_TYPES.contains(&fs_type)
}

fn decode_mountinfo_path(value: &str) -> String {
    let mut decoded = String::with_capacity(value.len());
    let mut chars = value.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch == '\\' {
            let digits = (0..3).filter_map(|_| chars.next()).collect::<String>();
            if digits.len() == 3 {
                if let Ok(byte) = u8::from_str_radix(&digits, 8) {
                    decoded.push(byte as char);
                    continue;
                }
            }
            decoded.push('\\');
            decoded.push_str(&digits);
        } else {
            decoded.push(ch);
        }
    }
    decoded
}

pub(super) fn relative_display_path(path: &Path, base: &Path) -> String {
    path.strip_prefix(base)
        .unwrap_or(path)
        .to_string_lossy()
        .replace(std::path::MAIN_SEPARATOR, "/")
}

pub(super) fn file_mtime_ms(path: &Path) -> u128 {
    fs::metadata(path)
        .and_then(|metadata| metadata.modified())
        .ok()
        .and_then(|mtime| mtime.duration_since(SystemTime::UNIX_EPOCH).ok())
        .map(|duration| duration.as_millis())
        .unwrap_or_default()
}

pub(super) fn sort_search_matches(matches: &mut [SearchMatch]) {
    matches.sort_by(|left, right| {
        right
            .mtime_ms
            .cmp(&left.mtime_ms)
            .then_with(|| left.path.cmp(&right.path))
    });
}

pub(super) fn remote_file_tool(
    operation: &str,
    arguments: &Map<String, Value>,
    host: &str,
    cwd: Option<&str>,
) -> Result<Value, LocalToolError> {
    let payload = json!({
        "operation": operation,
        "workspace_root": ".",
        "arguments": arguments,
    });
    let script = format!(
        "python3 - <<'PY'\n{}\nPY\n",
        remote_file_tool_script(&payload)
    );
    run_remote_json(host, cwd, &script)
}

fn remote_file_tool_script(payload: &Value) -> String {
    let payload = serde_json::to_string(payload).unwrap_or_else(|_| "{}".to_string());
    format!(
        r#"
import fnmatch
import json
import os
import re
import signal
import sys
import time

payload = json.loads({payload:?})

SEARCH_MAX_RESULTS = 100
GREP_MAX_CONTEXT_LINES = 10
GREP_DEFAULT_TOTAL_MAX_MATCHES = SEARCH_MAX_RESULTS
GREP_MAX_TOTAL_MATCHES = 1000
GREP_MAX_MATCHES_PER_FILE = 1000
GREP_MAX_LINE_CHARS = 600
LS_MAX_ENTRIES = 1000
COMMON_LS_SKIP_DIRS = {{"__pycache__", "node_modules", "target", "dist", "build", "coverage", "venv"}}
SLOW_MOUNT_FS_TYPES = {{"fuse.sshfs", "sshfs", "nfs", "nfs4", "cifs", "smb3", "9p", "davfs", "fuse.rclone", "fuse.s3fs", "fuse.gcsfuse"}}
OPERATION_TIMEOUT_SECONDS = {{"ls": 20, "grep": 45}}
OPERATION_ATTEMPTS = {{"ls": 2, "grep": 2}}

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
    attempts = max(1, int(attempts))
    for attempt in range(1, attempts + 1):
        try:
            return run_with_timeout(label, timeout_seconds, fn)
        except (OperationTimeout, OSError) as error:
            last_error = error
            if attempt >= attempts:
                break
            time.sleep(0.2 * attempt)
    raise RuntimeError(f"{{label}} failed after {{attempts}} attempt(s): {{last_error}}")

def resolve(path, root):
    if os.path.isabs(path):
        return os.path.abspath(path)
    return os.path.abspath(os.path.join(root, path))

def file_mtime_ms(path):
    try:
        return int(os.path.getmtime(path) * 1000)
    except OSError:
        return 0

def decode_mountinfo_path(value):
    def repl(match):
        try:
            return chr(int(match.group(1), 8))
        except ValueError:
            return match.group(0)
    return re.sub(r"\\([0-7]{{3}})", repl, value)

def load_slow_mounts():
    mounts = []
    try:
        with open("/proc/self/mountinfo", "r", encoding="utf-8") as handle:
            lines = handle.readlines()
    except OSError:
        return mounts
    for line in lines:
        if " - " not in line:
            continue
        before, after = line.split(" - ", 1)
        before_fields = before.split()
        after_fields = after.split()
        if len(before_fields) < 5 or not after_fields:
            continue
        mount_point = decode_mountinfo_path(before_fields[4])
        fs_type = after_fields[0]
        if fs_type in SLOW_MOUNT_FS_TYPES:
            mounts.append(os.path.abspath(mount_point))
    mounts.sort(key=lambda path: path.count(os.sep), reverse=True)
    return mounts

SLOW_MOUNTS = load_slow_mounts()

def is_slow_mount_path(path):
    try:
        absolute = os.path.abspath(path)
        for mount in SLOW_MOUNTS:
            if os.path.commonpath([absolute, mount]) == mount:
                return True
    except (OSError, ValueError):
        return False
    return False

def walk_files(base):
    if is_slow_mount_path(base):
        return []
    paths = []
    for root, dirs, files in os.walk(base, followlinks=False):
        dirs[:] = [
            name for name in dirs
            if not is_slow_mount_path(os.path.join(root, name))
        ]
        for name in files:
            path = os.path.join(root, name)
            if is_slow_mount_path(path):
                continue
            if os.path.isfile(path):
                paths.append(path)
    return paths

def int_arg(args, key, default, min_value, max_value):
    value = args.get(key, default)
    if isinstance(value, bool) or not isinstance(value, int):
        raise ValueError(f"{{key}} must be an integer")
    if value < min_value:
        raise ValueError(f"{{key}} must be greater than or equal to {{min_value}}")
    return min(value, max_value)

def bool_arg(args, key, default=False):
    value = args.get(key, default)
    if not isinstance(value, bool):
        raise ValueError(f"{{key}} must be a boolean")
    return value

def bounded_line(line):
    if len(line) <= GREP_MAX_LINE_CHARS:
        return line
    return line[:GREP_MAX_LINE_CHARS] + "... [truncated]"

def context_entries(lines, start, end):
    return [
        {{"line": index + 1, "text": bounded_line(lines[index])}}
        for index in range(start, end)
    ]

def handle_grep(args, workspace_root):
    pattern = args.get("pattern")
    if not isinstance(pattern, str):
        raise ValueError("missing required argument: pattern")
    base = resolve(args.get("path", "."), workspace_root)
    if not os.path.exists(base):
        raise ValueError(f"{{base}} does not exist")
    include = args.get("include")
    exclude = args.get("exclude")
    context_lines = int_arg(args, "context_lines", 0, 0, GREP_MAX_CONTEXT_LINES)
    max_matches_per_file = int_arg(args, "max_matches_per_file", GREP_MAX_MATCHES_PER_FILE, 1, GREP_MAX_MATCHES_PER_FILE)
    total_max_matches = int_arg(args, "total_max_matches", GREP_DEFAULT_TOTAL_MAX_MATCHES, 1, GREP_MAX_TOTAL_MATCHES)
    names_only = bool_arg(args, "names_only", False)
    regex = re.compile(pattern)
    file_matches = []
    result_matches = []
    matched_files = set()
    total_matches = 0
    for path in walk_files(base):
        rel = os.path.relpath(path, base).replace(os.sep, "/")
        if include and not fnmatch.fnmatch(rel, include):
            continue
        if exclude and fnmatch.fnmatch(rel, exclude):
            continue
        try:
            with open(path, "r", encoding="utf-8") as handle:
                text = handle.read()
        except (UnicodeDecodeError, OSError):
            continue
        lines = text.splitlines()
        file_match_count = 0
        for line_index, line in enumerate(lines):
            match = regex.search(line)
            if not match:
                continue
            if file_match_count == 0:
                matched_files.add(path)
                file_matches.append({{"path": path, "mtime_ms": file_mtime_ms(path)}})
            file_match_count += 1
            total_matches += 1
            if file_match_count > max_matches_per_file or len(result_matches) >= total_max_matches:
                continue
            if names_only:
                continue
            before_start = max(0, line_index - context_lines)
            after_end = min(len(lines), line_index + 1 + context_lines)
            result_matches.append({{
                "file": path,
                "line": line_index + 1,
                "column": match.start() + 1,
                "text": bounded_line(line),
                "before": context_entries(lines, before_start, line_index),
                "after": context_entries(lines, line_index + 1, after_end),
            }})
    file_matches.sort(key=lambda item: (-item["mtime_ms"], item["path"]))
    result = {{
        "pattern": pattern,
        "path": base,
        "context_lines": context_lines,
        "num_files": len(matched_files),
        "num_matches": total_matches,
        "filenames": [item["path"] for item in file_matches[:SEARCH_MAX_RESULTS]],
    }}
    if include:
        result["include"] = include
    if exclude:
        result["exclude"] = exclude
    if names_only:
        result["names_only"] = True
    else:
        result["matches"] = result_matches
    if (not names_only and total_matches > len(result_matches)) or len(file_matches) > SEARCH_MAX_RESULTS:
        result["truncated"] = True
    return result

def should_skip_ls(name, is_dir):
    if name.startswith("."):
        return True
    return is_dir and name in COMMON_LS_SKIP_DIRS

def handle_ls(args, workspace_root):
    path_arg = args.get("path", ".")
    if not isinstance(path_arg, str):
        raise ValueError("argument path must be a string")
    if path_arg == "":
        path_arg = "."
    shadow_roots = args.get("shadow_roots", [])
    if not isinstance(shadow_roots, list):
        shadow_roots = []
    shadow_roots = {{str(name) for name in shadow_roots}}
    base = resolve(path_arg, workspace_root)
    if not os.path.exists(base):
        raise ValueError(f"{{base}} does not exist")
    if not os.path.isdir(base):
        raise ValueError(f"{{base}} is not a directory")
    entries = []
    truncated = False
    if is_slow_mount_path(base):
        lines = [f"num_entries: 0", "", f"- {{base.rstrip(os.sep) + os.sep}}"]
        return "\n".join(lines)
    for root, dirs, files in os.walk(base, followlinks=False):
        at_base = os.path.abspath(root) == os.path.abspath(base)
        dirs[:] = [
            name for name in dirs
            if not (at_base and name in shadow_roots)
            and not should_skip_ls(name, True)
            and not is_slow_mount_path(os.path.join(root, name))
        ]
        for name in dirs:
            if len(entries) >= LS_MAX_ENTRIES:
                truncated = True
                break
            path = os.path.join(root, name)
            entries.append((os.path.relpath(path, base).replace(os.sep, "/"), True))
        if truncated:
            break
        for name in files:
            if at_base and name in shadow_roots:
                continue
            if should_skip_ls(name, False):
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
        lines.append(f"num_entries: >{{LS_MAX_ENTRIES}}")
        lines.append("truncated: true")
        lines.append(f"There are more than {{LS_MAX_ENTRIES}} files and directories under {{base}}. Use ls with a more specific path, grep for content search, or narrow shell rg/ripgrep commands such as rg --files for path pattern discovery. The first {{LS_MAX_ENTRIES}} files and directories are included below:")
        lines.append("")
    else:
        lines.append(f"num_entries: {{len(entries)}}")
        lines.append("")
    display_base = base.replace(os.sep, "/")
    if not display_base.endswith("/"):
        display_base += "/"
    lines.append(f"- {{display_base}}")
    for rel_path, is_dir in entries:
        parts = [part for part in rel_path.split("/") if part]
        if not parts:
            continue
        indent = "  " * len(parts)
        suffix = "/" if is_dir else ""
        lines.append(f"{{indent}}- {{parts[-1]}}{{suffix}}")
    return "\n".join(lines)

handlers = {{
    "grep": handle_grep,
    "ls": handle_ls,
}}

try:
    operation = payload["operation"]
    result = run_with_retry(
        operation,
        OPERATION_ATTEMPTS.get(operation, 1),
        OPERATION_TIMEOUT_SECONDS.get(operation, 30),
        lambda: handlers[operation](payload.get("arguments", {{}}), payload.get("workspace_root", ".")),
    )
    print(json.dumps(result, ensure_ascii=False))
except Exception as error:
    print(str(error), file=sys.stderr)
    raise SystemExit(1)
"#
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        io::Write,
        process::{Command, Stdio},
    };

    #[test]
    fn remote_file_tool_script_is_valid_python() {
        let script = remote_file_tool_script(&json!({
            "operation": "ls",
            "workspace_root": ".",
            "arguments": {"path": "."},
        }));
        assert_python_compiles(&script);
    }

    fn assert_python_compiles(script: &str) {
        let mut child = match Command::new("python3")
            .arg("-c")
            .arg("import sys; compile(sys.stdin.read(), '<remote-file-tool>', 'exec')")
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
