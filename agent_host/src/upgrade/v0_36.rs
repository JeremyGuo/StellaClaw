use super::WorkdirUpgrader;
use anyhow::{Context, Result};
use std::fs;
use std::path::{Path, PathBuf};

pub(super) struct Upgrade;

impl WorkdirUpgrader for Upgrade {
    fn from_version(&self) -> &'static str {
        "0.35"
    }

    fn to_version(&self) -> &'static str {
        "0.36"
    }

    fn upgrade(&self, workdir: &Path) -> Result<()> {
        let source = workdir.join("rundir").join("shared");
        fs::create_dir_all(&source)
            .with_context(|| format!("failed to create {}", source.display()))?;

        let workspaces_root = workdir.join("workspaces");
        if !workspaces_root.is_dir() {
            return Ok(());
        }

        for entry in fs::read_dir(&workspaces_root)
            .with_context(|| format!("failed to read {}", workspaces_root.display()))?
        {
            let workspace_root = entry?.path();
            let files_dir = workspace_root.join("files");
            if !files_dir.is_dir() {
                continue;
            }
            normalize_workspace_shared_dir(&source, &files_dir.join("shared"))?;
        }

        Ok(())
    }
}

fn normalize_workspace_shared_dir(source: &Path, target: &Path) -> Result<()> {
    if let Ok(metadata) = fs::symlink_metadata(target) {
        if metadata.file_type().is_symlink() {
            return Ok(());
        }
        if metadata.is_dir() {
            merge_directory_contents_if_missing(target, source)?;
            fs::remove_dir_all(target)
                .with_context(|| format!("failed to remove legacy {}", target.display()))?;
        } else {
            return Ok(());
        }
    }
    create_dir_symlink(source, target)
        .with_context(|| format!("failed to create shared dir link {}", target.display()))?;
    Ok(())
}

fn merge_directory_contents_if_missing(source: &Path, target: &Path) -> Result<()> {
    let metadata = fs::symlink_metadata(source)
        .with_context(|| format!("failed to stat {}", source.display()))?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Ok(());
    }
    fs::create_dir_all(target).with_context(|| format!("failed to create {}", target.display()))?;
    for entry in
        fs::read_dir(source).with_context(|| format!("failed to read {}", source.display()))?
    {
        let entry = entry?;
        let source_path = entry.path();
        let target_path = target.join(entry.file_name());
        if target_path.exists() {
            if source_path.is_dir() && target_path.is_dir() {
                merge_directory_contents_if_missing(&source_path, &target_path)?;
            }
            continue;
        }
        copy_path_recursive(&source_path, &target_path)?;
    }
    Ok(())
}

fn copy_path_recursive(source: &Path, target: &Path) -> Result<()> {
    let metadata =
        fs::metadata(source).with_context(|| format!("failed to stat {}", source.display()))?;
    if metadata.is_dir() {
        copy_dir_recursive(source, target)
    } else {
        if let Some(parent) = target.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("failed to create {}", parent.display()))?;
        }
        fs::copy(source, target).with_context(|| {
            format!(
                "failed to copy {} to {}",
                source.display(),
                target.display()
            )
        })?;
        Ok(())
    }
}

fn copy_dir_recursive(source: &Path, target: &Path) -> Result<()> {
    fs::create_dir_all(target).with_context(|| format!("failed to create {}", target.display()))?;
    for entry in
        fs::read_dir(source).with_context(|| format!("failed to read {}", source.display()))?
    {
        let entry = entry?;
        let source_path = entry.path();
        let target_path = target.join(entry.file_name());
        copy_path_recursive(&source_path, &target_path)?;
    }
    Ok(())
}

fn create_dir_symlink(source: &Path, target: &Path) -> std::io::Result<()> {
    let link_source = canonical_or_absolute(source)?;
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(&link_source, target)
    }
    #[cfg(windows)]
    {
        std::os::windows::fs::symlink_dir(&link_source, target)
    }
}

fn canonical_or_absolute(source: &Path) -> std::io::Result<PathBuf> {
    source.canonicalize().or_else(|_| {
        if source.is_absolute() {
            Ok(source.to_path_buf())
        } else {
            std::env::current_dir().map(|cwd| cwd.join(source))
        }
    })
}
