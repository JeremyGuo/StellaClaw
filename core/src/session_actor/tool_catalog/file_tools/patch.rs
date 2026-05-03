use std::{
    collections::BTreeSet,
    fs,
    io::Write,
    path::{Component, Path, PathBuf},
    process::{Command, Stdio},
    time::{SystemTime, UNIX_EPOCH},
};

use serde_json::{json, Map, Value};

use crate::session_actor::tool_runtime::{
    bool_arg_with_default, clamp_tool_output_chars, run_remote_command_with_stdin, shell_quote,
    string_arg, string_arg_with_default, truncate_tool_text, usize_arg_with_default,
    ExecutionTarget, LocalToolError, ToolExecutionContext,
};

pub(super) fn execute_patch_tool(
    tool_name: &str,
    arguments: &Map<String, Value>,
    context: &ToolExecutionContext<'_>,
) -> Result<Option<Value>, LocalToolError> {
    if tool_name != "apply_patch" {
        return Ok(None);
    }

    let result = match context.execution_target(arguments)? {
        ExecutionTarget::Local => {
            apply_patch_local(arguments, context.workspace_root, context.data_root)?
        }
        ExecutionTarget::RemoteSsh { host, cwd } => apply_patch_remote(
            arguments,
            context.workspace_root,
            context.data_root,
            &host,
            cwd.as_deref(),
        )?,
    };
    Ok(Some(result))
}

fn apply_patch_local(
    arguments: &Map<String, Value>,
    workspace_root: &Path,
    data_root: &Path,
) -> Result<Value, LocalToolError> {
    let patch = string_arg(arguments, "patch")?;
    let check = bool_arg_with_default(arguments, "check", false)?;
    let max_output_chars =
        clamp_tool_output_chars(usize_arg_with_default(arguments, "max_output_chars", 1000)?);
    match patch_format(arguments, &patch)? {
        PatchFormat::Codex => apply_codex_patch_local(arguments, workspace_root, data_root),
        PatchFormat::Unified => apply_unified_patch_local(
            arguments,
            workspace_root,
            data_root,
            check,
            max_output_chars,
        ),
    }
}

fn apply_unified_patch_local(
    arguments: &Map<String, Value>,
    workspace_root: &Path,
    data_root: &Path,
    check: bool,
    max_output_chars: usize,
) -> Result<Value, LocalToolError> {
    let patch = string_arg(arguments, "patch")?;
    let strip = usize_arg_with_default(arguments, "strip", 0)?;
    let reverse = bool_arg_with_default(arguments, "reverse", false)?;
    let out_path = make_patch_output_dir(data_root)?;

    let mut command = Command::new("git");
    command
        .arg("apply")
        .arg("--recount")
        .arg("--whitespace=nowarn")
        .arg(format!("-p{strip}"))
        .current_dir(workspace_root)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if reverse {
        command.arg("--reverse");
    }
    if check {
        command.arg("--check");
    }

    let mut child = command
        .spawn()
        .map_err(|error| LocalToolError::Io(format!("failed to spawn git apply: {error}")))?;
    child
        .stdin
        .as_mut()
        .ok_or_else(|| LocalToolError::Io("failed to open git apply stdin".to_string()))?
        .write_all(patch.as_bytes())
        .map_err(|error| LocalToolError::Io(format!("failed to write patch: {error}")))?;
    let _ = child.stdin.take();
    let output = child
        .wait_with_output()
        .map_err(|error| LocalToolError::Io(format!("failed to wait for git apply: {error}")))?;
    patch_result(output, out_path, None, max_output_chars)
}

fn apply_patch_remote(
    arguments: &Map<String, Value>,
    _workspace_root: &Path,
    data_root: &Path,
    host: &str,
    cwd: Option<&str>,
) -> Result<Value, LocalToolError> {
    let patch = string_arg(arguments, "patch")?;
    if patch_format(arguments, &patch)? == PatchFormat::Codex {
        return Err(LocalToolError::InvalidArguments(
            "format=codex is currently supported only for local workspace patches; use format=unified for remote patches".to_string(),
        ));
    }
    let strip = usize_arg_with_default(arguments, "strip", 0)?;
    let reverse = bool_arg_with_default(arguments, "reverse", false)?;
    let check = bool_arg_with_default(arguments, "check", false)?;
    let max_output_chars =
        clamp_tool_output_chars(usize_arg_with_default(arguments, "max_output_chars", 1000)?);
    let mut args = vec![
        "git".to_string(),
        "apply".to_string(),
        "--recount".to_string(),
        "--whitespace=nowarn".to_string(),
        format!("-p{strip}"),
    ];
    if reverse {
        args.push("--reverse".to_string());
    }
    if check {
        args.push("--check".to_string());
    }
    let remote_command = args
        .iter()
        .map(|arg| shell_quote(arg))
        .collect::<Vec<_>>()
        .join(" ");
    let remote_command = match cwd {
        Some(cwd) => format!("cd {} && {}", shell_quote(cwd), remote_command),
        None => remote_command,
    };
    let output = run_remote_command_with_stdin(host, &remote_command, patch.as_bytes())?;
    let out_path = make_patch_output_dir(data_root)?;
    patch_result(output, out_path, Some(host), max_output_chars)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PatchFormat {
    Codex,
    Unified,
}

fn patch_format(
    arguments: &Map<String, Value>,
    patch: &str,
) -> Result<PatchFormat, LocalToolError> {
    match string_arg_with_default(arguments, "format", "auto")?
        .trim()
        .to_ascii_lowercase()
        .as_str()
    {
        "auto" => {
            if patch.trim_start().starts_with("*** Begin Patch") {
                Ok(PatchFormat::Codex)
            } else {
                Ok(PatchFormat::Unified)
            }
        }
        "codex" => Ok(PatchFormat::Codex),
        "unified" => Ok(PatchFormat::Unified),
        other => Err(LocalToolError::InvalidArguments(format!(
            "unsupported patch format {other}; expected auto, codex, or unified"
        ))),
    }
}

#[derive(Debug, Clone)]
enum CodexPatchOp {
    Add {
        path: PathBuf,
        content: String,
    },
    Delete {
        path: PathBuf,
    },
    Update {
        path: PathBuf,
        move_to: Option<PathBuf>,
        chunks: Vec<CodexPatchChunk>,
    },
}

#[derive(Debug, Clone)]
struct CodexPatchChunk {
    old: String,
    new: String,
}

fn apply_codex_patch_local(
    arguments: &Map<String, Value>,
    workspace_root: &Path,
    data_root: &Path,
) -> Result<Value, LocalToolError> {
    let strip = usize_arg_with_default(arguments, "strip", 0)?;
    let reverse = bool_arg_with_default(arguments, "reverse", false)?;
    if strip != 0 || reverse {
        return Err(LocalToolError::InvalidArguments(
            "format=codex does not support strip or reverse".to_string(),
        ));
    }
    let patch = string_arg(arguments, "patch")?;
    let check = bool_arg_with_default(arguments, "check", false)?;
    let ops = parse_codex_patch(&patch)?;
    let mut files_changed = BTreeSet::new();
    for op in &ops {
        verify_codex_patch_op(op, workspace_root)?;
    }
    if !check {
        for op in &ops {
            apply_codex_patch_op(op, workspace_root, &mut files_changed)?;
        }
    } else {
        for op in &ops {
            collect_codex_patch_paths(op, &mut files_changed);
        }
    }

    let out_path = make_patch_output_dir(data_root)?;
    let summary = json!({
        "format": "codex",
        "applied": true,
        "check": check,
        "files_changed": files_changed.iter().cloned().collect::<Vec<_>>(),
        "operation_count": ops.len(),
    });
    let stdout = serde_json::to_vec_pretty(&summary).unwrap_or_default();
    fs::write(out_path.join("stdout"), &stdout).map_err(|error| {
        LocalToolError::Io(format!("failed to write patch stdout artifact: {error}"))
    })?;
    fs::write(out_path.join("stderr"), []).map_err(|error| {
        LocalToolError::Io(format!("failed to write patch stderr artifact: {error}"))
    })?;

    let mut result = summary.as_object().cloned().unwrap_or_else(Map::new);
    result.insert(
        "out_path".to_string(),
        Value::String(out_path.display().to_string()),
    );
    Ok(Value::Object(result))
}

fn parse_codex_patch(patch: &str) -> Result<Vec<CodexPatchOp>, LocalToolError> {
    let normalized = patch.replace("\r\n", "\n");
    let lines = normalized.split('\n').collect::<Vec<_>>();
    let mut index = 0usize;
    while index < lines.len() && lines[index].trim().is_empty() {
        index += 1;
    }
    expect_line(&lines, index, "*** Begin Patch")?;
    index += 1;
    let mut ops = Vec::new();
    loop {
        let Some(line) = lines.get(index).copied() else {
            return Err(LocalToolError::InvalidArguments(
                "codex patch missing *** End Patch".to_string(),
            ));
        };
        if line == "*** End Patch" {
            index += 1;
            break;
        }
        if let Some(path) = line.strip_prefix("*** Add File: ") {
            let path = safe_patch_path(path)?;
            index += 1;
            let mut content = String::new();
            while let Some(line) = lines.get(index).copied() {
                if is_codex_patch_header(line) {
                    break;
                }
                let Some(line) = line.strip_prefix('+') else {
                    return Err(LocalToolError::InvalidArguments(
                        "add file lines must start with +".to_string(),
                    ));
                };
                content.push_str(line);
                content.push('\n');
                index += 1;
            }
            if content.is_empty() {
                return Err(LocalToolError::InvalidArguments(
                    "add file section must contain at least one + line".to_string(),
                ));
            }
            ops.push(CodexPatchOp::Add { path, content });
            continue;
        }
        if let Some(path) = line.strip_prefix("*** Delete File: ") {
            ops.push(CodexPatchOp::Delete {
                path: safe_patch_path(path)?,
            });
            index += 1;
            continue;
        }
        if let Some(path) = line.strip_prefix("*** Update File: ") {
            let path = safe_patch_path(path)?;
            index += 1;
            let move_to = if let Some(line) = lines.get(index).copied() {
                if let Some(path) = line.strip_prefix("*** Move to: ") {
                    index += 1;
                    Some(safe_patch_path(path)?)
                } else {
                    None
                }
            } else {
                None
            };
            let mut chunks = Vec::new();
            let mut current = CodexPatchChunk {
                old: String::new(),
                new: String::new(),
            };
            while let Some(line) = lines.get(index).copied() {
                if is_codex_patch_header(line) {
                    break;
                }
                if line == "*** End of File" {
                    index += 1;
                    continue;
                }
                if line == "@@" || line.starts_with("@@ ") {
                    push_non_empty_chunk(&mut chunks, &mut current);
                    index += 1;
                    continue;
                }
                let Some((kind, text)) = split_patch_line(line) else {
                    return Err(LocalToolError::InvalidArguments(format!(
                        "invalid update line: {line}"
                    )));
                };
                match kind {
                    ' ' => {
                        current.old.push_str(text);
                        current.old.push('\n');
                        current.new.push_str(text);
                        current.new.push('\n');
                    }
                    '-' => {
                        current.old.push_str(text);
                        current.old.push('\n');
                    }
                    '+' => {
                        current.new.push_str(text);
                        current.new.push('\n');
                    }
                    _ => unreachable!(),
                }
                index += 1;
            }
            push_non_empty_chunk(&mut chunks, &mut current);
            if move_to.is_none() && chunks.is_empty() {
                return Err(LocalToolError::InvalidArguments(
                    "update file section must contain changes or a move".to_string(),
                ));
            }
            ops.push(CodexPatchOp::Update {
                path,
                move_to,
                chunks,
            });
            continue;
        }
        return Err(LocalToolError::InvalidArguments(format!(
            "unknown codex patch header: {line}"
        )));
    }
    if lines[index..].iter().any(|line| !line.trim().is_empty()) {
        return Err(LocalToolError::InvalidArguments(
            "unexpected content after *** End Patch".to_string(),
        ));
    }
    if ops.is_empty() {
        return Err(LocalToolError::InvalidArguments(
            "codex patch must contain at least one file operation".to_string(),
        ));
    }
    Ok(ops)
}

fn expect_line(lines: &[&str], index: usize, expected: &str) -> Result<(), LocalToolError> {
    if lines.get(index).copied() == Some(expected) {
        Ok(())
    } else {
        Err(LocalToolError::InvalidArguments(format!(
            "codex patch must start with {expected}"
        )))
    }
}

fn is_codex_patch_header(line: &str) -> bool {
    line == "*** End Patch"
        || line.starts_with("*** Add File: ")
        || line.starts_with("*** Delete File: ")
        || line.starts_with("*** Update File: ")
}

fn split_patch_line(line: &str) -> Option<(char, &str)> {
    let mut chars = line.chars();
    let kind = chars.next()?;
    if !matches!(kind, ' ' | '-' | '+') {
        return None;
    }
    Some((kind, chars.as_str()))
}

fn push_non_empty_chunk(chunks: &mut Vec<CodexPatchChunk>, current: &mut CodexPatchChunk) {
    if current.old.is_empty() && current.new.is_empty() {
        return;
    }
    chunks.push(current.clone());
    current.old.clear();
    current.new.clear();
}

fn safe_patch_path(path: &str) -> Result<PathBuf, LocalToolError> {
    let path = path.trim();
    if path.is_empty() {
        return Err(LocalToolError::InvalidArguments(
            "patch path must not be empty".to_string(),
        ));
    }
    let path = PathBuf::from(path);
    if path.is_absolute()
        || path.components().any(|component| {
            matches!(
                component,
                Component::ParentDir | Component::Prefix(_) | Component::RootDir
            )
        })
    {
        return Err(LocalToolError::InvalidArguments(
            "codex patch paths must be relative workspace paths without ..".to_string(),
        ));
    }
    Ok(path)
}

fn verify_codex_patch_op(op: &CodexPatchOp, workspace_root: &Path) -> Result<(), LocalToolError> {
    match op {
        CodexPatchOp::Add { path, .. } => {
            let target = workspace_root.join(path);
            if target.exists() {
                return Err(LocalToolError::InvalidArguments(format!(
                    "{} already exists",
                    path.display()
                )));
            }
        }
        CodexPatchOp::Delete { path } => {
            let target = workspace_root.join(path);
            if !target.is_file() {
                return Err(LocalToolError::InvalidArguments(format!(
                    "{} is not an existing file",
                    path.display()
                )));
            }
        }
        CodexPatchOp::Update {
            path,
            move_to,
            chunks,
        } => {
            let source = workspace_root.join(path);
            if !source.is_file() {
                return Err(LocalToolError::InvalidArguments(format!(
                    "{} is not an existing file",
                    path.display()
                )));
            }
            if let Some(move_to) = move_to {
                let target = workspace_root.join(move_to);
                if target.exists() && move_to != path {
                    return Err(LocalToolError::InvalidArguments(format!(
                        "{} already exists",
                        move_to.display()
                    )));
                }
            }
            let content = fs::read_to_string(&source).map_err(|error| {
                LocalToolError::Io(format!("failed to read {}: {error}", source.display()))
            })?;
            verify_chunks_match(path, &content, chunks)?;
        }
    }
    Ok(())
}

fn verify_chunks_match(
    path: &Path,
    content: &str,
    chunks: &[CodexPatchChunk],
) -> Result<(), LocalToolError> {
    let mut current = content.to_string();
    for chunk in chunks {
        if chunk.old.is_empty() {
            return Err(LocalToolError::InvalidArguments(format!(
                "update chunk for {} has no old/context lines",
                path.display()
            )));
        }
        let matches = current.matches(&chunk.old).count();
        if matches != 1 {
            return Err(LocalToolError::InvalidArguments(format!(
                "update chunk for {} matched {} locations; include more context",
                path.display(),
                matches
            )));
        }
        current = current.replacen(&chunk.old, &chunk.new, 1);
    }
    Ok(())
}

fn apply_codex_patch_op(
    op: &CodexPatchOp,
    workspace_root: &Path,
    files_changed: &mut BTreeSet<String>,
) -> Result<(), LocalToolError> {
    match op {
        CodexPatchOp::Add { path, content } => {
            let target = workspace_root.join(path);
            if let Some(parent) = target.parent() {
                fs::create_dir_all(parent).map_err(|error| {
                    LocalToolError::Io(format!("failed to create {}: {error}", parent.display()))
                })?;
            }
            fs::write(&target, content.as_bytes()).map_err(|error| {
                LocalToolError::Io(format!("failed to write {}: {error}", target.display()))
            })?;
            files_changed.insert(path.display().to_string());
        }
        CodexPatchOp::Delete { path } => {
            let target = workspace_root.join(path);
            fs::remove_file(&target).map_err(|error| {
                LocalToolError::Io(format!("failed to delete {}: {error}", target.display()))
            })?;
            files_changed.insert(path.display().to_string());
        }
        CodexPatchOp::Update {
            path,
            move_to,
            chunks,
        } => {
            let source = workspace_root.join(path);
            let mut content = fs::read_to_string(&source).map_err(|error| {
                LocalToolError::Io(format!("failed to read {}: {error}", source.display()))
            })?;
            for chunk in chunks {
                content = content.replacen(&chunk.old, &chunk.new, 1);
            }
            let output_path = move_to.as_ref().unwrap_or(path);
            let target = workspace_root.join(output_path);
            if let Some(parent) = target.parent() {
                fs::create_dir_all(parent).map_err(|error| {
                    LocalToolError::Io(format!("failed to create {}: {error}", parent.display()))
                })?;
            }
            fs::write(&target, content.as_bytes()).map_err(|error| {
                LocalToolError::Io(format!("failed to write {}: {error}", target.display()))
            })?;
            if move_to.as_ref().is_some_and(|move_to| move_to != path) {
                fs::remove_file(&source).map_err(|error| {
                    LocalToolError::Io(format!("failed to delete {}: {error}", source.display()))
                })?;
            }
            files_changed.insert(path.display().to_string());
            if let Some(move_to) = move_to {
                files_changed.insert(move_to.display().to_string());
            }
        }
    }
    Ok(())
}

fn collect_codex_patch_paths(op: &CodexPatchOp, files_changed: &mut BTreeSet<String>) {
    match op {
        CodexPatchOp::Add { path, .. }
        | CodexPatchOp::Delete { path }
        | CodexPatchOp::Update { path, .. } => {
            files_changed.insert(path.display().to_string());
        }
    }
    if let CodexPatchOp::Update {
        move_to: Some(move_to),
        ..
    } = op
    {
        files_changed.insert(move_to.display().to_string());
    }
}

fn patch_result(
    output: std::process::Output,
    out_path: PathBuf,
    remote: Option<&str>,
    max_output_chars: usize,
) -> Result<Value, LocalToolError> {
    fs::write(out_path.join("stdout"), &output.stdout).map_err(|error| {
        LocalToolError::Io(format!("failed to write patch stdout artifact: {error}"))
    })?;
    fs::write(out_path.join("stderr"), &output.stderr).map_err(|error| {
        LocalToolError::Io(format!("failed to write patch stderr artifact: {error}"))
    })?;

    let stdout_text = String::from_utf8_lossy(&output.stdout).to_string();
    let stderr_text = String::from_utf8_lossy(&output.stderr).to_string();
    let (stdout, stdout_truncated) = truncate_tool_text(&stdout_text, max_output_chars);
    let (stderr, stderr_truncated) = truncate_tool_text(&stderr_text, max_output_chars);

    let mut result = Map::new();
    result.insert("applied".to_string(), Value::Bool(output.status.success()));
    result.insert(
        "out_path".to_string(),
        Value::String(out_path.display().to_string()),
    );
    if let Some(returncode) = output.status.code() {
        result.insert("returncode".to_string(), Value::from(returncode));
    }
    if let Some(remote) = remote {
        result.insert("remote".to_string(), Value::String(remote.to_string()));
    }
    if !stdout.is_empty() {
        result.insert("stdout".to_string(), Value::String(stdout));
    }
    if !stderr.is_empty() {
        result.insert("stderr".to_string(), Value::String(stderr));
    }
    if stdout_truncated {
        result.insert("stdout_truncated".to_string(), Value::Bool(true));
    }
    if stderr_truncated {
        result.insert("stderr_truncated".to_string(), Value::Bool(true));
    }
    Ok(Value::Object(result))
}

fn make_patch_output_dir(data_root: &Path) -> Result<PathBuf, LocalToolError> {
    let out_path = data_root
        .join(".stellaclaw")
        .join("output")
        .join("apply_patch")
        .join(nonce());
    fs::create_dir_all(&out_path).map_err(|error| {
        LocalToolError::Io(format!("failed to create {}: {error}", out_path.display()))
    })?;
    Ok(out_path)
}

fn nonce() -> String {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos().to_string())
        .unwrap_or_else(|_| "0".to_string())
}
