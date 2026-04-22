use anyhow::{Context, Result, anyhow};
use std::fs;
use std::path::Path;

mod v0_10;
mod v0_11;
mod v0_12;
mod v0_13;
mod v0_14;
mod v0_15;
mod v0_16;
mod v0_17;
mod v0_18;
mod v0_19;
mod v0_20;
mod v0_21;
mod v0_22;
mod v0_23;
mod v0_24;
mod v0_25;
mod v0_26;
mod v0_27;
mod v0_28;
mod v0_29;
mod v0_30;
mod v0_31;
mod v0_32;
mod v0_33;
mod v0_34;
mod v0_35;
mod v0_36;
mod v0_37;
mod v0_38;
mod v0_5;
mod v0_6;
mod v0_7;
mod v0_8;
mod v0_9;

pub const LEGACY_WORKDIR_VERSION: &str = "0.4";
pub const LATEST_WORKDIR_VERSION: &str = "0.38";
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
    let upgraders: [&dyn WorkdirUpgrader; 34] = [
        &v0_5::Upgrade,
        &v0_6::Upgrade,
        &v0_7::Upgrade,
        &v0_8::Upgrade,
        &v0_9::Upgrade,
        &v0_10::Upgrade,
        &v0_11::Upgrade,
        &v0_12::Upgrade,
        &v0_13::Upgrade,
        &v0_14::Upgrade,
        &v0_15::Upgrade,
        &v0_16::Upgrade,
        &v0_17::Upgrade,
        &v0_18::Upgrade,
        &v0_19::Upgrade,
        &v0_20::Upgrade,
        &v0_21::Upgrade,
        &v0_22::Upgrade,
        &v0_23::Upgrade,
        &v0_24::Upgrade,
        &v0_25::Upgrade,
        &v0_26::Upgrade,
        &v0_27::Upgrade,
        &v0_28::Upgrade,
        &v0_29::Upgrade,
        &v0_30::Upgrade,
        &v0_31::Upgrade,
        &v0_32::Upgrade,
        &v0_33::Upgrade,
        &v0_34::Upgrade,
        &v0_35::Upgrade,
        &v0_36::Upgrade,
        &v0_37::Upgrade,
        &v0_38::Upgrade,
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
        "0.13" => Ok("0.13"),
        "0.14" => Ok("0.14"),
        "0.15" => Ok("0.15"),
        "0.16" => Ok("0.16"),
        "0.17" => Ok("0.17"),
        "0.18" => Ok("0.18"),
        "0.19" => Ok("0.19"),
        "0.20" => Ok("0.20"),
        "0.21" => Ok("0.21"),
        "0.22" => Ok("0.22"),
        "0.23" => Ok("0.23"),
        "0.24" => Ok("0.24"),
        "0.25" => Ok("0.25"),
        "0.26" => Ok("0.26"),
        "0.27" => Ok("0.27"),
        "0.28" => Ok("0.28"),
        "0.29" => Ok("0.29"),
        "0.30" => Ok("0.30"),
        "0.31" => Ok("0.31"),
        "0.32" => Ok("0.32"),
        "0.33" => Ok("0.33"),
        "0.34" => Ok("0.34"),
        "0.35" => Ok("0.35"),
        "0.36" => Ok("0.36"),
        "0.37" => Ok("0.37"),
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
    use base64::Engine;
    use serde_json::json;
    use std::fs;
    use std::path::{Path, PathBuf};
    use tempfile::TempDir;
    use uuid::Uuid;

    fn upgraded_session_json_path(workdir: &Path, original_session_dir: &Path) -> PathBuf {
        let original_path = original_session_dir.join("session.json");
        if original_path.is_file() {
            return original_path;
        }
        let session_id = original_session_dir
            .file_name()
            .and_then(|name| name.to_str())
            .expect("session dir should be named by session id");
        let roots = crate::session::find_session_roots(&workdir.join("sessions")).unwrap();
        if roots.len() == 1 {
            return roots[0].join("session.json");
        }
        roots
            .into_iter()
            .find(|root| root.file_name().and_then(|name| name.to_str()) == Some(session_id))
            .expect("upgraded session root should exist")
            .join("session.json")
    }

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

        let session: serde_json::Value = serde_json::from_str(
            &fs::read_to_string(upgraded_session_json_path(temp_dir.path(), &session_dir)).unwrap(),
        )
        .unwrap();
        assert!(session.get("pending_continue").is_none());

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
    fn v0_20_workdir_upgrade_backfills_exec_remote_metadata() {
        let temp_dir = TempDir::new().unwrap();
        fs::write(temp_dir.path().join("VERSION"), "0.19\n").unwrap();

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
        fs::write(
            processes_dir.join("exec-1.json"),
            serde_json::to_string_pretty(&json!({
                "exec_id": "exec-1",
                "worker_pid": 123,
                "tty": false,
                "command": "printf ok",
                "cwd": "/tmp",
                "stdout_path": "/tmp/exec-1.stdout",
                "stderr_path": "/tmp/exec-1.stderr",
                "status_path": "/tmp/exec-1.status.json",
                "worker_exit_code_path": "/tmp/exec-1.worker.exit",
                "requests_dir": "/tmp/exec-1.requests"
            }))
            .unwrap(),
        )
        .unwrap();
        fs::write(
            tool_workers_dir.join("job-1.json"),
            serde_json::to_string_pretty(&json!({
                "kind": "exec",
                "exec_id": "exec-1",
                "tty": false,
                "command": "printf ok",
                "cwd": "/tmp",
                "status_path": "/tmp/exec-1.status.json",
                "stdout_path": "/tmp/exec-1.stdout",
                "stderr_path": "/tmp/exec-1.stderr",
                "requests_dir": "/tmp/exec-1.requests"
            }))
            .unwrap(),
        )
        .unwrap();
        fs::write(
            processes_dir.join("exec-1.status.json"),
            serde_json::to_string_pretty(&json!({
                "exec_id": "exec-1",
                "running": false
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

        let metadata: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(processes_dir.join("exec-1.json")).unwrap())
                .unwrap();
        let status: serde_json::Value = serde_json::from_str(
            &fs::read_to_string(processes_dir.join("exec-1.status.json")).unwrap(),
        )
        .unwrap();
        let job: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(tool_workers_dir.join("job-1.json")).unwrap())
                .unwrap();
        assert_eq!(metadata["remote"].as_str(), Some("local"));
        assert_eq!(status["remote"].as_str(), Some("local"));
        assert!(job.get("remote").is_some_and(|value| value.is_null()));
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
            &fs::read_to_string(upgraded_session_json_path(temp_dir.path(), &session_dir)).unwrap(),
        )
        .unwrap();
        assert_eq!(
            session["last_user_message_at"].as_str(),
            Some("2026-04-09T00:00:00Z")
        );
    }

    #[test]
    fn v0_13_workdir_upgrade_seeds_context_attachment_store_dirs() {
        let temp_dir = TempDir::new().unwrap();
        fs::write(temp_dir.path().join("VERSION"), "0.13\n").unwrap();
        let workspace_files_dir = temp_dir.path().join("workspaces/workspace-1/files");
        fs::create_dir_all(&workspace_files_dir).unwrap();

        let upgraded = upgrade_workdir(temp_dir.path()).unwrap();

        assert!(upgraded);
        assert!(
            workspace_files_dir
                .join(crate::workspace::CONTEXT_ATTACHMENT_STORE_DIR_NAME)
                .is_dir()
        );
    }

    #[test]
    fn v0_14_workdir_upgrade_backfills_session_state() {
        let temp_dir = TempDir::new().unwrap();
        fs::write(temp_dir.path().join("VERSION"), "0.14\n").unwrap();
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
                "agent_messages": [{"role": "assistant", "content": "stale"}],
                "pending_continue": {
                    "model_key": "main",
                    "resume_messages": [{"role": "assistant", "content": "resume-here"}],
                    "error_summary": "legacy failure",
                    "progress_summary": "preserved",
                    "failed_at": "2026-04-10T00:00:00Z"
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
        let session: serde_json::Value = serde_json::from_str(
            &fs::read_to_string(upgraded_session_json_path(temp_dir.path(), &session_dir)).unwrap(),
        )
        .unwrap();
        assert_eq!(
            session["session_state"]["messages"][0]["content"].as_str(),
            Some("resume-here")
        );
        assert_eq!(session["session_state"]["phase"].as_str(), Some("yielded"));
        assert_eq!(
            session["session_state"]["errno"].as_str(),
            Some("runtime_failure")
        );
        assert_eq!(
            session["session_state"]["errinfo"].as_str(),
            Some("legacy failure")
        );
        assert!(session.get("pending_continue").is_none());
    }

    #[test]
    fn v0_21_workdir_upgrade_backfills_progress_message_slot() {
        let temp_dir = TempDir::new().unwrap();
        fs::write(temp_dir.path().join("VERSION"), "0.20\n").unwrap();
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
                    "conversation_id": "123"
                },
                "session_state": {
                    "messages": [],
                    "pending_messages": [],
                    "phase": "end",
                    "errno": null,
                    "errinfo": null
                }
            }))
            .unwrap(),
        )
        .unwrap();

        let upgraded = upgrade_workdir(temp_dir.path()).unwrap();

        assert!(upgraded);
        let session: serde_json::Value = serde_json::from_str(
            &fs::read_to_string(upgraded_session_json_path(temp_dir.path(), &session_dir)).unwrap(),
        )
        .unwrap();
        assert!(session["session_state"].get("progress_message").is_some());
        assert!(session["session_state"]["progress_message"].is_null());
    }

    #[test]
    fn v0_15_workdir_upgrade_removes_pending_continue_field() {
        let temp_dir = TempDir::new().unwrap();
        fs::write(temp_dir.path().join("VERSION"), "0.15\n").unwrap();

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
                "session_state": {
                    "messages": [{"role": "assistant", "content": "resume-here"}],
                    "pending_messages": [],
                    "phase": "yielded",
                    "errno": "api_failure",
                    "errinfo": "failed",
                },
                "pending_continue": {
                    "model_key": "main",
                    "resume_messages": [{"role": "assistant", "content": "resume-here"}],
                    "error_summary": "legacy failure",
                    "progress_summary": "preserved",
                    "failed_at": "2026-04-10T00:00:00Z"
                }
            }))
            .unwrap(),
        )
        .unwrap();

        let upgraded = upgrade_workdir(temp_dir.path()).unwrap();
        assert!(upgraded);
        let session: serde_json::Value = serde_json::from_str(
            &fs::read_to_string(upgraded_session_json_path(temp_dir.path(), &session_dir)).unwrap(),
        )
        .unwrap();
        assert!(session.get("pending_continue").is_none());
        assert_eq!(session["session_state"]["phase"].as_str(), Some("yielded"));
    }

    #[test]
    fn v0_16_workdir_upgrade_rewrites_legacy_pending_continue_errno() {
        let temp_dir = TempDir::new().unwrap();
        fs::write(temp_dir.path().join("VERSION"), "0.16\n").unwrap();

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
                "session_state": {
                    "messages": [{"role": "assistant", "content": "resume-here"}],
                    "pending_messages": [],
                    "phase": "yielded",
                    "errno": "legacy_pending_continue",
                    "errinfo": "legacy failure"
                }
            }))
            .unwrap(),
        )
        .unwrap();

        let upgraded = upgrade_workdir(temp_dir.path()).unwrap();
        assert!(upgraded);
        let session: serde_json::Value = serde_json::from_str(
            &fs::read_to_string(upgraded_session_json_path(temp_dir.path(), &session_dir)).unwrap(),
        )
        .unwrap();
        assert_eq!(
            session["session_state"]["errno"].as_str(),
            Some("runtime_failure")
        );
    }

    #[test]
    fn v0_17_workdir_upgrade_removes_legacy_agent_messages_field() {
        let temp_dir = TempDir::new().unwrap();
        fs::write(temp_dir.path().join("VERSION"), "0.17\n").unwrap();

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
                "agent_messages": [{"role": "assistant", "content": "legacy"}],
                "session_state": {
                    "messages": [{"role": "assistant", "content": "stable"}],
                    "pending_messages": [],
                    "phase": "end",
                    "errno": null,
                    "errinfo": null
                }
            }))
            .unwrap(),
        )
        .unwrap();

        let upgraded = upgrade_workdir(temp_dir.path()).unwrap();
        assert!(upgraded);
        let session: serde_json::Value = serde_json::from_str(
            &fs::read_to_string(upgraded_session_json_path(temp_dir.path(), &session_dir)).unwrap(),
        )
        .unwrap();
        assert!(session.get("agent_messages").is_none());
        assert_eq!(
            session["session_state"]["messages"][0]["content"].as_str(),
            Some("stable")
        );
    }

    #[test]
    fn v0_18_workdir_upgrade_removes_idle_compaction_retry_field() {
        let temp_dir = TempDir::new().unwrap();
        fs::write(temp_dir.path().join("VERSION"), "0.18\n").unwrap();

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
                "idle_compaction_retry": {
                    "error_summary": "old retry marker",
                    "failed_at": "2026-04-11T00:00:00Z"
                },
                "session_state": {
                    "messages": [{"role": "assistant", "content": "stable"}],
                    "pending_messages": [],
                    "phase": "end",
                    "errno": "idle_compaction_failure",
                    "errinfo": "old retry marker"
                }
            }))
            .unwrap(),
        )
        .unwrap();

        let upgraded = upgrade_workdir(temp_dir.path()).unwrap();
        assert!(upgraded);
        let session: serde_json::Value = serde_json::from_str(
            &fs::read_to_string(upgraded_session_json_path(temp_dir.path(), &session_dir)).unwrap(),
        )
        .unwrap();
        assert!(session.get("idle_compaction_retry").is_none());
        assert_eq!(
            session["session_state"]["errno"].as_str(),
            Some("idle_compaction_failure")
        );
        assert_eq!(
            session["session_state"]["errinfo"].as_str(),
            Some("old retry marker")
        );
    }

    #[test]
    fn v0_22_workdir_upgrade_backfills_conversation_remote_workpaths() {
        let temp_dir = TempDir::new().unwrap();
        fs::write(temp_dir.path().join("VERSION"), "0.21\n").unwrap();

        let conversation_dir = temp_dir
            .path()
            .join("conversations")
            .join(Uuid::new_v4().to_string());
        let snapshot_dir = temp_dir.path().join("snapshots").join("snap-1");
        fs::create_dir_all(&conversation_dir).unwrap();
        fs::create_dir_all(&snapshot_dir).unwrap();
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
        fs::write(
            snapshot_dir.join("snapshot.json"),
            serde_json::to_string_pretty(&json!({
                "saved_at": "2026-04-14T00:00:00Z",
                "source_address": {
                    "channel_id": "telegram-main",
                    "conversation_id": "123",
                    "user_id": "user-1",
                    "display_name": "User"
                },
                "settings": {
                    "main_model": "main"
                },
                "session": {
                    "id": Uuid::new_v4()
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
            conversation["settings"]["remote_workpaths"]
                .as_array()
                .unwrap()
                .len(),
            0
        );
        let snapshot: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(snapshot_dir.join("snapshot.json")).unwrap())
                .unwrap();
        assert_eq!(
            snapshot["settings"]["remote_workpaths"]
                .as_array()
                .unwrap()
                .len(),
            0
        );
    }

    #[test]
    fn v0_23_workdir_upgrade_backfills_system_prompt_prompt_state() {
        let temp_dir = TempDir::new().unwrap();
        fs::write(temp_dir.path().join("VERSION"), "0.22\n").unwrap();

        let session_dir = temp_dir
            .path()
            .join("sessions")
            .join(Uuid::new_v4().to_string());
        let snapshot_dir = temp_dir.path().join("snapshots").join("snap-1");
        fs::create_dir_all(&session_dir).unwrap();
        fs::create_dir_all(&snapshot_dir).unwrap();
        fs::write(
            session_dir.join("session.json"),
            serde_json::to_string_pretty(&json!({
                "id": Uuid::new_v4(),
                "agent_id": Uuid::new_v4(),
                "address": {
                    "channel_id": "telegram-main",
                    "conversation_id": "123"
                },
                "session_state": {
                    "messages": []
                }
            }))
            .unwrap(),
        )
        .unwrap();
        fs::write(
            snapshot_dir.join("snapshot.json"),
            serde_json::to_string_pretty(&json!({
                "saved_at": "2026-04-14T00:00:00Z",
                "source_address": {
                    "channel_id": "telegram-main",
                    "conversation_id": "123"
                },
                "settings": {},
                "session": {
                    "turn_count": 1
                }
            }))
            .unwrap(),
        )
        .unwrap();

        let upgraded = upgrade_workdir(temp_dir.path()).unwrap();

        assert!(upgraded);
        let session: serde_json::Value = serde_json::from_str(
            &fs::read_to_string(upgraded_session_json_path(temp_dir.path(), &session_dir)).unwrap(),
        )
        .unwrap();
        assert!(session["session_state"]["system_prompt_static_hash"].is_null());
        assert!(session["session_state"]["system_prompt_component_hashes"].is_null());
        assert!(session["session_state"]["pending_system_prompt_component_notices"].is_null());

        let snapshot: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(snapshot_dir.join("snapshot.json")).unwrap())
                .unwrap();
        assert!(snapshot["session"]["system_prompt_static_hash"].is_null());
        assert!(snapshot["session"]["system_prompt_component_hashes"].is_null());
        assert!(snapshot["session"]["pending_system_prompt_component_notices"].is_null());
    }

    #[test]
    fn v0_25_workdir_upgrade_backfills_actor_mailbox() {
        let temp_dir = TempDir::new().unwrap();
        fs::write(temp_dir.path().join("VERSION"), "0.25\n").unwrap();

        let session_dir = temp_dir
            .path()
            .join("sessions")
            .join(Uuid::new_v4().to_string());
        let snapshot_dir = temp_dir.path().join("snapshots").join("snap-1");
        fs::create_dir_all(&session_dir).unwrap();
        fs::create_dir_all(&snapshot_dir).unwrap();
        fs::write(
            session_dir.join("session.json"),
            serde_json::to_string_pretty(&json!({
                "id": Uuid::new_v4(),
                "agent_id": Uuid::new_v4(),
                "address": {
                    "channel_id": "telegram-main",
                    "conversation_id": "123"
                },
                "session_state": {
                    "messages": []
                }
            }))
            .unwrap(),
        )
        .unwrap();
        fs::write(
            snapshot_dir.join("snapshot.json"),
            serde_json::to_string_pretty(&json!({
                "saved_at": "2026-04-14T00:00:00Z",
                "source_address": {
                    "channel_id": "telegram-main",
                    "conversation_id": "123"
                },
                "settings": {},
                "session": {
                    "turn_count": 1
                }
            }))
            .unwrap(),
        )
        .unwrap();

        let upgraded = upgrade_workdir(temp_dir.path()).unwrap();

        assert!(upgraded);
        let session: serde_json::Value = serde_json::from_str(
            &fs::read_to_string(upgraded_session_json_path(temp_dir.path(), &session_dir)).unwrap(),
        )
        .unwrap();
        assert!(
            session["session_state"]["actor_mailbox"]
                .as_array()
                .unwrap()
                .is_empty()
        );

        let snapshot: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(snapshot_dir.join("snapshot.json")).unwrap())
                .unwrap();
        assert!(
            snapshot["session"]["actor_mailbox"]
                .as_array()
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn v0_26_workdir_upgrade_backfills_user_mailbox() {
        let temp_dir = TempDir::new().unwrap();
        fs::write(temp_dir.path().join("VERSION"), "0.26\n").unwrap();

        let session_dir = temp_dir
            .path()
            .join("sessions")
            .join(Uuid::new_v4().to_string());
        let snapshot_dir = temp_dir.path().join("snapshots").join("snap-1");
        fs::create_dir_all(&session_dir).unwrap();
        fs::create_dir_all(&snapshot_dir).unwrap();
        fs::write(
            session_dir.join("session.json"),
            serde_json::to_string_pretty(&json!({
                "id": Uuid::new_v4(),
                "agent_id": Uuid::new_v4(),
                "address": {
                    "channel_id": "telegram-main",
                    "conversation_id": "123"
                },
                "session_state": {
                    "messages": [],
                    "actor_mailbox": []
                }
            }))
            .unwrap(),
        )
        .unwrap();
        fs::write(
            snapshot_dir.join("snapshot.json"),
            serde_json::to_string_pretty(&json!({
                "saved_at": "2026-04-14T00:00:00Z",
                "source_address": {
                    "channel_id": "telegram-main",
                    "conversation_id": "123"
                },
                "settings": {},
                "session": {
                    "turn_count": 1,
                    "actor_mailbox": []
                }
            }))
            .unwrap(),
        )
        .unwrap();

        let upgraded = upgrade_workdir(temp_dir.path()).unwrap();

        assert!(upgraded);
        let session: serde_json::Value = serde_json::from_str(
            &fs::read_to_string(upgraded_session_json_path(temp_dir.path(), &session_dir)).unwrap(),
        )
        .unwrap();
        assert!(
            session["session_state"]["user_mailbox"]
                .as_array()
                .unwrap()
                .is_empty()
        );

        let snapshot: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(snapshot_dir.join("snapshot.json")).unwrap())
                .unwrap();
        assert!(
            snapshot["session"]["user_mailbox"]
                .as_array()
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn v0_28_workdir_upgrade_backfills_prompt_components() {
        let temp_dir = TempDir::new().unwrap();
        fs::write(temp_dir.path().join("VERSION"), "0.27\n").unwrap();

        let session_dir = temp_dir
            .path()
            .join("sessions")
            .join(Uuid::new_v4().to_string());
        let snapshot_dir = temp_dir.path().join("snapshots").join("snap-1");
        fs::create_dir_all(&session_dir).unwrap();
        fs::create_dir_all(&snapshot_dir).unwrap();
        fs::write(
            session_dir.join("session.json"),
            serde_json::to_string_pretty(&json!({
                "id": Uuid::new_v4(),
                "agent_id": Uuid::new_v4(),
                "address": {
                    "channel_id": "telegram-main",
                    "conversation_id": "123"
                },
                "seen_user_profile_version": "old-user",
                "seen_identity_profile_version": "old-identity",
                "pending_user_profile_notice": true,
                "pending_identity_profile_notice": true,
                "seen_model_catalog_version": "old-models",
                "pending_model_catalog_notice": true,
                "session_state": {
                    "messages": [],
                    "user_mailbox": [],
                    "system_prompt_component_hashes": {
                        "identity": "old-hash"
                    },
                    "pending_system_prompt_component_notices": [
                        "identity"
                    ]
                }
            }))
            .unwrap(),
        )
        .unwrap();
        fs::write(
            snapshot_dir.join("snapshot.json"),
            serde_json::to_string_pretty(&json!({
                "saved_at": "2026-04-14T00:00:00Z",
                "source_address": {
                    "channel_id": "telegram-main",
                    "conversation_id": "123"
                },
                "settings": {},
                "session": {
                    "turn_count": 1,
                    "user_mailbox": [],
                    "seen_user_profile_version": "old-user",
                    "seen_identity_profile_version": "old-identity",
                    "pending_user_profile_notice": true,
                    "pending_identity_profile_notice": true,
                    "seen_model_catalog_version": "old-models",
                    "pending_model_catalog_notice": true,
                    "system_prompt_component_hashes": {
                        "identity": "old-hash"
                    },
                    "pending_system_prompt_component_notices": [
                        "identity"
                    ]
                }
            }))
            .unwrap(),
        )
        .unwrap();

        let upgraded = upgrade_workdir(temp_dir.path()).unwrap();

        assert!(upgraded);
        let session: serde_json::Value = serde_json::from_str(
            &fs::read_to_string(upgraded_session_json_path(temp_dir.path(), &session_dir)).unwrap(),
        )
        .unwrap();
        assert_eq!(session["session_state"]["prompt_components"], json!({}));
        assert!(session["seen_user_profile_version"].is_null());
        assert!(session["seen_identity_profile_version"].is_null());
        assert!(session["pending_user_profile_notice"].is_null());
        assert!(session["pending_identity_profile_notice"].is_null());
        assert!(session["seen_model_catalog_version"].is_null());
        assert!(session["pending_model_catalog_notice"].is_null());
        assert!(session["session_state"]["system_prompt_component_hashes"].is_null());
        assert!(session["session_state"]["pending_system_prompt_component_notices"].is_null());

        let snapshot: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(snapshot_dir.join("snapshot.json")).unwrap())
                .unwrap();
        assert_eq!(snapshot["session"]["prompt_components"], json!({}));
        assert!(snapshot["session"]["seen_user_profile_version"].is_null());
        assert!(snapshot["session"]["seen_identity_profile_version"].is_null());
        assert!(snapshot["session"]["pending_user_profile_notice"].is_null());
        assert!(snapshot["session"]["pending_identity_profile_notice"].is_null());
        assert!(snapshot["session"]["seen_model_catalog_version"].is_null());
        assert!(snapshot["session"]["pending_model_catalog_notice"].is_null());
        assert!(snapshot["session"]["system_prompt_component_hashes"].is_null());
        assert!(snapshot["session"]["pending_system_prompt_component_notices"].is_null());
    }

    #[test]
    fn v0_29_workdir_upgrade_backfills_cron_task_timezone() {
        let temp_dir = TempDir::new().unwrap();
        fs::write(temp_dir.path().join("VERSION"), "0.28\n").unwrap();

        let cron_dir = temp_dir.path().join("cron");
        fs::create_dir_all(&cron_dir).unwrap();
        fs::write(
            cron_dir.join("tasks.json"),
            serde_json::to_string_pretty(&json!({
                "tasks": [
                    {
                        "id": Uuid::new_v4(),
                        "name": "reminder",
                        "description": "send a reminder",
                        "schedule": "0 7 13 * * *",
                        "agent_backend": "agent_frame",
                        "model_key": "main",
                        "prompt": "ping",
                        "sink": {
                            "type": "direct",
                            "address": {
                                "channel_id": "telegram-main",
                                "conversation_id": "123"
                            }
                        },
                        "address": {
                            "channel_id": "telegram-main",
                            "conversation_id": "123"
                        },
                        "enabled": true,
                        "created_at": "2026-04-18T00:00:00Z",
                        "updated_at": "2026-04-18T00:00:00Z"
                    }
                ]
            }))
            .unwrap(),
        )
        .unwrap();

        let upgraded = upgrade_workdir(temp_dir.path()).unwrap();

        assert!(upgraded);
        let store: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(cron_dir.join("tasks.json")).unwrap())
                .unwrap();
        assert_eq!(store["tasks"][0]["timezone"], json!("Asia/Shanghai"));
    }

    #[test]
    fn v0_30_workdir_upgrade_backfills_current_plan() {
        let temp_dir = TempDir::new().unwrap();
        fs::write(temp_dir.path().join("VERSION"), "0.29\n").unwrap();

        let session_dir = temp_dir
            .path()
            .join("sessions")
            .join(Uuid::new_v4().to_string());
        let snapshot_dir = temp_dir.path().join("snapshots").join("snap-1");
        fs::create_dir_all(&session_dir).unwrap();
        fs::create_dir_all(&snapshot_dir).unwrap();
        fs::write(
            session_dir.join("session.json"),
            serde_json::to_string_pretty(&json!({
                "id": Uuid::new_v4(),
                "agent_id": Uuid::new_v4(),
                "address": {
                    "channel_id": "telegram-main",
                    "conversation_id": "123"
                },
                "session_state": {
                    "messages": []
                }
            }))
            .unwrap(),
        )
        .unwrap();
        fs::write(
            snapshot_dir.join("snapshot.json"),
            serde_json::to_string_pretty(&json!({
                "saved_at": "2026-04-19T00:00:00Z",
                "source_address": {
                    "channel_id": "telegram-main",
                    "conversation_id": "123"
                },
                "settings": {},
                "session": {
                    "messages": []
                }
            }))
            .unwrap(),
        )
        .unwrap();

        let upgraded = upgrade_workdir(temp_dir.path()).unwrap();

        assert!(upgraded);
        let session: serde_json::Value = serde_json::from_str(
            &fs::read_to_string(upgraded_session_json_path(temp_dir.path(), &session_dir)).unwrap(),
        )
        .unwrap();
        assert!(session["session_state"]["current_plan"].is_null());

        let snapshot: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(snapshot_dir.join("snapshot.json")).unwrap())
                .unwrap();
        assert!(snapshot["session"]["current_plan"].is_null());
    }

    #[test]
    fn v0_31_workdir_upgrade_creates_session_transcripts() {
        let temp_dir = TempDir::new().unwrap();
        fs::write(temp_dir.path().join("VERSION"), "0.30\n").unwrap();

        let session_id = Uuid::new_v4();
        let session_dir = temp_dir
            .path()
            .join("sessions")
            .join(session_id.to_string());
        fs::create_dir_all(&session_dir).unwrap();
        fs::write(
            session_dir.join("session.json"),
            serde_json::to_string_pretty(&json!({
                "id": session_id,
                "agent_id": Uuid::new_v4(),
                "address": {
                    "channel_id": "web-main",
                    "conversation_id": "web-default"
                },
                "session_state": {
                    "messages": []
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
        assert!(
            temp_dir
                .path()
                .join("sessions")
                .join("web-default")
                .join("foreground")
                .join(session_id.to_string())
                .join("transcript.jsonl")
                .is_file()
        );
    }

    #[test]
    fn v0_32_workdir_upgrade_backfills_local_mounts() {
        let temp_dir = TempDir::new().unwrap();
        fs::write(temp_dir.path().join("VERSION"), "0.31\n").unwrap();

        let conversation_dir = temp_dir
            .path()
            .join("conversations")
            .join(Uuid::new_v4().to_string());
        let snapshot_dir = temp_dir
            .path()
            .join("snapshots")
            .join(Uuid::new_v4().to_string());
        fs::create_dir_all(&conversation_dir).unwrap();
        fs::create_dir_all(&snapshot_dir).unwrap();
        fs::write(
            conversation_dir.join("conversation.json"),
            serde_json::to_string_pretty(&json!({
                "id": Uuid::new_v4(),
                "address": {
                    "channel_id": "web-main",
                    "conversation_id": "web-default"
                },
                "settings": {
                    "remote_workpaths": []
                }
            }))
            .unwrap(),
        )
        .unwrap();
        fs::write(
            snapshot_dir.join("snapshot.json"),
            serde_json::to_string_pretty(&json!({
                "saved_at": "2026-04-20T00:00:00Z",
                "source_address": {
                    "channel_id": "web-main",
                    "conversation_id": "web-default"
                },
                "settings": {},
                "session": {
                    "messages": []
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
            conversation["settings"]["local_mounts"]
                .as_array()
                .unwrap()
                .len(),
            0
        );
        let snapshot: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(snapshot_dir.join("snapshot.json")).unwrap())
                .unwrap();
        assert_eq!(
            snapshot["settings"]["local_mounts"]
                .as_array()
                .unwrap()
                .len(),
            0
        );
    }

    #[test]
    fn v0_33_workdir_upgrade_moves_sessions_under_conversation_and_kind() {
        let temp_dir = TempDir::new().unwrap();
        fs::write(temp_dir.path().join("VERSION"), "0.32\n").unwrap();

        let foreground_id = Uuid::new_v4();
        let background_id = Uuid::new_v4();
        let sessions_root = temp_dir.path().join("sessions");
        for (session_id, kind) in [(foreground_id, "foreground"), (background_id, "background")] {
            let session_dir = sessions_root.join(session_id.to_string());
            fs::create_dir_all(&session_dir).unwrap();
            fs::write(
                session_dir.join("session.json"),
                serde_json::to_string_pretty(&json!({
                    "kind": kind,
                    "id": session_id,
                    "agent_id": Uuid::new_v4(),
                    "address": {
                        "channel_id": "telegram-main",
                        "conversation_id": "-100/test"
                    },
                    "session_state": {
                        "messages": []
                    }
                }))
                .unwrap(),
            )
            .unwrap();
            fs::write(session_dir.join("transcript.jsonl"), "").unwrap();
        }

        let upgraded = upgrade_workdir(temp_dir.path()).unwrap();

        assert!(upgraded);
        assert_eq!(
            fs::read_to_string(temp_dir.path().join("VERSION"))
                .unwrap()
                .trim(),
            LATEST_WORKDIR_VERSION
        );
        assert!(
            sessions_root
                .join("-100%2Ftest")
                .join("foreground")
                .join(foreground_id.to_string())
                .join("session.json")
                .is_file()
        );
        assert!(
            sessions_root
                .join("-100%2Ftest")
                .join("background")
                .join(background_id.to_string())
                .join("transcript.jsonl")
                .is_file()
        );
        assert!(!sessions_root.join(foreground_id.to_string()).exists());
        assert!(!sessions_root.join(background_id.to_string()).exists());
    }

    #[test]
    fn v0_34_workdir_upgrade_normalizes_persisted_inline_images() {
        let temp_dir = TempDir::new().unwrap();
        fs::write(temp_dir.path().join("VERSION"), "0.33\n").unwrap();

        let session_dir = temp_dir
            .path()
            .join("sessions")
            .join("conversation-1")
            .join("foreground")
            .join(Uuid::new_v4().to_string());
        let snapshot_dir = temp_dir.path().join("snapshots").join("snap-1");
        fs::create_dir_all(&session_dir).unwrap();
        fs::create_dir_all(&snapshot_dir).unwrap();

        let image = image::ImageBuffer::from_pixel(1, 1, image::Rgba([12, 34, 56, 255]));
        let mut tiff = Vec::new();
        image::DynamicImage::ImageRgba8(image)
            .write_to(
                &mut std::io::Cursor::new(&mut tiff),
                image::ImageFormat::Tiff,
            )
            .unwrap();
        let encoded_tiff = base64::engine::general_purpose::STANDARD.encode(tiff);

        fs::write(
            session_dir.join("session.json"),
            serde_json::to_string_pretty(&json!({
                "id": Uuid::new_v4(),
                "agent_id": Uuid::new_v4(),
                "address": {
                    "channel_id": "telegram-main",
                    "conversation_id": "conversation-1"
                },
                "session_state": {
                    "messages": [{
                        "role": "user",
                        "content": [
                            {"type": "text", "text": "look"},
                            {"type": "image_url", "image_url": {"url": format!("data:image/tiff;base64,{encoded_tiff}")}}
                        ]
                    }],
                    "pending_messages": [],
                    "user_mailbox": [{
                        "pending_message": {
                            "role": "user",
                            "content": [
                                {"type": "text", "text": "bad"},
                                {"type": "image_url", "image_url": {"url": "data:image/tiff;base64,AAAA"}}
                            ]
                        }
                    }]
                }
            }))
            .unwrap(),
        )
        .unwrap();

        fs::write(
            snapshot_dir.join("snapshot.json"),
            serde_json::to_string_pretty(&json!({
                "saved_at": "2026-04-22T00:00:00Z",
                "session": {
                    "messages": [{
                        "role": "user",
                        "content": [
                            {"type": "text", "text": "snapshot"},
                            {"type": "image_url", "image_url": {"url": format!("data:image/tiff;base64,{encoded_tiff}")}}
                        ]
                    }],
                    "pending_messages": [],
                    "user_mailbox": []
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

        let session: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(session_dir.join("session.json")).unwrap())
                .unwrap();
        let session_url = session["session_state"]["messages"][0]["content"][1]["image_url"]["url"]
            .as_str()
            .unwrap();
        assert!(session_url.starts_with("data:image/png;base64,"));
        let user_mailbox_item =
            &session["session_state"]["user_mailbox"][0]["pending_message"]["content"][1];
        assert_eq!(user_mailbox_item["type"], "text");
        assert!(
            user_mailbox_item["text"]
                .as_str()
                .is_some_and(|text| text.contains("could not be converted"))
        );

        let snapshot: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(snapshot_dir.join("snapshot.json")).unwrap())
                .unwrap();
        let snapshot_url = snapshot["session"]["messages"][0]["content"][1]["image_url"]["url"]
            .as_str()
            .unwrap();
        assert!(snapshot_url.starts_with("data:image/png;base64,"));
    }

    #[test]
    fn v0_35_workdir_upgrade_repairs_non_image_media_types_in_image_slots() {
        let temp_dir = TempDir::new().unwrap();
        fs::write(temp_dir.path().join("VERSION"), "0.34\n").unwrap();

        let session_dir = temp_dir
            .path()
            .join("sessions")
            .join("conversation-1")
            .join("foreground")
            .join(Uuid::new_v4().to_string());
        let snapshot_dir = temp_dir.path().join("snapshots").join("snap-1");
        fs::create_dir_all(&session_dir).unwrap();
        fs::create_dir_all(&snapshot_dir).unwrap();

        let image = image::ImageBuffer::from_pixel(1, 1, image::Rgba([12, 34, 56, 255]));
        let mut tiff = Vec::new();
        image::DynamicImage::ImageRgba8(image)
            .write_to(
                &mut std::io::Cursor::new(&mut tiff),
                image::ImageFormat::Tiff,
            )
            .unwrap();
        let encoded_tiff = base64::engine::general_purpose::STANDARD.encode(tiff);
        let encoded_pdf = base64::engine::general_purpose::STANDARD.encode(b"%PDF-1.4\n");

        fs::write(
            session_dir.join("session.json"),
            serde_json::to_string_pretty(&json!({
                "id": Uuid::new_v4(),
                "agent_id": Uuid::new_v4(),
                "address": {
                    "channel_id": "telegram-main",
                    "conversation_id": "conversation-1"
                },
                "session_state": {
                    "messages": [{
                        "role": "user",
                        "content": [
                            {"type": "input_image", "image_url": format!("data:application/octet-stream;base64,{encoded_tiff}")},
                            {"type": "input_image", "image_url": format!("data:application/octet-stream;base64,{encoded_pdf}")}
                        ]
                    }],
                    "pending_messages": [],
                    "user_mailbox": []
                }
            }))
            .unwrap(),
        )
        .unwrap();

        fs::write(
            snapshot_dir.join("snapshot.json"),
            serde_json::to_string_pretty(&json!({
                "saved_at": "2026-04-22T00:00:00Z",
                "session": {
                    "messages": [{
                        "role": "user",
                        "content": [
                            {"type": "input_image", "image_url": format!("data:application/octet-stream;base64,{encoded_tiff}")}
                        ]
                    }],
                    "pending_messages": [],
                    "user_mailbox": []
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

        let session: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(session_dir.join("session.json")).unwrap())
                .unwrap();
        let session_items = session["session_state"]["messages"][0]["content"]
            .as_array()
            .unwrap();
        assert_eq!(session_items[0]["type"], "input_image");
        assert!(
            session_items[0]["image_url"]
                .as_str()
                .is_some_and(|url| url.starts_with("data:image/png;base64,"))
        );
        assert_eq!(session_items[1]["type"], "text");
        assert!(
            session_items[1]["text"]
                .as_str()
                .is_some_and(|text| text.contains("could not be converted"))
        );

        let snapshot: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(snapshot_dir.join("snapshot.json")).unwrap())
                .unwrap();
        let snapshot_url = snapshot["session"]["messages"][0]["content"][0]["image_url"]
            .as_str()
            .unwrap();
        assert!(snapshot_url.starts_with("data:image/png;base64,"));
    }

    #[test]
    fn v0_36_workdir_upgrade_backfills_workspace_shared_links() {
        let temp_dir = TempDir::new().unwrap();
        fs::write(temp_dir.path().join("VERSION"), "0.35\n").unwrap();

        let workspace_files = temp_dir
            .path()
            .join("workspaces")
            .join("ws-1")
            .join("files");
        fs::create_dir_all(workspace_files.join("shared/nested")).unwrap();
        fs::write(
            workspace_files.join("shared/nested/note.txt"),
            "legacy shared",
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

        let shared_source = temp_dir.path().join("rundir").join("shared");
        let workspace_shared = workspace_files.join("shared");
        assert!(shared_source.is_dir());
        assert!(
            fs::symlink_metadata(&workspace_shared)
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert_eq!(
            fs::read_to_string(shared_source.join("nested/note.txt")).unwrap(),
            "legacy shared"
        );
        assert_eq!(
            fs::read_to_string(workspace_shared.join("nested/note.txt")).unwrap(),
            "legacy shared"
        );
    }

    #[test]
    fn v0_37_workdir_upgrade_normalizes_legacy_token_log_fields() {
        let temp_dir = TempDir::new().unwrap();
        fs::write(temp_dir.path().join("VERSION"), "0.36\n").unwrap();

        let agents_dir = temp_dir.path().join("logs").join("agents");
        let api_dir = temp_dir.path().join("logs").join("api");
        fs::create_dir_all(&agents_dir).unwrap();
        fs::create_dir_all(&api_dir).unwrap();

        fs::write(
            agents_dir.join("agent.jsonl"),
            [
                serde_json::json!({
                    "kind": "turn_token_usage",
                    "prompt_tokens": 120,
                    "completion_tokens": 30,
                    "total_tokens": 150,
                    "cache_read_tokens": 80,
                    "cache_write_tokens": 10,
                    "cache_miss_tokens": 40,
                    "legacy_prompt_tokens": 120,
                    "legacy_completion_tokens": 30,
                    "legacy_total_tokens": 150,
                    "legacy_cache_hit_tokens": 80,
                    "legacy_cache_miss_tokens": 40,
                    "legacy_cache_read_tokens": 80,
                    "legacy_cache_write_tokens": 10
                })
                .to_string(),
                serde_json::json!({
                    "kind": "agent_frame_model_call_completed",
                    "input_total_tokens": 10,
                    "output_total_tokens": 2,
                    "context_total_tokens": 12,
                    "cache_read_input_tokens": 4,
                    "cache_write_input_tokens": 1,
                    "cache_uncached_input_tokens": 6,
                    "normal_billed_input_tokens": 5,
                    "legacy_prompt_tokens": 10,
                    "legacy_completion_tokens": 2
                })
                .to_string(),
            ]
            .join("\n"),
        )
        .unwrap();
        fs::write(
            api_dir.join("api.jsonl"),
            serde_json::json!({
                "kind": "upstream_api_request_completed",
                "prompt_tokens": 70,
                "completion_tokens": 7,
                "total_tokens": 77,
                "cache_read_tokens": 20
            })
            .to_string(),
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

        let agent_lines = fs::read_to_string(agents_dir.join("agent.jsonl")).unwrap();
        let agent_events = agent_lines
            .lines()
            .map(|line| serde_json::from_str::<serde_json::Value>(line).unwrap())
            .collect::<Vec<_>>();
        assert_eq!(agent_events[0]["input_total_tokens"], 120);
        assert_eq!(agent_events[0]["output_total_tokens"], 30);
        assert_eq!(agent_events[0]["context_total_tokens"], 150);
        assert_eq!(agent_events[0]["cache_read_input_tokens"], 80);
        assert_eq!(agent_events[0]["cache_write_input_tokens"], 10);
        assert_eq!(agent_events[0]["cache_uncached_input_tokens"], 40);
        assert_eq!(agent_events[0]["normal_billed_input_tokens"], 30);
        assert!(agent_events[0].get("prompt_tokens").is_none());
        assert!(agent_events[0].get("legacy_prompt_tokens").is_none());
        assert!(agent_events[0].get("legacy_cache_hit_tokens").is_none());
        assert!(agent_events[1].get("legacy_prompt_tokens").is_none());

        let api_event: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(api_dir.join("api.jsonl")).unwrap()).unwrap();
        assert_eq!(api_event["input_total_tokens"], 70);
        assert_eq!(api_event["output_total_tokens"], 7);
        assert_eq!(api_event["context_total_tokens"], 77);
        assert_eq!(api_event["cache_read_input_tokens"], 20);
        assert_eq!(api_event["cache_uncached_input_tokens"], 50);
        assert_eq!(api_event["normal_billed_input_tokens"], 50);
        assert!(api_event.get("prompt_tokens").is_none());
        assert!(api_event.get("cache_read_tokens").is_none());
    }

    #[test]
    fn v0_38_workdir_upgrade_rewrites_inline_multimodal_messages_to_workspace_paths() {
        let temp_dir = TempDir::new().unwrap();
        fs::write(temp_dir.path().join("VERSION"), "0.37\n").unwrap();

        let workspace_root = temp_dir
            .path()
            .join("workspaces")
            .join("workspace-1")
            .join("files");
        fs::create_dir_all(&workspace_root).unwrap();

        let session_dir = temp_dir
            .path()
            .join("sessions")
            .join("conversation-1")
            .join("foreground")
            .join(Uuid::new_v4().to_string());
        fs::create_dir_all(&session_dir).unwrap();
        let encoded_image = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVQIHWP4//8/AwAI/AL+XxYl3wAAAABJRU5ErkJggg==";
        fs::write(
            session_dir.join("session.json"),
            serde_json::to_string_pretty(&json!({
                "kind": "foreground",
                "id": Uuid::new_v4(),
                "agent_id": Uuid::new_v4(),
                "address": {
                    "channel_id": "telegram-main",
                    "conversation_id": "conversation-1",
                    "user_id": "user-1",
                    "display_name": "User"
                },
                "workspace_id": "workspace-1",
                "session_state": {
                    "messages": [{
                        "role": "user",
                        "content": [{
                            "type": "input_image",
                            "image_url": format!("data:image/png;base64,{encoded_image}")
                        }]
                    }],
                    "pending_messages": [],
                    "user_mailbox": []
                }
            }))
            .unwrap(),
        )
        .unwrap();

        let snapshot_dir = temp_dir.path().join("snapshots").join("demo");
        fs::create_dir_all(snapshot_dir.join("workspace")).unwrap();
        fs::write(
            snapshot_dir.join("snapshot.json"),
            serde_json::to_string_pretty(&json!({
                "saved_at": "2026-04-22T00:00:00Z",
                "source_address": {
                    "channel_id": "telegram-main",
                    "conversation_id": "conversation-1",
                    "user_id": "user-1",
                    "display_name": "User"
                },
                "settings": {
                    "workspace_id": "workspace-1"
                },
                "session": {
                    "messages": [{
                        "role": "assistant",
                        "content": [{
                            "type": "image_url",
                            "image_url": {
                                "url": format!("data:image/png;base64,{encoded_image}")
                            }
                        }]
                    }],
                    "pending_messages": [],
                    "user_mailbox": []
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

        let session: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(session_dir.join("session.json")).unwrap())
                .unwrap();
        let session_item = &session["session_state"]["messages"][0]["content"][0];
        assert_eq!(session_item["type"], "input_image");
        let session_path = session_item["path"].as_str().unwrap();
        assert!(session_path.starts_with("media/legacy/by-hash/image-"));
        assert!(workspace_root.join(session_path).is_file());

        let snapshot: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(snapshot_dir.join("snapshot.json")).unwrap())
                .unwrap();
        let snapshot_item = &snapshot["session"]["messages"][0]["content"][0];
        assert_eq!(snapshot_item["type"], "output_image");
        let snapshot_path = snapshot_item["path"].as_str().unwrap();
        assert!(snapshot_path.starts_with("media/legacy/by-hash/image-"));
        assert!(snapshot_dir.join("workspace").join(snapshot_path).is_file());
    }
}
