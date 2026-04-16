use super::*;

impl AgentRuntimeView {
    pub(super) fn start_background_agent(
        &self,
        parent_agent_id: uuid::Uuid,
        session: SessionSnapshot,
        model_key: Option<String>,
        prompt: String,
    ) -> Result<Value> {
        let background_agent_id = uuid::Uuid::new_v4();
        let agent_backend = self.effective_agent_backend()?;
        let model_key = match model_key {
            Some(model_key) => model_key,
            None => self.effective_main_model_key()?,
        };
        self.model_config(&model_key)?;
        self.ensure_model_available_for_backend(agent_backend, &model_key)?;
        let background_session =
            self.create_background_session_for_conversation(&session.address, background_agent_id)?;
        self.register_managed_agent(
            background_agent_id,
            ManagedAgentKind::Background,
            model_key.clone(),
            Some(parent_agent_id),
            &background_session,
            ManagedAgentState::Enqueued,
        );
        self.background_job_sender
            .blocking_send(BackgroundJobRequest {
                agent_id: background_agent_id,
                parent_agent_id: Some(parent_agent_id),
                cron_task_id: None,
                session: background_session.clone(),
                agent_backend,
                model_key: model_key.clone(),
                prompt,
            })
            .context("failed to enqueue background agent")?;
        info!(
            log_stream = "agent",
            log_key = %background_agent_id,
            kind = "background_agent_enqueued",
            parent_agent_id = %parent_agent_id,
            session_id = %background_session.id,
            channel_id = %background_session.address.channel_id,
            model = %model_key,
            "background agent enqueued"
        );
        Ok(json!({
            "agent_id": background_agent_id,
            "parent_agent_id": parent_agent_id,
            "model": model_key,
            "delivery": "current_foreground_conversation"
        }))
    }

    pub(super) fn request_background_terminate(&self, agent_id: uuid::Uuid) -> Result<()> {
        let mut flags = self
            .background_terminate_flags
            .lock()
            .map_err(|_| anyhow!("background terminate flags lock poisoned"))?;
        flags.insert(agent_id);
        Ok(())
    }

    fn take_background_terminate_requested(&self, agent_id: uuid::Uuid) -> bool {
        self.background_terminate_flags
            .lock()
            .map(|mut flags| flags.remove(&agent_id))
            .unwrap_or(false)
    }

    fn background_session_snapshot(&self, session_id: uuid::Uuid) -> Result<SessionSnapshot> {
        self.with_sessions(|sessions| sessions.background_snapshot(session_id))
    }

    async fn run_background_agent_turn(
        &self,
        session: &SessionSnapshot,
        agent_id: uuid::Uuid,
        agent_backend: AgentBackendKind,
        model_key: &str,
        prompt: String,
        join_label: &str,
    ) -> Result<TimedRunOutcome> {
        let upstream_timeout_seconds = self.model_upstream_timeout_seconds(model_key)?;
        self.run_agent_turn_with_timeout(
            session.clone(),
            AgentPromptKind::MainBackground,
            agent_id,
            agent_backend,
            model_key.to_string(),
            session.request_messages(),
            prompt,
            Some(upstream_timeout_seconds),
            None,
            join_label,
        )
        .await
    }

    fn persist_background_report(
        &self,
        session_id: uuid::Uuid,
        report: &SessionState,
    ) -> Result<()> {
        self.with_sessions(|sessions| {
            sessions.update_background_checkpoint(
                session_id,
                report.messages.clone(),
                &report.usage,
                &report.compaction,
            )
        })
    }

    fn foreground_runtime_for_address(&self, address: &ChannelAddress) -> Result<AgentRuntimeView> {
        let mut runtime = self.clone();
        let settings = self.with_conversations(|conversations| {
            conversations
                .ensure_conversation(address)
                .map(|snapshot| snapshot.settings)
        })?;
        runtime.selected_agent_backend = settings.agent_backend;
        runtime.selected_main_model_key = settings.main_model.clone();
        runtime.selected_reasoning_effort = settings.reasoning_effort.clone();
        runtime.selected_context_compaction_enabled = settings.context_compaction_enabled;
        runtime.selected_chat_version_id = Some(settings.chat_version_id);
        runtime.sandbox.mode = settings.sandbox_mode.unwrap_or(runtime.sandbox.mode);
        Ok(runtime)
    }

    fn ensure_foreground_session_for_background(
        &self,
        address: &ChannelAddress,
    ) -> Result<SessionSnapshot> {
        let preferred_workspace_id = self.with_conversations(|conversations| {
            Ok(conversations
                .ensure_conversation(address)?
                .settings
                .workspace_id)
        })?;
        let session = self.with_sessions(|sessions| match preferred_workspace_id.as_deref() {
            Some(workspace_id) => sessions.ensure_foreground_in_workspace(address, workspace_id),
            None => sessions.ensure_foreground(address),
        })?;
        self.with_conversations(|conversations| {
            conversations.set_workspace_id(address, Some(session.workspace_id.clone()))?;
            Ok(())
        })?;
        Ok(session)
    }

    fn build_foreground_prompt_state_for_runtime(
        &self,
        session: &SessionSnapshot,
        model_key: &str,
    ) -> Result<AgentSystemPromptState> {
        let model = self.model_config(model_key)?;
        let commands = self
            .command_catalog
            .get(&session.address.channel_id)
            .cloned()
            .unwrap_or_else(default_bot_commands);
        let workspace_summary = self
            .workspace_manager
            .ensure_workspace_exists(&session.workspace_id)
            .map(|workspace| workspace.summary)
            .unwrap_or_default();
        let remote_workpaths = self.with_conversations(|conversations| {
            Ok(conversations
                .get_snapshot(&session.address)
                .map(|snapshot| snapshot.settings.remote_workpaths)
                .unwrap_or_default())
        })?;
        Ok(build_agent_system_prompt_state(
            &self.agent_workspace,
            session,
            &workspace_summary,
            &remote_workpaths,
            AgentPromptKind::MainForeground,
            model_key,
            model,
            &self.models,
            &self.available_agent_models(AgentBackendKind::AgentFrame),
            &self.main_agent,
            &commands,
        ))
    }

    async fn wait_for_foreground_turn_to_finish(&self, address: &ChannelAddress) {
        let session_key = address.session_key();
        loop {
            let active = self
                .active_foreground_phases
                .lock()
                .ok()
                .is_some_and(|phases| phases.contains_key(&session_key));
            if !active {
                return;
            }
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
    }

    async fn maybe_compact_foreground_after_background_insert(
        &self,
        session: &SessionSnapshot,
        fallback_model_key: &str,
    ) -> Result<()> {
        let runtime = self.foreground_runtime_for_address(&session.address)?;
        let model_key = runtime
            .selected_main_model_key
            .clone()
            .unwrap_or_else(|| fallback_model_key.to_string());
        if !runtime
            .selected_context_compaction_enabled
            .unwrap_or(runtime.main_agent.enable_context_compression)
            || session.stable_message_count() == 0
        {
            return Ok(());
        }
        let model = runtime.model_config(&model_key)?.clone();
        let estimated_tokens =
            estimate_current_context_tokens_for_session(&runtime, session, &model_key)?;
        let trigger_tokens = (effective_context_window_limit_for_session(session, &model) as f64
            * runtime.main_agent.context_compaction.trigger_ratio)
            .floor() as usize;
        if estimated_tokens < trigger_tokens {
            return Ok(());
        }

        let config = runtime.build_agent_frame_config(
            session,
            &session.workspace_root,
            AgentPromptKind::MainForeground,
            &model_key,
            None,
        )?;
        let extra_tools = runtime.build_extra_tools(
            session,
            AgentPromptKind::MainForeground,
            session.agent_id,
            None,
        );
        let persistence_system_prompt = config.system_prompt.clone();
        let compaction_messages = sanitize_messages_for_model_capabilities(
            &session.request_messages(),
            &model,
            backend_supports_native_multimodal_input(AgentBackendKind::AgentFrame),
        );
        let report = run_backend_compaction(
            AgentBackendKind::AgentFrame,
            compaction_messages,
            config,
            extra_tools,
        )
        .with_context(|| format!("failed to compact foreground session {}", session.id))?;
        if !report.compacted {
            return Ok(());
        }
        let normalized_messages = normalize_messages_for_persistence(
            report.messages.clone(),
            &persistence_system_prompt,
            &[],
        );
        persist_compaction_artifacts(session, &report)?;
        let compaction_stats = compaction_stats_from_report(&report);
        self.with_sessions(|sessions| {
            sessions.record_idle_compaction(
                &session.address,
                normalized_messages,
                &compaction_stats,
            )
        })?;
        let prompt_state =
            runtime.build_foreground_prompt_state_for_runtime(session, &model_key)?;
        self.with_sessions(|sessions| {
            sessions.mark_system_prompt_state_current(
                &session.address,
                prompt_state.static_hash,
                prompt_state.dynamic_hashes,
            )
        })?;
        self.with_conversations(|conversations| {
            conversations
                .rotate_chat_version_id(&session.address)
                .map(|_| ())
        })?;
        info!(
            log_stream = "session",
            log_key = %session.id,
            kind = "background_insert_context_compacted",
            estimated_tokens,
            trigger_tokens,
            "compacted foreground context after background result insertion"
        );
        Ok(())
    }

    async fn deliver_background_outgoing_to_foreground(
        &self,
        session: &SessionSnapshot,
        model_key: &str,
        outgoing: OutgoingMessage,
    ) -> Result<()> {
        self.wait_for_foreground_turn_to_finish(&session.address)
            .await;
        let _ = self.ensure_foreground_session_for_background(&session.address)?;
        let foreground_session = self.with_sessions(|sessions| {
            sessions.append_background_result_to_foreground(
                &session.address,
                outgoing.text.clone(),
                Vec::new(),
            )
        })?;
        let channel = self
            .channels
            .get(&session.address.channel_id)
            .cloned()
            .with_context(|| format!("unknown channel {}", session.address.channel_id))?;
        channel
            .send(&session.address, outgoing)
            .await
            .context("failed to deliver background agent reply to foreground conversation")?;
        if let Err(error) = self
            .maybe_compact_foreground_after_background_insert(&foreground_session, model_key)
            .await
        {
            warn!(
                log_stream = "session",
                log_key = %foreground_session.id,
                kind = "background_insert_context_compaction_failed",
                error = %format!("{error:#}"),
                "failed to compact foreground context after background result insertion"
            );
        }
        Ok(())
    }

    pub(super) async fn run_background_job(&self, job: BackgroundJobRequest) -> Result<()> {
        let session = self.background_session_snapshot(job.session.id)?;
        self.mark_managed_agent_running(job.agent_id);
        info!(
            log_stream = "agent",
            log_key = %job.agent_id,
            kind = "background_agent_started",
            parent_agent_id = job.parent_agent_id.map(|value| value.to_string()),
            cron_task_id = job.cron_task_id.map(|value| value.to_string()),
            session_id = %session.id,
            channel_id = %session.address.channel_id,
            model = %job.model_key,
            "background agent started"
        );
        let run_result = self
            .run_background_agent_turn(
                &session,
                job.agent_id,
                job.agent_backend,
                &job.model_key,
                job.prompt.clone(),
                "background agent task join failed",
            )
            .await;

        if self.take_background_terminate_requested(job.agent_id) {
            let usage = match &run_result {
                Ok(TimedRunOutcome::Completed(report)) | Ok(TimedRunOutcome::Yielded(report)) => {
                    self.persist_background_report(session.id, report)?;
                    report.usage.clone()
                }
                Ok(TimedRunOutcome::TimedOut { state, .. }) => {
                    if let Some(state) = state {
                        self.persist_background_report(session.id, state)?;
                        state.usage.clone()
                    } else {
                        TokenUsage::default()
                    }
                }
                Ok(TimedRunOutcome::Failed(_)) | Err(_) => TokenUsage::default(),
            };
            self.mark_managed_agent_completed(job.agent_id, &usage);
            info!(
                log_stream = "agent",
                log_key = %job.agent_id,
                kind = "background_agent_terminated_silently",
                parent_agent_id = job.parent_agent_id.map(|value| value.to_string()),
                cron_task_id = job.cron_task_id.map(|value| value.to_string()),
                session_id = %session.id,
                channel_id = %session.address.channel_id,
                "background agent terminated without user-facing reply"
            );
            let _ = self.with_sessions(|sessions| sessions.close_background(session.id));
            return Ok(());
        }

        let outcome = match run_result {
            Ok(TimedRunOutcome::Completed(report)) => {
                self.with_sessions(|sessions| {
                    sessions.record_background_turn(
                        session.id,
                        report.messages.clone(),
                        &report.usage,
                        &report.compaction,
                    )
                })?;
                self.with_sessions(|sessions| {
                    sessions.append_background_user_message(
                        session.id,
                        Some(job.prompt.clone()),
                        Vec::new(),
                    )
                })?;
                let assistant_text = extract_assistant_text(&report.messages);
                let outgoing = build_outgoing_message_for_session(
                    &session,
                    &assistant_text,
                    &session.workspace_root,
                )?;
                self.with_sessions(|sessions| {
                    sessions.append_background_assistant_message(
                        session.id,
                        outgoing.text.clone(),
                        Vec::new(),
                    )
                })?;
                log_turn_usage(
                    job.agent_id,
                    &session,
                    &report.usage,
                    false,
                    "main_background",
                    job.parent_agent_id,
                );
                info!(
                    log_stream = "agent",
                    log_key = %job.agent_id,
                    kind = "background_agent_replied",
                    parent_agent_id = job.parent_agent_id.map(|value| value.to_string()),
                    cron_task_id = job.cron_task_id.map(|value| value.to_string()),
                    session_id = %session.id,
                    channel_id = %session.address.channel_id,
                    has_text = outgoing.text.as_deref().is_some_and(|text| !text.trim().is_empty()),
                    attachment_count = outgoing.attachments.len() as u64 + outgoing.images.len() as u64,
                    "background agent produced reply"
                );
                if let Err(error) = self
                    .deliver_background_outgoing_to_foreground(&session, &job.model_key, outgoing)
                    .await
                    .context("failed to deliver background agent reply")
                {
                    self.mark_managed_agent_failed(job.agent_id, &report.usage, &error);
                    let recovery = self
                        .handle_background_job_failure(&job, &session, &error)
                        .await
                        .with_context(|| format!("{error:#}"));
                    let _ = self.with_sessions(|sessions| sessions.close_background(session.id));
                    return recovery;
                }
                self.mark_managed_agent_completed(job.agent_id, &report.usage);
                Ok(())
            }
            Ok(TimedRunOutcome::Yielded(report)) => {
                self.with_sessions(|sessions| {
                    sessions.record_background_yielded_turn(
                        session.id,
                        report.messages.clone(),
                        &report.usage,
                        &report.compaction,
                    )
                })?;
                self.with_sessions(|sessions| {
                    sessions.append_background_user_message(
                        session.id,
                        Some(job.prompt.clone()),
                        Vec::new(),
                    )
                })?;
                let assistant_text = extract_assistant_text(&report.messages);
                let outgoing = build_outgoing_message_for_session(
                    &session,
                    &assistant_text,
                    &session.workspace_root,
                )?;
                self.with_sessions(|sessions| {
                    sessions.append_background_assistant_message(
                        session.id,
                        outgoing.text.clone(),
                        Vec::new(),
                    )
                })?;
                log_turn_usage(
                    job.agent_id,
                    &session,
                    &report.usage,
                    false,
                    "main_background",
                    job.parent_agent_id,
                );
                self.deliver_background_outgoing_to_foreground(&session, &job.model_key, outgoing)
                    .await
                    .context("failed to deliver yielded background agent reply")?;
                self.mark_managed_agent_completed(job.agent_id, &report.usage);
                Ok(())
            }
            Ok(TimedRunOutcome::TimedOut { state, error }) => {
                if let Some(state) = &state {
                    self.persist_background_report(session.id, state)?;
                }
                let usage = state
                    .as_ref()
                    .map(|state| state.usage.clone())
                    .unwrap_or_default();
                self.mark_managed_agent_timed_out(job.agent_id, &usage, &error);
                self.handle_background_job_failure(&job, &session, &error)
                    .await
            }
            Ok(TimedRunOutcome::Failed(error)) => {
                self.mark_managed_agent_failed(job.agent_id, &TokenUsage::default(), &error);
                self.handle_background_job_failure(&job, &session, &error)
                    .await
            }
            Err(error) => {
                self.mark_managed_agent_failed(job.agent_id, &TokenUsage::default(), &error);
                self.handle_background_job_failure(&job, &session, &error)
                    .await
            }
        };
        let _ = self.with_sessions(|sessions| sessions.close_background(session.id));
        outcome
    }

    async fn handle_background_job_failure(
        &self,
        job: &BackgroundJobRequest,
        session: &SessionSnapshot,
        error: &anyhow::Error,
    ) -> Result<()> {
        let session = self.background_session_snapshot(session.id)?;
        if error
            .to_string()
            .contains("failed to deliver background agent reply")
            || error
                .to_string()
                .contains("background agent error delivery failed")
        {
            return Err(anyhow!(
                "background job failed and frontend delivery failed"
            ));
        }

        if is_timeout_like(error) && self.has_active_child_agents(job.agent_id) {
            warn!(
                log_stream = "agent",
                log_key = %job.agent_id,
                kind = "background_agent_recovery_skipped_active_children",
                session_id = %session.id,
                channel_id = %session.address.channel_id,
                "background agent timed out while child agents were still active; skipping automatic recovery"
            );
            self.wait_for_child_agents_to_finish(job.agent_id).await;
            let text = background_timeout_with_active_children_text(&self.main_agent.language);
            self.deliver_background_outgoing_to_foreground(
                &session,
                &job.model_key,
                OutgoingMessage::text(text),
            )
            .await
            .context("failed to deliver background timeout notification")?;
            return Ok(());
        }

        let recovery_agent_id = uuid::Uuid::new_v4();
        self.register_managed_agent(
            recovery_agent_id,
            ManagedAgentKind::Background,
            job.model_key.clone(),
            Some(job.agent_id),
            &session,
            ManagedAgentState::Running,
        );
        info!(
            log_stream = "agent",
            log_key = %recovery_agent_id,
            kind = "background_agent_recovery_started",
            failed_agent_id = %job.agent_id,
            parent_agent_id = job.parent_agent_id.map(|value| value.to_string()),
            session_id = %session.id,
            channel_id = %session.address.channel_id,
            model = %job.model_key,
            "background failure recovery agent started"
        );

        let recovery_prompt = format!(
            "A previous main background agent failed before completing its work.\n\nOriginal task:\n{}\n\nFailure:\n{}\n\nYour job now:\n1. Diagnose the failure.\n2. If it is recoverable without user intervention, continue or retry the original task yourself now and produce the final user-facing result. Do not mention the failure unless it is relevant.\n3. If it is not recoverable, produce a concise user-facing explanation of the problem and what the user should do next.\n4. Do not say that you will continue later. Either complete the work now or explain the blocker clearly.",
            job.prompt, error
        );
        let recovery_prompt_for_history = recovery_prompt.clone();
        let run_result = self
            .run_background_agent_turn(
                &session,
                recovery_agent_id,
                job.agent_backend,
                &job.model_key,
                recovery_prompt,
                "background failure recovery task join failed",
            )
            .await;

        match run_result {
            Ok(TimedRunOutcome::Completed(report)) => {
                self.with_sessions(|sessions| {
                    sessions.record_background_turn(
                        session.id,
                        report.messages.clone(),
                        &report.usage,
                        &report.compaction,
                    )
                })?;
                self.with_sessions(|sessions| {
                    sessions.append_background_user_message(
                        session.id,
                        Some(recovery_prompt_for_history.clone()),
                        Vec::new(),
                    )
                })?;
                let assistant_text = extract_assistant_text(&report.messages);
                let outgoing = build_outgoing_message_for_session(
                    &session,
                    &assistant_text,
                    &session.workspace_root,
                )?;
                self.with_sessions(|sessions| {
                    sessions.append_background_assistant_message(
                        session.id,
                        outgoing.text.clone(),
                        Vec::new(),
                    )
                })?;
                self.deliver_background_outgoing_to_foreground(&session, &job.model_key, outgoing)
                    .await
                    .context("failed to deliver recovered background agent reply")?;
                self.mark_managed_agent_completed(recovery_agent_id, &report.usage);
                info!(
                    log_stream = "agent",
                    log_key = %recovery_agent_id,
                    kind = "background_agent_recovery_completed",
                    failed_agent_id = %job.agent_id,
                    "background failure recovery agent completed"
                );
                Ok(())
            }
            Ok(TimedRunOutcome::Yielded(report)) => {
                self.with_sessions(|sessions| {
                    sessions.record_background_yielded_turn(
                        session.id,
                        report.messages.clone(),
                        &report.usage,
                        &report.compaction,
                    )
                })?;
                self.with_sessions(|sessions| {
                    sessions.append_background_user_message(
                        session.id,
                        Some(recovery_prompt_for_history.clone()),
                        Vec::new(),
                    )
                })?;
                let assistant_text = extract_assistant_text(&report.messages);
                let outgoing = build_outgoing_message_for_session(
                    &session,
                    &assistant_text,
                    &session.workspace_root,
                )?;
                self.with_sessions(|sessions| {
                    sessions.append_background_assistant_message(
                        session.id,
                        outgoing.text.clone(),
                        Vec::new(),
                    )
                })?;
                self.deliver_background_outgoing_to_foreground(&session, &job.model_key, outgoing)
                    .await
                    .context("failed to deliver yielded recovered background agent reply")?;
                self.mark_managed_agent_completed(recovery_agent_id, &report.usage);
                Ok(())
            }
            Ok(TimedRunOutcome::TimedOut {
                state,
                error: recovery_error,
            }) => {
                if let Some(state) = &state {
                    self.persist_background_report(session.id, state)?;
                }
                let usage = state
                    .as_ref()
                    .map(|state| state.usage.clone())
                    .unwrap_or_default();
                self.mark_managed_agent_timed_out(recovery_agent_id, &usage, &recovery_error);
                let text = user_facing_error_text(&self.main_agent.language, error);
                self.deliver_background_outgoing_to_foreground(
                    &session,
                    &job.model_key,
                    OutgoingMessage::text(text),
                )
                .await
                .context("failed to deliver background failure notification")?;
                warn!(
                    log_stream = "agent",
                    log_key = %recovery_agent_id,
                    kind = "background_agent_recovery_failed",
                    failed_agent_id = %job.agent_id,
                    error = %format!("{recovery_error:#}"),
                    "background failure recovery agent timed out; user was notified"
                );
                Ok(())
            }
            Ok(TimedRunOutcome::Failed(recovery_error)) => {
                self.mark_managed_agent_failed(
                    recovery_agent_id,
                    &TokenUsage::default(),
                    &recovery_error,
                );
                let text = user_facing_error_text(&self.main_agent.language, error);
                self.deliver_background_outgoing_to_foreground(
                    &session,
                    &job.model_key,
                    OutgoingMessage::text(text),
                )
                .await
                .context("failed to deliver background failure notification")?;
                warn!(
                    log_stream = "agent",
                    log_key = %recovery_agent_id,
                    kind = "background_agent_recovery_failed",
                    failed_agent_id = %job.agent_id,
                    error = %format!("{recovery_error:#}"),
                    "background failure recovery agent failed; user was notified"
                );
                Ok(())
            }
            Err(recovery_error) => {
                self.mark_managed_agent_failed(
                    recovery_agent_id,
                    &TokenUsage::default(),
                    &recovery_error,
                );
                let text = user_facing_error_text(&self.main_agent.language, error);
                self.deliver_background_outgoing_to_foreground(
                    &session,
                    &job.model_key,
                    OutgoingMessage::text(text),
                )
                .await
                .context("failed to deliver background failure notification")?;
                warn!(
                    log_stream = "agent",
                    log_key = %recovery_agent_id,
                    kind = "background_agent_recovery_failed",
                    failed_agent_id = %job.agent_id,
                    error = %format!("{recovery_error:#}"),
                    "background failure recovery agent failed; user was notified"
                );
                Ok(())
            }
        }
    }
}
