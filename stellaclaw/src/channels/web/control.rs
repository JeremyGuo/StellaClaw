use crate::{
    config::{ModelSelection, SessionProfile, StellaclawConfig},
    service_protos::{channel::ChannelIngress, kernel::KernelRuntimeConfigPatch},
};
use stellaclaw_core::session_actor::ToolRemoteMode;

use super::http::{HttpError, HttpResult};

pub(super) fn control_ingress_from_text(
    config: &StellaclawConfig,
    text: &str,
    foreground_session_id: &str,
) -> HttpResult<Option<ChannelIngress>> {
    let Some((command, argument)) = parse_web_control_command(text) else {
        return Ok(None);
    };
    let foreground_session_id = Some(foreground_session_id.to_string());
    let ingress = match command {
        "/continue" if argument.is_empty() => ChannelIngress::ContinueForegroundTurn {
            foreground_session_id,
            reason: Some("web requested continue".to_string()),
        },
        "/cancel" if argument.is_empty() => ChannelIngress::CancelForegroundTurn {
            foreground_session_id,
            reason: Some("web requested cancel".to_string()),
        },
        "/compact" if argument.is_empty() => ChannelIngress::CompactForegroundNow {
            foreground_session_id,
        },
        "/status" if argument.is_empty() => ChannelIngress::QueryForegroundStatus {
            foreground_session_id,
        },
        "/model" if argument.is_empty() => ChannelIngress::QueryForegroundStatus {
            foreground_session_id,
        },
        "/model" => {
            if !config.models.contains_key(argument) {
                return Err(HttpError::new(
                    400,
                    format!("unknown model alias {argument}"),
                ));
            }
            ChannelIngress::UpdateRuntimeConfig {
                patch: KernelRuntimeConfigPatch {
                    session_profile: Some(Some(SessionProfile {
                        main_model: ModelSelection::alias(argument.to_string()),
                    })),
                    ..Default::default()
                },
            }
        }
        "/reasoning" => {
            let effort = parse_reasoning_effort_argument(argument)?;
            ChannelIngress::UpdateRuntimeConfig {
                patch: KernelRuntimeConfigPatch {
                    reasoning_effort: Some(effort),
                    ..Default::default()
                },
            }
        }
        "/remote" if argument.is_empty() => ChannelIngress::QueryForegroundStatus {
            foreground_session_id,
        },
        "/remote" if matches!(argument, "off" | "disable" | "disabled" | "local") => {
            ChannelIngress::UpdateRuntimeConfig {
                patch: KernelRuntimeConfigPatch {
                    tool_remote_mode: Some(ToolRemoteMode::Selectable),
                    ..Default::default()
                },
            }
        }
        "/remote" => {
            let mut parts = argument.split_whitespace();
            let host = parts.next().unwrap_or_default();
            let path = parts.next().unwrap_or_default();
            if host.is_empty() || path.is_empty() || parts.next().is_some() {
                return Err(HttpError::new(400, "usage: /remote <host> <path>"));
            }
            ChannelIngress::UpdateRuntimeConfig {
                patch: KernelRuntimeConfigPatch {
                    tool_remote_mode: Some(ToolRemoteMode::FixedSsh {
                        host: host.to_string(),
                        cwd: Some(path.to_string()),
                    }),
                    ..Default::default()
                },
            }
        }
        "/sandbox" => {
            return Err(HttpError::new(
                400,
                "sandbox runtime switching is not exposed through web yet",
            ));
        }
        _ => return Ok(None),
    };
    Ok(Some(ingress))
}

fn parse_web_control_command(text: &str) -> Option<(&str, &str)> {
    let trimmed = text.trim();
    if !trimmed.starts_with('/') {
        return None;
    }
    let mut parts = trimmed.splitn(2, char::is_whitespace);
    let command = parts.next()?.split('@').next()?.trim();
    let argument = parts.next().unwrap_or_default().trim();
    Some((command, argument))
}

fn parse_reasoning_effort_argument(argument: &str) -> HttpResult<Option<String>> {
    match argument.trim().to_ascii_lowercase().as_str() {
        "" | "show" => Ok(None),
        "default" | "model" | "model_default" | "model-default" | "global" => Ok(None),
        "minimal" | "low" | "medium" | "high" | "xhigh" => {
            Ok(Some(argument.trim().to_ascii_lowercase()))
        }
        other => Err(HttpError::new(
            400,
            format!("unknown reasoning effort {other}"),
        )),
    }
}
