use std::{
    fs,
    path::{Path, PathBuf},
    process::Command,
};

use anyhow::{anyhow, Context, Result};
use stellaclaw_core::session_actor::ToolRemoteMode;

const RUNDIR: &str = "rundir";
const SHARED_DIR: &str = "shared";
const PROFILE_DIR: &str = ".stellaclaw";
const SKILL_DIR: &str = ".skill";
const SHARED_SKILL_MEMORY_DIR: &str = "skill_memory";
const WORKSPACE_SKILL_MEMORY_DIR: &str = ".skill_memory";

pub fn ensure_workspace_for_remote_mode(
    workdir: &Path,
    conversation_root: &Path,
    conversation_id: &str,
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
            let workspace = ensure_sshfs_workspace(workdir, conversation_id, host, remote_path)?;
            ensure_workspace_seed(workdir, &workspace)?;
            Ok(workspace)
        }
    }
}

pub fn ensure_workspace_seed(workdir: &Path, conversation_root: &Path) -> Result<()> {
    let runtime_root = workdir.join(RUNDIR);
    let runtime_shared = runtime_root.join(SHARED_DIR);
    let runtime_profile = runtime_root.join(PROFILE_DIR);
    let runtime_skill = runtime_root.join(SKILL_DIR);
    let runtime_skill_memory = runtime_root.join(SHARED_SKILL_MEMORY_DIR);

    fs::create_dir_all(&runtime_shared)
        .with_context(|| format!("failed to create {}", runtime_shared.display()))?;
    fs::create_dir_all(&runtime_profile)
        .with_context(|| format!("failed to create {}", runtime_profile.display()))?;
    fs::create_dir_all(&runtime_skill)
        .with_context(|| format!("failed to create {}", runtime_skill.display()))?;
    fs::create_dir_all(&runtime_skill_memory)
        .with_context(|| format!("failed to create {}", runtime_skill_memory.display()))?;

    let workspace_profile = conversation_root.join(PROFILE_DIR);
    let workspace_skill = conversation_root.join(SKILL_DIR);
    let workspace_skill_memory = conversation_root.join(WORKSPACE_SKILL_MEMORY_DIR);
    fs::create_dir_all(&workspace_profile)
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
    ensure_shared_link(&runtime_shared, &conversation_root.join(SHARED_DIR))?;
    Ok(())
}

pub fn sshfs_workspace_root(workdir: &Path, conversation_id: &str) -> PathBuf {
    workdir
        .join("conversations")
        .join(format!("sshfs-{conversation_id}"))
}

pub fn unmount_sshfs_workspace(workdir: &Path, conversation_id: &str) -> Result<()> {
    let mountpoint = sshfs_workspace_root(workdir, conversation_id);
    if !mountpoint.exists() || !is_mountpoint(&mountpoint) {
        return Ok(());
    }

    let status = Command::new("umount").arg(&mountpoint).status();
    if status.map(|status| status.success()).unwrap_or(false) {
        return Ok(());
    }

    let status = Command::new("fusermount")
        .arg("-u")
        .arg(&mountpoint)
        .status();
    if status.map(|status| status.success()).unwrap_or(false) {
        return Ok(());
    }

    Err(anyhow!(
        "failed to unmount sshfs workspace {}",
        mountpoint.display()
    ))
}

fn ensure_sshfs_workspace(
    workdir: &Path,
    conversation_id: &str,
    host: &str,
    remote_path: &str,
) -> Result<PathBuf> {
    validate_sshfs_binding(host, remote_path)?;
    ensure_sshfs_available()?;

    let mountpoint = sshfs_workspace_root(workdir, conversation_id);
    if is_mountpoint(&mountpoint) {
        return Ok(mountpoint);
    }

    if mountpoint.exists() && directory_has_entries(&mountpoint)? {
        let backup = next_backup_path(&mountpoint)?;
        fs::rename(&mountpoint, &backup).with_context(|| {
            format!(
                "failed to move existing sshfs workspace {} to {}",
                mountpoint.display(),
                backup.display()
            )
        })?;
    }
    fs::create_dir_all(&mountpoint)
        .with_context(|| format!("failed to create {}", mountpoint.display()))?;

    let target = format!("{host}:{remote_path}");
    let output = Command::new("sshfs")
        .arg(&target)
        .arg(&mountpoint)
        .arg("-o")
        .arg("BatchMode=yes")
        .arg("-o")
        .arg("reconnect")
        .arg("-o")
        .arg("ServerAliveInterval=15")
        .arg("-o")
        .arg("ServerAliveCountMax=3")
        .output()
        .with_context(|| format!("failed to run sshfs for {target}"))?;
    if !output.status.success() {
        return Err(anyhow!(
            "sshfs mount failed for {}: {}",
            target,
            String::from_utf8_lossy(&output.stderr)
        ));
    }
    Ok(mountpoint)
}

fn validate_sshfs_binding(host: &str, remote_path: &str) -> Result<()> {
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

fn ensure_sshfs_available() -> Result<()> {
    let output = Command::new("sshfs")
        .arg("-V")
        .output()
        .context("failed to execute sshfs")?;
    if output.status.success() {
        return Ok(());
    }
    Err(anyhow!(
        "sshfs is required for /remote <host> <path>, but `sshfs -V` failed: {}",
        String::from_utf8_lossy(&output.stderr)
    ))
}

fn is_mountpoint(path: &Path) -> bool {
    if Command::new("mountpoint")
        .arg("-q")
        .arg(path)
        .status()
        .map(|status| status.success())
        .unwrap_or(false)
    {
        return true;
    }
    let Ok(canonical) = path.canonicalize() else {
        return false;
    };
    let Ok(output) = Command::new("mount").output() else {
        return false;
    };
    if !output.status.success() {
        return false;
    }
    let needle = format!(" on {} ", canonical.display());
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .any(|line| line.contains(&needle))
}

fn directory_has_entries(path: &Path) -> Result<bool> {
    let mut entries =
        fs::read_dir(path).with_context(|| format!("failed to read {}", path.display()))?;
    Ok(entries.next().transpose()?.is_some())
}

fn next_backup_path(path: &Path) -> Result<PathBuf> {
    let parent = path
        .parent()
        .ok_or_else(|| anyhow!("{} has no parent", path.display()))?;
    let name = path
        .file_name()
        .and_then(|value| value.to_str())
        .ok_or_else(|| anyhow!("{} has no file name", path.display()))?;
    for index in 1..=1000 {
        let candidate = parent.join(format!("{name}-local-{index:04}"));
        if !candidate.exists() {
            return Ok(candidate);
        }
    }
    Err(anyhow!(
        "failed to allocate backup path for {}",
        path.display()
    ))
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
    use super::sshfs_workspace_root;
    use std::path::Path;

    #[test]
    fn sshfs_workspace_uses_prefixed_conversation_directory() {
        assert_eq!(
            sshfs_workspace_root(Path::new("/tmp/stellaclaw"), "telegram-main-000001"),
            Path::new("/tmp/stellaclaw")
                .join("conversations")
                .join("sshfs-telegram-main-000001")
        );
    }
}
