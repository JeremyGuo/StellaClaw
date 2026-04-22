use super::*;

pub(super) fn parse_uuid_arg(
    arguments: &serde_json::Map<String, Value>,
    key: &str,
) -> Result<uuid::Uuid> {
    let value = arguments
        .get(key)
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("{} must be a string UUID", key))?;
    uuid::Uuid::parse_str(value).with_context(|| format!("{} must be a valid UUID", key))
}

pub(super) fn string_arg_required(
    arguments: &serde_json::Map<String, Value>,
    key: &str,
) -> Result<String> {
    arguments
        .get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned)
        .ok_or_else(|| anyhow!("{} must be a non-empty string", key))
}

pub(super) fn optional_string_arg(
    arguments: &serde_json::Map<String, Value>,
    key: &str,
) -> Result<Option<String>> {
    match arguments.get(key) {
        Some(value) => value
            .as_str()
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(ToOwned::to_owned)
            .map(Some)
            .ok_or_else(|| anyhow!("{} must be a non-empty string", key)),
        None => Ok(None),
    }
}

#[cfg(test)]
mod tests {
    use super::{cron_schedule_from_required_tool_args, optional_cron_schedule_from_tool_args};
    use serde_json::{Value, json};

    fn object(value: Value) -> serde_json::Map<String, Value> {
        value.as_object().cloned().expect("test object")
    }

    #[test]
    fn named_cron_fields_compile_to_seconds_first_schedule() {
        let args = object(json!({
            "cron_second": "0",
            "cron_minute": "0",
            "cron_hour": "*",
            "cron_day_of_month": "*",
            "cron_month": "*",
            "cron_day_of_week": "*"
        }));

        let schedule = cron_schedule_from_required_tool_args(&args).unwrap();
        assert_eq!(schedule, "0 0 * * * *");
    }

    #[test]
    fn named_cron_fields_include_optional_year() {
        let args = object(json!({
            "cron_second": "0",
            "cron_minute": "7",
            "cron_hour": "13",
            "cron_day_of_month": "17",
            "cron_month": "4",
            "cron_day_of_week": "*",
            "cron_year": "2026"
        }));

        let schedule = cron_schedule_from_required_tool_args(&args).unwrap();
        assert_eq!(schedule, "0 7 13 17 4 * 2026");
    }

    #[test]
    fn optional_named_cron_fields_reject_partial_updates() {
        let args = object(json!({
            "cron_minute": "0"
        }));

        assert!(optional_cron_schedule_from_tool_args(&args).is_err());
    }

    #[test]
    fn optional_named_cron_fields_ignore_absent_schedule() {
        let args = object(json!({
            "task": "leave timing alone"
        }));

        assert_eq!(optional_cron_schedule_from_tool_args(&args).unwrap(), None);
    }
}

pub(super) fn cron_schedule_from_required_tool_args(
    arguments: &serde_json::Map<String, Value>,
) -> Result<String> {
    build_cron_schedule_from_tool_args(arguments, true)
        .and_then(|schedule| schedule.ok_or_else(|| anyhow!("cron schedule is required")))
}

pub(super) fn optional_cron_schedule_from_tool_args(
    arguments: &serde_json::Map<String, Value>,
) -> Result<Option<String>> {
    build_cron_schedule_from_tool_args(arguments, false)
}

fn build_cron_schedule_from_tool_args(
    arguments: &serde_json::Map<String, Value>,
    required: bool,
) -> Result<Option<String>> {
    let keys = [
        "cron_second",
        "cron_minute",
        "cron_hour",
        "cron_day_of_month",
        "cron_month",
        "cron_day_of_week",
    ];
    let has_required_field = keys.iter().any(|key| arguments.contains_key(*key));
    let has_year = arguments.contains_key("cron_year");
    if !required && !has_required_field && !has_year {
        return Ok(None);
    }

    let mut fields = Vec::with_capacity(7);
    for key in keys {
        fields.push(cron_field_arg_required(arguments, key)?);
    }
    if let Some(year) = optional_cron_field_arg(arguments, "cron_year")? {
        fields.push(year);
    }
    Ok(Some(fields.join(" ")))
}

fn cron_field_arg_required(
    arguments: &serde_json::Map<String, Value>,
    key: &str,
) -> Result<String> {
    let field = string_arg_required(arguments, key)?;
    validate_cron_field_arg(key, field)
}

fn optional_cron_field_arg(
    arguments: &serde_json::Map<String, Value>,
    key: &str,
) -> Result<Option<String>> {
    optional_string_arg(arguments, key)?
        .map(|field| validate_cron_field_arg(key, field))
        .transpose()
}

fn validate_cron_field_arg(key: &str, field: String) -> Result<String> {
    if field.chars().any(char::is_whitespace) {
        return Err(anyhow!(
            "{} must be a single cron field without whitespace",
            key
        ));
    }
    Ok(field)
}

pub(super) fn parse_checker_from_tool_args(
    arguments: &serde_json::Map<String, Value>,
) -> Result<Option<CronCheckerConfig>> {
    let Some(command) = arguments.get("checker_command").and_then(Value::as_str) else {
        return Ok(None);
    };
    let command = command.trim();
    if command.is_empty() {
        return Err(anyhow!("checker_command must not be empty"));
    }
    let timeout_seconds = arguments
        .get("checker_timeout_seconds")
        .and_then(Value::as_f64)
        .unwrap_or(30.0);
    let cwd = arguments
        .get("checker_cwd")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned);
    Ok(Some(CronCheckerConfig {
        command: command.to_string(),
        timeout_seconds,
        cwd,
    }))
}

fn build_shell_command(command: &str) -> Command {
    #[cfg(windows)]
    {
        let shell = std::env::var_os("COMSPEC").unwrap_or_else(|| "cmd.exe".into());
        let mut command_builder = Command::new(shell);
        command_builder.arg("/C").arg(command);
        command_builder
    }
    #[cfg(not(windows))]
    {
        let mut command_builder = Command::new("sh");
        command_builder.arg("-c").arg(command);
        command_builder
    }
}

pub(super) fn evaluate_cron_checker(
    checker: &CronCheckerConfig,
    workspace_root: &Path,
) -> Result<bool> {
    let cwd = checker
        .cwd
        .as_deref()
        .map(PathBuf::from)
        .map(|path| {
            if path.is_absolute() {
                path
            } else {
                workspace_root.join(path)
            }
        })
        .unwrap_or_else(|| workspace_root.to_path_buf());
    let command = checker.command.clone();
    let timeout_seconds = checker.timeout_seconds;
    let (sender, receiver) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let result = build_shell_command(&command)
            .current_dir(&cwd)
            .output()
            .with_context(|| format!("failed to execute checker in {}", cwd.display()))
            .map(|output| output.status.success());
        let _ = sender.send(result);
    });
    receiver
        .recv_timeout(Duration::from_secs_f64(timeout_seconds))
        .map_err(|_| anyhow!("checker timed out after {} seconds", timeout_seconds))?
}

pub(super) fn background_agent_timeout_seconds(model_timeout_seconds: f64) -> f64 {
    model_timeout_seconds + 15.0
}

pub(super) fn is_timeout_like(error: &anyhow::Error) -> bool {
    error.to_string().contains("timed out")
}

pub(super) fn session_errno_for_turn_error(error: &anyhow::Error) -> SessionErrno {
    let error_text = format!("{error:#}").to_ascii_lowercase();
    if error_text.contains("threshold context compaction failed") {
        SessionErrno::ThresholdCompactionFailure
    } else if error_text.contains("tool-wait context compaction failed") {
        SessionErrno::ToolWaitTimeout
    } else if error_text.contains("upstream")
        || error_text.contains("provider")
        || error_text.contains("chat completion")
        || error_text.contains("response body")
    {
        SessionErrno::ApiFailure
    } else {
        SessionErrno::RuntimeFailure
    }
}

pub(super) fn should_attempt_idle_context_compaction(
    session: &SessionSnapshot,
    now: chrono::DateTime<Utc>,
    idle_threshold: Duration,
    estimated_tokens: usize,
    min_tokens: usize,
) -> bool {
    let Some(last_returned_at) = session.last_agent_returned_at else {
        return false;
    };
    if session.turn_count <= session.last_compacted_turn_count {
        return false;
    }
    if estimated_tokens < min_tokens {
        return false;
    }
    let Ok(idle_elapsed) = now.signed_duration_since(last_returned_at).to_std() else {
        return false;
    };
    idle_elapsed > idle_threshold
}

pub(super) fn background_timeout_with_active_children_text(language: &str) -> String {
    let language = language.to_ascii_lowercase();
    if language.starts_with("zh") {
        "后台任务超时了，而且它启动的子任务可能还在收尾，所以系统没有自动重试以避免冲突。请稍后查看结果，或重新发起一个新任务。".to_string()
    } else {
        "The background task timed out, and child agents may still be finishing work, so the system skipped automatic recovery to avoid conflicts. Please check back later or start a new task.".to_string()
    }
}

pub(super) fn user_facing_error_text(language: &str, error: &anyhow::Error) -> String {
    let language = language.to_ascii_lowercase();
    let error_text = format!("{error:#}").to_ascii_lowercase();
    let timeout_like = is_timeout_like(error);
    let upstream_timeout = timeout_like
        && (error_text.contains("upstream")
            || error_text.contains("response body")
            || error_text.contains("chat completion")
            || error_text.contains("operation timed out"));
    let upstream_error = error_text.contains("upstream");
    if language.starts_with("zh") {
        if upstream_timeout {
            "这一轮请求上游模型超时了。通常是模型响应过慢或网络波动导致的。请稍后重试；如果系统提示存在恢复点，可以发送 /continue 从最近稳定状态继续。".to_string()
        } else if upstream_error {
            "这一轮请求上游模型时失败了。请稍后重试；如果系统提示存在恢复点，可以发送 /continue 从最近稳定状态继续。"
                .to_string()
        } else if timeout_like {
            "这一轮处理超时了。请稍后重试；如果系统提示存在恢复点，可以发送 /continue 从最近稳定状态继续。".to_string()
        } else {
            "这一轮处理失败了。请稍后重试；如果系统提示存在恢复点，可以发送 /continue 从最近稳定状态继续。".to_string()
        }
    } else if upstream_timeout {
        "This turn failed because the upstream model request timed out. Please try again; if the system indicates a recovery point is available, send /continue to resume from the last stable state.".to_string()
    } else if upstream_error {
        "This turn failed while calling the upstream model. Please try again; if the system indicates a recovery point is available, send /continue to resume from the last stable state.".to_string()
    } else if timeout_like {
        "This turn timed out. Please try again; if the system indicates a recovery point is available, send /continue to resume from the last stable state.".to_string()
    } else {
        "This turn failed. Please try again; if the system indicates a recovery point is available, send /continue to resume from the last stable state.".to_string()
    }
}

pub(super) fn user_facing_continue_error_text(
    language: &str,
    error: &anyhow::Error,
    progress_summary: &str,
) -> String {
    let language = language.to_ascii_lowercase();
    let error_text = format!("{error:#}").to_ascii_lowercase();
    let reason_summary = summarize_continue_error_reason(error);
    if error_text.contains("tool-wait context compaction failed")
        || error_text.contains("threshold context compaction failed")
    {
        if language.starts_with("zh") {
            return format!(
                "自动上下文压缩失败了，但系统已经保留到最近的稳定位置。\n\n当前进度：{}\n失败原因：{}\n\n发送 /continue 可以从这里继续。",
                progress_summary, reason_summary
            );
        }
        return format!(
            "Automatic context compaction failed, but the session has been preserved at the latest stable point.\n\nProgress so far: {}\nFailure reason: {}\n\nSend /continue to resume from there.",
            progress_summary, reason_summary
        );
    }
    let upstream_like = error_text.contains("upstream")
        || error_text.contains("provider")
        || error_text.contains("chat completion")
        || is_timeout_like(error);
    if language.starts_with("zh") {
        if upstream_like {
            format!(
                "这一轮在调用上游模型时失败了，但系统已经保留到最近的稳定位置。\n\n当前进度：{}\n失败原因：{}\n\n发送 /continue 可以从这里继续。",
                progress_summary, reason_summary
            )
        } else {
            format!(
                "这一轮在完成前失败了，但系统已经保留到最近的稳定位置。\n\n当前进度：{}\n失败原因：{}\n\n发送 /continue 可以尝试继续。",
                progress_summary, reason_summary
            )
        }
    } else if upstream_like {
        format!(
            "This turn failed while calling the upstream model, but the session has been preserved at the latest stable point.\n\nProgress so far: {}\nFailure reason: {}\n\nSend /continue to resume from there.",
            progress_summary, reason_summary
        )
    } else {
        format!(
            "This turn failed before finishing, but the session has been preserved at the latest stable point.\n\nProgress so far: {}\nFailure reason: {}\n\nSend /continue to try resuming from there.",
            progress_summary, reason_summary
        )
    }
}

fn summarize_continue_error_reason(error: &anyhow::Error) -> String {
    let summary = error
        .chain()
        .map(|cause| cause.to_string())
        .map(|text| text.trim().to_string())
        .filter(|text| !text.is_empty())
        .fold(Vec::<String>::new(), |mut acc, text| {
            if acc.last() != Some(&text) {
                acc.push(text);
            }
            acc
        });
    if summary.is_empty() {
        return "unknown error".to_string();
    }
    summary.into_iter().take(3).collect::<Vec<_>>().join(" -> ")
}

pub(super) fn summarize_resume_progress(language: &str, messages: &[ChatMessage]) -> String {
    let language = language.to_ascii_lowercase();
    let zh = language.starts_with("zh");
    let last_user_index = messages
        .iter()
        .rposition(|message| message.role == "user")
        .unwrap_or(0);
    let trailing = &messages[last_user_index.saturating_add(1)..];
    let tool_result_count = trailing
        .iter()
        .filter(|message| message.role == "tool")
        .count();
    let tool_names = trailing
        .iter()
        .filter(|message| message.role == "assistant")
        .filter_map(|message| message.tool_calls.as_ref())
        .flat_map(|tool_calls| {
            tool_calls
                .iter()
                .map(|tool_call| tool_call.function.name.clone())
        })
        .collect::<Vec<_>>();
    let recent_tools = tool_names.iter().rev().take(3).cloned().collect::<Vec<_>>();
    let recent_tools_text = recent_tools
        .into_iter()
        .rev()
        .collect::<Vec<_>>()
        .join(", ");
    let assistant_tool_call_count = trailing
        .iter()
        .filter(|message| message.role == "assistant")
        .filter_map(|message| message.tool_calls.as_ref())
        .map(Vec::len)
        .sum::<usize>();
    let partial_text = trailing
        .iter()
        .filter(|message| message.role == "assistant")
        .filter_map(|message| message.content.as_ref())
        .filter_map(Value::as_str)
        .find(|text| !text.trim().is_empty())
        .map(str::trim)
        .map(|text| text.chars().take(120).collect::<String>());
    if tool_result_count > 0 {
        if zh {
            if recent_tools_text.is_empty() {
                format!("上一轮已经执行到工具阶段，并保留了 {tool_result_count} 条工具结果")
            } else {
                format!(
                    "上一轮已经执行到工具阶段，并保留了 {tool_result_count} 条工具结果；最近工具：{recent_tools_text}"
                )
            }
        } else if recent_tools_text.is_empty() {
            format!(
                "the previous turn already reached tool execution and preserved {tool_result_count} tool result(s)"
            )
        } else {
            format!(
                "the previous turn already reached tool execution and preserved {} tool result(s); recent tools: {}",
                tool_result_count, recent_tools_text
            )
        }
    } else if assistant_tool_call_count > 0 {
        if zh {
            if recent_tools_text.is_empty() {
                format!(
                    "上一轮已经进入工具阶段，并保留了 {assistant_tool_call_count} 个待继续的工具调用"
                )
            } else {
                format!(
                    "上一轮已经进入工具阶段，并保留了 {assistant_tool_call_count} 个待继续的工具调用；最近工具：{recent_tools_text}"
                )
            }
        } else if recent_tools_text.is_empty() {
            format!(
                "the previous turn already planned {assistant_tool_call_count} tool call(s) and was preserved at that stage"
            )
        } else {
            format!(
                "the previous turn already planned {assistant_tool_call_count} tool call(s) and was preserved at that stage; recent tools: {recent_tools_text}"
            )
        }
    } else if let Some(text) = partial_text {
        if zh {
            format!("上一轮已保留部分助手输出：{}", text)
        } else {
            format!(
                "the previous turn preserved partial assistant progress: {}",
                text
            )
        }
    } else {
        if zh {
            "上一轮在助手完成回复前已经保留到最近的稳定位置".to_string()
        } else {
            "the previous turn was preserved before the assistant could finish responding"
                .to_string()
        }
    }
}

pub(super) fn format_session_status(
    language: &str,
    model_key: &str,
    model: &ModelConfig,
    session: &SessionSnapshot,
    effective_api_timeout_seconds: f64,
    timeout_source: &str,
    current_context_estimate: usize,
    current_context_limit: usize,
    current_reasoning_effort: Option<&str>,
    context_compaction_enabled: bool,
    conversation_usage_report: &ConversationUsageReport,
    conversation_pricing: &ConversationPricingBreakdown,
) -> String {
    let conversation_usage = &conversation_usage_report.total;
    let usage = &conversation_usage.usage;
    let today_usage = conversation_usage_report
        .days
        .last()
        .map(|day| (&day.date, &day.usage));
    let today_spend_usd = conversation_usage_report
        .days
        .last()
        .and_then(|day| conversation_pricing.daily_costs.get(&day.date))
        .copied()
        .unwrap_or_default();
    let legacy_usage = &session.cumulative_usage;
    let compaction = &session.cumulative_compaction;
    let cache_read_rate = if usage.input_total_tokens() == 0 {
        0.0
    } else {
        (usage.cache_read_input_tokens() as f64 / usage.input_total_tokens() as f64) * 100.0
    };
    let legacy_cache_read_rate = if legacy_usage.input_total_tokens() == 0 {
        0.0
    } else {
        (legacy_usage.cache_read_input_tokens() as f64 / legacy_usage.input_total_tokens() as f64)
            * 100.0
    };
    let compaction_pricing = estimate_compaction_savings_usd(model, compaction);
    let context_percent = if current_context_limit == 0 {
        0.0
    } else {
        (current_context_estimate as f64 / current_context_limit as f64) * 100.0
    };
    let context_source_label = format!("local estimate, {}", token_estimation_status_label(model));
    let language = language.to_ascii_lowercase();
    if language.starts_with("zh") {
        let mut lines = vec![
            format!("Session: {}", session.id),
            format!("Conversation: {}", session.address.conversation_id),
            format!("Workspace: {}", session.workspace_id),
            format!("Model: {} ({})", model_key, model.model),
            format!(
                "API timeout: {:.1}s ({})",
                effective_api_timeout_seconds, timeout_source
            ),
            format!(
                "Reasoning effort: {}",
                current_reasoning_effort.unwrap_or("default")
            ),
            format!(
                "Automatic context compaction: {}",
                if context_compaction_enabled {
                    "enabled"
                } else {
                    "disabled"
                }
            ),
            format!(
                "Idle compaction error: {}",
                format_idle_compaction_error_status("zh", session)
            ),
            format!("Turns: {}", session.turn_count),
            format!(
                "Current context estimate: {} / {} tokens ({:.1}%, {})",
                current_context_estimate,
                current_context_limit,
                context_percent,
                context_source_label.as_str()
            ),
            String::new(),
            match today_usage {
                Some((date, _usage)) => format!(
                    "今日 Conversation 用量（{}，Asia/Shanghai）：",
                    date.format("%Y-%m-%d")
                ),
                None => "今日 Conversation 用量（Asia/Shanghai）：".to_string(),
            },
            format!("- today_spend_usd: ${:.6}", today_spend_usd),
            format!(
                "- today_input_total_tokens: {}",
                today_usage
                    .map(|(_, usage)| usage.input_total_tokens())
                    .unwrap_or_default()
            ),
            format!(
                "- today_output_total_tokens: {}",
                today_usage
                    .map(|(_, usage)| usage.output_total_tokens())
                    .unwrap_or_default()
            ),
            format!(
                "- today_context_total_tokens: {}",
                today_usage
                    .map(|(_, usage)| usage.context_total_tokens())
                    .unwrap_or_default()
            ),
            format!(
                "- today_llm_calls: {}",
                today_usage.map(|(_, usage)| usage.llm_calls).unwrap_or_default()
            ),
            String::new(),
            format!(
                "最近 {} 天 Conversation 总用量（所有 Agent）：",
                conversation_usage_report.days.len()
            ),
            "- source: token totals use agent turn usage logs; model costs use per-model model-call logs".to_string(),
            format!("- window_spend_usd: ${:.6}", conversation_pricing.total_usd),
            format!("- sessions: {}", conversation_usage.session_count),
            format!("- usage_events: {}", conversation_usage.event_count),
            format!(
                "- model_call_events: {}",
                conversation_usage_report.model_call_event_count
            ),
            format!("- llm_calls: {}", usage.llm_calls),
            format!("- input_total_tokens: {}", usage.input_total_tokens()),
            format!("- output_total_tokens: {}", usage.output_total_tokens()),
            format!("- context_total_tokens: {}", usage.context_total_tokens()),
            format!(
                "- cache_read_input_tokens: {}",
                usage.cache_read_input_tokens()
            ),
            format!(
                "- cache_write_input_tokens: {}",
                usage.cache_write_input_tokens()
            ),
            format!(
                "- cache_uncached_input_tokens: {}",
                usage.cache_uncached_input_tokens()
            ),
            format!(
                "- normal_billed_input_tokens: {}",
                usage.normal_billed_input_tokens()
            ),
            format!("- cache_read_rate: {:.2}%", cache_read_rate),
        ];
        if conversation_usage.missing_cache_breakdown_events > 0 {
            lines.push(format!(
                "- cache_breakdown_note: {} old events did not include cache fields, so token totals are correct but billed-cache split is best-effort.",
                conversation_usage.missing_cache_breakdown_events
            ));
        }
        lines.push(String::new());
        lines.push("金额计算模型分类：".to_string());
        append_model_cost_lines(
            &mut lines,
            "正确计算的模型",
            &conversation_pricing.correctly_priced_models,
        );
        append_model_cost_lines(
            &mut lines,
            "错误计算风险模型（已用 best-effort）",
            &conversation_pricing.risky_priced_models,
        );
        append_model_cost_lines(
            &mut lines,
            "未知模型（已按 GLM 5.1 默认价格估算）",
            &conversation_pricing.unknown_priced_models,
        );
        if !conversation_pricing.pricing_fetch_errors.is_empty() {
            lines.push(format!(
                "- pricing_fetch_errors: {}",
                conversation_pricing.pricing_fetch_errors.join(" | ")
            ));
        }
        lines.push(String::new());
        lines.push("Legacy session cumulative usage（历史累计，仅供排查）：".to_string());
        lines.push("- note: this field may contain older accounting bugs and is no longer used as the /status primary usage total.".to_string());
        lines.push(format!("- llm_calls: {}", legacy_usage.llm_calls));
        lines.push(format!(
            "- input_total_tokens: {}",
            legacy_usage.input_total_tokens()
        ));
        lines.push(format!(
            "- output_total_tokens: {}",
            legacy_usage.output_total_tokens()
        ));
        lines.push(format!(
            "- context_total_tokens: {}",
            legacy_usage.context_total_tokens()
        ));
        lines.push(format!(
            "- cache_read_input_tokens: {}",
            legacy_usage.cache_read_input_tokens()
        ));
        lines.push(format!(
            "- cache_write_input_tokens: {}",
            legacy_usage.cache_write_input_tokens()
        ));
        lines.push(format!(
            "- normal_billed_input_tokens: {}",
            legacy_usage.normal_billed_input_tokens()
        ));
        lines.push(format!("- cache_read_rate: {:.2}%", legacy_cache_read_rate));
        lines.push(String::new());
        lines.push("价格估算：总 token 使用 turn 日志避免历史 cumulative_usage 污染；金额按带 model 字段的 model-call 日志分模型计算，未知模型会明确列出并按 GLM 5.1 默认价格估算。".to_string());
        lines.push(String::new());
        lines.push("累计上下文压缩统计：".to_string());
        lines.push(format!("- compaction_runs: {}", compaction.run_count));
        lines.push(format!(
            "- compacted_runs: {}",
            compaction.compacted_run_count
        ));
        lines.push(format!(
            "- estimated_tokens_before: {}",
            compaction.estimated_tokens_before
        ));
        lines.push(format!(
            "- estimated_tokens_after: {}",
            compaction.estimated_tokens_after
        ));
        lines.push(format!(
            "- estimated_tokens_saved: {}",
            compaction
                .estimated_tokens_before
                .saturating_sub(compaction.estimated_tokens_after)
        ));
        if let Some((formula, gross_usd, compaction_cost_usd, net_usd)) = compaction_pricing {
            lines.push(format!("- formula: {}", formula));
            lines.push(format!(
                "- estimated_cold_start_gross_usd: ${:.6}",
                gross_usd
            ));
            lines.push(format!(
                "- estimated_compaction_cost_usd: ${:.6}",
                compaction_cost_usd
            ));
            lines.push(format!("- estimated_net_usd: ${:.6}", net_usd));
        } else {
            lines.push(
                "- estimated_net_usd: unavailable for the current model pricing table.".to_string(),
            );
        }
        lines.join("\n")
    } else {
        let mut lines = vec![
            format!("Session: {}", session.id),
            format!("Conversation: {}", session.address.conversation_id),
            format!("Workspace: {}", session.workspace_id),
            format!("Model: {} ({})", model_key, model.model),
            format!(
                "API timeout: {:.1}s ({})",
                effective_api_timeout_seconds, timeout_source
            ),
            format!(
                "Reasoning effort: {}",
                current_reasoning_effort.unwrap_or("default")
            ),
            format!(
                "Automatic context compaction: {}",
                if context_compaction_enabled {
                    "enabled"
                } else {
                    "disabled"
                }
            ),
            format!(
                "Idle compaction error: {}",
                format_idle_compaction_error_status("en", session)
            ),
            format!("Turns: {}", session.turn_count),
            format!(
                "Current context estimate: {} / {} tokens ({:.1}%, {})",
                current_context_estimate,
                current_context_limit,
                context_percent,
                context_source_label.as_str()
            ),
            String::new(),
            match today_usage {
                Some((date, _usage)) => format!(
                    "Today conversation usage ({}, Asia/Shanghai):",
                    date.format("%Y-%m-%d")
                ),
                None => "Today conversation usage (Asia/Shanghai):".to_string(),
            },
            format!("- today_spend_usd: ${:.6}", today_spend_usd),
            format!(
                "- today_input_total_tokens: {}",
                today_usage
                    .map(|(_, usage)| usage.input_total_tokens())
                    .unwrap_or_default()
            ),
            format!(
                "- today_output_total_tokens: {}",
                today_usage
                    .map(|(_, usage)| usage.output_total_tokens())
                    .unwrap_or_default()
            ),
            format!(
                "- today_context_total_tokens: {}",
                today_usage
                    .map(|(_, usage)| usage.context_total_tokens())
                    .unwrap_or_default()
            ),
            format!(
                "- today_llm_calls: {}",
                today_usage.map(|(_, usage)| usage.llm_calls).unwrap_or_default()
            ),
            String::new(),
            format!(
                "Last {} days conversation usage (all agents):",
                conversation_usage_report.days.len()
            ),
            "- source: token totals use agent turn usage logs; model costs use per-model model-call logs".to_string(),
            format!("- window_spend_usd: ${:.6}", conversation_pricing.total_usd),
            format!("- sessions: {}", conversation_usage.session_count),
            format!("- usage_events: {}", conversation_usage.event_count),
            format!(
                "- model_call_events: {}",
                conversation_usage_report.model_call_event_count
            ),
            format!("- llm_calls: {}", usage.llm_calls),
            format!("- input_total_tokens: {}", usage.input_total_tokens()),
            format!("- output_total_tokens: {}", usage.output_total_tokens()),
            format!("- context_total_tokens: {}", usage.context_total_tokens()),
            format!(
                "- cache_read_input_tokens: {}",
                usage.cache_read_input_tokens()
            ),
            format!(
                "- cache_write_input_tokens: {}",
                usage.cache_write_input_tokens()
            ),
            format!(
                "- cache_uncached_input_tokens: {}",
                usage.cache_uncached_input_tokens()
            ),
            format!(
                "- normal_billed_input_tokens: {}",
                usage.normal_billed_input_tokens()
            ),
            format!("- cache_read_rate: {:.2}%", cache_read_rate),
        ];
        if conversation_usage.missing_cache_breakdown_events > 0 {
            lines.push(format!(
                "- cache_breakdown_note: {} old events did not include cache fields, so token totals are correct but billed-cache split is best-effort.",
                conversation_usage.missing_cache_breakdown_events
            ));
        }
        lines.push(String::new());
        lines.push("Model pricing classification:".to_string());
        append_model_cost_lines(
            &mut lines,
            "Correctly priced models",
            &conversation_pricing.correctly_priced_models,
        );
        append_model_cost_lines(
            &mut lines,
            "Risky/best-effort models",
            &conversation_pricing.risky_priced_models,
        );
        append_model_cost_lines(
            &mut lines,
            "Unknown models (GLM 5.1 fallback used)",
            &conversation_pricing.unknown_priced_models,
        );
        if !conversation_pricing.pricing_fetch_errors.is_empty() {
            lines.push(format!(
                "- pricing_fetch_errors: {}",
                conversation_pricing.pricing_fetch_errors.join(" | ")
            ));
        }
        lines.push(String::new());
        lines.push("Legacy session cumulative usage (debug only):".to_string());
        lines.push("- note: this field may contain older accounting bugs and is no longer used as the /status primary usage total.".to_string());
        lines.push(format!("- llm_calls: {}", legacy_usage.llm_calls));
        lines.push(format!(
            "- input_total_tokens: {}",
            legacy_usage.input_total_tokens()
        ));
        lines.push(format!(
            "- output_total_tokens: {}",
            legacy_usage.output_total_tokens()
        ));
        lines.push(format!(
            "- context_total_tokens: {}",
            legacy_usage.context_total_tokens()
        ));
        lines.push(format!(
            "- cache_read_input_tokens: {}",
            legacy_usage.cache_read_input_tokens()
        ));
        lines.push(format!(
            "- cache_write_input_tokens: {}",
            legacy_usage.cache_write_input_tokens()
        ));
        lines.push(format!(
            "- normal_billed_input_tokens: {}",
            legacy_usage.normal_billed_input_tokens()
        ));
        lines.push(format!("- cache_read_rate: {:.2}%", legacy_cache_read_rate));
        lines.push(String::new());
        lines.push("Estimated cost: token totals avoid legacy cumulative_usage pollution; spend is priced per model-call log, and unknown models are explicitly listed with the GLM 5.1 fallback price.".to_string());
        lines.push(String::new());
        lines.push("Cumulative context compaction stats:".to_string());
        lines.push(format!("- compaction_runs: {}", compaction.run_count));
        lines.push(format!(
            "- compacted_runs: {}",
            compaction.compacted_run_count
        ));
        lines.push(format!(
            "- estimated_tokens_before: {}",
            compaction.estimated_tokens_before
        ));
        lines.push(format!(
            "- estimated_tokens_after: {}",
            compaction.estimated_tokens_after
        ));
        lines.push(format!(
            "- estimated_tokens_saved: {}",
            compaction
                .estimated_tokens_before
                .saturating_sub(compaction.estimated_tokens_after)
        ));
        if let Some((formula, gross_usd, compaction_cost_usd, net_usd)) = compaction_pricing {
            lines.push(format!("- formula: {}", formula));
            lines.push(format!(
                "- estimated_cold_start_gross_usd: ${:.6}",
                gross_usd
            ));
            lines.push(format!(
                "- estimated_compaction_cost_usd: ${:.6}",
                compaction_cost_usd
            ));
            lines.push(format!("- estimated_net_usd: ${:.6}", net_usd));
        } else {
            lines.push(
                "- estimated_net_usd: unavailable for the current model pricing table.".to_string(),
            );
        }
        lines.join("\n")
    }
}

fn append_model_cost_lines(lines: &mut Vec<String>, label: &str, models: &[ModelCostSummary]) {
    if models.is_empty() {
        lines.push(format!("- {}: none", label));
        return;
    }
    let rendered = models
        .iter()
        .map(|model| {
            format!(
                "{} ${:.6} (input={}, output={}, calls={}, source={}, note={})",
                model.model,
                model.total_usd,
                model.input_tokens,
                model.output_tokens,
                model.event_count,
                model.pricing_source,
                model.note
            )
        })
        .collect::<Vec<_>>()
        .join("; ");
    lines.push(format!("- {}: {}", label, rendered));
}

#[derive(Clone, Debug, Default)]
pub(super) struct ConversationUsageWindow {
    pub usage: TokenUsage,
    pub event_count: u64,
    pub session_count: usize,
    pub missing_cache_breakdown_events: u64,
}

#[derive(Clone, Debug, Default)]
pub(super) struct ConversationUsageReport {
    pub days: Vec<ConversationDailyUsage>,
    pub total: ConversationUsageWindow,
    pub model_usages: BTreeMap<String, ConversationModelUsage>,
    pub model_call_event_count: u64,
}

#[derive(Clone, Debug)]
pub(super) struct ConversationDailyUsage {
    pub date: chrono::NaiveDate,
    pub usage: TokenUsage,
    pub event_count: u64,
    pub missing_cache_breakdown_events: u64,
}

#[derive(Clone, Debug, Default)]
pub(super) struct ConversationModelUsage {
    pub usage: TokenUsage,
    pub daily_usage: BTreeMap<chrono::NaiveDate, TokenUsage>,
    pub event_count: u64,
    pub missing_cache_breakdown_events: u64,
}

#[derive(Clone, Debug, Default)]
pub(super) struct ConversationPricingBreakdown {
    pub total_usd: f64,
    pub daily_costs: BTreeMap<chrono::NaiveDate, f64>,
    pub correctly_priced_models: Vec<ModelCostSummary>,
    pub risky_priced_models: Vec<ModelCostSummary>,
    pub unknown_priced_models: Vec<ModelCostSummary>,
    pub pricing_fetch_errors: Vec<String>,
}

#[derive(Clone, Debug)]
pub(super) struct ModelCostSummary {
    pub model: String,
    pub total_usd: f64,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub event_count: u64,
    pub pricing_source: String,
    pub note: String,
}

pub(super) fn collect_conversation_usage_report(
    workdir: &Path,
    address: &ChannelAddress,
    now: chrono::DateTime<Utc>,
    day_count: usize,
) -> ConversationUsageReport {
    let day_count = day_count.max(1);
    let session_ids = conversation_session_ids_from_workdir(workdir, address);
    let today = now.with_timezone(&chrono_tz::Asia::Shanghai).date_naive();
    let first_day = today
        .checked_sub_days(chrono::Days::new(day_count.saturating_sub(1) as u64))
        .unwrap_or(today);
    let days = (0..day_count)
        .filter_map(|offset| first_day.checked_add_days(chrono::Days::new(offset as u64)))
        .map(|date| ConversationDailyUsage {
            date,
            usage: TokenUsage::default(),
            event_count: 0,
            missing_cache_breakdown_events: 0,
        })
        .collect::<Vec<_>>();
    let mut report = ConversationUsageReport {
        total: ConversationUsageWindow {
            session_count: session_ids.len(),
            ..ConversationUsageWindow::default()
        },
        days,
        ..ConversationUsageReport::default()
    };
    if session_ids.is_empty() {
        return report;
    }
    let Some(start_local) = first_day.and_hms_opt(0, 0, 0) else {
        return report;
    };
    let Some(start_ms) = start_local
        .and_local_timezone(chrono_tz::Asia::Shanghai)
        .single()
        .map(|value| value.with_timezone(&Utc).timestamp_millis())
    else {
        return report;
    };
    let logs_dir = workdir.join("logs").join("agents");
    let Ok(entries) = fs::read_dir(&logs_dir) else {
        return report;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|ext| ext.to_str()) != Some("jsonl") {
            continue;
        }
        let Ok(file) = fs::File::open(&path) else {
            continue;
        };
        let reader = std::io::BufReader::new(file);
        for line in std::io::BufRead::lines(reader).map_while(std::result::Result::ok) {
            let Ok(value) = serde_json::from_str::<Value>(&line) else {
                continue;
            };
            let Some(ts_ms) = event_timestamp_ms(&value) else {
                continue;
            };
            if ts_ms < start_ms {
                continue;
            }
            if value.get("channel_id").and_then(Value::as_str) != Some(address.channel_id.as_str())
            {
                continue;
            }
            let Some(session_id) = value.get("session_id").and_then(Value::as_str) else {
                continue;
            };
            if !session_ids.contains(session_id) {
                continue;
            }
            let Some(date) = chrono::DateTime::<Utc>::from_timestamp_millis(ts_ms)
                .map(|value| value.with_timezone(&chrono_tz::Asia::Shanghai).date_naive())
            else {
                continue;
            };
            if date < first_day || date > today {
                continue;
            }
            match value.get("kind").and_then(Value::as_str) {
                Some("turn_token_usage") => {
                    let event_usage = token_usage_from_log_event(&value);
                    let has_cache_breakdown = event_has_cache_breakdown(&value);
                    if let Some(day) = report.days.iter_mut().find(|day| day.date == date) {
                        day.usage.add_assign(&event_usage);
                        day.event_count = day.event_count.saturating_add(1);
                        if !has_cache_breakdown {
                            day.missing_cache_breakdown_events =
                                day.missing_cache_breakdown_events.saturating_add(1);
                        }
                    }
                    report.total.usage.add_assign(&event_usage);
                    report.total.event_count = report.total.event_count.saturating_add(1);
                    if !has_cache_breakdown {
                        report.total.missing_cache_breakdown_events = report
                            .total
                            .missing_cache_breakdown_events
                            .saturating_add(1);
                    }
                }
                Some("agent_frame_model_call_completed") => {
                    let model = value
                        .get("model")
                        .and_then(Value::as_str)
                        .filter(|value| !value.trim().is_empty())
                        .unwrap_or("unknown")
                        .to_string();
                    let event_usage = token_usage_from_log_event(&value);
                    let entry = report.model_usages.entry(model).or_default();
                    entry.usage.add_assign(&event_usage);
                    entry
                        .daily_usage
                        .entry(date)
                        .or_insert_with(TokenUsage::default)
                        .add_assign(&event_usage);
                    entry.event_count = entry.event_count.saturating_add(1);
                    if !event_has_cache_breakdown(&value) {
                        entry.missing_cache_breakdown_events =
                            entry.missing_cache_breakdown_events.saturating_add(1);
                    }
                    report.model_call_event_count = report.model_call_event_count.saturating_add(1);
                }
                _ => {}
            }
        }
    }
    report.days.sort_by_key(|day| day.date);
    report
}

pub(super) fn price_conversation_usage_report(
    report: &ConversationUsageReport,
    pricing_by_model: &HashMap<String, ModelPricing>,
    pricing_fetch_errors: Vec<String>,
) -> ConversationPricingBreakdown {
    let fallback_pricing = glm_5_1_fallback_pricing();
    let mut breakdown = ConversationPricingBreakdown {
        pricing_fetch_errors,
        ..ConversationPricingBreakdown::default()
    };
    for day in &report.days {
        breakdown.daily_costs.insert(day.date, 0.0);
    }
    for (model, usage) in &report.model_usages {
        let (pricing, unknown_pricing) = match pricing_by_model.get(model) {
            Some(pricing) => (pricing.clone(), false),
            None => (fallback_pricing.clone(), true),
        };
        let total_usd = cost_usd_for_usage(&pricing, &usage.usage);
        breakdown.total_usd += total_usd;
        for (date, daily_usage) in &usage.daily_usage {
            *breakdown.daily_costs.entry(*date).or_insert(0.0) +=
                cost_usd_for_usage(&pricing, daily_usage);
        }
        let summary = ModelCostSummary {
            model: model.clone(),
            total_usd,
            input_tokens: usage.usage.input_total_tokens(),
            output_tokens: usage.usage.output_total_tokens(),
            event_count: usage.event_count,
            pricing_source: pricing.source.clone(),
            note: if unknown_pricing {
                "pricing unknown; GLM 5.1 fallback pricing used".to_string()
            } else if usage.missing_cache_breakdown_events > 0 {
                format!(
                    "{} model-call events missed cache fields; cost is best-effort",
                    usage.missing_cache_breakdown_events
                )
            } else {
                "priced from per-model model-call logs".to_string()
            },
        };
        if unknown_pricing {
            breakdown.unknown_priced_models.push(summary);
        } else if usage.missing_cache_breakdown_events > 0 {
            breakdown.risky_priced_models.push(summary);
        } else {
            breakdown.correctly_priced_models.push(summary);
        }
    }
    let mut unattributed_usage = TokenUsage::default();
    for day in &report.days {
        let mut attributed = TokenUsage::default();
        for usage in report.model_usages.values() {
            if let Some(daily_usage) = usage.daily_usage.get(&day.date) {
                attributed.add_assign(daily_usage);
            }
        }
        let unattributed = subtract_token_usage_floor(&day.usage, &attributed);
        if token_usage_has_billable_tokens(&unattributed) {
            let daily_cost = cost_usd_for_usage(&fallback_pricing, &unattributed);
            *breakdown.daily_costs.entry(day.date).or_insert(0.0) += daily_cost;
            breakdown.total_usd += daily_cost;
            unattributed_usage.add_assign(&unattributed);
        }
    }
    if token_usage_has_billable_tokens(&unattributed_usage) {
        breakdown.unknown_priced_models.push(ModelCostSummary {
            model: "unattributed_turn_usage".to_string(),
            total_usd: cost_usd_for_usage(&fallback_pricing, &unattributed_usage),
            input_tokens: unattributed_usage.input_total_tokens(),
            output_tokens: unattributed_usage.output_total_tokens(),
            event_count: report.total.event_count,
            pricing_source: fallback_pricing.source,
            note: "turn usage had no matching per-model model-call logs; GLM 5.1 fallback pricing used".to_string(),
        });
    }
    breakdown
}

fn token_usage_has_billable_tokens(usage: &TokenUsage) -> bool {
    usage.input_total_tokens() > 0 || usage.output_total_tokens() > 0
}

fn subtract_token_usage_floor(left: &TokenUsage, right: &TokenUsage) -> TokenUsage {
    TokenUsage {
        llm_calls: left.llm_calls.saturating_sub(right.llm_calls),
        prompt_tokens: left.prompt_tokens.saturating_sub(right.prompt_tokens),
        completion_tokens: left
            .completion_tokens
            .saturating_sub(right.completion_tokens),
        total_tokens: left.total_tokens.saturating_sub(right.total_tokens),
        cache_hit_tokens: left.cache_hit_tokens.saturating_sub(right.cache_hit_tokens),
        cache_miss_tokens: left
            .cache_miss_tokens
            .saturating_sub(right.cache_miss_tokens),
        cache_read_tokens: left
            .cache_read_tokens
            .saturating_sub(right.cache_read_tokens),
        cache_write_tokens: left
            .cache_write_tokens
            .saturating_sub(right.cache_write_tokens),
    }
}

#[cfg(test)]
pub(super) fn collect_conversation_usage_window(
    workdir: &Path,
    address: &ChannelAddress,
    now: chrono::DateTime<Utc>,
    window: chrono::Duration,
) -> ConversationUsageWindow {
    let session_ids = conversation_session_ids_from_workdir(workdir, address);
    let mut usage = ConversationUsageWindow {
        session_count: session_ids.len(),
        ..ConversationUsageWindow::default()
    };
    if session_ids.is_empty() {
        return usage;
    }
    let cutoff_ms = now
        .checked_sub_signed(window)
        .unwrap_or(now)
        .timestamp_millis();
    let logs_dir = workdir.join("logs").join("agents");
    let Ok(entries) = fs::read_dir(&logs_dir) else {
        return usage;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|ext| ext.to_str()) != Some("jsonl") {
            continue;
        }
        let Ok(file) = fs::File::open(&path) else {
            continue;
        };
        let reader = std::io::BufReader::new(file);
        for line in std::io::BufRead::lines(reader).map_while(std::result::Result::ok) {
            let Ok(value) = serde_json::from_str::<Value>(&line) else {
                continue;
            };
            if value.get("kind").and_then(Value::as_str) != Some("turn_token_usage") {
                continue;
            }
            if event_timestamp_ms(&value).is_none_or(|ts| ts < cutoff_ms) {
                continue;
            }
            if value.get("channel_id").and_then(Value::as_str) != Some(address.channel_id.as_str())
            {
                continue;
            }
            let Some(session_id) = value.get("session_id").and_then(Value::as_str) else {
                continue;
            };
            if !session_ids.contains(session_id) {
                continue;
            }
            if !event_has_cache_breakdown(&value) {
                usage.missing_cache_breakdown_events =
                    usage.missing_cache_breakdown_events.saturating_add(1);
            }
            usage.usage.add_assign(&token_usage_from_log_event(&value));
            usage.event_count = usage.event_count.saturating_add(1);
        }
    }
    usage
}

fn conversation_session_ids_from_workdir(
    workdir: &Path,
    address: &ChannelAddress,
) -> HashSet<String> {
    let sessions_dir = workdir.join("sessions");
    let Ok(session_roots) = crate::session::find_session_roots(&sessions_dir) else {
        return HashSet::new();
    };
    session_roots
        .into_iter()
        .filter_map(|root| {
            let path = root.join("session.json");
            let raw = fs::read_to_string(path).ok()?;
            let value = serde_json::from_str::<Value>(&raw).ok()?;
            let object_address = value.get("address")?.as_object()?;
            let channel_id = object_address.get("channel_id")?.as_str()?;
            let conversation_id = object_address.get("conversation_id")?.as_str()?;
            if channel_id != address.channel_id || conversation_id != address.conversation_id {
                return None;
            }
            value.get("id")?.as_str().map(ToOwned::to_owned)
        })
        .collect()
}

fn event_timestamp_ms(value: &Value) -> Option<i64> {
    let ts = value.get("ts")?.as_i64()?;
    if ts > 10_000_000_000 {
        Some(ts)
    } else {
        Some(ts.saturating_mul(1000))
    }
}

fn event_has_cache_breakdown(value: &Value) -> bool {
    [
        "cache_read_input_tokens",
        "cache_write_input_tokens",
        "cache_uncached_input_tokens",
        "cache_read_tokens",
        "cache_write_tokens",
        "cache_miss_tokens",
        "legacy_cache_read_tokens",
        "legacy_cache_write_tokens",
        "legacy_cache_miss_tokens",
    ]
    .iter()
    .any(|key| value.get(*key).is_some())
}

fn first_u64_field(value: &Value, keys: &[&str]) -> Option<u64> {
    keys.iter().find_map(|key| value.get(*key)?.as_u64())
}

fn token_usage_from_log_event(value: &Value) -> TokenUsage {
    let prompt_tokens = first_u64_field(
        value,
        &[
            "input_total_tokens",
            "prompt_tokens",
            "legacy_prompt_tokens",
        ],
    )
    .unwrap_or(0);
    let completion_tokens = first_u64_field(
        value,
        &[
            "output_total_tokens",
            "completion_tokens",
            "legacy_completion_tokens",
        ],
    )
    .unwrap_or(0);
    let total_tokens = first_u64_field(
        value,
        &[
            "context_total_tokens",
            "total_tokens",
            "legacy_total_tokens",
        ],
    )
    .unwrap_or_else(|| prompt_tokens.saturating_add(completion_tokens));
    let cache_read_tokens = first_u64_field(
        value,
        &[
            "cache_read_input_tokens",
            "cache_read_tokens",
            "legacy_cache_read_tokens",
        ],
    )
    .unwrap_or(0);
    let cache_write_tokens = first_u64_field(
        value,
        &[
            "cache_write_input_tokens",
            "cache_write_tokens",
            "legacy_cache_write_tokens",
        ],
    )
    .unwrap_or(0);
    let cache_miss_tokens = first_u64_field(
        value,
        &[
            "cache_uncached_input_tokens",
            "cache_miss_tokens",
            "legacy_cache_miss_tokens",
        ],
    )
    .unwrap_or_else(|| prompt_tokens.saturating_sub(cache_read_tokens));
    let cache_hit_tokens = first_u64_field(value, &["cache_hit_tokens", "legacy_cache_hit_tokens"])
        .unwrap_or(cache_read_tokens);
    TokenUsage {
        llm_calls: first_u64_field(value, &["llm_calls"]).unwrap_or(1),
        prompt_tokens,
        completion_tokens,
        total_tokens,
        cache_hit_tokens,
        cache_miss_tokens,
        cache_read_tokens,
        cache_write_tokens,
    }
}

fn token_estimation_status_label(model: &ModelConfig) -> String {
    let Some(config) = model.token_estimation.as_ref() else {
        return format!(
            "template=builtin, tokenizer={}",
            token_estimator_label_for_model(&model.model)
        );
    };
    let template = match config.template.as_ref() {
        Some(TokenEstimationTemplateConfig::Builtin) => "builtin".to_string(),
        Some(TokenEstimationTemplateConfig::Local { .. }) => "local".to_string(),
        Some(TokenEstimationTemplateConfig::Huggingface { repo, .. }) => {
            format!("huggingface:{repo}")
        }
        None if config.source == Some(TokenEstimationSource::Huggingface) => config
            .repo
            .as_deref()
            .map(|repo| format!("huggingface:{repo}"))
            .unwrap_or_else(|| "huggingface".to_string()),
        None => "builtin".to_string(),
    };
    let tokenizer = match config.tokenizer.as_ref() {
        Some(TokenEstimationTokenizerConfig::Tiktoken { encoding }) => {
            format!("tiktoken:{encoding:?}")
        }
        Some(TokenEstimationTokenizerConfig::Local { .. }) => "local".to_string(),
        Some(TokenEstimationTokenizerConfig::Huggingface { repo, .. }) => {
            format!("huggingface:{repo}")
        }
        None if config.source == Some(TokenEstimationSource::Huggingface) => config
            .repo
            .as_deref()
            .map(|repo| format!("huggingface:{repo}"))
            .unwrap_or_else(|| "huggingface".to_string()),
        None => token_estimator_label_for_model(&model.model).to_string(),
    };
    let calibration = prompt_token_calibration_for_model(&model.model)
        .map(|(ratio, samples)| format!(", calibration={ratio:.3}x/{samples} samples"))
        .unwrap_or_default();
    format!("template={template}, tokenizer={tokenizer}{calibration}")
}

fn format_idle_compaction_error_status(language: &str, session: &SessionSnapshot) -> String {
    if session.session_state.errno != Some(SessionErrno::IdleCompactionFailure) {
        return if language.starts_with("zh") {
            "无".to_string()
        } else {
            "none".to_string()
        };
    }

    let summary = summarize_idle_compaction_error(session.session_state.errinfo.as_deref());
    if summary.is_empty() {
        if language.starts_with("zh") {
            "失败，原因未知".to_string()
        } else {
            "failed, reason unknown".to_string()
        }
    } else {
        summary
    }
}

fn summarize_idle_compaction_error(error_summary: Option<&str>) -> String {
    const MAX_CHARS: usize = 120;

    let normalized = error_summary
        .unwrap_or_default()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    if normalized.chars().count() <= MAX_CHARS {
        return normalized;
    }

    normalized.chars().take(MAX_CHARS).collect::<String>() + "..."
}

#[derive(Clone, Debug)]
pub(super) struct ModelPricing {
    pub input_per_million: f64,
    pub output_per_million: f64,
    pub cache_read_per_million: Option<f64>,
    pub cache_write_per_million: Option<f64>,
    pub source: String,
}

fn model_pricing(model: &ModelConfig) -> Option<ModelPricing> {
    model_pricing_by_id(&model.model)
}

pub(super) fn model_pricing_by_id(model_id: &str) -> Option<ModelPricing> {
    match (model_id, model_id.trim_end_matches(":nitro")) {
        ("anthropic/claude-opus-4.6", _) => Some(ModelPricing {
            input_per_million: 5.0,
            output_per_million: 25.0,
            cache_read_per_million: Some(0.5),
            cache_write_per_million: Some(6.25),
            source: "builtin".to_string(),
        }),
        ("anthropic/claude-sonnet-4.6", _) => Some(ModelPricing {
            input_per_million: 3.0,
            output_per_million: 15.0,
            cache_read_per_million: Some(0.3),
            cache_write_per_million: Some(3.75),
            source: "builtin".to_string(),
        }),
        ("qwen/qwen3.5-27b", _) => Some(ModelPricing {
            input_per_million: 0.195,
            output_per_million: 1.56,
            cache_read_per_million: None,
            cache_write_per_million: None,
            source: "builtin".to_string(),
        }),
        (_, "z-ai/glm-5.1") => Some(glm_5_1_fallback_pricing()),
        _ => None,
    }
}

pub(super) fn glm_5_1_fallback_pricing() -> ModelPricing {
    ModelPricing {
        input_per_million: 0.95,
        output_per_million: 3.15,
        cache_read_per_million: Some(0.475),
        cache_write_per_million: None,
        source: "fallback_glm_5_1_openrouter".to_string(),
    }
}

pub(super) fn cost_usd_for_usage(pricing: &ModelPricing, usage: &TokenUsage) -> f64 {
    let cache_read_per_million = pricing
        .cache_read_per_million
        .unwrap_or(pricing.input_per_million * 0.1);
    let cache_write_per_million = pricing
        .cache_write_per_million
        .unwrap_or(pricing.input_per_million * 1.25);
    (usage.cache_read_input_tokens() as f64 / 1_000_000.0) * cache_read_per_million
        + (usage.cache_write_input_tokens() as f64 / 1_000_000.0) * cache_write_per_million
        + (usage.normal_billed_input_tokens() as f64 / 1_000_000.0) * pricing.input_per_million
        + (usage.output_total_tokens() as f64 / 1_000_000.0) * pricing.output_per_million
}

pub(super) fn model_pricing_from_openrouter(
    input_per_token: f64,
    output_per_token: f64,
    cache_read_per_token: Option<f64>,
    cache_write_per_token: Option<f64>,
) -> ModelPricing {
    ModelPricing {
        input_per_million: input_per_token * 1_000_000.0,
        output_per_million: output_per_token * 1_000_000.0,
        cache_read_per_million: cache_read_per_token.map(|value| value * 1_000_000.0),
        cache_write_per_million: cache_write_per_token.map(|value| value * 1_000_000.0),
        source: "openrouter_models_api".to_string(),
    }
}

pub(super) fn merge_builtin_pricing(
    model_ids: impl IntoIterator<Item = String>,
    fetched: &mut HashMap<String, ModelPricing>,
) {
    for model_id in model_ids {
        if fetched.contains_key(&model_id) {
            continue;
        }
        if let Some(pricing) = model_pricing_by_id(&model_id) {
            fetched.insert(model_id, pricing);
        }
    }
}

pub(super) fn automatic_anthropic_cache_ttl(model: &ModelConfig) -> Option<String> {
    if !supports_automatic_anthropic_cache(model) {
        return None;
    }
    Some(model.cache_ttl.clone().unwrap_or_else(|| "5m".to_string()))
}

pub(super) fn automatic_anthropic_cache_control(
    model: &ModelConfig,
) -> Option<agent_frame::config::CacheControlConfig> {
    if !supports_automatic_anthropic_cache(model) {
        return None;
    }
    automatic_anthropic_cache_ttl(model).map(|ttl| agent_frame::config::CacheControlConfig {
        cache_type: "ephemeral".to_string(),
        ttl: Some(ttl),
    })
}

fn supports_automatic_anthropic_cache(model: &ModelConfig) -> bool {
    match model.model_type {
        crate::config::ModelType::Openrouter | crate::config::ModelType::OpenrouterResp => {
            model.api_endpoint.contains("openrouter.ai")
                && model.model.starts_with("anthropic/claude-")
        }
        crate::config::ModelType::ClaudeCode => model.model.starts_with("claude-"),
        crate::config::ModelType::BraveSearch => false,
        crate::config::ModelType::CodexSubscription => false,
    }
}

pub(super) fn estimate_cost_usd(model: &ModelConfig, usage: &TokenUsage) -> Option<(String, f64)> {
    let pricing = model_pricing(model)?;
    let input_per_million = pricing.input_per_million;
    let output_per_million = pricing.output_per_million;
    let cache_read_per_million = input_per_million * 0.1;
    let cache_write_per_million = input_per_million * 1.25;
    let normal_billed_input_tokens = usage.normal_billed_input_tokens();
    let total_usd = (usage.cache_read_input_tokens() as f64 / 1_000_000.0) * cache_read_per_million
        + (usage.cache_write_input_tokens() as f64 / 1_000_000.0) * cache_write_per_million
        + (normal_billed_input_tokens as f64 / 1_000_000.0) * input_per_million
        + (usage.output_total_tokens() as f64 / 1_000_000.0) * output_per_million;
    let formula = format!(
        "cache_read_input_tokens * ${cache_read_per_million:.6}/1M + cache_write_input_tokens * ${cache_write_per_million:.6}/1M + normal_billed_input_tokens * ${input_per_million:.6}/1M + output_total_tokens * ${output_per_million:.6}/1M"
    );
    Some((formula, total_usd))
}

pub(super) fn estimate_compaction_savings_usd(
    model: &ModelConfig,
    compaction: &SessionCompactionStats,
) -> Option<(String, f64, f64, f64)> {
    let pricing = model_pricing(model)?;
    let saved_tokens = compaction
        .estimated_tokens_before
        .saturating_sub(compaction.estimated_tokens_after);
    let cold_start_gross_usd = (saved_tokens as f64 / 1_000_000.0) * pricing.input_per_million;
    let (_, compaction_cost_usd) = estimate_cost_usd(model, &compaction.usage)?;
    let net_usd = cold_start_gross_usd - compaction_cost_usd;
    let formula = format!(
        "(estimated_tokens_before - estimated_tokens_after) * ${:.6}/1M - compaction_run_cost",
        pricing.input_per_million
    );
    Some((formula, cold_start_gross_usd, compaction_cost_usd, net_usd))
}

pub(super) fn compaction_stats_from_report(
    report: &agent_frame::ContextCompactionReport,
) -> SessionCompactionStats {
    let mut stats = SessionCompactionStats::default();
    stats.run_count = 1;
    stats.compacted_run_count = u64::from(report.compacted);
    stats.estimated_tokens_before = report.estimated_tokens_before as u64;
    stats.estimated_tokens_after = report.estimated_tokens_after as u64;
    stats.usage = report.usage.clone();
    stats
}

pub(super) fn default_prompt_cache_retention(
    cache_ttl: Option<&str>,
    _model: &ModelConfig,
) -> Option<String> {
    cache_ttl.map(str::to_string)
}

pub(super) fn effective_reasoning_config(
    model: &ModelConfig,
    conversation_effort: Option<&str>,
) -> Option<agent_frame::config::ReasoningConfig> {
    let mut reasoning = model.reasoning.clone().unwrap_or_default();
    if let Some(effort) = conversation_effort {
        reasoning.effort = Some(effort.to_string());
    }
    if reasoning.effort.is_none()
        && reasoning.max_tokens.is_none()
        && reasoning.exclude.is_none()
        && reasoning.enabled.is_none()
    {
        None
    } else {
        Some(reasoning)
    }
}

pub(super) fn estimate_current_context_tokens_for_session(
    runtime: &AgentRuntimeView,
    session: &SessionSnapshot,
    model_key: &str,
) -> Result<usize> {
    let frame_config = runtime.build_agent_frame_config(
        session,
        &session.workspace_root,
        AgentPromptKind::MainForeground,
        model_key,
        None,
    )?;
    let extra_tools = runtime.build_extra_tools(
        session,
        AgentPromptKind::MainForeground,
        session.agent_id,
        None,
    );
    estimate_configured_session_tokens(session.request_messages(), "", frame_config, extra_tools)
}

pub(super) fn effective_context_window_limit_for_session(
    session: &SessionSnapshot,
    model: &ModelConfig,
) -> usize {
    let _ = session;
    model.context_window_tokens
}

pub(super) fn replace_directory_contents(target: &Path, source: &Path) -> Result<()> {
    if target.exists() {
        std::fs::remove_dir_all(target)
            .with_context(|| format!("failed to clear {}", target.display()))?;
    }
    copy_dir_recursive(source, target)
}

fn copy_dir_recursive(source: &Path, target: &Path) -> Result<()> {
    std::fs::create_dir_all(target)
        .with_context(|| format!("failed to create {}", target.display()))?;
    for entry in
        std::fs::read_dir(source).with_context(|| format!("failed to read {}", source.display()))?
    {
        let entry = entry?;
        let source_path = entry.path();
        let target_path = target.join(entry.file_name());
        let file_type = entry
            .file_type()
            .with_context(|| format!("failed to inspect {}", source_path.display()))?;
        if file_type.is_dir() {
            copy_dir_recursive(&source_path, &target_path)?;
        } else if file_type.is_symlink() {
            let link_target = std::fs::read_link(&source_path)
                .with_context(|| format!("failed to read link {}", source_path.display()))?;
            create_symlink(&link_target, &target_path)?;
        } else {
            std::fs::copy(&source_path, &target_path).with_context(|| {
                format!(
                    "failed to copy {} to {}",
                    source_path.display(),
                    target_path.display()
                )
            })?;
        }
    }
    Ok(())
}

fn create_symlink(source: &Path, target: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(source, target)
            .with_context(|| format!("failed to create symlink {}", target.display()))
    }
    #[cfg(windows)]
    {
        let metadata = std::fs::metadata(source)
            .with_context(|| format!("failed to stat symlink target {}", source.display()))?;
        if metadata.is_dir() {
            std::os::windows::fs::symlink_dir(source, target)
                .with_context(|| format!("failed to create symlink {}", target.display()))
        } else {
            std::os::windows::fs::symlink_file(source, target)
                .with_context(|| format!("failed to create symlink {}", target.display()))
        }
    }
}

pub(super) fn workspace_visible_in_list(
    workspace_id: &str,
    active_workspace_ids: &[String],
    is_archived: bool,
) -> bool {
    is_archived || !active_workspace_ids.iter().any(|id| id == workspace_id)
}

pub(super) fn tool_phase_timeout_grace_seconds() -> f64 {
    15.0
}

pub(super) fn log_turn_usage(
    agent_id: uuid::Uuid,
    session: &SessionSnapshot,
    usage: &TokenUsage,
    initialization: bool,
    agent_kind: &str,
    parent_agent_id: Option<uuid::Uuid>,
) {
    info!(
        log_stream = "agent",
        log_key = %agent_id,
        kind = "turn_token_usage",
        session_id = %session.id,
        channel_id = %session.address.channel_id,
        agent_kind,
        initialization,
        parent_agent_id = parent_agent_id.map(|value| value.to_string()),
        llm_calls = usage.llm_calls,
        input_total_tokens = usage.input_total_tokens(),
        output_total_tokens = usage.output_total_tokens(),
        context_total_tokens = usage.context_total_tokens(),
        cache_read_input_tokens = usage.cache_read_input_tokens(),
        cache_write_input_tokens = usage.cache_write_input_tokens(),
        cache_uncached_input_tokens = usage.cache_uncached_input_tokens(),
        normal_billed_input_tokens = usage.normal_billed_input_tokens(),
        legacy_prompt_tokens = usage.prompt_tokens,
        legacy_completion_tokens = usage.completion_tokens,
        legacy_total_tokens = usage.total_tokens,
        legacy_cache_hit_tokens = usage.cache_hit_tokens,
        legacy_cache_miss_tokens = usage.cache_miss_tokens,
        legacy_cache_read_tokens = usage.cache_read_tokens,
        legacy_cache_write_tokens = usage.cache_write_tokens,
        "recorded turn token usage"
    );
}

pub(super) fn log_agent_frame_event(
    agent_id: uuid::Uuid,
    session: &SessionSnapshot,
    kind: AgentPromptKind,
    model_key: &str,
    model_id: &str,
    event: &SessionEvent,
) {
    let agent_kind = match kind {
        AgentPromptKind::MainForeground => "main_foreground",
        AgentPromptKind::MainBackground => "main_background",
        AgentPromptKind::SubAgent => "subagent",
    };
    match event {
        SessionEvent::SessionStarted {
            previous_message_count,
            prompt_len,
            tool_definition_count,
            skill_count,
        } => info!(
            log_stream = "agent",
            log_key = %agent_id,
            kind = "agent_frame_session_started",
            session_id = %session.id,
            channel_id = %session.address.channel_id,
            agent_kind,
            model_key,
            model = model_id,
            previous_message_count = *previous_message_count as u64,
            prompt_len = *prompt_len as u64,
            tool_definition_count = *tool_definition_count as u64,
            skill_count = *skill_count as u64,
            "agent_frame session started"
        ),
        SessionEvent::CompactionStarted {
            phase,
            message_count,
        } => info!(
            log_stream = "agent",
            log_key = %agent_id,
            kind = "agent_frame_compaction_started",
            session_id = %session.id,
            channel_id = %session.address.channel_id,
            agent_kind,
            model_key,
            model = model_id,
            phase,
            message_count = *message_count as u64,
            "agent_frame compaction started"
        ),
        SessionEvent::CompactionCompleted {
            phase,
            compacted,
            estimated_tokens_before,
            estimated_tokens_after,
            token_limit,
            structured_output,
            compacted_messages,
        } => info!(
            log_stream = "agent",
            log_key = %agent_id,
            kind = "agent_frame_compaction_completed",
            session_id = %session.id,
            channel_id = %session.address.channel_id,
            agent_kind,
            model_key,
            model = model_id,
            phase,
            compacted = *compacted,
            estimated_tokens_before = *estimated_tokens_before as u64,
            estimated_tokens_after = *estimated_tokens_after as u64,
            token_limit = *token_limit as u64,
            structured_keywords = structured_output
                .as_ref()
                .map(|output| output.keywords.len() as u64)
                .unwrap_or(0),
            compacted_message_count = compacted_messages.len() as u64,
            "agent_frame compaction completed"
        ),
        SessionEvent::RoundStarted {
            round_index,
            message_count,
        } => info!(
            log_stream = "agent",
            log_key = %agent_id,
            kind = "agent_frame_round_started",
            session_id = %session.id,
            channel_id = %session.address.channel_id,
            agent_kind,
            model_key,
            model = model_id,
            round_index = *round_index as u64,
            message_count = *message_count as u64,
            "agent_frame round started"
        ),
        SessionEvent::ModelCallStarted {
            round_index,
            message_count,
        } => info!(
            log_stream = "agent",
            log_key = %agent_id,
            kind = "agent_frame_model_call_started",
            session_id = %session.id,
            channel_id = %session.address.channel_id,
            agent_kind,
            model_key,
            model = model_id,
            round_index = *round_index as u64,
            message_count = *message_count as u64,
            "agent_frame model call started"
        ),
        SessionEvent::ModelCallCompleted {
            round_index,
            tool_call_count,
            api_request_id,
            prompt_tokens,
            completion_tokens,
            total_tokens,
            cache_hit_tokens,
            cache_miss_tokens,
            cache_read_tokens,
            cache_write_tokens,
            ..
        } => info!(
            log_stream = "agent",
            log_key = %agent_id,
            kind = "agent_frame_model_call_completed",
            session_id = %session.id,
            channel_id = %session.address.channel_id,
            agent_kind,
            model_key,
            model = model_id,
            round_index = *round_index as u64,
            tool_call_count = *tool_call_count as u64,
            api_request_id = api_request_id.as_deref().unwrap_or(""),
            input_total_tokens = *prompt_tokens,
            output_total_tokens = *completion_tokens,
            context_total_tokens = *total_tokens,
            cache_read_input_tokens = *cache_read_tokens,
            cache_write_input_tokens = *cache_write_tokens,
            cache_uncached_input_tokens = *cache_miss_tokens,
            normal_billed_input_tokens = (*cache_miss_tokens).saturating_sub(*cache_write_tokens),
            legacy_prompt_tokens = *prompt_tokens,
            legacy_completion_tokens = *completion_tokens,
            legacy_total_tokens = *total_tokens,
            legacy_cache_hit_tokens = *cache_hit_tokens,
            legacy_cache_miss_tokens = *cache_miss_tokens,
            legacy_cache_read_tokens = *cache_read_tokens,
            legacy_cache_write_tokens = *cache_write_tokens,
            "agent_frame model call completed"
        ),
        SessionEvent::ToolWaitCompactionScheduled {
            tool_name,
            stable_prefix_message_count,
            delay_ms,
        } => info!(
            log_stream = "agent",
            log_key = %agent_id,
            kind = "agent_frame_tool_wait_compaction_scheduled",
            session_id = %session.id,
            channel_id = %session.address.channel_id,
            agent_kind,
            model_key,
            model = model_id,
            tool_name,
            stable_prefix_message_count = *stable_prefix_message_count as u64,
            delay_ms = *delay_ms,
            "agent_frame tool-wait compaction scheduled"
        ),
        SessionEvent::ToolWaitCompactionStarted {
            tool_name,
            stable_prefix_message_count,
        } => info!(
            log_stream = "agent",
            log_key = %agent_id,
            kind = "agent_frame_tool_wait_compaction_started",
            session_id = %session.id,
            channel_id = %session.address.channel_id,
            agent_kind,
            model_key,
            model = model_id,
            tool_name,
            stable_prefix_message_count = *stable_prefix_message_count as u64,
            "agent_frame tool-wait compaction started"
        ),
        SessionEvent::ToolWaitCompactionCompleted {
            tool_name,
            compacted,
            estimated_tokens_before,
            estimated_tokens_after,
            token_limit,
            structured_output,
            compacted_messages,
        } => info!(
            log_stream = "agent",
            log_key = %agent_id,
            kind = "agent_frame_tool_wait_compaction_completed",
            session_id = %session.id,
            channel_id = %session.address.channel_id,
            agent_kind,
            model_key,
            model = model_id,
            tool_name,
            compacted = *compacted,
            estimated_tokens_before = *estimated_tokens_before as u64,
            estimated_tokens_after = *estimated_tokens_after as u64,
            token_limit = *token_limit as u64,
            structured_keywords = structured_output
                .as_ref()
                .map(|output| output.keywords.len() as u64)
                .unwrap_or(0),
            compacted_message_count = compacted_messages.len() as u64,
            "agent_frame tool-wait compaction completed"
        ),
        SessionEvent::ToolCallStarted {
            round_index,
            tool_name,
            tool_call_id,
            arguments: _,
        } => info!(
            log_stream = "agent",
            log_key = %agent_id,
            kind = "agent_frame_tool_call_started",
            session_id = %session.id,
            channel_id = %session.address.channel_id,
            agent_kind,
            model_key,
            model = model_id,
            round_index = *round_index as u64,
            tool_name,
            tool_call_id,
            "agent_frame tool call started"
        ),
        SessionEvent::ToolCallCompleted {
            round_index,
            tool_name,
            tool_call_id,
            output_len,
            errored,
            ..
        } => info!(
            log_stream = "agent",
            log_key = %agent_id,
            kind = "agent_frame_tool_call_completed",
            session_id = %session.id,
            channel_id = %session.address.channel_id,
            agent_kind,
            model_key,
            model = model_id,
            round_index = *round_index as u64,
            tool_name,
            tool_call_id,
            output_len = *output_len as u64,
            errored = *errored,
            "agent_frame tool call completed"
        ),
        SessionEvent::SessionYielded {
            phase,
            message_count,
            total_tokens,
        } => info!(
            log_stream = "agent",
            log_key = %agent_id,
            kind = "agent_frame_session_yielded",
            session_id = %session.id,
            channel_id = %session.address.channel_id,
            agent_kind,
            model_key,
            model = model_id,
            phase,
            message_count = *message_count as u64,
            total_tokens = *total_tokens,
            "agent_frame session yielded at a safe boundary"
        ),
        SessionEvent::PrefixRewriteApplied {
            previous_prefix_message_count,
            replacement_prefix_message_count,
        } => info!(
            log_stream = "agent",
            log_key = %agent_id,
            kind = "agent_frame_prefix_rewrite_applied",
            session_id = %session.id,
            channel_id = %session.address.channel_id,
            agent_kind,
            model_key,
            model = model_id,
            previous_prefix_message_count = *previous_prefix_message_count as u64,
            replacement_prefix_message_count = *replacement_prefix_message_count as u64,
            "agent_frame prefix rewrite applied"
        ),
        SessionEvent::SessionCompleted {
            message_count,
            total_tokens,
        } => info!(
            log_stream = "agent",
            log_key = %agent_id,
            kind = "agent_frame_session_completed",
            session_id = %session.id,
            channel_id = %session.address.channel_id,
            agent_kind,
            model_key,
            model = model_id,
            message_count = *message_count as u64,
            total_tokens = *total_tokens,
            "agent_frame session completed"
        ),
    }
}
