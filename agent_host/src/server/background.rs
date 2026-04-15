use super::*;

impl AgentRuntimeView {
    pub(super) fn start_background_agent(
        &self,
        parent_agent_id: uuid::Uuid,
        session: SessionSnapshot,
        model_key: Option<String>,
        prompt: String,
        sink: SinkTarget,
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
                sink: sink.clone(),
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
            "sink": sink_target_to_value(&sink)
        }))
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
                let sink_router = self.sink_router.read().await;
                if let Err(error) = sink_router
                    .dispatch(&self.channels, &job.sink, outgoing)
                    .await
                    .context("failed to dispatch background agent reply")
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
                let sink_router = self.sink_router.read().await;
                sink_router
                    .dispatch(&self.channels, &job.sink, outgoing)
                    .await
                    .context("failed to dispatch yielded background agent reply")?;
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
            .contains("failed to dispatch background agent reply")
            || error
                .to_string()
                .contains("background agent error dispatch failed")
        {
            return Err(anyhow!(
                "background job failed and frontend dispatch is unavailable"
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
            let sink_router = self.sink_router.read().await;
            sink_router
                .dispatch(&self.channels, &job.sink, OutgoingMessage::text(text))
                .await
                .context("failed to dispatch background timeout notification")?;
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
                let sink_router = self.sink_router.read().await;
                sink_router
                    .dispatch(&self.channels, &job.sink, outgoing)
                    .await
                    .context("failed to dispatch recovered background agent reply")?;
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
                let sink_router = self.sink_router.read().await;
                sink_router
                    .dispatch(&self.channels, &job.sink, outgoing)
                    .await
                    .context("failed to dispatch yielded recovered background agent reply")?;
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
                let sink_router = self.sink_router.read().await;
                sink_router
                    .dispatch(&self.channels, &job.sink, OutgoingMessage::text(text))
                    .await
                    .context("failed to dispatch background failure notification")?;
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
                let sink_router = self.sink_router.read().await;
                sink_router
                    .dispatch(&self.channels, &job.sink, OutgoingMessage::text(text))
                    .await
                    .context("failed to dispatch background failure notification")?;
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
                let sink_router = self.sink_router.read().await;
                sink_router
                    .dispatch(&self.channels, &job.sink, OutgoingMessage::text(text))
                    .await
                    .context("failed to dispatch background failure notification")?;
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
