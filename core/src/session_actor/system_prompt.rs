use std::collections::BTreeSet;

use super::{
    runtime_metadata::{
        RuntimeMetadataState, IDENTITY_PROMPT_COMPONENT, REMOTE_ALIASES_PROMPT_COMPONENT,
        SKILLS_METADATA_PROMPT_COMPONENT, USER_MEMORY_PROMPT_COMPONENT, USER_META_PROMPT_COMPONENT,
    },
    tool_catalog::enabled_prompt_protocols,
    SessionInitial, SessionType, ToolRemoteMode,
};

pub(crate) fn system_prompt_for_initial(
    initial: &SessionInitial,
    runtime_metadata_state: &RuntimeMetadataState,
    enabled_tools: &BTreeSet<String>,
) -> String {
    let session_kind = match initial.session_type {
        SessionType::Foreground => foreground_prompt(),
        SessionType::Background => background_prompt(),
        SessionType::Subagent => subagent_prompt(),
    };
    let mut sections = vec![common_prompt().to_string()];
    if let Some(protocols_prompt) = render_prompt_protocols(enabled_tools) {
        sections.push(protocols_prompt);
    }
    sections.push(session_kind.to_string());
    if let Some(remote_prompt) = remote_prompt(&initial.tool_remote_mode) {
        sections.push(remote_prompt);
    }
    if let Some(remote_workspace_instructions) = initial
        .remote_workspace_instructions
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        sections.push(format!(
            "[Remote Workspace Instructions]\n{}",
            remote_workspace_instructions
        ));
    }
    sections.extend(snapshot_sections(initial, runtime_metadata_state));
    sections.push(tool_efficiency_prompt(&initial.session_type).to_string());
    sections.join("\n\n")
}

fn common_prompt() -> &'static str {
    "You are StellaClaw, a pragmatic coding agent. Work in Rust-first codebases with minimal, \
     direct abstractions. Use tools when they materially advance the task. Keep answers concise \
     and grounded in the current workspace. Before making tool calls, send a brief preamble message \
     explaining what you are about to do. Group related tool calls under one preamble, keep it to \
     one or two short sentences, and focus on the immediate next action rather than narrating every \
     command. Do not rely on model-internal memory; when prior durable \
     user, conversation, or project facts may matter and are not visible in current context, use an \
     available long-memory search tool, inspect the repository, or run a narrow verification step. Before using \
     any library, framework, command, flag, file path, or project capability, verify that it exists \
     in this repository or local environment instead of assuming it exists. Treat AGENTS.md and similar repository instruction files as scoped rules, not background lore. \
     When you start working in a subdirectory, check whether that subtree has a more local \
     AGENTS.md or similar instruction file before editing there; when rules conflict, \
     follow the more local file. Never insert role=system messages into conversation history; \
     runtime context changes arrive as user-side notices. To send files or images to the user, \
     append one or more tags in final answer text using exactly this format: \
     <attachment>relative/path/from/workspace_root</attachment>. Attachment paths must be relative \
     to the current workspace root, must not be absolute paths or file:// URIs, and must refer to \
     files visible in the conversation workspace when the message is sent. The workspace may \
     contain .stellaclaw/shared/; that directory is shared across conversations in this \
     Stellaclaw workdir and is appropriate for reusable artifacts. If STELLACLAW_SOFTWARE_DIR is \
     set in the tool environment, that path is the configured shared software directory for \
     reusable binaries, checkouts, caches, or other tool installations."
}

fn render_prompt_protocols(enabled_tools: &BTreeSet<String>) -> Option<String> {
    let rendered = enabled_prompt_protocols(enabled_tools)
        .into_iter()
        .map(|protocol| protocol.body.trim())
        .filter(|body| !body.is_empty())
        .collect::<Vec<_>>()
        .join(" ");

    (!rendered.is_empty()).then_some(rendered)
}

fn foreground_prompt() -> &'static str {
    "Session kind: foreground. You are interacting with the user directly. Prefer clear progress, \
     concrete code changes, and a short final summary with verification."
}

fn background_prompt() -> &'static str {
    "Session kind: background. Work autonomously on the assigned task and put the primary result in \
     the final assistant response."
}

fn subagent_prompt() -> &'static str {
    "Session kind: subagent. You are delegated a bounded task. Stay inside the requested scope, \
     report concrete findings or changed files, and avoid taking ownership of unrelated work."
}

fn tool_efficiency_prompt(session_type: &SessionType) -> &'static str {
    match session_type {
        SessionType::Foreground | SessionType::Background => {
            "Check that all the required parameters for each tool call are provided or can \
             reasonably be inferred from context. IF there are no relevant tools or there are \
             missing values for required parameters, ask the user to supply these values; otherwise \
             proceed with the tool calls. If the user provides a specific value for a parameter \
             (for example provided in quotes), make sure to use that value EXACTLY. DO NOT make up \
             values for or ask about optional parameters.\n\n\
             If you intend to call multiple tools and there are no dependencies between the calls, \
             make all of the independent calls in the same tool-call batch (same assistant turn), otherwise \
             you MUST wait for previous calls to finish first to determine the dependent values \
             (do NOT use placeholders or guess missing parameters)."
        }
        SessionType::Subagent => {
            "Check that all the required parameters for each tool call are provided or can \
             reasonably be inferred from context. IF there are no relevant tools or there are \
             missing values for required parameters, ask the user to supply these values; otherwise \
             proceed with the tool calls. If the user provides a specific value for a parameter \
             (for example provided in quotes), make sure to use that value EXACTLY. DO NOT make up \
             values for or ask about optional parameters.\n\n\
             If you intend to call multiple tools and there are no dependencies between the calls, \
             make all of the independent calls in the same tool-call batch (same assistant turn), otherwise \
             you MUST wait for previous calls to finish first to determine the dependent values \
             (do NOT use placeholders or guess missing parameters)."
        }
    }
}

fn remote_prompt(remote_mode: &ToolRemoteMode) -> Option<String> {
    match remote_mode {
        ToolRemoteMode::Selectable => Some(
            "Tool remote mode: selectable. When a tool exposes remote, its value must be an SSH \
             Host alias from ~/.ssh/config; omit remote for local execution."
                .to_string(),
        ),
        ToolRemoteMode::FixedSsh { .. } => None,
    }
}

fn snapshot_sections(
    initial: &SessionInitial,
    runtime_metadata_state: &RuntimeMetadataState,
) -> Vec<String> {
    let mut sections = Vec::new();

    if let Some(identity) = runtime_metadata_state.snapshot_value(IDENTITY_PROMPT_COMPONENT) {
        sections.push(format!(
            "[Identity Snapshot]\nTreat this as the canonical durable identity context:\n{}",
            identity
        ));
    }

    if let Some(user_meta) = runtime_metadata_state.snapshot_value(USER_META_PROMPT_COMPONENT) {
        sections.push(format!(
            "[User Metadata Snapshot]\nThis is metadata for .stellaclaw/USER.md, not the full file content. Treat it as the canonical profile file status; read .stellaclaw/USER.md with file_read when exact profile details are needed:\n{}",
            user_meta
        ));
    }

    if initial.memory_enabled {
        if let Some(user_memory) =
            runtime_metadata_state.snapshot_value(USER_MEMORY_PROMPT_COMPONENT)
        {
            sections.push(format!(
                "[User Memory Snapshot]\nTreat this as canonical durable collaboration memory for the user. Each entry is an active long-memory item by id:\n{}",
                user_memory
            ));
        }
    }

    if let Some(skills_metadata) =
        runtime_metadata_state.snapshot_value(SKILLS_METADATA_PROMPT_COMPONENT)
    {
        sections.push(format!(
            "[Skills Metadata Snapshot]\nTreat this as the canonical durable skill registry metadata:\n{}",
            skills_metadata
        ));
    }

    if matches!(initial.tool_remote_mode, ToolRemoteMode::Selectable) {
        if let Some(remote_aliases) =
            runtime_metadata_state.snapshot_value(REMOTE_ALIASES_PROMPT_COMPONENT)
        {
            sections.push(format!(
                "[Remote Aliases Snapshot]\nTreat this as the canonical durable SSH alias list for selectable remote tool calls:\n{}",
                remote_aliases
            ));
        }
    }

    sections
}

#[cfg(test)]
mod tests {
    use std::{
        fs,
        path::PathBuf,
        time::{SystemTime, UNIX_EPOCH},
    };

    use super::*;
    use crate::session_actor::runtime_metadata::remote_aliases_prompt_for_mode;

    fn temp_root() -> PathBuf {
        let id = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!("stellaclaw_system_prompt_{id}"))
    }

    fn enabled_tools(names: &[&str]) -> BTreeSet<String> {
        names.iter().map(|name| (*name).to_string()).collect()
    }

    fn default_enabled_tools() -> BTreeSet<String> {
        enabled_tools(&[
            "file_read",
            "file_write",
            "grep",
            "apply_patch",
            "shell_exec",
            "shell_write_stdin",
            "shell_stop",
            "user_tell",
            "update_plan",
            "subagent_start",
            "memory_search",
            "memory_write",
            "memory_update",
            "memory_delete",
        ])
    }

    #[test]
    fn system_prompt_changes_by_session_type() {
        let state = RuntimeMetadataState::default();
        let enabled_tools = default_enabled_tools();
        let foreground = system_prompt_for_initial(
            &SessionInitial::new("s1", SessionType::Foreground),
            &state,
            &enabled_tools,
        );
        let background = system_prompt_for_initial(
            &SessionInitial::new("s2", SessionType::Background),
            &state,
            &enabled_tools,
        );
        let subagent = system_prompt_for_initial(
            &SessionInitial::new("s3", SessionType::Subagent),
            &state,
            &enabled_tools,
        );

        assert!(foreground.contains("Session kind: foreground"));
        assert!(background.contains("Session kind: background"));
        assert!(subagent.contains("Session kind: subagent"));
    }

    #[test]
    fn system_prompt_guides_tool_preambles() {
        let state = RuntimeMetadataState::default();
        let enabled_tools = default_enabled_tools();
        let prompt = system_prompt_for_initial(
            &SessionInitial::new("s1", SessionType::Foreground),
            &state,
            &enabled_tools,
        );

        assert!(prompt.contains("Before making tool calls, send a brief preamble message"));
        assert!(prompt.contains("Group related tool calls under one preamble"));
        assert!(prompt.contains("rather than narrating every command"));
    }

    #[test]
    fn system_prompt_guides_apply_patch_without_redundant_rereads() {
        let state = RuntimeMetadataState::default();
        let enabled_tools = default_enabled_tools();
        let prompt = system_prompt_for_initial(
            &SessionInitial::new("s1", SessionType::Foreground),
            &state,
            &enabled_tools,
        );

        assert!(prompt.contains("use apply_patch for targeted edits"));
        assert!(prompt.contains("do not re-read changed files"));
        assert!(prompt.contains("the tool reports failure when it does not apply"));
        assert!(!prompt.contains("old_text"));
        assert!(!prompt.contains("replace_all"));
        assert!(!prompt.contains("create_if_missing"));
    }

    #[test]
    fn system_prompt_omits_protocols_for_disabled_tools() {
        let state = RuntimeMetadataState::default();
        let enabled_tools = enabled_tools(&["file_read", "file_write", "grep"]);
        let prompt = system_prompt_for_initial(
            &SessionInitial::new("s1", SessionType::Foreground),
            &state,
            &enabled_tools,
        );

        assert!(prompt.contains("For repository exploration"));
        assert!(prompt.contains("<attachment>relative/path/from/workspace_root</attachment>"));
        assert!(prompt.contains("Attachment paths must be relative"));
        assert!(prompt.contains(".stellaclaw/shared/"));
        assert!(prompt.contains("STELLACLAW_SOFTWARE_DIR"));
        assert!(prompt.contains("Broad directory listings hide .stellaclaw/"));
        assert!(!prompt.contains("use apply_patch for targeted edits"));
        assert!(!prompt.contains("Before referencing a file with <attachment>"));
        assert!(!prompt.contains("When using shell for commands"));
        assert!(!prompt.contains("user_tell"));
        assert!(!prompt.contains("Use user_tell only"));
        assert!(!prompt.contains("Use update_plan"));
        assert!(!prompt.contains("prefer subagent_start"));
    }

    #[test]
    fn system_prompt_uses_snapshot_values_not_notified_values() {
        let mut initial = SessionInitial::new("s1", SessionType::Foreground);
        initial.tool_remote_mode = ToolRemoteMode::Selectable;
        initial.memory_enabled = true;

        let mut state = RuntimeMetadataState::default();
        let workdir = temp_root();
        let root = workdir.join("conversations").join("c1");
        fs::create_dir_all(root.join(".stellaclaw")).unwrap();
        fs::create_dir_all(root.join(".stellaclaw/skill/demo")).unwrap();
        fs::create_dir_all(workdir.join("rundir/memory_v1/user")).unwrap();
        fs::write(root.join(".stellaclaw/IDENTITY.md"), "identity: old").unwrap();
        fs::write(root.join(".stellaclaw/USER.md"), "tier: old").unwrap();
        fs::write(
            workdir.join("rundir/memory_v1/user/entries.jsonl"),
            r#"{"id":"u_1","scope":"user","subject":"style","text":"Prefer concise Chinese answers.","tags":["style"],"created_at":"2026-01-01T00:00:00Z","updated_at":"2026-01-01T00:00:00Z","state":"active"}"#,
        )
        .unwrap();
        fs::write(
            root.join(".stellaclaw/skill/demo/SKILL.md"),
            "# Demo\n\nskills: old",
        )
        .unwrap();
        state
            .initialize_from_workspace(
                &root,
                &root,
                "Available SSH remote aliases from ~/.ssh/config:\n- `old-host`".to_string(),
                initial.memory_enabled,
            )
            .unwrap();

        fs::write(root.join(".stellaclaw/IDENTITY.md"), "identity: new").unwrap();
        fs::write(root.join(".stellaclaw/USER.md"), "tier: new").unwrap();
        fs::write(
            workdir.join("rundir/memory_v1/user/entries.jsonl"),
            r#"{"id":"u_1","scope":"user","subject":"style","text":"Prefer detailed Chinese answers.","tags":["style"],"created_at":"2026-01-01T00:00:00Z","updated_at":"2026-01-02T00:00:00Z","state":"active"}"#,
        )
        .unwrap();
        fs::write(
            root.join(".stellaclaw/skill/demo/SKILL.md"),
            "# Demo\n\nskills: new",
        )
        .unwrap();
        let notices = state
            .observe_for_user_turn_from_workspace(
                &root,
                &root,
                "Available SSH remote aliases from ~/.ssh/config:\n- `new-host`".to_string(),
                initial.memory_enabled,
            )
            .unwrap();
        let rendered_notices = notices.join("\n");
        assert!(rendered_notices.contains("USER.md metadata changed"));
        assert!(!rendered_notices.contains("tier: new"));

        let enabled_tools = default_enabled_tools();
        let prompt_before_promote = system_prompt_for_initial(&initial, &state, &enabled_tools);
        assert!(prompt_before_promote.contains("identity: old"));
        assert!(prompt_before_promote.contains("Profile metadata file: .stellaclaw/USER.md"));
        assert!(prompt_before_promote.contains("Read .stellaclaw/USER.md with file_read"));
        assert!(!prompt_before_promote.contains("tier: old"));
        assert!(prompt_before_promote.contains("Prefer concise Chinese answers."));
        assert!(prompt_before_promote.contains("skills: old"));
        assert!(prompt_before_promote.contains("old-host"));
        assert!(!prompt_before_promote.contains("identity: new"));
        assert!(!prompt_before_promote.contains("tier: new"));
        assert!(!prompt_before_promote.contains("Prefer detailed Chinese answers."));
        assert!(!prompt_before_promote.contains("skills: new"));
        assert!(!prompt_before_promote.contains("new-host"));

        state.promote_notified_components_to_system_snapshot();
        let prompt_after_promote = system_prompt_for_initial(&initial, &state, &enabled_tools);
        assert!(prompt_after_promote.contains("identity: new"));
        assert!(prompt_after_promote.contains("Profile metadata file: .stellaclaw/USER.md"));
        assert!(!prompt_after_promote.contains("tier: new"));
        assert!(prompt_after_promote.contains("Prefer detailed Chinese answers."));
        assert!(prompt_after_promote.contains("skills: new"));
        assert!(prompt_after_promote.contains("new-host"));

        let _ = fs::remove_dir_all(workdir);
    }

    #[test]
    fn fixed_ssh_system_prompt_does_not_include_remote_alias_snapshot() {
        let mut initial = SessionInitial::new("s1", SessionType::Foreground);
        initial.tool_remote_mode = ToolRemoteMode::FixedSsh {
            host: "prod".to_string(),
            cwd: Some("/work".to_string()),
        };
        let mut state = RuntimeMetadataState::default();
        let root = temp_root();
        state
            .initialize_from_workspace(
                &root,
                &root,
                remote_aliases_prompt_for_mode(&ToolRemoteMode::Selectable),
                initial.memory_enabled,
            )
            .unwrap();

        let enabled_tools = default_enabled_tools();
        let prompt = system_prompt_for_initial(&initial, &state, &enabled_tools);
        assert!(!prompt.contains("Tool remote mode: fixed SSH"));
        assert!(!prompt.contains("[Remote Aliases Snapshot]"));
    }

    #[test]
    fn system_prompt_includes_remote_workspace_instructions() {
        let mut initial = SessionInitial::new("s1", SessionType::Foreground);
        initial.remote_workspace_instructions =
            Some("[Remote AGENTS.md]\nremote rules".to_string());
        let state = RuntimeMetadataState::default();

        let enabled_tools = default_enabled_tools();
        let prompt = system_prompt_for_initial(&initial, &state, &enabled_tools);

        assert!(prompt.contains("[Remote Workspace Instructions]"));
        assert!(prompt.contains("remote rules"));
    }
}
