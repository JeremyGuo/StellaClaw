use anyhow::{Context, Result, anyhow};
use std::fs;
use std::path::Path;

mod v0_10;
mod v0_11;
mod v0_12;
mod v0_13;
mod v0_5;
mod v0_6;
mod v0_7;
mod v0_8;
mod v0_9;

pub const LEGACY_WORKDIR_VERSION: &str = "0.4";
pub const LATEST_WORKDIR_VERSION: &str = "0.13";
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
    let upgraders: [&dyn WorkdirUpgrader; 9] = [
        &v0_5::Upgrade,
        &v0_6::Upgrade,
        &v0_7::Upgrade,
        &v0_8::Upgrade,
        &v0_9::Upgrade,
        &v0_10::Upgrade,
        &v0_11::Upgrade,
        &v0_12::Upgrade,
        &v0_13::Upgrade,
    ];

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
        "0.7" => Ok("0.7"),
        "0.8" => Ok("0.8"),
        "0.9" => Ok("0.9"),
        "0.10" => Ok("0.10"),
        "0.11" => Ok("0.11"),
        "0.12" => Ok("0.12"),
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

    #[test]
    fn v0_8_workdir_upgrade_backfills_agent_backend_fields() {
        let temp_dir = TempDir::new().unwrap();
        fs::write(temp_dir.path().join("VERSION"), "0.8\n").unwrap();

        let conversation_dir = temp_dir
            .path()
            .join("conversations")
            .join(Uuid::new_v4().to_string());
        let snapshot_dir = temp_dir.path().join("snapshots").join("snap-1");
        let session_dir = temp_dir
            .path()
            .join("sessions")
            .join(Uuid::new_v4().to_string());
        let subagent_dir = temp_dir
            .path()
            .join("agent")
            .join("runtime")
            .join("workspace-1")
            .join("agent_frame")
            .join("subagents");
        fs::create_dir_all(&conversation_dir).unwrap();
        fs::create_dir_all(&snapshot_dir).unwrap();
        fs::create_dir_all(&session_dir).unwrap();
        fs::create_dir_all(&subagent_dir).unwrap();
        fs::create_dir_all(temp_dir.path().join("cron")).unwrap();

        let address = json!({
            "channel_id": "telegram-main",
            "conversation_id": "123",
            "user_id": "user-1",
            "display_name": "User"
        });

        fs::write(
            conversation_dir.join("conversation.json"),
            serde_json::to_string_pretty(&json!({
                "id": Uuid::new_v4(),
                "address": address,
                "settings": {
                    "main_model": "main"
                }
            }))
            .unwrap(),
        )
        .unwrap();
        fs::write(
            snapshot_dir.join("metadata.json"),
            serde_json::to_string_pretty(&json!({
                "name": "snap-1",
                "saved_at": "2026-04-08T00:00:00Z",
                "source_channel_id": "telegram-main",
                "source_conversation_id": "123",
                "main_model": "main",
                "sandbox_mode": "bubblewrap"
            }))
            .unwrap(),
        )
        .unwrap();
        fs::write(
            snapshot_dir.join("snapshot.json"),
            serde_json::to_string_pretty(&json!({
                "saved_at": "2026-04-08T00:00:00Z",
                "source_address": {
                    "channel_id": "telegram-main",
                    "conversation_id": "123",
                    "user_id": "user-1",
                    "display_name": "User"
                },
                "settings": {
                    "main_model": "main",
                    "sandbox_mode": "bubblewrap",
                    "workspace_id": "workspace-1",
                    "chat_version_id": Uuid::new_v4()
                },
                "session": {
                    "id": Uuid::new_v4()
                }
            }))
            .unwrap(),
        )
        .unwrap();
        fs::write(
            temp_dir.path().join("cron").join("tasks.json"),
            serde_json::to_string_pretty(&json!({
                "tasks": [{
                    "id": Uuid::new_v4(),
                    "name": "daily",
                    "description": "demo",
                    "schedule": "0 * * * *",
                    "model_key": "main",
                    "prompt": "hello",
                    "sink": {"kind": "conversation"},
                    "address": {
                        "channel_id": "telegram-main",
                        "conversation_id": "123",
                        "user_id": "user-1",
                        "display_name": "User"
                    },
                    "enabled": true,
                    "created_at": "2026-04-08T00:00:00Z",
                    "updated_at": "2026-04-08T00:00:00Z"
                }]
            }))
            .unwrap(),
        )
        .unwrap();
        fs::write(
            session_dir.join("session.json"),
            serde_json::to_string_pretty(&json!({
                "id": Uuid::new_v4(),
                "pending_continue": {
                    "model_key": "main",
                    "resume_messages": [],
                    "error_summary": "failed",
                    "progress_summary": "preserved",
                    "failed_at": "2026-04-08T00:00:00Z"
                }
            }))
            .unwrap(),
        )
        .unwrap();
        fs::write(
            subagent_dir.join("subagent.json"),
            serde_json::to_string_pretty(&json!({
                "id": Uuid::new_v4(),
                "parent_agent_id": Uuid::new_v4(),
                "session_id": Uuid::new_v4(),
                "channel_id": "telegram-main",
                "conversation_id": "123",
                "workspace_id": "workspace-1",
                "model_key": "main"
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
        assert!(conversation["settings"]["agent_backend"].is_null());

        let snapshot_metadata: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(snapshot_dir.join("metadata.json")).unwrap())
                .unwrap();
        assert!(snapshot_metadata["agent_backend"].is_null());

        let snapshot_bundle: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(snapshot_dir.join("snapshot.json")).unwrap())
                .unwrap();
        assert!(snapshot_bundle["settings"]["agent_backend"].is_null());

        let cron: serde_json::Value = serde_json::from_str(
            &fs::read_to_string(temp_dir.path().join("cron").join("tasks.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(
            cron["tasks"][0]["agent_backend"].as_str(),
            Some("agent_frame")
        );

        let session: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(session_dir.join("session.json")).unwrap())
                .unwrap();
        assert!(session["pending_continue"]["agent_backend"].is_null());

        let subagent: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(subagent_dir.join("subagent.json")).unwrap())
                .unwrap();
        assert_eq!(subagent["agent_backend"].as_str(), Some("agent_frame"));
    }

    #[test]
    fn v0_9_workdir_upgrade_clears_exec_runtime_state_for_tty_metadata_refresh() {
        let temp_dir = TempDir::new().unwrap();
        fs::write(temp_dir.path().join("VERSION"), "0.9\n").unwrap();

        let processes_dir = temp_dir
            .path()
            .join("agent")
            .join("runtime")
            .join("workspace-1")
            .join("agent_frame")
            .join("processes");
        let tool_workers_dir = temp_dir
            .path()
            .join("agent")
            .join("runtime")
            .join("workspace-1")
            .join("agent_frame")
            .join("tool_workers");
        fs::create_dir_all(&processes_dir).unwrap();
        fs::create_dir_all(&tool_workers_dir).unwrap();
        fs::write(processes_dir.join("exec-1.json"), "{}").unwrap();
        fs::write(tool_workers_dir.join("exec-1.job.json"), "{}").unwrap();
        fs::write(tool_workers_dir.join("image-1.job.json"), "{}").unwrap();

        let upgraded = upgrade_workdir(temp_dir.path()).unwrap();
        assert!(upgraded);
        assert_eq!(
            fs::read_to_string(temp_dir.path().join("VERSION"))
                .unwrap()
                .trim(),
            LATEST_WORKDIR_VERSION
        );
        assert!(!processes_dir.exists());
        assert!(!tool_workers_dir.join("exec-1.job.json").exists());
        assert!(tool_workers_dir.join("image-1.job.json").exists());
    }

    #[test]
    fn v0_11_workdir_upgrade_seeds_partclaw_files() {
        let temp_dir = TempDir::new().unwrap();
        fs::write(temp_dir.path().join("VERSION"), "0.11\n").unwrap();
        let workspace_files_dir = temp_dir.path().join("workspaces/workspace-1/files");
        fs::create_dir_all(&workspace_files_dir).unwrap();
        fs::create_dir_all(temp_dir.path().join("rundir")).unwrap();

        let upgraded = upgrade_workdir(temp_dir.path()).unwrap();

        assert!(upgraded);
        assert_eq!(
            fs::read_to_string(temp_dir.path().join("VERSION"))
                .unwrap()
                .trim(),
            LATEST_WORKDIR_VERSION
        );
        assert!(temp_dir.path().join("rundir/PARTCLAW.md").is_file());
        assert!(workspace_files_dir.join("PARTCLAW.md").is_file());
    }

    #[test]
    fn v0_12_workdir_upgrade_backfills_last_user_message_timestamp() {
        let temp_dir = TempDir::new().unwrap();
        fs::write(temp_dir.path().join("VERSION"), "0.12\n").unwrap();
        let session_dir = temp_dir
            .path()
            .join("sessions")
            .join(Uuid::new_v4().to_string());
        fs::create_dir_all(&session_dir).unwrap();
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
                "workspace_id": "workspace-1",
                "history": [
                    { "role": "user", "text": "hello", "attachments": [] },
                    { "role": "assistant", "text": "world", "attachments": [] }
                ],
                "last_agent_returned_at": "2026-04-09T00:00:00Z"
            }))
            .unwrap(),
        )
        .unwrap();

        let upgraded = upgrade_workdir(temp_dir.path()).unwrap();

        assert!(upgraded);
        let session: serde_json::Value = serde_json::from_str(
            &fs::read_to_string(session_dir.join("session.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(
            session["last_user_message_at"].as_str(),
            Some("2026-04-09T00:00:00Z")
        );
    }
}
