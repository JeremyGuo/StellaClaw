use super::*;

impl Server {
    pub(super) async fn handle_incoming_control(&self, incoming: &IncomingMessage) -> Result<bool> {
        let Some(control) = incoming.control.as_ref() else {
            return Ok(false);
        };

        match control {
            crate::channel::IncomingControl::ConversationClosed { reason } => {
                info!(
                    log_stream = "session",
                    kind = "channel_conversation_closed",
                    channel_id = %incoming.address.channel_id,
                    conversation_id = %incoming.address.conversation_id,
                    reason = reason,
                    "channel reported that the conversation should be closed"
                );
                self.close_and_remove_conversation(&incoming.address, reason)?;
                Ok(true)
            }
        }
    }

    fn requires_channel_authorization(&self, address: &ChannelAddress) -> bool {
        self.telegram_channel_ids.contains(&address.channel_id)
    }

    pub(super) fn allows_fast_path_agent_selection(
        &self,
        address: &ChannelAddress,
    ) -> Result<bool> {
        if !self.requires_channel_authorization(address) {
            return Ok(true);
        }
        Ok(self.with_channel_auth(|auth| {
            Ok(matches!(
                auth.current_conversation_state(address),
                Some(ConversationApprovalState::Approved)
            ))
        })?)
    }

    fn is_private_conversation(address: &ChannelAddress) -> bool {
        if let Ok(conversation_id) = address.conversation_id.parse::<i64>() {
            conversation_id > 0
        } else {
            address
                .user_id
                .as_deref()
                .is_some_and(|user_id| user_id == address.conversation_id)
        }
    }

    fn is_group_conversation_id(conversation_id: &str) -> bool {
        conversation_id
            .parse::<i64>()
            .map(|value| value < 0)
            .unwrap_or_else(|_| conversation_id.trim_start().starts_with('-'))
    }

    fn conversation_address_from_auth_snapshot(
        channel_id: &str,
        item: &ConversationApprovalSnapshot,
    ) -> ChannelAddress {
        ChannelAddress {
            channel_id: channel_id.to_string(),
            conversation_id: item.conversation_id.clone(),
            user_id: item.user_id.clone(),
            display_name: item.display_name.clone(),
        }
    }

    fn close_and_remove_conversation(&self, address: &ChannelAddress, reason: &str) -> Result<()> {
        self.destroy_foreground_session(address)?;
        self.with_conversations(|conversations| {
            conversations.remove_conversation(address)?;
            Ok(())
        })?;
        let removed_auth = self.with_channel_auth(|auth| {
            auth.remove_conversation(&address.channel_id, &address.conversation_id)
        })?;
        let disabled = self.disable_cron_tasks_for_conversation(address)?;
        if disabled > 0 {
            warn!(
                log_stream = "cron",
                kind = "cron_tasks_auto_disabled_for_removed_conversation",
                channel_id = %address.channel_id,
                conversation_id = %address.conversation_id,
                disabled_count = disabled as u64,
                "disabled cron tasks because the conversation was removed"
            );
        }
        info!(
            log_stream = "session",
            kind = "conversation_removed",
            channel_id = %address.channel_id,
            conversation_id = %address.conversation_id,
            auth_record_removed = removed_auth.is_some(),
            reason = reason,
            "removed closed conversation"
        );
        Ok(())
    }

    pub(super) async fn prune_closed_conversations_once(&self) -> Result<()> {
        let channel_ids = self
            .telegram_channel_ids
            .iter()
            .cloned()
            .collect::<Vec<_>>();
        for channel_id in channel_ids {
            let items = self.with_channel_auth(|auth| {
                Ok(auth.list_conversations_including_rejected(&channel_id))
            })?;
            let Some(channel) = self.channels.get(&channel_id).cloned() else {
                continue;
            };
            for item in items {
                if !Self::is_group_conversation_id(&item.conversation_id) {
                    continue;
                }
                let address = Self::conversation_address_from_auth_snapshot(&channel_id, &item);
                if item.state == ConversationApprovalState::Rejected {
                    self.close_and_remove_conversation(
                        &address,
                        "conversation was rejected by administrator",
                    )?;
                    continue;
                }
                match channel.probe_conversation(&address).await {
                    Ok(Some(ConversationProbe::Available {
                        member_count: Some(count),
                    })) if count <= 1 => {
                        self.close_and_remove_conversation(
                            &address,
                            &format!("telegram member_count is {count}"),
                        )?;
                    }
                    Ok(Some(ConversationProbe::Unavailable { reason })) => {
                        self.close_and_remove_conversation(&address, &reason)?;
                    }
                    Ok(_) => {}
                    Err(error) => {
                        warn!(
                            log_stream = "channel",
                            log_key = %channel_id,
                            kind = "conversation_probe_failed",
                            conversation_id = %address.conversation_id,
                            error = %format!("{error:#}"),
                            "conversation probe failed; keeping conversation"
                        );
                    }
                }
            }
        }
        Ok(())
    }

    fn render_chat_approval_label(state: ConversationApprovalState) -> &'static str {
        match state {
            ConversationApprovalState::Pending => "Pending Review",
            ConversationApprovalState::Approved => "Approved",
            ConversationApprovalState::Rejected => "Rejected",
        }
    }

    fn format_chat_approval_subject(
        item: &ConversationApprovalSnapshot,
        admin_private_conversation_id: Option<&str>,
    ) -> String {
        let mut parts = vec![format!("`{}`", item.conversation_id)];
        if item.display_name.is_some() || item.user_id.is_some() {
            let mut details = Vec::new();
            if let Some(name) = item.display_name.as_deref()
                && !name.trim().is_empty()
            {
                details.push(name.trim().to_string());
            }
            if let Some(user_id) = item.user_id.as_deref()
                && !user_id.trim().is_empty()
            {
                details.push(format!("user `{}`", user_id.trim()));
            }
            if !details.is_empty() {
                parts.push(format!("({})", details.join(", ")));
            }
        }
        if admin_private_conversation_id == Some(item.conversation_id.as_str()) {
            parts.push("[admin private chat]".to_string());
        }
        parts.join(" ")
    }

    pub(super) fn format_admin_chat_list_text(
        address: &ChannelAddress,
        admin: Option<ChannelAdminSnapshot>,
        items: &[ConversationApprovalSnapshot],
    ) -> String {
        let pending = items
            .iter()
            .filter(|item| item.state == ConversationApprovalState::Pending)
            .collect::<Vec<_>>();
        let approved = items
            .iter()
            .filter(|item| item.state == ConversationApprovalState::Approved)
            .collect::<Vec<_>>();
        let rejected = items
            .iter()
            .filter(|item| item.state == ConversationApprovalState::Rejected)
            .collect::<Vec<_>>();

        let mut lines = vec![
            format!("Approval dashboard for channel `{}`", address.channel_id),
            format!(
                "Summary: {} pending, {} approved, {} rejected",
                pending.len(),
                approved.len(),
                rejected.len()
            ),
        ];

        if let Some(ref admin) = admin {
            let admin_name = admin
                .display_name
                .as_deref()
                .filter(|value: &&str| !value.trim().is_empty())
                .unwrap_or("unknown");
            lines.push(format!(
                "Administrator: {} (user `{}`)",
                admin_name, admin.user_id
            ));
            if let Some(private_chat) = admin.private_conversation_id.as_deref() {
                lines.push(format!("Admin private chat: `{}`", private_chat));
            }
        }

        let admin_private_conversation_id = admin
            .as_ref()
            .and_then(|value| value.private_conversation_id.as_deref());

        if !pending.is_empty() {
            lines.push(String::new());
            lines.push(
                Self::render_chat_approval_label(ConversationApprovalState::Pending).to_string(),
            );
            for item in pending {
                lines.push(format!(
                    "- {}",
                    Self::format_chat_approval_subject(item, admin_private_conversation_id)
                ));
                lines.push(format!(
                    "  updated: `{}`",
                    item.updated_at.format("%Y-%m-%d %H:%M UTC")
                ));
                lines.push(format!(
                    "  approve: `/admin_chat_approve {}`",
                    item.conversation_id
                ));
                lines.push(format!(
                    "  reject: `/admin_chat_reject {}`",
                    item.conversation_id
                ));
            }
        }

        for (state, bucket) in [
            (ConversationApprovalState::Approved, approved),
            (ConversationApprovalState::Rejected, rejected),
        ] {
            if bucket.is_empty() {
                continue;
            }
            lines.push(String::new());
            lines.push(Self::render_chat_approval_label(state).to_string());
            for item in bucket {
                lines.push(format!(
                    "- {}",
                    Self::format_chat_approval_subject(item, admin_private_conversation_id)
                ));
            }
        }

        lines.join("\n")
    }

    async fn handle_admin_authorize_command(
        &self,
        channel: &Arc<dyn Channel>,
        address: &ChannelAddress,
    ) -> Result<()> {
        if !Self::is_private_conversation(address) {
            self.send_channel_message(
                channel,
                address,
                OutgoingMessage::text(
                    "Please open a private chat with the bot and send `/admin_authorize` there."
                        .to_string(),
                ),
            )
            .await?;
            return Ok(());
        }
        let outcome = self.with_channel_auth(|auth| auth.authorize_admin(address))?;
        let text = match outcome {
            AdminAuthorizeOutcome::Authorized(snapshot) => format!(
                "You are now the administrator for channel `{}` as user `{}`. This private chat is approved automatically. Use `/admin_chat_list` here to review chat requests.",
                address.channel_id, snapshot.user_id
            ),
            AdminAuthorizeOutcome::AlreadyAuthorized(snapshot) => format!(
                "You are already the administrator for channel `{}` as user `{}`. This private chat remains approved.",
                address.channel_id, snapshot.user_id
            ),
            AdminAuthorizeOutcome::OwnedByAnotherAdmin(snapshot) => format!(
                "This channel already has an administrator registered as user `{}`{}.",
                snapshot.user_id,
                snapshot
                    .display_name
                    .as_deref()
                    .map(|name| format!(" ({name})"))
                    .unwrap_or_default()
            ),
        };
        self.send_channel_message(channel, address, OutgoingMessage::text(text))
            .await
    }

    async fn handle_admin_chat_list_command(
        &self,
        channel: &Arc<dyn Channel>,
        address: &ChannelAddress,
    ) -> Result<()> {
        if !Self::is_private_conversation(address)
            || !self.with_channel_auth(|auth| Ok(auth.is_channel_admin(address)))?
        {
            self.send_channel_message(
                channel,
                address,
                OutgoingMessage::text(
                    "Only this channel's administrator can use `/admin_chat_list` from a private chat."
                        .to_string(),
                ),
            )
            .await?;
            return Ok(());
        }
        let items =
            self.with_channel_auth(|auth| Ok(auth.list_conversations(&address.channel_id)))?;
        if items.is_empty() {
            self.send_channel_message(
                channel,
                address,
                OutgoingMessage::text("No chats have requested access yet.".to_string()),
            )
            .await?;
            return Ok(());
        }
        let admin =
            self.with_channel_auth(|auth| Ok(auth.admin_for_channel(&address.channel_id)))?;
        let text = Self::format_admin_chat_list_text(address, admin, &items);
        self.send_channel_message(channel, address, OutgoingMessage::text(text))
            .await
    }

    async fn handle_admin_chat_state_command(
        &self,
        channel: &Arc<dyn Channel>,
        address: &ChannelAddress,
        conversation_id: &str,
        approve: bool,
    ) -> Result<()> {
        if !Self::is_private_conversation(address)
            || !self.with_channel_auth(|auth| Ok(auth.is_channel_admin(address)))?
        {
            let command_name = if approve {
                "/admin_chat_approve"
            } else {
                "/admin_chat_reject"
            };
            self.send_channel_message(
                channel,
                address,
                OutgoingMessage::text(format!(
                    "Only this channel's administrator can use `{command_name}` from a private chat."
                )),
            )
            .await?;
            return Ok(());
        }
        let snapshot: ConversationApprovalSnapshot = self.with_channel_auth(|auth| {
            if approve {
                auth.approve_conversation(&address.channel_id, conversation_id)
            } else {
                auth.reject_conversation(&address.channel_id, conversation_id)
            }
        })?;
        if !approve {
            let rejected_address =
                Self::conversation_address_from_auth_snapshot(&address.channel_id, &snapshot);
            self.close_and_remove_conversation(
                &rejected_address,
                "conversation was rejected by administrator",
            )?;
            self.send_channel_message(
                channel,
                address,
                OutgoingMessage::text(format!(
                    "Conversation `{}` was rejected and removed.",
                    snapshot.conversation_id
                )),
            )
            .await?;
            return Ok(());
        }
        self.send_channel_message(
            channel,
            address,
            OutgoingMessage::text(format!(
                "Conversation `{}` is now `{}`.",
                snapshot.conversation_id, "approved"
            )),
        )
        .await
    }

    pub(super) async fn enforce_channel_authorization(
        &self,
        channel: &Arc<dyn Channel>,
        incoming: &IncomingMessage,
    ) -> Result<bool> {
        if !self.requires_channel_authorization(&incoming.address) {
            return Ok(false);
        }

        let text = incoming.text.as_deref();
        if parse_admin_authorize_command(text) {
            self.handle_admin_authorize_command(channel, &incoming.address)
                .await?;
            return Ok(true);
        }

        let admin = self
            .with_channel_auth(|auth| Ok(auth.admin_for_channel(&incoming.address.channel_id)))?;
        let Some(_admin) = admin else {
            self.send_channel_message(
                channel,
                &incoming.address,
                OutgoingMessage::text(
                    "This channel has no administrator yet. Please open a private chat with the bot and send `/admin_authorize` (or `/authorize`) there."
                        .to_string(),
                ),
            )
            .await?;
            return Ok(true);
        };

        let is_admin_private = Self::is_private_conversation(&incoming.address)
            && self.with_channel_auth(|auth| Ok(auth.is_channel_admin(&incoming.address)))?;

        if is_admin_private && parse_admin_chat_list_command(text) {
            self.handle_admin_chat_list_command(channel, &incoming.address)
                .await?;
            return Ok(true);
        }
        if is_admin_private && let Some(conversation_id) = parse_admin_chat_approve_command(text) {
            self.handle_admin_chat_state_command(
                channel,
                &incoming.address,
                &conversation_id,
                true,
            )
            .await?;
            return Ok(true);
        }
        if is_admin_private && let Some(conversation_id) = parse_admin_chat_reject_command(text) {
            self.handle_admin_chat_state_command(
                channel,
                &incoming.address,
                &conversation_id,
                false,
            )
            .await?;
            return Ok(true);
        }

        let state = self.with_channel_auth(|auth| {
            let current = auth.current_conversation_state(&incoming.address);
            if current.is_none() {
                return auth.ensure_pending_conversation(&incoming.address);
            }
            Ok(current.expect("checked is_some above"))
        })?;
        match state {
            ConversationApprovalState::Approved => Ok(false),
            ConversationApprovalState::Pending => {
                self.send_channel_message(
                    channel,
                    &incoming.address,
                    OutgoingMessage::text(
                        "This conversation is waiting for administrator approval. Please ask the channel admin to review it with `/admin_chat_list` in their private chat."
                            .to_string(),
                    ),
                )
                .await?;
                Ok(true)
            }
            ConversationApprovalState::Rejected => Ok(true),
        }
    }

    fn should_auto_close_conversation_after_send_error(
        &self,
        address: &ChannelAddress,
        error: &anyhow::Error,
    ) -> bool {
        if !address.conversation_id.starts_with('-') {
            return false;
        }
        let message = format!("{error:#}").to_ascii_lowercase();
        message.contains("bot was kicked from the group chat")
            || message.contains("chat not found")
            || message.contains("group chat was deleted")
            || message.contains("bot is not a member of the channel chat")
            || message.contains("forbidden: bot was kicked")
    }

    pub(super) async fn send_channel_message(
        &self,
        channel: &Arc<dyn Channel>,
        address: &ChannelAddress,
        message: OutgoingMessage,
    ) -> Result<()> {
        match channel.send(address, message).await {
            Ok(()) => Ok(()),
            Err(error) => {
                if self.should_auto_close_conversation_after_send_error(address, &error) {
                    warn!(
                        log_stream = "session",
                        kind = "channel_send_closed_conversation",
                        channel_id = %address.channel_id,
                        conversation_id = %address.conversation_id,
                        error = %format!("{error:#}"),
                        "channel send indicates the conversation no longer exists; closing foreground session"
                    );
                    self.close_and_remove_conversation(
                        address,
                        &format!("channel send failed: {error:#}"),
                    )?;
                }
                Err(error)
            }
        }
    }

    fn disable_cron_tasks_for_conversation(&self, address: &ChannelAddress) -> Result<usize> {
        let mut manager = self
            .cron_manager
            .lock()
            .map_err(|_| anyhow!("cron manager lock poisoned"))?;
        manager.disable_for_address(address)
    }

    pub(super) async fn send_user_error_message(
        &self,
        channel: &Arc<dyn Channel>,
        address: &ChannelAddress,
        error: &anyhow::Error,
    ) {
        let text = user_facing_error_text(&self.main_agent.language, error);
        if let Err(send_error) = self
            .send_channel_message(channel, address, OutgoingMessage::text(text))
            .await
        {
            error!(
                log_stream = "server",
                kind = "send_user_error_failed",
                error = %format!("{send_error:#}"),
                "failed to send user-facing error message"
            );
        }
    }
}
