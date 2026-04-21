use super::{
    AgentConfig, ChannelConfig, ConfigLoader, LATEST_CONFIG_VERSION, LEGACY_CONFIG_VERSION,
    MainAgentConfig, ModelCatalogConfig, ModelConfig, ModelType, SandboxConfig, ServerConfig,
    build_server_config, default_api_key_env, default_chat_completions_path,
    default_context_window_tokens, default_cron_poll_interval_seconds,
    default_max_global_sub_agents, default_model_timeout_seconds,
};
use crate::backend::AgentBackendKind;
use agent_frame::config::{AuthCredentialsStoreMode, ExternalWebSearchConfig, NativeWebSearchConfig, ReasoningConfig};
use anyhow::{Context, Result};
use serde::Deserialize;
use serde_json::{Map, Value};
use std::collections::BTreeMap;

pub(super) struct LegacyConfigLoader;

#[derive(Clone, Debug, Deserialize)]
struct LegacyModelConfigRaw {
    pub api_endpoint: String,
    pub model: String,
    #[serde(default)]
    pub backend: AgentBackendKind,
    #[serde(default)]
    pub supports_vision_input: bool,
    #[serde(default)]
    pub image_tool_model: Option<String>,
    #[serde(default)]
    pub api_key: Option<String>,
    #[serde(default = "default_api_key_env")]
    pub api_key_env: String,
    #[serde(default = "default_chat_completions_path")]
    pub chat_completions_path: String,
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
    #[serde(default)]
    pub native_web_search: Option<NativeWebSearchConfig>,
    #[allow(dead_code)]
    #[serde(default)]
    pub external_web_search: Option<ExternalWebSearchConfig>,
}

#[derive(Clone, Debug, Deserialize)]
struct LegacyServerConfigRaw {
    pub models: BTreeMap<String, LegacyModelConfigRaw>,
    pub main_agent: MainAgentConfig,
    #[serde(default)]
    pub sandbox: SandboxConfig,
    #[serde(default = "default_max_global_sub_agents")]
    pub max_global_sub_agents: usize,
    #[serde(default = "default_cron_poll_interval_seconds")]
    pub cron_poll_interval_seconds: u64,
    pub channels: Vec<ChannelConfig>,
}

impl ConfigLoader for LegacyConfigLoader {
    fn version(&self) -> &'static str {
        LEGACY_CONFIG_VERSION
    }

    fn load_and_upgrade(&self, value: Value) -> Result<ServerConfig> {
        let raw: LegacyServerConfigRaw =
            serde_json::from_value(value).context("failed to parse legacy server config")?;
        let chat_model_keys = raw.models.keys().cloned().collect::<Vec<_>>();
        let models = raw
            .models
            .into_iter()
            .map(|(name, model)| {
                let upgraded = ModelConfig {
                    model_type: ModelType::Openrouter,
                    api_endpoint: model.api_endpoint,
                    model: model.model,
                    backend: model.backend,
                    supports_vision_input: model.supports_vision_input,
                    image_tool_model: model.image_tool_model,
                    web_search_model: None,
                    api_key: model.api_key,
                    api_key_env: model.api_key_env,
                    chat_completions_path: model.chat_completions_path,
                    codex_home: None,
                    auth_credentials_store_mode: AuthCredentialsStoreMode::Auto,
                    timeout_seconds: model.timeout_seconds,
                    retry_mode: Default::default(),
                    context_window_tokens: model.context_window_tokens,
                    cache_ttl: model.cache_ttl,
                    reasoning: model.reasoning,
                    headers: model.headers,
                    description: model.description,
                    agent_model_enabled: true,
                    capabilities: Vec::new(),
                    native_web_search: model.native_web_search,
                    token_estimation: None,
                };
                Ok((name, upgraded))
            })
            .collect::<Result<BTreeMap<_, _>>>()?;
        let model_catalog = ModelCatalogConfig {
            chat: models.clone(),
            vision: BTreeMap::new(),
            web_search: BTreeMap::new(),
        };
        Ok(build_server_config(
            LATEST_CONFIG_VERSION.to_string(),
            models,
            AgentConfig::default(),
            model_catalog,
            super::ToolingConfig::default(),
            chat_model_keys,
            raw.main_agent,
            raw.sandbox,
            raw.max_global_sub_agents,
            raw.cron_poll_interval_seconds,
            raw.channels,
        ))
    }
}
