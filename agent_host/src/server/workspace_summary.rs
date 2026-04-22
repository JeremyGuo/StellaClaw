use super::*;

impl Server {
    pub(super) async fn summarize_active_workspaces_on_shutdown(&self) -> Result<()> {
        let snapshots = self.with_sessions(|sessions| Ok(sessions.list_foreground_snapshots()))?;
        let mut first_error = None;
        for session in snapshots {
            let _ = self
                .with_sessions(|sessions| sessions.resolve_foreground_by_address(&session.address))
                .and_then(|actor| actor.mark_workspace_summary_state(true, false));
            if let Err(error) = self.summarize_workspace_before_destroy(&session).await {
                warn!(
                    log_stream = "session",
                    log_key = %session.id,
                    kind = "workspace_summary_on_shutdown_failed",
                    workspace_id = %session.workspace_id,
                    error = %format!("{error:#}"),
                    "failed to summarize workspace on shutdown"
                );
                if first_error.is_none() {
                    first_error = Some(error);
                }
            }
        }
        if let Some(error) = first_error {
            return Err(error);
        }
        Ok(())
    }

    async fn summarize_workspace_before_destroy(&self, session: &SessionSnapshot) -> Result<()> {
        let _summary_guard = SummaryInProgressGuard::new(Arc::clone(&self.summary_tracker));
        if self.remote_execution_active(&session.address)? {
            let actor = self.with_sessions(|sessions| {
                sessions.resolve_foreground_by_address(&session.address)
            })?;
            actor.mark_workspace_summary_state(false, false)?;
            return Ok(());
        }
        let entries =
            self.workspace_manager
                .list_workspace_contents(&session.workspace_id, None, 3, 200)?;
        let request_messages = session.request_messages();
        let request_message_count = request_messages.len();
        if request_message_count == 0 && entries.is_empty() {
            let actor = self.with_sessions(|sessions| {
                sessions.resolve_foreground_by_address(&session.address)
            })?;
            actor.mark_workspace_summary_state(false, false)?;
            return Ok(());
        }

        let mut previous_messages = request_messages;
        let effective_model_key = self.effective_main_model_key(&session.address)?;
        if self.effective_context_compaction_enabled(&session.address)? && request_message_count > 1
        {
            let runtime = self.agent_runtime_view();
            let config = runtime.build_agent_frame_config(
                session,
                &session.workspace_root,
                AgentPromptKind::MainForeground,
                &effective_model_key,
                None,
            )?;
            let extra_tools = runtime.build_extra_tools(
                session,
                AgentPromptKind::MainForeground,
                session.agent_id,
                None,
            );
            let compaction_messages = sanitize_messages_for_model_capabilities(
                &previous_messages,
                self.model_config_or_main(&effective_model_key)?,
                backend_supports_native_multimodal_input(AgentBackendKind::AgentFrame),
            );
            let compaction = run_backend_compaction(
                AgentBackendKind::AgentFrame,
                compaction_messages,
                config,
                extra_tools,
            )?;
            if compaction.compacted {
                previous_messages = compaction.messages;
            }
        }

        let workspace = self
            .workspace_manager
            .ensure_workspace_exists(&session.workspace_id)?;
        let tree = entries
            .iter()
            .map(|entry| format!("- {} ({})", entry.path, entry.entry_type))
            .collect::<Vec<_>>()
            .join("\n");
        let prompt = format!(
            "You are about to stop working in the current workspace.\n\nSummarize the work that has been done here for future agents.\nWrite a concise durable summary in plain text.\nFocus on:\n1. what work this workspace is mainly about\n2. what kinds of changes or outputs now exist in the workspace at a high level\n3. recent progress and current status\n4. any important unfinished direction a future agent should know\n\nKeep the summary at the level of work content and broad impact on the workspace.\nDo not explain files or directories one by one, and avoid file-level detail unless a path is truly the single most important entry point.\n\nCurrent stored summary:\n{}\n\nWorkspace file tree snapshot:\n{}\n\nReturn only the summary text. Do not use attachment tags.",
            workspace.summary,
            if tree.trim().is_empty() {
                "(workspace currently has no files)"
            } else {
                &tree
            }
        );
        let upstream_timeout_seconds = self.model_upstream_timeout_seconds(&effective_model_key)?;
        let runtime = self.agent_runtime_view_for_address(&session.address)?;
        let outcome = runtime
            .run_agent_turn_with_timeout(
                session.clone(),
                AgentPromptKind::MainForeground,
                session.agent_id,
                AgentBackendKind::AgentFrame,
                effective_model_key.clone(),
                previous_messages,
                prompt,
                Some(upstream_timeout_seconds),
                None,
                "workspace summary task join failed",
            )
            .await?;
        let state = match outcome {
            TimedRunOutcome::Completed(state) => state,
            TimedRunOutcome::Yielded(state) => state,
            TimedRunOutcome::TimedOut {
                state: Some(state), ..
            } => state,
            TimedRunOutcome::TimedOut { state: None, error } => return Err(error),
            TimedRunOutcome::Failed(error) => return Err(error),
        };
        let summary_text = extract_assistant_text(&state.messages);
        let (clean_summary, _) =
            extract_attachment_references(&summary_text, &session.workspace_root)?;
        let clean_summary = clean_summary.trim();
        if clean_summary.is_empty() {
            let actor = self.with_sessions(|sessions| {
                sessions.resolve_foreground_by_address(&session.address)
            })?;
            actor.mark_workspace_summary_state(false, false)?;
            return Ok(());
        }
        let updated = self.workspace_manager.update_summary(
            &session.workspace_id,
            clean_summary.to_string(),
            None,
        )?;
        let actor = self
            .with_sessions(|sessions| sessions.resolve_foreground_by_address(&session.address))?;
        actor.mark_workspace_summary_state(false, false)?;
        info!(
            log_stream = "session",
            log_key = %session.id,
            kind = "workspace_summary_updated",
            workspace_id = %session.workspace_id,
            summary_path = %updated.summary_path.display(),
            summary_len = clean_summary.len() as u64,
            "updated workspace summary before destroy"
        );
        Ok(())
    }

    pub(super) async fn retry_pending_workspace_summaries(&self) -> Result<()> {
        let pending =
            self.with_sessions(|sessions| Ok(sessions.pending_workspace_summary_snapshots()))?;
        for session in pending {
            if let Err(error) = self.summarize_workspace_before_destroy(&session).await {
                warn!(
                    log_stream = "session",
                    log_key = %session.id,
                    kind = "workspace_summary_retry_failed",
                    workspace_id = %session.workspace_id,
                    error = %format!("{error:#}"),
                    "failed to retry pending workspace summary on startup"
                );
                continue;
            }
            let actor = self.with_sessions(|sessions| {
                sessions.resolve_foreground_by_address(&session.address)
            })?;
            actor.mark_workspace_summary_state(false, false)?;
            if session.close_after_summary {
                self.destroy_foreground_session(&session.address)?;
            }
        }
        Ok(())
    }
}
