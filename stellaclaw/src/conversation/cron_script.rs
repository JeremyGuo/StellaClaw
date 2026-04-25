use std::{
    collections::HashSet,
    fs,
    path::{Component, Path, PathBuf},
    process::Stdio,
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use anyhow::{anyhow, Context, Result};

use crate::{config::SandboxConfig, cron::CronTaskRecord, sandbox::build_workspace_shell_command};

const DEFAULT_CRON_SCRIPT_TIMEOUT: Duration = Duration::from_secs(30);
const MAX_CRON_SCRIPT_LINES: usize = 3;
const MAX_CRON_SCRIPT_TEXT_CHARS: usize = 16_000;

#[derive(Debug)]
pub(super) struct CronScriptResult {
    pub exit_code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(super) enum CronScriptTarget {
    User,
    Foreground,
    Background,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct CronScriptMessage {
    pub targets: Vec<CronScriptTarget>,
    pub text: String,
}

pub(super) fn run_script_command(
    task: &CronTaskRecord,
    command: &str,
    conversation_root: &Path,
    workspace_root: &Path,
    sandbox: &SandboxConfig,
) -> Result<CronScriptResult> {
    let script_cwd = resolve_script_cwd(workspace_root, task.script_cwd.as_deref())?;
    let script_wrapper = script_wrapper(command, workspace_root, &script_cwd);
    let timeout = task
        .script_timeout_seconds
        .map(Duration::from_secs_f64)
        .unwrap_or(DEFAULT_CRON_SCRIPT_TIMEOUT);
    let output_root = conversation_root
        .join(".log")
        .join("stellaclaw")
        .join("cron_script")
        .join(&task.id)
        .join(format!(
            "{}",
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_millis()
        ));
    fs::create_dir_all(&output_root)
        .with_context(|| format!("failed to create {}", output_root.display()))?;
    let stdout_path = output_root.join("stdout");
    let stderr_path = output_root.join("stderr");
    let stdout = fs::File::create(&stdout_path)
        .with_context(|| format!("failed to create {}", stdout_path.display()))?;
    let stderr = fs::File::create(&stderr_path)
        .with_context(|| format!("failed to create {}", stderr_path.display()))?;
    let mut child =
        build_workspace_shell_command(sandbox, workspace_root, conversation_root, &script_wrapper)
            .context("failed to build cron script command")?
            .stdin(Stdio::null())
            .stdout(Stdio::from(stdout))
            .stderr(Stdio::from(stderr))
            .spawn()
            .with_context(|| format!("failed to spawn cron script for {}", task.id))?;

    let deadline = Instant::now() + timeout;
    let status = loop {
        if let Some(status) = child
            .try_wait()
            .with_context(|| format!("failed to wait for cron script {}", task.id))?
        {
            break status;
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            return Err(anyhow!(
                "cron script {} timed out after {:.1}s",
                task.id,
                timeout.as_secs_f64()
            ));
        }
        thread::sleep(Duration::from_millis(50));
    };

    Ok(CronScriptResult {
        exit_code: status.code(),
        stdout: fs::read_to_string(&stdout_path).unwrap_or_default(),
        stderr: fs::read_to_string(&stderr_path).unwrap_or_default(),
    })
}

fn resolve_script_cwd(workspace_root: &Path, script_cwd: Option<&str>) -> Result<PathBuf> {
    let Some(raw) = script_cwd.map(str::trim).filter(|value| !value.is_empty()) else {
        return Ok(workspace_root.to_path_buf());
    };
    let path = Path::new(raw);
    if path.is_absolute() {
        return Err(anyhow!(
            "script_cwd must be relative to the conversation workspace"
        ));
    }
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::Normal(value) => normalized.push(value),
            Component::ParentDir => return Err(anyhow!("script_cwd must not contain '..'")),
            Component::RootDir | Component::Prefix(_) => {
                return Err(anyhow!(
                    "script_cwd must be relative to the conversation workspace"
                ))
            }
        }
    }
    let path = workspace_root.join(normalized);
    if !path.is_dir() {
        return Err(anyhow!("script_cwd does not exist or is not a directory"));
    }
    Ok(path)
}

pub(super) fn parse_script_stdout(stdout: &str) -> Result<Vec<CronScriptMessage>> {
    let lines = stdout
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .collect::<Vec<_>>();
    if lines.len() > MAX_CRON_SCRIPT_LINES {
        return Err(anyhow!(
            "cron script stdout must contain at most {MAX_CRON_SCRIPT_LINES} non-empty JSONL lines"
        ));
    }

    let mut seen_targets = HashSet::new();
    let mut messages = Vec::new();
    for (index, line) in lines.iter().enumerate() {
        let value: serde_json::Value = serde_json::from_str(line)
            .with_context(|| format!("cron script stdout line {} is not valid JSON", index + 1))?;
        let object = value
            .as_object()
            .ok_or_else(|| anyhow!("cron script stdout line {} must be an object", index + 1))?;
        let text = object
            .get("text")
            .and_then(serde_json::Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .ok_or_else(|| {
                anyhow!(
                    "cron script stdout line {} requires non-empty text",
                    index + 1
                )
            })?;
        let to = object
            .get("to")
            .and_then(serde_json::Value::as_array)
            .ok_or_else(|| anyhow!("cron script stdout line {} requires to array", index + 1))?;

        let mut targets = Vec::new();
        for target in to {
            let raw = target.as_str().ok_or_else(|| {
                anyhow!(
                    "cron script stdout line {} contains a non-string to entry",
                    index + 1
                )
            })?;
            let Some(target) = parse_target(raw)? else {
                continue;
            };
            if !seen_targets.insert(target) {
                return Err(anyhow!(
                    "cron script target {target:?} appears in more than one stdout line"
                ));
            }
            targets.push(target);
        }
        if targets.is_empty() {
            return Err(anyhow!(
                "cron script stdout line {} does not contain a delivery target",
                index + 1
            ));
        }
        messages.push(CronScriptMessage {
            targets,
            text: truncate_script_text(text),
        });
    }
    Ok(messages)
}

fn parse_target(raw: &str) -> Result<Option<CronScriptTarget>> {
    match raw.trim() {
        "user" => Ok(Some(CronScriptTarget::User)),
        "foreground" => Ok(Some(CronScriptTarget::Foreground)),
        "background" => Ok(Some(CronScriptTarget::Background)),
        "text" => Ok(None),
        other => Err(anyhow!("unknown cron script target '{other}'")),
    }
}

fn script_wrapper(command: &str, workspace_root: &Path, script_cwd: &Path) -> String {
    if script_cwd == workspace_root {
        return command.to_string();
    }
    format!("cd {} && {}", shell_quote_path(script_cwd), command)
}

fn shell_quote_path(path: &Path) -> String {
    let value = path.to_string_lossy();
    format!("'{}'", value.replace('\'', "'\\''"))
}

fn truncate_script_text(text: &str) -> String {
    let mut output = String::new();
    for (index, ch) in text.chars().enumerate() {
        if index >= MAX_CRON_SCRIPT_TEXT_CHARS {
            output.push_str("\n[cron script text truncated]");
            return output;
        }
        output.push(ch);
    }
    output
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::SandboxConfig;

    #[test]
    fn run_script_captures_nonzero_output() {
        let root = std::env::temp_dir().join(format!(
            "stellaclaw-cron-script-{}",
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("time should move forward")
                .as_nanos()
        ));
        let conversation_root = root.join("conversation");
        let workspace_root = root.join("workspace");
        let script_dir = workspace_root.join("checks");
        fs::create_dir_all(&script_dir).expect("script dir should exist");
        let task = CronTaskRecord {
            id: "cron_test".to_string(),
            conversation_id: "telegram-main-000001".to_string(),
            channel_id: "telegram-main".to_string(),
            platform_chat_id: "123".to_string(),
            name: "checked".to_string(),
            description: "checked task".to_string(),
            schedule: "* * * * * *".to_string(),
            timezone: "Asia/Shanghai".to_string(),
            task: String::new(),
            model: None,
            script_command: Some("printf changed && exit 7".to_string()),
            script_timeout_seconds: Some(2.0),
            script_cwd: Some("checks".to_string()),
            enabled: true,
            next_run_at: None,
            last_run_at: None,
            last_error: None,
        };

        let result = run_script_command(
            &task,
            task.script_command.as_deref().unwrap(),
            &conversation_root,
            &workspace_root,
            &SandboxConfig::default(),
        )
        .expect("script should run");

        assert_eq!(result.exit_code, Some(7));
        assert_eq!(result.stdout, "changed");
        fs::remove_dir_all(&root).expect("temp root should be removed");
    }

    #[test]
    fn script_cwd_rejects_parent_components() {
        let workspace_root = Path::new("/tmp/stellaclaw-workspace");
        assert!(resolve_script_cwd(workspace_root, Some("../bad")).is_err());
        assert!(resolve_script_cwd(workspace_root, Some("/tmp")).is_err());
    }

    #[test]
    fn script_stdout_parses_jsonl_targets() {
        let messages =
            parse_script_stdout(r#"{"to":["user","foreground","text"],"text":"日历有新变动：A"}"#)
                .expect("script stdout should parse");

        assert_eq!(messages.len(), 1);
        assert_eq!(
            messages[0].targets,
            vec![CronScriptTarget::User, CronScriptTarget::Foreground]
        );
        assert_eq!(messages[0].text, "日历有新变动：A");
    }

    #[test]
    fn script_stdout_rejects_duplicate_targets_across_lines() {
        let error = parse_script_stdout(r#"{"to":["user"],"text":"one"}"#.to_string().as_str())
            .expect("single line should parse");
        assert_eq!(error.len(), 1);

        assert!(parse_script_stdout(
            r#"{"to":["user"],"text":"one"}
{"to":["user"],"text":"two"}"#
        )
        .is_err());
    }
}
