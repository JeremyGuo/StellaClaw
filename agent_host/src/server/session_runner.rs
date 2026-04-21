use super::*;

// Shared main-session turn policy.

#[derive(Clone, Debug)]
struct TurnSystemPromptState {
    active: String,
    full: String,
    static_hash: String,
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

enum MainSessionTurnOutcome {
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

fn prompt_kind_for_main_session(session: &SessionSnapshot) -> AgentPromptKind {
    match session.kind {
        SessionKind::Foreground => AgentPromptKind::MainForeground,
        SessionKind::Background => AgentPromptKind::MainBackground,
    }
}

// Foreground inbound orchestration.

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

        let session = server
            .ensure_foreground_actor(&incoming.address)?
            .snapshot()?;
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

        server.sync_runtime_profile_files(&session)?;
        let stored_attachments = incoming.stored_attachments.clone();
        let prompt_updates_prefix = server.observe_runtime_prompt_component_changes(&session)?;
        let skill_updates_prefix = server.observe_runtime_skill_changes(&session)?;
        let session_actor = server.ensure_foreground_actor(&incoming.address)?;
        let time_hints = session_actor.user_turn_time_hints(
            SessionTurnTimeHintConfig {
                emit_idle_time_gap_hint: server.main_agent.time_awareness.emit_idle_time_gap_hint,
                emit_system_date_on_user_message: server
                    .main_agent
                    .time_awareness
                    .emit_system_date_on_user_message,
            },
            Utc::now(),
        )?;
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
            time_hints.system_date.as_deref(),
        )?;
        let user_receipt = session_actor.tell_user_message(SessionUserMessage {
            pending_message: user_message.clone(),
            text: incoming.text.clone(),
            attachments: stored_attachments.clone(),
        })?;
        server
            .send_session_actor_outputs(channel, &incoming.address, user_receipt.outbound)
            .await;
        if let Some(entry) = user_receipt.transcript_entry
            && let Some(web_channel) = server.web_channels.get(&incoming.address.channel_id)
        {
            web_channel.publish_transcript_append(&incoming.address, entry);
        }
        let session = server
            .claim_foreground_turn_runner_when_ready(&incoming.address)
            .await?;
        let turn_result = async {
            if session.session_state.pending_messages.is_empty() {
                server.unregister_session_runtime_control(&incoming.address)?;
                return Ok(());
            }
            let prompt_state =
                server.build_foreground_prompt_state(&session, &effective_model_key)?;
            let prompt_observation =
                session_actor.observe_system_prompt_state(prompt_state.static_hash.clone())?;
            server.clear_process_restart_notice(&incoming.address);
            let synthetic_runtime_messages = build_synthetic_runtime_messages(
                prompt_updates_prefix.as_deref(),
                skill_updates_prefix.as_deref(),
            );
            let base_messages = session.request_messages();
            let (mut previous_messages, active_system_prompt, rebuilt_system_prompt) =
                prepare_system_prompt_for_turn(
                    &base_messages,
                    &prompt_state.system_prompt,
                    prompt_observation.static_changed,
                );
            previous_messages.extend(synthetic_runtime_messages.iter().cloned());
            if rebuilt_system_prompt {
                server.mark_conversation_context_changed(&incoming.address)?;
                session_actor.mark_system_prompt_state_current(prompt_state.static_hash.clone())?;
            }
            let turn_system_prompt = TurnSystemPromptState {
                active: active_system_prompt,
                full: prompt_state.system_prompt.clone(),
                static_hash: prompt_state.static_hash.clone(),
            };
            let mut active_session = session;
            let mut next_previous_messages = previous_messages;
            // Runtime notices are durable once sent; do not strip them at commit.
            let mut ephemeral_system_messages = Vec::new();

            channel
                .set_processing(&incoming.address, ProcessingState::Typing)
                .await
                .ok();
            let typing_guard = spawn_processing_keepalive(
                channel.clone(),
                incoming.address.clone(),
                ProcessingState::Typing,
            );

            let runtime_result = server
                .run_main_session_turn_until_settled(
                    &mut active_session,
                    &effective_model_key,
                    &mut next_previous_messages,
                    &turn_system_prompt,
                    &mut ephemeral_system_messages,
                    "foreground agent turn failed",
                )
                .await;
            session_actor.clear_pending_interrupt()?;
            if let Some(stop_sender) = typing_guard {
                let _ = stop_sender.send(());
            }
            if let Err(error) = &runtime_result {
                channel
                    .set_processing(&incoming.address, ProcessingState::Idle)
                    .await
                    .ok();
                server
                    .send_user_error_message(channel, &incoming.address, error)
                    .await;
            }
            match runtime_result? {
                MainSessionTurnOutcome::Replied { state, outgoing } => {
                    server
                        .finish_replied_foreground_turn_for_channel(
                            channel,
                            &active_session,
                            &incoming.address,
                            &effective_model_key,
                            state,
                            outgoing,
                            &turn_system_prompt,
                            &ephemeral_system_messages,
                            "user message reply",
                            "failed to persist agent_frame messages",
                        )
                        .await?;
                }
                outcome => {
                    if server
                        .finish_non_reply_foreground_outcome_for_channel(
                            channel,
                            &incoming.address,
                            &active_session,
                            outcome,
                            &turn_system_prompt,
                            &ephemeral_system_messages,
                        )
                        .await?
                    {
                        return Ok(());
                    }
                    unreachable!("replied branch should have matched above");
                }
            }
            Ok(())
        }
        .await;
        if turn_result.is_err() {
            let _ = server.unregister_session_runtime_control(&incoming.address);
        }
        turn_result
    }
}

impl Server {
    pub(super) async fn recover_pending_foreground_turns_after_startup(
        self: Arc<Self>,
    ) -> Result<()> {
        let sessions = self.with_sessions(|sessions| Ok(sessions.list_foreground_snapshots()))?;
        let pending_sessions = sessions
            .into_iter()
            .filter(|session| !session.session_state.pending_messages.is_empty())
            .collect::<Vec<_>>();
        if pending_sessions.is_empty() {
            return Ok(());
        }

        info!(
            log_stream = "server",
            kind = "pending_foreground_turn_recovery_started",
            session_count = pending_sessions.len() as u64,
            "recovering foreground turns that were pending at startup"
        );

        for session in pending_sessions {
            let Some(channel) = self.channels.get(&session.address.channel_id).cloned() else {
                warn!(
                    log_stream = "session",
                    log_key = %session.id,
                    kind = "pending_foreground_turn_recovery_skipped",
                    channel_id = %session.address.channel_id,
                    conversation_id = %session.address.conversation_id,
                    pending_message_count = session.session_state.pending_messages.len() as u64,
                    "cannot recover pending foreground turn because its channel is not configured"
                );
                continue;
            };
            let server = Arc::clone(&self);
            tokio::spawn(async move {
                if let Err(error) = server
                    .recover_pending_foreground_turn_for_channel(&channel, session)
                    .await
                {
                    error!(
                        log_stream = "session",
                        kind = "pending_foreground_turn_recovery_failed",
                        error = %format!("{error:#}"),
                        "failed to recover one pending foreground turn"
                    );
                }
            });
        }

        Ok(())
    }

    async fn recover_pending_foreground_turn_for_channel(
        &self,
        channel: &Arc<dyn Channel>,
        original_session: SessionSnapshot,
    ) -> Result<()> {
        let address = original_session.address.clone();
        if !self.has_complete_agent_selection(&address)? {
            self.send_channel_message(
                channel,
                &address,
                self.agent_selection_message(
                    &address,
                    "A pending message survived a service restart. Choose a model for this conversation to resume it.",
                )?,
            )
            .await?;
            return Ok(());
        }

        let mut active_session = self
            .claim_foreground_turn_runner_when_ready(&address)
            .await?;
        let recovery_result = async {
            if active_session.session_state.pending_messages.is_empty() {
                self.unregister_session_runtime_control(&address)?;
                return Ok(());
            }

            self.sync_runtime_profile_files(&active_session)?;
            let prompt_updates_prefix =
                self.observe_runtime_prompt_component_changes(&active_session)?;
            let skill_updates_prefix = self.observe_runtime_skill_changes(&active_session)?;
            let model_key = self.effective_main_model_key(&address)?;
            self.log_current_tools_for_user_message(
                &active_session,
                &model_key,
                "startup-recovery",
                "startup_pending_message_recovery",
            );

            let prompt_state = self.build_foreground_prompt_state(&active_session, &model_key)?;
            let actor = self.ensure_foreground_actor(&address)?;
            let prompt_observation =
                actor.observe_system_prompt_state(prompt_state.static_hash.clone())?;
            self.clear_process_restart_notice(&address);
            let synthetic_runtime_messages = build_synthetic_runtime_messages(
                prompt_updates_prefix.as_deref(),
                skill_updates_prefix.as_deref(),
            );
            let base_messages = active_session.request_messages();
            let (mut next_previous_messages, active_system_prompt, rebuilt_system_prompt) =
                prepare_system_prompt_for_turn(
                    &base_messages,
                    &prompt_state.system_prompt,
                    prompt_observation.static_changed,
            );
            next_previous_messages.extend(synthetic_runtime_messages.iter().cloned());
            if rebuilt_system_prompt {
                self.mark_conversation_context_changed(&address)?;
                actor.mark_system_prompt_state_current(prompt_state.static_hash.clone())?;
            }
            let turn_system_prompt = TurnSystemPromptState {
                active: active_system_prompt,
                full: prompt_state.system_prompt.clone(),
                static_hash: prompt_state.static_hash.clone(),
            };
            // Runtime notices are durable once sent; do not strip them at commit.
            let mut ephemeral_system_messages = Vec::new();

            channel
                .set_processing(&address, ProcessingState::Typing)
                .await
                .ok();
            let typing_guard =
                spawn_processing_keepalive(channel.clone(), address.clone(), ProcessingState::Typing);
            let outcome = self
                .run_main_session_turn_until_settled(
                    &mut active_session,
                    &model_key,
                    &mut next_previous_messages,
                    &turn_system_prompt,
                    &mut ephemeral_system_messages,
                    "failed to recover pending foreground turn after restart",
                )
                .await;
            let actor = self.ensure_foreground_actor(&address)?;
            actor.clear_pending_interrupt()?;
            if let Some(stop_sender) = typing_guard {
                let _ = stop_sender.send(());
            }
            if let Err(error) = &outcome {
                channel
                    .set_processing(&address, ProcessingState::Idle)
                    .await
                    .ok();
                self.send_user_error_message(channel, &address, error).await;
            }

            match outcome? {
                MainSessionTurnOutcome::Replied { state, outgoing } => {
                    self.finish_replied_foreground_turn_for_channel(
                        channel,
                        &active_session,
                        &address,
                        &model_key,
                        state,
                        outgoing,
                        &turn_system_prompt,
                        &ephemeral_system_messages,
                        "startup pending message recovery",
                        "failed to persist recovered pending foreground turn",
                    )
                    .await?;
                }
                outcome => {
                    let _ = self
                        .finish_non_reply_foreground_outcome_for_channel(
                            channel,
                            &address,
                            &active_session,
                            outcome,
                            &turn_system_prompt,
                            &ephemeral_system_messages,
                        )
                        .await?;
                }
            }

            info!(
                log_stream = "session",
                log_key = %original_session.id,
                kind = "pending_foreground_turn_recovered",
                channel_id = %address.channel_id,
                conversation_id = %address.conversation_id,
                pending_message_count = original_session.session_state.pending_messages.len() as u64,
                "recovered pending foreground turn after startup"
            );
            Ok(())
        }
        .await;

        if recovery_result.is_err() {
            let _ = self.unregister_session_runtime_control(&address);
        }
        recovery_result
    }

    pub(super) async fn prepare_regular_conversation_message(
        &self,
        mut incoming: IncomingMessage,
    ) -> Result<IncomingMessage> {
        if !incoming.attachments.is_empty() {
            let session = self
                .ensure_foreground_actor(&incoming.address)?
                .snapshot()?;
            let mut stored_attachments = materialize_conversation_attachments(
                &session.attachments_dir,
                std::mem::take(&mut incoming.attachments),
            )
            .await?;
            incoming.stored_attachments.append(&mut stored_attachments);
        }
        Ok(incoming)
    }

    async fn send_session_actor_outputs(
        &self,
        channel: &Arc<dyn Channel>,
        address: &ChannelAddress,
        outputs: Vec<SessionActorOutbound>,
    ) {
        for output in outputs {
            let message = match output {
                SessionActorOutbound::UserVisibleText(text) => OutgoingMessage::text(text),
            };
            if let Err(error) = channel.send(address, message).await {
                error!(
                    log_stream = "channel",
                    log_key = %address.channel_id,
                    kind = "session_actor_output_send_failed",
                    conversation_id = %address.conversation_id,
                    error = %format!("{error:#}"),
                    "failed to send session actor output"
                );
            }
        }
    }

    async fn claim_foreground_turn_runner_when_ready(
        &self,
        address: &ChannelAddress,
    ) -> Result<SessionSnapshot> {
        loop {
            let claimed = self.with_conversations_and_sessions(|conversations, sessions| {
                let actor = conversations.ensure_foreground_actor(address, sessions)?;
                if actor.try_claim_turn_runner()? {
                    actor.snapshot().map(Some)
                } else {
                    Ok(None)
                }
            })?;
            if let Some(session) = claimed {
                return Ok(session);
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    fn initialize_session_prompt_components_if_missing(
        &self,
        session: &SessionSnapshot,
    ) -> Result<()> {
        let actor = self.ensure_foreground_actor(&session.address)?;
        actor.initialize_prompt_component_if_missing(
            IDENTITY_PROMPT_COMPONENT,
            current_identity_prompt_for_workspace(&self.agent_workspace),
        )?;
        actor.initialize_prompt_component_if_missing(
            USER_META_PROMPT_COMPONENT,
            current_user_meta_prompt_for_workspace(&self.agent_workspace),
        )?;
        actor.initialize_prompt_component_if_missing(
            REMOTE_ALIASES_PROMPT_COMPONENT,
            current_ssh_remote_aliases_prompt(),
        )?;
        let discovered = discover_skills(std::slice::from_ref(&self.agent_workspace.skills_dir))?;
        actor.initialize_prompt_component_if_missing(
            crate::session::SKILLS_METADATA_PROMPT_COMPONENT,
            build_skills_meta_prompt(&discovered),
        )
    }

    fn clear_process_restart_notice(&self, address: &ChannelAddress) {
        let session_key = address.session_key();
        let Some(mut pending) = self.pending_process_restart_notices.lock().ok() else {
            return;
        };
        if pending.remove(&session_key) {
            info!(
                log_stream = "session",
                log_key = %session_key,
                kind = "process_restart_notice_cleared",
                "cleared one-shot process restart notice without injecting a model message"
            );
        }
    }

    pub(super) async fn handle_continue_command(
        &self,
        channel: &Arc<dyn Channel>,
        incoming: &IncomingMessage,
    ) -> Result<()> {
        let preflight_session = self
            .ensure_foreground_actor(&incoming.address)?
            .snapshot()?;
        if preflight_session.session_state.phase != SessionPhase::Yielded {
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
        let session = self
            .claim_foreground_turn_runner_when_ready(&incoming.address)
            .await?;
        let continue_result = async {
            if session.session_state.phase != SessionPhase::Yielded {
                self.unregister_session_runtime_control(&incoming.address)?;
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
            let actor = self.ensure_foreground_actor(&incoming.address)?;
            let prompt_observation =
                actor.observe_system_prompt_state(persistence_system_prompt.static_hash.clone())?;
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
                    self.mark_conversation_context_changed(&incoming.address)?;
                    let actor = self.ensure_foreground_actor(&incoming.address)?;
                    actor.mark_system_prompt_state_current(
                        persistence_system_prompt.static_hash.clone(),
                    )?;
                }
                (resume_messages, active_system_prompt)
            };
            let turn_system_prompt = TurnSystemPromptState {
                active: active_system_prompt,
                full: persistence_system_prompt.system_prompt.clone(),
                static_hash: persistence_system_prompt.static_hash.clone(),
            };
            self.clear_process_restart_notice(&incoming.address);
            // Runtime notices are durable once sent; do not strip them at commit.
            let mut ephemeral_system_messages = Vec::new();
            let outcome = self
                .run_main_session_turn_until_settled(
                    &mut active_session,
                    &continue_model_key,
                    &mut next_previous_messages,
                    &turn_system_prompt,
                    &mut ephemeral_system_messages,
                    "failed to continue interrupted foreground turn",
                )
                .await;
            let actor = self.ensure_foreground_actor(&incoming.address)?;
            actor.clear_pending_interrupt()?;
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
                MainSessionTurnOutcome::Replied { state, outgoing } => {
                    self.finish_replied_foreground_turn_for_channel(
                        channel,
                        &active_session,
                        &incoming.address,
                        &continue_model_key,
                        state,
                        outgoing,
                        &turn_system_prompt,
                        &ephemeral_system_messages,
                        "continued interrupted turn",
                        "failed to persist continued agent_frame messages",
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
                            &ephemeral_system_messages,
                        )
                        .await?
                    {
                        return Ok(());
                    }
                    unreachable!("replied branch should have matched above");
                }
            }
        }
        .await;
        if continue_result.is_err() {
            let _ = self.unregister_session_runtime_control(&incoming.address);
        }
        continue_result
    }

    pub(super) async fn handle_regular_foreground_message(
        &self,
        channel: &Arc<dyn Channel>,
        incoming: IncomingMessage,
    ) -> Result<()> {
        TurnCoordinator::new(self, channel, incoming).run().await
    }

    // Interactive foreground persistence and delivery.

    fn should_auto_resume_yielded_session(&self, session: &SessionSnapshot) -> bool {
        if session.session_state.phase != SessionPhase::Yielded
            || !session.session_state.pending_messages.is_empty()
            || session.session_state.errno.is_some()
        {
            return false;
        }
        // If a user message arrived while this turn was running, don't
        // auto-resume; let the worker process that interrupt as the next turn.
        let has_pending_interrupt = self
            .resolve_foreground_actor(&session.address)
            .ok()
            .flatten()
            .and_then(|actor| actor.has_pending_interrupt().ok())
            .unwrap_or(false);
        if has_pending_interrupt {
            return false;
        }
        true
    }

    fn persist_failed_interactive_turn(
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
        let actor = self.ensure_foreground_actor(address)?;
        actor.fail_runtime_turn(SessionRuntimeTurnFailure {
            resume_messages,
            errno: session_errno,
            errinfo: Some(format!("{error:#}")),
            compaction: compaction.clone(),
            system_prompt_static_hash_after_compaction: Some(system_prompt.static_hash.clone()),
        })?;
        self.rotate_chat_version_if_compacted(address, compaction)?;
        Ok(user_facing_continue_error_text(
            &self.main_agent.language,
            error,
            &progress_summary,
        ))
    }

    fn persist_completed_interactive_turn(
        &self,
        session: &SessionSnapshot,
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
        let loaded_skills = extract_loaded_skill_names(&messages, session.stable_message_count());
        let actor = self.with_sessions(|sessions| sessions.resolve_snapshot(session))?;
        actor.commit_runtime_turn(SessionRuntimeTurnCommit {
            messages,
            consumed_pending_messages: consumed_pending_messages.to_vec(),
            usage: usage.clone(),
            compaction: compaction.clone(),
            phase: SessionPhase::End,
            system_prompt_static_hash_after_compaction: Some(system_prompt.static_hash.clone()),
            loaded_skills,
            user_history_text: None,
            assistant_history_text: append_assistant_history
                .and_then(|outgoing| outgoing.text.clone()),
        })?;
        self.rotate_chat_version_if_compacted(&session.address, compaction)?;
        Ok(())
    }

    async fn finish_non_reply_foreground_outcome_for_channel(
        &self,
        channel: &Arc<dyn Channel>,
        address: &ChannelAddress,
        active_session: &SessionSnapshot,
        outcome: MainSessionTurnOutcome,
        system_prompt: &TurnSystemPromptState,
        ephemeral_system_messages: &[ChatMessage],
    ) -> Result<bool> {
        match outcome {
            MainSessionTurnOutcome::Yielded(state) => {
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
            MainSessionTurnOutcome::Failed {
                resume_messages,
                progress_summary,
                compaction,
                error,
            } => {
                let error_text = self.persist_failed_interactive_turn(
                    address,
                    resume_messages,
                    progress_summary,
                    &compaction,
                    &error,
                    system_prompt,
                    ephemeral_system_messages,
                )?;
                self.unregister_session_runtime_control(address)?;
                channel
                    .set_processing(address, ProcessingState::Idle)
                    .await
                    .ok();
                self.send_channel_message(channel, address, OutgoingMessage::text(error_text))
                    .await?;
                Ok(true)
            }
            MainSessionTurnOutcome::Replied { .. } => Ok(false),
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

    async fn finish_replied_foreground_turn_for_channel(
        &self,
        channel: &Arc<dyn Channel>,
        active_session: &SessionSnapshot,
        address: &ChannelAddress,
        model_key: &str,
        state: SessionState,
        outgoing: OutgoingMessage,
        system_prompt: &TurnSystemPromptState,
        ephemeral_system_messages: &[ChatMessage],
        log_message: &'static str,
        persist_context: &'static str,
    ) -> Result<()> {
        self.persist_completed_interactive_turn(
            active_session,
            state.messages,
            &active_session.session_state.pending_messages,
            &state.usage,
            &state.compaction,
            system_prompt,
            ephemeral_system_messages,
            Some(&outgoing),
        )
        .context(persist_context)?;
        self.unregister_session_runtime_control(address)?;
        self.finish_replied_foreground_outcome_for_channel(
            channel,
            active_session,
            address,
            model_key,
            &state.usage,
            outgoing,
            log_message,
        )
        .await
    }

    fn persist_yielded_interactive_turn(
        &self,
        session: &SessionSnapshot,
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
        let loaded_skills = extract_loaded_skill_names(&messages, session.stable_message_count());
        let actor = self.with_sessions(|sessions| sessions.resolve_snapshot(session))?;
        actor.commit_runtime_turn(SessionRuntimeTurnCommit {
            messages,
            consumed_pending_messages: consumed_pending_messages.to_vec(),
            usage: usage.clone(),
            compaction: compaction.clone(),
            phase: SessionPhase::Yielded,
            system_prompt_static_hash_after_compaction: Some(system_prompt.static_hash.clone()),
            loaded_skills,
            user_history_text: None,
            assistant_history_text: None,
        })?;
        self.rotate_chat_version_if_compacted(&session.address, compaction)?;
        let refreshed = self
            .with_sessions(|sessions| Ok(sessions.get_snapshot(&session.address)))?
            .expect("session should exist after persisting yielded turn");
        let should_auto_resume = self.should_auto_resume_yielded_session(&refreshed);
        Ok(PersistedYieldedForegroundTurn {
            session: refreshed,
            should_auto_resume,
        })
    }

    async fn run_main_session_turn_until_settled(
        &self,
        active_session: &mut SessionSnapshot,
        model_key: &str,
        next_previous_messages: &mut Vec<ChatMessage>,
        system_prompt: &TurnSystemPromptState,
        ephemeral_system_messages: &mut Vec<ChatMessage>,
        error_context: &str,
    ) -> Result<MainSessionTurnOutcome> {
        loop {
            let consumed_pending_messages = active_session.session_state.pending_messages.clone();
            let outcome = self
                .run_main_session_turn_with_messages(
                    active_session,
                    model_key,
                    next_previous_messages.clone(),
                )
                .await
                .with_context(|| error_context.to_string());
            match outcome {
                Ok(MainSessionTurnOutcome::Yielded(state)) => {
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
                        self.persist_failed_interactive_turn(
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
                        self.unregister_session_runtime_control(&active_session.address)?;
                        return Ok(MainSessionTurnOutcome::Yielded(SessionState {
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
                        .persist_yielded_interactive_turn(
                            active_session,
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
                    self.unregister_session_runtime_control(&active_session.address)?;
                    return Ok(MainSessionTurnOutcome::Yielded(SessionState {
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
                    self.unregister_session_runtime_control(&active_session.address)?;
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
        let mut active_session = self
            .claim_foreground_turn_runner_when_ready(&session.address)
            .await?;
        let init_result = async {
            if active_session.stable_message_count() > 0 {
                self.unregister_session_runtime_control(&session.address)?;
                return Ok(OutgoingMessage::default());
            }
            if self.main_agent.memory_system == agent_frame::config::MemorySystem::ClaudeCode {
                ensure_workspace_partclaw_file(
                    &self.agent_workspace,
                    &active_session.workspace_root,
                )?;
            }
            self.initialize_session_prompt_components_if_missing(&active_session)?;
            let greeting =
                ChatMessage::text("user", greeting_for_language(&self.main_agent.language));
            let effective_model_key = self.effective_main_model_key(&session.address)?;
            let prompt_state =
                self.build_foreground_prompt_state(&active_session, &effective_model_key)?;
            let actor = self.ensure_foreground_actor(&session.address)?;
            actor.mark_system_prompt_state_current(prompt_state.static_hash.clone())?;
            let turn_system_prompt = TurnSystemPromptState {
                active: prompt_state.system_prompt.clone(),
                full: prompt_state.system_prompt.clone(),
                static_hash: prompt_state.static_hash.clone(),
            };
            let mut next_previous_messages = {
                let mut messages = active_session.request_messages();
                messages.push(greeting);
                messages
            };
            let mut ephemeral_system_messages = Vec::new();
            let outcome = self
                .run_main_session_turn_until_settled(
                    &mut active_session,
                    &effective_model_key,
                    &mut next_previous_messages,
                    &turn_system_prompt,
                    &mut ephemeral_system_messages,
                    "failed to initialize foreground session",
                )
                .await;
            let actor = self.ensure_foreground_actor(&session.address)?;
            actor.clear_pending_interrupt()?;
            let outcome = outcome?;
            let (state, outgoing) = match outcome {
                MainSessionTurnOutcome::Replied { state, outgoing } => (state, outgoing),
                MainSessionTurnOutcome::Yielded(state) => {
                    self.log_turn_usage(&active_session, &state.usage, true);
                    return Ok(OutgoingMessage::default());
                }
                MainSessionTurnOutcome::Failed { error, .. } => {
                    self.unregister_session_runtime_control(&active_session.address)?;
                    return Err(error);
                }
            };
            self.persist_completed_interactive_turn(
                &active_session,
                state.messages,
                &[],
                &state.usage,
                &state.compaction,
                &turn_system_prompt,
                &[],
                show_reply.then_some(&outgoing),
            )?;
            self.unregister_session_runtime_control(&active_session.address)?;
            self.log_turn_usage(&active_session, &state.usage, true);
            Ok(outgoing)
        }
        .await;
        if init_result.is_err() {
            let _ = self.unregister_session_runtime_control(&session.address);
        }
        init_result
    }

    async fn run_main_session_turn_with_messages(
        &self,
        session: &SessionSnapshot,
        model_key: &str,
        previous_messages: Vec<ChatMessage>,
    ) -> Result<MainSessionTurnOutcome> {
        let workspace_root = session.workspace_root.clone();
        let upstream_timeout_seconds = session
            .api_timeout_override_seconds
            .unwrap_or(self.model_upstream_timeout_seconds(model_key)?);
        let runtime = self.agent_runtime_view_for_address(&session.address)?;
        let run_result = runtime
            .run_main_session_turn(
                session.clone(),
                session.agent_id,
                AgentBackendKind::AgentFrame,
                model_key.to_string(),
                previous_messages.clone(),
                String::new(),
                Some(upstream_timeout_seconds),
                "agent_frame task join failed",
            )
            .await;
        if run_result.is_err() {
            self.unregister_session_runtime_control(&session.address)?;
        }
        let run_result = run_result?;

        match run_result {
            TimedRunOutcome::Completed(state) => main_reply_from_completed_state(
                session,
                state,
                &workspace_root,
                &self.main_agent.language,
            ),
            TimedRunOutcome::Yielded(state) => Ok(MainSessionTurnOutcome::Yielded(state)),
            TimedRunOutcome::TimedOut { state, error } => {
                let Some(state) = state else {
                    self.unregister_session_runtime_control(&session.address)?;
                    return Err(error);
                };
                main_reply_from_completed_state(
                    session,
                    state,
                    &workspace_root,
                    &self.main_agent.language,
                )
            }
            TimedRunOutcome::Failed(error) => {
                let (resume_messages, compaction) =
                    (previous_messages.clone(), SessionCompactionStats::default());
                let progress_summary =
                    summarize_resume_progress(&self.main_agent.language, &resume_messages);
                Ok(MainSessionTurnOutcome::Failed {
                    resume_messages,
                    progress_summary,
                    compaction,
                    error,
                })
            }
        }
    }
}

// Shared actor turn execution.

impl AgentRuntimeView {
    async fn run_main_session_turn(
        &self,
        session: SessionSnapshot,
        agent_id: uuid::Uuid,
        agent_backend: AgentBackendKind,
        model_key: String,
        previous_messages: Vec<ChatMessage>,
        prompt: String,
        upstream_timeout_seconds: Option<f64>,
        join_label: &str,
    ) -> Result<TimedRunOutcome> {
        let prompt_kind = prompt_kind_for_main_session(&session);
        let previous_messages = sanitize_messages_for_model_capabilities(
            &previous_messages,
            self.model_config(&model_key)?,
            backend_supports_native_multimodal_input(agent_backend),
        );
        let control_observer = {
            let session_registry = Arc::clone(&self.sessions);
            let control_session = session.clone();
            Some(Arc::new(move |control| {
                if let Ok(sessions) = session_registry.lock()
                    && let Ok(actor) = sessions.resolve_snapshot(&control_session)
                {
                    let _ = actor.register_control(control);
                }
            })
                as Arc<dyn Fn(SessionExecutionControl) + Send + Sync>)
        };
        self.run_agent_turn_with_timeout(
            session,
            prompt_kind,
            agent_id,
            agent_backend,
            model_key,
            previous_messages,
            prompt,
            upstream_timeout_seconds,
            control_observer,
            join_label,
        )
        .await
    }
}

// Shared assistant reply shaping.

fn main_reply_from_completed_state(
    session: &SessionSnapshot,
    state: SessionState,
    workspace_root: &Path,
    language: &str,
) -> Result<MainSessionTurnOutcome> {
    if terminal_assistant_message_is_empty(&state.messages) {
        let mut resume_messages = state.messages;
        resume_messages.pop();
        let progress_summary = summarize_resume_progress(language, &resume_messages);
        return Ok(MainSessionTurnOutcome::Failed {
            resume_messages,
            progress_summary,
            compaction: state.compaction,
            error: anyhow!("upstream returned an empty final assistant message"),
        });
    }

    let assistant_text = extract_assistant_text(&state.messages);
    let outgoing = build_outgoing_message_for_session(session, &assistant_text, workspace_root)?;
    Ok(MainSessionTurnOutcome::Replied { state, outgoing })
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn active_prompt_persists_until_compaction() {
        let state = TurnSystemPromptState {
            active: "static prompt plus dynamic change notifications".to_string(),
            full: "static prompt plus latest dynamic components".to_string(),
            static_hash: "static-hash".to_string(),
        };

        assert_eq!(
            state.for_persistence(&SessionCompactionStats::default()),
            "static prompt plus dynamic change notifications"
        );

        let compacted = SessionCompactionStats {
            compacted_run_count: 1,
            ..SessionCompactionStats::default()
        };
        assert_eq!(
            state.for_persistence(&compacted),
            "static prompt plus latest dynamic components"
        );
    }
}

impl AgentRuntimeView {
    fn unregister_session_actor_runtime_control(&self, session: &SessionSnapshot) -> Result<()> {
        let actor = self.with_sessions(|sessions| sessions.resolve_snapshot(session))?;
        actor.unregister_control().map(|_| ())?;
        Ok(())
    }

    // Background session lifecycle.

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
        self.initialize_session_prompt_components_if_missing(&background_session)?;
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
        self.run_main_session_turn(
            session.clone(),
            agent_id,
            agent_backend,
            model_key.to_string(),
            session.request_messages(),
            prompt,
            Some(upstream_timeout_seconds),
            join_label,
        )
        .await
    }

    fn normalize_background_messages_for_persistence(
        &self,
        session: &SessionSnapshot,
        messages: &[ChatMessage],
        model_key: &str,
    ) -> Result<Vec<ChatMessage>> {
        let config = self.build_agent_frame_config(
            session,
            &session.workspace_root,
            AgentPromptKind::MainBackground,
            model_key,
            None,
        )?;
        Ok(normalize_messages_for_persistence(
            messages.to_vec(),
            &config.system_prompt,
            &[],
        ))
    }

    fn persist_background_report(
        &self,
        session: &SessionSnapshot,
        report: &SessionState,
        model_key: &str,
    ) -> Result<()> {
        let actor = self.with_sessions(|sessions| sessions.resolve_snapshot(session))?;
        let messages = self.normalize_background_messages_for_persistence(
            session,
            &report.messages,
            model_key,
        )?;
        actor.update_checkpoint(messages, &report.usage, &report.compaction)
    }

    fn persist_background_visible_turn(
        &self,
        session: &SessionSnapshot,
        report: &SessionState,
        model_key: &str,
        prompt_for_history: String,
        phase: SessionPhase,
    ) -> Result<OutgoingMessage> {
        let actor = self.with_sessions(|sessions| sessions.resolve_snapshot(session))?;
        let assistant_text = extract_assistant_text(&report.messages);
        let outgoing =
            build_outgoing_message_for_session(session, &assistant_text, &session.workspace_root)?;
        let messages = self.normalize_background_messages_for_persistence(
            session,
            &report.messages,
            model_key,
        )?;
        actor.commit_runtime_turn(SessionRuntimeTurnCommit {
            messages,
            consumed_pending_messages: Vec::new(),
            usage: report.usage.clone(),
            compaction: report.compaction.clone(),
            phase,
            system_prompt_static_hash_after_compaction: None,
            loaded_skills: Vec::new(),
            user_history_text: Some(prompt_for_history),
            assistant_history_text: outgoing.text.clone(),
        })?;
        Ok(outgoing)
    }

    fn persist_background_yielded_turn(
        &self,
        session: &SessionSnapshot,
        report: &SessionState,
        model_key: &str,
    ) -> Result<()> {
        let actor = self.with_sessions(|sessions| sessions.resolve_snapshot(session))?;
        let messages = self.normalize_background_messages_for_persistence(
            session,
            &report.messages,
            model_key,
        )?;
        actor.commit_runtime_turn(SessionRuntimeTurnCommit {
            messages,
            consumed_pending_messages: Vec::new(),
            usage: report.usage.clone(),
            compaction: report.compaction.clone(),
            phase: SessionPhase::Yielded,
            system_prompt_static_hash_after_compaction: None,
            loaded_skills: Vec::new(),
            user_history_text: None,
            assistant_history_text: None,
        })
    }

    fn persist_background_report_after_silent_terminate(
        &self,
        session: &SessionSnapshot,
        model_key: &str,
        run_result: &Result<TimedRunOutcome>,
        accumulated_usage: &mut TokenUsage,
    ) -> Result<()> {
        match run_result {
            Ok(TimedRunOutcome::Completed(report)) | Ok(TimedRunOutcome::Yielded(report)) => {
                self.persist_background_report(session, report, model_key)?;
                accumulated_usage.add_assign(&report.usage);
            }
            Ok(TimedRunOutcome::TimedOut { state, .. }) => {
                if let Some(state) = state {
                    self.persist_background_report(session, state, model_key)?;
                    accumulated_usage.add_assign(&state.usage);
                }
            }
            Ok(TimedRunOutcome::Failed(_)) | Err(_) => {}
        }
        Ok(())
    }

    async fn publish_completed_background_report(
        &self,
        job: &BackgroundJobRequest,
        session: &SessionSnapshot,
        report: &SessionState,
    ) -> Result<()> {
        let outgoing = self.persist_background_visible_turn(
            session,
            report,
            &job.model_key,
            job.prompt.clone(),
            SessionPhase::End,
        )?;
        self.unregister_session_actor_runtime_control(session)?;
        log_turn_usage(
            job.agent_id,
            session,
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
        self.deliver_background_outgoing_to_foreground(session, &job.model_key, outgoing)
            .await
            .context("failed to deliver background agent reply")
    }

    fn background_yield_error(report: &SessionState) -> anyhow::Error {
        anyhow!(
            "{}",
            report
                .errinfo
                .clone()
                .unwrap_or_else(|| "agent_frame yielded with an error".to_string())
        )
    }

    fn log_background_auto_resuming_yield(
        &self,
        job: &BackgroundJobRequest,
        session: &SessionSnapshot,
        report: &SessionState,
    ) {
        log_turn_usage(
            job.agent_id,
            session,
            &report.usage,
            false,
            "main_background",
            job.parent_agent_id,
        );
        info!(
            log_stream = "agent",
            log_key = %job.agent_id,
            kind = "background_agent_auto_resuming_yield",
            parent_agent_id = job.parent_agent_id.map(|value| value.to_string()),
            cron_task_id = job.cron_task_id.map(|value| value.to_string()),
            session_id = %session.id,
            channel_id = %session.address.channel_id,
            "background agent yielded after tool work; resuming to final reply"
        );
    }

    // Background-to-foreground delivery.

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

    fn build_foreground_prompt_state_for_runtime(
        &self,
        session: &SessionSnapshot,
        model_key: &str,
    ) -> Result<AgentSystemPromptState> {
        let model = self.model_config(model_key)?;
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
        let local_mounts = self.with_conversations(|conversations| {
            Ok(conversations
                .get_snapshot(&session.address)
                .map(|snapshot| snapshot.settings.local_mounts)
                .unwrap_or_default())
        })?;
        Ok(build_agent_system_prompt_state(
            &self.agent_workspace,
            session,
            &workspace_summary,
            &remote_workpaths,
            &local_mounts,
            AgentPromptKind::MainForeground,
            model_key,
            model,
            &self.models,
            &self.available_agent_models(AgentBackendKind::AgentFrame),
            &self.main_agent,
        ))
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
        let actor = self.ensure_foreground_actor(&session.address)?;
        actor.record_idle_compaction(normalized_messages, &compaction_stats)?;
        let prompt_state =
            runtime.build_foreground_prompt_state_for_runtime(session, &model_key)?;
        let actor = self.ensure_foreground_actor(&session.address)?;
        actor.mark_system_prompt_state_current(prompt_state.static_hash)?;
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
        let foreground_actor =
            self.with_conversations_and_sessions(|conversations, sessions| {
                conversations.ensure_foreground_actor(&session.address, sessions)
            })?;
        let receipt = foreground_actor.tell_actor_message(SessionActorMessage {
            from_session_id: session.id,
            role: MessageRole::Assistant,
            text: outgoing.text.clone(),
            attachments: Vec::new(),
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
        if receipt.applied_to_context {
            let foreground_session = foreground_actor.snapshot()?;
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
        }
        Ok(())
    }

    async fn deliver_background_recovery_failure_notice(
        &self,
        recovery_agent_id: uuid::Uuid,
        failed_agent_id: uuid::Uuid,
        session: &SessionSnapshot,
        model_key: &str,
        original_error: &anyhow::Error,
        recovery_error: &anyhow::Error,
        log_message: &'static str,
    ) -> Result<()> {
        let text = user_facing_error_text(&self.main_agent.language, original_error);
        self.deliver_background_outgoing_to_foreground(
            session,
            model_key,
            OutgoingMessage::text(text),
        )
        .await
        .context("failed to deliver background failure notification")?;
        warn!(
            log_stream = "agent",
            log_key = %recovery_agent_id,
            kind = "background_agent_recovery_failed",
            failed_agent_id = %failed_agent_id,
            error = %format!("{recovery_error:#}"),
            "{log_message}"
        );
        Ok(())
    }

    // Background job state machine.

    pub(super) async fn run_background_job(&self, job: BackgroundJobRequest) -> Result<()> {
        let mut session = self.background_session_snapshot(job.session.id)?;
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
        let mut next_prompt = job.prompt.clone();
        let mut accumulated_usage = TokenUsage::default();
        let outcome = loop {
            let run_result = self
                .run_background_agent_turn(
                    &session,
                    job.agent_id,
                    job.agent_backend,
                    &job.model_key,
                    std::mem::take(&mut next_prompt),
                    "background agent task join failed",
                )
                .await;

            if self.take_background_terminate_requested(job.agent_id) {
                self.persist_background_report_after_silent_terminate(
                    &session,
                    &job.model_key,
                    &run_result,
                    &mut accumulated_usage,
                )?;
                self.unregister_session_actor_runtime_control(&session)?;
                self.mark_managed_agent_completed(job.agent_id, &accumulated_usage);
                self.record_cron_trigger_result(job.cron_task_id, "terminated_silently");
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
                break Ok(());
            }

            match run_result {
                Ok(TimedRunOutcome::Completed(report)) => {
                    accumulated_usage.add_assign(&report.usage);
                    if let Err(error) = self
                        .publish_completed_background_report(&job, &session, &report)
                        .await
                    {
                        self.mark_managed_agent_failed(job.agent_id, &accumulated_usage, &error);
                        self.record_cron_trigger_result(job.cron_task_id, "failed");
                        break self
                            .handle_background_job_failure(&job, &session, &error)
                            .await
                            .with_context(|| format!("{error:#}"));
                    }
                    self.mark_managed_agent_completed(job.agent_id, &accumulated_usage);
                    self.record_cron_trigger_result(job.cron_task_id, "completed");
                    break Ok(());
                }
                Ok(TimedRunOutcome::Yielded(report)) => {
                    accumulated_usage.add_assign(&report.usage);
                    if report.errno.is_some() {
                        self.persist_background_report(&session, &report, &job.model_key)?;
                        self.unregister_session_actor_runtime_control(&session)?;
                        let error = Self::background_yield_error(&report);
                        self.mark_managed_agent_failed(job.agent_id, &accumulated_usage, &error);
                        self.record_cron_trigger_result(job.cron_task_id, "failed");
                        break self
                            .handle_background_job_failure(&job, &session, &error)
                            .await;
                    }
                    self.persist_background_yielded_turn(&session, &report, &job.model_key)?;
                    self.unregister_session_actor_runtime_control(&session)?;
                    self.log_background_auto_resuming_yield(&job, &session, &report);
                    session = self.background_session_snapshot(session.id)?;
                }
                Ok(TimedRunOutcome::TimedOut { state, error }) => {
                    if let Some(state) = &state {
                        self.persist_background_report(&session, state, &job.model_key)?;
                        accumulated_usage.add_assign(&state.usage);
                    }
                    self.unregister_session_actor_runtime_control(&session)?;
                    self.mark_managed_agent_timed_out(job.agent_id, &accumulated_usage, &error);
                    self.record_cron_trigger_result(job.cron_task_id, "timed_out");
                    break self
                        .handle_background_job_failure(&job, &session, &error)
                        .await;
                }
                Ok(TimedRunOutcome::Failed(error)) => {
                    self.unregister_session_actor_runtime_control(&session)?;
                    self.mark_managed_agent_failed(job.agent_id, &accumulated_usage, &error);
                    self.record_cron_trigger_result(job.cron_task_id, "failed");
                    break self
                        .handle_background_job_failure(&job, &session, &error)
                        .await;
                }
                Err(error) => {
                    let _ = self.unregister_session_actor_runtime_control(&session);
                    self.mark_managed_agent_failed(job.agent_id, &accumulated_usage, &error);
                    self.record_cron_trigger_result(job.cron_task_id, "failed");
                    break self
                        .handle_background_job_failure(&job, &session, &error)
                        .await;
                }
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
                let outgoing = self.persist_background_visible_turn(
                    &session,
                    &report,
                    &job.model_key,
                    recovery_prompt_for_history.clone(),
                    SessionPhase::End,
                )?;
                self.unregister_session_actor_runtime_control(&session)?;
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
                let outgoing = self.persist_background_visible_turn(
                    &session,
                    &report,
                    &job.model_key,
                    recovery_prompt_for_history.clone(),
                    SessionPhase::Yielded,
                )?;
                self.unregister_session_actor_runtime_control(&session)?;
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
                    self.persist_background_report(&session, state, &job.model_key)?;
                }
                self.unregister_session_actor_runtime_control(&session)?;
                let usage = state
                    .as_ref()
                    .map(|state| state.usage.clone())
                    .unwrap_or_default();
                self.mark_managed_agent_timed_out(recovery_agent_id, &usage, &recovery_error);
                self.deliver_background_recovery_failure_notice(
                    recovery_agent_id,
                    job.agent_id,
                    &session,
                    &job.model_key,
                    error,
                    &recovery_error,
                    "background failure recovery agent timed out; user was notified",
                )
                .await?;
                Ok(())
            }
            Ok(TimedRunOutcome::Failed(recovery_error)) => {
                self.mark_managed_agent_failed(
                    recovery_agent_id,
                    &TokenUsage::default(),
                    &recovery_error,
                );
                self.deliver_background_recovery_failure_notice(
                    recovery_agent_id,
                    job.agent_id,
                    &session,
                    &job.model_key,
                    error,
                    &recovery_error,
                    "background failure recovery agent failed; user was notified",
                )
                .await?;
                Ok(())
            }
            Err(recovery_error) => {
                self.mark_managed_agent_failed(
                    recovery_agent_id,
                    &TokenUsage::default(),
                    &recovery_error,
                );
                self.deliver_background_recovery_failure_notice(
                    recovery_agent_id,
                    job.agent_id,
                    &session,
                    &job.model_key,
                    error,
                    &recovery_error,
                    "background failure recovery agent failed; user was notified",
                )
                .await?;
                Ok(())
            }
        }
    }
}
