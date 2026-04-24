use std::path::Path;

use anyhow::{Context, Result};
use serde_json::Value;

use crate::config::StellaclawConfig;

use super::partyclaw;

pub fn load(raw: &str, path: &Path) -> Result<StellaclawConfig> {
    let value: Value =
        serde_json::from_str(raw).context("failed to parse v0.4 stellaclaw config")?;
    if is_partyclaw_compatible_config(&value) {
        return partyclaw::load_compatible(raw, path);
    }
    serde_json::from_value(value).context("failed to parse v0.4 stellaclaw runtime config")
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

    #[test]
    fn loads_repository_example_config() {
        let raw = include_str!("../../../../example_config.json");
        let config = load(raw, std::path::Path::new("example_config.json"))
            .expect("example config should load");
        let main_model = config
            .initial_main_model()
            .expect("main model should exist");
        let image_input = main_model
            .multimodal_input
            .as_ref()
            .and_then(|input| input.image.as_ref())
            .expect("example should configure image input");

        assert_eq!(main_model.model_name, "openai/gpt-4.1-mini");
        assert_eq!(image_input.max_width, Some(4096));
        assert!(image_input
            .supported_media_types
            .contains(&"image/webp".to_string()));
        assert!(config.session_defaults.search_tool_model.is_some());
        assert_eq!(config.sandbox.software_mount_path, "/opt");
    }
}
