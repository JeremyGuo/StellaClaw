#[cfg(test)]
use std::collections::BTreeSet;

use super::{
    runtime_metadata::{
        RuntimeMetadataState, IDENTITY_PROMPT_COMPONENT, REMOTE_ALIASES_PROMPT_COMPONENT,
        REMOTE_WORKSPACE_PROMPT_COMPONENT, SKILLS_METADATA_PROMPT_COMPONENT,
        USER_MEMORY_PROMPT_COMPONENT, USER_META_PROMPT_COMPONENT,
    },
    SessionInitial, SessionType, ToolRemoteMode,
};

#[cfg(test)]
pub(crate) fn system_prompt_for_initial(
    initial: &SessionInitial,
    runtime_metadata_state: &RuntimeMetadataState,
    _enabled_tools: &BTreeSet<String>,
) -> String {
    system_prompt_for_initial_with_common_prompt(initial, runtime_metadata_state, None)
}

pub(crate) fn system_prompt_for_initial_with_common_prompt(
    initial: &SessionInitial,
    runtime_metadata_state: &RuntimeMetadataState,
    common_prompt_override: Option<&str>,
) -> String {
    let session_kind = match initial.session_type {
        SessionType::Foreground => foreground_prompt(),
        SessionType::Background => background_prompt(),
        SessionType::Subagent => subagent_prompt(),
    };
    let common = match common_prompt_override
        .map(str::trim)
        .filter(|prompt| !prompt.is_empty())
    {
        Some(prompt) => prompt,
        None => common_prompt(),
    };
    let mut sections = vec![common.to_string()];
    sections.push(session_kind.to_string());
    if let Some(remote_prompt) = remote_prompt(&initial.tool_remote_mode) {
        sections.push(remote_prompt);
    }
    sections.extend(snapshot_sections(initial, runtime_metadata_state));
    sections.join("\n\n")
}

fn common_prompt() -> &'static str {
    "You are StellaClaw, a pragmatic coding agent. Work in Rust-first codebases with minimal, \
     direct abstractions. Use tools when they materially advance the task. Keep answers concise \
     and grounded in the current workspace. At the start of tool-using work, send a user-visible \
     preamble that briefly acknowledges the task and gives a one-to-two sentence plan before the first tool calls. During longer work, \
     send short progress updates every 1-3 execution steps, and at minimum within every 6 steps or \
     10 tool calls. Each update should state useful outcome or impact so far plus the next 1-3 \
     steps or open question when relevant. Treat preambles and progress messages as the normal way \
     to reply while work is underway; do not use a separate host tool just to tell the user something. Avoid headings, status labels, repetitive tics, and \
     command-by-command log narration. Do not rely on model-internal memory; when prior durable \
     user, conversation, or project facts may matter and are not visible in current context, use an \
     available long-memory search tool, inspect the repository, or run a narrow verification step. \
     When memory tools are available, use memory_write before finishing a turn when you learned a durable fact future turns, agents, compaction recovery, or future conversations would otherwise need to rediscover. Write immediately if the user asks you to remember/save something; if you learn a stable user fact, preference, correction, working style, or recurring expectation; if this conversation establishes goals, constraints, decisions, handoff state, running task/process ids, artifact paths, blockers, or next recovery steps; if you learn stable project, environment, deployment, or data facts, critical file/module responsibilities, architecture constraints, project-specific implementation requirements, or recurring pitfalls. Save scope=user for durable facts about the user, preferences, response style, recurring corrections, work habits, and global expectations; scope=conversation for current goals, constraints, decisions, active handoff, running task/process ids, artifact paths, blockers, next steps, and task-specific assumptions; scope=public for stable cross-conversation project/customer/data/deployment facts, critical file/module notes, architecture constraints, recurring implementation rules, and reusable operational knowledge. Do not save transient command output, one-off progress, guesses, ordinary chat, secrets, credentials, raw tool logs, compacted summaries, or facts already present in visible memory or returned by memory_search. Use memory_search when prior conversation decisions, public project conventions, deployment details, long-running task state, previous blockers, project-specific requirements, or critical module knowledge may matter and are not visible. Use memory_update or memory_delete only after memory_search returns the exact stale, duplicate, incomplete, or wrong entry id. \
     Check that all the required parameters for each tool call are provided or can reasonably be inferred from context. If there are no relevant tools or there are missing values for required parameters, ask the user to supply these values; otherwise proceed with the tool calls. If the user provides a specific value for a parameter, for example provided in quotes, make sure to use that value exactly. Do not make up values for or ask about optional parameters. If you intend to call multiple tools and there are no dependencies between the calls, make all of the independent calls in the same tool-call batch, same assistant turn; otherwise wait for previous calls to finish first to determine the dependent values. Do not use placeholders or guess missing parameters. Before repository exploration or other read-heavy work, think first about all files, searches, listings, and command outputs you are likely to need, then issue one bounded independent batch. This applies to read, list, search, git inspection, and similar shell commands. Do not read files one-by-one unless the next path truly depends on an earlier result. \
     Before using any library, framework, command, flag, file path, or project capability, verify that it exists \
     in this repository or local environment instead of assuming it exists. For repository \
     exploration, use narrow and bounded shell rg/ripgrep commands such as rg -n '<pattern>' \
     <dir>, rg --files -g '<pattern>', or rg --files <dir>; shell_exec ensures managed rg when \
     needed. Use shell commands such as sed, nl, or cat for file contents and line ranges. Keep \
     shell search scoped to the relevant directory. When using shell for commands that dedicated tools do not cover, start a new command with shell_exec.command. Use shell_write_stdin with process_id only to poll or continue an existing process, and shell_stop with process_id to stop one. Positive example: {\"command\":\"cargo check -p stellaclaw_core\"}. Negative example: using process_id to start work, or using cmd instead of command. \
     Broad directory listings hide .stellaclaw/. \
     File tools may access documented .stellaclaw/ paths when needed, but do not create paths \
     outside documented .stellaclaw workflows. Use apply_patch for targeted edits. For generated \
     files, complete rewrites, or append-only output, use shell commands through shell_exec. \
     apply_patch paths must be workspace-relative, and related multi-file edits should be combined \
     into one patch when practical. After a successful apply_patch, do not re-read changed files \
     just to verify that the patch applied; the tool reports failure when it does not apply. \
     Re-read only when you need new context, a follow-up command or formatter may have rewritten \
     the file, or a verification failure needs inspection. Use update_plan for multi-step, \
     long-running, or ambiguous work so the user can see the current checklist. If the first \
     planned step can start immediately, return update_plan in the same tool-call batch as the \
     next independent tool calls instead of spending a separate model round only updating the plan; \
     the host applies the plan immediately and continues with the remaining tools. Positive \
     examples: a refactor across several files, a bug investigation with multiple plausible \
     causes, or a task that needs several verification steps. Negative examples: a one-line fix, \
     a single file read, or a straightforward reply that can be finished immediately without a \
     visible plan. Treat AGENTS.md and similar repository instruction files as scoped rules, not background lore. \
     When you start working in a subdirectory, check whether that subtree has a more local \
     AGENTS.md or similar instruction file before editing there; when rules conflict, \
     follow the more local file. When a user message contains a selected content block, treat it \
     as the user's precise current focus; if editing a file, prefer the block's locator and \
     surrounding context over guessing where the text came from. Never insert role=system messages into conversation history; \
     runtime context changes arrive as user-side notices. To send files or images to the user, \
     append one or more tags in final answer text using exactly this format: \
     <attachment>relative/path/from/workspace_root</attachment>. Attachment paths must be relative \
     to the current workspace root, must not be absolute paths or file:// URIs, and must refer to \
     files visible in the conversation workspace when the message is sent. The workspace may \
     contain .stellaclaw/shared/; that directory is shared across conversations in this \
     Stellaclaw workdir and is appropriate for reusable artifacts. If STELLACLAW_SOFTWARE_DIR is \
     set in the tool environment, that path is the configured shared software directory for \
     reusable binaries, checkouts, caches, or other tool installations. Use documented \
     .stellaclaw/ paths only through their intended tool workflows. Do not create files or \
     directories under .stellaclaw/ outside documented tool workflows."
}

fn foreground_prompt() -> &'static str {
    "Session kind: foreground. You are interacting with the user directly. Prefer clear progress, \
     concrete code changes, and a short final summary with verification. In final assistant \
     messages, place <attachment>relative/path/from/workspace_root</attachment> exactly where a \
     produced file, image, HTML page, or other artifact should be embedded in the rendered reply."
}

fn background_prompt() -> &'static str {
    "Session kind: background. Work autonomously on the assigned task and put the primary result in \
     the final assistant response."
}

fn subagent_prompt() -> &'static str {
    "Session kind: subagent. You are delegated a bounded task. Stay inside the requested scope, \
     report concrete findings or changed files, and avoid taking ownership of unrelated work."
}

fn remote_prompt(remote_mode: &ToolRemoteMode) -> Option<String> {
    match remote_mode {
        ToolRemoteMode::Selectable => Some(
            "When a tool exposes remote, its value must be an SSH Host alias from ~/.ssh/config; \
             omit remote for local execution."
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
            "[User Metadata Snapshot]\nThis is metadata for .stellaclaw/USER.md, not the full file content. Treat it as the canonical profile file status; inspect .stellaclaw/USER.md with shell_exec when exact profile details are needed:\n{}",
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

    if let Some(remote_workspace) =
        runtime_metadata_state.snapshot_value(REMOTE_WORKSPACE_PROMPT_COMPONENT)
    {
        sections.push(format!(
            "[Remote Workspace Snapshot]\nTreat this as the canonical fixed SSH workspace instruction snapshot:\n{}",
            remote_workspace
        ));
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
            "apply_patch",
            "shell_exec",
            "shell_write_stdin",
            "shell_stop",
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
        assert!(foreground.contains("exactly where a"));
        assert!(foreground.contains("<attachment>relative/path/from/workspace_root</attachment>"));
        assert!(background.contains("Session kind: background"));
        assert!(subagent.contains("Session kind: subagent"));
        assert!(!background.contains("exactly where a"));
        assert!(!subagent.contains("exactly where a"));
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

        assert!(prompt.contains("user-visible"));
        assert!(prompt.contains("briefly acknowledges"));
        assert!(prompt.contains("one-to-two sentence plan before the first tool calls"));
        assert!(prompt.contains("normal way to reply while work is underway"));
        assert!(prompt.contains("within every 6 steps or 10 tool calls"));
        assert!(prompt.contains("Avoid headings, status labels"));
    }

    #[test]
    fn system_prompt_guides_parallel_reading_batches() {
        let state = RuntimeMetadataState::default();
        let enabled_tools = default_enabled_tools();
        let prompt = system_prompt_for_initial(
            &SessionInitial::new("s1", SessionType::Foreground),
            &state,
            &enabled_tools,
        );

        assert!(prompt.contains("Before repository exploration"));
        assert!(prompt.contains("one bounded independent batch"));
        assert!(prompt.contains("read, list, search, git inspection"));
        assert!(prompt.contains("Do not read files one-by-one"));
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

        assert!(prompt.contains("Use apply_patch for targeted edits"));
        assert!(prompt.contains("do not re-read changed files"));
        assert!(prompt.contains("the tool reports failure when it does not apply"));
        assert!(!prompt.contains("old_text"));
        assert!(!prompt.contains("replace_all"));
        assert!(!prompt.contains("create_if_missing"));
    }

    #[test]
    fn system_prompt_keeps_common_tool_guidance_without_protocol_tools() {
        let state = RuntimeMetadataState::default();
        let enabled_tools = enabled_tools(&["shell_exec"]);
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
        assert!(prompt.contains(
            "Do not create files or directories under .stellaclaw/ outside documented tool workflows"
        ));
        assert!(prompt.contains("Use apply_patch for targeted edits"));
        assert!(prompt.contains("Use update_plan"));
        assert!(prompt.contains("When memory tools are available"));
        assert!(prompt.contains("Use memory_update or memory_delete only after memory_search"));
        assert!(prompt.contains("When using shell for commands"));
        assert!(prompt.contains("shell_write_stdin with process_id only"));
        assert!(!prompt.contains("only after shell_make_visible"));
        assert!(!prompt.contains("Before referencing a file with <attachment>"));
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
                String::new(),
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
                String::new(),
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
        assert!(prompt_before_promote.contains("inspect .stellaclaw/USER.md with shell_exec"));
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
                String::new(),
                initial.memory_enabled,
            )
            .unwrap();

        let enabled_tools = default_enabled_tools();
        let prompt = system_prompt_for_initial(&initial, &state, &enabled_tools);
        assert!(!prompt.contains("Tool remote mode: fixed SSH"));
        assert!(!prompt.contains("[Remote Aliases Snapshot]"));
    }

    #[test]
    fn system_prompt_includes_remote_workspace_snapshot() {
        let initial = SessionInitial::new("s1", SessionType::Foreground);
        let mut state = RuntimeMetadataState::default();
        state.initialize_missing_component(
            REMOTE_WORKSPACE_PROMPT_COMPONENT,
            "[Remote AGENTS.md]\nremote rules".to_string(),
        );

        let enabled_tools = default_enabled_tools();
        let prompt = system_prompt_for_initial(&initial, &state, &enabled_tools);

        assert!(prompt.contains("[Remote Workspace Snapshot]"));
        assert!(prompt.contains("remote rules"));
    }
}
