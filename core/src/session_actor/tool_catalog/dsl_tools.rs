#![allow(dead_code)]

use std::{
    collections::HashMap,
    fs::{self, File},
    path::PathBuf,
    process::{Child, Command, Stdio},
    sync::{Mutex, OnceLock},
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use serde_json::{json, Map, Value};

use super::{
    schema::{object_schema, properties},
    ToolBackend, ToolDefinition, ToolExecutionMode,
};
use crate::session_actor::tool_runtime::{
    f64_arg_with_default, string_arg, string_arg_with_default, LocalToolError,
    ToolCancellationToken, ToolExecutionContext,
};

static DSL_JOBS: OnceLock<Mutex<HashMap<String, DslJob>>> = OnceLock::new();

struct DslJob {
    child: Child,
    stdout_path: PathBuf,
    stderr_path: PathBuf,
    out_path: PathBuf,
}

fn dsl_jobs() -> &'static Mutex<HashMap<String, DslJob>> {
    DSL_JOBS.get_or_init(|| Mutex::new(HashMap::new()))
}

pub fn dsl_tool_definitions() -> Vec<ToolDefinition> {
    vec![
        ToolDefinition::new(
            "dsl_start",
            "Start an exec-like DSL orchestration job backed by an isolated CPython worker. See the system prompt for DSL syntax and lifecycle rules. Output is capped by max_output_chars 0..1000 with complete files saved at returned paths.",
            object_schema(
                properties([
                    (
                        "code",
                        json!({
                            "type": "string",
                            "description": "DSL code executed inside a restricted CPython worker. See the system prompt for supported syntax and restrictions."
                        }),
                    ),
                    ("label", json!({"type": "string"})),
                    ("return_immediate", json!({"type": "boolean"})),
                    ("wait_timeout_seconds", json!({"type": "number", "minimum": 0})),
                    (
                        "on_timeout",
                        json!({"type": "string", "enum": ["continue", "kill", "CONTINUE", "KILL"]}),
                    ),
                    (
                        "max_output_chars",
                        json!({"type": "integer", "minimum": 0, "maximum": 1000}),
                    ),
                    ("max_runtime_seconds", json!({"type": "integer", "minimum": 1})),
                    ("max_llm_calls", json!({"type": "integer", "minimum": 0})),
                    ("max_tool_calls", json!({"type": "integer", "minimum": 0})),
                    ("max_emit_calls", json!({"type": "integer", "minimum": 0})),
                ]),
                &["code"],
            ),
            ToolExecutionMode::Interruptible,
            ToolBackend::Local,
        ),
        ToolDefinition::new(
            "dsl_wait",
            "Wait for or observe a previously started DSL job by dsl_id. Interrupting dsl_wait only interrupts the outer wait and returns running; the CPython DSL worker and any LLM/tool call it is currently performing continue in the background. Use wait_timeout_seconds=0 to observe without waiting. on_timeout=kill terminates the DSL job; otherwise timeout leaves it running.",
            object_schema(
                properties([
                    ("dsl_id", json!({"type": "string"})),
                    ("wait_timeout_seconds", json!({"type": "number", "minimum": 0})),
                    (
                        "on_timeout",
                        json!({"type": "string", "enum": ["continue", "kill", "CONTINUE", "KILL"]}),
                    ),
                    (
                        "max_output_chars",
                        json!({"type": "integer", "minimum": 0, "maximum": 1000}),
                    ),
                ]),
                &["dsl_id"],
            ),
            ToolExecutionMode::Interruptible,
            ToolBackend::Local,
        ),
        ToolDefinition::new(
            "dsl_kill",
            "Terminate a DSL job by dsl_id. By default this kills only the DSL CPython worker/job; child exec/download/image jobs continue unless kill_children=true is set.",
            object_schema(
                properties([
                    ("dsl_id", json!({"type": "string"})),
                    ("kill_children", json!({"type": "boolean"})),
                    (
                        "max_output_chars",
                        json!({"type": "integer", "minimum": 0, "maximum": 1000}),
                    ),
                ]),
                &["dsl_id"],
            ),
            ToolExecutionMode::Immediate,
            ToolBackend::Local,
        ),
    ]
}

pub(crate) fn execute_dsl_tool(
    tool_name: &str,
    arguments: &Map<String, Value>,
    context: &ToolExecutionContext<'_>,
) -> Result<Option<Value>, LocalToolError> {
    let result = match tool_name {
        "dsl_start" => dsl_start(arguments, context)?,
        "dsl_wait" => dsl_wait(arguments, context)?,
        "dsl_kill" => dsl_kill(arguments)?,
        _ => return Ok(None),
    };
    Ok(Some(result))
}

fn dsl_start(
    arguments: &Map<String, Value>,
    context: &ToolExecutionContext<'_>,
) -> Result<Value, LocalToolError> {
    let code = string_arg(arguments, "code")?;
    let dsl_id = format!("dsl_{}", nonce());
    let out_path = context
        .data_root
        .join(".stellaclaw")
        .join("output")
        .join("dsl")
        .join(&dsl_id);
    fs::create_dir_all(&out_path).map_err(|error| {
        LocalToolError::Io(format!("failed to create {}: {error}", out_path.display()))
    })?;
    let stdout_path = out_path.join("stdout");
    let stderr_path = out_path.join("stderr");
    let code_path = out_path.join("worker.py");
    fs::write(&code_path, code).map_err(|error| {
        LocalToolError::Io(format!("failed to write {}: {error}", code_path.display()))
    })?;
    let stdout = File::create(&stdout_path).map_err(|error| {
        LocalToolError::Io(format!(
            "failed to create {}: {error}",
            stdout_path.display()
        ))
    })?;
    let stderr = File::create(&stderr_path).map_err(|error| {
        LocalToolError::Io(format!(
            "failed to create {}: {error}",
            stderr_path.display()
        ))
    })?;

    let child = Command::new("python3")
        .arg(&code_path)
        .current_dir(context.workspace_root)
        .stdin(Stdio::null())
        .stdout(Stdio::from(stdout))
        .stderr(Stdio::from(stderr))
        .spawn()
        .map_err(|error| LocalToolError::Io(format!("failed to spawn python3: {error}")))?;
    dsl_jobs().lock().expect("mutex poisoned").insert(
        dsl_id.clone(),
        DslJob {
            child,
            stdout_path,
            stderr_path,
            out_path,
        },
    );

    let wait_timeout = f64_arg_with_default(arguments, "wait_timeout_seconds", 0.0)?;
    if arguments
        .get("return_immediate")
        .and_then(Value::as_bool)
        .unwrap_or(false)
        || wait_timeout <= 0.0
    {
        return dsl_snapshot(&dsl_id);
    }
    wait_for_dsl(
        &dsl_id,
        wait_timeout,
        timeout_action(arguments)?,
        &context.cancel_token,
    )
}

fn dsl_wait(
    arguments: &Map<String, Value>,
    context: &ToolExecutionContext<'_>,
) -> Result<Value, LocalToolError> {
    let dsl_id = string_arg(arguments, "dsl_id")?;
    let wait_timeout = f64_arg_with_default(arguments, "wait_timeout_seconds", 30.0)?;
    wait_for_dsl(
        &dsl_id,
        wait_timeout,
        timeout_action(arguments)?,
        &context.cancel_token,
    )
}

fn dsl_kill(arguments: &Map<String, Value>) -> Result<Value, LocalToolError> {
    let dsl_id = string_arg(arguments, "dsl_id")?;
    let mut jobs = dsl_jobs().lock().expect("mutex poisoned");
    let Some(mut job) = jobs.remove(&dsl_id) else {
        return Ok(json!({"dsl_id": dsl_id, "killed": false, "reason": "unknown_dsl_id"}));
    };
    let _ = job.child.kill();
    let _ = job.child.wait();
    Ok(json!({
        "dsl_id": dsl_id,
        "killed": true,
        "stdout": read_text(&job.stdout_path),
        "stderr": read_text(&job.stderr_path),
        "out_path": job.out_path.display().to_string(),
    }))
}

fn wait_for_dsl(
    dsl_id: &str,
    wait_timeout_seconds: f64,
    on_timeout: TimeoutAction,
    cancel_token: &ToolCancellationToken,
) -> Result<Value, LocalToolError> {
    let deadline = Instant::now() + Duration::from_secs_f64(wait_timeout_seconds);
    loop {
        let snapshot = dsl_snapshot(dsl_id)?;
        if !snapshot
            .get("running")
            .and_then(Value::as_bool)
            .unwrap_or(false)
        {
            return Ok(snapshot);
        }
        if Instant::now() >= deadline {
            if matches!(on_timeout, TimeoutAction::Kill) {
                return dsl_kill(&Map::from_iter([(
                    "dsl_id".to_string(),
                    Value::String(dsl_id.to_string()),
                )]));
            }
            return Ok(json!({"timeout": true, "dsl": snapshot}));
        }
        if cancel_token.is_cancelled() {
            return Ok(json!({"interrupted": true, "dsl": snapshot}));
        }
        thread::sleep(Duration::from_millis(50));
    }
}

fn dsl_snapshot(dsl_id: &str) -> Result<Value, LocalToolError> {
    let mut jobs = dsl_jobs().lock().expect("mutex poisoned");
    let job = jobs
        .get_mut(dsl_id)
        .ok_or_else(|| LocalToolError::InvalidArguments(format!("unknown dsl_id {dsl_id}")))?;
    match job
        .child
        .try_wait()
        .map_err(|error| LocalToolError::Io(format!("failed to poll dsl job: {error}")))?
    {
        Some(status) => {
            let stdout = read_text(&job.stdout_path);
            let stderr = read_text(&job.stderr_path);
            let out_path = job.out_path.clone();
            jobs.remove(dsl_id);
            Ok(json!({
                "dsl_id": dsl_id,
                "running": false,
                "exit_code": status.code(),
                "success": status.success(),
                "stdout": truncate(&stdout, max_output_chars_default()),
                "stderr": truncate(&stderr, max_output_chars_default()),
                "out_path": out_path.display().to_string(),
            }))
        }
        None => Ok(json!({
            "dsl_id": dsl_id,
            "running": true,
            "stdout": truncate(&read_text(&job.stdout_path), max_output_chars_default()),
            "stderr": truncate(&read_text(&job.stderr_path), max_output_chars_default()),
            "out_path": job.out_path.display().to_string(),
        })),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TimeoutAction {
    Continue,
    Kill,
}

fn timeout_action(arguments: &Map<String, Value>) -> Result<TimeoutAction, LocalToolError> {
    let value = string_arg_with_default(arguments, "on_timeout", "continue")?;
    match value.to_ascii_lowercase().as_str() {
        "continue" => Ok(TimeoutAction::Continue),
        "kill" => Ok(TimeoutAction::Kill),
        _ => Err(LocalToolError::InvalidArguments(
            "on_timeout must be continue or kill".to_string(),
        )),
    }
}

fn read_text(path: &std::path::Path) -> String {
    fs::read_to_string(path).unwrap_or_default()
}

fn truncate(text: &str, max_chars: usize) -> String {
    text.chars().take(max_chars).collect()
}

fn max_output_chars_default() -> usize {
    1000
}

fn nonce() -> String {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos().to_string())
        .unwrap_or_else(|_| "0".to_string())
}
