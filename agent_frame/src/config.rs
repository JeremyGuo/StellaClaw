use anyhow::{Context, Result, anyhow};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum UpstreamApiKind {
    #[default]
    ChatCompletions,
    Responses,
    ClaudeMessages,
}

#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum UpstreamAuthKind {
    #[default]
    ApiKey,
    CodexSubscription,
}

#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum AuthCredentialsStoreMode {
    File,
    Keyring,
    #[default]
    Auto,
    Ephemeral,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(tag = "mode", rename_all = "snake_case")]
pub enum RetryModeConfig {
    No,
    Random {
        max_retries: u32,
        retry_random_mean: f64,
    },
}

impl Default for RetryModeConfig {
    fn default() -> Self {
        Self::No
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct UpstreamConfig {
    pub base_url: String,
    pub model: String,
    #[serde(default)]
    pub api_kind: UpstreamApiKind,
    #[serde(default)]
    pub auth_kind: UpstreamAuthKind,
    #[serde(default)]
    pub supports_vision_input: bool,
    #[serde(default)]
    pub supports_pdf_input: bool,
    #[serde(default)]
    pub supports_audio_input: bool,
    #[serde(default)]
    pub api_key: Option<String>,
    #[serde(default = "default_api_key_env")]
    pub api_key_env: String,
    #[serde(default = "default_chat_completions_path")]
    pub chat_completions_path: String,
    #[serde(default)]
    pub codex_home: Option<PathBuf>,
    #[serde(default)]
    pub codex_auth: Option<CodexAuthConfig>,
    #[serde(default)]
    pub auth_credentials_store_mode: AuthCredentialsStoreMode,
    #[serde(default = "default_timeout_seconds")]
    pub timeout_seconds: f64,
    #[serde(default)]
    pub retry_mode: RetryModeConfig,
    #[serde(default = "default_context_window_tokens")]
    pub context_window_tokens: usize,
    #[serde(default)]
    pub cache_control: Option<CacheControlConfig>,
    #[serde(default)]
    pub prompt_cache_retention: Option<String>,
    #[serde(default)]
    pub prompt_cache_key: Option<String>,
    #[serde(default)]
    pub reasoning: Option<ReasoningConfig>,
    #[serde(default)]
    pub headers: serde_json::Map<String, Value>,
    #[serde(default)]
    pub native_web_search: Option<NativeWebSearchConfig>,
    #[serde(default)]
    pub external_web_search: Option<ExternalWebSearchConfig>,
    #[serde(default)]
    pub native_image_input: bool,
    #[serde(default)]
    pub native_pdf_input: bool,
    #[serde(default)]
    pub native_audio_input: bool,
    #[serde(default)]
    pub native_image_generation: bool,
    #[serde(default)]
    pub token_estimation: Option<TokenEstimationConfig>,
}

#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum TokenEstimationTiktokenEncoding {
    #[default]
    Auto,
    O200kBase,
    Cl100kBase,
    O200kHarmony,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum TokenEstimationSource {
    Huggingface,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "source", rename_all = "snake_case")]
pub enum TokenEstimationTemplateConfig {
    Builtin,
    Local {
        path: PathBuf,
        #[serde(default = "default_token_estimation_template_field")]
        field: String,
    },
    Huggingface {
        repo: String,
        #[serde(default = "default_huggingface_revision")]
        revision: String,
        #[serde(default = "default_huggingface_template_file")]
        file: String,
        #[serde(default = "default_token_estimation_template_field")]
        field: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        cache_dir: Option<PathBuf>,
    },
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "source", rename_all = "snake_case")]
pub enum TokenEstimationTokenizerConfig {
    Tiktoken {
        #[serde(default)]
        encoding: TokenEstimationTiktokenEncoding,
    },
    Local {
        path: PathBuf,
    },
    Huggingface {
        repo: String,
        #[serde(default = "default_huggingface_revision")]
        revision: String,
        #[serde(default = "default_huggingface_tokenizer_file")]
        file: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        cache_dir: Option<PathBuf>,
    },
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct TokenEstimationConfig {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<TokenEstimationSource>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repo: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub revision: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_dir: Option<PathBuf>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub template: Option<TokenEstimationTemplateConfig>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tokenizer: Option<TokenEstimationTokenizerConfig>,
}

impl TokenEstimationConfig {
    pub fn resolve_paths(&self, base_dir: &Path) -> Self {
        let mut resolved = self.clone();
        if let Some(TokenEstimationTemplateConfig::Local { path, .. }) = &mut resolved.template {
            *path = resolve_path(&path.to_string_lossy(), base_dir);
        }
        if let Some(TokenEstimationTemplateConfig::Huggingface {
            cache_dir: Some(cache_dir),
            ..
        }) = &mut resolved.template
        {
            *cache_dir = resolve_path(&cache_dir.to_string_lossy(), base_dir);
        }
        if let Some(TokenEstimationTokenizerConfig::Local { path }) = &mut resolved.tokenizer {
            *path = resolve_path(&path.to_string_lossy(), base_dir);
        }
        if let Some(TokenEstimationTokenizerConfig::Huggingface {
            cache_dir: Some(cache_dir),
            ..
        }) = &mut resolved.tokenizer
        {
            *cache_dir = resolve_path(&cache_dir.to_string_lossy(), base_dir);
        }
        if let Some(cache_dir) = &mut resolved.cache_dir {
            *cache_dir = resolve_path(&cache_dir.to_string_lossy(), base_dir);
        }
        resolved
    }
}

fn default_token_estimation_template_field() -> String {
    "chat_template".to_string()
}

fn default_huggingface_revision() -> String {
    "main".to_string()
}

fn default_huggingface_template_file() -> String {
    "tokenizer_config.json".to_string()
}

fn default_huggingface_tokenizer_file() -> String {
    "tokenizer.json".to_string()
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CodexAuthConfig {
    pub access_token: String,
    #[serde(default)]
    pub refresh_token: String,
    #[serde(default)]
    pub account_id: Option<String>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct CacheControlConfig {
    #[serde(rename = "type")]
    pub cache_type: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ttl: Option<String>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct ReasoningConfig {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effort: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exclude: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
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
    pub supports_vision_input: bool,
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
pub struct TimeoutObservationCompactionConfig {
    #[serde(default = "default_enable_timeout_observation_compaction")]
    pub enabled: bool,
}

#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum MemorySystem {
    #[default]
    Layered,
    ClaudeCode,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RemoteWorkpathConfig {
    pub host: String,
    pub path: String,
    #[serde(default)]
    pub description: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AgentConfig {
    pub upstream: UpstreamConfig,
    #[serde(default)]
    pub available_upstreams: BTreeMap<String, UpstreamConfig>,
    #[serde(default)]
    pub image_tool_upstream: Option<UpstreamConfig>,
    #[serde(default)]
    pub pdf_tool_upstream: Option<UpstreamConfig>,
    #[serde(default)]
    pub audio_tool_upstream: Option<UpstreamConfig>,
    #[serde(default)]
    pub image_generation_tool_upstream: Option<UpstreamConfig>,
    #[serde(default)]
    pub skills_dirs: Vec<PathBuf>,
    #[serde(default)]
    pub skills_metadata_prompt: Option<String>,
    #[serde(default)]
    pub system_prompt: String,
    #[serde(default)]
    pub remote_workpaths: Vec<RemoteWorkpathConfig>,
    #[serde(default = "default_enable_remote_tools")]
    pub enable_remote_tools: bool,
    #[serde(default = "default_max_tool_roundtrips")]
    pub max_tool_roundtrips: usize,
    pub workspace_root: PathBuf,
    pub runtime_state_root: PathBuf,
    #[serde(default = "default_enable_context_compression")]
    pub enable_context_compression: bool,
    #[serde(default)]
    pub context_compaction: ContextCompactionConfig,
    #[serde(default)]
    pub timeout_observation_compaction: TimeoutObservationCompactionConfig,
    #[serde(default)]
    pub memory_system: MemorySystem,
}

#[derive(Deserialize)]
struct AgentConfigRaw {
    upstream: UpstreamConfigRaw,
    #[serde(default)]
    available_upstreams: BTreeMap<String, UpstreamConfigRaw>,
    #[serde(default)]
    image_tool_upstream: Option<UpstreamConfigRaw>,
    #[serde(default)]
    pdf_tool_upstream: Option<UpstreamConfigRaw>,
    #[serde(default)]
    audio_tool_upstream: Option<UpstreamConfigRaw>,
    #[serde(default)]
    image_generation_tool_upstream: Option<UpstreamConfigRaw>,
    #[serde(default)]
    skills_dirs: Vec<String>,
    #[serde(default)]
    skills_metadata_prompt: Option<String>,
    #[serde(default)]
    system_prompt: String,
    #[serde(default)]
    remote_workpaths: Vec<RemoteWorkpathConfig>,
    #[serde(default = "default_enable_remote_tools")]
    enable_remote_tools: bool,
    #[serde(default = "default_max_tool_roundtrips")]
    max_tool_roundtrips: usize,
    #[serde(default)]
    workspace_root: Option<String>,
    #[serde(default)]
    runtime_state_root: Option<String>,
    #[serde(default = "default_enable_context_compression")]
    enable_context_compression: bool,
    #[serde(default)]
    context_compaction: Option<ContextCompactionConfig>,
    #[serde(default)]
    timeout_observation_compaction: Option<TimeoutObservationCompactionConfig>,
    #[serde(default)]
    memory_system: MemorySystem,
}

#[derive(Deserialize)]
struct UpstreamConfigRaw {
    base_url: String,
    model: String,
    #[serde(default)]
    api_kind: UpstreamApiKind,
    #[serde(default)]
    auth_kind: UpstreamAuthKind,
    #[serde(default)]
    supports_vision_input: bool,
    #[serde(default)]
    supports_pdf_input: bool,
    #[serde(default)]
    supports_audio_input: bool,
    #[serde(default)]
    api_key: Option<String>,
    #[serde(default = "default_api_key_env")]
    api_key_env: String,
    #[serde(default = "default_chat_completions_path")]
    chat_completions_path: String,
    #[serde(default)]
    codex_home: Option<String>,
    #[serde(default)]
    codex_auth: Option<CodexAuthConfig>,
    #[serde(default)]
    auth_credentials_store_mode: AuthCredentialsStoreMode,
    #[serde(default = "default_timeout_seconds")]
    timeout_seconds: f64,
    #[serde(default)]
    retry_mode: RetryModeConfig,
    #[serde(default = "default_context_window_tokens")]
    context_window_tokens: usize,
    #[serde(default)]
    cache_control: Option<CacheControlConfig>,
    #[serde(default)]
    prompt_cache_retention: Option<String>,
    #[serde(default)]
    prompt_cache_key: Option<String>,
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
    #[serde(default)]
    native_image_input: bool,
    #[serde(default)]
    native_pdf_input: bool,
    #[serde(default)]
    native_audio_input: bool,
    #[serde(default)]
    native_image_generation: bool,
    #[serde(default)]
    token_estimation: Option<TokenEstimationConfig>,
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

fn default_enable_remote_tools() -> bool {
    true
}

fn default_enable_context_compression() -> bool {
    true
}

fn default_compact_trigger_ratio() -> f64 {
    0.9
}

fn default_recent_fidelity_target_ratio() -> f64 {
    0.18
}

fn default_enable_timeout_observation_compaction() -> bool {
    true
}

pub fn expand_home_path(path: &str) -> PathBuf {
    if let Some(stripped) = path.strip_prefix("~/") {
        std::env::var_os("HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("~"))
            .join(stripped)
    } else {
        PathBuf::from(path)
    }
}

fn resolve_path(path: &str, base_dir: &Path) -> PathBuf {
    let expanded = expand_home_path(path);

    if expanded.is_absolute() {
        expanded
    } else {
        base_dir.join(expanded)
    }
}

fn resolve_upstream(raw: UpstreamConfigRaw, base_dir: &Path) -> UpstreamConfig {
    let reasoning = match (&raw.reasoning, &raw.reasoning_effort) {
        (Some(reasoning), _) => Some(reasoning.clone()),
        (None, Some(effort)) => Some(ReasoningConfig {
            effort: Some(effort.clone()),
            ..ReasoningConfig::default()
        }),
        (None, None) => None,
    };

    UpstreamConfig {
        base_url: raw.base_url,
        model: raw.model,
        api_kind: raw.api_kind,
        auth_kind: raw.auth_kind,
        supports_vision_input: raw.supports_vision_input,
        supports_pdf_input: raw.supports_pdf_input,
        supports_audio_input: raw.supports_audio_input,
        api_key: raw.api_key,
        api_key_env: raw.api_key_env,
        chat_completions_path: raw.chat_completions_path,
        codex_home: raw.codex_home.as_deref().map(expand_home_path),
        codex_auth: raw.codex_auth,
        auth_credentials_store_mode: raw.auth_credentials_store_mode,
        timeout_seconds: raw.timeout_seconds,
        retry_mode: raw.retry_mode,
        context_window_tokens: raw.context_window_tokens,
        cache_control: raw.cache_control,
        prompt_cache_retention: raw.prompt_cache_retention,
        prompt_cache_key: raw.prompt_cache_key,
        reasoning,
        headers: raw.headers,
        native_web_search: raw.native_web_search,
        external_web_search: raw.external_web_search,
        native_image_input: raw.native_image_input,
        native_pdf_input: raw.native_pdf_input,
        native_audio_input: raw.native_audio_input,
        native_image_generation: raw.native_image_generation,
        token_estimation: raw
            .token_estimation
            .map(|config| config.resolve_paths(base_dir)),
    }
}

fn validate_retry_mode(label: &str, retry_mode: &RetryModeConfig) -> Result<()> {
    match retry_mode {
        RetryModeConfig::No => Ok(()),
        RetryModeConfig::Random {
            max_retries,
            retry_random_mean,
        } => {
            if *max_retries == 0 {
                return Err(anyhow!("{label}.max_retries must be at least 1"));
            }
            if !retry_random_mean.is_finite() || *retry_random_mean <= 0.0 {
                return Err(anyhow!(
                    "{label}.retry_random_mean must be greater than 0 seconds"
                ));
            }
            Ok(())
        }
    }
}

#[derive(Deserialize)]
struct CodexAuthFile {
    #[serde(default)]
    tokens: Option<CodexAuthConfig>,
}

pub fn load_codex_auth_tokens(codex_home: &Path) -> Result<CodexAuthConfig> {
    let auth_path = codex_home.join("auth.json");
    let raw = fs::read_to_string(&auth_path)
        .with_context(|| format!("failed to read {}", auth_path.display()))?;
    let auth_file: CodexAuthFile =
        serde_json::from_str(&raw).context("failed to parse codex auth.json")?;
    auth_file
        .tokens
        .ok_or_else(|| anyhow!("codex auth.json does not contain tokens"))
}

pub fn load_config_value(config_value: Value, base_dir: impl AsRef<Path>) -> Result<AgentConfig> {
    let base_dir = base_dir.as_ref();
    reject_legacy_agent_config_fields(&config_value)?;
    let raw: AgentConfigRaw =
        serde_json::from_value(config_value).context("failed to parse config object")?;

    if raw.upstream.base_url.trim().is_empty() || raw.upstream.model.trim().is_empty() {
        return Err(anyhow!("config.upstream must include base_url and model"));
    }
    validate_retry_mode("config.upstream.retry_mode", &raw.upstream.retry_mode)?;
    if let Some(image_tool_upstream) = &raw.image_tool_upstream
        && (image_tool_upstream.base_url.trim().is_empty()
            || image_tool_upstream.model.trim().is_empty())
    {
        return Err(anyhow!(
            "config.image_tool_upstream must include base_url and model when provided"
        ));
    }
    if let Some(image_tool_upstream) = &raw.image_tool_upstream {
        validate_retry_mode(
            "config.image_tool_upstream.retry_mode",
            &image_tool_upstream.retry_mode,
        )?;
    }
    for (label, upstream) in [
        ("config.pdf_tool_upstream", raw.pdf_tool_upstream.as_ref()),
        (
            "config.audio_tool_upstream",
            raw.audio_tool_upstream.as_ref(),
        ),
        (
            "config.image_generation_tool_upstream",
            raw.image_generation_tool_upstream.as_ref(),
        ),
    ] {
        if let Some(upstream) = upstream
            && (upstream.base_url.trim().is_empty() || upstream.model.trim().is_empty())
        {
            return Err(anyhow!(
                "{label} must include base_url and model when provided"
            ));
        }
        if let Some(upstream) = upstream {
            validate_retry_mode(&format!("{label}.retry_mode"), &upstream.retry_mode)?;
        }
    }

    let workspace_root_explicit = raw.workspace_root.is_some();
    let workspace_root = raw
        .workspace_root
        .as_deref()
        .map(|value| resolve_path(value, base_dir))
        .unwrap_or_else(|| base_dir.to_path_buf());
    let runtime_state_root = raw
        .runtime_state_root
        .as_deref()
        .map(|value| resolve_path(value, base_dir))
        .unwrap_or_else(|| {
            if workspace_root_explicit {
                workspace_root.clone()
            } else {
                std::env::temp_dir().join("agent_frame")
            }
        });

    Ok(AgentConfig {
        upstream: resolve_upstream(raw.upstream, base_dir),
        available_upstreams: raw
            .available_upstreams
            .into_iter()
            .map(|(key, upstream)| (key, resolve_upstream(upstream, base_dir)))
            .collect(),
        image_tool_upstream: raw
            .image_tool_upstream
            .map(|upstream| resolve_upstream(upstream, base_dir)),
        pdf_tool_upstream: raw
            .pdf_tool_upstream
            .map(|upstream| resolve_upstream(upstream, base_dir)),
        audio_tool_upstream: raw
            .audio_tool_upstream
            .map(|upstream| resolve_upstream(upstream, base_dir)),
        image_generation_tool_upstream: raw
            .image_generation_tool_upstream
            .map(|upstream| resolve_upstream(upstream, base_dir)),
        skills_dirs: raw
            .skills_dirs
            .iter()
            .map(|path| resolve_path(path, base_dir))
            .collect(),
        skills_metadata_prompt: raw.skills_metadata_prompt,
        system_prompt: raw.system_prompt,
        remote_workpaths: raw.remote_workpaths,
        enable_remote_tools: raw.enable_remote_tools,
        max_tool_roundtrips: raw.max_tool_roundtrips,
        workspace_root,
        runtime_state_root,
        enable_context_compression: raw.enable_context_compression,
        context_compaction: raw.context_compaction.unwrap_or_default(),
        timeout_observation_compaction: raw.timeout_observation_compaction.unwrap_or_default(),
        memory_system: raw.memory_system,
    })
}

fn reject_legacy_agent_config_fields(config_value: &Value) -> Result<()> {
    let Some(object) = config_value.as_object() else {
        return Ok(());
    };
    for key in [
        "compact_trigger_ratio",
        "effective_context_window_percent",
        "recent_fidelity_target_ratio",
        "auto_compact_token_limit",
    ] {
        if object.contains_key(key) {
            return Err(anyhow!(
                "latest agent_frame config no longer accepts legacy field '{}'",
                key
            ));
        }
    }
    Ok(())
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

#[cfg(test)]
mod tests {
    use super::{expand_home_path, load_config_value};
    use serde_json::json;
    use std::path::PathBuf;
    use tempfile::TempDir;

    #[test]
    fn load_config_value_ignores_legacy_enabled_tools_field() {
        let temp_dir = TempDir::new().unwrap();
        let config = load_config_value(
            json!({
                "enabled_tools": ["read_file", "write_file", "file_read"],
                "upstream": {
                    "base_url": "https://example.com/v1",
                    "model": "demo"
                }
            }),
            temp_dir.path(),
        )
        .unwrap();

        assert_eq!(
            config.max_tool_roundtrips,
            super::default_max_tool_roundtrips()
        );
    }

    #[test]
    fn load_config_value_rejects_legacy_flat_compaction_fields() {
        let temp_dir = TempDir::new().unwrap();
        let error = load_config_value(
            json!({
                "compact_trigger_ratio": 0.8,
                "upstream": {
                    "base_url": "https://example.com/v1",
                    "model": "demo"
                }
            }),
            temp_dir.path(),
        )
        .unwrap_err();

        assert!(error.to_string().contains("compact_trigger_ratio"));
    }

    #[test]
    fn load_config_value_expands_tilde_in_codex_home() {
        let temp_dir = TempDir::new().unwrap();
        let config = load_config_value(
            json!({
                "upstream": {
                    "base_url": "https://example.com/v1",
                    "model": "demo",
                    "auth_kind": "codex_subscription",
                    "codex_home": "~/.codex"
                }
            }),
            temp_dir.path(),
        )
        .unwrap();

        assert_eq!(
            config.upstream.codex_home,
            Some(expand_home_path("~/.codex"))
        );
        assert_ne!(config.upstream.codex_home, Some(PathBuf::from("~/.codex")));
    }
}
