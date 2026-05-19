use std::path::Path;

use anyhow::Result;

use crate::config::StellaclawConfig;

use super::v0_4;

pub fn load(raw: &str, path: &Path) -> Result<StellaclawConfig> {
    v0_4::load(raw, path)
}
