use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
};

use serde::{Deserialize, Serialize};
use stellaclaw_core::model_config::{ModelCapability, ModelConfig};

pub mod loaders;

pub const LEGACY_CONFIG_VERSION: &str = "0.1";
pub const CONFIG_VERSION_0_2: &str = "0.2";
pub const CONFIG_VERSION_0_3: &str = "0.3";
pub const CONFIG_VERSION_0_4: &str = "0.4";
pub const CONFIG_VERSION_0_5: &str = "0.5";
pub const CONFIG_VERSION_0_6: &str = "0.6";
pub const CONFIG_VERSION_0_7: &str = "0.7";
pub const CONFIG_VERSION_0_8: &str = "0.8";
pub const CONFIG_VERSION_0_9: &str = "0.9";
pub const CONFIG_VERSION_0_10: &str = "0.10";
pub const LATEST_CONFIG_VERSION: &str = "0.11";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StellaclawConfig {
    #[serde(default = "default_version")]
    pub version: String,
    #[serde(default)]
    pub agent_server: AgentServerConfig,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_profile: Option<SessionProfile>,
    #[serde(default, alias = "named_models")]
    pub models: BTreeMap<String, ModelConfig>,
    #[serde(default)]
    pub available_agent_models: Vec<String>,
    #[serde(default)]
    pub session_defaults: SessionDefaults,
    #[serde(default)]
    pub skill_sync: Vec<SkillSyncConfig>,
    #[serde(default)]
    pub sandbox: SandboxConfig,
    pub channels: Vec<ChannelConfig>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AgentServerConfig {
    #[serde(default)]
    pub path: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SkillSyncConfig {
    #[serde(default)]
    pub skill_name: Vec<String>,
    #[serde(default)]
    pub upstream: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionProfile {
    pub main_model: ModelSelection,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum ModelSelection {
    Alias(String),
    Inline(ModelConfig),
}

impl ModelSelection {
    pub fn alias(alias: impl Into<String>) -> Self {
        Self::Alias(alias.into())
    }

    pub fn resolve(&self, models: &BTreeMap<String, ModelConfig>) -> Option<ModelConfig> {
        match self {
            Self::Alias(alias) => models.get(alias).cloned(),
            Self::Inline(model) => Some(model.clone()),
        }
    }

    pub fn display_name(&self, models: &BTreeMap<String, ModelConfig>) -> String {
        match self {
            Self::Alias(alias) => models
                .get(alias)
                .map(|model| model.model_name.clone())
                .unwrap_or_else(|| alias.clone()),
            Self::Inline(model) => model.model_name.clone(),
        }
    }

    pub fn alias_name(&self) -> Option<&str> {
        match self {
            Self::Alias(alias) => Some(alias),
            Self::Inline(_) => None,
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SessionDefaults {
    #[serde(default)]
    pub compression_threshold_tokens: Option<u64>,
    #[serde(default)]
    pub compression_retain_recent_tokens: Option<u64>,
    #[serde(default)]
    pub image_tool_model: Option<ToolModelTarget>,
    #[serde(default)]
    pub pdf_tool_model: Option<ToolModelTarget>,
    #[serde(default)]
    pub audio_tool_model: Option<ToolModelTarget>,
    #[serde(default)]
    pub image_generation_tool_model: Option<ToolModelTarget>,
    #[serde(default)]
    pub search_tool_model: Option<ToolModelTarget>,
    #[serde(default)]
    pub search_image_tool_model: Option<ToolModelTarget>,
    #[serde(default)]
    pub search_video_tool_model: Option<ToolModelTarget>,
    #[serde(default)]
    pub search_news_tool_model: Option<ToolModelTarget>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum ToolModelTarget {
    Alias(String),
    Inline(ModelConfig),
}

impl ToolModelTarget {
    pub fn inline(model_config: ModelConfig) -> Self {
        Self::Inline(model_config)
    }

    pub fn resolve(
        &self,
        models: &BTreeMap<String, ModelConfig>,
        session_model: &ModelConfig,
    ) -> Result<ModelConfig, String> {
        match self {
            Self::Inline(model_config) => Ok(model_config.clone()),
            Self::Alias(raw) => {
                let (alias, prefer_self) = parse_tool_model_target(raw)?;
                if prefer_self {
                    return Ok(session_model.clone());
                }
                models
                    .get(alias)
                    .cloned()
                    .ok_or_else(|| format!("unknown tool model alias '{}'", alias))
            }
        }
    }
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub software_dir: Option<String>,
    #[serde(
        default = "default_software_mount_path",
        skip_serializing_if = "is_default_software_mount_path"
    )]
    pub software_mount_path: String,
}

impl Default for SandboxConfig {
    fn default() -> Self {
        Self {
            mode: SandboxMode::Subprocess,
            bubblewrap_binary: default_bubblewrap_binary(),
            software_dir: None,
            software_mount_path: default_software_mount_path(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ChannelConfig {
    Telegram(TelegramChannelConfig),
    Web(WebChannelConfig),
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

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WebChannelConfig {
    pub id: String,
    #[serde(default = "default_web_bind_addr")]
    pub bind_addr: String,
    #[serde(default = "default_web_token_env")]
    pub token_env: String,
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
        if self.models.is_empty() {
            return Err("config must include at least one model".to_string());
        }
        self.validate_available_agent_models()?;
        if self.available_agent_models().is_empty() {
            return Err("config must include at least one chat-capable model".to_string());
        }
        self.sandbox.validate()?;
        for channel in &self.channels {
            if let ChannelConfig::Web(channel) = channel {
                channel.validate()?;
            }
        }
        validate_skill_sync(&self.skill_sync)?;
        Ok(())
    }

    fn validate_available_agent_models(&self) -> Result<(), String> {
        for alias in &self.available_agent_models {
            let model = self.models.get(alias).ok_or_else(|| {
                format!("available_agent_models references unknown model {alias}")
            })?;
            if !model.supports(ModelCapability::Chat) {
                return Err(format!(
                    "available_agent_models entry {alias} is not chat-capable"
                ));
            }
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
        self.models.get(name).cloned()
    }

    pub fn is_available_agent_model(&self, name: &str) -> bool {
        self.available_agent_models()
            .iter()
            .any(|(alias, _model)| alias.as_str() == name)
    }

    pub fn available_agent_models(&self) -> Vec<(&String, &ModelConfig)> {
        if self.available_agent_models.is_empty() {
            return self
                .models
                .iter()
                .filter(|(_, model)| model.supports(ModelCapability::Chat))
                .collect();
        }
        self.available_agent_models
            .iter()
            .filter_map(|alias| {
                self.models
                    .get_key_value(alias)
                    .filter(|(_, model)| model.supports(ModelCapability::Chat))
            })
            .collect()
    }

    pub fn resolve_session_model(&self, profile: &SessionProfile) -> Option<ModelConfig> {
        profile.main_model.resolve(&self.models)
    }

    pub fn initial_session_profile(&self) -> Result<SessionProfile, String> {
        self.initial_main_model_name()
            .map(|main_model| SessionProfile {
                main_model: ModelSelection::alias(main_model),
            })
            .ok_or_else(|| "config must include at least one chat-capable model".to_string())
    }

    pub fn initial_main_model_name(&self) -> Option<String> {
        self.default_profile
            .as_ref()
            .and_then(|profile| self.model_selection_alias(&profile.main_model))
            .or_else(|| {
                self.available_agent_models()
                    .into_iter()
                    .next()
                    .map(|(name, _model)| name.clone())
            })
    }

    pub fn initial_main_model(&self) -> Option<ModelConfig> {
        self.default_profile
            .as_ref()
            .and_then(|profile| self.resolve_session_model(profile))
            .or_else(|| {
                self.available_agent_models()
                    .into_iter()
                    .next()
                    .map(|(_name, model)| model.clone())
            })
    }

    fn model_selection_alias(&self, selection: &ModelSelection) -> Option<String> {
        match selection {
            ModelSelection::Alias(alias) if self.models.contains_key(alias) => Some(alias.clone()),
            ModelSelection::Alias(alias) => self
                .models
                .iter()
                .find_map(|(name, model)| (model.model_name == *alias).then(|| name.clone())),
            ModelSelection::Inline(target) => self
                .models
                .iter()
                .find_map(|(name, model)| (model == target).then(|| name.clone()))
                .or_else(|| {
                    self.models.iter().find_map(|(name, model)| {
                        (model.model_name == target.model_name).then(|| name.clone())
                    })
                }),
        }
    }
}

fn validate_skill_sync(entries: &[SkillSyncConfig]) -> Result<(), String> {
    for (index, entry) in entries.iter().enumerate() {
        if entry.skill_name.is_empty() {
            return Err(format!("skill_sync[{index}].skill_name must not be empty"));
        }
        if entry.upstream.is_empty() {
            return Err(format!("skill_sync[{index}].upstream must not be empty"));
        }
        for skill_name in &entry.skill_name {
            validate_skill_sync_name(skill_name)
                .map_err(|error| format!("skill_sync[{index}].skill_name: {error}"))?;
        }
        for upstream in &entry.upstream {
            validate_skill_sync_upstream(upstream)
                .map_err(|error| format!("skill_sync[{index}].upstream: {error}"))?;
        }
    }
    Ok(())
}

fn validate_skill_sync_name(skill_name: &str) -> Result<(), &'static str> {
    let name = skill_name.trim();
    if name.is_empty() {
        return Err("skill name must not be empty");
    }
    if name != skill_name {
        return Err("skill name must not contain leading or trailing whitespace");
    }
    if !name
        .chars()
        .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '_' | '-'))
    {
        return Err("skill name may only contain ASCII letters, digits, '_' and '-'");
    }
    Ok(())
}

fn validate_skill_sync_upstream(upstream: &str) -> Result<(), &'static str> {
    let trimmed = upstream.trim();
    if trimmed.is_empty() {
        return Err("upstream must not be empty");
    }
    if trimmed != upstream {
        return Err("upstream must not contain leading or trailing whitespace");
    }
    if trimmed.chars().any(char::is_whitespace) {
        return Err("upstream must not contain whitespace");
    }
    Ok(())
}

impl SandboxConfig {
    pub fn validate(&self) -> Result<(), String> {
        if self.bubblewrap_binary.trim().is_empty() {
            return Err("sandbox.bubblewrap_binary must not be empty".to_string());
        }
        if matches!(self.software_dir.as_deref().map(str::trim), Some("")) {
            return Err("sandbox.software_dir must not be empty when set".to_string());
        }
        let mount_path = self.software_mount_path.trim();
        if mount_path.is_empty() {
            return Err("sandbox.software_mount_path must not be empty".to_string());
        }
        if !Path::new(mount_path).is_absolute() {
            return Err("sandbox.software_mount_path must be an absolute path".to_string());
        }
        Ok(())
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

impl WebChannelConfig {
    pub fn resolve_token(&self) -> Result<String, String> {
        std::env::var(&self.token_env).map_err(|_| {
            format!(
                "web channel {} requires env {} for Authorization token",
                self.id, self.token_env
            )
        })
    }

    fn validate(&self) -> Result<(), String> {
        if self.id.trim().is_empty() {
            return Err("web channel id must not be empty".to_string());
        }
        if self.bind_addr.trim().is_empty() {
            return Err(format!(
                "web channel {} bind_addr must not be empty",
                self.id
            ));
        }
        if self.token_env.trim().is_empty() {
            return Err(format!(
                "web channel {} token_env must not be empty",
                self.id
            ));
        }
        Ok(())
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

fn default_web_bind_addr() -> String {
    "127.0.0.1:3111".to_string()
}

fn default_web_token_env() -> String {
    "STELLACLAW_WEB_TOKEN".to_string()
}

fn default_bubblewrap_binary() -> String {
    "bwrap".to_string()
}

fn default_software_mount_path() -> String {
    "/opt".to_string()
}

fn is_default_software_mount_path(value: &str) -> bool {
    value == default_software_mount_path()
}

fn parse_tool_model_target(raw: &str) -> Result<(&str, bool), String> {
    if raw.trim().is_empty() {
        return Err("tool model target must not be empty".to_string());
    }
    if let Some((alias, suffix)) = raw.split_once(':') {
        if suffix.trim() != "self" {
            return Err(format!(
                "unsupported tool model target suffix '{}'; expected ':self'",
                suffix.trim()
            ));
        }
        let alias = alias.trim();
        if alias.is_empty() {
            return Err("tool model target alias must not be empty".to_string());
        }
        return Ok((alias, true));
    }
    Ok((raw.trim(), false))
}
