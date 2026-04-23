use std::{
    fs,
    path::{Path, PathBuf},
};

use anyhow::{anyhow, Context, Result};

use crate::config::{StellaclawConfig, LATEST_CONFIG_VERSION, LEGACY_CONFIG_VERSION};

mod partyclaw;
mod v0_1;
mod v0_2;

const PARTYCLAW_LATEST_CONFIG_VERSION: &str = "0.28";

pub fn load_config_file_and_upgrade(path: &Path) -> Result<(StellaclawConfig, bool)> {
    let raw = fs::read_to_string(path)
        .with_context(|| format!("failed to read config file {}", path.display()))?;
    let version = detect_config_version(&raw)?;
    let mut config = match version.as_str() {
        LEGACY_CONFIG_VERSION => v0_1::load_and_upgrade(&raw)?,
        LATEST_CONFIG_VERSION => v0_2::load(&raw, path)?,
        PARTYCLAW_LATEST_CONFIG_VERSION => partyclaw::load_and_upgrade(&raw, path)?,
        other => return Err(anyhow!("unsupported config version '{}'", other)),
    };
    config.validate().map_err(anyhow::Error::msg)?;
    let upgraded = version != LATEST_CONFIG_VERSION;
    if upgraded {
        config.version = LATEST_CONFIG_VERSION.to_string();
        let rewritten =
            serde_json::to_string_pretty(&config).context("failed to serialize upgraded config")?;
        let backup_path = next_config_backup_path(path)?;
        fs::write(&backup_path, raw.as_bytes()).with_context(|| {
            format!(
                "failed to write config upgrade backup {}",
                backup_path.display()
            )
        })?;
        fs::write(path, rewritten)
            .with_context(|| format!("failed to rewrite upgraded config {}", path.display()))?;
    }
    Ok((config, upgraded))
}

fn detect_config_version(raw: &str) -> Result<String> {
    let value: serde_json::Value =
        serde_json::from_str(raw).context("failed to parse config JSON while checking version")?;
    Ok(value
        .get("version")
        .and_then(serde_json::Value::as_str)
        .unwrap_or(LEGACY_CONFIG_VERSION)
        .to_string())
}

fn next_config_backup_path(path: &Path) -> Result<PathBuf> {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    let file_name = path
        .file_name()
        .and_then(|value| value.to_str())
        .ok_or_else(|| anyhow!("config path {} has no valid file name", path.display()))?;

    for index in 0..1000 {
        let candidate_name = if index == 0 {
            format!("{file_name}.pre-stellaclaw-upgrade.bak")
        } else {
            format!("{file_name}.pre-stellaclaw-upgrade.{index}.bak")
        };
        let candidate = parent.join(candidate_name);
        if !candidate.exists() {
            return Ok(candidate);
        }
    }

    Err(anyhow!(
        "failed to allocate config upgrade backup path for {}",
        path.display()
    ))
}

#[cfg(test)]
mod tests {
    use super::next_config_backup_path;

    #[test]
    fn allocates_non_conflicting_config_backup_path() {
        let root =
            std::env::temp_dir().join(format!("stellaclaw-config-backup-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).expect("temp dir should be created");
        let config_path = root.join("config.json");
        std::fs::write(&config_path, "{}").expect("config should be written");
        std::fs::write(
            config_path.with_file_name("config.json.pre-stellaclaw-upgrade.bak"),
            "{}",
        )
        .expect("first backup should be written");

        let backup = next_config_backup_path(&config_path).expect("backup path should allocate");

        assert_eq!(
            backup.file_name().and_then(|value| value.to_str()),
            Some("config.json.pre-stellaclaw-upgrade.1.bak")
        );
        std::fs::remove_dir_all(&root).expect("temp dir should be cleaned");
    }
}
