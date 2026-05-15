use std::{
    env, fs,
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
            command.env("STELLACLAW_DATA_ROOT", session_root);
            if let Some(software_dir) = software_dir_or_default(sandbox) {
                command.env("STELLACLAW_SOFTWARE_DIR", software_dir);
            }
            Ok(command)
        }
        SandboxMode::Bubblewrap => {
            build_bubblewrap_command(sandbox, binary_path, current_dir, session_root)
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
    // Try session_root first for runtime root discovery.  When the
    // workspace is a symlink (localhost remote mode), canonicalize()
    // resolves to the real target directory which is outside the workdir
    // hierarchy, so discover_runtime_root would fail on workspace_root.
    // session_root is always under workdir/conversations/ and resolves
    // correctly.
    if let Some(runtime_root) =
        discover_runtime_root(&session_root).or_else(|| discover_runtime_root(&workspace_root))
    {
        bind_path(&mut command, &runtime_root, false)?;
    }
    let session_root_env = session_root.to_string_lossy().to_string();
    command.args([
        "--setenv",
        "STELLACLAW_SESSION_ROOT",
        session_root_env.as_str(),
    ]);
    command.args([
        "--setenv",
        "STELLACLAW_DATA_ROOT",
        session_root_env.as_str(),
    ]);
    mount_configured_software_dir_or_create_default(&mut command, sandbox)?;
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

fn configured_software_dir(sandbox: &SandboxConfig) -> Option<PathBuf> {
    sandbox
        .software_dir
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
}

fn software_dir_or_default(sandbox: &SandboxConfig) -> Option<PathBuf> {
    configured_software_dir(sandbox).or_else(default_software_dir)
}

fn default_software_dir() -> Option<PathBuf> {
    env::var_os("HOME").map(|home| PathBuf::from(home).join(".cache"))
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

fn mount_configured_software_dir_or_create_default(
    command: &mut Command,
    sandbox: &SandboxConfig,
) -> Result<()> {
    let Some(software_dir) = software_dir_or_default(sandbox) else {
        return Ok(());
    };
    fs::create_dir_all(&software_dir)
        .with_context(|| format!("failed to create {}", software_dir.display()))?;
    let software_dir = software_dir
        .canonicalize()
        .with_context(|| format!("failed to canonicalize {}", software_dir.display()))?;
    let software_mount_path = configured_software_mount_path(sandbox)?;
    bind_path_to(command, &software_dir, &software_mount_path, false)?;
    let software_mount_path = software_mount_path.to_string_lossy().to_string();
    command.args(["--setenv", "STELLACLAW_SOFTWARE_DIR", &software_mount_path]);
    Ok(())
}

#[cfg(test)]
fn create_empty_default_software_mount(command: &mut Command, sandbox: &SandboxConfig) {
    if sandbox.software_mount_path.trim() == "/opt" {
        command.args(["--dir", "/opt"]);
    }
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
    use super::{build_agent_server_command, create_empty_default_software_mount};
    use crate::config::{SandboxConfig, SandboxMode};
    use std::ffi::OsStr;

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

    #[test]
    fn bubblewrap_creates_empty_default_software_mount_when_unconfigured() {
        let sandbox = SandboxConfig {
            mode: SandboxMode::Bubblewrap,
            software_dir: None,
            software_mount_path: "/opt".to_string(),
            ..SandboxConfig::default()
        };
        let mut command = std::process::Command::new("bwrap");

        create_empty_default_software_mount(&mut command, &sandbox);

        let args = command.get_args().collect::<Vec<_>>();
        assert!(args
            .windows(2)
            .any(|window| window == [OsStr::new("--dir"), OsStr::new("/opt")]));
        assert!(command
            .get_envs()
            .all(|(key, _)| key != "STELLACLAW_SOFTWARE_DIR"));
    }

    #[test]
    fn bubblewrap_does_not_create_non_default_software_mount_when_unconfigured() {
        let sandbox = SandboxConfig {
            mode: SandboxMode::Bubblewrap,
            software_dir: None,
            software_mount_path: "/tools".to_string(),
            ..SandboxConfig::default()
        };
        let mut command = std::process::Command::new("bwrap");

        create_empty_default_software_mount(&mut command, &sandbox);

        assert!(command.get_args().next().is_none());
    }
}
