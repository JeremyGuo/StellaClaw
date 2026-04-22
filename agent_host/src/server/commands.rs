use super::*;

pub(super) enum AgentCommand {
    ShowSelection,
    SelectModel { model_key: String },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum IncomingCommandLane {
    Immediate,
    Conversation,
}

const IMMEDIATE_COMMANDS: &[&str] = &["/help", "/status"];
const KNOWN_COMMANDS: &[&str] = &[
    "/admin_authorize",
    "/admin_chat_approve",
    "/admin_chat_list",
    "/admin_chat_reject",
    "/agent",
    "/authorize",
    "/compact",
    "/compact_mode",
    "/continue",
    "/help",
    "/model",
    "/mount",
    "/remote",
    "/sandbox",
    "/set_api_timeout",
    "/snaplist",
    "/snapload",
    "/snapsave",
    "/status",
    "/think",
];

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

pub(super) fn parse_set_api_timeout_command(text: Option<&str>) -> Option<String> {
    parse_required_command_argument(text, "/set_api_timeout")
}

pub(super) fn parse_agent_command(text: Option<&str>) -> Option<AgentCommand> {
    let (_, suffix) = split_any_command_argument(text?, &["/agent", "/model"])?;
    if suffix.is_empty() {
        return Some(AgentCommand::ShowSelection);
    }

    let parts = suffix.split_whitespace().collect::<Vec<_>>();
    match parts.as_slice() {
        ["agent_frame"] => Some(AgentCommand::ShowSelection),
        [model_key] => Some(AgentCommand::SelectModel {
            model_key: (*model_key).to_string(),
        }),
        ["agent_frame", model_key] => Some(AgentCommand::SelectModel {
            model_key: (*model_key).to_string(),
        }),
        _ => {
            if parse_agent_backend_value(parts[0]).is_some() {
                None
            } else {
                Some(AgentCommand::SelectModel {
                    model_key: suffix.to_string(),
                })
            }
        }
    }
}

#[cfg(test)]
pub(super) fn parse_model_command(text: Option<&str>) -> Option<Option<String>> {
    parse_optional_command_argument(text, "/model")
}

pub(super) fn parse_compact_mode_command(text: Option<&str>) -> Option<Option<String>> {
    parse_optional_command_argument(text, "/compact_mode")
}

pub(super) fn parse_sandbox_command(text: Option<&str>) -> Option<Option<String>> {
    parse_optional_command_argument(text, "/sandbox")
}

pub(super) fn parse_mount_command(text: Option<&str>) -> Option<Option<String>> {
    parse_optional_command_argument(text, "/mount")
}

pub(super) fn parse_remote_command(text: Option<&str>) -> Option<Option<String>> {
    parse_optional_command_argument(text, "/remote")
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
    let (_, suffix) = split_command_argument(text?, command)?;
    Some((!suffix.is_empty()).then(|| suffix.to_string()))
}

fn parse_required_command_argument(text: Option<&str>, command: &str) -> Option<String> {
    let (_, suffix) = split_command_argument(text?, command)?;
    (!suffix.is_empty()).then(|| suffix.to_string())
}

fn normalized_command_parts(text: &str) -> Option<(String, String)> {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return None;
    }
    let mut parts = trimmed.splitn(2, char::is_whitespace);
    let first = parts.next()?;
    let normalized_first = first_token_without_mention(first);
    let rest = parts.next().map(str::trim_start).unwrap_or("");
    Some((normalized_first.to_string(), rest.to_string()))
}

fn first_token_without_mention(token: &str) -> &str {
    token.split_once('@').map_or(token, |(base, _)| base)
}

fn split_command_argument(text: &str, command: &str) -> Option<(String, String)> {
    let (normalized_command, suffix) = normalized_command_parts(text)?;
    (normalized_command == command).then_some((normalized_command, suffix.trim().to_string()))
}

fn split_any_command_argument(text: &str, commands: &[&str]) -> Option<(String, String)> {
    let (normalized_command, suffix) = normalized_command_parts(text)?;
    commands
        .contains(&normalized_command.as_str())
        .then_some((normalized_command, suffix.trim().to_string()))
}

pub(super) fn command_matches(text: &str, command: &str) -> bool {
    normalized_command_parts(text).is_some_and(|(normalized_command, suffix)| {
        normalized_command == command && suffix.is_empty()
    })
}

#[cfg(test)]
pub(super) fn is_out_of_band_command(text: Option<&str>) -> bool {
    command_name(text).is_some_and(|name| IMMEDIATE_COMMANDS.contains(&name.as_str()))
}

pub(super) fn command_starts_with(text: &str, command: &str) -> bool {
    normalized_command_parts(text)
        .is_some_and(|(normalized_command, _)| normalized_command == command)
}

pub(super) fn incoming_command_lane(text: Option<&str>) -> Option<IncomingCommandLane> {
    let command_name = command_name(text)?;
    if IMMEDIATE_COMMANDS.contains(&command_name.as_str()) {
        return Some(IncomingCommandLane::Immediate);
    }
    if KNOWN_COMMANDS.contains(&command_name.as_str()) {
        return Some(IncomingCommandLane::Conversation);
    }
    is_slash_command_name(&command_name).then_some(IncomingCommandLane::Immediate)
}

pub(super) fn is_command_like_text(text: Option<&str>) -> bool {
    command_name(text).is_some_and(|name| is_slash_command_name(&name))
}

fn command_name(text: Option<&str>) -> Option<String> {
    let (command_name, _) = normalized_command_parts(text?)?;
    Some(command_name)
}

fn is_slash_command_name(command_name: &str) -> bool {
    let Some(name) = command_name.strip_prefix('/') else {
        return false;
    };
    !name.trim().is_empty()
}

pub(super) fn sandbox_mode_label(mode: SandboxMode) -> &'static str {
    match mode {
        SandboxMode::Subprocess => "subprocess",
        SandboxMode::Bubblewrap => "bubblewrap",
    }
}

pub(super) fn sandbox_mode_value(mode: SandboxMode) -> &'static str {
    sandbox_mode_label(mode)
}

pub(super) fn parse_sandbox_mode_value(value: &str) -> Option<SandboxMode> {
    match value.trim() {
        "disabled" => Some(SandboxMode::Subprocess),
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

pub(super) fn parse_agent_backend_value(value: &str) -> Option<AgentBackendKind> {
    match value.trim() {
        "agent_frame" => Some(AgentBackendKind::AgentFrame),
        _ => None,
    }
}
