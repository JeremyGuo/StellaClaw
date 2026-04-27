use std::fs;

use serde_json::{Map, Value};

use crate::session_actor::tool_runtime::{
    resolve_local_path, string_arg_with_default, ExecutionTarget, LocalToolError,
    ToolExecutionContext,
};

use super::common::{
    relative_display_path, remote_file_tool, LsEntry, SlowMountTable, COMMON_LS_SKIP_DIRS,
    LS_MAX_ENTRIES,
};

pub(super) fn execute_list_tool(
    tool_name: &str,
    arguments: &Map<String, Value>,
    context: &ToolExecutionContext<'_>,
) -> Result<Option<Value>, LocalToolError> {
    if tool_name != "ls" {
        return Ok(None);
    }

    let result = match context.execution_target(arguments)? {
        ExecutionTarget::Local => ls_local(arguments, context.workspace_root)?,
        ExecutionTarget::RemoteSsh { host, cwd } => {
            remote_file_tool("ls", arguments, &host, cwd.as_deref())?
        }
    };
    Ok(Some(result))
}

fn ls_local(
    arguments: &Map<String, Value>,
    workspace_root: &std::path::Path,
) -> Result<Value, LocalToolError> {
    let base_path = resolve_local_path(
        workspace_root,
        &string_arg_with_default(arguments, "path", ".")?,
    );
    if !base_path.exists() {
        return Err(LocalToolError::Io(format!(
            "{} does not exist",
            base_path.display()
        )));
    }
    if !base_path.is_dir() {
        return Err(LocalToolError::InvalidArguments(format!(
            "{} is not a directory",
            base_path.display()
        )));
    }

    let mut entries = Vec::new();
    let mut truncated = false;
    let slow_mounts = SlowMountTable::load();
    collect_ls_entries(
        &base_path,
        &base_path,
        &slow_mounts,
        &mut entries,
        &mut truncated,
    )?;
    entries.sort_by(|left, right| left.path.cmp(&right.path));
    Ok(Value::String(render_ls_tree(
        &base_path, &entries, truncated,
    )))
}

fn collect_ls_entries(
    base: &std::path::Path,
    path: &std::path::Path,
    slow_mounts: &SlowMountTable,
    entries: &mut Vec<LsEntry>,
    truncated: &mut bool,
) -> Result<(), LocalToolError> {
    if *truncated {
        return Ok(());
    }
    if slow_mounts.contains(path) {
        return Ok(());
    }
    let mut children = fs::read_dir(path)
        .map_err(|error| LocalToolError::Io(format!("failed to read {}: {error}", path.display())))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| LocalToolError::Io(format!("failed to read dir entry: {error}")))?;
    children.sort_by_key(|entry| entry.path());

    for entry in children {
        if entries.len() >= LS_MAX_ENTRIES {
            *truncated = true;
            return Ok(());
        }
        let file_name = entry.file_name().to_string_lossy().to_string();
        let file_type = entry
            .file_type()
            .map_err(|error| LocalToolError::Io(format!("failed to inspect file type: {error}")))?;
        let path = entry.path();
        let is_shared_symlink_dir =
            is_top_level_shared_symlink_dir(base, &path, &file_name, file_type.is_symlink());
        let is_dir = file_type.is_dir() || is_shared_symlink_dir;
        if (file_type.is_symlink() && !is_shared_symlink_dir)
            || should_skip_ls_name(&file_name, is_dir)
        {
            continue;
        }
        if slow_mounts.contains(&path) {
            continue;
        }
        entries.push(LsEntry {
            path: relative_display_path(&path, base),
            is_dir,
        });
        if is_dir {
            collect_ls_entries(base, &path, slow_mounts, entries, truncated)?;
        }
    }
    Ok(())
}

fn is_top_level_shared_symlink_dir(
    base: &std::path::Path,
    path: &std::path::Path,
    file_name: &str,
    is_symlink: bool,
) -> bool {
    is_symlink
        && file_name == "shared"
        && path.parent().is_some_and(|parent| parent == base)
        && fs::metadata(path)
            .map(|metadata| metadata.is_dir())
            .unwrap_or(false)
}

fn should_skip_ls_name(name: &str, is_dir: bool) -> bool {
    if name.starts_with('.') {
        return true;
    }
    is_dir && COMMON_LS_SKIP_DIRS.contains(&name)
}

fn render_ls_tree(base_path: &std::path::Path, entries: &[LsEntry], truncated: bool) -> String {
    let mut lines = Vec::new();
    if truncated {
        lines.push(format!("num_entries: >{LS_MAX_ENTRIES}"));
        lines.push("truncated: true".to_string());
        lines.push(format!(
            "There are more than {LS_MAX_ENTRIES} files and directories under {}. Use ls with a more specific path, or use glob/grep to narrow the search. The first {LS_MAX_ENTRIES} files and directories are included below:",
            base_path.display()
        ));
        lines.push(String::new());
    } else {
        lines.push(format!("num_entries: {}", entries.len()));
        lines.push(String::new());
    }

    let mut display_base = base_path.display().to_string();
    if !display_base.ends_with(std::path::MAIN_SEPARATOR) {
        display_base.push(std::path::MAIN_SEPARATOR);
    }
    lines.push(format!("- {display_base}"));
    for entry in entries {
        let components = entry
            .path
            .split('/')
            .filter(|part| !part.is_empty())
            .collect::<Vec<_>>();
        let Some(name) = components.last() else {
            continue;
        };
        let indent = "  ".repeat(components.len());
        let suffix = if entry.is_dir { "/" } else { "" };
        lines.push(format!("{indent}- {name}{suffix}"));
    }
    lines.join("\n")
}

#[cfg(test)]
mod tests {
    use super::ls_local;
    use serde_json::Map;

    #[test]
    fn ls_includes_top_level_shared_symlink() {
        let root =
            std::env::temp_dir().join(format!("stellaclaw-ls-shared-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let workspace = root.join("workspace");
        let shared_target = root.join("runtime").join("shared");
        std::fs::create_dir_all(&workspace).expect("workspace should be created");
        std::fs::create_dir_all(&shared_target).expect("shared target should be created");
        std::fs::write(shared_target.join("marker.txt"), "shared")
            .expect("shared file should be written");

        #[cfg(unix)]
        std::os::unix::fs::symlink(&shared_target, workspace.join("shared"))
            .expect("shared symlink should be created");
        #[cfg(windows)]
        std::os::windows::fs::symlink_dir(&shared_target, workspace.join("shared"))
            .expect("shared symlink should be created");

        let result = ls_local(&Map::new(), &workspace).expect("ls should succeed");
        let text = result.as_str().expect("ls result should be text");
        assert!(text.contains("- shared/"), "{text}");
        assert!(text.contains("- marker.txt"), "{text}");

        std::fs::remove_dir_all(&root).expect("temp dir should be cleaned");
    }
}
