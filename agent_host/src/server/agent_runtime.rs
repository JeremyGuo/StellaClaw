use super::*;

#[derive(Clone)]
pub(super) struct AgentRuntimeView {
    pub(super) context: Arc<RuntimeContext>,
    pub(super) active_workspace_ids: Vec<String>,
    pub(super) selected_agent_backend: Option<AgentBackendKind>,
    pub(super) selected_main_model_key: Option<String>,
    pub(super) selected_reasoning_effort: Option<String>,
    pub(super) selected_context_compaction_enabled: Option<bool>,
    pub(super) selected_chat_version_id: Option<Uuid>,
    pub(super) sandbox: SandboxConfig,
}

pub(super) struct SubAgentSlot {
    pub(super) counter: Arc<AtomicUsize>,
}

pub(super) struct SummaryInProgressGuard {
    tracker: Arc<SummaryTracker>,
}

pub(super) struct SummaryTracker {
    count: Mutex<usize>,
    condvar: Condvar,
}

pub(super) enum TimedRunOutcome {
    Completed(SessionState),
    Yielded(SessionState),
    TimedOut {
        state: Option<SessionState>,
        error: anyhow::Error,
    },
    Failed(anyhow::Error),
}

impl Drop for SubAgentSlot {
    fn drop(&mut self) {
        self.counter.fetch_sub(1, Ordering::SeqCst);
    }
}

impl Drop for SummaryInProgressGuard {
    fn drop(&mut self) {
        let mut count = self.tracker.count.lock().unwrap();
        *count = count.saturating_sub(1);
        if *count == 0 {
            self.tracker.condvar.notify_all();
        }
    }
}

impl SummaryInProgressGuard {
    pub(super) fn new(tracker: Arc<SummaryTracker>) -> Self {
        let mut count = tracker.count.lock().unwrap();
        *count += 1;
        drop(count);
        Self { tracker }
    }
}

impl SummaryTracker {
    pub(super) fn new() -> Self {
        Self {
            count: Mutex::new(0),
            condvar: Condvar::new(),
        }
    }

    pub(super) fn wait_for_zero(&self) {
        let mut count = self.count.lock().unwrap();
        while *count > 0 {
            count = self.condvar.wait(count).unwrap();
        }
    }
}

impl Deref for AgentRuntimeView {
    type Target = RuntimeContext;

    fn deref(&self) -> &Self::Target {
        &self.context
    }
}
