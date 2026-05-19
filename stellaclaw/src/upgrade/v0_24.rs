use std::path::Path;

use anyhow::Result;

use super::{WorkdirUpgrader, WORKDIR_VERSION_0_24, WORKDIR_VERSION_0_25};
use crate::config::StellaclawConfig;

pub struct ToolCallItemIdUpgrade;

impl WorkdirUpgrader for ToolCallItemIdUpgrade {
    fn from_version(&self) -> &'static str {
        WORKDIR_VERSION_0_24
    }

    fn to_version(&self) -> &'static str {
        WORKDIR_VERSION_0_25
    }

    fn upgrade(&self, _workdir: &Path, _config: &StellaclawConfig) -> Result<()> {
        Ok(())
    }
}
