use crate::backend::AgentBackendKind;
use crate::zgent::zgent_runtime_available;
use agent_frame::config::{
    AuthCredentialsStoreMode, ExternalWebSearchConfig, MemorySystem, NativeWebSearchConfig,
    ReasoningConfig, UpstreamApiKind, UpstreamAuthKind,
};
use anyhow::{Context, Result, anyhow};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use std::collections::BTreeMap;
use std::fs;
use std::path::Path;

mod v0_1;
mod v0_10;
mod v0_11;
mod v0_12;
mod v0_2;
mod v0_3;
mod v0_4;
mod v0_5;
mod v0_6;
mod v0_7;
mod v0_8;
mod v0_9;

pub const LEGACY_CONFIG_VERSION: &str = "0.1";
pub const LATEST_CONFIG_VERSION: &str = "0.12";
pub const VERSION_0_2: &str = "0.2";
pub const VERSION_0_3: &str = "0.3";
pub const VERSION_0_4: &str = "0.4";
pub const VERSION_0_5: &str = "0.5";
pub const VERSION_0_6: &str = "0.6";
pub const VERSION_0_7: &str = "0.7";
pub const VERSION_0_8: &str = "0.8";
pub const VERSION_0_9: &str = "0.9";
pub const VERSION_0_10: &str = "0.10";
pub const VERSION_0_11: &str = "0.11";

trait ConfigLoader {
    fn version(&self) -> &'static str;
    fn load_and_upgrade(&self, value: Value) -> Result<ServerConfig>;
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct BotCommandConfig {
    pub command: String,
    pub description: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CommandLineChannelConfig {
    pub id: String,
    #[serde(default = "default_cli_prompt")]
    pub prompt: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TelegramChannelConfig {
    pub id: String,
    #[serde(default)]
    pub bot_token: Option<String>,
    #[serde(default = "default_telegram_bot_token_env")]
    pub bot_token_env: String,
    #[serde(default = "default_telegram_api_base_url")]
    pub api_base_url: String,
    #[serde(default = "default_poll_timeout_seconds")]
    pub poll_timeout_seconds: u64,
    #[serde(default = "default_poll_interval_ms")]
    pub poll_interval_ms: u64,
    #[serde(default = "default_telegram_commands")]
    pub commands: Vec<BotCommandConfig>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DingtalkChannelConfig {
    pub id: String,
    #[serde(default)]
    pub client_id: Option<String>,
    #[serde(default = "default_dingtalk_client_id_env")]
    pub client_id_env: String,
    #[serde(default)]
    pub client_secret: Option<String>,
    #[serde(default = "default_dingtalk_client_secret_env")]
    pub client_secret_env: String,
    #[serde(default = "default_dingtalk_api_base_url")]
    pub api_base_url: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ModelConfig {
    #[serde(rename = "type")]
    pub model_type: ModelType,
    pub api_endpoint: String,
    pub model: String,
    #[serde(default, skip_serializing, skip_deserializing)]
    pub backend: AgentBackendKind,
    #[serde(default)]
    pub supports_vision_input: bool,
    #[serde(default)]
    pub image_tool_model: Option<String>,
    #[serde(default)]
    #[serde(rename = "web_search", alias = "web_search_model")]
    pub web_search_model: Option<String>,
    #[serde(default)]
    pub api_key: Option<String>,
    #[serde(default = "default_api_key_env")]
    pub api_key_env: String,
    #[serde(default = "default_chat_completions_path")]
    pub chat_completions_path: String,
    #[serde(default)]
    pub codex_home: Option<String>,
    #[serde(default)]
    pub auth_credentials_store_mode: AuthCredentialsStoreMode,
    #[serde(default = "default_model_timeout_seconds")]
    pub timeout_seconds: f64,
    #[serde(default = "default_context_window_tokens")]
    pub context_window_tokens: usize,
    #[serde(default)]
    pub cache_ttl: Option<String>,
    #[serde(default)]
    pub reasoning: Option<ReasoningConfig>,
    #[serde(default)]
    pub headers: Map<String, Value>,
    #[serde(default)]
    pub description: String,
    #[serde(default = "default_agent_model_enabled")]
    pub agent_model_enabled: bool,
    #[serde(default)]
    pub capabilities: Vec<ModelCapability>,
    #[serde(default)]
    pub native_web_search: Option<NativeWebSearchConfig>,
    #[serde(default)]
    #[serde(skip_serializing)]
    pub external_web_search: Option<ExternalWebSearchConfig>,
}

impl ModelConfig {
    pub fn upstream_api_kind(&self) -> UpstreamApiKind {
        match self.model_type {
            ModelType::Openrouter => UpstreamApiKind::ChatCompletions,
            ModelType::OpenrouterResp | ModelType::CodexSubscription => UpstreamApiKind::Responses,
        }
    }

    pub fn upstream_auth_kind(&self) -> UpstreamAuthKind {
        match self.model_type {
            ModelType::CodexSubscription => UpstreamAuthKind::CodexSubscription,
            ModelType::Openrouter | ModelType::OpenrouterResp => UpstreamAuthKind::ApiKey,
        }
    }

    pub fn has_capability(&self, capability: ModelCapability) -> bool {
        self.capabilities.contains(&capability)
    }

    pub fn supports_image_input(&self) -> bool {
        self.supports_vision_input || self.has_capability(ModelCapability::ImageIn)
    }

    pub fn can_be_agent_model(&self) -> bool {
        self.agent_model_enabled && self.has_capability(ModelCapability::Chat)
    }
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum ModelType {
    Openrouter,
    OpenrouterResp,
    CodexSubscription,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "snake_case")]
pub enum ModelCapability {
    Chat,
    WebSearch,
    ImageIn,
    ImageOut,
    Pdf,
    AudioIn,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct AgentBackendConfig {
    #[serde(default)]
    pub available_models: Vec<String>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct AgentConfig {
    #[serde(default)]
    pub agent_frame: AgentBackendConfig,
    #[serde(default)]
    pub zgent: AgentBackendConfig,
}

impl AgentConfig {
    pub fn backend_config(&self, backend: AgentBackendKind) -> &AgentBackendConfig {
        match backend {
            AgentBackendKind::AgentFrame => &self.agent_frame,
            AgentBackendKind::Zgent => &self.zgent,
        }
    }

    pub fn backend_config_mut(&mut self, backend: AgentBackendKind) -> &mut AgentBackendConfig {
        match backend {
            AgentBackendKind::AgentFrame => &mut self.agent_frame,
            AgentBackendKind::Zgent => &mut self.zgent,
        }
    }

    pub fn available_models(&self, backend: AgentBackendKind) -> &[String] {
        &self.backend_config(backend).available_models
    }

    pub fn is_model_available(&self, backend: AgentBackendKind, model_key: &str) -> bool {
        self.available_models(backend)
            .iter()
            .any(|value| value == model_key)
    }

    pub fn backends_for_model(&self, model_key: &str) -> Vec<AgentBackendKind> {
        [AgentBackendKind::AgentFrame, AgentBackendKind::Zgent]
            .into_iter()
            .filter(|backend| self.is_model_available(*backend, model_key))
            .collect()
    }

    pub fn is_empty(&self) -> bool {
        self.agent_frame.available_models.is_empty() && self.zgent.available_models.is_empty()
    }

    pub fn all_available_models(&self) -> Vec<String> {
        let mut result = Vec::new();
        for backend in [AgentBackendKind::AgentFrame, AgentBackendKind::Zgent] {
            for model_key in self.available_models(backend) {
                if !result.iter().any(|value| value == model_key) {
                    result.push(model_key.clone());
                }
            }
        }
        result
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ToolingTarget {
    pub alias: String,
    pub prefer_self: bool,
}

impl ToolingTarget {
    pub fn parse(raw: &str) -> Result<Self> {
        let raw = raw.trim();
        if raw.is_empty() {
            return Err(anyhow!("tooling target must not be empty"));
        }
        let mut alias = raw;
        let mut prefer_self = false;
        if let Some((before, after)) = raw.split_once(':') {
            alias = before.trim();
            let suffix = after.trim();
            if suffix != "self" {
                return Err(anyhow!(
                    "unsupported tooling target suffix '{}'; expected ':self'",
                    suffix
                ));
            }
            prefer_self = true;
        }
        if alias.is_empty() {
            return Err(anyhow!("tooling target alias must not be empty"));
        }
        Ok(Self {
            alias: alias.to_string(),
            prefer_self,
        })
    }

    pub fn as_config_string(&self) -> String {
        if self.prefer_self {
            format!("{}:self", self.alias)
        } else {
            self.alias.clone()
        }
    }
}

impl Serialize for ToolingTarget {
    fn serialize<S>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer.serialize_str(&self.as_config_string())
    }
}

impl<'de> Deserialize<'de> for ToolingTarget {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let raw = String::deserialize(deserializer)?;
        Self::parse(&raw).map_err(serde::de::Error::custom)
    }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct ToolingConfig {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub web_search: Option<ToolingTarget>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub image: Option<ToolingTarget>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub image_gen: Option<ToolingTarget>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pdf: Option<ToolingTarget>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub audio_input: Option<ToolingTarget>,
}

impl ToolingConfig {
    pub fn is_empty(&self) -> bool {
        self.web_search.is_none()
            && self.image.is_none()
            && self.image_gen.is_none()
            && self.pdf.is_none()
            && self.audio_input.is_none()
    }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct ContextCompactionConfig {
    #[serde(default = "default_compact_trigger_ratio")]
    pub trigger_ratio: f64,
    #[serde(default)]
    pub token_limit_override: Option<usize>,
    #[serde(default = "default_recent_fidelity_target_ratio")]
    pub recent_fidelity_target_ratio: f64,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct IdleCompactionConfig {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default = "default_idle_context_compaction_poll_interval_seconds")]
    pub poll_interval_seconds: u64,
    #[serde(default = "default_idle_compact_min_ratio")]
    pub min_ratio: f64,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct TimeoutObservationCompactionConfig {
    #[serde(default = "default_enable_timeout_observation_compaction")]
    pub enabled: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TimeAwarenessConfig {
    #[serde(default = "default_emit_system_date_on_user_message")]
    pub emit_system_date_on_user_message: bool,
    #[serde(default = "default_emit_idle_time_gap_hint")]
    pub emit_idle_time_gap_hint: bool,
}

impl Default for TimeAwarenessConfig {
    fn default() -> Self {
        Self {
            emit_system_date_on_user_message: default_emit_system_date_on_user_message(),
            emit_idle_time_gap_hint: default_emit_idle_time_gap_hint(),
        }
    }
}

#[derive(Clone, Debug, Serialize)]
pub struct MainAgentConfig {
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub timeout_seconds: Option<f64>,
    #[serde(default = "default_global_install_root")]
    pub global_install_root: String,
    #[serde(default = "default_main_agent_language")]
    pub language: String,
    #[serde(default = "default_enabled_tools")]
    pub enabled_tools: Vec<String>,
    #[serde(default = "default_max_tool_roundtrips")]
    pub max_tool_roundtrips: usize,
    #[serde(default = "default_enable_context_compression")]
    pub enable_context_compression: bool,
    #[serde(default)]
    pub context_compaction: ContextCompactionConfig,
    #[serde(default)]
    pub idle_compaction: IdleCompactionConfig,
    #[serde(default)]
    pub timeout_observation_compaction: TimeoutObservationCompactionConfig,
    #[serde(default)]
    pub time_awareness: TimeAwarenessConfig,
    #[serde(default)]
    pub memory_system: MemorySystem,
}

#[derive(Debug, Deserialize)]
struct MainAgentConfigRaw {
    #[serde(default)]
    model: Option<String>,
    #[serde(default)]
    timeout_seconds: Option<f64>,
    #[serde(default = "default_global_install_root")]
    global_install_root: String,
    #[serde(default = "default_main_agent_language")]
    language: String,
    #[serde(default = "default_enabled_tools")]
    enabled_tools: Vec<String>,
    #[serde(default = "default_max_tool_roundtrips")]
    max_tool_roundtrips: usize,
    #[serde(default = "default_enable_context_compression")]
    enable_context_compression: bool,
    #[serde(default)]
    context_compaction: Option<ContextCompactionConfig>,
    #[serde(default)]
    idle_compaction: Option<IdleCompactionConfig>,
    #[serde(default)]
    timeout_observation_compaction: Option<TimeoutObservationCompactionConfig>,
    #[serde(default)]
    time_awareness: Option<TimeAwarenessConfig>,
    #[serde(default)]
    memory_system: MemorySystem,
    #[serde(default = "default_compact_trigger_ratio")]
    compact_trigger_ratio: f64,
    #[serde(default = "default_effective_context_window_percent")]
    effective_context_window_percent: f64,
    #[serde(default = "default_idle_compact_min_ratio")]
    idle_compact_min_ratio: f64,
    #[serde(default = "default_recent_fidelity_target_ratio")]
    recent_fidelity_target_ratio: f64,
    #[serde(default)]
    auto_compact_token_limit: Option<usize>,
    #[serde(default)]
    enable_idle_context_compaction: bool,
    #[serde(default = "default_idle_context_compaction_poll_interval_seconds")]
    idle_context_compaction_poll_interval_seconds: u64,
}

impl<'de> Deserialize<'de> for MainAgentConfig {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let raw = MainAgentConfigRaw::deserialize(deserializer)?;
        let legacy_trigger_ratio = if (raw.effective_context_window_percent
            - default_effective_context_window_percent())
        .abs()
            > f64::EPSILON
        {
            raw.effective_context_window_percent
        } else {
            raw.compact_trigger_ratio
        };
        Ok(Self {
            model: raw.model,
            timeout_seconds: raw.timeout_seconds,
            global_install_root: raw.global_install_root,
            language: raw.language,
            enabled_tools: normalize_enabled_tools(raw.enabled_tools),
            max_tool_roundtrips: raw.max_tool_roundtrips,
            enable_context_compression: raw.enable_context_compression,
            context_compaction: raw.context_compaction.unwrap_or(ContextCompactionConfig {
                trigger_ratio: legacy_trigger_ratio,
                token_limit_override: raw.auto_compact_token_limit,
                recent_fidelity_target_ratio: raw.recent_fidelity_target_ratio,
            }),
            idle_compaction: raw.idle_compaction.unwrap_or(IdleCompactionConfig {
                enabled: raw.enable_idle_context_compaction,
                poll_interval_seconds: raw.idle_context_compaction_poll_interval_seconds,
                min_ratio: raw.idle_compact_min_ratio,
            }),
            timeout_observation_compaction: raw.timeout_observation_compaction.unwrap_or_default(),
            time_awareness: raw.time_awareness.unwrap_or_default(),
            memory_system: raw.memory_system,
        })
    }
}

#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SandboxMode {
    #[default]
    Disabled,
    Subprocess,
    Bubblewrap,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct SandboxConfig {
    #[serde(default)]
    pub mode: SandboxMode,
    #[serde(default = "default_bubblewrap_binary")]
    pub bubblewrap_binary: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ChannelConfig {
    CommandLine(CommandLineChannelConfig),
    Telegram(TelegramChannelConfig),
    Dingtalk(DingtalkChannelConfig),
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ServerConfig {
    pub version: String,
    pub models: BTreeMap<String, ModelConfig>,
    #[serde(default)]
    pub agent: AgentConfig,
    pub model_catalog: ModelCatalogConfig,
    #[serde(default)]
    pub tooling: ToolingConfig,
    #[serde(default)]
    pub chat_model_keys: Vec<String>,
    pub main_agent: MainAgentConfig,
    #[serde(default)]
    pub sandbox: SandboxConfig,
    #[serde(default = "default_max_global_sub_agents")]
    pub max_global_sub_agents: usize,
    #[serde(default = "default_cron_poll_interval_seconds")]
    pub cron_poll_interval_seconds: u64,
    pub channels: Vec<ChannelConfig>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct ModelCatalogConfig {
    #[serde(default)]
    pub chat: BTreeMap<String, ModelConfig>,
    #[serde(default)]
    pub vision: BTreeMap<String, ModelConfig>,
    #[serde(default)]
    pub web_search: BTreeMap<String, ExternalWebSearchConfig>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ResolvedModelApiKey {
    pub model_name: String,
    pub source: String,
    pub api_key: Option<String>,
}

fn slice_is_empty<T>(value: &[T]) -> bool {
    value.is_empty()
}

fn str_is_empty(value: &str) -> bool {
    value.is_empty()
}

fn default_cli_prompt() -> String {
    "you> ".to_string()
}

pub(crate) fn default_api_key_env() -> String {
    "OPENAI_API_KEY".to_string()
}

pub(crate) fn default_chat_completions_path() -> String {
    "/chat/completions".to_string()
}

pub(crate) fn default_responses_path() -> String {
    "/responses".to_string()
}

pub(crate) fn default_codex_subscription_endpoint() -> String {
    "https://chatgpt.com/backend-api/codex".to_string()
}

pub(crate) fn default_model_timeout_seconds() -> f64 {
    120.0
}

pub(crate) fn default_context_window_tokens() -> usize {
    128_000
}

fn default_main_agent_language() -> String {
    "zh-CN".to_string()
}

fn default_global_install_root() -> String {
    if cfg!(target_os = "windows") {
        "C:/ClawPartyPrograms".to_string()
    } else {
        "/opt".to_string()
    }
}

fn canonical_enabled_tool_name(tool_name: &str) -> &str {
    match tool_name {
        "read_file" => "file_read",
        "write_file" => "file_write",
        _ => tool_name,
    }
}

fn normalize_enabled_tools(enabled_tools: Vec<String>) -> Vec<String> {
    let mut normalized = Vec::new();
    for tool_name in enabled_tools {
        let canonical = canonical_enabled_tool_name(&tool_name).to_string();
        if !normalized.iter().any(|existing| existing == &canonical) {
            normalized.push(canonical);
        }
    }
    normalized
}

pub fn default_enabled_tools() -> Vec<String> {
    vec![
        "file_read".to_string(),
        "file_write".to_string(),
        "glob".to_string(),
        "grep".to_string(),
        "ls".to_string(),
        "edit".to_string(),
        "apply_patch".to_string(),
        "exec_start".to_string(),
        "exec_observe".to_string(),
        "exec_wait".to_string(),
        "exec_kill".to_string(),
        "file_download_start".to_string(),
        "file_download_progress".to_string(),
        "file_download_wait".to_string(),
        "file_download_cancel".to_string(),
        "web_fetch".to_string(),
        "web_search".to_string(),
        "image_start".to_string(),
        "image_wait".to_string(),
        "image_cancel".to_string(),
    ]
}

fn default_max_tool_roundtrips() -> usize {
    120
}

fn default_enable_context_compression() -> bool {
    true
}

fn default_compact_trigger_ratio() -> f64 {
    0.9
}

fn default_effective_context_window_percent() -> f64 {
    0.9
}

fn default_idle_compact_min_ratio() -> f64 {
    0.5
}

fn default_recent_fidelity_target_ratio() -> f64 {
    0.18
}

fn default_idle_context_compaction_poll_interval_seconds() -> u64 {
    15
}

fn default_enable_timeout_observation_compaction() -> bool {
    true
}

fn default_emit_system_date_on_user_message() -> bool {
    false
}

fn default_emit_idle_time_gap_hint() -> bool {
    true
}

fn default_telegram_bot_token_env() -> String {
    "TELEGRAM_BOT_TOKEN".to_string()
}

fn default_dingtalk_client_id_env() -> String {
    "DINGTALK_CLIENT_ID".to_string()
}

fn default_dingtalk_client_secret_env() -> String {
    "DINGTALK_CLIENT_SECRET".to_string()
}

fn default_telegram_api_base_url() -> String {
    "https://api.telegram.org".to_string()
}

fn default_dingtalk_api_base_url() -> String {
    "https://api.dingtalk.com".to_string()
}

fn default_poll_timeout_seconds() -> u64 {
    30
}

fn default_poll_interval_ms() -> u64 {
    250
}

pub fn default_bot_commands() -> Vec<BotCommandConfig> {
    vec![
        BotCommandConfig {
            command: "oldspace".to_string(),
            description: "Reactivate an older workspace by id".to_string(),
        },
        BotCommandConfig {
            command: "help".to_string(),
            description: "Show available commands".to_string(),
        },
        BotCommandConfig {
            command: "status".to_string(),
            description: "Show current session usage and timeout settings".to_string(),
        },
        BotCommandConfig {
            command: "compact".to_string(),
            description: "Compact the current conversation context now".to_string(),
        },
        BotCommandConfig {
            command: "compact_mode".to_string(),
            description: "Show or set automatic context compaction".to_string(),
        },
        BotCommandConfig {
            command: "agent".to_string(),
            description: "Show or set the conversation agent backend and model".to_string(),
        },
        BotCommandConfig {
            command: "sandbox".to_string(),
            description: "Show or set the conversation sandbox mode".to_string(),
        },
        BotCommandConfig {
            command: "think".to_string(),
            description: "Show or set the conversation reasoning effort".to_string(),
        },
        BotCommandConfig {
            command: "set_api_timeout".to_string(),
            description: "Set session API timeout in seconds".to_string(),
        },
        BotCommandConfig {
            command: "snapsave".to_string(),
            description: "Save a named global snapshot".to_string(),
        },
        BotCommandConfig {
            command: "snapload".to_string(),
            description: "Load a named global snapshot".to_string(),
        },
        BotCommandConfig {
            command: "snaplist".to_string(),
            description: "List saved global snapshots".to_string(),
        },
        BotCommandConfig {
            command: "continue".to_string(),
            description: "Continue the latest interrupted turn".to_string(),
        },
    ]
}

pub(crate) fn default_telegram_commands() -> Vec<BotCommandConfig> {
    let mut commands = default_bot_commands();
    commands.splice(
        0..0,
        [
            BotCommandConfig {
                command: "admin_authorize".to_string(),
                description: "Authorize yourself as this channel's admin from a private chat"
                    .to_string(),
            },
            BotCommandConfig {
                command: "admin_chat_list".to_string(),
                description: "List approval state for this channel's chats".to_string(),
            },
            BotCommandConfig {
                command: "admin_chat_approve".to_string(),
                description: "Approve a chat id for this channel".to_string(),
            },
            BotCommandConfig {
                command: "admin_chat_reject".to_string(),
                description: "Reject a chat id for this channel".to_string(),
            },
        ],
    );
    commands
}

pub(crate) fn default_dingtalk_commands() -> Vec<BotCommandConfig> {
    default_bot_commands()
}

pub(crate) fn default_max_global_sub_agents() -> usize {
    4
}

fn default_bubblewrap_binary() -> String {
    "bwrap".to_string()
}

pub(crate) fn default_cron_poll_interval_seconds() -> u64 {
    5
}

fn default_agent_model_enabled() -> bool {
    true
}

pub fn load_server_config_file(path: impl AsRef<Path>) -> Result<ServerConfig> {
    let path = path.as_ref();
    let raw = fs::read_to_string(path)
        .with_context(|| format!("failed to read config file {}", path.display()))?;
    let value: Value = serde_json::from_str(&raw).context("failed to parse server config JSON")?;
    let version = match value.get("version") {
        Some(Value::String(version)) => version.clone(),
        _ => LEGACY_CONFIG_VERSION.to_string(),
    };
    let loaders: [&dyn ConfigLoader; 12] = [
        &v0_1::LegacyConfigLoader,
        &v0_2::VersionedConfigLoader,
        &v0_3::VersionedConfigLoader,
        &v0_4::LatestConfigLoader,
        &v0_5::LatestConfigLoader,
        &v0_6::LatestConfigLoader,
        &v0_7::LatestConfigLoader,
        &v0_8::LatestConfigLoader,
        &v0_9::LatestConfigLoader,
        &v0_10::LatestConfigLoader,
        &v0_11::LatestConfigLoader,
        &v0_12::LatestConfigLoader,
    ];
    let loader = loaders
        .into_iter()
        .find(|loader| loader.version() == version)
        .ok_or_else(|| anyhow!("unsupported config version '{}'", version))?;
    let config = loader.load_and_upgrade(value)?;
    validate_server_config(&config)?;
    Ok(config)
}

pub(crate) fn build_server_config(
    version: String,
    mut models: BTreeMap<String, ModelConfig>,
    mut agent: AgentConfig,
    mut model_catalog: ModelCatalogConfig,
    tooling: ToolingConfig,
    mut chat_model_keys: Vec<String>,
    main_agent: MainAgentConfig,
    sandbox: SandboxConfig,
    max_global_sub_agents: usize,
    cron_poll_interval_seconds: u64,
    channels: Vec<ChannelConfig>,
) -> ServerConfig {
    for (name, model) in &mut models {
        normalize_model_capabilities(
            model,
            name,
            &chat_model_keys,
            &model_catalog.vision,
            &model_catalog.chat,
        );
    }
    if chat_model_keys.is_empty() {
        chat_model_keys = models
            .iter()
            .filter_map(|(name, model)| model.can_be_agent_model().then_some(name.clone()))
            .collect();
    }
    if agent.is_empty() {
        for (name, model) in &models {
            if !model.can_be_agent_model() {
                continue;
            }
            let available_models = &mut agent.backend_config_mut(model.backend).available_models;
            if !available_models.iter().any(|value| value == name) {
                available_models.push(name.clone());
            }
        }
    }
    for backend in [AgentBackendKind::AgentFrame, AgentBackendKind::Zgent] {
        let mut normalized = Vec::new();
        for model_key in agent.available_models(backend) {
            if !normalized.iter().any(|value| value == model_key) {
                normalized.push(model_key.clone());
            }
        }
        agent.backend_config_mut(backend).available_models = normalized;
    }
    let configured_chat_models = agent.all_available_models();
    if !configured_chat_models.is_empty() {
        chat_model_keys = configured_chat_models;
    }
    if model_catalog.chat.is_empty() {
        model_catalog.chat = models
            .iter()
            .filter(|(_, model)| model.can_be_agent_model())
            .map(|(name, model)| (name.clone(), model.clone()))
            .collect();
    }
    if model_catalog.vision.is_empty() {
        model_catalog.vision = models
            .iter()
            .filter(|(_, model)| model.supports_image_input())
            .map(|(name, model)| (name.clone(), model.clone()))
            .collect();
    }
    ServerConfig {
        version,
        models,
        agent,
        model_catalog,
        tooling,
        chat_model_keys,
        main_agent,
        sandbox,
        max_global_sub_agents,
        cron_poll_interval_seconds,
        channels,
    }
}

pub fn load_server_config_file_and_upgrade(path: impl AsRef<Path>) -> Result<(ServerConfig, bool)> {
    let path = path.as_ref();
    let raw = fs::read_to_string(path)
        .with_context(|| format!("failed to read config file {}", path.display()))?;
    let value: Value = serde_json::from_str(&raw).context("failed to parse server config JSON")?;
    let version = match value.get("version") {
        Some(Value::String(version)) => version.clone(),
        _ => LEGACY_CONFIG_VERSION.to_string(),
    };
    let config = {
        let loaders: [&dyn ConfigLoader; 12] = [
            &v0_1::LegacyConfigLoader,
            &v0_2::VersionedConfigLoader,
            &v0_3::VersionedConfigLoader,
            &v0_4::LatestConfigLoader,
            &v0_5::LatestConfigLoader,
            &v0_6::LatestConfigLoader,
            &v0_7::LatestConfigLoader,
            &v0_8::LatestConfigLoader,
            &v0_9::LatestConfigLoader,
            &v0_10::LatestConfigLoader,
            &v0_11::LatestConfigLoader,
            &v0_12::LatestConfigLoader,
        ];
        let loader = loaders
            .into_iter()
            .find(|loader| loader.version() == version)
            .ok_or_else(|| anyhow!("unsupported config version '{}'", version))?;
        loader.load_and_upgrade(value)?
    };
    validate_server_config(&config)?;
    let upgraded = version != LATEST_CONFIG_VERSION;
    if upgraded {
        write_server_config_file(path, &config)?;
    }
    Ok((config, upgraded))
}

pub fn write_server_config_file(path: impl AsRef<Path>, config: &ServerConfig) -> Result<()> {
    #[derive(Serialize)]
    struct PersistedMainAgentConfig<'a> {
        model: &'a Option<String>,
        global_install_root: &'a str,
        language: &'a str,
        max_tool_roundtrips: usize,
        enable_context_compression: bool,
        context_compaction: &'a ContextCompactionConfig,
        idle_compaction: &'a IdleCompactionConfig,
        timeout_observation_compaction: &'a TimeoutObservationCompactionConfig,
        time_awareness: &'a TimeAwarenessConfig,
        memory_system: MemorySystem,
    }

    #[derive(Serialize)]
    struct PersistedModelConfig<'a> {
        #[serde(rename = "type")]
        model_type: ModelType,
        api_endpoint: &'a str,
        model: &'a str,
        #[serde(skip_serializing_if = "slice_is_empty")]
        capabilities: &'a [ModelCapability],
        #[serde(skip_serializing_if = "Option::is_none")]
        api_key: &'a Option<String>,
        api_key_env: &'a str,
        chat_completions_path: &'a str,
        #[serde(skip_serializing_if = "Option::is_none")]
        codex_home: &'a Option<String>,
        auth_credentials_store_mode: AuthCredentialsStoreMode,
        timeout_seconds: f64,
        context_window_tokens: usize,
        #[serde(skip_serializing_if = "Option::is_none")]
        cache_ttl: &'a Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        reasoning: &'a Option<ReasoningConfig>,
        #[serde(skip_serializing_if = "Map::is_empty")]
        headers: &'a Map<String, Value>,
        #[serde(skip_serializing_if = "str_is_empty")]
        description: &'a str,
        #[serde(skip_serializing_if = "is_true")]
        agent_model_enabled: bool,
        #[serde(skip_serializing_if = "Option::is_none")]
        native_web_search: &'a Option<NativeWebSearchConfig>,
        #[serde(skip_serializing_if = "Option::is_none")]
        supports_vision_input: Option<bool>,
        #[serde(skip_serializing_if = "Option::is_none")]
        image_tool_model: &'a Option<String>,
        #[serde(rename = "web_search", skip_serializing_if = "Option::is_none")]
        web_search_model: &'a Option<String>,
    }

    #[derive(Serialize)]
    struct PersistedAgentConfig<'a> {
        agent_frame: &'a AgentBackendConfig,
        zgent: &'a AgentBackendConfig,
    }

    #[derive(Serialize)]
    struct PersistedServerConfig<'a> {
        version: &'a str,
        models: BTreeMap<String, PersistedModelConfig<'a>>,
        agent: PersistedAgentConfig<'a>,
        #[serde(skip_serializing_if = "ToolingConfig::is_empty")]
        tooling: &'a ToolingConfig,
        main_agent: PersistedMainAgentConfig<'a>,
        sandbox: &'a SandboxConfig,
        max_global_sub_agents: usize,
        cron_poll_interval_seconds: u64,
        channels: &'a [ChannelConfig],
    }

    let persisted_models = config
        .models
        .iter()
        .map(|(name, model)| {
            (
                name.clone(),
                PersistedModelConfig {
                    model_type: model.model_type,
                    api_endpoint: &model.api_endpoint,
                    model: &model.model,
                    capabilities: &model.capabilities,
                    api_key: &model.api_key,
                    api_key_env: &model.api_key_env,
                    chat_completions_path: &model.chat_completions_path,
                    codex_home: &model.codex_home,
                    auth_credentials_store_mode: model.auth_credentials_store_mode,
                    timeout_seconds: model.timeout_seconds,
                    context_window_tokens: model.context_window_tokens,
                    cache_ttl: &model.cache_ttl,
                    reasoning: &model.reasoning,
                    headers: &model.headers,
                    description: &model.description,
                    agent_model_enabled: model.agent_model_enabled,
                    native_web_search: &model.native_web_search,
                    supports_vision_input: model
                        .supports_vision_input
                        .then_some(model.supports_vision_input),
                    image_tool_model: &model.image_tool_model,
                    web_search_model: &model.web_search_model,
                },
            )
        })
        .collect::<BTreeMap<_, _>>();

    let persisted = PersistedServerConfig {
        version: LATEST_CONFIG_VERSION,
        models: persisted_models,
        agent: PersistedAgentConfig {
            agent_frame: &config.agent.agent_frame,
            zgent: &config.agent.zgent,
        },
        tooling: &config.tooling,
        main_agent: PersistedMainAgentConfig {
            model: &config.main_agent.model,
            global_install_root: &config.main_agent.global_install_root,
            language: &config.main_agent.language,
            max_tool_roundtrips: config.main_agent.max_tool_roundtrips,
            enable_context_compression: config.main_agent.enable_context_compression,
            context_compaction: &config.main_agent.context_compaction,
            idle_compaction: &config.main_agent.idle_compaction,
            timeout_observation_compaction: &config.main_agent.timeout_observation_compaction,
            time_awareness: &config.main_agent.time_awareness,
            memory_system: config.main_agent.memory_system,
        },
        sandbox: &config.sandbox,
        max_global_sub_agents: config.max_global_sub_agents,
        cron_poll_interval_seconds: config.cron_poll_interval_seconds,
        channels: &config.channels,
    };
    let raw =
        serde_json::to_string_pretty(&persisted).context("failed to serialize server config")?;
    fs::write(path.as_ref(), raw)
        .with_context(|| format!("failed to write config file {}", path.as_ref().display()))
}

fn validate_server_config(config: &ServerConfig) -> Result<()> {
    if config.channels.is_empty() {
        return Err(anyhow!("server config must include at least one channel"));
    }
    if config.models.is_empty() {
        return Err(anyhow!("server config must include at least one model"));
    }
    if config.max_global_sub_agents == 0 {
        return Err(anyhow!("max_global_sub_agents must be at least 1"));
    }
    if config.cron_poll_interval_seconds == 0 {
        return Err(anyhow!("cron_poll_interval_seconds must be at least 1"));
    }
    if config.sandbox.mode == SandboxMode::Bubblewrap {
        if config.sandbox.bubblewrap_binary.trim().is_empty() {
            return Err(anyhow!("sandbox.bubblewrap_binary must not be empty"));
        }
    }
    if config.main_agent.idle_compaction.enabled
        && config.main_agent.idle_compaction.poll_interval_seconds == 0
    {
        return Err(anyhow!(
            "main_agent.idle_compaction.poll_interval_seconds must be at least 1"
        ));
    }
    if config
        .main_agent
        .timeout_seconds
        .is_some_and(|value| value < 0.0)
    {
        return Err(anyhow!(
            "main_agent.timeout_seconds must be greater than or equal to 0"
        ));
    }
    for (model_name, model) in &config.models {
        if model.api_endpoint.trim().is_empty() || model.model.trim().is_empty() {
            return Err(anyhow!(
                "model '{}' must include api_endpoint and model",
                model_name
            ));
        }
        if model.model_type == ModelType::CodexSubscription && model.codex_home.is_none() {
            return Err(anyhow!(
                "model '{}' uses codex-subscription and must include codex_home",
                model_name
            ));
        }
        if let Some(image_tool_model) = &model.image_tool_model
            && image_tool_model != "self"
            && !config.models.contains_key(image_tool_model)
        {
            return Err(anyhow!(
                "model '{}' references unknown image_tool_model '{}'",
                model_name,
                image_tool_model
            ));
        }
        if let Some(web_search_model) = &model.web_search_model
            && !config.models.contains_key(web_search_model)
            && !config
                .model_catalog
                .web_search
                .contains_key(web_search_model)
        {
            return Err(anyhow!(
                "model '{}' references unknown web_search_model '{}'",
                model_name,
                web_search_model
            ));
        }
    }
    for backend in [AgentBackendKind::AgentFrame, AgentBackendKind::Zgent] {
        let available_models = config.agent.available_models(backend);
        if backend == AgentBackendKind::Zgent
            && !available_models.is_empty()
            && !zgent_runtime_available()
        {
            return Err(anyhow!(
                "agent.zgent.available_models is configured but the local ./zgent runtime directory is unavailable"
            ));
        }
        for model_key in available_models {
            let Some(model) = config.models.get(model_key) else {
                return Err(anyhow!(
                    "agent.{}.available_models references unknown model alias '{}'",
                    agent_backend_field_name(backend),
                    model_key
                ));
            };
            if !model.can_be_agent_model() {
                return Err(anyhow!(
                    "agent.{}.available_models references model '{}' which is not an enabled agent chat model",
                    agent_backend_field_name(backend),
                    model_key
                ));
            }
            if backend == AgentBackendKind::Zgent
                && model.chat_completions_path != default_chat_completions_path()
            {
                return Err(anyhow!(
                    "agent.zgent.available_models references model '{}' but chat_completions_path must be '{}'",
                    model_key,
                    default_chat_completions_path()
                ));
            }
        }
    }
    validate_tooling_target(
        config,
        "tooling.web_search",
        config.tooling.web_search.as_ref(),
        ModelCapability::WebSearch,
    )?;
    validate_tooling_target(
        config,
        "tooling.image",
        config.tooling.image.as_ref(),
        ModelCapability::ImageIn,
    )?;
    validate_tooling_target(
        config,
        "tooling.image_gen",
        config.tooling.image_gen.as_ref(),
        ModelCapability::ImageOut,
    )?;
    validate_tooling_target(
        config,
        "tooling.pdf",
        config.tooling.pdf.as_ref(),
        ModelCapability::Pdf,
    )?;
    validate_tooling_target(
        config,
        "tooling.audio_input",
        config.tooling.audio_input.as_ref(),
        ModelCapability::AudioIn,
    )?;
    Ok(())
}

fn agent_backend_field_name(backend: AgentBackendKind) -> &'static str {
    match backend {
        AgentBackendKind::AgentFrame => "agent_frame",
        AgentBackendKind::Zgent => "zgent",
    }
}

fn validate_tooling_target(
    config: &ServerConfig,
    field_name: &str,
    target: Option<&ToolingTarget>,
    required_capability: ModelCapability,
) -> Result<()> {
    let Some(target) = target else {
        return Ok(());
    };
    let Some(model) = config.models.get(&target.alias) else {
        return Err(anyhow!(
            "{} references unknown model alias '{}'",
            field_name,
            target.alias
        ));
    };
    let supports_required_capability = match required_capability {
        ModelCapability::ImageIn => model.supports_image_input(),
        capability => model.has_capability(capability),
    };
    if !supports_required_capability {
        return Err(anyhow!(
            "{} references model '{}' which does not declare capability '{}'",
            field_name,
            target.alias,
            serde_json::to_string(&required_capability)
                .unwrap_or_else(|_| "\"unknown\"".to_string())
                .trim_matches('"')
        ));
    }
    Ok(())
}

fn normalize_model_capabilities(
    model: &mut ModelConfig,
    model_name: &str,
    chat_model_keys: &[String],
    vision_catalog: &BTreeMap<String, ModelConfig>,
    chat_catalog: &BTreeMap<String, ModelConfig>,
) {
    if model.capabilities.contains(&ModelCapability::ImageIn) {
        model.supports_vision_input = true;
    }
    if model.supports_vision_input && !model.capabilities.contains(&ModelCapability::ImageIn) {
        model.capabilities.push(ModelCapability::ImageIn);
    }
    if model.agent_model_enabled
        && (chat_model_keys.iter().any(|value| value == model_name)
            || chat_catalog.contains_key(model_name))
        && !model.capabilities.contains(&ModelCapability::Chat)
    {
        model.capabilities.push(ModelCapability::Chat);
    }
    if vision_catalog.contains_key(model_name)
        && !model.capabilities.contains(&ModelCapability::ImageIn)
    {
        model.capabilities.push(ModelCapability::ImageIn);
        model.supports_vision_input = true;
    }
    if model
        .native_web_search
        .as_ref()
        .is_some_and(|cfg| cfg.enabled)
        && !model.capabilities.contains(&ModelCapability::WebSearch)
    {
        model.capabilities.push(ModelCapability::WebSearch);
    }
    model.capabilities.sort();
    model.capabilities.dedup();
}

fn is_true(value: &bool) -> bool {
    *value
}

pub fn resolve_model_api_keys(config: &ServerConfig) -> Vec<ResolvedModelApiKey> {
    let mut resolved = config
        .models
        .iter()
        .map(|(model_name, model)| {
            let inline_key = model
                .api_key
                .as_ref()
                .map(|value| value.trim())
                .filter(|value| !value.is_empty())
                .map(ToOwned::to_owned);
            if let Some(api_key) = inline_key {
                return ResolvedModelApiKey {
                    model_name: model_name.clone(),
                    source: "config.api_key".to_string(),
                    api_key: Some(api_key),
                };
            }

            let env_name = model.api_key_env.trim();
            let env_key = if env_name.is_empty() {
                None
            } else {
                std::env::var(env_name)
                    .ok()
                    .map(|value| value.trim().to_string())
                    .filter(|value| !value.is_empty())
            };

            ResolvedModelApiKey {
                model_name: model_name.clone(),
                source: if env_name.is_empty() {
                    "missing".to_string()
                } else {
                    format!("env:{env_name}")
                },
                api_key: env_key,
            }
        })
        .collect::<Vec<_>>();

    resolved.extend(
        config
            .model_catalog
            .web_search
            .iter()
            .map(|(name, search)| {
                let inline_key = search
                    .api_key
                    .as_ref()
                    .map(|value| value.trim())
                    .filter(|value| !value.is_empty())
                    .map(ToOwned::to_owned);
                if let Some(api_key) = inline_key {
                    return ResolvedModelApiKey {
                        model_name: format!("web_search:{name}"),
                        source: "config.api_key".to_string(),
                        api_key: Some(api_key),
                    };
                }

                let env_name = search.api_key_env.trim();
                let env_key = if env_name.is_empty() {
                    None
                } else {
                    std::env::var(env_name)
                        .ok()
                        .map(|value| value.trim().to_string())
                        .filter(|value| !value.is_empty())
                };

                ResolvedModelApiKey {
                    model_name: format!("web_search:{name}"),
                    source: if env_name.is_empty() {
                        "missing".to_string()
                    } else {
                        format!("env:{env_name}")
                    },
                    api_key: env_key,
                }
            }),
    );

    resolved
}

#[cfg(test)]
mod tests {
    use super::{
        ChannelConfig, LATEST_CONFIG_VERSION, MainAgentConfig, ModelType,
        default_dingtalk_commands, default_telegram_commands, load_server_config_file,
        load_server_config_file_and_upgrade, resolve_model_api_keys,
    };
    use crate::backend::AgentBackendKind;
    use crate::zgent::zgent_runtime_available;
    use std::fs;
    use tempfile::TempDir;

    #[test]
    fn telegram_commands_default_to_builtin_list() {
        let temp_dir = TempDir::new().unwrap();
        let config_path = temp_dir.path().join("config.json");
        fs::write(
            &config_path,
            r#"
            {
              "models": {
                "main": {
                  "api_endpoint": "https://example.com/v1",
                  "model": "demo-model",
                  "description": "demo"
                }
              },
              "main_agent": {
                "model": "main"
              },
              "channels": [
                {
                  "kind": "telegram",
                  "id": "telegram-main",
                  "bot_token": "token"
                }
              ]
            }
            "#,
        )
        .unwrap();

        let config = load_server_config_file(&config_path).unwrap();
        match &config.channels[0] {
            ChannelConfig::Telegram(telegram) => {
                assert_eq!(telegram.commands, default_telegram_commands());
            }
            _ => panic!("expected telegram channel"),
        }
    }

    #[test]
    fn dingtalk_channel_defaults_to_env_based_credentials() {
        let temp_dir = TempDir::new().unwrap();
        let config_path = temp_dir.path().join("config.json");
        fs::write(
            &config_path,
            r#"
            {
              "models": {
                "main": {
                  "api_endpoint": "https://example.com/v1",
                  "model": "demo-model",
                  "description": "demo"
                }
              },
              "main_agent": {
                "model": "main"
              },
              "channels": [
                {
                  "kind": "dingtalk",
                  "id": "dingtalk-main"
                }
              ]
            }
            "#,
        )
        .unwrap();

        let config = load_server_config_file(&config_path).unwrap();
        match &config.channels[0] {
            ChannelConfig::Dingtalk(dingtalk) => {
                assert_eq!(dingtalk.client_id_env, "DINGTALK_CLIENT_ID");
                assert_eq!(dingtalk.client_secret_env, "DINGTALK_CLIENT_SECRET");
                assert_eq!(dingtalk.api_base_url, "https://api.dingtalk.com");
            }
            _ => panic!("expected dingtalk channel"),
        }
    }

    #[test]
    fn dingtalk_commands_default_to_builtin_list() {
        assert_eq!(default_dingtalk_commands(), super::default_bot_commands());
    }

    #[test]
    fn idle_context_compaction_requires_positive_poll_interval_when_enabled() {
        let temp_dir = TempDir::new().unwrap();
        let config_path = temp_dir.path().join("config.json");
        fs::write(
            &config_path,
            r#"
            {
              "models": {
                "main": {
                  "api_endpoint": "https://example.com/v1",
                  "model": "demo-model",
                  "description": "demo",
                  "cache_ttl": "5m"
                }
              },
              "main_agent": {
                "model": "main",
                "enable_idle_context_compaction": true,
                "idle_context_compaction_poll_interval_seconds": 0
              },
              "channels": [
                {
                  "kind": "command_line",
                  "id": "local-cli"
                }
              ]
            }
            "#,
        )
        .unwrap();

        let error = load_server_config_file(&config_path).unwrap_err();
        assert!(
            error
                .to_string()
                .contains("idle_compaction.poll_interval_seconds")
        );
    }

    #[test]
    fn main_agent_timeout_allows_zero_to_disable_external_timeout() {
        let temp_dir = TempDir::new().unwrap();
        let config_path = temp_dir.path().join("config.json");
        fs::write(
            &config_path,
            r#"
            {
              "models": {
                "main": {
                  "api_endpoint": "https://example.com/v1",
                  "model": "demo-model",
                  "description": "demo"
                }
              },
              "main_agent": {
                "model": "main",
                "timeout_seconds": 0
              },
              "channels": [
                {
                  "kind": "command_line",
                  "id": "local-cli"
                }
              ]
            }
            "#,
        )
        .unwrap();

        let config = load_server_config_file(&config_path).unwrap();
        assert_eq!(config.main_agent.timeout_seconds, Some(0.0));
    }

    #[test]
    fn main_agent_timeout_rejects_negative_values() {
        let temp_dir = TempDir::new().unwrap();
        let config_path = temp_dir.path().join("config.json");
        fs::write(
            &config_path,
            r#"
            {
              "models": {
                "main": {
                  "api_endpoint": "https://example.com/v1",
                  "model": "demo-model",
                  "description": "demo"
                }
              },
              "main_agent": {
                "model": "main",
                "timeout_seconds": -1
              },
              "channels": [
                {
                  "kind": "command_line",
                  "id": "local-cli"
                }
              ]
            }
            "#,
        )
        .unwrap();

        let error = load_server_config_file(&config_path).unwrap_err();
        assert!(error.to_string().contains("main_agent.timeout_seconds"));
    }

    #[test]
    fn image_tool_model_must_reference_existing_model_or_self() {
        let temp_dir = TempDir::new().unwrap();
        let config_path = temp_dir.path().join("config.json");
        fs::write(
            &config_path,
            r#"
            {
              "models": {
                "main": {
                  "api_endpoint": "https://example.com/v1",
                  "model": "demo-model",
                  "description": "demo",
                  "image_tool_model": "vision"
                }
              },
              "main_agent": {
                "model": "main"
              },
              "channels": [
                {
                  "kind": "command_line",
                  "id": "local-cli"
                }
              ]
            }
            "#,
        )
        .unwrap();

        let error = load_server_config_file(&config_path).unwrap_err();
        assert!(error.to_string().contains("unknown image_tool_model"));
    }

    #[test]
    fn model_backend_defaults_to_agent_frame() {
        let temp_dir = TempDir::new().unwrap();
        let config_path = temp_dir.path().join("config.json");
        fs::write(
            &config_path,
            r#"
            {
              "models": {
                "main": {
                  "api_endpoint": "https://example.com/v1",
                  "model": "demo-model",
                  "description": "demo"
                }
              },
              "main_agent": {
                "model": "main"
              },
              "channels": [
                {
                  "kind": "command_line",
                  "id": "local-cli"
                }
              ]
            }
            "#,
        )
        .unwrap();

        let config = load_server_config_file(&config_path).unwrap();
        assert_eq!(config.models["main"].backend, AgentBackendKind::AgentFrame);
    }

    #[test]
    fn zgent_backend_rejects_custom_chat_completions_path() {
        let temp_dir = TempDir::new().unwrap();
        let config_path = temp_dir.path().join("config.json");
        fs::write(
            &config_path,
            r#"
            {
              "models": {
                "main": {
                  "api_endpoint": "https://example.com/v1",
                  "model": "demo-model",
                  "backend": "zgent",
                  "chat_completions_path": "/custom/chat",
                  "description": "demo"
                }
              },
              "main_agent": {
                "model": "main"
              },
              "channels": [
                {
                  "kind": "command_line",
                  "id": "local-cli"
                }
              ]
            }
            "#,
        )
        .unwrap();

        let error = load_server_config_file(&config_path).unwrap_err();
        if zgent_runtime_available() {
            assert!(error.to_string().contains("chat_completions_path"));
        } else {
            assert!(
                error
                    .to_string()
                    .contains("local ./zgent runtime directory is unavailable")
            );
        }
    }

    #[test]
    fn resolve_model_api_keys_prefers_inline_and_falls_back_to_env() {
        let temp_dir = TempDir::new().unwrap();
        let config_path = temp_dir.path().join("config.json");
        fs::write(
            &config_path,
            r#"
            {
              "models": {
                "inline_model": {
                  "api_endpoint": "https://example.com/v1",
                  "model": "demo-inline",
                  "description": "demo",
                  "api_key": "inline-secret"
                },
                "env_model": {
                  "api_endpoint": "https://example.com/v1",
                  "model": "demo-env",
                  "description": "demo",
                  "api_key_env": "AGENT_HOST_TEST_API_KEY"
                },
                "missing_model": {
                  "api_endpoint": "https://example.com/v1",
                  "model": "demo-missing",
                  "description": "demo",
                  "api_key_env": "AGENT_HOST_TEST_MISSING"
                }
              },
              "main_agent": {
                "model": "inline_model"
              },
              "channels": [
                {
                  "kind": "command_line",
                  "id": "local-cli"
                }
              ]
            }
            "#,
        )
        .unwrap();

        // Safe in unit test; restored below.
        unsafe {
            std::env::set_var("AGENT_HOST_TEST_API_KEY", "env-secret");
            std::env::remove_var("AGENT_HOST_TEST_MISSING");
        }

        let config = load_server_config_file(&config_path).unwrap();
        let resolved = resolve_model_api_keys(&config);

        assert_eq!(resolved[0].model_name, "env_model");
        assert_eq!(resolved[0].source, "env:AGENT_HOST_TEST_API_KEY");
        assert_eq!(resolved[0].api_key.as_deref(), Some("env-secret"));

        assert_eq!(resolved[1].model_name, "inline_model");
        assert_eq!(resolved[1].source, "config.api_key");
        assert_eq!(resolved[1].api_key.as_deref(), Some("inline-secret"));

        assert_eq!(resolved[2].model_name, "missing_model");
        assert_eq!(resolved[2].source, "env:AGENT_HOST_TEST_MISSING");
        assert_eq!(resolved[2].api_key, None);

        unsafe {
            std::env::remove_var("AGENT_HOST_TEST_API_KEY");
            std::env::remove_var("AGENT_HOST_TEST_MISSING");
        }
    }

    #[test]
    fn legacy_config_without_version_is_upgraded_to_latest() {
        let temp_dir = TempDir::new().unwrap();
        let config_path = temp_dir.path().join("config.json");
        fs::write(
            &config_path,
            r#"
            {
              "models": {
                "main": {
                  "api_endpoint": "https://openrouter.ai/api/v1",
                  "model": "anthropic/claude-sonnet-4.6",
                  "description": "demo"
                }
              },
              "main_agent": {
                "model": "main"
              },
              "channels": [
                {
                  "kind": "command_line",
                  "id": "local-cli"
                }
              ]
            }
            "#,
        )
        .unwrap();

        let config = load_server_config_file(&config_path).unwrap();
        assert_eq!(config.version, LATEST_CONFIG_VERSION);
        assert_eq!(config.chat_model_keys, vec!["main".to_string()]);
        assert_eq!(config.models["main"].model_type, ModelType::Openrouter);
    }

    #[test]
    fn versioned_model_catalog_loads_and_flattens_by_kind() {
        let temp_dir = TempDir::new().unwrap();
        let config_path = temp_dir.path().join("config.json");
        fs::write(
            &config_path,
            r#"
            {
              "version": "0.2",
              "models": {
                "chat": {
                  "main": {
                    "type": "openrouter",
                    "model": "anthropic/claude-sonnet-4.6",
                    "description": "chat model"
                  }
                },
                "vision": {
                  "vision-model": {
                    "type": "openrouter",
                    "model": "openai/gpt-4.1-mini",
                    "supports_vision_input": true,
                    "description": "vision model"
                  }
                },
                "web_search": {
                  "search-model": {
                    "type": "openrouter-resp",
                    "model": "openai/gpt-4.1-mini",
                    "description": "web search model"
                  }
                }
              },
              "main_agent": {
                "model": "main"
              },
              "channels": [
                {
                  "kind": "command_line",
                  "id": "local-cli"
                }
              ]
            }
            "#,
        )
        .unwrap();

        let config = load_server_config_file(&config_path).unwrap();
        assert_eq!(config.version, LATEST_CONFIG_VERSION);
        assert_eq!(config.chat_model_keys, vec!["main".to_string()]);
        assert_eq!(config.models["main"].model_type, ModelType::Openrouter);
        assert_eq!(
            config.models["vision-model"].model_type,
            ModelType::Openrouter
        );
        assert!(!config.models.contains_key("search-model"));
        assert_eq!(
            config.model_catalog.web_search["search-model"].model,
            "openai/gpt-4.1-mini"
        );
        assert_eq!(
            config.model_catalog.web_search["search-model"].chat_completions_path,
            "/responses"
        );
    }

    #[test]
    fn openrouter_resp_defaults_to_responses_path() {
        let temp_dir = TempDir::new().unwrap();
        let config_path = temp_dir.path().join("config.json");
        fs::write(
            &config_path,
            r#"
            {
              "version": "0.2",
              "models": {
                "chat": {
                  "main": {
                    "type": "openrouter-resp",
                    "model": "openai/gpt-4.1-mini",
                    "description": "demo"
                  }
                },
                "vision": {},
                "web_search": {}
              },
              "main_agent": {
                "model": "main"
              },
              "channels": [
                {
                  "kind": "command_line",
                  "id": "local-cli"
                }
              ]
            }
            "#,
        )
        .unwrap();

        let config = load_server_config_file(&config_path).unwrap();
        assert_eq!(config.models["main"].chat_completions_path, "/responses");
    }

    #[test]
    fn codex_subscription_requires_codex_home_in_versioned_config() {
        let temp_dir = TempDir::new().unwrap();
        let config_path = temp_dir.path().join("config.json");
        fs::write(
            &config_path,
            r#"
            {
              "version": "0.2",
              "models": {
                "chat": {
                  "main": {
                    "type": "codex-subscription",
                    "model": "gpt-5",
                    "description": "demo"
                  }
                },
                "vision": {},
                "web_search": {}
              },
              "main_agent": {
                "model": "main"
              },
              "channels": [
                {
                  "kind": "command_line",
                  "id": "local-cli"
                }
              ]
            }
            "#,
        )
        .unwrap();

        let error = load_server_config_file(&config_path).unwrap_err();
        assert!(error.to_string().contains("must include codex_home"));
    }

    #[test]
    fn legacy_external_web_search_is_promoted_into_shared_web_search_catalog() {
        let temp_dir = TempDir::new().unwrap();
        let config_path = temp_dir.path().join("config.json");
        fs::write(
            &config_path,
            r#"
            {
              "models": {
                "main": {
                  "api_endpoint": "https://openrouter.ai/api/v1",
                  "model": "anthropic/claude-sonnet-4.6",
                  "description": "demo",
                  "external_web_search": {
                    "base_url": "https://openrouter.ai/api/v1",
                    "model": "perplexity/sonar-pro",
                    "api_key_env": "OPENROUTER_API_KEY",
                    "chat_completions_path": "/chat/completions",
                    "timeout_seconds": 60
                  }
                }
              },
              "main_agent": {
                "model": "main"
              },
              "channels": [
                {
                  "kind": "command_line",
                  "id": "local-cli"
                }
              ]
            }
            "#,
        )
        .unwrap();

        let (config, upgraded) = load_server_config_file_and_upgrade(&config_path).unwrap();
        assert!(upgraded);
        assert_eq!(
            config.models["main"].web_search_model.as_deref(),
            Some("main_web_search")
        );
        assert!(
            config
                .model_catalog
                .web_search
                .contains_key("main_web_search")
        );

        let written = fs::read_to_string(&config_path).unwrap();
        let written_json: serde_json::Value = serde_json::from_str(&written).unwrap();
        assert!(written.contains(&format!("\"version\": \"{LATEST_CONFIG_VERSION}\"")));
        assert!(written.contains("\"web_search\": \"main_web_search\""));
        assert!(written.contains("\"models\": {"));
        assert!(!written.contains("\"tooling\": {"));
        assert!(!written.contains("\"enabled_tools\""));
        assert!(written.contains("\"context_compaction\": {"));
        assert!(
            written_json["main_agent"]
                .as_object()
                .unwrap()
                .get("timeout_seconds")
                .is_none()
        );
    }

    #[test]
    fn latest_config_loads_chat_web_search_alias() {
        let temp_dir = TempDir::new().unwrap();
        let config_path = temp_dir.path().join("config.json");
        fs::write(
            &config_path,
            r#"
            {
              "version": "0.3",
              "models": {
                "chat": {
                  "main": {
                    "type": "openrouter",
                    "model": "anthropic/claude-sonnet-4.6",
                    "description": "demo",
                    "web_search": "default_search"
                  }
                },
                "vision": {},
                "web_search": {
                  "default_search": {
                    "base_url": "https://openrouter.ai/api/v1",
                    "model": "perplexity/sonar-pro",
                    "api_key_env": "OPENROUTER_API_KEY",
                    "chat_completions_path": "/chat/completions",
                    "timeout_seconds": 60,
                    "headers": {}
                  }
                }
              },
              "main_agent": {
                "model": "main"
              },
              "channels": [
                {
                  "kind": "command_line",
                  "id": "local-cli"
                }
              ]
            }
            "#,
        )
        .unwrap();

        let config = load_server_config_file(&config_path).unwrap();
        assert_eq!(
            config.models["main"].web_search_model.as_deref(),
            Some("default_search")
        );
        assert!(config.models["main"].external_web_search.is_some());
        assert!(
            config
                .model_catalog
                .web_search
                .contains_key("default_search")
        );
    }

    #[test]
    fn latest_config_supports_alias_keyed_models_and_tooling_targets() {
        let temp_dir = TempDir::new().unwrap();
        let config_path = temp_dir.path().join("config.json");
        fs::write(
            &config_path,
            r#"
            {
              "version": "0.9",
              "models": {
                "gpt54": {
                  "type": "codex-subscription",
                  "api_endpoint": "https://chatgpt.com/backend-api/codex",
                  "model": "gpt-5.4",
                  "codex_home": "~/.codex",
                  "description": "demo",
                  "capabilities": ["chat", "web_search", "image_in"]
                },
                "sonar": {
                  "type": "openrouter",
                  "api_endpoint": "https://openrouter.ai/api/v1",
                  "model": "perplexity/sonar-pro",
                  "description": "search",
                  "capabilities": ["chat", "web_search"],
                  "agent_model_enabled": false
                }
              },
              "agent": {
                "agent_frame": {
                  "available_models": ["gpt54"]
                },
                "zgent": {
                  "available_models": []
                }
              },
              "tooling": {
                "web_search": "sonar:self",
                "image": "gpt54:self"
              },
              "main_agent": {
                "model": "gpt54"
              },
              "channels": [
                {
                  "kind": "command_line",
                  "id": "local-cli"
                }
              ]
            }
            "#,
        )
        .unwrap();

        let config = load_server_config_file(&config_path).unwrap();
        assert_eq!(config.chat_model_keys, vec!["gpt54".to_string()]);
        assert_eq!(
            config.agent.agent_frame.available_models,
            vec!["gpt54".to_string()]
        );
        assert!(config.models["gpt54"].has_capability(super::ModelCapability::WebSearch));
        assert!(config.models["gpt54"].supports_image_input());
        assert!(!config.models["sonar"].agent_model_enabled);
        assert_eq!(
            config
                .tooling
                .web_search
                .as_ref()
                .map(|value| value.as_config_string()),
            Some("sonar:self".to_string())
        );
        assert_eq!(
            config
                .tooling
                .image
                .as_ref()
                .map(|value| value.as_config_string()),
            Some("gpt54:self".to_string())
        );
    }

    #[test]
    fn tooling_targets_require_models_with_matching_capabilities() {
        let temp_dir = TempDir::new().unwrap();
        let config_path = temp_dir.path().join("config.json");
        fs::write(
            &config_path,
            r#"
            {
              "version": "0.8",
              "models": {
                "main": {
                  "type": "openrouter",
                  "api_endpoint": "https://example.com/v1",
                  "model": "demo-model",
                  "description": "demo",
                  "capabilities": ["chat"]
                },
                "helper": {
                  "type": "openrouter",
                  "api_endpoint": "https://example.com/v1",
                  "model": "helper-model",
                  "description": "helper"
                }
              },
              "tooling": {
                "web_search": "helper"
              },
              "main_agent": {
                "model": "main"
              },
              "channels": [
                {
                  "kind": "command_line",
                  "id": "local-cli"
                }
              ]
            }
            "#,
        )
        .unwrap();

        let error = load_server_config_file(&config_path).unwrap_err();
        assert!(
            error
                .to_string()
                .contains("tooling.web_search references model 'helper' which does not declare capability 'web_search'")
        );
    }

    #[test]
    fn image_generation_tooling_targets_require_image_out_capability() {
        let temp_dir = TempDir::new().unwrap();
        let config_path = temp_dir.path().join("config.json");
        fs::write(
            &config_path,
            r#"
            {
              "version": "0.11",
              "models": {
                "main": {
                  "type": "openrouter",
                  "api_endpoint": "https://example.com/v1",
                  "model": "demo-main",
                  "description": "demo",
                  "capabilities": ["chat"]
                },
                "helper": {
                  "type": "openrouter-resp",
                  "api_endpoint": "https://example.com/v1",
                  "model": "demo-helper",
                  "description": "helper",
                  "capabilities": ["chat", "image_in"]
                }
              },
              "tooling": {
                "image_gen": "helper"
              },
              "main_agent": {
                "model": "main"
              },
              "channels": [
                {
                  "kind": "command_line",
                  "id": "local-cli"
                }
              ]
            }
            "#,
        )
        .unwrap();

        let error = load_server_config_file(&config_path).unwrap_err();
        assert!(
            error
                .to_string()
                .contains("tooling.image_gen references model 'helper' which does not declare capability 'image_out'")
        );
    }

    #[test]
    fn v0_11_configs_upgrade_with_time_awareness_defaults() {
        let temp_dir = TempDir::new().unwrap();
        let config_path = temp_dir.path().join("config.json");
        fs::write(
            &config_path,
            r#"
            {
              "version": "0.11",
              "models": {
                "main": {
                  "type": "openrouter",
                  "api_endpoint": "https://example.com/v1",
                  "model": "demo-main",
                  "description": "demo",
                  "capabilities": ["chat"]
                }
              },
              "main_agent": {
                "model": "main"
              },
              "channels": [
                {
                  "kind": "command_line",
                  "id": "local-cli"
                }
              ]
            }
            "#,
        )
        .unwrap();

        let (config, upgraded) = load_server_config_file_and_upgrade(&config_path).unwrap();
        assert!(upgraded);
        assert_eq!(config.version, LATEST_CONFIG_VERSION);
        assert!(
            !config
                .main_agent
                .time_awareness
                .emit_system_date_on_user_message
        );
        assert!(config.main_agent.time_awareness.emit_idle_time_gap_hint);

        let written = fs::read_to_string(&config_path).unwrap();
        assert!(written.contains("\"version\": \"0.12\""));
        assert!(written.contains("\"time_awareness\""));
    }

    #[test]
    fn disabled_agent_models_do_not_block_config_loading() {
        let temp_dir = TempDir::new().unwrap();
        let config_path = temp_dir.path().join("config.json");
        fs::write(
            &config_path,
            r#"
            {
              "version": "0.8",
              "models": {
                "main": {
                  "type": "openrouter",
                  "api_endpoint": "https://example.com/v1",
                  "model": "demo-main",
                  "description": "demo",
                  "capabilities": ["chat"]
                },
                "helper": {
                  "type": "openrouter-resp",
                  "api_endpoint": "https://example.com/v1",
                  "model": "demo-helper",
                  "description": "helper",
                  "capabilities": ["chat", "image_out"],
                  "agent_model_enabled": false
                }
              },
              "main_agent": {
                "model": "helper"
              },
              "channels": [
                {
                  "kind": "command_line",
                  "id": "local-cli"
                }
              ]
            }
            "#,
        )
        .unwrap();

        let config = load_server_config_file(&config_path).unwrap();
        assert_eq!(config.main_agent.model.as_deref(), Some("helper"));
        assert!(!config.models["helper"].agent_model_enabled);
    }

    #[test]
    fn main_agent_config_normalizes_legacy_file_tool_names() {
        let config: MainAgentConfig = serde_json::from_value(serde_json::json!({
            "enabled_tools": ["read_file", "write_file", "file_read"]
        }))
        .unwrap();

        assert_eq!(config.enabled_tools, vec!["file_read", "file_write"]);
    }
}
