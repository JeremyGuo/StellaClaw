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
            tracing::debug!(
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
        if self.try_handle_remote_command(channel, incoming).await? {
            return Ok(true);
        }
        if self.try_handle_mount_command(channel, incoming).await? {
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

    async fn try_handle_mount_command(
        &self,
        channel: &Arc<dyn Channel>,
        incoming: &IncomingMessage,
    ) -> Result<bool> {
        let Some(argument) = parse_mount_command(incoming.text.as_deref()) else {
            return Ok(false);
        };

        if self.remote_execution_active(&incoming.address)? {
            self.send_channel_message(
                channel,
                &incoming.address,
                OutgoingMessage::text(
                    "The current conversation is using `/remote`. `/mount` is unavailable until you run `/remote off`."
                        .to_string(),
                ),
            )
            .await?;
            return Ok(true);
        }

        let Some(folder) = argument else {
            let mounts = self.local_mount_paths_for_address(&incoming.address)?;
            let usage = if mounts.is_empty() {
                "Usage: `/mount <folder>`\nExample: `/mount /srv/shared`".to_string()
            } else {
                format!(
                    "Usage: `/mount <folder>`\nCurrent local mounts:\n{}",
                    mounts
                        .iter()
                        .map(|path| format!("- `{}`", path.display()))
                        .collect::<Vec<_>>()
                        .join("\n")
                )
            };
            self.send_channel_message(channel, &incoming.address, OutgoingMessage::text(usage))
                .await?;
            return Ok(true);
        };

        let session = self
            .ensure_foreground_actor(&incoming.address)?
            .snapshot()?;
        let mount_path = match resolve_local_mount_path(&folder, &session.workspace_root) {
            Ok(path) => path,
            Err(error) => {
                self.send_user_error_message(channel, &incoming.address, &error)
                    .await;
                return Err(error);
            }
        };
        self.with_conversations(|conversations| {
            conversations.add_local_mount(&incoming.address, mount_path.clone())
        })?;
        self.invalidate_foreground_agent_frame_runtime(&incoming.address)?;
        let effective_mode = self.effective_sandbox_mode(&incoming.address)?;
        self.send_channel_message(
            channel,
            &incoming.address,
            OutgoingMessage::text(format!(
                "Mounted `{}` for this conversation.{}",
                mount_path.display(),
                if effective_mode == SandboxMode::Bubblewrap {
                    " The AgentFrame bubblewrap runtime was refreshed so the path is available on the next turn."
                } else {
                    " The current sandbox mode is `subprocess`; the mount is stored and will apply when this conversation uses bubblewrap."
                }
            )),
        )
        .await?;
        Ok(true)
    }

    async fn try_handle_remote_command(
        &self,
        channel: &Arc<dyn Channel>,
        incoming: &IncomingMessage,
    ) -> Result<bool> {
        let Some(argument) = parse_remote_command(incoming.text.as_deref()) else {
            return Ok(false);
        };

        let current = self.remote_execution_binding(&incoming.address)?;
        let Some(argument) = argument else {
            let text = match current {
                Some(binding) => format!(
                    "Current remote execution mode: `{}`\nUsage:\n`/remote /absolute/local/path`\n`/remote <host> <path>`\n`/remote off`",
                    binding.describe()
                ),
                None if self.web_channels.contains_key(&incoming.address.channel_id) => "This web conversation requires remote execution before chatting.\nUsage:\n`/remote /absolute/local/path`\n`/remote <host> <path>`".to_string(),
                None => "Remote execution mode is off.\nUsage:\n`/remote /absolute/local/path`\n`/remote <host> <path>`\n`/remote off`".to_string(),
            };
            self.send_channel_message(channel, &incoming.address, OutgoingMessage::text(text))
                .await?;
            return Ok(true);
        };

        let trimmed = argument.trim();
        let result = if trimmed.eq_ignore_ascii_case("off") {
            if web_channel_disallows_remote_deactivation(
                self.web_channels.contains_key(&incoming.address.channel_id),
                trimmed,
            ) {
                let error = anyhow!(
                    "Web conversations must stay in remote execution mode. Rebind with `/remote /absolute/local/path` or `/remote <host> <path>` instead of turning it off."
                );
                self.send_user_error_message(channel, &incoming.address, &error)
                    .await;
                return Err(error);
            }
            self.deactivate_remote_execution(&incoming.address)
        } else if trimmed.starts_with('/') || trimmed.starts_with("~/") {
            let path = validate_local_execution_path(trimmed)?;
            self.activate_remote_execution(
                &incoming.address,
                RemoteExecutionBinding::Local { path },
            )
        } else {
            let mut parts = trimmed.splitn(2, char::is_whitespace);
            let host = parts.next().unwrap_or_default();
            let path = parts.next().map(str::trim).unwrap_or("");
            let (host, path) = validate_ssh_execution_binding(host, path)?;
            self.activate_remote_execution(
                &incoming.address,
                RemoteExecutionBinding::Ssh { host, path },
            )
        };

        match result {
            Ok(snapshot) => {
                let text = match snapshot.settings.remote_execution {
                    Some(binding) => format!(
                        "Remote execution is now bound to `{}`. Conversation-level workpaths were cleared, and future turns will use this execution root by default.",
                        binding.describe()
                    ),
                    None => {
                        "Remote execution is now off. Future turns will use the normal local workspace flow."
                            .to_string()
                    }
                };
                self.invalidate_foreground_agent_frame_runtime(&incoming.address)?;
                self.send_channel_message(channel, &incoming.address, OutgoingMessage::text(text))
                    .await?;
                Ok(true)
            }
            Err(error) => {
                self.send_user_error_message(channel, &incoming.address, &error)
                    .await;
                Err(error)
            }
        }
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
            let record =
                self.with_snapshot_manager_for_address(&incoming.address, |snapshots| {
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
                    "Saved snapshot `{}` at {}.",
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
            let snapshots = self
                .with_snapshot_manager_for_address(&incoming.address, |snapshots| {
                    Ok(snapshots.list_snapshots())
                })?;
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
                        "Saved snapshots:\n{}\n\nChoose one below or send `/snapload <name>`.",
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
            let loaded = match self
                .with_snapshot_manager_for_address(&incoming.address, |snapshots| {
                    snapshots.load_snapshot(&checkpoint_name)
                }) {
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
            let loaded_remote_execution = loaded.bundle.settings.remote_execution.clone();
            let restored = if let Some(binding) = loaded_remote_execution {
                self.activate_remote_execution(&incoming.address, binding)?;
                let context = self
                    .remote_execution_context(&incoming.address)?
                    .ok_or_else(|| {
                        anyhow!("remote execution context is missing after activation")
                    })?;
                self.destroy_foreground_session(&incoming.address)?;
                replace_directory_contents(&context.workspace_root, &loaded_workspace_dir)?;
                let restored = self.with_sessions(|sessions| {
                    sessions.restore_foreground_from_checkpoint_in_root(
                        &incoming.address,
                        loaded_session,
                        context.workspace_id.clone(),
                        context.workspace_root.clone(),
                        &context.sessions_root,
                    )
                })?;
                self.with_conversations(|conversations| {
                    conversations.set_workspace_id(&incoming.address, None)
                })?;
                restored
            } else {
                if self.remote_execution_active(&incoming.address)? {
                    self.deactivate_remote_execution(&incoming.address)?;
                }
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
                self.with_conversations(|conversations| {
                    conversations.set_workspace_id(&incoming.address, Some(workspace.id.clone()))
                })?;
                restored
            };
            if let Some(memory_dir) = loaded_conversation_memory_dir.as_ref() {
                let restored_memory_root = conversation_memory_root(&restored);
                replace_directory_contents(&restored_memory_root, memory_dir)?;
            }
            self.send_channel_message(
                channel,
                &incoming.address,
                OutgoingMessage::text(format!(
                    "Loaded snapshot `{}` into a new session with execution root `{}`.",
                    loaded_record.name,
                    restored.workspace_root.display()
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

pub(super) fn web_channel_disallows_remote_deactivation(
    is_web_channel: bool,
    argument: &str,
) -> bool {
    is_web_channel && argument.trim().eq_ignore_ascii_case("off")
}
