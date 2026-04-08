use super::WorkdirUpgrader;
use crate::backend::AgentBackendKind;
use crate::config::SandboxMode;
use crate::domain::ChannelAddress;
use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::fs;
use std::path::Path;
use uuid::Uuid;

pub(super) struct Upgrade;

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
struct ConversationSettings {
    #[serde(default)]
    agent_backend: Option<AgentBackendKind>,
    #[serde(default)]
    main_model: Option<String>,
    #[serde(default)]
    sandbox_mode: Option<SandboxMode>,
    #[serde(default)]
    reasoning_effort: Option<String>,
    #[serde(default)]
    context_compaction_enabled: Option<bool>,
    #[serde(default)]
    workspace_id: Option<String>,
    #[serde(default = "Uuid::new_v4")]
    chat_version_id: Uuid,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct PersistedConversation {
    id: Uuid,
    address: ChannelAddress,
    #[serde(default)]
    settings: ConversationSettings,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct SnapshotRecord {
    name: String,
    saved_at: DateTime<Utc>,
    source_channel_id: String,
    source_conversation_id: String,
    #[serde(default)]
    agent_backend: Option<AgentBackendKind>,
    #[serde(default)]
    main_model: Option<String>,
    #[serde(default)]
    sandbox_mode: Option<SandboxMode>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct SnapshotBundle {
    saved_at: DateTime<Utc>,
    source_address: ChannelAddress,
    settings: ConversationSettings,
    session: Value,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct CronTaskRecord {
    id: Uuid,
    name: String,
    description: String,
    schedule: String,
    #[serde(default)]
    agent_backend: AgentBackendKind,
    model_key: String,
    prompt: String,
    sink: Value,
    address: ChannelAddress,
    #[serde(default)]
    enabled: bool,
    #[serde(default)]
    checker: Option<Value>,
    created_at: DateTime<Utc>,
    updated_at: DateTime<Utc>,
    #[serde(default)]
    last_scheduled_for: Option<DateTime<Utc>>,
    #[serde(default)]
    last_checked_at: Option<DateTime<Utc>>,
    #[serde(default)]
    last_check_outcome: Option<String>,
    #[serde(default)]
    last_triggered_at: Option<DateTime<Utc>>,
    #[serde(default)]
    last_trigger_outcome: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct CronStoreFile {
    #[serde(default)]
    tasks: Vec<CronTaskRecord>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct PendingContinueState {
    #[serde(default)]
    agent_backend: Option<AgentBackendKind>,
    model_key: String,
    #[serde(default)]
    resume_messages: Vec<Value>,
    #[serde(default)]
    original_user_text: Option<String>,
    #[serde(default)]
    original_attachments: Vec<Value>,
    #[serde(default)]
    error_summary: String,
    #[serde(default)]
    progress_summary: String,
    failed_at: DateTime<Utc>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct PersistedSession {
    #[serde(default)]
    pending_continue: Option<PendingContinueState>,
    #[serde(flatten)]
    extra: Value,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct PersistedSubagentState {
    id: Uuid,
    parent_agent_id: Uuid,
    session_id: Uuid,
    channel_id: String,
    conversation_id: String,
    workspace_id: String,
    #[serde(default)]
    agent_backend: AgentBackendKind,
    model_key: String,
    #[serde(flatten)]
    extra: Value,
}

impl WorkdirUpgrader for Upgrade {
    fn from_version(&self) -> &'static str {
        "0.8"
    }

    fn to_version(&self) -> &'static str {
        "0.9"
    }

    fn upgrade(&self, workdir: &Path) -> Result<()> {
        rewrite_json_files(
            &workdir.join("conversations"),
            "conversation.json",
            |path| rewrite_json_file::<PersistedConversation>(path),
        )?;
        rewrite_json_files(&workdir.join("snapshots"), "metadata.json", |path| {
            rewrite_json_file::<SnapshotRecord>(path)
        })?;
        rewrite_json_files(&workdir.join("snapshots"), "snapshot.json", |path| {
            rewrite_json_file::<SnapshotBundle>(path)
        })?;
        let cron_store = workdir.join("cron").join("tasks.json");
        if cron_store.is_file() {
            rewrite_json_file::<CronStoreFile>(&cron_store)?;
        }
        rewrite_json_files(&workdir.join("sessions"), "session.json", |path| {
            rewrite_json_file::<PersistedSession>(path)
        })?;
        let runtime_root = workdir.join("agent").join("runtime");
        if runtime_root.exists() {
            for workspace_entry in fs::read_dir(&runtime_root)
                .with_context(|| format!("failed to read {}", runtime_root.display()))?
            {
                let subagents_dir = workspace_entry?
                    .path()
                    .join("agent_frame")
                    .join("subagents");
                if !subagents_dir.is_dir() {
                    continue;
                }
                for subagent_entry in fs::read_dir(&subagents_dir)
                    .with_context(|| format!("failed to read {}", subagents_dir.display()))?
                {
                    let path = subagent_entry?.path();
                    if path.extension().and_then(|value| value.to_str()) != Some("json") {
                        continue;
                    }
                    rewrite_json_file::<PersistedSubagentState>(&path)?;
                }
            }
        }
        Ok(())
    }
}

fn rewrite_json_files(
    root: &Path,
    file_name: &str,
    mut rewrite: impl FnMut(&Path) -> Result<()>,
) -> Result<()> {
    if !root.exists() {
        return Ok(());
    }
    for entry in fs::read_dir(root).with_context(|| format!("failed to read {}", root.display()))? {
        let path = entry?.path().join(file_name);
        if path.is_file() {
            rewrite(&path)?;
        }
    }
    Ok(())
}

fn rewrite_json_file<T>(path: &Path) -> Result<()>
where
    T: for<'de> Deserialize<'de> + Serialize,
{
    let raw =
        fs::read_to_string(path).with_context(|| format!("failed to read {}", path.display()))?;
    let value: T = serde_json::from_str(&raw)
        .with_context(|| format!("failed to parse {}", path.display()))?;
    let updated = serde_json::to_string_pretty(&value)
        .with_context(|| format!("failed to serialize {}", path.display()))?;
    fs::write(path, updated).with_context(|| format!("failed to write {}", path.display()))
}
