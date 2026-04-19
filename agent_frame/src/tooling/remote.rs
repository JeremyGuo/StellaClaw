use crate::config::RemoteWorkpathConfig;
use anyhow::{Context, Result, anyhow};
use serde_json::{Map, Value, json};
use std::collections::BTreeMap;
use std::io::Write;
use std::process::{Command, Stdio};
use std::sync::Arc;

pub(super) type RemoteWorkpathMap = Arc<BTreeMap<String, String>>;
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum ExecutionTarget {
    Local,
    RemoteSsh { host: String },
}

impl ExecutionTarget {
    pub(super) fn remote_name(&self) -> &str {
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

pub(super) fn remote_schema_property() -> Value {
    json!({
        "type": "string",
        "description": "Execution target."
    })
}

pub(super) fn execution_target_arg(arguments: &Map<String, Value>) -> Result<ExecutionTarget> {
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

pub(super) fn remote_workpath_map(remote_workpaths: &[RemoteWorkpathConfig]) -> RemoteWorkpathMap {
    let mut map = BTreeMap::new();
    for workpath in remote_workpaths {
        let host = workpath.host.trim();
        let path = workpath.path.trim();
        if !host.is_empty() && !path.is_empty() && host != "local" {
            map.insert(host.to_string(), path.to_string());
        }
    }
    Arc::new(map)
}

fn remote_cd_prefix(remote_cwd: &str) -> String {
    format!(
        "AGENT_FRAME_REMOTE_CWD={}; case \"$AGENT_FRAME_REMOTE_CWD\" in '~') AGENT_FRAME_REMOTE_CWD=\"$HOME\" ;; '~/'*) AGENT_FRAME_REMOTE_CWD=\"$HOME/${{AGENT_FRAME_REMOTE_CWD#~/}}\" ;; esac; cd \"$AGENT_FRAME_REMOTE_CWD\" &&",
        shell_quote(remote_cwd)
    )
}

fn is_remote_absolute_path(path: &str) -> bool {
    path.starts_with('/')
}

fn join_remote_path(base: &str, relative: &str) -> String {
    let relative = relative.trim();
    if relative.is_empty() || relative == "." {
        return base.to_string();
    }
    let relative = relative.strip_prefix("./").unwrap_or(relative);
    if base.ends_with('/') {
        format!("{base}{relative}")
    } else {
        format!("{base}/{relative}")
    }
}

fn remote_default_root<'a>(host: &str, remote_workpaths: &'a BTreeMap<String, String>) -> &'a str {
    remote_workpaths
        .get(host)
        .map(String::as_str)
        .unwrap_or("~")
}

pub(super) fn resolve_remote_cwd(
    host: &str,
    cwd: Option<&str>,
    remote_workpaths: &BTreeMap<String, String>,
) -> Result<String> {
    let root = remote_default_root(host, remote_workpaths);
    match (cwd.map(str::trim).filter(|value| !value.is_empty()), root) {
        (Some(cwd), _) if is_remote_absolute_path(cwd) || cwd == "~" || cwd.starts_with("~/") => {
            Ok(cwd.to_string())
        }
        (Some(cwd), root) => Ok(join_remote_path(root, cwd)),
        (None, root) => Ok(root.to_string()),
    }
}

fn remote_file_path_arg<'a>(operation: &str, arguments: &'a Map<String, Value>) -> Option<&'a str> {
    match operation {
        "file_read" | "file_write" => arguments
            .get("file_path")
            .or_else(|| arguments.get("path"))
            .and_then(Value::as_str),
        "glob" | "grep" | "ls" | "edit" => arguments.get("path").and_then(Value::as_str),
        _ => None,
    }
}

pub(super) fn remote_file_root(
    host: &str,
    operation: &str,
    arguments: &Map<String, Value>,
    remote_workpaths: &BTreeMap<String, String>,
) -> Result<(String, String)> {
    if let Some(path) = remote_file_path_arg(operation, arguments)
        && is_remote_absolute_path(path)
    {
        return Ok(("/".to_string(), "/".to_string()));
    }
    Ok((
        remote_default_root(host, remote_workpaths).to_string(),
        ".".to_string(),
    ))
}

pub(super) fn remote_python_command(script: &str) -> String {
    format!(
        "if command -v python3 >/dev/null 2>&1; then exec python3 -c {}; elif command -v python >/dev/null 2>&1; then exec python -c {}; else echo 'remote file tools require Python 3 on this host; install python3/python or use a remote shell command for shell-only commands' >&2; exit 127; fi",
        shell_quote(script),
        shell_quote(script)
    )
}

pub(super) fn run_remote_command(
    host: &str,
    remote_cwd: &str,
    command_args: &[String],
    stdin: Option<&[u8]>,
) -> Result<std::process::Output> {
    let mut script = remote_cd_prefix(remote_cwd);
    for arg in command_args {
        script.push(' ');
        script.push_str(&shell_quote(arg));
    }
    let mut command = ssh_command(host, false);
    let remote_command = format!("sh -c {}", shell_quote(&script));
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
