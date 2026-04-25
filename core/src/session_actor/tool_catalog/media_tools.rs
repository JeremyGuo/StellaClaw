use std::{
    collections::HashMap,
    fs::{self, OpenOptions},
    io::{Cursor, Write},
    path::{Path, PathBuf},
    process::Command,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex, OnceLock,
    },
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use image::ImageReader;
use serde_json::{json, Map, Value};

use super::{
    schema::{add_images_property, add_remote_property, object_schema, properties},
    BuiltinToolCatalogOptions, ProviderBackedToolKind, ToolBackend, ToolDefinition,
    ToolExecutionMode,
};
use crate::{
    model_config::ModelConfig,
    providers::{global_provider_fork_server, ProviderRequestAbortHandle, ProviderRequestOwned},
    session_actor::{
        normalize_messages_for_model,
        tool_runtime::{
            bool_arg_with_default, f64_arg_with_default, resolve_local_path, shell_quote,
            string_arg, ExecutionTarget, LocalToolError, ToolCancellationToken,
            ToolExecutionContext,
        },
        ChatMessage, ChatMessageItem, ChatRole, ContextItem, FileItem, FileState, TokenUsage,
        ToolResultContent,
    },
};

static MEDIA_JOBS: OnceLock<Mutex<HashMap<String, MediaJob>>> = OnceLock::new();
const TOOL_USAGE_LOG_PATH: &str = ".log/stellaclaw/tool_usage.jsonl";

struct MediaJob {
    status: Arc<Mutex<MediaJobStatus>>,
    cancel: Arc<AtomicBool>,
    provider_abort: Arc<Mutex<Option<ProviderRequestAbortHandle>>>,
}

#[derive(Clone)]
enum MediaJobStatus {
    Running,
    Completed(ToolResultContent),
    Failed(String),
    Cancelled,
}

fn media_jobs() -> &'static Mutex<HashMap<String, MediaJob>> {
    MEDIA_JOBS.get_or_init(|| Mutex::new(HashMap::new()))
}

pub fn media_tool_definitions(options: &BuiltinToolCatalogOptions) -> Vec<ToolDefinition> {
    let mut tools = Vec::new();

    if options.enable_provider_image_analysis {
        tools.push(analysis_tool_definition(
            "image_analysis",
            "image_id",
            "Analyze a local image using the configured helper model. First call with path and question starts a job and returns an id; call again with image_id to wait or observe.",
            ProviderBackedToolKind::ImageAnalysis,
            &options.remote_mode,
        ));
        tools.push(stop_tool_definition(
            "image_stop",
            "image_id",
            "Stop a running image_analysis job.",
        ));
    }

    if options.enable_native_image_load {
        tools.push(ToolDefinition::new(
            "image_load",
            "Load a local image file into the next model request for direct multimodal inspection by the current model. Returns immediately. Do not call image_load more than 3 times in the same assistant tool-call batch; excess image_load calls in that batch will fail. Load more images after inspecting the first batch.",
            media_load_schema("path", &options.remote_mode),
            ToolExecutionMode::Immediate,
            ToolBackend::Local,
        ));
    }

    if options.enable_native_pdf_load {
        tools.push(ToolDefinition::new(
            "pdf_load",
            "Load a local PDF file into the next model request for direct inspection by the current model. Returns immediately.",
            media_load_schema("path", &options.remote_mode),
            ToolExecutionMode::Immediate,
            ToolBackend::Local,
        ));
    }

    if options.enable_native_audio_load {
        tools.push(ToolDefinition::new(
            "audio_load",
            "Load a local audio file into the next model request for direct inspection by the current model. Returns immediately.",
            media_load_schema("path", &options.remote_mode),
            ToolExecutionMode::Immediate,
            ToolBackend::Local,
        ));
    }

    if options.enable_provider_pdf_analysis {
        tools.push(analysis_tool_definition(
            "pdf_analysis",
            "pdf_id",
            "Analyze a local PDF using the configured helper model. First call with path and question starts a job and returns an id; call again with pdf_id to wait or observe.",
            ProviderBackedToolKind::PdfAnalysis,
            &options.remote_mode,
        ));
        tools.push(stop_tool_definition(
            "pdf_stop",
            "pdf_id",
            "Stop a running pdf_analysis job.",
        ));
    }

    if options.enable_provider_audio_analysis {
        tools.push(analysis_tool_definition(
            "audio_analysis",
            "audio_id",
            "Analyze or transcribe a local audio file using the configured helper model. First call with path and optional question starts a job and returns an id; call again with audio_id to wait or observe.",
            ProviderBackedToolKind::AudioAnalysis,
            &options.remote_mode,
        ));
        tools.push(stop_tool_definition(
            "audio_stop",
            "audio_id",
            "Stop a running audio_analysis job.",
        ));
    }

    if options.enable_provider_image_generation {
        let mut schema_properties = properties([("prompt", json!({"type": "string"}))]);
        add_images_property(&mut schema_properties, true);
        schema_properties.insert("generation_id".to_string(), json!({"type": "string"}));
        schema_properties.insert("return_immediate".to_string(), json!({"type": "boolean"}));
        schema_properties.insert(
            "wait_timeout_seconds".to_string(),
            json!({"type": "number"}),
        );
        schema_properties.insert(
            "on_timeout".to_string(),
            json!({"type": "string", "enum": ["continue", "kill", "CONTINUE", "KILL"]}),
        );
        tools.push(ToolDefinition::new(
            "image_generation",
            "Generate an image using the configured generation model. First call with prompt starts a job and returns an id; call again with generation_id to wait or observe.",
            object_schema(schema_properties, &[]),
            ToolExecutionMode::Interruptible,
            ToolBackend::ProviderBacked {
                kind: ProviderBackedToolKind::ImageGeneration,
            },
        ));
        tools.push(stop_tool_definition(
            "image_generation_stop",
            "generation_id",
            "Stop a running image_generation job.",
        ));
    }

    tools
}

fn analysis_tool_definition(
    name: &str,
    id_field: &str,
    description: &str,
    kind: ProviderBackedToolKind,
    remote_mode: &super::ToolRemoteMode,
) -> ToolDefinition {
    let mut schema_properties = properties([
        ("path", json!({"type": "string"})),
        ("question", json!({"type": "string"})),
        ("return_immediate", json!({"type": "boolean"})),
        ("wait_timeout_seconds", json!({"type": "number"})),
        (
            "on_timeout",
            json!({"type": "string", "enum": ["continue", "kill", "CONTINUE", "KILL"]}),
        ),
    ]);
    schema_properties.insert(id_field.to_string(), json!({"type": "string"}));
    add_images_property(&mut schema_properties, true);
    add_remote_property(&mut schema_properties, remote_mode);

    ToolDefinition::new(
        name,
        description,
        object_schema(schema_properties, &[]),
        ToolExecutionMode::Interruptible,
        ToolBackend::ProviderBacked { kind },
    )
}

fn media_load_schema(path_field: &'static str, remote_mode: &super::ToolRemoteMode) -> Value {
    let mut schema_properties = properties([(path_field, json!({"type": "string"}))]);
    add_remote_property(&mut schema_properties, remote_mode);
    object_schema(schema_properties, &[path_field])
}

fn stop_tool_definition(name: &str, id_field: &str, description: &str) -> ToolDefinition {
    ToolDefinition::new(
        name,
        description,
        object_schema(
            {
                let mut schema_properties = Map::new();
                schema_properties.insert(id_field.to_string(), json!({"type": "string"}));
                schema_properties
            },
            &[id_field],
        ),
        ToolExecutionMode::Immediate,
        ToolBackend::Local,
    )
}

pub(crate) fn execute_media_tool(
    tool_name: &str,
    arguments: &Map<String, Value>,
    context: &ToolExecutionContext<'_>,
) -> Result<Option<ToolResultContent>, LocalToolError> {
    let result = match tool_name {
        "image_load" => native_load(arguments, context, "image")?,
        "pdf_load" => native_load(arguments, context, "pdf")?,
        "audio_load" => native_load(arguments, context, "audio")?,
        "image_stop" => stop_media_job(arguments, "image_id")?,
        "pdf_stop" => stop_media_job(arguments, "pdf_id")?,
        "audio_stop" => stop_media_job(arguments, "audio_id")?,
        "image_generation_stop" => stop_media_job(arguments, "generation_id")?,
        _ => return Ok(None),
    };
    Ok(Some(result))
}

pub(crate) fn execute_provider_backed_media_tool(
    tool_name: &str,
    kind: ProviderBackedToolKind,
    model_config: &ModelConfig,
    arguments: &Map<String, Value>,
    context: &ToolExecutionContext<'_>,
) -> Result<ToolResultContent, LocalToolError> {
    match kind {
        ProviderBackedToolKind::ImageAnalysis => analysis_tool(
            tool_name,
            arguments,
            context,
            model_config,
            "image_id",
            "image",
        ),
        ProviderBackedToolKind::PdfAnalysis => {
            analysis_tool(tool_name, arguments, context, model_config, "pdf_id", "pdf")
        }
        ProviderBackedToolKind::AudioAnalysis => analysis_tool(
            tool_name,
            arguments,
            context,
            model_config,
            "audio_id",
            "audio",
        ),
        ProviderBackedToolKind::ImageGeneration => {
            image_generation_tool(tool_name, arguments, context, model_config)
        }
    }
}

fn native_load(
    arguments: &Map<String, Value>,
    context: &ToolExecutionContext<'_>,
    media_kind: &str,
) -> Result<ToolResultContent, LocalToolError> {
    let file = file_item_from_path(arguments, context, media_kind)?;
    let status = match &file.state {
        Some(FileState::Crashed { reason }) => json!({
            "status": "crashed",
            "uri": file.uri,
            "media_type": file.media_type,
            "reason": reason,
        }),
        None => json!({
            "status": "loaded",
            "uri": file.uri,
            "media_type": file.media_type,
        }),
    };
    Ok(ToolResultContent {
        context: Some(ContextItem {
            text: status.to_string(),
        }),
        file: Some(file),
    })
}

fn analysis_tool(
    tool_name: &str,
    arguments: &Map<String, Value>,
    context: &ToolExecutionContext<'_>,
    model_config: &ModelConfig,
    id_field: &str,
    media_kind: &str,
) -> Result<ToolResultContent, LocalToolError> {
    if let Some(job_id) = optional_non_empty_string_arg(arguments, id_field)? {
        return wait_media_job(job_id, arguments, id_field, &context.cancel_token);
    }

    let file = file_item_from_path(arguments, context, media_kind)?;
    let question = arguments
        .get("question")
        .and_then(Value::as_str)
        .unwrap_or("Analyze this file.")
        .to_string();
    let job_id = next_media_job_id(media_kind);
    let prompt = format!("{question}\n\nReturn a concise answer based only on the attached file.");
    start_provider_job(
        tool_name.to_string(),
        job_id.clone(),
        model_config.clone(),
        prompt,
        vec![file],
        tool_usage_log_path(context.workspace_root),
    );
    initial_job_result(
        tool_name,
        &job_id,
        id_field,
        arguments,
        &context.cancel_token,
    )
}

fn image_generation_tool(
    tool_name: &str,
    arguments: &Map<String, Value>,
    context: &ToolExecutionContext<'_>,
    model_config: &ModelConfig,
) -> Result<ToolResultContent, LocalToolError> {
    if let Some(job_id) = optional_non_empty_string_arg(arguments, "generation_id")? {
        return wait_media_job(job_id, arguments, "generation_id", &context.cancel_token);
    }

    let prompt = string_arg(arguments, "prompt")?;
    let images = collect_optional_images(arguments, context)?;
    let job_id = next_media_job_id("image_generation");
    start_provider_job(
        tool_name.to_string(),
        job_id.clone(),
        model_config.clone(),
        prompt,
        images,
        tool_usage_log_path(context.workspace_root),
    );
    initial_job_result(
        tool_name,
        &job_id,
        "generation_id",
        arguments,
        &context.cancel_token,
    )
}

fn start_provider_job(
    tool_name: String,
    job_id: String,
    model_config: ModelConfig,
    prompt: String,
    files: Vec<FileItem>,
    usage_log_path: PathBuf,
) {
    let status = Arc::new(Mutex::new(MediaJobStatus::Running));
    let cancel = Arc::new(AtomicBool::new(false));
    let provider_abort = Arc::new(Mutex::new(None));
    media_jobs().lock().expect("mutex poisoned").insert(
        job_id.clone(),
        MediaJob {
            status: status.clone(),
            cancel: cancel.clone(),
            provider_abort: provider_abort.clone(),
        },
    );

    thread::spawn(move || {
        if cancel.load(Ordering::SeqCst) {
            *status.lock().expect("mutex poisoned") = MediaJobStatus::Cancelled;
            return;
        }

        let mut data = vec![ChatMessageItem::Context(ContextItem { text: prompt })];
        data.extend(files.into_iter().map(ChatMessageItem::File));
        let messages = vec![ChatMessage::new(ChatRole::User, data)];
        let normalized_messages = normalize_messages_for_model(&messages, &model_config);

        let fork_server = match global_provider_fork_server() {
            Ok(fork_server) => fork_server,
            Err(error) => {
                *status.lock().expect("mutex poisoned") = MediaJobStatus::Failed(format!(
                    "failed to access provider request runtime: {error}"
                ));
                return;
            }
        };
        let handle = match fork_server.start(
            model_config.clone(),
            ProviderRequestOwned::new(normalized_messages),
        ) {
            Ok(handle) => handle,
            Err(error) => {
                *status.lock().expect("mutex poisoned") = MediaJobStatus::Failed(error.to_string());
                return;
            }
        };

        *provider_abort.lock().expect("mutex poisoned") = Some(handle.abort_handle());

        if cancel.load(Ordering::SeqCst) {
            if let Some(abort) = provider_abort.lock().expect("mutex poisoned").as_ref() {
                let _ = abort.abort();
            }
            *status.lock().expect("mutex poisoned") = MediaJobStatus::Cancelled;
            return;
        }

        let result = handle
            .wait()
            .map(|message| {
                append_tool_usage(
                    &usage_log_path,
                    &tool_name,
                    &job_id,
                    &model_config,
                    &message,
                );
                provider_message_to_tool_result(message)
            })
            .map_err(|error| error.to_string());

        if cancel.load(Ordering::SeqCst) {
            *status.lock().expect("mutex poisoned") = MediaJobStatus::Cancelled;
            return;
        }

        *status.lock().expect("mutex poisoned") = match result {
            Ok(result) => MediaJobStatus::Completed(result),
            Err(error) => MediaJobStatus::Failed(error),
        };
    });
}

fn initial_job_result(
    tool_name: &str,
    job_id: &str,
    id_field: &str,
    arguments: &Map<String, Value>,
    cancel_token: &ToolCancellationToken,
) -> Result<ToolResultContent, LocalToolError> {
    if bool_arg_with_default(arguments, "return_immediate", true)? {
        return Ok(job_snapshot_result(tool_name, job_id, id_field, "running"));
    }
    wait_media_job(job_id, arguments, id_field, cancel_token)
}

fn wait_media_job(
    job_id: &str,
    arguments: &Map<String, Value>,
    id_field: &str,
    cancel_token: &ToolCancellationToken,
) -> Result<ToolResultContent, LocalToolError> {
    let timeout = Duration::from_secs_f64(f64_arg_with_default(
        arguments,
        "wait_timeout_seconds",
        0.0,
    )?);
    let started_at = Instant::now();

    loop {
        let status = {
            let jobs = media_jobs().lock().expect("mutex poisoned");
            let job = jobs.get(job_id).ok_or_else(|| {
                LocalToolError::InvalidArguments(format!("unknown media job id {job_id}"))
            })?;
            let status = job.status.lock().expect("mutex poisoned").clone();
            status
        };

        match status {
            MediaJobStatus::Running => {
                if timeout.is_zero() || started_at.elapsed() >= timeout {
                    if timeout_on_kill(arguments)? {
                        cancel_media_job(job_id);
                        return Ok(job_snapshot_result("media", job_id, id_field, "cancelled"));
                    }
                    return Ok(job_snapshot_result("media", job_id, id_field, "running"));
                }
                if cancel_token.is_cancelled() {
                    return Ok(job_snapshot_result(
                        "media",
                        job_id,
                        id_field,
                        "interrupted",
                    ));
                }
                thread::sleep(Duration::from_millis(25));
            }
            MediaJobStatus::Completed(result) => {
                media_jobs().lock().expect("mutex poisoned").remove(job_id);
                return Ok(result);
            }
            MediaJobStatus::Failed(error) => {
                media_jobs().lock().expect("mutex poisoned").remove(job_id);
                return Ok(ToolResultContent {
                    context: Some(ContextItem {
                        text: json!({"status": "failed", id_field: job_id, "error": error})
                            .to_string(),
                    }),
                    file: None,
                });
            }
            MediaJobStatus::Cancelled => {
                media_jobs().lock().expect("mutex poisoned").remove(job_id);
                return Ok(job_snapshot_result("media", job_id, id_field, "cancelled"));
            }
        }
    }
}

fn stop_media_job(
    arguments: &Map<String, Value>,
    id_field: &str,
) -> Result<ToolResultContent, LocalToolError> {
    let job_id = string_arg(arguments, id_field)?;
    cancel_media_job(&job_id);
    Ok(job_snapshot_result("media", &job_id, id_field, "cancelled"))
}

fn cancel_media_job(job_id: &str) {
    if let Some(job) = media_jobs().lock().expect("mutex poisoned").remove(job_id) {
        job.cancel.store(true, Ordering::SeqCst);
        if let Some(abort) = job.provider_abort.lock().expect("mutex poisoned").as_ref() {
            let _ = abort.abort();
        }
        *job.status.lock().expect("mutex poisoned") = MediaJobStatus::Cancelled;
    }
}

fn timeout_on_kill(arguments: &Map<String, Value>) -> Result<bool, LocalToolError> {
    Ok(matches!(
        arguments
            .get("on_timeout")
            .and_then(Value::as_str)
            .unwrap_or("continue")
            .to_ascii_lowercase()
            .as_str(),
        "kill" | "cancel"
    ))
}

fn optional_non_empty_string_arg<'a>(
    arguments: &'a Map<String, Value>,
    key: &str,
) -> Result<Option<&'a str>, LocalToolError> {
    let Some(value) = arguments.get(key) else {
        return Ok(None);
    };
    let value = value.as_str().ok_or_else(|| {
        LocalToolError::InvalidArguments(format!("argument {key} must be a string"))
    })?;
    let value = value.trim();
    if value.is_empty() {
        Ok(None)
    } else {
        Ok(Some(value))
    }
}

fn job_snapshot_result(
    tool_name: &str,
    job_id: &str,
    id_field: &str,
    status: &str,
) -> ToolResultContent {
    ToolResultContent {
        context: Some(ContextItem {
            text: json!({
                "tool": tool_name,
                "status": status,
                id_field: job_id,
            })
            .to_string(),
        }),
        file: None,
    }
}

fn provider_message_to_tool_result(message: ChatMessage) -> ToolResultContent {
    let mut text = Vec::new();
    let mut file = None;
    for item in message.data {
        match item {
            ChatMessageItem::Context(context) => text.push(context.text),
            ChatMessageItem::File(item) if file.is_none() => file = Some(item),
            ChatMessageItem::ToolResult(result) => {
                if let Some(context) = result.result.context {
                    text.push(context.text);
                }
                if file.is_none() {
                    file = result.result.file;
                }
            }
            _ => {}
        }
    }
    ToolResultContent {
        context: Some(ContextItem {
            text: if text.is_empty() {
                json!({"status": "completed"}).to_string()
            } else {
                text.join("\n")
            },
        }),
        file,
    }
}

fn tool_usage_log_path(workspace_root: &Path) -> PathBuf {
    workspace_root.join(TOOL_USAGE_LOG_PATH)
}

fn append_tool_usage(
    path: &Path,
    tool_name: &str,
    job_id: &str,
    model_config: &ModelConfig,
    message: &ChatMessage,
) {
    let Some(token_usage) = &message.token_usage else {
        return;
    };

    if let Some(parent) = path.parent() {
        if fs::create_dir_all(parent).is_err() {
            return;
        }
    }

    let Ok(mut file) = OpenOptions::new().create(true).append(true).open(path) else {
        return;
    };

    let record = json!({
        "time_unix_ms": current_unix_ms(),
        "kind": "provider_media_tool",
        "tool_name": tool_name,
        "job_id": job_id,
        "provider_type": model_config.provider_type,
        "model_name": model_config.model_name,
        "token_usage": token_usage_json(token_usage),
    });
    if let Ok(line) = serde_json::to_string(&record) {
        let _ = writeln!(file, "{line}");
    }
}

fn current_unix_ms() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis())
        .unwrap_or_default()
}

fn token_usage_json(token_usage: &TokenUsage) -> Value {
    json!({
        "cache_read": token_usage.cache_read,
        "cache_write": token_usage.cache_write,
        "uncache_input": token_usage.uncache_input,
        "output": token_usage.output,
        "cost_usd": token_usage.cost_usd.as_ref().map(|cost| json!({
            "cache_read": cost.cache_read,
            "cache_write": cost.cache_write,
            "uncache_input": cost.uncache_input,
            "output": cost.output,
        })),
    })
}

fn file_item_from_path(
    arguments: &Map<String, Value>,
    context: &ToolExecutionContext<'_>,
    media_kind: &str,
) -> Result<FileItem, LocalToolError> {
    let path = string_arg(arguments, "path")?;
    if matches!(context.remote_mode, super::ToolRemoteMode::Selectable) {
        if let ExecutionTarget::RemoteSsh { host, cwd } = context.execution_target(arguments)? {
            return remote_file_item_from_path(
                context.workspace_root,
                &host,
                cwd.as_deref(),
                &path,
                media_kind,
            );
        }
    }
    let path = resolve_local_path(context.workspace_root, &path);
    local_file_item_from_path(&path, media_kind)
}

fn local_file_item_from_path(path: &Path, media_kind: &str) -> Result<FileItem, LocalToolError> {
    if !path.is_file() {
        return Err(LocalToolError::InvalidArguments(format!(
            "{} path is not a file: {}",
            media_kind,
            path.display()
        )));
    }
    let canonical = path.canonicalize().map_err(|error| {
        LocalToolError::Io(format!(
            "failed to canonicalize {}: {error}",
            path.display()
        ))
    })?;
    let mut file = FileItem {
        uri: format!("file://{}", canonical.display()),
        name: canonical
            .file_name()
            .map(|name| name.to_string_lossy().to_string()),
        media_type: Some(media_type_for_path(&canonical, media_kind).to_string()),
        width: None,
        height: None,
        state: None,
    };
    enrich_or_mark_crashed(&mut file, &canonical, media_kind);
    Ok(file)
}

fn remote_file_item_from_path(
    workspace_root: &Path,
    host: &str,
    cwd: Option<&str>,
    path: &str,
    media_kind: &str,
) -> Result<FileItem, LocalToolError> {
    let file_name = remote_file_name(path);
    let local_dir = workspace_root.join(".output").join("remote-media");
    fs::create_dir_all(&local_dir).map_err(|error| {
        LocalToolError::Io(format!("failed to create {}: {error}", local_dir.display()))
    })?;
    let local_path = local_dir.join(format!("{}-{}", nonce(), sanitize_file_name(&file_name)));
    let remote_command = remote_media_read_command(cwd, path);
    let output = Command::new("ssh")
        .arg("-o")
        .arg("BatchMode=yes")
        .arg("-o")
        .arg("ConnectTimeout=10")
        .arg("-T")
        .arg(host)
        .arg(remote_command)
        .output()
        .map_err(|error| LocalToolError::Remote(format!("failed to spawn ssh: {error}")))?;
    if !output.status.success() {
        return Err(LocalToolError::Remote(format!(
            "ssh exited with {}; stderr: {}",
            output.status.code().unwrap_or(-1),
            String::from_utf8_lossy(&output.stderr)
        )));
    }
    fs::write(&local_path, &output.stdout).map_err(|error| {
        LocalToolError::Io(format!("failed to write {}: {error}", local_path.display()))
    })?;
    local_file_item_from_path(&local_path, media_kind)
}

fn remote_media_read_command(cwd: Option<&str>, path: &str) -> String {
    let script = r#"
import os, sys

path = os.path.expanduser(sys.argv[1])
if not os.path.isfile(path):
    raise SystemExit(f"path is not a file: {path}")
with open(path, "rb") as handle:
    sys.stdout.buffer.write(handle.read())
"#;
    let command = format!("python3 -c {} {}", shell_quote(script), shell_quote(path));
    match cwd.map(str::trim).filter(|cwd| !cwd.is_empty()) {
        Some(cwd) => format!("cd {} && {}", shell_quote(cwd), command),
        None => command,
    }
}

fn remote_file_name(path: &str) -> String {
    Path::new(path)
        .file_name()
        .and_then(|name| name.to_str())
        .filter(|name| !name.is_empty())
        .unwrap_or("remote-media")
        .to_string()
}

fn sanitize_file_name(name: &str) -> String {
    name.chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || matches!(ch, '.' | '_' | '-') {
                ch
            } else {
                '_'
            }
        })
        .collect()
}

fn enrich_or_mark_crashed(file: &mut FileItem, path: &Path, media_kind: &str) {
    match media_kind {
        "image" => match fs::read(path)
            .map_err(|error| format!("failed to read image: {error}"))
            .and_then(|bytes| {
                let reader = ImageReader::new(Cursor::new(bytes))
                    .with_guessed_format()
                    .map_err(|error| format!("failed to detect image format: {error}"))?;
                let media_type = reader
                    .format()
                    .and_then(image_format_media_type)
                    .unwrap_or("image/*")
                    .to_string();
                let image = reader
                    .decode()
                    .map_err(|error| format!("failed to decode image: {error}"))?;
                Ok((media_type, image.width(), image.height()))
            }) {
            Ok((media_type, width, height)) => {
                file.media_type = Some(media_type);
                file.width = Some(width);
                file.height = Some(height);
            }
            Err(reason) => {
                file.state = Some(FileState::Crashed { reason });
            }
        },
        "pdf" => match fs::read(path) {
            Ok(bytes) if bytes.starts_with(b"%PDF-") => {
                file.media_type = Some("application/pdf".to_string())
            }
            Ok(_) => {
                file.state = Some(FileState::Crashed {
                    reason: "file is not a valid PDF by signature".to_string(),
                });
            }
            Err(error) => {
                file.state = Some(FileState::Crashed {
                    reason: format!("failed to read PDF: {error}"),
                });
            }
        },
        "audio" => match fs::read(path) {
            Ok(bytes) => match audio_media_type_from_signature(&bytes) {
                Some(media_type) => file.media_type = Some(media_type.to_string()),
                None => {
                    file.state = Some(FileState::Crashed {
                        reason: "file is not a supported audio signature".to_string(),
                    });
                }
            },
            Err(error) => {
                file.state = Some(FileState::Crashed {
                    reason: format!("failed to read audio: {error}"),
                });
            }
        },
        _ => {}
    }
}

fn collect_optional_images(
    arguments: &Map<String, Value>,
    context: &ToolExecutionContext<'_>,
) -> Result<Vec<FileItem>, LocalToolError> {
    let Some(images) = arguments.get("images").and_then(Value::as_array) else {
        return Ok(Vec::new());
    };
    images
        .iter()
        .map(|image| {
            let path = image.as_str().ok_or_else(|| {
                LocalToolError::InvalidArguments("images entries must be strings".to_string())
            })?;
            let mut map = Map::new();
            map.insert("path".to_string(), Value::String(path.to_string()));
            file_item_from_path(&map, context, "image")
        })
        .collect()
}

fn nonce() -> String {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos().to_string())
        .unwrap_or_else(|_| "0".to_string())
}

fn media_type_for_path(path: &Path, media_kind: &str) -> &'static str {
    match path
        .extension()
        .and_then(|extension| extension.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase()
        .as_str()
    {
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "webp" => "image/webp",
        "gif" => "image/gif",
        "pdf" => "application/pdf",
        "mp3" => "audio/mpeg",
        "wav" => "audio/wav",
        "m4a" => "audio/mp4",
        "ogg" => "audio/ogg",
        "flac" => "audio/flac",
        _ if media_kind == "image" => "image/*",
        _ if media_kind == "pdf" => "application/pdf",
        _ if media_kind == "audio" => "audio/*",
        _ => "application/octet-stream",
    }
}

fn image_format_media_type(format: image::ImageFormat) -> Option<&'static str> {
    match format {
        image::ImageFormat::Png => Some("image/png"),
        image::ImageFormat::Jpeg => Some("image/jpeg"),
        image::ImageFormat::WebP => Some("image/webp"),
        image::ImageFormat::Gif => Some("image/gif"),
        image::ImageFormat::Bmp => Some("image/bmp"),
        image::ImageFormat::Tiff => Some("image/tiff"),
        _ => None,
    }
}

fn audio_media_type_from_signature(bytes: &[u8]) -> Option<&'static str> {
    if bytes.starts_with(b"ID3") || is_mp3_frame(bytes) {
        Some("audio/mpeg")
    } else if bytes.starts_with(b"fLaC") {
        Some("audio/flac")
    } else if bytes.starts_with(b"OggS") {
        Some("audio/ogg")
    } else if bytes.len() >= 12 && &bytes[0..4] == b"RIFF" && &bytes[8..12] == b"WAVE" {
        Some("audio/wav")
    } else if bytes.len() >= 12 && &bytes[4..8] == b"ftyp" {
        Some("audio/mp4")
    } else {
        None
    }
}

fn is_mp3_frame(bytes: &[u8]) -> bool {
    bytes.len() >= 2 && bytes[0] == 0xFF && (bytes[1] & 0xE0) == 0xE0
}

fn next_media_job_id(prefix: &str) -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or_default();
    format!("{prefix}_{nanos}")
}

#[cfg(test)]
mod tests {
    use std::{
        fs,
        io::{Read, Write},
        net::TcpListener,
        path::{Path, PathBuf},
        sync::{
            atomic::{AtomicBool, Ordering},
            Arc, Mutex, OnceLock,
        },
        thread,
        time::{Duration, Instant},
    };

    use serde_json::json;

    use super::*;
    use crate::{
        model_config::{ModelCapability, ModelConfig, ProviderType, RetryMode, TokenEstimatorType},
        providers::init_global_provider_fork_server,
        session_actor::{tool_runtime::ToolCancellationToken, ToolRemoteMode},
    };

    static MEDIA_TEST_LOCK: OnceLock<Mutex<()>> = OnceLock::new();

    #[test]
    #[cfg(unix)]
    fn provider_backed_media_job_returns_completed_worker_result() {
        let _lock = MEDIA_TEST_LOCK
            .get_or_init(|| Mutex::new(()))
            .lock()
            .expect("media test lock should not be poisoned");
        let temp = TempDir::new("provider-media-complete");
        let server = TestHttpServer::responding(openrouter_body("media ok"));
        let image = temp.write_file("input.png", b"not-a-real-image");
        let _env = EnvVarGuard::set("OPENROUTER_API_KEY_TEST", "test-key");
        init_global_provider_fork_server().expect("forkserver should start");

        let mut arguments = Map::new();
        arguments.insert(
            "path".to_string(),
            Value::String(image.to_string_lossy().to_string()),
        );
        arguments.insert("return_immediate".to_string(), Value::Bool(false));
        arguments.insert("wait_timeout_seconds".to_string(), json!(2.0));

        let result = execute_provider_backed_media_tool(
            "image_analysis",
            ProviderBackedToolKind::ImageAnalysis,
            &test_model_config(server.url()),
            &arguments,
            &test_context(temp.path()),
        )
        .expect("provider-backed media tool should complete");

        assert_eq!(result.context.unwrap().text, "media ok");
        let usage_log = fs::read_to_string(temp.path().join(TOOL_USAGE_LOG_PATH))
            .expect("tool usage log should be written");
        let usage_record: Value = serde_json::from_str(
            usage_log
                .lines()
                .last()
                .expect("usage log line should exist"),
        )
        .expect("usage log line should be JSON");
        assert_eq!(usage_record["kind"], "provider_media_tool");
        assert_eq!(usage_record["tool_name"], "image_analysis");
        assert_eq!(usage_record["model_name"], "openai/gpt-4o-mini");
        assert_eq!(usage_record["token_usage"]["uncache_input"], 1);
        assert_eq!(usage_record["token_usage"]["output"], 1);
    }

    #[test]
    #[cfg(unix)]
    fn image_generation_empty_generation_id_starts_new_job() {
        let _lock = MEDIA_TEST_LOCK
            .get_or_init(|| Mutex::new(()))
            .lock()
            .expect("media test lock should not be poisoned");
        let temp = TempDir::new("image-generation-empty-id");
        let _env = EnvVarGuard::set("OPENROUTER_API_KEY_TEST", "test-key");
        init_global_provider_fork_server().expect("forkserver should start");

        let mut arguments = Map::new();
        arguments.insert(
            "prompt".to_string(),
            Value::String("draw a quiet moon".to_string()),
        );
        arguments.insert("generation_id".to_string(), Value::String(String::new()));
        arguments.insert("return_immediate".to_string(), Value::Bool(true));

        let result = execute_provider_backed_media_tool(
            "image_generation",
            ProviderBackedToolKind::ImageGeneration,
            &test_model_config("http://127.0.0.1:9/v1/chat/completions".to_string()),
            &arguments,
            &test_context(temp.path()),
        )
        .expect("empty generation_id should start a new job");
        let payload: Value =
            serde_json::from_str(&result.context.unwrap().text).expect("valid json");

        let job_id = payload["generation_id"]
            .as_str()
            .expect("generation_id should be returned");
        assert!(job_id.starts_with("image_generation_"));
        assert_eq!(payload["status"], "running");
        cancel_media_job(job_id);
    }

    #[test]
    #[cfg(unix)]
    fn analysis_empty_job_id_starts_new_job() {
        let _lock = MEDIA_TEST_LOCK
            .get_or_init(|| Mutex::new(()))
            .lock()
            .expect("media test lock should not be poisoned");
        let temp = TempDir::new("analysis-empty-id");
        let image = temp.write_file("input.png", b"not-a-real-image");
        let _env = EnvVarGuard::set("OPENROUTER_API_KEY_TEST", "test-key");
        init_global_provider_fork_server().expect("forkserver should start");

        let mut arguments = Map::new();
        arguments.insert(
            "path".to_string(),
            Value::String(image.to_string_lossy().to_string()),
        );
        arguments.insert("image_id".to_string(), Value::String("   ".to_string()));
        arguments.insert("return_immediate".to_string(), Value::Bool(true));

        let result = execute_provider_backed_media_tool(
            "image_analysis",
            ProviderBackedToolKind::ImageAnalysis,
            &test_model_config("http://127.0.0.1:9/v1/chat/completions".to_string()),
            &arguments,
            &test_context(temp.path()),
        )
        .expect("empty image_id should start a new job");
        let payload: Value =
            serde_json::from_str(&result.context.unwrap().text).expect("valid json");

        let job_id = payload["image_id"]
            .as_str()
            .expect("image_id should be returned");
        assert!(job_id.starts_with("image_"));
        assert_eq!(payload["status"], "running");
        cancel_media_job(job_id);
    }

    #[test]
    #[cfg(unix)]
    fn provider_backed_media_stop_kills_worker_process() {
        let _lock = MEDIA_TEST_LOCK
            .get_or_init(|| Mutex::new(()))
            .lock()
            .expect("media test lock should not be poisoned");
        let temp = TempDir::new("provider-media-stop");
        let accepted = Arc::new(AtomicBool::new(false));
        let closed = Arc::new(AtomicBool::new(false));
        let server = TestHttpServer::hanging(accepted.clone(), closed.clone());
        let image = temp.write_file("input.png", b"not-a-real-image");
        let _env = EnvVarGuard::set("OPENROUTER_API_KEY_TEST", "test-key");
        init_global_provider_fork_server().expect("forkserver should start");

        let mut arguments = Map::new();
        arguments.insert(
            "path".to_string(),
            Value::String(image.to_string_lossy().to_string()),
        );
        let start_result = execute_provider_backed_media_tool(
            "image_analysis",
            ProviderBackedToolKind::ImageAnalysis,
            &test_model_config(server.url()),
            &arguments,
            &test_context(temp.path()),
        )
        .expect("provider-backed media tool should start");
        let start_payload: Value =
            serde_json::from_str(&start_result.context.unwrap().text).expect("valid json");
        let job_id = start_payload["image_id"]
            .as_str()
            .expect("image_id should be returned")
            .to_string();

        wait_until(Duration::from_secs(2), || accepted.load(Ordering::SeqCst))
            .expect("provider worker should connect before stop");

        let mut stop_arguments = Map::new();
        stop_arguments.insert("image_id".to_string(), Value::String(job_id));
        let stop_result =
            execute_media_tool("image_stop", &stop_arguments, &test_context(temp.path()))
                .expect("image_stop should run")
                .expect("image_stop should return result");
        let stop_payload: Value =
            serde_json::from_str(&stop_result.context.unwrap().text).expect("valid json");

        assert_eq!(stop_payload["status"], "cancelled");
        wait_until(Duration::from_secs(2), || closed.load(Ordering::SeqCst))
            .expect("provider worker connection should close after stop");
    }

    #[test]
    #[cfg(unix)]
    fn provider_backed_media_wait_returns_snapshot_on_interrupt() {
        let _lock = MEDIA_TEST_LOCK
            .get_or_init(|| Mutex::new(()))
            .lock()
            .expect("media test lock should not be poisoned");
        let temp = TempDir::new("provider-media-interrupt");
        let accepted = Arc::new(AtomicBool::new(false));
        let closed = Arc::new(AtomicBool::new(false));
        let server = TestHttpServer::hanging(accepted.clone(), closed.clone());
        let image = temp.write_file("input.png", b"not-a-real-image");
        let _env = EnvVarGuard::set("OPENROUTER_API_KEY_TEST", "test-key");
        init_global_provider_fork_server().expect("forkserver should start");

        let mut arguments = Map::new();
        arguments.insert(
            "path".to_string(),
            Value::String(image.to_string_lossy().to_string()),
        );
        let start_result = execute_provider_backed_media_tool(
            "image_analysis",
            ProviderBackedToolKind::ImageAnalysis,
            &test_model_config(server.url()),
            &arguments,
            &test_context(temp.path()),
        )
        .expect("provider-backed media tool should start");
        let start_payload: Value =
            serde_json::from_str(&start_result.context.unwrap().text).expect("valid json");
        let job_id = start_payload["image_id"]
            .as_str()
            .expect("image_id should be returned")
            .to_string();

        wait_until(Duration::from_secs(2), || accepted.load(Ordering::SeqCst))
            .expect("provider worker should connect before interrupt");

        let cancel_token = ToolCancellationToken::default();
        cancel_token.cancel();
        let mut wait_arguments = Map::new();
        wait_arguments.insert("image_id".to_string(), Value::String(job_id.clone()));
        wait_arguments.insert("wait_timeout_seconds".to_string(), json!(30.0));
        let wait_result = execute_provider_backed_media_tool(
            "image_analysis",
            ProviderBackedToolKind::ImageAnalysis,
            &test_model_config(server.url()),
            &wait_arguments,
            &test_context_with_cancel(temp.path(), cancel_token),
        )
        .expect("interrupted wait should return snapshot");
        let wait_payload: Value =
            serde_json::from_str(&wait_result.context.unwrap().text).expect("valid json");

        assert_eq!(wait_payload["status"], "interrupted");
        assert_eq!(wait_payload["image_id"], job_id);

        let mut stop_arguments = Map::new();
        stop_arguments.insert("image_id".to_string(), Value::String(job_id));
        let _ = execute_media_tool("image_stop", &stop_arguments, &test_context(temp.path()));
        wait_until(Duration::from_secs(2), || closed.load(Ordering::SeqCst))
            .expect("provider worker connection should close after cleanup");
    }

    fn test_context(root: &Path) -> ToolExecutionContext<'_> {
        test_context_with_cancel(root, ToolCancellationToken::default())
    }

    fn test_context_with_cancel(
        root: &Path,
        cancel_token: ToolCancellationToken,
    ) -> ToolExecutionContext<'_> {
        static REMOTE_MODE: ToolRemoteMode = ToolRemoteMode::Selectable;
        ToolExecutionContext {
            workspace_root: root,
            remote_mode: &REMOTE_MODE,
            cancel_token,
        }
    }

    fn test_model_config(url: String) -> ModelConfig {
        ModelConfig {
            provider_type: ProviderType::OpenRouterCompletion,
            model_name: "openai/gpt-4o-mini".to_string(),
            url,
            api_key_env: "OPENROUTER_API_KEY_TEST".to_string(),
            capabilities: vec![ModelCapability::Chat],
            token_max_context: 128_000,
            cache_timeout: 300,
            conn_timeout: 10,
            request_timeout: 600,
            max_request_size: 30 * 1024 * 1024,
            retry_mode: RetryMode::Once,
            reasoning: None,
            token_estimator_type: TokenEstimatorType::Local,
            multimodal_estimator: None,
            multimodal_input: None,
            token_estimator_url: None,
        }
    }

    fn wait_until(timeout: Duration, condition: impl Fn() -> bool) -> Result<(), String> {
        let started = Instant::now();
        while started.elapsed() < timeout {
            if condition() {
                return Ok(());
            }
            thread::sleep(Duration::from_millis(25));
        }
        Err("condition was not met before timeout".to_string())
    }

    struct EnvVarGuard {
        key: &'static str,
        previous: Option<std::ffi::OsString>,
    }

    impl EnvVarGuard {
        fn set(key: &'static str, value: &str) -> Self {
            let previous = std::env::var_os(key);
            std::env::set_var(key, value);
            Self { key, previous }
        }
    }

    impl Drop for EnvVarGuard {
        fn drop(&mut self) {
            match &self.previous {
                Some(value) => std::env::set_var(self.key, value),
                None => std::env::remove_var(self.key),
            }
        }
    }

    struct TempDir {
        path: PathBuf,
    }

    impl TempDir {
        fn new(prefix: &str) -> Self {
            let path = std::env::temp_dir().join(format!(
                "stellaclaw-{prefix}-{}-{}",
                std::process::id(),
                rand::random::<u64>()
            ));
            fs::create_dir_all(&path).expect("temp dir should be created");
            Self { path }
        }

        fn path(&self) -> &Path {
            &self.path
        }

        fn write_file(&self, name: &str, content: &[u8]) -> PathBuf {
            let path = self.path.join(name);
            fs::write(&path, content).expect("temp file should be written");
            path
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.path);
        }
    }

    struct TestHttpServer {
        url: String,
    }

    impl TestHttpServer {
        fn responding(body: String) -> Self {
            let listener = TcpListener::bind("127.0.0.1:0").expect("test server should bind");
            let url = format!(
                "http://{}/api/v1/chat/completions",
                listener.local_addr().unwrap()
            );
            thread::spawn(move || {
                if let Ok((mut stream, _)) = listener.accept() {
                    let mut request = [0_u8; 4096];
                    let _ = stream.read(&mut request);
                    let response = format!(
                        "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\n\r\n{}",
                        body.len(),
                        body
                    );
                    let _ = stream.write_all(response.as_bytes());
                    let _ = stream.flush();
                }
            });
            Self { url }
        }

        fn hanging(accepted: Arc<AtomicBool>, closed: Arc<AtomicBool>) -> Self {
            let listener = TcpListener::bind("127.0.0.1:0").expect("test server should bind");
            let url = format!(
                "http://{}/api/v1/chat/completions",
                listener.local_addr().unwrap()
            );
            thread::spawn(move || {
                if let Ok((mut stream, _)) = listener.accept() {
                    accepted.store(true, Ordering::SeqCst);
                    let mut request = [0_u8; 4096];
                    let _ = stream.read(&mut request);
                    let mut byte = [0_u8; 1];
                    loop {
                        match stream.read(&mut byte) {
                            Ok(0) => {
                                closed.store(true, Ordering::SeqCst);
                                break;
                            }
                            Ok(_) => {}
                            Err(_) => {
                                closed.store(true, Ordering::SeqCst);
                                break;
                            }
                        }
                    }
                }
            });
            Self { url }
        }

        fn url(&self) -> String {
            self.url.clone()
        }
    }

    fn openrouter_body(text: &str) -> String {
        format!(
            r#"{{
                "id": "gen_test",
                "model": "openai/gpt-4o-mini",
                "choices": [
                    {{
                        "finish_reason": "stop",
                        "message": {{
                            "content": {text:?},
                            "tool_calls": []
                        }}
                    }}
                ],
                "usage": {{
                    "prompt_tokens": 1,
                    "completion_tokens": 1
                }}
            }}"#
        )
    }
}
