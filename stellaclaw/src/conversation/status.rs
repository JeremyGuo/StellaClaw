use std::{
    fs,
    io::{BufRead, BufReader, ErrorKind},
    path::Path,
};

use anyhow::Result;
use stellaclaw_core::session_actor::{ChatMessage, TokenUsage, TokenUsageCost, ToolRemoteMode};

use crate::{
    channels::types::{
        OutgoingStatus, OutgoingUsageCost, OutgoingUsageSummary, OutgoingUsageTotals,
    },
    config::StellaclawConfig,
};

use super::{ConversationState, ManagedSessionStatus};

#[derive(Debug, Clone, Default)]
struct UsageTotals {
    cache_read: u64,
    cache_write: u64,
    uncache_input: u64,
    output: u64,
    cost: TokenUsageCost,
}

#[derive(Debug, Clone, Default)]
struct ConversationUsageSummary {
    foreground: UsageTotals,
    background: UsageTotals,
    subagents: UsageTotals,
    media_tools: UsageTotals,
}

impl UsageTotals {
    fn add_token_usage(&mut self, usage: &TokenUsage) {
        self.cache_read = self.cache_read.saturating_add(usage.cache_read);
        self.cache_write = self.cache_write.saturating_add(usage.cache_write);
        self.uncache_input = self.uncache_input.saturating_add(usage.uncache_input);
        self.output = self.output.saturating_add(usage.output);
        if let Some(cost) = &usage.cost_usd {
            self.add_cost(cost);
        }
    }

    fn add_cost(&mut self, cost: &TokenUsageCost) {
        self.cost.cache_read += cost.cache_read;
        self.cost.cache_write += cost.cache_write;
        self.cost.uncache_input += cost.uncache_input;
        self.cost.output += cost.output;
    }

    fn add_totals(&mut self, other: &UsageTotals) {
        self.cache_read = self.cache_read.saturating_add(other.cache_read);
        self.cache_write = self.cache_write.saturating_add(other.cache_write);
        self.uncache_input = self.uncache_input.saturating_add(other.uncache_input);
        self.output = self.output.saturating_add(other.output);
        self.add_cost(&other.cost);
    }
}

pub(super) fn conversation_status_snapshot(
    _workdir: &Path,
    session_root: &Path,
    workspace_root: &Path,
    state: &ConversationState,
    config: &StellaclawConfig,
) -> Result<OutgoingStatus> {
    let sandbox = state.sandbox.as_ref().unwrap_or(&config.sandbox);
    let sandbox_source = if state.sandbox.is_some() {
        "conversation"
    } else {
        "default"
    };
    let remote = match &state.tool_remote_mode {
        ToolRemoteMode::Selectable => "selectable".to_string(),
        ToolRemoteMode::FixedSsh { host, cwd } => {
            format!("fixed ssh `{host}` `{}`", cwd.as_deref().unwrap_or(""))
        }
    };
    let running_background = state
        .session_binding
        .background_sessions
        .values()
        .filter(|record| record.status == ManagedSessionStatus::Running)
        .count();
    let running_subagents = state
        .session_binding
        .subagent_sessions
        .values()
        .filter(|record| record.status == ManagedSessionStatus::Running)
        .count();
    let mut usage = ConversationUsageSummary::default();
    usage.foreground.add_totals(&session_usage_totals(
        session_root,
        &state.session_binding.foreground_session_id,
    ));
    for record in state.session_binding.background_sessions.values() {
        usage
            .background
            .add_totals(&session_usage_totals(session_root, &record.session_id));
    }
    for record in state.session_binding.subagent_sessions.values() {
        usage
            .subagents
            .add_totals(&session_usage_totals(session_root, &record.session_id));
    }
    usage
        .media_tools
        .add_totals(&media_tool_usage_totals(workspace_root));

    Ok(OutgoingStatus {
        channel_id: state.channel_id.clone(),
        platform_chat_id: state.platform_chat_id.clone(),
        conversation_id: state.conversation_id.clone(),
        model: state
            .session_profile
            .main_model
            .display_name(&config.models),
        reasoning: state
            .reasoning_effort
            .as_deref()
            .unwrap_or("model default")
            .to_string(),
        sandbox: sandbox_mode_label(&sandbox.mode).to_string(),
        sandbox_source: sandbox_source.to_string(),
        remote,
        workspace: workspace_root.display().to_string(),
        running_background,
        total_background: state.session_binding.background_sessions.len(),
        running_subagents,
        total_subagents: state.session_binding.subagent_sessions.len(),
        usage: outgoing_usage_summary(&usage),
    })
}

fn session_usage_totals(session_root: &Path, session_id: &str) -> UsageTotals {
    let path = session_root
        .join(".stellaclaw")
        .join("log")
        .join(sanitize_session_id_for_log_path(session_id))
        .join("all_messages.jsonl");
    let Ok(Some(reader)) = open_jsonl_reader(&path) else {
        return UsageTotals::default();
    };

    let mut totals = UsageTotals::default();
    for line in reader.lines().map_while(Result::ok) {
        if line.trim().is_empty() {
            continue;
        }
        let Ok(message) = serde_json::from_str::<ChatMessage>(&line) else {
            continue;
        };
        if let Some(usage) = &message.token_usage {
            totals.add_token_usage(usage);
        }
    }
    totals
}

fn media_tool_usage_totals(workspace_root: &Path) -> UsageTotals {
    let path = workspace_root
        .join(".stellaclaw")
        .join("log")
        .join("tool_usage.jsonl");
    let Ok(Some(reader)) = open_jsonl_reader(&path) else {
        return UsageTotals::default();
    };

    let mut totals = UsageTotals::default();
    for line in reader.lines().map_while(Result::ok) {
        if line.trim().is_empty() {
            continue;
        }
        let Ok(value) = serde_json::from_str::<serde_json::Value>(&line) else {
            continue;
        };
        let Some(token_usage) = value.get("token_usage") else {
            continue;
        };
        let Ok(usage) = serde_json::from_value::<TokenUsage>(token_usage.clone()) else {
            continue;
        };
        totals.add_token_usage(&usage);
    }
    totals
}

fn open_jsonl_reader(path: &Path) -> Result<Option<BufReader<fs::File>>, std::io::Error> {
    match fs::File::open(path) {
        Ok(file) => Ok(Some(BufReader::new(file))),
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error),
    }
}

fn sandbox_mode_label(mode: &crate::config::SandboxMode) -> &'static str {
    match mode {
        crate::config::SandboxMode::Subprocess => "subprocess",
        crate::config::SandboxMode::Bubblewrap => "bubblewrap",
    }
}

fn outgoing_usage_summary(summary: &ConversationUsageSummary) -> OutgoingUsageSummary {
    OutgoingUsageSummary {
        foreground: outgoing_usage_totals(&summary.foreground),
        background: outgoing_usage_totals(&summary.background),
        subagents: outgoing_usage_totals(&summary.subagents),
        media_tools: outgoing_usage_totals(&summary.media_tools),
    }
}

fn outgoing_usage_totals(totals: &UsageTotals) -> OutgoingUsageTotals {
    OutgoingUsageTotals {
        cache_read: totals.cache_read,
        cache_write: totals.cache_write,
        uncache_input: totals.uncache_input,
        output: totals.output,
        cost: OutgoingUsageCost {
            cache_read: totals.cost.cache_read,
            cache_write: totals.cost.cache_write,
            uncache_input: totals.cost.uncache_input,
            output: totals.cost.output,
        },
    }
}

fn sanitize_session_id_for_log_path(session_id: &str) -> String {
    let safe = session_id
        .chars()
        .map(|ch| match ch {
            'a'..='z' | 'A'..='Z' | '0'..='9' | '_' | '-' | '.' => ch,
            _ => '_',
        })
        .collect::<String>();
    if safe.trim_matches('_').is_empty() || safe == "." || safe == ".." {
        "session".to_string()
    } else {
        safe
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        fs::{self, File},
        io::Write,
        time::{SystemTime, UNIX_EPOCH},
    };
    use stellaclaw_core::session_actor::{ChatMessage, ChatMessageItem, ChatRole, ContextItem};

    fn temp_root(name: &str) -> std::path::PathBuf {
        let id = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!("stellaclaw-status-{name}-{id}"))
    }

    #[test]
    fn session_usage_reads_stellaclaw_log_directory() {
        let root = temp_root("session-usage");
        let message_path = root
            .join(".stellaclaw")
            .join("log")
            .join("foreground")
            .join("all_messages.jsonl");
        fs::create_dir_all(message_path.parent().unwrap()).unwrap();
        let mut message = ChatMessage::new(
            ChatRole::Assistant,
            vec![ChatMessageItem::Context(ContextItem {
                text: "hello".to_string(),
            })],
        );
        message.token_usage = Some(TokenUsage {
            cache_read: 1,
            cache_write: 2,
            uncache_input: 3,
            output: 4,
            cost_usd: None,
        });
        let mut file = File::create(&message_path).unwrap();
        writeln!(file, "{}", serde_json::to_string(&message).unwrap()).unwrap();

        let totals = session_usage_totals(&root, "foreground");
        assert_eq!(totals.cache_read, 1);
        assert_eq!(totals.cache_write, 2);
        assert_eq!(totals.uncache_input, 3);
        assert_eq!(totals.output, 4);

        let _ = fs::remove_dir_all(root);
    }
}
