use anyhow::{Context, Result, anyhow};
use std::fs;
use std::path::Path;

mod v0_5;
mod v0_6;
mod v0_7;

pub const LEGACY_WORKDIR_VERSION: &str = "0.4";
pub const LATEST_WORKDIR_VERSION: &str = "0.7";
const VERSION_FILE_NAME: &str = "VERSION";

trait WorkdirUpgrader {
    fn from_version(&self) -> &'static str;
    fn to_version(&self) -> &'static str;
    fn upgrade(&self, workdir: &Path) -> Result<()>;
}

pub fn upgrade_workdir(workdir: impl AsRef<Path>) -> Result<bool> {
    let workdir = workdir.as_ref();
    fs::create_dir_all(workdir)
        .with_context(|| format!("failed to create workdir {}", workdir.display()))?;
    let version_path = workdir.join(VERSION_FILE_NAME);
    let mut current = read_workdir_version(&version_path)?;
    let mut upgraded = false;
    let upgraders: [&dyn WorkdirUpgrader; 3] = [&v0_5::Upgrade, &v0_6::Upgrade, &v0_7::Upgrade];

    while current != LATEST_WORKDIR_VERSION {
        let upgrader = upgraders
            .iter()
            .find(|item| item.from_version() == current)
            .copied()
            .ok_or_else(|| anyhow!("unsupported workdir version '{}'", current))?;
        upgrader.upgrade(workdir)?;
        current = upgrader.to_version();
        write_workdir_version(&version_path, current)?;
        upgraded = true;
    }

    if !version_path.exists() {
        write_workdir_version(&version_path, current)?;
    }

    Ok(upgraded)
}

fn read_workdir_version(version_path: &Path) -> Result<&'static str> {
    if !version_path.exists() {
        return Ok(LEGACY_WORKDIR_VERSION);
    }
    let raw = fs::read_to_string(version_path)
        .with_context(|| format!("failed to read {}", version_path.display()))?;
    match raw.trim() {
        LEGACY_WORKDIR_VERSION => Ok(LEGACY_WORKDIR_VERSION),
        "0.5" => Ok("0.5"),
        "0.6" => Ok("0.6"),
        LATEST_WORKDIR_VERSION => Ok(LATEST_WORKDIR_VERSION),
        other => Err(anyhow!("unsupported workdir version '{}'", other)),
    }
}

fn write_workdir_version(version_path: &Path, version: &str) -> Result<()> {
    fs::write(version_path, format!("{version}\n"))
        .with_context(|| format!("failed to write {}", version_path.display()))
}

#[cfg(test)]
mod tests {
    use super::{LATEST_WORKDIR_VERSION, upgrade_workdir};
    use serde_json::json;
    use std::fs;
    use tempfile::TempDir;
    use uuid::Uuid;

    #[test]
    fn missing_version_file_upgrades_workdir_and_backfills_conversation_workspace() {
        let temp_dir = TempDir::new().unwrap();
        let session_dir = temp_dir
            .path()
            .join("sessions")
            .join(Uuid::new_v4().to_string());
        let conversation_dir = temp_dir
            .path()
            .join("conversations")
            .join(Uuid::new_v4().to_string());
        fs::create_dir_all(&session_dir).unwrap();
        fs::create_dir_all(&conversation_dir).unwrap();

        fs::write(
            session_dir.join("session.json"),
            serde_json::to_string_pretty(&json!({
                "id": Uuid::new_v4(),
                "agent_id": Uuid::new_v4(),
                "address": {
                    "channel_id": "telegram-main",
                    "conversation_id": "123",
                    "user_id": "user-1",
                    "display_name": "User"
                },
                "workspace_id": "workspace-1"
            }))
            .unwrap(),
        )
        .unwrap();
        fs::write(
            conversation_dir.join("conversation.json"),
            serde_json::to_string_pretty(&json!({
                "id": Uuid::new_v4(),
                "address": {
                    "channel_id": "telegram-main",
                    "conversation_id": "123",
                    "user_id": "user-1",
                    "display_name": "User"
                },
                "settings": {
                    "main_model": "main"
                }
            }))
            .unwrap(),
        )
        .unwrap();

        let upgraded = upgrade_workdir(temp_dir.path()).unwrap();
        assert!(upgraded);
        assert_eq!(
            fs::read_to_string(temp_dir.path().join("VERSION"))
                .unwrap()
                .trim(),
            LATEST_WORKDIR_VERSION
        );

        let conversation: serde_json::Value = serde_json::from_str(
            &fs::read_to_string(conversation_dir.join("conversation.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(
            conversation["settings"]["workspace_id"].as_str(),
            Some("workspace-1")
        );
    }
}
