use super::*;

pub(super) fn parse_sink_target(
    value: &Value,
    default_target: Option<SinkTarget>,
) -> Result<SinkTarget> {
    let object = value
        .as_object()
        .ok_or_else(|| anyhow!("sink must be an object"))?;
    let kind = object
        .get("kind")
        .and_then(Value::as_str)
        .unwrap_or("direct");
    match kind {
        "current_session" => default_target
            .ok_or_else(|| anyhow!("current_session sink requires a default session target")),
        "direct" => Ok(SinkTarget::Direct(ChannelAddress {
            channel_id: object
                .get("channel_id")
                .and_then(Value::as_str)
                .map(ToOwned::to_owned)
                .or_else(|| match &default_target {
                    Some(SinkTarget::Direct(address)) => Some(address.channel_id.clone()),
                    _ => None,
                })
                .ok_or_else(|| anyhow!("direct sink requires channel_id"))?,
            conversation_id: object
                .get("conversation_id")
                .and_then(Value::as_str)
                .map(ToOwned::to_owned)
                .or_else(|| match &default_target {
                    Some(SinkTarget::Direct(address)) => Some(address.conversation_id.clone()),
                    _ => None,
                })
                .ok_or_else(|| anyhow!("direct sink requires conversation_id"))?,
            user_id: object
                .get("user_id")
                .and_then(Value::as_str)
                .map(ToOwned::to_owned)
                .or_else(|| match &default_target {
                    Some(SinkTarget::Direct(address)) => address.user_id.clone(),
                    _ => None,
                }),
            display_name: object
                .get("display_name")
                .and_then(Value::as_str)
                .map(ToOwned::to_owned)
                .or_else(|| match &default_target {
                    Some(SinkTarget::Direct(address)) => address.display_name.clone(),
                    _ => None,
                }),
        })),
        "broadcast" => Ok(SinkTarget::Broadcast(
            object
                .get("topic")
                .and_then(Value::as_str)
                .map(ToOwned::to_owned)
                .filter(|value| !value.trim().is_empty())
                .ok_or_else(|| anyhow!("broadcast sink requires topic"))?,
        )),
        "multi" => {
            let targets = object
                .get("targets")
                .and_then(Value::as_array)
                .ok_or_else(|| anyhow!("multi sink requires targets"))?;
            let parsed = targets
                .iter()
                .map(|target| parse_sink_target(target, default_target.clone()))
                .collect::<Result<Vec<_>>>()?;
            Ok(SinkTarget::Multi(parsed))
        }
        other => Err(anyhow!("unsupported sink kind {}", other)),
    }
}

pub(super) fn sink_target_to_value(target: &SinkTarget) -> Value {
    match target {
        SinkTarget::Direct(address) => json!({
            "kind": "direct",
            "channel_id": address.channel_id,
            "conversation_id": address.conversation_id,
            "user_id": address.user_id,
            "display_name": address.display_name
        }),
        SinkTarget::Broadcast(topic) => json!({
            "kind": "broadcast",
            "topic": topic
        }),
        SinkTarget::Multi(targets) => json!({
            "kind": "multi",
            "targets": targets.iter().map(sink_target_to_value).collect::<Vec<_>>()
        }),
    }
}

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

pub(super) fn normalize_sink_target(target: SinkTarget, session: &SessionSnapshot) -> SinkTarget {
    match target {
        SinkTarget::Direct(address) => {
            if address.channel_id == session.address.channel_id
                && address.conversation_id == session.id.to_string()
            {
                warn!(
                    log_stream = "agent",
                    log_key = %session.agent_id,
                    kind = "background_sink_normalized",
                    session_id = %session.id,
                    channel_id = %session.address.channel_id,
                    incorrect_conversation_id = %address.conversation_id,
                    corrected_conversation_id = %session.address.conversation_id,
                    "background agent sink used session_id as conversation_id; correcting to the current channel conversation"
                );
                SinkTarget::Direct(session.address.clone())
            } else {
                SinkTarget::Direct(address)
            }
        }
        SinkTarget::Broadcast(topic) => SinkTarget::Broadcast(topic),
        SinkTarget::Multi(targets) => SinkTarget::Multi(
            targets
                .into_iter()
                .map(|target| normalize_sink_target(target, session))
                .collect(),
        ),
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
        let result = Command::new("sh")
            .arg("-c")
            .arg(&command)
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

pub(super) fn f64_arg_required(
    arguments: &serde_json::Map<String, Value>,
    key: &str,
) -> Result<f64> {
    arguments
        .get(key)
        .and_then(Value::as_f64)
        .ok_or_else(|| anyhow!("{} must be a number", key))
}

pub(super) fn background_agent_timeout_seconds(model_timeout_seconds: f64) -> f64 {
    model_timeout_seconds + 15.0
}

pub(super) fn is_timeout_like(error: &anyhow::Error) -> bool {
    error.to_string().contains("timed out")
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
    if error_text.contains("tool-wait context compaction failed")
        || error_text.contains("threshold context compaction failed")
    {
        if language.starts_with("zh") {
            return format!(
                "自动上下文压缩失败了，但系统已经保留到最近的稳定位置。\n\n当前进度：{}\n\n发送 /continue 可以从这里继续。",
                progress_summary
            );
        }
        return format!(
            "Automatic context compaction failed, but the session has been preserved at the latest stable point.\n\nProgress so far: {}\n\nSend /continue to resume from there.",
            progress_summary
        );
    }
    let upstream_like = error_text.contains("upstream")
        || error_text.contains("provider")
        || error_text.contains("chat completion")
        || is_timeout_like(error);
    if language.starts_with("zh") {
        if upstream_like {
            format!(
                "这一轮在调用上游模型时失败了，但系统已经保留到最近的稳定位置。\n\n当前进度：{}\n\n发送 /continue 可以从这里继续。",
                progress_summary
            )
        } else {
            format!(
                "这一轮在完成前失败了，但系统已经保留到最近的稳定位置。\n\n当前进度：{}\n\n发送 /continue 可以尝试继续。",
                progress_summary
            )
        }
    } else if upstream_like {
        format!(
            "This turn failed while calling the upstream model, but the session has been preserved at the latest stable point.\n\nProgress so far: {}\n\nSend /continue to resume from there.",
            progress_summary
        )
    } else {
        format!(
            "This turn failed before finishing, but the session has been preserved at the latest stable point.\n\nProgress so far: {}\n\nSend /continue to try resuming from there.",
            progress_summary
        )
    }
}

pub(super) fn summarize_resume_progress(messages: &[ChatMessage]) -> String {
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
    if tool_result_count > 0 {
        let recent_tools = tool_names.iter().rev().take(3).cloned().collect::<Vec<_>>();
        if recent_tools.is_empty() {
            format!(
                "the previous turn already reached tool execution and preserved {tool_result_count} tool result(s)"
            )
        } else {
            format!(
                "the previous turn already reached tool execution and preserved {} tool result(s); recent tools: {}",
                tool_result_count,
                recent_tools
                    .into_iter()
                    .rev()
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        }
    } else {
        let partial_text = trailing
            .iter()
            .filter(|message| message.role == "assistant")
            .filter_map(|message| message.content.as_ref())
            .filter_map(Value::as_str)
            .find(|text| !text.trim().is_empty())
            .map(str::trim)
            .map(|text| text.chars().take(120).collect::<String>());
        match partial_text {
            Some(text) => format!(
                "the previous turn preserved partial assistant progress: {}",
                text
            ),
            None => "the previous turn was preserved before the assistant could finish responding"
                .to_string(),
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
) -> String {
    let usage = &session.cumulative_usage;
    let compaction = &session.cumulative_compaction;
    let cache_hit_rate = if usage.prompt_tokens == 0 {
        0.0
    } else {
        (usage.cache_hit_tokens as f64 / usage.prompt_tokens as f64) * 100.0
    };
    let pricing = estimate_cost_usd(model, usage);
    let compaction_pricing = estimate_compaction_savings_usd(model, compaction);
    let context_percent = if current_context_limit == 0 {
        0.0
    } else {
        (current_context_estimate as f64 / current_context_limit as f64) * 100.0
    };
    let language = language.to_ascii_lowercase();
    if language.starts_with("zh") {
        let mut lines = vec![
            format!("Session: {}", session.id),
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
                "Idle compaction retry: {}",
                format_idle_compaction_retry_status("zh", session)
            ),
            format!("Turns: {}", session.turn_count),
            format!(
                "Current context estimate: {} / {} tokens ({:.1}%, local estimate)",
                current_context_estimate, current_context_limit, context_percent
            ),
            String::new(),
            "Token 用量：".to_string(),
            format!("- llm_calls: {}", usage.llm_calls),
            format!("- prompt_tokens: {}", usage.prompt_tokens),
            format!("- completion_tokens: {}", usage.completion_tokens),
            format!("- total_tokens: {}", usage.total_tokens),
            format!("- cache_hit_tokens: {}", usage.cache_hit_tokens),
            format!("- cache_miss_tokens: {}", usage.cache_miss_tokens),
            format!("- cache_read_tokens: {}", usage.cache_read_tokens),
            format!("- cache_write_tokens: {}", usage.cache_write_tokens),
            format!("- cache_hit_rate: {:.2}%", cache_hit_rate),
        ];
        if let Some((formula, total_usd)) = pricing {
            lines.push(String::new());
            lines.push("价格估算：".to_string());
            lines.push(format!("- formula: {}", formula));
            lines.push(format!("- estimated_total_usd: ${:.6}", total_usd));
        } else {
            lines.push(String::new());
            lines.push("价格估算：当前模型没有内置价格表，无法直接估算。".to_string());
        }
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
                "Idle compaction retry: {}",
                format_idle_compaction_retry_status("en", session)
            ),
            format!("Turns: {}", session.turn_count),
            format!(
                "Current context estimate: {} / {} tokens ({:.1}%, local estimate)",
                current_context_estimate, current_context_limit, context_percent
            ),
            String::new(),
            "Token usage:".to_string(),
            format!("- llm_calls: {}", usage.llm_calls),
            format!("- prompt_tokens: {}", usage.prompt_tokens),
            format!("- completion_tokens: {}", usage.completion_tokens),
            format!("- total_tokens: {}", usage.total_tokens),
            format!("- cache_hit_tokens: {}", usage.cache_hit_tokens),
            format!("- cache_miss_tokens: {}", usage.cache_miss_tokens),
            format!("- cache_read_tokens: {}", usage.cache_read_tokens),
            format!("- cache_write_tokens: {}", usage.cache_write_tokens),
            format!("- cache_hit_rate: {:.2}%", cache_hit_rate),
        ];
        if let Some((formula, total_usd)) = pricing {
            lines.push(String::new());
            lines.push("Estimated cost:".to_string());
            lines.push(format!("- formula: {}", formula));
            lines.push(format!("- estimated_total_usd: ${:.6}", total_usd));
        } else {
            lines.push(String::new());
            lines.push(
                "Estimated cost: unavailable for the current model pricing table.".to_string(),
            );
        }
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

fn format_idle_compaction_retry_status(language: &str, session: &SessionSnapshot) -> String {
    let Some(retry) = session.idle_compaction_retry.as_ref() else {
        return if language.starts_with("zh") {
            "无".to_string()
        } else {
            "none".to_string()
        };
    };

    let age = retry
        .failed_at
        .map(|failed_at| {
            let seconds = (Utc::now() - failed_at).num_seconds().max(0);
            if language.starts_with("zh") {
                format!("{seconds}s前")
            } else {
                format!("{seconds}s ago")
            }
        })
        .unwrap_or_else(|| {
            if language.starts_with("zh") {
                "时间未知".to_string()
            } else {
                "time unknown".to_string()
            }
        });
    let summary = summarize_idle_compaction_retry_error(&retry.error_summary);
    if language.starts_with("zh") {
        format!("待重试 ({age}): {summary}")
    } else {
        format!("pending ({age}): {summary}")
    }
}

fn summarize_idle_compaction_retry_error(error_summary: &str) -> String {
    const MAX_CHARS: usize = 120;

    let normalized = error_summary
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    if normalized.chars().count() <= MAX_CHARS {
        return normalized;
    }

    normalized.chars().take(MAX_CHARS).collect::<String>() + "..."
}

struct ModelPricing {
    input_per_million: f64,
    output_per_million: f64,
}

fn model_pricing(model: &ModelConfig) -> Option<ModelPricing> {
    match (
        model.api_endpoint.contains("openrouter.ai"),
        model.model.as_str(),
    ) {
        (true, "anthropic/claude-opus-4.6") => Some(ModelPricing {
            input_per_million: 15.0,
            output_per_million: 75.0,
        }),
        (true, "anthropic/claude-sonnet-4.6") => Some(ModelPricing {
            input_per_million: 3.0,
            output_per_million: 15.0,
        }),
        (true, "qwen/qwen3.5-27b") => Some(ModelPricing {
            input_per_million: 0.195,
            output_per_million: 1.56,
        }),
        _ => None,
    }
}

pub(super) fn estimate_cost_usd(model: &ModelConfig, usage: &TokenUsage) -> Option<(String, f64)> {
    let pricing = model_pricing(model)?;
    let input_per_million = pricing.input_per_million;
    let output_per_million = pricing.output_per_million;
    let cache_read_per_million = input_per_million * 0.1;
    let cache_write_per_million = input_per_million * 1.25;
    let uncached_input_tokens = usage
        .cache_miss_tokens
        .saturating_sub(usage.cache_write_tokens);
    let total_usd = (usage.cache_read_tokens as f64 / 1_000_000.0) * cache_read_per_million
        + (usage.cache_write_tokens as f64 / 1_000_000.0) * cache_write_per_million
        + (uncached_input_tokens as f64 / 1_000_000.0) * input_per_million
        + (usage.completion_tokens as f64 / 1_000_000.0) * output_per_million;
    let formula = format!(
        "cache_read_tokens * ${cache_read_per_million:.6}/1M + cache_write_tokens * ${cache_write_per_million:.6}/1M + (cache_miss_tokens - cache_write_tokens) * ${input_per_million:.6}/1M + completion_tokens * ${output_per_million:.6}/1M"
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
    model: &ModelConfig,
) -> Option<String> {
    cache_ttl.map(str::to_string).or_else(|| {
        (model.upstream_auth_kind() == agent_frame::config::UpstreamAuthKind::CodexSubscription)
            .then(|| "24h".to_string())
    })
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
    runtime: &ServerRuntime,
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
    let skills = discover_skills(&frame_config.skills_dirs)?;
    let extra_tools = runtime.build_extra_tools(
        session,
        AgentPromptKind::MainForeground,
        session.agent_id,
        None,
    );
    let registry = build_tool_registry(
        &frame_config.enabled_tools,
        &frame_config.workspace_root,
        &frame_config.runtime_state_root,
        &frame_config.upstream,
        frame_config.image_tool_upstream.as_ref(),
        &frame_config.skills_dirs,
        &skills,
        &extra_tools,
    )?;
    let tools = registry.into_values().collect::<Vec<_>>();
    Ok(estimate_session_tokens(&session.agent_messages, &tools, ""))
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
        prompt_tokens = usage.prompt_tokens,
        completion_tokens = usage.completion_tokens,
        total_tokens = usage.total_tokens,
        cache_hit_tokens = usage.cache_hit_tokens,
        cache_miss_tokens = usage.cache_miss_tokens,
        cache_read_tokens = usage.cache_read_tokens,
        cache_write_tokens = usage.cache_write_tokens,
        "recorded turn token usage"
    );
}

pub(super) fn log_agent_frame_event(
    agent_id: uuid::Uuid,
    session: &SessionSnapshot,
    kind: AgentPromptKind,
    model_key: &str,
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
            model = model_key,
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
            model = model_key,
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
            model = model_key,
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
            model = model_key,
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
            model = model_key,
            round_index = *round_index as u64,
            message_count = *message_count as u64,
            "agent_frame model call started"
        ),
        SessionEvent::ModelCallCompleted {
            round_index,
            tool_call_count,
            prompt_tokens,
            completion_tokens,
            total_tokens,
        } => info!(
            log_stream = "agent",
            log_key = %agent_id,
            kind = "agent_frame_model_call_completed",
            session_id = %session.id,
            channel_id = %session.address.channel_id,
            agent_kind,
            model = model_key,
            round_index = *round_index as u64,
            tool_call_count = *tool_call_count as u64,
            prompt_tokens = *prompt_tokens,
            completion_tokens = *completion_tokens,
            total_tokens = *total_tokens,
            "agent_frame model call completed"
        ),
        SessionEvent::CheckpointEmitted {
            message_count,
            total_tokens,
        } => info!(
            log_stream = "agent",
            log_key = %agent_id,
            kind = "agent_frame_checkpoint_emitted",
            session_id = %session.id,
            channel_id = %session.address.channel_id,
            agent_kind,
            model = model_key,
            message_count = *message_count as u64,
            total_tokens = *total_tokens,
            "agent_frame checkpoint emitted"
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
            model = model_key,
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
            model = model_key,
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
            model = model_key,
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
        } => info!(
            log_stream = "agent",
            log_key = %agent_id,
            kind = "agent_frame_tool_call_started",
            session_id = %session.id,
            channel_id = %session.address.channel_id,
            agent_kind,
            model = model_key,
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
        } => info!(
            log_stream = "agent",
            log_key = %agent_id,
            kind = "agent_frame_tool_call_completed",
            session_id = %session.id,
            channel_id = %session.address.channel_id,
            agent_kind,
            model = model_key,
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
            model = model_key,
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
            model = model_key,
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
            model = model_key,
            message_count = *message_count as u64,
            total_tokens = *total_tokens,
            "agent_frame session completed"
        ),
    }
}
