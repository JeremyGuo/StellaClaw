use super::*;
use crate::channel::{ProgressFeedback, ProgressFeedbackFinalState, ProgressFeedbackUpdate};
use agent_frame::{ExecutionProgressPhase, ToolExecutionStatus};

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
            important: matches!(progress.phase, ExecutionProgressPhase::Thinking),
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
        SessionEvent::SessionStarted { .. } => ("开始处理".to_string(), true, None),
        SessionEvent::CompactionStarted { phase, .. } => {
            (format!("压缩上下文：{phase}"), true, None)
        }
        SessionEvent::CompactionCompleted { compacted, .. } => (
            if *compacted {
                "上下文压缩完成".to_string()
            } else {
                "跳过上下文压缩".to_string()
            },
            true,
            None,
        ),
        SessionEvent::RoundStarted { .. }
        | SessionEvent::ModelCallStarted { .. }
        | SessionEvent::ModelCallCompleted { .. } => return None,
        SessionEvent::ToolWaitCompactionScheduled { tool_name, .. } => (
            format!("等待工具 `{}` 时准备压缩上下文", tool_name),
            false,
            None,
        ),
        SessionEvent::ToolWaitCompactionStarted { tool_name, .. } => (
            format!("等待工具 `{}`，正在压缩上下文", tool_name),
            true,
            None,
        ),
        SessionEvent::ToolWaitCompactionCompleted {
            tool_name,
            compacted,
            ..
        } => (
            format!(
                "工具 `{}` 等待期间上下文压缩{}",
                tool_name,
                if *compacted { "完成" } else { "跳过" }
            ),
            true,
            None,
        ),
        SessionEvent::ToolCallStarted { .. } | SessionEvent::ToolCallCompleted { .. } => {
            return None;
        }
        SessionEvent::SessionYielded { phase, .. } => {
            (format!("已到安全边界：{phase}，等待继续"), true, None)
        }
        SessionEvent::PrefixRewriteApplied { .. } => {
            ("已更新压缩后的上下文前缀".to_string(), false, None)
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
            let mut lines = vec![
                "正在执行".to_string(),
                format!("模型：{model_key}"),
                "工具：".to_string(),
            ];
            for tool in &progress.tools {
                let status = match tool.status {
                    ToolExecutionStatus::Running => "执行中",
                    ToolExecutionStatus::Completed => "已完成",
                    ToolExecutionStatus::Failed => "失败",
                };
                lines.push(format!(
                    "- {}：{}",
                    status,
                    render_tool_activity(&tool.tool_name, tool.arguments.as_deref())
                ));
            }
            lines.push(String::new());
            lines.push("发送新消息可打断；/continue 可继续最近中断的回合。".to_string());
            lines.join("\n")
        }
    }
}

fn render_tool_activity(tool_name: &str, arguments: Option<&str>) -> String {
    let args = arguments
        .and_then(|raw| serde_json::from_str::<Value>(raw).ok())
        .unwrap_or(Value::Null);
    let object = args.as_object();
    match tool_name {
        "exec_start" | "exec_command" => {
            let command = first_string(object, &["cmd", "command"])
                .map(|value| truncate_single_line(value, 160))
                .unwrap_or_else(|| "命令".to_string());
            with_remote(object, format!("执行 `{command}`"))
        }
        "exec_wait" => {
            let exec_id = first_string(object, &["exec_id", "id"]).unwrap_or("进程");
            with_remote(
                object,
                format!("等待 `{}`", truncate_single_line(exec_id, 80)),
            )
        }
        "exec_write" => {
            let exec_id = first_string(object, &["exec_id", "id"]).unwrap_or("进程");
            with_remote(
                object,
                format!("写入 `{}`", truncate_single_line(exec_id, 80)),
            )
        }
        "exec_kill" => {
            let exec_id = first_string(object, &["exec_id", "id"]).unwrap_or("进程");
            with_remote(
                object,
                format!("终止 `{}`", truncate_single_line(exec_id, 80)),
            )
        }
        "file_read" | "read_file" => with_remote(object, path_activity("读取文件", object)),
        "file_write" => with_remote(object, path_activity("写入文件", object)),
        "file_edit" => with_remote(object, path_activity("编辑文件", object)),
        "apply_patch" => with_remote(object, "应用补丁".to_string()),
        "ls" => with_remote(object, path_activity("列目录", object)),
        "glob" => with_remote(object, pattern_activity("查找文件", object)),
        "grep" => with_remote(object, pattern_activity("搜索文本", object)),
        "web_search" => first_string(object, &["query", "q"])
            .map(|query| format!("网页搜索 `{}`", truncate_single_line(query, 120)))
            .unwrap_or_else(|| "网页搜索".to_string()),
        "web_fetch" => first_string(object, &["url"])
            .map(|url| format!("读取网页 `{}`", truncate_single_line(url, 120)))
            .unwrap_or_else(|| "读取网页".to_string()),
        "image_load" => path_activity("读取图片", object),
        "image_generate" => first_string(object, &["prompt"])
            .map(|prompt| format!("生成图片 `{}`", truncate_single_line(prompt, 120)))
            .unwrap_or_else(|| "生成图片".to_string()),
        "pdf_read" => path_activity("读取 PDF", object),
        "audio_transcribe" => path_activity("转写音频", object),
        "user_tell" => "发送中间消息".to_string(),
        "subagent_start" | "spawn_agent" => first_string(object, &["prompt", "message", "task"])
            .map(|prompt| format!("启动子代理 `{}`", truncate_single_line(prompt, 120)))
            .unwrap_or_else(|| "启动子代理".to_string()),
        "wait_agent" => "等待子代理".to_string(),
        "close_agent" => "关闭子代理".to_string(),
        other => generic_tool_activity(other, object),
    }
}

fn path_activity(prefix: &str, object: Option<&serde_json::Map<String, Value>>) -> String {
    let path = first_string(object, &["path", "file_path", "notebook_path"])
        .map(|value| truncate_single_line(value, 120))
        .unwrap_or_else(|| "目标".to_string());
    format!("{prefix} `{path}`")
}

fn pattern_activity(prefix: &str, object: Option<&serde_json::Map<String, Value>>) -> String {
    let pattern = first_string(object, &["pattern", "query", "q"])
        .map(|value| truncate_single_line(value, 120))
        .unwrap_or_else(|| "模式".to_string());
    let path = first_string(object, &["path"])
        .filter(|value| !value.trim().is_empty())
        .map(|value| format!(" in `{}`", truncate_single_line(value, 80)))
        .unwrap_or_default();
    format!("{prefix} `{pattern}`{path}")
}

fn generic_tool_activity(
    tool_name: &str,
    object: Option<&serde_json::Map<String, Value>>,
) -> String {
    let details = object
        .map(|object| {
            object
                .iter()
                .take(2)
                .map(|(key, value)| {
                    let value = value
                        .as_str()
                        .map(|value| value.to_string())
                        .unwrap_or_else(|| value.to_string());
                    format!("{key}: {}", truncate_single_line(&value, 80))
                })
                .collect::<Vec<_>>()
                .join(", ")
        })
        .filter(|value| !value.is_empty());
    match details {
        Some(details) => format!("{} ({})", tool_name, details),
        None => tool_name.to_string(),
    }
}

fn with_remote(object: Option<&serde_json::Map<String, Value>>, activity: String) -> String {
    let Some(remote) = first_string(object, &["remote"])
        .map(str::trim)
        .filter(|value| !value.is_empty() && *value != "local")
    else {
        return activity;
    };
    format!("{activity} @ {remote}")
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renders_exec_start_as_human_progress() {
        let text = render_tool_activity(
            "exec_start",
            Some(
                r#"{"cmd":"cargo test --manifest-path agent_host/Cargo.toml","remote":"wuwen-dev6"}"#,
            ),
        );

        assert_eq!(
            text,
            "执行 `cargo test --manifest-path agent_host/Cargo.toml` @ wuwen-dev6"
        );
    }

    #[test]
    fn renders_file_tools_as_human_progress() {
        assert_eq!(
            render_tool_activity("file_read", Some(r#"{"path":"src/main.rs"}"#)),
            "读取文件 `src/main.rs`"
        );
        assert_eq!(
            render_tool_activity(
                "grep",
                Some(r#"{"pattern":"ToolCallStarted","path":"agent_host/src"}"#)
            ),
            "搜索文本 `ToolCallStarted` in `agent_host/src`"
        );
    }

    #[test]
    fn renders_execution_progress_as_tool_status_list() {
        let text = progress_text_for_execution(
            "gpt54",
            &ExecutionProgress {
                round_index: 0,
                phase: ExecutionProgressPhase::Tools,
                tools: vec![
                    agent_frame::ToolExecutionProgress {
                        tool_call_id: "call-1".to_string(),
                        tool_name: "exec_start".to_string(),
                        arguments: Some(r#"{"cmd":"cargo test"}"#.to_string()),
                        status: ToolExecutionStatus::Running,
                    },
                    agent_frame::ToolExecutionProgress {
                        tool_call_id: "call-2".to_string(),
                        tool_name: "file_read".to_string(),
                        arguments: Some(r#"{"path":"src/main.rs"}"#.to_string()),
                        status: ToolExecutionStatus::Completed,
                    },
                ],
            },
        );

        assert!(text.contains("工具："));
        assert!(text.contains("- 执行中：执行 `cargo test`"));
        assert!(text.contains("- 已完成：读取文件 `src/main.rs`"));
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
            zgent_native: None,
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
