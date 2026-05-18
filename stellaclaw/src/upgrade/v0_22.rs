use std::path::Path;

use anyhow::Result;

use super::{WorkdirUpgrader, LATEST_WORKDIR_VERSION, WORKDIR_VERSION_0_22};
use crate::config::StellaclawConfig;

pub struct RuntimeConfigIdleCompactUpgrade;

impl WorkdirUpgrader for RuntimeConfigIdleCompactUpgrade {
    fn from_version(&self) -> &'static str {
        WORKDIR_VERSION_0_22
    }

    fn to_version(&self) -> &'static str {
        LATEST_WORKDIR_VERSION
    }

    fn upgrade(&self, _workdir: &Path, _config: &StellaclawConfig) -> Result<()> {
        Ok(())
    }
}
