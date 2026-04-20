use super::args::{f64_arg, string_arg, string_arg_with_default, string_array_arg};
use super::exec::{process_is_running, read_exit_code, record_exit_code, terminate_process_pid};
use super::runtime_state::{
    BackgroundTaskMetadata, background_task_dir, background_task_dir_if_exists,
    background_task_is_running, iter_metadata_json_files, read_background_task_metadata,
    read_status_json, run_interruptible_worker_job, spawn_background_worker_process,
    write_background_task_metadata,
};
use super::{InterruptSignal, Tool, compact_tool_status_fields_for_model, resolve_path};
use crate::config::UpstreamConfig;
use crate::tool_worker::ToolWorkerJob;
use anyhow::{Context, Result, anyhow};
use serde_json::{Map, Value, json};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};
use uuid::Uuid;

fn read_image_task_snapshot(runtime_state_root: &Path, image_id: &str) -> Result<Value> {
    let metadata = read_background_task_metadata(
        &background_task_dir(runtime_state_root, "image_tasks")?,
        image_id,
    )?;
    let mut snapshot = read_status_json(Path::new(&metadata.status_path))?;
    if snapshot
        .get("running")
        .and_then(Value::as_bool)
        .unwrap_or(false)
        && (read_exit_code(Path::new(&metadata.exit_code_path)).is_some()
            || !process_is_running(metadata.pid))
    {
        snapshot = json!({
            "image_id": image_id,
            "path": snapshot["path"].clone(),
            "question": snapshot["question"].clone(),
            "running": false,
            "completed": false,
            "cancelled": false,
            "failed": true,
            "error": "image worker exited unexpectedly",
        });
    }
    Ok(snapshot)
}

const IMAGE_START_DEFAULT_WAIT_TIMEOUT_SECONDS: f64 = 270.0;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ImageTimeoutAction {
    Continue,
    Kill,
}

fn compact_image_status_for_model(mut value: Value) -> Value {
    compact_tool_status_fields_for_model(&mut value);
    value
}

impl ImageTimeoutAction {
    fn as_str(self) -> &'static str {
        match self {
            Self::Continue => "continue",
            Self::Kill => "kill",
        }
    }
}

fn image_timeout_action_arg(
    arguments: &serde_json::Map<String, Value>,
    key: &str,
    default: ImageTimeoutAction,
) -> Result<ImageTimeoutAction> {
    let Some(value) = arguments.get(key) else {
        return Ok(default);
    };
    let text = value
        .as_str()
        .ok_or_else(|| anyhow!("argument {} must be a string", key))?
        .trim()
        .to_ascii_lowercase();
    match text.as_str() {
        "continue" => Ok(ImageTimeoutAction::Continue),
        "kill" => Ok(ImageTimeoutAction::Kill),
        _ => Err(anyhow!("argument {} must be one of: continue, kill", key)),
    }
}

pub(super) fn cleanup_image_tasks(runtime_state_root: &Path) -> Result<usize> {
    let Some(task_dir) = background_task_dir_if_exists(runtime_state_root, "image_tasks") else {
        return Ok(0);
    };
    let mut cancelled = 0usize;
    for path in iter_metadata_json_files(&task_dir)? {
        let raw = fs::read_to_string(&path)
            .with_context(|| format!("failed to read {}", path.display()))?;
        let metadata: BackgroundTaskMetadata =
            serde_json::from_str(&raw).context("failed to parse background task metadata")?;
        if !background_task_is_running(&metadata) {
            continue;
        }
        let previous = read_status_json(Path::new(&metadata.status_path)).ok();
        terminate_process_pid(metadata.pid);
        let _ = record_exit_code(Path::new(&metadata.exit_code_path), -9);
        let snapshot = json!({
            "image_id": metadata.task_id,
            "path": previous.as_ref().and_then(|value| value.get("path")).cloned().unwrap_or(Value::String(String::new())),
            "question": previous.as_ref().and_then(|value| value.get("question")).cloned().unwrap_or(Value::String(String::new())),
            "running": false,
            "completed": false,
            "cancelled": true,
            "failed": false,
            "reason": "session_destroyed",
        });
        fs::write(
            Path::new(&metadata.status_path),
            serde_json::to_vec_pretty(&snapshot)
                .context("failed to serialize image cleanup snapshot")?,
        )
        .with_context(|| format!("failed to write {}", metadata.status_path))?;
        cancelled = cancelled.saturating_add(1);
    }
    Ok(cancelled)
}

fn cancel_image_task(runtime_state_root: &Path, image_id: &str) -> Result<Value> {
    let task_dir = background_task_dir(runtime_state_root, "image_tasks")?;
    let metadata = read_background_task_metadata(&task_dir, image_id)?;
    terminate_process_pid(metadata.pid);
    let _ = record_exit_code(Path::new(&metadata.exit_code_path), -9);
    let previous = read_image_task_snapshot(runtime_state_root, image_id).ok();
    let snapshot = json!({
        "image_id": image_id,
        "path": previous
            .as_ref()
            .and_then(|value| value.get("path").cloned())
            .unwrap_or(Value::String(String::new())),
        "question": previous
            .as_ref()
            .and_then(|value| value.get("question").cloned())
            .unwrap_or(Value::String(String::new())),
        "running": false,
        "completed": false,
        "cancelled": true,
        "failed": false,
    });
    fs::write(
        Path::new(&metadata.status_path),
        serde_json::to_vec_pretty(&snapshot)
            .context("failed to serialize image cancel snapshot")?,
    )
    .with_context(|| format!("failed to write {}", metadata.status_path))?;
    Ok(snapshot)
}

fn wait_for_image_task(
    runtime_state_root: &Path,
    image_id: &str,
    wait_timeout_seconds: f64,
    on_timeout: ImageTimeoutAction,
    cancel_flag: Option<&Arc<InterruptSignal>>,
) -> Result<Value> {
    if !wait_timeout_seconds.is_finite() || wait_timeout_seconds < 0.0 {
        return Err(anyhow!(
            "argument wait_timeout_seconds must be a finite non-negative number"
        ));
    }
    let deadline = Instant::now() + Duration::from_secs_f64(wait_timeout_seconds);
    let cancel_receiver = cancel_flag.map(|signal| signal.subscribe());
    loop {
        let snapshot = read_image_task_snapshot(runtime_state_root, image_id)?;
        let finished = snapshot
            .get("running")
            .and_then(Value::as_bool)
            .is_some_and(|running| !running);
        if finished {
            return Ok(compact_image_status_for_model(snapshot));
        }
        if let Some(cancel_receiver) = &cancel_receiver
            && cancel_receiver.try_recv().is_ok()
        {
            return Ok(compact_image_status_for_model(json!({
                "interrupted": true,
                "image": snapshot,
            })));
        }
        if Instant::now() >= deadline {
            if on_timeout == ImageTimeoutAction::Kill {
                let mut cancelled = cancel_image_task(runtime_state_root, image_id)?;
                if let Some(object) = cancelled.as_object_mut() {
                    object.insert("wait_timed_out".to_string(), Value::Bool(true));
                    object.insert(
                        "on_timeout".to_string(),
                        Value::String(on_timeout.as_str().to_string()),
                    );
                }
                return Ok(compact_image_status_for_model(cancelled));
            }
            let mut object = snapshot
                .as_object()
                .cloned()
                .ok_or_else(|| anyhow!("image snapshot must be a JSON object"))?;
            object.insert("wait_timed_out".to_string(), Value::Bool(true));
            object.insert(
                "on_timeout".to_string(),
                Value::String(on_timeout.as_str().to_string()),
            );
            object.insert("running".to_string(), Value::Bool(true));
            object.insert("completed".to_string(), Value::Bool(false));
            return Ok(compact_image_status_for_model(Value::Object(object)));
        }
        if let Some(cancel_receiver) = &cancel_receiver {
            crossbeam_channel::select! {
                recv(cancel_receiver) -> _ => {
                    return Ok(compact_image_status_for_model(json!({
                        "interrupted": true,
                        "image": snapshot,
                    })));
                }
                recv(crossbeam_channel::after(Duration::from_millis(200))) -> _ => {}
            }
        } else {
            thread::sleep(Duration::from_millis(200));
        }
    }
}

pub(super) fn image_start_tool(
    workspace_root: PathBuf,
    runtime_state_root: PathBuf,
    upstream: UpstreamConfig,
    image_tool_upstream: Option<UpstreamConfig>,
    cancel_flag: Option<Arc<InterruptSignal>>,
) -> Tool {
    Tool::new_interruptible(
        "image_start",
        "Start inspecting a local image file with the model's multimodal capability.",
        json!({
            "type": "object",
            "properties": {
                "path": {"type": "string"},
                "question": {"type": "string"},
                "return_immediate": {"type": "boolean"},
                "wait_timeout_seconds": {"type": "number"},
                "on_timeout": {"type": "string", "enum": ["continue", "kill", "CONTINUE", "KILL"]}
            },
            "required": ["path", "question"],
            "additionalProperties": false
        }),
        move |arguments| {
            let arguments = arguments
                .as_object()
                .ok_or_else(|| anyhow!("tool arguments must be an object"))?;
            let path = resolve_path(&string_arg(arguments, "path")?, &workspace_root);
            let question = string_arg(arguments, "question")?;
            let return_immediate = arguments
                .get("return_immediate")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            let wait_timeout_seconds = arguments
                .get("wait_timeout_seconds")
                .map(|_| f64_arg(arguments, "wait_timeout_seconds"))
                .transpose()?
                .unwrap_or(IMAGE_START_DEFAULT_WAIT_TIMEOUT_SECONDS);
            let on_timeout =
                image_timeout_action_arg(arguments, "on_timeout", ImageTimeoutAction::Continue)?;
            let upstream = image_tool_upstream
                .clone()
                .unwrap_or_else(|| upstream.clone());
            let image_id = Uuid::new_v4().to_string();
            let task_dir = background_task_dir(&runtime_state_root, "image_tasks")?;
            let status_path = task_dir.join(format!("{}.status.json", image_id));
            let initial = json!({
                "image_id": image_id,
                "path": path.display().to_string(),
                "question": question,
                "running": true,
                "completed": false,
                "cancelled": false,
                "failed": false,
            });
            fs::write(
                &status_path,
                serde_json::to_vec_pretty(&initial).context("failed to serialize image status")?,
            )
            .with_context(|| format!("failed to write {}", status_path.display()))?;
            let job = ToolWorkerJob::Image {
                image_id: image_id.clone(),
                path: path.display().to_string(),
                question: question.to_string(),
                upstream,
                status_path: status_path.display().to_string(),
            };
            let metadata =
                spawn_background_worker_process(&runtime_state_root, "image", &image_id, &job)?;
            write_background_task_metadata(&task_dir, &metadata)?;
            if return_immediate {
                read_image_task_snapshot(&runtime_state_root, &image_id)
                    .map(compact_image_status_for_model)
            } else {
                wait_for_image_task(
                    &runtime_state_root,
                    &image_id,
                    wait_timeout_seconds,
                    on_timeout,
                    cancel_flag.as_ref(),
                )
            }
        },
    )
}

pub(super) fn image_load_tool(
    workspace_root: PathBuf,
    upstream: UpstreamConfig,
    _cancel_flag: Option<Arc<InterruptSignal>>,
) -> Tool {
    Tool::new(
        "image_load",
        "Load a local image file into the next model request for direct multimodal inspection by the current model. Returns immediately. Do not call image_load more than 3 times in the same assistant tool-call batch; excess image_load calls in that batch will fail. Load more images after inspecting the first batch.",
        json!({
            "type": "object",
            "properties": {
                "path": {"type": "string"}
            },
            "required": ["path"],
            "additionalProperties": false
        }),
        move |arguments| {
            let arguments = arguments
                .as_object()
                .ok_or_else(|| anyhow!("tool arguments must be an object"))?;
            if !upstream.native_image_input {
                return Err(anyhow!(
                    "image_load requires a model with native image input support"
                ));
            }
            let path = resolve_path(&string_arg(arguments, "path")?, &workspace_root);
            Ok(json!({
                "loaded": true,
                "kind": "synthetic_user_multimodal",
                "media": [{
                    "type": "input_image",
                    "path": path.display().to_string(),
                }],
                "path": path.display().to_string(),
            }))
        },
    )
}

pub(super) fn pdf_load_tool(
    workspace_root: PathBuf,
    upstream: UpstreamConfig,
    _cancel_flag: Option<Arc<InterruptSignal>>,
) -> Tool {
    Tool::new(
        "pdf_load",
        "Load a local PDF file into the next model request for direct inspection by the current model. Returns immediately.",
        json!({
            "type": "object",
            "properties": {
                "path": {"type": "string"}
            },
            "required": ["path"],
            "additionalProperties": false
        }),
        move |arguments| {
            let arguments = arguments
                .as_object()
                .ok_or_else(|| anyhow!("tool arguments must be an object"))?;
            if !upstream.native_pdf_input {
                return Err(anyhow!(
                    "pdf_load requires a model with native PDF input support"
                ));
            }
            let path = resolve_path(&string_arg(arguments, "path")?, &workspace_root);
            Ok(json!({
                "loaded": true,
                "kind": "synthetic_user_multimodal",
                "media": [{
                    "type": "input_file",
                    "path": path.display().to_string(),
                    "filename": path.file_name().and_then(|value| value.to_str()).unwrap_or("document.pdf"),
                }],
                "path": path.display().to_string(),
            }))
        },
    )
}

pub(super) fn audio_load_tool(
    workspace_root: PathBuf,
    upstream: UpstreamConfig,
    _cancel_flag: Option<Arc<InterruptSignal>>,
) -> Tool {
    Tool::new(
        "audio_load",
        "Load a local audio file into the next model request for direct inspection by the current model. Returns immediately.",
        json!({
            "type": "object",
            "properties": {
                "path": {"type": "string"}
            },
            "required": ["path"],
            "additionalProperties": false
        }),
        move |arguments| {
            let arguments = arguments
                .as_object()
                .ok_or_else(|| anyhow!("tool arguments must be an object"))?;
            if !upstream.native_audio_input {
                return Err(anyhow!(
                    "audio_load requires a model with native audio input support"
                ));
            }
            let path = resolve_path(&string_arg(arguments, "path")?, &workspace_root);
            let format = path
                .extension()
                .and_then(|value| value.to_str())
                .unwrap_or_default()
                .to_ascii_lowercase();
            Ok(json!({
                "loaded": true,
                "kind": "synthetic_user_multimodal",
                "media": [{
                    "type": "input_audio",
                    "path": path.display().to_string(),
                    "format": format,
                }],
                "path": path.display().to_string(),
            }))
        },
    )
}

pub(super) fn maybe_add_images_schema(
    properties: &mut Map<String, Value>,
    upstream_supports_vision: bool,
) {
    if upstream_supports_vision {
        properties.insert(
            "images".to_string(),
            json!({
                "type": "array",
                "items": { "type": "string" }
            }),
        );
    }
}

pub(super) fn pdf_query_tool(
    workspace_root: PathBuf,
    runtime_state_root: PathBuf,
    upstream: UpstreamConfig,
    cancel_flag: Option<Arc<InterruptSignal>>,
) -> Tool {
    let mut properties = Map::new();
    properties.insert("path".to_string(), json!({"type": "string"}));
    properties.insert("question".to_string(), json!({"type": "string"}));
    maybe_add_images_schema(&mut properties, upstream.supports_vision_input);
    Tool::new_interruptible(
        "pdf_query",
        "Ask a question about a local PDF using a helper model. This can be interrupted and will cancel immediately.",
        Value::Object(Map::from_iter([
            ("type".to_string(), Value::String("object".to_string())),
            ("properties".to_string(), Value::Object(properties)),
            ("required".to_string(), json!(["path", "question"])),
            ("additionalProperties".to_string(), Value::Bool(false)),
        ])),
        move |arguments| {
            let arguments = arguments
                .as_object()
                .ok_or_else(|| anyhow!("tool arguments must be an object"))?;
            let path = resolve_path(&string_arg(arguments, "path")?, &workspace_root);
            let question = string_arg(arguments, "question")?;
            let images = string_array_arg(arguments, "images")?
                .into_iter()
                .map(|path| resolve_path(&path, &workspace_root).display().to_string())
                .collect::<Vec<_>>();
            let result = run_interruptible_worker_job(
                &runtime_state_root,
                &ToolWorkerJob::Pdf {
                    path: path.display().to_string(),
                    question,
                    upstream: upstream.clone(),
                    images,
                },
                upstream.timeout_seconds,
                cancel_flag.as_ref(),
            )?;
            Ok(result)
        },
    )
}

pub(super) fn audio_transcribe_tool(
    workspace_root: PathBuf,
    runtime_state_root: PathBuf,
    upstream: UpstreamConfig,
    cancel_flag: Option<Arc<InterruptSignal>>,
) -> Tool {
    let mut properties = Map::new();
    properties.insert("path".to_string(), json!({"type": "string"}));
    properties.insert("question".to_string(), json!({"type": "string"}));
    maybe_add_images_schema(&mut properties, upstream.supports_vision_input);
    Tool::new_interruptible(
        "audio_transcribe",
        "Transcribe or inspect a local audio file using a helper model. This can be interrupted and will cancel immediately.",
        Value::Object(Map::from_iter([
            ("type".to_string(), Value::String("object".to_string())),
            ("properties".to_string(), Value::Object(properties)),
            ("required".to_string(), json!(["path"])),
            ("additionalProperties".to_string(), Value::Bool(false)),
        ])),
        move |arguments| {
            let arguments = arguments
                .as_object()
                .ok_or_else(|| anyhow!("tool arguments must be an object"))?;
            let path = resolve_path(&string_arg(arguments, "path")?, &workspace_root);
            let question = string_arg_with_default(
                arguments,
                "question",
                "Transcribe the audio accurately and summarize anything important.",
            )?;
            let images = string_array_arg(arguments, "images")?
                .into_iter()
                .map(|path| resolve_path(&path, &workspace_root).display().to_string())
                .collect::<Vec<_>>();
            let result = run_interruptible_worker_job(
                &runtime_state_root,
                &ToolWorkerJob::Audio {
                    path: path.display().to_string(),
                    question,
                    upstream: upstream.clone(),
                    images,
                },
                upstream.timeout_seconds,
                cancel_flag.as_ref(),
            )?;
            Ok(result)
        },
    )
}

pub(super) fn image_generate_tool(
    workspace_root: PathBuf,
    runtime_state_root: PathBuf,
    upstream: UpstreamConfig,
    cancel_flag: Option<Arc<InterruptSignal>>,
) -> Tool {
    let mut properties = Map::new();
    properties.insert("prompt".to_string(), json!({"type": "string"}));
    maybe_add_images_schema(&mut properties, upstream.supports_vision_input);
    Tool::new_interruptible(
        "image_generate",
        "Generate a new image with a helper model. Returns a generated file path and attaches the image back into context. This can be interrupted and will cancel immediately.",
        Value::Object(Map::from_iter([
            ("type".to_string(), Value::String("object".to_string())),
            ("properties".to_string(), Value::Object(properties)),
            ("required".to_string(), json!(["prompt"])),
            ("additionalProperties".to_string(), Value::Bool(false)),
        ])),
        move |arguments| {
            let arguments = arguments
                .as_object()
                .ok_or_else(|| anyhow!("tool arguments must be an object"))?;
            let prompt = string_arg(arguments, "prompt")?;
            let images = string_array_arg(arguments, "images")?
                .into_iter()
                .map(|path| resolve_path(&path, &workspace_root).display().to_string())
                .collect::<Vec<_>>();
            let output_dir = workspace_root.join("generated");
            fs::create_dir_all(&output_dir)
                .with_context(|| format!("failed to create {}", output_dir.display()))?;
            let output_path = output_dir.join(format!("generated-{}.png", Uuid::new_v4()));
            let result = run_interruptible_worker_job(
                &runtime_state_root,
                &ToolWorkerJob::ImageGenerate {
                    prompt,
                    upstream: upstream.clone(),
                    output_path: output_path.display().to_string(),
                    images,
                },
                upstream.timeout_seconds,
                cancel_flag.as_ref(),
            )?;
            Ok(result)
        },
    )
}

pub(super) fn image_wait_tool(
    runtime_state_root: PathBuf,
    cancel_flag: Option<Arc<InterruptSignal>>,
) -> Tool {
    Tool::new_interruptible(
        "image_wait",
        "Wait for or observe a previously started image task by image_id.",
        json!({
            "type": "object",
            "properties": {
                "image_id": {"type": "string"},
                "wait_timeout_seconds": {"type": "number"},
                "on_timeout": {"type": "string", "enum": ["continue", "kill", "CONTINUE", "KILL"]}
            },
            "required": ["image_id"],
            "additionalProperties": false
        }),
        move |arguments| {
            let arguments = arguments
                .as_object()
                .ok_or_else(|| anyhow!("tool arguments must be an object"))?;
            let image_id = string_arg(arguments, "image_id")?;
            let wait_timeout_seconds = arguments
                .get("wait_timeout_seconds")
                .map(|_| f64_arg(arguments, "wait_timeout_seconds"))
                .transpose()?
                .unwrap_or(IMAGE_START_DEFAULT_WAIT_TIMEOUT_SECONDS);
            let on_timeout =
                image_timeout_action_arg(arguments, "on_timeout", ImageTimeoutAction::Continue)?;
            wait_for_image_task(
                &runtime_state_root,
                &image_id,
                wait_timeout_seconds,
                on_timeout,
                cancel_flag.as_ref(),
            )
        },
    )
}

pub(super) fn image_cancel_tool(
    runtime_state_root: PathBuf,
    _cancel_flag: Option<Arc<InterruptSignal>>,
) -> Tool {
    Tool::new(
        "image_cancel",
        "Cancel a previously started image task by image_id.",
        json!({
            "type": "object",
            "properties": {
                "image_id": {"type": "string"}
            },
            "required": ["image_id"],
            "additionalProperties": false
        }),
        move |arguments| {
            let arguments = arguments
                .as_object()
                .ok_or_else(|| anyhow!("tool arguments must be an object"))?;
            let image_id = string_arg(arguments, "image_id")?;
            cancel_image_task(&runtime_state_root, &image_id).map(compact_image_status_for_model)
        },
    )
}
