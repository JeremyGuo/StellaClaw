use std::path::Path;

use anyhow::{Context, Result};
use serde_json::Value;

use crate::config::StellaclawConfig;

use super::partyclaw;

pub fn load(raw: &str, path: &Path) -> Result<StellaclawConfig> {
    let value: Value =
        serde_json::from_str(raw).context("failed to parse v0.2 stellaclaw config")?;
    if value.get("models").is_some() {
        return partyclaw::load_compatible(raw, path);
    }
    serde_json::from_value(value).context("failed to parse v0.2 stellaclaw runtime config")
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
        assert_eq!(
            config.default_profile.main_model.model_name,
            "openai/gpt-4.1-mini"
        );
        assert_eq!(
            config.default_profile.main_model.provider_type,
            ProviderType::OpenRouterCompletion
        );
        assert!(config
            .default_profile
            .main_model
            .capabilities
            .contains(&ModelCapability::ImageIn));
        let image_input = config
            .default_profile
            .main_model
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
        assert_eq!(
            config
                .session_defaults
                .search_tool_model
                .as_ref()
                .map(|model| &model.provider_type),
            Some(&ProviderType::BraveSearch)
        );
    }

    #[test]
    fn loads_repository_example_config() {
        let raw = include_str!("../../../../example_config.json");
        let config = load(raw, std::path::Path::new("example_config.json"))
            .expect("example config should load");
        let image_input = config
            .default_profile
            .main_model
            .multimodal_input
            .as_ref()
            .and_then(|input| input.image.as_ref())
            .expect("example should configure image input");

        assert_eq!(
            config.default_profile.main_model.model_name,
            "openai/gpt-4.1-mini"
        );
        assert_eq!(image_input.max_width, Some(4096));
        assert!(image_input
            .supported_media_types
            .contains(&"image/webp".to_string()));
    }
}
