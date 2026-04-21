use super::InterruptSignal;
use super::download::{cleanup_file_downloads, list_active_file_download_summaries};
use super::dsl::{cleanup_dsl_tasks, list_active_dsl_summaries};
#[cfg(windows)]
use super::exec::record_exit_code;
use super::exec::{
    cleanup_exec_processes, list_active_exec_summaries, process_is_running, read_exit_code,
    terminate_process_pid,
};
use super::media::cleanup_image_tasks;
use crate::tool_worker::ToolWorkerJob;
use anyhow::{Context, Result, anyhow};
use serde::Serialize;
use serde_json::Value;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::thread;
use std::time::Duration;
use uuid::Uuid;

#[derive(Clone, Serialize, serde::Deserialize)]
pub(super) struct BackgroundTaskMetadata {
    pub(super) task_id: String,
    pub(super) pid: u32,
    pub(super) label: String,
    pub(super) status_path: String,
    pub(super) stdout_path: String,
    pub(super) stderr_path: String,
    pub(super) exit_code_path: String,
}

#[derive(Clone, Debug, Default)]
pub struct RuntimeTaskCleanupReport {
    pub exec_processes_killed: usize,
    pub file_downloads_cancelled: usize,
    pub image_tasks_cancelled: usize,
    pub dsl_tasks_killed: usize,
}

fn tool_worker_state_dir(runtime_state_root: &Path) -> Result<PathBuf> {
    let path = runtime_state_root.join("agent_frame").join("tool_workers");
    fs::create_dir_all(&path).with_context(|| format!("failed to create {}", path.display()))?;
    Ok(path)
}

pub(super) fn background_task_dir(runtime_state_root: &Path, kind: &str) -> Result<PathBuf> {
    let path = runtime_state_root.join("agent_frame").join(kind);
    fs::create_dir_all(&path).with_context(|| format!("failed to create {}", path.display()))?;
    Ok(path)
}

pub(super) fn background_task_dir_if_exists(
    runtime_state_root: &Path,
    kind: &str,
) -> Option<PathBuf> {
    let path = runtime_state_root.join("agent_frame").join(kind);
    path.exists().then_some(path)
}

fn background_task_meta_path(dir: &Path, task_id: &str) -> PathBuf {
    dir.join(format!("{}.json", task_id))
}

pub(super) fn write_background_task_metadata(
    dir: &Path,
    metadata: &BackgroundTaskMetadata,
) -> Result<()> {
    fs::write(
        background_task_meta_path(dir, &metadata.task_id),
        serde_json::to_vec_pretty(metadata).context("failed to serialize background task")?,
    )
    .with_context(|| format!("failed to write metadata for {}", metadata.task_id))
}

pub(super) fn read_background_task_metadata(
    dir: &Path,
    task_id: &str,
) -> Result<BackgroundTaskMetadata> {
    let path = background_task_meta_path(dir, task_id);
    let raw =
        fs::read_to_string(&path).with_context(|| format!("failed to read {}", path.display()))?;
    serde_json::from_str(&raw).context("failed to parse background task metadata")
}

pub(super) fn background_task_is_running(metadata: &BackgroundTaskMetadata) -> bool {
    read_exit_code(Path::new(&metadata.exit_code_path)).is_none()
        && process_is_running(metadata.pid)
}

fn resolve_tool_worker_executable() -> Result<PathBuf> {
    if let Ok(path) = std::env::var("AGENT_TOOL_WORKER_BIN") {
        let candidate = PathBuf::from(path);
        if candidate.exists() {
            return Ok(candidate);
        }
    }
    if let Ok(path) = std::env::var("CARGO_BIN_EXE_run_agent") {
        let candidate = PathBuf::from(path);
        if candidate.exists() {
            return Ok(candidate);
        }
    }
    if let Ok(path) = std::env::var("CARGO_BIN_EXE_partyclaw") {
        let candidate = PathBuf::from(path);
        if candidate.exists() {
            return Ok(candidate);
        }
    }
    let current = std::env::current_exe().context("failed to resolve current executable")?;
    if current
        .file_stem()
        .and_then(|value| value.to_str())
        .is_some_and(|name| matches!(name, "partyclaw" | "agent_host" | "run_agent"))
    {
        return Ok(current);
    }
    let mut candidates = Vec::new();
    if let Some(parent) = current.parent() {
        candidates.push(parent.join("run_agent"));
        candidates.push(parent.join("partyclaw"));
        candidates.push(parent.join("agent_host"));
        if parent.file_name().and_then(|value| value.to_str()) == Some("deps")
            && let Some(grandparent) = parent.parent()
        {
            candidates.push(grandparent.join("run_agent"));
            candidates.push(grandparent.join("partyclaw"));
            candidates.push(grandparent.join("agent_host"));
        }
    }
    candidates
        .into_iter()
        .find(|candidate| candidate.exists())
        .ok_or_else(|| {
            anyhow!("failed to locate tool worker executable; set AGENT_TOOL_WORKER_BIN")
        })
}

fn write_tool_worker_job_file(runtime_state_root: &Path, job: &ToolWorkerJob) -> Result<PathBuf> {
    let dir = tool_worker_state_dir(runtime_state_root)?;
    let path = dir.join(format!("job-{}.json", Uuid::new_v4()));
    fs::write(
        &path,
        serde_json::to_vec_pretty(job).context("failed to serialize tool worker job")?,
    )
    .with_context(|| format!("failed to write {}", path.display()))?;
    Ok(path)
}

pub(super) fn run_interruptible_worker_job(
    runtime_state_root: &Path,
    job: &ToolWorkerJob,
    timeout_seconds: f64,
    cancel_flag: Option<&Arc<InterruptSignal>>,
) -> Result<Value> {
    let job_file = write_tool_worker_job_file(runtime_state_root, job)?;
    let worker_executable = resolve_tool_worker_executable()?;
    let child = Command::new(worker_executable)
        .arg("run-tool-worker")
        .arg("--job-file")
        .arg(&job_file)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("failed to spawn tool worker")?;
    let pid = child.id();
    let (sender, receiver) = crossbeam_channel::bounded(1);
    thread::spawn(move || {
        let _ = sender.send(child.wait_with_output());
    });
    let cancel_receiver = cancel_flag.map(|signal| signal.subscribe());
    let timeout_receiver = crossbeam_channel::after(Duration::from_secs_f64(timeout_seconds));
    let output = match cancel_receiver {
        Some(cancel_receiver) => crossbeam_channel::select! {
            recv(receiver) -> result => Some(result),
            recv(cancel_receiver) -> _ => {
                terminate_process_pid(pid);
                None
            }
            recv(timeout_receiver) -> _ => {
                terminate_process_pid(pid);
                None
            }
        },
        None => crossbeam_channel::select! {
            recv(receiver) -> result => Some(result),
            recv(timeout_receiver) -> _ => {
                terminate_process_pid(pid);
                None
            }
        },
    };
    let _ = fs::remove_file(&job_file);
    let Some(output) = output else {
        let _ = receiver.recv_timeout(Duration::from_secs(5));
        if cancel_flag.is_some_and(|signal| signal.is_requested()) {
            return Err(anyhow!("operation cancelled"));
        }
        return Err(anyhow!(
            "operation timed out after {} seconds",
            timeout_seconds
        ));
    };
    let output = output
        .context("tool worker completion channel disconnected")?
        .context("failed to wait for tool worker process")?;
    if !output.status.success() {
        return Err(anyhow!(
            "tool worker failed with {}: {}",
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    serde_json::from_slice(&output.stdout).context("failed to parse tool worker output")
}

pub(super) fn spawn_background_worker_process(
    runtime_state_root: &Path,
    label: &str,
    task_id: &str,
    job: &ToolWorkerJob,
) -> Result<BackgroundTaskMetadata> {
    let worker_dir = tool_worker_state_dir(runtime_state_root)?;
    let job_file = worker_dir.join(format!("{}-{}.job.json", label, task_id));
    fs::write(
        &job_file,
        serde_json::to_vec_pretty(job).context("failed to serialize background worker job")?,
    )
    .with_context(|| format!("failed to write {}", job_file.display()))?;
    let stdout_path = worker_dir.join(format!("{}-{}.stdout", label, task_id));
    let stderr_path = worker_dir.join(format!("{}-{}.stderr", label, task_id));
    let exit_code_path = worker_dir.join(format!("{}-{}.exit", label, task_id));
    let stdout_file = fs::File::create(&stdout_path)
        .with_context(|| format!("failed to create {}", stdout_path.display()))?;
    let stderr_file = fs::File::create(&stderr_path)
        .with_context(|| format!("failed to create {}", stderr_path.display()))?;
    let worker_executable = resolve_tool_worker_executable()?;
    let child_pid = {
        #[cfg(windows)]
        {
            let mut child = Command::new(&worker_executable)
                .arg("run-tool-worker")
                .arg("--job-file")
                .arg(&job_file)
                .current_dir(runtime_state_root)
                .stdin(Stdio::null())
                .stdout(Stdio::from(stdout_file))
                .stderr(Stdio::from(stderr_file))
                .spawn()
                .context("failed to spawn background tool worker")?;
            let child_pid = child.id();
            let exit_code_path = exit_code_path.clone();
            thread::spawn(move || {
                let code = child
                    .wait()
                    .ok()
                    .and_then(|status| status.code())
                    .unwrap_or(-1);
                let _ = record_exit_code(&exit_code_path, code);
            });
            child_pid
        }
        #[cfg(not(windows))]
        {
            let child = Command::new("sh")
                .arg("-c")
                .arg("\"$@\"; code=$?; printf '%s' \"$code\" > \"$AGENT_FRAME_EXIT_CODE_PATH\"; exit $code")
                .arg("sh")
                .arg(&worker_executable)
                .arg("run-tool-worker")
                .arg("--job-file")
                .arg(&job_file)
                .current_dir(runtime_state_root)
                .env("AGENT_FRAME_EXIT_CODE_PATH", &exit_code_path)
                .stdin(Stdio::null())
                .stdout(Stdio::from(stdout_file))
                .stderr(Stdio::from(stderr_file))
                .spawn()
                .context("failed to spawn background tool worker")?;
            child.id()
        }
    };
    Ok(BackgroundTaskMetadata {
        task_id: task_id.to_string(),
        pid: child_pid,
        label: label.to_string(),
        status_path: match job {
            ToolWorkerJob::Image { status_path, .. } => status_path.clone(),
            ToolWorkerJob::FileDownload { status_path, .. } => status_path.clone(),
            ToolWorkerJob::Dsl { status_path, .. } => status_path.clone(),
            _ => String::new(),
        },
        stdout_path: stdout_path.display().to_string(),
        stderr_path: stderr_path.display().to_string(),
        exit_code_path: exit_code_path.display().to_string(),
    })
}

pub(super) fn read_status_json(path: &Path) -> Result<Value> {
    let mut last_error = None;
    for attempt in 0..10 {
        match fs::read_to_string(path) {
            Ok(raw) => match serde_json::from_str(&raw) {
                Ok(value) => return Ok(value),
                Err(error) => {
                    last_error = Some(format!("failed to parse status json: {error}"));
                }
            },
            Err(error) => {
                last_error = Some(format!("failed to read {}: {error}", path.display()));
            }
        }
        if attempt < 9 {
            thread::sleep(Duration::from_millis(20));
        }
    }
    Err(anyhow!(
        "{} after retries",
        last_error.unwrap_or_else(|| format!("failed to read {}", path.display()))
    ))
}

pub(crate) fn active_runtime_state_summary(runtime_state_root: &Path) -> Result<Option<String>> {
    let active_execs = list_active_exec_summaries(runtime_state_root)?;
    let active_downloads = list_active_file_download_summaries(runtime_state_root)?;
    let active_dsls = list_active_dsl_summaries(runtime_state_root)?;
    let active_subagents = list_active_subagent_summaries(runtime_state_root)?;
    if active_execs.is_empty()
        && active_downloads.is_empty()
        && active_dsls.is_empty()
        && active_subagents.is_empty()
    {
        return Ok(None);
    }
    let mut sections = vec![
        "[Active Runtime Tasks]".to_string(),
        "These tasks are still in progress across turns. Reuse their ids with observe/wait/cancel tools instead of starting duplicates.".to_string(),
    ];
    if !active_execs.is_empty() {
        sections.push("Active shell sessions:".to_string());
        sections.extend(active_execs);
    }
    if !active_downloads.is_empty() {
        sections.push("Active file downloads:".to_string());
        sections.extend(active_downloads);
    }
    if !active_dsls.is_empty() {
        sections.push("Active DSL jobs:".to_string());
        sections.extend(active_dsls);
    }
    if !active_subagents.is_empty() {
        sections.push("Active subagents:".to_string());
        sections.extend(active_subagents);
    }
    Ok(Some(sections.join("\n")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use tempfile::TempDir;

    #[test]
    fn read_status_json_retries_transient_partial_writes() {
        let temp_dir = TempDir::new().unwrap();
        let status_path = temp_dir.path().join("status.json");
        fs::write(&status_path, "{").unwrap();

        let writer_path = status_path.clone();
        let writer = thread::spawn(move || {
            thread::sleep(Duration::from_millis(10));
            fs::write(
                &writer_path,
                serde_json::to_vec(&json!({"ok": true})).unwrap(),
            )
            .unwrap();
        });

        let value = read_status_json(&status_path).unwrap();
        writer.join().unwrap();
        assert_eq!(value["ok"], json!(true));
    }
}

fn list_active_subagent_summaries(runtime_state_root: &Path) -> Result<Vec<String>> {
    let Some(dir) = background_task_dir_if_exists(runtime_state_root, "subagents") else {
        return Ok(Vec::new());
    };
    let mut entries = Vec::new();
    for path in iter_metadata_json_files(&dir)? {
        let raw = fs::read_to_string(&path)
            .with_context(|| format!("failed to read {}", path.display()))?;
        let value: Value =
            serde_json::from_str(&raw).context("failed to parse subagent state json")?;
        let state = value
            .get("state")
            .and_then(Value::as_str)
            .unwrap_or("unknown");
        if !matches!(state, "running" | "waiting_for_charge" | "ready") {
            continue;
        }
        let id = value
            .get("id")
            .or_else(|| value.get("agent_id"))
            .and_then(Value::as_str)
            .unwrap_or("unknown");
        let description = value
            .get("description")
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim();
        let model = value
            .get("model_key")
            .or_else(|| value.get("model"))
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim();
        let mut line = format!("- id={} state={}", id, state);
        if !description.is_empty() {
            line.push_str(&format!(" description={:?}", description));
        }
        if !model.is_empty() {
            line.push_str(&format!(" model={}", model));
        }
        entries.push(line);
    }
    entries.sort();
    Ok(entries)
}

pub(super) fn iter_metadata_json_files(dir: &Path) -> Result<Vec<PathBuf>> {
    let mut paths = Vec::new();
    for entry in fs::read_dir(dir).with_context(|| format!("failed to read {}", dir.display()))? {
        let entry = entry?;
        let path = entry.path();
        let Some(file_name) = path.file_name().and_then(|value| value.to_str()) else {
            continue;
        };
        if !file_name.ends_with(".json") || file_name.ends_with(".status.json") {
            continue;
        }
        paths.push(path);
    }
    paths.sort();
    Ok(paths)
}

pub fn terminate_runtime_state_tasks(
    runtime_state_root: &Path,
) -> Result<RuntimeTaskCleanupReport> {
    Ok(RuntimeTaskCleanupReport {
        exec_processes_killed: cleanup_exec_processes(runtime_state_root)?,
        file_downloads_cancelled: cleanup_file_downloads(runtime_state_root)?,
        image_tasks_cancelled: cleanup_image_tasks(runtime_state_root)?,
        dsl_tasks_killed: cleanup_dsl_tasks(runtime_state_root)?,
    })
}
