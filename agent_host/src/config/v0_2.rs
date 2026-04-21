use super::{
    AgentConfig, ChannelConfig, ConfigLoader, MainAgentConfig, ModelCatalogConfig, ModelConfig,
    ModelType, SandboxConfig, ServerConfig, VERSION_0_2, build_server_config, default_api_key_env,
    default_chat_completions_path, default_codex_subscription_endpoint,
    default_context_window_tokens, default_cron_poll_interval_seconds,
    default_max_global_sub_agents, default_model_timeout_seconds, default_responses_path,
};
use crate::backend::AgentBackendKind;
use agent_frame::config::{
    AuthCredentialsStoreMode, ExternalWebSearchConfig, NativeWebSearchConfig, ReasoningConfig,
};
use anyhow::{Context, Result, anyhow};
use serde::Deserialize;
use serde_json::{Map, Value};
use std::collections::BTreeMap;

pub(super) struct VersionedConfigLoader;

#[derive(Clone, Debug, Deserialize)]
struct VersionedServerConfigRaw {
    pub version: String,
    pub models: VersionedModelCatalogRaw,
    pub main_agent: MainAgentConfig,
    #[serde(default)]
    pub sandbox: SandboxConfig,
    #[serde(default = "default_max_global_sub_agents")]
    pub max_global_sub_agents: usize,
    #[serde(default = "default_cron_poll_interval_seconds")]
    pub cron_poll_interval_seconds: u64,
    pub channels: Vec<ChannelConfig>,
}

#[derive(Clone, Debug, Default, Deserialize)]
struct VersionedModelCatalogRaw {
    #[serde(default)]
    pub chat: BTreeMap<String, VersionedModelConfigRaw>,
    #[serde(default)]
    pub vision: BTreeMap<String, VersionedModelConfigRaw>,
    #[serde(default)]
    pub web_search: BTreeMap<String, VersionedModelConfigRaw>,
}

#[derive(Clone, Debug, Deserialize)]
struct VersionedModelConfigRaw {
    #[serde(rename = "type")]
    pub model_type: ModelType,
    pub model: String,
    #[serde(default)]
    pub api_endpoint: Option<String>,
    #[serde(default)]
    pub backend: AgentBackendKind,
    #[serde(default)]
    pub supports_vision_input: bool,
    #[serde(default)]
    pub image_tool_model: Option<String>,
    #[serde(default)]
    pub web_search_model: Option<String>,
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
    #[serde(default)]
    pub native_web_search: Option<NativeWebSearchConfig>,
    #[allow(dead_code)]
    #[serde(default)]
    pub external_web_search: Option<ExternalWebSearchConfig>,
}

impl ConfigLoader for VersionedConfigLoader {
    fn version(&self) -> &'static str {
        VERSION_0_2
    }

    fn load_and_upgrade(&self, value: Value) -> Result<ServerConfig> {
        let raw: VersionedServerConfigRaw =
            serde_json::from_value(value).context("failed to parse versioned server config")?;
        if raw.version != VERSION_0_2 {
            return Err(anyhow!(
                "versioned config loader expected version '{}' but received '{}'",
                VERSION_0_2,
                raw.version
            ));
        }

        let chat_model_keys = raw.models.chat.keys().cloned().collect::<Vec<_>>();
        let mut web_search_catalog = raw
            .models
            .web_search
            .into_iter()
            .map(|(name, raw_model)| {
                upgrade_external_web_search_model(raw_model).map(|cfg| (name, cfg))
            })
            .collect::<Result<BTreeMap<_, _>>>()?;

        let mut dedup_index = web_search_catalog
            .iter()
            .map(|(name, cfg)| Ok((search_fingerprint(cfg)?, name.clone())))
            .collect::<Result<BTreeMap<_, _>>>()?;

        let chat = raw
            .models
            .chat
            .into_iter()
            .map(|(name, raw_model)| {
                let model = upgrade_chat_or_vision_model(
                    &name,
                    raw_model,
                    &mut web_search_catalog,
                    &mut dedup_index,
                )?;
                Ok((name, model))
            })
            .collect::<Result<BTreeMap<_, _>>>()?;

        let vision = raw
            .models
            .vision
            .into_iter()
            .map(|(name, raw_model)| {
                let model = upgrade_chat_or_vision_model(
                    &name,
                    raw_model,
                    &mut web_search_catalog,
                    &mut dedup_index,
                )?;
                Ok((name, model))
            })
            .collect::<Result<BTreeMap<_, _>>>()?;

        let model_catalog = ModelCatalogConfig {
            chat: chat.clone(),
            vision: vision.clone(),
            web_search: web_search_catalog,
        };

        let mut models = chat;
        for (name, model) in vision {
            if models.insert(name.clone(), model).is_some() {
                return Err(anyhow!(
                    "duplicate model name '{}' across model catalogs",
                    name
                ));
            }
        }

        Ok(build_server_config(
            super::LATEST_CONFIG_VERSION.to_string(),
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

fn upgrade_chat_or_vision_model(
    _model_name: &str,
    raw: VersionedModelConfigRaw,
    _web_search_catalog: &mut BTreeMap<String, ExternalWebSearchConfig>,
    _dedup_index: &mut BTreeMap<String, String>,
) -> Result<ModelConfig> {
    let preferred_alias = raw.web_search_model.clone();
    let mut model = upgrade_base_model(raw);

    if let Some(alias) = preferred_alias {
        model.web_search_model = Some(alias);
    }

    Ok(model)
}

fn upgrade_base_model(raw: VersionedModelConfigRaw) -> ModelConfig {
    let api_endpoint = raw.api_endpoint.unwrap_or_else(|| match raw.model_type {
        ModelType::Openrouter | ModelType::OpenrouterResp => {
            "https://openrouter.ai/api/v1".to_string()
        }
        ModelType::CodexSubscription => default_codex_subscription_endpoint(),
        ModelType::ClaudeCode => "https://api.anthropic.com/v1".to_string(),
    });
    let chat_completions_path = raw
        .chat_completions_path
        .unwrap_or_else(|| match raw.model_type {
            ModelType::Openrouter => default_chat_completions_path(),
            ModelType::OpenrouterResp | ModelType::CodexSubscription => default_responses_path(),
            ModelType::ClaudeCode => "/messages".to_string(),
        });
    ModelConfig {
        model_type: raw.model_type,
        api_endpoint,
        model: raw.model,
        backend: raw.backend,
        supports_vision_input: raw.supports_vision_input,
        image_tool_model: raw.image_tool_model,
        web_search_model: raw.web_search_model,
        api_key: raw.api_key,
        api_key_env: raw.api_key_env,
        chat_completions_path,
        codex_home: raw.codex_home,
        auth_credentials_store_mode: raw.auth_credentials_store_mode,
        timeout_seconds: raw.timeout_seconds,
        retry_mode: Default::default(),
        context_window_tokens: raw.context_window_tokens,
        cache_ttl: raw.cache_ttl,
        reasoning: raw.reasoning,
        headers: raw.headers,
        description: raw.description,
        agent_model_enabled: true,
        capabilities: Vec::new(),
        native_web_search: raw.native_web_search,
        token_estimation: None,
    }
}

fn upgrade_external_web_search_model(
    raw: VersionedModelConfigRaw,
) -> Result<ExternalWebSearchConfig> {
    Ok(ExternalWebSearchConfig {
        base_url: raw
            .api_endpoint
            .unwrap_or_else(|| "https://openrouter.ai/api/v1".to_string()),
        model: raw.model,
        supports_vision_input: false,
        api_key: raw.api_key,
        api_key_env: raw.api_key_env,
        chat_completions_path: raw
            .chat_completions_path
            .unwrap_or_else(|| match raw.model_type {
                ModelType::Openrouter => default_chat_completions_path(),
                ModelType::OpenrouterResp | ModelType::CodexSubscription => {
                    default_responses_path()
                }
                ModelType::ClaudeCode => "/messages".to_string(),
            }),
        timeout_seconds: raw.timeout_seconds,
        headers: raw.headers,
    })
}

fn search_fingerprint(config: &ExternalWebSearchConfig) -> Result<String> {
    serde_json::to_string(config).context("failed to fingerprint web_search config")
}
