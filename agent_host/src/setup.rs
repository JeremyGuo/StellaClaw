use anyhow::{Context, Result, bail};
use std::ffi::OsStr;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

pub fn run_setup(config: &Path, workdir: &Path, service_name: Option<&str>) -> Result<()> {
    if !cfg!(target_os = "linux") {
        println!("`partyclaw setup` 目前只支持 Linux。");
        return Ok(());
    }

    let executable_path = resolve_executable_path()?;
    let config_path = absolutize_path(config)?;
    let workdir_path = absolutize_path(workdir)?;
    let working_directory = infer_working_directory(&executable_path)?;
    let service_name = normalize_service_name(service_name)?;
    let service_path = user_systemd_dir()?.join(&service_name);
    let environment_files = collect_environment_files(&working_directory, &config_path);
    let unit = render_systemd_unit(
        &executable_path,
        &config_path,
        &workdir_path,
        &working_directory,
        &environment_files,
    );

    if let Some(parent) = service_path.parent() {
        fs::create_dir_all(parent).with_context(|| {
            format!(
                "failed to create user systemd directory {}",
                parent.display()
            )
        })?;
    }
    fs::write(&service_path, unit)
        .with_context(|| format!("failed to write {}", service_path.display()))?;

    let daemon_reload_status = Command::new("systemctl")
        .args(["--user", "daemon-reload"])
        .status();

    println!("已写入 user systemd unit:");
    println!("  {}", service_path.display());
    println!();
    println!("解析后的路径:");
    println!("  ExecStart 二进制: {}", executable_path.display());
    println!("  配置文件: {}", config_path.display());
    println!("  工作目录: {}", workdir_path.display());
    println!("  WorkingDirectory: {}", working_directory.display());
    if environment_files.is_empty() {
        println!("  EnvironmentFile: 未发现 .env，已省略");
    } else {
        println!("  EnvironmentFile:");
        for env_file in &environment_files {
            println!("    {}", env_file.display());
        }
    }
    println!();

    match daemon_reload_status {
        Ok(status) if status.success() => {
            println!("已执行 `systemctl --user daemon-reload`。");
        }
        Ok(status) => {
            println!(
                "`systemctl --user daemon-reload` 返回非 0 状态码: {}",
                status
            );
            println!("请稍后手动执行一次该命令。");
        }
        Err(error) => {
            println!("未能自动执行 `systemctl --user daemon-reload`: {error}");
            println!("请稍后手动执行一次该命令。");
        }
    }

    println!();
    println!("下一步建议:");
    println!("  1. 立即启动或重启服务:");
    println!("     systemctl --user restart {}", service_name);
    println!("  2. 设置开机自启:");
    println!("     systemctl --user enable {}", service_name);
    println!("  3. 如果你希望机器重启后在“用户未登录”时也自动拉起 user service，需要启用 linger:");
    println!("     sudo loginctl enable-linger \"$USER\"");
    println!("  4. 可选检查 linger 状态:");
    println!("     loginctl show-user \"$USER\" -p Linger");

    Ok(())
}

fn resolve_executable_path() -> Result<PathBuf> {
    let executable = std::env::current_exe().context("failed to resolve current executable")?;
    match executable.canonicalize() {
        Ok(path) => Ok(path),
        Err(_) => Ok(executable),
    }
}

fn absolutize_path(path: &Path) -> Result<PathBuf> {
    if path.is_absolute() {
        return Ok(path.to_path_buf());
    }
    Ok(std::env::current_dir()
        .context("failed to read current working directory")?
        .join(path))
}

fn infer_working_directory(executable_path: &Path) -> Result<PathBuf> {
    let inferred = executable_path.parent().and_then(|profile_dir| {
        let profile = profile_dir.file_name().and_then(OsStr::to_str)?;
        if !matches!(profile, "debug" | "release") {
            return None;
        }
        let target_dir = profile_dir.parent()?;
        if target_dir.file_name() != Some(OsStr::new("target")) {
            return None;
        }
        target_dir.parent().map(Path::to_path_buf)
    });
    match inferred {
        Some(path) => Ok(path),
        None => std::env::current_dir().context("failed to read current working directory"),
    }
}

fn normalize_service_name(service_name: Option<&str>) -> Result<String> {
    let raw = service_name.unwrap_or("partyclaw").trim();
    if raw.is_empty() {
        bail!("systemd service name must not be empty");
    }
    if raw.contains('/') || raw.contains('\\') {
        bail!("systemd service name must not contain path separators");
    }
    if raw.ends_with(".service") {
        Ok(raw.to_string())
    } else {
        Ok(format!("{raw}.service"))
    }
}

fn user_systemd_dir() -> Result<PathBuf> {
    if let Some(path) = std::env::var_os("XDG_CONFIG_HOME") {
        return Ok(PathBuf::from(path).join("systemd").join("user"));
    }
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .context("HOME or XDG_CONFIG_HOME is required to install a user systemd unit")?;
    Ok(home.join(".config").join("systemd").join("user"))
}

fn collect_environment_files(working_directory: &Path, config_path: &Path) -> Vec<PathBuf> {
    let mut result = Vec::new();
    let working_env = working_directory.join(".env");
    if working_env.is_file() {
        result.push(working_env);
    }
    if let Some(config_dir) = config_path.parent() {
        let config_env = config_dir.join(".env");
        if config_env.is_file() && !result.iter().any(|path| path == &config_env) {
            result.push(config_env);
        }
    }
    result
}

fn render_systemd_unit(
    executable_path: &Path,
    config_path: &Path,
    workdir_path: &Path,
    working_directory: &Path,
    environment_files: &[PathBuf],
) -> String {
    let mut lines = vec![
        "[Unit]".to_string(),
        "Description=Partyclaw Agent Host".to_string(),
        "After=network.target".to_string(),
        String::new(),
        "[Service]".to_string(),
        "Type=simple".to_string(),
        format!(
            "WorkingDirectory={}",
            systemd_quote(&working_directory.display().to_string())
        ),
        format!(
            "ExecStart={} --config {} --workdir {}",
            systemd_quote(&executable_path.display().to_string()),
            systemd_quote(&config_path.display().to_string()),
            systemd_quote(&workdir_path.display().to_string())
        ),
        "Restart=always".to_string(),
        "RestartSec=3".to_string(),
    ];
    for env_file in environment_files {
        lines.push(format!(
            "EnvironmentFile=-{}",
            systemd_quote(&env_file.display().to_string())
        ));
    }
    lines.extend([
        String::new(),
        "[Install]".to_string(),
        "WantedBy=default.target".to_string(),
        String::new(),
    ]);
    lines.join("\n")
}

fn systemd_quote(value: &str) -> String {
    format!("\"{}\"", value.replace('\\', "\\\\").replace('"', "\\\""))
}

#[cfg(test)]
mod tests {
    use super::{
        infer_working_directory, normalize_service_name, render_systemd_unit, systemd_quote,
    };
    use std::path::Path;

    #[test]
    fn infers_repo_root_from_target_release_binary() {
        let path = Path::new("/srv/clawparty/ClawParty2.0/target/release/partyclaw");
        let working_directory = infer_working_directory(path).unwrap();
        assert_eq!(working_directory, Path::new("/srv/clawparty/ClawParty2.0"));
    }

    #[test]
    fn service_name_defaults_and_adds_suffix() {
        assert_eq!(normalize_service_name(None).unwrap(), "partyclaw.service");
        assert_eq!(
            normalize_service_name(Some("clawparty2")).unwrap(),
            "clawparty2.service"
        );
        assert_eq!(
            normalize_service_name(Some("clawparty2.service")).unwrap(),
            "clawparty2.service"
        );
    }

    #[test]
    fn rendered_unit_uses_absolute_execstart_and_optional_env_files() {
        let unit = render_systemd_unit(
            Path::new("/srv/app/target/release/partyclaw"),
            Path::new("/srv/app/deploy_telegram.json"),
            Path::new("/srv/app/workdir"),
            Path::new("/srv/app"),
            &[Path::new("/srv/app/.env").to_path_buf()],
        );
        assert!(unit.contains("ExecStart=\"/srv/app/target/release/partyclaw\" --config \"/srv/app/deploy_telegram.json\" --workdir \"/srv/app/workdir\""));
        assert!(unit.contains("WorkingDirectory=\"/srv/app\""));
        assert!(unit.contains("EnvironmentFile=-\"/srv/app/.env\""));
    }

    #[test]
    fn systemd_quote_escapes_quotes_and_backslashes() {
        assert_eq!(
            systemd_quote(r#"C:\Tools\"quote""#),
            r#""C:\\Tools\\\"quote\"""#
        );
    }
}
