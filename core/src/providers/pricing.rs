use std::{collections::BTreeMap, sync::LazyLock};

use serde::Deserialize;

use crate::{
    model_config::{ModelConfig, ProviderType},
    session_actor::{TokenUsage, TokenUsageCost},
};

const PRICE_UNIT_TOKENS: f64 = 1_000_000.0;

#[derive(Debug, Clone, Copy, Deserialize)]
struct TokenPrice {
    cache_read: f64,
    cache_write: f64,
    input: f64,
    output: f64,
}

type ProviderPricing = BTreeMap<String, TokenPrice>;

static OPEN_ROUTER_COMPLETION_PRICING: LazyLock<ProviderPricing> = LazyLock::new(|| {
    load_provider_pricing(include_str!("../../../pricing/open_router_completion.json"))
});
static OPEN_ROUTER_RESPONSES_PRICING: LazyLock<ProviderPricing> = LazyLock::new(|| {
    load_provider_pricing(include_str!("../../../pricing/open_router_responses.json"))
});
static OPENAI_IMAGE_PRICING: LazyLock<ProviderPricing> =
    LazyLock::new(|| load_provider_pricing(include_str!("../../../pricing/openai_image.json")));
static CLAUDE_CODE_PRICING: LazyLock<ProviderPricing> =
    LazyLock::new(|| load_provider_pricing(include_str!("../../../pricing/claude_code.json")));
static CODEX_SUBSCRIPTION_PRICING: LazyLock<ProviderPricing> = LazyLock::new(|| {
    load_provider_pricing(include_str!("../../../pricing/codex_subscription.json"))
});
static BRAVE_SEARCH_PRICING: LazyLock<ProviderPricing> =
    LazyLock::new(|| load_provider_pricing(include_str!("../../../pricing/brave_search.json")));
static BRAVE_SEARCH_IMAGE_PRICING: LazyLock<ProviderPricing> = LazyLock::new(|| {
    load_provider_pricing(include_str!("../../../pricing/brave_search_image.json"))
});
static BRAVE_SEARCH_VIDEO_PRICING: LazyLock<ProviderPricing> = LazyLock::new(|| {
    load_provider_pricing(include_str!("../../../pricing/brave_search_video.json"))
});
static BRAVE_SEARCH_NEWS_PRICING: LazyLock<ProviderPricing> = LazyLock::new(|| {
    load_provider_pricing(include_str!("../../../pricing/brave_search_news.json"))
});

pub(crate) struct PriceManager;

impl PriceManager {
    pub(crate) fn attach_cost(model_config: &ModelConfig, usage: &mut TokenUsage) {
        usage.cost_usd = Self::token_usage_cost(model_config, usage);
    }

    pub(crate) fn token_usage_cost(
        model_config: &ModelConfig,
        usage: &TokenUsage,
    ) -> Option<TokenUsageCost> {
        let price = provider_pricing(&model_config.provider_type).get(&model_config.model_name)?;
        Some(TokenUsageCost {
            cache_read: usage.cache_read as f64 * price.cache_read / PRICE_UNIT_TOKENS,
            cache_write: usage.cache_write as f64 * price.cache_write / PRICE_UNIT_TOKENS,
            uncache_input: usage.uncache_input as f64 * price.input / PRICE_UNIT_TOKENS,
            output: usage.output as f64 * price.output / PRICE_UNIT_TOKENS,
        })
    }
}

fn provider_pricing(provider_type: &ProviderType) -> &'static ProviderPricing {
    match provider_type {
        ProviderType::OpenRouterCompletion => &OPEN_ROUTER_COMPLETION_PRICING,
        ProviderType::OpenRouterResponses => &OPEN_ROUTER_RESPONSES_PRICING,
        ProviderType::OpenAiImageEdit => &OPENAI_IMAGE_PRICING,
        ProviderType::ClaudeCode => &CLAUDE_CODE_PRICING,
        ProviderType::CodexSubscription => &CODEX_SUBSCRIPTION_PRICING,
        ProviderType::BraveSearch => &BRAVE_SEARCH_PRICING,
        ProviderType::BraveSearchImage => &BRAVE_SEARCH_IMAGE_PRICING,
        ProviderType::BraveSearchVideo => &BRAVE_SEARCH_VIDEO_PRICING,
        ProviderType::BraveSearchNews => &BRAVE_SEARCH_NEWS_PRICING,
    }
}

fn load_provider_pricing(raw: &str) -> ProviderPricing {
    serde_json::from_str(raw).expect("provider pricing JSON must be a model pricing dictionary")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model_config::{
        ModelCapability, MultimodalEstimatorConfig, MultimodalInputConfig, RetryMode,
        TokenEstimatorType,
    };

    #[test]
    fn parses_embedded_pricing_files() {
        for provider_type in [
            ProviderType::OpenRouterCompletion,
            ProviderType::OpenRouterResponses,
            ProviderType::OpenAiImageEdit,
            ProviderType::ClaudeCode,
            ProviderType::CodexSubscription,
            ProviderType::BraveSearch,
            ProviderType::BraveSearchImage,
            ProviderType::BraveSearchVideo,
            ProviderType::BraveSearchNews,
        ] {
            let _ = provider_pricing(&provider_type);
        }
    }

    #[test]
    fn missing_price_returns_no_cost() {
        let model_config = test_model_config("missing/model");
        let usage = TokenUsage {
            cache_read: 1,
            cache_write: 2,
            uncache_input: 3,
            output: 4,
            cost_usd: None,
        };

        assert!(PriceManager::token_usage_cost(&model_config, &usage).is_none());
    }

    fn test_model_config(model_name: &str) -> ModelConfig {
        ModelConfig {
            provider_type: ProviderType::OpenRouterCompletion,
            model_name: model_name.to_string(),
            url: "https://openrouter.ai/api/v1/chat/completions".to_string(),
            api_key_env: "OPENROUTER_API_KEY".to_string(),
            capabilities: vec![ModelCapability::Chat],
            token_max_context: 128_000,
            cache_timeout: 300,
            conn_timeout: 30,
            request_timeout: 600,
            max_request_size: 30 * 1024 * 1024,
            retry_mode: RetryMode::Once,
            reasoning: None,
            token_estimator_type: TokenEstimatorType::Local,
            multimodal_estimator: None::<MultimodalEstimatorConfig>,
            multimodal_input: None::<MultimodalInputConfig>,
            token_estimator_url: None,
        }
    }
}
