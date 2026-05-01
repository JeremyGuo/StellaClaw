use std::path::Path;

use anyhow::{anyhow, Context, Result};
use stellaclaw_core::session_actor::{ChatMessage, ChatMessageItem, FileItem, ToolCallItem};

use crate::channels::types::{OutgoingAttachment, OutgoingAttachmentKind};

pub(crate) struct AttachmentExtraction {
    pub clean_text: String,
    pub marked_text: String,
    pub attachments: Vec<OutgoingAttachment>,
}

pub(crate) fn extract_attachment_references(
    text: &str,
    workspace_root: &Path,
    shared_root: &Path,
) -> Result<(String, Vec<OutgoingAttachment>)> {
    let extracted =
        extract_attachment_references_with_markers(text, workspace_root, shared_root, 0)?;
    Ok((extracted.clean_text, extracted.attachments))
}

pub(crate) fn extract_attachment_references_with_markers(
    text: &str,
    workspace_root: &Path,
    shared_root: &Path,
    base_attachment_index: usize,
) -> Result<AttachmentExtraction> {
    const START: &str = "<attachment>";
    const END: &str = "</attachment>";

    let mut clean = String::with_capacity(text.len());
    let mut marked = String::with_capacity(text.len());
    let mut attachments = Vec::new();
    let mut cursor = 0usize;

    while let Some(start_rel) = text[cursor..].find(START) {
        let start = cursor + start_rel;
        if is_inside_fenced_code_block(text, start) {
            let start_end = start + START.len();
            clean.push_str(&text[cursor..start_end]);
            marked.push_str(&text[cursor..start_end]);
            cursor = start_end;
            continue;
        }
        clean.push_str(&text[cursor..start]);
        marked.push_str(&text[cursor..start]);
        let path_start = start + START.len();
        let Some(end_rel) = text[path_start..].find(END) else {
            clean.push_str(&text[start..]);
            marked.push_str(&text[start..]);
            return Ok(AttachmentExtraction {
                clean_text: clean.trim().to_string(),
                marked_text: marked.trim().to_string(),
                attachments,
            });
        };
        let path_end = path_start + end_rel;
        let path_text = text[path_start..path_end].trim();
        if !path_text.is_empty() {
            let marker = attachment_marker(base_attachment_index + attachments.len());
            attachments.push(resolve_outgoing_attachment(
                workspace_root,
                shared_root,
                path_text,
            )?);
            marked.push_str(&marker);
        }
        cursor = path_end + END.len();
    }

    clean.push_str(&text[cursor..]);
    marked.push_str(&text[cursor..]);
    Ok(AttachmentExtraction {
        clean_text: clean.trim().to_string(),
        marked_text: marked.trim().to_string(),
        attachments,
    })
}

pub(crate) fn attachment_marker(index: usize) -> String {
    format!("[[attachment:{index}]]")
}

pub(crate) fn strip_attachment_tags(text: &str) -> String {
    const START: &str = "<attachment>";
    const END: &str = "</attachment>";

    let mut clean = String::with_capacity(text.len());
    let mut cursor = 0usize;
    while let Some(start_rel) = text[cursor..].find(START) {
        let start = cursor + start_rel;
        if is_inside_fenced_code_block(text, start) {
            let start_end = start + START.len();
            clean.push_str(&text[cursor..start_end]);
            cursor = start_end;
            continue;
        }
        clean.push_str(&text[cursor..start]);
        let path_start = start + START.len();
        let Some(end_rel) = text[path_start..].find(END) else {
            clean.push_str(&text[start..]);
            return clean;
        };
        let path_end = path_start + end_rel;
        let path_text = text[path_start..path_end].trim();
        if !path_text.is_empty() {
            clean.push_str(path_text);
        }
        cursor = path_end + END.len();
    }
    clean.push_str(&text[cursor..]);
    clean
}

fn is_inside_fenced_code_block(text: &str, byte_index: usize) -> bool {
    let mut inside = false;
    let mut offset = 0usize;
    for line in text.split_inclusive('\n') {
        if offset >= byte_index {
            break;
        }
        let trimmed = line.trim_start();
        if trimmed.starts_with("```") || trimmed.starts_with("~~~") {
            inside = !inside;
        }
        offset += line.len();
    }
    inside
}

fn is_shared_attachment_path(path_text: &str) -> bool {
    // Exact directory references (no trailing separator).
    if path_text == ".stellaclaw/stellaclaw_shared" || path_text == "shared" {
        return true;
    }
    // Prefix with content after the separator.
    for prefix in [
        ".stellaclaw/stellaclaw_shared/",
        ".stellaclaw/stellaclaw_shared\\",
        "shared/",
        "shared\\",
    ] {
        if let Some(relative) = path_text.strip_prefix(prefix) {
            if !relative.trim().is_empty() {
                return true;
            }
        }
    }
    false
}

fn resolve_outgoing_attachment(
    workspace_root: &Path,
    shared_root: &Path,
    path_text: &str,
) -> Result<OutgoingAttachment> {
    let joined = attachment_candidate_path(workspace_root, path_text);
    let canonical = joined
        .canonicalize()
        .with_context(|| format!("attachment path does not exist: {}", joined.display()))?;
    let root = workspace_root
        .canonicalize()
        .with_context(|| format!("failed to canonicalize {}", workspace_root.display()))?;
    let allowed_runtime_shared = is_shared_attachment_path(path_text);
    let shared_root = shared_root.canonicalize().ok();
    let in_runtime_shared = allowed_runtime_shared
        && shared_root
            .as_ref()
            .is_some_and(|shared_root| canonical.starts_with(shared_root));
    if !canonical.starts_with(&root) && !in_runtime_shared {
        return Err(anyhow!(
            "attachment path escapes conversation root: {}",
            canonical.display()
        ));
    }
    if !canonical.is_file() {
        return Err(anyhow!(
            "attachment path is not a regular file: {}",
            canonical.display()
        ));
    }
    Ok(OutgoingAttachment {
        kind: infer_outgoing_attachment_kind(&canonical),
        path: canonical,
    })
}

fn attachment_candidate_path(workspace_root: &Path, path_text: &str) -> std::path::PathBuf {
    let path = Path::new(path_text);
    if !path.is_absolute() {
        return workspace_root.join(path);
    }

    if let Some(remapped) = remap_absolute_conversation_path(workspace_root, path) {
        if remapped.exists() {
            return remapped;
        }
    }
    path.to_path_buf()
}

fn remap_absolute_conversation_path(
    workspace_root: &Path,
    path: &Path,
) -> Option<std::path::PathBuf> {
    let conversation_name = workspace_root.file_name()?;
    let mut components = path.components().peekable();
    while let Some(component) = components.next() {
        if component.as_os_str() != "conversations" {
            continue;
        }
        let Some(next) = components.next() else {
            return None;
        };
        if next.as_os_str() != conversation_name {
            continue;
        }
        let mut remapped = workspace_root.to_path_buf();
        for rest in components {
            remapped.push(rest.as_os_str());
        }
        return Some(remapped);
    }
    None
}

fn infer_outgoing_attachment_kind(path: &Path) -> OutgoingAttachmentKind {
    match path
        .extension()
        .and_then(|value| value.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase()
        .as_str()
    {
        "png" | "jpg" | "jpeg" | "webp" => OutgoingAttachmentKind::Image,
        "gif" => OutgoingAttachmentKind::Animation,
        "mp3" | "wav" => OutgoingAttachmentKind::Audio,
        "ogg" => OutgoingAttachmentKind::Voice,
        "mp4" | "mov" | "mkv" => OutgoingAttachmentKind::Video,
        _ => OutgoingAttachmentKind::Document,
    }
}

pub fn render_chat_message(message: &ChatMessage) -> String {
    let mut parts = Vec::new();
    for item in &message.data {
        match item {
            ChatMessageItem::Context(context) => parts.push(context.text.clone()),
            ChatMessageItem::File(file) => parts.push(render_file_item(file)),
            ChatMessageItem::Reasoning(_) => {}
            ChatMessageItem::ToolCall(ToolCallItem {
                tool_name,
                arguments,
                ..
            }) => parts.push(format!("[tool_call {tool_name}] {}", arguments.text)),
            ChatMessageItem::ToolResult(tool_result) => {
                let mut line = format!("[tool_result {}]", tool_result.tool_name);
                if let Some(context) = &tool_result.result.context {
                    line.push('\n');
                    line.push_str(&context.text);
                }
                if let Some(file) = &tool_result.result.file {
                    line.push('\n');
                    line.push_str(&render_file_item(file));
                }
                parts.push(line);
            }
        }
    }
    if parts.is_empty() {
        String::new()
    } else {
        parts.join("\n\n")
    }
}

fn render_file_item(file: &FileItem) -> String {
    match &file.name {
        Some(name) => format!("[file] {name} ({})", file.uri),
        None => format!("[file] {}", file.uri),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use stellaclaw_core::session_actor::{ChatRole, ContextItem, ReasoningItem};

    #[test]
    fn extract_attachment_references_ignores_tags_in_fenced_code() {
        let text = "example:\n```text\n<attachment>shared/foo.pdf</attachment>\n```\ndone";
        let root = Path::new("/tmp/stellaclaw-no-such-workspace");

        let (clean, attachments) = extract_attachment_references(text, root, &root.join("shared"))
            .expect("code-only tag should not resolve");

        assert_eq!(clean, text);
        assert!(attachments.is_empty());
    }

    #[test]
    fn shared_attachment_path_accepts_unix_and_windows_separators() {
        // New .stellaclaw/stellaclaw_shared/ paths.
        assert!(is_shared_attachment_path(".stellaclaw/stellaclaw_shared"));
        assert!(is_shared_attachment_path(".stellaclaw/stellaclaw_shared/foo.pdf"));
        assert!(!is_shared_attachment_path(".stellaclaw/stellaclaw_shared/"));
        // Legacy shared/ paths for backward compatibility.
        assert!(is_shared_attachment_path("shared"));
        assert!(is_shared_attachment_path("shared/foo.pdf"));
        assert!(is_shared_attachment_path("shared\\foo.pdf"));
        assert!(!is_shared_attachment_path("shared/"));
        assert!(!is_shared_attachment_path("shared\\"));
        assert!(!is_shared_attachment_path("not-shared/foo.pdf"));
    }

    #[test]
    fn render_chat_message_hides_reasoning_items() {
        let message = ChatMessage::new(
            ChatRole::Assistant,
            vec![
                ChatMessageItem::Reasoning(ReasoningItem::codex(
                    None,
                    Some("opaque".to_string()),
                    None,
                )),
                ChatMessageItem::Context(ContextItem {
                    text: "visible answer".to_string(),
                }),
            ],
        );

        assert_eq!(render_chat_message(&message), "visible answer");
    }

    #[test]
    fn canonicalized_shared_path_can_escape_workspace_root() {
        let root =
            std::env::temp_dir().join(format!("stellaclaw-attachment-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let workspace = root.join("workspace");
        let shared = root.join("shared");
        std::fs::create_dir_all(&workspace).expect("workspace should exist");
        std::fs::create_dir_all(&shared).expect("shared should exist");
        let file = shared.join("report.txt");
        std::fs::write(&file, "hello").expect("file should write");
        #[cfg(unix)]
        std::os::unix::fs::symlink(&shared, workspace.join("shared"))
            .expect("shared symlink should exist");
        #[cfg(windows)]
        std::os::windows::fs::symlink_dir(&shared, workspace.join("shared"))
            .expect("shared symlink should exist");

        let resolved = resolve_outgoing_attachment(&workspace, &shared, "shared/report.txt")
            .expect("shared attachment should resolve");
        assert_eq!(resolved.path, file.canonicalize().unwrap());
        std::fs::remove_dir_all(&root).expect("temp root should be removed");
    }

    #[test]
    fn remaps_absolute_path_from_same_conversation_workspace() {
        let root = std::env::temp_dir().join(format!(
            "stellaclaw-attachment-remap-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&root);
        let workspace = root
            .join("current")
            .join("conversations")
            .join("telegram-main-000009");
        let shared = root.join("shared");
        std::fs::create_dir_all(&workspace).expect("workspace should exist");
        std::fs::create_dir_all(&shared).expect("shared should exist");
        let file = workspace.join("shibuya_sky_1.jpg");
        std::fs::write(&file, "jpg").expect("file should write");

        let old_absolute =
            "/Users/syk/Desktop/ClawParty/deploy_telegram_workdir/conversations/telegram-main-000009/shibuya_sky_1.jpg";
        let resolved = resolve_outgoing_attachment(&workspace, &shared, old_absolute)
            .expect("old absolute workspace path should remap");

        assert_eq!(resolved.path, file.canonicalize().unwrap());
        std::fs::remove_dir_all(&root).expect("temp root should be removed");
    }
}
