use std::path::Path;

use anyhow::{Context, Result};
use serde_json::Value;

use crate::config::{StellaclawConfig, ToolModelTarget, LATEST_CONFIG_VERSION};

use super::partyclaw;

pub fn load(raw: &str, path: &Path) -> Result<StellaclawConfig> {
    let value: Value =
        serde_json::from_str(raw).context("failed to parse v0.2 stellaclaw config")?;
    if is_partyclaw_compatible_config(&value) {
        return partyclaw::load_compatible(raw, path);
    }
    serde_json::from_value(value).context("failed to parse v0.2 stellaclaw runtime config")
}

pub fn load_and_upgrade(raw: &str, path: &Path) -> Result<StellaclawConfig> {
    let mut config = load(raw, path)?;
    prefer_named_tool_model_targets(&mut config);
    config.version = LATEST_CONFIG_VERSION.to_string();
    Ok(config)
}

fn prefer_named_tool_model_targets(config: &mut StellaclawConfig) {
    let models = &config.models;
    prefer_named_tool_model_target(&mut config.session_defaults.image_tool_model, models);
    prefer_named_tool_model_target(&mut config.session_defaults.pdf_tool_model, models);
    prefer_named_tool_model_target(&mut config.session_defaults.audio_tool_model, models);
    prefer_named_tool_model_target(
        &mut config.session_defaults.image_generation_tool_model,
        models,
    );
    prefer_named_tool_model_target(&mut config.session_defaults.search_tool_model, models);
}

fn prefer_named_tool_model_target(
    target: &mut Option<ToolModelTarget>,
    models: &std::collections::BTreeMap<String, stellaclaw_core::model_config::ModelConfig>,
) {
    let Some(ToolModelTarget::Inline(model)) = target else {
        return;
    };
    let Some(alias) = models
        .iter()
        .find_map(|(alias, candidate)| (candidate == model).then(|| alias.clone()))
    else {
        return;
    };
    *target = Some(ToolModelTarget::Alias(alias));
}

fn is_partyclaw_compatible_config(value: &Value) -> bool {
    value
        .get("models")
        .and_then(Value::as_object)
        .map(|models| {
            models.values().any(|model| {
                model.get("type").is_some()
                    || model.get("api_endpoint").is_some()
                    || model.get("context_window_tokens").is_some()
            })
        })
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::load;
    use stellaclaw_core::model_config::{ModelCapability, ProviderType};

    #[test]
    fn loads_compact_v0_2_config() {
        let raw = r#"
        {
          "version": "0.2",
          "agent_server": {"path": "target/debug/agent_server"},
          "models": {
            "main": {
              "type": "openrouter",
              "api_endpoint": "https://openrouter.ai/api/v1",
              "model": "openai/gpt-4.1-mini",
              "api_key_env": "OPENROUTER_API_KEY",
              "capabilities": ["chat", "image_in"],
              "multimodal_input": {
                "image": {
                  "transport": "inline_base64",
                  "supported_media_types": ["image/png"],
                  "max_width": 2048,
                  "max_height": 2048
                }
              },
              "context_window_tokens": 128000,
              "timeout_seconds": 30,
              "retry_mode": {"mode": "no"}
            },
            "search": {
              "type": "brave-search",
              "api_endpoint": "https://api.search.brave.com",
              "model": "brave-web-search",
              "api_key_env": "BRAVE_SEARCH_API_KEY",
              "chat_completions_path": "/res/v1/web/search",
              "capabilities": ["web_search"],
              "context_window_tokens": 32768,
              "timeout_seconds": 15,
              "retry_mode": {"mode": "no"}
            }
          },
          "available_models": ["main"],
          "tooling": {"web_search": "search"},
          "main_agent": {
            "enable_context_compression": true,
            "context_compaction": {
              "token_limit_override": 100000,
              "recent_fidelity_target_ratio": 0.25
            }
          },
          "channels": [
            {
              "kind": "telegram",
              "id": "telegram-main",
              "bot_token_env": "TELEGRAM_BOT_TOKEN",
              "admin_user_ids": [42]
            }
          ]
        }
        "#;

        let config = load(raw, std::path::Path::new("/tmp/config.json")).unwrap();

        assert_eq!(
            config.agent_server.path.as_deref(),
            Some("target/debug/agent_server")
        );
        let main_model = config
            .initial_main_model()
            .expect("main model should exist");
        assert_eq!(main_model.model_name, "openai/gpt-4.1-mini");
        assert_eq!(main_model.provider_type, ProviderType::OpenRouterCompletion);
        assert!(main_model.capabilities.contains(&ModelCapability::ImageIn));
        let main_model = config
            .initial_main_model()
            .expect("main model should exist");
        let image_input = main_model
            .multimodal_input
            .as_ref()
            .and_then(|input| input.image.as_ref())
            .expect("image input should be configured");
        assert_eq!(image_input.supported_media_types, vec!["image/png"]);
        assert_eq!(image_input.max_width, Some(2048));
        assert_eq!(image_input.max_height, Some(2048));
        assert_eq!(
            config.session_defaults.compression_threshold_tokens,
            Some(100000)
        );
        assert_eq!(
            config.session_defaults.compression_retain_recent_tokens,
            Some(25000)
        );
        let search_model = config
            .session_defaults
            .search_tool_model
            .as_ref()
            .expect("search model target should exist")
            .resolve(&config.models, &main_model)
            .expect("search model target should resolve");
        assert_eq!(search_model.provider_type, ProviderType::BraveSearch);
    }

    #[test]
    fn loads_native_runtime_config_without_default_profile() {
        let raw = r#"
        {
          "version": "0.2",
          "agent_server": {"path": null},
          "models": {
            "main": {
              "provider_type": "claude_code",
              "model_name": "claude-opus-4-6",
              "url": "https://example.invalid/v1/messages",
              "api_key_env": "TEST_API_KEY",
              "capabilities": ["chat", "image_in"],
              "token_max_context": 262144,
              "cache_timeout": 300,
              "conn_timeout": 300,
              "retry_mode": "once",
              "token_estimator_type": "local",
              "multimodal_input": {
                "image": {
                  "transport": "inline_base64",
                  "supported_media_types": ["image/png", "image/jpeg"],
                  "max_width": 4096,
                  "max_height": 4096
                }
              }
            }
          },
          "channels": [
            {
              "kind": "telegram",
              "id": "telegram-main",
              "bot_token_env": "TELEGRAM_BOT_TOKEN"
            }
          ]
        }
        "#;

        let config = load(raw, std::path::Path::new("/tmp/config.json"))
            .expect("native runtime config should load");

        assert!(config.default_profile.is_none());
        assert_eq!(
            config
                .initial_main_model()
                .expect("main model should exist")
                .model_name,
            "claude-opus-4-6"
        );
    }

    #[test]
    fn loads_native_runtime_config_with_tool_model_aliases() {
        let raw = r#"
        {
          "version": "0.2",
          "models": {
            "main": {
              "provider_type": "claude_code",
              "model_name": "claude-opus-4-6",
              "url": "https://example.invalid/v1/messages",
              "api_key_env": "TEST_API_KEY",
              "capabilities": ["chat"],
              "token_max_context": 262144,
              "cache_timeout": 300,
              "conn_timeout": 300,
              "retry_mode": "once",
              "token_estimator_type": "local"
            },
            "search": {
              "provider_type": "brave_search",
              "model_name": "brave-web-search",
              "url": "https://api.search.brave.com/res/v1/web/search",
              "api_key_env": "BRAVE_SEARCH_API_KEY",
              "capabilities": ["web_search"],
              "token_max_context": 32768,
              "cache_timeout": 300,
              "conn_timeout": 30,
              "retry_mode": "once",
              "token_estimator_type": "local"
            },
            "image": {
              "provider_type": "open_router_completion",
              "model_name": "openai/gpt-image-1",
              "url": "https://openrouter.ai/api/v1/chat/completions",
              "api_key_env": "OPENROUTER_API_KEY",
              "capabilities": ["chat", "image_out"],
              "token_max_context": 128000,
              "cache_timeout": 300,
              "conn_timeout": 120,
              "retry_mode": "once",
              "token_estimator_type": "local"
            }
          },
          "session_defaults": {
            "search_tool_model": "search",
            "image_generation_tool_model": "image",
            "image_tool_model": "image:self"
          },
          "channels": [
            {
              "kind": "telegram",
              "id": "telegram-main",
              "bot_token_env": "TELEGRAM_BOT_TOKEN"
            }
          ]
        }
        "#;

        let config = load(raw, std::path::Path::new("/tmp/config.json"))
            .expect("native runtime config should load");
        let main_model = config
            .initial_main_model()
            .expect("main model should exist");
        let search_model = config
            .session_defaults
            .search_tool_model
            .as_ref()
            .expect("search model target should exist")
            .resolve(&config.models, &main_model)
            .expect("search model target should resolve");
        let image_model = config
            .session_defaults
            .image_generation_tool_model
            .as_ref()
            .expect("image generation target should exist")
            .resolve(&config.models, &main_model)
            .expect("image generation target should resolve");
        let self_model = config
            .session_defaults
            .image_tool_model
            .as_ref()
            .expect("self target should exist")
            .resolve(&config.models, &main_model)
            .expect("self target should resolve");

        assert_eq!(search_model.provider_type, ProviderType::BraveSearch);
        assert_eq!(image_model.model_name, "openai/gpt-image-1");
        assert_eq!(self_model.model_name, main_model.model_name);
    }
}
