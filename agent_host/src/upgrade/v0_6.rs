use super::WorkdirUpgrader;
use anyhow::Result;
use std::path::Path;

pub(super) struct Upgrade;

impl WorkdirUpgrader for Upgrade {
    fn from_version(&self) -> &'static str {
        "0.5"
    }

    fn to_version(&self) -> &'static str {
        "0.6"
    }

    fn upgrade(&self, _workdir: &Path) -> Result<()> {
        Ok(())
    }
}
