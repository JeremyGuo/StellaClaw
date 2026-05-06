use std::{
    fs,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result};

use super::{WorkdirUpgrader, LATEST_WORKDIR_VERSION, WORKDIR_VERSION_0_13};
use crate::{config::StellaclawConfig, workspace::is_sshfs_workspace_entry_name};

pub struct StellaclawConversationSpecialPathUpgrade;

impl WorkdirUpgrader for StellaclawConversationSpecialPathUpgrade {
    fn from_version(&self) -> &'static str {
        WORKDIR_VERSION_0_13
    }

    fn to_version(&self) -> &'static str {
        LATEST_WORKDIR_VERSION
    }

    fn upgrade(&self, workdir: &Path, _config: &StellaclawConfig) -> Result<()> {
        migrate_runtime_special_paths(workdir)?;

        let conversations_root = workdir.join("conversations");
        if !conversations_root.exists() {
            return Ok(());
        }

        for entry in fs::read_dir(&conversations_root)
            .with_context(|| format!("failed to read {}", conversations_root.display()))?
        {
            let entry = entry
                .with_context(|| format!("failed to enumerate {}", conversations_root.display()))?;
            let name = entry.file_name().to_string_lossy().to_string();
            if is_sshfs_workspace_entry_name(&name) {
                continue;
            }
            let root = entry.path();
            if root.is_dir() {
                migrate_conversation_special_paths(workdir, &root)?;
            }
        }

        Ok(())
    }
}

fn migrate_runtime_special_paths(workdir: &Path) -> Result<()> {
    let runtime_root = workdir.join("rundir");
    let stellaclaw_root = runtime_root.join(".stellaclaw");
    fs::create_dir_all(&stellaclaw_root)
        .with_context(|| format!("failed to create {}", stellaclaw_root.display()))?;

    for (old, new) in [
        (".skill", ".stellaclaw/skill"),
        ("skill_memory", ".stellaclaw/skill_memory"),
    ] {
        let source = runtime_root.join(old);
        if source.symlink_metadata().is_err() {
            continue;
        }
        migrate_entry_without_following_symlink(&source, &runtime_root.join(new))?;
    }
    Ok(())
}

fn migrate_conversation_special_paths(workdir: &Path, root: &Path) -> Result<()> {
    let stellaclaw_root = root.join(".stellaclaw");
    fs::create_dir_all(&stellaclaw_root)
        .with_context(|| format!("failed to create {}", stellaclaw_root.display()))?;

    for (old, new) in [
        ("STELLACLAW.md", ".stellaclaw/STELLACLAW.md"),
        (".output", ".stellaclaw/output"),
        ("attachments", ".stellaclaw/attachments"),
        ("shared", ".stellaclaw/shared"),
        (".skill", ".stellaclaw/skill"),
        (".skill_memory", ".stellaclaw/skill_memory"),
        ("skill_memory", ".stellaclaw/skill_memory"),
        (".stellaclaw/stellaclaw_shared", ".stellaclaw/shared"),
    ] {
        let source = root.join(old);
        if source.symlink_metadata().is_err() {
            continue;
        }
        let destination = root.join(new);
        migrate_entry_without_following_symlink(&source, &destination)?;
    }

    ensure_conversation_skill_memory_link(workdir, root)?;
    Ok(())
}

fn ensure_conversation_skill_memory_link(workdir: &Path, root: &Path) -> Result<()> {
    let link_path = root.join(".stellaclaw").join("skill_memory");
    let target = workdir
        .join("rundir")
        .join(".stellaclaw")
        .join("skill_memory");
    fs::create_dir_all(&target)
        .with_context(|| format!("failed to create {}", target.display()))?;

    let Ok(metadata) = fs::symlink_metadata(&link_path) else {
        return create_directory_link(&target, &link_path);
    };
    if metadata.file_type().is_symlink() {
        fs::remove_file(&link_path)
            .with_context(|| format!("failed to remove {}", link_path.display()))?;
        return create_directory_link(&target, &link_path);
    }
    Ok(())
}

fn migrate_entry_without_following_symlink(source: &Path, destination: &Path) -> Result<()> {
    let metadata = fs::symlink_metadata(source)
        .with_context(|| format!("failed to inspect {}", source.display()))?;
    if metadata.is_dir() && !metadata.file_type().is_symlink() {
        migrate_directory(source, destination)?;
        return Ok(());
    }

    move_leaf_entry(source, destination)
}

fn migrate_directory(source: &Path, destination: &Path) -> Result<()> {
    if destination.symlink_metadata().is_err() {
        if let Some(parent) = destination.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("failed to create {}", parent.display()))?;
        }
        match fs::rename(source, destination) {
            Ok(()) => return Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::CrossesDevices => {}
            Err(error) => {
                return Err(error).with_context(|| {
                    format!(
                        "failed to move {} to {}",
                        source.display(),
                        destination.display()
                    )
                });
            }
        }
    }

    if !destination.exists() {
        fs::create_dir_all(destination)
            .with_context(|| format!("failed to create {}", destination.display()))?;
    }
    if !destination.is_dir() {
        let alternate = non_overwriting_destination(destination)?;
        fs::rename(source, &alternate).with_context(|| {
            format!(
                "failed to move {} to {}",
                source.display(),
                alternate.display()
            )
        })?;
        return Ok(());
    }

    for entry in
        fs::read_dir(source).with_context(|| format!("failed to read {}", source.display()))?
    {
        let entry = entry.with_context(|| format!("failed to enumerate {}", source.display()))?;
        let target = destination.join(entry.file_name());
        migrate_entry_without_following_symlink(&entry.path(), &target)?;
    }
    let _ = fs::remove_dir(source);
    Ok(())
}

fn move_leaf_entry(source: &Path, destination: &Path) -> Result<()> {
    let target = if destination.symlink_metadata().is_err() {
        destination.to_path_buf()
    } else if same_file_bytes(source, destination)? {
        fs::remove_file(source)
            .with_context(|| format!("failed to remove duplicate {}", source.display()))?;
        return Ok(());
    } else {
        non_overwriting_destination(destination)?
    };

    if let Some(parent) = target.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }
    fs::rename(source, &target).with_context(|| {
        format!(
            "failed to move {} to {}",
            source.display(),
            target.display()
        )
    })
}

fn same_file_bytes(left: &Path, right: &Path) -> Result<bool> {
    if !left.is_file() || !right.is_file() {
        return Ok(false);
    }
    let left_bytes =
        fs::read(left).with_context(|| format!("failed to read {}", left.display()))?;
    let right_bytes =
        fs::read(right).with_context(|| format!("failed to read {}", right.display()))?;
    Ok(left_bytes == right_bytes)
}

fn non_overwriting_destination(destination: &Path) -> Result<PathBuf> {
    let parent = destination.parent().unwrap_or_else(|| Path::new("."));
    let file_name = destination
        .file_name()
        .map(|value| value.to_string_lossy().to_string())
        .unwrap_or_else(|| "migrated-path".to_string());
    for index in 1..=1000 {
        let candidate = parent.join(format!("{file_name}.migrated-copy-{index:04}"));
        if candidate.symlink_metadata().is_err() {
            return Ok(candidate);
        }
    }
    Ok(parent.join(format!("{file_name}.migrated-copy")))
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
    use super::*;

    #[test]
    fn migrates_special_paths_under_stellaclaw_without_overwriting() {
        let root = test_root("conversation-special-paths");
        let conversation = root.join("conversations").join("web-main-000001");
        fs::create_dir_all(root.join("rundir/.skill/demo")).unwrap();
        fs::write(root.join("rundir/.skill/demo/SKILL.md"), "runtime skill").unwrap();
        fs::create_dir_all(root.join("rundir/skill_memory")).unwrap();
        fs::write(root.join("rundir/skill_memory/runtime.txt"), "runtime").unwrap();
        fs::create_dir_all(conversation.join(".skill/demo")).unwrap();
        fs::write(conversation.join(".skill/demo/SKILL.md"), "workspace skill").unwrap();
        fs::create_dir_all(conversation.join("attachments")).unwrap();
        fs::create_dir_all(conversation.join(".output")).unwrap();
        fs::create_dir_all(conversation.join(".skill_memory")).unwrap();
        fs::create_dir_all(conversation.join(".stellaclaw")).unwrap();
        fs::write(conversation.join("STELLACLAW.md"), "old memory").unwrap();
        fs::write(
            conversation.join(".stellaclaw").join("STELLACLAW.md"),
            "new memory",
        )
        .unwrap();
        fs::write(conversation.join("attachments").join("photo.png"), "photo").unwrap();
        fs::write(conversation.join(".output").join("out.txt"), "output").unwrap();
        fs::write(
            conversation.join(".skill_memory").join("skill.txt"),
            "skill",
        )
        .unwrap();

        StellaclawConversationSpecialPathUpgrade
            .upgrade(&root, &test_config())
            .unwrap();

        assert_eq!(
            fs::read_to_string(conversation.join(".stellaclaw/attachments/photo.png")).unwrap(),
            "photo"
        );
        assert_eq!(
            fs::read_to_string(conversation.join(".stellaclaw/output/out.txt")).unwrap(),
            "output"
        );
        assert_eq!(
            fs::read_to_string(conversation.join(".stellaclaw/skill_memory/skill.txt")).unwrap(),
            "skill"
        );
        assert_eq!(
            fs::read_to_string(root.join("rundir/.stellaclaw/skill_memory/runtime.txt")).unwrap(),
            "runtime"
        );
        assert_eq!(
            fs::read_to_string(root.join("rundir/.stellaclaw/skill/demo/SKILL.md")).unwrap(),
            "runtime skill"
        );
        assert_eq!(
            fs::read_to_string(conversation.join(".stellaclaw/skill/demo/SKILL.md")).unwrap(),
            "workspace skill"
        );
        assert_eq!(
            fs::read_to_string(conversation.join(".stellaclaw/STELLACLAW.md")).unwrap(),
            "new memory"
        );
        assert_eq!(
            fs::read_to_string(conversation.join(".stellaclaw/STELLACLAW.md.migrated-copy-0001"))
                .unwrap(),
            "old memory"
        );
        assert!(!conversation.join("attachments").exists());
        assert!(!conversation.join(".output").exists());
        assert!(!conversation.join(".skill").exists());
        assert!(!conversation.join(".skill_memory").exists());
        assert!(!root.join("rundir/.skill").exists());
        assert!(!root.join("rundir/skill_memory").exists());

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn skips_legacy_sshfs_workspace_roots() {
        let root = test_root("conversation-special-paths-skip-sshfs");
        let sshfs = root.join("conversations").join("sshfs-web-main-000001");
        fs::create_dir_all(&sshfs).unwrap();
        fs::write(sshfs.join("STELLACLAW.md"), "remote").unwrap();

        StellaclawConversationSpecialPathUpgrade
            .upgrade(&root, &test_config())
            .unwrap();

        assert!(sshfs.join("STELLACLAW.md").exists());
        assert!(!sshfs.join(".stellaclaw/STELLACLAW.md").exists());

        let _ = fs::remove_dir_all(root);
    }

    #[cfg(unix)]
    #[test]
    fn replaces_stale_conversation_skill_memory_symlink() {
        let root = test_root("conversation-special-paths-stale-symlink");
        let conversation = root.join("conversations").join("web-main-000001");
        fs::create_dir_all(root.join("rundir/skill_memory")).unwrap();
        fs::create_dir_all(conversation.join(".stellaclaw")).unwrap();
        std::os::unix::fs::symlink(
            root.join("rundir").join("skill_memory"),
            conversation.join(".stellaclaw/skill_memory"),
        )
        .unwrap();

        StellaclawConversationSpecialPathUpgrade
            .upgrade(&root, &test_config())
            .unwrap();

        assert_eq!(
            fs::read_link(conversation.join(".stellaclaw/skill_memory")).unwrap(),
            root.join("rundir/.stellaclaw/skill_memory")
        );
        assert!(!root.join("rundir/skill_memory").exists());

        let _ = fs::remove_dir_all(root);
    }

    fn test_root(name: &str) -> PathBuf {
        let root =
            std::env::temp_dir().join(format!("stellaclaw-v0_13-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        root
    }

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
            sandbox: SandboxConfig::default(),
            skill_sync: Vec::new(),
            available_agent_models: Vec::new(),
        }
    }
}
