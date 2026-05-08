mod common;
mod patch;
mod read_write;
mod search;
mod visibility;

use serde_json::{json, Map, Value};

use super::{
    schema::{file_tool_schema, object_schema, properties},
    PromptProtocol, ToolBackend, ToolConcurrency, ToolDefinition, ToolExecutionMode,
    ToolRemoteMode,
};
use crate::session_actor::tool_runtime::{LocalToolError, ToolExecutionContext};

pub fn file_tool_definitions(remote_mode: &ToolRemoteMode) -> Vec<ToolDefinition> {
    let mut tools = vec![
        ToolDefinition::new(
            "file_read",
            "Read a UTF-8 text file. Supports file_path plus optional start_line and end_line for large files. Lines are 1-based and end_line is inclusive. When end_line is omitted, reads up to 200 lines from start_line. All tool results are capped by the runtime; use smaller ranges for huge files.",
            file_tool_schema(
                properties([
                    ("file_path", json!({"type": "string"})),
                    ("start_line", json!({"type": "integer", "minimum": 1})),
                    ("end_line", json!({"type": "integer", "minimum": 1})),
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
        )
        .with_concurrency(ToolConcurrency::Serial),
        ToolDefinition::new(
            "grep",
            "Fast content search tool. Searches file contents with a regex pattern and returns matching files plus line-level matches. Omit path or pass an empty string to search from the current workspace directory. Set context_lines from 0 to 10 to include surrounding lines. Use names_only=true when only matching paths are needed. Skips slow remote mounts such as sshfs/NFS by default.",
            file_tool_schema(
                properties([
                    ("pattern", json!({"type": "string"})),
                    ("path", json!({"type": "string"})),
                    ("include", json!({"type": "string"})),
                    ("exclude", json!({"type": "string"})),
                    ("context_lines", json!({"type": "integer", "minimum": 0, "maximum": 10})),
                    ("max_matches_per_file", json!({"type": "integer", "minimum": 1})),
                    ("total_max_matches", json!({"type": "integer", "minimum": 1})),
                    ("names_only", json!({"type": "boolean"})),
                ]),
                &["pattern"],
                remote_mode,
            ),
            ToolExecutionMode::Immediate,
            ToolBackend::Local,
        ),
        ToolDefinition::new(
            "apply_patch",
            "Apply a patch inside the workspace. Supports format=auto, format=codex, or format=unified. Codex format uses an envelope with *** Begin Patch / *** End Patch and file sections such as *** Add File, *** Delete File, and *** Update File, returning files_changed. Unified format is passed to git apply; non-empty stdout/stderr are returned and capped by max_output_chars.",
            file_tool_schema(
                properties([
                    ("patch", json!({"type": "string"})),
                    (
                        "format",
                        json!({"type": "string", "enum": ["auto", "codex", "unified"]}),
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
                "Copy a local workspace-relative file or directory to the fixed remote workspace at the same relative path so remote shell and remote file tools can see it. Requires path and optional timeout_seconds.",
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
                "Make a workspace-relative file or directory visible for attachment sending. Requires path and optional timeout_seconds.",
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

pub(crate) fn file_prompt_protocols() -> &'static [PromptProtocol] {
    FILE_PROMPT_PROTOCOLS
}

const FILE_PROMPT_PROTOCOLS: &[PromptProtocol] = &[
    PromptProtocol {
        id: "file.discovery",
        priority: 100,
        required_tools: &["grep", "file_read"],
        body: "For repository exploration, prefer grep to find files by content pattern with line numbers and optional context, and file_read for file contents. For path pattern discovery or directory listings, use narrow and bounded shell rg/ripgrep commands such as rg --files -g '<pattern>' or rg --files <dir>; if rg is unavailable, use bounded alternatives. Keep shell search scoped to the relevant directory, and avoid direct cat/head/tail when file_read covers the need. Recursive search skips slow remote mounts such as sshfs/NFS by default. Broad directory listings hide .stellaclaw/, but exact .stellaclaw/ paths remain valid for file tools.",
    },
    PromptProtocol {
        id: "file.editing",
        priority: 110,
        required_tools: &["file_write", "apply_patch"],
        body: "Use file_write for new files, complete rewrites, or append-only output; use apply_patch for targeted edits. After a successful apply_patch, do not re-read changed files just to verify that the patch applied; the tool reports failure when it does not apply. Re-read only when you need new context, a follow-up command or formatter may have rewritten the file, or a verification failure needs inspection.",
    },
    PromptProtocol {
        id: "file.attachment_visibility",
        priority: 120,
        required_tools: &["attachment_make_visible"],
        body: "Before referencing a file with <attachment>, use attachment_make_visible when that file is not yet visible to the conversation workspace; reference it only after the tool succeeds.",
    },
];

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
    if let Some(result) = patch::execute_patch_tool(tool_name, arguments, context)? {
        return Ok(Some(result));
    }
    visibility::execute_visibility_tool(tool_name, arguments, context)
}
