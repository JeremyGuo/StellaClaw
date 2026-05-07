use std::{
    fs,
    io::{Cursor, Read},
    path::{Component, Path, PathBuf},
    process::{Command, Stdio},
    thread,
    time::{Duration, Instant},
};

use anyhow::{anyhow, Context, Result};
use flate2::read::GzDecoder;
use serde::Deserialize;

use super::{WorkdirUpgrader, WORKDIR_VERSION_0_12, WORKDIR_VERSION_0_13};
use crate::{config::StellaclawConfig, workspace::is_sshfs_workspace_entry_name};

const LOCAL_KEY_PATHS: [&str; 8] = [
    ".stellaclaw",
    ".output",
    ".skill",
    ".skill_memory",
    "attachments",
    "shared",
    "skill_memory",
    "STELLACLAW.md",
];
const REMOTE_COPY_TIMEOUT: Duration = Duration::from_secs(300);

pub struct SshfsWorkspaceMaterializeUpgrade;

impl WorkdirUpgrader for SshfsWorkspaceMaterializeUpgrade {
    fn from_version(&self) -> &'static str {
        WORKDIR_VERSION_0_12
    }

    fn to_version(&self) -> &'static str {
        WORKDIR_VERSION_0_13
    }

    fn upgrade(&self, workdir: &Path, _config: &StellaclawConfig) -> Result<()> {
        let conversations_root = workdir.join("conversations");
        if !conversations_root.exists() {
            return Ok(());
        }

        for conversation in fixed_ssh_conversations(&conversations_root)? {
            match materialize_remote_key_paths(
                &conversation.host,
                &conversation.cwd,
                &conversation.root,
            ) {
                Ok(copied) => {
                    if copied > 0 {
                        eprintln!(
                            "stellaclaw: materialized {copied} fixed SSH local key file(s) for {} from {}:{}",
                            conversation.conversation_id, conversation.host, conversation.cwd,
                        );
                    }
                }
                Err(remote_error) => {
                    let legacy_source =
                        conversations_root.join(format!("sshfs-{}", conversation.conversation_id));
                    let fallback_copied =
                        materialize_local_key_paths_from_directory(&legacy_source, &conversation.root)
                            .with_context(|| {
                                format!(
                                    "failed to materialize legacy sshfs fallback {} into {} after remote copy failed: {remote_error:#}",
                                    legacy_source.display(),
                                    conversation.root.display(),
                                )
                            })?;
                    if fallback_copied == 0 {
                        return Err(anyhow!(
                            "failed to materialize fixed SSH local key paths for conversation {} from {}:{}; legacy sshfs fallback {} had no key files to copy: {remote_error:#}",
                            conversation.conversation_id,
                            conversation.host,
                            conversation.cwd,
                            legacy_source.display(),
                        ));
                    }
                    eprintln!(
                        "stellaclaw: warning: remote key-path materialization failed for {}:{}; copied {fallback_copied} key file(s) from legacy sshfs fallback {}",
                        conversation.host,
                        conversation.cwd,
                        legacy_source.display(),
                    );
                }
            }
        }

        for entry in fs::read_dir(&conversations_root)
            .with_context(|| format!("failed to read {}", conversations_root.display()))?
        {
            let entry = entry
                .with_context(|| format!("failed to enumerate {}", conversations_root.display()))?;
            let name = entry.file_name().to_string_lossy().to_string();
            if !is_sshfs_workspace_entry_name(&name) {
                continue;
            }
            let Some(conversation_id) = name.strip_prefix("sshfs-") else {
                continue;
            };
            if conversation_id.is_empty() {
                continue;
            }
            let source = entry.path();
            let destination = conversations_root.join(conversation_id);
            if let Err(error) = materialize_local_key_paths_from_directory(&source, &destination) {
                eprintln!(
                    "stellaclaw: warning: failed to materialize old sshfs key paths {} into {}: {error:#}",
                    source.display(),
                    destination.display()
                );
            }
        }

        Ok(())
    }
}

#[derive(Debug)]
struct FixedSshConversation {
    conversation_id: String,
    root: PathBuf,
    host: String,
    cwd: String,
}

#[derive(Debug, Deserialize)]
struct ConversationStateDisk {
    #[serde(default)]
    conversation_id: Option<String>,
    #[serde(default)]
    tool_remote_mode: ToolRemoteModeDisk,
}

#[derive(Debug, Default, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum ToolRemoteModeDisk {
    #[default]
    Selectable,
    FixedSsh {
        host: String,
        #[serde(default)]
        cwd: Option<String>,
    },
}

fn fixed_ssh_conversations(conversations_root: &Path) -> Result<Vec<FixedSshConversation>> {
    let mut conversations = Vec::new();
    for entry in fs::read_dir(conversations_root)
        .with_context(|| format!("failed to read {}", conversations_root.display()))?
    {
        let entry = entry
            .with_context(|| format!("failed to enumerate {}", conversations_root.display()))?;
        let name = entry.file_name().to_string_lossy().to_string();
        if is_sshfs_workspace_entry_name(&name) {
            continue;
        }
        let root = entry.path();
        if !root.is_dir() {
            continue;
        }
        let state_path = root.join("conversation.json");
        if !state_path.is_file() {
            continue;
        }
        let raw = fs::read_to_string(&state_path)
            .with_context(|| format!("failed to read {}", state_path.display()))?;
        let state: ConversationStateDisk = serde_json::from_str(&raw)
            .with_context(|| format!("failed to parse {}", state_path.display()))?;
        let ToolRemoteModeDisk::FixedSsh { host, cwd } = state.tool_remote_mode else {
            continue;
        };
        let conversation_id = state
            .conversation_id
            .filter(|value| !value.trim().is_empty())
            .unwrap_or_else(|| name.clone());
        let host = host.trim().to_string();
        let cwd = cwd.unwrap_or_default().trim().to_string();
        if host.is_empty() || cwd.is_empty() {
            return Err(anyhow!(
                "conversation {} has fixed SSH remote mode with empty host or cwd",
                conversation_id
            ));
        }
        conversations.push(FixedSshConversation {
            conversation_id,
            root,
            host,
            cwd,
        });
    }
    conversations.sort_by(|left, right| left.conversation_id.cmp(&right.conversation_id));
    Ok(conversations)
}

fn materialize_remote_key_paths(host: &str, cwd: &str, destination: &Path) -> Result<usize> {
    validate_remote_host(host)?;
    let script = remote_key_paths_tar_script();
    let remote_command = format!(
        "cd {} && python3 -c {}",
        shell_quote(cwd),
        shell_quote(&script),
    );
    let tmp_root = destination
        .join(".stellaclaw")
        .join("upgrade_remote_key_paths_tmp")
        .join(format!("{}", current_nanos()));
    fs::create_dir_all(&tmp_root)
        .with_context(|| format!("failed to create {}", tmp_root.display()))?;
    let archive_path = tmp_root.join("key_paths.tar.gz");
    if let Err(error) =
        run_ssh_archive_to_path(host, &remote_command, &archive_path, REMOTE_COPY_TIMEOUT)
    {
        let _ = fs::remove_dir_all(&tmp_root);
        return Err(error);
    }
    let archive_data = match fs::read(&archive_path) {
        Ok(data) => data,
        Err(error) => {
            let _ = fs::remove_dir_all(&tmp_root);
            return Err(error)
                .with_context(|| format!("failed to read {}", archive_path.display()));
        }
    };
    fs::create_dir_all(destination)
        .with_context(|| format!("failed to create {}", destination.display()))?;
    let result = materialize_key_paths_from_tar_gz(&archive_data, destination);
    let _ = fs::remove_dir_all(&tmp_root);
    result
}

fn materialize_key_paths_from_tar_gz(data: &[u8], destination: &Path) -> Result<usize> {
    let decoder = GzDecoder::new(Cursor::new(data));
    let mut archive = tar::Archive::new(decoder);
    let mut copied = 0usize;
    for entry in archive
        .entries()
        .with_context(|| "failed to read remote key-path archive")?
    {
        let mut entry = entry.with_context(|| "failed to read remote key-path archive entry")?;
        let relative = normalized_key_path(&entry.path()?.to_path_buf())?;
        let destination_path = destination.join(&relative);
        let entry_type = entry.header().entry_type();
        if entry_type.is_symlink() || entry_type.is_hard_link() {
            continue;
        }
        if entry_type.is_dir() {
            fs::create_dir_all(&destination_path)
                .with_context(|| format!("failed to create {}", destination_path.display()))?;
            continue;
        }
        if !entry_type.is_file() {
            continue;
        }
        let mut bytes = Vec::new();
        entry
            .read_to_end(&mut bytes)
            .with_context(|| format!("failed to read archive entry {}", relative.display()))?;
        if write_non_overwriting_bytes(&bytes, &destination_path)? {
            copied += 1;
        }
    }
    Ok(copied)
}

fn materialize_local_key_paths_from_directory(source: &Path, destination: &Path) -> Result<usize> {
    if !source.exists() || !source.is_dir() {
        return Ok(0);
    }
    fs::create_dir_all(destination)
        .with_context(|| format!("failed to create {}", destination.display()))?;
    let mut copied = 0usize;
    for key_path in LOCAL_KEY_PATHS {
        let source_path = source.join(key_path);
        if source_path.symlink_metadata().is_err() {
            continue;
        }
        copied += copy_entry_without_following_symlink(&source_path, &destination.join(key_path))?;
    }
    Ok(copied)
}

fn copy_directory_contents(source: &Path, destination: &Path) -> Result<usize> {
    let mut copied = 0usize;
    for entry in
        fs::read_dir(source).with_context(|| format!("failed to read {}", source.display()))?
    {
        let entry = entry.with_context(|| format!("failed to enumerate {}", source.display()))?;
        let source_path = entry.path();
        let destination_path = destination.join(entry.file_name());
        copied += copy_entry_without_following_symlink(&source_path, &destination_path)?;
    }
    Ok(copied)
}

fn copy_entry_without_following_symlink(source: &Path, destination: &Path) -> Result<usize> {
    let metadata = fs::symlink_metadata(source)
        .with_context(|| format!("failed to inspect {}", source.display()))?;
    if metadata.file_type().is_symlink() {
        return Ok(0);
    }
    if metadata.is_dir() {
        fs::create_dir_all(destination)
            .with_context(|| format!("failed to create {}", destination.display()))?;
        return copy_directory_contents(source, destination);
    }
    if !metadata.is_file() {
        return Ok(0);
    }

    let bytes = fs::read(source).with_context(|| format!("failed to read {}", source.display()))?;
    Ok(write_non_overwriting_bytes(&bytes, destination)? as usize)
}

fn write_non_overwriting_bytes(bytes: &[u8], destination: &Path) -> Result<bool> {
    let Some(target) = non_overwriting_destination_for_bytes(bytes, destination)? else {
        return Ok(false);
    };
    if let Some(parent) = target.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }
    fs::write(&target, bytes).with_context(|| format!("failed to write {}", target.display()))?;
    Ok(true)
}

fn non_overwriting_destination_for_bytes(
    bytes: &[u8],
    destination: &Path,
) -> Result<Option<PathBuf>> {
    if !destination.exists() {
        return Ok(Some(destination.to_path_buf()));
    }
    if destination.is_file() {
        let existing = fs::read(destination)
            .with_context(|| format!("failed to read {}", destination.display()))?;
        if existing == bytes {
            return Ok(None);
        }
    }
    let parent = destination.parent().unwrap_or_else(|| Path::new("."));
    let file_name = destination
        .file_name()
        .map(|value| value.to_string_lossy().to_string())
        .unwrap_or_else(|| "remote-file".to_string());
    for index in 1..=1000 {
        let candidate = parent.join(format!("{file_name}.remote-copy-{index:04}"));
        if !candidate.exists() {
            return Ok(Some(candidate));
        }
    }
    Ok(Some(parent.join(format!("{file_name}.remote-copy"))))
}

fn normalized_key_path(path: &Path) -> Result<PathBuf> {
    if path.is_absolute() {
        return Err(anyhow!(
            "remote archive path must be relative: {}",
            path.display()
        ));
    }
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::Normal(value) => normalized.push(value),
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => {
                return Err(anyhow!(
                    "remote archive path must stay inside workspace: {}",
                    path.display()
                ));
            }
        }
    }
    let Some(first) = normalized.components().next() else {
        return Err(anyhow!("remote archive path must not be empty"));
    };
    let Component::Normal(first) = first else {
        return Err(anyhow!("remote archive path must be normal"));
    };
    if !LOCAL_KEY_PATHS
        .iter()
        .any(|key| first.to_string_lossy() == *key)
    {
        return Err(anyhow!(
            "remote archive contains non-key path {}",
            normalized.display()
        ));
    }
    Ok(normalized)
}

fn validate_remote_host(host: &str) -> Result<()> {
    if host.trim().is_empty()
        || !host
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '.' | '_' | '-'))
    {
        return Err(anyhow!(
            "remote host must be a safe ~/.ssh/config Host alias"
        ));
    }
    Ok(())
}

fn run_ssh_archive_to_path(
    host: &str,
    remote_command: &str,
    archive_path: &Path,
    timeout: Duration,
) -> Result<()> {
    let archive_file = fs::File::create(archive_path)
        .with_context(|| format!("failed to create {}", archive_path.display()))?;
    let mut child = Command::new("ssh")
        .arg("-o")
        .arg("BatchMode=yes")
        .arg("-o")
        .arg("ConnectTimeout=10")
        .arg("-T")
        .arg(host)
        .arg(remote_command)
        .stdin(Stdio::null())
        .stdout(Stdio::from(archive_file))
        .stderr(Stdio::piped())
        .spawn()
        .with_context(|| "failed to spawn SSH for remote key-path materialization")?;
    let deadline = Instant::now() + timeout;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                let output = child
                    .wait_with_output()
                    .with_context(|| "failed to collect SSH key-path materialization output")?;
                if status.success() {
                    return Ok(());
                }
                return Err(anyhow!(
                    "SSH key-path materialization exited with {}; stderr: {}",
                    status.code().unwrap_or(-1),
                    String::from_utf8_lossy(&output.stderr).trim()
                ));
            }
            Ok(None) => {
                if Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(anyhow!(
                        "SSH key-path materialization timed out after {} seconds",
                        timeout.as_secs()
                    ));
                }
                thread::sleep(Duration::from_millis(100));
            }
            Err(error) => {
                return Err(anyhow!(
                    "failed to wait for SSH key-path materialization: {error}"
                ))
            }
        }
    }
}

fn shell_quote(value: &str) -> String {
    if value.is_empty() {
        return "''".to_string();
    }
    let escaped = value.replace('\'', "'\"'\"'");
    format!("'{escaped}'")
}

fn remote_key_paths_tar_script() -> String {
    let key_paths = serde_json::to_string(&LOCAL_KEY_PATHS).unwrap_or_else(|_| "[]".to_string());
    format!(
        r#"
import pathlib
import sys
import tarfile

key_paths = {key_paths}
with tarfile.open(fileobj=sys.stdout.buffer, mode="w:gz", dereference=False) as archive:
    for raw in key_paths:
        path = pathlib.Path(raw)
        try:
            path.lstat()
        except FileNotFoundError:
            continue
        archive.add(path, arcname=raw, recursive=True)
"#,
        key_paths = key_paths,
    )
}

fn current_nanos() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or_default()
}

#[cfg(test)]
fn test_root(name: &str) -> PathBuf {
    std::env::temp_dir().join(format!("stellaclaw-{name}-{}", current_nanos()))
}

#[cfg(test)]
fn test_config() -> StellaclawConfig {
    use crate::config::*;
    use std::collections::BTreeMap;
    StellaclawConfig {
        version: LATEST_CONFIG_VERSION.to_string(),
        agent_server: AgentServerConfig::default(),
        default_profile: None,
        channels: Vec::new(),
        models: BTreeMap::new(),
        session_defaults: SessionDefaults::default(),
        memory: crate::config::MemoryConfig::default(),
        sandbox: SandboxConfig::default(),
        skill_sync: Vec::new(),
        available_agent_models: Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn materializes_only_local_key_paths_without_overwriting_local_files() {
        let root = test_root("workdir-upgrade-v0_12");
        let _ = fs::remove_dir_all(&root);
        let conversations = root.join("conversations");
        let sshfs = conversations.join("sshfs-web-main-000001");
        let local = conversations.join("web-main-000001");
        fs::create_dir_all(sshfs.join("src")).unwrap();
        fs::create_dir_all(sshfs.join("attachments")).unwrap();
        fs::create_dir_all(sshfs.join(".skill").join("demo")).unwrap();
        fs::create_dir_all(sshfs.join(".skill_memory")).unwrap();
        fs::create_dir_all(&local).unwrap();
        fs::write(sshfs.join("src").join("main.rs"), "remote").unwrap();
        fs::write(
            sshfs.join("attachments").join("report.txt"),
            "remote attachment",
        )
        .unwrap();
        fs::write(
            sshfs.join(".skill").join("demo").join("SKILL.md"),
            "remote skill",
        )
        .unwrap();
        fs::write(
            sshfs.join(".skill_memory").join("skill.txt"),
            "remote skill",
        )
        .unwrap();
        fs::write(sshfs.join("STELLACLAW.md"), "remote memory").unwrap();
        fs::write(local.join("STELLACLAW.md"), "local memory").unwrap();

        SshfsWorkspaceMaterializeUpgrade
            .upgrade(&root, &test_config())
            .unwrap();

        assert!(!local.join("src").join("main.rs").exists());
        assert_eq!(
            fs::read_to_string(local.join("attachments").join("report.txt")).unwrap(),
            "remote attachment"
        );
        assert_eq!(
            fs::read_to_string(local.join(".skill_memory").join("skill.txt")).unwrap(),
            "remote skill"
        );
        assert_eq!(
            fs::read_to_string(local.join(".skill").join("demo").join("SKILL.md")).unwrap(),
            "remote skill"
        );
        assert_eq!(
            fs::read_to_string(local.join("STELLACLAW.md")).unwrap(),
            "local memory"
        );
        assert_eq!(
            fs::read_to_string(local.join("STELLACLAW.md.remote-copy-0001")).unwrap(),
            "remote memory"
        );
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn finds_fixed_ssh_conversations_from_state() {
        let root = test_root("workdir-upgrade-v0_12-fixed");
        let conversations = root.join("conversations");
        let local = conversations.join("web-main-000001");
        fs::create_dir_all(&local).unwrap();
        fs::write(
            local.join("conversation.json"),
            r#"{
                "conversation_id": "web-main-000001",
                "tool_remote_mode": {
                    "type": "fixed_ssh",
                    "host": "demo-host",
                    "cwd": "/srv/app"
                }
            }"#,
        )
        .unwrap();

        let found = fixed_ssh_conversations(&conversations).unwrap();

        assert_eq!(found.len(), 1);
        assert_eq!(found[0].host, "demo-host");
        assert_eq!(found[0].cwd, "/srv/app");
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn fixed_ssh_conversation_id_falls_back_to_directory_name() {
        let root = test_root("workdir-upgrade-v0_12-fixed-no-id");
        let conversations = root.join("conversations");
        let local = conversations.join("web-main-000001");
        fs::create_dir_all(&local).unwrap();
        fs::write(
            local.join("conversation.json"),
            r#"{
                "tool_remote_mode": {
                    "type": "fixed_ssh",
                    "host": "demo-host",
                    "cwd": "/srv/app"
                }
            }"#,
        )
        .unwrap();

        let found = fixed_ssh_conversations(&conversations).unwrap();

        assert_eq!(found.len(), 1);
        assert_eq!(found[0].conversation_id, "web-main-000001");
        let _ = fs::remove_dir_all(root);
    }
}
