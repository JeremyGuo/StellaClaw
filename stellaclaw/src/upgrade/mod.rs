use std::fs;
use std::path::Path;

use anyhow::{anyhow, Context, Result};

use crate::config::StellaclawConfig;

mod v0_1;
mod v0_2;
mod v0_3;
mod v0_4;
mod v0_5;

pub const LEGACY_WORKDIR_VERSION: &str = "0.1";
pub const WORKDIR_VERSION_0_2: &str = "0.2";
pub const WORKDIR_VERSION_0_3: &str = "0.3";
pub const WORKDIR_VERSION_0_4: &str = "0.4";
pub const WORKDIR_VERSION_0_5: &str = "0.5";
pub const LATEST_WORKDIR_VERSION: &str = "0.6";
pub const PARTYCLAW_LATEST_WORKDIR_VERSION: &str = "0.39";

const WORKDIR_VERSION_FILE: &str = "STELLA_VERSION";
const LEGACY_WORKDIR_VERSION_FILE: &str = "VERSION";

trait WorkdirUpgrader {
    fn from_version(&self) -> &'static str;
    fn to_version(&self) -> &'static str;
    fn upgrade(&self, workdir: &Path, config: &StellaclawConfig) -> Result<()>;
}

pub fn upgrade_workdir(workdir: &Path, config: &StellaclawConfig) -> Result<bool> {
    fs::create_dir_all(workdir)
        .with_context(|| format!("failed to create workdir {}", workdir.display()))?;
    let version_path = workdir.join(WORKDIR_VERSION_FILE);
    let legacy_version_path = workdir.join(LEGACY_WORKDIR_VERSION_FILE);
    let mut current = read_workdir_version(&version_path, &legacy_version_path)?;
    let mut upgraded = false;
    let upgraders: [&dyn WorkdirUpgrader; 6] = [
        &v0_1::LegacyUpgrade,
        &v0_1::PartyClawUpgrade,
        &v0_2::ChatMessageReasoningUpgrade,
        &v0_3::ModelSelectionUpgrade,
        &v0_4::SkillUpstreamUpgrade,
        &v0_5::TokenUsageCostUpgrade,
    ];

    while current != LATEST_WORKDIR_VERSION {
        let upgrader = upgraders
            .iter()
            .find(|item| item.from_version() == current)
            .copied()
            .ok_or_else(|| anyhow!("unsupported workdir version '{}'", current))?;
        upgrader.upgrade(workdir, config)?;
        current = upgrader.to_version();
        write_workdir_version(&version_path, current)?;
        upgraded = true;
    }

    if !version_path.exists() {
        write_workdir_version(&version_path, current)?;
        remove_legacy_version_file_if_present(&legacy_version_path)?;
        upgraded = true;
    }

    Ok(upgraded)
}

fn read_workdir_version(version_path: &Path, legacy_version_path: &Path) -> Result<&'static str> {
    if !version_path.exists() {
        if legacy_version_path.exists() {
            return read_version_file(legacy_version_path);
        }
        return Ok(LEGACY_WORKDIR_VERSION);
    }
    read_version_file(version_path)
}

fn read_version_file(version_path: &Path) -> Result<&'static str> {
    let raw = fs::read_to_string(version_path)
        .with_context(|| format!("failed to read {}", version_path.display()))?;
    match raw.trim() {
        LEGACY_WORKDIR_VERSION => Ok(LEGACY_WORKDIR_VERSION),
        PARTYCLAW_LATEST_WORKDIR_VERSION => Ok(PARTYCLAW_LATEST_WORKDIR_VERSION),
        WORKDIR_VERSION_0_2 => Ok(WORKDIR_VERSION_0_2),
        WORKDIR_VERSION_0_3 => Ok(WORKDIR_VERSION_0_3),
        WORKDIR_VERSION_0_4 => Ok(WORKDIR_VERSION_0_4),
        WORKDIR_VERSION_0_5 => Ok(WORKDIR_VERSION_0_5),
        LATEST_WORKDIR_VERSION => Ok(LATEST_WORKDIR_VERSION),
        other => Err(anyhow!("unsupported workdir version '{}'", other)),
    }
}

fn write_workdir_version(version_path: &Path, version: &str) -> Result<()> {
    fs::write(version_path, format!("{version}\n"))
        .with_context(|| format!("failed to write {}", version_path.display()))
}

fn remove_legacy_version_file_if_present(path: &Path) -> Result<()> {
    if path.exists() {
        fs::remove_file(path)
            .with_context(|| format!("failed to remove legacy version file {}", path.display()))?;
    }
    Ok(())
}
