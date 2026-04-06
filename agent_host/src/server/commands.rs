use super::*;

pub(super) fn format_api_timeout_update(
    session: &SessionSnapshot,
    model_timeout_seconds: f64,
    argument: &str,
) -> Result<(Option<f64>, String)> {
    let normalized = argument.trim().to_ascii_lowercase();
    if normalized == "default" || normalized == "reset" || normalized == "0" {
        return Ok((
            None,
            format!(
                "API timeout reset for session {}. Effective timeout is now {:.1}s (model default).",
                session.id, model_timeout_seconds
            ),
        ));
    }
    let timeout_seconds: f64 = argument
        .trim()
        .parse()
        .with_context(|| format!("invalid timeout value '{}'", argument.trim()))?;
    if timeout_seconds <= 0.0 {
        return Err(anyhow!(
            "API timeout must be greater than 0 seconds, or use 0/default/reset to restore the model default"
        ));
    }
    Ok((
        Some(timeout_seconds),
        format!(
            "API timeout updated for session {}. Effective timeout is now {:.1}s (session override).",
            session.id, timeout_seconds
        ),
    ))
}

pub(super) fn parse_oldspace_command(text: Option<&str>) -> Option<String> {
    let text = normalized_command_text(text?)?;
    let suffix = text.strip_prefix("/oldspace")?.trim();
    if suffix.is_empty() {
        None
    } else {
        Some(suffix.to_string())
    }
}

pub(super) fn parse_set_api_timeout_command(text: Option<&str>) -> Option<String> {
    let text = normalized_command_text(text?)?;
    let suffix = text.strip_prefix("/set_api_timeout")?.trim();
    if suffix.is_empty() {
        None
    } else {
        Some(suffix.to_string())
    }
}

pub(super) fn parse_model_command(text: Option<&str>) -> Option<Option<String>> {
    parse_optional_command_argument(text, "/model")
}

pub(super) fn parse_compact_mode_command(text: Option<&str>) -> Option<Option<String>> {
    parse_optional_command_argument(text, "/compact_mode")
}

pub(super) fn parse_sandbox_command(text: Option<&str>) -> Option<Option<String>> {
    parse_optional_command_argument(text, "/sandbox")
}

pub(super) fn parse_think_command(text: Option<&str>) -> Option<Option<String>> {
    parse_optional_command_argument(text, "/think")
}

pub(super) fn parse_snap_save_command(text: Option<&str>) -> Option<String> {
    parse_required_command_argument(text, "/snapsave")
}

pub(super) fn parse_snap_load_command(text: Option<&str>) -> Option<String> {
    parse_required_command_argument(text, "/snapload")
}

pub(super) fn parse_snap_list_command(text: Option<&str>) -> bool {
    matches!(parse_optional_command_argument(text, "/snaplist"), Some(_))
}

pub(super) fn parse_continue_command(text: Option<&str>) -> bool {
    matches!(parse_optional_command_argument(text, "/continue"), Some(_))
}

pub(super) fn parse_admin_authorize_command(text: Option<&str>) -> bool {
    text.map(str::trim).is_some_and(|value| {
        command_matches(value, "/admin_authorize") || command_matches(value, "/authorize")
    })
}

pub(super) fn parse_admin_chat_list_command(text: Option<&str>) -> bool {
    matches!(
        parse_optional_command_argument(text, "/admin_chat_list"),
        Some(_)
    )
}

pub(super) fn parse_admin_chat_approve_command(text: Option<&str>) -> Option<String> {
    parse_required_command_argument(text, "/admin_chat_approve")
}

pub(super) fn parse_admin_chat_reject_command(text: Option<&str>) -> Option<String> {
    parse_required_command_argument(text, "/admin_chat_reject")
}

pub(super) fn parse_optional_command_argument(
    text: Option<&str>,
    command: &str,
) -> Option<Option<String>> {
    let text = normalized_command_text(text?)?;
    if text == command {
        return Some(None);
    }
    let suffix = text.strip_prefix(command)?.trim();
    if suffix.is_empty() {
        Some(None)
    } else {
        Some(Some(suffix.to_string()))
    }
}

fn parse_required_command_argument(text: Option<&str>, command: &str) -> Option<String> {
    let text = normalized_command_text(text?)?;
    let suffix = text.strip_prefix(command)?.trim();
    if suffix.is_empty() {
        None
    } else {
        Some(suffix.to_string())
    }
}

fn normalized_command_text(text: &str) -> Option<String> {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return None;
    }
    let mut parts = trimmed.splitn(2, char::is_whitespace);
    let first = parts.next()?;
    let normalized_first = first_token_without_mention(first);
    let rest = parts.next().map(str::trim_start).unwrap_or("");
    if rest.is_empty() {
        Some(normalized_first.to_string())
    } else {
        Some(format!("{normalized_first} {rest}"))
    }
}

fn first_token_without_mention(token: &str) -> &str {
    token.split_once('@').map_or(token, |(base, _)| base)
}

pub(super) fn command_matches(text: &str, command: &str) -> bool {
    normalized_command_text(text).as_deref() == Some(command)
}

pub(super) fn command_starts_with(text: &str, command: &str) -> bool {
    normalized_command_text(text)
        .as_deref()
        .is_some_and(|normalized| normalized.starts_with(command))
}

pub(super) fn sandbox_mode_label(mode: SandboxMode) -> &'static str {
    match mode {
        SandboxMode::Disabled => "disabled",
        SandboxMode::Subprocess => "subprocess",
        SandboxMode::Bubblewrap => "bubblewrap",
    }
}

pub(super) fn sandbox_mode_value(mode: SandboxMode) -> &'static str {
    sandbox_mode_label(mode)
}

pub(super) fn parse_sandbox_mode_value(value: &str) -> Option<SandboxMode> {
    match value.trim() {
        "disabled" => Some(SandboxMode::Disabled),
        "subprocess" => Some(SandboxMode::Subprocess),
        "bubblewrap" => Some(SandboxMode::Bubblewrap),
        _ => None,
    }
}

pub(super) fn parse_reasoning_effort_value(value: &str) -> Option<&'static str> {
    match value.trim().to_ascii_lowercase().as_str() {
        "low" => Some("low"),
        "medium" => Some("medium"),
        "high" => Some("high"),
        _ => None,
    }
}
