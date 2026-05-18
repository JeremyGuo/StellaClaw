use std::path::Path;

use anyhow::Result;

use super::{WorkdirUpgrader, WORKDIR_VERSION_0_22, WORKDIR_VERSION_0_23};
use crate::config::StellaclawConfig;

pub struct RuntimeConfigIdleCompactUpgrade;

impl WorkdirUpgrader for RuntimeConfigIdleCompactUpgrade {
    fn from_version(&self) -> &'static str {
        WORKDIR_VERSION_0_22
    }

    fn to_version(&self) -> &'static str {
        WORKDIR_VERSION_0_23
    }

    fn upgrade(&self, _workdir: &Path, _config: &StellaclawConfig) -> Result<()> {
        Ok(())
    }
}
