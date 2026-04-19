use super::*;

impl Server {
    pub(super) async fn try_handle_incoming_command(
        &self,
        channel: &Arc<dyn Channel>,
        incoming: &IncomingMessage,
    ) -> Result<bool> {
        if incoming
            .text
            .as_deref()
            .map(str::trim)
            .is_some_and(|text| command_matches(text, "/help"))
        {
            let help_text = self.help_text_for_channel(&incoming.address.channel_id);
            info!(
                log_stream = "server",
                kind = "help_requested",
                channel_id = %incoming.address.channel_id,
                conversation_id = %incoming.address.conversation_id,
                "rendering help text"
            );
            self.send_channel_message(channel, &incoming.address, OutgoingMessage::text(help_text))
                .await?;
            return Ok(true);
        }

        if parse_agent_command(incoming.text.as_deref()).is_none()
            && let Some(missing_model_key) =
                self.clear_missing_selected_main_model(&incoming.address)?
        {
            self.prompt_missing_conversation_model(channel, &incoming.address, &missing_model_key)
                .await?;
            return Ok(true);
        }

        if self.try_handle_status_command(channel, incoming).await? {
            return Ok(true);
        }
        if self.try_handle_compact_command(channel, incoming).await? {
            return Ok(true);
        }
        if self
            .try_handle_compact_mode_command(channel, incoming)
            .await?
        {
            return Ok(true);
        }
        if self.try_handle_agent_command(channel, incoming).await? {
            return Ok(true);
        }
        if self.try_handle_sandbox_command(channel, incoming).await? {
            return Ok(true);
        }
        if self.try_handle_think_command(channel, incoming).await? {
            return Ok(true);
        }
        if self.try_handle_snapshot_command(channel, incoming).await? {
            return Ok(true);
        }
        if self
            .try_handle_api_timeout_command(channel, incoming)
            .await?
        {
            return Ok(true);
        }
        if parse_continue_command(incoming.text.as_deref()) {
            self.handle_continue_command(channel, incoming).await?;
            return Ok(true);
        }
        if is_command_like_text(incoming.text.as_deref()) {
            self.send_channel_message(
                channel,
                &incoming.address,
                OutgoingMessage::text(
                    "Unknown command. Use `/help` to see available commands.".to_string(),
                ),
            )
            .await?;
            return Ok(true);
        }

        Ok(false)
    }

    async fn try_handle_status_command(
        &self,
        channel: &Arc<dyn Channel>,
        incoming: &IncomingMessage,
    ) -> Result<bool> {
        if !incoming
            .text
            .as_deref()
            .map(str::trim)
            .is_some_and(|text| command_matches(text, "/status"))
        {
            return Ok(false);
        }

        let Ok(effective_model_key) = self.effective_main_model_key(&incoming.address) else {
            self.send_channel_message(
                channel,
                &incoming.address,
                self.agent_selection_message(
                    &incoming.address,
                    "Choose a model for this conversation before using `/status`.",
                )?,
            )
            .await?;
            return Ok(true);
        };
        let session = self
            .ensure_foreground_actor(&incoming.address)?
            .snapshot()?;
        let status_message = self
            .status_message_for_session(&session, &effective_model_key)
            .await?;
        self.send_channel_message(channel, &incoming.address, status_message)
            .await?;
        Ok(true)
    }

    async fn try_handle_compact_command(
        &self,
        channel: &Arc<dyn Channel>,
        incoming: &IncomingMessage,
    ) -> Result<bool> {
        if !incoming
            .text
            .as_deref()
            .map(str::trim)
            .is_some_and(|text| command_matches(text, "/compact"))
        {
            return Ok(false);
        }

        let Ok(effective_model_key) = self.effective_main_model_key(&incoming.address) else {
            self.send_channel_message(
                channel,
                &incoming.address,
                self.agent_selection_message(
                    &incoming.address,
                    "Choose a model for this conversation before using `/compact`.",
                )?,
            )
            .await?;
            return Ok(true);
        };
        let session = self
            .ensure_foreground_actor(&incoming.address)?
            .snapshot()?;
        self.send_channel_message(
            channel,
            &incoming.address,
            OutgoingMessage::text("正在压缩当前上下文，请稍候。".to_string()),
        )
        .await?;
        let compacted = self
            .compact_session_now(&session, &effective_model_key, true)
            .await?;
        let message = if compacted {
            "Compacted the current conversation context.".to_string()
        } else {
            "The current conversation context did not need compaction.".to_string()
        };
        self.send_channel_message(channel, &incoming.address, OutgoingMessage::text(message))
            .await?;
        Ok(true)
    }

    async fn try_handle_compact_mode_command(
        &self,
        channel: &Arc<dyn Channel>,
        incoming: &IncomingMessage,
    ) -> Result<bool> {
        let Some(argument) = parse_compact_mode_command(incoming.text.as_deref()) else {
            return Ok(false);
        };

        if let Some(mode_name) = argument {
            let enabled = match mode_name.trim() {
                "on" | "enable" | "enabled" => true,
                "off" | "disable" | "disabled" => false,
                _ => {
                    let error = anyhow!("unknown compact mode {}", mode_name);
                    self.send_user_error_message(channel, &incoming.address, &error)
                        .await;
                    return Err(error);
                }
            };
            self.with_conversations(|conversations| {
                conversations.set_context_compaction_enabled(&incoming.address, Some(enabled))
            })?;
            self.send_channel_message(
                channel,
                &incoming.address,
                OutgoingMessage::text(format!(
                    "Automatic context compaction is now `{}` for this conversation.",
                    if enabled { "enabled" } else { "disabled" }
                )),
            )
            .await?;
            return Ok(true);
        }

        self.send_channel_message(
            channel,
            &incoming.address,
            self.compact_mode_message(&incoming.address)?,
        )
        .await?;
        Ok(true)
    }

    async fn try_handle_agent_command(
        &self,
        channel: &Arc<dyn Channel>,
        incoming: &IncomingMessage,
    ) -> Result<bool> {
        let Some(command) = parse_agent_command(incoming.text.as_deref()) else {
            return Ok(false);
        };

        match command {
            AgentCommand::ShowSelection => {
                self.send_channel_message(
                    channel,
                    &incoming.address,
                    self.agent_model_selection_message(
                        &incoming.address,
                        "Choose a model for this conversation.",
                    )?,
                )
                .await?;
            }
            AgentCommand::SelectModel { model_key } => {
                if !self.models.contains_key(&model_key) {
                    let error = anyhow!("unknown model {}", model_key);
                    self.send_user_error_message(channel, &incoming.address, &error)
                        .await;
                    return Err(error);
                }
                let selected_backend = AgentBackendKind::AgentFrame;
                self.ensure_model_available_for_backend(selected_backend, &model_key)?;
                let stored_settings = self.effective_conversation_settings(&incoming.address)?;
                let current_backend = self.selected_agent_backend(&incoming.address)?;
                let current_model_key = self.selected_main_model_key(&incoming.address)?;
                if current_backend == Some(selected_backend)
                    && current_model_key.as_deref() == Some(model_key.as_str())
                {
                    if stored_settings.agent_backend != Some(selected_backend) {
                        self.with_conversations(|conversations| {
                            conversations.set_agent_selection(
                                &incoming.address,
                                Some(selected_backend),
                                Some(model_key.clone()),
                            )
                        })?;
                        self.send_channel_message(
                            channel,
                            &incoming.address,
                            OutgoingMessage::text(format!(
                                "Conversation model updated to `{}`.",
                                model_key
                            )),
                        )
                        .await?;
                        return Ok(true);
                    }
                    self.send_channel_message(
                        channel,
                        &incoming.address,
                        OutgoingMessage::text(format!(
                            "Conversation model is already `{}`. No change was made.",
                            model_key
                        )),
                    )
                    .await?;
                    return Ok(true);
                }
                let compacted = if let Some(previous_model_key) = current_model_key {
                    let session = self
                        .ensure_foreground_actor(&incoming.address)?
                        .snapshot()?;
                    self.compact_session_now(&session, &previous_model_key, false)
                        .await
                        .unwrap_or(false)
                } else {
                    false
                };
                let conversation = self.with_conversations(|conversations| {
                    conversations.set_agent_selection(
                        &incoming.address,
                        Some(selected_backend),
                        Some(model_key.clone()),
                    )
                })?;
                self.invalidate_foreground_agent_frame_runtime(&incoming.address)?;
                let effective_model_key = conversation
                    .settings
                    .main_model
                    .clone()
                    .expect("model just set");
                self.send_channel_message(
                    channel,
                    &incoming.address,
                    OutgoingMessage::text(format!(
                        "Conversation model updated to `{}`.{}",
                        effective_model_key,
                        if compacted {
                            " Existing context was compacted before the switch."
                        } else {
                            ""
                        }
                    )),
                )
                .await?;
            }
        }

        Ok(true)
    }

    async fn try_handle_sandbox_command(
        &self,
        channel: &Arc<dyn Channel>,
        incoming: &IncomingMessage,
    ) -> Result<bool> {
        let Some(argument) = parse_sandbox_command(incoming.text.as_deref()) else {
            return Ok(false);
        };

        if let Some(mode_name) = argument {
            let selected_mode = if mode_name == "default" {
                None
            } else {
                let parsed = parse_sandbox_mode_value(&mode_name)
                    .ok_or_else(|| anyhow!("unknown sandbox mode {}", mode_name));
                let parsed = match parsed {
                    Ok(mode) => mode,
                    Err(error) => {
                        self.send_user_error_message(channel, &incoming.address, &error)
                            .await;
                        return Err(error);
                    }
                };
                if !self.available_sandbox_modes().contains(&parsed) {
                    let error =
                        anyhow!("sandbox mode {} is not available on this system", mode_name);
                    self.send_user_error_message(channel, &incoming.address, &error)
                        .await;
                    return Err(error);
                }
                Some(parsed)
            };
            let conversation = self.with_conversations(|conversations| {
                conversations.set_sandbox_mode(&incoming.address, selected_mode)
            })?;
            self.invalidate_foreground_agent_frame_runtime(&incoming.address)?;
            let effective_mode = conversation
                .settings
                .sandbox_mode
                .unwrap_or(self.sandbox.mode);
            self.send_channel_message(
                channel,
                &incoming.address,
                OutgoingMessage::text(format!(
                    "Conversation sandbox mode updated to `{}`.",
                    sandbox_mode_label(effective_mode)
                )),
            )
            .await?;
            return Ok(true);
        }

        let current_mode = self.effective_sandbox_mode(&incoming.address)?;
        let options = self
            .available_sandbox_modes()
            .into_iter()
            .map(|mode| ShowOption {
                label: sandbox_mode_label(mode).to_string(),
                value: format!("/sandbox {}", sandbox_mode_value(mode)),
            })
            .chain(std::iter::once(ShowOption {
                label: "default".to_string(),
                value: "/sandbox default".to_string(),
            }))
            .collect::<Vec<_>>();
        self.send_channel_message(
            channel,
            &incoming.address,
            OutgoingMessage::with_options(
                format!(
                    "Current conversation sandbox mode: `{}`\nChoose a mode below or send `/sandbox <mode>`.",
                    sandbox_mode_label(current_mode)
                ),
                "Choose a sandbox mode",
                options,
            ),
        )
        .await?;
        Ok(true)
    }

    async fn try_handle_think_command(
        &self,
        channel: &Arc<dyn Channel>,
        incoming: &IncomingMessage,
    ) -> Result<bool> {
        let Some(argument) = parse_think_command(incoming.text.as_deref()) else {
            return Ok(false);
        };

        if let Some(effort_name) = argument {
            let selected_effort = if effort_name == "default" {
                None
            } else {
                let parsed = parse_reasoning_effort_value(&effort_name)
                    .ok_or_else(|| anyhow!("unknown reasoning effort {}", effort_name));
                let parsed = match parsed {
                    Ok(effort) => effort,
                    Err(error) => {
                        self.send_user_error_message(channel, &incoming.address, &error)
                            .await;
                        return Err(error);
                    }
                };
                Some(parsed.to_string())
            };
            let conversation = self.with_conversations(|conversations| {
                conversations.set_reasoning_effort(&incoming.address, selected_effort)
            })?;
            let effective_effort = conversation
                .settings
                .reasoning_effort
                .clone()
                .or_else(|| {
                    self.selected_main_model_key(&incoming.address)
                        .ok()
                        .flatten()
                        .and_then(|model_key| {
                            self.models.get(&model_key).and_then(|model| {
                                model
                                    .reasoning
                                    .as_ref()
                                    .and_then(|reasoning| reasoning.effort.clone())
                            })
                        })
                })
                .unwrap_or_else(|| "default".to_string());
            self.send_channel_message(
                channel,
                &incoming.address,
                OutgoingMessage::text(format!(
                    "Conversation reasoning effort updated to `{}`.",
                    effective_effort
                )),
            )
            .await?;
            return Ok(true);
        }

        self.send_channel_message(
            channel,
            &incoming.address,
            self.reasoning_effort_message(&incoming.address)?,
        )
        .await?;
        Ok(true)
    }

    async fn try_handle_snapshot_command(
        &self,
        channel: &Arc<dyn Channel>,
        incoming: &IncomingMessage,
    ) -> Result<bool> {
        if matches!(
            parse_optional_command_argument(incoming.text.as_deref(), "/snapsave"),
            Some(None)
        ) {
            self.send_channel_message(
                channel,
                &incoming.address,
                OutgoingMessage::text(
                    "Usage: `/snapsave <name>`\nExample: `/snapsave demo`".to_string(),
                ),
            )
            .await?;
            return Ok(true);
        }

        if let Some(checkpoint_name) = parse_snap_save_command(incoming.text.as_deref()) {
            let session = self
                .ensure_foreground_actor(&incoming.address)?
                .snapshot()?;
            let checkpoint =
                self.with_sessions(|sessions| sessions.export_checkpoint(&incoming.address))?;
            let bundle = SnapshotBundle {
                saved_at: Utc::now(),
                source_address: incoming.address.clone(),
                settings: self.effective_conversation_settings(&incoming.address)?,
                session: checkpoint,
            };
            let conversation_memory_root = conversation_memory_root(&session);
            let record = self.with_snapshots(|snapshots| {
                snapshots.save_snapshot(
                    &incoming.address,
                    &checkpoint_name,
                    bundle,
                    &session.workspace_root,
                    Some(&conversation_memory_root),
                )
            })?;
            self.send_channel_message(
                channel,
                &incoming.address,
                OutgoingMessage::text(format!(
                    "Saved global snapshot `{}` at {}.",
                    record.name, record.saved_at
                )),
            )
            .await?;
            return Ok(true);
        }

        if parse_snap_list_command(incoming.text.as_deref())
            || matches!(
                parse_optional_command_argument(incoming.text.as_deref(), "/snapload"),
                Some(None)
            )
        {
            let snapshots = self.with_snapshots(|snapshots| Ok(snapshots.list_snapshots()))?;
            if snapshots.is_empty() {
                self.send_channel_message(
                    channel,
                    &incoming.address,
                    OutgoingMessage::text(
                        "There are no saved snapshots yet. Use `/snapsave <name>` first."
                            .to_string(),
                    ),
                )
                .await?;
                return Ok(true);
            }
            let lines = snapshots
                .iter()
                .map(|record| {
                    format!(
                        "- `{}` ({}, from `{}`)",
                        record.name, record.saved_at, record.source_conversation_id
                    )
                })
                .collect::<Vec<_>>();
            let options = snapshots
                .iter()
                .map(|record| ShowOption {
                    label: record.name.clone(),
                    value: format!("/snapload {}", record.name),
                })
                .collect::<Vec<_>>();
            self.send_channel_message(
                channel,
                &incoming.address,
                OutgoingMessage::with_options(
                    format!(
                        "Saved global snapshots:\n{}\n\nChoose one below or send `/snapload <name>`.",
                        lines.join("\n")
                    ),
                    "Choose a snapshot to load",
                    options,
                ),
            )
            .await?;
            return Ok(true);
        }

        if let Some(checkpoint_name) = parse_snap_load_command(incoming.text.as_deref()) {
            let loaded =
                match self.with_snapshots(|snapshots| snapshots.load_snapshot(&checkpoint_name)) {
                    Ok(loaded) => loaded,
                    Err(error) => {
                        self.send_user_error_message(channel, &incoming.address, &error)
                            .await;
                        return Err(error);
                    }
                };
            self.with_conversations(|conversations| {
                conversations.set_agent_selection(
                    &incoming.address,
                    loaded.bundle.settings.agent_backend,
                    loaded.bundle.settings.main_model.clone(),
                )
            })?;
            self.with_conversations(|conversations| {
                conversations
                    .set_sandbox_mode(&incoming.address, loaded.bundle.settings.sandbox_mode)
            })?;
            self.with_conversations(|conversations| {
                conversations.set_reasoning_effort(
                    &incoming.address,
                    loaded.bundle.settings.reasoning_effort.clone(),
                )
            })?;
            self.with_conversations(|conversations| {
                conversations.set_context_compaction_enabled(
                    &incoming.address,
                    loaded.bundle.settings.context_compaction_enabled,
                )
            })?;
            let loaded_record = loaded.record.clone();
            let loaded_workspace_dir = loaded.workspace_dir.clone();
            let loaded_conversation_memory_dir = loaded.conversation_memory_dir.clone();
            let loaded_session = loaded.bundle.session.clone();
            self.destroy_foreground_session(&incoming.address)?;
            let workspace = self.workspace_manager.create_workspace(
                uuid::Uuid::new_v4(),
                uuid::Uuid::new_v4(),
                Some(&format!("snapshot-{}", loaded_record.name)),
            )?;
            replace_directory_contents(&workspace.files_dir, &loaded_workspace_dir)?;
            let restored = self.with_sessions(|sessions| {
                sessions.restore_foreground_from_checkpoint(
                    &incoming.address,
                    loaded_session,
                    workspace.id.clone(),
                    workspace.files_dir.clone(),
                )
            })?;
            if let Some(memory_dir) = loaded_conversation_memory_dir.as_ref() {
                let restored_memory_root = conversation_memory_root(&restored);
                replace_directory_contents(&restored_memory_root, memory_dir)?;
            }
            self.send_channel_message(
                channel,
                &incoming.address,
                OutgoingMessage::text(format!(
                    "Loaded snapshot `{}` into a new session with workspace `{}`.",
                    loaded_record.name, restored.workspace_id
                )),
            )
            .await?;
            return Ok(true);
        }

        Ok(false)
    }

    async fn try_handle_api_timeout_command(
        &self,
        channel: &Arc<dyn Channel>,
        incoming: &IncomingMessage,
    ) -> Result<bool> {
        if incoming
            .text
            .as_deref()
            .map(str::trim)
            .is_some_and(|text| command_starts_with(text, "/set_api_timeout"))
            && parse_set_api_timeout_command(incoming.text.as_deref()).is_none()
        {
            let usage = "Usage: /set_api_timeout <seconds|default>\nExamples:\n/set_api_timeout 300\n/set_api_timeout default";
            self.send_channel_message(channel, &incoming.address, OutgoingMessage::text(usage))
                .await?;
            return Ok(true);
        }

        let Some(argument) = parse_set_api_timeout_command(incoming.text.as_deref()) else {
            return Ok(false);
        };

        let session = self
            .ensure_foreground_actor(&incoming.address)?
            .snapshot()?;
        let effective_model_key = self.effective_main_model_key(&incoming.address)?;
        let model_timeout_seconds = self.model_upstream_timeout_seconds(&effective_model_key)?;
        let (override_timeout, status_text) =
            match format_api_timeout_update(&session, model_timeout_seconds, &argument) {
                Ok(result) => result,
                Err(error) => {
                    self.send_user_error_message(channel, &incoming.address, &error)
                        .await;
                    return Err(error);
                }
            };
        let actor = self
            .with_sessions(|sessions| sessions.resolve_foreground_by_address(&incoming.address))?;
        actor.set_api_timeout_override(override_timeout)?;
        self.send_channel_message(
            channel,
            &incoming.address,
            OutgoingMessage::text(status_text),
        )
        .await?;
        Ok(true)
    }
}
