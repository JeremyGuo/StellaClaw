use super::*;
use crate::conversation::ConversationSnapshot;
use crate::session::SessionActorRef;
use std::fs;

#[derive(Clone, Debug)]
pub(super) struct ExecutionStorageContext {
    pub(super) workspace_root: PathBuf,
    pub(super) storage_root: PathBuf,
    pub(super) sessions_root: PathBuf,
    pub(super) runtime_state_root: PathBuf,
    pub(super) workspace_id: String,
}

impl AgentRuntimeView {
    pub(super) fn remote_execution_binding(
        &self,
        address: &ChannelAddress,
    ) -> Result<Option<RemoteExecutionBinding>> {
        Ok(self.with_conversations(|conversations| {
            Ok(conversations
                .ensure_conversation(address)?
                .settings
                .remote_execution)
        })?)
    }

    pub(super) fn remote_execution_active(&self, address: &ChannelAddress) -> Result<bool> {
        Ok(self.remote_execution_binding(address)?.is_some())
    }

    fn remote_mounts_root(&self) -> PathBuf {
        self.workdir.join("remote_mounts")
    }

    fn remote_mountpoint_for_conversation(&self, conversation: &ConversationSnapshot) -> PathBuf {
        self.remote_mounts_root()
            .join(conversation.id.to_string())
            .join("workspace")
    }

    fn remote_workspace_id(&self, conversation: &ConversationSnapshot) -> String {
        format!("remote-exec-{}", conversation.id.simple())
    }

    pub(super) fn remote_execution_context(
        &self,
        address: &ChannelAddress,
    ) -> Result<Option<ExecutionStorageContext>> {
        let conversation =
            self.with_conversations(|conversations| conversations.ensure_conversation(address))?;
        let Some(binding) = conversation.settings.remote_execution.clone() else {
            return Ok(None);
        };
        let workspace_root = self.ensure_execution_root_for_binding(&conversation, &binding)?;
        let storage_root = storage_root_for_execution_root(&workspace_root);
        fs::create_dir_all(&storage_root)
            .with_context(|| format!("failed to create {}", storage_root.display()))?;
        Ok(Some(ExecutionStorageContext {
            sessions_root: storage_root.join("sessions"),
            runtime_state_root: storage_root
                .join("runtime")
                .join(conversation.id.to_string()),
            workspace_id: self.remote_workspace_id(&conversation),
            workspace_root,
            storage_root,
        }))
    }

    fn ensure_execution_root_for_binding(
        &self,
        conversation: &ConversationSnapshot,
        binding: &RemoteExecutionBinding,
    ) -> Result<PathBuf> {
        match binding {
            RemoteExecutionBinding::Local { path } => {
                if !path.is_dir() {
                    return Err(anyhow!(
                        "remote execution path is not a directory: {}",
                        path.display()
                    ));
                }
                Ok(path.clone())
            }
            RemoteExecutionBinding::Ssh { host, path } => {
                self.ensure_sshfs_mount(conversation, host, path)
            }
        }
    }

    fn ensure_sshfs_mount(
        &self,
        conversation: &ConversationSnapshot,
        host: &str,
        remote_path: &str,
    ) -> Result<PathBuf> {
        self.ensure_sshfs_available()?;
        let mountpoint = self.remote_mountpoint_for_conversation(conversation);
        if self.is_mountpoint(&mountpoint) {
            return Ok(mountpoint);
        }
        if mountpoint.exists() {
            clear_directory_contents(&mountpoint)?;
        }
        fs::create_dir_all(&mountpoint)
            .with_context(|| format!("failed to create {}", mountpoint.display()))?;
        let target = format!("{host}:{remote_path}");
        let output = Command::new("sshfs")
            .arg(&target)
            .arg(&mountpoint)
            .arg("-o")
            .arg("BatchMode=yes")
            .arg("-o")
            .arg("reconnect")
            .arg("-o")
            .arg("ServerAliveInterval=15")
            .arg("-o")
            .arg("ServerAliveCountMax=3")
            .output()
            .with_context(|| format!("failed to run sshfs for {}", target))?;
        if !output.status.success() {
            return Err(anyhow!(
                "sshfs mount failed for {}: {}",
                target,
                String::from_utf8_lossy(&output.stderr)
            ));
        }
        Ok(mountpoint)
    }

    fn ensure_sshfs_available(&self) -> Result<()> {
        let output = Command::new("sshfs")
            .arg("-V")
            .output()
            .context("failed to execute sshfs")?;
        if !output.status.success() {
            return Err(anyhow!(
                "sshfs is required for /remote <host> <path>, but `sshfs -V` failed: {}",
                String::from_utf8_lossy(&output.stderr)
            ));
        }
        Ok(())
    }

    fn is_mountpoint(&self, path: &Path) -> bool {
        Command::new("mountpoint")
            .arg("-q")
            .arg(path)
            .status()
            .map(|status| status.success())
            .unwrap_or(false)
    }

    pub(super) fn create_background_session_for_conversation(
        &self,
        address: &ChannelAddress,
        agent_id: uuid::Uuid,
    ) -> Result<SessionSnapshot> {
        if let Some(context) = self.remote_execution_context(address)? {
            let actor = self.with_sessions(|sessions| {
                sessions.create_background_actor_in_root(
                    address,
                    agent_id,
                    &context.workspace_id,
                    &context.workspace_root,
                    &context.sessions_root,
                )
            })?;
            return actor.snapshot();
        }

        let preferred_workspace_id = self.with_conversations(|conversations| {
            Ok(conversations
                .ensure_conversation(address)?
                .settings
                .workspace_id)
        })?;
        let actor = self.with_sessions(|sessions| match preferred_workspace_id.as_deref() {
            Some(workspace_id) => {
                sessions.create_background_in_workspace_actor(address, agent_id, workspace_id)
            }
            None => sessions.create_background_actor(address, agent_id),
        })?;
        let session = actor.snapshot()?;
        self.with_conversations(|conversations| {
            conversations.set_workspace_id(address, Some(session.workspace_id.clone()))?;
            Ok(())
        })?;
        Ok(session)
    }

    pub(super) fn runtime_state_root_for_session(
        &self,
        session: &SessionSnapshot,
    ) -> Result<PathBuf> {
        if let Some(context) = self.remote_execution_context(&session.address)? {
            return Ok(context.runtime_state_root);
        }
        Ok(self
            .agent_workspace
            .root_dir
            .join("runtime")
            .join(&session.workspace_id))
    }
}

impl Server {
    pub(super) fn remote_execution_binding(
        &self,
        address: &ChannelAddress,
    ) -> Result<Option<RemoteExecutionBinding>> {
        Ok(self
            .effective_conversation_settings(address)?
            .remote_execution)
    }

    pub(super) fn remote_execution_active(&self, address: &ChannelAddress) -> Result<bool> {
        Ok(self.remote_execution_binding(address)?.is_some())
    }

    fn remote_mounts_root(&self) -> PathBuf {
        self.workdir.join("remote_mounts")
    }

    fn remote_mountpoint_for_conversation(&self, conversation: &ConversationSnapshot) -> PathBuf {
        self.remote_mounts_root()
            .join(conversation.id.to_string())
            .join("workspace")
    }

    fn remote_workspace_id(&self, conversation: &ConversationSnapshot) -> String {
        format!("remote-exec-{}", conversation.id.simple())
    }

    pub(super) fn remote_execution_context(
        &self,
        address: &ChannelAddress,
    ) -> Result<Option<ExecutionStorageContext>> {
        let conversation =
            self.with_conversations(|conversations| conversations.ensure_conversation(address))?;
        let Some(binding) = conversation.settings.remote_execution.clone() else {
            return Ok(None);
        };
        let workspace_root = self.ensure_execution_root_for_binding(&conversation, &binding)?;
        let storage_root = storage_root_for_execution_root(&workspace_root);
        fs::create_dir_all(&storage_root)
            .with_context(|| format!("failed to create {}", storage_root.display()))?;
        Ok(Some(ExecutionStorageContext {
            sessions_root: storage_root.join("sessions"),
            runtime_state_root: storage_root
                .join("runtime")
                .join(conversation.id.to_string()),
            workspace_id: self.remote_workspace_id(&conversation),
            workspace_root,
            storage_root,
        }))
    }

    fn ensure_execution_root_for_binding(
        &self,
        conversation: &ConversationSnapshot,
        binding: &RemoteExecutionBinding,
    ) -> Result<PathBuf> {
        match binding {
            RemoteExecutionBinding::Local { path } => {
                if !path.is_dir() {
                    return Err(anyhow!(
                        "remote execution path is not a directory: {}",
                        path.display()
                    ));
                }
                Ok(path.clone())
            }
            RemoteExecutionBinding::Ssh { host, path } => {
                self.ensure_sshfs_mount(conversation, host, path)
            }
        }
    }

    fn ensure_sshfs_mount(
        &self,
        conversation: &ConversationSnapshot,
        host: &str,
        remote_path: &str,
    ) -> Result<PathBuf> {
        self.ensure_sshfs_available()?;
        let mountpoint = self.remote_mountpoint_for_conversation(conversation);
        if self.is_mountpoint(&mountpoint) {
            return Ok(mountpoint);
        }
        if mountpoint.exists() {
            clear_directory_contents(&mountpoint)?;
        }
        fs::create_dir_all(&mountpoint)
            .with_context(|| format!("failed to create {}", mountpoint.display()))?;
        let target = format!("{host}:{remote_path}");
        let output = Command::new("sshfs")
            .arg(&target)
            .arg(&mountpoint)
            .arg("-o")
            .arg("BatchMode=yes")
            .arg("-o")
            .arg("reconnect")
            .arg("-o")
            .arg("ServerAliveInterval=15")
            .arg("-o")
            .arg("ServerAliveCountMax=3")
            .output()
            .with_context(|| format!("failed to run sshfs for {}", target))?;
        if !output.status.success() {
            return Err(anyhow!(
                "sshfs mount failed for {}: {}",
                target,
                String::from_utf8_lossy(&output.stderr)
            ));
        }
        Ok(mountpoint)
    }

    fn ensure_sshfs_available(&self) -> Result<()> {
        let output = Command::new("sshfs")
            .arg("-V")
            .output()
            .context("failed to execute sshfs")?;
        if !output.status.success() {
            return Err(anyhow!(
                "sshfs is required for /remote <host> <path>, but `sshfs -V` failed: {}",
                String::from_utf8_lossy(&output.stderr)
            ));
        }
        Ok(())
    }

    fn is_mountpoint(&self, path: &Path) -> bool {
        Command::new("mountpoint")
            .arg("-q")
            .arg(path)
            .status()
            .map(|status| status.success())
            .unwrap_or(false)
    }

    fn unmount_remote_execution_mount(&self, conversation: &ConversationSnapshot) -> Result<()> {
        let mountpoint = self.remote_mountpoint_for_conversation(conversation);
        if !self.is_mountpoint(&mountpoint) {
            return Ok(());
        }
        for candidate in [
            ("fusermount3", vec!["-u"]),
            ("fusermount", vec!["-u"]),
            ("umount", Vec::new()),
        ] {
            let status = Command::new(candidate.0)
                .args(candidate.1)
                .arg(&mountpoint)
                .status();
            if status.as_ref().is_ok_and(|value| value.success()) {
                return Ok(());
            }
        }
        Err(anyhow!(
            "failed to unmount remote execution mount {}",
            mountpoint.display()
        ))
    }

    pub(super) fn ensure_foreground_actor(
        &self,
        address: &ChannelAddress,
    ) -> Result<SessionActorRef> {
        if let Some(context) = self.remote_execution_context(address)? {
            return self.with_sessions(|sessions| {
                sessions.ensure_foreground_actor_in_root(
                    address,
                    &context.workspace_id,
                    &context.workspace_root,
                    &context.sessions_root,
                )
            });
        }
        self.with_conversations_and_sessions(|conversations, sessions| {
            conversations.ensure_foreground_actor(address, sessions)
        })
    }

    pub(super) fn with_snapshot_manager_for_address<T>(
        &self,
        address: &ChannelAddress,
        action: impl FnOnce(&mut SnapshotManager) -> Result<T>,
    ) -> Result<T> {
        if let Some(context) = self.remote_execution_context(address)? {
            let mut manager = SnapshotManager::new(&context.storage_root)?;
            return action(&mut manager);
        }
        self.with_snapshots(action)
    }

    pub(super) fn activate_remote_execution(
        &self,
        address: &ChannelAddress,
        binding: RemoteExecutionBinding,
    ) -> Result<ConversationSnapshot> {
        let conversation =
            self.with_conversations(|conversations| conversations.ensure_conversation(address))?;
        let old_remote = conversation.settings.remote_execution.clone();
        let active_session = self.with_sessions(|sessions| Ok(sessions.get_snapshot(address)))?;
        let checkpoint = if active_session.is_some() {
            Some(self.with_sessions(|sessions| sessions.export_checkpoint(address))?)
        } else {
            None
        };

        let target_workspace_root =
            self.ensure_execution_root_for_binding(&conversation, &binding)?;
        let target_storage_root = storage_root_for_execution_root(&target_workspace_root);
        fs::create_dir_all(&target_storage_root)
            .with_context(|| format!("failed to create {}", target_storage_root.display()))?;
        if checkpoint.is_none() {
            copy_conversation_sessions_between_roots(
                &self.source_sessions_root_for_binding(address, old_remote.as_ref())?,
                &target_storage_root.join("sessions"),
                address,
            )?;
        }

        if active_session.is_some() {
            self.destroy_foreground_session(address)?;
        }
        let snapshot = self.with_conversations(|conversations| {
            conversations.set_remote_execution(address, Some(binding.clone()))
        })?;
        self.with_conversations(|conversations| {
            conversations.set_workspace_id(address, None).map(|_| ())
        })?;
        if let Some(checkpoint) = checkpoint {
            let restored = self.with_sessions(|sessions| {
                sessions.restore_foreground_from_checkpoint_in_root(
                    address,
                    checkpoint,
                    self.remote_workspace_id(&snapshot),
                    target_workspace_root.clone(),
                    &target_storage_root.join("sessions"),
                )
            })?;
            self.append_remote_execution_notice(
                &restored,
                format!("Remote execution root is now `{}`.", binding.describe()),
            )?;
        }
        if let Some(RemoteExecutionBinding::Ssh { .. }) = old_remote {
            let _ = self.unmount_remote_execution_mount(&conversation);
        }
        Ok(snapshot)
    }

    pub(super) fn deactivate_remote_execution(
        &self,
        address: &ChannelAddress,
    ) -> Result<ConversationSnapshot> {
        let conversation =
            self.with_conversations(|conversations| conversations.ensure_conversation(address))?;
        let Some(old_binding) = conversation.settings.remote_execution.clone() else {
            return Ok(conversation);
        };
        let active_session = self.with_sessions(|sessions| Ok(sessions.get_snapshot(address)))?;
        let checkpoint = if active_session.is_some() {
            Some(self.with_sessions(|sessions| sessions.export_checkpoint(address))?)
        } else {
            None
        };
        let current_context = self
            .remote_execution_context(address)?
            .ok_or_else(|| anyhow!("remote execution context is missing"))?;

        if active_session.is_some() {
            self.destroy_foreground_session(address)?;
        }
        let snapshot = self.with_conversations(|conversations| {
            conversations.set_remote_execution(address, None)
        })?;

        if let Some(checkpoint) = checkpoint {
            let workspace = self.workspace_manager.create_workspace(
                uuid::Uuid::new_v4(),
                uuid::Uuid::new_v4(),
                Some("remote-off"),
            )?;
            replace_directory_contents(&workspace.files_dir, &current_context.workspace_root)?;
            let restored = self.with_sessions(|sessions| {
                sessions.restore_foreground_from_checkpoint(
                    address,
                    checkpoint,
                    workspace.id.clone(),
                    workspace.files_dir.clone(),
                )
            })?;
            self.with_conversations(|conversations| {
                conversations
                    .set_workspace_id(address, Some(workspace.id.clone()))
                    .map(|_| ())
            })?;
            self.append_remote_execution_notice(
                &restored,
                "Remote execution mode is now off. This conversation is back on a local workspace."
                    .to_string(),
            )?;
        }

        if matches!(old_binding, RemoteExecutionBinding::Ssh { .. }) {
            let _ = self.unmount_remote_execution_mount(&conversation);
        }
        Ok(snapshot)
    }

    fn append_remote_execution_notice(
        &self,
        session: &SessionSnapshot,
        text: String,
    ) -> Result<()> {
        let actor = self.with_sessions(|sessions| sessions.resolve_snapshot(session))?;
        actor.tell_actor_message(SessionActorMessage {
            from_session_id: session.id,
            role: MessageRole::System,
            text: Some(text),
            attachments: Vec::new(),
        })?;
        Ok(())
    }

    fn source_sessions_root_for_binding(
        &self,
        address: &ChannelAddress,
        binding: Option<&RemoteExecutionBinding>,
    ) -> Result<PathBuf> {
        if binding.is_none() {
            return Ok(self.workdir.join("sessions"));
        }
        self.remote_execution_context(address)?
            .map(|context| context.sessions_root)
            .ok_or_else(|| anyhow!("remote execution context is missing"))
    }
}

fn clear_directory_contents(path: &Path) -> Result<()> {
    if !path.is_dir() {
        return Ok(());
    }
    for entry in fs::read_dir(path).with_context(|| format!("failed to read {}", path.display()))? {
        let entry = entry?;
        let child = entry.path();
        let metadata = fs::symlink_metadata(&child)
            .with_context(|| format!("failed to inspect {}", child.display()))?;
        if metadata.is_dir() && !metadata.file_type().is_symlink() {
            fs::remove_dir_all(&child)
                .with_context(|| format!("failed to remove {}", child.display()))?;
        } else {
            fs::remove_file(&child)
                .with_context(|| format!("failed to remove {}", child.display()))?;
        }
    }
    Ok(())
}

fn copy_conversation_sessions_between_roots(
    source_sessions_root: &Path,
    target_sessions_root: &Path,
    address: &ChannelAddress,
) -> Result<()> {
    let source = source_sessions_root.join(crate::session::session_conversation_dir_name(
        &address.conversation_id,
    ));
    if !source.exists() {
        return Ok(());
    }
    let target = target_sessions_root.join(crate::session::session_conversation_dir_name(
        &address.conversation_id,
    ));
    if target.exists() {
        return Ok(());
    }
    copy_dir_recursive(&source, &target)
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
            let link_target = fs::read_link(&source_path)
                .with_context(|| format!("failed to read link {}", source_path.display()))?;
            std::os::unix::fs::symlink(&link_target, &target_path)
                .with_context(|| format!("failed to create symlink {}", target_path.display()))?;
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
