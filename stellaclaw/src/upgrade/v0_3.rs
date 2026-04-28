use std::{fs, path::Path};

use anyhow::{anyhow, Context, Result};
use serde_json::Value;
use stellaclaw_core::model_config::ModelConfig;

use super::{WorkdirUpgrader, WORKDIR_VERSION_0_3, WORKDIR_VERSION_0_4};
use crate::config::StellaclawConfig;

pub struct ModelSelectionUpgrade;

impl WorkdirUpgrader for ModelSelectionUpgrade {
    fn from_version(&self) -> &'static str {
        WORKDIR_VERSION_0_3
    }

    fn to_version(&self) -> &'static str {
        WORKDIR_VERSION_0_4
    }

    fn upgrade(&self, workdir: &Path, config: &StellaclawConfig) -> Result<()> {
        let conversations_root = workdir.join("conversations");
        if !conversations_root.is_dir() {
            return Ok(());
        }

        for entry in fs::read_dir(&conversations_root)
            .with_context(|| format!("failed to read {}", conversations_root.display()))?
        {
            let entry = entry
                .with_context(|| format!("failed to enumerate {}", conversations_root.display()))?;
            let path = entry.path().join("conversation.json");
            if path.is_file() {
                upgrade_conversation_state(&path, config)?;
            }
        }
        Ok(())
    }
}

fn upgrade_conversation_state(path: &Path, config: &StellaclawConfig) -> Result<()> {
    let raw =
        fs::read_to_string(path).with_context(|| format!("failed to read {}", path.display()))?;
    let mut value: Value = serde_json::from_str(&raw)
        .with_context(|| format!("failed to parse {}", path.display()))?;
    let mut changed = false;

    if let Some(main_model) = value
        .get_mut("session_profile")
        .and_then(Value::as_object_mut)
        .and_then(|profile| profile.get_mut("main_model"))
    {
        let alias = model_alias_from_value(main_model, config)?;
        if main_model.as_str() != Some(alias.as_str()) {
            *main_model = Value::String(alias);
            changed = true;
        }
    }

    if let Some(binding) = value
        .get_mut("session_binding")
        .and_then(Value::as_object_mut)
    {
        changed |= upgrade_managed_session_records(binding.get_mut("background_sessions"), config)?;
        changed |= upgrade_managed_session_records(binding.get_mut("subagent_sessions"), config)?;
    }

    if changed {
        fs::write(
            path,
            serde_json::to_string_pretty(&value)
                .context("failed to serialize upgraded conversation state")?,
        )
        .with_context(|| format!("failed to write {}", path.display()))?;
    }

    Ok(())
}

fn upgrade_managed_session_records(
    records: Option<&mut Value>,
    config: &StellaclawConfig,
) -> Result<bool> {
    let Some(records) = records.and_then(Value::as_object_mut) else {
        return Ok(false);
    };
    let mut changed = false;
    for record in records.values_mut() {
        let Some(model_override) = record
            .as_object_mut()
            .and_then(|record| record.get_mut("model_override"))
        else {
            continue;
        };
        if model_override.is_null() {
            continue;
        }
        let alias = model_alias_from_value(model_override, config)?;
        if model_override.as_str() != Some(alias.as_str()) {
            *model_override = Value::String(alias);
            changed = true;
        }
    }
    Ok(changed)
}

fn model_alias_from_value(value: &Value, config: &StellaclawConfig) -> Result<String> {
    if let Some(name) = value.as_str() {
        return Ok(resolve_alias_or_model_name(name, config)
            .or_else(|| config.initial_main_model_name())
            .ok_or_else(|| anyhow!("missing fallback main model"))?);
    }

    if let Ok(model) = serde_json::from_value::<ModelConfig>(value.clone()) {
        if let Some(alias) = config
            .models
            .iter()
            .find_map(|(alias, candidate)| (candidate == &model).then(|| alias.clone()))
        {
            return Ok(alias);
        }
        if let Some(alias) = config.models.iter().find_map(|(alias, candidate)| {
            (candidate.model_name == model.model_name).then(|| alias.clone())
        }) {
            return Ok(alias);
        }
    }

    config
        .initial_main_model_name()
        .ok_or_else(|| anyhow!("missing fallback main model"))
}

fn resolve_alias_or_model_name(name: &str, config: &StellaclawConfig) -> Option<String> {
    if config.models.contains_key(name) {
        return Some(name.to_string());
    }
    config
        .models
        .iter()
        .find_map(|(alias, model)| (model.model_name == name).then(|| alias.clone()))
}

#[cfg(test)]
mod tests {
    use std::{
        collections::BTreeMap,
        time::{SystemTime, UNIX_EPOCH},
    };

    use serde_json::json;
    use stellaclaw_core::model_config::{
        MediaInputConfig, MediaInputTransport, ModelCapability, ModelConfig, MultimodalInputConfig,
        ProviderType, RetryMode, TokenEstimatorType,
    };

    use super::*;
    use crate::config::{
        AgentServerConfig, ChannelConfig, SandboxConfig, SessionDefaults, TelegramChannelConfig,
    };

    #[test]
    fn upgrades_persisted_model_snapshots_to_aliases_by_provider_model_name() {
        let root = std::env::temp_dir().join(format!(
            "stellaclaw-model-selection-upgrade-{}",
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let conversation_root = root.join("conversations").join("telegram-main-000009");
        fs::create_dir_all(&conversation_root).unwrap();

        let old_model = test_model_config(4096);
        let new_model = test_model_config(40);
        fs::write(
            conversation_root.join("conversation.json"),
            serde_json::to_string_pretty(&json!({
                "version": 1,
                "conversation_id": "telegram-main-000009",
                "channel_id": "telegram-main",
                "platform_chat_id": "9",
                "session_profile": {"main_model": old_model},
                "model_selection_pending": false,
                "tool_remote_mode": {"type": "selectable"},
                "session_binding": {
                    "foreground_session_id": "telegram-main-000009.foreground",
                    "background_sessions": {
                        "background_0001": {"model_override": old_model}
                    },
                    "subagent_sessions": {}
                }
            }))
            .unwrap(),
        )
        .unwrap();

        ModelSelectionUpgrade
            .upgrade(&root, &test_config(new_model))
            .unwrap();

        let upgraded: Value = serde_json::from_str(
            &fs::read_to_string(conversation_root.join("conversation.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(
            upgraded["session_profile"]["main_model"].as_str(),
            Some("opus-4.6")
        );
        assert_eq!(
            upgraded["session_binding"]["background_sessions"]["background_0001"]["model_override"]
                .as_str(),
            Some("opus-4.6")
        );

        let _ = fs::remove_dir_all(root);
    }

    fn test_config(model: ModelConfig) -> StellaclawConfig {
        StellaclawConfig {
            version: crate::config::LATEST_CONFIG_VERSION.to_string(),
            agent_server: AgentServerConfig::default(),
            default_profile: None,
            models: BTreeMap::from([("opus-4.6".to_string(), model)]),
            available_agent_models: Vec::new(),
            session_defaults: SessionDefaults::default(),
            skill_sync: Vec::new(),
            sandbox: SandboxConfig::default(),
            channels: vec![ChannelConfig::Telegram(TelegramChannelConfig {
                id: "telegram-main".to_string(),
                bot_token: Some("test".to_string()),
                bot_token_env: "TELEGRAM_BOT_TOKEN".to_string(),
                api_base_url: "https://api.telegram.org".to_string(),
                poll_timeout_seconds: 30,
                poll_interval_ms: 250,
                admin_user_ids: vec![],
            })],
        }
    }

    fn test_model_config(max_image_dimension: u32) -> ModelConfig {
        ModelConfig {
            provider_type: ProviderType::OpenRouterCompletion,
            model_name: "anthropic/claude-opus-4.6".to_string(),
            url: "https://openrouter.ai/api/v1/chat/completions".to_string(),
            api_key_env: "OPENROUTER_API_KEY".to_string(),
            capabilities: vec![ModelCapability::Chat, ModelCapability::ImageIn],
            token_max_context: 200_000,
            max_tokens: 0,
            cache_timeout: 300,
            conn_timeout: 30,
            request_timeout: 600,
            max_request_size: 30 * 1024 * 1024,
            retry_mode: RetryMode::Once,
            reasoning: Some(json!({"effort": "medium"})),
            token_estimator_type: TokenEstimatorType::Local,
            multimodal_estimator: None,
            multimodal_input: Some(MultimodalInputConfig {
                image: Some(MediaInputConfig {
                    transport: MediaInputTransport::InlineBase64,
                    supported_media_types: vec!["image/png".to_string()],
                    max_width: Some(max_image_dimension),
                    max_height: Some(max_image_dimension),
                }),
                pdf: None,
                audio: None,
            }),
            token_estimator_url: None,
        }
    }
}
