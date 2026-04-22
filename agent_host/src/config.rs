use crate::backend::AgentBackendKind;
use agent_frame::config::{
    AuthCredentialsStoreMode, ExternalWebSearchConfig, MemorySystem, NativeWebSearchConfig,
    ReasoningConfig, RetryModeConfig, TokenEstimationConfig, UpstreamApiKind, UpstreamAuthKind,
    expand_home_path,
};
use anyhow::{Context, Result, anyhow};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

mod v0_1;
mod v0_10;
mod v0_11;
mod v0_12;
mod v0_13;
mod v0_14;
mod v0_15;
mod v0_16;
mod v0_17;
mod v0_18;
mod v0_19;
mod v0_2;
mod v0_20;
mod v0_21;
mod v0_22;
mod v0_23;
mod v0_24;
mod v0_25;
mod v0_26;
mod v0_27;
mod v0_3;
mod v0_4;
mod v0_5;
mod v0_6;
mod v0_7;
mod v0_8;
mod v0_9;

pub const LEGACY_CONFIG_VERSION: &str = "0.1";
pub const LATEST_CONFIG_VERSION: &str = "0.27";
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
pub const VERSION_0_12: &str = "0.12";
pub const VERSION_0_13: &str = "0.13";
pub const VERSION_0_14: &str = "0.14";
pub const VERSION_0_15: &str = "0.15";
pub const VERSION_0_16: &str = "0.16";
pub const VERSION_0_17: &str = "0.17";
pub const VERSION_0_18: &str = "0.18";
pub const VERSION_0_19: &str = "0.19";
pub const VERSION_0_20: &str = "0.20";
pub const VERSION_0_21: &str = "0.21";
pub const VERSION_0_22: &str = "0.22";
pub const VERSION_0_23: &str = "0.23";
pub const VERSION_0_24: &str = "0.24";
pub const VERSION_0_25: &str = "0.25";
pub const VERSION_0_26: &str = "0.26";
pub const VERSION_0_27: &str = "0.27";

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
pub struct DingtalkRobotChannelConfig {
    pub id: String,
    #[serde(default)]
    pub webhook_url: Option<String>,
    #[serde(default = "default_dingtalk_robot_webhook_url_env")]
    pub webhook_url_env: String,
    #[serde(default)]
    pub app_key: Option<String>,
    #[serde(default = "default_dingtalk_robot_app_key_env")]
    pub app_key_env: String,
    #[serde(default)]
    pub app_secret: Option<String>,
    #[serde(default = "default_dingtalk_robot_app_secret_env")]
    pub app_secret_env: String,
    #[serde(default = "default_dingtalk_robot_http_listen_addr")]
    pub http_listen_addr: String,
    #[serde(default = "default_dingtalk_robot_http_callback_path")]
    pub http_callback_path: String,
    #[serde(default = "default_dingtalk_api_base_url")]
    pub api_base_url: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct WebChannelConfig {
    pub id: String,
    #[serde(default = "default_web_listen_addr")]
    pub listen_addr: String,
    #[serde(default)]
    pub auth_token: Option<String>,
    #[serde(default = "default_web_auth_token_env")]
    pub auth_token_env: String,
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
    #[serde(default)]
    pub retry_mode: RetryModeConfig,
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
    pub token_estimation: Option<TokenEstimationConfig>,
}

impl ModelConfig {
    pub fn upstream_api_kind(&self) -> UpstreamApiKind {
        match self.model_type {
            ModelType::Openrouter => UpstreamApiKind::ChatCompletions,
            ModelType::OpenrouterResp | ModelType::CodexSubscription => UpstreamApiKind::Responses,
            ModelType::ClaudeCode => UpstreamApiKind::ClaudeMessages,
            ModelType::BraveSearch => UpstreamApiKind::ChatCompletions,
        }
    }

    pub fn upstream_auth_kind(&self) -> UpstreamAuthKind {
        match self.model_type {
            ModelType::CodexSubscription => UpstreamAuthKind::CodexSubscription,
            ModelType::Openrouter
            | ModelType::OpenrouterResp
            | ModelType::ClaudeCode
            | ModelType::BraveSearch => UpstreamAuthKind::ApiKey,
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

    pub fn resolved_codex_home(&self) -> Option<PathBuf> {
        self.codex_home.as_deref().map(expand_home_path)
    }
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum ModelType {
    Openrouter,
    OpenrouterResp,
    CodexSubscription,
    ClaudeCode,
    BraveSearch,
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
}

impl AgentConfig {
    pub fn backend_config(&self, backend: AgentBackendKind) -> &AgentBackendConfig {
        match backend {
            AgentBackendKind::AgentFrame => &self.agent_frame,
        }
    }

    pub fn backend_config_mut(&mut self, backend: AgentBackendKind) -> &mut AgentBackendConfig {
        match backend {
            AgentBackendKind::AgentFrame => &mut self.agent_frame,
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
        [AgentBackendKind::AgentFrame]
            .into_iter()
            .filter(|backend| self.is_model_available(*backend, model_key))
            .collect()
    }

    pub fn is_empty(&self) -> bool {
        self.agent_frame.available_models.is_empty()
    }

    pub fn all_available_models(&self) -> Vec<String> {
        let mut result = Vec::new();
        for backend in [AgentBackendKind::AgentFrame] {
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

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TokenEstimationSourceCacheConfig {
    #[serde(default)]
    pub hf: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TokenEstimationCacheConfig {
    #[serde(default = "default_token_estimation_template_cache")]
    pub template: TokenEstimationSourceCacheConfig,
    #[serde(default = "default_token_estimation_tokenizer_cache")]
    pub tokenizer: TokenEstimationSourceCacheConfig,
}

impl Default for TokenEstimationCacheConfig {
    fn default() -> Self {
        Self {
            template: default_token_estimation_template_cache(),
            tokenizer: default_token_estimation_tokenizer_cache(),
        }
    }
}

impl TokenEstimationCacheConfig {
    fn normalized(mut self) -> Self {
        if self.template.hf.trim().is_empty() {
            self.template.hf = default_token_estimation_hf_template_cache_dir();
        }
        if self.tokenizer.hf.trim().is_empty() {
            self.tokenizer.hf = default_token_estimation_hf_tokenizer_cache_dir();
        }
        self
    }
}

#[derive(Clone, Debug, Serialize)]
pub struct MainAgentConfig {
    #[serde(default = "default_global_install_root")]
    pub global_install_root: String,
    #[serde(default = "default_main_agent_language")]
    pub language: String,
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
    pub token_estimation_cache: TokenEstimationCacheConfig,
    #[serde(default)]
    pub memory_system: MemorySystem,
}

#[derive(Debug, Deserialize)]
struct MainAgentConfigRaw {
    #[serde(default = "default_global_install_root")]
    global_install_root: String,
    #[serde(default = "default_main_agent_language")]
    language: String,
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
    token_estimation_cache: Option<TokenEstimationCacheConfig>,
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
            global_install_root: raw.global_install_root,
            language: raw.language,
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
            token_estimation_cache: raw.token_estimation_cache.unwrap_or_default().normalized(),
            memory_system: raw.memory_system,
        })
    }
}

#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SandboxMode {
    #[default]
    #[serde(alias = "disabled")]
    Subprocess,
    Bubblewrap,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct SandboxConfig {
    #[serde(default)]
    pub mode: SandboxMode,
    #[serde(default = "default_bubblewrap_binary")]
    pub bubblewrap_binary: String,
    #[serde(default)]
    pub map_docker_socket: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ChannelConfig {
    CommandLine(CommandLineChannelConfig),
    Telegram(TelegramChannelConfig),
    Dingtalk(DingtalkChannelConfig),
    DingtalkRobot(DingtalkRobotChannelConfig),
    Web(WebChannelConfig),
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

pub(crate) fn default_anthropic_api_key_env() -> String {
    "ANTHROPIC_API_KEY".to_string()
}

pub(crate) fn default_brave_search_api_key_env() -> String {
    "BRAVE_SEARCH_API_KEY".to_string()
}

pub(crate) fn default_chat_completions_path() -> String {
    "/chat/completions".to_string()
}

pub(crate) fn default_responses_path() -> String {
    "/responses".to_string()
}

pub(crate) fn default_claude_messages_path() -> String {
    "/messages".to_string()
}

pub(crate) fn default_brave_search_path() -> String {
    "/res/v1/web/search".to_string()
}

pub(crate) fn default_codex_subscription_endpoint() -> String {
    "https://chatgpt.com/backend-api/codex".to_string()
}

pub(crate) fn default_claude_messages_endpoint() -> String {
    "https://api.anthropic.com/v1".to_string()
}

pub(crate) fn default_brave_search_endpoint() -> String {
    "https://api.search.brave.com".to_string()
}

pub(crate) fn default_brave_search_model_name() -> String {
    "brave-web-search".to_string()
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

fn default_token_estimation_hf_template_cache_dir() -> String {
    "template-cache/hf".to_string()
}

fn default_token_estimation_hf_tokenizer_cache_dir() -> String {
    "tokenizer-cache/hf".to_string()
}

fn default_token_estimation_template_cache() -> TokenEstimationSourceCacheConfig {
    TokenEstimationSourceCacheConfig {
        hf: default_token_estimation_hf_template_cache_dir(),
    }
}

fn default_token_estimation_tokenizer_cache() -> TokenEstimationSourceCacheConfig {
    TokenEstimationSourceCacheConfig {
        hf: default_token_estimation_hf_tokenizer_cache_dir(),
    }
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

fn default_dingtalk_robot_webhook_url_env() -> String {
    "DINGTALK_ROBOT_WEBHOOK_URL".to_string()
}

fn default_dingtalk_robot_app_key_env() -> String {
    "DINGTALK_ROBOT_APP_KEY".to_string()
}

fn default_dingtalk_robot_app_secret_env() -> String {
    "DINGTALK_ROBOT_APP_SECRET".to_string()
}

fn default_dingtalk_robot_http_listen_addr() -> String {
    "127.0.0.1:35888".to_string()
}

fn default_dingtalk_robot_http_callback_path() -> String {
    "/dingtalk/robot".to_string()
}

fn default_telegram_api_base_url() -> String {
    "https://api.telegram.org".to_string()
}

fn default_dingtalk_api_base_url() -> String {
    "https://api.dingtalk.com".to_string()
}

fn default_web_listen_addr() -> String {
    "127.0.0.1:8080".to_string()
}

fn default_web_auth_token_env() -> String {
    "CLAWPARTY_WEB_AUTH_TOKEN".to_string()
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
            description: "Show or set the conversation model".to_string(),
        },
        BotCommandConfig {
            command: "sandbox".to_string(),
            description: "Show or set the conversation sandbox mode".to_string(),
        },
        BotCommandConfig {
            command: "mount".to_string(),
            description: "Mount a local folder into bubblewrap for this conversation".to_string(),
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
    let loaders: [&dyn ConfigLoader; 27] = [
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
        &v0_13::LatestConfigLoader,
        &v0_14::LatestConfigLoader,
        &v0_15::LatestConfigLoader,
        &v0_16::LatestConfigLoader,
        &v0_17::LatestConfigLoader,
        &v0_18::LatestConfigLoader,
        &v0_19::LatestConfigLoader,
        &v0_20::LatestConfigLoader,
        &v0_21::LatestConfigLoader,
        &v0_22::LatestConfigLoader,
        &v0_23::LatestConfigLoader,
        &v0_24::LatestConfigLoader,
        &v0_25::LatestConfigLoader,
        &v0_26::LatestConfigLoader,
        &v0_27::LatestConfigLoader,
    ];
    let loader = loaders
        .into_iter()
        .find(|loader| loader.version() == version)
        .ok_or_else(|| anyhow!("unsupported config version '{}'", version))?;
    let mut config = loader.load_and_upgrade(value)?;
    resolve_config_token_estimation_paths(
        &mut config,
        path.parent().unwrap_or_else(|| Path::new(".")),
    );
    validate_server_config(&config)?;
    Ok(config)
}

fn resolve_config_token_estimation_paths(config: &mut ServerConfig, base_dir: &Path) {
    for model in config.models.values_mut() {
        if let Some(token_estimation) = &model.token_estimation {
            model.token_estimation = Some(token_estimation.resolve_paths(base_dir));
        }
    }
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
    for backend in [AgentBackendKind::AgentFrame] {
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
    let mut config = {
        let loaders: [&dyn ConfigLoader; 27] = [
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
            &v0_13::LatestConfigLoader,
            &v0_14::LatestConfigLoader,
            &v0_15::LatestConfigLoader,
            &v0_16::LatestConfigLoader,
            &v0_17::LatestConfigLoader,
            &v0_18::LatestConfigLoader,
            &v0_19::LatestConfigLoader,
            &v0_20::LatestConfigLoader,
            &v0_21::LatestConfigLoader,
            &v0_22::LatestConfigLoader,
            &v0_23::LatestConfigLoader,
            &v0_24::LatestConfigLoader,
            &v0_25::LatestConfigLoader,
            &v0_26::LatestConfigLoader,
            &v0_27::LatestConfigLoader,
        ];
        let loader = loaders
            .into_iter()
            .find(|loader| loader.version() == version)
            .ok_or_else(|| anyhow!("unsupported config version '{}'", version))?;
        loader.load_and_upgrade(value)?
    };
    resolve_config_token_estimation_paths(
        &mut config,
        path.parent().unwrap_or_else(|| Path::new(".")),
    );
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
        global_install_root: &'a str,
        language: &'a str,
        max_tool_roundtrips: usize,
        enable_context_compression: bool,
        context_compaction: &'a ContextCompactionConfig,
        idle_compaction: &'a IdleCompactionConfig,
        timeout_observation_compaction: &'a TimeoutObservationCompactionConfig,
        time_awareness: &'a TimeAwarenessConfig,
        token_estimation_cache: &'a TokenEstimationCacheConfig,
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
        retry_mode: &'a RetryModeConfig,
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
        token_estimation: &'a Option<TokenEstimationConfig>,
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
                    retry_mode: &model.retry_mode,
                    context_window_tokens: model.context_window_tokens,
                    cache_ttl: &model.cache_ttl,
                    reasoning: &model.reasoning,
                    headers: &model.headers,
                    description: &model.description,
                    agent_model_enabled: model.agent_model_enabled,
                    native_web_search: &model.native_web_search,
                    token_estimation: &model.token_estimation,
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
        },
        tooling: &config.tooling,
        main_agent: PersistedMainAgentConfig {
            global_install_root: &config.main_agent.global_install_root,
            language: &config.main_agent.language,
            max_tool_roundtrips: config.main_agent.max_tool_roundtrips,
            enable_context_compression: config.main_agent.enable_context_compression,
            context_compaction: &config.main_agent.context_compaction,
            idle_compaction: &config.main_agent.idle_compaction,
            timeout_observation_compaction: &config.main_agent.timeout_observation_compaction,
            time_awareness: &config.main_agent.time_awareness,
            token_estimation_cache: &config.main_agent.token_estimation_cache,
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
        validate_model_retry_mode(model_name, &model.retry_mode)?;
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
    for backend in [AgentBackendKind::AgentFrame] {
        let available_models = config.agent.available_models(backend);
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

fn validate_model_retry_mode(model_name: &str, retry_mode: &RetryModeConfig) -> Result<()> {
    match retry_mode {
        RetryModeConfig::No => Ok(()),
        RetryModeConfig::Random {
            max_retries,
            retry_random_mean,
        } => {
            if *max_retries == 0 {
                return Err(anyhow!(
                    "model '{}'.retry_mode.max_retries must be at least 1",
                    model_name
                ));
            }
            if !retry_random_mean.is_finite() || *retry_random_mean <= 0.0 {
                return Err(anyhow!(
                    "model '{}'.retry_mode.retry_random_mean must be greater than 0 seconds",
                    model_name
                ));
            }
            Ok(())
        }
    }
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
        ChannelConfig, LATEST_CONFIG_VERSION, MainAgentConfig, ModelType, SandboxMode,
        default_dingtalk_commands, expand_home_path, load_server_config_file,
        load_server_config_file_and_upgrade, resolve_model_api_keys,
    };
    use crate::backend::AgentBackendKind;
    use agent_frame::config::{
        TokenEstimationSource, TokenEstimationTemplateConfig, TokenEstimationTokenizerConfig,
    };
    use std::fs;
    use std::path::Path;
    use tempfile::TempDir;

    #[test]
    fn telegram_channel_config_loads_without_commands_field() {
        let temp_dir = TempDir::new().unwrap();
        let config_path = temp_dir.path().join("config.json");
        fs::write(
            &config_path,
            r#"
            {
              "models": {
                "main": {
                  "type": "openrouter",
                  "api_endpoint": "https://example.com/v1",
                  "model": "demo-model",
                  "description": "demo",
                  "capabilities": ["chat"]
                }
              },
              "agent": {
                "agent_frame": {"available_models": ["main"]}
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
                assert_eq!(telegram.bot_token.as_deref(), Some("token"));
                assert_eq!(telegram.poll_timeout_seconds, 30);
                assert_eq!(telegram.poll_interval_ms, 250);
            }
            _ => panic!("expected telegram channel"),
        }
    }

    #[test]
    fn web_channel_config_loads_with_auth_defaults() {
        let temp_dir = TempDir::new().unwrap();
        let config_path = temp_dir.path().join("config.json");
        fs::write(
            &config_path,
            r#"
            {
              "version": "0.21",
              "models": {
                "main": {
                  "type": "openrouter",
                  "api_endpoint": "https://example.com/v1",
                  "model": "demo-model",
                  "description": "demo",
                  "capabilities": ["chat"]
                }
              },
              "agent": {
                "agent_frame": {"available_models": ["main"]}
              },
              "main_agent": {
                "model": "main"
              },
              "channels": [
                {
                  "kind": "web",
                  "id": "web-main"
                }
              ]
            }
            "#,
        )
        .unwrap();

        let config = load_server_config_file(&config_path).unwrap();
        assert_eq!(config.version, LATEST_CONFIG_VERSION);
        match &config.channels[0] {
            ChannelConfig::Web(web) => {
                assert_eq!(web.listen_addr, "127.0.0.1:8080");
                assert_eq!(web.auth_token_env, "CLAWPARTY_WEB_AUTH_TOKEN");
            }
            _ => panic!("expected web channel"),
        }
    }

    #[test]
    fn token_estimation_config_loads_and_resolves_local_paths() {
        let temp_dir = TempDir::new().unwrap();
        let config_path = temp_dir.path().join("config.json");
        fs::write(
            &config_path,
            r#"
            {
              "version": "0.16",
              "models": {
                "main": {
                  "type": "openrouter",
                  "api_endpoint": "https://example.com/v1",
                  "model": "demo-model",
                  "capabilities": ["chat"],
                  "token_estimation": {
                    "template": {
                      "source": "local",
                      "path": "assets/tokenizer_config.json",
                      "field": "chat_template"
                    },
                    "tokenizer": {
                      "source": "local",
                      "path": "assets/tokenizer.json"
                    }
                  }
                }
              },
              "agent": {
                "agent_frame": {"available_models": ["main"]}
              },
              "main_agent": {
                "model": "main"
              },
              "channels": [
                {
                  "kind": "command_line",
                  "id": "cli"
                }
              ]
            }
            "#,
        )
        .unwrap();

        let config = load_server_config_file(&config_path).unwrap();
        let token_estimation = config.models["main"].token_estimation.as_ref().unwrap();
        match token_estimation.template.as_ref().unwrap() {
            TokenEstimationTemplateConfig::Local { path, field } => {
                assert_eq!(path, &temp_dir.path().join("assets/tokenizer_config.json"));
                assert_eq!(field, "chat_template");
            }
            other => panic!("expected local template, got {other:?}"),
        }
        match token_estimation.tokenizer.as_ref().unwrap() {
            TokenEstimationTokenizerConfig::Local { path } => {
                assert_eq!(path, &temp_dir.path().join("assets/tokenizer.json"));
            }
            other => panic!("expected local tokenizer, got {other:?}"),
        }
    }

    #[test]
    fn token_estimation_huggingface_shorthand_loads() {
        let temp_dir = TempDir::new().unwrap();
        let config_path = temp_dir.path().join("config.json");
        fs::write(
            &config_path,
            r#"
            {
              "version": "0.17",
              "models": {
                "main": {
                  "type": "openrouter",
                  "api_endpoint": "https://example.com/v1",
                  "model": "demo-model",
                  "capabilities": ["chat"],
                  "token_estimation": {
                    "source": "huggingface",
                    "repo": "Qwen/Qwen2.5-Coder-7B-Instruct",
                    "revision": "main",
                    "cache_dir": "hf-cache"
                  }
                }
              },
              "agent": {
                "agent_frame": {"available_models": ["main"]}
              },
              "main_agent": {
                "model": "main"
              },
              "channels": [
                {
                  "kind": "command_line",
                  "id": "cli"
                }
              ]
            }
            "#,
        )
        .unwrap();

        let config = load_server_config_file(&config_path).unwrap();
        let token_estimation = config.models["main"].token_estimation.as_ref().unwrap();
        assert_eq!(
            token_estimation.source,
            Some(TokenEstimationSource::Huggingface)
        );
        assert_eq!(
            token_estimation.repo.as_deref(),
            Some("Qwen/Qwen2.5-Coder-7B-Instruct")
        );
        assert_eq!(token_estimation.revision.as_deref(), Some("main"));
        let expected_cache_dir = temp_dir.path().join("hf-cache");
        assert_eq!(
            token_estimation.cache_dir.as_deref(),
            Some(expected_cache_dir.as_path())
        );
    }

    #[test]
    fn main_agent_token_estimation_cache_defaults_and_persists() {
        let temp_dir = TempDir::new().unwrap();
        let config_path = temp_dir.path().join("config.json");
        fs::write(
            &config_path,
            r#"
            {
              "version": "0.17",
              "models": {
                "main": {
                  "type": "openrouter",
                  "api_endpoint": "https://example.com/v1",
                  "model": "demo-model",
                  "capabilities": ["chat"]
                }
              },
              "agent": {
                "agent_frame": {"available_models": ["main"]}
              },
              "main_agent": {
                "model": "main"
              },
              "channels": [
                {
                  "kind": "command_line",
                  "id": "cli"
                }
              ]
            }
            "#,
        )
        .unwrap();

        let (config, upgraded) = load_server_config_file_and_upgrade(&config_path).unwrap();
        assert!(upgraded);
        assert_eq!(
            config.main_agent.token_estimation_cache.template.hf,
            "template-cache/hf"
        );
        assert_eq!(
            config.main_agent.token_estimation_cache.tokenizer.hf,
            "tokenizer-cache/hf"
        );

        let written = fs::read_to_string(&config_path).unwrap();
        assert!(written.contains(&format!("\"version\": \"{LATEST_CONFIG_VERSION}\"")));
        assert!(written.contains("\"token_estimation_cache\""));
        assert!(written.contains("\"template-cache/hf\""));
        assert!(written.contains("\"tokenizer-cache/hf\""));
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
                  "type": "openrouter",
                  "api_endpoint": "https://example.com/v1",
                  "model": "demo-model",
                  "description": "demo",
                  "capabilities": ["chat"]
                }
              },
              "agent": {
                "agent_frame": {"available_models": ["main"]}
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
    fn dingtalk_robot_channel_defaults_to_env_based_webhook() {
        let temp_dir = TempDir::new().unwrap();
        let config_path = temp_dir.path().join("config.json");
        fs::write(
            &config_path,
            r#"
            {
              "version": "0.20",
              "models": {
                "main": {
                  "type": "openrouter",
                  "api_endpoint": "https://example.com/v1",
                  "model": "demo-model",
                  "description": "demo",
                  "capabilities": ["chat"]
                }
              },
              "agent": {
                "agent_frame": {"available_models": ["main"]}
              },
              "main_agent": {
                "model": "main"
              },
              "channels": [
                {
                  "kind": "dingtalk_robot",
                  "id": "dingtalk-robot-main"
                }
              ]
            }
            "#,
        )
        .unwrap();

        let config = load_server_config_file(&config_path).unwrap();
        match &config.channels[0] {
            ChannelConfig::DingtalkRobot(dingtalk) => {
                assert_eq!(dingtalk.webhook_url_env, "DINGTALK_ROBOT_WEBHOOK_URL");
                assert!(dingtalk.webhook_url.is_none());
                assert_eq!(dingtalk.app_key_env, "DINGTALK_ROBOT_APP_KEY");
                assert!(dingtalk.app_key.is_none());
                assert_eq!(dingtalk.app_secret_env, "DINGTALK_ROBOT_APP_SECRET");
                assert!(dingtalk.app_secret.is_none());
                assert_eq!(dingtalk.http_listen_addr, "127.0.0.1:35888");
                assert_eq!(dingtalk.http_callback_path, "/dingtalk/robot");
                assert_eq!(dingtalk.api_base_url, "https://api.dingtalk.com");
            }
            _ => panic!("expected dingtalk robot channel"),
        }
    }

    #[test]
    fn dingtalk_commands_default_to_builtin_list() {
        assert_eq!(default_dingtalk_commands(), super::default_bot_commands());
    }

    #[test]
    fn default_bot_commands_omit_retired_session_commands() {
        let commands = super::default_bot_commands()
            .into_iter()
            .map(|command| command.command)
            .collect::<Vec<_>>();
        assert!(!commands.iter().any(|command| command == "new"));
        assert!(!commands.iter().any(|command| command == "oldspace"));
        assert!(commands.iter().any(|command| command == "mount"));
    }

    #[test]
    fn repository_telegram_templates_omit_commands_array() {
        let manifest_dir = Path::new(env!("CARGO_MANIFEST_DIR"));
        let template_paths = [
            manifest_dir.join("../deploy_telegram.json"),
            manifest_dir.join("../test_telegram.json"),
            manifest_dir.join("example_telegram_config.json"),
        ];

        for path in template_paths {
            let content = fs::read_to_string(&path)
                .unwrap_or_else(|err| panic!("failed to read {}: {err}", path.display()));
            let value: serde_json::Value = serde_json::from_str(&content)
                .unwrap_or_else(|err| panic!("failed to parse {}: {err}", path.display()));
            let channels = value
                .get("channels")
                .and_then(|channels| channels.as_array())
                .unwrap_or_else(|| panic!("{} has no channels array", path.display()));
            let telegram = channels
                .iter()
                .find(|channel| {
                    channel.get("kind").and_then(|kind| kind.as_str()) == Some("telegram")
                })
                .unwrap_or_else(|| panic!("{} has no telegram channel", path.display()));
            assert!(
                telegram.get("commands").is_none(),
                "{} telegram channel should not configure commands explicitly",
                path.display()
            );
        }
    }

    #[test]
    fn latest_templates_omit_legacy_model_and_external_search_fields() {
        let manifest_dir = Path::new(env!("CARGO_MANIFEST_DIR"));
        let template_paths = [
            manifest_dir.join("../deploy_telegram.json"),
            manifest_dir.join("example_telegram_config.json"),
            manifest_dir.join("example_config.json"),
        ];

        for path in template_paths {
            let content = fs::read_to_string(&path)
                .unwrap_or_else(|err| panic!("failed to read {}: {err}", path.display()));
            let value: serde_json::Value = serde_json::from_str(&content)
                .unwrap_or_else(|err| panic!("failed to parse {}: {err}", path.display()));

            assert!(
                value["main_agent"].as_object().is_some_and(|main_agent| {
                    !main_agent.contains_key("model") && !main_agent.contains_key("timeout_seconds")
                }),
                "{} should not emit legacy main_agent fields",
                path.display()
            );

            let models = value["models"]
                .as_object()
                .unwrap_or_else(|| panic!("{} has no models object", path.display()));
            for (alias, model) in models {
                assert!(
                    model
                        .as_object()
                        .is_some_and(|object| !object.contains_key("external_web_search")),
                    "{} model '{}' should not emit external_web_search",
                    path.display(),
                    alias
                );
            }
        }
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
    fn main_agent_config_ignores_legacy_timeout_seconds_field() {
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
        assert_eq!(
            config.main_agent.language,
            super::default_main_agent_language()
        );
        assert_eq!(
            config.main_agent.max_tool_roundtrips,
            super::default_max_tool_roundtrips()
        );
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
    fn legacy_disabled_sandbox_mode_loads_as_subprocess() {
        let temp_dir = TempDir::new().unwrap();
        let config_path = temp_dir.path().join("config.json");
        fs::write(
            &config_path,
            r#"
            {
              "version": "0.12",
              "models": {
                "main": {
                  "type": "openrouter",
                  "api_endpoint": "https://openrouter.ai/api/v1",
                  "model": "anthropic/claude-sonnet-4.6",
                  "capabilities": ["chat"],
                  "description": "demo"
                }
              },
              "agent": {
                "agent_frame": {
                  "available_models": ["main"]
                }
              },
              "main_agent": {
                "model": "main"
              },
              "sandbox": {
                "mode": "disabled"
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
        assert_eq!(config.sandbox.mode, SandboxMode::Subprocess);
        let rewritten = fs::read_to_string(&config_path).unwrap();
        assert!(rewritten.contains(r#""mode": "subprocess""#));
        assert!(!rewritten.contains(r#""mode": "disabled""#));
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
    fn claude_code_upgrade_defaults_to_messages_path_and_anthropic_endpoint() {
        let temp_dir = TempDir::new().unwrap();
        let config_path = temp_dir.path().join("config.json");
        fs::write(
            &config_path,
            r#"
            {
              "version": "0.25",
              "models": {
                "main": {
                  "type": "claude-code",
                  "model": "claude-opus-4-6",
                  "capabilities": ["chat"],
                  "description": "demo"
                }
              },
              "agent": {
                "agent_frame": {
                  "available_models": ["main"]
                }
              },
              "main_agent": {
                "language": "zh-CN"
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
        assert_eq!(config.models["main"].model_type, ModelType::ClaudeCode);
        assert_eq!(
            config.models["main"].api_endpoint,
            "https://api.anthropic.com/v1"
        );
        assert_eq!(config.models["main"].chat_completions_path, "/messages");
        assert_eq!(config.models["main"].api_key_env, "ANTHROPIC_API_KEY");
    }

    #[test]
    fn brave_search_upgrade_defaults_to_brave_endpoint_and_search_path() {
        let temp_dir = TempDir::new().unwrap();
        let config_path = temp_dir.path().join("config.json");
        fs::write(
            &config_path,
            r#"
            {
              "version": "0.26",
              "models": {
                "brave": {
                  "type": "brave-search",
                  "model": "",
                  "capabilities": ["web_search"],
                  "description": "Brave search helper",
                  "agent_model_enabled": false
                },
                "main": {
                  "type": "openrouter",
                  "model": "openai/gpt-4.1-mini",
                  "capabilities": ["chat"],
                  "description": "demo"
                }
              },
              "agent": {
                "agent_frame": {
                  "available_models": ["main"]
                }
              },
              "tooling": {
                "web_search": "brave"
              },
              "main_agent": {
                "language": "zh-CN"
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
        assert_eq!(config.models["brave"].model_type, ModelType::BraveSearch);
        assert_eq!(
            config.models["brave"].api_endpoint,
            "https://api.search.brave.com"
        );
        assert_eq!(
            config.models["brave"].chat_completions_path,
            "/res/v1/web/search"
        );
        assert_eq!(config.models["brave"].api_key_env, "BRAVE_SEARCH_API_KEY");
        assert_eq!(config.models["brave"].model, "brave-web-search");
        assert_eq!(
            config
                .tooling
                .web_search
                .as_ref()
                .map(|target| target.alias.as_str()),
            Some("brave")
        );
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
    fn legacy_external_web_search_is_ignored_during_upgrade() {
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
        assert!(config.models["main"].web_search_model.is_none());
        assert!(config.model_catalog.web_search.is_empty());

        let written = fs::read_to_string(&config_path).unwrap();
        let written_json: serde_json::Value = serde_json::from_str(&written).unwrap();
        assert!(written.contains(&format!("\"version\": \"{LATEST_CONFIG_VERSION}\"")));
        assert!(written.contains("\"models\": {"));
        assert!(!written.contains("\"tooling\": {"));
        assert!(!written.contains("\"enabled_tools\""));
        assert!(!written.contains("\"external_web_search\""));
        assert!(
            written_json["models"]["main"]
                .as_object()
                .unwrap()
                .get("web_search")
                .is_none()
        );
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
        assert_eq!(
            config.models["gpt54"].resolved_codex_home(),
            Some(expand_home_path("~/.codex"))
        );
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
        assert!(written.contains(&format!("\"version\": \"{LATEST_CONFIG_VERSION}\"")));
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
        assert!(!config.models["helper"].agent_model_enabled);
    }

    #[test]
    fn latest_version_config_loads_on_non_upgrade_path() {
        let temp_dir = TempDir::new().unwrap();
        let config_path = temp_dir.path().join("config.json");
        fs::write(
            &config_path,
            format!(
                r#"
            {{
              "version": "{LATEST_CONFIG_VERSION}",
              "models": {{
                "main": {{
                  "type": "openrouter",
                  "api_endpoint": "https://example.com/v1",
                  "model": "demo-main",
                  "description": "demo",
                  "capabilities": ["chat"]
                }}
              }},
              "agent": {{
                "agent_frame": {{"available_models": ["main"]}}
              }},
              "main_agent": {{
                "global_install_root": "/opt",
                "language": "zh-CN",
                "memory_system": "claude_code",
                "time_awareness": {{
                  "emit_system_date_on_user_message": true,
                  "emit_idle_time_gap_hint": true
                }},
                "enable_context_compression": true,
                "context_compaction": {{
                  "trigger_ratio": 0.9,
                  "token_limit_override": null,
                  "recent_fidelity_target_ratio": 0.18
                }},
                "idle_compaction": {{
                  "enabled": false,
                  "poll_interval_seconds": 15,
                  "min_ratio": 0.5
                }},
                "timeout_observation_compaction": {{
                  "enabled": true
                }}
              }},
              "sandbox": {{
                "mode": "subprocess",
                "bubblewrap_binary": "bwrap",
                "map_docker_socket": false
              }},
              "max_global_sub_agents": 4,
              "cron_poll_interval_seconds": 5,
              "channels": [
                {{
                  "kind": "command_line",
                  "id": "local-cli"
                }}
              ]
            }}
            "#
            ),
        )
        .unwrap();

        let config = load_server_config_file(&config_path).unwrap();
        assert_eq!(config.version, LATEST_CONFIG_VERSION);
    }

    #[test]
    fn main_agent_config_ignores_legacy_model_field() {
        let config: MainAgentConfig = serde_json::from_value(serde_json::json!({
            "model": "helper"
        }))
        .unwrap();

        assert_eq!(config.language, super::default_main_agent_language());
        assert_eq!(
            config.max_tool_roundtrips,
            super::default_max_tool_roundtrips()
        );
    }

    #[test]
    fn main_agent_config_ignores_legacy_timeout_field() {
        let config: MainAgentConfig = serde_json::from_value(serde_json::json!({
            "timeout_seconds": 0
        }))
        .unwrap();

        assert_eq!(config.language, super::default_main_agent_language());
        assert_eq!(
            config.max_tool_roundtrips,
            super::default_max_tool_roundtrips()
        );
    }

    #[test]
    fn main_agent_config_ignores_legacy_enabled_tools_field() {
        let config: MainAgentConfig = serde_json::from_value(serde_json::json!({
            "enabled_tools": ["read_file", "write_file", "file_read"]
        }))
        .unwrap();

        assert_eq!(config.language, super::default_main_agent_language());
        assert_eq!(
            config.max_tool_roundtrips,
            super::default_max_tool_roundtrips()
        );
    }
}
