use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
};

use serde::{Deserialize, Serialize};
use stellaclaw_core::model_config::ModelConfig;

pub mod loaders;

pub const LEGACY_CONFIG_VERSION: &str = "0.1";
pub const LATEST_CONFIG_VERSION: &str = "0.2";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StellaclawConfig {
    #[serde(default = "default_version")]
    pub version: String,
    #[serde(default)]
    pub agent_server: AgentServerConfig,
    pub default_profile: SessionProfile,
    #[serde(default)]
    pub named_models: BTreeMap<String, ModelConfig>,
    #[serde(default)]
    pub session_defaults: SessionDefaults,
    #[serde(default)]
    pub sandbox: SandboxConfig,
    pub channels: Vec<ChannelConfig>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AgentServerConfig {
    #[serde(default)]
    pub path: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionProfile {
    pub main_model: ModelConfig,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SessionDefaults {
    #[serde(default)]
    pub compression_threshold_tokens: Option<u64>,
    #[serde(default)]
    pub compression_retain_recent_tokens: Option<u64>,
    #[serde(default)]
    pub image_tool_model: Option<ModelConfig>,
    #[serde(default)]
    pub pdf_tool_model: Option<ModelConfig>,
    #[serde(default)]
    pub audio_tool_model: Option<ModelConfig>,
    #[serde(default)]
    pub image_generation_tool_model: Option<ModelConfig>,
    #[serde(default)]
    pub search_tool_model: Option<ModelConfig>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SandboxMode {
    Subprocess,
    Bubblewrap,
}

impl Default for SandboxMode {
    fn default() -> Self {
        Self::Subprocess
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SandboxConfig {
    #[serde(default)]
    pub mode: SandboxMode,
    #[serde(default = "default_bubblewrap_binary")]
    pub bubblewrap_binary: String,
}

impl Default for SandboxConfig {
    fn default() -> Self {
        Self {
            mode: SandboxMode::Subprocess,
            bubblewrap_binary: default_bubblewrap_binary(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ChannelConfig {
    Telegram(TelegramChannelConfig),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
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
    #[serde(default)]
    pub admin_user_ids: Vec<i64>,
}

impl StellaclawConfig {
    pub fn validate(&self) -> Result<(), String> {
        if self.version != LATEST_CONFIG_VERSION {
            return Err(format!(
                "unsupported config version {}; expected {}",
                self.version, LATEST_CONFIG_VERSION
            ));
        }
        if self.channels.is_empty() {
            return Err("config must include at least one channel".to_string());
        }
        Ok(())
    }

    pub fn resolve_agent_server_path(&self, config_path: &Path) -> PathBuf {
        if let Some(path) = self.agent_server.path.as_deref() {
            let path = PathBuf::from(path);
            if path.is_absolute() {
                return path;
            }
            return config_path
                .parent()
                .unwrap_or_else(|| Path::new("."))
                .join(path);
        }

        if let Ok(path) = std::env::var("STELLACLAW_AGENT_SERVER_PATH") {
            return PathBuf::from(path);
        }

        let executable_name = if cfg!(windows) {
            "agent_server.exe"
        } else {
            "agent_server"
        };
        if let Ok(current_exe) = std::env::current_exe() {
            if let Some(parent) = current_exe.parent() {
                return parent.join(executable_name);
            }
        }

        PathBuf::from(executable_name)
    }

    pub fn resolve_named_model(&self, name: &str) -> Option<ModelConfig> {
        self.named_models.get(name).cloned()
    }
}

impl TelegramChannelConfig {
    pub fn resolve_bot_token(&self) -> Result<String, String> {
        match self.bot_token.as_deref() {
            Some(token) if !token.trim().is_empty() => Ok(token.to_string()),
            _ => std::env::var(&self.bot_token_env).map_err(|_| {
                format!(
                    "telegram channel {} requires bot_token or env {}",
                    self.id, self.bot_token_env
                )
            }),
        }
    }
}

fn default_version() -> String {
    LATEST_CONFIG_VERSION.to_string()
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

fn default_bubblewrap_binary() -> String {
    "bwrap".to_string()
}
