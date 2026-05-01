use super::{
    runtime_metadata::{
        RuntimeMetadataState, IDENTITY_PROMPT_COMPONENT, REMOTE_ALIASES_PROMPT_COMPONENT,
        SKILLS_METADATA_PROMPT_COMPONENT, STELLACLAW_MEMORY_PROMPT_COMPONENT,
        USER_META_PROMPT_COMPONENT,
    },
    SessionInitial, SessionType, ToolRemoteMode,
};

pub(crate) fn system_prompt_for_initial(
    initial: &SessionInitial,
    runtime_metadata_state: &RuntimeMetadataState,
) -> String {
    let session_kind = match initial.session_type {
        SessionType::Foreground => foreground_prompt(),
        SessionType::Background => background_prompt(),
        SessionType::Subagent => subagent_prompt(),
    };
    let mut sections = vec![
        common_prompt().to_string(),
        session_kind.to_string(),
        remote_prompt(&initial.tool_remote_mode),
    ];
    sections.extend(snapshot_sections(initial, runtime_metadata_state));
    sections.push(tool_efficiency_prompt(&initial.session_type).to_string());
    sections.join("\n\n")
}

fn common_prompt() -> &'static str {
    "You are StellaClaw, a pragmatic coding agent. Work in Rust-first codebases with minimal, \
     direct abstractions. Use tools when they materially advance the task. Keep answers concise \
     and grounded in the current workspace. If you are unsure, do not answer from memory: inspect \
     the repository, current session context, or run a narrow verification step first. Before using \
     any library, framework, command, flag, file path, or project capability, verify that it exists \
     in this repository or local environment instead of assuming it exists. For repository \
     exploration, use the dedicated tools instead of shell read/search commands: use glob to find \
     files by path pattern, grep to find files by content pattern, ls for narrowed directory \
     listings, and file_read for file contents. These dedicated recursive tools skip slow remote \
     mounts such as sshfs/NFS by default. Do not use shell for direct grep, find, cat, head, \
     tail, or ls. When using shell for commands that dedicated tools do not cover, start a new \
     command with the command field and omit session_id; use session_id only to poll or continue an \
     existing shell session. Positive example: {\"command\":\"cargo check -p stellaclaw_core\"}. \
     Negative example: {\"session_id\":\"sh_123\"} to start work, or using cmd instead of command. \
     Treat AGENTS.md and similar repository instruction files as scoped rules, not background lore. \
     When you start working in a subdirectory, check whether that subtree has a more local \
     AGENTS.md or similar instruction file before editing there; when rules conflict, \
     follow the more local file. Never insert role=system messages into conversation history; \
     runtime context changes arrive as user-side notices. STELLACLAW.md in the workspace root is \
     the durable project memory file: keep it concise and factual, update it only when long-lived \
     project facts, stable conventions, confirmed architecture notes, or handoff-critical decisions \
     change. Do not use STELLACLAW.md for transient per-turn chatter, guesses, or unconfirmed \
     notes. Use user_tell only for mid-task progress or coordination that must become visible \
     before the current turn is ready to finish. If you can return the final answer now, do not \
     send an extra user_tell first. Positive example: a long-running edit, benchmark, or debug \
     session is still in progress and the user needs an immediate visible status update. Negative \
     example: you already have the result and are about to finish, so a separate 'working on it' \
     or 'done' user_tell is unnecessary. Use update_plan for multi-step, long-running, or \
     ambiguous work so the user can see the current checklist. Positive examples: a refactor \
     across several files, a bug investigation with multiple plausible causes, or a task that \
     needs several verification steps. Negative examples: a one-line fix, a single file read, or \
     a straightforward reply that can be finished immediately without a visible plan. If you need \
     to send files or images back to the user, append one or more tags in this exact format: \
     <attachment>relative/path/from/workspace_root</attachment>. Each path must be relative to the \
     current workspace root. This attachment syntax is supported in both the final assistant reply \
     and user_tell text. The workspace may contain .stellaclaw/stellaclaw_shared/; that directory is shared across \
     conversations in this Stellaclaw workdir and is appropriate for reusable artifacts. If \
     STELLACLAW_SOFTWARE_DIR is set in the tool environment, that path is the configured shared \
     software directory for reusable binaries, checkouts, caches, or other tool installations."
}

fn foreground_prompt() -> &'static str {
    "Session kind: foreground. You are interacting with the user directly. Prefer clear progress, \
     concrete code changes, and a short final summary with verification."
}

fn background_prompt() -> &'static str {
    "Session kind: background. Work autonomously on the assigned task. Use user_tell only for \
     useful progress or coordination; put the primary result in the final assistant response."
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
             (do NOT use placeholders or guess missing parameters).\n\n\
             For multi-step tasks that require more than 3 sequential tool operations and can be \
             clearly scoped (e.g. explore a codebase module, run benchmarks, set up a dependency), \
             prefer subagent_start to keep the main conversation context lean. Do NOT batch tool \
             calls that could cause irreversible damage if an earlier step produces unexpected \
             results (destructive shell commands, production deploys, database mutations); use \
             subagent_start for those instead so that intermediate results can be inspected."
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

fn remote_prompt(remote_mode: &ToolRemoteMode) -> String {
    match remote_mode {
        ToolRemoteMode::Selectable => {
            "Tool remote mode: selectable. When a tool exposes remote, its value must be an SSH \
             Host alias from ~/.ssh/config; omit remote for local execution."
                .to_string()
        }
        ToolRemoteMode::FixedSsh { host, cwd } => match cwd.as_deref().map(str::trim).filter(|cwd| !cwd.is_empty()) {
            Some(cwd) => format!(
                "Tool remote mode: fixed SSH. Tools execute on SSH Host alias `{host}` with cwd `{cwd}` when remote execution applies."
            ),
            None => format!(
                "Tool remote mode: fixed SSH. Tools execute on SSH Host alias `{host}` when remote execution applies."
            ),
        },
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
            "[User Metadata Snapshot]\nTreat this as the canonical durable user metadata:\n{}",
            user_meta
        ));
    }

    if let Some(stellaclaw_memory) =
        runtime_metadata_state.snapshot_value(STELLACLAW_MEMORY_PROMPT_COMPONENT)
    {
        sections.push(format!(
            "[STELLACLAW Memory Snapshot]\nTreat this as the canonical durable project memory from STELLACLAW.md:\n{}",
            stellaclaw_memory
        ));
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

    #[test]
    fn system_prompt_changes_by_session_type() {
        let state = RuntimeMetadataState::default();
        let foreground =
            system_prompt_for_initial(&SessionInitial::new("s1", SessionType::Foreground), &state);
        let background =
            system_prompt_for_initial(&SessionInitial::new("s2", SessionType::Background), &state);
        let subagent =
            system_prompt_for_initial(&SessionInitial::new("s3", SessionType::Subagent), &state);

        assert!(foreground.contains("Session kind: foreground"));
        assert!(background.contains("Session kind: background"));
        assert!(subagent.contains("Session kind: subagent"));
    }

    #[test]
    fn system_prompt_uses_snapshot_values_not_notified_values() {
        let mut initial = SessionInitial::new("s1", SessionType::Foreground);
        initial.tool_remote_mode = ToolRemoteMode::Selectable;

        let mut state = RuntimeMetadataState::default();
        let root = temp_root();
        fs::create_dir_all(root.join(".stellaclaw")).unwrap();
        fs::create_dir_all(root.join(".stellaclaw/skill/demo")).unwrap();
        fs::write(root.join(".stellaclaw/IDENTITY.md"), "identity: old").unwrap();
        fs::write(root.join(".stellaclaw/USER.md"), "tier: old").unwrap();
        fs::write(root.join("STELLACLAW.md"), "memory: old").unwrap();
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
            )
            .unwrap();

        fs::write(root.join(".stellaclaw/IDENTITY.md"), "identity: new").unwrap();
        fs::write(root.join(".stellaclaw/USER.md"), "tier: new").unwrap();
        fs::write(root.join("STELLACLAW.md"), "memory: new").unwrap();
        fs::write(
            root.join(".stellaclaw/skill/demo/SKILL.md"),
            "# Demo\n\nskills: new",
        )
        .unwrap();
        state
            .observe_for_user_turn_from_workspace(
                &root,
                &root,
                "Available SSH remote aliases from ~/.ssh/config:\n- `new-host`".to_string(),
            )
            .unwrap();

        let prompt_before_promote = system_prompt_for_initial(&initial, &state);
        assert!(prompt_before_promote.contains("identity: old"));
        assert!(prompt_before_promote.contains("tier: old"));
        assert!(prompt_before_promote.contains("memory: old"));
        assert!(prompt_before_promote.contains("skills: old"));
        assert!(prompt_before_promote.contains("old-host"));
        assert!(!prompt_before_promote.contains("identity: new"));
        assert!(!prompt_before_promote.contains("tier: new"));
        assert!(!prompt_before_promote.contains("memory: new"));
        assert!(!prompt_before_promote.contains("skills: new"));
        assert!(!prompt_before_promote.contains("new-host"));

        state.promote_notified_components_to_system_snapshot();
        let prompt_after_promote = system_prompt_for_initial(&initial, &state);
        assert!(prompt_after_promote.contains("identity: new"));
        assert!(prompt_after_promote.contains("tier: new"));
        assert!(prompt_after_promote.contains("memory: new"));
        assert!(prompt_after_promote.contains("skills: new"));
        assert!(prompt_after_promote.contains("new-host"));

        let _ = fs::remove_dir_all(root);
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
            )
            .unwrap();

        let prompt = system_prompt_for_initial(&initial, &state);
        assert!(prompt.contains("Tool remote mode: fixed SSH"));
        assert!(!prompt.contains("[Remote Aliases Snapshot]"));
    }
}
