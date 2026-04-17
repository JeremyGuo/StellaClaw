use super::{ConfigLoader, LATEST_CONFIG_VERSION, ServerConfig};
use anyhow::Result;
use serde_json::Value;

pub(super) struct LatestConfigLoader;

impl ConfigLoader for LatestConfigLoader {
    fn version(&self) -> &'static str {
        LATEST_CONFIG_VERSION
    }

    fn load_and_upgrade(&self, value: Value) -> Result<ServerConfig> {
        super::v0_14::load_versioned_config(value, LATEST_CONFIG_VERSION)
    }
}
