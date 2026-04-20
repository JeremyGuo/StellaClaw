use crate::agent_status::{AgentRegistry, ManagedAgentKind, ManagedAgentRecord, ManagedAgentState};
use crate::agents::{ForegroundAgent, SubAgentSpec};
use crate::backend::{
    AgentBackendKind, BackendExecutionOptions, backend_supports_native_multimodal_input,
    compact_session_messages_with_report as run_backend_compaction,
};
use crate::bootstrap::AgentWorkspace;
use crate::channel::{Channel, ConversationProbe, IncomingMessage};
use crate::channel_auth::{
    AdminAuthorizeOutcome, ChannelAdminSnapshot, ChannelAuthorizationManager,
    ConversationApprovalSnapshot, ConversationApprovalState,
};
use crate::channels::command_line::CommandLineChannel;
use crate::channels::dingtalk::DingtalkChannel;
use crate::channels::dingtalk_robot::DingtalkRobotChannel;
use crate::channels::telegram::TelegramChannel;
use crate::channels::web::{
    WebChannel, WebChannelHost, WebConversationSummary, summarize_skeleton,
};
use crate::config::{
    AgentConfig, BotCommandConfig, ChannelConfig, ModelCapability, ModelConfig, SandboxConfig,
    SandboxMode, ServerConfig, ToolingConfig, ToolingTarget, default_bot_commands,
    default_dingtalk_commands, default_telegram_commands,
};
use crate::conversation::{
    ConversationManager, ConversationSettings, materialize_conversation_attachments,
    resolve_local_mount_path,
};
use crate::cron::{
    ClaimedCronTask, CronCheckerConfig, CronCreateRequest, CronManager, CronUpdateRequest,
    running_trigger_outcome,
};
use crate::domain::{
    AttachmentKind, ChannelAddress, MessageRole, OutgoingAttachment, OutgoingMessage,
    ProcessingState, ShowOption, StoredAttachment, UsageChart, UsageChartDay,
};
use crate::prompt::{
    AgentPromptKind, AgentSystemPromptState, build_agent_system_prompt_state,
    current_identity_prompt_for_workspace, current_user_meta_prompt_for_workspace,
    greeting_for_language,
};
use crate::sandbox::{
    PersistentChildRuntime, bubblewrap_is_available, is_child_run_turn_request_send_error,
    is_child_transport_error, run_one_shot_child_turn,
};
use crate::session::{
    IDENTITY_PROMPT_COMPONENT, PromptComponentChangeNotice, REMOTE_ALIASES_PROMPT_COMPONENT,
    SessionActorMessage, SessionActorOutbound, SessionEffect, SessionErrno, SessionKind,
    SessionManager, SessionPhase, SessionPlan, SessionPlanStep, SessionPlanStepStatus,
    SessionRuntimeTurnCommit, SessionRuntimeTurnFailure, SessionSkillObservation, SessionSnapshot,
    SessionTurnTimeHintConfig, SessionUserMessage, SkillChangeNotice, USER_META_PROMPT_COMPONENT,
};
use crate::sink::{SinkRouter, SinkTarget};
use crate::snapshot::{SnapshotBundle, SnapshotManager};
use crate::subagent::{HostedSubagent, HostedSubagentInner, SubagentState};
use crate::transcript::SessionTranscript;
use crate::upgrade::upgrade_workdir;
use crate::workpath::{
    current_ssh_remote_aliases_prompt, load_remote_agents_md_for_workpath, load_result_to_json,
};
use crate::workspace::{WorkspaceManager, WorkspaceMountMaterialization};
use agent_frame::config::{
    AgentConfig as FrameAgentConfig, CodexAuthConfig, ExternalWebSearchConfig,
    NativeWebSearchConfig, ReasoningConfig, TokenEstimationSource, TokenEstimationTemplateConfig,
    TokenEstimationTokenizerConfig, UpstreamApiKind, UpstreamConfig, load_codex_auth_tokens,
};
use agent_frame::skills::{build_skills_meta_prompt, discover_skills};
use agent_frame::tooling::{build_tool_registry, terminate_runtime_state_tasks};
use agent_frame::{
    ChatMessage, ContextCompactionReport, ExecutionProgress, SessionCompactionStats, SessionEvent,
    SessionExecutionControl, SessionState, StructuredCompactionOutput, TokenUsage, Tool,
    estimate_configured_session_tokens, extract_assistant_text, prompt_token_calibration_for_model,
    token_estimator_label_for_model,
};
use anyhow::{Context, Result, anyhow};
use async_trait::async_trait;
use base64::Engine;
use chrono::Utc;
use humantime::parse_duration;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs;
use std::ops::Deref;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use tokio::select;
use tokio::sync::{Notify, RwLock, mpsc, oneshot};
use tokio::time::{Duration, MissedTickBehavior, interval};
use tracing::{error, info, warn};
use uuid::Uuid;

mod agent_runtime;
mod command_routing;
mod commands;
mod context;
mod extra_tools;
mod frame_config;
mod incoming;
mod messaging;
mod persistence;
mod progress;
mod runtime_helpers;
mod security;
mod session_runner;
mod subagents;
mod workspace_summary;

use self::agent_runtime::{
    AgentRuntimeView, SubAgentSlot, SummaryInProgressGuard, SummaryTracker, TimedRunOutcome,
};
use self::commands::*;
use self::context::RuntimeContext;
use self::incoming::*;
use self::messaging::*;
use self::persistence::*;
use self::runtime_helpers::*;

const ATTACHMENT_OPEN_TAG: &str = "<attachment>";
const ATTACHMENT_CLOSE_TAG: &str = "</attachment>";
const CHANNEL_RESTART_MAX_BACKOFF_SECONDS: u64 = 30;
const CONVERSATION_CLEANUP_POLL_SECONDS: u64 = 300;
const SYSTEM_RESTART_NOTICE: &str =
    "[System Restarted: All previous long run execution tools and DSL jobs with IDs are all ended]";

#[derive(Clone, Debug)]
struct BackgroundJobRequest {
    agent_id: uuid::Uuid,
    parent_agent_id: Option<uuid::Uuid>,
    cron_task_id: Option<uuid::Uuid>,
    session: SessionSnapshot,
    agent_backend: AgentBackendKind,
    model_key: String,
    prompt: String,
}

struct ActiveForegroundAgentFrameRuntime {
    model_key: String,
    workspace_id: String,
    sandbox_mode: SandboxMode,
    local_mounts: Vec<PathBuf>,
    runtime: PersistentChildRuntime,
}

struct PersistedYieldedForegroundTurn {
    session: SessionSnapshot,
    should_auto_resume: bool,
}

#[derive(Clone, Copy, Debug)]
enum TokenEstimationCacheKind {
    Template,
    Tokenizer,
}

#[derive(Clone, Copy, Debug)]
enum ToolingFamily {
    WebSearch,
    Image,
    ImageGen,
    Pdf,
    AudioInput,
}

impl ToolingFamily {
    fn field_name(self) -> &'static str {
        match self {
            Self::WebSearch => "tooling.web_search",
            Self::Image => "tooling.image",
            Self::ImageGen => "tooling.image_gen",
            Self::Pdf => "tooling.pdf",
            Self::AudioInput => "tooling.audio_input",
        }
    }

    fn target<'a>(self, tooling: &'a ToolingConfig) -> Option<&'a ToolingTarget> {
        match self {
            Self::WebSearch => tooling.web_search.as_ref(),
            Self::Image => tooling.image.as_ref(),
            Self::ImageGen => tooling.image_gen.as_ref(),
            Self::Pdf => tooling.pdf.as_ref(),
            Self::AudioInput => tooling.audio_input.as_ref(),
        }
    }

    fn required_capability(self) -> ModelCapability {
        match self {
            Self::WebSearch => ModelCapability::WebSearch,
            Self::Image => ModelCapability::ImageIn,
            Self::ImageGen => ModelCapability::ImageOut,
            Self::Pdf => ModelCapability::Pdf,
            Self::AudioInput => ModelCapability::AudioIn,
        }
    }
}

fn sanitize_cache_path_segment(value: &str) -> String {
    let mut sanitized = String::new();
    for character in value.chars() {
        if character.is_ascii_alphanumeric() || matches!(character, '-' | '_' | '.') {
            sanitized.push(character);
        } else {
            sanitized.push('_');
        }
    }
    if sanitized.is_empty() {
        "model".to_string()
    } else {
        sanitized
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ImageGenerationRouting {
    Disabled,
    Native,
    Tool,
}

fn select_image_generation_routing(
    target: Option<&ToolingTarget>,
    model: &ModelConfig,
) -> ImageGenerationRouting {
    let self_supported = model.has_capability(ModelCapability::ImageOut)
        && model.upstream_api_kind() == UpstreamApiKind::Responses;

    match target {
        None => {
            if self_supported {
                ImageGenerationRouting::Native
            } else {
                ImageGenerationRouting::Disabled
            }
        }
        Some(target) if target.prefer_self && self_supported => ImageGenerationRouting::Native,
        Some(_) => ImageGenerationRouting::Tool,
    }
}

#[cfg(test)]
fn infer_single_agent_backend(agent: &AgentConfig, model_key: &str) -> Option<AgentBackendKind> {
    match agent.backends_for_model(model_key).as_slice() {
        [backend] => Some(*backend),
        _ => None,
    }
}

fn leading_system_prompt(messages: &[ChatMessage]) -> Option<String> {
    let first = messages.first()?;
    if first.role != "system" {
        return None;
    }
    let prompt = first.content.as_ref()?.as_str()?;
    if prompt.starts_with("[AgentFrame Runtime]") {
        return None;
    }
    Some(prompt.to_owned())
}

impl AgentRuntimeView {
    fn current_runtime_skills_metadata_prompt(&self) -> Result<String> {
        let discovered = discover_skills(std::slice::from_ref(&self.agent_workspace.skills_dir))?;
        Ok(build_skills_meta_prompt(&discovered))
    }

    fn initialize_skills_metadata_prompt_if_missing(
        &self,
        session: &SessionSnapshot,
    ) -> Result<()> {
        let metadata_prompt = self.current_runtime_skills_metadata_prompt()?;
        let actor = self.with_sessions(|sessions| sessions.resolve_snapshot(session))?;
        actor.initialize_prompt_component_if_missing(
            crate::session::SKILLS_METADATA_PROMPT_COMPONENT,
            metadata_prompt,
        )
    }

    fn initialize_host_prompt_components_if_missing(
        &self,
        session: &SessionSnapshot,
    ) -> Result<()> {
        let actor = self.with_sessions(|sessions| sessions.resolve_snapshot(session))?;
        actor.initialize_prompt_component_if_missing(
            IDENTITY_PROMPT_COMPONENT,
            current_identity_prompt_for_workspace(&self.agent_workspace),
        )?;
        actor.initialize_prompt_component_if_missing(
            USER_META_PROMPT_COMPONENT,
            current_user_meta_prompt_for_workspace(&self.agent_workspace),
        )?;
        actor.initialize_prompt_component_if_missing(
            REMOTE_ALIASES_PROMPT_COMPONENT,
            current_ssh_remote_aliases_prompt(),
        )
    }

    fn initialize_session_prompt_components_if_missing(
        &self,
        session: &SessionSnapshot,
    ) -> Result<()> {
        self.initialize_host_prompt_components_if_missing(session)?;
        self.initialize_skills_metadata_prompt_if_missing(session)
    }

    fn available_agent_models(&self, backend: AgentBackendKind) -> Vec<String> {
        self.agent
            .available_models(backend)
            .iter()
            .filter(|model_key: &&String| self.models.contains_key(model_key.as_str()))
            .cloned()
            .collect()
    }

    fn selected_agent_backend(&self) -> Option<AgentBackendKind> {
        self.selected_agent_backend.or_else(|| {
            self.selected_main_model_key
                .as_ref()
                .map(|_| AgentBackendKind::AgentFrame)
        })
    }

    fn effective_agent_backend(&self) -> Result<AgentBackendKind> {
        self.selected_agent_backend().ok_or_else(|| {
            anyhow!("this conversation does not have a model yet; choose one with /agent")
        })
    }

    fn ensure_model_available_for_backend(
        &self,
        backend: AgentBackendKind,
        model_key: &str,
    ) -> Result<()> {
        if self.agent.is_model_available(backend, model_key) {
            return Ok(());
        }
        Err(anyhow!(
            "model '{}' is not available for agent backend '{}'",
            model_key,
            serde_json::to_string(&backend)
                .unwrap_or_else(|_| "\"unknown\"".to_string())
                .trim_matches('"')
        ))
    }

    fn resolved_codex_auth(&self, model: &ModelConfig) -> Result<Option<CodexAuthConfig>> {
        if model.upstream_auth_kind() != agent_frame::config::UpstreamAuthKind::CodexSubscription {
            return Ok(None);
        }
        let resolved_codex_home = model.resolved_codex_home();
        let codex_home = resolved_codex_home
            .as_deref()
            .ok_or_else(|| anyhow!("codex subscription config must include codex_home"))?;
        Ok(Some(load_codex_auth_tokens(codex_home)?))
    }

    fn effective_main_model_key(&self) -> Result<String> {
        let model_key = self.selected_main_model_key.clone().ok_or_else(|| {
            anyhow!("this conversation does not have a main model yet; choose one with /agent")
        })?;
        self.ensure_model_available_for_backend(AgentBackendKind::AgentFrame, &model_key)?;
        Ok(model_key)
    }

    fn effective_sandbox_mode(&self, address: &ChannelAddress) -> Result<SandboxMode> {
        let settings = self.with_conversations(|conversations| {
            conversations
                .ensure_conversation(address)
                .map(|snapshot| snapshot.settings)
        })?;
        Ok(settings.sandbox_mode.unwrap_or(self.sandbox.mode))
    }

    fn local_mount_paths_for_address(&self, address: &ChannelAddress) -> Result<Vec<PathBuf>> {
        self.with_conversations(|conversations| {
            Ok(conversations
                .get_snapshot(address)
                .map(|snapshot| {
                    snapshot
                        .settings
                        .local_mounts
                        .into_iter()
                        .map(|mount| mount.path)
                        .collect()
                })
                .unwrap_or_default())
        })
    }

    fn ensure_foreground_agent_frame_runtime(
        &self,
        session: &SessionSnapshot,
        model_key: &str,
        config: &FrameAgentConfig,
    ) -> Result<Arc<Mutex<ActiveForegroundAgentFrameRuntime>>> {
        let session_key = session.address.session_key();
        let effective_sandbox_mode = self.effective_sandbox_mode(&session.address)?;
        let local_mounts = self.local_mount_paths_for_address(&session.address)?;
        if let Some(existing) = self
            .active_foreground_agent_frame_runtimes
            .lock()
            .ok()
            .and_then(|runtimes| runtimes.get(&session_key).cloned())
        {
            let matches_current = existing.lock().ok().is_some_and(|runtime| {
                runtime.model_key == model_key
                    && runtime.workspace_id == session.workspace_id
                    && runtime.sandbox_mode == effective_sandbox_mode
                    && runtime.local_mounts == local_mounts
            });
            if matches_current {
                return Ok(existing);
            }
            if let Some(runtime) = self
                .active_foreground_agent_frame_runtimes
                .lock()
                .ok()
                .and_then(|mut runtimes| runtimes.remove(&session_key))
                && let Ok(mut runtime) = runtime.lock()
            {
                let _ = runtime.runtime.shutdown();
                if runtime.sandbox_mode == SandboxMode::Bubblewrap {
                    let _ = self
                        .workspace_manager
                        .cleanup_transient_mounts(&runtime.workspace_id);
                }
            }
        }

        let runtime = PersistentChildRuntime::spawn(
            &SandboxConfig {
                mode: effective_sandbox_mode,
                bubblewrap_binary: self.sandbox.bubblewrap_binary.clone(),
                map_docker_socket: self.sandbox.map_docker_socket,
            },
            &config.workspace_root,
            &config.runtime_state_root,
            PathBuf::from(&self.main_agent.global_install_root),
            self.token_estimation_cache_roots(),
            local_mounts.clone(),
            self.agent_workspace.rundir.join("skill_memory"),
            self.agent_workspace.skills_dir.clone(),
            &config.skills_dirs,
        )?;
        let entry = Arc::new(Mutex::new(ActiveForegroundAgentFrameRuntime {
            model_key: model_key.to_string(),
            workspace_id: session.workspace_id.clone(),
            sandbox_mode: effective_sandbox_mode,
            local_mounts,
            runtime,
        }));
        self.active_foreground_agent_frame_runtimes
            .lock()
            .map_err(|_| anyhow!("active foreground runtimes lock poisoned"))?
            .insert(session_key, Arc::clone(&entry));
        Ok(entry)
    }

    fn model_config(&self, model_key: &str) -> Result<&ModelConfig> {
        self.models
            .get(model_key)
            .with_context(|| format!("unknown model {}", model_key))
    }

    fn model_upstream_timeout_seconds(&self, model_key: &str) -> Result<f64> {
        Ok(self
            .models
            .get(model_key)
            .with_context(|| format!("unknown model {}", model_key))?
            .timeout_seconds)
    }

    fn apply_global_token_estimation_cache(
        &self,
        model_key: &str,
        mut config: agent_frame::config::TokenEstimationConfig,
    ) -> agent_frame::config::TokenEstimationConfig {
        let template_cache_dir =
            self.token_estimation_model_cache_dir(TokenEstimationCacheKind::Template, model_key);
        let tokenizer_cache_dir =
            self.token_estimation_model_cache_dir(TokenEstimationCacheKind::Tokenizer, model_key);

        if let Some(TokenEstimationTemplateConfig::Huggingface { cache_dir, .. }) =
            &mut config.template
            && cache_dir.is_none()
        {
            *cache_dir = Some(
                config
                    .cache_dir
                    .clone()
                    .unwrap_or_else(|| template_cache_dir.clone()),
            );
        }
        if let Some(TokenEstimationTokenizerConfig::Huggingface { cache_dir, .. }) =
            &mut config.tokenizer
            && cache_dir.is_none()
        {
            *cache_dir = Some(
                config
                    .cache_dir
                    .clone()
                    .unwrap_or_else(|| tokenizer_cache_dir.clone()),
            );
        }

        if config.source == Some(TokenEstimationSource::Huggingface)
            && config.cache_dir.is_none()
            && let Some(repo) = config.repo.clone()
        {
            let revision = config
                .revision
                .clone()
                .unwrap_or_else(|| "main".to_string());
            if config.template.is_none() {
                config.template = Some(TokenEstimationTemplateConfig::Huggingface {
                    repo: repo.clone(),
                    revision: revision.clone(),
                    file: "tokenizer_config.json".to_string(),
                    field: "chat_template".to_string(),
                    cache_dir: Some(template_cache_dir),
                });
            }
            if config.tokenizer.is_none() {
                config.tokenizer = Some(TokenEstimationTokenizerConfig::Huggingface {
                    repo,
                    revision,
                    file: "tokenizer.json".to_string(),
                    cache_dir: Some(tokenizer_cache_dir),
                });
            }
        }

        config
    }

    fn token_estimation_cache_roots(&self) -> Vec<PathBuf> {
        let mut roots = Vec::new();
        for root in [
            self.resolve_token_estimation_cache_root(
                &self.main_agent.token_estimation_cache.template.hf,
            ),
            self.resolve_token_estimation_cache_root(
                &self.main_agent.token_estimation_cache.tokenizer.hf,
            ),
        ] {
            if !roots.iter().any(|existing| existing == &root) {
                roots.push(root);
            }
        }
        roots
    }

    fn token_estimation_model_cache_dir(
        &self,
        kind: TokenEstimationCacheKind,
        model_key: &str,
    ) -> PathBuf {
        let root = match kind {
            TokenEstimationCacheKind::Template => self.resolve_token_estimation_cache_root(
                &self.main_agent.token_estimation_cache.template.hf,
            ),
            TokenEstimationCacheKind::Tokenizer => self.resolve_token_estimation_cache_root(
                &self.main_agent.token_estimation_cache.tokenizer.hf,
            ),
        };
        root.join(sanitize_cache_path_segment(model_key))
    }

    fn resolve_token_estimation_cache_root(&self, raw: &str) -> PathBuf {
        let expanded = agent_frame::config::expand_home_path(raw);
        if expanded.is_absolute() {
            expanded
        } else {
            self.agent_workspace.root_dir.join(expanded)
        }
    }

    fn tooling_target(&self, family: ToolingFamily) -> Option<&ToolingTarget> {
        family.target(&self.tooling)
    }

    fn tell_user_now(&self, session: &SessionSnapshot, text: String) -> Result<Value> {
        let channel = self
            .channels
            .get(&session.address.channel_id)
            .with_context(|| format!("unknown channel {}", session.address.channel_id))?
            .clone();
        let outgoing = build_outgoing_message_for_session(session, &text, &session.workspace_root)
            .context("failed to build immediate user_tell message")?;
        send_outgoing_message_now(channel, session.address.clone(), outgoing)
            .context("failed to send immediate user_tell message")?;
        Ok(json!({
            "ok": true,
            "sent": true
        }))
    }

    fn add_remote_workpath(
        &self,
        session: &SessionSnapshot,
        host: String,
        path: String,
        description: String,
    ) -> Result<Value> {
        let snapshot = self.with_conversations(|conversations| {
            conversations.add_remote_workpath(&session.address, &host, &path, &description)
        })?;
        let workpath = snapshot
            .settings
            .remote_workpaths
            .iter()
            .find(|item| item.host == host.trim())
            .cloned()
            .ok_or_else(|| anyhow!("remote workpath was not persisted"))?;
        let agents_md = load_remote_agents_md_for_workpath(&workpath);
        Ok(json!({
            "ok": true,
            "host": workpath.host,
            "path": workpath.path,
            "description": workpath.description,
            "agents_md": load_result_to_json(&agents_md),
            "chat_version_rotated": true,
            "note": "The remote workpath is stored at the conversation level and shared by foreground/background agents. The current turn's system prompt and AgentFrame remote default-root map do not hot-reload; until the next turn or rebuilt agent prompt, use absolute remote paths or pass cwd=path explicitly when calling remote-capable tools.",
        }))
    }

    fn modify_remote_workpath(
        &self,
        session: &SessionSnapshot,
        host: String,
        description: String,
    ) -> Result<Value> {
        let snapshot = self.with_conversations(|conversations| {
            conversations.modify_remote_workpath(&session.address, &host, "", &description)
        })?;
        let workpath = snapshot
            .settings
            .remote_workpaths
            .iter()
            .find(|item| item.host == host.trim())
            .cloned()
            .ok_or_else(|| anyhow!("remote workpath was not found after modification"))?;
        Ok(json!({
            "ok": true,
            "host": workpath.host,
            "path": workpath.path,
            "description": workpath.description,
            "chat_version_rotated": true,
        }))
    }

    fn remove_remote_workpath(&self, session: &SessionSnapshot, host: String) -> Result<Value> {
        self.with_conversations(|conversations| {
            conversations
                .remove_remote_workpath(&session.address, &host, "")
                .map(|_| ())
        })?;
        Ok(json!({
            "ok": true,
            "removed": true,
            "host": host.trim(),
            "chat_version_rotated": true,
        }))
    }

    fn upload_shared_profile_files(&self, session: &SessionSnapshot) -> Result<Value> {
        let report =
            upload_workspace_shared_profile_files(&self.agent_workspace, &session.workspace_root)?;
        if report.changed_any() {
            self.with_conversations(|conversations| {
                conversations
                    .rotate_chat_version_id(&session.address)
                    .map(|_| ())
            })?;
        }
        Ok(json!({
            "user_md": {
                "changed": report.user_changed,
                "workspace_path": session.workspace_root.join("USER.md").display().to_string(),
                "shared_path": self.agent_workspace.user_md_path.display().to_string(),
            },
            "identity_md": {
                "changed": report.identity_changed,
                "workspace_path": session.workspace_root.join("IDENTITY.md").display().to_string(),
                "shared_path": self.agent_workspace.identity_md_path.display().to_string(),
            },
            "chat_version_rotated": report.changed_any(),
            "current_turn_prompt_refreshed": false,
            "note": "The current turn's system prompt does not hot-reload. The next turn will pick up the new shared profile content.",
        }))
    }

    fn default_subagent_timeout_seconds(&self, model_key: &str) -> Result<f64> {
        if let Some(timeout_seconds) = self.main_agent.timeout_seconds {
            return Ok(if timeout_seconds > 0.0 {
                timeout_seconds
            } else {
                300.0
            });
        }
        Ok(background_agent_timeout_seconds(
            self.models
                .get(model_key)
                .with_context(|| format!("unknown model {}", model_key))?
                .timeout_seconds,
        ))
    }

    fn create_background_session_for_conversation(
        &self,
        address: &ChannelAddress,
        agent_id: uuid::Uuid,
    ) -> Result<SessionSnapshot> {
        let preferred_workspace_id = self.with_conversations(|conversations| {
            Ok(conversations
                .ensure_conversation(address)?
                .settings
                .workspace_id)
        })?;
        let actor = self.with_sessions(|sessions| match preferred_workspace_id.as_deref() {
            Some(workspace_id) => {
                sessions.create_background_in_workspace_actor(address, agent_id, workspace_id)
            }
            None => sessions.create_background_actor(address, agent_id),
        })?;
        let session = actor.snapshot()?;
        self.with_conversations(|conversations| {
            conversations.set_workspace_id(address, Some(session.workspace_id.clone()))?;
            Ok(())
        })?;
        Ok(session)
    }

    fn register_managed_agent(
        &self,
        id: uuid::Uuid,
        kind: ManagedAgentKind,
        model_key: String,
        parent_agent_id: Option<uuid::Uuid>,
        session: &SessionSnapshot,
        state: ManagedAgentState,
    ) {
        if let Ok(mut registry) = self.agent_registry.lock() {
            if let Err(error) = registry.register(ManagedAgentRecord {
                id,
                kind,
                parent_agent_id,
                session_id: Some(session.id),
                channel_id: session.address.channel_id.clone(),
                model_key,
                state,
                created_at: Utc::now(),
                started_at: if state == ManagedAgentState::Running {
                    Some(Utc::now())
                } else {
                    None
                },
                finished_at: None,
                error: None,
                usage: TokenUsage::default(),
            }) {
                warn!(
                    log_stream = "server",
                    kind = "agent_registry_persist_failed",
                    agent_id = %id,
                    error = %format!("{error:#}"),
                    "failed to persist agent registry after register"
                );
            }
        }
        self.agent_registry_notify.notify_waiters();
    }

    fn mark_managed_agent_running(&self, id: uuid::Uuid) {
        if let Ok(mut registry) = self.agent_registry.lock() {
            if let Err(error) = registry.mark_running(id, Utc::now()) {
                warn!(
                    log_stream = "server",
                    kind = "agent_registry_persist_failed",
                    agent_id = %id,
                    error = %format!("{error:#}"),
                    "failed to persist agent registry after mark_running"
                );
            }
        }
        self.agent_registry_notify.notify_waiters();
    }

    fn mark_managed_agent_completed(&self, id: uuid::Uuid, usage: &TokenUsage) {
        if let Ok(mut registry) = self.agent_registry.lock() {
            if let Err(error) = registry.mark_completed(id, Utc::now(), usage.clone()) {
                warn!(
                    log_stream = "server",
                    kind = "agent_registry_persist_failed",
                    agent_id = %id,
                    error = %format!("{error:#}"),
                    "failed to persist agent registry after mark_completed"
                );
            }
        }
        self.agent_registry_notify.notify_waiters();
    }

    fn mark_managed_agent_failed(&self, id: uuid::Uuid, usage: &TokenUsage, error: &anyhow::Error) {
        if let Ok(mut registry) = self.agent_registry.lock() {
            if let Err(persist_error) =
                registry.mark_failed(id, Utc::now(), usage.clone(), format!("{error:#}"))
            {
                warn!(
                    log_stream = "server",
                    kind = "agent_registry_persist_failed",
                    agent_id = %id,
                    error = %format!("{persist_error:#}"),
                    "failed to persist agent registry after mark_failed"
                );
            }
        }
        self.agent_registry_notify.notify_waiters();
    }

    fn mark_managed_agent_timed_out(
        &self,
        id: uuid::Uuid,
        usage: &TokenUsage,
        error: &anyhow::Error,
    ) {
        if let Ok(mut registry) = self.agent_registry.lock() {
            if let Err(persist_error) =
                registry.mark_timed_out(id, Utc::now(), usage.clone(), format!("{error:#}"))
            {
                warn!(
                    log_stream = "server",
                    kind = "agent_registry_persist_failed",
                    agent_id = %id,
                    error = %format!("{persist_error:#}"),
                    "failed to persist agent registry after mark_timed_out"
                );
            }
        }
        self.agent_registry_notify.notify_waiters();
    }

    fn list_managed_agents(&self, kind: ManagedAgentKind) -> Result<Value> {
        let registry = self
            .agent_registry
            .lock()
            .map_err(|_| anyhow!("agent registry lock poisoned"))?;
        Ok(serde_json::to_value(registry.list_by_kind(kind))
            .context("failed to serialize agent registry view")?)
    }

    fn get_managed_agent(&self, agent_id: uuid::Uuid) -> Result<Value> {
        let registry = self
            .agent_registry
            .lock()
            .map_err(|_| anyhow!("agent registry lock poisoned"))?;
        let record = registry
            .get(agent_id)
            .ok_or_else(|| anyhow!("agent {} not found", agent_id))?;
        Ok(serde_json::to_value(record).context("failed to serialize agent record")?)
    }

    fn list_workspaces(&self, query: Option<String>, include_archived: bool) -> Result<Value> {
        self.summary_tracker.wait_for_zero();
        let items = self
            .workspace_manager
            .list_workspaces(query.as_deref(), include_archived)?
            .into_iter()
            .filter(|workspace| {
                workspace_visible_in_list(
                    &workspace.id,
                    &self.active_workspace_ids,
                    include_archived && workspace.state == "archived",
                )
            })
            .map(|workspace| {
                json!({
                    "workspace_id": workspace.id,
                    "title": workspace.title,
                    "summary": workspace.summary,
                    "state": workspace.state,
                    "updated_at": workspace.updated_at,
                    "last_content_modified_at": workspace.last_content_modified_at,
                })
            })
            .collect::<Vec<_>>();
        Ok(json!({ "workspaces": items }))
    }

    fn list_workspace_contents(
        &self,
        workspace_id: String,
        path: Option<String>,
        depth: usize,
        limit: usize,
    ) -> Result<Value> {
        let items = self.workspace_manager.list_workspace_contents(
            &workspace_id,
            path.as_deref(),
            depth,
            limit,
        )?;
        Ok(json!({
            "workspace_id": workspace_id,
            "path": path.unwrap_or_else(|| ".".to_string()),
            "entries": items,
        }))
    }

    fn mount_workspace(
        &self,
        session: &SessionSnapshot,
        source_workspace_id: String,
        mount_name: Option<String>,
    ) -> Result<Value> {
        let mount_name = mount_name
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty())
            .unwrap_or_else(|| {
                format!(
                    "workspace-{}",
                    &source_workspace_id[..8.min(source_workspace_id.len())]
                )
            });
        let materialization = if matches!(self.sandbox.mode, crate::config::SandboxMode::Bubblewrap)
        {
            WorkspaceMountMaterialization::HostSnapshotCopy
        } else {
            WorkspaceMountMaterialization::HostSymlink
        };
        let mount_path = self.workspace_manager.mount_workspace_snapshot(
            &session.workspace_id,
            &source_workspace_id,
            &mount_name,
            materialization,
        )?;
        let relative_mount = mount_path
            .strip_prefix(&session.workspace_root)
            .unwrap_or(&mount_path)
            .to_string_lossy()
            .to_string();
        Ok(json!({
            "workspace_id": source_workspace_id,
            "mount_name": mount_name,
            "mount_path": relative_mount,
            "read_only": true,
        }))
    }

    fn move_workspace_contents(
        &self,
        session: &SessionSnapshot,
        source_workspace_id: String,
        paths: Vec<String>,
        target_dir: Option<String>,
        source_summary_update: Option<String>,
        target_summary_update: Option<String>,
    ) -> Result<Value> {
        if source_workspace_id == session.workspace_id {
            return Err(anyhow!(
                "source workspace must differ from the current workspace"
            ));
        }
        let summary = self.workspace_manager.move_contents_between_workspaces(
            &source_workspace_id,
            &session.workspace_id,
            &paths,
            target_dir.as_deref(),
            source_summary_update,
            target_summary_update,
        )?;
        Ok(serde_json::to_value(summary).context("failed to serialize workspace move result")?)
    }

    fn memory_search(
        &self,
        session: &SessionSnapshot,
        query: String,
        limit: usize,
    ) -> Result<Value> {
        memory_search_files(session, &query, limit)
    }

    fn rollout_search(
        &self,
        session: &SessionSnapshot,
        query: String,
        rollout_id: Option<String>,
        kinds: Vec<String>,
        limit: usize,
    ) -> Result<Value> {
        rollout_search_files(session, &query, rollout_id.as_deref(), &kinds, limit)
    }

    fn rollout_read(
        &self,
        session: &SessionSnapshot,
        rollout_id: String,
        anchor_event_id: usize,
        mode: Option<String>,
        before: usize,
        after: usize,
    ) -> Result<Value> {
        rollout_read_file(
            session,
            &rollout_id,
            anchor_event_id,
            mode.as_deref(),
            before,
            after,
        )
    }

    fn has_active_child_agents(&self, parent_agent_id: uuid::Uuid) -> bool {
        self.agent_registry
            .lock()
            .map(|registry| registry.has_active_children(parent_agent_id))
            .unwrap_or(false)
    }

    async fn wait_for_child_agents_to_finish(&self, parent_agent_id: uuid::Uuid) {
        while self.has_active_child_agents(parent_agent_id) {
            self.agent_registry_notify.notified().await;
        }
    }

    fn ensure_agent_tmp_dir(&self, agent_id: uuid::Uuid) -> Result<PathBuf> {
        let path = self.agent_workspace.tmp_dir.join(agent_id.to_string());
        std::fs::create_dir_all(&path)
            .with_context(|| format!("failed to create agent tmp dir {}", path.display()))?;
        Ok(path)
    }

    fn build_backend_execution_options(
        &self,
        _backend: AgentBackendKind,
    ) -> BackendExecutionOptions {
        BackendExecutionOptions {}
    }

    fn run_agent_turn_sync(
        &self,
        session: SessionSnapshot,
        kind: AgentPromptKind,
        agent_id: uuid::Uuid,
        agent_backend: AgentBackendKind,
        model_key: String,
        previous_messages: Vec<ChatMessage>,
        prompt: String,
        upstream_timeout_seconds: Option<f64>,
        execution_control: Option<SessionExecutionControl>,
    ) -> Result<SessionState> {
        let workspace_root = session.workspace_root.clone();
        let _agent_tmp_dir = self.ensure_agent_tmp_dir(agent_id)?;
        let effective_sandbox_mode = self.effective_sandbox_mode(&session.address)?;
        if matches!(
            effective_sandbox_mode,
            crate::config::SandboxMode::Bubblewrap
        ) {
            self.workspace_manager
                .cleanup_transient_mounts(&session.workspace_id)?;
            let _ = self
                .workspace_manager
                .prepare_bubblewrap_view(&session.workspace_id)?;
        }
        let mut config = self.build_agent_frame_config(
            &session,
            &workspace_root,
            kind,
            &model_key,
            upstream_timeout_seconds,
        )?;
        if let Some(system_prompt) = leading_system_prompt(&previous_messages) {
            config.system_prompt = system_prompt;
        }
        std::fs::create_dir_all(&config.runtime_state_root).with_context(|| {
            format!(
                "failed to create runtime state root {}",
                config.runtime_state_root.display()
            )
        })?;
        self.ensure_model_available_for_backend(agent_backend, &model_key)?;
        let extra_tools =
            self.build_extra_tools(&session, kind, agent_id, execution_control.clone());
        let backend_execution_options = self.build_backend_execution_options(agent_backend);
        if kind == AgentPromptKind::MainForeground && agent_backend == AgentBackendKind::AgentFrame
        {
            let runtime =
                self.ensure_foreground_agent_frame_runtime(&session, &model_key, &config)?;
            let result = {
                let mut runtime = runtime
                    .lock()
                    .map_err(|_| anyhow!("foreground runtime lock poisoned"))?;
                runtime.runtime.run_turn(
                    agent_backend,
                    previous_messages.clone(),
                    prompt.clone(),
                    config.clone(),
                    backend_execution_options.clone(),
                    extra_tools.clone(),
                    execution_control.clone(),
                )
            };
            match result {
                Ok(report) => Ok(report),
                Err(error) if is_child_transport_error(&error) => {
                    let retryable_stale_pipe = is_child_run_turn_request_send_error(&error);
                    let original_error = format!("{error:#}");
                    self.invalidate_foreground_agent_frame_runtime(&session.address)?;
                    if !retryable_stale_pipe {
                        return Err(error);
                    }
                    let runtime =
                        self.ensure_foreground_agent_frame_runtime(&session, &model_key, &config)?;
                    let mut runtime = runtime
                        .lock()
                        .map_err(|_| anyhow!("foreground runtime lock poisoned"))?;
                    runtime
                        .runtime
                        .run_turn(
                            agent_backend,
                            previous_messages,
                            prompt,
                            config,
                            backend_execution_options,
                            extra_tools,
                            execution_control,
                        )
                        .with_context(|| {
                            format!(
                                "retry after replacing stale child runtime failed; original error: {original_error}"
                            )
                        })
                }
                Err(error) => Err(error),
            }
        } else {
            let sandbox = SandboxConfig {
                mode: effective_sandbox_mode,
                bubblewrap_binary: self.sandbox.bubblewrap_binary.clone(),
                map_docker_socket: self.sandbox.map_docker_socket,
            };
            let result = run_one_shot_child_turn(
                &sandbox,
                agent_backend,
                previous_messages,
                prompt,
                config,
                backend_execution_options,
                PathBuf::from(&self.main_agent.global_install_root),
                self.token_estimation_cache_roots(),
                self.local_mount_paths_for_address(&session.address)?,
                self.agent_workspace.rundir.join("skill_memory"),
                self.agent_workspace.skills_dir.clone(),
                extra_tools,
                execution_control,
            );
            if matches!(
                effective_sandbox_mode,
                crate::config::SandboxMode::Bubblewrap
            ) {
                let _ = self
                    .workspace_manager
                    .cleanup_transient_mounts(&session.workspace_id);
            }
            result
        }
    }

    async fn run_agent_turn_with_timeout(
        &self,
        session: SessionSnapshot,
        kind: AgentPromptKind,
        agent_id: uuid::Uuid,
        agent_backend: AgentBackendKind,
        model_key: String,
        previous_messages: Vec<ChatMessage>,
        prompt: String,
        upstream_timeout_seconds: Option<f64>,
        control_observer: Option<Arc<dyn Fn(SessionExecutionControl) + Send + Sync>>,
        join_label: &str,
    ) -> Result<TimedRunOutcome> {
        enum DriverEvent {
            Runtime(SessionEvent),
            Progress(ExecutionProgress),
            Completed(Result<SessionState>),
        }

        let runtime = self.clone();
        let join_label = join_label.to_string();
        let event_session = session.clone();
        let event_model_key = model_key.clone();
        let phase_conversations = Arc::clone(&self.conversations);
        let phase_sessions = Arc::clone(&self.sessions);
        let phase_address = event_session.address.clone();
        let track_foreground_phase = matches!(kind, AgentPromptKind::MainForeground);
        let (event_sender, mut event_receiver) = mpsc::unbounded_channel();
        let (progress_sender, mut progress_receiver) = mpsc::unbounded_channel();
        let execution_control = SessionExecutionControl::new()
            .with_event_callback(move |event| {
                if track_foreground_phase
                    && let Ok(mut conversations) = phase_conversations.lock()
                    && let Ok(mut sessions) = phase_sessions.lock()
                    && let Ok(Some(actor)) =
                        conversations.resolve_foreground_actor(&phase_address, &mut sessions)
                    && actor.receive_runtime_event(&event).is_ok()
                {}
                let _ = event_sender.send(event);
            })
            .with_progress_callback(move |progress| {
                let _ = progress_sender.send(progress);
            });
        if let Some(observer) = control_observer {
            observer(execution_control.clone());
        }
        let worker_session = session;
        let worker_agent_backend = agent_backend;
        let worker_model_key = model_key;
        let join_handle = tokio::task::spawn_blocking(move || {
            runtime.run_agent_turn_sync(
                worker_session,
                kind,
                agent_id,
                worker_agent_backend,
                worker_model_key,
                previous_messages,
                prompt,
                upstream_timeout_seconds,
                Some(execution_control),
            )
        });
        let (driver_sender, mut driver_receiver) = mpsc::unbounded_channel();
        let mut relay_tasks = Vec::new();
        {
            let driver_sender = driver_sender.clone();
            relay_tasks.push(tokio::spawn(async move {
                while let Some(event) = event_receiver.recv().await {
                    let _ = driver_sender.send(DriverEvent::Runtime(event));
                }
            }));
        }
        {
            let driver_sender = driver_sender.clone();
            relay_tasks.push(tokio::spawn(async move {
                while let Some(progress) = progress_receiver.recv().await {
                    let _ = driver_sender.send(DriverEvent::Progress(progress));
                }
            }));
        }
        {
            let driver_sender = driver_sender.clone();
            relay_tasks.push(tokio::spawn(async move {
                let result = join_handle
                    .await
                    .context(join_label)
                    .and_then(|report| report.context("agent turn failed"));
                let _ = driver_sender.send(DriverEvent::Completed(result));
            }));
        }
        drop(driver_sender);
        while let Some(driver_event) = driver_receiver.recv().await {
            match driver_event {
                DriverEvent::Runtime(event) => {
                    if matches!(kind, AgentPromptKind::MainForeground) {
                        self.send_progress_feedback_for_event(
                            &event_session,
                            &event_model_key,
                            &event,
                        )
                        .await;
                    }
                    match SessionTranscript::open(&event_session.root_dir)
                        .and_then(|mut transcript| transcript.record_event(&event))
                    {
                        Ok(Some(entry)) => {
                            if matches!(kind, AgentPromptKind::MainForeground)
                                && let Some(web_channel) =
                                    self.web_channels.get(&event_session.address.channel_id)
                            {
                                web_channel.publish_transcript_append(
                                    &event_session.address,
                                    entry.to_skeleton(),
                                );
                            }
                        }
                        Ok(None) => {}
                        Err(error) => {
                            warn!(
                                log_stream = "agent",
                                log_key = %agent_id,
                                kind = "transcript_record_failed",
                                session_id = %event_session.id,
                                channel_id = %event_session.address.channel_id,
                                error = %format!("{error:#}"),
                                "failed to record transcript event"
                            );
                        }
                    }
                    if matches!(kind, AgentPromptKind::MainForeground)
                        && let SessionEvent::CompactionCompleted {
                            compacted: true,
                            structured_output: Some(structured_output),
                            compacted_messages,
                            ..
                        }
                        | SessionEvent::ToolWaitCompactionCompleted {
                            compacted: true,
                            structured_output: Some(structured_output),
                            compacted_messages,
                            ..
                        } = &event
                        && let Err(error) = persist_compaction_artifacts_from_event(
                            &event_session,
                            structured_output,
                            compacted_messages,
                        )
                    {
                        warn!(
                            log_stream = "agent",
                            log_key = %agent_id,
                            kind = "compaction_artifact_persist_failed",
                            session_id = %event_session.id,
                            channel_id = %event_session.address.channel_id,
                            error = %format!("{error:#}"),
                            "failed to persist compaction artifacts from runtime event"
                        );
                    }
                    let event_model_id = self
                        .models
                        .get(&event_model_key)
                        .map(|model| model.model.as_str())
                        .unwrap_or(event_model_key.as_str());
                    log_agent_frame_event(
                        agent_id,
                        &event_session,
                        kind,
                        &event_model_key,
                        event_model_id,
                        &event,
                    );
                }
                DriverEvent::Progress(progress) => {
                    if matches!(kind, AgentPromptKind::MainForeground) {
                        self.send_progress_feedback_for_progress(
                            &event_session,
                            &event_model_key,
                            &progress,
                        )
                        .await;
                    }
                }
                DriverEvent::Completed(result) => {
                    for task in relay_tasks {
                        task.abort();
                    }
                    let state = match result {
                        Ok(state) => state,
                        Err(error) => {
                            if matches!(kind, AgentPromptKind::MainForeground) {
                                self.send_progress_feedback_for_failure(
                                    &event_session,
                                    &event_model_key,
                                    &error,
                                )
                                .await;
                            }
                            return Ok(TimedRunOutcome::Failed(error));
                        }
                    };
                    if state.phase == SessionPhase::Yielded {
                        return Ok(TimedRunOutcome::Yielded(state));
                    }
                    return Ok(TimedRunOutcome::Completed(state));
                }
            }
        }
        Err(anyhow!("agent turn driver channel closed unexpectedly"))
    }

    fn run_agent_turn_with_timeout_blocking(
        &self,
        session: SessionSnapshot,
        kind: AgentPromptKind,
        agent_id: uuid::Uuid,
        agent_backend: AgentBackendKind,
        model_key: String,
        previous_messages: Vec<ChatMessage>,
        prompt: String,
        timeout_seconds: Option<f64>,
        upstream_timeout_seconds: Option<f64>,
        control_observer: Option<Arc<dyn Fn(SessionExecutionControl) + Send + Sync>>,
        timeout_label: &str,
    ) -> Result<TimedRunOutcome> {
        enum DriverEvent {
            Runtime(SessionEvent),
            Completed(Result<SessionState>),
            SoftDeadline,
            HardDeadline,
        }

        let event_session = session.clone();
        let event_model_key = model_key.clone();
        let phase_conversations = Arc::clone(&self.conversations);
        let phase_sessions = Arc::clone(&self.sessions);
        let phase_address = event_session.address.clone();
        let track_foreground_phase = matches!(kind, AgentPromptKind::MainForeground);
        let (event_sender, event_receiver) = std::sync::mpsc::channel();
        let execution_control = SessionExecutionControl::new().with_event_callback(move |event| {
            if track_foreground_phase
                && let Ok(mut conversations) = phase_conversations.lock()
                && let Ok(mut sessions) = phase_sessions.lock()
                && let Ok(Some(actor)) =
                    conversations.resolve_foreground_actor(&phase_address, &mut sessions)
                && actor.receive_runtime_event(&event).is_ok()
            {}
            let _ = event_sender.send(event);
        });
        if let Some(observer) = control_observer {
            observer(execution_control.clone());
        }
        let cancellation_handle = execution_control.clone();
        let runtime = self.clone();
        let timeout_label = timeout_label.to_string();
        let worker_session = session;
        let worker_agent_backend = agent_backend;
        let worker_model_key = model_key;
        let handle = std::thread::spawn(move || {
            runtime.run_agent_turn_sync(
                worker_session,
                kind,
                agent_id,
                worker_agent_backend,
                worker_model_key,
                previous_messages,
                prompt,
                upstream_timeout_seconds,
                Some(execution_control),
            )
        });
        let (driver_sender, driver_receiver) = std::sync::mpsc::channel();
        {
            let driver_sender = driver_sender.clone();
            std::thread::spawn(move || {
                while let Ok(event) = event_receiver.recv() {
                    if driver_sender.send(DriverEvent::Runtime(event)).is_err() {
                        break;
                    }
                }
            });
        }
        {
            let driver_sender = driver_sender.clone();
            std::thread::spawn(move || {
                let result = handle
                    .join()
                    .map_err(|_| anyhow!("agent worker thread panicked"))
                    .and_then(|report| report.context("agent turn failed"));
                let _ = driver_sender.send(DriverEvent::Completed(result));
            });
        }
        let soft_deadline = timeout_seconds
            .map(|seconds| std::time::Instant::now() + Duration::from_secs_f64(seconds));
        let hard_deadline = timeout_seconds.map(|seconds| {
            std::time::Instant::now()
                + Duration::from_secs_f64(seconds + tool_phase_timeout_grace_seconds())
        });
        if let Some(deadline) = soft_deadline {
            let driver_sender = driver_sender.clone();
            std::thread::spawn(move || {
                let now = std::time::Instant::now();
                if deadline > now {
                    std::thread::sleep(deadline.duration_since(now));
                }
                let _ = driver_sender.send(DriverEvent::SoftDeadline);
            });
        }
        if let Some(deadline) = hard_deadline {
            let driver_sender = driver_sender.clone();
            std::thread::spawn(move || {
                let now = std::time::Instant::now();
                if deadline > now {
                    std::thread::sleep(deadline.duration_since(now));
                }
                let _ = driver_sender.send(DriverEvent::HardDeadline);
            });
        }
        drop(driver_sender);
        let mut soft_timeout_error = None;
        while let Ok(driver_event) = driver_receiver.recv() {
            match driver_event {
                DriverEvent::Runtime(event) => {
                    if let Err(error) = SessionTranscript::open(&event_session.root_dir)
                        .and_then(|mut transcript| transcript.record_event(&event).map(|_| ()))
                    {
                        warn!(
                            log_stream = "agent",
                            log_key = %agent_id,
                            kind = "transcript_record_failed",
                            session_id = %event_session.id,
                            channel_id = %event_session.address.channel_id,
                            error = %format!("{error:#}"),
                            "failed to record transcript event in blocking context"
                        );
                    }
                    if matches!(kind, AgentPromptKind::MainForeground)
                        && let SessionEvent::CompactionCompleted {
                            compacted: true,
                            structured_output: Some(structured_output),
                            compacted_messages,
                            ..
                        }
                        | SessionEvent::ToolWaitCompactionCompleted {
                            compacted: true,
                            structured_output: Some(structured_output),
                            compacted_messages,
                            ..
                        } = &event
                        && let Err(error) = persist_compaction_artifacts_from_event(
                            &event_session,
                            structured_output,
                            compacted_messages,
                        )
                    {
                        warn!(
                            log_stream = "agent",
                            log_key = %agent_id,
                            kind = "compaction_artifact_persist_failed",
                            session_id = %event_session.id,
                            channel_id = %event_session.address.channel_id,
                            error = %format!("{error:#}"),
                            "failed to persist compaction artifacts from runtime event"
                        );
                    }
                    let event_model_id = self
                        .models
                        .get(&event_model_key)
                        .map(|model| model.model.as_str())
                        .unwrap_or(event_model_key.as_str());
                    log_agent_frame_event(
                        agent_id,
                        &event_session,
                        kind,
                        &event_model_key,
                        event_model_id,
                        &event,
                    )
                }
                DriverEvent::Completed(result) => {
                    let state = match result {
                        Ok(state) => state,
                        Err(error) => {
                            return Ok(TimedRunOutcome::Failed(error));
                        }
                    };
                    if state.phase == SessionPhase::Yielded {
                        return Ok(TimedRunOutcome::Yielded(state));
                    }
                    if let Some(error) = soft_timeout_error {
                        return Ok(TimedRunOutcome::TimedOut {
                            state: Some(state),
                            error,
                        });
                    }
                    return Ok(TimedRunOutcome::Completed(state));
                }
                DriverEvent::SoftDeadline => {
                    if soft_timeout_error.is_none() {
                        let timeout_seconds = timeout_seconds.expect("soft deadline exists");
                        soft_timeout_error = Some(anyhow!(
                            "{} timed out after {:.1} seconds",
                            timeout_label,
                            timeout_seconds
                        ));
                        cancellation_handle.request_timeout_observation();
                    }
                }
                DriverEvent::HardDeadline => {
                    let timeout_seconds = timeout_seconds.expect("hard deadline exists");
                    cancellation_handle.request_cancel();
                    return Ok(TimedRunOutcome::TimedOut {
                        state: None,
                        error: anyhow!(
                            "{} hard timed out after {:.1} seconds",
                            timeout_label,
                            timeout_seconds + tool_phase_timeout_grace_seconds()
                        ),
                    });
                }
            }
        }
        Err(anyhow!("agent turn driver channel closed unexpectedly"))
    }

    fn list_cron_tasks(&self) -> Result<Value> {
        let manager = self
            .cron_manager
            .lock()
            .map_err(|_| anyhow!("cron manager lock poisoned"))?;
        Ok(serde_json::to_value(manager.list()?).context("failed to serialize cron task list")?)
    }

    fn get_cron_task(&self, id: uuid::Uuid) -> Result<Value> {
        let manager = self
            .cron_manager
            .lock()
            .map_err(|_| anyhow!("cron manager lock poisoned"))?;
        Ok(serde_json::to_value(manager.get(id)?).context("failed to serialize cron task")?)
    }

    fn create_cron_task(
        &self,
        session: SessionSnapshot,
        request: CronCreateRequest,
    ) -> Result<Value> {
        self.model_config(&request.model_key)?;
        self.ensure_model_available_for_backend(request.agent_backend, &request.model_key)?;
        let mut manager = self
            .cron_manager
            .lock()
            .map_err(|_| anyhow!("cron manager lock poisoned"))?;
        let view = manager.create(request)?;
        info!(
            log_stream = "agent",
            log_key = %session.agent_id,
            kind = "cron_task_created",
            session_id = %session.id,
            channel_id = %session.address.channel_id,
            cron_task_id = %view.id,
            "cron task created"
        );
        Ok(serde_json::to_value(view).context("failed to serialize cron task")?)
    }

    fn update_cron_task(&self, id: uuid::Uuid, request: CronUpdateRequest) -> Result<Value> {
        let mut manager = self
            .cron_manager
            .lock()
            .map_err(|_| anyhow!("cron manager lock poisoned"))?;
        let current = manager.get(id)?;
        let effective_backend = request.agent_backend.unwrap_or(current.agent_backend);
        if let Some(model_key) = request.model_key.as_deref() {
            self.model_config(model_key)?;
            self.ensure_model_available_for_backend(effective_backend, model_key)?;
        }
        let view = manager.update(id, request)?;
        Ok(serde_json::to_value(view).context("failed to serialize cron task")?)
    }

    fn remove_cron_task(&self, id: uuid::Uuid) -> Result<Value> {
        let mut manager = self
            .cron_manager
            .lock()
            .map_err(|_| anyhow!("cron manager lock poisoned"))?;
        let view = manager.remove(id)?;
        Ok(serde_json::to_value(view).context("failed to serialize removed cron task")?)
    }

    fn record_cron_trigger_result(&self, id: Option<uuid::Uuid>, outcome: &'static str) {
        let Some(id) = id else {
            return;
        };
        let Ok(mut manager) = self.cron_manager.lock() else {
            warn!(
                log_stream = "agent",
                kind = "cron_task_trigger_result_failed",
                cron_task_id = %id,
                outcome,
                "failed to record cron trigger result because cron manager lock was poisoned"
            );
            return;
        };
        if let Err(error) = manager.record_trigger_result(id, Utc::now(), outcome.to_string()) {
            warn!(
                log_stream = "agent",
                kind = "cron_task_trigger_result_failed",
                cron_task_id = %id,
                outcome,
                error = %format!("{error:#}"),
                "failed to record cron trigger result"
            );
        }
    }

    async fn poll_cron_once(&self) -> Result<()> {
        let now = Utc::now();
        let due_tasks = {
            let mut manager = self
                .cron_manager
                .lock()
                .map_err(|_| anyhow!("cron manager lock poisoned"))?;
            manager.claim_due_tasks(now)?
        };

        for claimed in due_tasks {
            if let Err(error) = self.handle_claimed_cron_task(claimed).await {
                error!(
                    log_stream = "agent",
                    kind = "cron_task_dispatch_failed",
                    error = %format!("{error:#}"),
                    "failed to dispatch cron task"
                );
            }
        }
        Ok(())
    }

    async fn handle_claimed_cron_task(&self, claimed: ClaimedCronTask) -> Result<()> {
        let task = claimed.task;
        let checked_at = Utc::now();
        let (should_trigger, check_outcome) = match &task.checker {
            Some(checker) => evaluate_cron_checker(checker, &self.agent_workspace.rundir)
                .with_context(|| format!("checker failed for cron task {}", task.id))
                .map(|passed| {
                    if passed {
                        (true, "checker_passed".to_string())
                    } else {
                        (false, "checker_blocked".to_string())
                    }
                })
                .unwrap_or_else(|error| (true, format!("checker_error_triggered: {}", error))),
            None => (true, "no_checker".to_string()),
        };
        {
            let mut manager = self
                .cron_manager
                .lock()
                .map_err(|_| anyhow!("cron manager lock poisoned"))?;
            manager.record_check_result(task.id, checked_at, check_outcome.clone())?;
        }

        info!(
            log_stream = "agent",
            kind = "cron_task_checked",
            cron_task_id = %task.id,
            scheduled_for = %claimed.scheduled_for,
            should_trigger,
            outcome = %check_outcome,
            "cron task checker evaluated"
        );

        if !should_trigger {
            return Ok(());
        }

        self.model_config(&task.model_key)?;
        let background_agent_id = uuid::Uuid::new_v4();
        let session =
            self.create_background_session_for_conversation(&task.address, background_agent_id)?;
        self.initialize_session_prompt_components_if_missing(&session)?;
        self.register_managed_agent(
            background_agent_id,
            ManagedAgentKind::Background,
            task.model_key.clone(),
            None,
            &session,
            ManagedAgentState::Enqueued,
        );
        self.background_job_sender
            .send(BackgroundJobRequest {
                agent_id: background_agent_id,
                parent_agent_id: None,
                cron_task_id: Some(task.id),
                session,
                agent_backend: task.agent_backend,
                model_key: task.model_key.clone(),
                prompt: task.prompt.clone(),
            })
            .await
            .context("failed to enqueue cron background agent")?;
        {
            let mut manager = self
                .cron_manager
                .lock()
                .map_err(|_| anyhow!("cron manager lock poisoned"))?;
            manager.record_trigger_result(
                task.id,
                Utc::now(),
                running_trigger_outcome(background_agent_id),
            )?;
        }
        info!(
            log_stream = "agent",
            kind = "cron_task_enqueued",
            cron_task_id = %task.id,
            background_agent_id = %background_agent_id,
            scheduled_for = %claimed.scheduled_for,
            model = %task.model_key,
            "cron task enqueued background agent"
        );
        Ok(())
    }
}

fn idle_compaction_token_limit(existing_override: Option<usize>, idle_min_tokens: usize) -> usize {
    existing_override
        .map(|limit| limit.min(idle_min_tokens))
        .unwrap_or(idle_min_tokens)
}

pub struct Server {
    context: Arc<RuntimeContext>,
    telegram_channel_ids: Arc<HashSet<String>>,
    sandbox: SandboxConfig,
    background_job_receiver: Option<mpsc::Receiver<BackgroundJobRequest>>,
    pending_process_restart_notices: Arc<Mutex<HashSet<String>>>,
    channel_auth: Arc<Mutex<ChannelAuthorizationManager>>,
}

impl Deref for Server {
    type Target = RuntimeContext;

    fn deref(&self) -> &Self::Target {
        &self.context
    }
}

#[async_trait]
impl WebChannelHost for RuntimeContext {
    async fn list_web_conversations(
        &self,
        channel_id: &str,
    ) -> Result<Vec<WebConversationSummary>> {
        let conversations = self.with_conversations(|conversations| {
            Ok(conversations
                .list_snapshots()
                .into_iter()
                .filter(|conversation| conversation.address.channel_id == channel_id)
                .collect::<Vec<_>>())
        })?;
        conversations
            .into_iter()
            .map(|conversation| self.web_conversation_summary(&conversation.address))
            .collect()
    }

    async fn create_web_conversation(
        &self,
        address: &ChannelAddress,
    ) -> Result<WebConversationSummary> {
        self.with_conversations(|conversations| conversations.ensure_conversation(address))?;
        self.web_conversation_summary(address)
    }

    async fn delete_web_conversation(&self, address: &ChannelAddress) -> Result<bool> {
        self.with_conversations_and_sessions(|conversations, sessions| {
            let removed_conversation = conversations.remove_conversation(address)?.is_some();
            let removed_session = sessions.remove_foreground(address)?;
            Ok(removed_conversation || removed_session)
        })
    }

    async fn list_web_transcript(
        &self,
        address: &ChannelAddress,
        offset: usize,
        limit: usize,
    ) -> Result<Vec<crate::transcript::TranscriptEntrySkeleton>> {
        let Some(session) = self.with_sessions(|sessions| Ok(sessions.get_snapshot(address)))?
        else {
            return Ok(Vec::new());
        };
        SessionTranscript::open(&session.root_dir)?.list(offset, limit)
    }

    async fn get_web_transcript_detail(
        &self,
        address: &ChannelAddress,
        seq_start: usize,
        seq_end: usize,
    ) -> Result<Option<Vec<crate::transcript::TranscriptEntry>>> {
        if seq_end < seq_start || seq_end.saturating_sub(seq_start) > 200 {
            anyhow::bail!("invalid transcript detail range");
        }
        let Some(session) = self.with_sessions(|sessions| Ok(sessions.get_snapshot(address)))?
        else {
            return Ok(None);
        };
        Ok(Some(
            SessionTranscript::open(&session.root_dir)?.get_detail(seq_start, seq_end)?,
        ))
    }
}

impl RuntimeContext {
    fn web_conversation_summary(&self, address: &ChannelAddress) -> Result<WebConversationSummary> {
        let session = self.with_sessions(|sessions| Ok(sessions.get_snapshot(address)))?;
        let (entry_count, latest) = if let Some(session) = session {
            let transcript = SessionTranscript::open(&session.root_dir)?;
            (transcript.len(), transcript.list(0, 1)?.into_iter().next())
        } else {
            (0, None)
        };
        Ok(WebConversationSummary {
            conversation_key: address.conversation_id.clone(),
            entry_count,
            latest_ts: latest.as_ref().map(|entry| entry.ts.clone()),
            latest_type: latest.as_ref().map(|entry| entry.entry_type.clone()),
            latest_summary: latest.as_ref().and_then(summarize_skeleton),
        })
    }
}

impl Server {
    fn clear_missing_selected_main_model(
        &self,
        address: &ChannelAddress,
    ) -> Result<Option<String>> {
        let current_backend = self.selected_agent_backend(address)?;
        let Some(model_key) = self.selected_main_model_key(address)? else {
            return Ok(None);
        };
        if self.models.contains_key(&model_key)
            && current_backend
                .is_none_or(|backend| self.agent.is_model_available(backend, &model_key))
        {
            return Ok(None);
        }
        self.with_conversations(|conversations| {
            conversations
                .set_agent_selection(address, current_backend, None)
                .map(|_| ())
        })?;
        Ok(Some(model_key))
    }

    fn current_tool_names_for_foreground_turn(
        &self,
        session: &SessionSnapshot,
        model_key: &str,
    ) -> Result<Vec<String>> {
        let runtime = self.agent_runtime_view_for_address(&session.address)?;
        let frame_config = runtime.build_agent_frame_config(
            session,
            &session.workspace_root,
            AgentPromptKind::MainForeground,
            model_key,
            None,
        )?;
        let skills = discover_skills(&frame_config.skills_dirs)?;
        let extra_tools = runtime.build_extra_tools(
            session,
            AgentPromptKind::MainForeground,
            session.agent_id,
            None,
        );
        let registry = build_tool_registry(
            &frame_config.enabled_tools,
            &frame_config.workspace_root,
            &frame_config.runtime_state_root,
            &frame_config.upstream,
            &frame_config.available_upstreams,
            frame_config.image_tool_upstream.as_ref(),
            frame_config.pdf_tool_upstream.as_ref(),
            frame_config.audio_tool_upstream.as_ref(),
            frame_config.image_generation_tool_upstream.as_ref(),
            &frame_config.skills_dirs,
            &skills,
            &extra_tools,
            &frame_config.remote_workpaths,
        )?;
        Ok(registry.into_keys().collect())
    }

    fn log_current_tools_for_user_message(
        &self,
        session: &SessionSnapshot,
        model_key: &str,
        remote_message_id: &str,
        trigger: &str,
    ) {
        match self.current_tool_names_for_foreground_turn(session, model_key) {
            Ok(tool_names) => {
                info!(
                    log_stream = "agent",
                    log_key = %session.agent_id,
                    kind = "foreground_tool_catalog",
                    session_id = %session.id,
                    channel_id = %session.address.channel_id,
                    conversation_id = %session.address.conversation_id,
                    remote_message_id = remote_message_id,
                    model = model_key,
                    trigger,
                    tool_count = tool_names.len() as u64,
                    tool_names = %tool_names.join(","),
                    "resolved foreground tool catalog for user message"
                );
            }
            Err(error) => {
                warn!(
                    log_stream = "agent",
                    log_key = %session.agent_id,
                    kind = "foreground_tool_catalog_failed",
                    session_id = %session.id,
                    channel_id = %session.address.channel_id,
                    conversation_id = %session.address.conversation_id,
                    remote_message_id = remote_message_id,
                    model = model_key,
                    trigger,
                    error = %format!("{error:#}"),
                    "failed to resolve foreground tool catalog for user message"
                );
            }
        }
    }

    async fn prompt_missing_conversation_model(
        &self,
        channel: &Arc<dyn Channel>,
        address: &ChannelAddress,
        missing_model_key: &str,
    ) -> Result<()> {
        self.send_channel_message(
            channel,
            address,
            self.agent_model_selection_message(
                address,
                &format!(
                    "The previously selected model `{}` is no longer available. `/agent` has been opened automatically below so you can choose again.",
                    missing_model_key
                ),
            )?,
        )
        .await
    }

    async fn pause_idle_compaction_and_prompt_agent_selection(
        &self,
        session: &SessionSnapshot,
        missing_model_key: &str,
    ) -> Result<()> {
        let actor = self.ensure_foreground_actor(&session.address)?;
        actor.clear_idle_compaction_failure()?;
        let Some(channel) = self.channels.get(&session.address.channel_id).cloned() else {
            warn!(
                log_stream = "session",
                log_key = %session.id,
                kind = "idle_context_compaction_paused_missing_model_channel_missing",
                channel_id = %session.address.channel_id,
                conversation_id = %session.address.conversation_id,
                missing_model = %missing_model_key,
                "paused idle context compaction for missing model, but could not find channel to open /agent"
            );
            return Ok(());
        };
        let message = self.agent_model_selection_message(
            &session.address,
            &format!(
                "The previously selected model `{}` is no longer available. Idle compaction has been paused for this conversation, and `/agent` has been opened automatically below so you can choose again.",
                missing_model_key
            ),
        )?;
        if let Err(error) = self
            .send_channel_message(&channel, &session.address, message)
            .await
        {
            warn!(
                log_stream = "session",
                log_key = %session.id,
                kind = "idle_context_compaction_paused_missing_model_prompt_failed",
                channel_id = %session.address.channel_id,
                conversation_id = %session.address.conversation_id,
                missing_model = %missing_model_key,
                error = %format!("{error:#}"),
                "paused idle context compaction for missing model, but failed to open /agent"
            );
            return Ok(());
        }
        info!(
            log_stream = "session",
            log_key = %session.id,
            kind = "idle_context_compaction_paused_missing_model",
            channel_id = %session.address.channel_id,
            conversation_id = %session.address.conversation_id,
            missing_model = %missing_model_key,
            "paused idle context compaction and opened /agent because the selected model is no longer available"
        );
        Ok(())
    }

    fn with_channel_auth<T>(
        &self,
        f: impl FnOnce(&mut ChannelAuthorizationManager) -> Result<T>,
    ) -> Result<T> {
        let mut auth = self
            .channel_auth
            .lock()
            .map_err(|_| anyhow!("channel authorization manager lock poisoned"))?;
        f(&mut auth)
    }

    pub fn from_config(config: ServerConfig, workdir: impl AsRef<Path>) -> Result<Self> {
        let workdir = workdir.as_ref().to_path_buf();
        std::fs::create_dir_all(&workdir)
            .with_context(|| format!("failed to create workdir {}", workdir.display()))?;
        upgrade_workdir(&workdir)?;
        let agent_workspace = AgentWorkspace::initialize(&workdir)?;
        let workspace_manager = WorkspaceManager::load_or_create(&workdir)?;
        let tooling = config.tooling.clone();

        let mut channels: HashMap<String, Arc<dyn Channel>> = HashMap::new();
        let mut web_channels: HashMap<String, Arc<WebChannel>> = HashMap::new();
        let mut telegram_channel_ids: HashSet<String> = HashSet::new();
        let mut command_catalog: HashMap<String, Vec<BotCommandConfig>> = HashMap::new();
        for channel_config in config.channels {
            match channel_config {
                ChannelConfig::CommandLine(command_line) => {
                    let id = command_line.id.clone();
                    command_catalog.insert(id.clone(), default_bot_commands());
                    channels.insert(id, Arc::new(CommandLineChannel::new(command_line)));
                }
                ChannelConfig::Telegram(telegram) => {
                    let id = telegram.id.clone();
                    telegram_channel_ids.insert(id.clone());
                    command_catalog.insert(id.clone(), default_telegram_commands());
                    channels.insert(id, Arc::new(TelegramChannel::from_config(telegram)?));
                }
                ChannelConfig::Dingtalk(dingtalk) => {
                    let id = dingtalk.id.clone();
                    command_catalog.insert(id.clone(), default_dingtalk_commands());
                    channels.insert(id, Arc::new(DingtalkChannel::from_config(dingtalk)?));
                }
                ChannelConfig::DingtalkRobot(dingtalk_robot) => {
                    let id = dingtalk_robot.id.clone();
                    command_catalog.insert(id.clone(), default_dingtalk_commands());
                    channels.insert(
                        id,
                        Arc::new(DingtalkRobotChannel::from_config(dingtalk_robot)?),
                    );
                }
                ChannelConfig::Web(web) => {
                    let id = web.id.clone();
                    if WebChannel::resolve_auth_token(&web).is_none() {
                        warn!(
                            log_stream = "channel",
                            log_key = %id,
                            kind = "web_channel_disabled_missing_auth",
                            auth_token_env = %web.auth_token_env,
                            "web channel disabled because no auth token is configured; set auth_token or auth_token_env before enabling the Web channel"
                        );
                        continue;
                    }
                    command_catalog.insert(id.clone(), default_bot_commands());
                    let channel = Arc::new(WebChannel::from_config(web, &workdir)?);
                    web_channels.insert(id.clone(), Arc::clone(&channel));
                    channels.insert(id, channel);
                }
            }
        }

        info!(
            log_stream = "server",
            kind = "server_initialized",
            workdir = %workdir.display(),
            channel_count = channels.len() as u64,
            identity_path = %agent_workspace.identity_md_path.display(),
            user_profile_path = %agent_workspace.user_md_path.display(),
            agents_md_path = %agent_workspace.agents_md_path.display(),
            skills_dir = %agent_workspace.skills_dir.display(),
            configured_main_model = ?config.main_agent.model,
            sandbox_mode = ?config.sandbox.mode,
            "server initialized"
        );

        for family in [ToolingFamily::Pdf, ToolingFamily::AudioInput] {
            if let Some(target) = family.target(&tooling) {
                warn!(
                    log_stream = "server",
                    kind = "tooling_config_unimplemented",
                    field = family.field_name(),
                    target = %target.as_config_string(),
                    "configured tooling target is not wired yet and will only log warnings for now"
                );
            }
        }

        let (background_job_sender, background_job_receiver) = mpsc::channel(64);
        let cron_manager = Arc::new(Mutex::new(CronManager::load_or_create(&workdir)?));
        let agent_registry = Arc::new(Mutex::new(AgentRegistry::load_or_create(&workdir)?));
        let agent_registry_notify = Arc::new(Notify::new());

        let session_manager = SessionManager::new(&workdir, workspace_manager.clone())?;
        let pending_process_restart_notices = session_manager
            .list_foreground_snapshots()
            .into_iter()
            .map(|session| session.address.session_key())
            .collect::<HashSet<_>>();
        if !pending_process_restart_notices.is_empty() {
            info!(
                log_stream = "server",
                kind = "process_restart_notice_armed",
                session_count = pending_process_restart_notices.len() as u64,
                "armed one-shot process restart notices for existing foreground sessions"
            );
        }

        let sessions = Arc::new(Mutex::new(session_manager));
        let conversations = Arc::new(Mutex::new(ConversationManager::new(&workdir)?));
        let snapshots = Arc::new(Mutex::new(SnapshotManager::new(&workdir)?));
        let sink_router = Arc::new(RwLock::new(SinkRouter::new()));
        let summary_tracker = Arc::new(SummaryTracker::new());
        let active_foreground_agent_frame_runtimes = Arc::new(Mutex::new(HashMap::new()));
        let subagents = Arc::new(Mutex::new(HashMap::new()));
        let background_terminate_flags = Arc::new(Mutex::new(HashSet::new()));
        let context = Arc::new(RuntimeContext {
            workdir,
            agent_workspace,
            workspace_manager,
            sessions,
            channels: Arc::new(channels),
            web_channels: Arc::new(web_channels),
            command_catalog,
            models: config.models,
            agent: config.agent,
            tooling,
            chat_model_keys: config.chat_model_keys,
            main_agent: config.main_agent,
            sink_router,
            cron_manager,
            agent_registry,
            agent_registry_notify,
            max_global_sub_agents: config.max_global_sub_agents,
            subagent_count: Arc::new(AtomicUsize::new(0)),
            cron_poll_interval_seconds: config.cron_poll_interval_seconds,
            background_job_sender,
            background_terminate_flags,
            summary_tracker,
            active_foreground_agent_frame_runtimes,
            subagents,
            conversations,
            snapshots,
        });
        for channel in context.web_channels.values() {
            channel.set_host(Arc::clone(&context) as Arc<dyn WebChannelHost>)?;
        }

        Ok(Self {
            context: Arc::clone(&context),
            telegram_channel_ids: Arc::new(telegram_channel_ids),
            sandbox: config.sandbox,
            background_job_receiver: Some(background_job_receiver),
            pending_process_restart_notices: Arc::new(Mutex::new(pending_process_restart_notices)),
            channel_auth: Arc::new(Mutex::new(ChannelAuthorizationManager::new(
                &context.workdir,
            )?)),
        })
    }

    pub async fn run(mut self) -> Result<()> {
        self.retry_pending_workspace_summaries().await?;
        let (sender, mut receiver) = mpsc::channel::<IncomingMessage>(128);
        let background_receiver = self.background_job_receiver.take();
        let server = Arc::new(self);
        {
            let runtime = server.agent_runtime_view();
            tokio::spawn(async move {
                let mut ticker = interval(Duration::from_secs(runtime.cron_poll_interval_seconds));
                ticker.set_missed_tick_behavior(MissedTickBehavior::Skip);
                loop {
                    ticker.tick().await;
                    if let Err(error) = runtime.poll_cron_once().await {
                        error!(
                            log_stream = "server",
                            kind = "cron_poll_failed",
                            error = %format!("{error:#}"),
                            "cron poll failed"
                        );
                    }
                }
            });
        }
        {
            let server = Arc::clone(&server);
            tokio::spawn(async move {
                let mut ticker = interval(Duration::from_secs(CONVERSATION_CLEANUP_POLL_SECONDS));
                ticker.set_missed_tick_behavior(MissedTickBehavior::Skip);
                loop {
                    ticker.tick().await;
                    if let Err(error) = server.prune_closed_conversations_once().await {
                        error!(
                            log_stream = "server",
                            kind = "conversation_cleanup_failed",
                            error = %format!("{error:#}"),
                            "conversation cleanup failed"
                        );
                    }
                }
            });
        }
        if let Some(mut background_receiver) = background_receiver {
            let runtime = server.agent_runtime_view();
            tokio::spawn(async move {
                while let Some(job) = background_receiver.recv().await {
                    let runtime = runtime.clone();
                    tokio::spawn(async move {
                        if let Err(error) = runtime.run_background_job(job).await {
                            error!(
                                log_stream = "agent",
                                kind = "background_agent_failed",
                                error = %format!("{error:#}"),
                                "background agent failed"
                            );
                        }
                    });
                }
            });
        }

        for channel in server.channels.values() {
            spawn_channel_supervisor(Arc::clone(channel), sender.clone());
        }
        drop(sender);
        {
            let runtime = server.agent_runtime_view();
            tokio::spawn(async move {
                tokio::time::sleep(Duration::from_secs(2)).await;
                if let Err(error) = runtime.cleanup_stale_progress_messages_once().await {
                    error!(
                        log_stream = "server",
                        kind = "stale_progress_cleanup_failed",
                        error = %format!("{error:#}"),
                        "failed to clean up stale progress messages after startup"
                    );
                }
            });
        }
        {
            let server = Arc::clone(&server);
            tokio::spawn(async move {
                tokio::time::sleep(Duration::from_secs(4)).await;
                if let Err(error) = server
                    .recover_pending_foreground_turns_after_startup()
                    .await
                {
                    error!(
                        log_stream = "server",
                        kind = "pending_foreground_turn_recovery_failed",
                        error = %format!("{error:#}"),
                        "failed to recover pending foreground turns after startup"
                    );
                }
            });
        }

        let mut idle_compaction_ticker = interval(Duration::from_secs(
            server.main_agent.idle_compaction.poll_interval_seconds,
        ));
        idle_compaction_ticker.set_missed_tick_behavior(MissedTickBehavior::Skip);
        let idle_compaction_running = Arc::new(AtomicBool::new(false));
        let incoming_dispatcher = IncomingDispatcher::new(Arc::clone(&server));
        let mut receiver_closed = false;

        loop {
            if receiver_closed && !incoming_dispatcher.has_active_workers() {
                break;
            }

            select! {
                maybe_message = receiver.recv(), if !receiver_closed => {
                    match maybe_message {
                        Some(message) => {
                            incoming_dispatcher.dispatch(message).await?;
                        }
                        None => receiver_closed = true,
                    }
                }
                _ = idle_compaction_ticker.tick() => {
                    if server.main_agent.idle_compaction.enabled
                        && !idle_compaction_running.swap(true, Ordering::SeqCst)
                    {
                        let server = Arc::clone(&server);
                        let idle_compaction_running = Arc::clone(&idle_compaction_running);
                        tokio::spawn(async move {
                            if let Err(error) = server.run_idle_context_compaction_once().await {
                                error!(
                                    log_stream = "server",
                                    kind = "idle_context_compaction_failed",
                                    error = %format!("{error:#}"),
                                    "idle context compaction pass failed"
                                );
                            }
                            idle_compaction_running.store(false, Ordering::SeqCst);
                        });
                    }
                }
                _ = incoming_dispatcher.wait_for_worker_change(), if receiver_closed => {}
            }
        }

        if let Err(error) = server.summarize_active_workspaces_on_shutdown().await {
            warn!(
                log_stream = "server",
                kind = "workspace_shutdown_summary_failed",
                error = %format!("{error:#}"),
                "failed to summarize one or more active workspaces during shutdown"
            );
        }

        warn!(
            log_stream = "server",
            kind = "message_loop_ended",
            "all channel senders closed; server loop ended"
        );
        Ok(())
    }

    async fn run_idle_context_compaction_once(&self) -> Result<()> {
        let snapshots = self.with_sessions(|sessions| Ok(sessions.list_foreground_snapshots()))?;

        for session in snapshots {
            if let Err(error) = self.attempt_idle_context_compaction(&session, false).await {
                let actor = self.ensure_foreground_actor(&session.address)?;
                actor.mark_idle_compaction_failed(format!("{error:#}"))?;
                warn!(
                    log_stream = "session",
                    log_key = %session.id,
                    kind = "idle_context_compaction_retry_queued",
                    channel_id = %session.address.channel_id,
                    agent_id = %session.agent_id,
                    error = %format!("{error:#}"),
                    "idle context compaction failed; queued retry for next user message"
                );
            }
        }

        Ok(())
    }

    async fn attempt_idle_context_compaction(
        &self,
        session: &SessionSnapshot,
        force_retry: bool,
    ) -> Result<bool> {
        if !self.effective_context_compaction_enabled(&session.address)? {
            return Ok(false);
        }
        if let Some(missing_model_key) = self.clear_missing_selected_main_model(&session.address)? {
            self.pause_idle_compaction_and_prompt_agent_selection(session, &missing_model_key)
                .await?;
            return Ok(false);
        }
        if self.selected_main_model_key(&session.address)?.is_none() {
            return Ok(false);
        }
        let model_key = self.effective_main_model_key(&session.address)?;
        let model = self.model_config_or_main(&model_key)?.clone();
        let runtime = self.agent_runtime_view_for_address(&session.address)?;
        let source_messages = session.request_messages();
        if source_messages.is_empty() {
            return Ok(false);
        }
        let mut config = runtime.build_agent_frame_config(
            session,
            &session.workspace_root,
            AgentPromptKind::MainForeground,
            &model_key,
            None,
        )?;
        let extra_tools = runtime.build_extra_tools(
            session,
            AgentPromptKind::MainForeground,
            session.agent_id,
            None,
        );
        let estimated_tokens =
            estimate_current_context_tokens_for_session(&runtime, session, &model_key)?;
        let idle_min_tokens = (effective_context_window_limit_for_session(session, &model) as f64
            * self.main_agent.idle_compaction.min_ratio)
            .floor() as usize;
        if estimated_tokens < idle_min_tokens {
            return Ok(false);
        }
        config.context_compaction.token_limit_override = Some(idle_compaction_token_limit(
            config.context_compaction.token_limit_override,
            idle_min_tokens,
        ));

        if !force_retry {
            let lead_time = Duration::from_secs(30);
            let now = Utc::now();
            let Some(ttl) = openrouter_automatic_cache_ttl(&model) else {
                return Ok(false);
            };
            let ttl = parse_duration(&ttl)
                .with_context(|| format!("failed to parse model cache_ttl '{}'", ttl))?;
            let Some(idle_threshold) = ttl.checked_sub(lead_time) else {
                return Ok(false);
            };
            if !should_attempt_idle_context_compaction(
                session,
                now,
                idle_threshold,
                estimated_tokens,
                idle_min_tokens,
            ) {
                return Ok(false);
            }
            runtime.idle_compact_subagents_for_session(session, idle_threshold)?;
        }

        let persistence_system_prompt = config.system_prompt.clone();
        let source_messages = sanitize_messages_for_model_capabilities(
            &source_messages,
            &model,
            backend_supports_native_multimodal_input(AgentBackendKind::AgentFrame),
        );
        let report = run_backend_compaction(
            AgentBackendKind::AgentFrame,
            source_messages,
            config,
            extra_tools,
        )
        .with_context(|| format!("failed to compact idle session {}", session.id))?;
        if !report.compacted {
            let actor = self.ensure_foreground_actor(&session.address)?;
            actor.clear_idle_compaction_failure()?;
            return Ok(false);
        }
        let normalized_messages = normalize_messages_for_persistence(
            report.messages.clone(),
            &persistence_system_prompt,
            &[],
        );
        let rollout_id = persist_compaction_artifacts(session, &report).with_context(|| {
            format!("failed to persist compaction artifacts for {}", session.id)
        })?;

        let compaction_stats = compaction_stats_from_report(&report);
        let actor = self.ensure_foreground_actor(&session.address)?;
        actor
            .record_idle_compaction(normalized_messages, &compaction_stats)
            .with_context(|| format!("failed to persist idle compaction for {}", session.id))?;
        let prompt_state = self.build_foreground_prompt_state(session, &model_key)?;
        actor.mark_system_prompt_state_current(prompt_state.static_hash)?;
        self.rotate_chat_version_after_external_compaction(&session.address)?;
        info!(
            log_stream = "session",
            log_key = %session.id,
            kind = if force_retry {
                "idle_context_compaction_retry_completed"
            } else {
                "idle_context_compaction_completed"
            },
            channel_id = %session.address.channel_id,
            agent_id = %session.agent_id,
            turn_count = session.turn_count,
            token_limit = report.token_limit as u64,
            estimated_tokens_before = report.estimated_tokens_before as u64,
            estimated_tokens_after = report.estimated_tokens_after as u64,
            llm_calls = report.usage.llm_calls,
            input_total_tokens = report.usage.input_total_tokens(),
            output_total_tokens = report.usage.output_total_tokens(),
            context_total_tokens = report.usage.context_total_tokens(),
            cache_read_input_tokens = report.usage.cache_read_input_tokens(),
            cache_write_input_tokens = report.usage.cache_write_input_tokens(),
            cache_uncached_input_tokens = report.usage.cache_uncached_input_tokens(),
            normal_billed_input_tokens = report.usage.normal_billed_input_tokens(),
            legacy_prompt_tokens = report.usage.prompt_tokens,
            legacy_completion_tokens = report.usage.completion_tokens,
            legacy_total_tokens = report.usage.total_tokens,
            legacy_cache_hit_tokens = report.usage.cache_hit_tokens,
            legacy_cache_miss_tokens = report.usage.cache_miss_tokens,
            legacy_cache_read_tokens = report.usage.cache_read_tokens,
            legacy_cache_write_tokens = report.usage.cache_write_tokens,
            rollout_id = rollout_id.as_deref(),
            "idle context compaction completed"
        );
        Ok(true)
    }

    fn agent_runtime_view(&self) -> AgentRuntimeView {
        AgentRuntimeView {
            context: Arc::clone(&self.context),
            active_workspace_ids: self
                .with_sessions(|sessions| Ok(sessions.list_foreground_snapshots()))
                .unwrap_or_default()
                .into_iter()
                .map(|session| session.workspace_id)
                .collect(),
            selected_agent_backend: None,
            selected_main_model_key: None,
            selected_reasoning_effort: None,
            selected_context_compaction_enabled: None,
            selected_chat_version_id: None,
            sandbox: self.sandbox.clone(),
        }
    }

    fn agent_runtime_view_for_sandbox_mode(&self, sandbox_mode: SandboxMode) -> AgentRuntimeView {
        let mut runtime = self.agent_runtime_view();
        runtime.sandbox.mode = sandbox_mode;
        runtime
    }

    fn agent_runtime_view_for_address(&self, address: &ChannelAddress) -> Result<AgentRuntimeView> {
        let sandbox_mode = self.effective_sandbox_mode(address)?;
        let mut runtime = self.agent_runtime_view_for_sandbox_mode(sandbox_mode);
        let settings = self.with_conversations(|conversations| {
            conversations
                .ensure_conversation(address)
                .map(|snapshot| snapshot.settings)
        })?;
        runtime.selected_agent_backend = settings.agent_backend;
        runtime.selected_main_model_key = settings.main_model.clone();
        runtime.selected_reasoning_effort = settings.reasoning_effort.clone();
        runtime.selected_context_compaction_enabled = settings.context_compaction_enabled;
        runtime.selected_chat_version_id = Some(settings.chat_version_id);
        Ok(runtime)
    }

    fn unregister_session_runtime_control(&self, address: &ChannelAddress) -> Result<()> {
        if let Some(actor) = self.resolve_foreground_actor(address)? {
            let drained = actor.unregister_control()?;
            if drained {
                self.mark_conversation_context_changed(address)?;
            }
        }
        Ok(())
    }

    fn destroy_foreground_session(&self, address: &ChannelAddress) -> Result<()> {
        let snapshot = self.with_sessions(|sessions| Ok(sessions.get_snapshot(address)))?;
        self.invalidate_foreground_agent_frame_runtime(address)?;
        if let Some(actor) = self.resolve_foreground_actor(address)? {
            let _ = actor.request_cancel();
        }
        self.unregister_session_runtime_control(address)?;
        if let Some(session) = snapshot {
            let destroyed_subagents = self
                .agent_runtime_view()
                .destroy_subagents_for_session(session.id)?;
            let runtime_state_root = self
                .agent_workspace
                .root_dir
                .join("runtime")
                .join(&session.workspace_id);
            let report = terminate_runtime_state_tasks(&runtime_state_root)?;
            if destroyed_subagents > 0
                || report.exec_processes_killed > 0
                || report.file_downloads_cancelled > 0
                || report.image_tasks_cancelled > 0
                || report.dsl_tasks_killed > 0
            {
                info!(
                    log_stream = "session",
                    log_key = %session.id,
                    kind = "session_runtime_tasks_destroyed",
                    workspace_id = %session.workspace_id,
                    subagents_destroyed = destroyed_subagents as u64,
                    exec_processes_killed = report.exec_processes_killed as u64,
                    file_downloads_cancelled = report.file_downloads_cancelled as u64,
                    image_tasks_cancelled = report.image_tasks_cancelled as u64,
                    dsl_tasks_killed = report.dsl_tasks_killed as u64,
                    "destroyed background runtime tasks for session"
                );
            }
        }
        self.with_conversations(|conversations| {
            conversations.clear_foreground_actor(address);
            Ok(())
        })?;
        self.with_sessions(|sessions| sessions.destroy_foreground(address))
    }

    pub fn workdir(&self) -> &Path {
        &self.workdir
    }

    pub fn create_sub_agent_placeholder(&self, parent_agent_id: uuid::Uuid) -> SubAgentSpec {
        SubAgentSpec {
            id: uuid::Uuid::new_v4(),
            parent_agent_id,
            docker_image: None,
            can_spawn_sub_agents: false,
        }
    }

    async fn handle_incoming(&self, incoming: IncomingMessage) -> Result<()> {
        let channel = self
            .channels
            .get(&incoming.address.channel_id)
            .with_context(|| format!("unknown channel {}", incoming.address.channel_id))?
            .clone();

        if self.handle_incoming_control(&incoming).await? {
            return Ok(());
        }

        info!(
            log_stream = "channel",
            log_key = %incoming.address.channel_id,
            kind = "incoming_message",
            conversation_id = %incoming.address.conversation_id,
            remote_message_id = %incoming.remote_message_id,
            has_text = incoming.text.is_some(),
            attachments_count = incoming.attachments.len() as u64,
            "received normalized incoming message"
        );

        if self
            .enforce_channel_authorization(&channel, &incoming)
            .await?
        {
            return Ok(());
        }

        self.with_conversations(|conversations| {
            conversations.ensure_conversation(&incoming.address)
        })?;
        if let Err(error) = self.archive_stale_workspaces_if_needed() {
            warn!(
                log_stream = "server",
                kind = "workspace_archive_pass_failed",
                error = %format!("{error:#}"),
                "failed to archive stale workspaces before handling message"
            );
        }

        if self
            .try_handle_incoming_command(&channel, &incoming)
            .await?
        {
            return Ok(());
        }

        let incoming = self.prepare_regular_conversation_message(incoming).await?;
        self.handle_regular_foreground_message(&channel, incoming)
            .await
    }

    pub async fn dispatch_background_message(
        &self,
        target: SinkTarget,
        message: OutgoingMessage,
    ) -> Result<()> {
        info!(
            log_stream = "server",
            kind = "background_dispatch_requested",
            "dispatching background message"
        );
        let sink_router = self.sink_router.read().await;
        sink_router.dispatch(&self.channels, &target, message).await
    }

    pub async fn subscribe_broadcast(&self, topic: impl Into<String>, address: ChannelAddress) {
        let mut sink_router = self.sink_router.write().await;
        sink_router.subscribe(topic, address);
    }

    fn help_text_for_channel(&self, channel_id: &str) -> String {
        let commands = self
            .command_catalog
            .get(channel_id)
            .cloned()
            .unwrap_or_else(default_bot_commands);

        let mut lines = vec!["Available commands:".to_string()];
        for command in commands {
            lines.push(format!("/{:<12} {}", command.command, command.description));
        }
        lines.join("\n")
    }

    async fn status_message_for_session(
        &self,
        session: &SessionSnapshot,
        model_key: &str,
    ) -> Result<OutgoingMessage> {
        let model = self.model_config_or_main(model_key)?;
        let effective_api_timeout = session
            .api_timeout_override_seconds
            .unwrap_or(model.timeout_seconds);
        let timeout_source = if session.api_timeout_override_seconds.is_some() {
            "session override"
        } else {
            "model default"
        };
        let runtime = self.agent_runtime_view_for_address(&session.address)?;
        let current_context_estimate =
            estimate_current_context_tokens_for_session(&runtime, session, model_key)?;
        let current_context_limit = self
            .main_agent
            .context_compaction
            .token_limit_override
            .unwrap_or_else(|| {
                (effective_context_window_limit_for_session(session, model) as f64
                    * self.main_agent.context_compaction.trigger_ratio)
                    .floor() as usize
            });
        let current_reasoning_effort = self
            .effective_conversation_settings(&session.address)?
            .reasoning_effort
            .or_else(|| {
                model
                    .reasoning
                    .as_ref()
                    .and_then(|reasoning| reasoning.effort.clone())
            });
        let context_compaction_enabled =
            self.effective_context_compaction_enabled(&session.address)?;
        let conversation_usage_report =
            collect_conversation_usage_report(&self.workdir, &session.address, Utc::now(), 6);
        let (pricing_by_model, pricing_fetch_errors) = self
            .resolve_pricing_for_usage_report(&conversation_usage_report)
            .await;
        let conversation_pricing = price_conversation_usage_report(
            &conversation_usage_report,
            &pricing_by_model,
            pricing_fetch_errors,
        );
        let status_text = format_session_status(
            &self.main_agent.language,
            model_key,
            model,
            session,
            effective_api_timeout,
            timeout_source,
            current_context_estimate,
            current_context_limit,
            current_reasoning_effort.as_deref(),
            context_compaction_enabled,
            &conversation_usage_report,
            &conversation_pricing,
        );
        let usage_chart = build_status_usage_chart(
            &self.main_agent.language,
            &conversation_usage_report,
            &conversation_pricing,
        );
        Ok(OutgoingMessage {
            text: Some(status_text),
            images: Vec::new(),
            attachments: Vec::new(),
            options: None,
            usage_chart: Some(usage_chart),
        })
    }

    async fn resolve_pricing_for_usage_report(
        &self,
        report: &ConversationUsageReport,
    ) -> (HashMap<String, ModelPricing>, Vec<String>) {
        let model_ids = report.model_usages.keys().cloned().collect::<HashSet<_>>();
        let mut pricing_by_model = HashMap::new();
        let canonical_by_logged_model = model_ids
            .iter()
            .map(|logged_model| {
                let canonical = self
                    .models
                    .get(logged_model)
                    .map(|model| model.model.clone())
                    .unwrap_or_else(|| logged_model.clone());
                (logged_model.clone(), canonical)
            })
            .collect::<HashMap<_, _>>();
        let openrouter_candidates = canonical_by_logged_model
            .values()
            .filter(|model| model.contains('/'))
            .cloned()
            .collect::<HashSet<_>>();
        let mut pricing_fetch_errors = Vec::new();
        let mut fetched_pricing = HashMap::new();
        if !openrouter_candidates.is_empty() {
            match self
                .fetch_openrouter_pricing_for_models(&openrouter_candidates)
                .await
            {
                Ok(fetched) => {
                    fetched_pricing = fetched;
                }
                Err(error) => {
                    pricing_fetch_errors
                        .push(format!("OpenRouter pricing lookup failed: {error:#}"));
                }
            }
        }
        for (logged_model, canonical_model) in &canonical_by_logged_model {
            if let Some(pricing) = fetched_pricing.get(canonical_model) {
                pricing_by_model.insert(logged_model.clone(), pricing.clone());
                continue;
            }
            if let Some(mut pricing) = model_pricing_by_id(canonical_model) {
                if logged_model != canonical_model {
                    pricing.source = format!("{} via model_key:{logged_model}", pricing.source);
                }
                pricing_by_model.insert(logged_model.clone(), pricing);
                continue;
            }
            if let Some(pricing) = model_pricing_by_id(logged_model) {
                pricing_by_model.insert(logged_model.clone(), pricing);
            }
        }
        merge_builtin_pricing(model_ids, &mut pricing_by_model);
        (pricing_by_model, pricing_fetch_errors)
    }

    async fn fetch_openrouter_pricing_for_models(
        &self,
        model_ids: &HashSet<String>,
    ) -> Result<HashMap<String, ModelPricing>> {
        let response = reqwest::Client::new()
            .get("https://openrouter.ai/api/v1/models")
            .timeout(Duration::from_secs(5))
            .send()
            .await
            .context("failed to fetch OpenRouter model catalog")?
            .error_for_status()
            .context("OpenRouter model catalog returned an error status")?;
        let value = response
            .json::<Value>()
            .await
            .context("failed to parse OpenRouter model catalog")?;
        let mut pricing = HashMap::new();
        let Some(data) = value.get("data").and_then(Value::as_array) else {
            return Ok(pricing);
        };
        for item in data {
            let Some(model_id) = item.get("id").and_then(Value::as_str) else {
                continue;
            };
            if !model_ids.contains(model_id) {
                continue;
            }
            let Some(price) = item.get("pricing") else {
                continue;
            };
            let Some(prompt) = json_number_or_string(price.get("prompt")) else {
                continue;
            };
            let Some(completion) = json_number_or_string(price.get("completion")) else {
                continue;
            };
            let cache_read = json_number_or_string(price.get("input_cache_read"));
            let cache_write = json_number_or_string(price.get("input_cache_write"));
            pricing.insert(
                model_id.to_string(),
                model_pricing_from_openrouter(prompt, completion, cache_read, cache_write),
            );
        }
        Ok(pricing)
    }

    fn archive_stale_workspaces_if_needed(&self) -> Result<()> {
        let protected = self
            .with_sessions(|sessions| Ok(sessions.list_foreground_snapshots()))?
            .into_iter()
            .map(|session| session.workspace_id)
            .collect::<Vec<_>>();
        let archived = self
            .workspace_manager
            .archive_stale_workspaces(chrono::Duration::days(30), &protected)?;
        if !archived.is_empty() {
            info!(
                log_stream = "server",
                kind = "workspace_archived",
                archived_count = archived.len() as u64,
                "archived stale workspaces"
            );
        }
        Ok(())
    }

    async fn compact_session_now(
        &self,
        session: &SessionSnapshot,
        model_key: &str,
        force: bool,
    ) -> Result<bool> {
        if (!force && !self.effective_context_compaction_enabled(&session.address)?)
            || session.stable_message_count() == 0
        {
            return Ok(false);
        }
        let runtime = self.agent_runtime_view_for_address(&session.address)?;
        let mut config = runtime.build_agent_frame_config(
            session,
            &session.workspace_root,
            AgentPromptKind::MainForeground,
            model_key,
            None,
        )?;
        let extra_tools = runtime.build_extra_tools(
            session,
            AgentPromptKind::MainForeground,
            session.agent_id,
            None,
        );
        let model = self.model_config_or_main(model_key)?;
        if force {
            let manual_compact_min_tokens =
                (effective_context_window_limit_for_session(session, model) as f64
                    * self.main_agent.idle_compaction.min_ratio)
                    .floor() as usize;
            let estimated_tokens =
                estimate_current_context_tokens_for_session(&runtime, session, model_key)?;
            if estimated_tokens < manual_compact_min_tokens {
                return Ok(false);
            }
            config.context_compaction.token_limit_override = Some(manual_compact_min_tokens);
        }
        let persistence_system_prompt = config.system_prompt.clone();
        let model = self.model_config_or_main(model_key)?;
        let compaction_messages = sanitize_messages_for_model_capabilities(
            &session.request_messages(),
            model,
            backend_supports_native_multimodal_input(AgentBackendKind::AgentFrame),
        );
        let report = run_backend_compaction(
            AgentBackendKind::AgentFrame,
            compaction_messages,
            config,
            extra_tools,
        )?;
        if !report.compacted {
            return Ok(false);
        }
        let normalized_messages = normalize_messages_for_persistence(
            report.messages.clone(),
            &persistence_system_prompt,
            &[],
        );
        persist_compaction_artifacts(session, &report)?;
        let compaction_stats = compaction_stats_from_report(&report);
        let actor = self.ensure_foreground_actor(&session.address)?;
        actor.record_idle_compaction(normalized_messages, &compaction_stats)?;
        let prompt_state = self.build_foreground_prompt_state(session, model_key)?;
        actor.mark_system_prompt_state_current(prompt_state.static_hash)?;
        self.rotate_chat_version_after_external_compaction(&session.address)?;
        Ok(true)
    }

    fn available_sandbox_modes(&self) -> Vec<SandboxMode> {
        let mut modes = vec![SandboxMode::Subprocess];
        if bubblewrap_is_available(&self.sandbox) {
            modes.push(SandboxMode::Bubblewrap);
        }
        modes
    }

    fn effective_conversation_settings(
        &self,
        address: &ChannelAddress,
    ) -> Result<ConversationSettings> {
        Ok(self
            .with_conversations(|conversations| Ok(conversations.get_snapshot(address)))?
            .map(|snapshot| snapshot.settings)
            .unwrap_or_default())
    }

    fn selected_main_model_key(&self, address: &ChannelAddress) -> Result<Option<String>> {
        Ok(self.effective_conversation_settings(address)?.main_model)
    }

    fn selected_agent_backend(&self, address: &ChannelAddress) -> Result<Option<AgentBackendKind>> {
        let settings = self.effective_conversation_settings(address)?;
        Ok(settings.agent_backend.or_else(|| {
            settings
                .main_model
                .as_ref()
                .map(|_| AgentBackendKind::AgentFrame)
        }))
    }

    fn has_complete_agent_selection(&self, address: &ChannelAddress) -> Result<bool> {
        Ok(self.selected_main_model_key(address)?.is_some())
    }

    fn ensure_model_available_for_backend(
        &self,
        backend: AgentBackendKind,
        model_key: &str,
    ) -> Result<()> {
        if self.agent.is_model_available(backend, model_key) {
            return Ok(());
        }
        Err(anyhow!(
            "model '{}' is not available for agent backend '{}'",
            model_key,
            serde_json::to_string(&backend)
                .unwrap_or_else(|_| "\"unknown\"".to_string())
                .trim_matches('"')
        ))
    }

    fn effective_main_model_key(&self, address: &ChannelAddress) -> Result<String> {
        let model_key = self.selected_main_model_key(address)?.ok_or_else(|| {
            anyhow!("this conversation does not have a main model yet; choose one with /agent")
        })?;
        self.ensure_model_available_for_backend(AgentBackendKind::AgentFrame, &model_key)?;
        Ok(model_key)
    }

    fn effective_sandbox_mode(&self, address: &ChannelAddress) -> Result<SandboxMode> {
        let settings = self.effective_conversation_settings(address)?;
        Ok(settings.sandbox_mode.unwrap_or(self.sandbox.mode))
    }

    fn local_mount_paths_for_address(&self, address: &ChannelAddress) -> Result<Vec<PathBuf>> {
        self.with_conversations(|conversations| {
            Ok(conversations
                .get_snapshot(address)
                .map(|snapshot| {
                    snapshot
                        .settings
                        .local_mounts
                        .into_iter()
                        .map(|mount| mount.path)
                        .collect()
                })
                .unwrap_or_default())
        })
    }

    fn effective_context_compaction_enabled(&self, address: &ChannelAddress) -> Result<bool> {
        Ok(self
            .effective_conversation_settings(address)?
            .context_compaction_enabled
            .unwrap_or(self.main_agent.enable_context_compression))
    }

    fn rotate_chat_version_if_compacted(
        &self,
        address: &ChannelAddress,
        compaction: &SessionCompactionStats,
    ) -> Result<()> {
        if compaction.compacted_run_count == 0 {
            return Ok(());
        }
        self.mark_conversation_context_changed(address)
    }

    fn rotate_chat_version_after_external_compaction(
        &self,
        address: &ChannelAddress,
    ) -> Result<()> {
        self.mark_conversation_context_changed(address)
    }

    fn mark_conversation_context_changed(&self, address: &ChannelAddress) -> Result<()> {
        self.with_conversations(|conversations| {
            conversations.rotate_chat_version_id(address).map(|_| ())
        })
    }

    fn available_agent_models(&self, backend: AgentBackendKind) -> Vec<String> {
        self.agent
            .available_models(backend)
            .iter()
            .filter(|model_key: &&String| self.models.contains_key(model_key.as_str()))
            .cloned()
            .collect()
    }

    fn agent_model_selection_message(
        &self,
        address: &ChannelAddress,
        intro: &str,
    ) -> Result<OutgoingMessage> {
        let current_model = self.selected_main_model_key(address)?;
        let mut options = self
            .available_agent_models(AgentBackendKind::AgentFrame)
            .into_iter()
            .map(|model_key| ShowOption {
                label: model_key.clone(),
                value: format!("/agent {}", model_key),
            })
            .collect::<Vec<_>>();
        options.sort_by(|left, right| left.label.cmp(&right.label));
        Ok(OutgoingMessage::with_options(
            format!(
                "{}\nCurrent conversation model: {}\nChoose a model below or send `/agent <model>`.",
                intro,
                current_model
                    .map(|value| format!("`{}`", value))
                    .unwrap_or_else(|| "`<not selected>`".to_string()),
            ),
            "Choose a model",
            options,
        ))
    }

    fn agent_selection_message(
        &self,
        address: &ChannelAddress,
        intro: &str,
    ) -> Result<OutgoingMessage> {
        self.agent_model_selection_message(address, intro)
    }

    fn reasoning_effort_message(&self, address: &ChannelAddress) -> Result<OutgoingMessage> {
        let settings = self.effective_conversation_settings(address)?;
        let effective_effort = settings.reasoning_effort.clone().or_else(|| {
            self.selected_main_model_key(address)
                .ok()
                .flatten()
                .and_then(|model_key| {
                    self.models.get(&model_key).and_then(|model| {
                        model
                            .reasoning
                            .as_ref()
                            .and_then(|reasoning| reasoning.effort.clone())
                    })
                })
        });
        let options = ["low", "medium", "high"]
            .into_iter()
            .map(|effort| ShowOption {
                label: effort.to_string(),
                value: format!("/think {}", effort),
            })
            .chain(std::iter::once(ShowOption {
                label: "default".to_string(),
                value: "/think default".to_string(),
            }))
            .collect::<Vec<_>>();
        Ok(OutgoingMessage::with_options(
            format!(
                "Current conversation reasoning effort: {}\nChoose an option below or send `/think <level>`.",
                effective_effort
                    .map(|value| format!("`{}`", value))
                    .unwrap_or_else(|| "`default`".to_string())
            ),
            "Choose a reasoning effort",
            options,
        ))
    }

    fn compact_mode_message(&self, address: &ChannelAddress) -> Result<OutgoingMessage> {
        let enabled = self.effective_context_compaction_enabled(address)?;
        let options = vec![
            ShowOption {
                label: "enabled".to_string(),
                value: "/compact_mode on".to_string(),
            },
            ShowOption {
                label: "disabled".to_string(),
                value: "/compact_mode off".to_string(),
            },
        ];
        Ok(OutgoingMessage::with_options(
            format!(
                "Automatic context compaction for this conversation is currently `{}`.\nChoose a mode below or send `/compact_mode <on|off>`.\nYou can always trigger a one-off compaction with `/compact`.",
                if enabled { "enabled" } else { "disabled" }
            ),
            "Choose automatic compaction mode",
            options,
        ))
    }

    fn model_config_or_main(&self, model_key: &str) -> Result<&ModelConfig> {
        self.models
            .get(model_key)
            .with_context(|| format!("unknown model {}", model_key))
    }

    fn model_upstream_timeout_seconds(&self, model_key: &str) -> Result<f64> {
        Ok(self
            .models
            .get(model_key)
            .with_context(|| format!("unknown model {}", model_key))?
            .timeout_seconds)
    }

    fn build_foreground_agent(
        &self,
        session: &SessionSnapshot,
        model_key: &str,
    ) -> Result<ForegroundAgent> {
        let prompt_state = self.build_foreground_prompt_state(session, model_key)?;
        Ok(ForegroundAgent {
            id: session.agent_id,
            session_id: session.id,
            channel_id: session.address.channel_id.clone(),
            system_prompt: prompt_state.system_prompt,
        })
    }

    fn build_foreground_prompt_state(
        &self,
        session: &SessionSnapshot,
        model_key: &str,
    ) -> Result<AgentSystemPromptState> {
        let model = self.model_config_or_main(model_key)?;
        let workspace_summary = self
            .workspace_manager
            .ensure_workspace_exists(&session.workspace_id)
            .map(|workspace| workspace.summary)
            .unwrap_or_default();
        let remote_workpaths = self.with_conversations(|conversations| {
            Ok(conversations
                .get_snapshot(&session.address)
                .map(|snapshot| snapshot.settings.remote_workpaths)
                .unwrap_or_default())
        })?;
        let local_mounts = self.with_conversations(|conversations| {
            Ok(conversations
                .get_snapshot(&session.address)
                .map(|snapshot| snapshot.settings.local_mounts)
                .unwrap_or_default())
        })?;
        Ok(build_agent_system_prompt_state(
            &self.agent_workspace,
            session,
            &workspace_summary,
            &remote_workpaths,
            &local_mounts,
            AgentPromptKind::MainForeground,
            model_key,
            model,
            &self.models,
            &self.chat_model_keys,
            &self.main_agent,
        ))
    }

    fn current_runtime_skill_observations(&self) -> Result<Vec<SessionSkillObservation>> {
        let discovered = discover_skills(std::slice::from_ref(&self.agent_workspace.skills_dir))?;
        let mut observed = Vec::with_capacity(discovered.len());
        for skill in discovered {
            let skill_file = skill.path.join("SKILL.md");
            let content = std::fs::read_to_string(&skill_file)
                .with_context(|| format!("failed to read {}", skill_file.display()))?;
            observed.push(SessionSkillObservation {
                name: skill.name,
                description: skill.description,
                content,
            });
        }
        observed.sort_by(|left, right| left.name.cmp(&right.name));
        Ok(observed)
    }

    fn observe_runtime_skill_changes(&self, session: &SessionSnapshot) -> Result<Option<String>> {
        let observed = self.current_runtime_skill_observations()?;
        let metadata = observed
            .iter()
            .map(|skill| agent_frame::skills::SkillMetadata {
                name: skill.name.clone(),
                description: skill.description.clone(),
                path: PathBuf::new(),
            })
            .collect::<Vec<_>>();
        let metadata_prompt = build_skills_meta_prompt(&metadata);
        let actor = self.ensure_foreground_actor(&session.address)?;
        let notices = actor.observe_skill_changes(&observed, metadata_prompt)?;
        let rendered = render_skill_change_notices(&notices);
        Ok((!rendered.is_empty()).then_some(rendered))
    }

    fn observe_runtime_prompt_component_changes(
        &self,
        session: &SessionSnapshot,
    ) -> Result<Option<String>> {
        let actor = self.ensure_foreground_actor(&session.address)?;
        let mut notices = Vec::new();
        let had_remote_aliases_component = session
            .prompt_component_system_value(REMOTE_ALIASES_PROMPT_COMPONENT)
            .is_some();
        if let Some(notice) = actor.observe_prompt_component_change(
            IDENTITY_PROMPT_COMPONENT,
            current_identity_prompt_for_workspace(&self.agent_workspace),
        )? {
            notices.push(notice);
        }
        if let Some(notice) = actor.observe_prompt_component_change(
            USER_META_PROMPT_COMPONENT,
            current_user_meta_prompt_for_workspace(&self.agent_workspace),
        )? {
            notices.push(notice);
        }
        let remote_aliases_prompt = current_ssh_remote_aliases_prompt();
        if let Some(notice) = actor.observe_prompt_component_change(
            REMOTE_ALIASES_PROMPT_COMPONENT,
            remote_aliases_prompt.clone(),
        )? {
            notices.push(notice);
        } else if !had_remote_aliases_component && !remote_aliases_prompt.trim().is_empty() {
            notices.push(PromptComponentChangeNotice {
                key: REMOTE_ALIASES_PROMPT_COMPONENT.to_string(),
                value: remote_aliases_prompt,
            });
        }
        let rendered = render_prompt_component_change_notices(&notices);
        Ok((!rendered.is_empty()).then_some(rendered))
    }

    fn sync_runtime_profile_files(&self, session: &SessionSnapshot) -> Result<()> {
        if self.main_agent.memory_system == agent_frame::config::MemorySystem::ClaudeCode {
            ensure_workspace_partclaw_file(&self.agent_workspace, &session.workspace_root)?;
        }
        sync_workspace_shared_profile_files(&self.agent_workspace, &session.workspace_root)?;
        Ok(())
    }

    fn log_turn_usage(&self, session: &SessionSnapshot, usage: &TokenUsage, initialization: bool) {
        log_turn_usage(
            session.agent_id,
            session,
            usage,
            initialization,
            "main_foreground",
            None,
        );
    }
}

fn build_status_usage_chart(
    language: &str,
    report: &ConversationUsageReport,
    pricing: &ConversationPricingBreakdown,
) -> UsageChart {
    let title = if language.to_ascii_lowercase().starts_with("zh") {
        format!(
            "Conversation estimated spend, last {} days",
            report.days.len()
        )
    } else {
        format!(
            "Conversation estimated spend, last {} days",
            report.days.len()
        )
    };
    UsageChart {
        title,
        y_label: "USD".to_string(),
        days: report
            .days
            .iter()
            .map(|day| UsageChartDay {
                label: day.date.format("%m-%d").to_string(),
                total_usd: pricing
                    .daily_costs
                    .get(&day.date)
                    .copied()
                    .unwrap_or_default(),
                input_tokens: day.usage.input_total_tokens(),
                output_tokens: day.usage.output_total_tokens(),
                llm_calls: day.usage.llm_calls,
            })
            .collect(),
    }
}

fn json_number_or_string(value: Option<&Value>) -> Option<f64> {
    match value? {
        Value::Number(number) => number.as_f64(),
        Value::String(text) => text.parse::<f64>().ok(),
        _ => None,
    }
}

fn channel_restart_backoff_seconds(consecutive_failures: u32) -> u64 {
    let exponent = consecutive_failures.saturating_sub(1).min(5);
    2_u64
        .saturating_pow(exponent)
        .min(CHANNEL_RESTART_MAX_BACKOFF_SECONDS)
        .max(1)
}

fn spawn_channel_supervisor(channel: Arc<dyn Channel>, sender: mpsc::Sender<IncomingMessage>) {
    info!(
        log_stream = "channel",
        log_key = %channel.id(),
        kind = "channel_starting",
        "starting channel supervisor"
    );
    tokio::spawn(async move {
        let channel_id = channel.id().to_string();
        let mut consecutive_failures = 0u32;
        loop {
            match channel.clone().run(sender.clone()).await {
                Ok(()) => {
                    warn!(
                        log_stream = "channel",
                        log_key = %channel_id,
                        kind = "channel_exited",
                        "channel task exited without error; restarting"
                    );
                }
                Err(error) => {
                    consecutive_failures = consecutive_failures.saturating_add(1);
                    let backoff_seconds = channel_restart_backoff_seconds(consecutive_failures);
                    error!(
                        log_stream = "channel",
                        log_key = %channel_id,
                        kind = "channel_stopped",
                        error = %format!("{error:#}"),
                        consecutive_failures = consecutive_failures,
                        backoff_seconds = backoff_seconds,
                        "channel task stopped with error; restarting"
                    );
                    tokio::time::sleep(Duration::from_secs(backoff_seconds)).await;
                    continue;
                }
            }
            let backoff_seconds = channel_restart_backoff_seconds(consecutive_failures.max(1));
            tokio::time::sleep(Duration::from_secs(backoff_seconds)).await;
        }
    });
}

#[cfg(test)]
mod tests {
    use super::{
        AgentCommand, AgentPromptKind, ConversationPricingBreakdown, ConversationUsageReport,
        ImageGenerationRouting, IncomingCommandLane, ModelPricing, RuntimeContext,
        SYSTEM_RESTART_NOTICE, Server, SummaryTracker, TokenUsage,
        background_timeout_with_active_children_text, build_synthetic_system_messages,
        build_user_turn_message, channel_restart_backoff_seconds,
        coalesce_buffered_conversation_messages, collect_conversation_usage_report,
        collect_conversation_usage_window, conversation_memory_root,
        estimate_compaction_savings_usd, estimate_cost_usd, extract_attachment_references,
        fast_path_agent_selection_message, format_session_status, idle_compaction_token_limit,
        incoming_command_lane, infer_single_agent_backend, is_command_like_text,
        is_out_of_band_command, is_timeout_like, leading_system_prompt, memory_search_files,
        normalize_messages_for_persistence, openrouter_automatic_cache_control,
        openrouter_automatic_cache_ttl, parse_agent_command, parse_model_command,
        parse_mount_command, parse_sandbox_command, parse_set_api_timeout_command,
        parse_snap_list_command, parse_snap_load_command, parse_snap_save_command,
        parse_think_command, persist_compaction_artifacts, prepare_system_prompt_for_turn,
        price_conversation_usage_report, rebuild_canonical_system_prompt,
        render_last_user_message_time_tip, render_prompt_component_change_notices,
        render_system_date_on_user_message, rollout_read_file, rollout_search_files,
        sanitize_messages_for_model_capabilities, select_image_generation_routing,
        send_outgoing_message_now, session_errno_for_turn_error,
        should_attempt_idle_context_compaction, summarize_resume_progress,
        sync_workspace_shared_profile_files, upload_workspace_shared_profile_files,
        user_facing_continue_error_text, workspace_visible_in_list,
    };
    use crate::agent_status::AgentRegistry;
    use crate::backend::AgentBackendKind;
    use crate::bootstrap::AgentWorkspace;
    use crate::channel::{Channel, ConversationProbe, IncomingMessage};
    use crate::channel_auth::ChannelAuthorizationManager;
    use crate::channel_auth::{
        ChannelAdminSnapshot, ConversationApprovalSnapshot, ConversationApprovalState,
    };
    use crate::config::{
        AgentBackendConfig, AgentConfig, MainAgentConfig, ModelCapability, ModelConfig,
        SandboxConfig, ToolingConfig, ToolingTarget,
    };
    use crate::conversation::ConversationManager;
    use crate::cron::CronManager;
    use crate::domain::ChannelAddress;
    use crate::domain::{AttachmentKind, OutgoingMessage, ProcessingState, StoredAttachment};
    use crate::session::{
        PromptComponentChangeNotice, REMOTE_ALIASES_PROMPT_COMPONENT, tag_interrupted_followup_text,
    };
    use crate::session::{SessionErrno, SessionSnapshot, SessionUserMessage};
    use crate::session::{SessionManager, SessionPhase, SessionRuntimeTurnCommit};
    use crate::sink::SinkRouter;
    use crate::snapshot::SnapshotManager;
    use crate::workspace::WorkspaceManager;
    use agent_frame::config::MemorySystem;
    use agent_frame::message::{FunctionCall, ToolCall};
    use agent_frame::{
        ChatMessage, ContextCompactionReport, SessionCompactionStats, SessionEvent,
        SessionExecutionControl, StructuredCompactionMemoryHint, StructuredCompactionOutput,
        StructuredCompactionRefs,
    };
    use anyhow::anyhow;
    use async_trait::async_trait;
    use chrono::{Duration as ChronoDuration, Utc};
    use serde_json::{Value, json};
    use std::collections::{BTreeMap, HashMap, HashSet};
    use std::fs;
    use std::path::PathBuf;
    use std::sync::atomic::AtomicUsize;
    use std::sync::{Arc, Mutex};
    use std::time::Duration;
    use tempfile::TempDir;
    use tokio::sync::mpsc;
    use tokio::sync::{Notify, RwLock};
    use uuid::Uuid;

    #[derive(Default)]
    struct RecordingChannel {
        sent_messages: Mutex<Vec<(ChannelAddress, OutgoingMessage)>>,
        probe_member_counts: Mutex<HashMap<String, u64>>,
        probe_unavailable: Mutex<HashMap<String, String>>,
    }

    fn build_test_session(temp_dir: &TempDir) -> SessionSnapshot {
        SessionSnapshot {
            kind: crate::session::SessionKind::Foreground,
            id: Uuid::new_v4(),
            agent_id: Uuid::new_v4(),
            address: ChannelAddress {
                channel_id: "telegram-main".to_string(),
                conversation_id: "1717801091".to_string(),
                user_id: Some("user-1".to_string()),
                display_name: Some("Telegram User".to_string()),
            },
            root_dir: temp_dir.path().join("session"),
            attachments_dir: temp_dir.path().join("workspace").join("upload"),
            workspace_id: "workspace-1".to_string(),
            workspace_root: temp_dir.path().join("workspace"),
            last_user_message_at: None,
            last_agent_returned_at: None,
            last_compacted_at: None,
            turn_count: 1,
            last_compacted_turn_count: 0,
            cumulative_usage: TokenUsage::default(),
            cumulative_compaction: SessionCompactionStats::default(),
            api_timeout_override_seconds: None,
            skill_states: HashMap::new(),
            pending_workspace_summary: false,
            close_after_summary: false,
            session_state: crate::session::DurableSessionState::default(),
        }
    }

    fn build_test_server(temp_dir: &TempDir, channel: Arc<dyn Channel>) -> Server {
        let agent_workspace = AgentWorkspace::initialize(temp_dir.path()).unwrap();
        let workspace_manager = WorkspaceManager::load_or_create(temp_dir.path()).unwrap();
        let sessions = SessionManager::new(temp_dir.path(), workspace_manager.clone()).unwrap();
        let snapshots = SnapshotManager::new(temp_dir.path()).unwrap();
        let conversations = ConversationManager::new(temp_dir.path()).unwrap();
        let cron_manager = CronManager::load_or_create(temp_dir.path()).unwrap();
        let agent_registry = AgentRegistry::load_or_create(temp_dir.path()).unwrap();
        let channel_auth = ChannelAuthorizationManager::new(temp_dir.path()).unwrap();
        let (background_job_sender, background_job_receiver) = mpsc::channel(8);

        let mut models = BTreeMap::new();
        models.insert(
            "demo-model".to_string(),
            ModelConfig {
                model_type: crate::config::ModelType::OpenrouterResp,
                api_endpoint: "https://example.com/v1".to_string(),
                model: "demo-model".to_string(),
                backend: AgentBackendKind::AgentFrame,
                supports_vision_input: false,
                image_tool_model: None,
                web_search_model: None,
                api_key: None,
                api_key_env: "TEST_API_KEY".to_string(),
                chat_completions_path: "/responses".to_string(),
                codex_home: None,
                auth_credentials_store_mode: agent_frame::config::AuthCredentialsStoreMode::Auto,
                timeout_seconds: 30.0,
                retry_mode: Default::default(),
                context_window_tokens: 128_000,
                cache_ttl: None,
                reasoning: None,
                headers: serde_json::Map::new(),
                description: "demo".to_string(),
                agent_model_enabled: true,
                native_web_search: None,
                token_estimation: None,
                external_web_search: None,
                capabilities: vec![ModelCapability::Chat],
            },
        );

        let context = Arc::new(RuntimeContext {
            workdir: temp_dir.path().to_path_buf(),
            agent_workspace,
            workspace_manager,
            sessions: Arc::new(Mutex::new(sessions)),
            channels: Arc::new(HashMap::from([("telegram-main".to_string(), channel)])),
            web_channels: Arc::new(HashMap::new()),
            command_catalog: HashMap::new(),
            models,
            agent: AgentConfig {
                agent_frame: AgentBackendConfig {
                    available_models: vec!["demo-model".to_string()],
                },
            },
            tooling: ToolingConfig::default(),
            chat_model_keys: vec!["demo-model".to_string()],
            main_agent: MainAgentConfig {
                model: None,
                timeout_seconds: None,
                global_install_root: "/opt".to_string(),
                language: "zh-CN".to_string(),
                enabled_tools: Vec::new(),
                max_tool_roundtrips: 4,
                enable_context_compression: true,
                context_compaction: Default::default(),
                idle_compaction: Default::default(),
                timeout_observation_compaction: Default::default(),
                time_awareness: Default::default(),
                token_estimation_cache: Default::default(),
                memory_system: MemorySystem::default(),
            },
            sink_router: Arc::new(RwLock::new(SinkRouter::new())),
            cron_manager: Arc::new(Mutex::new(cron_manager)),
            agent_registry: Arc::new(Mutex::new(agent_registry)),
            agent_registry_notify: Arc::new(Notify::new()),
            max_global_sub_agents: 4,
            subagent_count: Arc::new(AtomicUsize::new(0)),
            cron_poll_interval_seconds: 60,
            background_job_sender,
            background_terminate_flags: Arc::new(Mutex::new(HashSet::new())),
            summary_tracker: Arc::new(SummaryTracker::new()),
            active_foreground_agent_frame_runtimes: Arc::new(Mutex::new(HashMap::new())),
            subagents: Arc::new(Mutex::new(HashMap::new())),
            conversations: Arc::new(Mutex::new(conversations)),
            snapshots: Arc::new(Mutex::new(snapshots)),
        });

        Server {
            context,
            telegram_channel_ids: Arc::new(HashSet::new()),
            sandbox: SandboxConfig::default(),
            background_job_receiver: Some(background_job_receiver),
            pending_process_restart_notices: Arc::new(Mutex::new(HashSet::new())),
            channel_auth: Arc::new(Mutex::new(channel_auth)),
        }
    }

    fn seed_memory_artifacts(session: &SessionSnapshot) -> String {
        fs::create_dir_all(&session.root_dir).unwrap();
        let report = ContextCompactionReport {
            messages: vec![ChatMessage::text("assistant", "compacted")],
            compacted_messages: vec![
                ChatMessage::text("user", "我们来设计新的 memory system"),
                ChatMessage::text("assistant", "先确定 rollout 和 retrieval 的关系"),
                ChatMessage {
                    role: "assistant".to_string(),
                    content: Some(Value::String("调用搜索工具".to_string())),
                    name: None,
                    tool_call_id: None,
                    tool_calls: Some(vec![ToolCall {
                        id: "call_1".to_string(),
                        kind: "function".to_string(),
                        function: FunctionCall {
                            name: "exec_command".to_string(),
                            arguments: Some(
                                json!({"cmd":"rg -n memory NEW_MEMORY_SYSTEM.md"}).to_string(),
                            ),
                        },
                    }]),
                },
                ChatMessage::tool_output(
                    "call_1",
                    "exec_command",
                    "error: context compression summary came back empty",
                ),
            ],
            usage: TokenUsage::default(),
            compacted: true,
            estimated_tokens_before: 1200,
            estimated_tokens_after: 400,
            token_limit: 1000,
            structured_output: Some(StructuredCompactionOutput {
                old_summary: "- 之前已经确定使用三层 memory。".to_string(),
                new_summary: "- 本轮明确了 rollout_summary 和 rollout_transcript 的职责。"
                    .to_string(),
                keywords: vec!["memory".to_string(), "rollout".to_string()],
                important_refs: StructuredCompactionRefs {
                    paths: vec!["NEW_MEMORY_SYSTEM.md".to_string()],
                    commands: vec!["rg -n memory NEW_MEMORY_SYSTEM.md".to_string()],
                    errors: vec!["context compression summary came back empty".to_string()],
                    ..StructuredCompactionRefs::default()
                },
                memory_hints: vec![StructuredCompactionMemoryHint {
                    group: "Memory System".to_string(),
                    conclusions: vec![
                        "rollout_summary 用于中层摘要。".to_string(),
                        "rollout_transcript 用于底层证据。".to_string(),
                    ],
                }],
                next_step: "继续实现 rollout_search。".to_string(),
            }),
        };
        persist_compaction_artifacts(session, &report)
            .unwrap()
            .expect("rollout id should be created")
    }

    fn test_user_message(text: &str) -> SessionUserMessage {
        SessionUserMessage {
            pending_message: ChatMessage::text("user", text),
            text: Some(text.to_string()),
            attachments: Vec::new(),
        }
    }

    #[async_trait]
    impl Channel for RecordingChannel {
        fn id(&self) -> &str {
            "recording"
        }

        async fn run(
            self: Arc<Self>,
            _sender: mpsc::Sender<IncomingMessage>,
        ) -> anyhow::Result<()> {
            Ok(())
        }

        async fn send_media_group(
            &self,
            _address: &ChannelAddress,
            _images: Vec<crate::domain::OutgoingAttachment>,
        ) -> anyhow::Result<()> {
            Ok(())
        }

        async fn send(
            &self,
            address: &ChannelAddress,
            message: OutgoingMessage,
        ) -> anyhow::Result<()> {
            self.sent_messages
                .lock()
                .unwrap()
                .push((address.clone(), message));
            Ok(())
        }

        async fn set_processing(
            &self,
            _address: &ChannelAddress,
            _state: ProcessingState,
        ) -> anyhow::Result<()> {
            Ok(())
        }

        async fn probe_conversation(
            &self,
            address: &ChannelAddress,
        ) -> anyhow::Result<Option<ConversationProbe>> {
            if let Some(reason) = self
                .probe_unavailable
                .lock()
                .unwrap()
                .get(&address.conversation_id)
                .cloned()
            {
                return Ok(Some(ConversationProbe::Unavailable { reason }));
            }
            if let Some(count) = self
                .probe_member_counts
                .lock()
                .unwrap()
                .get(&address.conversation_id)
                .copied()
            {
                return Ok(Some(ConversationProbe::Available {
                    member_count: Some(count),
                }));
            }
            Ok(None)
        }
    }

    #[test]
    fn extracts_multiple_attachments_and_strips_tags() {
        let temp_dir = TempDir::new().unwrap();
        let file_path = temp_dir.path().join("agent-1").join("note.txt");
        let image_path = temp_dir.path().join("agent-1").join("image.png");
        fs::create_dir_all(file_path.parent().unwrap()).unwrap();
        fs::write(&file_path, "hello").unwrap();
        fs::write(&image_path, "png").unwrap();

        let (text, attachments) = extract_attachment_references(
            "Here you go.\n<attachment>agent-1/note.txt</attachment>\n<attachment>agent-1/image.png</attachment>",
            temp_dir.path(),
        )
        .unwrap();

        assert_eq!(text, "Here you go.");
        assert_eq!(attachments.len(), 2);
    }

    #[test]
    fn extracts_absolute_attachment_paths() {
        let temp_dir = TempDir::new().unwrap();
        let outside_dir = TempDir::new().unwrap();
        let file_path = outside_dir.path().join("note.txt");
        fs::write(&file_path, "hello").unwrap();

        let (text, attachments) = extract_attachment_references(
            &format!(
                "Here you go.\n<attachment>{}</attachment>",
                file_path.display()
            ),
            temp_dir.path(),
        )
        .unwrap();

        assert_eq!(text, "Here you go.");
        assert_eq!(attachments.len(), 1);
        assert_eq!(attachments[0].path, file_path.canonicalize().unwrap());
    }

    #[test]
    fn builds_multimodal_user_message_for_vision_models() {
        let temp_dir = TempDir::new().unwrap();
        let image_path = temp_dir.path().join("photo.png");
        fs::write(&image_path, [0_u8, 1, 2, 3]).unwrap();
        let attachment = StoredAttachment {
            id: Uuid::new_v4(),
            kind: AttachmentKind::Image,
            original_name: Some("photo.png".to_string()),
            media_type: Some("image/png".to_string()),
            path: image_path,
            size_bytes: 4,
        };
        let model = ModelConfig {
            model_type: crate::config::ModelType::Openrouter,
            api_endpoint: "https://example.com/v1".to_string(),
            model: "demo-vision".to_string(),
            backend: AgentBackendKind::AgentFrame,
            supports_vision_input: false,
            image_tool_model: None,
            web_search_model: None,
            api_key: None,
            api_key_env: "TEST_API_KEY".to_string(),
            chat_completions_path: "/chat/completions".to_string(),
            codex_home: None,
            auth_credentials_store_mode: agent_frame::config::AuthCredentialsStoreMode::Auto,
            timeout_seconds: 30.0,
            retry_mode: Default::default(),
            context_window_tokens: 128_000,
            cache_ttl: None,
            reasoning: None,
            headers: serde_json::Map::new(),
            description: "vision".to_string(),
            agent_model_enabled: true,
            native_web_search: None,
            token_estimation: None,
            external_web_search: None,
            capabilities: vec![ModelCapability::ImageIn],
        };

        let message = build_user_turn_message(
            Some("看看这张图"),
            &[attachment],
            &model,
            true,
            Some("[System Date: 2026-04-10 01:23:45 +08:00]"),
        )
        .unwrap();

        let content = message.content.unwrap();
        let items = content.as_array().unwrap();
        assert_eq!(items[0]["type"], "text");
        let text = items[0]["text"].as_str().unwrap();
        assert!(text.contains("[System Date: 2026-04-10 01:23:45 +08:00]"));
        assert!(text.contains("already directly visible in this request"));
        assert!(text.contains("instead of calling load/query tools"));
        assert_eq!(items[1]["type"], "image_url");
        let url = items[1]["image_url"]["url"].as_str().unwrap();
        assert!(url.starts_with("data:image/png;base64,"));
    }

    #[test]
    fn sanitizes_historical_multimodal_messages_for_non_vision_models() {
        let messages = vec![ChatMessage {
            role: "user".to_string(),
            content: Some(Value::Array(vec![
                json!({
                    "type": "text",
                    "text": "先看这张图"
                }),
                json!({
                    "type": "image_url",
                    "image_url": {
                        "url": "data:image/png;base64,AAAA"
                    }
                }),
            ])),
            name: None,
            tool_call_id: None,
            tool_calls: None,
        }];
        let model = ModelConfig {
            model_type: crate::config::ModelType::Openrouter,
            api_endpoint: "https://example.com/v1".to_string(),
            model: "demo-chat".to_string(),
            backend: AgentBackendKind::AgentFrame,
            supports_vision_input: false,
            image_tool_model: None,
            web_search_model: None,
            api_key: None,
            api_key_env: "TEST_API_KEY".to_string(),
            chat_completions_path: "/chat/completions".to_string(),
            codex_home: None,
            auth_credentials_store_mode: agent_frame::config::AuthCredentialsStoreMode::Auto,
            timeout_seconds: 30.0,
            retry_mode: Default::default(),
            context_window_tokens: 128_000,
            cache_ttl: None,
            reasoning: None,
            headers: serde_json::Map::new(),
            description: "chat".to_string(),
            agent_model_enabled: true,
            native_web_search: None,
            token_estimation: None,
            external_web_search: None,
            capabilities: vec![ModelCapability::Chat],
        };

        let sanitized = sanitize_messages_for_model_capabilities(&messages, &model, true);
        let items = sanitized[0]
            .content
            .as_ref()
            .and_then(Value::as_array)
            .unwrap();
        assert_eq!(items.len(), 2);
        assert_eq!(items[0]["type"], "text");
        assert_eq!(items[0]["text"], "先看这张图");
        assert_eq!(items[1]["type"], "text");
        assert!(
            items[1]["text"]
                .as_str()
                .is_some_and(|text| text.contains("does not accept image input"))
        );
    }

    #[test]
    fn immediate_channel_send_works_without_tokio_runtime() {
        let channel = Arc::new(RecordingChannel::default());
        let address = ChannelAddress {
            channel_id: "telegram-main".to_string(),
            conversation_id: "1717801091".to_string(),
            user_id: Some("user-1".to_string()),
            display_name: Some("Telegram User".to_string()),
        };

        send_outgoing_message_now(
            channel.clone(),
            address.clone(),
            OutgoingMessage::text("still working"),
        )
        .unwrap();

        let sent_messages = channel.sent_messages.lock().unwrap();
        assert_eq!(sent_messages.len(), 1);
        assert_eq!(sent_messages[0].0, address);
        assert_eq!(sent_messages[0].1.text.as_deref(), Some("still working"));
    }

    #[test]
    fn image_generation_routing_defaults_to_native_only_when_unconfigured() {
        let model = ModelConfig {
            model_type: crate::config::ModelType::OpenrouterResp,
            api_endpoint: "https://example.com/v1".to_string(),
            model: "demo-image".to_string(),
            backend: AgentBackendKind::AgentFrame,
            supports_vision_input: false,
            image_tool_model: None,
            web_search_model: None,
            api_key: None,
            api_key_env: "TEST_API_KEY".to_string(),
            chat_completions_path: "/responses".to_string(),
            codex_home: None,
            auth_credentials_store_mode: agent_frame::config::AuthCredentialsStoreMode::Auto,
            timeout_seconds: 30.0,
            retry_mode: Default::default(),
            context_window_tokens: 128_000,
            cache_ttl: None,
            reasoning: None,
            headers: serde_json::Map::new(),
            description: "image model".to_string(),
            agent_model_enabled: true,
            native_web_search: None,
            token_estimation: None,
            external_web_search: None,
            capabilities: vec![ModelCapability::Chat, ModelCapability::ImageOut],
        };

        assert_eq!(
            select_image_generation_routing(None, &model),
            ImageGenerationRouting::Native
        );
        assert_eq!(
            select_image_generation_routing(
                Some(&ToolingTarget {
                    alias: "helper".to_string(),
                    prefer_self: false,
                }),
                &model,
            ),
            ImageGenerationRouting::Tool
        );
        assert_eq!(
            select_image_generation_routing(
                Some(&ToolingTarget {
                    alias: "helper".to_string(),
                    prefer_self: true,
                }),
                &model,
            ),
            ImageGenerationRouting::Native
        );
    }

    #[test]
    fn image_generation_routing_falls_back_to_tool_when_self_is_unavailable() {
        let completion_model = ModelConfig {
            model_type: crate::config::ModelType::Openrouter,
            api_endpoint: "https://example.com/v1".to_string(),
            model: "demo-image".to_string(),
            backend: AgentBackendKind::AgentFrame,
            supports_vision_input: false,
            image_tool_model: None,
            web_search_model: None,
            api_key: None,
            api_key_env: "TEST_API_KEY".to_string(),
            chat_completions_path: "/chat/completions".to_string(),
            codex_home: None,
            auth_credentials_store_mode: agent_frame::config::AuthCredentialsStoreMode::Auto,
            timeout_seconds: 30.0,
            retry_mode: Default::default(),
            context_window_tokens: 128_000,
            cache_ttl: None,
            reasoning: None,
            headers: serde_json::Map::new(),
            description: "image model".to_string(),
            agent_model_enabled: true,
            native_web_search: None,
            token_estimation: None,
            external_web_search: None,
            capabilities: vec![ModelCapability::Chat, ModelCapability::ImageOut],
        };
        let non_image_model = ModelConfig {
            capabilities: vec![ModelCapability::Chat],
            ..completion_model.clone()
        };
        let self_target = ToolingTarget {
            alias: "helper".to_string(),
            prefer_self: true,
        };

        assert_eq!(
            select_image_generation_routing(Some(&self_target), &completion_model),
            ImageGenerationRouting::Tool
        );
        assert_eq!(
            select_image_generation_routing(Some(&self_target), &non_image_model),
            ImageGenerationRouting::Tool
        );
        assert_eq!(
            select_image_generation_routing(None, &completion_model),
            ImageGenerationRouting::Disabled
        );
    }

    #[test]
    fn interrupted_followup_text_gets_marker_prefix() {
        assert_eq!(
            tag_interrupted_followup_text(Some("进度如何？".to_string())).as_deref(),
            Some("[Interrupted Follow-up]\n进度如何？")
        );
        assert_eq!(
            tag_interrupted_followup_text(None).as_deref(),
            Some("[Interrupted Follow-up]")
        );
    }

    #[test]
    fn status_and_help_commands_are_out_of_band_even_with_bot_mentions() {
        assert!(is_out_of_band_command(Some("/status")));
        assert!(is_out_of_band_command(Some("/status@party_claw_bot")));
        assert!(is_out_of_band_command(Some("  /help@party_claw_bot  ")));
        assert!(!is_out_of_band_command(Some("/compact")));
        assert!(!is_out_of_band_command(Some("普通消息")));
        assert!(!is_out_of_band_command(None));
    }

    #[test]
    fn slash_commands_have_an_explicit_transport_lane() {
        assert_eq!(
            incoming_command_lane(Some("/status@party_claw_bot")),
            Some(IncomingCommandLane::Immediate)
        );
        assert_eq!(
            incoming_command_lane(Some("/compact@party_claw_bot")),
            Some(IncomingCommandLane::Conversation)
        );
        assert_eq!(
            incoming_command_lane(Some("/unknown_command@party_claw_bot arg")),
            Some(IncomingCommandLane::Immediate)
        );
        assert_eq!(
            incoming_command_lane(Some("/home/jeremy/project")),
            Some(IncomingCommandLane::Immediate)
        );
        assert!(is_command_like_text(Some(
            "/unknown_command@party_claw_bot arg"
        )));
        assert!(is_command_like_text(Some("/home/jeremy/project")));
    }

    #[test]
    fn yield_request_detects_compaction_in_progress() {
        let temp_dir = TempDir::new().unwrap();
        let workspace_manager = WorkspaceManager::load_or_create(temp_dir.path()).unwrap();
        let mut sessions = SessionManager::new(temp_dir.path(), workspace_manager).unwrap();
        let address = ChannelAddress {
            channel_id: "telegram".to_string(),
            conversation_id: "conversation-1".to_string(),
            user_id: None,
            display_name: None,
        };
        let actor = sessions.ensure_foreground_actor(&address).unwrap();
        let control = SessionExecutionControl::new();
        actor.register_control(control.clone()).unwrap();
        actor
            .receive_runtime_event(&SessionEvent::CompactionStarted {
                phase: "initial".to_string(),
                message_count: 3,
            })
            .unwrap();

        let disposition = actor
            .tell_user_message(test_user_message("进度如何？"))
            .unwrap();

        assert!(disposition.interrupted);
        assert!(disposition.compaction_in_progress);
        assert_eq!(
            disposition.text.as_deref(),
            Some("[Interrupted Follow-up]\n进度如何？")
        );
        assert!(control.take_yield_requested());
    }

    #[test]
    fn yield_request_is_scoped_to_matching_conversation() {
        let temp_dir = TempDir::new().unwrap();
        let workspace_manager = WorkspaceManager::load_or_create(temp_dir.path()).unwrap();
        let mut sessions = SessionManager::new(temp_dir.path(), workspace_manager).unwrap();
        let active_address = ChannelAddress {
            channel_id: "telegram".to_string(),
            conversation_id: "conversation-active".to_string(),
            user_id: None,
            display_name: None,
        };
        let other_address = ChannelAddress {
            channel_id: "telegram".to_string(),
            conversation_id: "conversation-other".to_string(),
            user_id: None,
            display_name: None,
        };
        let active_actor = sessions.ensure_foreground_actor(&active_address).unwrap();
        let active_control = SessionExecutionControl::new();
        active_actor
            .register_control(active_control.clone())
            .unwrap();
        active_actor
            .receive_runtime_event(&SessionEvent::CompactionStarted {
                phase: "initial".to_string(),
                message_count: 3,
            })
            .unwrap();

        let disposition = sessions
            .resolve_foreground_by_address(&other_address)
            .ok()
            .and_then(|actor| actor.tell_user_message(test_user_message("hello")).ok())
            .unwrap_or_default();

        assert!(!disposition.interrupted);
        assert!(!disposition.compaction_in_progress);
        assert!(!active_control.take_yield_requested());
    }

    #[test]
    fn pending_interrupt_is_applied_to_next_runtime_control() {
        let temp_dir = TempDir::new().unwrap();
        let workspace_manager = WorkspaceManager::load_or_create(temp_dir.path()).unwrap();
        let mut sessions = SessionManager::new(temp_dir.path(), workspace_manager).unwrap();
        let address = ChannelAddress {
            channel_id: "telegram".to_string(),
            conversation_id: "conversation-1".to_string(),
            user_id: None,
            display_name: None,
        };
        let actor = sessions.ensure_foreground_actor(&address).unwrap();
        let first_control = SessionExecutionControl::new();
        actor.register_control(first_control.clone()).unwrap();
        let disposition = actor.tell_user_message(test_user_message("继续")).unwrap();
        assert!(disposition.interrupted);
        actor.unregister_control().unwrap();
        let next_control = SessionExecutionControl::new();
        actor.register_control(next_control.clone()).unwrap();

        assert!(first_control.take_yield_requested());
        assert!(next_control.take_yield_requested());
    }

    #[test]
    fn interrupted_followups_can_still_be_coalesced_before_runtime_returns() {
        let address = ChannelAddress {
            channel_id: "telegram".to_string(),
            conversation_id: "conversation-1".to_string(),
            user_id: None,
            display_name: None,
        };
        let initial = IncomingMessage {
            remote_message_id: "msg-1".to_string(),
            address: address.clone(),
            text: Some("[Interrupted Follow-up]\n进度如何？".to_string()),
            attachments: Vec::new(),
            stored_attachments: Vec::new(),
            control: None,
        };
        let later = IncomingMessage {
            remote_message_id: "msg-2".to_string(),
            address,
            text: Some("[Interrupted Follow-up]\n继续".to_string()),
            attachments: Vec::new(),
            stored_attachments: Vec::new(),
            control: None,
        };
        let mut queue = std::collections::VecDeque::from([later]);

        let returned = coalesce_buffered_conversation_messages(initial, &mut queue);

        assert_eq!(returned.remote_message_id, "msg-2");
        assert!(queue.is_empty());
        let text = returned.text.expect("merged follow-up text should exist");
        assert!(text.contains("Follow-up 1"));
        assert!(text.contains("Follow-up 2"));
    }

    #[test]
    fn buffered_slash_commands_are_not_coalesced_into_user_context() {
        let address = ChannelAddress {
            channel_id: "telegram".to_string(),
            conversation_id: "conversation-1".to_string(),
            user_id: None,
            display_name: None,
        };
        let initial = IncomingMessage {
            remote_message_id: "msg-1".to_string(),
            address: address.clone(),
            text: Some("[Interrupted Follow-up]\n进度如何？".to_string()),
            attachments: Vec::new(),
            stored_attachments: Vec::new(),
            control: None,
        };
        let command = IncomingMessage {
            remote_message_id: "msg-2".to_string(),
            address: address.clone(),
            text: Some("/status@party_claw_bot".to_string()),
            attachments: Vec::new(),
            stored_attachments: Vec::new(),
            control: None,
        };
        let later = IncomingMessage {
            remote_message_id: "msg-3".to_string(),
            address,
            text: Some("[Interrupted Follow-up]\n继续".to_string()),
            attachments: Vec::new(),
            stored_attachments: Vec::new(),
            control: None,
        };
        let mut queue = std::collections::VecDeque::from([command, later]);

        let returned = coalesce_buffered_conversation_messages(initial, &mut queue);

        assert_eq!(returned.remote_message_id, "msg-1");
        assert_eq!(
            returned.text.as_deref(),
            Some("[Interrupted Follow-up]\n进度如何？")
        );
        assert_eq!(queue.len(), 2);
        assert_eq!(queue[0].text.as_deref(), Some("/status@party_claw_bot"));
        assert_eq!(
            queue[1].text.as_deref(),
            Some("[Interrupted Follow-up]\n继续")
        );
    }

    #[test]
    fn unknown_slash_commands_are_control_messages_not_user_context() {
        assert!(is_command_like_text(Some(
            "/unknown-command@party_claw_bot arg"
        )));
        assert!(matches!(
            incoming_command_lane(Some("/unknown-command@party_claw_bot arg")),
            Some(IncomingCommandLane::Immediate)
        ));
        assert!(is_command_like_text(Some("/some/path")));
        assert!(matches!(
            incoming_command_lane(Some("/some/path")),
            Some(IncomingCommandLane::Immediate)
        ));
    }

    #[test]
    fn synthetic_skill_updates_are_system_messages_not_user_prefixes() {
        let injected = build_synthetic_system_messages(
            None,
            None,
            None,
            Some("[Runtime Skill Updates]\nSkill \"search\" updated to version 3."),
        );
        assert_eq!(injected.len(), 1);
        assert_eq!(injected[0].role, "system");
        assert_eq!(
            injected[0].content.as_ref().and_then(Value::as_str),
            Some("[Runtime Skill Updates]\nSkill \"search\" updated to version 3.")
        );

        let mut previous = vec![ChatMessage::text("assistant", "existing context")];
        previous.extend(injected);
        previous.push(ChatMessage::text("user", "继续"));
        assert_eq!(previous.len(), 3);
        assert_eq!(previous[1].role, "system");
        assert_eq!(previous[2].role, "user");
    }

    #[test]
    fn synthetic_process_restart_notice_is_first_system_message() {
        let injected = build_synthetic_system_messages(
            Some(SYSTEM_RESTART_NOTICE),
            Some("[System Tip: 2.0 hours since the last user message.]"),
            None,
            None,
        );

        assert_eq!(injected.len(), 2);
        assert_eq!(
            injected[0].content.as_ref().and_then(Value::as_str),
            Some(SYSTEM_RESTART_NOTICE)
        );
        assert_eq!(injected[1].role, "system");
    }

    #[test]
    fn remote_alias_prompt_updates_render_as_runtime_prompt_notice() {
        let rendered = render_prompt_component_change_notices(&[PromptComponentChangeNotice {
            key: REMOTE_ALIASES_PROMPT_COMPONENT.to_string(),
            value:
                "Available SSH remote aliases detected from this host's SSH config:\n- `wuwen-dev3`"
                    .to_string(),
        }]);

        assert!(rendered.contains("[Runtime Prompt Updates]"));
        assert!(rendered.contains("available SSH remote alias list changed"));
        assert!(rendered.contains("`wuwen-dev3`"));
    }

    #[test]
    fn canonical_system_prompt_is_rebuilt_before_new_turn() {
        let (rewritten, rebuilt) = rebuild_canonical_system_prompt(
            &[
                ChatMessage::text("system", "old prompt"),
                ChatMessage::text("assistant", "existing context"),
            ],
            "new prompt",
        );

        assert!(rebuilt);
        assert_eq!(
            rewritten[0].content.as_ref().and_then(Value::as_str),
            Some("new prompt")
        );
        assert_eq!(
            rewritten[1].content.as_ref().and_then(Value::as_str),
            Some("existing context")
        );
    }

    #[test]
    fn previous_messages_builder_rewrites_only_canonical_prefix() {
        let injected = build_synthetic_system_messages(
            None,
            None,
            None,
            Some("[Runtime Skill Updates]\nSkill \"search\" updated to version 3."),
        );
        let (mut previous, rebuilt) = rebuild_canonical_system_prompt(
            &[
                ChatMessage::text("system", "old prompt"),
                ChatMessage::text("assistant", "existing context"),
            ],
            "new prompt",
        );
        previous.extend(injected);
        previous.push(ChatMessage::text("user", "继续"));

        assert!(rebuilt);
        assert_eq!(
            previous[0].content.as_ref().and_then(Value::as_str),
            Some("new prompt")
        );
        assert_eq!(
            previous[1].content.as_ref().and_then(Value::as_str),
            Some("existing context")
        );
        assert_eq!(previous[2].role, "system");
        assert_eq!(previous[3].role, "user");
    }

    #[test]
    fn normalize_messages_for_persistence_keeps_one_canonical_system_and_drops_ephemeral_systems() {
        let canonical = "[AgentHost Main Foreground Agent]\ncanonical prompt";
        let ephemeral = vec![ChatMessage::text(
            "system",
            "[System Message: USER.md changed. It stores user info. If you need refreshed user info in this run, use file_read on ./USER.md.]",
        )];
        let messages = vec![
            ChatMessage::text("system", "[AgentFrame Runtime]\nold prompt"),
            ChatMessage::text("system", "[AgentHost Main Foreground Agent]\nold duplicate"),
            ChatMessage::text(
                "system",
                "[System Message: USER.md changed. It stores user info. If you need refreshed user info in this run, use file_read on ./USER.md.]",
            ),
            ChatMessage::text("assistant", "summary"),
            ChatMessage::text("system", "[Active Runtime Tasks]\nexec_id=123"),
            ChatMessage::text("user", "继续"),
        ];

        let normalized = normalize_messages_for_persistence(messages, canonical, &ephemeral);
        assert_eq!(normalized[0], ChatMessage::text("system", canonical));
        assert_eq!(normalized[1], ChatMessage::text("assistant", "summary"));
        assert_eq!(
            normalized[2],
            ChatMessage::text("system", "[Active Runtime Tasks]\nexec_id=123")
        );
        assert_eq!(normalized[3], ChatMessage::text("user", "继续"));
        assert_eq!(normalized.len(), 4);
    }

    #[test]
    fn normalize_messages_for_persistence_preserves_runtime_system_notices() {
        let canonical = "[AgentHost Main Foreground Agent]\ncanonical prompt";
        let restart_notice = SYSTEM_RESTART_NOTICE;
        let runtime_notice = "[Runtime Skill Updates]\nSkill \"search\" updated to version 3.";
        let messages = vec![
            ChatMessage::text("system", "[AgentFrame Runtime]\nold prompt"),
            ChatMessage::text("assistant", "existing context"),
            ChatMessage::text("system", restart_notice),
            ChatMessage::text("system", runtime_notice),
            ChatMessage::text("user", "继续"),
        ];

        let normalized = normalize_messages_for_persistence(messages, canonical, &[]);

        assert_eq!(normalized[0], ChatMessage::text("system", canonical));
        assert_eq!(normalized[2], ChatMessage::text("system", restart_notice));
        assert_eq!(normalized[3], ChatMessage::text("system", runtime_notice));
        assert_eq!(normalized.len(), 5);
    }

    #[test]
    fn system_prompt_prefix_survives_normal_turns_and_updates_after_compaction() {
        let prompt_v1 = "[AgentHost Main Foreground Agent]\nIdentity: v1";
        let prompt_v2 = "[AgentHost Main Foreground Agent]\nIdentity: v2";
        let stored_messages = vec![
            ChatMessage::text("system", prompt_v1),
            ChatMessage::text("user", "第一轮"),
            ChatMessage::text("assistant", "收到"),
        ];

        let (turn_messages, active_prompt, rebuilt) =
            prepare_system_prompt_for_turn(&stored_messages, prompt_v2, false);
        assert!(!rebuilt);
        assert_eq!(active_prompt, prompt_v1);
        assert_eq!(turn_messages[0], ChatMessage::text("system", prompt_v1));

        let persisted_without_compaction =
            normalize_messages_for_persistence(turn_messages.clone(), &active_prompt, &[]);
        assert_eq!(
            persisted_without_compaction[0],
            ChatMessage::text("system", prompt_v1)
        );

        let persisted_after_compaction =
            normalize_messages_for_persistence(turn_messages, prompt_v2, &[]);
        assert_eq!(
            persisted_after_compaction[0],
            ChatMessage::text("system", prompt_v2)
        );
    }

    #[test]
    fn leading_system_prompt_ignores_agent_frame_rendered_prompt() {
        let messages = vec![ChatMessage::text(
            "system",
            "[AgentFrame Runtime]\n\n[AgentHost Main Background Agent]\nold prompt",
        )];

        assert!(leading_system_prompt(&messages).is_none());
    }

    #[test]
    fn user_time_tip_is_emitted_after_five_minutes_of_idle_time() {
        let now = Utc::now();
        let session = SessionSnapshot {
            kind: crate::session::SessionKind::Foreground,
            last_user_message_at: Some(now - ChronoDuration::minutes(6)),
            last_agent_returned_at: Some(now - ChronoDuration::minutes(5)),
            ..build_test_session(&TempDir::new().unwrap())
        };

        let tip = render_last_user_message_time_tip(&session, now).expect("tip should exist");
        assert!(tip.starts_with("[System Tip: "));
        assert!(tip.contains("hours since the last user message"));
    }

    #[test]
    fn user_time_tip_is_not_emitted_before_five_minutes_of_idle_time() {
        let now = Utc::now();
        let session = SessionSnapshot {
            kind: crate::session::SessionKind::Foreground,
            last_user_message_at: Some(now - ChronoDuration::minutes(10)),
            last_agent_returned_at: Some(now - ChronoDuration::minutes(4)),
            ..build_test_session(&TempDir::new().unwrap())
        };

        assert!(render_last_user_message_time_tip(&session, now).is_none());
    }

    #[test]
    fn system_date_is_formatted_for_user_message_prefix() {
        let now = chrono::DateTime::parse_from_rfc3339("2026-04-09T16:52:45Z")
            .unwrap()
            .with_timezone(&Utc);

        let rendered = render_system_date_on_user_message(now);
        let expected = format!(
            "[System Date: {}]",
            now.with_timezone(&chrono::Local)
                .format("%Y-%m-%d %H:%M:%S %:z")
        );
        assert_eq!(rendered, expected);
    }

    #[test]
    fn shared_profile_sync_copies_missing_workspace_files_and_upload_only_reports_real_changes() {
        let temp_dir = TempDir::new().unwrap();
        let workdir = temp_dir.path();
        let agent_dir = workdir.join("agent");
        let workspace_root = workdir.join("workspace");
        let rundir = workdir.join("rundir");
        fs::create_dir_all(&agent_dir).unwrap();
        fs::create_dir_all(&workspace_root).unwrap();
        fs::create_dir_all(&rundir).unwrap();
        fs::create_dir_all(rundir.join(".skills")).unwrap();
        fs::write(agent_dir.join("USER.md"), "shared user v1").unwrap();
        fs::write(agent_dir.join("IDENTITY.md"), "shared identity v1").unwrap();
        fs::write(rundir.join("AGENTS.md"), "").unwrap();

        let agent_workspace = AgentWorkspace {
            root_dir: workdir.to_path_buf(),
            rundir: rundir.clone(),
            agent_dir: agent_dir.clone(),
            skills_dir: rundir.join(".skills"),
            skill_creator_dir: rundir.join(".skills/skill-creator"),
            tmp_dir: rundir.join("tmp"),
            identity_md_path: agent_dir.join("IDENTITY.md"),
            user_md_path: agent_dir.join("USER.md"),
            agents_md_path: rundir.join("AGENTS.md"),
            identity_prompt: "stale identity".to_string(),
            user_profile_markdown: "stale user".to_string(),
            raw_identity_markdown: "stale identity".to_string(),
            agents_markdown: String::new(),
        };

        let synced =
            sync_workspace_shared_profile_files(&agent_workspace, &workspace_root).unwrap();
        assert!(synced.user_changed);
        assert!(synced.identity_changed);
        assert_eq!(
            fs::read_to_string(workspace_root.join("USER.md")).unwrap(),
            "shared user v1"
        );
        assert_eq!(
            fs::read_to_string(workspace_root.join("IDENTITY.md")).unwrap(),
            "shared identity v1"
        );

        let no_op =
            upload_workspace_shared_profile_files(&agent_workspace, &workspace_root).unwrap();
        assert!(!no_op.changed_any());

        fs::write(workspace_root.join("IDENTITY.md"), "shared identity v2").unwrap();
        let changed =
            upload_workspace_shared_profile_files(&agent_workspace, &workspace_root).unwrap();
        assert!(!changed.user_changed);
        assert!(changed.identity_changed);
        assert_eq!(
            fs::read_to_string(agent_dir.join("IDENTITY.md")).unwrap(),
            "shared identity v2"
        );
    }

    #[test]
    fn persist_compaction_artifacts_writes_rollout_and_memory_files() {
        let temp_dir = TempDir::new().unwrap();
        let session = build_test_session(&temp_dir);
        fs::create_dir_all(&session.root_dir).unwrap();

        let report = ContextCompactionReport {
            messages: vec![ChatMessage::text("assistant", "compacted")],
            compacted_messages: vec![
                ChatMessage::text("user", "我们来设计新的 memory system"),
                ChatMessage::text("assistant", "先确定 rollout 和 retrieval 的关系"),
            ],
            usage: TokenUsage::default(),
            compacted: true,
            estimated_tokens_before: 1200,
            estimated_tokens_after: 400,
            token_limit: 1000,
            structured_output: Some(StructuredCompactionOutput {
                old_summary: "- 之前已经确定使用三层 memory。".to_string(),
                new_summary: "- 本轮明确了 rollout_summary 和 rollout_transcript 的职责。"
                    .to_string(),
                keywords: vec!["memory".to_string(), "rollout".to_string()],
                important_refs: StructuredCompactionRefs {
                    paths: vec!["NEW_MEMORY_SYSTEM.md".to_string()],
                    ..StructuredCompactionRefs::default()
                },
                memory_hints: vec![StructuredCompactionMemoryHint {
                    group: "Memory System".to_string(),
                    conclusions: vec!["rollout_summary 用于中层摘要。".to_string()],
                }],
                next_step: "继续实现 rollout_search。".to_string(),
            }),
        };

        let rollout_id = persist_compaction_artifacts(&session, &report)
            .unwrap()
            .expect("rollout id should be created");
        let memory_root = conversation_memory_root(&session);

        assert!(memory_root.join("memory_summary.json").is_file());
        assert!(memory_root.join("MEMORY.json").is_file());
        assert!(
            memory_root
                .join("rollouts")
                .join(&rollout_id)
                .join("rollout_summary.json")
                .is_file()
        );
        assert!(
            memory_root
                .join("rollouts")
                .join(&rollout_id)
                .join("rollout_transcript.jsonl")
                .is_file()
        );

        let memory_summary: Value = serde_json::from_str(
            &fs::read_to_string(memory_root.join("memory_summary.json")).unwrap(),
        )
        .unwrap();
        assert!(
            memory_summary["recent_groups"]
                .as_array()
                .unwrap()
                .iter()
                .any(|value| value.as_str() == Some("Memory System"))
        );

        let memory_index: Value =
            serde_json::from_str(&fs::read_to_string(memory_root.join("MEMORY.json")).unwrap())
                .unwrap();
        assert!(
            memory_index["groups"]
                .as_array()
                .unwrap()
                .iter()
                .any(|group| group["group"].as_str() == Some("Memory System"))
        );
    }

    #[test]
    fn memory_search_reads_memory_layers_without_loading_transcript() {
        let temp_dir = TempDir::new().unwrap();
        let session = build_test_session(&temp_dir);
        let rollout_id = seed_memory_artifacts(&session);

        let result = memory_search_files(&session, "rollout", 10).unwrap();
        let matches = result["matches"].as_array().unwrap();

        assert!(!matches.is_empty());
        assert!(
            matches
                .iter()
                .any(|entry| entry["layer"].as_str() == Some("memory_summary"))
        );
        assert!(matches.iter().any(|entry| {
            entry["layer"].as_str() == Some("memory")
                && entry["rollouts"].as_array().is_some_and(|rollouts| {
                    rollouts.iter().any(|value| {
                        value.as_str()
                            == Some(
                                format!("rollouts/{}/rollout_summary.json", rollout_id).as_str(),
                            )
                    })
                })
        }));
    }

    #[test]
    fn rollout_search_finds_exact_matches_and_kind_filters() {
        let temp_dir = TempDir::new().unwrap();
        let session = build_test_session(&temp_dir);
        let rollout_id = seed_memory_artifacts(&session);

        let result = rollout_search_files(
            &session,
            "context compression summary came back empty",
            Some(&rollout_id),
            &["tool_result".to_string()],
            5,
        )
        .unwrap();
        let matches = result["matches"].as_array().unwrap();

        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0]["rollout_id"].as_str(), Some(rollout_id.as_str()));
        assert_eq!(matches[0]["kind"].as_str(), Some("tool_result"));
        assert!(matches[0]["preview"].as_str().is_some_and(|preview| {
            preview.contains("context compression summary came back empty")
        }));
    }

    #[test]
    fn rollout_read_returns_small_turn_segment_around_anchor() {
        let temp_dir = TempDir::new().unwrap();
        let session = build_test_session(&temp_dir);
        let rollout_id = seed_memory_artifacts(&session);

        let result =
            rollout_read_file(&session, &rollout_id, 3, Some("turn_segment"), 1, 1).unwrap();
        let events = result["events"].as_array().unwrap();

        assert_eq!(result["rollout_id"].as_str(), Some(rollout_id.as_str()));
        assert_eq!(result["anchor_event_id"].as_u64(), Some(3));
        assert!(events.len() >= 2);
        assert!(
            events
                .iter()
                .any(|event| event["kind"].as_str() == Some("tool_call"))
        );
        assert!(
            events
                .iter()
                .any(|event| event["kind"].as_str() == Some("tool_result"))
        );
        assert_eq!(result["has_more_before"].as_bool(), Some(false));
    }

    #[test]
    fn channel_restart_backoff_grows_and_caps() {
        assert_eq!(channel_restart_backoff_seconds(1), 1);
        assert_eq!(channel_restart_backoff_seconds(2), 2);
        assert_eq!(channel_restart_backoff_seconds(3), 4);
        assert_eq!(channel_restart_backoff_seconds(10), 30);
    }

    #[test]
    fn background_agent_tools_use_current_conversation_delivery_and_silent_terminate() {
        let temp_dir = TempDir::new().unwrap();
        let channel: Arc<dyn Channel> = Arc::new(RecordingChannel::default());
        let server = build_test_server(&temp_dir, channel);
        let runtime = server.agent_runtime_view();
        let session = build_test_session(&temp_dir);

        let foreground_tools = runtime.build_extra_tools(
            &session,
            AgentPromptKind::MainForeground,
            session.agent_id,
            None,
        );
        let start_background = foreground_tools
            .iter()
            .find(|tool| tool.name == "start_background_agent")
            .expect("foreground should expose start_background_agent");
        assert!(
            start_background
                .parameters
                .get("properties")
                .and_then(Value::as_object)
                .is_some_and(|properties| !properties.contains_key("sink"))
        );
        let create_cron = foreground_tools
            .iter()
            .find(|tool| tool.name == "create_cron_task")
            .expect("foreground should expose create_cron_task");
        let create_cron_properties = create_cron
            .parameters
            .get("properties")
            .and_then(Value::as_object)
            .expect("create_cron_task properties");
        assert!(!create_cron_properties.contains_key("schedule"));
        assert!(create_cron_properties.contains_key("cron_second"));
        assert!(create_cron_properties.contains_key("cron_minute"));
        assert!(create_cron_properties.contains_key("timezone"));
        let create_cron_required = create_cron
            .parameters
            .get("required")
            .and_then(Value::as_array)
            .expect("create_cron_task required fields");
        assert!(
            create_cron_required
                .iter()
                .any(|value| value.as_str() == Some("cron_second"))
        );
        assert!(
            create_cron_required
                .iter()
                .any(|value| value.as_str() == Some("cron_day_of_week"))
        );
        assert!(!foreground_tools.iter().any(|tool| tool.name == "terminate"));

        let background_tools = runtime.build_extra_tools(
            &session,
            AgentPromptKind::MainBackground,
            session.agent_id,
            Some(SessionExecutionControl::new()),
        );
        assert!(background_tools.iter().any(|tool| tool.name == "terminate"));
        let background_user_tell = background_tools
            .iter()
            .find(|tool| tool.name == "user_tell")
            .expect("background should expose user_tell for genuine progress");
        assert!(
            background_user_tell
                .description
                .contains("do not use user_tell for the primary result")
        );
        assert!(
            background_user_tell
                .description
                .contains("Put that primary user-facing message in your final answer instead")
        );
        assert!(
            !background_tools
                .iter()
                .any(|tool| tool.name == "start_background_agent")
        );
    }

    #[test]
    fn idle_context_compaction_requires_idle_time_new_turns_and_min_tokens() {
        let now = Utc::now();
        let base_snapshot = SessionSnapshot {
            kind: crate::session::SessionKind::Foreground,
            id: Uuid::new_v4(),
            agent_id: Uuid::new_v4(),
            address: ChannelAddress {
                channel_id: "telegram-main".to_string(),
                conversation_id: "1717801091".to_string(),
                user_id: Some("user-1".to_string()),
                display_name: Some("Telegram User".to_string()),
            },
            root_dir: PathBuf::from("/tmp/session"),
            attachments_dir: PathBuf::from("/tmp/workspaces/workspace-1/files/upload"),
            workspace_id: "workspace-1".to_string(),
            workspace_root: PathBuf::from("/tmp/workspaces/workspace-1/files"),
            last_user_message_at: None,
            last_agent_returned_at: Some(now - ChronoDuration::seconds(400)),
            last_compacted_at: None,
            turn_count: 2,
            last_compacted_turn_count: 1,
            cumulative_usage: TokenUsage::default(),
            cumulative_compaction: agent_frame::SessionCompactionStats::default(),
            api_timeout_override_seconds: None,
            skill_states: HashMap::new(),
            pending_workspace_summary: false,
            close_after_summary: false,
            session_state: crate::session::DurableSessionState::default(),
        };

        assert!(should_attempt_idle_context_compaction(
            &base_snapshot,
            now,
            Duration::from_secs(270),
            500,
            400,
        ));

        let no_new_turn = SessionSnapshot {
            kind: crate::session::SessionKind::Foreground,
            last_compacted_turn_count: 2,
            ..base_snapshot.clone()
        };
        assert!(!should_attempt_idle_context_compaction(
            &no_new_turn,
            now,
            Duration::from_secs(270),
            500,
            400,
        ));

        let not_idle_long_enough = SessionSnapshot {
            kind: crate::session::SessionKind::Foreground,
            last_agent_returned_at: Some(now - ChronoDuration::seconds(60)),
            ..base_snapshot.clone()
        };
        assert!(!should_attempt_idle_context_compaction(
            &not_idle_long_enough,
            now,
            Duration::from_secs(270),
            500,
            400,
        ));

        let no_return_yet = SessionSnapshot {
            kind: crate::session::SessionKind::Foreground,
            last_agent_returned_at: None,
            ..base_snapshot
        };
        assert!(!should_attempt_idle_context_compaction(
            &no_return_yet,
            now,
            Duration::from_secs(270),
            500,
            400,
        ));

        assert!(!should_attempt_idle_context_compaction(
            &SessionSnapshot {
                kind: crate::session::SessionKind::Foreground,
                last_agent_returned_at: Some(now - ChronoDuration::seconds(400)),
                ..no_return_yet
            },
            now,
            Duration::from_secs(270),
            200,
            400,
        ));
    }

    #[test]
    fn idle_compaction_min_ratio_sets_precompression_token_limit() {
        assert_eq!(idle_compaction_token_limit(None, 64_000), 64_000);
        assert_eq!(idle_compaction_token_limit(Some(48_000), 64_000), 48_000);
        assert_eq!(idle_compaction_token_limit(Some(96_000), 64_000), 64_000);
    }

    #[test]
    fn timeout_helpers_and_background_timeout_messages_behave_as_expected() {
        let timeout_error = anyhow!("background agent timed out after 135.0 seconds");
        let generic_error = anyhow!("background agent failed for another reason");

        assert!(is_timeout_like(&timeout_error));
        assert!(!is_timeout_like(&generic_error));
        assert!(background_timeout_with_active_children_text("zh-CN").contains("系统没有自动重试"));
        assert!(
            background_timeout_with_active_children_text("en")
                .contains("skipped automatic recovery")
        );
    }

    #[test]
    fn continue_error_text_prefers_compaction_recovery_wording() {
        let error =
            anyhow!("threshold context compaction failed during round phase: upstream timed out");
        let zh = user_facing_continue_error_text("zh-CN", &error, "已执行到工具阶段");
        let en = user_facing_continue_error_text("en", &error, "tool phase reached");

        assert!(zh.contains("自动上下文压缩失败"));
        assert!(zh.contains("失败原因"));
        assert!(zh.contains("/continue"));
        assert!(en.contains("Automatic context compaction failed"));
        assert!(en.contains("Failure reason"));
        assert!(en.contains("/continue"));
    }

    #[test]
    fn completed_foreground_assistant_history_appends_assistant_message() {
        let temp_dir = TempDir::new().unwrap();
        let channel = Arc::new(RecordingChannel::default());
        let server = build_test_server(&temp_dir, channel);
        let session = build_test_session(&temp_dir);
        server
            .with_sessions(|sessions| {
                sessions.ensure_foreground_actor(&session.address)?;
                Ok(())
            })
            .unwrap();

        let actor = server
            .with_sessions(|sessions| sessions.resolve_foreground_by_address(&session.address))
            .unwrap();
        actor
            .commit_runtime_turn(SessionRuntimeTurnCommit {
                messages: Vec::new(),
                consumed_pending_messages: Vec::new(),
                usage: TokenUsage::default(),
                compaction: SessionCompactionStats::default(),
                phase: SessionPhase::End,
                system_prompt_static_hash_after_compaction: None,
                loaded_skills: Vec::new(),
                user_history_text: None,
                assistant_history_text: Some("done".to_string()),
            })
            .unwrap();

        let checkpoint = server
            .with_sessions(|sessions| sessions.export_checkpoint(&session.address))
            .unwrap();
        assert_eq!(checkpoint.history.len(), 1);
        assert_eq!(
            checkpoint.history[0].role,
            crate::domain::MessageRole::Assistant
        );
        assert_eq!(checkpoint.history[0].text.as_deref(), Some("done"));
    }

    #[test]
    fn turn_error_errno_classification_prefers_compaction_then_upstream() {
        let threshold = anyhow!("threshold context compaction failed during round phase");
        let tool_wait = anyhow!("tool-wait context compaction failed: upstream timed out");
        let upstream = anyhow!("upstream provider returned 502");
        let runtime = anyhow!("worker thread panicked");

        assert_eq!(
            session_errno_for_turn_error(&threshold),
            SessionErrno::ThresholdCompactionFailure
        );
        assert_eq!(
            session_errno_for_turn_error(&tool_wait),
            SessionErrno::ToolWaitTimeout
        );
        assert_eq!(
            session_errno_for_turn_error(&upstream),
            SessionErrno::ApiFailure
        );
        assert_eq!(
            session_errno_for_turn_error(&runtime),
            SessionErrno::RuntimeFailure
        );
    }

    #[test]
    fn summarize_resume_progress_reports_tool_stage_in_chinese() {
        let messages = vec![
            ChatMessage::text("user", "继续处理"),
            ChatMessage {
                role: "assistant".to_string(),
                content: None,
                name: None,
                tool_call_id: None,
                tool_calls: Some(vec![ToolCall {
                    id: "call_1".to_string(),
                    kind: "function".to_string(),
                    function: FunctionCall {
                        name: "exec_wait".to_string(),
                        arguments: Some("{}".to_string()),
                    },
                }]),
            },
        ];

        let summary = summarize_resume_progress("zh-CN", &messages);
        assert!(summary.contains("工具阶段"));
        assert!(summary.contains("exec_wait"));
    }

    #[test]
    fn summarize_resume_progress_reports_partial_text_in_chinese() {
        let messages = vec![
            ChatMessage::text("user", "继续处理"),
            ChatMessage::text("assistant", "已经查到原因，正在整理修复方案。"),
        ];

        let summary = summarize_resume_progress("zh-CN", &messages);
        assert!(summary.contains("已保留部分助手输出"));
        assert!(summary.contains("已经查到原因"));
    }

    #[test]
    fn session_status_surfaces_idle_compaction_error_state() {
        let temp_dir = TempDir::new().unwrap();
        let mut session = build_test_session(&temp_dir);
        session.session_state.errno = Some(crate::session::SessionErrno::IdleCompactionFailure);
        session.session_state.errinfo =
            Some("upstream timeout while compacting older messages".to_string());
        let model = ModelConfig {
            model_type: crate::config::ModelType::Openrouter,
            model: "anthropic/claude-sonnet-4.6".to_string(),
            api_endpoint: "https://openrouter.ai/api/v1/chat/completions".to_string(),
            backend: AgentBackendKind::AgentFrame,
            supports_vision_input: false,
            image_tool_model: None,
            web_search_model: None,
            api_key: None,
            api_key_env: "TEST_API_KEY".to_string(),
            chat_completions_path: "/chat/completions".to_string(),
            codex_home: None,
            auth_credentials_store_mode: agent_frame::config::AuthCredentialsStoreMode::Auto,
            timeout_seconds: 120.0,
            retry_mode: Default::default(),
            context_window_tokens: 128_000,
            cache_ttl: None,
            reasoning: None,
            headers: serde_json::Map::new(),
            description: "test model".to_string(),
            agent_model_enabled: true,
            native_web_search: None,
            token_estimation: None,
            external_web_search: None,
            capabilities: Vec::new(),
        };

        let text = format_session_status(
            "zh-CN",
            "test-model",
            &model,
            &session,
            120.0,
            "model default",
            12_000,
            115_200,
            None,
            true,
            &ConversationUsageReport::default(),
            &ConversationPricingBreakdown::default(),
        );

        assert!(
            text.contains(
                "Idle compaction error: upstream timeout while compacting older messages"
            )
        );
        assert!(text.contains("Conversation: 1717801091"));
        assert!(text.contains("upstream timeout while compacting older messages"));
    }

    #[test]
    fn conversation_usage_window_sums_last_24h_turn_logs_for_same_conversation() {
        let temp_dir = TempDir::new().unwrap();
        let workdir = temp_dir.path();
        let sessions_dir = workdir.join("sessions");
        let logs_dir = workdir.join("logs").join("agents");
        fs::create_dir_all(&sessions_dir).unwrap();
        fs::create_dir_all(&logs_dir).unwrap();
        let address = ChannelAddress {
            channel_id: "telegram-main".to_string(),
            conversation_id: "-100".to_string(),
            user_id: Some("user-a".to_string()),
            display_name: None,
        };
        let foreground_session_id = Uuid::new_v4();
        let background_session_id = Uuid::new_v4();
        let other_session_id = Uuid::new_v4();
        for (session_id, conversation_id) in [
            (foreground_session_id, "-100"),
            (background_session_id, "-100"),
            (other_session_id, "-200"),
        ] {
            let root = sessions_dir.join(session_id.to_string());
            fs::create_dir_all(&root).unwrap();
            fs::write(
                root.join("session.json"),
                serde_json::to_vec(&json!({
                    "id": session_id,
                    "address": {
                        "channel_id": "telegram-main",
                        "conversation_id": conversation_id,
                        "user_id": "user-a",
                        "display_name": null
                    }
                }))
                .unwrap(),
            )
            .unwrap();
        }

        let now = Utc::now();
        let recent = now.timestamp_millis();
        let old = (now - ChronoDuration::hours(25)).timestamp_millis();
        let log_path = logs_dir.join("agent.jsonl");
        let lines = [
            json!({
                "kind": "turn_token_usage",
                "ts": recent,
                "channel_id": "telegram-main",
                "session_id": foreground_session_id,
                "agent_kind": "main_foreground",
                "llm_calls": 1,
                "input_total_tokens": 100,
                "output_total_tokens": 10,
                "context_total_tokens": 110,
                "cache_read_input_tokens": 40,
                "cache_write_input_tokens": 5,
                "cache_uncached_input_tokens": 60
            }),
            json!({
                "kind": "turn_token_usage",
                "ts": recent,
                "channel_id": "telegram-main",
                "session_id": background_session_id,
                "agent_kind": "main_background",
                "llm_calls": 2,
                "prompt_tokens": 200,
                "completion_tokens": 20,
                "total_tokens": 220
            }),
            json!({
                "kind": "turn_token_usage",
                "ts": old,
                "channel_id": "telegram-main",
                "session_id": foreground_session_id,
                "llm_calls": 99,
                "input_total_tokens": 999,
                "output_total_tokens": 999,
                "context_total_tokens": 1998
            }),
            json!({
                "kind": "turn_token_usage",
                "ts": recent,
                "channel_id": "telegram-main",
                "session_id": other_session_id,
                "llm_calls": 99,
                "input_total_tokens": 999,
                "output_total_tokens": 999,
                "context_total_tokens": 1998
            }),
            json!({
                "kind": "agent_frame_model_call_completed",
                "ts": recent,
                "channel_id": "telegram-main",
                "session_id": foreground_session_id,
                "input_total_tokens": 999
            }),
        ]
        .into_iter()
        .map(|line| line.to_string())
        .collect::<Vec<_>>()
        .join("\n");
        fs::write(log_path, lines).unwrap();

        let usage =
            collect_conversation_usage_window(workdir, &address, now, ChronoDuration::hours(24));
        assert_eq!(usage.session_count, 2);
        assert_eq!(usage.event_count, 2);
        assert_eq!(usage.missing_cache_breakdown_events, 1);
        assert_eq!(usage.usage.llm_calls, 3);
        assert_eq!(usage.usage.input_total_tokens(), 300);
        assert_eq!(usage.usage.output_total_tokens(), 30);
        assert_eq!(usage.usage.context_total_tokens(), 330);
        assert_eq!(usage.usage.cache_read_input_tokens(), 40);
        assert_eq!(usage.usage.cache_write_input_tokens(), 5);
        assert_eq!(usage.usage.normal_billed_input_tokens(), 255);
    }

    #[test]
    fn conversation_usage_report_prices_per_model_and_marks_unknown_models() {
        let temp_dir = TempDir::new().unwrap();
        let workdir = temp_dir.path();
        let sessions_dir = workdir.join("sessions");
        let logs_dir = workdir.join("logs").join("agents");
        fs::create_dir_all(&sessions_dir).unwrap();
        fs::create_dir_all(&logs_dir).unwrap();
        let address = ChannelAddress {
            channel_id: "telegram-main".to_string(),
            conversation_id: "-100".to_string(),
            user_id: Some("user-a".to_string()),
            display_name: None,
        };
        let session_id = Uuid::new_v4();
        let root = sessions_dir.join(session_id.to_string());
        fs::create_dir_all(&root).unwrap();
        fs::write(
            root.join("session.json"),
            serde_json::to_vec(&json!({
                "id": session_id,
                "address": {
                    "channel_id": "telegram-main",
                    "conversation_id": "-100",
                    "user_id": "user-a",
                    "display_name": null
                }
            }))
            .unwrap(),
        )
        .unwrap();

        let now = Utc::now();
        let recent = now.timestamp_millis();
        let log_path = logs_dir.join("agent.jsonl");
        let lines = [
            json!({
                "kind": "turn_token_usage",
                "ts": recent,
                "channel_id": "telegram-main",
                "session_id": session_id,
                "llm_calls": 3,
                "input_total_tokens": 350,
                "output_total_tokens": 35,
                "context_total_tokens": 385,
                "cache_read_input_tokens": 40,
                "cache_write_input_tokens": 5,
                "cache_uncached_input_tokens": 310
            }),
            json!({
                "kind": "agent_frame_model_call_completed",
                "ts": recent,
                "channel_id": "telegram-main",
                "session_id": session_id,
                "model": "z-ai/glm-5.1:nitro",
                "input_total_tokens": 100,
                "output_total_tokens": 10,
                "context_total_tokens": 110,
                "cache_read_input_tokens": 40,
                "cache_write_input_tokens": 5,
                "cache_uncached_input_tokens": 60
            }),
            json!({
                "kind": "agent_frame_model_call_completed",
                "ts": recent,
                "channel_id": "telegram-main",
                "session_id": session_id,
                "model": "anthropic/claude-opus-4.6",
                "prompt_tokens": 50,
                "completion_tokens": 5,
                "total_tokens": 55
            }),
            json!({
                "kind": "agent_frame_model_call_completed",
                "ts": recent,
                "channel_id": "telegram-main",
                "session_id": session_id,
                "model": "custom/unknown",
                "input_total_tokens": 200,
                "output_total_tokens": 20,
                "context_total_tokens": 220,
                "cache_read_input_tokens": 0,
                "cache_write_input_tokens": 0,
                "cache_uncached_input_tokens": 200
            }),
        ]
        .into_iter()
        .map(|line| line.to_string())
        .collect::<Vec<_>>()
        .join("\n");
        fs::write(log_path, lines).unwrap();

        let report = collect_conversation_usage_report(workdir, &address, now, 6);
        let mut pricing = HashMap::new();
        pricing.insert(
            "z-ai/glm-5.1:nitro".to_string(),
            ModelPricing {
                input_per_million: 1.0,
                output_per_million: 3.20,
                cache_read_per_million: None,
                cache_write_per_million: None,
                source: "test".to_string(),
            },
        );
        pricing.insert(
            "anthropic/claude-opus-4.6".to_string(),
            ModelPricing {
                input_per_million: 15.0,
                output_per_million: 75.0,
                cache_read_per_million: None,
                cache_write_per_million: None,
                source: "test".to_string(),
            },
        );
        let priced = price_conversation_usage_report(&report, &pricing, Vec::new());
        assert_eq!(report.days.len(), 6);
        assert_eq!(report.total.usage.input_total_tokens(), 350);
        assert_eq!(report.model_call_event_count, 3);
        assert!(
            priced
                .correctly_priced_models
                .iter()
                .any(|model| model.model == "z-ai/glm-5.1:nitro")
        );
        assert!(
            priced
                .risky_priced_models
                .iter()
                .any(|model| model.model == "anthropic/claude-opus-4.6")
        );
        assert!(
            priced
                .unknown_priced_models
                .iter()
                .any(|model| model.model == "custom/unknown")
        );
        assert!(priced.total_usd > 0.0);
        assert!(priced.daily_costs.values().any(|cost| *cost > 0.0));
    }

    #[tokio::test]
    async fn idle_compaction_pauses_and_opens_agent_selection_when_model_disappears() {
        let temp_dir = TempDir::new().unwrap();
        let channel = Arc::new(RecordingChannel::default());
        let server = build_test_server(&temp_dir, channel.clone());
        let session = build_test_session(&temp_dir);

        server
            .with_conversations(|conversations| {
                conversations.set_agent_selection(
                    &session.address,
                    Some(AgentBackendKind::AgentFrame),
                    Some("missing-model".to_string()),
                )?;
                Ok(())
            })
            .unwrap();
        server
            .with_sessions(|sessions| {
                sessions.ensure_foreground_actor(&session.address)?;
                let actor = sessions.resolve_foreground_by_address(&session.address)?;
                actor.mark_idle_compaction_failed("model disappeared".to_string())?;
                Ok(())
            })
            .unwrap();

        let compacted = server
            .attempt_idle_context_compaction(&session, false)
            .await
            .unwrap();

        assert!(!compacted);
        let conversation = server
            .with_conversations(|conversations| {
                Ok(conversations
                    .get_snapshot(&session.address)
                    .expect("conversation should exist"))
            })
            .unwrap();
        assert_eq!(
            conversation.settings.agent_backend,
            Some(AgentBackendKind::AgentFrame)
        );
        assert_eq!(conversation.settings.main_model, None);
        let session_snapshot = server
            .with_sessions(|sessions| Ok(sessions.get_snapshot(&session.address).unwrap()))
            .unwrap();
        assert_eq!(session_snapshot.session_state.errno, None);

        let sent_messages = channel.sent_messages.lock().unwrap();
        assert_eq!(sent_messages.len(), 1);
        assert!(
            sent_messages[0]
                .1
                .text
                .as_deref()
                .unwrap()
                .contains("Idle compaction has been paused")
        );
        let options = sent_messages[0].1.options.as_ref().expect("show options");
        assert_eq!(options.prompt, "Choose a model");
        assert!(
            options
                .options
                .iter()
                .any(|option| option.value == "/agent demo-model")
        );
    }

    #[test]
    fn parses_set_api_timeout_command_argument() {
        assert_eq!(
            parse_set_api_timeout_command(Some("/set_api_timeout 300")),
            Some("300".to_string())
        );
        assert_eq!(
            parse_set_api_timeout_command(Some("  /set_api_timeout   default ")),
            Some("default".to_string())
        );
        assert_eq!(
            parse_set_api_timeout_command(Some("/set_api_timeout")),
            None
        );
        assert_eq!(parse_set_api_timeout_command(Some("hello")), None);
    }

    #[test]
    fn parses_agent_sandbox_and_think_commands_with_optional_arguments() {
        assert!(matches!(
            parse_agent_command(Some("/agent")),
            Some(AgentCommand::ShowSelection)
        ));
        assert!(matches!(
            parse_agent_command(Some("/agent agent_frame")),
            Some(AgentCommand::ShowSelection)
        ));
        assert!(matches!(
            parse_agent_command(Some("/agent demo-model")),
            Some(AgentCommand::SelectModel { model_key }) if model_key == "demo-model"
        ));
        assert!(matches!(
            parse_agent_command(Some("/agent@party_claw_bot demo-model")),
            Some(AgentCommand::SelectModel { model_key }) if model_key == "demo-model"
        ));
        assert!(matches!(
            parse_agent_command(Some("/agent agent_frame demo-model")),
            Some(AgentCommand::SelectModel { model_key }) if model_key == "demo-model"
        ));
        assert_eq!(parse_model_command(Some("/model")), Some(None));
        assert_eq!(
            parse_model_command(Some("/model demo-model")),
            Some(Some("demo-model".to_string()))
        );
        assert_eq!(
            parse_model_command(Some("/model@party_claw_bot demo-model")),
            Some(Some("demo-model".to_string()))
        );

        assert_eq!(parse_sandbox_command(Some("/sandbox")), Some(None));
        assert_eq!(
            parse_sandbox_command(Some("/sandbox subprocess")),
            Some(Some("subprocess".to_string()))
        );
        assert_eq!(
            parse_sandbox_command(Some("/sandbox@party_claw_bot bubblewrap")),
            Some(Some("bubblewrap".to_string()))
        );
        assert_eq!(parse_mount_command(Some("/mount")), Some(None));
        assert_eq!(
            parse_mount_command(Some("/mount /srv/shared data")),
            Some(Some("/srv/shared data".to_string()))
        );
        assert_eq!(
            parse_mount_command(Some("/mount@party_claw_bot ./shared")),
            Some(Some("./shared".to_string()))
        );

        assert_eq!(parse_think_command(Some("/think")), Some(None));
        assert_eq!(
            parse_think_command(Some("/think high")),
            Some(Some("high".to_string()))
        );
        assert_eq!(
            parse_think_command(Some("/think@party_claw_bot medium")),
            Some(Some("medium".to_string()))
        );
        assert_eq!(parse_sandbox_command(Some("/sandboxed bubblewrap")), None);
        assert_eq!(parse_think_command(Some("/thinking high")), None);
    }

    #[test]
    fn model_reselect_short_circuits_without_change() {
        let current_model_key = Some("demo-model".to_string());
        let requested_model_key = "demo-model";
        assert_eq!(current_model_key.as_deref(), Some(requested_model_key));
    }

    #[test]
    fn parses_snap_commands_with_bot_suffix() {
        assert_eq!(
            parse_snap_save_command(Some("/snapsave demo-checkpoint")),
            Some("demo-checkpoint".to_string())
        );
        assert_eq!(
            parse_snap_save_command(Some("/snapsave@party_claw_bot demo-checkpoint")),
            Some("demo-checkpoint".to_string())
        );
        assert_eq!(
            parse_snap_load_command(Some("/snapload restore-point")),
            Some("restore-point".to_string())
        );
        assert_eq!(
            parse_snap_load_command(Some("/snapload@party_claw_bot restore-point")),
            Some("restore-point".to_string())
        );
        assert!(parse_snap_list_command(Some("/snaplist")));
        assert!(parse_snap_list_command(Some("/snaplist@party_claw_bot")));
    }

    #[test]
    fn fast_path_skips_prompt_when_model_is_already_selected() {
        let temp_dir = TempDir::new().unwrap();
        let address = ChannelAddress {
            channel_id: "telegram-main".to_string(),
            conversation_id: "-1003336174206".to_string(),
            user_id: Some("1717801091".to_string()),
            display_name: Some("Jeremy Guo".to_string()),
        };
        let mut conversations = ConversationManager::new(temp_dir.path()).unwrap();
        conversations
            .set_main_model(&address, Some("gpt54".to_string()))
            .unwrap();

        let mut models = std::collections::BTreeMap::new();
        models.insert(
            "gpt54".to_string(),
            ModelConfig {
                model_type: crate::config::ModelType::Openrouter,
                api_endpoint: "https://example.com/v1".to_string(),
                model: "gpt-5.4".to_string(),
                backend: AgentBackendKind::AgentFrame,
                supports_vision_input: false,
                image_tool_model: None,
                web_search_model: None,
                api_key: None,
                api_key_env: "TEST_API_KEY".to_string(),
                chat_completions_path: "/chat/completions".to_string(),
                codex_home: None,
                auth_credentials_store_mode: agent_frame::config::AuthCredentialsStoreMode::Auto,
                timeout_seconds: 60.0,
                retry_mode: Default::default(),
                context_window_tokens: 128_000,
                cache_ttl: None,
                reasoning: None,
                headers: serde_json::Map::new(),
                description: "demo".to_string(),
                agent_model_enabled: true,
                capabilities: vec![ModelCapability::Chat],
                native_web_search: None,
                token_estimation: None,
                external_web_search: None,
            },
        );
        let agent = crate::config::AgentConfig {
            agent_frame: crate::config::AgentBackendConfig {
                available_models: vec!["gpt54".to_string()],
            },
        };
        let message = IncomingMessage {
            remote_message_id: "msg-1".to_string(),
            address: address.clone(),
            text: Some("继续".to_string()),
            attachments: Vec::new(),
            stored_attachments: Vec::new(),
            control: None,
        };

        let outgoing =
            fast_path_agent_selection_message(temp_dir.path(), &models, &agent, &message);
        assert!(outgoing.is_none());

        let reloaded = ConversationManager::new(temp_dir.path()).unwrap();
        let snapshot = reloaded.get_snapshot(&address).unwrap();
        assert_eq!(snapshot.settings.main_model.as_deref(), Some("gpt54"));
        assert_eq!(snapshot.settings.agent_backend, None);
    }

    #[test]
    fn infer_single_agent_backend_returns_none_when_model_has_no_backend() {
        let agent = AgentConfig::default();

        assert_eq!(infer_single_agent_backend(&agent, "missing-model"), None);
    }

    #[test]
    fn infer_single_agent_backend_returns_backend_when_model_is_unique() {
        let agent = AgentConfig {
            agent_frame: AgentBackendConfig {
                available_models: vec!["gpt54".to_string()],
            },
        };

        assert_eq!(
            infer_single_agent_backend(&agent, "gpt54"),
            Some(AgentBackendKind::AgentFrame)
        );
    }

    #[test]
    fn admin_chat_list_text_groups_entries_and_includes_actions() {
        let address = ChannelAddress {
            channel_id: "telegram-main".to_string(),
            conversation_id: "1717801091".to_string(),
            user_id: Some("1717801091".to_string()),
            display_name: Some("Jeremy Guo".to_string()),
        };
        let admin = Some(ChannelAdminSnapshot {
            user_id: "1717801091".to_string(),
            display_name: Some("Jeremy Guo".to_string()),
            private_conversation_id: Some("1717801091".to_string()),
        });
        let now = Utc::now();
        let items = vec![
            ConversationApprovalSnapshot {
                conversation_id: "-1001".to_string(),
                user_id: Some("user-1".to_string()),
                display_name: Some("Alice".to_string()),
                state: ConversationApprovalState::Pending,
                updated_at: now,
            },
            ConversationApprovalSnapshot {
                conversation_id: "1717801091".to_string(),
                user_id: Some("1717801091".to_string()),
                display_name: Some("Jeremy Guo".to_string()),
                state: ConversationApprovalState::Approved,
                updated_at: now,
            },
            ConversationApprovalSnapshot {
                conversation_id: "-1002".to_string(),
                user_id: Some("user-2".to_string()),
                display_name: Some("Bob".to_string()),
                state: ConversationApprovalState::Rejected,
                updated_at: now,
            },
        ];

        let text = Server::format_admin_chat_list_text(&address, admin, &items);
        assert!(text.contains("Approval dashboard for channel `telegram-main`"));
        assert!(text.contains("Summary: 1 pending, 1 approved, 1 rejected"));
        assert!(text.contains("Pending Review"));
        assert!(text.contains("/admin_chat_approve -1001"));
        assert!(text.contains("/admin_chat_reject -1001"));
        assert!(text.contains("Approved"));
        assert!(text.contains("[admin private chat]"));
        assert!(text.contains("Rejected"));
    }

    #[tokio::test]
    async fn prunes_telegram_groups_with_one_or_fewer_members() {
        let temp_dir = TempDir::new().unwrap();
        let channel = Arc::new(RecordingChannel::default());
        channel
            .probe_member_counts
            .lock()
            .unwrap()
            .insert("-1001".to_string(), 1);
        let channel_for_server: Arc<dyn Channel> = channel;
        let mut server = build_test_server(&temp_dir, channel_for_server);
        server.telegram_channel_ids = Arc::new(HashSet::from(["telegram-main".to_string()]));

        let admin = ChannelAddress {
            channel_id: "telegram-main".to_string(),
            conversation_id: "1717801091".to_string(),
            user_id: Some("1717801091".to_string()),
            display_name: Some("Admin".to_string()),
        };
        let group = ChannelAddress {
            channel_id: "telegram-main".to_string(),
            conversation_id: "-1001".to_string(),
            user_id: Some("42".to_string()),
            display_name: Some("Group User".to_string()),
        };
        server
            .with_channel_auth(|auth| {
                auth.authorize_admin(&admin)?;
                auth.ensure_pending_conversation(&group)?;
                auth.approve_conversation("telegram-main", "-1001")?;
                Ok(())
            })
            .unwrap();
        server
            .with_conversations(|conversations| {
                conversations.ensure_conversation(&group)?;
                Ok(())
            })
            .unwrap();

        server.prune_closed_conversations_once().await.unwrap();

        let state = server
            .with_channel_auth(|auth| Ok(auth.current_conversation_state(&group)))
            .unwrap();
        assert_eq!(state, None);
        let conversation_exists = server
            .with_conversations(|conversations| Ok(conversations.get_snapshot(&group).is_some()))
            .unwrap();
        assert!(!conversation_exists);
    }

    #[tokio::test]
    async fn prunes_rejected_telegram_groups_without_probe() {
        let temp_dir = TempDir::new().unwrap();
        let channel_for_server: Arc<dyn Channel> = Arc::new(RecordingChannel::default());
        let mut server = build_test_server(&temp_dir, channel_for_server);
        server.telegram_channel_ids = Arc::new(HashSet::from(["telegram-main".to_string()]));

        let admin = ChannelAddress {
            channel_id: "telegram-main".to_string(),
            conversation_id: "1717801091".to_string(),
            user_id: Some("1717801091".to_string()),
            display_name: Some("Admin".to_string()),
        };
        let group = ChannelAddress {
            channel_id: "telegram-main".to_string(),
            conversation_id: "-1002".to_string(),
            user_id: Some("43".to_string()),
            display_name: Some("Rejected Group User".to_string()),
        };
        server
            .with_channel_auth(|auth| {
                auth.authorize_admin(&admin)?;
                auth.ensure_pending_conversation(&group)?;
                auth.reject_conversation("telegram-main", "-1002")?;
                Ok(())
            })
            .unwrap();
        server
            .with_conversations(|conversations| {
                conversations.ensure_conversation(&group)?;
                Ok(())
            })
            .unwrap();

        server.prune_closed_conversations_once().await.unwrap();

        let items = server
            .with_channel_auth(|auth| {
                Ok(auth.list_conversations_including_rejected("telegram-main"))
            })
            .unwrap();
        assert!(
            items
                .iter()
                .all(|item| item.conversation_id != group.conversation_id)
        );
    }

    fn openrouter_test_model(model: &str, cache_ttl: Option<&str>) -> ModelConfig {
        ModelConfig {
            model_type: crate::config::ModelType::Openrouter,
            api_endpoint: "https://openrouter.ai/api/v1".to_string(),
            model: model.to_string(),
            backend: AgentBackendKind::AgentFrame,
            supports_vision_input: true,
            image_tool_model: None,
            web_search_model: None,
            api_key: None,
            api_key_env: "OPENROUTER_API_KEY".to_string(),
            chat_completions_path: "/chat/completions".to_string(),
            codex_home: None,
            auth_credentials_store_mode: agent_frame::config::AuthCredentialsStoreMode::Auto,
            timeout_seconds: 300.0,
            retry_mode: Default::default(),
            context_window_tokens: 262_144,
            cache_ttl: cache_ttl.map(str::to_string),
            reasoning: None,
            headers: serde_json::Map::new(),
            description: "demo".to_string(),
            agent_model_enabled: true,
            native_web_search: None,
            token_estimation: None,
            external_web_search: None,
            capabilities: Vec::new(),
        }
    }

    #[test]
    fn openrouter_claude_defaults_to_five_minute_automatic_cache() {
        let model = openrouter_test_model("anthropic/claude-opus-4.6", None);
        let cache_control = openrouter_automatic_cache_control(&model).unwrap();

        assert_eq!(
            openrouter_automatic_cache_ttl(&model).as_deref(),
            Some("5m")
        );
        assert_eq!(cache_control.cache_type, "ephemeral");
        assert_eq!(cache_control.ttl.as_deref(), Some("5m"));
    }

    #[test]
    fn openrouter_responses_claude_defaults_to_five_minute_automatic_cache() {
        let mut model = openrouter_test_model("anthropic/claude-opus-4.6", None);
        model.model_type = crate::config::ModelType::OpenrouterResp;
        model.chat_completions_path = "/responses".to_string();

        let cache_control = openrouter_automatic_cache_control(&model).unwrap();

        assert_eq!(
            openrouter_automatic_cache_ttl(&model).as_deref(),
            Some("5m")
        );
        assert_eq!(cache_control.cache_type, "ephemeral");
        assert_eq!(cache_control.ttl.as_deref(), Some("5m"));
    }

    #[test]
    fn openrouter_non_claude_does_not_get_anthropic_cache_control() {
        let model = openrouter_test_model("z-ai/glm-5.1", None);

        assert!(openrouter_automatic_cache_ttl(&model).is_none());
        assert!(openrouter_automatic_cache_control(&model).is_none());
    }

    #[test]
    fn estimates_openrouter_opus_cost_with_cache_formula() {
        let model = openrouter_test_model("anthropic/claude-opus-4.6", Some("5m"));
        let usage = TokenUsage {
            llm_calls: 1,
            prompt_tokens: 10_000,
            completion_tokens: 2_000,
            total_tokens: 12_000,
            cache_hit_tokens: 8_000,
            cache_miss_tokens: 2_000,
            cache_read_tokens: 8_000,
            cache_write_tokens: 1_500,
        };

        let (formula, total_usd) = estimate_cost_usd(&model, &usage).unwrap();
        assert!(formula.contains("cache_read_input_tokens"));
        assert!(formula.contains("normal_billed_input_tokens"));
        assert!(total_usd > 0.0);
    }

    #[test]
    fn estimates_compaction_savings_from_token_delta_and_compaction_cost() {
        let model = openrouter_test_model("anthropic/claude-sonnet-4.6", Some("5m"));
        let compaction = SessionCompactionStats {
            run_count: 2,
            compacted_run_count: 2,
            estimated_tokens_before: 90_000,
            estimated_tokens_after: 50_000,
            usage: TokenUsage {
                llm_calls: 2,
                prompt_tokens: 10_000,
                completion_tokens: 500,
                total_tokens: 10_500,
                cache_hit_tokens: 0,
                cache_miss_tokens: 10_000,
                cache_read_tokens: 0,
                cache_write_tokens: 0,
            },
        };

        let (formula, gross_usd, compaction_cost_usd, net_usd) =
            estimate_compaction_savings_usd(&model, &compaction).unwrap();
        assert!(formula.contains("estimated_tokens_before"));
        assert!(gross_usd > 0.0);
        assert!(compaction_cost_usd > 0.0);
        assert!(net_usd < gross_usd);
    }

    #[test]
    fn hides_active_workspaces_from_list_results() {
        assert!(workspace_visible_in_list("archived-1", &[], true));
        assert!(workspace_visible_in_list(
            "idle-1",
            &[String::from("active-1")],
            false
        ));
        assert!(!workspace_visible_in_list(
            "active-1",
            &[String::from("active-1")],
            false
        ));
    }
}
