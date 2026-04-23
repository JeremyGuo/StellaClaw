mod common;
mod edit;
mod list;
mod patch;
mod read_write;
mod search;

use serde_json::{json, Map, Value};

use super::{
    schema::{file_tool_schema, properties},
    ToolBackend, ToolDefinition, ToolExecutionMode, ToolRemoteMode,
};
use crate::session_actor::tool_runtime::{LocalToolError, ToolExecutionContext};

pub fn file_tool_definitions(remote_mode: &ToolRemoteMode) -> Vec<ToolDefinition> {
    vec![
        ToolDefinition::new(
            "file_read",
            "Read a UTF-8 text file. Supports file_path plus optional offset and limit for large files.",
            file_tool_schema(
                properties([
                    ("file_path", json!({"type": "string"})),
                    ("offset", json!({"type": "integer"})),
                    ("limit", json!({"type": "integer"})),
                ]),
                &["file_path"],
                remote_mode,
            ),
            ToolExecutionMode::Immediate,
            ToolBackend::Local,
        ),
        ToolDefinition::new(
            "file_write",
            "Write a UTF-8 text file.",
            file_tool_schema(
                properties([
                    ("file_path", json!({"type": "string"})),
                    ("content", json!({"type": "string"})),
                    (
                        "mode",
                        json!({"type": "string", "enum": ["overwrite", "append"]}),
                    ),
                    ("encoding", json!({"type": "string"})),
                ]),
                &["file_path", "content"],
                remote_mode,
            ),
            ToolExecutionMode::Immediate,
            ToolBackend::Local,
        ),
        ToolDefinition::new(
            "glob",
            "Fast file pattern matching tool. Supports glob patterns like **/*.rs and src/**/*.ts. Omit path or pass an empty string to search from the current workspace directory.",
            file_tool_schema(
                properties([
                    ("pattern", json!({"type": "string"})),
                    ("path", json!({"type": "string"})),
                ]),
                &["pattern"],
                remote_mode,
            ),
            ToolExecutionMode::Immediate,
            ToolBackend::Local,
        ),
        ToolDefinition::new(
            "grep",
            "Fast content search tool. Searches file contents with a regex pattern and returns matching file paths. Omit path or pass an empty string to search from the current workspace directory.",
            file_tool_schema(
                properties([
                    ("pattern", json!({"type": "string"})),
                    ("path", json!({"type": "string"})),
                    ("include", json!({"type": "string"})),
                ]),
                &["pattern"],
                remote_mode,
            ),
            ToolExecutionMode::Immediate,
            ToolBackend::Local,
        ),
        ToolDefinition::new(
            "ls",
            "List a recursive directory tree for non-hidden files and directories under a path. Omit path or pass an empty string to list the current workspace directory. Skips common cache/build directories by default. Large trees are truncated to the first 1000 files and directories; pass a more specific path or use glob/grep when you know what to search for.",
            file_tool_schema(
                properties([("path", json!({"type": "string"}))]),
                &[],
                remote_mode,
            ),
            ToolExecutionMode::Immediate,
            ToolBackend::Local,
        ),
        ToolDefinition::new(
            "edit",
            "Edit a UTF-8 text file by replacing old_text with new_text. When replace_all=false, old_text must match exactly one location; if it matches multiple locations, include more surrounding context.",
            file_tool_schema(
                properties([
                    ("path", json!({"type": "string"})),
                    ("old_text", json!({"type": "string"})),
                    ("new_text", json!({"type": "string"})),
                    ("replace_all", json!({"type": "boolean"})),
                    ("create_if_missing", json!({"type": "boolean"})),
                    ("encoding", json!({"type": "string"})),
                ]),
                &["path", "old_text", "new_text"],
                remote_mode,
            ),
            ToolExecutionMode::Immediate,
            ToolBackend::Local,
        ),
        ToolDefinition::new(
            "apply_patch",
            "Apply a unified diff patch inside the workspace using git apply. The patch must be a valid unified diff. Returned stdout/stderr are capped by max_output_chars; full stdout/stderr are saved under out_path.",
            file_tool_schema(
                properties([
                    ("patch", json!({"type": "string"})),
                    ("strip", json!({"type": "integer"})),
                    ("reverse", json!({"type": "boolean"})),
                    ("check", json!({"type": "boolean"})),
                    (
                        "max_output_chars",
                        json!({"type": "integer", "minimum": 0, "maximum": 1000}),
                    ),
                ]),
                &["patch"],
                remote_mode,
            ),
            ToolExecutionMode::Immediate,
            ToolBackend::Local,
        ),
    ]
}

pub(crate) fn execute_file_tool(
    tool_name: &str,
    arguments: &Map<String, Value>,
    context: &ToolExecutionContext<'_>,
) -> Result<Option<Value>, LocalToolError> {
    if context.cancel_token.is_cancelled() {
        return Err(LocalToolError::Io("tool operation cancelled".to_string()));
    }

    if let Some(result) = read_write::execute_read_write_tool(tool_name, arguments, context)? {
        return Ok(Some(result));
    }
    if let Some(result) = search::execute_search_tool(tool_name, arguments, context)? {
        return Ok(Some(result));
    }
    if let Some(result) = list::execute_list_tool(tool_name, arguments, context)? {
        return Ok(Some(result));
    }
    if let Some(result) = edit::execute_edit_tool(tool_name, arguments, context)? {
        return Ok(Some(result));
    }
    patch::execute_patch_tool(tool_name, arguments, context)
}
