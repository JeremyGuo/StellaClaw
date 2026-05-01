use std::{
    fs, io,
    path::{Path, PathBuf},
    process::Command,
    thread,
    time::{Duration, Instant},
};

use anyhow::{anyhow, Context, Result};
use stellaclaw_core::session_actor::ToolRemoteMode;

const RUNDIR: &str = "rundir";
const STELLACLAW_DIR: &str = ".stellaclaw";
const SHARED_DIR: &str = "stellaclaw_shared";
const SKILL_DIR: &str = "skill";
const SHARED_SKILL_MEMORY_DIR: &str = "skill_memory";

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
            let workspace = if is_localhost_ssh_host(host) {
                ensure_local_workspace(workdir, conversation_id, remote_path)?
            } else {
                let ws = ensure_sshfs_workspace(workdir, conversation_id, host, remote_path)?;
                // If the mount is disconnected, remount and retry.
                match ws.metadata() {
                    Ok(_) => ws,
                    Err(error) => {
                        let error: anyhow::Error = error.into();
                        if is_disconnected_mount_error(&error) {
                            let _ = unmount_sshfs_workspace(workdir, conversation_id);
                            ensure_sshfs_workspace(workdir, conversation_id, host, remote_path)?
                        } else {
                            return Err(error);
                        }
                    }
                }
            };
            ensure_workspace_seed(workdir, conversation_root)?;
            Ok(workspace)
        }
    }
}

pub fn ensure_workspace_seed(workdir: &Path, data_root: &Path) -> Result<()> {
    migrate_legacy_workspace_layout(data_root);

    let runtime_root = workdir.join(RUNDIR);
    let runtime_shared = runtime_root.join("shared");
    let runtime_profile = runtime_root.join(STELLACLAW_DIR);
    let runtime_skill = runtime_root.join(".skill");
    let runtime_skill_memory = runtime_root.join(SHARED_SKILL_MEMORY_DIR);

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

    // shared -> .stellaclaw/stellaclaw_shared
    let old_shared = data_root.join("shared");
    let new_shared = stellaclaw_dir.join(SHARED_DIR);
    if old_shared.symlink_metadata().is_ok() && !new_shared.exists() {
        let _ = fs::create_dir_all(&stellaclaw_dir);
        let _ = fs::rename(&old_shared, &new_shared);
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

pub fn sshfs_workspace_root(workdir: &Path, conversation_id: &str) -> PathBuf {
    workdir
        .join("conversations")
        .join(format!("sshfs-{conversation_id}"))
}

pub fn is_sshfs_workspace_entry_name(name: &str) -> bool {
    name.starts_with("sshfs-")
}

pub fn unmount_sshfs_workspace(workdir: &Path, conversation_id: &str) -> Result<()> {
    let mountpoint = sshfs_workspace_root(workdir, conversation_id);

    // If the workspace is a symlink (localhost workspace), just remove the link.
    if mountpoint
        .symlink_metadata()
        .map_or(false, |m| m.file_type().is_symlink())
    {
        fs::remove_file(&mountpoint)
            .with_context(|| format!("failed to remove symlink {}", mountpoint.display()))?;
        return Ok(());
    }

    // Kill tracked sshfs process before attempting unmount so the FUSE daemon
    // is gone and the kernel releases the mount immediately.
    kill_tracked_sshfs_process(workdir, conversation_id);

    if !is_mountpoint(&mountpoint) {
        return Ok(());
    }

    let status = Command::new("fusermount")
        .arg("-uz")
        .arg(&mountpoint)
        .status();
    if status.map(|status| status.success()).unwrap_or(false) {
        return Ok(());
    }

    let status = Command::new("umount").arg("-l").arg(&mountpoint).status();
    if status.map(|status| status.success()).unwrap_or(false) {
        return Ok(());
    }

    Err(anyhow!(
        "failed to unmount sshfs workspace {}",
        mountpoint.display()
    ))
}

// ---------------------------------------------------------------------------
// Localhost detection and local workspace (symlink-based)
// ---------------------------------------------------------------------------

/// Check whether an SSH host alias resolves to the local machine by running
/// `ssh -G <host>` and inspecting the resolved `hostname` field.  Returns
/// `true` for `localhost`, `127.0.0.1`, `::1`, and any alias that resolves to
/// one of these.
fn is_localhost_ssh_host(host: &str) -> bool {
    use std::process::Stdio;
    let Ok(output) = Command::new("ssh")
        .args(["-G", host])
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
    else {
        return false;
    };
    if !output.status.success() {
        return false;
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    for line in stdout.lines() {
        let line = line.trim();
        if let Some(value) = line.strip_prefix("hostname ") {
            let value = value.trim();
            return matches!(value, "localhost" | "127.0.0.1" | "::1");
        }
    }
    false
}

/// For localhost targets we create a symlink instead of an sshfs mount.  This
/// avoids the FUSE layer entirely: no recursive mount loops, no zombie sshfs
/// daemons, no D-state kernel hangs.
fn ensure_local_workspace(
    workdir: &Path,
    conversation_id: &str,
    remote_path: &str,
) -> Result<PathBuf> {
    let target = PathBuf::from(remote_path);
    if !target.is_absolute() {
        return Err(anyhow!(
            "localhost remote path must be absolute, got: {remote_path}"
        ));
    }
    if !target.exists() {
        return Err(anyhow!(
            "localhost remote path does not exist: {remote_path}"
        ));
    }

    let link_path = sshfs_workspace_root(workdir, conversation_id);

    // If the symlink already points to the right place, reuse it.
    if let Ok(existing_target) = fs::read_link(&link_path) {
        if existing_target == target {
            return Ok(link_path);
        }
        // Wrong target — remove and recreate.
        let _ = fs::remove_file(&link_path);
    }

    // If there's a stale mount or directory, clean it up first.
    if link_path.exists() || link_path.symlink_metadata().is_ok() {
        let _ = unmount_sshfs_workspace(workdir, conversation_id);
        if link_path.exists() {
            let _ = fs::remove_dir_all(&link_path);
        }
    }

    #[cfg(unix)]
    std::os::unix::fs::symlink(&target, &link_path).with_context(|| {
        format!(
            "failed to create symlink {} -> {}",
            link_path.display(),
            target.display()
        )
    })?;

    #[cfg(windows)]
    std::os::windows::fs::symlink_dir(&target, &link_path).with_context(|| {
        format!(
            "failed to create symlink {} -> {}",
            link_path.display(),
            target.display()
        )
    })?;

    Ok(link_path)
}

// ---------------------------------------------------------------------------
// sshfs workspace (remote hosts)
// ---------------------------------------------------------------------------

fn ensure_sshfs_workspace(
    workdir: &Path,
    conversation_id: &str,
    host: &str,
    remote_path: &str,
) -> Result<PathBuf> {
    validate_sshfs_binding(host, remote_path)?;
    ensure_passwordless_ssh_login(host)?;
    ensure_sshfs_available()?;

    let mountpoint = sshfs_workspace_root(workdir, conversation_id);
    if is_mountpoint(&mountpoint) {
        if sshfs_workspace_is_usable(&mountpoint) {
            return Ok(mountpoint);
        }
        unmount_sshfs_workspace(workdir, conversation_id)?;
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
    create_mountpoint_directory(&mountpoint)?;

    let target = format!("{host}:{remote_path}");
    let mut command = Command::new("sshfs");
    command.arg(&target).arg(&mountpoint);
    add_sshfs_mount_options(&mut command);
    let output = command
        .output()
        .with_context(|| format!("failed to run sshfs for {target}"))?;
    if !output.status.success() {
        return Err(anyhow!(
            "sshfs mount failed for {}: {}",
            target,
            String::from_utf8_lossy(&output.stderr)
        ));
    }

    // Record the sshfs PID so we can kill it during cleanup.
    record_sshfs_pid(workdir, conversation_id, &mountpoint);

    Ok(mountpoint)
}

fn add_sshfs_mount_options(command: &mut Command) {
    for option in [
        "BatchMode=yes",
        "reconnect",
        "ServerAliveInterval=15",
        "ServerAliveCountMax=3",
        "dir_cache=yes",
        "dcache_timeout=60",
        "dcache_stat_timeout=60",
        "dcache_link_timeout=60",
        "dcache_dir_timeout=60",
        "entry_timeout=60",
        "attr_timeout=60",
        "negative_timeout=5",
        "auto_cache",
        "remember=60",
    ] {
        command.arg("-o").arg(option);
    }
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

// ---------------------------------------------------------------------------
// sshfs PID tracking
// ---------------------------------------------------------------------------

/// Path to the file that stores the sshfs daemon PID for a conversation.
fn sshfs_pid_path(workdir: &Path, conversation_id: &str) -> PathBuf {
    workdir
        .join("conversations")
        .join(conversation_id)
        .join(".sshfs_pid")
}

/// After a successful sshfs mount, find the sshfs process that owns the
/// mountpoint and record its PID.
fn record_sshfs_pid(workdir: &Path, conversation_id: &str, mountpoint: &Path) {
    use std::process::Stdio;
    // `pgrep -f` with the mountpoint path to find the sshfs process.
    let Ok(output) = Command::new("pgrep")
        .args(["-f", &format!("sshfs.*{}", mountpoint.display())])
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
    else {
        return;
    };
    if !output.status.success() {
        return;
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    // Take the last PID (most recently started sshfs for this mount).
    if let Some(pid_str) = stdout
        .lines()
        .last()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        let pid_file = sshfs_pid_path(workdir, conversation_id);
        let _ = fs::write(&pid_file, pid_str);
    }
}

/// Kill a previously tracked sshfs daemon.  Best-effort; failures are ignored.
fn kill_tracked_sshfs_process(workdir: &Path, conversation_id: &str) {
    let pid_file = sshfs_pid_path(workdir, conversation_id);
    let Ok(pid_str) = fs::read_to_string(&pid_file) else {
        return;
    };
    let _ = fs::remove_file(&pid_file);
    let pid_str = pid_str.trim();
    if pid_str.is_empty() {
        return;
    }
    // SIGKILL the sshfs daemon so the FUSE mount becomes immediately dead
    // (returns ENOTCONN) rather than hanging.
    let _ = Command::new("kill").args(["-9", pid_str]).status();
    // Give the kernel a moment to process the signal.
    thread::sleep(Duration::from_millis(100));
}

// ---------------------------------------------------------------------------
// Mountpoint and health checks (non-blocking)
// ---------------------------------------------------------------------------

/// Check whether a path is a FUSE/mount point.  Uses `timeout` to avoid
/// blocking on dead FUSE mounts, and polls with `try_wait` to guarantee this
/// function always returns within the deadline.
fn is_mountpoint(path: &Path) -> bool {
    // Fast path: if the path doesn't exist at all, it's not a mountpoint.
    // Use symlink_metadata to avoid following into broken FUSE mounts.
    if fs::symlink_metadata(path).is_err() {
        return false;
    }

    if run_with_timeout(&["mountpoint", "-q"], path, Duration::from_secs(3)) == Some(true) {
        return true;
    }

    // Fallback: parse `mount` output (does not touch the mountpoint itself).
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

fn sshfs_workspace_is_usable(path: &Path) -> bool {
    sshfs_health_check(path, Duration::from_secs(5))
}

/// Check whether an sshfs mountpoint is responsive by running `stat` with a
/// timeout.  Returns `true` when the mountpoint responds within the deadline,
/// `false` otherwise.
///
/// This function is guaranteed to return within `timeout + 1s` even when the
/// FUSE mount is dead and processes enter uninterruptible sleep.
pub fn sshfs_health_check(path: &Path, timeout: Duration) -> bool {
    run_with_timeout(&["stat"], path, timeout) == Some(true)
}

/// Run `[args..., path]` as a child process with a deadline.  Returns
/// `Some(true)` on success, `Some(false)` on non-zero exit, `None` on timeout
/// or error.
///
/// Unlike the previous approach of `timeout(1) <cmd>` + `child.wait()`, this
/// uses `child.try_wait()` polling.  If the child is stuck in D-state (which
/// makes even `timeout`'s SIGKILL ineffective), we stop waiting and return
/// `None`.  The stuck process is orphaned but harmless — it will be reaped
/// when the FUSE mount is eventually cleaned up.
fn run_with_timeout(args: &[&str], path: &Path, timeout: Duration) -> Option<bool> {
    use std::process::Stdio;
    let mut child = Command::new(args[0])
        .args(&args[1..])
        .arg(path)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;

    let deadline = Instant::now() + timeout;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return Some(status.success()),
            Ok(None) => {
                if Instant::now() >= deadline {
                    // Timed out.  Try to kill, but don't wait — the process
                    // may be in D-state and immune to signals.
                    let _ = child.kill();
                    return None;
                }
                thread::sleep(Duration::from_millis(200));
            }
            Err(_) => return None,
        }
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn directory_has_entries(path: &Path) -> Result<bool> {
    let mut entries =
        fs::read_dir(path).with_context(|| format!("failed to read {}", path.display()))?;
    Ok(entries.next().transpose()?.is_some())
}

fn create_mountpoint_directory(path: &Path) -> Result<()> {
    match fs::create_dir_all(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists && path.is_dir() => Ok(()),
        Err(error) => Err(error).with_context(|| format!("failed to create {}", path.display())),
    }
}

fn is_disconnected_mount_error(error: &anyhow::Error) -> bool {
    error.chain().any(|cause| {
        cause
            .downcast_ref::<io::Error>()
            .is_some_and(|error| error.raw_os_error() == Some(107))
            || cause
                .to_string()
                .contains("Transport endpoint is not connected")
    })
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
    use super::{
        add_sshfs_mount_options, ensure_workspace_seed, is_sshfs_workspace_entry_name,
        passwordless_ssh_login_check_command, sshfs_workspace_root, trimmed_output_preview,
        validate_sshfs_binding,
    };
    use std::{path::Path, process::Command};

    #[test]
    fn sshfs_workspace_uses_prefixed_conversation_directory() {
        assert_eq!(
            sshfs_workspace_root(Path::new("/tmp/stellaclaw"), "telegram-main-000001"),
            Path::new("/tmp/stellaclaw")
                .join("conversations")
                .join("sshfs-telegram-main-000001")
        );
    }

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
                .join("stellaclaw_shared")
                .join("marker.txt"),
            "shared",
        )
        .expect("shared file should be writable");

        assert_eq!(
            std::fs::read_to_string(
                conversation_b
                    .join(".stellaclaw")
                    .join("stellaclaw_shared")
                    .join("marker.txt")
            )
            .expect("shared file should be visible from other workspace"),
            "shared"
        );
        std::fs::remove_dir_all(&root).expect("temp dir should be cleaned");
    }

    #[test]
    fn sshfs_binding_rejects_unsafe_remote_hosts() {
        assert!(validate_sshfs_binding("demo-host", "~/repo").is_ok());
        assert!(validate_sshfs_binding("", "~/repo").is_err());
        assert!(validate_sshfs_binding("demo host", "~/repo").is_err());
        assert!(validate_sshfs_binding("demo;host", "~/repo").is_err());
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
    fn sshfs_mount_options_enable_local_caches() {
        let mut command = Command::new("sshfs");
        add_sshfs_mount_options(&mut command);
        let args = command
            .get_args()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect::<Vec<_>>();

        assert!(has_mount_option(&args, "dir_cache=yes"));
        assert!(has_mount_option(&args, "dcache_timeout=60"));
        assert!(has_mount_option(&args, "attr_timeout=60"));
        assert!(has_mount_option(&args, "auto_cache"));
        assert!(has_mount_option(&args, "remember=60"));
    }

    #[test]
    fn command_output_preview_is_trimmed_and_bounded() {
        let output = format!("  {}  ", "x".repeat(700));
        let preview = trimmed_output_preview(output.as_bytes());
        assert_eq!(preview.chars().count(), 603);
        assert!(preview.ends_with("..."));
    }

    fn has_mount_option(args: &[String], option: &str) -> bool {
        args.windows(2)
            .any(|pair| pair[0] == "-o" && pair[1] == option)
    }
}
