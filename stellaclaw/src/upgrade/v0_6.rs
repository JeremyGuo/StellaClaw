use std::path::Path;

use anyhow::Result;

use super::{WorkdirUpgrader, WORKDIR_VERSION_0_6, WORKDIR_VERSION_0_7};
use crate::config::StellaclawConfig;

pub struct CronScriptUpgrade;

impl WorkdirUpgrader for CronScriptUpgrade {
    fn from_version(&self) -> &'static str {
        WORKDIR_VERSION_0_6
    }

    fn to_version(&self) -> &'static str {
        WORKDIR_VERSION_0_7
    }

    fn upgrade(&self, _workdir: &Path, _config: &StellaclawConfig) -> Result<()> {
        Ok(())
    }
}
