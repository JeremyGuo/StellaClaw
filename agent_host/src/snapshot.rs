use crate::conversation::ConversationSettings;
use crate::domain::ChannelAddress;
use crate::session::SessionCheckpointData;
use anyhow::{Context, Result, anyhow};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SnapshotRecord {
    pub name: String,
    pub saved_at: DateTime<Utc>,
    pub source_channel_id: String,
    pub source_conversation_id: String,
    #[serde(default)]
    pub main_model: Option<String>,
    #[serde(default)]
    pub sandbox_mode: Option<crate::config::SandboxMode>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SnapshotBundle {
    pub saved_at: DateTime<Utc>,
    pub source_address: ChannelAddress,
    pub settings: ConversationSettings,
    pub session: SessionCheckpointData,
}

#[derive(Clone, Debug)]
pub struct LoadedSnapshot {
    pub record: SnapshotRecord,
    pub workspace_dir: PathBuf,
    pub conversation_memory_dir: Option<PathBuf>,
    pub bundle: SnapshotBundle,
}

#[derive(Clone, Debug)]
struct SnapshotState {
    root_dir: PathBuf,
    record: SnapshotRecord,
}

impl SnapshotState {
    fn metadata_path(&self) -> PathBuf {
        self.root_dir.join("metadata.json")
    }

    fn persist(&self) -> Result<()> {
        let raw = serde_json::to_string_pretty(&self.record)
            .context("failed to serialize snapshot metadata")?;
        fs::write(self.metadata_path(), raw)
            .with_context(|| format!("failed to write {}", self.metadata_path().display()))
    }
}

pub struct SnapshotManager {
    snapshots_root: PathBuf,
    snapshots: HashMap<String, SnapshotState>,
}

impl SnapshotManager {
    pub fn new(workdir: impl AsRef<Path>) -> Result<Self> {
        let snapshots_root = workdir.as_ref().join("snapshots");
        fs::create_dir_all(&snapshots_root)
            .with_context(|| format!("failed to create {}", snapshots_root.display()))?;
        let snapshots = load_persisted_snapshots(&snapshots_root)?;
        Ok(Self {
            snapshots_root,
            snapshots,
        })
    }

    pub fn save_snapshot(
        &mut self,
        address: &ChannelAddress,
        snapshot_name: &str,
        bundle: SnapshotBundle,
        workspace_root: &Path,
        conversation_memory_root: Option<&Path>,
    ) -> Result<SnapshotRecord> {
        let sanitized_name = sanitize_snapshot_name(snapshot_name)?;
        let snapshot_dir = self.snapshots_root.join(&sanitized_name);
        if snapshot_dir.exists() {
            fs::remove_dir_all(&snapshot_dir).with_context(|| {
                format!(
                    "failed to replace existing snapshot {}",
                    snapshot_dir.display()
                )
            })?;
        }
        fs::create_dir_all(&snapshot_dir)
            .with_context(|| format!("failed to create {}", snapshot_dir.display()))?;
        let workspace_dir = snapshot_dir.join("workspace");
        copy_dir_recursive(workspace_root, &workspace_dir)?;
        if let Some(memory_root) = conversation_memory_root.filter(|path| path.is_dir()) {
            let memory_dir = snapshot_dir.join("conversation_memory");
            copy_dir_recursive(memory_root, &memory_dir)?;
        }
        let bundle_path = snapshot_dir.join("snapshot.json");
        let raw = serde_json::to_string_pretty(&bundle).context("failed to serialize snapshot")?;
        fs::write(&bundle_path, raw)
            .with_context(|| format!("failed to write {}", bundle_path.display()))?;

        let record = SnapshotRecord {
            name: sanitized_name.clone(),
            saved_at: bundle.saved_at,
            source_channel_id: address.channel_id.clone(),
            source_conversation_id: address.conversation_id.clone(),
            main_model: bundle.settings.main_model.clone(),
            sandbox_mode: bundle.settings.sandbox_mode,
        };
        let state = SnapshotState {
            root_dir: snapshot_dir,
            record: record.clone(),
        };
        state.persist()?;
        self.snapshots.insert(sanitized_name, state);
        Ok(record)
    }

    pub fn list_snapshots(&self) -> Vec<SnapshotRecord> {
        let mut records = self
            .snapshots
            .values()
            .map(|state| state.record.clone())
            .collect::<Vec<_>>();
        records.sort_by(|left, right| left.name.cmp(&right.name));
        records
    }

    pub fn load_snapshot(&self, snapshot_name: &str) -> Result<LoadedSnapshot> {
        let sanitized_name = sanitize_snapshot_name(snapshot_name)?;
        let state = self
            .snapshots
            .get(&sanitized_name)
            .ok_or_else(|| anyhow!("snapshot `{}` not found", sanitized_name))?;
        let bundle_path = state.root_dir.join("snapshot.json");
        let workspace_dir = state.root_dir.join("workspace");
        let conversation_memory_dir = state.root_dir.join("conversation_memory");
        let raw = fs::read_to_string(&bundle_path)
            .with_context(|| format!("failed to read {}", bundle_path.display()))?;
        let bundle: SnapshotBundle =
            serde_json::from_str(&raw).context("failed to parse snapshot bundle")?;
        if !workspace_dir.is_dir() {
            return Err(anyhow!(
                "snapshot workspace directory is missing: {}",
                workspace_dir.display()
            ));
        }
        Ok(LoadedSnapshot {
            record: state.record.clone(),
            workspace_dir,
            conversation_memory_dir: conversation_memory_dir
                .is_dir()
                .then_some(conversation_memory_dir),
            bundle,
        })
    }
}

fn load_persisted_snapshots(root: &Path) -> Result<HashMap<String, SnapshotState>> {
    let mut snapshots = HashMap::new();
    for entry in fs::read_dir(root).with_context(|| format!("failed to read {}", root.display()))? {
        let entry = entry?;
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let metadata_path = path.join("metadata.json");
        if !metadata_path.is_file() {
            continue;
        }
        let raw = fs::read_to_string(&metadata_path)
            .with_context(|| format!("failed to read {}", metadata_path.display()))?;
        let record: SnapshotRecord =
            serde_json::from_str(&raw).context("failed to parse snapshot metadata")?;
        snapshots.insert(
            record.name.clone(),
            SnapshotState {
                root_dir: path,
                record,
            },
        );
    }
    Ok(snapshots)
}

fn sanitize_snapshot_name(name: &str) -> Result<String> {
    let trimmed = name.trim();
    if trimmed.is_empty() {
        return Err(anyhow!("snapshot name must not be empty"));
    }
    let sanitized: String = trimmed
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '.') {
                ch
            } else {
                '_'
            }
        })
        .collect();
    let sanitized = sanitized.trim_matches('_').to_string();
    if sanitized.is_empty() {
        return Err(anyhow!(
            "snapshot name must contain at least one safe character"
        ));
    }
    Ok(sanitized)
}

fn copy_dir_recursive(source: &Path, target: &Path) -> Result<()> {
    fs::create_dir_all(target).with_context(|| format!("failed to create {}", target.display()))?;
    for entry in
        fs::read_dir(source).with_context(|| format!("failed to read {}", source.display()))?
    {
        let entry = entry?;
        let source_path = entry.path();
        let target_path = target.join(entry.file_name());
        let file_type = entry
            .file_type()
            .with_context(|| format!("failed to inspect {}", source_path.display()))?;
        if file_type.is_dir() {
            copy_dir_recursive(&source_path, &target_path)?;
        } else if file_type.is_symlink() {
            let target_link = fs::read_link(&source_path)
                .with_context(|| format!("failed to read link {}", source_path.display()))?;
            create_symlink(&target_link, &target_path)?;
        } else {
            fs::copy(&source_path, &target_path).with_context(|| {
                format!(
                    "failed to copy {} to {}",
                    source_path.display(),
                    target_path.display()
                )
            })?;
        }
    }
    Ok(())
}

fn create_symlink(source: &Path, target: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(source, target)
            .with_context(|| format!("failed to create symlink {}", target.display()))
    }
    #[cfg(windows)]
    {
        let metadata = fs::metadata(source)
            .with_context(|| format!("failed to stat symlink target {}", source.display()))?;
        if metadata.is_dir() {
            std::os::windows::fs::symlink_dir(source, target)
                .with_context(|| format!("failed to create symlink {}", target.display()))
        } else {
            std::os::windows::fs::symlink_file(source, target)
                .with_context(|| format!("failed to create symlink {}", target.display()))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{SnapshotBundle, SnapshotManager};
    use crate::domain::ChannelAddress;
    use crate::session::SessionCheckpointData;
    use chrono::Utc;
    use std::fs;
    use tempfile::TempDir;

    fn test_address(conversation_id: &str) -> ChannelAddress {
        ChannelAddress {
            channel_id: "telegram-main".to_string(),
            conversation_id: conversation_id.to_string(),
            user_id: Some("user-1".to_string()),
            display_name: Some("Test User".to_string()),
        }
    }

    #[test]
    fn saves_and_loads_global_snapshot_bundle() {
        let temp_dir = TempDir::new().unwrap();
        let address = test_address("conversation-1");
        let mut manager = SnapshotManager::new(temp_dir.path()).unwrap();

        let workspace_root = temp_dir.path().join("workspace");
        fs::create_dir_all(&workspace_root).unwrap();
        fs::write(workspace_root.join("note.txt"), "hello").unwrap();

        let bundle = SnapshotBundle {
            saved_at: Utc::now(),
            source_address: address.clone(),
            settings: Default::default(),
            session: SessionCheckpointData {
                turn_count: 3,
                ..Default::default()
            },
        };
        manager
            .save_snapshot(&address, "demo", bundle, &workspace_root, None)
            .unwrap();

        let loaded = manager.load_snapshot("demo").unwrap();
        assert_eq!(loaded.record.name, "demo");
        assert_eq!(loaded.bundle.session.turn_count, 3);
        assert_eq!(
            fs::read_to_string(loaded.workspace_dir.join("note.txt")).unwrap(),
            "hello"
        );
    }

    #[test]
    fn lists_snapshots_across_source_conversations() {
        let temp_dir = TempDir::new().unwrap();
        let mut manager = SnapshotManager::new(temp_dir.path()).unwrap();
        let workspace_root = temp_dir.path().join("workspace");
        fs::create_dir_all(&workspace_root).unwrap();
        fs::write(workspace_root.join("note.txt"), "hello").unwrap();

        for (name, conversation_id) in [("alpha", "conversation-1"), ("beta", "conversation-2")] {
            let address = test_address(conversation_id);
            let bundle = SnapshotBundle {
                saved_at: Utc::now(),
                source_address: address.clone(),
                settings: Default::default(),
                session: SessionCheckpointData::default(),
            };
            manager
                .save_snapshot(&address, name, bundle, &workspace_root, None)
                .unwrap();
        }

        let names = manager
            .list_snapshots()
            .into_iter()
            .map(|record| record.name)
            .collect::<Vec<_>>();
        assert_eq!(names, vec!["alpha".to_string(), "beta".to_string()]);
    }

    #[test]
    fn saves_and_loads_conversation_memory_artifacts() {
        let temp_dir = TempDir::new().unwrap();
        let address = test_address("conversation-memory");
        let mut manager = SnapshotManager::new(temp_dir.path()).unwrap();

        let workspace_root = temp_dir.path().join("workspace");
        fs::create_dir_all(&workspace_root).unwrap();
        fs::write(workspace_root.join("note.txt"), "hello").unwrap();

        let conversation_memory_root = temp_dir.path().join("conversation_memory");
        fs::create_dir_all(conversation_memory_root.join("rollouts/rollout_0001")).unwrap();
        fs::write(
            conversation_memory_root.join("memory_summary.json"),
            "{\"routes\":[]}",
        )
        .unwrap();
        fs::write(
            conversation_memory_root.join("rollouts/rollout_0001/rollout_summary.json"),
            "{\"summary\":\"demo\"}",
        )
        .unwrap();

        let bundle = SnapshotBundle {
            saved_at: Utc::now(),
            source_address: address.clone(),
            settings: Default::default(),
            session: SessionCheckpointData::default(),
        };
        manager
            .save_snapshot(
                &address,
                "with-memory",
                bundle,
                &workspace_root,
                Some(&conversation_memory_root),
            )
            .unwrap();

        let loaded = manager.load_snapshot("with-memory").unwrap();
        let loaded_memory_dir = loaded.conversation_memory_dir.unwrap();
        assert_eq!(
            fs::read_to_string(loaded_memory_dir.join("memory_summary.json")).unwrap(),
            "{\"routes\":[]}"
        );
        assert_eq!(
            fs::read_to_string(
                loaded_memory_dir.join("rollouts/rollout_0001/rollout_summary.json")
            )
            .unwrap(),
            "{\"summary\":\"demo\"}"
        );
    }
}
