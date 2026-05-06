use std::fs;

use serde_json::{json, Map, Value};

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

    if matches!(
        context.remote_mode,
        super::super::ToolRemoteMode::FixedSsh { .. }
    ) && is_root_ls(arguments)
    {
        if let ExecutionTarget::RemoteSsh { host, cwd } =
            context.execution_target_for_path(arguments, &["path"])?
        {
            let mut remote_arguments = arguments.clone();
            remote_arguments.insert("shadow_roots".to_string(), json!(LOCAL_SPECIAL_ROOT_NAMES));
            let remote_result = remote_file_tool("ls", &remote_arguments, &host, cwd.as_deref())?;
            return Ok(Some(append_local_special_root_entries(
                remote_result,
                context.workspace_root,
            )?));
        }
    }

    let result = match context.execution_target_for_path(arguments, &["path"])? {
        ExecutionTarget::Local => ls_local(arguments, context.workspace_root)?,
        ExecutionTarget::RemoteSsh { host, cwd } => {
            remote_file_tool("ls", arguments, &host, cwd.as_deref())?
        }
    };
    Ok(Some(result))
}

const LOCAL_SPECIAL_ROOT_NAMES: &[&str] = &[".stellaclaw"];
const LOCAL_SPECIAL_CHILD_NAMES: &[&str] = &[
    "IDENTITY.md",
    "STELLACLAW.md",
    "USER.md",
    "attachments",
    "output",
    "shared",
    "skill",
    "skill_memory",
];

fn is_root_ls(arguments: &Map<String, Value>) -> bool {
    match arguments.get("path").and_then(Value::as_str) {
        None => true,
        Some(path) => path.trim().is_empty() || path.trim() == ".",
    }
}

fn append_local_special_root_entries(
    remote_result: Value,
    workspace_root: &std::path::Path,
) -> Result<Value, LocalToolError> {
    let Some(remote_text) = remote_result.as_str() else {
        return Ok(remote_result);
    };
    let entries = local_special_root_entries(workspace_root);
    if entries.is_empty() {
        return Ok(remote_result);
    }
    let mut text = remote_text.to_string();
    text.push_str("\n\nlocal_special_paths:");
    text.push_str(
        "\nThese workspace-relative paths are stored locally in fixed Remote Mode and shadow remote paths under .stellaclaw:",
    );
    for entry in entries {
        let suffix = if entry.is_dir { "/" } else { "" };
        text.push_str(&format!("\n- {}{}", entry.path, suffix));
    }
    Ok(Value::String(text))
}

fn local_special_root_entries(workspace_root: &std::path::Path) -> Vec<LsEntry> {
    let mut entries = Vec::new();
    let stellaclaw_root = workspace_root.join(".stellaclaw");
    if let Some(entry) = local_special_entry(&stellaclaw_root, ".stellaclaw") {
        entries.push(entry);
    }
    for name in LOCAL_SPECIAL_CHILD_NAMES {
        let path = stellaclaw_root.join(name);
        let display_path = format!(".stellaclaw/{name}");
        if let Some(entry) = local_special_entry(&path, &display_path) {
            entries.push(entry);
        }
    }
    entries.sort_by(|left, right| left.path.cmp(&right.path));
    entries
}

fn local_special_entry(path: &std::path::Path, display_path: &str) -> Option<LsEntry> {
    let Ok(metadata) = fs::symlink_metadata(&path) else {
        return None;
    };
    let is_dir = metadata.is_dir()
        || (metadata.file_type().is_symlink()
            && fs::metadata(&path)
                .map(|target| target.is_dir())
                .unwrap_or(false));
    Some(LsEntry {
        path: display_path.to_string(),
        is_dir,
    })
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
    use super::{append_local_special_root_entries, ls_local};
    use serde_json::Map;

    #[test]
    fn ls_hides_stellaclaw_directory_from_listing() {
        let root =
            std::env::temp_dir().join(format!("stellaclaw-ls-shared-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let workspace = root.join("workspace");
        let shared_target = root.join("runtime").join("shared");
        std::fs::create_dir_all(&workspace).expect("workspace should be created");
        std::fs::create_dir_all(&shared_target).expect("shared target should be created");
        std::fs::write(shared_target.join("marker.txt"), "shared")
            .expect("shared file should be written");

        let stellaclaw_dir = workspace.join(".stellaclaw");
        std::fs::create_dir_all(&stellaclaw_dir).expect("stellaclaw dir should be created");

        #[cfg(unix)]
        std::os::unix::fs::symlink(&shared_target, stellaclaw_dir.join("shared"))
            .expect("shared symlink should be created");

        #[cfg(windows)]
        std::os::windows::fs::symlink_dir(&shared_target, stellaclaw_dir.join("shared"))
            .expect("shared symlink should be created");

        let result = ls_local(&Map::new(), &workspace).expect("ls should succeed");
        let text = result.as_str().expect("ls result should be text");
        assert!(
            !text.contains(".stellaclaw"),
            ".stellaclaw should be hidden: {text}"
        );
        assert!(
            !text.contains("marker.txt"),
            "marker.txt inside .stellaclaw should be hidden: {text}"
        );

        std::fs::remove_dir_all(&root).expect("temp dir should be cleaned");
    }

    #[test]
    fn root_remote_ls_can_surface_local_special_paths() {
        let root =
            std::env::temp_dir().join(format!("stellaclaw-ls-special-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join(".stellaclaw/attachments"))
            .expect("attachments should exist");
        std::fs::write(root.join(".stellaclaw/STELLACLAW.md"), "memory")
            .expect("memory should exist");

        let result = append_local_special_root_entries(
            serde_json::Value::String("num_entries: 0\n\n- /remote/".to_string()),
            &root,
        )
        .expect("append should succeed");
        let text = result.as_str().expect("result should be text");

        assert!(text.contains("local_special_paths:"));
        assert!(text.contains("- .stellaclaw/"));
        assert!(text.contains("- .stellaclaw/STELLACLAW.md"));
        assert!(text.contains("- .stellaclaw/attachments/"));
        std::fs::remove_dir_all(&root).expect("temp dir should be cleaned");
    }
}
