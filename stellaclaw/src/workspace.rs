use std::{
    fs,
    path::{Path, PathBuf},
    process::Command,
};

use anyhow::{anyhow, Context, Result};
use stellaclaw_core::session_actor::ToolRemoteMode;

const RUNDIR: &str = "rundir";
const STELLACLAW_DIR: &str = ".stellaclaw";
const SHARED_DIR: &str = "shared";
const SKILL_DIR: &str = "skill";
const SHARED_SKILL_MEMORY_DIR: &str = "skill_memory";

pub fn ensure_workspace_for_remote_mode(
    workdir: &Path,
    conversation_root: &Path,
    _conversation_id: &str,
    remote_mode: &ToolRemoteMode,
) -> Result<PathBuf> {
    match remote_mode {
        ToolRemoteMode::Selectable => {
            ensure_workspace_seed(workdir, conversation_root)?;
            Ok(conversation_root.to_path_buf())
        }
        ToolRemoteMode::FixedSsh { host, cwd } => {
            let remote_path = cwd
                .as_deref()
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .ok_or_else(|| anyhow!("remote workspace path must not be empty"))?;
            validate_remote_binding(host, remote_path)?;
            ensure_passwordless_ssh_login(host)?;
            ensure_workspace_seed(workdir, conversation_root)?;
            Ok(conversation_root.to_path_buf())
        }
    }
}

pub fn ensure_workspace_seed(workdir: &Path, data_root: &Path) -> Result<()> {
    migrate_legacy_workspace_layout(data_root);

    let runtime_root = workdir.join(RUNDIR);
    let runtime_shared = runtime_root.join("shared");
    let runtime_profile = runtime_root.join(STELLACLAW_DIR);
    let runtime_skill = runtime_profile.join(SKILL_DIR);
    let runtime_skill_memory = runtime_profile.join(SHARED_SKILL_MEMORY_DIR);

    fs::create_dir_all(&runtime_shared)
        .with_context(|| format!("failed to create {}", runtime_shared.display()))?;
    fs::create_dir_all(&runtime_profile)
        .with_context(|| format!("failed to create {}", runtime_profile.display()))?;
    fs::create_dir_all(&runtime_skill)
        .with_context(|| format!("failed to create {}", runtime_skill.display()))?;
    fs::create_dir_all(&runtime_skill_memory)
        .with_context(|| format!("failed to create {}", runtime_skill_memory.display()))?;

    let stellaclaw_dir = data_root.join(STELLACLAW_DIR);
    let workspace_profile = &stellaclaw_dir;
    let workspace_skill = stellaclaw_dir.join(SKILL_DIR);
    let workspace_shared = stellaclaw_dir.join(SHARED_DIR);
    let workspace_skill_memory = stellaclaw_dir.join(SHARED_SKILL_MEMORY_DIR);
    fs::create_dir_all(workspace_profile)
        .with_context(|| format!("failed to create {}", workspace_profile.display()))?;
    fs::create_dir_all(&workspace_skill)
        .with_context(|| format!("failed to create {}", workspace_skill.display()))?;

    ensure_seed_file(
        &runtime_profile.join("USER.md"),
        &workspace_profile.join("USER.md"),
    )?;
    ensure_seed_file(
        &runtime_profile.join("IDENTITY.md"),
        &workspace_profile.join("IDENTITY.md"),
    )?;
    ensure_skill_seed(&runtime_skill, &workspace_skill)?;
    ensure_shared_link(&runtime_skill_memory, &workspace_skill_memory)?;
    ensure_shared_link(&runtime_shared, &workspace_shared)?;
    Ok(())
}

/// Migrate legacy workspace layout to the new .stellaclaw/ structure.
fn migrate_legacy_workspace_layout(data_root: &Path) {
    let stellaclaw_dir = data_root.join(STELLACLAW_DIR);

    // .skill/ -> .stellaclaw/skill/
    let old_skill = data_root.join(".skill");
    let new_skill = stellaclaw_dir.join(SKILL_DIR);
    if old_skill.exists() && !new_skill.exists() {
        let _ = fs::create_dir_all(&stellaclaw_dir);
        let _ = fs::rename(&old_skill, &new_skill);
    }

    // shared -> .stellaclaw/shared
    let old_shared = data_root.join("shared");
    let new_shared = stellaclaw_dir.join(SHARED_DIR);
    if old_shared.symlink_metadata().is_ok() && !new_shared.exists() {
        let _ = fs::create_dir_all(&stellaclaw_dir);
        let _ = fs::rename(&old_shared, &new_shared);
    }

    // .stellaclaw/stellaclaw_shared -> .stellaclaw/shared
    let old_internal_shared = stellaclaw_dir.join("stellaclaw_shared");
    if old_internal_shared.symlink_metadata().is_ok() && !new_shared.exists() {
        let _ = fs::rename(&old_internal_shared, &new_shared);
    }

    // .skill_memory -> .stellaclaw/skill_memory
    let old_skill_memory = data_root.join(".skill_memory");
    let new_skill_memory = stellaclaw_dir.join(SHARED_SKILL_MEMORY_DIR);
    if old_skill_memory.symlink_metadata().is_ok() && !new_skill_memory.exists() {
        let _ = fs::create_dir_all(&stellaclaw_dir);
        let _ = fs::rename(&old_skill_memory, &new_skill_memory);
    }

    // .output/ -> .stellaclaw/output/
    let old_output = data_root.join(".output");
    let new_output = stellaclaw_dir.join("output");
    if old_output.exists() && !new_output.exists() {
        let _ = fs::create_dir_all(&stellaclaw_dir);
        let _ = fs::rename(&old_output, &new_output);
    }

    // .cache/ -> .stellaclaw/cache/
    let old_cache = data_root.join(".cache");
    let new_cache = stellaclaw_dir.join("cache");
    if old_cache.exists() && !new_cache.exists() {
        let _ = fs::create_dir_all(&stellaclaw_dir);
        let _ = fs::rename(&old_cache, &new_cache);
    }

    // .log/stellaclaw/ -> .stellaclaw/log/
    let old_log = data_root.join(".log").join("stellaclaw");
    let new_log = stellaclaw_dir.join("log");
    if old_log.exists() {
        let _ = fs::create_dir_all(&stellaclaw_dir);
        if !new_log.exists() {
            let _ = fs::rename(&old_log, &new_log);
        } else {
            // new_log was already created (e.g. by the conversation logger).
            // Move remaining old files into the new directory, then remove old.
            if let Ok(entries) = fs::read_dir(&old_log) {
                for entry in entries.flatten() {
                    let new_dest = new_log.join(entry.file_name());
                    if !new_dest.exists() {
                        let _ = fs::rename(entry.path(), new_dest);
                    } else if entry.path().is_file() {
                        // Prepend old content for log files.
                        if let Ok(old_data) = fs::read(entry.path()) {
                            if !old_data.is_empty() {
                                if let Ok(new_data) = fs::read(&new_dest) {
                                    let mut merged = old_data;
                                    merged.extend_from_slice(&new_data);
                                    let _ = fs::write(&new_dest, merged);
                                }
                            }
                        }
                        let _ = fs::remove_file(entry.path());
                    }
                }
            }
            let _ = fs::remove_dir_all(&old_log);
        }
        // Clean up empty .log/ parent
        let _ = fs::remove_dir(data_root.join(".log"));
    }
}

pub fn is_sshfs_workspace_entry_name(name: &str) -> bool {
    name.starts_with("sshfs-")
}

fn validate_remote_binding(host: &str, remote_path: &str) -> Result<()> {
    let host = host.trim();
    if host.is_empty() {
        return Err(anyhow!("remote host must not be empty"));
    }
    if !host
        .chars()
        .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '.' | '_' | '-'))
    {
        return Err(anyhow!(
            "remote host must be a safe ~/.ssh/config Host alias"
        ));
    }
    if remote_path.trim().is_empty() {
        return Err(anyhow!("remote path must not be empty"));
    }
    Ok(())
}

fn ensure_passwordless_ssh_login(host: &str) -> Result<()> {
    let output = passwordless_ssh_login_check_command(host)
        .output()
        .with_context(|| format!("failed to run passwordless SSH login check for {host}"))?;
    if output.status.success() {
        return Ok(());
    }

    let stderr = trimmed_output_preview(&output.stderr);
    let suffix = if stderr.is_empty() {
        format!(
            "ssh exited with status {}",
            output.status.code().unwrap_or(-1)
        )
    } else {
        stderr
    };
    Err(anyhow!(
        "remote host `{host}` must allow passwordless SSH login before Remote Mode can be enabled: {suffix}"
    ))
}

fn passwordless_ssh_login_check_command(host: &str) -> Command {
    let mut command = Command::new("ssh");
    command
        .arg("-o")
        .arg("BatchMode=yes")
        .arg("-o")
        .arg("PasswordAuthentication=no")
        .arg("-o")
        .arg("KbdInteractiveAuthentication=no")
        .arg("-o")
        .arg("ConnectTimeout=5")
        .arg("-T")
        .arg(host)
        .arg("true");
    command
}

fn trimmed_output_preview(output: &[u8]) -> String {
    let text = String::from_utf8_lossy(output);
    let trimmed = text.trim();
    const MAX_CHARS: usize = 600;
    if trimmed.chars().count() <= MAX_CHARS {
        return trimmed.to_string();
    }
    let mut preview = trimmed.chars().take(MAX_CHARS).collect::<String>();
    preview.push_str("...");
    preview
}

fn ensure_seed_file(source: &Path, destination: &Path) -> Result<()> {
    if !source.exists() {
        fs::write(source, "").with_context(|| format!("failed to create {}", source.display()))?;
    }
    if destination.exists() {
        return Ok(());
    }
    fs::copy(source, destination).with_context(|| {
        format!(
            "failed to seed {} from {}",
            destination.display(),
            source.display()
        )
    })?;
    Ok(())
}

fn ensure_skill_seed(source_root: &Path, destination_root: &Path) -> Result<()> {
    let entries = fs::read_dir(source_root)
        .with_context(|| format!("failed to read {}", source_root.display()))?;
    let mut source_dirs = Vec::new();
    for entry in entries {
        let entry =
            entry.with_context(|| format!("failed to enumerate {}", source_root.display()))?;
        if entry
            .file_type()
            .with_context(|| format!("failed to inspect {}", entry.path().display()))?
            .is_dir()
        {
            source_dirs.push((
                entry.file_name().to_string_lossy().to_string(),
                entry.path(),
            ));
        }
    }
    source_dirs.sort_by(|left, right| left.0.cmp(&right.0));

    for (name, source_path) in source_dirs {
        let destination_path = destination_root.join(name);
        if destination_path.exists() {
            continue;
        }
        copy_directory_recursive(&source_path, &destination_path)?;
    }
    Ok(())
}

fn copy_directory_recursive(source: &Path, destination: &Path) -> Result<()> {
    fs::create_dir_all(destination)
        .with_context(|| format!("failed to create {}", destination.display()))?;
    let entries =
        fs::read_dir(source).with_context(|| format!("failed to read {}", source.display()))?;
    for entry in entries {
        let entry = entry.with_context(|| format!("failed to enumerate {}", source.display()))?;
        let source_path = entry.path();
        let destination_path = destination.join(entry.file_name());
        if entry
            .file_type()
            .with_context(|| format!("failed to inspect {}", source_path.display()))?
            .is_dir()
        {
            copy_directory_recursive(&source_path, &destination_path)?;
        } else {
            fs::copy(&source_path, &destination_path).with_context(|| {
                format!(
                    "failed to copy {} to {}",
                    source_path.display(),
                    destination_path.display()
                )
            })?;
        }
    }
    Ok(())
}

fn ensure_shared_link(target: &Path, link_path: &Path) -> Result<()> {
    if link_path.exists() {
        if let Ok(existing) = fs::read_link(link_path) {
            if normalize_link_target(link_path, &existing) == target {
                return Ok(());
            }
        }
        if link_path.is_dir() {
            return Ok(());
        }
        fs::remove_file(link_path)
            .with_context(|| format!("failed to remove {}", link_path.display()))?;
    }

    create_directory_link(target, link_path).or_else(|_| {
        fs::create_dir_all(link_path)
            .with_context(|| format!("failed to create {}", link_path.display()))
    })
}

fn normalize_link_target(link_path: &Path, raw_target: &Path) -> PathBuf {
    if raw_target.is_absolute() {
        raw_target.to_path_buf()
    } else {
        link_path
            .parent()
            .unwrap_or_else(|| Path::new("."))
            .join(raw_target)
    }
}

#[cfg(unix)]
fn create_directory_link(target: &Path, link_path: &Path) -> Result<()> {
    std::os::unix::fs::symlink(target, link_path).with_context(|| {
        format!(
            "failed to create symlink {} -> {}",
            link_path.display(),
            target.display()
        )
    })
}

#[cfg(windows)]
fn create_directory_link(target: &Path, link_path: &Path) -> Result<()> {
    std::os::windows::fs::symlink_dir(target, link_path).with_context(|| {
        format!(
            "failed to create symlink {} -> {}",
            link_path.display(),
            target.display()
        )
    })
}

#[cfg(test)]
mod tests {
    use super::{
        ensure_workspace_seed, is_sshfs_workspace_entry_name, passwordless_ssh_login_check_command,
        trimmed_output_preview, validate_remote_binding,
    };
    #[test]
    fn sshfs_workspace_entry_names_are_detected_by_prefix() {
        assert!(is_sshfs_workspace_entry_name("sshfs-web-main-000001"));
        assert!(is_sshfs_workspace_entry_name("sshfs-telegram-main-000001"));
        assert!(!is_sshfs_workspace_entry_name("web-main-000001"));
        assert!(!is_sshfs_workspace_entry_name("telegram-main-000001"));
    }

    #[test]
    fn workspace_shared_directory_points_to_runtime_shared() {
        let root = std::env::temp_dir().join(format!(
            "stellaclaw-workspace-shared-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&root);
        let conversation_a = root.join("conversations").join("telegram-main-000001");
        let conversation_b = root.join("conversations").join("telegram-main-000002");

        ensure_workspace_seed(&root, &conversation_a).expect("first workspace should seed");
        ensure_workspace_seed(&root, &conversation_b).expect("second workspace should seed");
        std::fs::write(
            conversation_a
                .join(".stellaclaw")
                .join("shared")
                .join("marker.txt"),
            "shared",
        )
        .expect("shared file should be writable");

        assert_eq!(
            std::fs::read_to_string(
                conversation_b
                    .join(".stellaclaw")
                    .join("shared")
                    .join("marker.txt")
            )
            .expect("shared file should be visible from other workspace"),
            "shared"
        );
        std::fs::remove_dir_all(&root).expect("temp dir should be cleaned");
    }

    #[test]
    fn sshfs_binding_rejects_unsafe_remote_hosts() {
        assert!(validate_remote_binding("demo-host", "~/repo").is_ok());
        assert!(validate_remote_binding("", "~/repo").is_err());
        assert!(validate_remote_binding("demo host", "~/repo").is_err());
        assert!(validate_remote_binding("demo;host", "~/repo").is_err());
    }

    #[test]
    fn passwordless_ssh_check_uses_batch_mode_without_password_auth() {
        let command = passwordless_ssh_login_check_command("demo-host");
        let args = command
            .get_args()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect::<Vec<_>>();
        assert_eq!(
            args,
            [
                "-o",
                "BatchMode=yes",
                "-o",
                "PasswordAuthentication=no",
                "-o",
                "KbdInteractiveAuthentication=no",
                "-o",
                "ConnectTimeout=5",
                "-T",
                "demo-host",
                "true",
            ]
        );
    }

    #[test]
    fn command_output_preview_is_trimmed_and_bounded() {
        let output = format!("  {}  ", "x".repeat(700));
        let preview = trimmed_output_preview(output.as_bytes());
        assert_eq!(preview.chars().count(), 603);
        assert!(preview.ends_with("..."));
    }
}
