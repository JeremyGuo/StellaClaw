mod patch;
mod visibility;

use serde_json::{json, Map, Value};

use super::{
    schema::{file_tool_schema, object_schema, properties},
    ToolBackend, ToolConcurrency, ToolDefinition, ToolExecutionMode, ToolRemoteMode,
};
use crate::session_actor::tool_runtime::{LocalToolError, ToolExecutionContext};

pub fn file_tool_definitions(remote_mode: &ToolRemoteMode) -> Vec<ToolDefinition> {
    let mut tools = vec![
        ToolDefinition::new(
            "apply_patch",
            "Apply a patch inside the workspace. Patch file paths must be workspace-relative paths, or remote-cwd-relative paths when remote execution is selected. Absolute paths under the active workspace/cwd are normalized to relative paths before applying; absolute paths outside it are rejected. Supports format=auto, format=freeform, format=codex, or format=unified. Freeform/codex format uses *** Begin Patch / *** End Patch sections. Unified format is passed to git apply; non-empty stdout/stderr are returned and capped by max_output_chars.",
            file_tool_schema(
                properties([
                    ("patch", json!({"type": "string"})),
                    (
                        "format",
                        json!({"type": "string", "enum": ["auto", "freeform", "codex", "unified"]}),
                    ),
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
        )
        .with_concurrency(ToolConcurrency::Serial),
    ];
    if matches!(remote_mode, ToolRemoteMode::FixedSsh { .. }) {
        tools.extend([
            ToolDefinition::new(
                "shell_make_visible",
                "Copy a local workspace-relative file or directory to the fixed remote workspace at the same relative path so remote shell and remote file tools can see it. Use this in Fixed SSH mode before shell_exec reads .stellaclaw/... paths; treat copied .stellaclaw files as read-only from shell and do not mutate .stellaclaw/ from shell. Requires path and optional timeout_seconds.",
                object_schema(
                    properties([
                        ("path", json!({"type": "string"})),
                        ("timeout_seconds", json!({"type": "number"})),
                    ]),
                    &["path"],
                ),
                ToolExecutionMode::Interruptible,
                ToolBackend::Local,
            )
            .with_concurrency(ToolConcurrency::Serial),
            ToolDefinition::new(
                "attachment_make_visible",
                "Make a workspace-relative file or directory visible for attachment sending. Before referencing a file with <attachment>, call this when that file is not yet visible to the conversation workspace; reference it only after this tool succeeds. Requires path and optional timeout_seconds.",
                object_schema(
                    properties([
                        ("path", json!({"type": "string"})),
                        ("timeout_seconds", json!({"type": "number"})),
                    ]),
                    &["path"],
                ),
                ToolExecutionMode::Interruptible,
                ToolBackend::Local,
            )
            .with_concurrency(ToolConcurrency::Serial),
        ]);
    }
    tools
}

pub(crate) fn execute_file_tool(
    tool_name: &str,
    arguments: &Map<String, Value>,
    context: &ToolExecutionContext<'_>,
) -> Result<Option<Value>, LocalToolError> {
    if context.cancel_token.is_cancelled() {
        return Err(LocalToolError::Io("tool operation cancelled".to_string()));
    }

    if let Some(result) = patch::execute_patch_tool(tool_name, arguments, context)? {
        return Ok(Some(result));
    }
    visibility::execute_visibility_tool(tool_name, arguments, context)
}
