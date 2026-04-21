use crate::config::{ExternalWebSearchConfig, RemoteWorkpathConfig, UpstreamConfig};
use crate::skills::SkillMetadata;
use crate::tool_worker::ToolWorkerJob;
use anyhow::{Context, Result, anyhow};
use crossbeam_channel::{self, Receiver};
use serde::de::DeserializeOwned;
use serde_json::{Map, Value, json};
use std::collections::BTreeMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

mod args;
mod download;
pub(crate) mod dsl;
mod exec;
#[path = "tooling/fs.rs"]
mod file_tools;
mod media;
mod remote;
mod runtime_state;
mod skills;

use args::{f64_arg, string_arg, string_array_arg, usize_arg_with_default};
use download::{
    file_download_cancel_tool, file_download_progress_tool, file_download_start_tool,
    file_download_wait_tool,
};
use dsl::{dsl_kill_tool, dsl_start_tool, dsl_wait_tool};
pub use exec::terminate_all_managed_processes;
#[cfg(test)]
use exec::{ShellSessionMetadata, process_is_running, process_meta_path};
use exec::{shell_close_tool, shell_tool};
#[cfg(test)]
use file_tools::{FILE_READ_MAX_OUTPUT_BYTES, LS_MAX_ENTRIES};
use file_tools::{edit_tool, file_read_tool, file_write_tool, glob_tool, grep_tool, ls_tool};
use media::{
    audio_load_tool, audio_transcribe_tool, image_cancel_tool, image_generate_tool,
    image_load_tool, image_start_tool, image_wait_tool, maybe_add_images_schema, pdf_load_tool,
    pdf_query_tool,
};
#[cfg(test)]
use remote::remote_file_root;
#[cfg(test)]
use remote::resolve_remote_cwd;
use remote::{
    ExecutionTarget, RemoteWorkpathMap, execution_target_arg, remote_schema_property,
    remote_workpath_map, run_remote_command,
};
pub(crate) use runtime_state::active_runtime_state_summary;
use runtime_state::run_interruptible_worker_job;
#[cfg(test)]
use runtime_state::{BackgroundTaskMetadata, write_background_task_metadata};
pub use runtime_state::{RuntimeTaskCleanupReport, terminate_runtime_state_tasks};
use skills::{skill_create_tool, skill_load_tool, skill_update_tool};

type ToolHandler = dyn Fn(Value) -> Result<Value> + Send + Sync + 'static;
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ToolExecutionMode {
    Immediate,
    Interruptible,
}

#[derive(Clone)]
pub struct Tool {
    pub name: String,
    pub description: String,
    pub parameters: Value,
    pub execution_mode: ToolExecutionMode,
    handler: Arc<ToolHandler>,
}

impl Tool {
    pub fn new(
        name: impl Into<String>,
        description: impl Into<String>,
        parameters: Value,
        handler: impl Fn(Value) -> Result<Value> + Send + Sync + 'static,
    ) -> Self {
        Self::new_with_mode(
            ToolExecutionMode::Immediate,
            name,
            description,
            parameters,
            handler,
        )
    }

    pub fn new_interruptible(
        name: impl Into<String>,
        description: impl Into<String>,
        parameters: Value,
        handler: impl Fn(Value) -> Result<Value> + Send + Sync + 'static,
    ) -> Self {
        Self::new_with_mode(
            ToolExecutionMode::Interruptible,
            name,
            description,
            parameters,
            handler,
        )
    }

    pub fn new_with_mode(
        execution_mode: ToolExecutionMode,
        name: impl Into<String>,
        description: impl Into<String>,
        parameters: Value,
        handler: impl Fn(Value) -> Result<Value> + Send + Sync + 'static,
    ) -> Self {
        Self {
            name: name.into(),
            description: description.into(),
            parameters,
            execution_mode,
            handler: Arc::new(handler),
        }
    }

    pub fn as_openai_tool(&self) -> Value {
        let execution_guidance = match self.execution_mode {
            ToolExecutionMode::Immediate => {
                "Execution mode: immediate. This tool returns promptly and does not use a top-level timeout parameter."
            }
            ToolExecutionMode::Interruptible => {
                "Execution mode: interruptible. This tool may wait, but the runtime can interrupt it when a newer user message arrives or the turn hits its timeout observation boundary."
            }
        };
        json!({
            "type": "function",
            "function": {
                "name": self.name,
                "description": format!("{} {}", execution_guidance, self.description),
                "parameters": self.parameters,
            }
        })
    }

    pub fn as_responses_tool(&self) -> Value {
        let execution_guidance = match self.execution_mode {
            ToolExecutionMode::Immediate => {
                "Execution mode: immediate. This tool returns promptly and does not use a top-level timeout parameter."
            }
            ToolExecutionMode::Interruptible => {
                "Execution mode: interruptible. This tool may wait, but the runtime can interrupt it when a newer user message arrives or the turn hits its timeout observation boundary."
            }
        };
        json!({
            "type": "function",
            "name": self.name,
            "description": format!("{} {}", execution_guidance, self.description),
            "parameters": self.parameters,
        })
    }

    pub fn as_claude_tool(&self) -> Value {
        let execution_guidance = match self.execution_mode {
            ToolExecutionMode::Immediate => {
                "Execution mode: immediate. This tool returns promptly and does not use a top-level timeout parameter."
            }
            ToolExecutionMode::Interruptible => {
                "Execution mode: interruptible. This tool may wait, but the runtime can interrupt it when a newer user message arrives or the turn hits its timeout observation boundary."
            }
        };
        json!({
            "name": self.name,
            "description": format!("{} {}", execution_guidance, self.description),
            "input_schema": self.parameters,
        })
    }

    pub fn invoke(&self, arguments: Value) -> Result<Value> {
        (self.handler)(arguments)
    }
}

const MAX_ASSISTANT_TOOL_ERROR_CHARS: usize = 10_000;

fn truncate_tool_error_text(text: &str) -> String {
    let char_count = text.chars().count();
    if char_count <= MAX_ASSISTANT_TOOL_ERROR_CHARS {
        return text.to_string();
    }
    let head_len = MAX_ASSISTANT_TOOL_ERROR_CHARS / 2;
    let tail_len = MAX_ASSISTANT_TOOL_ERROR_CHARS.saturating_sub(head_len);
    let head = text.chars().take(head_len).collect::<String>();
    let tail = text
        .chars()
        .rev()
        .take(tail_len)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect::<String>();
    format!(
        "{head}\n...[tool error truncated: original {char_count} chars, kept first {head_len} and last {tail_len}]...\n{tail}"
    )
}

fn normalize_error_fields(value: &mut Value) {
    match value {
        Value::Object(object) => {
            for (key, value) in object.iter_mut() {
                if key == "error"
                    && let Value::String(text) = value
                {
                    *text = truncate_tool_error_text(text);
                } else {
                    normalize_error_fields(value);
                }
            }
        }
        Value::Array(items) => {
            for item in items {
                normalize_error_fields(item);
            }
        }
        _ => {}
    }
}

pub(super) fn compact_tool_status_fields_for_model(value: &mut Value) {
    match value {
        Value::Object(object) => {
            for value in object.values_mut() {
                compact_tool_status_fields_for_model(value);
            }
            object.retain(|key, value| !is_redundant_status_field_for_model(key, value));
        }
        Value::Array(items) => {
            for item in items {
                compact_tool_status_fields_for_model(item);
            }
        }
        _ => {}
    }
}

fn is_redundant_status_field_for_model(key: &str, value: &Value) -> bool {
    match value {
        Value::Null => matches!(
            key,
            "error"
                | "returncode"
                | "pid"
                | "total_bytes"
                | "http_status"
                | "final_url"
                | "content_type"
        ),
        Value::Bool(false) => matches!(
            key,
            "running" | "completed" | "cancelled" | "failed" | "stdin_closed" | "tty"
        ),
        Value::String(text) if text.is_empty() => matches!(
            key,
            "error" | "stdout" | "stderr" | "final_url" | "content_type"
        ),
        Value::String(text) if key == "remote" && text == "local" => true,
        _ => false,
    }
}

fn normalize_tool_result(mut result: Value) -> String {
    normalize_error_fields(&mut result);
    match result {
        Value::String(text) => text,
        other => serde_json::to_string_pretty(&other).unwrap_or_else(|_| "{}".to_string()),
    }
}

fn resolve_path(path: &str, workspace_root: &Path) -> PathBuf {
    let path_buf = PathBuf::from(path);
    if path_buf.is_absolute() {
        path_buf
    } else {
        workspace_root.join(path_buf)
    }
}

fn canonical_tool_name(tool_name: &str) -> &str {
    match tool_name {
        "read_file" => "file_read",
        "write_file" => "file_write",
        _ => tool_name,
    }
}

#[derive(Default)]
pub struct InterruptSignal {
    flag: AtomicBool,
    subscribers: Mutex<Vec<crossbeam_channel::Sender<()>>>,
}

impl InterruptSignal {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn request(&self) {
        self.flag.store(true, Ordering::SeqCst);
        let mut subscribers = self.subscribers.lock().unwrap();
        subscribers.retain(|subscriber| subscriber.try_send(()).is_ok());
    }

    pub fn clear(&self) {
        self.flag.store(false, Ordering::SeqCst);
    }

    pub fn is_requested(&self) -> bool {
        self.flag.load(Ordering::SeqCst)
    }

    pub fn subscribe(&self) -> Receiver<()> {
        let (sender, receiver) = crossbeam_channel::bounded(1);
        if self.is_requested() {
            let _ = sender.try_send(());
        } else {
            self.subscribers.lock().unwrap().push(sender);
        }
        receiver
    }
}

fn run_remote_apply_patch(
    host: &str,
    remote_workpaths: &BTreeMap<String, String>,
    patch: &str,
    strip: usize,
    reverse: bool,
    check: bool,
) -> Result<Value> {
    let remote_cwd = remote_workpaths
        .get(host)
        .map(String::as_str)
        .unwrap_or("~");
    let mut args = vec![
        "git".to_string(),
        "apply".to_string(),
        "--recount".to_string(),
        "--whitespace=nowarn".to_string(),
        format!("-p{}", strip),
    ];
    if reverse {
        args.push("--reverse".to_string());
    }
    if check {
        args.push("--check".to_string());
    }
    let output = run_remote_command(host, remote_cwd, &args, Some(patch.as_bytes()))?;
    Ok(json!({
        "remote": host,
        "applied": output.status.success(),
        "returncode": output.status.code().unwrap_or(-1),
        "stdout": String::from_utf8_lossy(&output.stdout),
        "stderr": String::from_utf8_lossy(&output.stderr)
    }))
}

fn apply_patch_tool(
    workspace_root: PathBuf,
    remote_workpaths: RemoteWorkpathMap,
    _cancel_flag: Option<Arc<InterruptSignal>>,
) -> Tool {
    Tool::new(
        "apply_patch",
        "Apply a unified diff patch inside the workspace using git apply. The patch must be a valid unified diff.",
        json!({
            "type": "object",
            "properties": {
                "patch": {"type": "string"},
                "strip": {"type": "integer"},
                "reverse": {"type": "boolean"},
                "check": {"type": "boolean"},
                "remote": remote_schema_property()
            },
            "required": ["patch"],
            "additionalProperties": false
        }),
        move |arguments| {
            let arguments = arguments
                .as_object()
                .ok_or_else(|| anyhow!("tool arguments must be an object"))?;
            let patch = string_arg(arguments, "patch")?;
            let strip = usize_arg_with_default(arguments, "strip", 0)?;
            let reverse = arguments
                .get("reverse")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            let check = arguments
                .get("check")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            if let ExecutionTarget::RemoteSsh { host } = execution_target_arg(arguments)? {
                return run_remote_apply_patch(
                    &host,
                    &remote_workpaths,
                    &patch,
                    strip,
                    reverse,
                    check,
                );
            }
            let patch_workspace_root = workspace_root.clone();

            let mut command = Command::new("git");
            command
                .arg("apply")
                .arg("--recount")
                .arg("--whitespace=nowarn")
                .arg(format!("-p{}", strip))
                .current_dir(&patch_workspace_root)
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped());
            if reverse {
                command.arg("--reverse");
            }
            if check {
                command.arg("--check");
            }
            let mut child = command.spawn().context("failed to spawn git apply")?;
            child
                .stdin
                .as_mut()
                .ok_or_else(|| anyhow!("failed to open git apply stdin"))?
                .write_all(patch.as_bytes())
                .context("failed to write patch to git apply stdin")?;
            let _ = child.stdin.take();
            let output = child
                .wait_with_output()
                .context("failed to wait for git apply")?;
            Ok(json!({
                "applied": output.status.success(),
                "returncode": output.status.code().unwrap_or(-1),
                "stdout": String::from_utf8_lossy(&output.stdout),
                "stderr": String::from_utf8_lossy(&output.stderr)
            }))
        },
    )
}

fn web_fetch_tool(runtime_state_root: PathBuf, cancel_flag: Option<Arc<InterruptSignal>>) -> Tool {
    Tool::new_interruptible(
        "web_fetch",
        "Fetch a web page or HTTP resource and return a readable text body. If interrupted by a newer user message or timeout observation, cancel the in-flight fetch. The model must choose timeout_seconds.",
        json!({
            "type": "object",
            "properties": {
                "url": {"type": "string"},
                "timeout_seconds": {"type": "number"},
                "max_chars": {"type": "integer"},
                "headers": {"type": "object"}
            },
            "required": ["url", "timeout_seconds"],
            "additionalProperties": false
        }),
        move |arguments| {
            let arguments = arguments
                .as_object()
                .ok_or_else(|| anyhow!("tool arguments must be an object"))?;
            let url = string_arg(arguments, "url")?;
            let timeout_seconds = f64_arg(arguments, "timeout_seconds")?;
            let max_chars = usize_arg_with_default(arguments, "max_chars", 20_000)?;
            let headers = arguments
                .get("headers")
                .and_then(Value::as_object)
                .cloned()
                .unwrap_or_default();
            let result = run_interruptible_worker_job(
                &runtime_state_root,
                &ToolWorkerJob::WebFetch {
                    url: url.clone(),
                    max_chars,
                    headers,
                },
                timeout_seconds,
                cancel_flag.as_ref(),
            );
            match result {
                Ok(value) => Ok(value),
                Err(error) if error.to_string() == "operation cancelled" => Ok(json!({
                    "url": url,
                    "interrupted": true,
                    "cancelled": true,
                })),
                Err(error) if error.to_string().contains("timed out") => Ok(json!({
                    "url": url,
                    "timed_out": true,
                    "cancelled": true,
                })),
                Err(error) => Err(error),
            }
        },
    )
}

fn web_search_tool(
    runtime_state_root: PathBuf,
    workspace_root: PathBuf,
    search_config: ExternalWebSearchConfig,
    cancel_flag: Option<Arc<InterruptSignal>>,
) -> Tool {
    let mut properties = Map::new();
    properties.insert("query".to_string(), json!({"type": "string"}));
    properties.insert("timeout_seconds".to_string(), json!({"type": "number"}));
    properties.insert("max_results".to_string(), json!({"type": "integer"}));
    maybe_add_images_schema(&mut properties, search_config.supports_vision_input);
    Tool::new_interruptible(
        "web_search",
        "Search the web using the configured search provider and return an answer plus citations. If interrupted by a newer user message or timeout observation, this tool cancels the in-flight search result and returns immediately.",
        Value::Object(Map::from_iter([
            ("type".to_string(), Value::String("object".to_string())),
            ("properties".to_string(), Value::Object(properties)),
            ("required".to_string(), json!(["query", "timeout_seconds"])),
            ("additionalProperties".to_string(), Value::Bool(false)),
        ])),
        move |arguments| {
            let arguments = arguments
                .as_object()
                .ok_or_else(|| anyhow!("tool arguments must be an object"))?;
            let query = string_arg(arguments, "query")?;
            let timeout_seconds = f64_arg(arguments, "timeout_seconds")?;
            let max_results = usize_arg_with_default(arguments, "max_results", 8)?;
            let images = string_array_arg(arguments, "images")?
                .into_iter()
                .map(|path| resolve_path(&path, &workspace_root).display().to_string())
                .collect::<Vec<_>>();
            let mut runtime_search_config = search_config.clone();
            runtime_search_config.timeout_seconds = timeout_seconds;
            let search_result = run_interruptible_worker_job(
                &runtime_state_root,
                &ToolWorkerJob::WebSearch {
                    search_config: runtime_search_config,
                    query: query.clone(),
                    max_results,
                    images,
                },
                timeout_seconds,
                cancel_flag.as_ref(),
            );

            match search_result {
                Ok(value) => Ok(value),
                Err(error) if error.to_string() == "operation cancelled" => Ok(json!({
                    "query": query,
                    "interrupted": true,
                    "cancelled": true,
                })),
                Err(error) if error.to_string().contains("timed out") => Ok(json!({
                    "query": query,
                    "timed_out": true,
                    "cancelled": true,
                })),
                Err(error) => Err(error),
            }
        },
    )
}

struct ToolRegistryBuilder {
    tools: BTreeMap<String, Tool>,
}

impl ToolRegistryBuilder {
    fn new() -> Self {
        Self {
            tools: BTreeMap::new(),
        }
    }

    fn add(&mut self, tool: Tool) -> Result<()> {
        let name = tool.name.clone();
        if self.tools.contains_key(&name) {
            return Err(anyhow!("tool name collision: {}", name));
        }
        self.tools.insert(name, tool);
        Ok(())
    }

    fn finish(self) -> BTreeMap<String, Tool> {
        self.tools
    }
}

pub fn build_tool_registry(
    workspace_root: &Path,
    runtime_state_root: &Path,
    upstream: &UpstreamConfig,
    available_upstreams: &BTreeMap<String, UpstreamConfig>,
    image_tool_upstream: Option<&UpstreamConfig>,
    pdf_tool_upstream: Option<&UpstreamConfig>,
    audio_tool_upstream: Option<&UpstreamConfig>,
    image_generation_tool_upstream: Option<&UpstreamConfig>,
    skill_roots: &[PathBuf],
    skills: &[SkillMetadata],
    extra_tools: &[Tool],
    remote_workpaths: &[RemoteWorkpathConfig],
) -> Result<BTreeMap<String, Tool>> {
    build_tool_registry_with_cancel(
        workspace_root,
        runtime_state_root,
        upstream,
        available_upstreams,
        image_tool_upstream,
        pdf_tool_upstream,
        audio_tool_upstream,
        image_generation_tool_upstream,
        skill_roots,
        skills,
        extra_tools,
        remote_workpaths,
        None,
    )
}

pub fn build_tool_registry_with_cancel(
    workspace_root: &Path,
    runtime_state_root: &Path,
    upstream: &UpstreamConfig,
    available_upstreams: &BTreeMap<String, UpstreamConfig>,
    image_tool_upstream: Option<&UpstreamConfig>,
    pdf_tool_upstream: Option<&UpstreamConfig>,
    audio_tool_upstream: Option<&UpstreamConfig>,
    image_generation_tool_upstream: Option<&UpstreamConfig>,
    skill_roots: &[PathBuf],
    skills: &[SkillMetadata],
    extra_tools: &[Tool],
    remote_workpaths: &[RemoteWorkpathConfig],
    cancel_flag: Option<Arc<InterruptSignal>>,
) -> Result<BTreeMap<String, Tool>> {
    let remote_workpath_configs = remote_workpaths.to_vec();
    let remote_workpaths = remote_workpath_map(remote_workpaths);
    let mut registry = ToolRegistryBuilder::new();
    registry.add(file_read_tool(
        workspace_root.to_path_buf(),
        remote_workpaths.clone(),
        cancel_flag.clone(),
    ))?;
    registry.add(file_write_tool(
        workspace_root.to_path_buf(),
        remote_workpaths.clone(),
        cancel_flag.clone(),
    ))?;
    registry.add(glob_tool(
        workspace_root.to_path_buf(),
        remote_workpaths.clone(),
        cancel_flag.clone(),
    ))?;
    registry.add(grep_tool(
        workspace_root.to_path_buf(),
        remote_workpaths.clone(),
        cancel_flag.clone(),
    ))?;
    registry.add(ls_tool(
        workspace_root.to_path_buf(),
        remote_workpaths.clone(),
        cancel_flag.clone(),
    ))?;
    registry.add(edit_tool(
        workspace_root.to_path_buf(),
        remote_workpaths.clone(),
        cancel_flag.clone(),
    ))?;
    registry.add(shell_tool(
        workspace_root.to_path_buf(),
        runtime_state_root.to_path_buf(),
        remote_workpaths.clone(),
        cancel_flag.clone(),
    ))?;
    registry.add(shell_close_tool(
        runtime_state_root.to_path_buf(),
        cancel_flag.clone(),
    ))?;
    registry.add(dsl_start_tool(
        workspace_root.to_path_buf(),
        runtime_state_root.to_path_buf(),
        upstream.clone(),
        available_upstreams.clone(),
        remote_workpath_configs,
        cancel_flag.clone(),
    ))?;
    registry.add(dsl_wait_tool(
        workspace_root.to_path_buf(),
        runtime_state_root.to_path_buf(),
        cancel_flag.clone(),
    ))?;
    registry.add(dsl_kill_tool(
        workspace_root.to_path_buf(),
        runtime_state_root.to_path_buf(),
        cancel_flag.clone(),
    ))?;
    registry.add(apply_patch_tool(
        workspace_root.to_path_buf(),
        remote_workpaths.clone(),
        cancel_flag.clone(),
    ))?;
    registry.add(file_download_start_tool(
        workspace_root.to_path_buf(),
        runtime_state_root.to_path_buf(),
        cancel_flag.clone(),
    ))?;
    registry.add(file_download_progress_tool(
        runtime_state_root.to_path_buf(),
        cancel_flag.clone(),
    ))?;
    registry.add(file_download_wait_tool(
        runtime_state_root.to_path_buf(),
        cancel_flag.clone(),
    ))?;
    registry.add(file_download_cancel_tool(
        runtime_state_root.to_path_buf(),
        cancel_flag.clone(),
    ))?;
    registry.add(web_fetch_tool(
        runtime_state_root.to_path_buf(),
        cancel_flag.clone(),
    ))?;
    if image_tool_upstream.is_some() {
        registry.add(image_start_tool(
            workspace_root.to_path_buf(),
            runtime_state_root.to_path_buf(),
            upstream.clone(),
            image_tool_upstream.cloned(),
            cancel_flag.clone(),
        ))?;
        registry.add(image_wait_tool(
            runtime_state_root.to_path_buf(),
            cancel_flag.clone(),
        ))?;
        registry.add(image_cancel_tool(
            runtime_state_root.to_path_buf(),
            cancel_flag.clone(),
        ))?;
    } else if upstream.native_image_input {
        registry.add(image_load_tool(
            workspace_root.to_path_buf(),
            upstream.clone(),
            cancel_flag.clone(),
        ))?;
    }
    if let Some(pdf_tool_upstream) = pdf_tool_upstream {
        registry.add(pdf_query_tool(
            workspace_root.to_path_buf(),
            runtime_state_root.to_path_buf(),
            pdf_tool_upstream.clone(),
            cancel_flag.clone(),
        ))?;
    } else if upstream.native_pdf_input {
        registry.add(pdf_load_tool(
            workspace_root.to_path_buf(),
            upstream.clone(),
            cancel_flag.clone(),
        ))?;
    }
    if let Some(audio_tool_upstream) = audio_tool_upstream {
        registry.add(audio_transcribe_tool(
            workspace_root.to_path_buf(),
            runtime_state_root.to_path_buf(),
            audio_tool_upstream.clone(),
            cancel_flag.clone(),
        ))?;
    } else if upstream.native_audio_input {
        registry.add(audio_load_tool(
            workspace_root.to_path_buf(),
            upstream.clone(),
            cancel_flag.clone(),
        ))?;
    }
    if let Some(image_generation_tool_upstream) = image_generation_tool_upstream {
        registry.add(image_generate_tool(
            workspace_root.to_path_buf(),
            runtime_state_root.to_path_buf(),
            image_generation_tool_upstream.clone(),
            cancel_flag.clone(),
        ))?;
    }
    let native_web_search_enabled = upstream
        .native_web_search
        .as_ref()
        .is_some_and(|settings| settings.enabled);
    if !native_web_search_enabled {
        if let Some(web_search_config) = upstream.external_web_search.clone() {
            registry.add(web_search_tool(
                runtime_state_root.to_path_buf(),
                workspace_root.to_path_buf(),
                web_search_config,
                cancel_flag.clone(),
            ))?;
        }
    }

    if !skills.is_empty() {
        registry.add(skill_load_tool(skills, cancel_flag.clone())?)?;
    }

    if !skill_roots.is_empty() {
        registry.add(skill_create_tool(
            workspace_root.to_path_buf(),
            skill_roots.to_vec(),
        ))?;
        registry.add(skill_update_tool(
            workspace_root.to_path_buf(),
            skill_roots.to_vec(),
        ))?;
    }

    for tool in extra_tools {
        registry.add(tool.clone())?;
    }
    Ok(registry.finish())
}

pub fn execute_tool_call(
    registry: &BTreeMap<String, Tool>,
    tool_name: &str,
    raw_arguments: Option<&str>,
) -> String {
    let normalized_name = canonical_tool_name(tool_name);
    let Some(tool) = registry.get(normalized_name) else {
        return normalize_tool_result(json!({"error": format!("unknown tool: {}", tool_name)}));
    };

    execute_tool(tool, raw_arguments)
}

pub fn execute_tool(tool: &Tool, raw_arguments: Option<&str>) -> String {
    let arguments = match raw_arguments {
        Some(text) if !text.trim().is_empty() => match serde_json::from_str::<Value>(text) {
            Ok(value) => value,
            Err(error) => {
                return normalize_tool_result(
                    json!({"error": format!("invalid tool arguments: {}", error)}),
                );
            }
        },
        _ => Value::Object(Map::new()),
    };

    match tool.invoke(arguments) {
        Ok(result) => normalize_tool_result(result),
        Err(error) => normalize_tool_result(json!({"error": error.to_string(), "tool": tool.name})),
    }
}

pub mod macro_support {
    use super::*;
    use serde::Serialize;

    pub fn normalize_type_name(type_name: &str) -> String {
        type_name.chars().filter(|ch| !ch.is_whitespace()).collect()
    }

    pub fn schema_for_type_name(type_name: &str) -> Value {
        let normalized = normalize_type_name(type_name);
        let normalized = if normalized.starts_with("Option<") && normalized.ends_with('>') {
            &normalized["Option<".len()..normalized.len() - 1]
        } else {
            normalized.as_str()
        };

        match normalized {
            "i8" | "i16" | "i32" | "i64" | "i128" | "isize" | "u8" | "u16" | "u32" | "u64"
            | "u128" | "usize" => json!({"type": "integer"}),
            "f32" | "f64" => json!({"type": "number"}),
            "bool" => json!({"type": "boolean"}),
            "String" | "&str" | "str" => json!({"type": "string"}),
            _ if normalized.starts_with("Vec<") => json!({"type": "array"}),
            _ if normalized.starts_with("HashMap<")
                || normalized.starts_with("BTreeMap<")
                || normalized.starts_with("serde_json::Map<") =>
            {
                json!({"type": "object"})
            }
            _ if normalized == "Value" || normalized.ends_with("::Value") => json!({}),
            _ => json!({}),
        }
    }

    pub fn type_is_optional(type_name: &str) -> bool {
        let normalized = normalize_type_name(type_name);
        normalized.starts_with("Option<") && normalized.ends_with('>')
    }

    pub fn parse_argument<T: DeserializeOwned>(
        arguments: &Map<String, Value>,
        key: &str,
        optional: bool,
    ) -> Result<T> {
        let value = match arguments.get(key) {
            Some(value) => value.clone(),
            None if optional => Value::Null,
            None => return Err(anyhow!("missing required argument: {}", key)),
        };
        serde_json::from_value(value)
            .with_context(|| format!("failed to parse argument {} from JSON", key))
    }

    pub fn result_to_value<T: Serialize>(value: T) -> Result<Value> {
        serde_json::to_value(value).context("failed to serialize tool result")
    }

    pub fn arguments_object(arguments: &Value) -> Result<&Map<String, Value>> {
        arguments
            .as_object()
            .ok_or_else(|| anyhow!("tool arguments must be an object"))
    }
}

#[cfg(test)]
mod tests {
    use super::{
        BackgroundTaskMetadata, ExecutionTarget, FILE_READ_MAX_OUTPUT_BYTES, LS_MAX_ENTRIES,
        ShellSessionMetadata, Tool, ToolExecutionMode, active_runtime_state_summary,
        build_tool_registry_with_cancel, compact_tool_status_fields_for_model, execute_tool_call,
        execution_target_arg, normalize_tool_result, process_is_running, process_meta_path,
        remote_file_root, resolve_remote_cwd, terminate_runtime_state_tasks,
        write_background_task_metadata,
    };
    use crate::config::{
        AuthCredentialsStoreMode, RemoteWorkpathConfig, UpstreamApiKind, UpstreamAuthKind,
        UpstreamConfig,
    };
    use serde_json::json;
    use std::collections::BTreeMap;
    use std::ffi::OsString;
    use std::fs;
    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt;
    use std::path::PathBuf;
    use std::process::{Command, Stdio};
    use std::sync::{Mutex, OnceLock};
    use std::thread;
    use std::time::Duration;
    use tempfile::TempDir;

    fn test_upstream() -> UpstreamConfig {
        UpstreamConfig {
            base_url: "https://example.com".to_string(),
            model: "demo".to_string(),
            api_kind: UpstreamApiKind::Responses,
            auth_kind: UpstreamAuthKind::ApiKey,
            supports_vision_input: false,
            supports_pdf_input: false,
            supports_audio_input: false,
            api_key: None,
            api_key_env: "OPENAI_API_KEY".to_string(),
            chat_completions_path: "/responses".to_string(),
            codex_home: None,
            codex_auth: None,
            auth_credentials_store_mode: AuthCredentialsStoreMode::Auto,
            timeout_seconds: 30.0,
            retry_mode: Default::default(),
            context_window_tokens: 200_000,
            cache_control: None,
            prompt_cache_retention: None,
            prompt_cache_key: None,
            reasoning: None,
            headers: serde_json::Map::new(),
            native_web_search: None,
            external_web_search: None,
            native_image_input: false,
            native_pdf_input: false,
            native_audio_input: false,
            native_image_generation: false,
            token_estimation: None,
        }
    }

    fn env_test_lock() -> &'static Mutex<()> {
        static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        LOCK.get_or_init(|| Mutex::new(()))
    }

    #[test]
    fn normalize_tool_result_truncates_large_error_fields() {
        let result = normalize_tool_result(json!({
            "error": "x".repeat(12_000),
            "stdout": "ok"
        }));
        assert!(result.contains("tool error truncated"));
        assert!(result.contains("\"stdout\": \"ok\""));
        assert!(result.len() < 11_000);
    }

    #[test]
    fn compact_tool_status_fields_removes_default_status_noise() {
        let mut value = json!({
            "exec_id": "abc",
            "remote": "local",
            "running": false,
            "completed": true,
            "cancelled": false,
            "failed": false,
            "returncode": null,
            "error": null,
            "stdout": "",
            "process": {
                "running": true,
                "completed": false,
                "error": null
            }
        });

        compact_tool_status_fields_for_model(&mut value);

        assert_eq!(value["completed"], json!(true));
        assert_eq!(value["process"]["running"], json!(true));
        assert!(value.get("remote").is_none());
        assert!(value.get("running").is_none());
        assert!(value.get("cancelled").is_none());
        assert!(value.get("failed").is_none());
        assert!(value.get("returncode").is_none());
        assert!(value.get("error").is_none());
        assert!(value.get("stdout").is_none());
        assert!(value["process"].get("completed").is_none());
        assert!(value["process"].get("error").is_none());
    }

    struct EnvVarGuard {
        key: &'static str,
        previous: Option<OsString>,
    }

    impl EnvVarGuard {
        fn set(key: &'static str, value: impl AsRef<std::ffi::OsStr>) -> Self {
            let previous = std::env::var_os(key);
            unsafe {
                std::env::set_var(key, value);
            }
            Self { key, previous }
        }
    }

    impl Drop for EnvVarGuard {
        fn drop(&mut self) {
            unsafe {
                if let Some(previous) = &self.previous {
                    std::env::set_var(self.key, previous);
                } else {
                    std::env::remove_var(self.key);
                }
            }
        }
    }

    #[cfg(unix)]
    fn write_fake_ssh(temp_dir: &TempDir) -> PathBuf {
        write_fake_ssh_with_path(temp_dir, "exec sh -c \"$remote_command\"")
    }

    #[cfg(unix)]
    fn write_fake_ssh_with_path(temp_dir: &TempDir, path_line: &str) -> PathBuf {
        let path = temp_dir.path().join("fake-ssh");
        fs::write(
            &path,
            format!(
                r#"#!/bin/sh
while [ "$1" = "-o" ]; do
  shift 2
done
if [ "$1" = "-T" ] || [ "$1" = "-tt" ]; then
  shift
fi
shift
remote_command="$*"
{path_line}
"#
            ),
        )
        .unwrap();
        let mut permissions = fs::metadata(&path).unwrap().permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&path, permissions).unwrap();
        path
    }

    #[test]
    fn openai_tool_description_includes_execution_mode_guidance() {
        let immediate = Tool::new(
            "immediate_demo",
            "Immediate demo tool.",
            json!({"type": "object", "properties": {}, "additionalProperties": false}),
            |_| Ok(json!({"ok": true})),
        );
        let interruptible = Tool::new_interruptible(
            "interruptible_demo",
            "Interruptible demo tool.",
            json!({"type": "object", "properties": {}, "additionalProperties": false}),
            |_| Ok(json!({"ok": true})),
        );

        let immediate_description = immediate
            .as_openai_tool()
            .get("function")
            .and_then(|value| value.get("description"))
            .and_then(|value| value.as_str())
            .unwrap_or_default()
            .to_string();
        let interruptible_description = interruptible
            .as_openai_tool()
            .get("function")
            .and_then(|value| value.get("description"))
            .and_then(|value| value.as_str())
            .unwrap_or_default()
            .to_string();

        assert!(immediate_description.contains("Execution mode: immediate."));
        assert!(interruptible_description.contains("Execution mode: interruptible."));
    }

    #[test]
    fn responses_tool_schema_is_flattened() {
        let tool = Tool::new(
            "demo",
            "A demo tool.",
            json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string" }
                }
            }),
            |_| Ok(json!({"ok": true})),
        );

        let value = tool.as_responses_tool();
        assert_eq!(value["type"], "function");
        assert_eq!(value["name"], "demo");
        assert_eq!(value["parameters"]["type"], "object");
        assert!(value.get("function").is_none());
    }

    #[test]
    fn execute_tool_call_accepts_legacy_file_tool_names() {
        let temp_dir = TempDir::new().unwrap();
        let workspace_root = temp_dir.path().join("workspace");
        fs::create_dir_all(&workspace_root).unwrap();
        let runtime_state_root = temp_dir.path().join("runtime");
        fs::create_dir_all(&runtime_state_root).unwrap();
        let upstream = UpstreamConfig {
            base_url: "https://example.com".to_string(),
            model: "demo".to_string(),
            api_kind: UpstreamApiKind::Responses,
            auth_kind: UpstreamAuthKind::ApiKey,
            supports_vision_input: false,
            supports_pdf_input: false,
            supports_audio_input: false,
            api_key: None,
            api_key_env: "OPENAI_API_KEY".to_string(),
            chat_completions_path: "/responses".to_string(),
            codex_home: None,
            codex_auth: None,
            auth_credentials_store_mode: AuthCredentialsStoreMode::Auto,
            timeout_seconds: 30.0,
            retry_mode: Default::default(),
            context_window_tokens: 200_000,
            cache_control: None,
            prompt_cache_retention: None,
            prompt_cache_key: None,
            reasoning: None,
            headers: serde_json::Map::new(),
            native_web_search: None,
            external_web_search: None,
            native_image_input: false,
            native_pdf_input: false,
            native_audio_input: false,
            native_image_generation: false,
            token_estimation: None,
        };
        let registry = build_tool_registry_with_cancel(
            &workspace_root,
            &runtime_state_root,
            &upstream,
            &BTreeMap::new(),
            None,
            None,
            None,
            None,
            &Vec::<PathBuf>::new(),
            &[],
            &[],
            &[],
            None,
        )
        .unwrap();

        let write_result = execute_tool_call(
            &registry,
            "write_file",
            Some(r#"{"path":"legacy.txt","content":"hello"}"#),
        );
        assert!(write_result.contains("legacy.txt"));

        let read_result = execute_tool_call(
            &registry,
            "read_file",
            Some(r#"{"path":"legacy.txt","offset_lines":0,"limit_lines":10}"#),
        );
        assert!(read_result.contains("1: hello"));
    }

    #[test]
    fn remote_file_root_uses_registered_workpath() {
        let mut workpaths = BTreeMap::new();
        workpaths.insert("wuwen-dev3".to_string(), "/srv/project".to_string());
        let arguments = json!({"path": "src/main.rs"}).as_object().cloned().unwrap();

        let (remote_cwd, workspace_root) =
            remote_file_root("wuwen-dev3", "edit", &arguments, &workpaths).unwrap();

        assert_eq!(remote_cwd, "/srv/project");
        assert_eq!(workspace_root, ".");
    }

    #[test]
    fn remote_file_root_without_workpath_defaults_to_home_for_relative_path() {
        let workpaths = BTreeMap::new();
        let relative_args = json!({"path": "src/main.rs"}).as_object().cloned().unwrap();
        let (relative_cwd, relative_root) =
            remote_file_root("wuwen-dev3", "edit", &relative_args, &workpaths).unwrap();
        assert_eq!(relative_cwd, "~");
        assert_eq!(relative_root, ".");

        let absolute_args = json!({"path": "/srv/project/src/main.rs"})
            .as_object()
            .cloned()
            .unwrap();
        let (remote_cwd, workspace_root) =
            remote_file_root("wuwen-dev3", "edit", &absolute_args, &workpaths).unwrap();
        assert_eq!(remote_cwd, "/");
        assert_eq!(workspace_root, "/");
    }

    #[test]
    fn remote_file_root_without_path_defaults_to_home() {
        let workpaths = BTreeMap::new();
        let arguments = json!({}).as_object().cloned().unwrap();
        let (remote_cwd, workspace_root) =
            remote_file_root("wuwen-dev3", "ls", &arguments, &workpaths).unwrap();
        assert_eq!(remote_cwd, "~");
        assert_eq!(workspace_root, ".");
    }

    #[test]
    fn remote_cwd_uses_workpath_for_relative_paths() {
        let mut workpaths = BTreeMap::new();
        workpaths.insert("wuwen-dev3".to_string(), "/srv/project".to_string());

        assert_eq!(
            resolve_remote_cwd("wuwen-dev3", None, &workpaths).unwrap(),
            "/srv/project"
        );
        assert_eq!(
            resolve_remote_cwd("wuwen-dev3", Some(""), &workpaths).unwrap(),
            "/srv/project"
        );
        assert_eq!(
            resolve_remote_cwd("wuwen-dev3", Some("scripts"), &workpaths).unwrap(),
            "/srv/project/scripts"
        );
        assert_eq!(
            resolve_remote_cwd("wuwen-dev3", Some("/tmp"), &workpaths).unwrap(),
            "/tmp"
        );
        assert_eq!(
            resolve_remote_cwd("wuwen-dev6", None, &workpaths).unwrap(),
            "~"
        );
        assert_eq!(
            resolve_remote_cwd("wuwen-dev6", Some(""), &workpaths).unwrap(),
            "~"
        );
        assert_eq!(
            resolve_remote_cwd("wuwen-dev6", Some("scripts"), &workpaths).unwrap(),
            "~/scripts"
        );
    }

    #[test]
    fn glob_grep_and_ls_tools_explore_workspace() {
        let temp_dir = TempDir::new().unwrap();
        let workspace_root = temp_dir.path().join("workspace");
        let src_dir = workspace_root.join("src");
        fs::create_dir_all(&src_dir).unwrap();
        fs::write(
            src_dir.join("main.rs"),
            "fn main() { println!(\"hello\"); }\n",
        )
        .unwrap();
        fs::write(src_dir.join("lib.rs"), "pub fn helper() {}\n").unwrap();
        let runtime_state_root = temp_dir.path().join("runtime");
        fs::create_dir_all(&runtime_state_root).unwrap();
        let upstream = UpstreamConfig {
            base_url: "https://example.com".to_string(),
            model: "demo".to_string(),
            api_kind: UpstreamApiKind::Responses,
            auth_kind: UpstreamAuthKind::ApiKey,
            supports_vision_input: false,
            supports_pdf_input: false,
            supports_audio_input: false,
            api_key: None,
            api_key_env: "OPENAI_API_KEY".to_string(),
            chat_completions_path: "/responses".to_string(),
            codex_home: None,
            codex_auth: None,
            auth_credentials_store_mode: AuthCredentialsStoreMode::Auto,
            timeout_seconds: 30.0,
            retry_mode: Default::default(),
            context_window_tokens: 200_000,
            cache_control: None,
            prompt_cache_retention: None,
            prompt_cache_key: None,
            reasoning: None,
            headers: serde_json::Map::new(),
            native_web_search: None,
            external_web_search: None,
            native_image_input: false,
            native_pdf_input: false,
            native_audio_input: false,
            native_image_generation: false,
            token_estimation: None,
        };
        let registry = build_tool_registry_with_cancel(
            &workspace_root,
            &runtime_state_root,
            &upstream,
            &BTreeMap::new(),
            None,
            None,
            None,
            None,
            &Vec::<PathBuf>::new(),
            &[],
            &[],
            &[],
            None,
        )
        .unwrap();

        let glob_result = registry["glob"]
            .invoke(json!({"pattern":"src/**/*.rs"}))
            .unwrap();
        assert_eq!(glob_result["num_files"].as_u64(), Some(2));

        let grep_result = registry["grep"]
            .invoke(json!({"pattern":"println!", "path":"src"}))
            .unwrap();
        assert_eq!(grep_result["num_files"].as_u64(), Some(1));
        assert!(
            grep_result["filenames"][0]
                .as_str()
                .is_some_and(|path| path.ends_with("main.rs"))
        );

        let ls_result = registry["ls"].invoke(json!({"path":"src"})).unwrap();
        let ls_result = ls_result.as_str().unwrap();
        assert!(ls_result.contains("num_entries: 2"));
        assert!(!ls_result.contains("truncated: false"));
        assert!(ls_result.contains("- main.rs"));
        assert!(ls_result.contains("- lib.rs"));
        assert!(!ls_result.contains("\"entries\""));
        assert!(!ls_result.contains("\"type\""));
    }

    #[test]
    fn supported_tools_expose_optional_remote_parameter() {
        let temp_dir = TempDir::new().unwrap();
        let workspace_root = temp_dir.path().join("workspace");
        fs::create_dir_all(&workspace_root).unwrap();
        let runtime_state_root = temp_dir.path().join("runtime");
        fs::create_dir_all(&runtime_state_root).unwrap();
        let upstream = test_upstream();
        let remote_workpaths = vec![RemoteWorkpathConfig {
            host: "fake-host".to_string(),
            path: workspace_root.display().to_string(),
            description: "test remote workspace".to_string(),
        }];
        let registry = build_tool_registry_with_cancel(
            &workspace_root,
            &runtime_state_root,
            &upstream,
            &BTreeMap::new(),
            None,
            None,
            None,
            None,
            &Vec::<PathBuf>::new(),
            &[],
            &[],
            &remote_workpaths,
            None,
        )
        .unwrap();

        for name in [
            "file_read",
            "file_write",
            "glob",
            "grep",
            "ls",
            "edit",
            "shell",
            "apply_patch",
        ] {
            let tool = registry.get(name).expect("tool should be registered");
            let remote = &tool.parameters["properties"]["remote"];
            assert_eq!(remote["type"], "string");
            assert!(
                remote["description"]
                    .as_str()
                    .unwrap()
                    .contains("local SSH alias")
            );
            assert!(
                !tool.description.contains("remote=")
                    && !tool.description.contains("Optional remote")
                    && !tool.description.contains("SSH"),
                "remote guidance should stay in system prompt, not {name} description"
            );
            let required = tool
                .parameters
                .get("required")
                .and_then(|value| value.as_array())
                .cloned()
                .unwrap_or_default();
            assert!(
                !required.iter().any(|item| item.as_str() == Some("remote")),
                "remote must stay optional for {name}"
            );
        }
    }

    #[test]
    fn shell_close_does_not_expose_remote_parameter() {
        let temp_dir = TempDir::new().unwrap();
        let workspace_root = temp_dir.path().join("workspace");
        fs::create_dir_all(&workspace_root).unwrap();
        let runtime_state_root = temp_dir.path().join("runtime");
        fs::create_dir_all(&runtime_state_root).unwrap();
        let upstream = test_upstream();
        let remote_workpaths = vec![RemoteWorkpathConfig {
            host: "fake-host".to_string(),
            path: workspace_root.display().to_string(),
            description: "test remote workspace".to_string(),
        }];
        let registry = build_tool_registry_with_cancel(
            &workspace_root,
            &runtime_state_root,
            &upstream,
            &BTreeMap::new(),
            None,
            None,
            None,
            None,
            &Vec::<PathBuf>::new(),
            &[],
            &[],
            &remote_workpaths,
            None,
        )
        .unwrap();

        let tool = registry
            .get("shell_close")
            .expect("shell_close should be registered");
        assert!(tool.parameters["properties"].get("remote").is_none());
    }

    #[test]
    fn long_running_tool_families_use_start_wait_and_terminate_lifecycle() {
        let temp_dir = TempDir::new().unwrap();
        let workspace_root = temp_dir.path().join("workspace");
        fs::create_dir_all(&workspace_root).unwrap();
        let runtime_state_root = temp_dir.path().join("runtime");
        fs::create_dir_all(&runtime_state_root).unwrap();
        let upstream = test_upstream();
        let registry = build_tool_registry_with_cancel(
            &workspace_root,
            &runtime_state_root,
            &upstream,
            &BTreeMap::new(),
            Some(&upstream),
            None,
            None,
            None,
            &Vec::<PathBuf>::new(),
            &[],
            &[],
            &[],
            None,
        )
        .unwrap();

        let shell = registry.get("shell").expect("shell registered");
        assert_eq!(shell.execution_mode, ToolExecutionMode::Interruptible);
        assert!(shell.parameters["properties"].get("session_id").is_some());
        assert!(shell.parameters["properties"].get("command").is_some());
        assert!(shell.parameters["properties"].get("input").is_some());
        assert!(shell.parameters["properties"].get("wait_ms").is_some());
        assert!(shell.parameters["properties"].get("remote").is_some());

        let shell_close = registry.get("shell_close").expect("shell_close registered");
        assert_eq!(shell_close.execution_mode, ToolExecutionMode::Immediate);
        assert!(
            shell_close.parameters["properties"]
                .get("session_id")
                .is_some()
        );

        let download_start = registry
            .get("file_download_start")
            .expect("file_download_start registered");
        assert_eq!(
            download_start.execution_mode,
            ToolExecutionMode::Interruptible
        );
        assert!(download_start.parameters["properties"].get("url").is_some());
        assert!(
            download_start.parameters["properties"]
                .get("return_immediate")
                .is_some()
        );
        assert!(
            download_start.parameters["properties"]
                .get("wait_timeout_seconds")
                .is_some()
        );
        assert!(
            download_start.parameters["properties"]
                .get("on_timeout")
                .is_some()
        );
        assert_start_description_keeps_wait_policy_centralized(download_start);

        let download_wait = registry
            .get("file_download_wait")
            .expect("file_download_wait registered");
        assert_eq!(
            download_wait.execution_mode,
            ToolExecutionMode::Interruptible
        );
        assert!(
            download_wait.parameters["properties"]
                .get("download_id")
                .is_some()
        );
        assert!(
            download_wait.parameters["properties"]
                .get("wait_timeout_seconds")
                .is_some()
        );
        assert!(
            download_wait.parameters["properties"]
                .get("on_timeout")
                .is_some()
        );

        let download_cancel = registry
            .get("file_download_cancel")
            .expect("file_download_cancel registered");
        assert_eq!(download_cancel.execution_mode, ToolExecutionMode::Immediate);
        assert!(
            download_cancel.parameters["properties"]
                .get("download_id")
                .is_some()
        );

        let image_start = registry.get("image_start").expect("image_start registered");
        assert_eq!(image_start.execution_mode, ToolExecutionMode::Interruptible);
        assert!(image_start.parameters["properties"].get("path").is_some());
        assert!(
            image_start.parameters["properties"]
                .get("return_immediate")
                .is_some()
        );
        assert!(
            image_start.parameters["properties"]
                .get("wait_timeout_seconds")
                .is_some()
        );
        assert!(
            image_start.parameters["properties"]
                .get("on_timeout")
                .is_some()
        );
        assert_start_description_keeps_wait_policy_centralized(image_start);

        let image_wait = registry.get("image_wait").expect("image_wait registered");
        assert_eq!(image_wait.execution_mode, ToolExecutionMode::Interruptible);
        assert!(
            image_wait.parameters["properties"]
                .get("image_id")
                .is_some()
        );
        assert!(
            image_wait.parameters["properties"]
                .get("wait_timeout_seconds")
                .is_some()
        );
        assert!(
            image_wait.parameters["properties"]
                .get("on_timeout")
                .is_some()
        );

        let image_cancel = registry
            .get("image_cancel")
            .expect("image_cancel registered");
        assert_eq!(image_cancel.execution_mode, ToolExecutionMode::Immediate);
        assert!(
            image_cancel.parameters["properties"]
                .get("image_id")
                .is_some()
        );

        let dsl_start = registry.get("dsl_start").expect("dsl_start registered");
        assert_eq!(dsl_start.execution_mode, ToolExecutionMode::Interruptible);
        assert!(dsl_start.parameters["properties"].get("code").is_some());
        assert!(
            dsl_start.parameters["properties"]
                .get("return_immediate")
                .is_some()
        );
        assert!(
            dsl_start.parameters["properties"]
                .get("wait_timeout_seconds")
                .is_some()
        );
        assert!(
            dsl_start.parameters["properties"]
                .get("on_timeout")
                .is_some()
        );
        assert_start_description_keeps_wait_policy_centralized(dsl_start);

        let dsl_wait = registry.get("dsl_wait").expect("dsl_wait registered");
        assert_eq!(dsl_wait.execution_mode, ToolExecutionMode::Interruptible);
        assert!(dsl_wait.parameters["properties"].get("dsl_id").is_some());

        let dsl_kill = registry.get("dsl_kill").expect("dsl_kill registered");
        assert_eq!(dsl_kill.execution_mode, ToolExecutionMode::Immediate);
        assert!(dsl_kill.parameters["properties"].get("dsl_id").is_some());
        assert!(
            dsl_kill.parameters["properties"]
                .get("kill_children")
                .is_some()
        );
    }

    fn assert_start_description_keeps_wait_policy_centralized(tool: &Tool) {
        for phrase in [
            "By default",
            "return_immediate",
            "wait_timeout_seconds",
            "on_timeout",
        ] {
            assert!(
                !tool.description.contains(phrase),
                "start tool {} should leave shared wait policy in the system prompt",
                tool.name
            );
        }
    }

    #[test]
    fn empty_remote_argument_is_treated_as_local() {
        let arguments = json!({"remote": ""});
        let target = execution_target_arg(arguments.as_object().unwrap()).unwrap();
        assert_eq!(target, ExecutionTarget::Local);
    }

    #[test]
    fn placeholder_remote_argument_is_rejected() {
        let arguments = json!({"remote": "host"});
        let error = execution_target_arg(arguments.as_object().unwrap()).unwrap_err();
        assert!(format!("{error:#}").contains("actual SSH host alias"));
    }

    #[cfg(unix)]
    #[test]
    fn remote_filesystem_tools_use_ssh_and_omit_local_state() {
        let _env_guard = env_test_lock()
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let temp_dir = TempDir::new().unwrap();
        let fake_ssh = write_fake_ssh(&temp_dir);
        let _ssh_guard = EnvVarGuard::set("AGENT_FRAME_SSH_BIN", fake_ssh.as_os_str());
        let workspace_root = temp_dir.path().join("workspace");
        fs::create_dir_all(workspace_root.join("src")).unwrap();
        let runtime_state_root = temp_dir.path().join("runtime");
        fs::create_dir_all(&runtime_state_root).unwrap();
        let upstream = test_upstream();
        let remote_workpaths = vec![RemoteWorkpathConfig {
            host: "fake-host".to_string(),
            path: workspace_root.display().to_string(),
            description: "test remote workspace".to_string(),
        }];
        let registry = build_tool_registry_with_cancel(
            &workspace_root,
            &runtime_state_root,
            &upstream,
            &BTreeMap::new(),
            None,
            None,
            None,
            None,
            &Vec::<PathBuf>::new(),
            &[],
            &[],
            &remote_workpaths,
            None,
        )
        .unwrap();

        let write_result = registry["file_write"]
            .invoke(json!({
                "file_path": "src/remote.txt",
                "content": "alpha\nbeta\n",
                "remote": "fake-host"
            }))
            .unwrap();
        assert_eq!(write_result["remote"].as_str(), Some("fake-host"));
        assert_eq!(
            fs::read_to_string(workspace_root.join("src/remote.txt")).unwrap(),
            "alpha\nbeta\n"
        );

        let read_result = registry["file_read"]
            .invoke(json!({
                "file_path": "src/remote.txt",
                "offset": 1,
                "limit": 10,
                "remote": "fake-host"
            }))
            .unwrap();
        assert_eq!(read_result["remote"].as_str(), Some("fake-host"));
        assert!(
            read_result["content"]
                .as_str()
                .unwrap()
                .contains("1: alpha")
        );

        let grep_result = registry["grep"]
            .invoke(json!({
                "pattern": "beta",
                "path": "src",
                "include": "*.txt",
                "remote": "fake-host"
            }))
            .unwrap();
        assert_eq!(grep_result["num_files"].as_u64(), Some(1));

        let ls_result = registry["ls"]
            .invoke(json!({
                "path": "src",
                "remote": "fake-host"
            }))
            .unwrap();
        let ls_result = ls_result.as_str().unwrap();
        assert!(ls_result.contains("remote: fake-host"));
        assert!(ls_result.contains("num_entries: 1"));
        assert!(!ls_result.contains("truncated: false"));
        assert!(ls_result.contains("- remote.txt"));

        let remote_crowded_dir = workspace_root.join("crowded");
        fs::create_dir_all(&remote_crowded_dir).unwrap();
        for index in 0..(LS_MAX_ENTRIES + 5) {
            fs::write(
                remote_crowded_dir.join(format!("file_{index:04}.txt")),
                "data\n",
            )
            .unwrap();
        }
        let remote_crowded_result = registry["ls"]
            .invoke(json!({
                "path": "crowded",
                "remote": "fake-host"
            }))
            .unwrap();
        let remote_crowded_result = remote_crowded_result.as_str().unwrap();
        assert!(remote_crowded_result.contains("remote: fake-host"));
        assert!(remote_crowded_result.contains("num_entries: >1000"));
        assert!(remote_crowded_result.contains("truncated: true"));
        assert!(!remote_crowded_result.contains("\"entries\""));
    }

    #[cfg(unix)]
    #[test]
    fn remote_file_tools_avoid_login_shell_banner_pollution() {
        let _env_guard = env_test_lock()
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let temp_dir = TempDir::new().unwrap();
        let fake_ssh = write_fake_ssh_with_path(
            &temp_dir,
            r#"case "$remote_command" in
  *-lc*) printf 'REMOTE LOGIN BANNER\n' ;;
esac
exec sh -c "$remote_command""#,
        );
        let _ssh_guard = EnvVarGuard::set("AGENT_FRAME_SSH_BIN", fake_ssh.as_os_str());
        let workspace_root = temp_dir.path().join("workspace");
        fs::create_dir_all(workspace_root.join("src")).unwrap();
        fs::write(workspace_root.join("src/remote.txt"), "clean\n").unwrap();
        let runtime_state_root = temp_dir.path().join("runtime");
        fs::create_dir_all(&runtime_state_root).unwrap();
        let upstream = test_upstream();
        let remote_workpaths = vec![RemoteWorkpathConfig {
            host: "fake-host".to_string(),
            path: workspace_root.display().to_string(),
            description: "test remote workspace".to_string(),
        }];
        let registry = build_tool_registry_with_cancel(
            &workspace_root,
            &runtime_state_root,
            &upstream,
            &BTreeMap::new(),
            None,
            None,
            None,
            None,
            &Vec::<PathBuf>::new(),
            &[],
            &[],
            &remote_workpaths,
            None,
        )
        .unwrap();

        let read_result = registry["file_read"]
            .invoke(json!({
                "file_path": "src/remote.txt",
                "remote": "fake-host"
            }))
            .unwrap();
        assert_eq!(read_result["remote"].as_str(), Some("fake-host"));
        assert_eq!(read_result["content"].as_str(), Some("1: clean"));
    }

    #[cfg(unix)]
    #[test]
    fn remote_file_tools_fall_back_to_python_when_python3_is_missing() {
        let _env_guard = env_test_lock()
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let temp_dir = TempDir::new().unwrap();
        let bin_dir = temp_dir.path().join("bin");
        fs::create_dir_all(&bin_dir).unwrap();
        std::os::unix::fs::symlink("/bin/sh", bin_dir.join("sh")).unwrap();
        let python3_path = std::env::var("PATH")
            .unwrap_or_default()
            .split(':')
            .map(|path| std::path::Path::new(path).join("python3"))
            .find(|path| path.is_file())
            .expect("test host must have python3 on PATH");
        std::os::unix::fs::symlink(python3_path, bin_dir.join("python")).unwrap();
        let fake_ssh = write_fake_ssh_with_path(
            &temp_dir,
            &format!(
                "PATH='{}' exec sh -c \"$remote_command\"",
                bin_dir.display()
            ),
        );
        let _ssh_guard = EnvVarGuard::set("AGENT_FRAME_SSH_BIN", fake_ssh.as_os_str());
        let workspace_root = temp_dir.path().join("workspace");
        fs::create_dir_all(workspace_root.join("src")).unwrap();
        fs::write(workspace_root.join("src/fallback.txt"), "ok\n").unwrap();
        let runtime_state_root = temp_dir.path().join("runtime");
        fs::create_dir_all(&runtime_state_root).unwrap();
        let upstream = test_upstream();
        let remote_workpaths = vec![RemoteWorkpathConfig {
            host: "fake-host".to_string(),
            path: workspace_root.display().to_string(),
            description: "test remote workspace".to_string(),
        }];
        let registry = build_tool_registry_with_cancel(
            &workspace_root,
            &runtime_state_root,
            &upstream,
            &BTreeMap::new(),
            None,
            None,
            None,
            None,
            &Vec::<PathBuf>::new(),
            &[],
            &[],
            &remote_workpaths,
            None,
        )
        .unwrap();

        let ls_result = registry["ls"]
            .invoke(json!({"path": "src", "remote": "fake-host"}))
            .unwrap();
        let ls_result = ls_result.as_str().unwrap();
        assert!(ls_result.contains("remote: fake-host"));
        assert!(ls_result.contains("- fallback.txt"));
    }

    #[cfg(unix)]
    #[test]
    fn remote_file_tools_report_missing_python_clearly() {
        let _env_guard = env_test_lock()
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let temp_dir = TempDir::new().unwrap();
        let bin_dir = temp_dir.path().join("bin");
        fs::create_dir_all(&bin_dir).unwrap();
        std::os::unix::fs::symlink("/bin/sh", bin_dir.join("sh")).unwrap();
        let fake_ssh = write_fake_ssh_with_path(
            &temp_dir,
            &format!(
                "PATH='{}' exec sh -c \"$remote_command\"",
                bin_dir.display()
            ),
        );
        let _ssh_guard = EnvVarGuard::set("AGENT_FRAME_SSH_BIN", fake_ssh.as_os_str());
        let workspace_root = temp_dir.path().join("workspace");
        fs::create_dir_all(workspace_root.join("src")).unwrap();
        let runtime_state_root = temp_dir.path().join("runtime");
        fs::create_dir_all(&runtime_state_root).unwrap();
        let upstream = test_upstream();
        let remote_workpaths = vec![RemoteWorkpathConfig {
            host: "fake-host".to_string(),
            path: workspace_root.display().to_string(),
            description: "test remote workspace".to_string(),
        }];
        let registry = build_tool_registry_with_cancel(
            &workspace_root,
            &runtime_state_root,
            &upstream,
            &BTreeMap::new(),
            None,
            None,
            None,
            None,
            &Vec::<PathBuf>::new(),
            &[],
            &[],
            &remote_workpaths,
            None,
        )
        .unwrap();

        let error = registry["ls"]
            .invoke(json!({"path": "src", "remote": "fake-host"}))
            .unwrap_err();
        assert!(format!("{error:#}").contains("remote file tools require Python 3"));
    }

    #[test]
    fn ls_skips_hidden_and_common_cache_directories() {
        let temp_dir = TempDir::new().unwrap();
        let workspace_root = temp_dir.path().join("workspace");
        let visible_dir = workspace_root.join("src");
        let hidden_dir = workspace_root.join(".venv_tools");
        let cache_dir = workspace_root.join("node_modules");
        fs::create_dir_all(&visible_dir).unwrap();
        fs::create_dir_all(&hidden_dir).unwrap();
        fs::create_dir_all(&cache_dir).unwrap();
        fs::write(visible_dir.join("main.rs"), "fn main() {}\n").unwrap();
        fs::write(hidden_dir.join("ignored.py"), "print('ignore')\n").unwrap();
        fs::write(cache_dir.join("ignored.js"), "console.log('ignore')\n").unwrap();
        fs::write(workspace_root.join(".env"), "SECRET=1\n").unwrap();
        let runtime_state_root = temp_dir.path().join("runtime");
        fs::create_dir_all(&runtime_state_root).unwrap();
        let upstream = UpstreamConfig {
            base_url: "https://example.com".to_string(),
            model: "demo".to_string(),
            api_kind: UpstreamApiKind::Responses,
            auth_kind: UpstreamAuthKind::ApiKey,
            supports_vision_input: false,
            supports_pdf_input: false,
            supports_audio_input: false,
            api_key: None,
            api_key_env: "OPENAI_API_KEY".to_string(),
            chat_completions_path: "/responses".to_string(),
            codex_home: None,
            codex_auth: None,
            auth_credentials_store_mode: AuthCredentialsStoreMode::Auto,
            timeout_seconds: 30.0,
            retry_mode: Default::default(),
            context_window_tokens: 200_000,
            cache_control: None,
            prompt_cache_retention: None,
            prompt_cache_key: None,
            reasoning: None,
            headers: serde_json::Map::new(),
            native_web_search: None,
            external_web_search: None,
            native_image_input: false,
            native_pdf_input: false,
            native_audio_input: false,
            native_image_generation: false,
            token_estimation: None,
        };
        let registry = build_tool_registry_with_cancel(
            &workspace_root,
            &runtime_state_root,
            &upstream,
            &BTreeMap::new(),
            None,
            None,
            None,
            None,
            &Vec::<PathBuf>::new(),
            &[],
            &[],
            &[],
            None,
        )
        .unwrap();

        let ls_result = registry["ls"].invoke(json!({"path":"."})).unwrap();
        let ls_result = ls_result.as_str().unwrap();
        assert!(ls_result.contains("- src/"));
        assert!(ls_result.contains("  - main.rs"));
        assert!(!ls_result.contains(".venv_tools"));
        assert!(!ls_result.contains("node_modules"));
        assert!(!ls_result.contains("target"));
    }

    #[test]
    fn ls_truncates_when_entry_limit_is_hit() {
        let temp_dir = TempDir::new().unwrap();
        let workspace_root = temp_dir.path().join("workspace");
        let crowded_dir = workspace_root.join("crowded");
        fs::create_dir_all(&crowded_dir).unwrap();
        for index in 0..(LS_MAX_ENTRIES + 25) {
            let filename = format!("file_{index:04}.txt");
            fs::write(crowded_dir.join(filename), "data\n").unwrap();
        }
        let runtime_state_root = temp_dir.path().join("runtime");
        fs::create_dir_all(&runtime_state_root).unwrap();
        let upstream = UpstreamConfig {
            base_url: "https://example.com".to_string(),
            model: "demo".to_string(),
            api_kind: UpstreamApiKind::Responses,
            auth_kind: UpstreamAuthKind::ApiKey,
            supports_vision_input: false,
            supports_pdf_input: false,
            supports_audio_input: false,
            api_key: None,
            api_key_env: "OPENAI_API_KEY".to_string(),
            chat_completions_path: "/responses".to_string(),
            codex_home: None,
            codex_auth: None,
            auth_credentials_store_mode: AuthCredentialsStoreMode::Auto,
            timeout_seconds: 30.0,
            retry_mode: Default::default(),
            context_window_tokens: 200_000,
            cache_control: None,
            prompt_cache_retention: None,
            prompt_cache_key: None,
            reasoning: None,
            headers: serde_json::Map::new(),
            native_web_search: None,
            external_web_search: None,
            native_image_input: false,
            native_pdf_input: false,
            native_audio_input: false,
            native_image_generation: false,
            token_estimation: None,
        };
        let registry = build_tool_registry_with_cancel(
            &workspace_root,
            &runtime_state_root,
            &upstream,
            &BTreeMap::new(),
            None,
            None,
            None,
            None,
            &Vec::<PathBuf>::new(),
            &[],
            &[],
            &[],
            None,
        )
        .unwrap();

        let ls_result = registry["ls"].invoke(json!({"path":"crowded"})).unwrap();
        let ls_result = ls_result.as_str().unwrap();
        assert!(ls_result.contains("num_entries: >1000"));
        assert!(ls_result.contains("truncated: true"));
        assert!(ls_result.contains("There are more than 1000 files and directories"));
        let printed_nodes = ls_result
            .lines()
            .filter(|line| line.trim_start().starts_with("- "))
            .count();
        assert!(printed_nodes <= LS_MAX_ENTRIES + 1);
        assert!(!ls_result.contains("\"entries\""));
        assert!(!ls_result.contains("\"type\""));
    }

    #[test]
    fn file_read_rejects_large_files_without_explicit_window() {
        let temp_dir = TempDir::new().unwrap();
        let workspace_root = temp_dir.path().join("workspace");
        fs::create_dir_all(&workspace_root).unwrap();
        let large_file = workspace_root.join("large.txt");
        fs::write(&large_file, "x".repeat(FILE_READ_MAX_OUTPUT_BYTES + 1024)).unwrap();
        let runtime_state_root = temp_dir.path().join("runtime");
        fs::create_dir_all(&runtime_state_root).unwrap();
        let upstream = UpstreamConfig {
            base_url: "https://example.com".to_string(),
            model: "demo".to_string(),
            api_kind: UpstreamApiKind::Responses,
            auth_kind: UpstreamAuthKind::ApiKey,
            supports_vision_input: false,
            supports_pdf_input: false,
            supports_audio_input: false,
            api_key: None,
            api_key_env: "OPENAI_API_KEY".to_string(),
            chat_completions_path: "/responses".to_string(),
            codex_home: None,
            codex_auth: None,
            auth_credentials_store_mode: AuthCredentialsStoreMode::Auto,
            timeout_seconds: 30.0,
            retry_mode: Default::default(),
            context_window_tokens: 200_000,
            cache_control: None,
            prompt_cache_retention: None,
            prompt_cache_key: None,
            reasoning: None,
            headers: serde_json::Map::new(),
            native_web_search: None,
            external_web_search: None,
            native_image_input: false,
            native_pdf_input: false,
            native_audio_input: false,
            native_image_generation: false,
            token_estimation: None,
        };
        let registry = build_tool_registry_with_cancel(
            &workspace_root,
            &runtime_state_root,
            &upstream,
            &BTreeMap::new(),
            None,
            None,
            None,
            None,
            &Vec::<PathBuf>::new(),
            &[],
            &[],
            &[],
            None,
        )
        .unwrap();

        let error = registry["file_read"]
            .invoke(json!({"file_path":"large.txt"}))
            .unwrap_err()
            .to_string();
        assert!(error.contains("provide offset and/or limit"));

        let ok = registry["file_read"]
            .invoke(json!({"file_path":"large.txt","offset":1,"limit":10}))
            .unwrap();
        assert_eq!(ok["start_line"].as_u64(), Some(1));
    }

    #[test]
    fn image_load_returns_small_multimodal_marker_payload() {
        let temp_dir = TempDir::new().unwrap();
        let workspace_root = temp_dir.path().join("workspace");
        fs::create_dir_all(&workspace_root).unwrap();
        let image_path = workspace_root.join("demo.png");
        fs::write(&image_path, b"png-bytes").unwrap();
        let runtime_state_root = temp_dir.path().join("runtime");
        fs::create_dir_all(&runtime_state_root).unwrap();
        let upstream = UpstreamConfig {
            base_url: "https://example.com".to_string(),
            model: "demo".to_string(),
            api_kind: UpstreamApiKind::Responses,
            auth_kind: UpstreamAuthKind::CodexSubscription,
            supports_vision_input: true,
            supports_pdf_input: false,
            supports_audio_input: false,
            api_key: None,
            api_key_env: "OPENAI_API_KEY".to_string(),
            chat_completions_path: "/responses".to_string(),
            codex_home: None,
            codex_auth: None,
            auth_credentials_store_mode: AuthCredentialsStoreMode::Auto,
            timeout_seconds: 30.0,
            retry_mode: Default::default(),
            context_window_tokens: 200_000,
            cache_control: None,
            prompt_cache_retention: None,
            prompt_cache_key: None,
            reasoning: None,
            headers: serde_json::Map::new(),
            native_web_search: None,
            external_web_search: None,
            native_image_input: true,
            native_pdf_input: false,
            native_audio_input: false,
            native_image_generation: false,
            token_estimation: None,
        };
        let registry = build_tool_registry_with_cancel(
            &workspace_root,
            &runtime_state_root,
            &upstream,
            &BTreeMap::new(),
            None,
            None,
            None,
            None,
            &Vec::<PathBuf>::new(),
            &[],
            &[],
            &[],
            None,
        )
        .unwrap();

        let result = registry["image_load"]
            .invoke(json!({
                "path": "demo.png"
            }))
            .unwrap();

        assert_eq!(result["kind"], "synthetic_user_multimodal");
        assert_eq!(result["media"][0]["type"], "input_image");
        assert_eq!(result["media"][0]["path"], image_path.display().to_string());
        assert!(result["media"][0].get("image_url").is_none());
    }

    #[test]
    fn vision_upstream_registers_image_load_instead_of_async_image_tools() {
        let temp_dir = TempDir::new().unwrap();
        let workspace_root = temp_dir.path().join("workspace");
        fs::create_dir_all(&workspace_root).unwrap();
        let runtime_state_root = temp_dir.path().join("runtime");
        fs::create_dir_all(&runtime_state_root).unwrap();
        let upstream = UpstreamConfig {
            base_url: "https://example.com".to_string(),
            model: "demo".to_string(),
            api_kind: UpstreamApiKind::Responses,
            auth_kind: UpstreamAuthKind::CodexSubscription,
            supports_vision_input: true,
            supports_pdf_input: false,
            supports_audio_input: false,
            api_key: None,
            api_key_env: "OPENAI_API_KEY".to_string(),
            chat_completions_path: "/responses".to_string(),
            codex_home: None,
            codex_auth: None,
            auth_credentials_store_mode: AuthCredentialsStoreMode::Auto,
            timeout_seconds: 30.0,
            retry_mode: Default::default(),
            context_window_tokens: 200_000,
            cache_control: None,
            prompt_cache_retention: None,
            prompt_cache_key: None,
            reasoning: None,
            headers: serde_json::Map::new(),
            native_web_search: None,
            external_web_search: None,
            native_image_input: true,
            native_pdf_input: false,
            native_audio_input: false,
            native_image_generation: false,
            token_estimation: None,
        };
        let registry = build_tool_registry_with_cancel(
            &workspace_root,
            &runtime_state_root,
            &upstream,
            &BTreeMap::new(),
            None,
            None,
            None,
            None,
            &Vec::<PathBuf>::new(),
            &[],
            &[],
            &[],
            None,
        )
        .unwrap();

        assert!(registry.contains_key("image_load"));
        assert!(!registry.contains_key("image_start"));
        assert!(!registry.contains_key("image_wait"));
        assert!(!registry.contains_key("image_cancel"));
    }

    #[test]
    fn external_image_tool_target_registers_async_image_tools() {
        let temp_dir = TempDir::new().unwrap();
        let workspace_root = temp_dir.path().join("workspace");
        fs::create_dir_all(&workspace_root).unwrap();
        let runtime_state_root = temp_dir.path().join("runtime");
        fs::create_dir_all(&runtime_state_root).unwrap();
        let upstream = UpstreamConfig {
            base_url: "https://example.com".to_string(),
            model: "demo".to_string(),
            api_kind: UpstreamApiKind::Responses,
            auth_kind: UpstreamAuthKind::CodexSubscription,
            supports_vision_input: true,
            supports_pdf_input: false,
            supports_audio_input: false,
            api_key: None,
            api_key_env: "OPENAI_API_KEY".to_string(),
            chat_completions_path: "/responses".to_string(),
            codex_home: None,
            codex_auth: None,
            auth_credentials_store_mode: AuthCredentialsStoreMode::Auto,
            timeout_seconds: 30.0,
            retry_mode: Default::default(),
            context_window_tokens: 200_000,
            cache_control: None,
            prompt_cache_retention: None,
            prompt_cache_key: None,
            reasoning: None,
            headers: serde_json::Map::new(),
            native_web_search: None,
            external_web_search: None,
            native_image_input: false,
            native_pdf_input: false,
            native_audio_input: false,
            native_image_generation: false,
            token_estimation: None,
        };
        let image_helper = UpstreamConfig {
            supports_vision_input: true,
            ..upstream.clone()
        };
        let registry = build_tool_registry_with_cancel(
            &workspace_root,
            &runtime_state_root,
            &upstream,
            &BTreeMap::new(),
            Some(&image_helper),
            None,
            None,
            None,
            &Vec::<PathBuf>::new(),
            &[],
            &[],
            &[],
            None,
        )
        .unwrap();

        assert!(!registry.contains_key("image_load"));
        assert!(registry.contains_key("image_start"));
        assert!(registry.contains_key("image_wait"));
        assert!(registry.contains_key("image_cancel"));
    }

    #[test]
    fn native_pdf_input_registers_pdf_load_without_external_helper() {
        let temp_dir = TempDir::new().unwrap();
        let workspace_root = temp_dir.path().join("workspace");
        fs::create_dir_all(&workspace_root).unwrap();
        let runtime_state_root = temp_dir.path().join("runtime");
        fs::create_dir_all(&runtime_state_root).unwrap();
        let upstream = UpstreamConfig {
            base_url: "https://example.com".to_string(),
            model: "demo".to_string(),
            api_kind: UpstreamApiKind::Responses,
            auth_kind: UpstreamAuthKind::ApiKey,
            supports_vision_input: false,
            supports_pdf_input: true,
            supports_audio_input: false,
            api_key: None,
            api_key_env: "OPENAI_API_KEY".to_string(),
            chat_completions_path: "/responses".to_string(),
            codex_home: None,
            codex_auth: None,
            auth_credentials_store_mode: AuthCredentialsStoreMode::Auto,
            timeout_seconds: 30.0,
            retry_mode: Default::default(),
            context_window_tokens: 200_000,
            cache_control: None,
            prompt_cache_retention: None,
            prompt_cache_key: None,
            reasoning: None,
            headers: serde_json::Map::new(),
            native_web_search: None,
            external_web_search: None,
            native_image_input: false,
            native_pdf_input: true,
            native_audio_input: false,
            native_image_generation: false,
            token_estimation: None,
        };
        let registry = build_tool_registry_with_cancel(
            &workspace_root,
            &runtime_state_root,
            &upstream,
            &BTreeMap::new(),
            None,
            None,
            None,
            None,
            &Vec::<PathBuf>::new(),
            &[],
            &[],
            &[],
            None,
        )
        .unwrap();

        assert!(registry.contains_key("pdf_load"));
        assert!(!registry.contains_key("pdf_query"));
    }

    #[test]
    fn native_audio_input_registers_audio_load_without_external_helper() {
        let temp_dir = TempDir::new().unwrap();
        let workspace_root = temp_dir.path().join("workspace");
        fs::create_dir_all(&workspace_root).unwrap();
        let runtime_state_root = temp_dir.path().join("runtime");
        fs::create_dir_all(&runtime_state_root).unwrap();
        let upstream = UpstreamConfig {
            base_url: "https://example.com".to_string(),
            model: "demo".to_string(),
            api_kind: UpstreamApiKind::Responses,
            auth_kind: UpstreamAuthKind::ApiKey,
            supports_vision_input: false,
            supports_pdf_input: false,
            supports_audio_input: true,
            api_key: None,
            api_key_env: "OPENAI_API_KEY".to_string(),
            chat_completions_path: "/responses".to_string(),
            codex_home: None,
            codex_auth: None,
            auth_credentials_store_mode: AuthCredentialsStoreMode::Auto,
            timeout_seconds: 30.0,
            retry_mode: Default::default(),
            context_window_tokens: 200_000,
            cache_control: None,
            prompt_cache_retention: None,
            prompt_cache_key: None,
            reasoning: None,
            headers: serde_json::Map::new(),
            native_web_search: None,
            external_web_search: None,
            native_image_input: false,
            native_pdf_input: false,
            native_audio_input: true,
            native_image_generation: false,
            token_estimation: None,
        };
        let registry = build_tool_registry_with_cancel(
            &workspace_root,
            &runtime_state_root,
            &upstream,
            &BTreeMap::new(),
            None,
            None,
            None,
            None,
            &Vec::<PathBuf>::new(),
            &[],
            &[],
            &[],
            None,
        )
        .unwrap();

        assert!(registry.contains_key("audio_load"));
        assert!(!registry.contains_key("audio_transcribe"));
    }

    #[test]
    fn active_runtime_state_summary_lists_running_execs_and_downloads() {
        let temp_dir = TempDir::new().unwrap();
        let runtime_state_root = temp_dir.path();
        let processes_dir = runtime_state_root.join("agent_frame").join("processes");
        let downloads_dir = runtime_state_root
            .join("agent_frame")
            .join("file_downloads");
        let subagents_dir = runtime_state_root.join("agent_frame").join("subagents");
        fs::create_dir_all(&processes_dir).unwrap();
        fs::create_dir_all(&downloads_dir).unwrap();
        fs::create_dir_all(&subagents_dir).unwrap();

        let exec_status_path = processes_dir.join("shell-1.status.json");
        fs::write(
            &exec_status_path,
            serde_json::to_vec_pretty(&json!({
                "session_id": "shell-1",
                "pid": std::process::id(),
                "interactive": false,
                "remote": "local",
                "process_id": "proc-1",
                "command": "sleep 10",
                "running": true,
                "exit_code": json!(null),
                "stdout_path": processes_dir.join("shell-1").join("proc-1").join("stdout").display().to_string(),
                "stderr_path": processes_dir.join("shell-1").join("proc-1").join("stderr").display().to_string(),
                "error": json!(null),
            }))
            .unwrap(),
        )
        .unwrap();
        let exec_metadata = ShellSessionMetadata {
            session_id: "shell-1".to_string(),
            worker_pid: std::process::id(),
            interactive: false,
            remote: "local".to_string(),
            status_path: exec_status_path.display().to_string(),
            worker_exit_code_path: processes_dir
                .join("shell-1.worker.exit")
                .display()
                .to_string(),
            requests_dir: processes_dir.join("shell-1.requests").display().to_string(),
            output_root: processes_dir.join("shell-1").display().to_string(),
            delivered_process_id: None,
        };
        fs::write(
            process_meta_path(&processes_dir, "shell-1"),
            serde_json::to_vec_pretty(&exec_metadata).unwrap(),
        )
        .unwrap();
        fs::create_dir_all(processes_dir.join("shell-1").join("proc-1")).unwrap();
        fs::write(
            processes_dir.join("shell-1").join("proc-1").join("stdout"),
            b"hello\nnon-utf8:\x9f\nstill visible\n",
        )
        .unwrap();
        fs::write(
            processes_dir.join("shell-1").join("proc-1").join("stderr"),
            b"",
        )
        .unwrap();

        let finished_status_path = processes_dir.join("shell-finished.status.json");
        fs::write(
            &finished_status_path,
            serde_json::to_vec_pretty(&json!({
                "session_id": "shell-finished",
                "pid": std::process::id(),
                "interactive": false,
                "remote": "local",
                "process_id": "proc-finished",
                "command": "echo done",
                "running": false,
                "exit_code": 0,
                "stdout_path": processes_dir.join("shell-finished").join("proc-finished").join("stdout").display().to_string(),
                "stderr_path": processes_dir.join("shell-finished").join("proc-finished").join("stderr").display().to_string(),
                "error": json!(null),
            }))
            .unwrap(),
        )
        .unwrap();
        let finished_metadata = ShellSessionMetadata {
            session_id: "shell-finished".to_string(),
            worker_pid: std::process::id(),
            interactive: false,
            remote: "local".to_string(),
            status_path: finished_status_path.display().to_string(),
            worker_exit_code_path: processes_dir
                .join("shell-finished.worker.exit")
                .display()
                .to_string(),
            requests_dir: processes_dir
                .join("shell-finished.requests")
                .display()
                .to_string(),
            output_root: processes_dir.join("shell-finished").display().to_string(),
            delivered_process_id: None,
        };
        fs::write(
            process_meta_path(&processes_dir, "shell-finished"),
            serde_json::to_vec_pretty(&finished_metadata).unwrap(),
        )
        .unwrap();
        fs::create_dir_all(processes_dir.join("shell-finished").join("proc-finished")).unwrap();
        fs::write(
            processes_dir
                .join("shell-finished")
                .join("proc-finished")
                .join("stdout"),
            b"done\n",
        )
        .unwrap();
        fs::write(
            processes_dir
                .join("shell-finished")
                .join("proc-finished")
                .join("stderr"),
            b"",
        )
        .unwrap();

        let download_status_path = downloads_dir.join("download-1.status.json");
        let download_exit_path = downloads_dir.join("download-1.exit");
        fs::write(
            &download_status_path,
            serde_json::to_vec_pretty(&json!({
                "download_id": "download-1",
                "url": "https://example.com/file.bin",
                "path": "/tmp/file.bin",
                "running": true,
                "completed": false,
                "cancelled": false,
                "bytes_downloaded": 128
            }))
            .unwrap(),
        )
        .unwrap();
        write_background_task_metadata(
            &downloads_dir,
            &BackgroundTaskMetadata {
                task_id: "download-1".to_string(),
                pid: std::process::id(),
                label: "file-download".to_string(),
                status_path: download_status_path.display().to_string(),
                stdout_path: downloads_dir
                    .join("download-1.stdout")
                    .display()
                    .to_string(),
                stderr_path: downloads_dir
                    .join("download-1.stderr")
                    .display()
                    .to_string(),
                exit_code_path: download_exit_path.display().to_string(),
            },
        )
        .unwrap();

        fs::write(
            subagents_dir.join("subagent-1.json"),
            serde_json::to_vec_pretty(&json!({
                "id": "subagent-1",
                "description": "inspect logs and summarize the issue",
                "model_key": "main",
                "state": "ready"
            }))
            .unwrap(),
        )
        .unwrap();

        let summary = active_runtime_state_summary(runtime_state_root)
            .unwrap()
            .expect("expected active runtime summary");
        assert!(summary.contains("Active shell sessions:"));
        assert!(summary.contains("session_id=`shell-1`"));
        assert!(!summary.contains("shell-finished"));
        assert!(summary.contains("Active file downloads:"));
        assert!(summary.contains("download_id=`download-1`"));
        assert!(summary.contains("Active subagents:"));
        assert!(summary.contains("subagent-1"));
    }

    #[test]
    fn active_runtime_state_summary_ignores_unknown_process_metadata_files() {
        let temp_dir = TempDir::new().unwrap();
        let runtime_state_root = temp_dir.path();
        let processes_dir = runtime_state_root.join("agent_frame").join("processes");
        fs::create_dir_all(&processes_dir).unwrap();

        let legacy_exec_id = "legacy-exec";
        fs::write(
            process_meta_path(&processes_dir, legacy_exec_id),
            serde_json::to_vec_pretty(&json!({
                "exec_id": legacy_exec_id,
                "pid": 12345,
                "command": "sleep 10",
                "cwd": "/tmp/legacy",
                "stdout_path": processes_dir.join(format!("{legacy_exec_id}.stdout")).display().to_string(),
                "stderr_path": processes_dir.join(format!("{legacy_exec_id}.stderr")).display().to_string(),
                "exit_code_path": processes_dir.join(format!("{legacy_exec_id}.exit")).display().to_string(),
            }))
            .unwrap(),
        )
        .unwrap();

        let current_exec_id = "current-shell";
        let current_status_path = processes_dir.join(format!("{current_exec_id}.status.json"));
        fs::write(
            &current_status_path,
            serde_json::to_vec_pretty(&json!({
                "session_id": current_exec_id,
                "pid": std::process::id(),
                "interactive": false,
                "remote": "local",
                "process_id": "proc-current",
                "command": "sleep 10",
                "running": true,
                "exit_code": json!(null),
                "stdout_path": processes_dir.join(current_exec_id).join("proc-current").join("stdout").display().to_string(),
                "stderr_path": processes_dir.join(current_exec_id).join("proc-current").join("stderr").display().to_string(),
                "error": json!(null),
            }))
            .unwrap(),
        )
        .unwrap();
        let current_metadata = ShellSessionMetadata {
            session_id: current_exec_id.to_string(),
            worker_pid: std::process::id(),
            interactive: false,
            remote: "local".to_string(),
            status_path: current_status_path.display().to_string(),
            worker_exit_code_path: processes_dir
                .join(format!("{current_exec_id}.worker.exit"))
                .display()
                .to_string(),
            requests_dir: processes_dir
                .join(format!("{current_exec_id}.requests"))
                .display()
                .to_string(),
            output_root: processes_dir.join(current_exec_id).display().to_string(),
            delivered_process_id: None,
        };
        fs::write(
            process_meta_path(&processes_dir, current_exec_id),
            serde_json::to_vec_pretty(&current_metadata).unwrap(),
        )
        .unwrap();
        fs::create_dir_all(processes_dir.join(current_exec_id).join("proc-current")).unwrap();
        fs::write(
            processes_dir
                .join(current_exec_id)
                .join("proc-current")
                .join("stdout"),
            b"running\n",
        )
        .unwrap();
        fs::write(
            processes_dir
                .join(current_exec_id)
                .join("proc-current")
                .join("stderr"),
            b"",
        )
        .unwrap();

        let summary = active_runtime_state_summary(runtime_state_root)
            .unwrap()
            .expect("expected active runtime summary");
        assert!(summary.contains("current-shell"));
        assert!(!summary.contains("legacy-exec"));
    }

    #[test]
    fn terminate_runtime_state_tasks_kills_running_tasks() {
        let temp_dir = TempDir::new().unwrap();
        let runtime_state_root = temp_dir.path();
        let processes_dir = runtime_state_root.join("agent_frame").join("processes");
        let downloads_dir = runtime_state_root
            .join("agent_frame")
            .join("file_downloads");
        let images_dir = runtime_state_root.join("agent_frame").join("image_tasks");
        fs::create_dir_all(&processes_dir).unwrap();
        fs::create_dir_all(&downloads_dir).unwrap();
        fs::create_dir_all(&images_dir).unwrap();

        let mut exec_child = Command::new("sh")
            .arg("-c")
            .arg("sleep 30")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let exec_status_path = processes_dir.join("shell-cleanup.status.json");
        fs::write(
            &exec_status_path,
            serde_json::to_vec_pretty(&json!({
                "session_id": "shell-cleanup",
                "pid": exec_child.id(),
                "interactive": false,
                "remote": "local",
                "process_id": "proc-cleanup",
                "command": "sleep 30",
                "running": true,
                "exit_code": json!(null),
                "stdout_path": processes_dir.join("shell-cleanup").join("proc-cleanup").join("stdout").display().to_string(),
                "stderr_path": processes_dir.join("shell-cleanup").join("proc-cleanup").join("stderr").display().to_string(),
                "error": json!(null),
            }))
            .unwrap(),
        )
        .unwrap();
        let exec_metadata = ShellSessionMetadata {
            session_id: "shell-cleanup".to_string(),
            worker_pid: exec_child.id(),
            interactive: false,
            remote: "local".to_string(),
            status_path: exec_status_path.display().to_string(),
            worker_exit_code_path: processes_dir
                .join("shell-cleanup.worker.exit")
                .display()
                .to_string(),
            requests_dir: processes_dir
                .join("shell-cleanup.requests")
                .display()
                .to_string(),
            output_root: processes_dir.join("shell-cleanup").display().to_string(),
            delivered_process_id: None,
        };
        fs::write(
            process_meta_path(&processes_dir, "shell-cleanup"),
            serde_json::to_vec_pretty(&exec_metadata).unwrap(),
        )
        .unwrap();
        fs::create_dir_all(processes_dir.join("shell-cleanup").join("proc-cleanup")).unwrap();
        fs::write(
            processes_dir
                .join("shell-cleanup")
                .join("proc-cleanup")
                .join("stdout"),
            b"",
        )
        .unwrap();
        fs::write(
            processes_dir
                .join("shell-cleanup")
                .join("proc-cleanup")
                .join("stderr"),
            b"",
        )
        .unwrap();

        let mut download_child = Command::new("sh")
            .arg("-c")
            .arg("sleep 30")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let download_status_path = downloads_dir.join("download-cleanup.status.json");
        fs::write(
            &download_status_path,
            serde_json::to_vec_pretty(&json!({
                "download_id": "download-cleanup",
                "url": "https://example.com/archive.tar",
                "path": "/tmp/archive.tar",
                "running": true,
                "completed": false,
                "cancelled": false,
                "failed": false,
                "bytes_downloaded": 64
            }))
            .unwrap(),
        )
        .unwrap();
        write_background_task_metadata(
            &downloads_dir,
            &BackgroundTaskMetadata {
                task_id: "download-cleanup".to_string(),
                pid: download_child.id(),
                label: "file-download".to_string(),
                status_path: download_status_path.display().to_string(),
                stdout_path: downloads_dir
                    .join("download-cleanup.stdout")
                    .display()
                    .to_string(),
                stderr_path: downloads_dir
                    .join("download-cleanup.stderr")
                    .display()
                    .to_string(),
                exit_code_path: downloads_dir
                    .join("download-cleanup.exit")
                    .display()
                    .to_string(),
            },
        )
        .unwrap();

        let mut image_child = Command::new("sh")
            .arg("-c")
            .arg("sleep 30")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let image_status_path = images_dir.join("image-cleanup.status.json");
        fs::write(
            &image_status_path,
            serde_json::to_vec_pretty(&json!({
                "image_id": "image-cleanup",
                "path": "/tmp/demo.png",
                "question": "what is in the image?",
                "running": true,
                "completed": false,
                "cancelled": false,
                "failed": false
            }))
            .unwrap(),
        )
        .unwrap();
        write_background_task_metadata(
            &images_dir,
            &BackgroundTaskMetadata {
                task_id: "image-cleanup".to_string(),
                pid: image_child.id(),
                label: "image".to_string(),
                status_path: image_status_path.display().to_string(),
                stdout_path: images_dir
                    .join("image-cleanup.stdout")
                    .display()
                    .to_string(),
                stderr_path: images_dir
                    .join("image-cleanup.stderr")
                    .display()
                    .to_string(),
                exit_code_path: images_dir.join("image-cleanup.exit").display().to_string(),
            },
        )
        .unwrap();

        let report = terminate_runtime_state_tasks(runtime_state_root).unwrap();
        assert_eq!(report.exec_processes_killed, 1);
        assert_eq!(report.file_downloads_cancelled, 1);
        assert_eq!(report.image_tasks_cancelled, 1);

        let download_snapshot: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(download_status_path).unwrap()).unwrap();
        assert_eq!(download_snapshot["cancelled"], json!(true));
        assert_eq!(download_snapshot["reason"], json!("session_destroyed"));
        let image_snapshot: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(image_status_path).unwrap()).unwrap();
        assert_eq!(image_snapshot["cancelled"], json!(true));
        assert_eq!(image_snapshot["reason"], json!("session_destroyed"));

        let _ = exec_child.wait();
        let _ = download_child.wait();
        let _ = image_child.wait();
        thread::sleep(Duration::from_millis(50));
        assert!(!process_is_running(exec_metadata.worker_pid));
        assert!(!process_is_running(download_child.id()));
        assert!(!process_is_running(image_child.id()));
    }
}

#[macro_export]
macro_rules! __agent_frame_build_tool_schema {
    ($( $arg:ident : $arg_ty:ty ),* $(,)?) => {{
        let mut properties = $crate::serde_json::Map::new();
        let mut required = Vec::<String>::new();
        $(
            properties.insert(
                stringify!($arg).to_string(),
                $crate::tooling::macro_support::schema_for_type_name(stringify!($arg_ty)),
            );
            if !$crate::tooling::macro_support::type_is_optional(stringify!($arg_ty)) {
                required.push(stringify!($arg).to_string());
            }
        )*
        $crate::serde_json::json!({
            "type": "object",
            "properties": properties,
            "required": required,
            "additionalProperties": false
        })
    }};
}

#[macro_export]
macro_rules! tool {
    (
        description: $description:expr,
        fn $fn_name:ident ( $( $arg:ident : $arg_ty:ty ),* $(,)? ) -> $ret:ty $body:block
    ) => {
        $crate::tool! {
            name: stringify!($fn_name),
            description: $description,
            fn $fn_name( $( $arg : $arg_ty ),* ) -> $ret $body
        }
    };
    (
        name: $name:expr,
        description: $description:expr,
        fn $fn_name:ident ( $( $arg:ident : $arg_ty:ty ),* $(,)? ) -> $ret:ty $body:block
    ) => {{
        $crate::tooling::Tool::new(
            $name,
            $description,
            $crate::__agent_frame_build_tool_schema!($( $arg : $arg_ty ),*),
            move |__tool_arguments| {
                let __tool_arguments = $crate::tooling::macro_support::arguments_object(&__tool_arguments)?;
                $(
                    let $arg: $arg_ty = $crate::tooling::macro_support::parse_argument::<$arg_ty>(
                        __tool_arguments,
                        stringify!($arg),
                        $crate::tooling::macro_support::type_is_optional(stringify!($arg_ty)),
                    )?;
                )*
                let __tool_result: $ret = { $body };
                $crate::tooling::macro_support::result_to_value(__tool_result)
            },
        )
    }};
}
