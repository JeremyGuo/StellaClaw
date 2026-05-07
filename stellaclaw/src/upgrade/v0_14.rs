use std::{
    fs,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result};

use super::{WorkdirUpgrader, WORKDIR_VERSION_0_14, WORKDIR_VERSION_0_15};
use crate::{config::StellaclawConfig, workspace::is_sshfs_workspace_entry_name};

pub struct StaleSpecialLinkRepairUpgrade;

impl WorkdirUpgrader for StaleSpecialLinkRepairUpgrade {
    fn from_version(&self) -> &'static str {
        WORKDIR_VERSION_0_14
    }

    fn to_version(&self) -> &'static str {
        WORKDIR_VERSION_0_15
    }

    fn upgrade(&self, workdir: &Path, _config: &StellaclawConfig) -> Result<()> {
        migrate_runtime_skill_paths(workdir)?;
        repair_conversation_skill_memory_links(workdir)
    }
}

fn migrate_runtime_skill_paths(workdir: &Path) -> Result<()> {
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
        move_entry_without_following_symlink(&source, &runtime_root.join(new))?;
    }
    Ok(())
}

fn repair_conversation_skill_memory_links(workdir: &Path) -> Result<()> {
    let conversations_root = workdir.join("conversations");
    if !conversations_root.exists() {
        return Ok(());
    }
    let target = workdir
        .join("rundir")
        .join(".stellaclaw")
        .join("skill_memory");
    fs::create_dir_all(&target)
        .with_context(|| format!("failed to create {}", target.display()))?;

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
        if !root.is_dir() {
            continue;
        }
        let stellaclaw_root = root.join(".stellaclaw");
        fs::create_dir_all(&stellaclaw_root)
            .with_context(|| format!("failed to create {}", stellaclaw_root.display()))?;
        let link_path = stellaclaw_root.join("skill_memory");
        let Ok(metadata) = fs::symlink_metadata(&link_path) else {
            create_directory_link(&target, &link_path)?;
            continue;
        };
        if metadata.file_type().is_symlink() {
            fs::remove_file(&link_path)
                .with_context(|| format!("failed to remove {}", link_path.display()))?;
            create_directory_link(&target, &link_path)?;
        }
    }
    Ok(())
}

fn move_entry_without_following_symlink(source: &Path, destination: &Path) -> Result<()> {
    let metadata = fs::symlink_metadata(source)
        .with_context(|| format!("failed to inspect {}", source.display()))?;
    if metadata.is_dir() && !metadata.file_type().is_symlink() {
        move_directory(source, destination)?;
        return Ok(());
    }

    let target = if destination.symlink_metadata().is_err() {
        destination.to_path_buf()
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

fn move_directory(source: &Path, destination: &Path) -> Result<()> {
    if destination.symlink_metadata().is_err() {
        if let Some(parent) = destination.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("failed to create {}", parent.display()))?;
        }
        fs::rename(source, destination).with_context(|| {
            format!(
                "failed to move {} to {}",
                source.display(),
                destination.display()
            )
        })?;
        return Ok(());
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
        move_entry_without_following_symlink(&entry.path(), &destination.join(entry.file_name()))?;
    }
    let _ = fs::remove_dir(source);
    Ok(())
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

    #[cfg(unix)]
    #[test]
    fn repairs_stale_skill_memory_links_for_already_upgraded_workdir() {
        let root = test_root("stale-special-link");
        let conversation = root.join("conversations").join("web-main-000001");
        fs::create_dir_all(root.join("rundir/.skill/demo")).unwrap();
        fs::write(root.join("rundir/.skill/demo/SKILL.md"), "skill").unwrap();
        fs::create_dir_all(root.join("rundir/skill_memory")).unwrap();
        fs::write(root.join("rundir/skill_memory/runtime.txt"), "runtime").unwrap();
        fs::create_dir_all(conversation.join(".stellaclaw")).unwrap();
        std::os::unix::fs::symlink(
            root.join("rundir").join("skill_memory"),
            conversation.join(".stellaclaw/skill_memory"),
        )
        .unwrap();

        StaleSpecialLinkRepairUpgrade
            .upgrade(&root, &test_config())
            .unwrap();

        assert_eq!(
            fs::read_to_string(root.join("rundir/.stellaclaw/skill/demo/SKILL.md")).unwrap(),
            "skill"
        );
        assert_eq!(
            fs::read_to_string(root.join("rundir/.stellaclaw/skill_memory/runtime.txt")).unwrap(),
            "runtime"
        );
        assert_eq!(
            fs::read_link(conversation.join(".stellaclaw/skill_memory")).unwrap(),
            root.join("rundir/.stellaclaw/skill_memory")
        );
        assert!(!root.join("rundir/.skill").exists());
        assert!(!root.join("rundir/skill_memory").exists());

        let _ = fs::remove_dir_all(root);
    }

    fn test_root(name: &str) -> PathBuf {
        let root =
            std::env::temp_dir().join(format!("stellaclaw-v0_14-{name}-{}", std::process::id()));
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
            memory: crate::config::MemoryConfig::default(),
            sandbox: SandboxConfig::default(),
            skill_sync: Vec::new(),
            available_agent_models: Vec::new(),
        }
    }
}
