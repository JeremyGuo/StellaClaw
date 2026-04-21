use agent_frame::{extract_assistant_text, load_config_value, run_session};
use anyhow::{Result, anyhow};
use serde_json::json;
use std::env;
use std::fs;
use tempfile::TempDir;

const LIVE_MODEL_ENV: &str = "AGENT_FRAME_OPENROUTER_MODEL";
const LIVE_TIMEOUT_ENV: &str = "AGENT_FRAME_LIVE_TIMEOUT_SECONDS";
const DEFAULT_LIVE_MODEL: &str = "moonshotai/kimi-k2.5";
const DEFAULT_LIVE_TIMEOUT_SECONDS: f64 = 300.0;

fn live_model() -> String {
    env::var(LIVE_MODEL_ENV).unwrap_or_else(|_| DEFAULT_LIVE_MODEL.to_string())
}

fn live_timeout_seconds() -> f64 {
    env::var(LIVE_TIMEOUT_ENV)
        .ok()
        .and_then(|value| value.parse::<f64>().ok())
        .filter(|value| *value > 0.0)
        .unwrap_or(DEFAULT_LIVE_TIMEOUT_SECONDS)
}

fn require_openrouter_api_key() -> Result<()> {
    env::var("OPENROUTER_API_KEY")
        .map(|_| ())
        .map_err(|_| anyhow!("OPENROUTER_API_KEY is not set; source .env before running this test"))
}

fn live_config(workspace_root: &str, tool_friendly: bool) -> Result<agent_frame::AgentConfig> {
    let system_prompt = if tool_friendly {
        "Use tools when needed. Keep the final answer short."
    } else {
        "Reply with exactly the requested token."
    };
    load_config_value(
        json!({
            "upstream": {
                "base_url": "https://openrouter.ai/api/v1",
                "model": live_model(),
                "api_key_env": "OPENROUTER_API_KEY",
                "timeout_seconds": live_timeout_seconds(),
                "headers": {
                    "HTTP-Referer": "https://local.agent-frame.test",
                    "X-Title": "agent_frame_live_test"
                }
            },
            "system_prompt": system_prompt,
            "workspace_root": workspace_root
        }),
        ".",
    )
}

#[test]
#[ignore = "requires sourced OPENROUTER_API_KEY and live OpenRouter access"]
fn live_openrouter_kimi_roundtrip_and_tool_call() -> Result<()> {
    require_openrouter_api_key()?;

    let plain_config = live_config(".", false)?;
    let plain_messages = run_session(
        Vec::new(),
        "Reply with exactly: KIMI_OK",
        plain_config,
        Vec::new(),
    )?;
    assert_eq!(extract_assistant_text(&plain_messages).trim(), "KIMI_OK");

    let temp_dir = TempDir::new()?;
    let file_content = "hello from kimi live test";
    fs::write(
        temp_dir.path().join("hello.txt"),
        format!("{file_content}\n"),
    )?;

    let tool_config = live_config(&temp_dir.path().display().to_string(), true)?;
    let tool_messages = run_session(
        Vec::new(),
        "Read hello.txt and reply with only its content.",
        tool_config,
        Vec::new(),
    )?;

    assert!(
        tool_messages.iter().any(|message| {
            message.role == "tool" && message.name.as_deref() == Some("file_read")
        })
    );
    assert!(extract_assistant_text(&tool_messages).contains(file_content));
    Ok(())
}
