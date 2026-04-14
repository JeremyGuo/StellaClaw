use super::*;

impl ServerRuntime {
    fn subagent_prompt(description: &str) -> String {
        format!(
            "{description}\n\nThis is a delegated subtask for the caller. Keep the work narrowly scoped, prefer the fastest path to a correct result, and avoid exploring unrelated directions. Return a concise summary when you finish, including any files you changed and anything the caller must know before continuing."
        )
    }

    fn get_subagent_handle(&self, subagent_id: uuid::Uuid) -> Result<Arc<HostedSubagent>> {
        self.with_subagents(|subagents| {
            subagents
                .get(&subagent_id)
                .cloned()
                .ok_or_else(|| anyhow!("unknown subagent {}", subagent_id))
        })
    }

    fn persist_subagent_locked(
        subagent: &HostedSubagent,
        inner: &HostedSubagentInner,
    ) -> Result<()> {
        subagent.persist_locked(inner)
    }

    fn cleanup_subagent(&self, subagent: &HostedSubagent) -> Result<()> {
        self.with_subagents(|subagents| {
            subagents.remove(&subagent.id);
            Ok(())
        })?;
        subagent.remove_state_file()
    }

    pub(super) fn start_subagent(
        &self,
        parent_agent_id: uuid::Uuid,
        session: SessionSnapshot,
        description: String,
        model_key: Option<String>,
    ) -> Result<Value> {
        let agent_backend = self.effective_agent_backend()?;
        let model_key = model_key.unwrap_or(self.effective_main_model_key()?);
        self.model_config(&model_key)?;
        self.ensure_model_available_for_backend(agent_backend, &model_key)?;
        let timeout_seconds = self.default_subagent_timeout_seconds(&model_key)?;
        let subagent_id = uuid::Uuid::new_v4();
        let runtime_state_root = self
            .agent_workspace
            .root_dir
            .join("runtime")
            .join(&session.workspace_id);
        let initial_prompt = Self::subagent_prompt(&description);
        let subagent = HostedSubagent::create(
            subagent_id,
            parent_agent_id,
            session.id,
            session.address.clone(),
            session.workspace_id.clone(),
            session.workspace_root.clone(),
            runtime_state_root,
            agent_backend,
            model_key.clone(),
            description.clone(),
            timeout_seconds,
            timeout_seconds,
            initial_prompt,
        )?;
        self.register_managed_agent(
            subagent_id,
            ManagedAgentKind::Subagent,
            model_key.clone(),
            Some(parent_agent_id),
            &session,
            ManagedAgentState::Running,
        );
        self.with_subagents(|subagents| {
            subagents.insert(subagent_id, Arc::clone(&subagent));
            Ok(())
        })?;
        let runtime = self.clone();
        thread::spawn(move || {
            if let Err(error) = runtime.run_subagent_worker(subagent) {
                error!(
                    log_stream = "agent",
                    kind = "subagent_worker_failed",
                    agent_id = %subagent_id,
                    error = %format!("{error:#}"),
                    "subagent worker failed"
                );
            }
        });
        Ok(json!({
            "agent_id": subagent_id,
            "model": model_key,
            "description": description,
            "timeout_seconds": timeout_seconds,
        }))
    }

    pub(super) fn kill_subagent(
        &self,
        session: &SessionSnapshot,
        subagent_id: uuid::Uuid,
    ) -> Result<Value> {
        let subagent = self.get_subagent_handle(subagent_id)?;
        if subagent.session_id != session.id {
            return Err(anyhow!(
                "subagent {} does not belong to this session",
                subagent_id
            ));
        }
        {
            let mut inner = subagent
                .inner
                .lock()
                .map_err(|_| anyhow!("subagent state lock poisoned"))?;
            inner.persisted.state = SubagentState::Destroyed;
            inner.persisted.updated_at = Utc::now();
            inner.persisted.last_error = Some("destroyed".to_string());
            if let Some(control) = inner.active_control.take() {
                control.request_cancel();
            }
            Self::persist_subagent_locked(&subagent, &inner)?;
        }
        subagent.condvar.notify_all();
        self.cleanup_subagent(&subagent)?;
        Ok(json!({
            "agent_id": subagent_id,
            "killed": true
        }))
    }

    pub(super) fn join_subagent(
        &self,
        session: &SessionSnapshot,
        subagent_id: uuid::Uuid,
        timeout_seconds: f64,
        control: Option<SessionExecutionControl>,
    ) -> Result<Value> {
        if timeout_seconds < 0.0 {
            return Err(anyhow!("timeout_seconds must be non-negative"));
        }
        let subagent = self.get_subagent_handle(subagent_id)?;
        if subagent.session_id != session.id {
            return Err(anyhow!(
                "subagent {} does not belong to this session",
                subagent_id
            ));
        }
        let deadline = (timeout_seconds > 0.0)
            .then(|| std::time::Instant::now() + Duration::from_secs_f64(timeout_seconds));
        let signal_receiver = control
            .as_ref()
            .map(SessionExecutionControl::signal_receiver);
        loop {
            let inner = subagent
                .inner
                .lock()
                .map_err(|_| anyhow!("subagent state lock poisoned"))?;
            match inner.persisted.state {
                SubagentState::Ready => {
                    let payload = json!({
                        "agent_id": subagent_id,
                        "running": false,
                        "completed": true,
                        "reason": "completed",
                        "text": inner.persisted.last_result_text,
                        "attachment_paths": inner.persisted.last_attachment_paths,
                    });
                    drop(inner);
                    self.cleanup_subagent(&subagent)?;
                    return Ok(payload);
                }
                SubagentState::Failed => {
                    let payload = json!({
                        "agent_id": subagent_id,
                        "running": false,
                        "completed": false,
                        "reason": "failed",
                        "error": inner.persisted.last_error,
                    });
                    drop(inner);
                    self.cleanup_subagent(&subagent)?;
                    return Ok(payload);
                }
                SubagentState::Destroyed => {
                    return Ok(json!({
                        "agent_id": subagent_id,
                        "running": false,
                        "completed": false,
                        "reason": "destroyed",
                    }));
                }
                SubagentState::Running | SubagentState::WaitingForCharge => {}
            }
            let wait_duration = deadline.map(|deadline| {
                deadline
                    .saturating_duration_since(std::time::Instant::now())
                    .min(Duration::from_millis(200))
            });
            drop(inner);
            if let Some(receiver) = &signal_receiver
                && receiver.try_recv().is_ok()
            {
                return Ok(json!({
                    "agent_id": subagent_id,
                    "running": true,
                    "completed": false,
                    "interrupted": true,
                    "reason": "agent_turn_interrupted",
                }));
            }
            if let Some(deadline) = deadline
                && std::time::Instant::now() >= deadline
            {
                return Ok(json!({
                    "agent_id": subagent_id,
                    "running": true,
                    "completed": false,
                    "timed_out": true,
                    "reason": "wait_timed_out",
                }));
            }
            let inner = subagent
                .inner
                .lock()
                .map_err(|_| anyhow!("subagent state lock poisoned"))?;
            let wait_duration = wait_duration.unwrap_or(Duration::from_millis(200));
            let _ = subagent
                .condvar
                .wait_timeout(inner, wait_duration)
                .map_err(|_| anyhow!("subagent state lock poisoned"))?;
        }
    }

    pub(super) fn destroy_subagents_for_session(&self, session_id: uuid::Uuid) -> Result<usize> {
        let targets = self.with_subagents(|subagents| {
            Ok(subagents
                .values()
                .filter(|subagent| subagent.session_id == session_id)
                .cloned()
                .collect::<Vec<_>>())
        })?;
        let mut destroyed = 0usize;
        for subagent in targets {
            {
                let mut inner = subagent
                    .inner
                    .lock()
                    .map_err(|_| anyhow!("subagent state lock poisoned"))?;
                if inner.persisted.state == SubagentState::Destroyed {
                    continue;
                }
                inner.persisted.state = SubagentState::Destroyed;
                inner.persisted.updated_at = Utc::now();
                inner.persisted.last_error = Some("session_destroyed".to_string());
                if let Some(control) = inner.active_control.take() {
                    control.request_cancel();
                }
                Self::persist_subagent_locked(&subagent, &inner)?;
            }
            subagent.condvar.notify_all();
            self.cleanup_subagent(&subagent)?;
            destroyed = destroyed.saturating_add(1);
        }
        Ok(destroyed)
    }

    fn subagent_session_snapshot(&self, subagent: &HostedSubagent) -> SessionSnapshot {
        SessionSnapshot {
            id: subagent.session_id,
            agent_id: subagent.id,
            address: subagent.address.clone(),
            root_dir: self.agent_workspace.root_dir.join("subagent-sessions"),
            attachments_dir: subagent.workspace_root.join("upload"),
            workspace_id: subagent.workspace_id.clone(),
            workspace_root: subagent.workspace_root.clone(),
            last_user_message_at: None,
            last_agent_returned_at: None,
            last_compacted_at: None,
            turn_count: 0,
            last_compacted_turn_count: 0,
            cumulative_usage: TokenUsage::default(),
            cumulative_compaction: SessionCompactionStats::default(),
            api_timeout_override_seconds: None,
            skill_states: HashMap::new(),
            seen_user_profile_version: None,
            seen_identity_profile_version: None,
            seen_model_catalog_version: None,
            zgent_native: None,
            pending_workspace_summary: false,
            close_after_summary: false,
            session_state: crate::session::DurableSessionState::default(),
        }
    }

    pub(super) fn idle_compact_subagents_for_session(
        &self,
        session: &SessionSnapshot,
        idle_threshold: Duration,
    ) -> Result<()> {
        let now = Utc::now();
        let subagents = self.with_subagents(|subagents| {
            Ok(subagents
                .values()
                .filter(|subagent| subagent.session_id == session.id)
                .cloned()
                .collect::<Vec<_>>())
        })?;
        for subagent in subagents {
            let (agent_backend, model_key, messages, turn_count) = {
                let inner = subagent
                    .inner
                    .lock()
                    .map_err(|_| anyhow!("subagent state lock poisoned"))?;
                if inner.persisted.state != SubagentState::Ready {
                    continue;
                }
                let Some(last_returned_at) = inner.persisted.last_returned_at else {
                    continue;
                };
                if now
                    .signed_duration_since(last_returned_at)
                    .to_std()
                    .unwrap_or_default()
                    < idle_threshold
                {
                    continue;
                }
                if inner.persisted.turn_count <= inner.persisted.last_compacted_turn_count
                    || inner.persisted.messages.is_empty()
                {
                    continue;
                }
                (
                    inner.persisted.agent_backend,
                    inner.persisted.model_key.clone(),
                    inner.persisted.messages.clone(),
                    inner.persisted.turn_count,
                )
            };
            let subagent_session = self.subagent_session_snapshot(&subagent);
            let config = self.build_agent_frame_config(
                &subagent_session,
                &subagent.workspace_root,
                AgentPromptKind::SubAgent,
                &model_key,
                None,
            )?;
            let extra_tools = self.build_extra_tools(
                &subagent_session,
                AgentPromptKind::SubAgent,
                subagent.id,
                None,
            );
            let report = run_backend_compaction(agent_backend, messages, config, extra_tools)?;
            if !report.compacted {
                continue;
            }
            let stats = compaction_stats_from_report(&report);
            let mut inner = subagent
                .inner
                .lock()
                .map_err(|_| anyhow!("subagent state lock poisoned"))?;
            inner.persisted.messages = report.messages;
            inner.persisted.last_compacted_turn_count = turn_count;
            inner.persisted.updated_at = Utc::now();
            inner.persisted.cumulative_compaction.run_count = inner
                .persisted
                .cumulative_compaction
                .run_count
                .saturating_add(stats.run_count);
            inner.persisted.cumulative_compaction.compacted_run_count = inner
                .persisted
                .cumulative_compaction
                .compacted_run_count
                .saturating_add(stats.compacted_run_count);
            inner
                .persisted
                .cumulative_compaction
                .estimated_tokens_before = inner
                .persisted
                .cumulative_compaction
                .estimated_tokens_before
                .saturating_add(stats.estimated_tokens_before);
            inner.persisted.cumulative_compaction.estimated_tokens_after = inner
                .persisted
                .cumulative_compaction
                .estimated_tokens_after
                .saturating_add(stats.estimated_tokens_after);
            inner
                .persisted
                .cumulative_compaction
                .usage
                .add_assign(&stats.usage);
            Self::persist_subagent_locked(&subagent, &inner)?;
        }
        Ok(())
    }

    fn try_acquire_subagent_slot(&self) -> Result<SubAgentSlot> {
        loop {
            let current = self.subagent_count.load(Ordering::SeqCst);
            if current >= self.max_global_sub_agents {
                return Err(anyhow!(
                    "global subagent limit reached ({})",
                    self.max_global_sub_agents
                ));
            }
            if self
                .subagent_count
                .compare_exchange(current, current + 1, Ordering::SeqCst, Ordering::SeqCst)
                .is_ok()
            {
                return Ok(SubAgentSlot {
                    counter: Arc::clone(&self.subagent_count),
                });
            }
        }
    }

    fn run_subagent_worker(&self, subagent: Arc<HostedSubagent>) -> Result<()> {
        let _slot = self.try_acquire_subagent_slot()?;
        let (agent_backend, model_key, previous_messages, prompt, timeout_seconds) = {
            let mut inner = subagent
                .inner
                .lock()
                .map_err(|_| anyhow!("subagent state lock poisoned"))?;
            loop {
                match inner.persisted.state {
                    SubagentState::Destroyed | SubagentState::Failed => {
                        Self::persist_subagent_locked(&subagent, &inner)?;
                        return Ok(());
                    }
                    SubagentState::Ready if !inner.queued_prompts.is_empty() => {
                        inner.persisted.state = SubagentState::Running;
                        break;
                    }
                    SubagentState::Running => break,
                    SubagentState::Ready | SubagentState::WaitingForCharge => {
                        inner = subagent
                            .condvar
                            .wait(inner)
                            .map_err(|_| anyhow!("subagent state lock poisoned"))?;
                    }
                }
            }

            let prompt = inner.queued_prompts.pop_front().unwrap_or_default();
            inner.persisted.pending_prompts = inner.queued_prompts.iter().cloned().collect();
            let timeout_seconds = inner.persisted.default_charge_seconds;
            inner.persisted.updated_at = Utc::now();
            Self::persist_subagent_locked(&subagent, &inner)?;
            (
                inner.persisted.agent_backend,
                inner.persisted.model_key.clone(),
                inner.persisted.messages.clone(),
                prompt,
                timeout_seconds,
            )
        };

        self.mark_managed_agent_running(subagent.id);
        let runtime = self.clone();
        let subagent_for_observer = Arc::clone(&subagent);
        let control_observer: Arc<dyn Fn(SessionExecutionControl) + Send + Sync> =
            Arc::new(move |control| {
                if let Ok(mut inner) = subagent_for_observer.inner.lock() {
                    inner.active_control = Some(control);
                }
            });
        let session = SessionSnapshot {
            id: subagent.session_id,
            agent_id: subagent.id,
            address: subagent.address.clone(),
            root_dir: self.agent_workspace.root_dir.join("subagent-sessions"),
            attachments_dir: subagent.workspace_root.join("upload"),
            workspace_id: subagent.workspace_id.clone(),
            workspace_root: subagent.workspace_root.clone(),
            last_user_message_at: None,
            last_agent_returned_at: None,
            last_compacted_at: None,
            turn_count: 0,
            last_compacted_turn_count: 0,
            cumulative_usage: TokenUsage::default(),
            cumulative_compaction: SessionCompactionStats::default(),
            api_timeout_override_seconds: None,
            skill_states: HashMap::new(),
            seen_user_profile_version: None,
            seen_identity_profile_version: None,
            seen_model_catalog_version: None,
            zgent_native: None,
            pending_workspace_summary: false,
            close_after_summary: false,
            session_state: crate::session::DurableSessionState {
                messages: previous_messages.clone(),
                pending_messages: Vec::new(),
                system_prompt_static_hash: None,
                system_prompt_component_hashes: Default::default(),
                pending_system_prompt_component_notices: Default::default(),
                phase: crate::session::SessionPhase::End,
                errno: None,
                errinfo: None,
                progress_message: None,
            },
        };
        let upstream_timeout_seconds = self.model_upstream_timeout_seconds(&model_key)?;
        let outcome = runtime.run_agent_turn_with_timeout_blocking(
            session.clone(),
            AgentPromptKind::SubAgent,
            subagent.id,
            agent_backend,
            model_key.clone(),
            previous_messages,
            prompt,
            Some(timeout_seconds),
            Some(upstream_timeout_seconds),
            Some(control_observer),
            "subagent",
        )?;
        let mut inner = subagent
            .inner
            .lock()
            .map_err(|_| anyhow!("subagent state lock poisoned"))?;
        inner.active_control = None;
        match outcome {
            TimedRunOutcome::Completed(report) | TimedRunOutcome::Yielded(report) => {
                inner.persisted.messages = report.messages;
                inner.persisted.resume_pending = false;
                inner.persisted.state = SubagentState::Ready;
                inner.persisted.updated_at = Utc::now();
                inner.persisted.last_returned_at = Some(Utc::now());
                inner.persisted.turn_count = inner.persisted.turn_count.saturating_add(1);
                inner.persisted.cumulative_usage.add_assign(&report.usage);
                inner.persisted.cumulative_compaction.run_count = inner
                    .persisted
                    .cumulative_compaction
                    .run_count
                    .saturating_add(report.compaction.run_count);
                inner.persisted.cumulative_compaction.compacted_run_count = inner
                    .persisted
                    .cumulative_compaction
                    .compacted_run_count
                    .saturating_add(report.compaction.compacted_run_count);
                inner
                    .persisted
                    .cumulative_compaction
                    .estimated_tokens_before = inner
                    .persisted
                    .cumulative_compaction
                    .estimated_tokens_before
                    .saturating_add(report.compaction.estimated_tokens_before);
                inner.persisted.cumulative_compaction.estimated_tokens_after = inner
                    .persisted
                    .cumulative_compaction
                    .estimated_tokens_after
                    .saturating_add(report.compaction.estimated_tokens_after);
                inner
                    .persisted
                    .cumulative_compaction
                    .usage
                    .add_assign(&report.compaction.usage);
                let assistant_text = extract_assistant_text(&inner.persisted.messages);
                let (clean_text, attachments) =
                    extract_attachment_references(&assistant_text, &subagent.workspace_root)?;
                inner.persisted.last_result_text = Some(clean_text);
                inner.persisted.last_attachment_paths = attachments
                    .iter()
                    .map(|item| relative_attachment_path(&subagent.workspace_root, &item.path))
                    .collect::<Result<Vec<_>>>()?;
                inner.persisted.last_error = None;
                self.mark_managed_agent_completed(subagent.id, &report.usage);
                log_turn_usage(
                    subagent.id,
                    &session,
                    &report.usage,
                    false,
                    "subagent",
                    Some(inner.persisted.parent_agent_id),
                );
            }
            TimedRunOutcome::TimedOut { state, error } => {
                if let Some(state) = state {
                    inner.persisted.messages = state.messages;
                    inner.persisted.cumulative_usage.add_assign(&state.usage);
                    inner.persisted.cumulative_compaction.run_count = inner
                        .persisted
                        .cumulative_compaction
                        .run_count
                        .saturating_add(state.compaction.run_count);
                    inner.persisted.cumulative_compaction.compacted_run_count = inner
                        .persisted
                        .cumulative_compaction
                        .compacted_run_count
                        .saturating_add(state.compaction.compacted_run_count);
                    inner
                        .persisted
                        .cumulative_compaction
                        .estimated_tokens_before = inner
                        .persisted
                        .cumulative_compaction
                        .estimated_tokens_before
                        .saturating_add(state.compaction.estimated_tokens_before);
                    inner.persisted.cumulative_compaction.estimated_tokens_after = inner
                        .persisted
                        .cumulative_compaction
                        .estimated_tokens_after
                        .saturating_add(state.compaction.estimated_tokens_after);
                    inner
                        .persisted
                        .cumulative_compaction
                        .usage
                        .add_assign(&state.compaction.usage);
                }
                inner.persisted.resume_pending = false;
                inner.persisted.state = SubagentState::Failed;
                inner.persisted.updated_at = Utc::now();
                inner.persisted.last_returned_at = Some(Utc::now());
                inner.persisted.last_error = Some(format!("{error:#}"));
                self.mark_managed_agent_timed_out(
                    subagent.id,
                    &inner.persisted.cumulative_usage,
                    &error,
                );
            }
            TimedRunOutcome::Failed(error) => {
                inner.persisted.resume_pending = false;
                inner.persisted.state = SubagentState::Failed;
                inner.persisted.updated_at = Utc::now();
                inner.persisted.last_error = Some(format!("{error:#}"));
                self.mark_managed_agent_failed(
                    subagent.id,
                    &inner.persisted.cumulative_usage,
                    &error,
                );
            }
        }
        Self::persist_subagent_locked(&subagent, &inner)?;
        drop(inner);
        subagent.condvar.notify_all();
        Ok(())
    }
}
