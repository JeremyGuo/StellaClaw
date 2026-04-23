use std::fs;

use regex::Regex;
use serde_json::{Map, Value};

use crate::session_actor::tool_runtime::{
    resolve_local_path, string_arg, string_arg_with_default, ExecutionTarget, LocalToolError,
    ToolExecutionContext,
};

use super::common::{
    build_glob_matcher, collect_walk_paths, file_mtime_ms, relative_display_path, remote_file_tool,
    search_result, sort_search_matches, SearchMatch,
};

pub(super) fn execute_search_tool(
    tool_name: &str,
    arguments: &Map<String, Value>,
    context: &ToolExecutionContext<'_>,
) -> Result<Option<Value>, LocalToolError> {
    let result = match tool_name {
        "glob" => glob(arguments, context)?,
        "grep" => grep(arguments, context)?,
        _ => return Ok(None),
    };
    Ok(Some(result))
}

fn glob(
    arguments: &Map<String, Value>,
    context: &ToolExecutionContext<'_>,
) -> Result<Value, LocalToolError> {
    match context.execution_target(arguments)? {
        ExecutionTarget::Local => glob_local(arguments, context.workspace_root),
        ExecutionTarget::RemoteSsh { host, cwd } => {
            remote_file_tool("glob", arguments, &host, cwd.as_deref())
        }
    }
}

fn grep(
    arguments: &Map<String, Value>,
    context: &ToolExecutionContext<'_>,
) -> Result<Value, LocalToolError> {
    match context.execution_target(arguments)? {
        ExecutionTarget::Local => grep_local(arguments, context.workspace_root),
        ExecutionTarget::RemoteSsh { host, cwd } => {
            remote_file_tool("grep", arguments, &host, cwd.as_deref())
        }
    }
}

fn glob_local(
    arguments: &Map<String, Value>,
    workspace_root: &std::path::Path,
) -> Result<Value, LocalToolError> {
    let pattern = string_arg(arguments, "pattern")?;
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

    let matcher = build_glob_matcher(&pattern)?;
    let mut matches = collect_walk_paths(&base_path, true)?
        .into_iter()
        .filter_map(|path| {
            let relative = relative_display_path(&path, &base_path);
            matcher.is_match(&relative).then(|| SearchMatch {
                path: path.display().to_string(),
                mtime_ms: file_mtime_ms(&path),
            })
        })
        .collect::<Vec<_>>();
    sort_search_matches(&mut matches);
    search_result("pattern", &pattern, &base_path, None, matches)
}

fn grep_local(
    arguments: &Map<String, Value>,
    workspace_root: &std::path::Path,
) -> Result<Value, LocalToolError> {
    let pattern = string_arg(arguments, "pattern")?;
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

    let regex = Regex::new(&pattern).map_err(|error| {
        LocalToolError::InvalidArguments(format!("invalid regex pattern: {error}"))
    })?;
    let include = match arguments.get("include").and_then(Value::as_str) {
        Some(include) => Some(build_glob_matcher(include)?),
        None => None,
    };

    let mut matches = Vec::new();
    for path in collect_walk_paths(&base_path, true)? {
        let relative = relative_display_path(&path, &base_path);
        if let Some(include) = &include {
            if !include.is_match(&relative) {
                continue;
            }
        }
        let Ok(text) = fs::read_to_string(&path) else {
            continue;
        };
        if regex.is_match(&text) {
            matches.push(SearchMatch {
                path: path.display().to_string(),
                mtime_ms: file_mtime_ms(&path),
            });
        }
    }
    sort_search_matches(&mut matches);
    search_result(
        "pattern",
        &pattern,
        &base_path,
        arguments.get("include").and_then(Value::as_str),
        matches,
    )
}
