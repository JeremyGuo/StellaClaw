use std::{
    env,
    path::{Path, PathBuf},
    process::Command,
};

use anyhow::{anyhow, Context, Result};

use crate::config::{SandboxConfig, SandboxMode};

pub fn bubblewrap_support_error(sandbox: &SandboxConfig) -> Option<String> {
    if !cfg!(target_os = "linux") {
        return Some("sandbox mode 'bubblewrap' requires Linux".to_string());
    }
    let binary = sandbox.bubblewrap_binary.trim();
    if binary.is_empty() {
        return Some("sandbox.bubblewrap_binary must not be empty".to_string());
    }
    if !binary_in_path(binary) {
        return Some(format!(
            "sandbox.bubblewrap_binary '{}' was not found in PATH",
            binary
        ));
    }
    None
}

pub fn build_agent_server_command(
    sandbox: &SandboxConfig,
    binary_path: &Path,
    current_dir: &Path,
) -> Result<Command> {
    match sandbox.mode {
        SandboxMode::Subprocess => {
            let mut command = Command::new(binary_path);
            command.current_dir(current_dir);
            Ok(command)
        }
        SandboxMode::Bubblewrap => build_bubblewrap_command(sandbox, binary_path, current_dir),
    }
}

fn build_bubblewrap_command(
    sandbox: &SandboxConfig,
    binary_path: &Path,
    current_dir: &Path,
) -> Result<Command> {
    if let Some(reason) = bubblewrap_support_error(sandbox) {
        return Err(anyhow!(reason));
    }

    let executable = binary_path
        .canonicalize()
        .with_context(|| format!("failed to canonicalize {}", binary_path.display()))?;
    let workspace_root = current_dir
        .canonicalize()
        .with_context(|| format!("failed to canonicalize {}", current_dir.display()))?;

    let mut command = Command::new(&sandbox.bubblewrap_binary);
    command.arg("--die-with-parent").arg("--new-session");

    for system_path in ["/usr", "/bin", "/sbin", "/lib", "/lib64", "/etc"] {
        if Path::new(system_path).exists() {
            command.args(["--ro-bind", system_path, system_path]);
        }
    }
    if Path::new("/run").exists() {
        command.args(["--dir", "/run"]);
        if Path::new("/run/systemd/resolve").exists() {
            command.args(["--dir", "/run/systemd"]);
            command.args(["--ro-bind", "/run/systemd/resolve", "/run/systemd/resolve"]);
        }
    }
    command.args(["--dev", "/dev"]);
    command.args(["--proc", "/proc"]);
    command.args(["--tmpfs", "/tmp"]);
    command.args(["--tmpfs", "/var/tmp"]);
    command.args(["--dir", "/__stellaclaw"]);
    command.args(["--dir", "/__stellaclaw/bin"]);

    bind_path_to(
        &mut command,
        &executable,
        Path::new("/__stellaclaw/bin/agent_server"),
        true,
    )?;
    bind_path(&mut command, &workspace_root, false)?;
    if let Some(runtime_root) = discover_runtime_root(&workspace_root) {
        bind_path(&mut command, &runtime_root, false)?;
    }
    if let Some(home_ssh_dir) = discover_home_ssh_dir() {
        bind_path(&mut command, &home_ssh_dir, false)?;
    }
    if let Some(home_dir) = env::var_os("HOME").map(PathBuf::from) {
        command.args(["--setenv", "HOME", &home_dir.to_string_lossy()]);
    }
    command.args(["--chdir", &workspace_root.to_string_lossy()]);
    command.arg("/__stellaclaw/bin/agent_server");
    Ok(command)
}

fn bind_path(command: &mut Command, source: &Path, readonly: bool) -> Result<()> {
    bind_path_to(command, source, source, readonly)
}

fn bind_path_to(command: &mut Command, source: &Path, target: &Path, readonly: bool) -> Result<()> {
    let source = source
        .to_str()
        .ok_or_else(|| anyhow!("path is not valid UTF-8: {}", source.display()))?;
    let target = target
        .to_str()
        .ok_or_else(|| anyhow!("path is not valid UTF-8: {}", target.display()))?;
    if readonly {
        command.args(["--ro-bind", source, target]);
    } else {
        command.args(["--bind", source, target]);
    }
    Ok(())
}

fn discover_home_ssh_dir() -> Option<PathBuf> {
    let home_dir = env::var_os("HOME").map(PathBuf::from)?;
    let ssh_dir = home_dir.join(".ssh");
    ssh_dir.exists().then_some(ssh_dir)
}

fn discover_runtime_root(workspace_root: &Path) -> Option<PathBuf> {
    let conversations_root = workspace_root.parent()?;
    if conversations_root.file_name()? != "conversations" {
        return None;
    }
    let workdir = conversations_root.parent()?;
    let runtime_root = workdir.join("rundir");
    runtime_root.exists().then_some(runtime_root)
}

fn binary_in_path(binary: &str) -> bool {
    if binary.contains('/') {
        return Path::new(binary).is_file();
    }
    let Some(paths) = env::var_os("PATH") else {
        return false;
    };
    for dir in env::split_paths(&paths) {
        if dir.join(binary).is_file() {
            return true;
        }
    }
    false
}
