use std::path::Path;

use anyhow::Result;

use super::{WorkdirUpgrader, WORKDIR_VERSION_0_23, WORKDIR_VERSION_0_24};
use crate::config::StellaclawConfig;

pub struct ChatMessageCompactionItemUpgrade;

impl WorkdirUpgrader for ChatMessageCompactionItemUpgrade {
    fn from_version(&self) -> &'static str {
        WORKDIR_VERSION_0_23
    }

    fn to_version(&self) -> &'static str {
        WORKDIR_VERSION_0_24
    }

    fn upgrade(&self, _workdir: &Path, _config: &StellaclawConfig) -> Result<()> {
        Ok(())
    }
}
