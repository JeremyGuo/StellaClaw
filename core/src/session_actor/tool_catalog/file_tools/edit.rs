use std::fs;

use serde_json::{json, Map, Value};

use crate::session_actor::tool_runtime::{
    bool_arg_with_default, resolve_local_path, string_arg, string_arg_with_default,
    ExecutionTarget, LocalToolError, ToolExecutionContext,
};

use super::common::remote_file_tool;

pub(super) fn execute_edit_tool(
    tool_name: &str,
    arguments: &Map<String, Value>,
    context: &ToolExecutionContext<'_>,
) -> Result<Option<Value>, LocalToolError> {
    if tool_name != "edit" {
        return Ok(None);
    }

    let result = match context.execution_target(arguments)? {
        ExecutionTarget::Local => edit_local(arguments, context.workspace_root)?,
        ExecutionTarget::RemoteSsh { host, cwd } => {
            remote_file_tool("edit", arguments, &host, cwd.as_deref())?
        }
    };
    Ok(Some(result))
}

fn edit_local(
    arguments: &Map<String, Value>,
    workspace_root: &std::path::Path,
) -> Result<Value, LocalToolError> {
    let path = resolve_local_path(workspace_root, &string_arg(arguments, "path")?);
    let old_text = string_arg(arguments, "old_text")?;
    let new_text = string_arg(arguments, "new_text")?;
    let replace_all = bool_arg_with_default(arguments, "replace_all", false)?;
    let create_if_missing = bool_arg_with_default(arguments, "create_if_missing", false)?;
    let encoding = string_arg_with_default(arguments, "encoding", "utf-8")?;

    if encoding.to_lowercase() != "utf-8" {
        return Err(LocalToolError::InvalidArguments(
            "only utf-8 encoding is supported".to_string(),
        ));
    }

    if !path.exists() && create_if_missing {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).map_err(|error| {
                LocalToolError::Io(format!("failed to create {}: {error}", parent.display()))
            })?;
        }
        fs::write(&path, new_text.as_bytes()).map_err(|error| {
            LocalToolError::Io(format!("failed to write {}: {error}", path.display()))
        })?;
        return Ok(json!({
            "path": path.display().to_string(),
            "created": true,
            "replacements": 1,
            "bytes_written": new_text.len()
        }));
    }

    let content = fs::read_to_string(&path).map_err(|error| {
        LocalToolError::Io(format!("failed to read {}: {error}", path.display()))
    })?;
    let replacements = content.matches(&old_text).count();
    if replacements == 0 {
        return Err(LocalToolError::InvalidArguments(format!(
            "old_text was not found in {}",
            path.display()
        )));
    }
    if replacements > 1 && !replace_all {
        return Err(LocalToolError::InvalidArguments(format!(
            "old_text matched {} locations in {}; include more surrounding context or set replace_all=true",
            replacements,
            path.display()
        )));
    }

    let updated = if replace_all {
        content.replace(&old_text, &new_text)
    } else {
        content.replacen(&old_text, &new_text, 1)
    };
    fs::write(&path, updated.as_bytes()).map_err(|error| {
        LocalToolError::Io(format!("failed to write {}: {error}", path.display()))
    })?;
    Ok(json!({
        "path": path.display().to_string(),
        "created": false,
        "replacements": if replace_all { replacements } else { 1 },
        "bytes_written": updated.len()
    }))
}
