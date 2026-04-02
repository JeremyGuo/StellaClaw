use crate::backend::AgentBackendKind;
use agent_frame::config::{ExternalWebSearchConfig, NativeWebSearchConfig, ReasoningConfig};
use anyhow::{Context, Result, anyhow};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use std::collections::BTreeMap;
use std::fs;
use std::path::Path;

fn zgent_checkout_available() -> bool {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../zgent/crates/zgent-core/Cargo.toml")
        .is_file()
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
    #[serde(default)]
    pub external_web_search: Option<ExternalWebSearchConfig>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct MainAgentConfig {
    pub model: String,
    #[serde(default)]
    pub timeout_seconds: Option<f64>,
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
    pub models: BTreeMap<String, ModelConfig>,
    pub main_agent: MainAgentConfig,
    #[serde(default)]
    pub sandbox: SandboxConfig,
    #[serde(default = "default_max_global_sub_agents")]
    pub max_global_sub_agents: usize,
    #[serde(default = "default_cron_poll_interval_seconds")]
    pub cron_poll_interval_seconds: u64,
    pub channels: Vec<ChannelConfig>,
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

fn default_api_key_env() -> String {
    "OPENAI_API_KEY".to_string()
}

fn default_chat_completions_path() -> String {
    "/chat/completions".to_string()
}

fn default_model_timeout_seconds() -> f64 {
    120.0
}

fn default_context_window_tokens() -> usize {
    128_000
}

fn default_main_agent_language() -> String {
    "zh-CN".to_string()
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
        "download_file".to_string(),
        "web_fetch".to_string(),
        "web_search".to_string(),
        "image".to_string(),
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
            command: "model".to_string(),
            description: "Show or set the conversation model".to_string(),
        },
        BotCommandConfig {
            command: "sandbox".to_string(),
            description: "Show or set the conversation sandbox mode".to_string(),
        },
        BotCommandConfig {
            command: "set_api_timeout".to_string(),
            description: "Set session API timeout in seconds".to_string(),
        },
        BotCommandConfig {
            command: "snap_save".to_string(),
            description: "Save a named global snapshot".to_string(),
        },
        BotCommandConfig {
            command: "snap_load".to_string(),
            description: "Load a named global snapshot".to_string(),
        },
        BotCommandConfig {
            command: "snap_list".to_string(),
            description: "List saved global snapshots".to_string(),
        },
    ]
}

fn default_telegram_commands() -> Vec<BotCommandConfig> {
    default_bot_commands()
}

fn default_max_global_sub_agents() -> usize {
    4
}

fn default_bubblewrap_binary() -> String {
    "bwrap".to_string()
}

fn default_cron_poll_interval_seconds() -> u64 {
    5
}

pub fn load_server_config_file(path: impl AsRef<Path>) -> Result<ServerConfig> {
    let path = path.as_ref();
    let raw = fs::read_to_string(path)
        .with_context(|| format!("failed to read config file {}", path.display()))?;
    let config: ServerConfig =
        serde_json::from_str(&raw).context("failed to parse server config JSON")?;
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
    if !config.models.contains_key(&config.main_agent.model) {
        return Err(anyhow!(
            "main_agent.model '{}' does not exist in models",
            config.main_agent.model
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
    }
    Ok(config)
}

pub fn resolve_model_api_keys(config: &ServerConfig) -> Vec<ResolvedModelApiKey> {
    config
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
        .collect()
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
        ChannelConfig, default_bot_commands, load_server_config_file, resolve_model_api_keys,
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
                assert_eq!(telegram.commands, default_bot_commands());
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
}
