use std::{
    collections::BTreeSet,
    fs,
    io::Write,
    path::{Component, Path, PathBuf},
    process::{Command, Stdio},
};

use serde_json::{json, Map, Value};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PatchFormat {
    Auto,
    Codex,
    Unified,
}

#[derive(Debug, Clone)]
pub struct ApplyPatchOptions {
    pub workspace: PathBuf,
    pub format: PatchFormat,
    pub check: bool,
    pub strip: usize,
    pub reverse: bool,
    pub max_output_chars: usize,
}

pub fn apply_patch(patch: &str, options: &ApplyPatchOptions) -> Value {
    match apply_patch_inner(patch, options) {
        Ok(result) => result,
        Err(error) => json!({
            "applied": false,
            "error_kind": error.kind,
            "error": error.message,
        }),
    }
}

fn apply_patch_inner(patch: &str, options: &ApplyPatchOptions) -> Result<Value, PatchError> {
    let workspace = canonical_workspace(&options.workspace)?;
    match resolve_format(options.format, patch) {
        PatchFormat::Codex => apply_codex_patch(patch, options, &workspace),
        PatchFormat::Unified => apply_unified_patch(patch, options, &workspace),
        PatchFormat::Auto => unreachable!(),
    }
}

fn resolve_format(format: PatchFormat, patch: &str) -> PatchFormat {
    match format {
        PatchFormat::Auto => {
            if patch.trim_start().starts_with("*** Begin Patch") {
                PatchFormat::Codex
            } else {
                PatchFormat::Unified
            }
        }
        other => other,
    }
}

fn canonical_workspace(workspace: &Path) -> Result<PathBuf, PatchError> {
    let canonical = workspace.canonicalize().map_err(|error| {
        PatchError::io(format!(
            "failed to canonicalize workspace {}: {error}",
            workspace.display()
        ))
    })?;
    if !canonical.is_dir() {
        return Err(PatchError::invalid(format!(
            "workspace {} is not a directory",
            workspace.display()
        )));
    }
    Ok(canonical)
}

fn apply_unified_patch(
    patch: &str,
    options: &ApplyPatchOptions,
    workspace: &Path,
) -> Result<Value, PatchError> {
    let patch = normalize_unified_patch_paths(patch, Some(workspace), "workspace")?;
    let mut command = Command::new("git");
    command
        .arg("apply")
        .arg("--recount")
        .arg("--whitespace=nowarn")
        .arg(format!("-p{}", options.strip))
        .current_dir(workspace)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if options.reverse {
        command.arg("--reverse");
    }
    if options.check {
        command.arg("--check");
    }

    let mut child = command
        .spawn()
        .map_err(|error| PatchError::io(format!("failed to spawn git apply: {error}")))?;
    child
        .stdin
        .as_mut()
        .ok_or_else(|| PatchError::io("failed to open git apply stdin"))?
        .write_all(patch.as_bytes())
        .map_err(|error| PatchError::io(format!("failed to write patch to git apply: {error}")))?;
    let _ = child.stdin.take();
    let output = child
        .wait_with_output()
        .map_err(|error| PatchError::io(format!("failed to wait for git apply: {error}")))?;
    Ok(process_output_result(output, options.max_output_chars))
}

fn apply_codex_patch(
    patch: &str,
    options: &ApplyPatchOptions,
    workspace: &Path,
) -> Result<Value, PatchError> {
    if options.strip != 0 || options.reverse {
        return Err(PatchError::invalid(
            "format=codex does not support --strip or --reverse",
        ));
    }

    let ops = parse_codex_patch(patch)?;
    let mut files_changed = BTreeSet::new();
    for op in &ops {
        verify_codex_patch_op(op, workspace)?;
    }
    if options.check {
        for op in &ops {
            collect_codex_patch_paths(op, &mut files_changed);
        }
    } else {
        for op in &ops {
            apply_codex_patch_op(op, workspace, &mut files_changed)?;
        }
    }

    Ok(json!({
        "format": "codex",
        "applied": true,
        "check": options.check,
        "files_changed": files_changed.into_iter().collect::<Vec<_>>(),
        "operation_count": ops.len(),
    }))
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

fn parse_codex_patch(patch: &str) -> Result<Vec<CodexPatchOp>, PatchError> {
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
            return Err(PatchError::invalid("codex patch missing *** End Patch"));
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
                    return Err(PatchError::invalid("add file lines must start with +"));
                };
                content.push_str(line);
                content.push('\n');
                index += 1;
            }
            if content.is_empty() {
                return Err(PatchError::invalid(
                    "add file section must contain at least one + line",
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
                    return Err(PatchError::invalid(format!("invalid update line: {line}")));
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
                return Err(PatchError::invalid(
                    "update file section must contain changes or a move",
                ));
            }
            ops.push(CodexPatchOp::Update {
                path,
                move_to,
                chunks,
            });
            continue;
        }
        return Err(PatchError::invalid(format!(
            "unknown codex patch header: {line}"
        )));
    }
    if lines[index..].iter().any(|line| !line.trim().is_empty()) {
        return Err(PatchError::invalid(
            "unexpected content after *** End Patch",
        ));
    }
    if ops.is_empty() {
        return Err(PatchError::invalid(
            "codex patch must contain at least one file operation",
        ));
    }
    Ok(ops)
}

fn expect_line(lines: &[&str], index: usize, expected: &str) -> Result<(), PatchError> {
    if lines.get(index).copied() == Some(expected) {
        Ok(())
    } else {
        Err(PatchError::invalid(format!(
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

fn safe_patch_path(path: &str) -> Result<PathBuf, PatchError> {
    let path = path.trim();
    if path.is_empty() {
        return Err(PatchError::invalid("patch path must not be empty"));
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
        return Err(PatchError::invalid(
            "codex patch paths must be relative workspace paths without ..",
        ));
    }
    Ok(path)
}

fn verify_codex_patch_op(op: &CodexPatchOp, workspace: &Path) -> Result<(), PatchError> {
    match op {
        CodexPatchOp::Add { path, .. } => {
            let target = workspace_path_for_write(workspace, path)?;
            if target.exists() {
                return Err(PatchError::invalid(format!(
                    "{} already exists",
                    display_patch_path(path)
                )));
            }
        }
        CodexPatchOp::Delete { path } => {
            let target = workspace_existing_file(workspace, path)?;
            reject_symlink(&target)?;
        }
        CodexPatchOp::Update {
            path,
            move_to,
            chunks,
        } => {
            let source = workspace_existing_file(workspace, path)?;
            reject_symlink(&source)?;
            if let Some(move_to) = move_to {
                let target = workspace_path_for_write(workspace, move_to)?;
                if target.exists() && move_to != path {
                    return Err(PatchError::invalid(format!(
                        "{} already exists",
                        display_patch_path(move_to)
                    )));
                }
                if target.exists() {
                    reject_symlink(&target)?;
                }
            }
            let content = fs::read_to_string(&source).map_err(|error| {
                PatchError::io(format!("failed to read {}: {error}", source.display()))
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
) -> Result<(), PatchError> {
    let mut current = content.to_string();
    for chunk in chunks {
        if chunk.old.is_empty() {
            return Err(PatchError::invalid(format!(
                "update chunk for {} has no old/context lines",
                display_patch_path(path)
            )));
        }
        let matches = current.matches(&chunk.old).count();
        if matches != 1 {
            return Err(PatchError::invalid(format!(
                "update chunk for {} matched {} locations; include more context",
                display_patch_path(path),
                matches
            )));
        }
        current = current.replacen(&chunk.old, &chunk.new, 1);
    }
    Ok(())
}

fn apply_codex_patch_op(
    op: &CodexPatchOp,
    workspace: &Path,
    files_changed: &mut BTreeSet<String>,
) -> Result<(), PatchError> {
    match op {
        CodexPatchOp::Add { path, content } => {
            let target = workspace_path_for_write(workspace, path)?;
            ensure_parent_dir(&target, workspace)?;
            fs::write(&target, content.as_bytes()).map_err(|error| {
                PatchError::io(format!("failed to write {}: {error}", target.display()))
            })?;
            files_changed.insert(display_patch_path(path));
        }
        CodexPatchOp::Delete { path } => {
            let target = workspace_existing_file(workspace, path)?;
            reject_symlink(&target)?;
            fs::remove_file(&target).map_err(|error| {
                PatchError::io(format!("failed to delete {}: {error}", target.display()))
            })?;
            files_changed.insert(display_patch_path(path));
        }
        CodexPatchOp::Update {
            path,
            move_to,
            chunks,
        } => {
            let source = workspace_existing_file(workspace, path)?;
            reject_symlink(&source)?;
            let mut content = fs::read_to_string(&source).map_err(|error| {
                PatchError::io(format!("failed to read {}: {error}", source.display()))
            })?;
            for chunk in chunks {
                content = content.replacen(&chunk.old, &chunk.new, 1);
            }
            let output_path = move_to.as_ref().unwrap_or(path);
            let target = workspace_path_for_write(workspace, output_path)?;
            if target.exists() {
                reject_symlink(&target)?;
            }
            ensure_parent_dir(&target, workspace)?;
            fs::write(&target, content.as_bytes()).map_err(|error| {
                PatchError::io(format!("failed to write {}: {error}", target.display()))
            })?;
            if move_to.as_ref().is_some_and(|move_to| move_to != path) {
                fs::remove_file(&source).map_err(|error| {
                    PatchError::io(format!("failed to delete {}: {error}", source.display()))
                })?;
            }
            files_changed.insert(display_patch_path(path));
            if let Some(move_to) = move_to {
                files_changed.insert(display_patch_path(move_to));
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
            files_changed.insert(display_patch_path(path));
        }
    }
    if let CodexPatchOp::Update {
        move_to: Some(move_to),
        ..
    } = op
    {
        files_changed.insert(display_patch_path(move_to));
    }
}

fn workspace_existing_file(workspace: &Path, relative: &Path) -> Result<PathBuf, PatchError> {
    let target = workspace.join(relative);
    let canonical = target.canonicalize().map_err(|_| {
        PatchError::invalid(format!(
            "{} is not an existing file",
            display_patch_path(relative)
        ))
    })?;
    ensure_inside_workspace(workspace, &canonical)?;
    let metadata = fs::symlink_metadata(&target)
        .map_err(|error| PatchError::io(format!("failed to stat {}: {error}", target.display())))?;
    if !metadata.file_type().is_file() {
        return Err(PatchError::invalid(format!(
            "{} is not an existing file",
            display_patch_path(relative)
        )));
    }
    Ok(target)
}

fn workspace_path_for_write(workspace: &Path, relative: &Path) -> Result<PathBuf, PatchError> {
    let target = workspace.join(relative);
    ensure_parent_stays_inside_workspace(workspace, &target)?;
    Ok(target)
}

fn ensure_parent_dir(target: &Path, workspace: &Path) -> Result<(), PatchError> {
    let Some(parent) = target.parent() else {
        return Err(PatchError::invalid("patch target has no parent directory"));
    };
    ensure_parent_stays_inside_workspace(workspace, target)?;
    fs::create_dir_all(parent)
        .map_err(|error| PatchError::io(format!("failed to create {}: {error}", parent.display())))
}

fn ensure_parent_stays_inside_workspace(workspace: &Path, target: &Path) -> Result<(), PatchError> {
    let mut current = target.parent().ok_or_else(|| {
        PatchError::invalid(format!("patch path {} has no parent", target.display()))
    })?;
    while !current.exists() {
        current = current.parent().ok_or_else(|| {
            PatchError::invalid(format!(
                "patch path {} has no existing ancestor",
                target.display()
            ))
        })?;
    }
    let canonical = current.canonicalize().map_err(|error| {
        PatchError::io(format!(
            "failed to canonicalize parent {}: {error}",
            current.display()
        ))
    })?;
    ensure_inside_workspace(workspace, &canonical)
}

fn ensure_inside_workspace(workspace: &Path, path: &Path) -> Result<(), PatchError> {
    if path == workspace || path.starts_with(workspace) {
        Ok(())
    } else {
        Err(PatchError::invalid(format!(
            "patch path {} escapes workspace {}",
            path.display(),
            workspace.display()
        )))
    }
}

fn reject_symlink(path: &Path) -> Result<(), PatchError> {
    let metadata = fs::symlink_metadata(path)
        .map_err(|error| PatchError::io(format!("failed to stat {}: {error}", path.display())))?;
    if metadata.file_type().is_symlink() {
        return Err(PatchError::invalid(format!(
            "refusing to patch symlink {}",
            path.display()
        )));
    }
    Ok(())
}

fn display_patch_path(path: &Path) -> String {
    path.to_string_lossy().replace('\\', "/")
}

fn normalize_unified_patch_paths(
    patch: &str,
    base: Option<&Path>,
    base_label: &str,
) -> Result<String, PatchError> {
    let mut changed = false;
    let mut output = String::with_capacity(patch.len());
    for segment in patch.split_inclusive('\n') {
        let (line, newline) = segment
            .strip_suffix('\n')
            .map_or((segment, ""), |line| (line, "\n"));
        let normalized = if let Some(rest) = line.strip_prefix("--- ") {
            normalize_unified_file_header("--- ", rest, base, base_label, &mut changed)?
        } else if let Some(rest) = line.strip_prefix("+++ ") {
            normalize_unified_file_header("+++ ", rest, base, base_label, &mut changed)?
        } else if let Some(rest) = line.strip_prefix("diff --git ") {
            normalize_diff_git_header(rest, base, base_label, &mut changed)?
        } else {
            line.to_string()
        };
        output.push_str(&normalized);
        output.push_str(newline);
    }
    if changed {
        Ok(output)
    } else {
        Ok(patch.to_string())
    }
}

fn normalize_unified_file_header(
    prefix: &str,
    rest: &str,
    base: Option<&Path>,
    base_label: &str,
    changed: &mut bool,
) -> Result<String, PatchError> {
    let (path, suffix) = split_unified_header_path(rest);
    let normalized = normalize_unified_path_token(path, base, base_label)?;
    if normalized != path {
        *changed = true;
    }
    Ok(format!("{prefix}{normalized}{suffix}"))
}

fn normalize_diff_git_header(
    rest: &str,
    base: Option<&Path>,
    base_label: &str,
    changed: &mut bool,
) -> Result<String, PatchError> {
    let mut parts = rest.split_whitespace();
    let Some(old_path) = parts.next() else {
        return Ok("diff --git ".to_string());
    };
    let Some(new_path) = parts.next() else {
        return Ok(format!("diff --git {rest}"));
    };
    if parts.next().is_some() {
        return Ok(format!("diff --git {rest}"));
    }
    let normalized_old = normalize_unified_path_token(old_path, base, base_label)?;
    let normalized_new = normalize_unified_path_token(new_path, base, base_label)?;
    if normalized_old != old_path || normalized_new != new_path {
        *changed = true;
    }
    Ok(format!("diff --git {normalized_old} {normalized_new}"))
}

fn split_unified_header_path(rest: &str) -> (&str, &str) {
    if let Some(index) = rest.find('\t') {
        return rest.split_at(index);
    }
    if rest.starts_with('/') {
        if let Some(index) = rest.find(char::is_whitespace) {
            return rest.split_at(index);
        }
    }
    (rest, "")
}

fn normalize_unified_path_token(
    path: &str,
    base: Option<&Path>,
    base_label: &str,
) -> Result<String, PatchError> {
    if path == "/dev/null" || !Path::new(path).is_absolute() {
        return Ok(path.to_string());
    }
    let Some(base) = base else {
        return Err(PatchError::invalid(format!(
            "unified patch path {path:?} is absolute; use a workspace-relative path"
        )));
    };
    let relative = Path::new(path).strip_prefix(base).map_err(|_| {
        PatchError::invalid(format!(
            "unified patch path {path:?} is absolute and outside the {base_label} {}; use a relative patch path",
            base.display()
        ))
    })?;
    if relative.as_os_str().is_empty() {
        return Err(PatchError::invalid(format!(
            "unified patch path {path:?} points at the {base_label} root; use a file path"
        )));
    }
    Ok(relative.display().to_string())
}

fn process_output_result(output: std::process::Output, max_output_chars: usize) -> Value {
    let stdout_text = String::from_utf8_lossy(&output.stdout).to_string();
    let stderr_text = String::from_utf8_lossy(&output.stderr).to_string();
    let (stdout, stdout_truncated) = truncate_text(&stdout_text, max_output_chars);
    let (stderr, stderr_truncated) = truncate_text(&stderr_text, max_output_chars);

    let mut result = Map::new();
    result.insert("format".to_string(), Value::String("unified".to_string()));
    result.insert("applied".to_string(), Value::Bool(output.status.success()));
    if let Some(returncode) = output.status.code() {
        result.insert("returncode".to_string(), Value::from(returncode));
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
    Value::Object(result)
}

fn truncate_text(text: &str, max_chars: usize) -> (String, bool) {
    if text.chars().count() <= max_chars {
        return (text.to_string(), false);
    }
    let mut output = text.chars().take(max_chars).collect::<String>();
    output.push_str("\n... truncated ...");
    (output, true)
}

#[derive(Debug)]
struct PatchError {
    kind: &'static str,
    message: String,
}

impl PatchError {
    fn invalid(message: impl Into<String>) -> Self {
        Self {
            kind: "invalid_arguments",
            message: message.into(),
        }
    }

    fn io(message: impl Into<String>) -> Self {
        Self {
            kind: "io",
            message: message.into(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    #[test]
    fn applies_codex_add_update_delete_and_move() {
        let workspace = temp_workspace("codex-full");
        fs::write(workspace.join("old.txt"), "alpha\nbeta\n").expect("write old");
        fs::write(workspace.join("delete.txt"), "remove me\n").expect("write delete");
        let patch = "\
*** Begin Patch
*** Add File: added.txt
+new file
*** Update File: old.txt
*** Move to: moved.txt
@@
 alpha
-beta
+gamma
*** Delete File: delete.txt
*** End Patch
";
        let result = apply_patch(patch, &codex_options(&workspace, false));

        assert_eq!(result["applied"], true);
        assert_eq!(
            fs::read_to_string(workspace.join("added.txt")).unwrap(),
            "new file\n"
        );
        assert_eq!(
            fs::read_to_string(workspace.join("moved.txt")).unwrap(),
            "alpha\ngamma\n"
        );
        assert!(!workspace.join("old.txt").exists());
        assert!(!workspace.join("delete.txt").exists());
        let _ = fs::remove_dir_all(workspace);
    }

    #[test]
    fn codex_check_does_not_write_files() {
        let workspace = temp_workspace("codex-check");
        let patch = "\
*** Begin Patch
*** Add File: added.txt
+new file
*** End Patch
";
        let result = apply_patch(patch, &codex_options(&workspace, true));

        assert_eq!(result["applied"], true);
        assert!(!workspace.join("added.txt").exists());
        let _ = fs::remove_dir_all(workspace);
    }

    #[test]
    fn rejects_ambiguous_update_chunks() {
        let workspace = temp_workspace("codex-ambiguous");
        fs::write(workspace.join("a.txt"), "same\nsame\n").expect("write source");
        let patch = "\
*** Begin Patch
*** Update File: a.txt
@@
-same
+other
*** End Patch
";
        let result = apply_patch(patch, &codex_options(&workspace, false));

        assert_eq!(result["applied"], false);
        assert!(result["error"]
            .as_str()
            .expect("error text")
            .contains("matched 2 locations"));
        let _ = fs::remove_dir_all(workspace);
    }

    #[test]
    fn normalizes_absolute_unified_paths_under_workspace() {
        let workspace = Path::new("/home/me/work");
        let patch = "\
diff --git /home/me/work/src/a.py /home/me/work/src/a.py
--- /home/me/work/src/a.py\t2026-05-08
+++ /home/me/work/src/a.py\t2026-05-08
@@ -1 +1 @@
-old
+new
";
        let normalized = normalize_unified_patch_paths(patch, Some(workspace), "workspace")
            .expect("patch should normalize");

        assert!(normalized.contains("diff --git src/a.py src/a.py"));
        assert!(normalized.contains("--- src/a.py\t2026-05-08"));
        assert!(normalized.contains("+++ src/a.py\t2026-05-08"));
    }

    #[test]
    fn rejects_absolute_unified_paths_outside_workspace() {
        let err = normalize_unified_patch_paths(
            "--- /tmp/outside.txt\n+++ /tmp/outside.txt\n",
            Some(Path::new("/home/me/work")),
            "workspace",
        )
        .expect_err("outside path should fail");

        assert!(err.message.contains("outside the workspace"));
    }

    fn codex_options(workspace: &Path, check: bool) -> ApplyPatchOptions {
        ApplyPatchOptions {
            workspace: workspace.to_path_buf(),
            format: PatchFormat::Codex,
            check,
            strip: 0,
            reverse: false,
            max_output_chars: 1000,
        }
    }

    fn temp_workspace(label: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock")
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "stellaclaw-apply-patch-{label}-{}-{nanos}",
            std::process::id()
        ));
        fs::create_dir_all(&root).expect("create temp workspace");
        root
    }
}
