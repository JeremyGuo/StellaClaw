use super::*;
use crate::channel::{ProgressFeedback, ProgressFeedbackFinalState, ProgressFeedbackUpdate};

impl AgentRuntimeView {
    pub(super) async fn cleanup_stale_progress_messages_once(&self) -> Result<()> {
        let sessions = self.with_sessions(|sessions| Ok(sessions.list_foreground_snapshots()))?;
        for session in sessions {
            let Some(progress_message) = session.session_state.progress_message.clone() else {
                continue;
            };
            let Some(channel) = self.channels.get(&session.address.channel_id) else {
                continue;
            };
            let feedback = ProgressFeedback {
                turn_id: progress_message.turn_id,
                text: String::new(),
                important: true,
                final_state: Some(ProgressFeedbackFinalState::Done),
                message_id: Some(progress_message.message_id),
            };
            match channel
                .update_progress_feedback(&session.address, feedback)
                .await
            {
                Ok(update) => {
                    self.apply_progress_feedback_update(&session, update)?;
                }
                Err(error) => {
                    warn!(
                        log_stream = "channel",
                        kind = "stale_progress_cleanup_failed",
                        channel_id = %session.address.channel_id,
                        conversation_id = %session.address.conversation_id,
                        error = %format!("{error:#}"),
                        "failed to clean up stale progress message"
                    );
                }
            }
        }
        Ok(())
    }

    pub(super) async fn send_progress_feedback_for_event(
        &self,
        session: &SessionSnapshot,
        model_key: &str,
        event: &SessionEvent,
    ) {
        let Some(channel) = self.channels.get(&session.address.channel_id) else {
            return;
        };
        let effects = match self.with_conversations_and_sessions(|conversations, sessions| {
            let Some(actor) = conversations.resolve_foreground_actor(&session.address, sessions)?
            else {
                return Ok(Vec::new());
            };
            actor.receive_runtime_event_with_effects(model_key, event)
        }) {
            Ok(effects) => effects,
            Err(error) => {
                warn!(
                    log_stream = "channel",
                    kind = "progress_feedback_failed",
                    channel_id = %session.address.channel_id,
                    conversation_id = %session.address.conversation_id,
                    error = %format!("{error:#}"),
                    "failed to receive runtime event in session actor"
                );
                return;
            }
        };
        for effect in effects {
            self.execute_session_effect(channel, session, effect).await;
        }
    }

    pub(super) async fn send_progress_feedback_for_failure(
        &self,
        session: &SessionSnapshot,
        model_key: &str,
        error: &anyhow::Error,
    ) {
        let Some(channel) = self.channels.get(&session.address.channel_id) else {
            return;
        };
        let effects = match self.with_conversations_and_sessions(|conversations, sessions| {
            let Some(actor) = conversations.resolve_foreground_actor(&session.address, sessions)?
            else {
                return Ok(Vec::new());
            };
            actor.receive_runtime_failure(model_key, error)
        }) {
            Ok(effects) => effects,
            Err(error) => {
                warn!(
                    log_stream = "channel",
                    kind = "progress_feedback_failed",
                    channel_id = %session.address.channel_id,
                    conversation_id = %session.address.conversation_id,
                    error = %format!("{error:#}"),
                    "failed to receive runtime failure in session actor"
                );
                return;
            }
        };
        for effect in effects {
            self.execute_session_effect(channel, session, effect).await;
        }
    }

    pub(super) async fn send_progress_feedback_for_progress(
        &self,
        session: &SessionSnapshot,
        model_key: &str,
        progress: &ExecutionProgress,
    ) {
        let Some(channel) = self.channels.get(&session.address.channel_id) else {
            return;
        };
        let effects = match self.with_conversations_and_sessions(|conversations, sessions| {
            let Some(actor) = conversations.resolve_foreground_actor(&session.address, sessions)?
            else {
                return Ok(Vec::new());
            };
            actor.receive_runtime_progress(model_key, progress)
        }) {
            Ok(effects) => effects,
            Err(error) => {
                warn!(
                    log_stream = "channel",
                    kind = "progress_feedback_failed",
                    channel_id = %session.address.channel_id,
                    conversation_id = %session.address.conversation_id,
                    error = %format!("{error:#}"),
                    "failed to receive runtime progress in session actor"
                );
                return;
            }
        };
        for effect in effects {
            self.execute_session_effect(channel, session, effect).await;
        }
    }

    async fn execute_session_effect(
        &self,
        channel: &Arc<dyn Channel>,
        session: &SessionSnapshot,
        effect: SessionEffect,
    ) {
        match effect {
            SessionEffect::UpdateProgress(feedback) => {
                if let Err(error) = channel
                    .update_progress_feedback(&session.address, feedback)
                    .await
                    .and_then(|update| self.apply_progress_feedback_update(session, update))
                {
                    warn!(
                        log_stream = "channel",
                        kind = "progress_feedback_failed",
                        channel_id = %session.address.channel_id,
                        conversation_id = %session.address.conversation_id,
                        error = %format!("{error:#}"),
                        "failed to update channel progress feedback"
                    );
                }
            }
            SessionEffect::UserVisibleText(text) => {
                if let Err(error) = channel
                    .send(&session.address, OutgoingMessage::text(text))
                    .await
                {
                    warn!(
                        log_stream = "channel",
                        kind = "session_effect_user_visible_send_failed",
                        channel_id = %session.address.channel_id,
                        conversation_id = %session.address.conversation_id,
                        error = %format!("{error:#}"),
                        "failed to deliver session effect message"
                    );
                }
            }
        }
    }

    fn apply_progress_feedback_update(
        &self,
        session: &SessionSnapshot,
        update: ProgressFeedbackUpdate,
    ) -> Result<()> {
        self.with_conversations_and_sessions(|conversations, sessions| {
            let Some(actor) = conversations.resolve_foreground_actor(&session.address, sessions)?
            else {
                return Ok(());
            };
            actor.apply_progress_feedback_update(update)
        })
    }
}
