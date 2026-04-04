use crate::backend::AgentBackendKind;
use agent_frame::config::{
    AuthCredentialsStoreMode, ExternalWebSearchConfig, NativeWebSearchConfig, ReasoningConfig,
    UpstreamApiKind, UpstreamAuthKind,
};
use anyhow::{Context, Result, anyhow};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use std::collections::BTreeMap;
use std::fs;
use std::path::Path;

mod v0_1;
mod v0_2;
mod v0_3;

pub const LEGACY_CONFIG_VERSION: &str = "0.1";
pub const LATEST_CONFIG_VERSION: &str = "0.3";
pub const VERSION_0_2: &str = "0.2";

fn zgent_checkout_available() -> bool {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../zgent/crates/zgent-core/Cargo.toml")
        .is_file()
}

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
pub struct ModelConfig {
    #[serde(rename = "type")]
    pub model_type: ModelType,
    pub api_endpoint: String,
    pub model: String,
    #[serde(default)]
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
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum ModelType {
    Openrouter,
    OpenrouterResp,
    CodexSubscription,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
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
    #[serde(default = "default_effective_context_window_percent")]
    pub effective_context_window_percent: f64,
    #[serde(default)]
    pub auto_compact_token_limit: Option<usize>,
    #[serde(default = "default_retain_recent_messages")]
    pub retain_recent_messages: usize,
    #[serde(default)]
    pub enable_idle_context_compaction: bool,
    #[serde(default = "default_idle_context_compaction_poll_interval_seconds")]
    pub idle_context_compaction_poll_interval_seconds: u64,
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
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ServerConfig {
    pub version: String,
    pub models: BTreeMap<String, ModelConfig>,
    pub model_catalog: ModelCatalogConfig,
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

pub fn default_enabled_tools() -> Vec<String> {
    vec![
        "read_file".to_string(),
        "write_file".to_string(),
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

fn default_effective_context_window_percent() -> f64 {
    0.9
}

fn default_retain_recent_messages() -> usize {
    8
}

fn default_idle_context_compaction_poll_interval_seconds() -> u64 {
    15
}

fn default_telegram_bot_token_env() -> String {
    "TELEGRAM_BOT_TOKEN".to_string()
}

fn default_telegram_api_base_url() -> String {
    "https://api.telegram.org".to_string()
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
            command: "new".to_string(),
            description: "Start a new session".to_string(),
        },
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
            command: "model".to_string(),
            description: "Show or set the conversation model".to_string(),
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

fn default_telegram_commands() -> Vec<BotCommandConfig> {
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

pub(crate) fn default_max_global_sub_agents() -> usize {
    4
}

fn default_bubblewrap_binary() -> String {
    "bwrap".to_string()
}

pub(crate) fn default_cron_poll_interval_seconds() -> u64 {
    5
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
    let loaders: [&dyn ConfigLoader; 3] = [
        &v0_1::LegacyConfigLoader,
        &v0_2::VersionedConfigLoader,
        &v0_3::LatestConfigLoader,
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
    models: BTreeMap<String, ModelConfig>,
    model_catalog: ModelCatalogConfig,
    chat_model_keys: Vec<String>,
    main_agent: MainAgentConfig,
    sandbox: SandboxConfig,
    max_global_sub_agents: usize,
    cron_poll_interval_seconds: u64,
    channels: Vec<ChannelConfig>,
) -> ServerConfig {
    ServerConfig {
        version,
        models,
        model_catalog,
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
        let loaders: [&dyn ConfigLoader; 3] = [
            &v0_1::LegacyConfigLoader,
            &v0_2::VersionedConfigLoader,
            &v0_3::LatestConfigLoader,
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
    struct PersistedServerConfig<'a> {
        version: &'a str,
        models: &'a ModelCatalogConfig,
        main_agent: &'a MainAgentConfig,
        sandbox: &'a SandboxConfig,
        max_global_sub_agents: usize,
        cron_poll_interval_seconds: u64,
        channels: &'a [ChannelConfig],
    }

    let persisted = PersistedServerConfig {
        version: LATEST_CONFIG_VERSION,
        models: &config.model_catalog,
        main_agent: &config.main_agent,
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
        if !cfg!(target_os = "linux") {
            return Err(anyhow!(
                "sandbox mode 'bubblewrap' requires Linux with bubblewrap installed"
            ));
        }
        if config.sandbox.bubblewrap_binary.trim().is_empty() {
            return Err(anyhow!("sandbox.bubblewrap_binary must not be empty"));
        }
    }
    if config.main_agent.enable_idle_context_compaction
        && config
            .main_agent
            .idle_context_compaction_poll_interval_seconds
            == 0
    {
        return Err(anyhow!(
            "main_agent.idle_context_compaction_poll_interval_seconds must be at least 1"
        ));
    }
    if let Some(model) = config.main_agent.model.as_deref() {
        if !config.models.contains_key(model) {
            return Err(anyhow!(
                "main_agent.model '{}' does not exist in models",
                model
            ));
        }
        if !config.chat_model_keys.iter().any(|value| value == model) {
            return Err(anyhow!(
                "main_agent.model '{}' must reference a chat model",
                model
            ));
        }
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
    for channel in &config.channels {
        if let ChannelConfig::Telegram(telegram) = channel {
            validate_bot_commands(&telegram.commands).with_context(|| {
                format!("invalid telegram commands for channel {}", telegram.id)
            })?;
        }
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
        if model.backend == AgentBackendKind::Zgent {
            if !zgent_checkout_available() {
                return Err(anyhow!(
                    "model '{}' uses zgent backend but the local zgent checkout is unavailable",
                    model_name
                ));
            }
            if model.chat_completions_path != default_chat_completions_path() {
                return Err(anyhow!(
                    "model '{}' uses zgent backend but chat_completions_path must be '{}'",
                    model_name,
                    default_chat_completions_path()
                ));
            }
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
    Ok(())
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

fn validate_bot_commands(commands: &[BotCommandConfig]) -> Result<()> {
    if commands.len() > 100 {
        return Err(anyhow!("telegram supports at most 100 commands"));
    }

    for command in commands {
        if command.command.is_empty() || command.command.len() > 32 {
            return Err(anyhow!(
                "command '{}' must be 1-32 characters",
                command.command
            ));
        }
        if !command
            .command
            .chars()
            .all(|ch| ch.is_ascii_lowercase() || ch.is_ascii_digit() || ch == '_')
        {
            return Err(anyhow!(
                "command '{}' must use only lowercase letters, digits, and underscores",
                command.command
            ));
        }
        if command.description.trim().is_empty() || command.description.len() > 256 {
            return Err(anyhow!(
                "description for command '{}' must be 1-256 characters",
                command.command
            ));
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{
        ChannelConfig, LATEST_CONFIG_VERSION, ModelType, default_telegram_commands,
        load_server_config_file, load_server_config_file_and_upgrade, resolve_model_api_keys,
        zgent_checkout_available,
    };
    use crate::backend::AgentBackendKind;
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
                .contains("idle_context_compaction_poll_interval_seconds")
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
        if zgent_checkout_available() {
            assert!(error.to_string().contains("chat_completions_path"));
        } else {
            assert!(
                error
                    .to_string()
                    .contains("local zgent checkout is unavailable")
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
        assert!(written.contains("\"version\": \"0.3\""));
        assert!(written.contains("\"web_search\": \"main_web_search\""));
        assert!(written.contains("\"models\": {"));
        assert!(written.contains("\"web_search\": {"));
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
}
