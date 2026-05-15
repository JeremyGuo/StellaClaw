use std::{
    collections::{BTreeMap, HashMap},
    fs,
    path::Path,
};

use anyhow::{Context, Result};
use serde_json::{json, Value};

use super::{WorkdirUpgrader, WORKDIR_VERSION_0_18, WORKDIR_VERSION_0_19};
use crate::{
    config::StellaclawConfig,
    conversation_new::{ServiceAddr, ServiceKind, ServiceScope},
    service_protos::agent_session::{AgentSessionBinding, AgentSessionKind},
};

pub struct ConversationServiceStateUpgrade;

impl WorkdirUpgrader for ConversationServiceStateUpgrade {
    fn from_version(&self) -> &'static str {
        WORKDIR_VERSION_0_18
    }

    fn to_version(&self) -> &'static str {
        WORKDIR_VERSION_0_19
    }

    fn upgrade(&self, workdir: &Path, config: &StellaclawConfig) -> Result<()> {
        let conversations_root = workdir.join("conversations");
        if !conversations_root.exists() {
            return Ok(());
        }

        let legacy_cron = load_legacy_cron_store(workdir)?;
        let mut cron_tasks_by_conversation: HashMap<String, Vec<Value>> = HashMap::new();
        for task in legacy_cron.tasks.values() {
            if let Some(conversation_id) = task.get("conversation_id").and_then(Value::as_str) {
                cron_tasks_by_conversation
                    .entry(conversation_id.to_string())
                    .or_default()
                    .push(task.clone());
            }
        }

        for entry in fs::read_dir(&conversations_root)
            .with_context(|| format!("failed to read {}", conversations_root.display()))?
        {
            let entry = entry
                .with_context(|| format!("failed to enumerate {}", conversations_root.display()))?;
            let conversation_root = entry.path();
            if !conversation_root.is_dir() {
                continue;
            }
            let state_path = conversation_root.join("conversation.json");
            if !state_path.is_file() {
                continue;
            }
            migrate_conversation(
                workdir,
                config,
                &conversation_root,
                &state_path,
                cron_tasks_by_conversation
                    .remove(&entry.file_name().to_string_lossy().to_string())
                    .unwrap_or_default(),
                legacy_cron.next_index,
            )?;
        }

        Ok(())
    }
}

#[derive(Debug, Default)]
struct LegacyCronStore {
    next_index: u64,
    tasks: BTreeMap<String, Value>,
}

fn load_legacy_cron_store(workdir: &Path) -> Result<LegacyCronStore> {
    let path = workdir.join(".stellaclaw").join("cron_tasks.json");
    if !path.is_file() {
        return Ok(LegacyCronStore {
            next_index: 1,
            tasks: BTreeMap::new(),
        });
    }
    let raw =
        fs::read_to_string(&path).with_context(|| format!("failed to read {}", path.display()))?;
    let value: Value = serde_json::from_str(&raw)
        .with_context(|| format!("failed to parse {}", path.display()))?;
    let next_index = value
        .get("next_index")
        .and_then(Value::as_u64)
        .unwrap_or(1)
        .max(1);
    let tasks = value
        .get("tasks")
        .and_then(Value::as_object)
        .map(|tasks| {
            tasks
                .iter()
                .map(|(id, task)| (id.clone(), task.clone()))
                .collect()
        })
        .unwrap_or_default();
    Ok(LegacyCronStore { next_index, tasks })
}

fn migrate_conversation(
    workdir: &Path,
    config: &StellaclawConfig,
    conversation_root: &Path,
    state_path: &Path,
    legacy_cron_tasks: Vec<Value>,
    legacy_cron_next_index: u64,
) -> Result<()> {
    let raw = fs::read_to_string(state_path)
        .with_context(|| format!("failed to read {}", state_path.display()))?;
    let mut state: Value = serde_json::from_str(&raw)
        .with_context(|| format!("failed to parse {}", state_path.display()))?;
    let conversation_id = state
        .get("conversation_id")
        .and_then(Value::as_str)
        .map(str::to_string)
        .unwrap_or_else(|| {
            conversation_root
                .file_name()
                .and_then(|name| name.to_str())
                .unwrap_or("unknown")
                .to_string()
        });
    let service_root = workdir.join("services").join(&conversation_id);
    fs::create_dir_all(&service_root)
        .with_context(|| format!("failed to create {}", service_root.display()))?;

    let old_foreground_session_id = state
        .pointer("/session_binding/foreground_session_id")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let new_foreground_session_id = storage_component(&local_addr(["agent", "foreground", "main"]));
    migrate_session_log_dir(
        conversation_root,
        &old_foreground_session_id,
        &new_foreground_session_id,
    )?;
    if let Some(binding) = state
        .get_mut("session_binding")
        .and_then(Value::as_object_mut)
    {
        binding.insert(
            "foreground_session_id".to_string(),
            Value::String(new_foreground_session_id.clone()),
        );
    }

    write_conversation_metadata(
        &service_root,
        &conversation_id,
        &new_foreground_session_id,
        &state,
    )?;
    write_runtime_config(&service_root, config, &state)?;
    write_manifest(&service_root, &state)?;
    write_agent_session_state(
        &service_root,
        conversation_root,
        &new_foreground_session_id,
        &state,
    )?;
    write_cron_state(&service_root, legacy_cron_tasks, legacy_cron_next_index)?;

    fs::write(
        state_path,
        serde_json::to_string_pretty(&state)
            .context("failed to encode migrated conversation state")?,
    )
    .with_context(|| format!("failed to write {}", state_path.display()))?;

    Ok(())
}

fn write_conversation_metadata(
    service_root: &Path,
    conversation_id: &str,
    foreground_session_id: &str,
    state: &Value,
) -> Result<()> {
    let path = service_root.join("conversation_metadata.json");
    if path.is_file() {
        return Ok(());
    }
    let nickname = state
        .get("nickname")
        .and_then(Value::as_str)
        .filter(|nickname| !nickname.trim().is_empty())
        .unwrap_or(conversation_id);
    let channel_id = state
        .get("channel_id")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let platform_chat_id = state
        .get("platform_chat_id")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let mut session_nicknames = serde_json::Map::new();
    session_nicknames.insert(
        foreground_session_id.to_string(),
        Value::String("Main".to_string()),
    );
    collect_managed_session_nicknames(
        state.pointer("/session_binding/background_sessions"),
        "background",
        &mut session_nicknames,
    );
    collect_managed_session_nicknames(
        state.pointer("/session_binding/subagent_sessions"),
        "subagent",
        &mut session_nicknames,
    );
    write_pretty_json(
        &path,
        &json!({
            "version": 1,
            "conversation_id": conversation_id,
            "nickname": nickname,
            "channel_id": channel_id,
            "platform_chat_id": platform_chat_id,
            "foreground_session_id": foreground_session_id,
            "model_selection_pending": state.get("model_selection_pending").and_then(Value::as_bool).unwrap_or(false),
            "session_nicknames": session_nicknames,
        }),
    )
}

fn collect_managed_session_nicknames(
    value: Option<&Value>,
    kind: &str,
    session_nicknames: &mut serde_json::Map<String, Value>,
) {
    let Some(records) = value.and_then(Value::as_object) else {
        return;
    };
    let service_name = if kind == "background" {
        "background"
    } else {
        "subagent"
    };
    for record in records.values() {
        let Some(agent_id) = record.get("agent_id").and_then(Value::as_str) else {
            continue;
        };
        let addr = local_addr(vec![
            "agent".to_string(),
            service_name.to_string(),
            agent_id.to_string(),
        ]);
        let nickname = record
            .get("name")
            .and_then(Value::as_str)
            .or_else(|| record.get("task").and_then(Value::as_str))
            .filter(|value| !value.trim().is_empty())
            .unwrap_or(agent_id);
        session_nicknames.insert(
            storage_component(&addr),
            Value::String(nickname.to_string()),
        );
    }
}

fn write_runtime_config(
    service_root: &Path,
    config: &StellaclawConfig,
    state: &Value,
) -> Result<()> {
    let path = service_root.join("runtime_config.json");
    if path.is_file() {
        return Ok(());
    }
    let runtime_config = json!({
        "session_profile": state.get("session_profile").cloned().unwrap_or(Value::Null),
        "models": &config.models,
        "session_defaults": &config.session_defaults,
        "memory_enabled": config.memory.enabled,
        "tool_remote_mode": state.get("tool_remote_mode").cloned().unwrap_or_else(|| json!({"type": "selectable"})),
        "sandbox": state.get("sandbox").cloned().unwrap_or(Value::Null),
        "reasoning_effort": state.get("reasoning_effort").cloned().unwrap_or(Value::Null),
    });
    write_pretty_json(&path, &runtime_config)
}

fn write_manifest(service_root: &Path, state: &Value) -> Result<()> {
    let path = service_root.join("manifest.json");
    if path.is_file() {
        return Ok(());
    }
    let next_background_id = state
        .pointer("/session_binding/next_background_index")
        .and_then(Value::as_u64)
        .unwrap_or(1)
        .max(1);
    let next_subagent_id = state
        .pointer("/session_binding/next_subagent_index")
        .and_then(Value::as_u64)
        .unwrap_or(1)
        .max(1);
    let service_specs = standard_service_specs();
    let services = service_specs
        .iter()
        .map(|(addr, kind)| {
            json!({
                "addr": addr,
                "kind": kind,
                "storage": service_root.join(storage_component(addr)).display().to_string(),
            })
        })
        .collect::<Vec<_>>();
    for (addr, _) in service_specs {
        fs::create_dir_all(service_root.join(storage_component(&addr))).with_context(|| {
            format!(
                "failed to create service storage {}",
                service_root.join(storage_component(&addr)).display()
            )
        })?;
    }
    write_pretty_json(
        &path,
        &json!({
            "version": 1,
            "services": services,
            "next_background_id": next_background_id,
            "next_subagent_id": next_subagent_id,
        }),
    )
}

fn write_agent_session_state(
    service_root: &Path,
    conversation_root: &Path,
    foreground_session_id: &str,
    state: &Value,
) -> Result<()> {
    let storage = service_root.join(storage_component(&local_addr([
        "agent",
        "foreground",
        "main",
    ])));
    fs::create_dir_all(&storage)
        .with_context(|| format!("failed to create {}", storage.display()))?;
    let path = storage.join("service_state.json");
    if path.is_file() {
        return Ok(());
    }
    let log_path = conversation_root
        .join(".stellaclaw")
        .join("log")
        .join(sanitize_session_id_for_log_path(foreground_session_id))
        .join("all_messages.jsonl");
    let (message_count, last_message) = summarize_message_log(&log_path)?;
    let service_state = json!({
        "kind": "foreground",
        "binding": {
            "event_sink": local_addr(["channel", "main"]),
        },
        "state": "idle",
        "message_count": message_count,
        "last_message": last_message,
        "next_subagent_index": state.pointer("/session_binding/next_subagent_index").and_then(Value::as_u64).unwrap_or(1).max(1),
        "next_background_index": state.pointer("/session_binding/next_background_index").and_then(Value::as_u64).unwrap_or(1).max(1),
        "next_cron_index": 1,
        "subagents": managed_agent_records(state.pointer("/session_binding/subagent_sessions"), "subagent"),
        "background_agents": managed_agent_records(state.pointer("/session_binding/background_sessions"), "background"),
    });
    write_pretty_json(&path, &service_state)
}

fn write_cron_state(
    service_root: &Path,
    legacy_tasks: Vec<Value>,
    legacy_next_index: u64,
) -> Result<()> {
    let storage = service_root.join(storage_component(&local_addr(["cron"])));
    fs::create_dir_all(&storage)
        .with_context(|| format!("failed to create {}", storage.display()))?;
    let path = storage.join("tasks.json");
    if path.is_file() {
        return Ok(());
    }
    let tasks = legacy_tasks
        .into_iter()
        .filter_map(migrate_legacy_cron_task)
        .collect::<Vec<_>>();
    write_pretty_json(
        &path,
        &json!({
            "version": 1,
            "next_run_id": legacy_next_index.max(1),
            "tasks": tasks,
        }),
    )
}

fn migrate_legacy_cron_task(task: Value) -> Option<Value> {
    let id = task.get("id").and_then(Value::as_str)?.to_string();
    let script_command = task.get("script_command").and_then(Value::as_str);
    let prompt = task
        .get("task")
        .and_then(Value::as_str)
        .filter(|value| !value.trim().is_empty())
        .map(str::to_string)
        .or_else(|| script_command.map(|command| format!("Legacy script cron task:\n{command}")))
        .unwrap_or_default();
    let mut enabled = task.get("enabled").and_then(Value::as_bool).unwrap_or(true);
    let mut last_error = task
        .get("last_error")
        .and_then(Value::as_str)
        .map(str::to_string);
    if script_command.is_some() {
        enabled = false;
        last_error.get_or_insert_with(|| {
            "legacy script cron task was disabled during service-state upgrade".to_string()
        });
    }
    Some(json!({
        "registration": {
            "task_id": id,
            "registered_by": local_addr(["agent", "foreground", "main"]),
            "channel_addr": local_addr(["channel", "main"]),
            "name": task.get("name").cloned().unwrap_or(Value::Null),
            "description": task.get("description").cloned().unwrap_or(Value::Null),
            "enabled": enabled,
            "foreground_session_addr": local_addr(["agent", "foreground", "main"]),
            "schedule": legacy_cron_schedule(&task),
            "payload": {
                "type": "prompt",
                "prompt": prompt,
                "output_policy": "forward_result_to_foreground",
            },
        },
        "consecutive_failures": if last_error.is_some() { 1 } else { 0 },
        "last_error": last_error,
        "last_run_status": legacy_last_run_status(&task),
    }))
}

fn legacy_cron_schedule(task: &Value) -> Value {
    let expression = task
        .get("schedule")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .trim();
    if expression.is_empty() {
        return json!({"type": "manual"});
    }
    json!({
        "type": "cron_expression",
        "expression": expression,
        "timezone": task.get("timezone").and_then(Value::as_str),
    })
}

fn legacy_last_run_status(task: &Value) -> Value {
    if task.get("last_error").and_then(Value::as_str).is_some() {
        Value::String("failed".to_string())
    } else if task.get("last_run_at").and_then(Value::as_str).is_some() {
        Value::String("completed".to_string())
    } else {
        Value::Null
    }
}

fn managed_agent_records(value: Option<&Value>, kind: &str) -> Value {
    let Some(records) = value.and_then(Value::as_object) else {
        return json!({});
    };
    let entries = records
        .values()
        .filter_map(|record| managed_agent_record(record, kind))
        .collect::<serde_json::Map<_, _>>();
    Value::Object(entries)
}

fn managed_agent_record(record: &Value, kind: &str) -> Option<(String, Value)> {
    let agent_id = record.get("agent_id").and_then(Value::as_str)?.to_string();
    let status = match record
        .get("status")
        .and_then(Value::as_str)
        .unwrap_or("failed")
    {
        "completed" => "completed",
        "failed" => "failed",
        "killed" => "killed",
        _ => "failed",
    };
    let last_error = record
        .get("last_error")
        .cloned()
        .filter(|value| !value.is_null())
        .or_else(|| {
            if status == "failed" {
                Some(Value::String(
                    "agent was not resumed after service-state upgrade".to_string(),
                ))
            } else {
                None
            }
        });
    let service_name = if kind == "background" {
        "background"
    } else {
        "subagent"
    };
    Some((
        agent_id.clone(),
        json!({
            "agent_id": agent_id,
            "addr": local_addr(vec![
                "agent".to_string(),
                service_name.to_string(),
                agent_id.clone(),
            ]),
            "status": status,
            "task": record.get("task").cloned().unwrap_or_else(|| Value::String(String::new())),
            "last_message": record.get("last_message").cloned().unwrap_or(Value::Null),
            "last_error": last_error.unwrap_or(Value::Null),
        }),
    ))
}

fn summarize_message_log(path: &Path) -> Result<(usize, Value)> {
    if !path.is_file() {
        return Ok((0, Value::Null));
    }
    let raw =
        fs::read_to_string(path).with_context(|| format!("failed to read {}", path.display()))?;
    let mut count = 0usize;
    let mut last_message = Value::Null;
    for line in raw.lines() {
        if line.trim().is_empty() {
            continue;
        }
        count += 1;
        last_message = serde_json::from_str(line)
            .with_context(|| format!("failed to parse message in {}", path.display()))?;
    }
    Ok((count, last_message))
}

fn migrate_session_log_dir(
    conversation_root: &Path,
    old_session_id: &str,
    new_session_id: &str,
) -> Result<()> {
    if old_session_id.is_empty() || old_session_id == new_session_id {
        return Ok(());
    }
    let log_root = conversation_root.join(".stellaclaw").join("log");
    let old_path = log_root.join(sanitize_session_id_for_log_path(old_session_id));
    let new_path = log_root.join(sanitize_session_id_for_log_path(new_session_id));
    if !old_path.is_dir() || new_path.exists() {
        return Ok(());
    }
    fs::rename(&old_path, &new_path).with_context(|| {
        format!(
            "failed to rename session log {} to {}",
            old_path.display(),
            new_path.display()
        )
    })
}

fn standard_service_specs() -> Vec<(ServiceAddr, ServiceKind)> {
    vec![
        (ServiceAddr::channel(), ServiceKind::Channel),
        (
            ServiceAddr::agent_foreground(),
            ServiceKind::AgentSession {
                kind: AgentSessionKind::Foreground,
                binding: AgentSessionBinding {
                    event_sink: ServiceAddr::channel(),
                    parent_addr: None,
                },
            },
        ),
        (ServiceAddr::cron(), ServiceKind::Cron),
        (ServiceAddr::memory(), ServiceKind::Memory),
        (ServiceAddr::skill(), ServiceKind::Skill),
        (ServiceAddr::tool_binary(), ServiceKind::ToolBinary),
        (ServiceAddr::workspace(), ServiceKind::Workspace),
        (ServiceAddr::terminal(), ServiceKind::Terminal),
        (ServiceAddr::status(), ServiceKind::Status),
    ]
}

fn local_addr<I, S>(segments: I) -> ServiceAddr
where
    I: IntoIterator<Item = S>,
    S: Into<String>,
{
    ServiceAddr::local_path(segments)
}

fn storage_component(addr: &ServiceAddr) -> String {
    let scope = match &addr.scope {
        ServiceScope::Local => "local".to_string(),
        ServiceScope::Conversation(conversation_id) => format!("conversation_{conversation_id}"),
    };
    let path = addr.path.join("__");
    format!("{scope}__{path}")
}

fn sanitize_session_id_for_log_path(session_id: &str) -> String {
    session_id
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '.') {
                ch
            } else {
                '_'
            }
        })
        .collect()
}

fn write_pretty_json(path: &Path, value: &Value) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }
    fs::write(
        path,
        serde_json::to_string_pretty(value).context("failed to encode JSON")?,
    )
    .with_context(|| format!("failed to write {}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{
        AgentServerConfig, MemoryConfig, SandboxConfig, SessionDefaults, StellaclawConfig,
        LATEST_CONFIG_VERSION,
    };
    use std::collections::BTreeMap;

    #[test]
    fn splits_legacy_conversation_state_into_service_storage() {
        let root = std::env::temp_dir().join(format!(
            "stellaclaw-v0-18-service-state-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&root);
        let conversation_id = "web-main-000001";
        let conversation_root = root.join("conversations").join(conversation_id);
        let old_session_id = format!("{conversation_id}.foreground");
        fs::create_dir_all(
            conversation_root
                .join(".stellaclaw")
                .join("log")
                .join(&old_session_id),
        )
        .unwrap();
        fs::write(
            conversation_root
                .join(".stellaclaw")
                .join("log")
                .join(&old_session_id)
                .join("all_messages.jsonl"),
            format!(
                "{}\n{}\n",
                json!({"role": "user", "data": [{"type": "context", "text": "hello"}]}),
                json!({"role": "assistant", "data": [{"type": "context", "text": "hi"}]}),
            ),
        )
        .unwrap();
        fs::write(
            conversation_root.join("conversation.json"),
            serde_json::to_string_pretty(&json!({
                "version": 1,
                "conversation_id": conversation_id,
                "nickname": "Demo",
                "channel_id": "web-main",
                "platform_chat_id": "web-chat",
                "session_profile": {"main_model": "main"},
                "model_selection_pending": false,
                "tool_remote_mode": {"type": "fixed_ssh", "host": "devbox", "cwd": "/work"},
                "reasoning_effort": "high",
                "session_binding": {
                    "foreground_session_id": old_session_id,
                    "next_background_index": 7,
                    "next_subagent_index": 3,
                    "background_sessions": {
                        "background_0001": {
                            "agent_id": "background_0001",
                            "session_id": "background_0001",
                            "session_type": "background",
                            "status": "completed",
                            "last_message": {"role": "assistant", "data": [{"type": "context", "text": "done"}]},
                            "task": "legacy background"
                        }
                    },
                    "subagent_sessions": {}
                }
            }))
            .unwrap(),
        )
        .unwrap();
        fs::create_dir_all(root.join(".stellaclaw")).unwrap();
        fs::write(
            root.join(".stellaclaw").join("cron_tasks.json"),
            serde_json::to_string_pretty(&json!({
                "next_index": 8,
                "tasks": {
                    "cron_0007": {
                        "id": "cron_0007",
                        "conversation_id": conversation_id,
                        "channel_id": "web-main",
                        "platform_chat_id": "web-chat",
                        "name": "Daily",
                        "description": "Do work",
                        "schedule": "0 0 8 * * *",
                        "timezone": "Asia/Shanghai",
                        "task": "check status",
                        "enabled": true
                    }
                }
            }))
            .unwrap(),
        )
        .unwrap();

        ConversationServiceStateUpgrade
            .upgrade(&root, &test_config())
            .unwrap();

        let service_root = root.join("services").join(conversation_id);
        let manifest: Value =
            serde_json::from_str(&fs::read_to_string(service_root.join("manifest.json")).unwrap())
                .unwrap();
        assert_eq!(manifest["services"].as_array().unwrap().len(), 9);
        assert_eq!(manifest["next_background_id"], 7);
        assert_eq!(manifest["next_subagent_id"], 3);

        let metadata: Value = serde_json::from_str(
            &fs::read_to_string(service_root.join("conversation_metadata.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(metadata["nickname"], "Demo");
        assert_eq!(metadata["channel_id"], "web-main");
        assert_eq!(metadata["platform_chat_id"], "web-chat");
        assert_eq!(
            metadata["foreground_session_id"],
            "local__agent__foreground__main"
        );
        assert_eq!(
            metadata["session_nicknames"]["local__agent__foreground__main"],
            "Main"
        );
        assert_eq!(
            metadata["session_nicknames"]["local__agent__background__background_0001"],
            "legacy background"
        );

        let runtime_config: Value = serde_json::from_str(
            &fs::read_to_string(service_root.join("runtime_config.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(runtime_config["session_profile"]["main_model"], "main");
        assert_eq!(runtime_config["tool_remote_mode"]["type"], "fixed_ssh");
        assert_eq!(runtime_config["reasoning_effort"], "high");

        let new_session_id = "local__agent__foreground__main";
        assert!(!conversation_root
            .join(".stellaclaw/log")
            .join(&old_session_id)
            .exists());
        assert!(conversation_root
            .join(".stellaclaw/log")
            .join(new_session_id)
            .join("all_messages.jsonl")
            .is_file());
        let migrated_state: Value = serde_json::from_str(
            &fs::read_to_string(conversation_root.join("conversation.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(
            migrated_state["session_binding"]["foreground_session_id"],
            new_session_id
        );

        let agent_state: Value = serde_json::from_str(
            &fs::read_to_string(
                service_root
                    .join("local__agent__foreground__main")
                    .join("service_state.json"),
            )
            .unwrap(),
        )
        .unwrap();
        assert_eq!(agent_state["message_count"], 2);
        assert_eq!(agent_state["last_message"]["role"], "assistant");
        assert_eq!(
            agent_state["background_agents"]["background_0001"]["status"],
            "completed"
        );

        let cron_state: Value = serde_json::from_str(
            &fs::read_to_string(service_root.join("local__cron").join("tasks.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(cron_state["next_run_id"], 8);
        assert_eq!(
            cron_state["tasks"][0]["registration"]["registered_by"]["path"][2],
            "main"
        );
        assert_eq!(
            cron_state["tasks"][0]["registration"]["schedule"]["type"],
            "cron_expression"
        );

        let _ = fs::remove_dir_all(root);
    }

    fn test_config() -> StellaclawConfig {
        StellaclawConfig {
            version: LATEST_CONFIG_VERSION.to_string(),
            agent_server: AgentServerConfig::default(),
            default_profile: None,
            channels: Vec::new(),
            models: BTreeMap::new(),
            session_defaults: SessionDefaults::default(),
            memory: MemoryConfig::default(),
            sandbox: SandboxConfig::default(),
            skill_sync: Vec::new(),
            available_agent_models: Vec::new(),
        }
    }
}
