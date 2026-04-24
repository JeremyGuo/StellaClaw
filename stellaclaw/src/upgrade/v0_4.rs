use std::path::Path;

use anyhow::{Context, Result};

use super::{WorkdirUpgrader, WORKDIR_VERSION_0_4, WORKDIR_VERSION_0_5};
use crate::config::StellaclawConfig;

pub struct SkillUpstreamUpgrade;

impl WorkdirUpgrader for SkillUpstreamUpgrade {
    fn from_version(&self) -> &'static str {
        WORKDIR_VERSION_0_4
    }

    fn to_version(&self) -> &'static str {
        WORKDIR_VERSION_0_5
    }

    fn upgrade(&self, workdir: &Path, _config: &StellaclawConfig) -> Result<()> {
        std::fs::create_dir_all(workdir.join("rundir").join(".skill_upstreams")).with_context(
            || {
                format!(
                    "failed to create {}",
                    workdir.join("rundir/.skill_upstreams").display()
                )
            },
        )
    }
}
