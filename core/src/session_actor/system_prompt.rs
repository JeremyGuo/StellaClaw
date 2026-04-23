use super::{SessionInitial, SessionType, ToolRemoteMode};

pub fn system_prompt_for_initial(initial: &SessionInitial) -> String {
    let session_kind = match initial.session_type {
        SessionType::Foreground => foreground_prompt(),
        SessionType::Background => background_prompt(),
        SessionType::Subagent => subagent_prompt(),
    };

    format!(
        "{}\n\n{}\n\n{}",
        common_prompt(),
        session_kind,
        remote_prompt(&initial.tool_remote_mode)
    )
}

fn common_prompt() -> &'static str {
    "You are PartyClaw, a pragmatic coding agent. Work in Rust-first codebases with minimal, \
     direct abstractions. Use tools when they materially advance the task. Keep answers concise \
     and grounded in the current workspace. Never insert role=system messages into conversation \
     history; runtime context changes arrive as user-side notices."
}

fn foreground_prompt() -> &'static str {
    "Session kind: foreground. You are interacting with the user directly. Prefer clear progress, \
     concrete code changes, and a short final summary with verification."
}

fn background_prompt() -> &'static str {
    "Session kind: background. Work autonomously on the assigned task. Use user_tell only for \
     useful progress or coordination; put the primary result in the final assistant response."
}

fn subagent_prompt() -> &'static str {
    "Session kind: subagent. You are delegated a bounded task. Stay inside the requested scope, \
     report concrete findings or changed files, and avoid taking ownership of unrelated work."
}

fn remote_prompt(remote_mode: &ToolRemoteMode) -> String {
    match remote_mode {
        ToolRemoteMode::Selectable => {
            "Tool remote mode: selectable. When a tool exposes remote, its value must be an SSH \
             Host alias from ~/.ssh/config; omit remote for local execution."
                .to_string()
        }
        ToolRemoteMode::FixedSsh { host, cwd } => match cwd.as_deref().map(str::trim).filter(|cwd| !cwd.is_empty()) {
            Some(cwd) => format!(
                "Tool remote mode: fixed SSH. Tools execute on SSH Host alias `{host}` with cwd `{cwd}` when remote execution applies."
            ),
            None => format!(
                "Tool remote mode: fixed SSH. Tools execute on SSH Host alias `{host}` when remote execution applies."
            ),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn system_prompt_changes_by_session_type() {
        let foreground =
            system_prompt_for_initial(&SessionInitial::new("s1", SessionType::Foreground));
        let background =
            system_prompt_for_initial(&SessionInitial::new("s2", SessionType::Background));
        let subagent = system_prompt_for_initial(&SessionInitial::new("s3", SessionType::Subagent));

        assert!(foreground.contains("Session kind: foreground"));
        assert!(background.contains("Session kind: background"));
        assert!(subagent.contains("Session kind: subagent"));
    }
}
