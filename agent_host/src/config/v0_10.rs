use super::{
    AgentConfig, ChannelConfig, ConfigLoader, LATEST_CONFIG_VERSION, MainAgentConfig,
    ModelCapability, ModelCatalogConfig, ModelConfig, ModelType, SandboxConfig, ServerConfig,
    ToolingConfig, build_server_config, default_agent_model_enabled, default_api_key_env,
    default_chat_completions_path, default_codex_subscription_endpoint,
    default_context_window_tokens, default_cron_poll_interval_seconds,
    default_max_global_sub_agents, default_model_timeout_seconds, default_responses_path,
};
use crate::backend::AgentBackendKind;
use agent_frame::config::{AuthCredentialsStoreMode, NativeWebSearchConfig, ReasoningConfig};
use anyhow::{Context, Result, anyhow};
use serde::Deserialize;
use serde_json::{Map, Value};
use std::collections::BTreeMap;

pub(super) struct LatestConfigLoader;

#[derive(Clone, Debug, Deserialize)]
struct VersionedServerConfigRaw {
    pub version: String,
    pub models: BTreeMap<String, VersionedModelConfigRaw>,
    #[serde(default)]
    pub agent: AgentConfig,
    #[serde(default)]
    pub tooling: ToolingConfig,
    pub main_agent: MainAgentConfig,
    #[serde(default)]
    pub sandbox: SandboxConfig,
    #[serde(default = "default_max_global_sub_agents")]
    pub max_global_sub_agents: usize,
    #[serde(default = "default_cron_poll_interval_seconds")]
    pub cron_poll_interval_seconds: u64,
    pub channels: Vec<ChannelConfig>,
}

#[derive(Clone, Debug, Deserialize)]
struct VersionedModelConfigRaw {
    #[serde(rename = "type")]
    pub model_type: ModelType,
    pub model: String,
    #[serde(default)]
    pub api_endpoint: Option<String>,
    #[serde(default)]
    pub backend: Option<AgentBackendKind>,
    #[serde(default)]
    pub capabilities: Vec<ModelCapability>,
    #[serde(default)]
    pub supports_vision_input: bool,
    #[serde(default)]
    pub image_tool_model: Option<String>,
    #[serde(default, alias = "web_search_model")]
    pub web_search: Option<String>,
    #[serde(default)]
    pub api_key: Option<String>,
    #[serde(default = "default_api_key_env")]
    pub api_key_env: String,
    #[serde(default)]
    pub chat_completions_path: Option<String>,
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
    pub native_web_search: Option<NativeWebSearchConfig>,
}

impl ConfigLoader for LatestConfigLoader {
    fn version(&self) -> &'static str {
        LATEST_CONFIG_VERSION
    }

    fn load_and_upgrade(&self, value: Value) -> Result<ServerConfig> {
        let raw: VersionedServerConfigRaw =
            serde_json::from_value(value).context("failed to parse latest server config")?;
        if raw.version != LATEST_CONFIG_VERSION {
            return Err(anyhow!(
                "latest config loader expected version '{}' but received '{}'",
                LATEST_CONFIG_VERSION,
                raw.version
            ));
        }

        let models = raw
            .models
            .into_iter()
            .map(|(name, raw_model)| Ok((name, upgrade_versioned_model(raw_model))))
            .collect::<Result<BTreeMap<_, _>>>()?;

        Ok(build_server_config(
            LATEST_CONFIG_VERSION.to_string(),
            models,
            raw.agent,
            ModelCatalogConfig::default(),
            raw.tooling,
            Vec::new(),
            raw.main_agent,
            raw.sandbox,
            raw.max_global_sub_agents,
            raw.cron_poll_interval_seconds,
            raw.channels,
        ))
    }
}

fn upgrade_versioned_model(raw: VersionedModelConfigRaw) -> ModelConfig {
    let api_endpoint = raw.api_endpoint.unwrap_or_else(|| match raw.model_type {
        ModelType::Openrouter | ModelType::OpenrouterResp => {
            "https://openrouter.ai/api/v1".to_string()
        }
        ModelType::CodexSubscription => default_codex_subscription_endpoint(),
    });
    let chat_completions_path = raw
        .chat_completions_path
        .unwrap_or_else(|| match raw.model_type {
            ModelType::Openrouter => default_chat_completions_path(),
            ModelType::OpenrouterResp | ModelType::CodexSubscription => default_responses_path(),
        });

    ModelConfig {
        model_type: raw.model_type,
        api_endpoint,
        model: raw.model,
        backend: raw.backend.unwrap_or(AgentBackendKind::AgentFrame),
        supports_vision_input: raw.supports_vision_input,
        image_tool_model: raw.image_tool_model,
        web_search_model: raw.web_search,
        api_key: raw.api_key,
        api_key_env: raw.api_key_env,
        chat_completions_path,
        codex_home: raw.codex_home,
        auth_credentials_store_mode: raw.auth_credentials_store_mode,
        timeout_seconds: raw.timeout_seconds,
        context_window_tokens: raw.context_window_tokens,
        cache_ttl: raw.cache_ttl,
        reasoning: raw.reasoning,
        headers: raw.headers,
        description: raw.description,
        agent_model_enabled: raw.agent_model_enabled,
        capabilities: raw.capabilities,
        native_web_search: raw.native_web_search,
        external_web_search: None,
    }
}
