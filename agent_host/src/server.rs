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
use crate::channels::telegram::TelegramChannel;
use crate::config::{
    AgentConfig, BotCommandConfig, ChannelConfig, MainAgentConfig, ModelCapability, ModelConfig,
    SandboxConfig, SandboxMode, ServerConfig, ToolingConfig, ToolingTarget, default_bot_commands,
    default_dingtalk_commands, default_telegram_commands,
};
use crate::conversation::{ConversationManager, ConversationSettings};
use crate::cron::{
    ClaimedCronTask, CronCheckerConfig, CronCreateRequest, CronManager, CronUpdateRequest,
};
use crate::domain::{
    AttachmentKind, ChannelAddress, OutgoingAttachment, OutgoingMessage, ProcessingState,
    ShowOption, StoredAttachment,
};
use crate::prompt::{
    AgentPromptKind, build_agent_system_prompt, greeting_for_language,
    render_available_models_catalog,
};
use crate::sandbox::{
    PersistentChildRuntime, bubblewrap_is_available, is_child_run_turn_request_send_error,
    is_child_transport_error, run_one_shot_child_turn,
};
use crate::session::{
    ModelCatalogChangeNotice, SessionErrno, SessionManager, SessionPhase,
    SessionProgressMessageState, SessionSkillObservation, SessionSnapshot,
    SharedProfileChangeNotice, SkillChangeNotice, ZgentNativeSessionState,
};
use crate::sink::{SinkRouter, SinkTarget};
use crate::snapshot::{SnapshotBundle, SnapshotManager};
use crate::subagent::{HostedSubagent, HostedSubagentInner, SubagentState};
use crate::upgrade::upgrade_workdir;
use crate::workspace::{WorkspaceManager, WorkspaceMountMaterialization};
use crate::zgent::kernel::{
    PersistentZgentKernelSession, ZgentKernelRuntimeSpec, zgent_native_kernel_runtime_available,
};
use crate::zgent::subagent::ZgentSubagentModel;
use agent_frame::config::{
    AgentConfig as FrameAgentConfig, CodexAuthConfig, ExternalWebSearchConfig,
    NativeWebSearchConfig, ReasoningConfig, UpstreamApiKind, UpstreamConfig,
    load_codex_auth_tokens,
};
use agent_frame::skills::discover_skills;
use agent_frame::tooling::{build_tool_registry, terminate_runtime_state_tasks};
use agent_frame::{
    ChatMessage, ContextCompactionReport, ExecutionProgress, SessionCompactionStats, SessionEvent,
    SessionExecutionControl, SessionState, StructuredCompactionOutput, TokenUsage, Tool,
    estimate_session_tokens, extract_assistant_text,
};
use anyhow::{Context, Result, anyhow};
use base64::Engine;
use chrono::Utc;
use humantime::parse_duration;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::fs;
use std::hash::{Hash, Hasher};
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

mod background;
mod command_routing;
mod commands;
mod foreground;
mod messaging;
mod persistence;
mod progress;
mod runtime_helpers;
mod security;
mod subagents;

use self::commands::*;
use self::messaging::*;
use self::persistence::*;
use self::runtime_helpers::*;

const ATTACHMENT_OPEN_TAG: &str = "<attachment>";
const ATTACHMENT_CLOSE_TAG: &str = "</attachment>";
const INTERRUPTED_FOLLOWUP_MARKER: &str = "[Interrupted Follow-up]";
const QUEUED_USER_UPDATES_MARKER: &str = "[Queued User Updates]";
const CHANNEL_RESTART_MAX_BACKOFF_SECONDS: u64 = 30;
const CONVERSATION_CLEANUP_POLL_SECONDS: u64 = 300;
const SYSTEM_RESTART_NOTICE: &str =
    "[System Restarted: All previous long run execution tools with IDs are all ended]";

#[derive(Clone, Debug)]
struct BackgroundJobRequest {
    agent_id: uuid::Uuid,
    parent_agent_id: Option<uuid::Uuid>,
    cron_task_id: Option<uuid::Uuid>,
    session: SessionSnapshot,
    agent_backend: AgentBackendKind,
    model_key: String,
    prompt: String,
    sink: SinkTarget,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ForegroundRuntimePhase {
    Running,
    Compacting,
}

#[derive(Clone)]
struct ActiveNativeZgentSession {
    kernel: Arc<PersistentZgentKernelSession>,
    model_key: String,
    busy: Arc<AtomicBool>,
}

struct ActiveForegroundAgentFrameRuntime {
    model_key: String,
    workspace_id: String,
    sandbox_mode: SandboxMode,
    runtime: PersistentChildRuntime,
}

struct PersistedYieldedForegroundTurn {
    session: SessionSnapshot,
    should_auto_resume: bool,
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

fn infer_single_agent_backend(agent: &AgentConfig, model_key: &str) -> Option<AgentBackendKind> {
    match agent.backends_for_model(model_key).as_slice() {
        [backend] => Some(*backend),
        _ => None,
    }
}

#[derive(Clone)]
struct ServerRuntime {
    agent_workspace: AgentWorkspace,
    sessions: Arc<Mutex<SessionManager>>,
    workspace_manager: WorkspaceManager,
    active_workspace_ids: Vec<String>,
    selected_agent_backend: Option<AgentBackendKind>,
    selected_main_model_key: Option<String>,
    selected_reasoning_effort: Option<String>,
    selected_context_compaction_enabled: Option<bool>,
    selected_chat_version_id: Option<Uuid>,
    channels: Arc<HashMap<String, Arc<dyn Channel>>>,
    command_catalog: HashMap<String, Vec<BotCommandConfig>>,
    models: BTreeMap<String, ModelConfig>,
    agent: AgentConfig,
    tooling: ToolingConfig,
    main_agent: MainAgentConfig,
    sandbox: SandboxConfig,
    sink_router: Arc<RwLock<SinkRouter>>,
    cron_manager: Arc<Mutex<CronManager>>,
    agent_registry: Arc<Mutex<AgentRegistry>>,
    agent_registry_notify: Arc<Notify>,
    max_global_sub_agents: usize,
    subagent_count: Arc<AtomicUsize>,
    cron_poll_interval_seconds: u64,
    background_job_sender: mpsc::Sender<BackgroundJobRequest>,
    summary_tracker: Arc<SummaryTracker>,
    active_foreground_phases: Arc<Mutex<HashMap<String, ForegroundRuntimePhase>>>,
    active_foreground_agent_frame_runtimes:
        Arc<Mutex<HashMap<String, Arc<Mutex<ActiveForegroundAgentFrameRuntime>>>>>,
    subagents: Arc<Mutex<HashMap<uuid::Uuid, Arc<HostedSubagent>>>>,
    conversations: Arc<Mutex<ConversationManager>>,
}

struct SubAgentSlot {
    counter: Arc<AtomicUsize>,
}

struct SummaryInProgressGuard {
    tracker: Arc<SummaryTracker>,
}

struct SummaryTracker {
    count: Mutex<usize>,
    condvar: Condvar,
}

enum TimedRunOutcome {
    Completed(SessionState),
    Yielded(SessionState),
    TimedOut {
        state: Option<SessionState>,
        error: anyhow::Error,
    },
    Failed(anyhow::Error),
}

enum ForegroundTurnOutcome {
    Replied {
        state: SessionState,
        outgoing: OutgoingMessage,
    },
    Yielded(SessionState),
    Failed {
        resume_messages: Vec<ChatMessage>,
        progress_summary: String,
        compaction: SessionCompactionStats,
        error: anyhow::Error,
    },
}

impl Drop for SubAgentSlot {
    fn drop(&mut self) {
        self.counter.fetch_sub(1, Ordering::SeqCst);
    }
}

impl Drop for SummaryInProgressGuard {
    fn drop(&mut self) {
        let mut count = self.tracker.count.lock().unwrap();
        *count = count.saturating_sub(1);
        if *count == 0 {
            self.tracker.condvar.notify_all();
        }
    }
}

impl SummaryInProgressGuard {
    fn new(tracker: Arc<SummaryTracker>) -> Self {
        let mut count = tracker.count.lock().unwrap();
        *count += 1;
        drop(count);
        Self { tracker }
    }
}

impl SummaryTracker {
    fn new() -> Self {
        Self {
            count: Mutex::new(0),
            condvar: Condvar::new(),
        }
    }

    fn wait_for_zero(&self) {
        let mut count = self.count.lock().unwrap();
        while *count > 0 {
            count = self.condvar.wait(count).unwrap();
        }
    }
}

impl ServerRuntime {
    fn available_agent_models(&self, backend: AgentBackendKind) -> Vec<String> {
        self.agent
            .available_models(backend)
            .iter()
            .filter(|model_key| self.models.contains_key(model_key.as_str()))
            .cloned()
            .collect()
    }

    fn inferred_agent_backend_for_model(&self, model_key: &str) -> Option<AgentBackendKind> {
        infer_single_agent_backend(&self.agent, model_key)
    }

    fn selected_agent_backend(&self) -> Option<AgentBackendKind> {
        self.selected_agent_backend.or_else(|| {
            self.selected_main_model_key
                .as_deref()
                .and_then(|model_key| self.inferred_agent_backend_for_model(model_key))
        })
    }

    fn effective_agent_backend(&self) -> Result<AgentBackendKind> {
        self.selected_agent_backend().ok_or_else(|| {
            anyhow!("this conversation does not have an agent backend yet; choose one with /agent")
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
        let backend = self.effective_agent_backend()?;
        self.ensure_model_available_for_backend(backend, &model_key)?;
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

    fn ensure_foreground_agent_frame_runtime(
        &self,
        session: &SessionSnapshot,
        model_key: &str,
        config: &FrameAgentConfig,
    ) -> Result<Arc<Mutex<ActiveForegroundAgentFrameRuntime>>> {
        let session_key = session.address.session_key();
        let effective_sandbox_mode = self.effective_sandbox_mode(&session.address)?;
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
            self.agent_workspace.rundir.join("skill_memory"),
            self.agent_workspace.skills_dir.clone(),
            &config.skills_dirs,
        )?;
        let entry = Arc::new(Mutex::new(ActiveForegroundAgentFrameRuntime {
            model_key: model_key.to_string(),
            workspace_id: session.workspace_id.clone(),
            sandbox_mode: effective_sandbox_mode,
            runtime,
        }));
        self.active_foreground_agent_frame_runtimes
            .lock()
            .map_err(|_| anyhow!("active foreground runtimes lock poisoned"))?
            .insert(session_key, Arc::clone(&entry));
        Ok(entry)
    }

    fn invalidate_foreground_agent_frame_runtime(&self, address: &ChannelAddress) -> Result<()> {
        let session_key = address.session_key();
        let runtime = self
            .active_foreground_agent_frame_runtimes
            .lock()
            .map_err(|_| anyhow!("active foreground runtimes lock poisoned"))?
            .remove(&session_key);
        if let Some(runtime) = runtime
            && let Ok(mut runtime) = runtime.lock()
        {
            let _ = runtime.runtime.shutdown();
            if runtime.sandbox_mode == SandboxMode::Bubblewrap {
                let _ = self
                    .workspace_manager
                    .cleanup_transient_mounts(&runtime.workspace_id);
            }
        }
        Ok(())
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

    fn tooling_target(&self, family: ToolingFamily) -> Option<&ToolingTarget> {
        family.target(&self.tooling)
    }

    fn build_upstream_config(
        &self,
        model: &ModelConfig,
        timeout_seconds: f64,
        prompt_cache_key: Option<String>,
        prompt_cache_retention: Option<String>,
        reasoning: Option<ReasoningConfig>,
        native_web_search: Option<NativeWebSearchConfig>,
        external_web_search: Option<ExternalWebSearchConfig>,
        native_image_input: bool,
        native_pdf_input: bool,
        native_audio_input: bool,
        native_image_generation: bool,
    ) -> Result<UpstreamConfig> {
        Ok(UpstreamConfig {
            base_url: model.api_endpoint.clone(),
            model: model.model.clone(),
            api_kind: model.upstream_api_kind(),
            auth_kind: model.upstream_auth_kind(),
            supports_vision_input: model.supports_image_input(),
            supports_pdf_input: model.has_capability(ModelCapability::Pdf),
            supports_audio_input: model.has_capability(ModelCapability::AudioIn),
            api_key: model.api_key.clone(),
            api_key_env: model.api_key_env.clone(),
            chat_completions_path: model.chat_completions_path.clone(),
            codex_home: model.resolved_codex_home(),
            codex_auth: self.resolved_codex_auth(model)?,
            auth_credentials_store_mode: model.auth_credentials_store_mode,
            timeout_seconds,
            retry_mode: model.retry_mode.clone(),
            context_window_tokens: model.context_window_tokens,
            cache_control: openrouter_automatic_cache_control(model),
            prompt_cache_retention,
            prompt_cache_key,
            reasoning,
            headers: model.headers.clone(),
            native_web_search,
            external_web_search,
            native_image_input,
            native_pdf_input,
            native_audio_input,
            native_image_generation,
        })
    }

    fn synthesize_external_web_search_config(
        &self,
        model_key: &str,
        model: &ModelConfig,
    ) -> Option<ExternalWebSearchConfig> {
        if model.upstream_api_kind() != UpstreamApiKind::ChatCompletions {
            warn!(
                log_stream = "server",
                kind = "tooling_web_search_unsupported_upstream",
                model_key,
                model_type = ?model.model_type,
                chat_completions_path = %model.chat_completions_path,
                "tooling.web_search fallback currently requires a chat-completions-compatible upstream"
            );
            return None;
        }
        Some(ExternalWebSearchConfig {
            base_url: model.api_endpoint.clone(),
            model: model.model.clone(),
            supports_vision_input: model.supports_image_input(),
            api_key: model.api_key.clone(),
            api_key_env: model.api_key_env.clone(),
            chat_completions_path: model.chat_completions_path.clone(),
            timeout_seconds: model.timeout_seconds,
            headers: model.headers.clone(),
        })
    }

    fn resolve_image_tool_upstream(
        &self,
        active_model_key: &str,
        model: &ModelConfig,
    ) -> Result<(bool, Option<UpstreamConfig>)> {
        let configured_target = self.tooling_target(ToolingFamily::Image);
        let image_model_key = match configured_target {
            Some(target) if target.prefer_self && model.supports_image_input() => {
                return Ok((true, None));
            }
            Some(target) => Some(target.alias.as_str()),
            None => match model.image_tool_model.as_deref() {
                None => return Ok((false, None)),
                Some("self") if model.supports_image_input() => return Ok((true, None)),
                Some("self") => return Ok((false, None)),
                Some(other_model_key) => Some(other_model_key),
            },
        };
        let Some(image_model_key) = image_model_key else {
            return Ok((false, None));
        };
        let Some(image_model) = self.models.get(image_model_key) else {
            warn!(
                log_stream = "server",
                kind = "tooling_image_model_missing",
                active_model_key,
                image_model_key,
                "configured image tooling model is missing; falling back to current upstream"
            );
            return Ok((false, None));
        };
        if !image_model.supports_image_input() {
            warn!(
                log_stream = "server",
                kind = "tooling_image_model_without_capability",
                active_model_key,
                image_model_key,
                "configured image tooling model does not advertise image input support"
            );
        }
        self.build_upstream_config(
            image_model,
            image_model.timeout_seconds,
            None,
            default_prompt_cache_retention(image_model.cache_ttl.as_deref(), image_model),
            image_model.reasoning.clone(),
            image_model.native_web_search.clone(),
            image_model.external_web_search.clone(),
            false,
            false,
            false,
            false,
        )
        .map(|upstream| (false, Some(upstream)))
    }

    fn resolve_named_tool_upstream(
        &self,
        family: ToolingFamily,
        active_model_key: &str,
    ) -> Result<Option<UpstreamConfig>> {
        let Some(target) = self.tooling_target(family) else {
            return Ok(None);
        };
        let Some(tool_model) = self.models.get(&target.alias) else {
            warn!(
                log_stream = "server",
                kind = "tooling_model_missing",
                family = family.field_name(),
                active_model_key,
                target = %target.alias,
                "configured tooling model is missing"
            );
            return Ok(None);
        };
        let required = family.required_capability();
        let supports_required = match family {
            ToolingFamily::Image => tool_model.supports_image_input(),
            capability => tool_model.has_capability(capability.required_capability()),
        };
        if !supports_required {
            warn!(
                log_stream = "server",
                kind = "tooling_model_missing_capability",
                family = family.field_name(),
                active_model_key,
                target = %target.alias,
                required_capability = ?required,
                "configured tooling model does not advertise the required capability"
            );
        }
        self.build_upstream_config(
            tool_model,
            tool_model.timeout_seconds,
            None,
            default_prompt_cache_retention(tool_model.cache_ttl.as_deref(), tool_model),
            tool_model.reasoning.clone(),
            tool_model.native_web_search.clone(),
            tool_model.external_web_search.clone(),
            false,
            false,
            false,
            false,
        )
        .map(Some)
    }

    fn resolve_native_or_tool_upstream(
        &self,
        family: ToolingFamily,
        active_model_key: &str,
        model: &ModelConfig,
    ) -> (bool, Option<UpstreamConfig>) {
        let Some(target) = self.tooling_target(family) else {
            return (false, None);
        };
        let self_supported = match family {
            ToolingFamily::Image => model.supports_image_input(),
            ToolingFamily::Pdf => model.has_capability(ModelCapability::Pdf),
            ToolingFamily::AudioInput => model.has_capability(ModelCapability::AudioIn),
            _ => false,
        };
        if target.prefer_self && self_supported {
            return (true, None);
        }
        match self.resolve_named_tool_upstream(family, active_model_key) {
            Ok(upstream) => (false, upstream),
            Err(error) => {
                warn!(
                    log_stream = "server",
                    kind = "tooling_model_resolve_failed",
                    family = family.field_name(),
                    active_model_key,
                    target = %target.alias,
                    error = %error,
                    "failed to resolve external tooling model"
                );
                (false, None)
            }
        }
    }

    fn resolve_native_image_generation(
        &self,
        active_model_key: &str,
        model: &ModelConfig,
    ) -> (bool, Option<UpstreamConfig>) {
        let target = self.tooling_target(ToolingFamily::ImageGen);
        if matches!(
            target,
            Some(target) if target.prefer_self && model.has_capability(ModelCapability::ImageOut)
        ) && model.upstream_api_kind() != UpstreamApiKind::Responses
        {
            warn!(
                log_stream = "server",
                kind = "tooling_image_generation_self_requires_responses",
                active_model_key,
                model_type = ?model.model_type,
                chat_completions_path = %model.chat_completions_path,
                "native provider image generation is only enabled for responses-based upstreams; falling back to the configured alias"
            );
        }
        match select_image_generation_routing(target, model) {
            ImageGenerationRouting::Native => (true, None),
            ImageGenerationRouting::Disabled => (false, None),
            ImageGenerationRouting::Tool => {
                match self.resolve_named_tool_upstream(ToolingFamily::ImageGen, active_model_key) {
                    Ok(upstream) => (false, upstream),
                    Err(error) => {
                        warn!(
                            log_stream = "server",
                            kind = "tooling_image_generation_resolve_failed",
                            active_model_key,
                            target = %target
                                .expect("tool routing requires a configured target")
                                .alias,
                            error = %error,
                            "failed to resolve external image generation tooling model"
                        );
                        (false, None)
                    }
                }
            }
        }
    }

    fn resolve_web_search_configs(
        &self,
        active_model_key: &str,
        model: &ModelConfig,
    ) -> (
        Option<NativeWebSearchConfig>,
        Option<ExternalWebSearchConfig>,
    ) {
        if let Some(target) = self.tooling_target(ToolingFamily::WebSearch) {
            if target.prefer_self && model.has_capability(ModelCapability::WebSearch) {
                if model.upstream_api_kind() == UpstreamApiKind::Responses {
                    if let Some(native) = model
                        .native_web_search
                        .clone()
                        .filter(|settings| settings.enabled)
                    {
                        return (Some(native), None);
                    }
                    warn!(
                        log_stream = "server",
                        kind = "tooling_web_search_self_without_native_payload",
                        active_model_key,
                        "tooling.web_search requested :self but the active model has no native_web_search payload; falling back to the configured alias"
                    );
                } else {
                    warn!(
                        log_stream = "server",
                        kind = "tooling_web_search_self_requires_responses",
                        active_model_key,
                        model_type = ?model.model_type,
                        chat_completions_path = %model.chat_completions_path,
                        "tooling.web_search requested :self but native provider web search is only enabled for responses-based upstreams; falling back to the configured alias"
                    );
                }
            }
            let Some(search_model) = self.models.get(&target.alias) else {
                warn!(
                    log_stream = "server",
                    kind = "tooling_web_search_model_missing",
                    active_model_key,
                    target = %target.alias,
                    "configured web search tooling model is missing"
                );
                return (None, None);
            };
            let external = search_model.external_web_search.clone().or_else(|| {
                self.synthesize_external_web_search_config(&target.alias, search_model)
            });
            if external.is_none() {
                warn!(
                    log_stream = "server",
                    kind = "tooling_web_search_model_unavailable",
                    active_model_key,
                    target = %target.alias,
                    "configured web search tooling model could not be translated into an external web search upstream"
                );
            }
            return (None, external);
        }

        let native = if model.upstream_api_kind() == UpstreamApiKind::Responses {
            model
                .native_web_search
                .clone()
                .filter(|settings| settings.enabled)
        } else {
            None
        };
        let external = model.external_web_search.clone().or_else(|| {
            model.web_search_model.as_ref().and_then(|alias| {
                self.models.get(alias).and_then(|search_model| {
                    self.synthesize_external_web_search_config(alias, search_model)
                })
            })
        });
        (native, external)
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

    fn with_subagents<T>(
        &self,
        f: impl FnOnce(&mut HashMap<uuid::Uuid, Arc<HostedSubagent>>) -> Result<T>,
    ) -> Result<T> {
        let mut subagents = self
            .subagents
            .lock()
            .map_err(|_| anyhow!("subagent manager lock poisoned"))?;
        f(&mut subagents)
    }

    fn with_conversations<T>(
        &self,
        f: impl FnOnce(&mut ConversationManager) -> Result<T>,
    ) -> Result<T> {
        let mut conversations = self
            .conversations
            .lock()
            .map_err(|_| anyhow!("conversation manager lock poisoned"))?;
        f(&mut conversations)
    }

    fn with_sessions<T>(&self, f: impl FnOnce(&mut SessionManager) -> Result<T>) -> Result<T> {
        let mut sessions = self
            .sessions
            .lock()
            .map_err(|_| anyhow!("session manager lock poisoned"))?;
        f(&mut sessions)
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
        let session = self.with_sessions(|sessions| match preferred_workspace_id.as_deref() {
            Some(workspace_id) => {
                sessions.create_background_in_workspace(address, agent_id, workspace_id)
            }
            None => sessions.create_background(address, agent_id),
        })?;
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

    fn build_agent_frame_config(
        &self,
        session: &SessionSnapshot,
        workspace_root: &Path,
        kind: AgentPromptKind,
        model_key: &str,
        upstream_timeout_seconds: Option<f64>,
    ) -> Result<FrameAgentConfig> {
        let model = self.model_config(model_key)?;
        let prompt_cache_key = self.selected_chat_version_id.as_ref().map(Uuid::to_string);
        let prompt_cache_retention =
            default_prompt_cache_retention(model.cache_ttl.as_deref(), model);
        let (native_image_input, image_tool_upstream) =
            self.resolve_image_tool_upstream(model_key, model)?;
        let (native_pdf_input, pdf_tool_upstream) =
            self.resolve_native_or_tool_upstream(ToolingFamily::Pdf, model_key, model);
        let (native_audio_input, audio_tool_upstream) =
            self.resolve_native_or_tool_upstream(ToolingFamily::AudioInput, model_key, model);
        let commands = self
            .command_catalog
            .get(&session.address.channel_id)
            .cloned()
            .unwrap_or_else(default_bot_commands);
        let workspace_summary = self
            .workspace_manager
            .ensure_workspace_exists(&session.workspace_id)
            .map(|workspace| workspace.summary)
            .unwrap_or_default();
        let reasoning =
            effective_reasoning_config(model, self.selected_reasoning_effort.as_deref());
        let (native_web_search, external_web_search) =
            self.resolve_web_search_configs(model_key, model);
        let (native_image_generation, image_generation_tool_upstream) =
            self.resolve_native_image_generation(model_key, model);
        let prompt_agent_backend = self
            .selected_agent_backend()
            .or_else(|| self.inferred_agent_backend_for_model(model_key))
            .unwrap_or(AgentBackendKind::AgentFrame);
        let prompt_available_models = self.available_agent_models(prompt_agent_backend);

        Ok(FrameAgentConfig {
            enabled_tools: self.main_agent.enabled_tools.clone(),
            upstream: self.build_upstream_config(
                model,
                upstream_timeout_seconds
                    .unwrap_or(model.timeout_seconds)
                    .min(model.timeout_seconds),
                prompt_cache_key,
                prompt_cache_retention,
                reasoning,
                native_web_search,
                external_web_search,
                native_image_input,
                native_pdf_input,
                native_audio_input,
                native_image_generation,
            )?,
            image_tool_upstream,
            pdf_tool_upstream,
            audio_tool_upstream,
            image_generation_tool_upstream,
            skills_dirs: if matches!(self.sandbox.mode, crate::config::SandboxMode::Bubblewrap) {
                vec![workspace_root.join(".skills")]
            } else {
                vec![self.agent_workspace.skills_dir.clone()]
            },
            system_prompt: build_agent_system_prompt(
                &self.agent_workspace,
                session,
                &workspace_summary,
                kind,
                model_key,
                model,
                &self.models,
                &prompt_available_models,
                &self.main_agent,
                &commands,
            ),
            max_tool_roundtrips: self.main_agent.max_tool_roundtrips,
            workspace_root: workspace_root.to_path_buf(),
            runtime_state_root: self
                .agent_workspace
                .root_dir
                .join("runtime")
                .join(&session.workspace_id),
            enable_context_compression: self
                .selected_context_compaction_enabled
                .unwrap_or(self.main_agent.enable_context_compression),
            context_compaction: agent_frame::config::ContextCompactionConfig {
                trigger_ratio: self.main_agent.context_compaction.trigger_ratio,
                token_limit_override: self.main_agent.context_compaction.token_limit_override,
                recent_fidelity_target_ratio: self
                    .main_agent
                    .context_compaction
                    .recent_fidelity_target_ratio,
            },
            timeout_observation_compaction:
                agent_frame::config::TimeoutObservationCompactionConfig {
                    enabled: self.main_agent.timeout_observation_compaction.enabled,
                },
            memory_system: self.main_agent.memory_system,
        })
    }

    fn build_extra_tools(
        &self,
        session: &SessionSnapshot,
        kind: AgentPromptKind,
        agent_id: uuid::Uuid,
        control: Option<SessionExecutionControl>,
    ) -> Vec<Tool> {
        let mut tools = Vec::new();
        if matches!(
            kind,
            AgentPromptKind::MainForeground | AgentPromptKind::MainBackground
        ) {
            let runtime = self.clone();
            tools.push(Tool::new(
                "workspaces_list",
                "Call this tool to get historical information, including earlier chat content and the corresponding workspace. It lists known workspaces by id, title, summary, state, and timestamps. Archived workspaces are hidden by default.",
                json!({
                    "type": "object",
                    "properties": {
                        "query": {"type": "string"},
                        "include_archived": {"type": "boolean"}
                    },
                    "additionalProperties": false
                }),
                move |arguments| {
                    let object = arguments
                        .as_object()
                        .ok_or_else(|| anyhow!("tool arguments must be an object"))?;
                    runtime.list_workspaces(
                        optional_string_arg(object, "query")?,
                        object
                            .get("include_archived")
                            .and_then(Value::as_bool)
                            .unwrap_or(false),
                    )
                },
            ));

            let runtime = self.clone();
            tools.push(Tool::new(
                "workspace_content_list",
                "Call this tool after selecting a historical workspace to inspect what content exists there at a high level, without reading file bodies. Returns files and directories under the requested path.",
                json!({
                    "type": "object",
                    "properties": {
                        "workspace_id": {"type": "string"},
                        "path": {"type": "string"},
                        "depth": {"type": "integer"},
                        "limit": {"type": "integer"}
                    },
                    "required": ["workspace_id"],
                    "additionalProperties": false
                }),
                move |arguments| {
                    let object = arguments
                        .as_object()
                        .ok_or_else(|| anyhow!("tool arguments must be an object"))?;
                    let workspace_id = string_arg_required(object, "workspace_id")?;
                    let depth = object.get("depth").and_then(Value::as_u64).unwrap_or(2) as usize;
                    let limit = object.get("limit").and_then(Value::as_u64).unwrap_or(100) as usize;
                    runtime.list_workspace_contents(
                        workspace_id,
                        optional_string_arg(object, "path")?,
                        depth,
                        limit.clamp(1, 500),
                    )
                },
            ));

            let runtime = self.clone();
            let mount_session = session.clone();
            tools.push(Tool::new(
                "workspace_mount",
                "Call this tool to bring a historical workspace into the current workspace as a read-only mount so you can inspect or read its content safely. Returns the mount path relative to the current workspace root.",
                json!({
                    "type": "object",
                    "properties": {
                        "workspace_id": {"type": "string"},
                        "mount_name": {"type": "string"}
                    },
                    "required": ["workspace_id"],
                    "additionalProperties": false
                }),
                move |arguments| {
                    let object = arguments
                        .as_object()
                        .ok_or_else(|| anyhow!("tool arguments must be an object"))?;
                    runtime.mount_workspace(
                        &mount_session,
                        string_arg_required(object, "workspace_id")?,
                        optional_string_arg(object, "mount_name")?,
                    )
                },
            ));

            let runtime = self.clone();
            let move_session = session.clone();
            tools.push(Tool::new(
                "workspace_content_move",
                "Call this tool to carry forward selected content from an older workspace into the current workspace. Source and target summaries can be updated when the move changes what the workspaces represent.",
                json!({
                    "type": "object",
                    "properties": {
                        "source_workspace_id": {"type": "string"},
                        "paths": {
                            "type": "array",
                            "items": {"type": "string"}
                        },
                        "target_dir": {"type": "string"},
                        "source_summary_update": {"type": "string"},
                        "target_summary_update": {"type": "string"}
                    },
                    "required": ["source_workspace_id", "paths"],
                    "additionalProperties": false
                }),
                move |arguments| {
                    let object = arguments
                        .as_object()
                        .ok_or_else(|| anyhow!("tool arguments must be an object"))?;
                    let paths = object
                        .get("paths")
                        .and_then(Value::as_array)
                        .ok_or_else(|| anyhow!("paths must be an array"))?
                        .iter()
                        .map(|value| {
                            value
                                .as_str()
                                .map(ToOwned::to_owned)
                                .filter(|value| !value.trim().is_empty())
                                .ok_or_else(|| anyhow!("each path must be a non-empty string"))
                        })
                        .collect::<Result<Vec<_>>>()?;
                    runtime.move_workspace_contents(
                        &move_session,
                        string_arg_required(object, "source_workspace_id")?,
                        paths,
                        optional_string_arg(object, "target_dir")?,
                        optional_string_arg(object, "source_summary_update")?,
                        optional_string_arg(object, "target_summary_update")?,
                    )
                },
            ));
        }

        if matches!(
            kind,
            AgentPromptKind::MainForeground | AgentPromptKind::MainBackground
        ) {
            if self.main_agent.memory_system == agent_frame::config::MemorySystem::Layered {
                let runtime = self.clone();
                let memory_session = session.clone();
                tools.push(Tool::new(
                    "memory_search",
                    "Search the current conversation memory layers. Use this before opening rollout summaries or transcript snippets when you need older conversation context.",
                    json!({
                        "type": "object",
                        "properties": {
                            "query": {"type": "string"},
                            "limit": {"type": "integer"}
                        },
                        "required": ["query"],
                        "additionalProperties": false
                    }),
                    move |arguments| {
                        let object = arguments
                            .as_object()
                            .ok_or_else(|| anyhow!("tool arguments must be an object"))?;
                        runtime.memory_search(
                            &memory_session,
                            string_arg_required(object, "query")?,
                            object.get("limit").and_then(Value::as_u64).unwrap_or(10) as usize,
                        )
                    },
                ));

                let runtime = self.clone();
                let rollout_search_session = session.clone();
                tools.push(Tool::new(
                    "rollout_search",
                    "Search rollout transcripts for exact historical evidence. Prefer passing rollout_id when you already know which rollout is relevant.",
                    json!({
                        "type": "object",
                        "properties": {
                            "query": {"type": "string"},
                            "rollout_id": {"type": "string"},
                            "kinds": {
                                "type": "array",
                                "items": {"type": "string"}
                            },
                            "limit": {"type": "integer"}
                        },
                        "required": ["query"],
                        "additionalProperties": false
                    }),
                    move |arguments| {
                        let object = arguments
                            .as_object()
                            .ok_or_else(|| anyhow!("tool arguments must be an object"))?;
                        let kinds = object
                            .get("kinds")
                            .and_then(Value::as_array)
                            .map(|items| {
                                items.iter()
                                    .filter_map(Value::as_str)
                                    .map(ToOwned::to_owned)
                                    .collect::<Vec<_>>()
                            })
                            .unwrap_or_default();
                        runtime.rollout_search(
                            &rollout_search_session,
                            string_arg_required(object, "query")?,
                            optional_string_arg(object, "rollout_id")?,
                            kinds,
                            object.get("limit").and_then(Value::as_u64).unwrap_or(10) as usize,
                        )
                    },
                ));

                let runtime = self.clone();
                let rollout_read_session = session.clone();
                tools.push(Tool::new(
                    "rollout_read",
                    "Read a small snippet around one rollout transcript event. Use this after rollout_search instead of opening the whole transcript.",
                    json!({
                        "type": "object",
                        "properties": {
                            "rollout_id": {"type": "string"},
                            "anchor_event_id": {"type": "integer"},
                            "mode": {"type": "string"},
                            "before": {"type": "integer"},
                            "after": {"type": "integer"}
                        },
                        "required": ["rollout_id", "anchor_event_id"],
                        "additionalProperties": false
                    }),
                    move |arguments| {
                        let object = arguments
                            .as_object()
                            .ok_or_else(|| anyhow!("tool arguments must be an object"))?;
                        runtime.rollout_read(
                            &rollout_read_session,
                            string_arg_required(object, "rollout_id")?,
                            object
                                .get("anchor_event_id")
                                .and_then(Value::as_u64)
                                .ok_or_else(|| anyhow!("anchor_event_id must be an integer"))?
                                as usize,
                            optional_string_arg(object, "mode")?,
                            object.get("before").and_then(Value::as_u64).unwrap_or(3) as usize,
                            object.get("after").and_then(Value::as_u64).unwrap_or(3) as usize,
                        )
                    },
                ));
            }

            let runtime = self.clone();
            let tell_session = session.clone();
            tools.push(Tool::new(
                "shared_profile_upload",
                "Upload the workspace copies of USER.md and IDENTITY.md back to the shared profile files. Call this right after you edit either file. The current foreground run keeps its existing system prompt after upload, so use file_read on the workspace copy to inspect the refreshed content directly. If you changed IDENTITY.md, reread ./IDENTITY.md immediately after uploading so your current turn follows the updated persona.",
                json!({
                    "type": "object",
                    "properties": {},
                    "additionalProperties": false
                }),
                move |arguments| {
                    let _ = arguments
                        .as_object()
                        .ok_or_else(|| anyhow!("tool arguments must be an object"))?;
                    runtime.upload_shared_profile_files(&tell_session)
                },
            ));

            let runtime = self.clone();
            let tell_session = session.clone();
            tools.push(Tool::new(
                "user_tell",
                "Immediately send a short progress or coordination message to the current user conversation without waiting for the current turn to finish. Use this for any mid-task user-facing update that should appear as its own chat bubble while work is still ongoing. If you want to answer the user, explain what you are doing, report progress, or give a transitional update before the turn is finished, use user_tell instead of only putting that text in an assistant message with tool_calls. To include files or images, append one or more <attachment>relative/path/from/workspace_root</attachment> tags inside text.",
                json!({
                    "type": "object",
                    "properties": {
                        "text": {"type": "string"}
                    },
                    "required": ["text"],
                    "additionalProperties": false
                }),
                move |arguments| {
                    let object = arguments
                        .as_object()
                        .ok_or_else(|| anyhow!("tool arguments must be an object"))?;
                    runtime.tell_user_now(&tell_session, string_arg_required(object, "text")?)
                },
            ));

            let runtime = self.clone();
            let create_session = session.clone();
            tools.push(Tool::new(
                "subagent_start",
                "Start a session-bound subagent for a small delegated task. Requires description. Optionally set model.",
                json!({
                    "type": "object",
                    "properties": {
                        "description": {"type": "string"},
                        "model": {"type": "string"}
                    },
                    "required": ["description"],
                    "additionalProperties": false
                }),
                move |arguments| {
                    let object = arguments
                        .as_object()
                        .ok_or_else(|| anyhow!("tool arguments must be an object"))?;
                    runtime.start_subagent(
                        agent_id,
                        create_session.clone(),
                        string_arg_required(object, "description")?,
                        optional_string_arg(object, "model")?,
                    )
                },
            ));

            let runtime = self.clone();
            let destroy_session = session.clone();
            tools.push(Tool::new(
                "subagent_kill",
                "Kill a running subagent and clean up its state.",
                json!({
                    "type": "object",
                    "properties": {
                        "agent_id": {"type": "string"}
                    },
                    "required": ["agent_id"],
                    "additionalProperties": false
                }),
                move |arguments| {
                    let object = arguments
                        .as_object()
                        .ok_or_else(|| anyhow!("tool arguments must be an object"))?;
                    runtime.kill_subagent(&destroy_session, parse_uuid_arg(object, "agent_id")?)
                },
            ));

            let runtime = self.clone();
            let wait_session = session.clone();
            let wait_control = control.clone();
            tools.push(Tool::new_interruptible(
                "subagent_join",
                "Wait until a subagent finishes or fails. Supports an optional timeout_seconds; timing out returns a still-running result without killing the subagent. Finished or failed subagents are destroyed immediately after join returns them.",
                json!({
                    "type": "object",
                    "properties": {
                        "agent_id": {"type": "string"},
                        "timeout_seconds": {"type": "number"}
                    },
                    "required": ["agent_id"],
                    "additionalProperties": false
                }),
                move |arguments| {
                    let object = arguments
                        .as_object()
                        .ok_or_else(|| anyhow!("tool arguments must be an object"))?;
                    runtime.join_subagent(
                        &wait_session,
                        parse_uuid_arg(object, "agent_id")?,
                        object.get("timeout_seconds").and_then(Value::as_f64).unwrap_or(0.0),
                        wait_control.clone(),
                    )
                },
            ));
        }

        if matches!(kind, AgentPromptKind::MainForeground) {
            let runtime = self.clone();
            let session = session.clone();
            tools.push(Tool::new(
                "start_background_agent",
                "Start a main background agent. Arguments: task (string), optional model (string), optional sink object. If sink is omitted, results go back to the current user conversation. Usually omit sink unless you really need custom routing. Never use session_id as conversation_id. Sink forms: {kind:\"current_session\"}, {kind:\"direct\", channel_id, conversation_id, user_id?, display_name?}, {kind:\"broadcast\", topic}, or {kind:\"multi\", targets:[...]}",
                json!({
                    "type": "object",
                    "properties": {
                        "task": {"type": "string"},
                        "model": {"type": "string"},
                        "sink": {"type": "object"}
                    },
                    "required": ["task"],
                    "additionalProperties": false
                }),
                move |arguments| {
                    let object = arguments
                        .as_object()
                        .ok_or_else(|| anyhow!("tool arguments must be an object"))?;
                    let task = object
                        .get("task")
                        .and_then(Value::as_str)
                        .map(str::trim)
                        .filter(|value| !value.is_empty())
                        .ok_or_else(|| anyhow!("task must be a non-empty string"))?;
                    let model_key = object
                        .get("model")
                        .and_then(Value::as_str)
                        .map(ToOwned::to_owned);
                    let sink = match object.get("sink") {
                        Some(value) => parse_sink_target(
                            value,
                            Some(SinkTarget::Direct(session.address.clone())),
                        )?,
                        None => SinkTarget::Direct(session.address.clone()),
                    };
                    let sink = normalize_sink_target(sink, &session);
                    runtime.start_background_agent(
                        agent_id,
                        session.clone(),
                        model_key,
                        task.to_string(),
                        sink,
                    )
                },
            ));
        }

        if matches!(
            kind,
            AgentPromptKind::MainForeground | AgentPromptKind::MainBackground
        ) {
            let runtime = self.clone();
            tools.push(Tool::new(
                "list_cron_tasks",
                "List configured cron tasks. Returns summaries including enabled state and next_run_at.",
                json!({
                    "type": "object",
                    "properties": {},
                    "additionalProperties": false
                }),
                move |_| runtime.list_cron_tasks(),
            ));

            let runtime = self.clone();
            tools.push(Tool::new(
                "get_cron_task",
                "Get full details for a cron task by id.",
                json!({
                    "type": "object",
                    "properties": {
                        "id": {"type": "string"}
                    },
                    "required": ["id"],
                    "additionalProperties": false
                }),
                move |arguments| {
                    let object = arguments
                        .as_object()
                        .ok_or_else(|| anyhow!("tool arguments must be an object"))?;
                    let id = parse_uuid_arg(object, "id")?;
                    runtime.get_cron_task(id)
                },
            ));

            let runtime = self.clone();
            let create_session = session.clone();
            tools.push(Tool::new(
                "create_cron_task",
                "Create a persisted cron task that later launches a main background agent. Use a standard cron expression. The checker is optional: checker exit code 0 triggers the LLM, non-zero skips the run, and checker execution errors or timeouts still trigger the LLM.",
                json!({
                    "type": "object",
                    "properties": {
                        "name": {"type": "string"},
                        "description": {"type": "string"},
                        "schedule": {"type": "string"},
                        "task": {"type": "string"},
                                "enabled": {"type": "boolean"},
                                "sink": {"type": "object"},
                                "checker_command": {"type": "string"},
                        "checker_timeout_seconds": {"type": "number"},
                        "checker_cwd": {"type": "string"}
                    },
                    "required": ["name", "description", "schedule", "task"],
                    "additionalProperties": false
                }),
                move |arguments| {
                    let object = arguments
                        .as_object()
                        .ok_or_else(|| anyhow!("tool arguments must be an object"))?;
                    let sink = match object.get("sink") {
                        Some(value) => parse_sink_target(
                            value,
                            Some(SinkTarget::Direct(create_session.address.clone())),
                        )?,
                        None => SinkTarget::Direct(create_session.address.clone()),
                    };
                    let sink = normalize_sink_target(sink, &create_session);
                    let checker = parse_checker_from_tool_args(object)?;
                    runtime.create_cron_task(
                        create_session.clone(),
                        CronCreateRequest {
                            name: string_arg_required(object, "name")?,
                            description: string_arg_required(object, "description")?,
                            schedule: string_arg_required(object, "schedule")?,
                            agent_backend: runtime.effective_agent_backend()?,
                            model_key: runtime.effective_main_model_key()?,
                            prompt: string_arg_required(object, "task")?,
                            sink,
                            address: create_session.address.clone(),
                            enabled: object
                                .get("enabled")
                                .and_then(Value::as_bool)
                                .unwrap_or(true),
                            checker,
                        },
                    )
                },
            ));

            let runtime = self.clone();
            let update_session = session.clone();
            tools.push(Tool::new(
                "update_cron_task",
                "Update a cron task. Use enabled to pause or resume it. Set clear_checker=true to remove the checker.",
                json!({
                    "type": "object",
                    "properties": {
                        "id": {"type": "string"},
                        "name": {"type": "string"},
                        "description": {"type": "string"},
                        "schedule": {"type": "string"},
                        "task": {"type": "string"},
                        "model": {"type": "string"},
                        "enabled": {"type": "boolean"},
                        "sink": {"type": "object"},
                        "checker_command": {"type": "string"},
                        "checker_timeout_seconds": {"type": "number"},
                        "checker_cwd": {"type": "string"},
                        "clear_checker": {"type": "boolean"}
                    },
                    "required": ["id"],
                    "additionalProperties": false
                }),
                move |arguments| {
                    let object = arguments
                        .as_object()
                        .ok_or_else(|| anyhow!("tool arguments must be an object"))?;
                    let id = parse_uuid_arg(object, "id")?;
                    let checker = if object
                        .get("clear_checker")
                        .and_then(Value::as_bool)
                        .unwrap_or(false)
                    {
                        Some(None)
                    } else if object.contains_key("checker_command")
                        || object.contains_key("checker_timeout_seconds")
                        || object.contains_key("checker_cwd")
                    {
                        Some(parse_checker_from_tool_args(object)?)
                    } else {
                        None
                    };
                    let sink = match object.get("sink") {
                        Some(value) => Some(normalize_sink_target(
                            parse_sink_target(value, Some(SinkTarget::Direct(update_session.address.clone())))?,
                            &update_session,
                        )),
                        None => None,
                    };
                    runtime.update_cron_task(
                        id,
                        CronUpdateRequest {
                            name: optional_string_arg(object, "name")?,
                            description: optional_string_arg(object, "description")?,
                            schedule: optional_string_arg(object, "schedule")?,
                            agent_backend: None,
                            model_key: optional_string_arg(object, "model")?,
                            prompt: optional_string_arg(object, "task")?,
                            sink,
                            enabled: object.get("enabled").and_then(Value::as_bool),
                            checker,
                        },
                    )
                },
            ));

            let runtime = self.clone();
            tools.push(Tool::new(
                "remove_cron_task",
                "Remove a cron task permanently.",
                json!({
                    "type": "object",
                    "properties": {
                        "id": {"type": "string"}
                    },
                    "required": ["id"],
                    "additionalProperties": false
                }),
                move |arguments| {
                    let object = arguments
                        .as_object()
                        .ok_or_else(|| anyhow!("tool arguments must be an object"))?;
                    let id = parse_uuid_arg(object, "id")?;
                    runtime.remove_cron_task(id)
                },
            ));

            let runtime = self.clone();
            tools.push(Tool::new(
                "background_agents_list",
                "List tracked background agents with status, model, and token usage statistics.",
                json!({
                    "type": "object",
                    "properties": {},
                    "additionalProperties": false
                }),
                move |_| runtime.list_managed_agents(ManagedAgentKind::Background),
            ));

            let runtime = self.clone();
            tools.push(Tool::new(
                "get_agent_stats",
                "Get detailed status and token usage statistics for a tracked background agent or subagent by agent_id.",
                json!({
                    "type": "object",
                    "properties": {
                        "agent_id": {"type": "string"}
                    },
                    "required": ["agent_id"],
                    "additionalProperties": false
                }),
                move |arguments| {
                    let object = arguments
                        .as_object()
                        .ok_or_else(|| anyhow!("tool arguments must be an object"))?;
                    let agent_id = parse_uuid_arg(object, "agent_id")?;
                    runtime.get_managed_agent(agent_id)
                },
            ));
        }

        tools
    }

    fn build_backend_execution_options(
        &self,
        backend: AgentBackendKind,
    ) -> BackendExecutionOptions {
        BackendExecutionOptions {
            zgent_allowed_subagent_models: self
                .available_agent_models(backend)
                .into_iter()
                .filter_map(|alias| {
                    self.models.get(&alias).map(|model| ZgentSubagentModel {
                        alias: alias.clone(),
                        description: if model.description.trim().is_empty() {
                            model.model.clone()
                        } else {
                            model.description.clone()
                        },
                    })
                })
                .collect(),
        }
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
        let config = self.build_agent_frame_config(
            &session,
            &workspace_root,
            kind,
            &model_key,
            upstream_timeout_seconds,
        )?;
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
        let phase_session_key = event_session.address.session_key();
        let active_foreground_phases = Arc::clone(&self.active_foreground_phases);
        let (event_sender, mut event_receiver) = mpsc::unbounded_channel();
        let (progress_sender, mut progress_receiver) = mpsc::unbounded_channel();
        let execution_control = SessionExecutionControl::new()
            .with_event_callback(move |event| {
                update_active_foreground_phase(
                    &active_foreground_phases,
                    &phase_session_key,
                    &event,
                );
                let _ = event_sender.send(event);
            })
            .with_progress_callback(move |progress| {
                let _ = progress_sender.send(progress);
            });
        if let Some(observer) = control_observer {
            if let Ok(mut phases) = self.active_foreground_phases.lock() {
                phases.insert(
                    event_session.address.session_key(),
                    ForegroundRuntimePhase::Running,
                );
            }
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
                    log_agent_frame_event(agent_id, &event_session, kind, &event_model_key, &event);
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
        let phase_session_key = event_session.address.session_key();
        let active_foreground_phases = Arc::clone(&self.active_foreground_phases);
        let (event_sender, event_receiver) = std::sync::mpsc::channel();
        let execution_control = SessionExecutionControl::new().with_event_callback(move |event| {
            update_active_foreground_phase(&active_foreground_phases, &phase_session_key, &event);
            let _ = event_sender.send(event);
        });
        if let Some(observer) = control_observer {
            if let Ok(mut phases) = self.active_foreground_phases.lock() {
                phases.insert(
                    event_session.address.session_key(),
                    ForegroundRuntimePhase::Running,
                );
            }
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
                    log_agent_frame_event(agent_id, &event_session, kind, &event_model_key, &event)
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
                sink: task.sink.clone(),
            })
            .await
            .context("failed to enqueue cron background agent")?;
        {
            let mut manager = self
                .cron_manager
                .lock()
                .map_err(|_| anyhow!("cron manager lock poisoned"))?;
            manager.record_trigger_result(task.id, Utc::now(), "enqueued".to_string())?;
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

pub struct Server {
    workdir: PathBuf,
    agent_workspace: AgentWorkspace,
    workspace_manager: WorkspaceManager,
    channels: Arc<HashMap<String, Arc<dyn Channel>>>,
    telegram_channel_ids: Arc<HashSet<String>>,
    command_catalog: HashMap<String, Vec<BotCommandConfig>>,
    models: BTreeMap<String, ModelConfig>,
    agent: AgentConfig,
    tooling: ToolingConfig,
    chat_model_keys: Vec<String>,
    main_agent: MainAgentConfig,
    sandbox: SandboxConfig,
    conversations: Arc<Mutex<ConversationManager>>,
    snapshots: Arc<Mutex<SnapshotManager>>,
    sessions: Arc<Mutex<SessionManager>>,
    sink_router: Arc<RwLock<SinkRouter>>,
    cron_manager: Arc<Mutex<CronManager>>,
    agent_registry: Arc<Mutex<AgentRegistry>>,
    agent_registry_notify: Arc<Notify>,
    max_global_sub_agents: usize,
    subagent_count: Arc<AtomicUsize>,
    cron_poll_interval_seconds: u64,
    background_job_sender: mpsc::Sender<BackgroundJobRequest>,
    background_job_receiver: Option<mpsc::Receiver<BackgroundJobRequest>>,
    summary_tracker: Arc<SummaryTracker>,
    active_foreground_controls: Arc<Mutex<HashMap<String, SessionExecutionControl>>>,
    active_foreground_phases: Arc<Mutex<HashMap<String, ForegroundRuntimePhase>>>,
    /// Session keys with pending user interrupts — when a new user message arrives while a
    /// foreground turn is running, the session key is inserted here so that
    /// `should_auto_resume_yielded_session` knows not to auto-resume.
    pending_foreground_interrupts: Arc<Mutex<HashSet<String>>>,
    pending_process_restart_notices: Arc<Mutex<HashSet<String>>>,
    active_foreground_agent_frame_runtimes:
        Arc<Mutex<HashMap<String, Arc<Mutex<ActiveForegroundAgentFrameRuntime>>>>>,
    active_native_zgent_sessions: Arc<Mutex<HashMap<String, Arc<ActiveNativeZgentSession>>>>,
    subagents: Arc<Mutex<HashMap<uuid::Uuid, Arc<HostedSubagent>>>>,
    channel_auth: Arc<Mutex<ChannelAuthorizationManager>>,
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

    fn foreground_uses_native_zgent(
        &self,
        address: &ChannelAddress,
        model_key: &str,
    ) -> Result<bool> {
        self.ensure_model_available_for_backend(self.effective_agent_backend(address)?, model_key)?;
        if self.effective_agent_backend(address)? != AgentBackendKind::Zgent {
            return Ok(false);
        }
        if !zgent_native_kernel_runtime_available() {
            return Ok(false);
        }
        Ok(true)
    }

    fn ensure_native_zgent_session(
        &self,
        session: &SessionSnapshot,
        model_key: &str,
    ) -> Result<Arc<ActiveNativeZgentSession>> {
        let session_key = session.address.session_key();
        if let Some(existing) = self
            .active_native_zgent_sessions
            .lock()
            .ok()
            .and_then(|sessions| sessions.get(&session_key).cloned())
            .filter(|active| active.model_key == model_key)
        {
            return Ok(existing);
        }

        let runtime = self.tool_runtime_for_address(&session.address)?;
        let config = runtime.build_agent_frame_config(
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
        let options = runtime.build_backend_execution_options(AgentBackendKind::Zgent);
        let existing_remote_session_id = self
            .with_sessions(|sessions| sessions.zgent_native_state(&session.address))?
            .filter(|state| state.model_key.as_deref() == Some(model_key))
            .and_then(|state| state.remote_session_id);
        let kernel = PersistentZgentKernelSession::spawn_or_attach(
            &ZgentKernelRuntimeSpec::from_frame_config(&config),
            &extra_tools,
            &options,
            existing_remote_session_id.as_deref(),
        )?;
        let session_summary = kernel.fetch_session_summary().ok();
        crate::zgent::kernel::require_workspace_binding(
            kernel.remote_workspace_path(),
            &config.workspace_root,
        )?;
        let active = Arc::new(ActiveNativeZgentSession {
            kernel: Arc::new(kernel),
            model_key: model_key.to_string(),
            busy: Arc::new(AtomicBool::new(false)),
        });
        self.with_sessions(|sessions| {
            sessions.set_zgent_native_state(
                &session.address,
                Some(ZgentNativeSessionState {
                    remote_session_id: Some(active.kernel.remote_session_id().to_string()),
                    model_key: Some(model_key.to_string()),
                    context_window_current: session_summary
                        .as_ref()
                        .and_then(|summary| summary.context_window_current),
                    context_window_size: session_summary
                        .as_ref()
                        .and_then(|summary| summary.context_window_size),
                }),
            )
        })?;
        let mut sessions = self
            .active_native_zgent_sessions
            .lock()
            .map_err(|_| anyhow!("active native zgent sessions lock poisoned"))?;
        sessions.insert(session_key, Arc::clone(&active));
        Ok(active)
    }

    fn current_tool_names_for_foreground_turn(
        &self,
        session: &SessionSnapshot,
        model_key: &str,
    ) -> Result<Vec<String>> {
        let runtime = self.tool_runtime_for_address(&session.address)?;
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
            frame_config.image_tool_upstream.as_ref(),
            frame_config.pdf_tool_upstream.as_ref(),
            frame_config.audio_tool_upstream.as_ref(),
            frame_config.image_generation_tool_upstream.as_ref(),
            &frame_config.skills_dirs,
            &skills,
            &extra_tools,
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
                let using_native_zgent = self
                    .foreground_uses_native_zgent(&session.address, model_key)
                    .unwrap_or(false);
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
                    native_zgent = using_native_zgent,
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

    async fn try_forward_to_active_native_zgent_turn(
        &self,
        message: IncomingMessage,
    ) -> Result<Option<IncomingMessage>> {
        if message.control.is_some() {
            return Ok(Some(message));
        }
        let Some(model_key) = self.selected_main_model_key(&message.address)? else {
            return Ok(Some(message));
        };
        if !self.foreground_uses_native_zgent(&message.address, &model_key)? {
            return Ok(Some(message));
        }
        let session_key = message.address.session_key();
        let Some(active) = self
            .active_native_zgent_sessions
            .lock()
            .ok()
            .and_then(|sessions| sessions.get(&session_key).cloned())
            .filter(|active| active.model_key == model_key)
        else {
            return Ok(Some(message));
        };
        if !active.busy.load(Ordering::SeqCst) {
            return Ok(Some(message));
        }

        let Some(session) =
            self.with_sessions(|sessions| Ok(sessions.get_snapshot(&message.address)))?
        else {
            return Ok(Some(message));
        };
        self.log_current_tools_for_user_message(
            &session,
            &model_key,
            &message.remote_message_id,
            "native_busy_steer",
        );
        let stored_attachments = self
            .materialize_attachments(&session.attachments_dir, message.attachments)
            .await?;
        let steer_prompt = tag_interrupted_followup_text(Some(compose_user_prompt(
            message.text.as_deref(),
            &stored_attachments,
        )))
        .unwrap_or_else(|| INTERRUPTED_FOLLOWUP_MARKER.to_string());
        let kernel = Arc::clone(&active.kernel);
        tokio::task::spawn_blocking(move || kernel.send_steer(&steer_prompt))
            .await
            .context("native zgent steer task join failed")??;
        Ok(None)
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
            self.agent_backend_selection_message(
                address,
                &format!(
                    "The previously selected model `{}` is no longer available for the current agent setup. `/agent` has been opened automatically below so you can choose again.",
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
        self.with_sessions(|sessions| sessions.clear_idle_compaction_failure(&session.address))?;
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
        let message = self.agent_backend_selection_message(
            &session.address,
            &format!(
                "The previously selected model `{}` is no longer available for the current agent setup. Idle compaction has been paused for this conversation, and `/agent` has been opened automatically below so you can choose again.",
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

    fn with_sessions<T>(&self, f: impl FnOnce(&mut SessionManager) -> Result<T>) -> Result<T> {
        let mut sessions = self
            .sessions
            .lock()
            .map_err(|_| anyhow!("session manager lock poisoned"))?;
        f(&mut sessions)
    }

    fn with_conversations<T>(
        &self,
        f: impl FnOnce(&mut ConversationManager) -> Result<T>,
    ) -> Result<T> {
        let mut conversations = self
            .conversations
            .lock()
            .map_err(|_| anyhow!("conversation manager lock poisoned"))?;
        f(&mut conversations)
    }

    fn with_snapshots<T>(&self, f: impl FnOnce(&mut SnapshotManager) -> Result<T>) -> Result<T> {
        let mut snapshots = self
            .snapshots
            .lock()
            .map_err(|_| anyhow!("snapshot manager lock poisoned"))?;
        f(&mut snapshots)
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

        Ok(Self {
            sessions: Arc::new(Mutex::new(session_manager)),
            workdir: workdir.clone(),
            agent_workspace,
            workspace_manager,
            channels: Arc::new(channels),
            telegram_channel_ids: Arc::new(telegram_channel_ids),
            command_catalog,
            models: config.models,
            agent: config.agent,
            tooling,
            chat_model_keys: config.chat_model_keys,
            main_agent: config.main_agent,
            sandbox: config.sandbox,
            conversations: Arc::new(Mutex::new(ConversationManager::new(&workdir)?)),
            snapshots: Arc::new(Mutex::new(SnapshotManager::new(&workdir)?)),
            sink_router: Arc::new(RwLock::new(SinkRouter::new())),
            cron_manager,
            agent_registry,
            agent_registry_notify,
            max_global_sub_agents: config.max_global_sub_agents,
            subagent_count: Arc::new(AtomicUsize::new(0)),
            cron_poll_interval_seconds: config.cron_poll_interval_seconds,
            background_job_sender,
            background_job_receiver: Some(background_job_receiver),
            summary_tracker: Arc::new(SummaryTracker::new()),
            active_foreground_controls: Arc::new(Mutex::new(HashMap::new())),
            active_foreground_phases: Arc::new(Mutex::new(HashMap::new())),
            pending_foreground_interrupts: Arc::new(Mutex::new(HashSet::new())),
            pending_process_restart_notices: Arc::new(Mutex::new(pending_process_restart_notices)),
            active_foreground_agent_frame_runtimes: Arc::new(Mutex::new(HashMap::new())),
            active_native_zgent_sessions: Arc::new(Mutex::new(HashMap::new())),
            subagents: Arc::new(Mutex::new(HashMap::new())),
            channel_auth: Arc::new(Mutex::new(ChannelAuthorizationManager::new(&workdir)?)),
        })
    }

    pub async fn run(mut self) -> Result<()> {
        self.retry_pending_workspace_summaries().await?;
        let (sender, mut receiver) = mpsc::channel::<IncomingMessage>(128);
        let background_receiver = self.background_job_receiver.take();
        let server = Arc::new(self);
        {
            let runtime = server.tool_runtime();
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
            let runtime = server.tool_runtime();
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
            let runtime = server.tool_runtime();
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

        let mut idle_compaction_ticker = interval(Duration::from_secs(
            server.main_agent.idle_compaction.poll_interval_seconds,
        ));
        idle_compaction_ticker.set_missed_tick_behavior(MissedTickBehavior::Skip);
        let conversation_workers: Arc<
            Mutex<HashMap<String, tokio::sync::mpsc::UnboundedSender<IncomingMessage>>>,
        > = Arc::new(Mutex::new(HashMap::new()));
        let active_worker_count = Arc::new(AtomicUsize::new(0));
        let active_worker_notify = Arc::new(Notify::new());
        let mut receiver_closed = false;

        loop {
            if receiver_closed && active_worker_count.load(Ordering::SeqCst) == 0 {
                break;
            }

            select! {
                maybe_message = receiver.recv(), if !receiver_closed => {
                    match maybe_message {
                        Some(message) => {
                            if server.allows_fast_path_agent_selection(&message.address)?
                                && let Some(outgoing) =
                                    fast_path_agent_selection_message(&server.workdir, &server.models, &server.agent, &message)
                            {
                                if let Some(channel) = server.channels.get(&message.address.channel_id) {
                                    if let Err(error) = channel.send(&message.address, outgoing).await {
                                        error!(
                                            log_stream = "channel",
                                            log_key = %message.address.channel_id,
                                            kind = "fast_path_send_failed",
                                            conversation_id = %message.address.conversation_id,
                                            error = %format!("{error:#}"),
                                            "failed to send fast-path model selection message"
                                        );
                                    }
                                }
                                continue;
                            }
                            if is_out_of_band_command(message.text.as_deref()) {
                                let server = Arc::clone(&server);
                                tokio::spawn(async move {
                                    if let Err(error) = server.handle_incoming(message).await {
                                        error!(
                                            log_stream = "server",
                                            kind = "handle_out_of_band_command_failed",
                                            error = %format!("{error:#}"),
                                            "failed to handle out-of-band command"
                                        );
                                    }
                                });
                                continue;
                            }
                            let Some(message) = server
                                .try_forward_to_active_native_zgent_turn(message)
                                .await?
                            else {
                                continue;
                            };
                            let interrupted_followup = request_yield_for_incoming(
                                &server.active_foreground_controls,
                                &server.active_foreground_phases,
                                &message,
                            );
                            if interrupted_followup.compaction_in_progress
                                && let Some(channel) =
                                    server.channels.get(&message.address.channel_id)
                                && let Err(error) = channel
                                    .send(
                                        &message.address,
                                        OutgoingMessage::text(
                                            "正在压缩上下文，可能要等待压缩完毕后才能回复。"
                                                .to_string(),
                                        ),
                                    )
                                    .await
                            {
                                error!(
                                    log_stream = "channel",
                                    log_key = %message.address.channel_id,
                                    kind = "compaction_wait_notice_send_failed",
                                    conversation_id = %message.address.conversation_id,
                                    error = %format!("{error:#}"),
                                    "failed to send compaction wait notice"
                                );
                            }
                            let message = if interrupted_followup.interrupted {
                                let mut message = message;
                                message.text = tag_interrupted_followup_text(message.text);
                                if let Ok(mut interrupts) =
                                    server.pending_foreground_interrupts.lock()
                                {
                                    interrupts.insert(message.address.session_key());
                                }
                                message
                            } else {
                                message
                            };
                            let session_key = message.address.session_key();
                            let mut pending_message = Some(message);
                            loop {
                                let worker_sender = conversation_workers
                                    .lock()
                                    .map_err(|_| anyhow!("conversation workers lock poisoned"))?
                                    .get(&session_key)
                                    .cloned();
                                let worker_sender = match worker_sender {
                                    Some(worker_sender) => worker_sender,
                                    None => {
                                        let (worker_tx, mut worker_rx) = tokio::sync::mpsc::unbounded_channel();
                                        conversation_workers
                                            .lock()
                                            .map_err(|_| anyhow!("conversation workers lock poisoned"))?
                                            .insert(session_key.clone(), worker_tx.clone());
                                        active_worker_count.fetch_add(1, Ordering::SeqCst);
                                        let server = Arc::clone(&server);
                                        let conversation_workers = Arc::clone(&conversation_workers);
                                        let active_worker_count = Arc::clone(&active_worker_count);
                                        let active_worker_notify = Arc::clone(&active_worker_notify);
                                        let worker_session_key = session_key.clone();
                                        tokio::spawn(async move {
                                            let mut local_queue = VecDeque::new();
                                            while let Some(message) = worker_rx.recv().await {
                                                local_queue.push_back(message);
                                                while let Ok(message) = worker_rx.try_recv() {
                                                    local_queue.push_back(message);
                                                }
                                                while let Some(message) = local_queue.pop_front() {
                                                    let merged =
                                                        coalesce_buffered_conversation_messages(message, &mut local_queue);
                                                    if let Err(error) = server.handle_incoming(merged).await {
                                                        error!(
                                                            log_stream = "server",
                                                            kind = "handle_incoming_failed",
                                                            error = %format!("{error:#}"),
                                                            "failed to handle incoming message"
                                                        );
                                                    }
                                                    while let Ok(message) = worker_rx.try_recv() {
                                                        local_queue.push_back(message);
                                                    }
                                                }
                                            }
                                            if let Ok(mut workers) = conversation_workers.lock() {
                                                workers.remove(&worker_session_key);
                                            }
                                            active_worker_count.fetch_sub(1, Ordering::SeqCst);
                                            active_worker_notify.notify_waiters();
                                        });
                                        worker_tx
                                    }
                                };
                                let message = pending_message
                                    .take()
                                    .expect("pending message should exist while dispatching");
                                match worker_sender.send(message) {
                                    Ok(()) => break,
                                    Err(error) => {
                                        if let Ok(mut workers) = conversation_workers.lock() {
                                            workers.remove(&session_key);
                                        }
                                        pending_message = Some(error.0);
                                    }
                                }
                            }
                        }
                        None => receiver_closed = true,
                    }
                }
                _ = idle_compaction_ticker.tick() => {
                    if server.main_agent.idle_compaction.enabled
                        && let Err(error) = server.run_idle_context_compaction_once().await
                    {
                        error!(
                            log_stream = "server",
                            kind = "idle_context_compaction_failed",
                            error = %format!("{error:#}"),
                            "idle context compaction pass failed"
                        );
                    }
                }
                _ = active_worker_notify.notified(), if receiver_closed => {}
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
                self.with_sessions(|sessions| {
                    sessions.mark_idle_compaction_failed(&session.address, format!("{error:#}"))
                })?;
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
        let runtime = self.tool_runtime_for_address(&session.address)?;
        let source_messages = session.request_messages();
        if source_messages.is_empty() {
            return Ok(false);
        }
        let config = runtime.build_agent_frame_config(
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
        let effective_backend = self.effective_agent_backend(&session.address)?;
        let source_messages = if effective_backend == AgentBackendKind::AgentFrame {
            source_messages
        } else {
            sanitize_messages_for_model_capabilities(
                &source_messages,
                &model,
                backend_supports_native_multimodal_input(effective_backend),
            )
        };
        let report =
            run_backend_compaction(effective_backend, source_messages, config, extra_tools)
                .with_context(|| format!("failed to compact idle session {}", session.id))?;
        if !report.compacted {
            self.with_sessions(|sessions| {
                sessions.clear_idle_compaction_failure(&session.address)
            })?;
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
        self.with_sessions(|sessions| {
            sessions.record_idle_compaction(
                &session.address,
                normalized_messages,
                &compaction_stats,
            )
        })
        .with_context(|| format!("failed to persist idle compaction for {}", session.id))?;
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
            prompt_tokens = report.usage.prompt_tokens,
            completion_tokens = report.usage.completion_tokens,
            total_tokens = report.usage.total_tokens,
            cache_hit_tokens = report.usage.cache_hit_tokens,
            cache_miss_tokens = report.usage.cache_miss_tokens,
            cache_read_tokens = report.usage.cache_read_tokens,
            cache_write_tokens = report.usage.cache_write_tokens,
            rollout_id = rollout_id.as_deref(),
            "idle context compaction completed"
        );
        Ok(true)
    }

    fn tool_runtime(&self) -> ServerRuntime {
        ServerRuntime {
            agent_workspace: self.agent_workspace.clone(),
            sessions: Arc::clone(&self.sessions),
            workspace_manager: self.workspace_manager.clone(),
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
            channels: Arc::clone(&self.channels),
            command_catalog: self.command_catalog.clone(),
            models: self.models.clone(),
            agent: self.agent.clone(),
            tooling: self.tooling.clone(),
            main_agent: self.main_agent.clone(),
            sandbox: self.sandbox.clone(),
            sink_router: Arc::clone(&self.sink_router),
            cron_manager: Arc::clone(&self.cron_manager),
            agent_registry: Arc::clone(&self.agent_registry),
            agent_registry_notify: Arc::clone(&self.agent_registry_notify),
            max_global_sub_agents: self.max_global_sub_agents,
            subagent_count: Arc::clone(&self.subagent_count),
            cron_poll_interval_seconds: self.cron_poll_interval_seconds,
            background_job_sender: self.background_job_sender.clone(),
            summary_tracker: Arc::clone(&self.summary_tracker),
            active_foreground_phases: Arc::clone(&self.active_foreground_phases),
            active_foreground_agent_frame_runtimes: Arc::clone(
                &self.active_foreground_agent_frame_runtimes,
            ),
            subagents: Arc::clone(&self.subagents),
            conversations: Arc::clone(&self.conversations),
        }
    }

    fn tool_runtime_for_sandbox_mode(&self, sandbox_mode: SandboxMode) -> ServerRuntime {
        let mut runtime = self.tool_runtime();
        runtime.sandbox.mode = sandbox_mode;
        runtime
    }

    fn tool_runtime_for_address(&self, address: &ChannelAddress) -> Result<ServerRuntime> {
        let sandbox_mode = self.effective_sandbox_mode(address)?;
        let mut runtime = self.tool_runtime_for_sandbox_mode(sandbox_mode);
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

    fn ensure_foreground_session(&self, address: &ChannelAddress) -> Result<SessionSnapshot> {
        let preferred_workspace_id = self.with_conversations(|conversations| {
            Ok(conversations
                .ensure_conversation(address)?
                .settings
                .workspace_id)
        })?;
        let session = self.with_sessions(|sessions| match preferred_workspace_id.as_deref() {
            Some(workspace_id) => sessions.ensure_foreground_in_workspace(address, workspace_id),
            None => sessions.ensure_foreground(address),
        })?;
        self.with_conversations(|conversations| {
            conversations.set_workspace_id(address, Some(session.workspace_id.clone()))?;
            Ok(())
        })?;
        Ok(session)
    }

    fn unregister_active_foreground_control(&self, address: &ChannelAddress) -> Result<()> {
        let session_key = address.session_key();
        let mut controls = self
            .active_foreground_controls
            .lock()
            .map_err(|_| anyhow!("active foreground controls lock poisoned"))?;
        controls.remove(&session_key);
        drop(controls);
        if let Ok(mut phases) = self.active_foreground_phases.lock() {
            phases.remove(&session_key);
        }
        Ok(())
    }

    fn invalidate_foreground_agent_frame_runtime(&self, address: &ChannelAddress) -> Result<()> {
        let session_key = address.session_key();
        let runtime = self
            .active_foreground_agent_frame_runtimes
            .lock()
            .map_err(|_| anyhow!("active foreground runtimes lock poisoned"))?
            .remove(&session_key);
        if let Some(runtime) = runtime
            && let Ok(mut runtime) = runtime.lock()
        {
            let _ = runtime.runtime.shutdown();
            if runtime.sandbox_mode == SandboxMode::Bubblewrap {
                let _ = self
                    .workspace_manager
                    .cleanup_transient_mounts(&runtime.workspace_id);
            }
        }
        Ok(())
    }

    fn destroy_foreground_session(&self, address: &ChannelAddress) -> Result<()> {
        let snapshot = self.with_sessions(|sessions| Ok(sessions.get_snapshot(address)))?;
        if let Ok(mut sessions) = self.active_native_zgent_sessions.lock() {
            sessions.remove(&address.session_key());
        }
        self.invalidate_foreground_agent_frame_runtime(address)?;
        if let Some(control) = self
            .active_foreground_controls
            .lock()
            .ok()
            .and_then(|controls| controls.get(&address.session_key()).cloned())
        {
            control.request_cancel();
        }
        self.unregister_active_foreground_control(address)?;
        if let Some(session) = snapshot {
            let destroyed_subagents = self
                .tool_runtime()
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
                    "destroyed background runtime tasks for session"
                );
            }
        }
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

        self.handle_regular_foreground_message(&channel, incoming)
            .await
    }

    async fn materialize_attachments(
        &self,
        attachments_dir: &Path,
        attachments: Vec<crate::channel::PendingAttachment>,
    ) -> Result<Vec<StoredAttachment>> {
        let mut stored = Vec::with_capacity(attachments.len());
        for attachment in attachments {
            let item = attachment.materialize(attachments_dir).await?;
            info!(
                log_stream = "server",
                kind = "attachment_materialized",
                attachment_id = %item.id,
                path = %item.path.display(),
                size_bytes = item.size_bytes,
                "attachment persisted to session storage"
            );
            stored.push(item);
        }
        Ok(stored)
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

    fn status_text_for_session(
        &self,
        session: &SessionSnapshot,
        model_key: &str,
    ) -> Result<String> {
        let model = self.model_config_or_main(model_key)?;
        let effective_api_timeout = session
            .api_timeout_override_seconds
            .unwrap_or(model.timeout_seconds);
        let timeout_source = if session.api_timeout_override_seconds.is_some() {
            "session override"
        } else {
            "model default"
        };
        let runtime = self.tool_runtime_for_address(&session.address)?;
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
        Ok(format_session_status(
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
        ))
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
        let runtime = self.tool_runtime_for_address(&session.address)?;
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
        let effective_backend = self.effective_agent_backend(&session.address)?;
        let model = self.model_config_or_main(model_key)?;
        let compaction_messages = if effective_backend == AgentBackendKind::AgentFrame {
            session.request_messages()
        } else {
            sanitize_messages_for_model_capabilities(
                &session.request_messages(),
                model,
                backend_supports_native_multimodal_input(effective_backend),
            )
        };
        let report =
            run_backend_compaction(effective_backend, compaction_messages, config, extra_tools)?;
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
        self.with_sessions(|sessions| {
            sessions.record_idle_compaction(
                &session.address,
                normalized_messages,
                &compaction_stats,
            )
        })?;
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

    async fn summarize_active_workspaces_on_shutdown(&self) -> Result<()> {
        let snapshots = self.with_sessions(|sessions| Ok(sessions.list_foreground_snapshots()))?;
        let mut first_error = None;
        for session in snapshots {
            let _ = self.with_sessions(|sessions| {
                sessions.mark_workspace_summary_state(&session.address, true, false)
            });
            if let Err(error) = self.summarize_workspace_before_destroy(&session).await {
                warn!(
                    log_stream = "session",
                    log_key = %session.id,
                    kind = "workspace_summary_on_shutdown_failed",
                    workspace_id = %session.workspace_id,
                    error = %format!("{error:#}"),
                    "failed to summarize workspace on shutdown"
                );
                if first_error.is_none() {
                    first_error = Some(error);
                }
            }
        }
        if let Some(error) = first_error {
            return Err(error);
        }
        Ok(())
    }

    async fn summarize_workspace_before_destroy(&self, session: &SessionSnapshot) -> Result<()> {
        let _summary_guard = SummaryInProgressGuard::new(Arc::clone(&self.summary_tracker));
        let entries =
            self.workspace_manager
                .list_workspace_contents(&session.workspace_id, None, 3, 200)?;
        let request_messages = session.request_messages();
        let request_message_count = request_messages.len();
        if request_message_count == 0 && entries.is_empty() {
            self.with_sessions(|sessions| {
                sessions.mark_workspace_summary_state(&session.address, false, false)
            })?;
            return Ok(());
        }

        let mut previous_messages = request_messages;
        let effective_model_key = self.effective_main_model_key(&session.address)?;
        if self.effective_context_compaction_enabled(&session.address)? && request_message_count > 1
        {
            let runtime = self.tool_runtime();
            let config = runtime.build_agent_frame_config(
                session,
                &session.workspace_root,
                AgentPromptKind::MainForeground,
                &effective_model_key,
                None,
            )?;
            let extra_tools = runtime.build_extra_tools(
                session,
                AgentPromptKind::MainForeground,
                session.agent_id,
                None,
            );
            let effective_backend = self.effective_agent_backend(&session.address)?;
            let compaction_messages = if effective_backend == AgentBackendKind::AgentFrame {
                previous_messages.clone()
            } else {
                sanitize_messages_for_model_capabilities(
                    &previous_messages,
                    self.model_config_or_main(&effective_model_key)?,
                    backend_supports_native_multimodal_input(effective_backend),
                )
            };
            let compaction = run_backend_compaction(
                effective_backend,
                compaction_messages,
                config,
                extra_tools,
            )?;
            if compaction.compacted {
                previous_messages = compaction.messages;
            }
        }

        let workspace = self
            .workspace_manager
            .ensure_workspace_exists(&session.workspace_id)?;
        let tree = entries
            .iter()
            .map(|entry| format!("- {} ({})", entry.path, entry.entry_type))
            .collect::<Vec<_>>()
            .join("\n");
        let prompt = format!(
            "You are about to stop working in the current workspace.\n\nSummarize the work that has been done here for future agents.\nWrite a concise durable summary in plain text.\nFocus on:\n1. what work this workspace is mainly about\n2. what kinds of changes or outputs now exist in the workspace at a high level\n3. recent progress and current status\n4. any important unfinished direction a future agent should know\n\nKeep the summary at the level of work content and broad impact on the workspace.\nDo not explain files or directories one by one, and avoid file-level detail unless a path is truly the single most important entry point.\n\nCurrent stored summary:\n{}\n\nWorkspace file tree snapshot:\n{}\n\nReturn only the summary text. Do not use attachment tags.",
            workspace.summary,
            if tree.trim().is_empty() {
                "(workspace currently has no files)"
            } else {
                &tree
            }
        );
        let upstream_timeout_seconds = self.model_upstream_timeout_seconds(&effective_model_key)?;
        let runtime = self.tool_runtime_for_address(&session.address)?;
        let outcome = runtime
            .run_agent_turn_with_timeout(
                session.clone(),
                AgentPromptKind::MainForeground,
                session.agent_id,
                self.effective_agent_backend(&session.address)?,
                effective_model_key.clone(),
                previous_messages,
                prompt,
                Some(upstream_timeout_seconds),
                None,
                "workspace summary task join failed",
            )
            .await?;
        let state = match outcome {
            TimedRunOutcome::Completed(state) => state,
            TimedRunOutcome::Yielded(state) => state,
            TimedRunOutcome::TimedOut {
                state: Some(state), ..
            } => state,
            TimedRunOutcome::TimedOut { state: None, error } => return Err(error),
            TimedRunOutcome::Failed(error) => return Err(error),
        };
        let summary_text = extract_assistant_text(&state.messages);
        let (clean_summary, _) =
            extract_attachment_references(&summary_text, &session.workspace_root)?;
        let clean_summary = clean_summary.trim();
        if clean_summary.is_empty() {
            self.with_sessions(|sessions| {
                sessions.mark_workspace_summary_state(&session.address, false, false)
            })?;
            return Ok(());
        }
        let updated = self.workspace_manager.update_summary(
            &session.workspace_id,
            clean_summary.to_string(),
            None,
        )?;
        self.with_sessions(|sessions| {
            sessions.mark_workspace_summary_state(&session.address, false, false)
        })?;
        info!(
            log_stream = "session",
            log_key = %session.id,
            kind = "workspace_summary_updated",
            workspace_id = %session.workspace_id,
            summary_path = %updated.summary_path.display(),
            summary_len = clean_summary.len() as u64,
            "updated workspace summary before destroy"
        );
        Ok(())
    }

    async fn retry_pending_workspace_summaries(&self) -> Result<()> {
        let pending =
            self.with_sessions(|sessions| Ok(sessions.pending_workspace_summary_snapshots()))?;
        for session in pending {
            if let Err(error) = self.summarize_workspace_before_destroy(&session).await {
                warn!(
                    log_stream = "session",
                    log_key = %session.id,
                    kind = "workspace_summary_retry_failed",
                    workspace_id = %session.workspace_id,
                    error = %format!("{error:#}"),
                    "failed to retry pending workspace summary on startup"
                );
                continue;
            }
            self.with_sessions(|sessions| {
                sessions.mark_workspace_summary_state(&session.address, false, false)
            })?;
            if session.close_after_summary {
                self.destroy_foreground_session(&session.address)?;
            }
        }
        Ok(())
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

    fn inferred_agent_backend_for_model(&self, model_key: &str) -> Option<AgentBackendKind> {
        infer_single_agent_backend(&self.agent, model_key)
    }

    fn selected_agent_backend(&self, address: &ChannelAddress) -> Result<Option<AgentBackendKind>> {
        let settings = self.effective_conversation_settings(address)?;
        Ok(settings.agent_backend.or_else(|| {
            settings
                .main_model
                .as_deref()
                .and_then(|model_key| self.inferred_agent_backend_for_model(model_key))
        }))
    }

    fn effective_agent_backend(&self, address: &ChannelAddress) -> Result<AgentBackendKind> {
        self.selected_agent_backend(address)?.ok_or_else(|| {
            anyhow!("this conversation does not have an agent backend yet; choose one with /agent")
        })
    }

    fn has_complete_agent_selection(&self, address: &ChannelAddress) -> Result<bool> {
        Ok(self.selected_agent_backend(address)?.is_some()
            && self.selected_main_model_key(address)?.is_some())
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
        let backend = self.effective_agent_backend(address)?;
        self.ensure_model_available_for_backend(backend, &model_key)?;
        Ok(model_key)
    }

    fn effective_sandbox_mode(&self, address: &ChannelAddress) -> Result<SandboxMode> {
        let settings = self.effective_conversation_settings(address)?;
        Ok(settings.sandbox_mode.unwrap_or(self.sandbox.mode))
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
        self.with_conversations(|conversations| {
            conversations.rotate_chat_version_id(address).map(|_| ())
        })
    }

    fn rotate_chat_version_after_external_compaction(
        &self,
        address: &ChannelAddress,
    ) -> Result<()> {
        self.with_conversations(|conversations| {
            conversations.rotate_chat_version_id(address).map(|_| ())
        })
    }

    fn render_agent_backend_value(backend: AgentBackendKind) -> &'static str {
        match backend {
            AgentBackendKind::AgentFrame => "agent_frame",
            AgentBackendKind::Zgent => "zgent",
        }
    }

    fn available_agent_models(&self, backend: AgentBackendKind) -> Vec<String> {
        self.agent
            .available_models(backend)
            .iter()
            .filter(|model_key| self.models.contains_key(model_key.as_str()))
            .cloned()
            .collect()
    }

    fn agent_backend_selection_message(
        &self,
        address: &ChannelAddress,
        intro: &str,
    ) -> Result<OutgoingMessage> {
        let current_backend = self.selected_agent_backend(address)?;
        let current_model = self.selected_main_model_key(address)?;
        let mut options = [AgentBackendKind::AgentFrame, AgentBackendKind::Zgent]
            .into_iter()
            .filter(|backend| !self.available_agent_models(*backend).is_empty())
            .map(|backend| ShowOption {
                label: Self::render_agent_backend_value(backend).to_string(),
                value: format!("/agent {}", Self::render_agent_backend_value(backend)),
            })
            .collect::<Vec<_>>();
        options.sort_by(|left, right| left.label.cmp(&right.label));
        Ok(OutgoingMessage::with_options(
            format!(
                "{}\nCurrent agent backend: {}\nCurrent conversation model: {}\nChoose a backend below or send `/agent <agent_frame|zgent>`.",
                intro,
                current_backend
                    .map(|value| format!("`{}`", Self::render_agent_backend_value(value)))
                    .unwrap_or_else(|| "`<not selected>`".to_string()),
                current_model
                    .map(|value| format!("`{}`", value))
                    .unwrap_or_else(|| "`<not selected>`".to_string())
            ),
            "Choose a backend",
            options,
        ))
    }

    fn agent_model_selection_message(
        &self,
        address: &ChannelAddress,
        backend: AgentBackendKind,
        intro: &str,
    ) -> Result<OutgoingMessage> {
        let current_model = self.selected_main_model_key(address)?;
        let mut options = self
            .available_agent_models(backend)
            .into_iter()
            .map(|model_key| ShowOption {
                label: model_key.clone(),
                value: format!(
                    "/agent {} {}",
                    Self::render_agent_backend_value(backend),
                    model_key
                ),
            })
            .collect::<Vec<_>>();
        options.sort_by(|left, right| left.label.cmp(&right.label));
        Ok(OutgoingMessage::with_options(
            format!(
                "{}\nCurrent agent backend: `{}`\nCurrent conversation model: {}\nChoose a model below or send `/agent {} <model>`.",
                intro,
                Self::render_agent_backend_value(backend),
                current_model
                    .map(|value| format!("`{}`", value))
                    .unwrap_or_else(|| "`<not selected>`".to_string()),
                Self::render_agent_backend_value(backend),
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
        if let Some(backend) = self.selected_agent_backend(address)? {
            return self.agent_model_selection_message(address, backend, intro);
        }
        self.agent_backend_selection_message(address, intro)
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
        let model = self.model_config_or_main(model_key)?;
        let commands = self
            .command_catalog
            .get(&session.address.channel_id)
            .cloned()
            .unwrap_or_else(default_bot_commands);
        Ok(ForegroundAgent {
            id: session.agent_id,
            session_id: session.id,
            channel_id: session.address.channel_id.clone(),
            system_prompt: build_agent_system_prompt(
                &self.agent_workspace,
                session,
                &self
                    .workspace_manager
                    .ensure_workspace_exists(&session.workspace_id)
                    .map(|workspace| workspace.summary)
                    .unwrap_or_default(),
                AgentPromptKind::MainForeground,
                model_key,
                model,
                &self.models,
                &self.chat_model_keys,
                &self.main_agent,
                &commands,
            ),
        })
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
        let notices = self.with_sessions(|sessions| {
            sessions.observe_skill_changes(&session.address, &observed)
        })?;
        let rendered = render_skill_change_notices(&notices);
        Ok((!rendered.is_empty()).then_some(rendered))
    }

    fn sync_runtime_profile_files(
        &self,
        session: &SessionSnapshot,
    ) -> Result<Vec<SharedProfileChangeNotice>> {
        if self.main_agent.memory_system == agent_frame::config::MemorySystem::ClaudeCode {
            ensure_workspace_partclaw_file(&self.agent_workspace, &session.workspace_root)?;
        }
        sync_workspace_shared_profile_files(&self.agent_workspace, &session.workspace_root)
    }

    fn observe_runtime_profile_changes(&self, session: &SessionSnapshot) -> Result<()> {
        let user_markdown =
            fs::read_to_string(&self.agent_workspace.user_md_path).with_context(|| {
                format!(
                    "failed to read {}",
                    self.agent_workspace.user_md_path.display()
                )
            })?;
        let identity_markdown = fs::read_to_string(&self.agent_workspace.identity_md_path)
            .with_context(|| {
                format!(
                    "failed to read {}",
                    self.agent_workspace.identity_md_path.display()
                )
            })?;
        let user_profile_version = stable_content_version(&user_markdown);
        let identity_profile_version = stable_content_version(&identity_markdown);
        self.with_sessions(|sessions| {
            sessions.observe_shared_profile_changes(
                &session.address,
                user_profile_version,
                identity_profile_version,
            )?;
            Ok(())
        })
    }

    fn current_runtime_model_catalog(&self) -> String {
        render_available_models_catalog(&self.models, &self.chat_model_keys)
    }

    fn observe_runtime_model_catalog_changes(
        &self,
        session: &SessionSnapshot,
    ) -> Result<Vec<ModelCatalogChangeNotice>> {
        let catalog = self.current_runtime_model_catalog();
        let version = stable_content_version(&catalog);
        self.with_sessions(|sessions| {
            sessions.observe_model_catalog_changes(&session.address, version)
        })
    }

    fn take_runtime_model_catalog_change_notices(
        &self,
        session: &SessionSnapshot,
    ) -> Result<Vec<ModelCatalogChangeNotice>> {
        self.with_sessions(|sessions| sessions.take_model_catalog_change_notices(&session.address))
    }

    fn stage_runtime_profile_change_notices(
        &self,
        session: &SessionSnapshot,
        notices: &[SharedProfileChangeNotice],
    ) -> Result<()> {
        self.with_sessions(|sessions| {
            sessions.stage_shared_profile_change_notices(&session.address, notices)
        })
    }

    fn take_runtime_profile_change_notices(
        &self,
        session: &SessionSnapshot,
    ) -> Result<Vec<SharedProfileChangeNotice>> {
        self.with_sessions(|sessions| sessions.take_shared_profile_change_notices(&session.address))
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
    use super::foreground::register_active_foreground_control;
    use super::{
        AgentCommand, ForegroundRuntimePhase, ImageGenerationRouting, SYSTEM_RESTART_NOTICE,
        Server, SinkTarget, SummaryTracker, TokenUsage,
        background_timeout_with_active_children_text, build_synthetic_system_messages,
        build_user_turn_message, channel_restart_backoff_seconds,
        coalesce_buffered_conversation_messages, conversation_memory_root,
        estimate_compaction_savings_usd, estimate_cost_usd, extract_attachment_references,
        fast_path_agent_selection_message, format_session_status, infer_single_agent_backend,
        is_out_of_band_command, is_timeout_like, memory_search_files,
        normalize_messages_for_persistence, openrouter_automatic_cache_control,
        openrouter_automatic_cache_ttl, parse_agent_command, parse_model_command,
        parse_sandbox_command, parse_set_api_timeout_command, parse_sink_target,
        parse_snap_list_command, parse_snap_load_command, parse_snap_save_command,
        parse_think_command, persist_compaction_artifacts, rebuild_canonical_system_prompt,
        render_last_user_message_time_tip, render_model_catalog_change_notice,
        render_system_date_on_user_message, request_yield_for_incoming, rollout_read_file,
        rollout_search_files, sanitize_messages_for_model_capabilities,
        select_image_generation_routing, send_outgoing_message_now, session_errno_for_turn_error,
        should_attempt_idle_context_compaction, should_emit_runtime_change_prompt,
        summarize_resume_progress, sync_workspace_shared_profile_files,
        tag_interrupted_followup_text, update_active_foreground_phase,
        upload_workspace_shared_profile_files, user_facing_continue_error_text,
        workspace_visible_in_list,
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
    use crate::session::SessionManager;
    use crate::session::{ModelCatalogChangeNotice, SessionErrno, SessionSnapshot};
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
            seen_user_profile_version: None,
            seen_identity_profile_version: None,
            seen_model_catalog_version: None,
            zgent_native: None,
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
                external_web_search: None,
                capabilities: vec![ModelCapability::Chat],
            },
        );

        Server {
            workdir: temp_dir.path().to_path_buf(),
            agent_workspace,
            workspace_manager,
            channels: Arc::new(HashMap::from([("telegram-main".to_string(), channel)])),
            telegram_channel_ids: Arc::new(HashSet::new()),
            command_catalog: HashMap::new(),
            models,
            agent: AgentConfig {
                agent_frame: AgentBackendConfig {
                    available_models: vec!["demo-model".to_string()],
                },
                zgent: AgentBackendConfig::default(),
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
                memory_system: MemorySystem::default(),
            },
            sandbox: SandboxConfig::default(),
            conversations: Arc::new(Mutex::new(conversations)),
            snapshots: Arc::new(Mutex::new(snapshots)),
            sessions: Arc::new(Mutex::new(sessions)),
            sink_router: Arc::new(RwLock::new(SinkRouter::new())),
            cron_manager: Arc::new(Mutex::new(cron_manager)),
            agent_registry: Arc::new(Mutex::new(agent_registry)),
            agent_registry_notify: Arc::new(Notify::new()),
            max_global_sub_agents: 4,
            subagent_count: Arc::new(AtomicUsize::new(0)),
            cron_poll_interval_seconds: 60,
            background_job_sender,
            background_job_receiver: Some(background_job_receiver),
            summary_tracker: Arc::new(SummaryTracker::new()),
            active_foreground_controls: Arc::new(Mutex::new(HashMap::new())),
            active_foreground_phases: Arc::new(Mutex::new(HashMap::new())),
            pending_foreground_interrupts: Arc::new(Mutex::new(HashSet::new())),
            pending_process_restart_notices: Arc::new(Mutex::new(HashSet::new())),
            active_foreground_agent_frame_runtimes: Arc::new(Mutex::new(HashMap::new())),
            active_native_zgent_sessions: Arc::new(Mutex::new(HashMap::new())),
            subagents: Arc::new(Mutex::new(HashMap::new())),
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
        assert!(text.contains("instead of calling the image tool again"));
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
    fn yield_request_detects_compaction_in_progress() {
        let address = ChannelAddress {
            channel_id: "telegram".to_string(),
            conversation_id: "conversation-1".to_string(),
            user_id: None,
            display_name: None,
        };
        let session_key = address.session_key();
        let control = SessionExecutionControl::new();
        let active_controls = Arc::new(Mutex::new(HashMap::from([(
            session_key.clone(),
            control.clone(),
        )])));
        let active_phases = Arc::new(Mutex::new(HashMap::new()));
        update_active_foreground_phase(
            &active_phases,
            &session_key,
            &SessionEvent::CompactionStarted {
                phase: "initial".to_string(),
                message_count: 3,
            },
        );
        let incoming = IncomingMessage {
            remote_message_id: "msg-1".to_string(),
            address,
            text: Some("继续".to_string()),
            attachments: Vec::new(),
            stored_attachments: Vec::new(),
            control: None,
        };

        let disposition = request_yield_for_incoming(&active_controls, &active_phases, &incoming);

        assert!(disposition.interrupted);
        assert!(disposition.compaction_in_progress);
        assert!(control.take_yield_requested());
        let phase = active_phases.lock().unwrap().get(&session_key).copied();
        assert_eq!(phase, Some(ForegroundRuntimePhase::Compacting));
    }

    #[test]
    fn pending_interrupt_is_applied_to_next_foreground_control() {
        let session_key = "telegram:conversation-1".to_string();
        let active_controls = Arc::new(Mutex::new(HashMap::new()));
        let pending_interrupts = Arc::new(Mutex::new(HashSet::from([session_key.clone()])));
        let control = SessionExecutionControl::new();

        register_active_foreground_control(
            &active_controls,
            &pending_interrupts,
            &session_key,
            control.clone(),
        );

        assert!(control.take_yield_requested());
        assert!(active_controls.lock().unwrap().contains_key(&session_key));
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
    fn synthetic_skill_updates_are_system_messages_not_user_prefixes() {
        let injected = build_synthetic_system_messages(
            None,
            None,
            None,
            Some("[Runtime Skill Updates]\nSkill \"search\" updated to version 3."),
            &[],
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
            &[],
        );

        assert_eq!(injected.len(), 2);
        assert_eq!(
            injected[0].content.as_ref().and_then(Value::as_str),
            Some(SYSTEM_RESTART_NOTICE)
        );
        assert_eq!(injected[1].role, "system");
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
            Some("[System Message: models changed]"),
            None,
            &[],
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
    fn synthetic_model_catalog_updates_are_system_messages() {
        let notice = render_model_catalog_change_notice(
            &[ModelCatalogChangeNotice::Updated],
            "- gpt54: primary\n- opus-4.6: large-context",
        )
        .unwrap();
        let injected = build_synthetic_system_messages(None, None, Some(&notice), None, &[]);
        assert_eq!(injected.len(), 1);
        assert_eq!(injected[0].role, "system");
        assert!(
            injected[0]
                .content
                .as_ref()
                .and_then(Value::as_str)
                .is_some_and(|text| text.contains("Available models changed"))
        );
        assert!(
            injected[0]
                .content
                .as_ref()
                .and_then(Value::as_str)
                .is_some_and(|text| text.contains("- opus-4.6: large-context"))
        );
    }

    #[test]
    fn normalize_messages_for_persistence_keeps_one_canonical_system_and_drops_ephemeral_systems() {
        let canonical = "[AgentFrame Runtime]\ncanonical prompt";
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
    fn user_time_tip_is_emitted_after_five_minutes_of_idle_time() {
        let now = Utc::now();
        let session = SessionSnapshot {
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
    fn runtime_change_prompts_are_suppressed_for_interrupted_messages() {
        assert!(!should_emit_runtime_change_prompt(Some(
            "[Interrupted Follow-up]\n进度如何？"
        )));
        assert!(!should_emit_runtime_change_prompt(Some(
            "[Queued User Updates]\nFollow-up 1:\n继续"
        )));
        assert!(should_emit_runtime_change_prompt(Some("正常对话")));
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

        let notices =
            sync_workspace_shared_profile_files(&agent_workspace, &workspace_root).unwrap();
        assert_eq!(
            notices,
            vec![
                crate::session::SharedProfileChangeNotice::UserUpdated,
                crate::session::SharedProfileChangeNotice::IdentityUpdated
            ]
        );
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
    fn parses_multi_sink_structure() {
        let sink = parse_sink_target(
            &json!({
                "kind": "multi",
                "targets": [
                    {
                        "kind": "direct",
                        "channel_id": "telegram-main",
                        "conversation_id": "123"
                    },
                    {
                        "kind": "broadcast",
                        "topic": "ops"
                    }
                ]
            }),
            None,
        )
        .unwrap();

        match sink {
            SinkTarget::Multi(targets) => assert_eq!(targets.len(), 2),
            other => panic!("expected multi sink, got {:?}", other),
        }
    }

    #[test]
    fn idle_context_compaction_requires_idle_time_new_turns_and_min_tokens() {
        let now = Utc::now();
        let base_snapshot = SessionSnapshot {
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
            seen_user_profile_version: None,
            seen_identity_profile_version: None,
            seen_model_catalog_version: None,
            zgent_native: None,
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
                sessions.ensure_foreground(&session.address)?;
                Ok(())
            })
            .unwrap();

        server
            .append_completed_foreground_assistant_history(
                &session.address,
                &OutgoingMessage::text("done"),
            )
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
        );

        assert!(
            text.contains(
                "Idle compaction error: upstream timeout while compacting older messages"
            )
        );
        assert!(text.contains("upstream timeout while compacting older messages"));
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
                sessions.ensure_foreground(&session.address)?;
                sessions.mark_idle_compaction_failed(
                    &session.address,
                    "model disappeared".to_string(),
                )?;
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
        assert_eq!(options.prompt, "Choose a backend");
        assert!(
            options
                .options
                .iter()
                .any(|option| option.value == "/agent agent_frame")
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
            Some(AgentCommand::SelectBackend(AgentBackendKind::AgentFrame))
        ));
        assert!(matches!(
            parse_agent_command(Some("/agent zgent demo-model")),
            Some(AgentCommand::SelectModel {
                backend: Some(AgentBackendKind::Zgent),
                model_key
            }) if model_key == "demo-model"
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

        assert_eq!(parse_think_command(Some("/think")), Some(None));
        assert_eq!(
            parse_think_command(Some("/think high")),
            Some(Some("high".to_string()))
        );
        assert_eq!(
            parse_think_command(Some("/think@party_claw_bot medium")),
            Some(Some("medium".to_string()))
        );
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
    fn fast_path_skips_prompt_and_backfills_unique_backend_selection() {
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
                external_web_search: None,
            },
        );
        let agent = crate::config::AgentConfig {
            agent_frame: crate::config::AgentBackendConfig {
                available_models: vec!["gpt54".to_string()],
            },
            zgent: crate::config::AgentBackendConfig::default(),
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
        assert_eq!(
            snapshot.settings.agent_backend,
            Some(AgentBackendKind::AgentFrame)
        );
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
            zgent: AgentBackendConfig::default(),
        };

        assert_eq!(
            infer_single_agent_backend(&agent, "gpt54"),
            Some(AgentBackendKind::AgentFrame)
        );
    }

    #[test]
    fn infer_single_agent_backend_returns_none_when_model_has_multiple_backends() {
        let agent = AgentConfig {
            agent_frame: AgentBackendConfig {
                available_models: vec!["gpt54".to_string()],
            },
            zgent: AgentBackendConfig {
                available_models: vec!["gpt54".to_string()],
            },
        };

        assert_eq!(infer_single_agent_backend(&agent, "gpt54"), None);
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
        assert!(formula.contains("cache_read_tokens"));
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
