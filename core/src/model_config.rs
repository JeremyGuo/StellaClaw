use serde::{Deserialize, Serialize};

use crate::session_actor::MultimodalTokenStrategy;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderType {
    OpenRouterCompletion,
    OpenRouterResponses,
    ClaudeCode,
    CodexSubscription,
    BraveSearch,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ModelCapability {
    Chat,
    ImageIn,
    ImageOut,
    PdfIn,
    AudioIn,
    WebSearch,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TokenEstimatorType {
    Local,
    HuggingFace,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RetryMode {
    Once,
    RandomInterval {
        max_interval_secs: u64,
        max_retries: u64,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MultimodalEstimatorConfig {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub image: Option<MultimodalTokenStrategy>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MultimodalInputConfig {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub image: Option<MediaInputConfig>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pdf: Option<MediaInputConfig>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub audio: Option<MediaInputConfig>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MediaInputConfig {
    pub transport: MediaInputTransport,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub supported_media_types: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_width: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_height: Option<u32>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MediaInputTransport {
    FileReference,
    InlineBase64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ModelConfig {
    pub provider_type: ProviderType,
    pub model_name: String,
    pub url: String,
    pub api_key_env: String,
    pub capabilities: Vec<ModelCapability>,
    pub token_max_context: u64,
    pub cache_timeout: u64,
    pub conn_timeout: u64,
    pub retry_mode: RetryMode,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning: Option<serde_json::Value>,
    pub token_estimator_type: TokenEstimatorType,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub multimodal_estimator: Option<MultimodalEstimatorConfig>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub multimodal_input: Option<MultimodalInputConfig>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub token_estimator_url: Option<String>,
}

impl ModelConfig {
    pub fn supports(&self, capability: ModelCapability) -> bool {
        self.capabilities.contains(&capability)
    }

    pub fn uses_huggingface_token_estimator(&self) -> bool {
        matches!(self.token_estimator_type, TokenEstimatorType::HuggingFace)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn serializes_model_config_with_huggingface_token_estimator() {
        let config = ModelConfig {
            provider_type: ProviderType::OpenRouterCompletion,
            model_name: "openai/gpt-4o-mini".to_string(),
            url: "https://openrouter.ai/api/v1/chat/completions".to_string(),
            api_key_env: "OPENAI_API_KEY".to_string(),
            capabilities: vec![
                ModelCapability::Chat,
                ModelCapability::ImageIn,
                ModelCapability::WebSearch,
            ],
            token_max_context: 128_000,
            cache_timeout: 300,
            conn_timeout: 30,
            retry_mode: RetryMode::RandomInterval {
                max_interval_secs: 3,
                max_retries: 5,
            },
            reasoning: Some(serde_json::json!({
                "effort": "medium",
                "max_tokens": 4096
            })),
            token_estimator_type: TokenEstimatorType::HuggingFace,
            multimodal_estimator: Some(MultimodalEstimatorConfig {
                image: Some(MultimodalTokenStrategy::PatchGrid {
                    patch_size: 32,
                    patch_budget: 1536,
                    multiplier: 1.62,
                }),
            }),
            multimodal_input: Some(MultimodalInputConfig {
                image: Some(MediaInputConfig {
                    transport: MediaInputTransport::InlineBase64,
                    supported_media_types: vec!["image/png".to_string(), "image/jpeg".to_string()],
                    max_width: Some(4096),
                    max_height: Some(4096),
                }),
                pdf: None,
                audio: None,
            }),
            token_estimator_url: Some(
                "https://huggingface.co/openai/gpt-oss-20b/raw/main/tokenizer_config.json"
                    .to_string(),
            ),
        };

        let json = serde_json::to_value(&config).expect("model config should serialize");

        assert_eq!(json["provider_type"], "open_router_completion");
        assert_eq!(json["model_name"], "openai/gpt-4o-mini");
        assert_eq!(json["capabilities"][0], "chat");
        assert_eq!(json["token_max_context"], 128000);
        assert_eq!(json["conn_timeout"], 30);
        assert_eq!(json["retry_mode"]["random_interval"]["max_retries"], 5);
        assert_eq!(json["reasoning"]["effort"], "medium");
        assert_eq!(json["token_estimator_type"], "hugging_face");
        assert_eq!(
            json["multimodal_estimator"]["image"]["patch_grid"]["patch_size"],
            32
        );
        assert_eq!(
            json["multimodal_input"]["image"]["transport"],
            "inline_base64"
        );
    }

    #[test]
    fn checks_capability_support() {
        let config = ModelConfig {
            provider_type: ProviderType::OpenRouterCompletion,
            model_name: "claude-sonnet-4".to_string(),
            url: "https://openrouter.ai/api/v1/chat/completions".to_string(),
            api_key_env: "OPENROUTER_API_KEY".to_string(),
            capabilities: vec![ModelCapability::Chat],
            token_max_context: 200_000,
            cache_timeout: 120,
            conn_timeout: 10,
            retry_mode: RetryMode::Once,
            reasoning: None,
            token_estimator_type: TokenEstimatorType::Local,
            multimodal_estimator: None,
            multimodal_input: None,
            token_estimator_url: None,
        };

        assert!(config.supports(ModelCapability::Chat));
        assert!(!config.supports(ModelCapability::ImageOut));
        assert!(!config.uses_huggingface_token_estimator());
    }
}
