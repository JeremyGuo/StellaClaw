use super::{
    ActiveForegroundAgentFrameRuntime, BackgroundJobRequest, HostedSubagent, SummaryTracker,
};
use crate::agent_status::AgentRegistry;
use crate::bootstrap::AgentWorkspace;
use crate::channel::Channel;
use crate::channels::web::WebChannel;
use crate::config::{
    AgentConfig, BotCommandConfig, MainAgentConfig, ModelCatalogConfig, ModelConfig, SandboxMode,
    ToolingConfig,
};
use crate::conversation::ConversationManager;
use crate::cron::CronManager;
use crate::domain::ChannelAddress;
use crate::session::{SessionActorRef, SessionManager};
use crate::sink::SinkRouter;
use crate::snapshot::SnapshotManager;
use crate::workspace::WorkspaceManager;
use anyhow::{Result, anyhow};
use chrono::{DateTime, Utc};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::PathBuf;
use std::sync::atomic::AtomicUsize;
use std::sync::{Arc, Mutex};
use tokio::sync::{Notify, RwLock, mpsc};
use uuid::Uuid;

pub struct RuntimeContext {
    pub(super) workdir: PathBuf,
    pub(super) agent_workspace: AgentWorkspace,
    pub(super) sessions: Arc<Mutex<SessionManager>>,
    pub(super) workspace_manager: WorkspaceManager,
    pub(super) channels: Arc<HashMap<String, Arc<dyn Channel>>>,
    pub(super) web_channels: Arc<HashMap<String, Arc<WebChannel>>>,
    pub(super) command_catalog: HashMap<String, Vec<BotCommandConfig>>,
    pub(super) models: BTreeMap<String, ModelConfig>,
    pub(super) model_catalog: ModelCatalogConfig,
    pub(super) agent: AgentConfig,
    pub(super) tooling: ToolingConfig,
    pub(super) chat_model_keys: Vec<String>,
    pub(super) main_agent: MainAgentConfig,
    pub(super) sink_router: Arc<RwLock<SinkRouter>>,
    pub(super) cron_manager: Arc<Mutex<CronManager>>,
    pub(super) last_cron_poll_at: Arc<Mutex<DateTime<Utc>>>,
    pub(super) agent_registry: Arc<Mutex<AgentRegistry>>,
    pub(super) agent_registry_notify: Arc<Notify>,
    pub(super) max_global_sub_agents: usize,
    pub(super) subagent_count: Arc<AtomicUsize>,
    pub(super) cron_poll_interval_seconds: u64,
    pub(super) background_job_sender: mpsc::Sender<BackgroundJobRequest>,
    pub(super) background_terminate_flags: Arc<Mutex<HashSet<Uuid>>>,
    pub(super) summary_tracker: Arc<SummaryTracker>,
    pub(super) active_foreground_agent_frame_runtimes:
        Arc<Mutex<HashMap<String, Arc<Mutex<ActiveForegroundAgentFrameRuntime>>>>>,
    pub(super) subagents: Arc<Mutex<HashMap<Uuid, Arc<HostedSubagent>>>>,
    pub(super) conversations: Arc<Mutex<ConversationManager>>,
    pub(super) snapshots: Arc<Mutex<SnapshotManager>>,
}

impl RuntimeContext {
    pub(super) fn with_sessions<T>(
        &self,
        f: impl FnOnce(&mut SessionManager) -> Result<T>,
    ) -> Result<T> {
        let mut sessions = self
            .sessions
            .lock()
            .map_err(|_| anyhow!("session manager lock poisoned"))?;
        f(&mut sessions)
    }

    pub(super) fn with_conversations<T>(
        &self,
        f: impl FnOnce(&mut ConversationManager) -> Result<T>,
    ) -> Result<T> {
        let mut conversations = self
            .conversations
            .lock()
            .map_err(|_| anyhow!("conversation manager lock poisoned"))?;
        f(&mut conversations)
    }

    pub(super) fn with_conversations_and_sessions<T>(
        &self,
        f: impl FnOnce(&mut ConversationManager, &mut SessionManager) -> Result<T>,
    ) -> Result<T> {
        let mut conversations = self
            .conversations
            .lock()
            .map_err(|_| anyhow!("conversation manager lock poisoned"))?;
        let mut sessions = self
            .sessions
            .lock()
            .map_err(|_| anyhow!("session manager lock poisoned"))?;
        f(&mut conversations, &mut sessions)
    }

    pub(super) fn with_snapshots<T>(
        &self,
        f: impl FnOnce(&mut SnapshotManager) -> Result<T>,
    ) -> Result<T> {
        let mut snapshots = self
            .snapshots
            .lock()
            .map_err(|_| anyhow!("snapshot manager lock poisoned"))?;
        f(&mut snapshots)
    }

    pub(super) fn ensure_foreground_actor(
        &self,
        address: &ChannelAddress,
    ) -> Result<SessionActorRef> {
        self.with_conversations_and_sessions(|conversations, sessions| {
            conversations.ensure_foreground_actor(address, sessions)
        })
    }

    pub(super) fn resolve_foreground_actor(
        &self,
        address: &ChannelAddress,
    ) -> Result<Option<SessionActorRef>> {
        self.with_conversations_and_sessions(|conversations, sessions| {
            conversations.resolve_foreground_actor(address, sessions)
        })
    }

    pub(super) fn with_subagents<T>(
        &self,
        f: impl FnOnce(&mut HashMap<Uuid, Arc<HostedSubagent>>) -> Result<T>,
    ) -> Result<T> {
        let mut subagents = self
            .subagents
            .lock()
            .map_err(|_| anyhow!("subagent manager lock poisoned"))?;
        f(&mut subagents)
    }

    pub(super) fn invalidate_foreground_agent_frame_runtime(
        &self,
        address: &crate::domain::ChannelAddress,
    ) -> Result<()> {
        let session_key = address.session_key();
        let runtime = self
            .active_foreground_agent_frame_runtimes
            .lock()
            .map_err(|_| anyhow!("active foreground runtimes lock poisoned"))?
            .remove(&session_key);
        if let Some(runtime) = runtime
            && let Ok(mut runtime) = runtime.lock()
        {
            let _ = runtime.runtime.shutdown();
            if runtime.sandbox_mode == SandboxMode::Bubblewrap {
                let _ = self
                    .workspace_manager
                    .cleanup_transient_mounts(&runtime.workspace_id);
            }
        }
        Ok(())
    }
}
