use super::args::{f64_arg, string_arg};
use super::exec::{process_is_running, read_exit_code, record_exit_code, terminate_process_pid};
use super::runtime_state::{
    BackgroundTaskMetadata, background_task_dir, background_task_dir_if_exists,
    background_task_is_running, iter_metadata_json_files, read_background_task_metadata,
    read_status_json, spawn_background_worker_process, write_background_task_metadata,
};
use super::{InterruptSignal, Tool, compact_tool_status_fields_for_model, resolve_path};
use crate::tool_worker::ToolWorkerJob;
use anyhow::{Context, Result, anyhow};
use serde_json::{Value, json};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};
use uuid::Uuid;

fn download_temp_path(path: &Path, download_id: &str) -> PathBuf {
    let file_name = path
        .file_name()
        .and_then(|value| value.to_str())
        .unwrap_or("download");
    path.with_file_name(format!(".{}.{}.part", file_name, download_id))
}

const DOWNLOAD_START_DEFAULT_WAIT_TIMEOUT_SECONDS: f64 = 270.0;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum DownloadTimeoutAction {
    Continue,
    Kill,
}

fn compact_download_status_for_model(mut value: Value) -> Value {
    compact_tool_status_fields_for_model(&mut value);
    value
}

impl DownloadTimeoutAction {
    fn as_str(self) -> &'static str {
        match self {
            Self::Continue => "continue",
            Self::Kill => "kill",
        }
    }
}

fn download_timeout_action_arg(
    arguments: &serde_json::Map<String, Value>,
    key: &str,
    default: DownloadTimeoutAction,
) -> Result<DownloadTimeoutAction> {
    let Some(value) = arguments.get(key) else {
        return Ok(default);
    };
    let text = value
        .as_str()
        .ok_or_else(|| anyhow!("argument {} must be a string", key))?
        .trim()
        .to_ascii_lowercase();
    match text.as_str() {
        "continue" => Ok(DownloadTimeoutAction::Continue),
        "kill" => Ok(DownloadTimeoutAction::Kill),
        _ => Err(anyhow!("argument {} must be one of: continue, kill", key)),
    }
}

pub(super) fn read_file_download_snapshot(
    runtime_state_root: &Path,
    download_id: &str,
) -> Result<Value> {
    let metadata = read_background_task_metadata(
        &background_task_dir(runtime_state_root, "file_downloads")?,
        download_id,
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
            "download_id": download_id,
            "url": snapshot["url"].clone(),
            "path": snapshot["path"].clone(),
            "running": false,
            "completed": false,
            "cancelled": false,
            "failed": true,
            "error": "file download worker exited unexpectedly",
        });
    }
    Ok(snapshot)
}

pub(super) fn list_active_file_download_summaries(
    runtime_state_root: &Path,
) -> Result<Vec<String>> {
    let Some(task_dir) = background_task_dir_if_exists(runtime_state_root, "file_downloads") else {
        return Ok(Vec::new());
    };
    let mut entries = Vec::new();
    for entry in
        fs::read_dir(&task_dir).with_context(|| format!("failed to read {}", task_dir.display()))?
    {
        let entry = entry?;
        let path = entry.path();
        let Some(file_name) = path.file_name().and_then(|value| value.to_str()) else {
            continue;
        };
        if !file_name.ends_with(".json") || file_name.ends_with(".status.json") {
            continue;
        }
        let Some(download_id) = path.file_stem().and_then(|value| value.to_str()) else {
            continue;
        };
        let snapshot = read_file_download_snapshot(runtime_state_root, download_id)?;
        if !snapshot
            .get("running")
            .and_then(Value::as_bool)
            .unwrap_or(false)
        {
            continue;
        }
        let url = snapshot
            .get("url")
            .and_then(Value::as_str)
            .unwrap_or("<unknown>");
        let target_path = snapshot
            .get("path")
            .and_then(Value::as_str)
            .unwrap_or("<unknown>");
        let bytes_downloaded = snapshot
            .get("bytes_downloaded")
            .and_then(Value::as_u64)
            .unwrap_or(0);
        entries.push(format!(
            "- download_id=`{}` path=`{}` bytes_downloaded={} url=`{}`",
            download_id, target_path, bytes_downloaded, url
        ));
    }
    entries.sort();
    Ok(entries)
}

pub(super) fn cleanup_file_downloads(runtime_state_root: &Path) -> Result<usize> {
    let Some(task_dir) = background_task_dir_if_exists(runtime_state_root, "file_downloads") else {
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
            "download_id": metadata.task_id,
            "url": previous.as_ref().and_then(|value| value.get("url")).cloned().unwrap_or(Value::String(String::new())),
            "path": previous.as_ref().and_then(|value| value.get("path")).cloned().unwrap_or(Value::String(String::new())),
            "running": false,
            "completed": false,
            "cancelled": true,
            "failed": false,
            "bytes_downloaded": previous.as_ref().and_then(|value| value.get("bytes_downloaded")).cloned().unwrap_or(Value::from(0_u64)),
            "total_bytes": previous.as_ref().and_then(|value| value.get("total_bytes")).cloned().unwrap_or(Value::Null),
            "http_status": previous.as_ref().and_then(|value| value.get("http_status")).cloned().unwrap_or(Value::Null),
            "final_url": previous.as_ref().and_then(|value| value.get("final_url")).cloned().unwrap_or(Value::Null),
            "content_type": previous.as_ref().and_then(|value| value.get("content_type")).cloned().unwrap_or(Value::Null),
            "reason": "session_destroyed",
        });
        fs::write(
            Path::new(&metadata.status_path),
            serde_json::to_vec_pretty(&snapshot)
                .context("failed to serialize file download cleanup snapshot")?,
        )
        .with_context(|| format!("failed to write {}", metadata.status_path))?;
        cancelled = cancelled.saturating_add(1);
    }
    Ok(cancelled)
}

fn cancel_file_download(runtime_state_root: &Path, download_id: &str) -> Result<Value> {
    let task_dir = background_task_dir(runtime_state_root, "file_downloads")?;
    let metadata = read_background_task_metadata(&task_dir, download_id)?;
    let previous = read_file_download_snapshot(runtime_state_root, download_id).ok();
    terminate_process_pid(metadata.pid);
    let _ = record_exit_code(Path::new(&metadata.exit_code_path), -9);
    let snapshot = json!({
        "download_id": download_id,
        "url": previous
            .as_ref()
            .and_then(|value| value.get("url").cloned())
            .unwrap_or(Value::String(String::new())),
        "path": previous
            .as_ref()
            .and_then(|value| value.get("path").cloned())
            .unwrap_or(Value::String(String::new())),
        "running": false,
        "completed": false,
        "cancelled": true,
        "failed": false,
    });
    fs::write(
        Path::new(&metadata.status_path),
        serde_json::to_vec_pretty(&snapshot)
            .context("failed to serialize file download cancel snapshot")?,
    )
    .with_context(|| format!("failed to write {}", metadata.status_path))?;
    Ok(snapshot)
}

fn wait_for_file_download(
    runtime_state_root: &Path,
    download_id: &str,
    wait_timeout_seconds: f64,
    on_timeout: DownloadTimeoutAction,
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
        let snapshot = read_file_download_snapshot(runtime_state_root, download_id)?;
        let finished = snapshot
            .get("running")
            .and_then(Value::as_bool)
            .is_some_and(|running| !running);
        if finished {
            return Ok(compact_download_status_for_model(snapshot));
        }
        if let Some(cancel_receiver) = &cancel_receiver
            && cancel_receiver.try_recv().is_ok()
        {
            return Ok(compact_download_status_for_model(json!({
                "interrupted": true,
                "download": snapshot,
            })));
        }
        if Instant::now() >= deadline {
            if on_timeout == DownloadTimeoutAction::Kill {
                let mut cancelled = cancel_file_download(runtime_state_root, download_id)?;
                if let Some(object) = cancelled.as_object_mut() {
                    object.insert("wait_timed_out".to_string(), Value::Bool(true));
                    object.insert(
                        "on_timeout".to_string(),
                        Value::String(on_timeout.as_str().to_string()),
                    );
                }
                return Ok(compact_download_status_for_model(cancelled));
            }
            let mut object = snapshot
                .as_object()
                .cloned()
                .ok_or_else(|| anyhow!("download snapshot must be a JSON object"))?;
            object.insert("wait_timed_out".to_string(), Value::Bool(true));
            object.insert(
                "on_timeout".to_string(),
                Value::String(on_timeout.as_str().to_string()),
            );
            object.insert("running".to_string(), Value::Bool(true));
            object.insert("completed".to_string(), Value::Bool(false));
            return Ok(compact_download_status_for_model(Value::Object(object)));
        }
        if let Some(cancel_receiver) = &cancel_receiver {
            crossbeam_channel::select! {
                recv(cancel_receiver) -> _ => {
                    return Ok(compact_download_status_for_model(json!({
                        "interrupted": true,
                        "download": snapshot,
                    })));
                }
                recv(crossbeam_channel::after(Duration::from_millis(200))) -> _ => {}
            }
        } else {
            thread::sleep(Duration::from_millis(200));
        }
    }
}

pub(super) fn file_download_start_tool(
    workspace_root: PathBuf,
    runtime_state_root: PathBuf,
    cancel_flag: Option<Arc<InterruptSignal>>,
) -> Tool {
    Tool::new_interruptible(
        "file_download_start",
        "Start downloading an HTTP resource to a local file.",
        json!({
            "type": "object",
            "properties": {
                "url": {"type": "string"},
                "path": {"type": "string"},
                "headers": {"type": "object"},
                "overwrite": {"type": "boolean"},
                "return_immediate": {"type": "boolean"},
                "wait_timeout_seconds": {"type": "number"},
                "on_timeout": {"type": "string", "enum": ["continue", "kill", "CONTINUE", "KILL"]}
            },
            "required": ["url", "path"],
            "additionalProperties": false
        }),
        move |arguments| {
            let arguments = arguments
                .as_object()
                .ok_or_else(|| anyhow!("tool arguments must be an object"))?;
            let url = string_arg(arguments, "url")?;
            let path = resolve_path(&string_arg(arguments, "path")?, &workspace_root);
            let overwrite = arguments
                .get("overwrite")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            let return_immediate = arguments
                .get("return_immediate")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            let wait_timeout_seconds = arguments
                .get("wait_timeout_seconds")
                .map(|_| f64_arg(arguments, "wait_timeout_seconds"))
                .transpose()?
                .unwrap_or(DOWNLOAD_START_DEFAULT_WAIT_TIMEOUT_SECONDS);
            let on_timeout = download_timeout_action_arg(
                arguments,
                "on_timeout",
                DownloadTimeoutAction::Continue,
            )?;
            let headers = arguments
                .get("headers")
                .and_then(Value::as_object)
                .cloned()
                .unwrap_or_default();
            if path.exists() && !overwrite {
                return Err(anyhow!(
                    "destination already exists and overwrite=false: {}",
                    path.display()
                ));
            }
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent).with_context(|| {
                    format!("failed to create parent directory {}", parent.display())
                })?;
            }
            let download_id = Uuid::new_v4().to_string();
            let temp_path = download_temp_path(&path, &download_id);
            let task_dir = background_task_dir(&runtime_state_root, "file_downloads")?;
            let status_path = task_dir.join(format!("{}.status.json", download_id));
            let initial = json!({
                "download_id": download_id,
                "url": url,
                "path": path.display().to_string(),
                "running": true,
                "completed": false,
                "cancelled": false,
                "failed": false,
                "bytes_downloaded": 0,
                "total_bytes": Value::Null,
                "http_status": Value::Null,
                "final_url": Value::Null,
                "content_type": Value::Null,
            });
            fs::write(
                &status_path,
                serde_json::to_vec_pretty(&initial)
                    .context("failed to serialize file download status")?,
            )
            .with_context(|| format!("failed to write {}", status_path.display()))?;
            let job = ToolWorkerJob::FileDownload {
                download_id: download_id.clone(),
                url: url.clone(),
                path: path.display().to_string(),
                temp_path: temp_path.display().to_string(),
                headers,
                status_path: status_path.display().to_string(),
            };
            let metadata = spawn_background_worker_process(
                &runtime_state_root,
                "file-download",
                &download_id,
                &job,
            )?;
            write_background_task_metadata(&task_dir, &metadata)?;
            if return_immediate {
                read_file_download_snapshot(&runtime_state_root, &download_id)
                    .map(compact_download_status_for_model)
            } else {
                wait_for_file_download(
                    &runtime_state_root,
                    &download_id,
                    wait_timeout_seconds,
                    on_timeout,
                    cancel_flag.as_ref(),
                )
            }
        },
    )
}

pub(super) fn file_download_progress_tool(
    runtime_state_root: PathBuf,
    _cancel_flag: Option<Arc<InterruptSignal>>,
) -> Tool {
    Tool::new(
        "file_download_progress",
        "Read the latest progress snapshot for a previously started download by download_id.",
        json!({
            "type": "object",
            "properties": {
                "download_id": {"type": "string"}
            },
            "required": ["download_id"],
            "additionalProperties": false
        }),
        move |arguments| {
            let arguments = arguments
                .as_object()
                .ok_or_else(|| anyhow!("tool arguments must be an object"))?;
            let download_id = string_arg(arguments, "download_id")?;
            read_file_download_snapshot(&runtime_state_root, &download_id)
                .map(compact_download_status_for_model)
        },
    )
}

pub(super) fn file_download_wait_tool(
    runtime_state_root: PathBuf,
    cancel_flag: Option<Arc<InterruptSignal>>,
) -> Tool {
    Tool::new_interruptible(
        "file_download_wait",
        "Wait for or observe a previously started download by download_id.",
        json!({
            "type": "object",
            "properties": {
                "download_id": {"type": "string"},
                "wait_timeout_seconds": {"type": "number"},
                "on_timeout": {"type": "string", "enum": ["continue", "kill", "CONTINUE", "KILL"]}
            },
            "required": ["download_id"],
            "additionalProperties": false
        }),
        move |arguments| {
            let arguments = arguments
                .as_object()
                .ok_or_else(|| anyhow!("tool arguments must be an object"))?;
            let download_id = string_arg(arguments, "download_id")?;
            let wait_timeout_seconds = arguments
                .get("wait_timeout_seconds")
                .map(|_| f64_arg(arguments, "wait_timeout_seconds"))
                .transpose()?
                .unwrap_or(DOWNLOAD_START_DEFAULT_WAIT_TIMEOUT_SECONDS);
            let on_timeout = download_timeout_action_arg(
                arguments,
                "on_timeout",
                DownloadTimeoutAction::Continue,
            )?;
            wait_for_file_download(
                &runtime_state_root,
                &download_id,
                wait_timeout_seconds,
                on_timeout,
                cancel_flag.as_ref(),
            )
        },
    )
}

pub(super) fn file_download_cancel_tool(
    runtime_state_root: PathBuf,
    _cancel_flag: Option<Arc<InterruptSignal>>,
) -> Tool {
    Tool::new(
        "file_download_cancel",
        "Cancel a previously started download by download_id.",
        json!({
            "type": "object",
            "properties": {
                "download_id": {"type": "string"}
            },
            "required": ["download_id"],
            "additionalProperties": false
        }),
        move |arguments| {
            let arguments = arguments
                .as_object()
                .ok_or_else(|| anyhow!("tool arguments must be an object"))?;
            let download_id = string_arg(arguments, "download_id")?;
            cancel_file_download(&runtime_state_root, &download_id)
                .map(compact_download_status_for_model)
        },
    )
}
