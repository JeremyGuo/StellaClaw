use std::{
    fs,
    path::{Component, Path, PathBuf},
    process::{Command, Stdio},
    thread,
    time::{Duration, Instant},
};

use serde::Deserialize;
use serde_json::{json, Map, Value};

use crate::session_actor::tool_runtime::{
    f64_arg_with_default, shell_quote, string_arg, LocalToolError, ToolExecutionContext,
};

#[derive(Debug, Clone, Copy)]
enum CopyMethod {
    TarGzip,
    Scp,
}

impl CopyMethod {
    fn as_str(self) -> &'static str {
        match self {
            CopyMethod::TarGzip => "tar_gzip",
            CopyMethod::Scp => "scp",
        }
    }
}

#[derive(Debug, Deserialize)]
struct RemoteCapabilities {
    tar_gzip: bool,
    python3: bool,
}

#[derive(Debug, Deserialize)]
struct RemotePathInfo {
    kind: String,
}

pub(super) fn execute_visibility_tool(
    tool_name: &str,
    arguments: &Map<String, Value>,
    context: &ToolExecutionContext<'_>,
) -> Result<Option<Value>, LocalToolError> {
    let super::super::ToolRemoteMode::FixedSsh { host, cwd } = context.remote_mode else {
        return Ok(None);
    };
    let result = match tool_name {
        "shell_make_visible" => {
            shell_make_visible(arguments, context.workspace_root, host, cwd.as_deref())?
        }
        "attachment_make_visible" => {
            attachment_make_visible(arguments, context.workspace_root, host, cwd.as_deref())?
        }
        _ => return Ok(None),
    };
    Ok(Some(result))
}

fn shell_make_visible(
    arguments: &Map<String, Value>,
    workspace_root: &Path,
    host: &str,
    cwd: Option<&str>,
) -> Result<Value, LocalToolError> {
    let relative = relative_path_arg(arguments)?;
    let timeout = timeout_duration(arguments)?;
    let rel = display_relative(&relative);
    let source = workspace_root.join(&relative);
    let kind = local_source_kind(&source, &rel)?;
    ensure_local_path_has_no_symlink(&source, &rel)?;

    let remote_caps = remote_capabilities(host, cwd, timeout)?;
    if !remote_caps.python3 {
        return Err(LocalToolError::Remote(
            "remote host must provide python3 to validate workspace path safety".to_string(),
        ));
    }
    ensure_remote_write_path_safe(host, cwd, &rel, timeout)?;

    let method = choose_copy_method(remote_caps.tar_gzip)?;
    match method {
        CopyMethod::TarGzip => {
            copy_local_to_remote_tar(workspace_root, host, cwd, &relative, timeout)?
        }
        CopyMethod::Scp => copy_local_to_remote_scp(&source, host, cwd, &relative, timeout)?,
    }

    Ok(json!({
        "ok": true,
        "direction": "local_to_remote",
        "kind": kind,
        "copied": true,
        "method": method.as_str(),
    }))
}

fn attachment_make_visible(
    arguments: &Map<String, Value>,
    workspace_root: &Path,
    host: &str,
    cwd: Option<&str>,
) -> Result<Value, LocalToolError> {
    let relative = relative_path_arg(arguments)?;
    let timeout = timeout_duration(arguments)?;
    let rel = display_relative(&relative);
    ensure_local_write_path_safe(workspace_root, &relative)?;

    let remote_caps = remote_capabilities(host, cwd, timeout)?;
    if !remote_caps.python3 {
        return Err(LocalToolError::Remote(
            "remote host must provide python3 to validate workspace path safety".to_string(),
        ));
    }
    let remote_info = remote_source_info(host, cwd, &rel, timeout)?;

    let method = choose_copy_method(remote_caps.tar_gzip)?;
    match method {
        CopyMethod::TarGzip => {
            copy_remote_to_local_tar(workspace_root, host, cwd, &relative, timeout)?
        }
        CopyMethod::Scp => copy_remote_to_local_scp(workspace_root, host, cwd, &relative, timeout)?,
    }

    Ok(json!({
        "ok": true,
        "direction": "remote_to_local",
        "kind": remote_info.kind,
        "copied": true,
        "method": method.as_str(),
    }))
}

fn copy_local_to_remote_tar(
    workspace_root: &Path,
    host: &str,
    cwd: Option<&str>,
    relative: &Path,
    timeout: Duration,
) -> Result<(), LocalToolError> {
    ensure_local_tool("tar")?;
    let rel = display_relative(relative);
    let install_script = remote_install_script(&rel, "tar -xzf - -C \"$tmp\"");
    let remote_command = remote_shell_command(cwd, &install_script);
    let command = format!(
        "tar -C {} -czf - {} | ssh -o BatchMode=yes -o ConnectTimeout=10 -T {} {}",
        shell_quote(&workspace_root.display().to_string()),
        shell_quote(&rel),
        shell_quote(host),
        shell_quote(&remote_command),
    );
    run_shell_command_with_timeout(&command, timeout).map(|_| ())
}

fn copy_remote_to_local_tar(
    workspace_root: &Path,
    host: &str,
    cwd: Option<&str>,
    relative: &Path,
    timeout: Duration,
) -> Result<(), LocalToolError> {
    ensure_local_tool("tar")?;
    let rel = display_relative(relative);
    let tmp_root = local_visibility_tmp_root(workspace_root)?;
    let script = format!("tar -czf - {}", shell_quote(&rel));
    let remote_command = remote_shell_command(cwd, &script);
    let command = format!(
        "ssh -o BatchMode=yes -o ConnectTimeout=10 -T {} {} | tar -xzf - -C {}",
        shell_quote(host),
        shell_quote(&remote_command),
        shell_quote(&tmp_root.display().to_string()),
    );
    if let Err(error) = run_shell_command_with_timeout(&command, timeout) {
        let _ = fs::remove_dir_all(&tmp_root);
        return Err(error);
    }
    let staged = tmp_root.join(relative);
    let result = install_local_staged_path(workspace_root, relative, &staged);
    let _ = fs::remove_dir_all(&tmp_root);
    result
}

fn copy_local_to_remote_scp(
    source: &Path,
    host: &str,
    cwd: Option<&str>,
    relative: &Path,
    timeout: Duration,
) -> Result<(), LocalToolError> {
    ensure_local_tool("scp")?;
    let rel = display_relative(relative);
    let remote_tmp = create_remote_tmp_dir(host, cwd, relative, timeout)?;
    let command = format!(
        "scp -r -p -o BatchMode=yes -o ConnectTimeout=10 {} {}:{}",
        shell_quote(&source.display().to_string()),
        shell_quote(host),
        shell_quote(&format!("{remote_tmp}/")),
    );
    if let Err(error) = run_shell_command_with_timeout(&command, timeout) {
        let _ = cleanup_remote_path(host, cwd, &remote_tmp, timeout);
        return Err(error);
    }

    let staged = format!(
        "{}/{}",
        remote_tmp.trim_end_matches('/'),
        shell_path_basename(relative)
    );
    let install = remote_install_existing_path_script(&rel, &staged);
    let result = run_ssh_capture(host, cwd, &install, timeout).map(|_| ());
    let _ = cleanup_remote_path(host, cwd, &remote_tmp, timeout);
    result
}

fn copy_remote_to_local_scp(
    workspace_root: &Path,
    host: &str,
    cwd: Option<&str>,
    relative: &Path,
    timeout: Duration,
) -> Result<(), LocalToolError> {
    ensure_local_tool("scp")?;
    let rel = display_relative(relative);
    let remote_source = remote_absolute_path(host, cwd, &rel, timeout)?;
    let tmp_root = local_visibility_tmp_root(workspace_root)?;
    let command = format!(
        "scp -r -p -o BatchMode=yes -o ConnectTimeout=10 {}:{} {}",
        shell_quote(host),
        shell_quote(&remote_source),
        shell_quote(&tmp_root.display().to_string()),
    );
    if let Err(error) = run_shell_command_with_timeout(&command, timeout) {
        let _ = fs::remove_dir_all(&tmp_root);
        return Err(error);
    }
    let staged = tmp_root.join(shell_path_basename(relative));
    let result = install_local_staged_path(workspace_root, relative, &staged);
    let _ = fs::remove_dir_all(&tmp_root);
    result
}

fn choose_copy_method(remote_tar_gzip: bool) -> Result<CopyMethod, LocalToolError> {
    if has_local_tool("tar") && remote_tar_gzip {
        return Ok(CopyMethod::TarGzip);
    }
    if has_local_tool("scp") {
        return Ok(CopyMethod::Scp);
    }
    Err(LocalToolError::Io(
        "visibility copy requires local tar plus remote tar/gzip, or local scp fallback"
            .to_string(),
    ))
}

fn local_source_kind(path: &Path, rel: &str) -> Result<&'static str, LocalToolError> {
    let metadata = fs::symlink_metadata(path).map_err(|error| {
        LocalToolError::InvalidArguments(format!("local path {rel} is not readable: {error}"))
    })?;
    if metadata.file_type().is_symlink() {
        return Err(LocalToolError::InvalidArguments(format!(
            "visibility copy refuses symlink path: {rel}"
        )));
    }
    if metadata.is_dir() {
        Ok("directory")
    } else if metadata.is_file() {
        Ok("file")
    } else {
        Err(LocalToolError::InvalidArguments(format!(
            "visibility copy only supports regular files and directories: {rel}"
        )))
    }
}

fn ensure_local_path_has_no_symlink(path: &Path, rel: &str) -> Result<(), LocalToolError> {
    let metadata = fs::symlink_metadata(path)
        .map_err(|error| LocalToolError::Io(format!("failed to inspect {rel}: {error}")))?;
    if metadata.file_type().is_symlink() {
        return Err(LocalToolError::InvalidArguments(format!(
            "visibility copy refuses symlink path: {rel}"
        )));
    }
    if metadata.is_dir() {
        for entry in fs::read_dir(path)
            .map_err(|error| LocalToolError::Io(format!("failed to list {rel}: {error}")))?
        {
            let entry = entry
                .map_err(|error| LocalToolError::Io(format!("failed to list {rel}: {error}")))?;
            let child_rel = format!("{rel}/{}", entry.file_name().to_string_lossy());
            ensure_local_path_has_no_symlink(&entry.path(), &child_rel)?;
        }
    }
    Ok(())
}

fn ensure_local_write_path_safe(
    workspace_root: &Path,
    relative: &Path,
) -> Result<(), LocalToolError> {
    let mut current = workspace_root.to_path_buf();
    if let Some(parent) = relative.parent() {
        for component in parent.components() {
            let Component::Normal(part) = component else {
                continue;
            };
            current.push(part);
            match fs::symlink_metadata(&current) {
                Ok(metadata) if metadata.file_type().is_symlink() => {
                    return Err(LocalToolError::InvalidArguments(format!(
                        "visibility copy refuses symlink parent: {}",
                        current.display()
                    )));
                }
                Ok(metadata) if !metadata.is_dir() => {
                    return Err(LocalToolError::InvalidArguments(format!(
                        "visibility copy parent is not a directory: {}",
                        current.display()
                    )));
                }
                Ok(_) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => break,
                Err(error) => {
                    return Err(LocalToolError::Io(format!(
                        "failed to inspect {}: {error}",
                        current.display()
                    )));
                }
            }
        }
    }
    let destination = workspace_root.join(relative);
    if fs::symlink_metadata(&destination)
        .map(|metadata| metadata.file_type().is_symlink())
        .unwrap_or(false)
    {
        return Err(LocalToolError::InvalidArguments(format!(
            "visibility copy refuses to overwrite symlink: {}",
            display_relative(relative)
        )));
    }
    Ok(())
}

fn install_local_staged_path(
    workspace_root: &Path,
    relative: &Path,
    staged: &Path,
) -> Result<(), LocalToolError> {
    if !staged.exists() {
        return Err(LocalToolError::Remote(format!(
            "copied archive did not contain {}",
            display_relative(relative)
        )));
    }
    ensure_local_path_has_no_symlink(staged, &display_relative(relative))?;
    ensure_local_write_path_safe(workspace_root, relative)?;
    let destination = workspace_root.join(relative);
    if let Some(parent) = destination.parent() {
        fs::create_dir_all(parent).map_err(|error| {
            LocalToolError::Io(format!("failed to create {}: {error}", parent.display()))
        })?;
    }
    let backup = destination.with_file_name(format!(
        ".{}.backup.{}",
        destination
            .file_name()
            .map(|name| name.to_string_lossy())
            .unwrap_or_else(|| "path".into()),
        current_nanos()
    ));
    let had_destination = destination.exists();
    if had_destination {
        fs::rename(&destination, &backup).map_err(|error| {
            LocalToolError::Io(format!(
                "failed to stage existing {}: {error}",
                destination.display()
            ))
        })?;
    }
    if let Err(error) = fs::rename(staged, &destination) {
        if had_destination {
            let _ = fs::rename(&backup, &destination);
        }
        return Err(LocalToolError::Io(format!(
            "failed to move {} to {}: {error}",
            staged.display(),
            destination.display()
        )));
    }
    if had_destination {
        let _ = remove_path_any(&backup);
    }
    Ok(())
}

fn remove_path_any(path: &Path) -> std::io::Result<()> {
    let metadata = fs::symlink_metadata(path)?;
    if metadata.is_dir() && !metadata.file_type().is_symlink() {
        fs::remove_dir_all(path)
    } else {
        fs::remove_file(path)
    }
}

fn relative_path_arg(arguments: &Map<String, Value>) -> Result<PathBuf, LocalToolError> {
    let raw = string_arg(arguments, "path")?;
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Err(LocalToolError::InvalidArguments(
            "path must not be empty".to_string(),
        ));
    }
    let path = Path::new(trimmed);
    if path.is_absolute() {
        return Err(LocalToolError::InvalidArguments(
            "path must be relative".to_string(),
        ));
    }
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::Normal(value) => normalized.push(value),
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => {
                return Err(LocalToolError::InvalidArguments(
                    "path must stay inside the workspace and must not contain ..".to_string(),
                ));
            }
        }
    }
    if normalized.as_os_str().is_empty() {
        return Err(LocalToolError::InvalidArguments(
            "path must not be empty".to_string(),
        ));
    }
    Ok(normalized)
}

fn timeout_duration(arguments: &Map<String, Value>) -> Result<Duration, LocalToolError> {
    let seconds = f64_arg_with_default(arguments, "timeout_seconds", 120.0)?;
    Ok(Duration::from_secs_f64(seconds.clamp(1.0, 3600.0)))
}

fn remote_capabilities(
    host: &str,
    cwd: Option<&str>,
    timeout: Duration,
) -> Result<RemoteCapabilities, LocalToolError> {
    let script = "printf '{\"tar_gzip\":'; if command -v tar >/dev/null && command -v gzip >/dev/null; then printf true; else printf false; fi; printf ',\"python3\":'; if command -v python3 >/dev/null; then printf true; else printf false; fi; printf '}'";
    let output = run_ssh_capture(host, cwd, script, timeout)?;
    serde_json::from_str(&output).map_err(|error| {
        LocalToolError::Remote(format!("failed to parse remote capabilities: {error}"))
    })
}

fn remote_source_info(
    host: &str,
    cwd: Option<&str>,
    rel: &str,
    timeout: Duration,
) -> Result<RemotePathInfo, LocalToolError> {
    let script = format!(
        "python3 -c {} source {}",
        shell_quote(REMOTE_PATH_CHECK_SCRIPT),
        shell_quote(rel),
    );
    let output = run_ssh_capture(host, cwd, &script, timeout)?;
    serde_json::from_str(&output).map_err(|error| {
        LocalToolError::Remote(format!("failed to parse remote path info: {error}"))
    })
}

fn ensure_remote_write_path_safe(
    host: &str,
    cwd: Option<&str>,
    rel: &str,
    timeout: Duration,
) -> Result<(), LocalToolError> {
    let script = format!(
        "python3 -c {} target {}",
        shell_quote(REMOTE_PATH_CHECK_SCRIPT),
        shell_quote(rel),
    );
    run_ssh_capture(host, cwd, &script, timeout).map(|_| ())
}

fn remote_absolute_path(
    host: &str,
    cwd: Option<&str>,
    rel: &str,
    timeout: Duration,
) -> Result<String, LocalToolError> {
    let script = format!(
        "python3 -c {} {}",
        shell_quote("import pathlib, sys; print(pathlib.Path(sys.argv[1]).absolute())"),
        shell_quote(rel),
    );
    Ok(run_ssh_capture(host, cwd, &script, timeout)?
        .trim()
        .to_string())
}

fn create_remote_tmp_dir(
    host: &str,
    cwd: Option<&str>,
    relative: &Path,
    timeout: Duration,
) -> Result<String, LocalToolError> {
    let parent = parent_relative(relative).unwrap_or_else(|| ".".to_string());
    let base = temp_name_fragment(relative);
    let script = format!(
        "set -e; mkdir -p {parent}; tmp=$(mktemp -d {parent}/.{base}.incoming.XXXXXX); case \"$tmp\" in /*) printf '%s' \"$tmp\" ;; *) printf '%s/%s' \"$(pwd -P)\" \"$tmp\" ;; esac",
        parent = shell_quote(&parent),
        base = base,
    );
    Ok(run_ssh_capture(host, cwd, &script, timeout)?
        .trim()
        .to_string())
}

fn cleanup_remote_path(
    host: &str,
    cwd: Option<&str>,
    path: &str,
    timeout: Duration,
) -> Result<(), LocalToolError> {
    let script = format!("rm -rf {}", shell_quote(path));
    run_ssh_capture(host, cwd, &script, timeout).map(|_| ())
}

fn remote_install_script(rel: &str, extract_command: &str) -> String {
    let parent = parent_relative(Path::new(rel)).unwrap_or_else(|| ".".to_string());
    let base = temp_name_fragment(Path::new(rel));
    let staged = format!("\"$tmp\"/{}", shell_quote(rel));
    format!(
        "set -e; parent={parent}; target={target}; mkdir -p \"$parent\"; tmp=$(mktemp -d \"$parent/.{base}.incoming.XXXXXX\"); backup=; cleanup() {{ rm -rf \"$tmp\"; if [ -n \"$backup\" ] && [ -e \"$backup/old\" ] && [ ! -e \"$target\" ]; then mv \"$backup/old\" \"$target\"; fi; [ -n \"$backup\" ] && rm -rf \"$backup\"; }}; trap cleanup EXIT; {extract}; staged={staged}; test -e \"$staged\"; if [ -e \"$target\" ] || [ -L \"$target\" ]; then backup=$(mktemp -d \"$parent/.{base}.backup.XXXXXX\"); mv \"$target\" \"$backup/old\"; fi; mv \"$staged\" \"$target\"",
        parent = shell_quote(&parent),
        target = shell_quote(rel),
        base = base,
        extract = extract_command,
        staged = staged,
    )
}

fn remote_install_existing_path_script(rel: &str, staged: &str) -> String {
    let parent = parent_relative(Path::new(rel)).unwrap_or_else(|| ".".to_string());
    let base = temp_name_fragment(Path::new(rel));
    format!(
        "set -e; parent={parent}; target={target}; staged={staged}; mkdir -p \"$parent\"; test -e \"$staged\"; backup=; cleanup() {{ if [ -n \"$backup\" ] && [ -e \"$backup/old\" ] && [ ! -e \"$target\" ]; then mv \"$backup/old\" \"$target\"; fi; [ -n \"$backup\" ] && rm -rf \"$backup\"; }}; trap cleanup EXIT; if [ -e \"$target\" ] || [ -L \"$target\" ]; then backup=$(mktemp -d \"$parent/.{base}.backup.XXXXXX\"); mv \"$target\" \"$backup/old\"; fi; mv \"$staged\" \"$target\"",
        parent = shell_quote(&parent),
        target = shell_quote(rel),
        staged = shell_quote(staged),
        base = base,
    )
}

fn ensure_local_tool(name: &str) -> Result<(), LocalToolError> {
    if has_local_tool(name) {
        Ok(())
    } else {
        Err(LocalToolError::Io(format!("{name} is required")))
    }
}

fn has_local_tool(name: &str) -> bool {
    Command::new("sh")
        .arg("-c")
        .arg(format!("command -v {} >/dev/null", shell_quote(name)))
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|status| status.success())
        .unwrap_or(false)
}

fn run_ssh_capture(
    host: &str,
    cwd: Option<&str>,
    command: &str,
    timeout: Duration,
) -> Result<String, LocalToolError> {
    let remote_command = remote_shell_command(cwd, command);
    let shell_command = format!(
        "ssh -o BatchMode=yes -o ConnectTimeout=10 -T {} {}",
        shell_quote(host),
        shell_quote(&remote_command),
    );
    let output = run_shell_command_with_timeout(&shell_command, timeout)?;
    Ok(String::from_utf8_lossy(&output.stdout).to_string())
}

fn run_shell_command_with_timeout(
    command: &str,
    timeout: Duration,
) -> Result<std::process::Output, LocalToolError> {
    let mut child = Command::new("sh")
        .arg("-c")
        .arg(command)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| LocalToolError::Io(format!("failed to spawn copy command: {error}")))?;
    let deadline = Instant::now() + timeout;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                let output = child.wait_with_output().map_err(|error| {
                    LocalToolError::Io(format!("failed to collect copy output: {error}"))
                })?;
                if status.success() {
                    return Ok(output);
                }
                return Err(LocalToolError::Remote(format!(
                    "copy command exited with {}; stderr: {}",
                    status.code().unwrap_or(-1),
                    String::from_utf8_lossy(&output.stderr)
                )));
            }
            Ok(None) => {
                if Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(LocalToolError::Remote(format!(
                        "copy command timed out after {} seconds",
                        timeout.as_secs()
                    )));
                }
                thread::sleep(Duration::from_millis(100));
            }
            Err(error) => {
                return Err(LocalToolError::Io(format!(
                    "failed to wait for copy command: {error}"
                )));
            }
        }
    }
}

fn remote_shell_command(cwd: Option<&str>, command: &str) -> String {
    match cwd.map(str::trim).filter(|value| !value.is_empty()) {
        Some(cwd) => format!("cd {} && {}", shell_quote(cwd), command),
        None => command.to_string(),
    }
}

fn parent_relative(path: &Path) -> Option<String> {
    path.parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .map(display_relative)
}

fn display_relative(path: &Path) -> String {
    path.components()
        .filter_map(|component| match component {
            Component::Normal(value) => Some(value.to_string_lossy().to_string()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("/")
}

fn shell_path_basename(path: &Path) -> String {
    path.file_name()
        .map(|name| name.to_string_lossy().to_string())
        .unwrap_or_else(|| "path".to_string())
}

fn temp_name_fragment(path: &Path) -> String {
    let value = shell_path_basename(path)
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || matches!(ch, '.' | '_' | '-') {
                ch
            } else {
                '_'
            }
        })
        .collect::<String>()
        .trim_matches('.')
        .chars()
        .take(48)
        .collect::<String>();
    if value.is_empty() {
        "path".to_string()
    } else {
        value
    }
}

fn local_visibility_tmp_root(workspace_root: &Path) -> Result<PathBuf, LocalToolError> {
    let tmp_root = workspace_root
        .join(".stellaclaw")
        .join("visibility_tmp")
        .join(format!("{}", current_nanos()));
    fs::create_dir_all(&tmp_root).map_err(|error| {
        LocalToolError::Io(format!("failed to create {}: {error}", tmp_root.display()))
    })?;
    Ok(tmp_root)
}

fn current_nanos() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or_default()
}

const REMOTE_PATH_CHECK_SCRIPT: &str = r#"
import json
import os
import pathlib
import stat
import sys

mode = sys.argv[1]
rel = pathlib.PurePosixPath(sys.argv[2])
if rel.is_absolute() or not rel.parts or any(part in ("", ".", "..") for part in rel.parts):
    raise SystemExit("path must be a workspace-relative path without . or ..")

def kind_from_mode(value):
    if stat.S_ISDIR(value):
        return "directory"
    if stat.S_ISREG(value):
        return "file"
    if stat.S_ISLNK(value):
        return "symlink"
    return "other"

def reject_parent_symlinks(path):
    current = pathlib.Path(".")
    for part in path.parent.parts:
        if part in ("", "."):
            continue
        current = current / part
        try:
            st = os.lstat(current)
        except FileNotFoundError:
            return
        if stat.S_ISLNK(st.st_mode):
            raise SystemExit(f"visibility copy refuses symlink parent: {current}")
        if not stat.S_ISDIR(st.st_mode):
            raise SystemExit(f"visibility copy parent is not a directory: {current}")

def reject_tree_symlinks(path):
    try:
        st = os.lstat(path)
    except FileNotFoundError:
        raise SystemExit(f"remote path does not exist: {path}")
    if stat.S_ISLNK(st.st_mode):
        raise SystemExit(f"visibility copy refuses symlink path: {path}")
    if stat.S_ISDIR(st.st_mode):
        for root, dirs, files in os.walk(path, followlinks=False):
            for name in dirs + files:
                child = os.path.join(root, name)
                child_st = os.lstat(child)
                if stat.S_ISLNK(child_st.st_mode):
                    raise SystemExit(f"visibility copy refuses symlink path: {child}")
    return kind_from_mode(st.st_mode)

path = pathlib.Path(*rel.parts)
if mode == "source":
    kind = reject_tree_symlinks(path)
    if kind not in ("file", "directory"):
        raise SystemExit(f"visibility copy only supports regular files and directories: {path}")
    print(json.dumps({"kind": kind}))
elif mode == "target":
    reject_parent_symlinks(path)
    try:
        st = os.lstat(path)
    except FileNotFoundError:
        print(json.dumps({"kind": "missing"}))
    else:
        if stat.S_ISLNK(st.st_mode):
            raise SystemExit(f"visibility copy refuses to overwrite symlink: {path}")
        print(json.dumps({"kind": kind_from_mode(st.st_mode)}))
else:
    raise SystemExit("unknown mode")
"#;

#[cfg(test)]
mod tests {
    use std::{
        fs,
        time::{SystemTime, UNIX_EPOCH},
    };

    use serde_json::json;

    use super::*;

    fn temp_root() -> PathBuf {
        let id = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!("stellaclaw_visibility_{id}"))
    }

    #[test]
    fn relative_path_arg_rejects_escape() {
        let mut args = Map::new();
        args.insert("path".to_string(), json!("../outside"));

        assert!(relative_path_arg(&args).is_err());
    }

    #[test]
    #[cfg(unix)]
    fn local_visibility_rejects_symlink_tree() {
        use std::os::unix::fs::symlink;

        let root = temp_root();
        fs::create_dir_all(root.join("dir")).unwrap();
        fs::write(root.join("target.txt"), "secret").unwrap();
        symlink(root.join("target.txt"), root.join("dir/link.txt")).unwrap();

        let error = ensure_local_path_has_no_symlink(&root.join("dir"), "dir").unwrap_err();

        assert!(error.to_string().contains("refuses symlink path"));
        let _ = fs::remove_dir_all(root);
    }
}
