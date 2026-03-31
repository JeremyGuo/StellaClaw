use anyhow::{Context, Result, anyhow};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::fs;
use std::path::{Path, PathBuf};

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct UpstreamConfig {
    pub base_url: String,
    pub model: String,
    #[serde(default)]
    pub supports_vision_input: bool,
    #[serde(default)]
    pub api_key: Option<String>,
    #[serde(default = "default_api_key_env")]
    pub api_key_env: String,
    #[serde(default = "default_chat_completions_path")]
    pub chat_completions_path: String,
    #[serde(default = "default_timeout_seconds")]
    pub timeout_seconds: f64,
    #[serde(default = "default_context_window_tokens")]
    pub context_window_tokens: usize,
    #[serde(default)]
    pub cache_control: Option<CacheControlConfig>,
    #[serde(default)]
    pub reasoning: Option<ReasoningConfig>,
    #[serde(default)]
    pub headers: serde_json::Map<String, Value>,
    #[serde(default)]
    pub native_web_search: Option<NativeWebSearchConfig>,
    #[serde(default)]
    pub external_web_search: Option<ExternalWebSearchConfig>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct CacheControlConfig {
    #[serde(rename = "type")]
    pub cache_type: String,
    #[serde(default)]
    pub ttl: Option<String>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct ReasoningConfig {
    #[serde(default)]
    pub effort: Option<String>,
    #[serde(default)]
    pub max_tokens: Option<u64>,
    #[serde(default)]
    pub exclude: Option<bool>,
    #[serde(default)]
    pub enabled: Option<bool>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct NativeWebSearchConfig {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default)]
    pub payload: serde_json::Map<String, Value>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ExternalWebSearchConfig {
    #[serde(default = "default_external_web_search_base_url")]
    pub base_url: String,
    #[serde(default = "default_external_web_search_model")]
    pub model: String,
    #[serde(default)]
    pub api_key: Option<String>,
    #[serde(default = "default_external_web_search_api_key_env")]
    pub api_key_env: String,
    #[serde(default = "default_chat_completions_path")]
    pub chat_completions_path: String,
    #[serde(default = "default_external_web_search_timeout_seconds")]
    pub timeout_seconds: f64,
    #[serde(default)]
    pub headers: serde_json::Map<String, Value>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AgentConfig {
    #[serde(default)]
    pub enabled_tools: Vec<String>,
    pub upstream: UpstreamConfig,
    #[serde(default)]
    pub skills_dirs: Vec<PathBuf>,
    #[serde(default)]
    pub system_prompt: String,
    #[serde(default = "default_max_tool_roundtrips")]
    pub max_tool_roundtrips: usize,
    pub workspace_root: PathBuf,
    #[serde(default = "default_enable_context_compression")]
    pub enable_context_compression: bool,
    #[serde(default = "default_effective_context_window_percent")]
    pub effective_context_window_percent: f64,
    #[serde(default)]
    pub auto_compact_token_limit: Option<usize>,
    #[serde(default = "default_retain_recent_messages")]
    pub retain_recent_messages: usize,
}

#[derive(Deserialize)]
struct AgentConfigRaw {
    #[serde(default)]
    enabled_tools: Vec<String>,
    upstream: UpstreamConfigRaw,
    #[serde(default)]
    skills_dirs: Vec<String>,
    #[serde(default)]
    system_prompt: String,
    #[serde(default = "default_max_tool_roundtrips")]
    max_tool_roundtrips: usize,
    #[serde(default)]
    workspace_root: Option<String>,
    #[serde(default = "default_enable_context_compression")]
    enable_context_compression: bool,
    #[serde(default = "default_effective_context_window_percent")]
    effective_context_window_percent: f64,
    #[serde(default)]
    auto_compact_token_limit: Option<usize>,
    #[serde(default = "default_retain_recent_messages")]
    retain_recent_messages: usize,
}

#[derive(Deserialize)]
struct UpstreamConfigRaw {
    base_url: String,
    model: String,
    #[serde(default)]
    supports_vision_input: bool,
    #[serde(default)]
    api_key: Option<String>,
    #[serde(default = "default_api_key_env")]
    api_key_env: String,
    #[serde(default = "default_chat_completions_path")]
    chat_completions_path: String,
    #[serde(default = "default_timeout_seconds")]
    timeout_seconds: f64,
    #[serde(default = "default_context_window_tokens")]
    context_window_tokens: usize,
    #[serde(default)]
    cache_control: Option<CacheControlConfig>,
    #[serde(default)]
    reasoning: Option<ReasoningConfig>,
    #[serde(default)]
    reasoning_effort: Option<String>,
    #[serde(default)]
    headers: serde_json::Map<String, Value>,
    #[serde(default)]
    native_web_search: Option<NativeWebSearchConfig>,
    #[serde(default)]
    external_web_search: Option<ExternalWebSearchConfig>,
}

fn default_api_key_env() -> String {
    "OPENAI_API_KEY".to_string()
}

fn default_chat_completions_path() -> String {
    "/chat/completions".to_string()
}

fn default_external_web_search_base_url() -> String {
    "https://openrouter.ai/api/v1".to_string()
}

fn default_external_web_search_model() -> String {
    "perplexity/sonar".to_string()
}

fn default_external_web_search_api_key_env() -> String {
    "OPENROUTER_API_KEY".to_string()
}

fn default_external_web_search_timeout_seconds() -> f64 {
    60.0
}

fn default_timeout_seconds() -> f64 {
    120.0
}

fn default_context_window_tokens() -> usize {
    128_000
}

fn default_max_tool_roundtrips() -> usize {
    12
}

fn default_enable_context_compression() -> bool {
    true
}

fn default_effective_context_window_percent() -> f64 {
    0.9
}

fn default_retain_recent_messages() -> usize {
    8
}

fn resolve_path(path: &str, base_dir: &Path) -> PathBuf {
    let expanded = if let Some(stripped) = path.strip_prefix("~/") {
        std::env::var_os("HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("~"))
            .join(stripped)
    } else {
        PathBuf::from(path)
    };

    if expanded.is_absolute() {
        expanded
    } else {
        base_dir.join(expanded)
    }
}

pub fn load_config_value(config_value: Value, base_dir: impl AsRef<Path>) -> Result<AgentConfig> {
    let base_dir = base_dir.as_ref();
    let raw: AgentConfigRaw =
        serde_json::from_value(config_value).context("failed to parse config object")?;

    if raw.upstream.base_url.trim().is_empty() || raw.upstream.model.trim().is_empty() {
        return Err(anyhow!("config.upstream must include base_url and model"));
    }

    let reasoning = match (&raw.upstream.reasoning, &raw.upstream.reasoning_effort) {
        (Some(reasoning), _) => Some(reasoning.clone()),
        (None, Some(effort)) => Some(ReasoningConfig {
            effort: Some(effort.clone()),
            ..ReasoningConfig::default()
        }),
        (None, None) => None,
    };

    let workspace_root = raw
        .workspace_root
        .as_deref()
        .map(|value| resolve_path(value, base_dir))
        .unwrap_or_else(|| base_dir.to_path_buf());

    Ok(AgentConfig {
        enabled_tools: raw.enabled_tools,
        upstream: UpstreamConfig {
            base_url: raw.upstream.base_url,
            model: raw.upstream.model,
            supports_vision_input: raw.upstream.supports_vision_input,
            api_key: raw.upstream.api_key,
            api_key_env: raw.upstream.api_key_env,
            chat_completions_path: raw.upstream.chat_completions_path,
            timeout_seconds: raw.upstream.timeout_seconds,
            context_window_tokens: raw.upstream.context_window_tokens,
            cache_control: raw.upstream.cache_control,
            reasoning,
            headers: raw.upstream.headers,
            native_web_search: raw.upstream.native_web_search,
            external_web_search: raw.upstream.external_web_search,
        },
        skills_dirs: raw
            .skills_dirs
            .iter()
            .map(|path| resolve_path(path, base_dir))
            .collect(),
        system_prompt: raw.system_prompt,
        max_tool_roundtrips: raw.max_tool_roundtrips,
        workspace_root,
        enable_context_compression: raw.enable_context_compression,
        effective_context_window_percent: raw.effective_context_window_percent,
        auto_compact_token_limit: raw.auto_compact_token_limit,
        retain_recent_messages: raw.retain_recent_messages,
    })
}

pub fn load_config_file(path: impl AsRef<Path>) -> Result<AgentConfig> {
    let path = path.as_ref();
    let config_text = fs::read_to_string(path)
        .with_context(|| format!("failed to read config file {}", path.display()))?;
    let value: Value =
        serde_json::from_str(&config_text).context("failed to parse config file as JSON")?;
    let base_dir = path.parent().unwrap_or_else(|| Path::new("."));
    load_config_value(value, base_dir)
}
