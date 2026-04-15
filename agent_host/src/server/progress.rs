use super::*;
use crate::channel::{ProgressFeedback, ProgressFeedbackFinalState, ProgressFeedbackUpdate};
use agent_frame::ExecutionProgressPhase;

impl ServerRuntime {
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
        let Some(mut feedback) = progress_feedback_for_event(session, model_key, event) else {
            return;
        };
        feedback.message_id = self.current_progress_message_id(session);
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

    pub(super) async fn send_progress_feedback_for_failure(
        &self,
        session: &SessionSnapshot,
        model_key: &str,
        error: &anyhow::Error,
    ) {
        let Some(channel) = self.channels.get(&session.address.channel_id) else {
            return;
        };
        let feedback = ProgressFeedback {
            turn_id: session.id.to_string(),
            text: progress_text(
                model_key,
                &format!("失败：{}", truncate_single_line(&format!("{error:#}"), 160)),
            ),
            important: true,
            final_state: Some(ProgressFeedbackFinalState::Failed),
            message_id: self.current_progress_message_id(session),
        };
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
                "failed to update failed channel progress feedback"
            );
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
        let feedback = ProgressFeedback {
            turn_id: session.id.to_string(),
            text: progress_text_for_execution(model_key, progress),
            important: true,
            final_state: None,
            message_id: self.current_progress_message_id(session),
        };
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
                "failed to update execution progress feedback"
            );
        }
    }

    fn current_progress_message_id(&self, session: &SessionSnapshot) -> Option<String> {
        self.with_sessions(|sessions| Ok(sessions.get_snapshot(&session.address)))
            .ok()
            .flatten()
            .and_then(|snapshot| snapshot.session_state.progress_message)
            .or_else(|| session.session_state.progress_message.clone())
            .map(|state| state.message_id)
    }

    fn apply_progress_feedback_update(
        &self,
        session: &SessionSnapshot,
        update: ProgressFeedbackUpdate,
    ) -> Result<()> {
        match update {
            ProgressFeedbackUpdate::Unchanged => Ok(()),
            ProgressFeedbackUpdate::StoreMessage { message_id } => self.with_sessions(|sessions| {
                sessions.set_progress_message(
                    &session.address,
                    Some(SessionProgressMessageState {
                        turn_id: session.id.to_string(),
                        message_id,
                    }),
                )
            }),
            ProgressFeedbackUpdate::ClearMessage => {
                self.with_sessions(|sessions| sessions.set_progress_message(&session.address, None))
            }
        }
    }
}

fn progress_feedback_for_event(
    session: &SessionSnapshot,
    model_key: &str,
    event: &SessionEvent,
) -> Option<ProgressFeedback> {
    let (activity, important, final_state) = match event {
        SessionEvent::CompactionStarted { .. } => ("压缩中...".to_string(), true, None),
        SessionEvent::SessionStarted { .. } | SessionEvent::CompactionCompleted { .. } => {
            return None;
        }
        SessionEvent::RoundStarted { .. }
        | SessionEvent::ModelCallStarted { .. }
        | SessionEvent::ModelCallCompleted { .. } => return None,
        SessionEvent::ToolWaitCompactionStarted { .. } => ("压缩中...".to_string(), true, None),
        SessionEvent::ToolWaitCompactionScheduled { .. }
        | SessionEvent::ToolWaitCompactionCompleted { .. } => return None,
        SessionEvent::ToolCallStarted { .. } | SessionEvent::ToolCallCompleted { .. } => {
            return None;
        }
        SessionEvent::SessionYielded { .. } | SessionEvent::PrefixRewriteApplied { .. } => {
            return None;
        }
        SessionEvent::SessionCompleted { .. } => (
            "完成".to_string(),
            true,
            Some(ProgressFeedbackFinalState::Done),
        ),
    };

    Some(ProgressFeedback {
        turn_id: session.id.to_string(),
        text: progress_text(model_key, &activity),
        important,
        final_state,
        message_id: session
            .session_state
            .progress_message
            .as_ref()
            .map(|state| state.message_id.clone()),
    })
}

fn progress_text(model_key: &str, activity: &str) -> String {
    format!(
        "正在执行\n模型：{}\n阶段：{}\n\n发送新消息可打断；/continue 可继续最近中断的回合。",
        model_key, activity
    )
}

fn progress_text_for_execution(model_key: &str, progress: &ExecutionProgress) -> String {
    match progress.phase {
        ExecutionProgressPhase::Thinking => format!(
            "正在执行\n模型：{}\n状态：思考中...\n\n发送新消息可打断；/continue 可继续最近中断的回合。",
            model_key
        ),
        ExecutionProgressPhase::Tools => {
            let mut lines = vec!["正在执行".to_string(), format!("模型：{model_key}")];
            lines.push("状态：工具执行中".to_string());
            for tool in &progress.tools {
                lines.push(format!(
                    "- {}：{}",
                    tool.tool_name,
                    render_tool_brief_arguments(&tool.tool_name, tool.arguments.as_deref())
                ));
            }
            lines.push(String::new());
            lines.push("发送新消息可打断；/continue 可继续最近中断的回合。".to_string());
            lines.join("\n")
        }
    }
}

fn render_tool_brief_arguments(tool_name: &str, arguments: Option<&str>) -> String {
    let args = arguments
        .and_then(|raw| serde_json::from_str::<Value>(raw).ok())
        .unwrap_or(Value::Null);
    let object = args.as_object();
    let detail = match tool_name {
        "exec_start" | "exec_command" => first_string(object, &["cmd", "command"]),
        "exec_wait" | "exec_write" | "exec_kill" => first_string(object, &["exec_id", "id"]),
        "file_read" | "read_file" | "file_write" | "file_edit" | "ls" | "image_load"
        | "pdf_read" | "audio_transcribe" => first_string(object, &["path", "file_path"]),
        "glob" | "grep" | "web_search" => first_string(object, &["pattern", "query", "q"]),
        "web_fetch" => first_string(object, &["url"]),
        "image_generate" | "subagent_start" | "spawn_agent" => {
            first_string(object, &["prompt", "message", "task"])
        }
        _ => object.and_then(|object| {
            object
                .values()
                .find_map(|value| value.as_str().filter(|value| !value.trim().is_empty()))
        }),
    };
    detail
        .map(|value| truncate_single_line_strict(value, 20))
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| "-".to_string())
}

fn first_string<'a>(
    object: Option<&'a serde_json::Map<String, Value>>,
    keys: &[&str],
) -> Option<&'a str> {
    let object = object?;
    keys.iter()
        .find_map(|key| object.get(*key).and_then(Value::as_str))
}

fn truncate_single_line(input: &str, max_chars: usize) -> String {
    let compact = input.split_whitespace().collect::<Vec<_>>().join(" ");
    if compact.chars().count() <= max_chars {
        return compact;
    }
    let mut out = compact.chars().take(max_chars).collect::<String>();
    out.push_str("...");
    out
}

fn truncate_single_line_strict(input: &str, max_chars: usize) -> String {
    let compact = input.split_whitespace().collect::<Vec<_>>().join(" ");
    if compact.chars().count() <= max_chars {
        return compact;
    }
    if max_chars <= 3 {
        return compact.chars().take(max_chars).collect();
    }
    let mut out = compact.chars().take(max_chars - 3).collect::<String>();
    out.push_str("...");
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renders_execution_progress_as_compact_tool_list() {
        let text = progress_text_for_execution(
            "gpt54",
            &ExecutionProgress {
                round_index: 0,
                phase: ExecutionProgressPhase::Tools,
                tools: vec![
                    agent_frame::ToolExecutionProgress {
                        tool_call_id: "call-1".to_string(),
                        tool_name: "exec_start".to_string(),
                        arguments: Some(
                            r#"{"cmd":"cargo test --manifest-path agent_host/Cargo.toml"}"#
                                .to_string(),
                        ),
                        status: agent_frame::ToolExecutionStatus::Running,
                    },
                    agent_frame::ToolExecutionProgress {
                        tool_call_id: "call-2".to_string(),
                        tool_name: "file_read".to_string(),
                        arguments: Some(r#"{"path":"src/main.rs"}"#.to_string()),
                        status: agent_frame::ToolExecutionStatus::Completed,
                    },
                ],
            },
        );

        assert!(text.contains("状态：工具执行中"));
        assert!(text.contains("- exec_start：cargo test --mani..."));
        assert!(text.contains("- file_read：src/main.rs"));
        assert!(!text.contains("已完成"));
    }

    #[test]
    fn final_session_completed_deletes_progress() {
        let session = SessionSnapshot {
            id: Uuid::nil(),
            agent_id: Uuid::nil(),
            address: ChannelAddress {
                channel_id: "telegram-main".to_string(),
                conversation_id: "1".to_string(),
                user_id: Some("1".to_string()),
                display_name: None,
            },
            root_dir: PathBuf::new(),
            attachments_dir: PathBuf::new(),
            workspace_id: "workspace".to_string(),
            workspace_root: PathBuf::new(),
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
            pending_workspace_summary: false,
            close_after_summary: false,
            session_state: crate::session::DurableSessionState::default(),
        };
        let feedback = progress_feedback_for_event(
            &session,
            "gpt54",
            &SessionEvent::SessionCompleted {
                message_count: 3,
                total_tokens: 42,
            },
        )
        .unwrap();

        assert_eq!(feedback.final_state, Some(ProgressFeedbackFinalState::Done));
    }
}
