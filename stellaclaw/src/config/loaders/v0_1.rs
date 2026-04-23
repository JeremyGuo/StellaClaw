use anyhow::{Context, Result};

use crate::config::{StellaclawConfig, LATEST_CONFIG_VERSION};

pub fn load_and_upgrade(raw: &str) -> Result<StellaclawConfig> {
    let mut config: StellaclawConfig =
        serde_json::from_str(raw).context("failed to parse v0.1 stellaclaw config")?;
    config.version = LATEST_CONFIG_VERSION.to_string();
    Ok(config)
}
