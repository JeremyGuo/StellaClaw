use super::*;

#[derive(Clone, Debug)]
struct TurnSystemPromptState {
    active: String,
    full: String,
    static_hash: String,
    component_hashes: BTreeMap<String, String>,
}

impl TurnSystemPromptState {
    fn for_persistence(&self, compaction: &SessionCompactionStats) -> &str {
        if compaction.compacted_run_count > 0 {
            &self.full
        } else {
            &self.active
        }
    }
}

enum ForegroundTurnOutcome {
    Replied {
        state: SessionState,
        outgoing: OutgoingMessage,
    },
    Yielded(SessionState),
    Failed {
        resume_messages: Vec<ChatMessage>,
        progress_summary: String,
        compaction: SessionCompactionStats,
        error: anyhow::Error,
    },
}

pub(super) fn register_active_foreground_control(
    active_controls: &Arc<Mutex<HashMap<String, SessionExecutionControl>>>,
    pending_interrupts: &Arc<Mutex<HashSet<String>>>,
    session_key: &str,
    control: SessionExecutionControl,
) {
    if pending_interrupts
        .lock()
        .ok()
        .is_some_and(|interrupts| interrupts.contains(session_key))
    {
        control.request_yield();
    }
    if let Ok(mut controls) = active_controls.lock() {
        controls.insert(session_key.to_string(), control);
    }
}

struct TurnCoordinator<'a> {
    server: &'a Server,
    channel: &'a Arc<dyn Channel>,
    incoming: IncomingMessage,
}

impl<'a> TurnCoordinator<'a> {
    fn new(server: &'a Server, channel: &'a Arc<dyn Channel>, incoming: IncomingMessage) -> Self {
        Self {
            server,
            channel,
            incoming,
        }
    }

    async fn run(self) -> Result<()> {
        let server = self.server;
        let channel = self.channel;
        let incoming = self.incoming;

        if !server.has_complete_agent_selection(&incoming.address)? {
            server
                .send_channel_message(
                    channel,
                    &incoming.address,
                    server.agent_selection_message(
                        &incoming.address,
                        "Choose a model for this conversation before sending messages.",
                    )?,
                )
                .await?;
            return Ok(());
        }

        let session = server.ensure_foreground_session(&incoming.address)?;
        if session.stable_message_count() == 0 {
            if let Err(error) = server.initialize_foreground_session(&session, false).await {
                server
                    .send_user_error_message(channel, &incoming.address, &error)
                    .await;
                return Err(error);
            }
        }
        let session = server
            .with_sessions(|sessions| Ok(sessions.get_snapshot(&incoming.address)))?
            .expect("session should exist after initialization");

        let stored_attachments = if incoming.stored_attachments.is_empty() {
            server
                .materialize_attachments(&session.attachments_dir, incoming.attachments)
                .await?
        } else {
            incoming.stored_attachments.clone()
        };
        let skill_updates_prefix = server.observe_runtime_skill_changes(&session)?;
        let workspace_profile_notices = server.sync_runtime_profile_files(&session)?;
        server.observe_runtime_profile_changes(&session)?;
        server.observe_runtime_model_catalog_changes(&session)?;
        server.stage_runtime_profile_change_notices(&session, &workspace_profile_notices)?;
        let should_emit_runtime_change_notice =
            should_emit_runtime_change_prompt(incoming.text.as_deref());
        let mut profile_change_notices = if should_emit_runtime_change_notice {
            server.take_runtime_profile_change_notices(&session)?
        } else {
            Vec::new()
        };
        let mut model_catalog_change_notice = if should_emit_runtime_change_notice {
            render_model_catalog_change_notice(
                &server.take_runtime_model_catalog_change_notices(&session)?,
                &server.current_runtime_model_catalog(),
            )
        } else {
            None
        };
        let now = Utc::now();
        let user_time_tip = server
            .main_agent
            .time_awareness
            .emit_idle_time_gap_hint
            .then(|| render_last_user_message_time_tip(&session, now))
            .flatten();
        let system_date = server
            .main_agent
            .time_awareness
            .emit_system_date_on_user_message
            .then(|| render_system_date_on_user_message(now));
        let effective_model_key = server.effective_main_model_key(&incoming.address)?;
        server.log_current_tools_for_user_message(
            &session,
            &effective_model_key,
            &incoming.remote_message_id,
            "user_message",
        );
        let effective_model = server.model_config_or_main(&effective_model_key)?.clone();
        let user_message = build_user_turn_message(
            incoming.text.as_deref(),
            &stored_attachments,
            &effective_model,
            backend_supports_native_multimodal_input(AgentBackendKind::AgentFrame),
            system_date.as_deref(),
        )?;
        server.with_sessions(|sessions| {
            sessions.stage_foreground_user_turn(
                &incoming.address,
                user_message.clone(),
                incoming.text.clone(),
                stored_attachments.clone(),
            )
        })?;
        let session = server
            .with_sessions(|sessions| Ok(sessions.get_snapshot(&incoming.address)))?
            .expect("session should exist after staging pending user message");
        let prompt_state = server.build_foreground_prompt_state(&session, &effective_model_key)?;
        let prompt_observation = server.with_sessions(|sessions| {
            sessions.observe_system_prompt_state(
                &incoming.address,
                prompt_state.static_hash.clone(),
                prompt_state.dynamic_hashes.clone(),
            )
        })?;
        let dynamic_notice_keys = if should_emit_runtime_change_notice
            && !prompt_observation.static_changed
            && !prompt_observation.dynamic_notice_keys.is_empty()
        {
            server.with_sessions(|sessions| {
                sessions.take_system_prompt_dynamic_notices(&incoming.address)
            })?
        } else {
            Default::default()
        };
        let dynamic_system_prompt_notices = dynamic_notice_keys
            .iter()
            .filter_map(|key| prompt_state.dynamic_notices.get(key))
            .cloned()
            .collect::<Vec<_>>();
        if dynamic_notice_keys.contains("identity") {
            profile_change_notices
                .retain(|notice| !matches!(notice, SharedProfileChangeNotice::IdentityUpdated));
        }
        if dynamic_notice_keys.contains("user_meta") {
            profile_change_notices
                .retain(|notice| !matches!(notice, SharedProfileChangeNotice::UserUpdated));
        }
        if dynamic_notice_keys.contains("available_models")
            || dynamic_notice_keys.contains("current_model_profile")
        {
            model_catalog_change_notice = None;
        }
        let synthetic_system_messages = build_synthetic_system_messages(
            server.take_process_restart_notice(&incoming.address),
            user_time_tip.as_deref(),
            &dynamic_system_prompt_notices,
            model_catalog_change_notice.as_deref(),
            skill_updates_prefix.as_deref(),
            &profile_change_notices,
        );
        let base_messages = session.request_messages();
        let (mut previous_messages, active_system_prompt, rebuilt_system_prompt) =
            prepare_system_prompt_for_turn(
                &base_messages,
                &prompt_state.system_prompt,
                prompt_observation.static_changed,
            );
        previous_messages.extend(synthetic_system_messages.iter().cloned());
        if rebuilt_system_prompt {
            server.with_conversations(|conversations| {
                conversations
                    .rotate_chat_version_id(&incoming.address)
                    .map(|_| ())
            })?;
            server.with_sessions(|sessions| {
                sessions.mark_system_prompt_state_current(
                    &incoming.address,
                    prompt_state.static_hash.clone(),
                    prompt_state.dynamic_hashes.clone(),
                )
            })?;
        }
        let turn_system_prompt = TurnSystemPromptState {
            active: active_system_prompt,
            full: prompt_state.system_prompt.clone(),
            static_hash: prompt_state.static_hash.clone(),
            component_hashes: prompt_state.dynamic_hashes.clone(),
        };
        let mut active_session = session;
        let mut next_previous_messages = previous_messages;
        let mut ephemeral_system_messages = synthetic_system_messages.clone();

        channel
            .set_processing(&incoming.address, ProcessingState::Typing)
            .await
            .ok();
        let typing_guard = spawn_processing_keepalive(
            channel.clone(),
            incoming.address.clone(),
            ProcessingState::Typing,
        );

        let turn_result = server
            .run_foreground_turn_until_settled(
                &mut active_session,
                &effective_model_key,
                &mut next_previous_messages,
                &turn_system_prompt,
                &mut ephemeral_system_messages,
                "foreground agent turn failed",
            )
            .await;
        // Clear the pending-interrupt flag for this session so future turns
        // resume normally. The flag was set by the main event loop when a user
        // message arrived during this turn.
        if let Ok(mut interrupts) = server.pending_foreground_interrupts.lock() {
            interrupts.remove(&incoming.address.session_key());
        }
        if let Some(stop_sender) = typing_guard {
            let _ = stop_sender.send(());
        }
        if let Err(error) = &turn_result {
            channel
                .set_processing(&incoming.address, ProcessingState::Idle)
                .await
                .ok();
            server
                .send_user_error_message(channel, &incoming.address, error)
                .await;
        }
        let outcome = turn_result?;
        let (state, outgoing) = match outcome {
            ForegroundTurnOutcome::Replied { state, outgoing } => (state, outgoing),
            outcome => {
                if server
                    .finish_non_reply_foreground_outcome_for_channel(
                        channel,
                        &incoming.address,
                        &active_session,
                        outcome,
                        &turn_system_prompt,
                        &synthetic_system_messages,
                    )
                    .await?
                {
                    return Ok(());
                }
                unreachable!("replied branch should have matched above");
            }
        };

        server
            .persist_completed_foreground_turn(
                &incoming.address,
                active_session.stable_message_count(),
                state.messages,
                &active_session.session_state.pending_messages,
                &state.usage,
                &state.compaction,
                &turn_system_prompt,
                &synthetic_system_messages,
                Some(&outgoing),
            )
            .context("failed to persist agent_frame messages")?;
        server
            .finish_replied_foreground_outcome_for_channel(
                channel,
                &active_session,
                &incoming.address,
                &effective_model_key,
                &state.usage,
                outgoing,
                "user message reply",
            )
            .await?;
        Ok(())
    }
}

impl Server {
    fn take_process_restart_notice(&self, address: &ChannelAddress) -> Option<&'static str> {
        let session_key = address.session_key();
        let mut pending = self.pending_process_restart_notices.lock().ok()?;
        if pending.remove(&session_key) {
            info!(
                log_stream = "session",
                log_key = %session_key,
                kind = "process_restart_notice_emitted",
                "emitted one-shot process restart notice for foreground session"
            );
            Some(SYSTEM_RESTART_NOTICE)
        } else {
            None
        }
    }

    pub(super) async fn handle_continue_command(
        &self,
        channel: &Arc<dyn Channel>,
        incoming: &IncomingMessage,
    ) -> Result<()> {
        let session = self.ensure_foreground_session(&incoming.address)?;
        if session.session_state.phase != SessionPhase::Yielded {
            self.send_channel_message(
                channel,
                &incoming.address,
                OutgoingMessage::text(
                    "There is no interrupted turn to continue right now.".to_string(),
                ),
            )
            .await?;
            return Ok(());
        }
        let continue_model_key = self.effective_main_model_key(&incoming.address)?;
        channel
            .set_processing(&incoming.address, ProcessingState::Typing)
            .await
            .ok();
        let typing_guard = spawn_processing_keepalive(
            channel.clone(),
            incoming.address.clone(),
            ProcessingState::Typing,
        );
        let persistence_system_prompt =
            self.build_foreground_prompt_state(&session, &continue_model_key)?;
        let prompt_observation = self.with_sessions(|sessions| {
            sessions.observe_system_prompt_state(
                &incoming.address,
                persistence_system_prompt.static_hash.clone(),
                persistence_system_prompt.dynamic_hashes.clone(),
            )
        })?;
        self.log_current_tools_for_user_message(
            &session,
            &continue_model_key,
            &incoming.remote_message_id,
            "continue",
        );
        let mut active_session = session;
        let (mut next_previous_messages, active_system_prompt) = {
            let (resume_messages, active_system_prompt, rebuilt_system_prompt) =
                prepare_system_prompt_for_turn(
                    &active_session.request_messages(),
                    &persistence_system_prompt.system_prompt,
                    prompt_observation.static_changed,
                );
            if rebuilt_system_prompt {
                self.with_conversations(|conversations| {
                    conversations
                        .rotate_chat_version_id(&incoming.address)
                        .map(|_| ())
                })?;
                self.with_sessions(|sessions| {
                    sessions.mark_system_prompt_state_current(
                        &incoming.address,
                        persistence_system_prompt.static_hash.clone(),
                        persistence_system_prompt.dynamic_hashes.clone(),
                    )
                })?;
            }
            (resume_messages, active_system_prompt)
        };
        let turn_system_prompt = TurnSystemPromptState {
            active: active_system_prompt,
            full: persistence_system_prompt.system_prompt.clone(),
            static_hash: persistence_system_prompt.static_hash.clone(),
            component_hashes: persistence_system_prompt.dynamic_hashes.clone(),
        };
        let synthetic_system_messages = build_synthetic_system_messages(
            self.take_process_restart_notice(&incoming.address),
            None,
            &[],
            None,
            None,
            &[],
        );
        next_previous_messages.extend(synthetic_system_messages.iter().cloned());
        let mut ephemeral_system_messages = synthetic_system_messages.clone();
        let outcome = self
            .run_foreground_turn_until_settled(
                &mut active_session,
                &continue_model_key,
                &mut next_previous_messages,
                &turn_system_prompt,
                &mut ephemeral_system_messages,
                "failed to continue interrupted foreground turn",
            )
            .await;
        if let Ok(mut interrupts) = self.pending_foreground_interrupts.lock() {
            interrupts.remove(&incoming.address.session_key());
        }
        if let Some(stop_sender) = typing_guard {
            let _ = stop_sender.send(());
        }
        if let Err(error) = &outcome {
            channel
                .set_processing(&incoming.address, ProcessingState::Idle)
                .await
                .ok();
            self.send_user_error_message(channel, &incoming.address, error)
                .await;
        }
        match outcome? {
            ForegroundTurnOutcome::Replied { state, outgoing } => {
                self.persist_completed_foreground_turn(
                    &incoming.address,
                    active_session.stable_message_count(),
                    state.messages,
                    &active_session.session_state.pending_messages,
                    &state.usage,
                    &state.compaction,
                    &turn_system_prompt,
                    &synthetic_system_messages,
                    Some(&outgoing),
                )
                .context("failed to persist continued agent_frame messages")?;
                self.finish_replied_foreground_outcome_for_channel(
                    channel,
                    &active_session,
                    &incoming.address,
                    &continue_model_key,
                    &state.usage,
                    outgoing,
                    "continued interrupted turn",
                )
                .await?;
                Ok(())
            }
            outcome => {
                if self
                    .finish_non_reply_foreground_outcome_for_channel(
                        channel,
                        &incoming.address,
                        &active_session,
                        outcome,
                        &turn_system_prompt,
                        &synthetic_system_messages,
                    )
                    .await?
                {
                    return Ok(());
                }
                unreachable!("replied branch should have matched above");
            }
        }
    }

    pub(super) async fn handle_regular_foreground_message(
        &self,
        channel: &Arc<dyn Channel>,
        incoming: IncomingMessage,
    ) -> Result<()> {
        TurnCoordinator::new(self, channel, incoming).run().await
    }

    fn should_auto_resume_yielded_session(&self, session: &SessionSnapshot) -> bool {
        if session.session_state.phase != SessionPhase::Yielded
            || !session.session_state.pending_messages.is_empty()
            || session.session_state.errno.is_some()
        {
            return false;
        }
        // If a user message arrived while this turn was running (yield was requested
        // by request_yield_for_incoming), don't auto-resume — let the handler finish
        // so the conversation worker can process the queued interrupting message as
        // the next turn.
        let session_key = session.address.session_key();
        if let Ok(interrupts) = self.pending_foreground_interrupts.lock() {
            if interrupts.contains(&session_key) {
                return false;
            }
        }
        true
    }

    fn persist_failed_foreground_turn(
        &self,
        address: &ChannelAddress,
        resume_messages: Vec<ChatMessage>,
        progress_summary: String,
        compaction: &SessionCompactionStats,
        error: &anyhow::Error,
        system_prompt: &TurnSystemPromptState,
        ephemeral_system_messages: &[ChatMessage],
    ) -> Result<String> {
        let persistence_system_prompt = system_prompt.for_persistence(compaction);
        let resume_messages = normalize_messages_for_persistence(
            resume_messages,
            persistence_system_prompt,
            ephemeral_system_messages,
        );
        let session_errno = session_errno_for_turn_error(error);
        self.with_sessions(|sessions| {
            sessions.set_failed_foreground_turn(
                address,
                resume_messages,
                session_errno,
                Some(format!("{error:#}")),
            )
        })?;
        self.rotate_chat_version_if_compacted(address, compaction)?;
        self.mark_system_prompt_state_after_compaction(address, system_prompt, compaction)?;
        Ok(user_facing_continue_error_text(
            &self.main_agent.language,
            error,
            &progress_summary,
        ))
    }

    fn persist_completed_foreground_turn(
        &self,
        address: &ChannelAddress,
        previous_message_count: usize,
        messages: Vec<ChatMessage>,
        consumed_pending_messages: &[ChatMessage],
        usage: &TokenUsage,
        compaction: &SessionCompactionStats,
        system_prompt: &TurnSystemPromptState,
        ephemeral_system_messages: &[ChatMessage],
        append_assistant_history: Option<&OutgoingMessage>,
    ) -> Result<()> {
        let persistence_system_prompt = system_prompt.for_persistence(compaction);
        let messages = normalize_messages_for_persistence(
            messages,
            persistence_system_prompt,
            ephemeral_system_messages,
        );
        let loaded_skills = extract_loaded_skill_names(&messages, previous_message_count);
        self.with_sessions(|sessions| {
            sessions.record_agent_turn(
                address,
                messages,
                consumed_pending_messages,
                usage,
                compaction,
            )
        })?;
        self.rotate_chat_version_if_compacted(address, compaction)?;
        self.mark_system_prompt_state_after_compaction(address, system_prompt, compaction)?;
        self.with_sessions(|sessions| {
            sessions.mark_skills_loaded_current_turn(address, &loaded_skills)
        })?;
        if let Some(outgoing) = append_assistant_history {
            self.append_completed_foreground_assistant_history(address, outgoing)?;
        }
        Ok(())
    }

    fn mark_system_prompt_state_after_compaction(
        &self,
        address: &ChannelAddress,
        system_prompt: &TurnSystemPromptState,
        compaction: &SessionCompactionStats,
    ) -> Result<()> {
        if compaction.compacted_run_count == 0 {
            return Ok(());
        }
        self.with_sessions(|sessions| {
            sessions.mark_system_prompt_state_current(
                address,
                system_prompt.static_hash.clone(),
                system_prompt.component_hashes.clone(),
            )
        })
    }

    pub(super) fn append_completed_foreground_assistant_history(
        &self,
        address: &ChannelAddress,
        outgoing: &OutgoingMessage,
    ) -> Result<()> {
        self.with_sessions(|sessions| {
            sessions.append_assistant_message(address, outgoing.text.clone(), Vec::new())
        })?;
        Ok(())
    }

    async fn finish_non_reply_foreground_outcome_for_channel(
        &self,
        channel: &Arc<dyn Channel>,
        address: &ChannelAddress,
        active_session: &SessionSnapshot,
        outcome: ForegroundTurnOutcome,
        system_prompt: &TurnSystemPromptState,
        ephemeral_system_messages: &[ChatMessage],
    ) -> Result<bool> {
        match outcome {
            ForegroundTurnOutcome::Yielded(state) => {
                channel
                    .set_processing(address, ProcessingState::Idle)
                    .await
                    .ok();
                self.log_turn_usage(active_session, &state.usage, false);
                if state.errno.is_some() {
                    let error = anyhow!(
                        "{}",
                        state
                            .errinfo
                            .as_deref()
                            .unwrap_or("agent_frame yielded with an error")
                    );
                    let progress_summary =
                        summarize_resume_progress(&self.main_agent.language, &state.messages);
                    self.send_channel_message(
                        channel,
                        address,
                        OutgoingMessage::text(user_facing_continue_error_text(
                            &self.main_agent.language,
                            &error,
                            &progress_summary,
                        )),
                    )
                    .await?;
                }
                Ok(true)
            }
            ForegroundTurnOutcome::Failed {
                resume_messages,
                progress_summary,
                compaction,
                error,
            } => {
                let error_text = self.persist_failed_foreground_turn(
                    address,
                    resume_messages,
                    progress_summary,
                    &compaction,
                    &error,
                    system_prompt,
                    ephemeral_system_messages,
                )?;
                channel
                    .set_processing(address, ProcessingState::Idle)
                    .await
                    .ok();
                self.send_channel_message(channel, address, OutgoingMessage::text(error_text))
                    .await?;
                Ok(true)
            }
            ForegroundTurnOutcome::Replied { .. } => Ok(false),
        }
    }

    async fn finish_replied_foreground_outcome_for_channel(
        &self,
        channel: &Arc<dyn Channel>,
        active_session: &SessionSnapshot,
        address: &ChannelAddress,
        model_key: &str,
        usage: &TokenUsage,
        outgoing: OutgoingMessage,
        log_message: &'static str,
    ) -> Result<()> {
        let foreground = self.build_foreground_agent(active_session, model_key)?;
        self.log_turn_usage(active_session, usage, false);
        info!(
            log_stream = "agent",
            log_key = %foreground.id,
            kind = "foreground_agent_replied",
            session_id = %foreground.session_id,
            channel_id = %foreground.channel_id,
            system_prompt_len = foreground.system_prompt.len() as u64,
            has_text = outgoing.text.as_deref().is_some_and(|text| !text.trim().is_empty()),
            attachment_count = outgoing.attachments.len() as u64 + outgoing.images.len() as u64,
            reply_context = log_message,
            "foreground agent produced reply"
        );
        self.send_channel_message(channel, address, outgoing)
            .await?;
        channel
            .set_processing(address, ProcessingState::Idle)
            .await
            .ok();
        Ok(())
    }

    fn persist_yielded_foreground_turn(
        &self,
        address: &ChannelAddress,
        previous_message_count: usize,
        messages: Vec<ChatMessage>,
        consumed_pending_messages: &[ChatMessage],
        usage: &TokenUsage,
        compaction: &SessionCompactionStats,
        system_prompt: &TurnSystemPromptState,
        ephemeral_system_messages: &[ChatMessage],
    ) -> Result<PersistedYieldedForegroundTurn> {
        let persistence_system_prompt = system_prompt.for_persistence(compaction);
        let messages = normalize_messages_for_persistence(
            messages,
            persistence_system_prompt,
            ephemeral_system_messages,
        );
        let loaded_skills = extract_loaded_skill_names(&messages, previous_message_count);
        self.with_sessions(|sessions| {
            sessions.record_yielded_turn(
                address,
                messages,
                consumed_pending_messages,
                usage,
                compaction,
            )
        })?;
        self.rotate_chat_version_if_compacted(address, compaction)?;
        self.mark_system_prompt_state_after_compaction(address, system_prompt, compaction)?;
        self.with_sessions(|sessions| {
            sessions.mark_skills_loaded_current_turn(address, &loaded_skills)
        })?;
        let refreshed = self
            .with_sessions(|sessions| Ok(sessions.get_snapshot(address)))?
            .expect("session should exist after persisting yielded turn");
        let should_auto_resume = self.should_auto_resume_yielded_session(&refreshed);
        Ok(PersistedYieldedForegroundTurn {
            session: refreshed,
            should_auto_resume,
        })
    }

    async fn run_foreground_turn_until_settled(
        &self,
        active_session: &mut SessionSnapshot,
        model_key: &str,
        next_previous_messages: &mut Vec<ChatMessage>,
        system_prompt: &TurnSystemPromptState,
        ephemeral_system_messages: &mut Vec<ChatMessage>,
        error_context: &str,
    ) -> Result<ForegroundTurnOutcome> {
        loop {
            let consumed_pending_messages = active_session.session_state.pending_messages.clone();
            let outcome = self
                .run_main_agent_turn_with_previous_messages(
                    active_session,
                    model_key,
                    next_previous_messages.clone(),
                )
                .await
                .with_context(|| error_context.to_string());
            match outcome {
                Ok(ForegroundTurnOutcome::Yielded(state)) => {
                    if state.errno.is_some() {
                        let error = anyhow!(
                            "{}",
                            state
                                .errinfo
                                .clone()
                                .unwrap_or_else(|| "agent_frame yielded with an error".to_string())
                        );
                        let progress_summary =
                            summarize_resume_progress(&self.main_agent.language, &state.messages);
                        self.persist_failed_foreground_turn(
                            &active_session.address,
                            state.messages,
                            progress_summary,
                            &state.compaction,
                            &error,
                            system_prompt,
                            ephemeral_system_messages,
                        )
                        .context("failed to persist error yielded agent_frame state")?;
                        let refreshed = self
                            .with_sessions(|sessions| {
                                Ok(sessions.get_snapshot(&active_session.address))
                            })?
                            .expect("session should exist after persisting error yielded state");
                        *active_session = refreshed;
                        self.unregister_active_foreground_control(&active_session.address)?;
                        return Ok(ForegroundTurnOutcome::Yielded(SessionState {
                            messages: active_session.request_messages(),
                            pending_messages: active_session.session_state.pending_messages.clone(),
                            phase: SessionPhase::Yielded,
                            errno: state.errno,
                            errinfo: state.errinfo,
                            usage: state.usage,
                            compaction: state.compaction,
                        }));
                    }
                    let persisted = self
                        .persist_yielded_foreground_turn(
                            &active_session.address,
                            active_session.stable_message_count(),
                            state.messages,
                            &consumed_pending_messages,
                            &state.usage,
                            &state.compaction,
                            system_prompt,
                            ephemeral_system_messages,
                        )
                        .context("failed to persist yielded agent_frame messages")?;
                    *active_session = persisted.session;
                    if persisted.should_auto_resume {
                        *next_previous_messages = active_session.request_messages();
                        ephemeral_system_messages.clear();
                        continue;
                    }
                    self.unregister_active_foreground_control(&active_session.address)?;
                    return Ok(ForegroundTurnOutcome::Yielded(SessionState {
                        messages: active_session.request_messages(),
                        pending_messages: active_session.session_state.pending_messages.clone(),
                        phase: SessionPhase::Yielded,
                        errno: None,
                        errinfo: None,
                        usage: state.usage,
                        compaction: state.compaction,
                    }));
                }
                Ok(other) => return Ok(other),
                Err(error) => {
                    self.unregister_active_foreground_control(&active_session.address)?;
                    return Err(error);
                }
            }
        }
    }

    pub(super) async fn initialize_foreground_session(
        &self,
        session: &SessionSnapshot,
        show_reply: bool,
    ) -> Result<OutgoingMessage> {
        if self.main_agent.memory_system == agent_frame::config::MemorySystem::ClaudeCode {
            ensure_workspace_partclaw_file(&self.agent_workspace, &session.workspace_root)?;
        }
        let greeting = ChatMessage::text("user", greeting_for_language(&self.main_agent.language));
        let effective_model_key = self.effective_main_model_key(&session.address)?;
        let prompt_state = self.build_foreground_prompt_state(session, &effective_model_key)?;
        self.with_sessions(|sessions| {
            sessions.mark_system_prompt_state_current(
                &session.address,
                prompt_state.static_hash.clone(),
                prompt_state.dynamic_hashes.clone(),
            )
        })?;
        let turn_system_prompt = TurnSystemPromptState {
            active: prompt_state.system_prompt.clone(),
            full: prompt_state.system_prompt.clone(),
            static_hash: prompt_state.static_hash.clone(),
            component_hashes: prompt_state.dynamic_hashes.clone(),
        };
        let mut active_session = session.clone();
        let mut next_previous_messages = {
            let mut messages = active_session.request_messages();
            messages.push(greeting);
            messages
        };
        let mut ephemeral_system_messages = Vec::new();
        let outcome = self
            .run_foreground_turn_until_settled(
                &mut active_session,
                &effective_model_key,
                &mut next_previous_messages,
                &turn_system_prompt,
                &mut ephemeral_system_messages,
                "failed to initialize foreground session",
            )
            .await;
        if let Ok(mut interrupts) = self.pending_foreground_interrupts.lock() {
            interrupts.remove(&session.address.session_key());
        }
        let outcome = outcome?;
        let (state, outgoing) = match outcome {
            ForegroundTurnOutcome::Replied { state, outgoing } => (state, outgoing),
            ForegroundTurnOutcome::Yielded(state) => {
                self.log_turn_usage(&active_session, &state.usage, true);
                return Ok(OutgoingMessage::default());
            }
            ForegroundTurnOutcome::Failed { error, .. } => return Err(error),
        };
        self.persist_completed_foreground_turn(
            &session.address,
            active_session.stable_message_count(),
            state.messages,
            &[],
            &state.usage,
            &state.compaction,
            &turn_system_prompt,
            &[],
            show_reply.then_some(&outgoing),
        )?;
        self.log_turn_usage(&active_session, &state.usage, true);
        Ok(outgoing)
    }

    async fn run_main_agent_turn_with_previous_messages(
        &self,
        session: &SessionSnapshot,
        model_key: &str,
        previous_messages: Vec<ChatMessage>,
    ) -> Result<ForegroundTurnOutcome> {
        let workspace_root = session.workspace_root.clone();
        let upstream_timeout_seconds = session
            .api_timeout_override_seconds
            .unwrap_or(self.model_upstream_timeout_seconds(model_key)?);
        let runtime = self.agent_runtime_view_for_address(&session.address)?;
        let active_controls = Arc::clone(&self.active_foreground_controls);
        let pending_interrupts = Arc::clone(&self.pending_foreground_interrupts);
        let session_key = session.address.session_key();
        let control_observer: Arc<dyn Fn(SessionExecutionControl) + Send + Sync> =
            Arc::new(move |control| {
                register_active_foreground_control(
                    &active_controls,
                    &pending_interrupts,
                    &session_key,
                    control,
                );
            });
        let run_result = runtime
            .run_agent_turn_with_timeout(
                session.clone(),
                AgentPromptKind::MainForeground,
                session.agent_id,
                AgentBackendKind::AgentFrame,
                model_key.to_string(),
                previous_messages.clone(),
                String::new(),
                Some(upstream_timeout_seconds),
                Some(control_observer),
                "agent_frame task join failed",
            )
            .await;
        if run_result.is_err() {
            self.unregister_active_foreground_control(&session.address)?;
        }
        let run_result = run_result?;

        match run_result {
            TimedRunOutcome::Completed(state) => {
                self.unregister_active_foreground_control(&session.address)?;
                foreground_reply_from_completed_state(
                    session,
                    state,
                    &workspace_root,
                    &self.main_agent.language,
                )
            }
            TimedRunOutcome::Yielded(state) => Ok(ForegroundTurnOutcome::Yielded(state)),
            TimedRunOutcome::TimedOut { state, error } => {
                self.unregister_active_foreground_control(&session.address)?;
                let state = state.ok_or(error)?;
                foreground_reply_from_completed_state(
                    session,
                    state,
                    &workspace_root,
                    &self.main_agent.language,
                )
            }
            TimedRunOutcome::Failed(error) => {
                self.unregister_active_foreground_control(&session.address)?;
                let (resume_messages, compaction) =
                    (previous_messages.clone(), SessionCompactionStats::default());
                let progress_summary =
                    summarize_resume_progress(&self.main_agent.language, &resume_messages);
                Ok(ForegroundTurnOutcome::Failed {
                    resume_messages,
                    progress_summary,
                    compaction,
                    error,
                })
            }
        }
    }
}

fn foreground_reply_from_completed_state(
    session: &SessionSnapshot,
    state: SessionState,
    workspace_root: &Path,
    language: &str,
) -> Result<ForegroundTurnOutcome> {
    if terminal_assistant_message_is_empty(&state.messages) {
        let mut resume_messages = state.messages;
        resume_messages.pop();
        let progress_summary = summarize_resume_progress(language, &resume_messages);
        return Ok(ForegroundTurnOutcome::Failed {
            resume_messages,
            progress_summary,
            compaction: state.compaction,
            error: anyhow!("upstream returned an empty final assistant message"),
        });
    }

    let assistant_text = extract_assistant_text(&state.messages);
    let outgoing = build_outgoing_message_for_session(session, &assistant_text, workspace_root)?;
    Ok(ForegroundTurnOutcome::Replied { state, outgoing })
}

fn terminal_assistant_message_is_empty(messages: &[ChatMessage]) -> bool {
    let Some(message) = messages.last() else {
        return false;
    };
    message.role == "assistant" && !assistant_message_has_content_or_tool_calls(message)
}

fn assistant_message_has_content_or_tool_calls(message: &ChatMessage) -> bool {
    if message
        .tool_calls
        .as_ref()
        .is_some_and(|tool_calls| !tool_calls.is_empty())
    {
        return true;
    }
    match &message.content {
        None | Some(Value::Null) => false,
        Some(Value::String(text)) => !text.trim().is_empty(),
        Some(Value::Array(items)) => items.iter().any(|item| match item {
            Value::String(text) => !text.trim().is_empty(),
            Value::Object(object) => match object.get("type").and_then(Value::as_str) {
                Some("text" | "input_text" | "output_text") => object
                    .get("text")
                    .and_then(Value::as_str)
                    .is_some_and(|text| !text.trim().is_empty()),
                _ => true,
            },
            Value::Null => false,
            _ => true,
        }),
        Some(_) => true,
    }
}
