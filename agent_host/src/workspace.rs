use anyhow::{Context, Result, anyhow};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use uuid::Uuid;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct WorkspaceRecord {
    pub id: String,
    pub title: String,
    pub summary: String,
    pub state: String,
    pub root_dir: PathBuf,
    pub files_dir: PathBuf,
    pub mounts_dir: PathBuf,
    pub host_dir: PathBuf,
    pub summary_path: PathBuf,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub last_content_modified_at: DateTime<Utc>,
    pub last_main_agent_id: Option<Uuid>,
    pub last_session_id: Option<Uuid>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct WorkspaceMountRecord {
    pub owner_workspace_id: String,
    pub source_workspace_id: String,
    pub mount_name: String,
    pub mode: String,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WorkspaceMountMaterialization {
    HostSymlink,
    SandboxPlaceholder,
    HostSnapshotCopy,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct WorkspaceContentEntry {
    pub path: String,
    pub entry_type: String,
    pub size_bytes: Option<u64>,
    pub modified_at: Option<DateTime<Utc>>,
}

#[derive(Clone)]
pub struct WorkspaceManager {
    workspaces_root: PathBuf,
    meta_root: PathBuf,
    template_root: PathBuf,
    registry_path: PathBuf,
    registry: Arc<Mutex<WorkspaceRegistry>>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
struct WorkspaceRegistry {
    #[serde(default)]
    workspaces: Vec<StoredWorkspaceRecord>,
    #[serde(default)]
    mounts: Vec<StoredWorkspaceMountRecord>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct StoredWorkspaceRecord {
    id: String,
    title: String,
    summary: String,
    state: String,
    created_at: DateTime<Utc>,
    updated_at: DateTime<Utc>,
    last_content_modified_at: DateTime<Utc>,
    last_main_agent_id: Option<Uuid>,
    last_session_id: Option<Uuid>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct StoredWorkspaceMountRecord {
    owner_workspace_id: String,
    source_workspace_id: String,
    mount_name: String,
    mode: String,
    created_at: DateTime<Utc>,
    updated_at: DateTime<Utc>,
}

impl WorkspaceManager {
    pub fn load_or_create(workdir: impl AsRef<Path>) -> Result<Self> {
        let workdir = workdir.as_ref();
        let sandbox_dir = workdir.join("sandbox");
        let meta_root = sandbox_dir.join("workspace_meta");
        let workspaces_root = workdir.join("workspaces");
        let template_root = workdir.join("rundir");
        let registry_path = sandbox_dir.join("workspaces.json");
        fs::create_dir_all(&sandbox_dir)
            .with_context(|| format!("failed to create {}", sandbox_dir.display()))?;
        fs::create_dir_all(&meta_root)
            .with_context(|| format!("failed to create {}", meta_root.display()))?;
        fs::create_dir_all(&workspaces_root)
            .with_context(|| format!("failed to create {}", workspaces_root.display()))?;

        let registry = if registry_path.exists() {
            let raw = fs::read_to_string(&registry_path)
                .with_context(|| format!("failed to read {}", registry_path.display()))?;
            if raw.trim().is_empty() {
                WorkspaceRegistry::default()
            } else {
                serde_json::from_str(&raw).with_context(|| {
                    format!(
                        "failed to parse workspace registry {}",
                        registry_path.display()
                    )
                })?
            }
        } else {
            let registry = WorkspaceRegistry::default();
            let raw = serde_json::to_string_pretty(&registry)
                .context("failed to serialize initial workspace registry")?;
            fs::write(&registry_path, raw)
                .with_context(|| format!("failed to write {}", registry_path.display()))?;
            registry
        };

        Ok(Self {
            workspaces_root,
            meta_root,
            template_root,
            registry_path,
            registry: Arc::new(Mutex::new(registry)),
        })
    }

    pub fn create_workspace(
        &self,
        main_agent_id: Uuid,
        session_id: Uuid,
        title: Option<&str>,
    ) -> Result<WorkspaceRecord> {
        let id = Uuid::new_v4().to_string();
        let record = self.workspace_record(
            id.clone(),
            title
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .map(ToOwned::to_owned)
                .unwrap_or_else(|| format!("workspace-{}", &id[..8])),
            "New workspace with no summary yet.".to_string(),
            "active".to_string(),
            Utc::now(),
            Utc::now(),
            Utc::now(),
            Some(main_agent_id),
            Some(session_id),
        );
        self.ensure_workspace_dirs(&record)?;
        self.seed_workspace_from_template(&record)?;
        self.persist_record(&record)?;
        Ok(record)
    }

    pub fn get_workspace(&self, id: &str) -> Result<Option<WorkspaceRecord>> {
        let registry = self
            .registry
            .lock()
            .map_err(|_| anyhow!("workspace registry lock poisoned"))?;
        Ok(registry
            .workspaces
            .iter()
            .find(|record| record.id == id)
            .map(|record| self.workspace_record_from_stored(record.clone())))
    }

    pub fn ensure_workspace_exists(&self, id: &str) -> Result<WorkspaceRecord> {
        let Some(record) = self.get_workspace(id)? else {
            return Err(anyhow!("workspace {} not found", id));
        };
        self.ensure_workspace_dirs(&record)?;
        Ok(record)
    }

    pub fn reactivate_workspace(&self, id: &str) -> Result<WorkspaceRecord> {
        let mut registry = self
            .registry
            .lock()
            .map_err(|_| anyhow!("workspace registry lock poisoned"))?;
        let record = registry
            .workspaces
            .iter_mut()
            .find(|record| record.id == id)
            .ok_or_else(|| anyhow!("workspace {} not found", id))?;
        record.state = "active".to_string();
        record.updated_at = Utc::now();
        let materialized = self.workspace_record_from_stored(record.clone());
        self.ensure_workspace_dirs(&materialized)?;
        self.persist_registry(&registry)?;
        Ok(materialized)
    }

    pub fn list_workspaces(
        &self,
        query: Option<&str>,
        include_archived: bool,
    ) -> Result<Vec<WorkspaceRecord>> {
        let registry = self
            .registry
            .lock()
            .map_err(|_| anyhow!("workspace registry lock poisoned"))?;
        let query = query
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(|value| value.to_ascii_lowercase());
        let mut records = registry
            .workspaces
            .iter()
            .filter(|record| include_archived || record.state != "archived")
            .filter(|record| {
                if let Some(query) = &query {
                    record.id.to_ascii_lowercase().contains(query)
                        || record.title.to_ascii_lowercase().contains(query)
                        || record.summary.to_ascii_lowercase().contains(query)
                } else {
                    true
                }
            })
            .cloned()
            .map(|record| self.workspace_record_from_stored(record))
            .collect::<Vec<_>>();
        records.sort_by(|left, right| right.updated_at.cmp(&left.updated_at));
        Ok(records)
    }

    pub fn list_workspace_contents(
        &self,
        workspace_id: &str,
        relative_path: Option<&str>,
        depth: usize,
        limit: usize,
    ) -> Result<Vec<WorkspaceContentEntry>> {
        let workspace = self.ensure_workspace_exists(workspace_id)?;
        let base_dir = match relative_path
            .map(str::trim)
            .filter(|value| !value.is_empty())
        {
            Some(path) => {
                let candidate = workspace.files_dir.join(path);
                let canonical = candidate
                    .canonicalize()
                    .with_context(|| format!("failed to resolve {}", candidate.display()))?;
                let root = workspace.files_dir.canonicalize().with_context(|| {
                    format!(
                        "failed to resolve workspace root {}",
                        workspace.files_dir.display()
                    )
                })?;
                if !canonical.starts_with(&root) {
                    return Err(anyhow!("path escapes workspace root"));
                }
                canonical
            }
            None => workspace.files_dir.clone(),
        };
        if !base_dir.exists() {
            return Err(anyhow!("path does not exist: {}", base_dir.display()));
        }
        let root = workspace.files_dir.canonicalize().with_context(|| {
            format!(
                "failed to resolve workspace root {}",
                workspace.files_dir.display()
            )
        })?;
        let mut entries = Vec::new();
        collect_workspace_entries(&root, &base_dir, depth, limit, &mut entries)?;
        Ok(entries)
    }

    pub fn update_summary(
        &self,
        workspace_id: &str,
        summary: String,
        title: Option<String>,
    ) -> Result<WorkspaceRecord> {
        let mut registry = self
            .registry
            .lock()
            .map_err(|_| anyhow!("workspace registry lock poisoned"))?;
        let record = registry
            .workspaces
            .iter_mut()
            .find(|record| record.id == workspace_id)
            .ok_or_else(|| anyhow!("workspace {} not found", workspace_id))?;
        let now = Utc::now();
        record.summary = summary.trim().to_string();
        if let Some(title) = title.map(|value| value.trim().to_string())
            && !title.is_empty()
        {
            record.title = title;
        }
        record.updated_at = now;
        let materialized = self.workspace_record_from_stored(record.clone());
        self.ensure_workspace_dirs(&materialized)?;
        fs::write(&materialized.summary_path, materialized.summary.as_bytes()).with_context(
            || {
                format!(
                    "failed to write workspace summary {}",
                    materialized.summary_path.display()
                )
            },
        )?;
        self.persist_registry(&registry)?;
        Ok(materialized)
    }

    pub fn mark_content_modified(&self, workspace_id: &str) -> Result<WorkspaceRecord> {
        let mut registry = self
            .registry
            .lock()
            .map_err(|_| anyhow!("workspace registry lock poisoned"))?;
        let record = registry
            .workspaces
            .iter_mut()
            .find(|record| record.id == workspace_id)
            .ok_or_else(|| anyhow!("workspace {} not found", workspace_id))?;
        let now = Utc::now();
        record.updated_at = now;
        record.last_content_modified_at = now;
        let materialized = self.workspace_record_from_stored(record.clone());
        self.persist_registry(&registry)?;
        Ok(materialized)
    }

    pub fn archive_stale_workspaces(
        &self,
        max_age: chrono::Duration,
        protected_workspace_ids: &[String],
    ) -> Result<Vec<WorkspaceRecord>> {
        let mut registry = self
            .registry
            .lock()
            .map_err(|_| anyhow!("workspace registry lock poisoned"))?;
        let now = Utc::now();
        let protected = protected_workspace_ids
            .iter()
            .map(String::as_str)
            .collect::<Vec<_>>();
        let mut archived = Vec::new();
        for record in &mut registry.workspaces {
            if protected.contains(&record.id.as_str()) {
                continue;
            }
            if record.state == "archived" {
                continue;
            }
            let materialized = self.workspace_record_from_stored(record.clone());
            if let Some(last_modified) = compute_latest_workspace_mtime(&materialized.files_dir)? {
                record.last_content_modified_at = last_modified;
            }
            if now - record.last_content_modified_at >= max_age {
                record.state = "archived".to_string();
                record.updated_at = now;
                archived.push(self.workspace_record_from_stored(record.clone()));
            }
        }
        if !archived.is_empty() {
            self.persist_registry(&registry)?;
        }
        Ok(archived)
    }

    pub fn mount_workspace_snapshot(
        &self,
        owner_workspace_id: &str,
        source_workspace_id: &str,
        mount_name: &str,
        materialization: WorkspaceMountMaterialization,
    ) -> Result<PathBuf> {
        let owner = self.ensure_workspace_exists(owner_workspace_id)?;
        let source = self.ensure_workspace_exists(source_workspace_id)?;
        let mount_path = owner.mounts_dir.join(mount_name);
        self.materialize_mount_path(&mount_path, &source.files_dir, materialization)?;
        let now = Utc::now();
        let mut registry = self
            .registry
            .lock()
            .map_err(|_| anyhow!("workspace registry lock poisoned"))?;
        let original_len = registry.mounts.len();
        if matches!(
            materialization,
            WorkspaceMountMaterialization::HostSnapshotCopy
        ) {
            registry.mounts.retain(|record| {
                !(record.owner_workspace_id == owner_workspace_id
                    && record.mount_name == mount_name)
            });
        } else if let Some(existing) = registry.mounts.iter_mut().find(|record| {
            record.owner_workspace_id == owner_workspace_id && record.mount_name == mount_name
        }) {
            existing.source_workspace_id = source_workspace_id.to_string();
            existing.mode = "ro".to_string();
            existing.updated_at = now;
        } else {
            registry.mounts.push(StoredWorkspaceMountRecord {
                owner_workspace_id: owner_workspace_id.to_string(),
                source_workspace_id: source_workspace_id.to_string(),
                mount_name: mount_name.to_string(),
                mode: "ro".to_string(),
                created_at: now,
                updated_at: now,
            });
        }
        if registry.mounts.len() != original_len
            || !matches!(
                materialization,
                WorkspaceMountMaterialization::HostSnapshotCopy
            )
        {
            self.persist_registry(&registry)?;
        }
        Ok(mount_path)
    }

    pub fn list_workspace_mounts(
        &self,
        owner_workspace_id: &str,
    ) -> Result<Vec<WorkspaceMountRecord>> {
        let registry = self
            .registry
            .lock()
            .map_err(|_| anyhow!("workspace registry lock poisoned"))?;
        Ok(registry
            .mounts
            .iter()
            .filter(|record| record.owner_workspace_id == owner_workspace_id)
            .cloned()
            .map(|record| WorkspaceMountRecord {
                owner_workspace_id: record.owner_workspace_id,
                source_workspace_id: record.source_workspace_id,
                mount_name: record.mount_name,
                mode: record.mode,
                created_at: record.created_at,
                updated_at: record.updated_at,
            })
            .collect())
    }

    pub fn prepare_bubblewrap_view(&self, workspace_id: &str) -> Result<WorkspaceRecord> {
        let workspace = self.ensure_workspace_exists(workspace_id)?;
        self.materialize_mount_placeholder(&workspace.files_dir.join(".skill_memory"))?;
        Ok(workspace)
    }

    pub fn cleanup_transient_mounts(&self, workspace_id: &str) -> Result<()> {
        let workspace = self.ensure_workspace_exists(workspace_id)?;
        if !workspace.mounts_dir.exists() {
            return Ok(());
        }
        for entry in fs::read_dir(&workspace.mounts_dir)
            .with_context(|| format!("failed to read {}", workspace.mounts_dir.display()))?
        {
            let entry = entry?;
            let path = entry.path();
            let metadata = fs::symlink_metadata(&path)
                .with_context(|| format!("failed to inspect {}", path.display()))?;
            if metadata.file_type().is_symlink() || metadata.is_file() {
                fs::remove_file(&path)
                    .with_context(|| format!("failed to remove {}", path.display()))?;
            } else if metadata.is_dir() {
                fs::remove_dir_all(&path)
                    .with_context(|| format!("failed to remove {}", path.display()))?;
            }
        }
        Ok(())
    }

    pub fn move_contents_between_workspaces(
        &self,
        source_workspace_id: &str,
        target_workspace_id: &str,
        paths: &[String],
        target_dir: Option<&str>,
        source_summary_update: Option<String>,
        target_summary_update: Option<String>,
    ) -> Result<ValueMoveSummary> {
        let source = self.ensure_workspace_exists(source_workspace_id)?;
        let target = self.ensure_workspace_exists(target_workspace_id)?;
        let source_files_dir = source.files_dir.clone();
        let target_files_dir = target.files_dir.clone();
        let target_base = match target_dir.map(str::trim).filter(|value| !value.is_empty()) {
            Some(dir) => resolve_workspace_path(&target_files_dir, dir)?,
            None => target_files_dir.clone(),
        };
        fs::create_dir_all(&target_base)
            .with_context(|| format!("failed to create {}", target_base.display()))?;

        let mut moved = Vec::new();
        for relative in paths {
            let relative = relative.trim();
            if relative.is_empty() {
                continue;
            }
            let source_path = resolve_workspace_path(&source.files_dir, relative)?;
            if !source_path.exists() {
                return Err(anyhow!(
                    "source path does not exist: {}",
                    source_path.display()
                ));
            }
            let target_path = target_base.join(
                source_path
                    .file_name()
                    .ok_or_else(|| anyhow!("invalid source path {}", source_path.display()))?,
            );
            copy_path_recursive(&source_path, &target_path)?;
            if source_path.is_dir() {
                fs::remove_dir_all(&source_path)
                    .with_context(|| format!("failed to remove {}", source_path.display()))?;
            } else {
                fs::remove_file(&source_path)
                    .with_context(|| format!("failed to remove {}", source_path.display()))?;
            }
            moved.push((source_path, target_path));
        }

        self.mark_content_modified(source_workspace_id)?;
        self.mark_content_modified(target_workspace_id)?;
        if let Some(summary) = source_summary_update {
            let _ = self.update_summary(source_workspace_id, summary, None)?;
        }
        if let Some(summary) = target_summary_update {
            let _ = self.update_summary(target_workspace_id, summary, None)?;
        }

        Ok(ValueMoveSummary {
            source_workspace_id: source_workspace_id.to_string(),
            target_workspace_id: target_workspace_id.to_string(),
            moved_paths: moved
                .into_iter()
                .map(|(source, target)| MovedPath {
                    source: source
                        .strip_prefix(&source_files_dir)
                        .unwrap_or(&source)
                        .to_string_lossy()
                        .to_string(),
                    target: target
                        .strip_prefix(&target_files_dir)
                        .unwrap_or(&target)
                        .to_string_lossy()
                        .to_string(),
                })
                .collect(),
        })
    }

    fn persist_record(&self, record: &WorkspaceRecord) -> Result<()> {
        let mut registry = self
            .registry
            .lock()
            .map_err(|_| anyhow!("workspace registry lock poisoned"))?;
        let stored = StoredWorkspaceRecord {
            id: record.id.clone(),
            title: record.title.clone(),
            summary: record.summary.clone(),
            state: record.state.clone(),
            created_at: record.created_at,
            updated_at: record.updated_at,
            last_content_modified_at: record.last_content_modified_at,
            last_main_agent_id: record.last_main_agent_id,
            last_session_id: record.last_session_id,
        };
        if let Some(existing) = registry
            .workspaces
            .iter_mut()
            .find(|existing| existing.id == stored.id)
        {
            *existing = stored;
        } else {
            registry.workspaces.push(stored);
        }
        self.persist_registry(&registry)
    }

    fn persist_registry(&self, registry: &WorkspaceRegistry) -> Result<()> {
        let raw = serde_json::to_string_pretty(registry)
            .context("failed to serialize workspace registry")?;
        fs::write(&self.registry_path, raw)
            .with_context(|| format!("failed to write {}", self.registry_path.display()))
    }

    fn ensure_workspace_dirs(&self, record: &WorkspaceRecord) -> Result<()> {
        fs::create_dir_all(&record.files_dir)
            .with_context(|| format!("failed to create {}", record.files_dir.display()))?;
        fs::create_dir_all(record.files_dir.join("upload")).with_context(|| {
            format!(
                "failed to create {}",
                record.files_dir.join("upload").display()
            )
        })?;
        fs::create_dir_all(&record.mounts_dir)
            .with_context(|| format!("failed to create {}", record.mounts_dir.display()))?;
        self.normalize_skill_memory_layout(record)?;
        fs::create_dir_all(&record.host_dir)
            .with_context(|| format!("failed to create {}", record.host_dir.display()))?;
        if !record.summary_path.exists() {
            fs::write(&record.summary_path, record.summary.as_bytes()).with_context(|| {
                format!(
                    "failed to write workspace summary {}",
                    record.summary_path.display()
                )
            })?;
        }
        Ok(())
    }

    fn seed_workspace_from_template(&self, record: &WorkspaceRecord) -> Result<()> {
        if !self.template_root.exists() {
            return Ok(());
        }
        for entry in fs::read_dir(&self.template_root)
            .with_context(|| format!("failed to read {}", self.template_root.display()))?
        {
            let entry = entry?;
            let source_path = entry.path();
            if entry.file_name() == "skill_memory" {
                continue;
            }
            let target_path = record.files_dir.join(entry.file_name());
            if target_path.exists() {
                continue;
            }
            copy_path_recursive(&source_path, &target_path)?;
        }
        Ok(())
    }

    fn normalize_skill_memory_layout(&self, record: &WorkspaceRecord) -> Result<()> {
        let source = self.template_root.join("skill_memory");
        fs::create_dir_all(&source)
            .with_context(|| format!("failed to create {}", source.display()))?;
        let legacy = record.files_dir.join("skill_memory");
        if legacy.exists() {
            merge_directory_contents_if_missing(&legacy, &source)?;
            fs::remove_dir_all(&legacy)
                .with_context(|| format!("failed to remove legacy {}", legacy.display()))?;
        }
        let target = record.files_dir.join(".skill_memory");
        if target.exists() {
            return Ok(());
        }
        create_dir_symlink(&source, &target)
            .with_context(|| format!("failed to create skill memory link {}", target.display()))
    }

    fn materialize_mount_path(
        &self,
        mount_path: &Path,
        source_files_dir: &Path,
        materialization: WorkspaceMountMaterialization,
    ) -> Result<()> {
        match materialization {
            WorkspaceMountMaterialization::HostSymlink => {
                self.materialize_live_mount_path(mount_path, source_files_dir)
            }
            WorkspaceMountMaterialization::SandboxPlaceholder => {
                self.materialize_mount_placeholder(mount_path)
            }
            WorkspaceMountMaterialization::HostSnapshotCopy => {
                self.materialize_snapshot_mount_path(mount_path, source_files_dir)
            }
        }
    }

    fn materialize_live_mount_path(
        &self,
        mount_path: &Path,
        source_files_dir: &Path,
    ) -> Result<()> {
        if let Ok(metadata) = fs::symlink_metadata(mount_path) {
            if metadata.file_type().is_symlink() || metadata.is_file() {
                fs::remove_file(mount_path)
                    .with_context(|| format!("failed to replace {}", mount_path.display()))?;
            } else if metadata.is_dir() {
                fs::remove_dir_all(mount_path)
                    .with_context(|| format!("failed to replace {}", mount_path.display()))?;
            }
        }
        if let Some(parent) = mount_path.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("failed to create {}", parent.display()))?;
        }
        create_dir_symlink(source_files_dir, mount_path)
            .with_context(|| format!("failed to create mount link {}", mount_path.display()))
    }

    fn materialize_mount_placeholder(&self, mount_path: &Path) -> Result<()> {
        if let Ok(metadata) = fs::symlink_metadata(mount_path) {
            if metadata.file_type().is_symlink() || metadata.is_file() {
                fs::remove_file(mount_path)
                    .with_context(|| format!("failed to replace {}", mount_path.display()))?;
            } else if metadata.is_dir() {
                fs::remove_dir_all(mount_path)
                    .with_context(|| format!("failed to replace {}", mount_path.display()))?;
            }
        }
        fs::create_dir_all(mount_path).with_context(|| {
            format!(
                "failed to create mount placeholder {}",
                mount_path.display()
            )
        })
    }

    fn materialize_snapshot_mount_path(
        &self,
        mount_path: &Path,
        source_files_dir: &Path,
    ) -> Result<()> {
        if let Ok(metadata) = fs::symlink_metadata(mount_path) {
            if metadata.file_type().is_symlink() || metadata.is_file() {
                fs::remove_file(mount_path)
                    .with_context(|| format!("failed to replace {}", mount_path.display()))?;
            } else if metadata.is_dir() {
                fs::remove_dir_all(mount_path)
                    .with_context(|| format!("failed to replace {}", mount_path.display()))?;
            }
        }
        copy_dir_recursive(source_files_dir, mount_path)?;
        set_readonly_recursive(mount_path)
    }

    fn workspace_record_from_stored(&self, record: StoredWorkspaceRecord) -> WorkspaceRecord {
        self.workspace_record(
            record.id,
            record.title,
            record.summary,
            record.state,
            record.created_at,
            record.updated_at,
            record.last_content_modified_at,
            record.last_main_agent_id,
            record.last_session_id,
        )
    }

    fn workspace_record(
        &self,
        id: String,
        title: String,
        summary: String,
        state: String,
        created_at: DateTime<Utc>,
        updated_at: DateTime<Utc>,
        last_content_modified_at: DateTime<Utc>,
        last_main_agent_id: Option<Uuid>,
        last_session_id: Option<Uuid>,
    ) -> WorkspaceRecord {
        let root_dir = self.workspaces_root.join(&id);
        let files_dir = root_dir.join("files");
        let mounts_dir = files_dir.join("mounts");
        let host_dir = self.meta_root.join(&id);
        let summary_path = host_dir.join("summary.md");
        WorkspaceRecord {
            id,
            title,
            summary,
            state,
            root_dir,
            files_dir,
            mounts_dir,
            host_dir,
            summary_path,
            created_at,
            updated_at,
            last_content_modified_at,
            last_main_agent_id,
            last_session_id,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ValueMoveSummary {
    pub source_workspace_id: String,
    pub target_workspace_id: String,
    pub moved_paths: Vec<MovedPath>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct MovedPath {
    pub source: String,
    pub target: String,
}

fn resolve_workspace_path(workspace_root: &Path, relative: &str) -> Result<PathBuf> {
    let candidate = workspace_root.join(relative);
    let root = workspace_root.canonicalize().with_context(|| {
        format!(
            "failed to resolve workspace root {}",
            workspace_root.display()
        )
    })?;
    let canonical_parent = candidate
        .parent()
        .unwrap_or(workspace_root)
        .canonicalize()
        .with_context(|| format!("failed to resolve parent for {}", candidate.display()))?;
    if !canonical_parent.starts_with(&root) {
        return Err(anyhow!("path escapes workspace root"));
    }
    Ok(candidate)
}

fn compute_latest_workspace_mtime(path: &Path) -> Result<Option<DateTime<Utc>>> {
    if !path.exists() {
        return Ok(None);
    }
    let metadata =
        fs::metadata(path).with_context(|| format!("failed to stat {}", path.display()))?;
    let mut latest = metadata.modified().ok().map(DateTime::<Utc>::from);
    if metadata.is_dir() {
        for entry in
            fs::read_dir(path).with_context(|| format!("failed to read {}", path.display()))?
        {
            let entry = entry?;
            if let Some(child_latest) = compute_latest_workspace_mtime(&entry.path())? {
                latest = Some(match latest {
                    Some(current) if current >= child_latest => current,
                    _ => child_latest,
                });
            }
        }
    }
    Ok(latest)
}

fn copy_dir_recursive(source: &Path, target: &Path) -> Result<()> {
    fs::create_dir_all(target).with_context(|| format!("failed to create {}", target.display()))?;
    for entry in
        fs::read_dir(source).with_context(|| format!("failed to read {}", source.display()))?
    {
        let entry = entry?;
        let source_path = entry.path();
        let target_path = target.join(entry.file_name());
        copy_path_recursive(&source_path, &target_path)?;
    }
    Ok(())
}

fn merge_directory_contents_if_missing(source: &Path, target: &Path) -> Result<()> {
    let metadata = fs::symlink_metadata(source)
        .with_context(|| format!("failed to stat {}", source.display()))?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Ok(());
    }
    fs::create_dir_all(target).with_context(|| format!("failed to create {}", target.display()))?;
    for entry in fs::read_dir(source).with_context(|| format!("failed to read {}", source.display()))? {
        let entry = entry?;
        let source_path = entry.path();
        let target_path = target.join(entry.file_name());
        if target_path.exists() {
            if source_path.is_dir() && target_path.is_dir() {
                merge_directory_contents_if_missing(&source_path, &target_path)?;
            }
            continue;
        }
        copy_path_recursive(&source_path, &target_path)?;
    }
    Ok(())
}

fn copy_path_recursive(source: &Path, target: &Path) -> Result<()> {
    let metadata =
        fs::metadata(source).with_context(|| format!("failed to stat {}", source.display()))?;
    if metadata.is_dir() {
        copy_dir_recursive(source, target)
    } else {
        if let Some(parent) = target.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("failed to create {}", parent.display()))?;
        }
        fs::copy(source, target).with_context(|| {
            format!(
                "failed to copy {} to {}",
                source.display(),
                target.display()
            )
        })?;
        Ok(())
    }
}

fn create_dir_symlink(source: &Path, target: &Path) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(source, target)
    }
    #[cfg(windows)]
    {
        std::os::windows::fs::symlink_dir(source, target)
    }
}

fn set_readonly_recursive(path: &Path) -> Result<()> {
    let metadata =
        fs::symlink_metadata(path).with_context(|| format!("failed to stat {}", path.display()))?;
    if metadata.file_type().is_symlink() {
        return Ok(());
    }
    let mut permissions = metadata.permissions();
    permissions.set_readonly(true);
    fs::set_permissions(path, permissions)
        .with_context(|| format!("failed to mark {} readonly", path.display()))?;
    if metadata.is_dir() {
        for entry in
            fs::read_dir(path).with_context(|| format!("failed to read {}", path.display()))?
        {
            let entry = entry?;
            set_readonly_recursive(&entry.path())?;
        }
    }
    Ok(())
}

fn collect_workspace_entries(
    workspace_root: &Path,
    current_dir: &Path,
    remaining_depth: usize,
    limit: usize,
    output: &mut Vec<WorkspaceContentEntry>,
) -> Result<()> {
    if output.len() >= limit {
        return Ok(());
    }
    let mut entries = fs::read_dir(current_dir)
        .with_context(|| format!("failed to read {}", current_dir.display()))?
        .collect::<Result<Vec<_>, _>>()
        .with_context(|| format!("failed to read entries from {}", current_dir.display()))?;
    entries.sort_by_key(|entry| entry.path());

    for entry in entries {
        if output.len() >= limit {
            break;
        }
        let path = entry.path();
        let metadata = entry
            .metadata()
            .with_context(|| format!("failed to stat {}", path.display()))?;
        let relative = path
            .strip_prefix(workspace_root)
            .unwrap_or(&path)
            .to_string_lossy()
            .to_string();
        let modified_at = metadata.modified().ok().map(DateTime::<Utc>::from);
        let entry_type = if metadata.is_dir() { "dir" } else { "file" };
        output.push(WorkspaceContentEntry {
            path: relative,
            entry_type: entry_type.to_string(),
            size_bytes: if metadata.is_file() {
                Some(metadata.len())
            } else {
                None
            },
            modified_at,
        });
        if metadata.is_dir() && remaining_depth > 0 {
            collect_workspace_entries(
                workspace_root,
                &path,
                remaining_depth.saturating_sub(1),
                limit,
                output,
            )?;
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{WorkspaceManager, WorkspaceMountMaterialization};
    use std::fs;
    use tempfile::TempDir;
    use uuid::Uuid;

    #[test]
    fn workspace_manager_creates_and_restores_workspace() {
        let temp_dir = TempDir::new().unwrap();
        let manager = WorkspaceManager::load_or_create(temp_dir.path()).unwrap();
        let main_agent_id = Uuid::new_v4();
        let session_id = Uuid::new_v4();
        let created = manager
            .create_workspace(main_agent_id, session_id, Some("Main Workspace"))
            .unwrap();

        assert!(created.files_dir.exists());
        assert!(created.mounts_dir.exists());
        assert!(created.summary_path.exists());

        let restored = manager.get_workspace(&created.id).unwrap().unwrap();
        assert_eq!(restored.id, created.id);
        assert_eq!(restored.title, "Main Workspace");
        assert_eq!(restored.last_main_agent_id, Some(main_agent_id));
        assert_eq!(restored.last_session_id, Some(session_id));
    }

    #[test]
    fn workspace_manager_lists_and_updates_workspaces() {
        let temp_dir = TempDir::new().unwrap();
        let manager = WorkspaceManager::load_or_create(temp_dir.path()).unwrap();
        let created = manager
            .create_workspace(Uuid::new_v4(), Uuid::new_v4(), Some("Design Notes"))
            .unwrap();

        let listed = manager.list_workspaces(Some("design"), false).unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].id, created.id);

        let updated = manager
            .update_summary(
                &created.id,
                "Workspace for design exploration.".to_string(),
                Some("Design Workspace".to_string()),
            )
            .unwrap();
        assert_eq!(updated.title, "Design Workspace");
        assert_eq!(updated.summary, "Workspace for design exploration.");
        let summary_text = fs::read_to_string(updated.summary_path).unwrap();
        assert_eq!(summary_text, "Workspace for design exploration.");
    }

    #[test]
    fn workspace_manager_seeds_new_workspace_from_rundir_template() {
        let temp_dir = TempDir::new().unwrap();
        fs::create_dir_all(temp_dir.path().join("rundir/.skills")).unwrap();
        fs::write(temp_dir.path().join("rundir/AGENTS.md"), "template agents").unwrap();
        fs::write(
            temp_dir.path().join("rundir/.skills/template.txt"),
            "skill template",
        )
        .unwrap();

        let manager = WorkspaceManager::load_or_create(temp_dir.path()).unwrap();
        let created = manager
            .create_workspace(Uuid::new_v4(), Uuid::new_v4(), Some("Seeded"))
            .unwrap();

        assert_eq!(
            fs::read_to_string(created.files_dir.join("AGENTS.md")).unwrap(),
            "template agents"
        );
        assert_eq!(
            fs::read_to_string(created.files_dir.join(".skills/template.txt")).unwrap(),
            "skill template"
        );
    }

    #[test]
    fn workspace_manager_lists_workspace_contents() {
        let temp_dir = TempDir::new().unwrap();
        let manager = WorkspaceManager::load_or_create(temp_dir.path()).unwrap();
        let created = manager
            .create_workspace(Uuid::new_v4(), Uuid::new_v4(), Some("Files"))
            .unwrap();
        fs::create_dir_all(created.files_dir.join("projects/demo")).unwrap();
        fs::write(created.files_dir.join("projects/demo/README.md"), "hello").unwrap();

        let items = manager
            .list_workspace_contents(&created.id, Some("projects"), 3, 20)
            .unwrap();
        assert!(items.iter().any(|item| item.path == "projects/demo"));
        assert!(
            items
                .iter()
                .any(|item| item.path == "projects/demo/README.md" && item.entry_type == "file")
        );
    }

    #[test]
    fn workspace_manager_archives_reactivates_mounts_and_moves_content() {
        let temp_dir = TempDir::new().unwrap();
        let manager = WorkspaceManager::load_or_create(temp_dir.path()).unwrap();
        let source = manager
            .create_workspace(Uuid::new_v4(), Uuid::new_v4(), Some("Source"))
            .unwrap();
        let target = manager
            .create_workspace(Uuid::new_v4(), Uuid::new_v4(), Some("Target"))
            .unwrap();

        fs::create_dir_all(source.files_dir.join("projects/demo")).unwrap();
        fs::write(source.files_dir.join("projects/demo/README.md"), "hello").unwrap();

        let mounted = manager
            .mount_workspace_snapshot(
                &target.id,
                &source.id,
                "imported",
                WorkspaceMountMaterialization::HostSymlink,
            )
            .unwrap();
        assert!(mounted.join("projects/demo/README.md").exists());

        let moved = manager
            .move_contents_between_workspaces(
                &source.id,
                &target.id,
                &[String::from("projects/demo")],
                Some("projects"),
                Some("Source after move".to_string()),
                Some("Target after move".to_string()),
            )
            .unwrap();
        assert_eq!(moved.moved_paths.len(), 1);
        assert!(!source.files_dir.join("projects/demo").exists());
        assert!(target.files_dir.join("projects/demo/README.md").exists());

        let archived = manager
            .archive_stale_workspaces(chrono::Duration::zero(), std::slice::from_ref(&target.id))
            .unwrap();
        assert!(archived.iter().any(|item| item.id == source.id));
        assert!(!archived.iter().any(|item| item.id == target.id));

        let reactivated = manager.reactivate_workspace(&source.id).unwrap();
        assert_eq!(reactivated.state, "active");
    }

    #[test]
    fn bubblewrap_view_materializes_skill_memory_placeholder_only() {
        let temp_dir = TempDir::new().unwrap();
        let manager = WorkspaceManager::load_or_create(temp_dir.path()).unwrap();
        let source = manager
            .create_workspace(Uuid::new_v4(), Uuid::new_v4(), Some("Source"))
            .unwrap();
        let target = manager
            .create_workspace(Uuid::new_v4(), Uuid::new_v4(), Some("Target"))
            .unwrap();

        manager
            .mount_workspace_snapshot(
                &target.id,
                &source.id,
                "shared",
                WorkspaceMountMaterialization::HostSymlink,
            )
            .unwrap();
        assert!(
            fs::symlink_metadata(target.files_dir.join(".skill_memory"))
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert!(target.mounts_dir.join("shared").exists());

        manager.prepare_bubblewrap_view(&target.id).unwrap();

        assert!(target.files_dir.join(".skill_memory").is_dir());
        assert!(
            !fs::symlink_metadata(target.files_dir.join(".skill_memory"))
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert!(
            fs::symlink_metadata(target.mounts_dir.join("shared"))
                .unwrap()
                .file_type()
                .is_symlink()
        );
    }

    #[test]
    fn host_snapshot_copy_mount_is_readonly_materialized_content() {
        let temp_dir = TempDir::new().unwrap();
        let manager = WorkspaceManager::load_or_create(temp_dir.path()).unwrap();
        let source = manager
            .create_workspace(Uuid::new_v4(), Uuid::new_v4(), Some("Source"))
            .unwrap();
        let target = manager
            .create_workspace(Uuid::new_v4(), Uuid::new_v4(), Some("Target"))
            .unwrap();

        fs::create_dir_all(source.files_dir.join("notes")).unwrap();
        fs::write(source.files_dir.join("notes/hello.txt"), "hello").unwrap();

        let mounted = manager
            .mount_workspace_snapshot(
                &target.id,
                &source.id,
                "copied",
                WorkspaceMountMaterialization::HostSnapshotCopy,
            )
            .unwrap();

        assert!(mounted.join("notes/hello.txt").exists());
        assert!(
            !fs::symlink_metadata(&mounted)
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert!(
            fs::metadata(mounted.join("notes/hello.txt"))
                .unwrap()
                .permissions()
                .readonly()
        );
        assert!(
            manager
                .list_workspace_mounts(&target.id)
                .unwrap()
                .is_empty()
        );
    }
}
