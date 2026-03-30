use crate::bootstrap::AgentWorkspace;
use crate::config::{BotCommandConfig, MainAgentConfig, ModelConfig};
use crate::session::SessionSnapshot;
use std::collections::BTreeMap;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AgentPromptKind {
    MainForeground,
    MainBackground,
    SubAgent,
}

pub fn build_agent_system_prompt(
    workspace: &AgentWorkspace,
    session: &SessionSnapshot,
    kind: AgentPromptKind,
    model_name: &str,
    model: &ModelConfig,
    models: &BTreeMap<String, ModelConfig>,
    main_agent: &MainAgentConfig,
    commands: &[BotCommandConfig],
) -> String {
    let header = match kind {
        AgentPromptKind::MainForeground => "[AgentHost Main Foreground Agent]",
        AgentPromptKind::MainBackground => "[AgentHost Main Background Agent]",
        AgentPromptKind::SubAgent => "[AgentHost Sub-Agent]",
    };
    let role_line = match kind {
        AgentPromptKind::MainForeground => {
            "You are the Main Foreground Agent running inside AgentHost."
        }
        AgentPromptKind::MainBackground => {
            "You are a Main Background Agent running inside AgentHost."
        }
        AgentPromptKind::SubAgent => "You are a Sub-Agent running inside AgentHost.",
    };
    let mut parts = vec![
        header.to_string(),
        role_line.to_string(),
        "Use tools when they materially help. Prefer direct, efficient execution over long explanations.".to_string(),
        "All agents share the same rundir workspace.".to_string(),
        "Organize project-specific work under ./projects/<NAME>/.".to_string(),
        "Every project directory must maintain ./projects/<NAME>/README.md and ./projects/<NAME>/ABSTRACT.md.".to_string(),
        "README.md must remain the detailed project description. ABSTRACT.md must remain the short version, and if you do not know what a project is about you should check ABSTRACT.md before doing deeper reads.".to_string(),
        "Any material project change must be reflected in both README.md and ABSTRACT.md before you finish the turn.".to_string(),
        "Skills are available. When a skill may help, inspect the preloaded skill metadata and call load_skill before relying on the skill's detailed instructions.".to_string(),
        "If you need to send one file or image back to the user, append exactly one tag in your final reply using this format: <attachment>relative/path/from/rundir</attachment>. The path must be relative to the current workspace root, and you must return at most one attachment tag.".to_string(),
        "Do not describe a file path to the user without using the attachment tag if you expect the file to be delivered.".to_string(),
        "You are talking to the user inside a chat application. Your normal user-facing replies should be plain chat text, not Markdown-heavy formatting or other special layout syntax unless the user explicitly asks for it.".to_string(),
        format!(
            "Reply to the user in {} unless the user clearly asks for another language.",
            main_agent.language
        ),
        format!(
            "Current model profile: {} - {}",
            model_name,
            if model.description.trim().is_empty() {
                "No description provided."
            } else {
                model.description.trim()
            }
        ),
    ];

    let model_catalog = models
        .iter()
        .map(|(name, config)| {
            let description = if config.description.trim().is_empty() {
                "No description provided."
            } else {
                config.description.trim()
            };
            format!("- {}: {}", name, description)
        })
        .collect::<Vec<_>>()
        .join("\n");
    if !model_catalog.is_empty() {
        parts.push(format!("# Available Models\n\n{}", model_catalog));
    }

    match kind {
        AgentPromptKind::MainForeground => {
            parts.push("You may launch one or more subagents with the run_subagent tool when another model or an isolated delegated pass would materially help. Subagents share rundir, cannot create further subagents, and each subagent call must include a timeout_seconds value that fits the task.".to_string());
            parts.push("You may launch a main background agent with the start_background_agent tool when work should continue asynchronously after this turn.".to_string());
            parts.push("You may create and manage persisted cron jobs with the cron tools when work should run on a schedule.".to_string());
            parts.push("You can inspect tracked background agents and subagents with the agent status tools when you need model, state, or token-usage statistics.".to_string());
            parts.push("If the user is explicitly asking for a background task, or the work should continue after you reply, you should use start_background_agent instead of trying to finish everything in the foreground turn.".to_string());
            parts.push("When you start a background agent, omit sink unless you truly need custom routing. The default is to send results back to the current user conversation. Do not use session_id as a chat or conversation identifier.".to_string());
            parts.push("For cron jobs, use a checker when a cheap precondition can avoid an unnecessary LLM run. Checker semantics: exit code 0 means trigger the LLM, non-zero means skip this scheduled run, and checker execution errors or timeouts still trigger the LLM.".to_string());
        }
        AgentPromptKind::MainBackground => {
            parts.push("You may launch one or more subagents with the run_subagent tool when another model or an isolated delegated pass would materially help. Subagents share rundir, cannot create further subagents, and each subagent call must include a timeout_seconds value that fits the task.".to_string());
            parts
                .push("You cannot launch additional main background agents from here.".to_string());
            parts.push("You may create and manage persisted cron jobs with the cron tools when scheduled follow-up work is appropriate.".to_string());
            parts.push("You can inspect tracked background agents and subagents with the agent status tools when you need model, state, or token-usage statistics.".to_string());
            parts.push("Plan the task decomposition carefully. Split work into as few large delegated chunks as practical, choose models deliberately, and avoid over-fragmenting the work.".to_string());
            parts.push("If you delegate a chunk to one or more subagents, including parallel subagents, wait until all required subagent results are available before you return your final answer.".to_string());
            parts.push("When a later subagent will continue from files written by an earlier subagent, prefer not to reread large generated content unless it is actually necessary. Instead, rely on the earlier subagent's concise summary of what it created and inspect the files only when needed.".to_string());
            parts.push("When you ask a subagent to write substantial content, require it to summarize what it created so downstream work can continue without rereading everything.".to_string());
        }
        AgentPromptKind::SubAgent => {
            parts.push(
                "You are a subagent. You must not create subagents or main background agents."
                    .to_string(),
            );
            parts.push("When you generate substantial files or large content, end by clearly summarizing what you created, where it lives, and what a downstream agent should know before continuing. Keep that summary concise.".to_string());
        }
    }

    let identity = workspace.identity_prompt.trim();
    if !identity.is_empty() {
        parts.push(format!("# Identity\n\n{}", identity));
    }

    if let Some(user_meta) = extract_frontmatter(&workspace.user_profile_markdown) {
        parts.push(format!("# User Meta\n\n{}", user_meta.trim()));
    }

    if !workspace.agents_markdown.trim().is_empty() {
        parts.push(format!(
            "# Runtime Notes\n\n{}",
            workspace.agents_markdown.trim()
        ));
    }

    let commands_text = if commands.is_empty() {
        "No commands configured.".to_string()
    } else {
        commands
            .iter()
            .map(|command| format!("/{} - {}", command.command, command.description))
            .collect::<Vec<_>>()
            .join("\n")
    };

    parts.push(format!(
        "# Runtime Context\n\n- channel_id: {}\n- session_id: {}\n- agent_id: {}\n- workspace root: {}\n- startup directory: {}\n- projects root: {}\n- available commands:\n{}",
        session.address.channel_id,
        session.id,
        session.agent_id,
        workspace.rundir.display(),
        workspace.rundir.display(),
        workspace.projects_dir.display(),
        commands_text
    ));

    parts.join("\n\n")
}

pub fn greeting_for_language(language: &str) -> &'static str {
    let normalized = language.trim().to_ascii_lowercase();
    match normalized.as_str() {
        "zh" | "zh-cn" | "zh-hans" | "cn" => "你好",
        "zh-tw" | "zh-hant" => "你好",
        "en" | "en-us" | "en-gb" => "Hello",
        "ja" | "ja-jp" => "こんにちは",
        "ko" | "ko-kr" => "안녕하세요",
        "fr" | "fr-fr" => "Bonjour",
        "de" | "de-de" => "Hallo",
        "es" | "es-es" => "Hola",
        _ => "Hello",
    }
}

fn extract_frontmatter(markdown: &str) -> Option<String> {
    let mut lines = markdown.lines();
    if lines.next()? != "---" {
        return None;
    }
    let mut meta = Vec::new();
    for line in lines {
        if line == "---" {
            break;
        }
        meta.push(line);
    }
    if meta.is_empty() {
        None
    } else {
        Some(meta.join("\n"))
    }
}
