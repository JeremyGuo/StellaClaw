use std::{fs, path::Path};

use anyhow::{anyhow, Context, Result};

use crate::config::{StellaclawConfig, LATEST_CONFIG_VERSION, LEGACY_CONFIG_VERSION};

mod partyclaw;
mod v0_1;
mod v0_2;

const PARTYCLAW_LATEST_CONFIG_VERSION: &str = "0.28";

pub fn load_config_file_and_upgrade(path: &Path) -> Result<(StellaclawConfig, bool)> {
    let raw = fs::read_to_string(path)
        .with_context(|| format!("failed to read config file {}", path.display()))?;
    let version = detect_config_version(&raw)?;
    let mut config = match version.as_str() {
        LEGACY_CONFIG_VERSION => v0_1::load_and_upgrade(&raw)?,
        LATEST_CONFIG_VERSION => v0_2::load(&raw, path)?,
        PARTYCLAW_LATEST_CONFIG_VERSION => partyclaw::load_and_upgrade(&raw, path)?,
        other => return Err(anyhow!("unsupported config version '{}'", other)),
    };
    config.validate().map_err(anyhow::Error::msg)?;
    let upgraded = version != LATEST_CONFIG_VERSION;
    if upgraded {
        config.version = LATEST_CONFIG_VERSION.to_string();
        let rewritten =
            serde_json::to_string_pretty(&config).context("failed to serialize upgraded config")?;
        fs::write(path, rewritten)
            .with_context(|| format!("failed to rewrite upgraded config {}", path.display()))?;
    }
    Ok((config, upgraded))
}

fn detect_config_version(raw: &str) -> Result<String> {
    let value: serde_json::Value =
        serde_json::from_str(raw).context("failed to parse config JSON while checking version")?;
    Ok(value
        .get("version")
        .and_then(serde_json::Value::as_str)
        .unwrap_or(LEGACY_CONFIG_VERSION)
        .to_string())
}
