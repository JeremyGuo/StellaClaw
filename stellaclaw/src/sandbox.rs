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
    session_root: &Path,
) -> Result<Command> {
    match sandbox.mode {
        SandboxMode::Subprocess => {
            let mut command = Command::new(binary_path);
            command.current_dir(current_dir);
            command.env("STELLACLAW_SESSION_ROOT", session_root);
            if let Some(software_dir) = configured_software_dir(sandbox) {
                command.env("STELLACLAW_SOFTWARE_DIR", software_dir);
            }
            Ok(command)
        }
        SandboxMode::Bubblewrap => {
            build_bubblewrap_command(sandbox, binary_path, current_dir, session_root)
        }
    }
}

pub fn build_workspace_shell_command(
    sandbox: &SandboxConfig,
    current_dir: &Path,
    session_root: &Path,
    script: &str,
) -> Result<Command> {
    match sandbox.mode {
        SandboxMode::Subprocess => {
            let mut command = Command::new("sh");
            command.arg("-lc").arg(script);
            command.current_dir(current_dir);
            command.env("STELLACLAW_SESSION_ROOT", session_root);
            if let Some(software_dir) = configured_software_dir(sandbox) {
                command.env("STELLACLAW_SOFTWARE_DIR", software_dir);
            }
            Ok(command)
        }
        SandboxMode::Bubblewrap => {
            build_bubblewrap_shell_command(sandbox, current_dir, session_root, script)
        }
    }
}

fn build_bubblewrap_command(
    sandbox: &SandboxConfig,
    binary_path: &Path,
    current_dir: &Path,
    session_root: &Path,
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
    let session_root = session_root
        .canonicalize()
        .with_context(|| format!("failed to canonicalize {}", session_root.display()))?;

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
    if session_root != workspace_root {
        bind_path(&mut command, &session_root, false)?;
    }
    if let Some(runtime_root) = discover_runtime_root(&workspace_root) {
        bind_path(&mut command, &runtime_root, false)?;
    }
    let session_root_env = session_root.to_string_lossy().to_string();
    command.args([
        "--setenv",
        "STELLACLAW_SESSION_ROOT",
        session_root_env.as_str(),
    ]);
    if let Some(software_dir) = configured_software_dir(sandbox) {
        let software_dir = software_dir
            .canonicalize()
            .with_context(|| format!("failed to canonicalize {}", software_dir.display()))?;
        let software_mount_path = configured_software_mount_path(sandbox)?;
        bind_path_to(&mut command, &software_dir, &software_mount_path, false)?;
        let software_mount_path = software_mount_path.to_string_lossy().to_string();
        command.args(["--setenv", "STELLACLAW_SOFTWARE_DIR", &software_mount_path]);
    }
    if let Some(home_ssh_dir) = discover_home_ssh_dir() {
        bind_path(&mut command, &home_ssh_dir, false)?;
    }
    if let Some(home_config_dir) = discover_home_config_dir() {
        bind_path(&mut command, &home_config_dir, false)?;
    }
    for codex_auth_path in discover_codex_auth_paths() {
        bind_path(&mut command, &codex_auth_path, false)?;
    }
    if let Some(home_dir) = env::var_os("HOME").map(PathBuf::from) {
        command.args(["--setenv", "HOME", &home_dir.to_string_lossy()]);
    }
    command.args(["--chdir", &workspace_root.to_string_lossy()]);
    command.arg("/__stellaclaw/bin/agent_server");
    Ok(command)
}

fn build_bubblewrap_shell_command(
    sandbox: &SandboxConfig,
    current_dir: &Path,
    session_root: &Path,
    script: &str,
) -> Result<Command> {
    if let Some(reason) = bubblewrap_support_error(sandbox) {
        return Err(anyhow!(reason));
    }

    let workspace_root = current_dir
        .canonicalize()
        .with_context(|| format!("failed to canonicalize {}", current_dir.display()))?;
    let session_root = session_root
        .canonicalize()
        .with_context(|| format!("failed to canonicalize {}", session_root.display()))?;

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

    bind_path(&mut command, &workspace_root, false)?;
    if session_root != workspace_root {
        bind_path(&mut command, &session_root, false)?;
    }
    if let Some(runtime_root) = discover_runtime_root(&workspace_root) {
        bind_path(&mut command, &runtime_root, false)?;
    }
    let session_root_env = session_root.to_string_lossy().to_string();
    command.args([
        "--setenv",
        "STELLACLAW_SESSION_ROOT",
        session_root_env.as_str(),
    ]);
    if let Some(software_dir) = configured_software_dir(sandbox) {
        let software_dir = software_dir
            .canonicalize()
            .with_context(|| format!("failed to canonicalize {}", software_dir.display()))?;
        let software_mount_path = configured_software_mount_path(sandbox)?;
        bind_path_to(&mut command, &software_dir, &software_mount_path, false)?;
        let software_mount_path = software_mount_path.to_string_lossy().to_string();
        command.args(["--setenv", "STELLACLAW_SOFTWARE_DIR", &software_mount_path]);
    }
    if let Some(home_ssh_dir) = discover_home_ssh_dir() {
        bind_path(&mut command, &home_ssh_dir, false)?;
    }
    if let Some(home_config_dir) = discover_home_config_dir() {
        bind_path(&mut command, &home_config_dir, false)?;
    }
    for codex_auth_path in discover_codex_auth_paths() {
        bind_path(&mut command, &codex_auth_path, false)?;
    }
    if let Some(home_dir) = env::var_os("HOME").map(PathBuf::from) {
        command.args(["--setenv", "HOME", &home_dir.to_string_lossy()]);
    }
    command.args(["--chdir", &workspace_root.to_string_lossy()]);
    command.arg("/bin/sh").arg("-lc").arg(script);
    Ok(command)
}

fn configured_software_dir(sandbox: &SandboxConfig) -> Option<PathBuf> {
    sandbox
        .software_dir
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
}

fn configured_software_mount_path(sandbox: &SandboxConfig) -> Result<PathBuf> {
    let path = PathBuf::from(sandbox.software_mount_path.trim());
    if path.is_absolute() {
        return Ok(path);
    }
    Err(anyhow!(
        "sandbox.software_mount_path must be absolute, got {}",
        sandbox.software_mount_path
    ))
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

fn discover_home_config_dir() -> Option<PathBuf> {
    let home_dir = env::var_os("HOME").map(PathBuf::from)?;
    let config_dir = home_dir.join(".config");
    config_dir.exists().then_some(config_dir)
}

fn discover_codex_auth_paths() -> Vec<PathBuf> {
    let mut paths = Vec::new();
    for env_key in ["CODEX_AUTH_JSON", "CHATGPT_AUTH_JSON"] {
        if let Some(path) = env::var_os(env_key).map(PathBuf::from) {
            push_existing_unique_path(&mut paths, path);
        }
    }
    if let Some(path) = env::var_os("CODEX_HOME").map(PathBuf::from) {
        push_existing_unique_path(&mut paths, path);
    }
    if let Some(home_dir) = env::var_os("HOME").map(PathBuf::from) {
        push_existing_unique_path(&mut paths, home_dir.join(".codex"));
    }
    paths
}

fn push_existing_unique_path(paths: &mut Vec<PathBuf>, path: PathBuf) {
    if !path.exists() || paths.iter().any(|existing| existing == &path) {
        return;
    }
    paths.push(path);
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

#[cfg(test)]
mod tests {
    use super::build_agent_server_command;
    use crate::config::{SandboxConfig, SandboxMode};

    #[test]
    fn subprocess_sets_software_dir_env_when_configured() {
        let sandbox = SandboxConfig {
            mode: SandboxMode::Subprocess,
            software_dir: Some("/opt/stellaclaw-software".to_string()),
            ..SandboxConfig::default()
        };
        let command = build_agent_server_command(
            &sandbox,
            std::path::Path::new("/bin/echo"),
            std::path::Path::new("/tmp"),
            std::path::Path::new("/tmp/session"),
        )
        .expect("subprocess command should build");

        let software_env = command
            .get_envs()
            .find_map(|(key, value)| {
                (key == "STELLACLAW_SOFTWARE_DIR").then(|| value.map(|value| value.to_owned()))
            })
            .flatten();

        assert_eq!(
            software_env.as_deref(),
            Some(std::ffi::OsStr::new("/opt/stellaclaw-software"))
        );

        let session_root_env = command
            .get_envs()
            .find_map(|(key, value)| {
                (key == "STELLACLAW_SESSION_ROOT").then(|| value.map(|value| value.to_owned()))
            })
            .flatten();

        assert_eq!(
            session_root_env.as_deref(),
            Some(std::ffi::OsStr::new("/tmp/session"))
        );
    }
}
