use crate::bootstrap::AgentWorkspace;
use crate::config::{BotCommandConfig, MainAgentConfig, ModelConfig};
use crate::session::SessionSnapshot;
use std::collections::BTreeMap;
use std::fs;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AgentPromptKind {
    MainForeground,
    MainBackground,
    SubAgent,
}

pub fn build_agent_system_prompt(
    workspace: &AgentWorkspace,
    session: &SessionSnapshot,
    workspace_summary: &str,
    kind: AgentPromptKind,
    model_name: &str,
    model: &ModelConfig,
    models: &BTreeMap<String, ModelConfig>,
    chat_model_keys: &[String],
    main_agent: &MainAgentConfig,
    commands: &[BotCommandConfig],
) -> String {
    let current_identity_prompt = fs::read_to_string(&workspace.identity_md_path)
        .ok()
        .map(|markdown| crate::bootstrap::render_identity_prompt_for_runtime(&markdown))
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| workspace.identity_prompt.clone());
    let current_user_profile_markdown = fs::read_to_string(&workspace.user_md_path)
        .ok()
        .unwrap_or_else(|| workspace.user_profile_markdown.clone());
    let current_agents_markdown = fs::read_to_string(&workspace.agents_md_path)
        .ok()
        .unwrap_or_else(|| workspace.agents_markdown.clone());

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
    let skill_line = match kind {
        AgentPromptKind::SubAgent => {
            "Skills may be available. If a skill seems relevant, inspect the preloaded skill metadata and load the relevant skill before relying on its detailed instructions."
        }
        AgentPromptKind::MainForeground | AgentPromptKind::MainBackground => {
            "Skills are available. If a skill seems relevant, inspect the preloaded skill metadata and load the relevant skill before relying on its detailed instructions."
        }
    };
    let mut parts = vec![
        header.to_string(),
        role_line.to_string(),
        "Your primary writable workspace is the current workspace root for this session.".to_string(),
        skill_line.to_string(),
        "The path ./.skill_memory is shared persistent memory for skills. Do not proactively read from or write to ./.skill_memory unless a loaded skill explicitly instructs you to use it.".to_string(),
        "If you want to say something to the user while you are still working and before the turn is ready to finish, you must use the user_tell tool instead of only writing that text in an assistant message with tool_calls. Mid-task progress updates, coordination, status pings, and transitional explanations must go through user_tell so the user actually receives them as chat bubbles.".to_string(),
        "If a user message starts with [Interrupted Follow-up], it means the user sent that message while you were still working on the previous turn. Treat it as an interruption signal. Give immediate visible feedback. If you can stop and answer directly, do that. If you will continue doing more work after acknowledging it, send the acknowledgement with user_tell before continuing.".to_string(),
        "If a user message starts with [Queued User Updates], it means multiple follow-up messages arrived while you were still working. Treat the newest items as the latest steering, and give immediate visible feedback. If you continue working after that acknowledgement, use user_tell for the acknowledgement instead of hiding it inside an assistant message with tool_calls.".to_string(),
        format!(
            "Some system-wide software packages are installed under {}. If you need to install global software packages, install them under that directory unless the user explicitly asks for a different location.",
            main_agent.global_install_root
        ),
        "If you need to send files or images back to the user, append one or more tags in your final reply using this format: <attachment>relative/path/from/workspace_root</attachment>. Each path must be relative to the current workspace root.".to_string(),
        "Do not describe a file path to the user without using the attachment tag if you expect the file to be delivered.".to_string(),
        "You are talking to the user inside a chat application. You may reply naturally, including structured Markdown when it helps.".to_string(),
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

    let model_catalog = chat_model_keys
        .iter()
        .filter_map(|name| models.get_key_value(name))
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
        parts.push("Available models:".to_string());
        parts.extend(model_catalog.lines().map(ToOwned::to_owned));
    }

    match kind {
        AgentPromptKind::MainForeground => {
            parts.push("You are the primary agent for this user-facing conversation.".to_string());
            parts.push("If the user asks about earlier chat content, a previous session, something you sent before, or historical work, use the available workspace history tools before saying you cannot remember.".to_string());
            parts.push("When a distinct chunk of work would be better handled by delegation, use subagents and choose the model whose description best matches the task instead of defaulting mechanically.".to_string());
        }
        AgentPromptKind::MainBackground => {
            parts.push("Plan the task decomposition carefully. Split work into as few large delegated chunks as practical, choose models deliberately, and avoid over-fragmenting the work.".to_string());
            parts.push("Match delegated work to the model that is most suited to it based on the model descriptions. Use subagents to exploit those strengths rather than routing everything through the current model.".to_string());
            parts.push("If you delegate a chunk to one or more subagents, wait until all required subagent results are available before you return your final answer.".to_string());
            parts.push("When a later subagent will continue from files written by an earlier subagent, prefer not to reread large generated content unless it is actually necessary. Instead, rely on the earlier subagent's concise summary of what it created and inspect the files only when needed.".to_string());
            parts.push("When you ask a subagent to write substantial content, require it to summarize what it created so downstream work can continue without rereading everything.".to_string());
            parts.push("If you need historical information from earlier workspaces, use the available workspace history tools instead of assuming the information is unavailable.".to_string());
        }
        AgentPromptKind::SubAgent => {
            parts.push(
                "Focus on the delegated task and return concise results for the caller."
                    .to_string(),
            );
            parts.push("When you generate substantial files or large content, end by clearly summarizing what you created, where it lives, and what a downstream agent should know before continuing. Keep that summary concise.".to_string());
        }
    }

    let identity = current_identity_prompt.trim();
    if !identity.is_empty() {
        parts.push("Identity:".to_string());
        parts.push(identity.to_string());
    }

    if let Some(user_meta) = extract_frontmatter(&current_user_profile_markdown) {
        parts.push("User meta:".to_string());
        parts.push(user_meta.trim().to_string());
    }

    let workspace_summary = workspace_summary.trim();
    if !workspace_summary.is_empty() {
        parts.push("Current workspace summary:".to_string());
        parts.push(workspace_summary.to_string());
    }

    if !current_agents_markdown.trim().is_empty() {
        parts.push("Runtime notes:".to_string());
        parts.push(current_agents_markdown.trim().to_string());
    }

    let _ = commands;

    parts.push(format!(
        "Runtime context: channel_id={}, session_id={}, agent_id={}, workspace_id={}, workspace_root={}",
        session.address.channel_id,
        session.id,
        session.agent_id,
        session.workspace_id,
        session.workspace_root.display(),
    ));

    parts.join("\n")
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

#[cfg(test)]
mod tests {
    use super::{AgentPromptKind, build_agent_system_prompt};
    use crate::backend::AgentBackendKind;
    use crate::bootstrap::AgentWorkspace;
    use crate::config::{
        ContextCompactionConfig, IdleCompactionConfig, MainAgentConfig, ModelConfig,
        TimeoutObservationCompactionConfig,
    };
    use crate::domain::ChannelAddress;
    use crate::session::SessionSnapshot;
    use std::collections::{BTreeMap, HashMap};
    use std::fs;
    use std::path::PathBuf;
    use tempfile::TempDir;
    use uuid::Uuid;

    #[test]
    fn prompt_includes_attachment_guidance_and_omits_channel_details() {
        let workspace = AgentWorkspace {
            root_dir: PathBuf::from("/tmp/work"),
            rundir: PathBuf::from("/tmp/work/rundir"),
            agent_dir: PathBuf::from("/tmp/work/agent"),
            skills_dir: PathBuf::from("/tmp/work/rundir/.skills"),
            skill_creator_dir: PathBuf::from("/tmp/work/rundir/.skills/skill-creator"),
            tmp_dir: PathBuf::from("/tmp/work/rundir/tmp"),
            identity_md_path: PathBuf::from("/tmp/work/agent/IDENTITY.md"),
            user_md_path: PathBuf::from("/tmp/work/agent/USER.md"),
            agents_md_path: PathBuf::from("/tmp/work/rundir/AGENTS.md"),
            identity_prompt: "You are Test Agent.".to_string(),
            user_profile_markdown: "---\nname: Test User\n---".to_string(),
            raw_identity_markdown: "# ignored\nYou are Test Agent.".to_string(),
            agents_markdown: String::new(),
        };
        let session = SessionSnapshot {
            id: Uuid::nil(),
            agent_id: Uuid::nil(),
            address: ChannelAddress {
                channel_id: "telegram-main".to_string(),
                conversation_id: "123".to_string(),
                user_id: None,
                display_name: None,
            },
            root_dir: PathBuf::from("/tmp/work/sessions/test"),
            attachments_dir: PathBuf::from("/tmp/work/workspaces/workspace-1/files/upload"),
            workspace_id: "workspace-1".to_string(),
            workspace_root: PathBuf::from("/tmp/work/workspaces/workspace-1/files"),
            message_count: 0,
            agent_message_count: 0,
            agent_messages: Vec::new(),
            last_agent_returned_at: None,
            last_compacted_at: None,
            turn_count: 0,
            last_compacted_turn_count: 0,
            cumulative_usage: agent_frame::TokenUsage::default(),
            cumulative_compaction: agent_frame::SessionCompactionStats::default(),
            api_timeout_override_seconds: None,
            skill_states: HashMap::new(),
            seen_user_profile_version: None,
            seen_identity_profile_version: None,
            idle_compaction_retry: None,
            pending_continue: None,
            pending_workspace_summary: false,
            close_after_summary: false,
        };
        let model = ModelConfig {
            model_type: crate::config::ModelType::Openrouter,
            api_endpoint: "https://example.com/v1".to_string(),
            model: "example-model".to_string(),
            backend: AgentBackendKind::AgentFrame,
            supports_vision_input: false,
            image_tool_model: None,
            web_search_model: None,
            api_key: None,
            api_key_env: "TEST_API_KEY".to_string(),
            chat_completions_path: "/chat/completions".to_string(),
            codex_home: None,
            auth_credentials_store_mode: agent_frame::config::AuthCredentialsStoreMode::Auto,
            headers: serde_json::Map::new(),
            context_window_tokens: 100_000,
            description: "General-purpose test model".to_string(),
            timeout_seconds: 30.0,
            cache_ttl: None,
            reasoning: None,
            native_web_search: None,
            external_web_search: None,
        };
        let mut models = BTreeMap::new();
        models.insert("main".to_string(), model.clone());
        let mut vision_model = model.clone();
        vision_model.description = "vision only".to_string();
        models.insert("vision-only".to_string(), vision_model);
        let main_agent = MainAgentConfig {
            model: Some("main".to_string()),
            global_install_root: "/opt".to_string(),
            language: "zh-CN".to_string(),
            timeout_seconds: Some(60.0),
            enabled_tools: vec!["read_file".to_string()],
            max_tool_roundtrips: 8,
            enable_context_compression: true,
            context_compaction: ContextCompactionConfig {
                trigger_ratio: 0.9,
                token_limit_override: None,
                recent_fidelity_target_ratio: 0.18,
            },
            idle_compaction: IdleCompactionConfig {
                enabled: false,
                poll_interval_seconds: 15,
                min_ratio: 0.5,
            },
            timeout_observation_compaction: TimeoutObservationCompactionConfig {
                enabled: true,
            },
        };

        let prompt = build_agent_system_prompt(
            &workspace,
            &session,
            "Current workspace summary.",
            AgentPromptKind::MainForeground,
            "main",
            &model,
            &models,
            &["main".to_string()],
            &main_agent,
            &[],
        );

        assert!(prompt.contains("append one or more tags in your final reply"));
        assert!(prompt.contains("Some system-wide software packages are installed under /opt."));
        assert!(prompt.contains("Current workspace summary."));
        assert!(prompt.contains("workspace_id=workspace-1"));
        assert!(prompt.contains("use the available workspace history tools"));
        assert!(prompt.contains("choose the model whose description best matches the task"));
        assert!(prompt.contains("- main: General-purpose test model"));
        assert!(!prompt.contains("vision-only"));
        assert!(prompt.contains("you must use the user_tell tool"));
        assert!(prompt.contains("If a user message starts with [Interrupted Follow-up]"));
        assert!(prompt.contains("If a user message starts with [Queued User Updates]"));
        assert!(!prompt.contains("Use subagent_create to start delegated work"));
        assert!(!prompt.contains("prefer leaving stdout/stderr unredirected"));
        assert!(!prompt.contains("Use only tools that are actually available to this agent"));
        assert!(!prompt.contains("available commands:"));
        assert!(!prompt.contains("delivery channel may translate rich text"));
    }

    #[test]
    fn prompt_rereads_user_and_identity_files_from_disk() {
        let temp_dir = TempDir::new().unwrap();
        let agent_dir = temp_dir.path().join("agent");
        let rundir = temp_dir.path().join("rundir");
        fs::create_dir_all(&agent_dir).unwrap();
        fs::create_dir_all(&rundir).unwrap();
        let user_md_path = agent_dir.join("USER.md");
        let identity_md_path = agent_dir.join("IDENTITY.md");
        let agents_md_path = rundir.join("AGENTS.md");
        fs::write(&user_md_path, "---\nname: Fresh User\n---\n").unwrap();
        fs::write(&identity_md_path, "You are Fresh Identity.\n").unwrap();
        fs::write(&agents_md_path, "runtime notes").unwrap();

        let workspace = AgentWorkspace {
            root_dir: temp_dir.path().to_path_buf(),
            rundir: rundir.clone(),
            agent_dir: agent_dir.clone(),
            skills_dir: rundir.join(".skills"),
            skill_creator_dir: rundir.join(".skills/skill-creator"),
            tmp_dir: rundir.join("tmp"),
            identity_md_path: identity_md_path.clone(),
            user_md_path: user_md_path.clone(),
            agents_md_path: agents_md_path.clone(),
            identity_prompt: "Stale Identity".to_string(),
            user_profile_markdown: "---\nname: Stale User\n---".to_string(),
            raw_identity_markdown: "Stale Identity".to_string(),
            agents_markdown: "stale runtime notes".to_string(),
        };
        let session = SessionSnapshot {
            id: Uuid::nil(),
            agent_id: Uuid::nil(),
            address: ChannelAddress {
                channel_id: "telegram-main".to_string(),
                conversation_id: "123".to_string(),
                user_id: None,
                display_name: None,
            },
            root_dir: temp_dir.path().join("sessions/test"),
            attachments_dir: temp_dir.path().join("workspace/upload"),
            workspace_id: "workspace-1".to_string(),
            workspace_root: temp_dir.path().join("workspace"),
            message_count: 0,
            agent_message_count: 0,
            agent_messages: Vec::new(),
            last_agent_returned_at: None,
            last_compacted_at: None,
            turn_count: 0,
            last_compacted_turn_count: 0,
            cumulative_usage: agent_frame::TokenUsage::default(),
            cumulative_compaction: agent_frame::SessionCompactionStats::default(),
            api_timeout_override_seconds: None,
            skill_states: HashMap::new(),
            seen_user_profile_version: None,
            seen_identity_profile_version: None,
            idle_compaction_retry: None,
            pending_continue: None,
            pending_workspace_summary: false,
            close_after_summary: false,
        };
        let model = ModelConfig {
            model_type: crate::config::ModelType::Openrouter,
            api_endpoint: "https://example.com/v1".to_string(),
            model: "example-model".to_string(),
            backend: AgentBackendKind::AgentFrame,
            supports_vision_input: false,
            image_tool_model: None,
            web_search_model: None,
            api_key: None,
            api_key_env: "TEST_API_KEY".to_string(),
            chat_completions_path: "/chat/completions".to_string(),
            codex_home: None,
            auth_credentials_store_mode: agent_frame::config::AuthCredentialsStoreMode::Auto,
            headers: serde_json::Map::new(),
            context_window_tokens: 100_000,
            description: "General-purpose test model".to_string(),
            timeout_seconds: 30.0,
            cache_ttl: None,
            reasoning: None,
            native_web_search: None,
            external_web_search: None,
        };
        let mut models = BTreeMap::new();
        models.insert("main".to_string(), model.clone());
        let main_agent = MainAgentConfig {
            model: Some("main".to_string()),
            global_install_root: "/opt".to_string(),
            language: "zh-CN".to_string(),
            timeout_seconds: Some(60.0),
            enabled_tools: vec!["read_file".to_string()],
            max_tool_roundtrips: 8,
            enable_context_compression: true,
            context_compaction: ContextCompactionConfig {
                trigger_ratio: 0.9,
                token_limit_override: None,
                recent_fidelity_target_ratio: 0.18,
            },
            idle_compaction: IdleCompactionConfig {
                enabled: false,
                poll_interval_seconds: 15,
                min_ratio: 0.5,
            },
            timeout_observation_compaction: TimeoutObservationCompactionConfig {
                enabled: true,
            },
        };

        let prompt = build_agent_system_prompt(
            &workspace,
            &session,
            "",
            AgentPromptKind::MainForeground,
            "main",
            &model,
            &models,
            &["main".to_string()],
            &main_agent,
            &[],
        );

        assert!(prompt.contains("Fresh Identity"));
        assert!(prompt.contains("name: Fresh User"));
        assert!(prompt.contains("runtime notes"));
        assert!(!prompt.contains("Stale Identity"));
        assert!(!prompt.contains("name: Stale User"));
    }
}
