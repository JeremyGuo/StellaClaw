use std::path::Path;

use anyhow::Result;

use super::{WorkdirUpgrader, WORKDIR_VERSION_0_2, WORKDIR_VERSION_0_3};
use crate::config::StellaclawConfig;

pub struct ChatMessageReasoningUpgrade;

impl WorkdirUpgrader for ChatMessageReasoningUpgrade {
    fn from_version(&self) -> &'static str {
        WORKDIR_VERSION_0_2
    }

    fn to_version(&self) -> &'static str {
        WORKDIR_VERSION_0_3
    }

    fn upgrade(&self, _workdir: &Path, _config: &StellaclawConfig) -> Result<()> {
        Ok(())
    }
}
