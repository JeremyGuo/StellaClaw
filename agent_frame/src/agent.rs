use crate::compaction::{
    COMPACTION_MARKER, ContextCompactionReport, maybe_compact_messages_with_report,
};
use crate::config::AgentConfig;
use crate::llm::{TokenUsage, create_chat_completion};
use crate::message::ChatMessage;
use crate::skills::{SkillMetadata, build_skills_meta_prompt, discover_skills};
use crate::tooling::{Tool, build_tool_registry, build_tool_registry_with_cancel, execute_tool_call};
use anyhow::{Result, anyhow};
use serde_json::Value;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

const AGENT_FRAME_MARKER: &str = "[AgentFrame Runtime]";

fn compose_system_prompt(config: &AgentConfig, skills: &[SkillMetadata]) -> String {
    let skills_prompt = build_skills_meta_prompt(skills);
    let mut parts = vec![
        AGENT_FRAME_MARKER.to_string(),
        "You are running inside AgentFrame. Use tools when they materially help.".to_string(),
        "The model is responsible for choosing timeout_seconds for any built-in tool call."
            .to_string(),
    ];
    if config
        .upstream
        .native_web_search
        .as_ref()
        .is_some_and(|settings| settings.enabled)
    {
        parts.push(
            "Native provider web search is enabled for this session. Prefer that built-in capability instead of expecting a separate external web_search tool."
                .to_string(),
        );
    }
    if !config.system_prompt.is_empty() {
        parts.push(config.system_prompt.clone());
    }
    if !skills_prompt.is_empty() {
        parts.push(skills_prompt);
    }
    parts.join("\n\n")
}

fn ensure_system_message(messages: &[ChatMessage], system_prompt: &str) -> Vec<ChatMessage> {
    let mut cloned = messages.to_vec();
    if let Some(first) = cloned.first_mut()
        && first.role == "system"
    {
        let first_content = match &first.content {
            Some(Value::String(text)) => text.clone(),
            _ => String::new(),
        };
        if first_content == system_prompt {
            return cloned;
        }
        if first_content.starts_with(AGENT_FRAME_MARKER)
            || first_content.starts_with(COMPACTION_MARKER)
        {
            first.content = Some(Value::String(system_prompt.to_string()));
            return cloned;
        }
    }
    let mut with_system = vec![ChatMessage::text("system", system_prompt)];
    with_system.extend(cloned);
    with_system
}

pub fn extract_assistant_text(messages: &[ChatMessage]) -> String {
    messages
        .iter()
        .rev()
        .find(|message| message.role == "assistant")
        .map(|message| match &message.content {
            Some(Value::String(text)) => text.clone(),
            Some(Value::Array(items)) => items
                .iter()
                .filter_map(|item| {
                    let object = item.as_object()?;
                    let item_type = object.get("type")?.as_str()?;
                    match item_type {
                        "text" | "input_text" | "output_text" => {
                            object.get("text")?.as_str().map(ToOwned::to_owned)
                        }
                        _ => None,
                    }
                })
                .collect::<Vec<_>>()
                .join("\n"),
            Some(other) => other.to_string(),
            None => String::new(),
        })
        .unwrap_or_default()
}

pub fn run_session(
    previous_messages: Vec<ChatMessage>,
    prompt: impl Into<String>,
    config: AgentConfig,
    extra_tools: Vec<Tool>,
) -> Result<Vec<ChatMessage>> {
    Ok(run_session_with_report(previous_messages, prompt, config, extra_tools)?.messages)
}

#[derive(Clone, Debug, Default)]
pub struct SessionRunReport {
    pub messages: Vec<ChatMessage>,
    pub usage: TokenUsage,
}

#[derive(Clone)]
pub struct SessionExecutionControl {
    cancel_flag: Arc<AtomicBool>,
    checkpoint_callback: Option<Arc<dyn Fn(SessionRunReport) + Send + Sync>>,
}

impl SessionExecutionControl {
    pub fn new() -> Self {
        Self {
            cancel_flag: Arc::new(AtomicBool::new(false)),
            checkpoint_callback: None,
        }
    }

    pub fn with_checkpoint_callback(
        callback: impl Fn(SessionRunReport) + Send + Sync + 'static,
    ) -> Self {
        Self {
            cancel_flag: Arc::new(AtomicBool::new(false)),
            checkpoint_callback: Some(Arc::new(callback)),
        }
    }

    pub fn request_cancel(&self) {
        self.cancel_flag.store(true, Ordering::SeqCst);
    }

    pub fn is_cancelled(&self) -> bool {
        self.cancel_flag.load(Ordering::SeqCst)
    }

    pub fn cancel_flag(&self) -> Arc<AtomicBool> {
        Arc::clone(&self.cancel_flag)
    }

    fn ensure_not_cancelled(&self) -> Result<()> {
        if self.is_cancelled() {
            return Err(anyhow!("session execution cancelled"));
        }
        Ok(())
    }

    fn emit_checkpoint(&self, messages: &[ChatMessage], usage: &TokenUsage) {
        if let Some(callback) = &self.checkpoint_callback {
            callback(SessionRunReport {
                messages: messages.to_vec(),
                usage: usage.clone(),
            });
        }
    }
}

impl Default for SessionExecutionControl {
    fn default() -> Self {
        Self::new()
    }
}

pub fn run_session_with_report(
    previous_messages: Vec<ChatMessage>,
    prompt: impl Into<String>,
    config: AgentConfig,
    extra_tools: Vec<Tool>,
) -> Result<SessionRunReport> {
    run_session_with_report_controlled(previous_messages, prompt, config, extra_tools, None)
}

pub fn run_session_with_report_controlled(
    previous_messages: Vec<ChatMessage>,
    prompt: impl Into<String>,
    config: AgentConfig,
    extra_tools: Vec<Tool>,
    control: Option<SessionExecutionControl>,
) -> Result<SessionRunReport> {
    let prompt = prompt.into();
    let discovered_skills = discover_skills(&config.skills_dirs)?;
    let system_prompt = compose_system_prompt(&config, &discovered_skills);
    let mut messages = ensure_system_message(&previous_messages, &system_prompt);
    let mut usage = TokenUsage::default();

    if let Some(control) = &control {
        control.ensure_not_cancelled()?;
    }

    let registry = build_tool_registry_with_cancel(
        &config.enabled_tools,
        &config.workspace_root,
        &config.upstream,
        config.image_tool_upstream.as_ref(),
        &discovered_skills,
        &extra_tools,
        control.as_ref().map(SessionExecutionControl::cancel_flag),
    )?;
    let tool_definitions = registry.values().cloned().collect::<Vec<_>>();

    let initial_compaction =
        maybe_compact_messages_with_report(&config, &messages, &tool_definitions, &prompt)?;
    usage.add_assign(&initial_compaction.usage);
    messages = initial_compaction.messages;
    if !prompt.is_empty() {
        messages.push(ChatMessage::text("user", prompt));
    }

    for round_index in 0..config.max_tool_roundtrips {
        if let Some(control) = &control {
            control.ensure_not_cancelled()?;
        }
        if round_index > 0 {
            let compaction =
                maybe_compact_messages_with_report(&config, &messages, &tool_definitions, "")?;
            usage.add_assign(&compaction.usage);
            messages = compaction.messages;
        }
        if let Some(control) = &control {
            control.ensure_not_cancelled()?;
        }
        let outcome = create_chat_completion(&config.upstream, &messages, &tool_definitions, None)?;
        usage.add_assign(&outcome.usage);
        let tool_calls = outcome.message.tool_calls.clone().unwrap_or_default();
        messages.push(outcome.message);
        if let Some(control) = &control
            && !extract_assistant_text(&messages).trim().is_empty()
        {
            control.emit_checkpoint(&messages, &usage);
        }
        if tool_calls.is_empty() {
            return Ok(SessionRunReport { messages, usage });
        }

        for tool_call in tool_calls {
            if let Some(control) = &control {
                control.ensure_not_cancelled()?;
            }
            let result = execute_tool_call(
                &registry,
                &tool_call.function.name,
                tool_call.function.arguments.as_deref(),
            );
            messages.push(ChatMessage::tool_output(
                tool_call.id,
                tool_call.function.name,
                result,
            ));
        }
    }

    Err(anyhow!(
        "Agent stopped after exceeding max_tool_roundtrips={}",
        config.max_tool_roundtrips
    ))
}

pub fn compact_session_messages(
    previous_messages: Vec<ChatMessage>,
    config: AgentConfig,
    extra_tools: Vec<Tool>,
) -> Result<Vec<ChatMessage>> {
    Ok(compact_session_messages_with_report(previous_messages, config, extra_tools)?.messages)
}

pub fn compact_session_messages_with_report(
    previous_messages: Vec<ChatMessage>,
    config: AgentConfig,
    extra_tools: Vec<Tool>,
) -> Result<ContextCompactionReport> {
    let discovered_skills = discover_skills(&config.skills_dirs)?;
    let system_prompt = compose_system_prompt(&config, &discovered_skills);
    let messages = ensure_system_message(&previous_messages, &system_prompt);
    let registry = build_tool_registry(
        &config.enabled_tools,
        &config.workspace_root,
        &config.upstream,
        config.image_tool_upstream.as_ref(),
        &discovered_skills,
        &extra_tools,
    )?;
    let tool_definitions = registry.values().cloned().collect::<Vec<_>>();
    maybe_compact_messages_with_report(&config, &messages, &tool_definitions, "")
}
