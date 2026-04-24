use std::{
    collections::HashMap,
    fs,
    io::{Read, Write},
    path::PathBuf,
    process::{Command, Stdio},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex, OnceLock,
    },
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use reqwest::header::{HeaderMap, HeaderName, HeaderValue};
use serde_json::{json, Map, Value};

use super::{
    schema::{add_remote_property, object_schema, properties},
    ToolBackend, ToolDefinition, ToolExecutionMode, ToolRemoteMode,
};
use crate::session_actor::tool_runtime::{
    bool_arg_with_default, f64_arg_with_default, resolve_local_path, shell_quote, string_arg,
    string_arg_with_default, truncate_tool_text, ExecutionTarget, LocalToolError,
    ToolCancellationToken, ToolExecutionContext,
};

static DOWNLOADS: OnceLock<Mutex<HashMap<String, DownloadTask>>> = OnceLock::new();

struct DownloadTask {
    status: Arc<Mutex<Value>>,
    cancel: Arc<AtomicBool>,
}

enum DownloadTarget {
    Local {
        path: PathBuf,
    },
    RemoteSsh {
        host: String,
        cwd: Option<String>,
        path: String,
    },
}

impl DownloadTarget {
    fn remote_label(&self) -> Option<&str> {
        match self {
            Self::Local { .. } => None,
            Self::RemoteSsh { host, .. } => Some(host.as_str()),
        }
    }
}

fn downloads() -> &'static Mutex<HashMap<String, DownloadTask>> {
    DOWNLOADS.get_or_init(|| Mutex::new(HashMap::new()))
}

pub fn download_tool_definitions(remote_mode: &ToolRemoteMode) -> Vec<ToolDefinition> {
    let mut start_properties = properties([
        ("url", json!({"type": "string"})),
        ("path", json!({"type": "string"})),
        ("headers", json!({"type": "object"})),
        ("overwrite", json!({"type": "boolean"})),
        ("return_immediate", json!({"type": "boolean"})),
        ("wait_timeout_seconds", json!({"type": "number"})),
        (
            "on_timeout",
            json!({"type": "string", "enum": ["continue", "kill", "CONTINUE", "KILL"]}),
        ),
    ]);
    add_remote_property(&mut start_properties, remote_mode);

    vec![
        ToolDefinition::new(
            "file_download_start",
            "Start downloading an HTTP resource to a file. In selectable remote mode, pass remote to download using that SSH host's network directly.",
            object_schema(start_properties, &["url", "path"]),
            ToolExecutionMode::Interruptible,
            ToolBackend::Local,
        ),
        ToolDefinition::new(
            "file_download_progress",
            "Read the latest progress snapshot for a previously started download by download_id.",
            object_schema(
                properties([("download_id", json!({"type": "string"}))]),
                &["download_id"],
            ),
            ToolExecutionMode::Immediate,
            ToolBackend::Local,
        ),
        ToolDefinition::new(
            "file_download_wait",
            "Wait for or observe a previously started download by download_id.",
            object_schema(
                properties([
                    ("download_id", json!({"type": "string"})),
                    ("wait_timeout_seconds", json!({"type": "number"})),
                    (
                        "on_timeout",
                        json!({"type": "string", "enum": ["continue", "kill", "CONTINUE", "KILL"]}),
                    ),
                ]),
                &["download_id"],
            ),
            ToolExecutionMode::Interruptible,
            ToolBackend::Local,
        ),
        ToolDefinition::new(
            "file_download_cancel",
            "Cancel a previously started download by download_id.",
            object_schema(
                properties([("download_id", json!({"type": "string"}))]),
                &["download_id"],
            ),
            ToolExecutionMode::Immediate,
            ToolBackend::Local,
        ),
    ]
}

pub(crate) fn execute_download_tool(
    tool_name: &str,
    arguments: &Map<String, Value>,
    context: &ToolExecutionContext<'_>,
) -> Result<Option<Value>, LocalToolError> {
    let result = match tool_name {
        "file_download_start" => file_download_start(arguments, context)?,
        "file_download_progress" => file_download_progress(arguments)?,
        "file_download_wait" => file_download_wait(arguments, context)?,
        "file_download_cancel" => file_download_cancel(arguments)?,
        _ => return Ok(None),
    };
    Ok(Some(result))
}

fn file_download_start(
    arguments: &Map<String, Value>,
    context: &ToolExecutionContext<'_>,
) -> Result<Value, LocalToolError> {
    let url = string_arg(arguments, "url")?;
    let path_arg = string_arg(arguments, "path")?;
    let overwrite = bool_arg_with_default(arguments, "overwrite", false)?;
    let download_id = format!("dl_{}", nonce());
    let target = context.execution_target(arguments)?;
    let display_path = match &target {
        ExecutionTarget::Local => resolve_local_path(context.workspace_root, &path_arg)
            .display()
            .to_string(),
        ExecutionTarget::RemoteSsh { host, cwd } => {
            remote_display_path(host, cwd.as_deref(), &path_arg)
        }
    };
    let download_target = match target {
        ExecutionTarget::Local => {
            let path = resolve_local_path(context.workspace_root, &path_arg);
            if path.exists() && !overwrite {
                return Err(LocalToolError::InvalidArguments(format!(
                    "{} already exists; pass overwrite=true to replace it",
                    path.display()
                )));
            }
            DownloadTarget::Local { path }
        }
        ExecutionTarget::RemoteSsh { host, cwd } => DownloadTarget::RemoteSsh {
            host,
            cwd,
            path: path_arg,
        },
    };
    let status = Arc::new(Mutex::new(json!({
        "download_id": download_id,
        "url": url,
        "path": display_path,
        "remote": download_target.remote_label(),
        "running": true,
        "bytes_downloaded": 0_u64,
    })));
    let cancel = Arc::new(AtomicBool::new(false));
    downloads().lock().expect("mutex poisoned").insert(
        download_id.clone(),
        DownloadTask {
            status: status.clone(),
            cancel: cancel.clone(),
        },
    );

    let headers = request_headers(arguments.get("headers"))?;
    thread::spawn(move || match download_target {
        DownloadTarget::Local { path } => {
            run_download(url, path, headers, overwrite, status, cancel)
        }
        DownloadTarget::RemoteSsh { host, cwd, path } => {
            run_remote_download(url, path, headers, overwrite, host, cwd, status, cancel)
        }
    });

    let wait_timeout = f64_arg_with_default(arguments, "wait_timeout_seconds", 0.0)?;
    if bool_arg_with_default(arguments, "return_immediate", false)? || wait_timeout <= 0.0 {
        return file_download_progress_by_id(&download_id);
    }
    wait_for_download(
        &download_id,
        wait_timeout,
        timeout_action(arguments)?,
        &context.cancel_token,
    )
}

fn file_download_progress(arguments: &Map<String, Value>) -> Result<Value, LocalToolError> {
    file_download_progress_by_id(&string_arg(arguments, "download_id")?)
}

fn file_download_wait(
    arguments: &Map<String, Value>,
    context: &ToolExecutionContext<'_>,
) -> Result<Value, LocalToolError> {
    let download_id = string_arg(arguments, "download_id")?;
    let wait_timeout = f64_arg_with_default(arguments, "wait_timeout_seconds", 30.0)?;
    wait_for_download(
        &download_id,
        wait_timeout,
        timeout_action(arguments)?,
        &context.cancel_token,
    )
}

fn file_download_cancel(arguments: &Map<String, Value>) -> Result<Value, LocalToolError> {
    let download_id = string_arg(arguments, "download_id")?;
    let task = downloads()
        .lock()
        .expect("mutex poisoned")
        .get(&download_id)
        .map(|task| (task.status.clone(), task.cancel.clone()))
        .ok_or_else(|| {
            LocalToolError::InvalidArguments(format!("unknown download_id {download_id}"))
        })?;
    task.1.store(true, Ordering::SeqCst);
    let mut status = task.0.lock().expect("mutex poisoned");
    if status
        .get("running")
        .and_then(Value::as_bool)
        .unwrap_or(false)
    {
        set_status_fields(
            &mut status,
            json!({"running": false, "cancelled": true, "completed": false}),
        );
    }
    Ok(compact_download_snapshot(status.clone()))
}

fn wait_for_download(
    download_id: &str,
    wait_timeout_seconds: f64,
    on_timeout: TimeoutAction,
    cancel_token: &ToolCancellationToken,
) -> Result<Value, LocalToolError> {
    if !wait_timeout_seconds.is_finite() || wait_timeout_seconds < 0.0 {
        return Err(LocalToolError::InvalidArguments(
            "wait_timeout_seconds must be a finite non-negative number".to_string(),
        ));
    }
    let deadline = Instant::now() + Duration::from_secs_f64(wait_timeout_seconds);
    loop {
        let snapshot = file_download_progress_by_id(download_id)?;
        if !snapshot
            .get("running")
            .and_then(Value::as_bool)
            .unwrap_or(false)
        {
            return Ok(snapshot);
        }
        if Instant::now() >= deadline {
            if matches!(on_timeout, TimeoutAction::Kill) {
                return file_download_cancel(&Map::from_iter([(
                    "download_id".to_string(),
                    Value::String(download_id.to_string()),
                )]));
            }
            return Ok(json!({"timeout": true, "download": snapshot}));
        }
        if cancel_token.is_cancelled() {
            return Ok(json!({"interrupted": true, "download": snapshot}));
        }
        thread::sleep(Duration::from_millis(50));
    }
}

fn file_download_progress_by_id(download_id: &str) -> Result<Value, LocalToolError> {
    let status = downloads()
        .lock()
        .expect("mutex poisoned")
        .get(download_id)
        .map(|task| task.status.clone())
        .ok_or_else(|| {
            LocalToolError::InvalidArguments(format!("unknown download_id {download_id}"))
        })?;
    let snapshot = status.lock().expect("mutex poisoned").clone();
    Ok(compact_download_snapshot(snapshot))
}

fn run_download(
    url: String,
    path: PathBuf,
    headers: HeaderMap,
    overwrite: bool,
    status: Arc<Mutex<Value>>,
    cancel: Arc<AtomicBool>,
) {
    let result = (|| -> Result<(), String> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).map_err(|error| error.to_string())?;
        }
        if path.exists() && !overwrite {
            return Err(format!(
                "{} already exists; pass overwrite=true to replace it",
                path.display()
            ));
        }
        let temp_path = path.with_extension("stellaclaw-download-part");
        let client = reqwest::blocking::Client::builder()
            .build()
            .map_err(|error| error.to_string())?;
        let mut response = client
            .get(&url)
            .headers(headers)
            .send()
            .map_err(|error| error.to_string())?;
        let total_bytes = response.content_length();
        {
            let mut value = status.lock().expect("mutex poisoned");
            set_status_fields(
                &mut value,
                json!({
                    "http_status": response.status().as_u16(),
                    "final_url": response.url().to_string(),
                    "content_type": response.headers().get(reqwest::header::CONTENT_TYPE).and_then(|value| value.to_str().ok()),
                    "total_bytes": total_bytes,
                }),
            );
        }
        if !response.status().is_success() {
            return Err(format!("download HTTP status {}", response.status()));
        }
        let mut file = fs::File::create(&temp_path).map_err(|error| error.to_string())?;
        let mut buffer = [0_u8; 16 * 1024];
        let mut bytes_downloaded = 0_u64;
        loop {
            if cancel.load(Ordering::SeqCst) {
                let _ = fs::remove_file(&temp_path);
                return Err("cancelled".to_string());
            }
            let read = response
                .read(&mut buffer)
                .map_err(|error| error.to_string())?;
            if read == 0 {
                break;
            }
            file.write_all(&buffer[..read])
                .map_err(|error| error.to_string())?;
            bytes_downloaded = bytes_downloaded.saturating_add(read as u64);
            let mut value = status.lock().expect("mutex poisoned");
            set_status_fields(&mut value, json!({"bytes_downloaded": bytes_downloaded}));
        }
        fs::rename(&temp_path, &path).map_err(|error| error.to_string())?;
        Ok(())
    })();

    let mut value = status.lock().expect("mutex poisoned");
    match result {
        Ok(()) => set_status_fields(&mut value, json!({"running": false, "completed": true})),
        Err(error) if error == "cancelled" => {
            set_status_fields(&mut value, json!({"running": false, "cancelled": true}))
        }
        Err(error) => {
            let (error, _) = truncate_tool_text(&error, 1000);
            set_status_fields(
                &mut value,
                json!({"running": false, "failed": true, "error": error}),
            )
        }
    }
}

fn run_remote_download(
    url: String,
    path: String,
    headers: HeaderMap,
    overwrite: bool,
    host: String,
    cwd: Option<String>,
    status: Arc<Mutex<Value>>,
    cancel: Arc<AtomicBool>,
) {
    let result = (|| -> Result<(), String> {
        let payload = remote_download_payload(&url, &path, &headers, overwrite)?;
        let command = remote_download_command(cwd.as_deref(), &payload);
        let mut child = Command::new("ssh")
            .arg("-o")
            .arg("BatchMode=yes")
            .arg("-o")
            .arg("ConnectTimeout=10")
            .arg("-T")
            .arg(&host)
            .arg(command)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|error| format!("failed to spawn ssh: {error}"))?;

        loop {
            if cancel.load(Ordering::SeqCst) {
                let _ = child.kill();
                let _ = child.wait();
                return Err("cancelled".to_string());
            }
            match child.try_wait().map_err(|error| error.to_string())? {
                Some(_) => break,
                None => thread::sleep(Duration::from_millis(100)),
            }
        }

        let output = child
            .wait_with_output()
            .map_err(|error| format!("failed to wait for ssh: {error}"))?;
        if !output.status.success() {
            return Err(format!(
                "ssh exited with {}; stderr: {}",
                output.status.code().unwrap_or(-1),
                String::from_utf8_lossy(&output.stderr)
            ));
        }
        let value: Value = serde_json::from_slice(&output.stdout).map_err(|error| {
            format!(
                "remote download output was not JSON: {error}; stdout: {}",
                String::from_utf8_lossy(&output.stdout)
            )
        })?;
        let mut status_value = status.lock().expect("mutex poisoned");
        set_status_fields(&mut status_value, value);
        Ok(())
    })();

    let mut value = status.lock().expect("mutex poisoned");
    match result {
        Ok(()) => set_status_fields(&mut value, json!({"running": false, "completed": true})),
        Err(error) if error == "cancelled" => {
            set_status_fields(&mut value, json!({"running": false, "cancelled": true}))
        }
        Err(error) => {
            let (error, _) = truncate_tool_text(&error, 1000);
            set_status_fields(
                &mut value,
                json!({"running": false, "failed": true, "error": error}),
            )
        }
    }
}

fn remote_download_payload(
    url: &str,
    path: &str,
    headers: &HeaderMap,
    overwrite: bool,
) -> Result<String, String> {
    let headers = headers
        .iter()
        .map(|(name, value)| {
            let value = value
                .to_str()
                .map_err(|error| format!("invalid header value for remote download: {error}"))?;
            Ok(json!([name.as_str(), value]))
        })
        .collect::<Result<Vec<_>, String>>()?;
    serde_json::to_string(&json!({
        "url": url,
        "path": path,
        "headers": headers,
        "overwrite": overwrite,
    }))
    .map_err(|error| error.to_string())
}

fn remote_download_command(cwd: Option<&str>, payload: &str) -> String {
    let script = r#"
import json, os, sys, tempfile, urllib.request

payload = json.loads(sys.argv[1])
path = os.path.expanduser(payload["path"])
parent = os.path.dirname(path)
if parent:
    os.makedirs(parent, exist_ok=True)
if os.path.exists(path) and not payload.get("overwrite", False):
    raise SystemExit(f"{path} already exists; pass overwrite=true to replace it")

request = urllib.request.Request(payload["url"])
for name, value in payload.get("headers", []):
    request.add_header(name, value)

with urllib.request.urlopen(request) as response:
    fd, temp_path = tempfile.mkstemp(prefix=".stellaclaw-download-", dir=parent or ".")
    bytes_downloaded = 0
    try:
        with os.fdopen(fd, "wb") as output:
            while True:
                chunk = response.read(1024 * 64)
                if not chunk:
                    break
                output.write(chunk)
                bytes_downloaded += len(chunk)
        os.replace(temp_path, path)
    except Exception:
        try:
            os.unlink(temp_path)
        except OSError:
            pass
        raise

    print(json.dumps({
        "http_status": response.getcode(),
        "final_url": response.geturl(),
        "content_type": response.headers.get("content-type"),
        "total_bytes": int(response.headers.get("content-length") or 0) or None,
        "bytes_downloaded": bytes_downloaded,
    }))
"#;
    let command = format!(
        "python3 -c {} {}",
        shell_quote(script),
        shell_quote(payload)
    );
    match cwd.map(str::trim).filter(|cwd| !cwd.is_empty()) {
        Some(cwd) => format!("cd {} && {}", shell_quote(cwd), command),
        None => command,
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

fn request_headers(value: Option<&Value>) -> Result<HeaderMap, LocalToolError> {
    let mut headers = HeaderMap::new();
    let Some(Value::Object(object)) = value else {
        return Ok(headers);
    };
    for (key, value) in object {
        let value = value.as_str().ok_or_else(|| {
            LocalToolError::InvalidArguments(format!("header {key} must be a string"))
        })?;
        headers.insert(
            HeaderName::from_bytes(key.as_bytes()).map_err(|error| {
                LocalToolError::InvalidArguments(format!("invalid header name {key}: {error}"))
            })?,
            HeaderValue::from_str(value).map_err(|error| {
                LocalToolError::InvalidArguments(format!("invalid header value: {error}"))
            })?,
        );
    }
    Ok(headers)
}

fn remote_display_path(host: &str, cwd: Option<&str>, path: &str) -> String {
    match cwd.map(str::trim).filter(|cwd| !cwd.is_empty()) {
        Some(cwd) => format!("{host}:{cwd}/{path}"),
        None => format!("{host}:{path}"),
    }
}

fn set_status_fields(status: &mut Value, patch: Value) {
    let Some(status) = status.as_object_mut() else {
        return;
    };
    if let Value::Object(patch) = patch {
        for (key, value) in patch {
            status.insert(key, value);
        }
    }
}

fn compact_download_snapshot(snapshot: Value) -> Value {
    let Some(object) = snapshot.as_object() else {
        return snapshot;
    };

    let mut compact = Map::new();
    for required in ["download_id", "url", "path"] {
        if let Some(value) = object.get(required) {
            compact.insert(required.to_string(), value.clone());
        }
    }
    for truthy in ["running", "completed", "cancelled", "failed"] {
        if object.get(truthy).and_then(Value::as_bool).unwrap_or(false) {
            compact.insert(truthy.to_string(), Value::Bool(true));
        }
    }
    for numeric in ["bytes_downloaded", "total_bytes", "http_status"] {
        if let Some(value) = object.get(numeric) {
            if !value.is_null() {
                compact.insert(numeric.to_string(), value.clone());
            }
        }
    }
    for text in ["final_url", "content_type", "error", "remote"] {
        if let Some(value) = object.get(text).and_then(Value::as_str) {
            if !value.is_empty() {
                compact.insert(text.to_string(), Value::String(value.to_string()));
            }
        }
    }

    Value::Object(compact)
}

fn nonce() -> String {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos().to_string())
        .unwrap_or_else(|_| "0".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wait_returns_snapshot_when_cancel_token_is_cancelled() {
        let download_id = format!("dl_test_{}", nonce());
        let status = Arc::new(Mutex::new(json!({
            "download_id": download_id,
            "url": "https://example.invalid/file",
            "path": "/tmp/file",
            "running": true,
            "bytes_downloaded": 42_u64,
        })));
        downloads().lock().expect("mutex poisoned").insert(
            download_id.clone(),
            DownloadTask {
                status,
                cancel: Arc::new(AtomicBool::new(false)),
            },
        );
        let cancel_token = ToolCancellationToken::default();
        cancel_token.cancel();

        let result = wait_for_download(&download_id, 30.0, TimeoutAction::Continue, &cancel_token)
            .expect("wait should return snapshot");

        assert_eq!(result["interrupted"], true);
        assert_eq!(result["download"]["running"], true);
        assert_eq!(result["download"]["bytes_downloaded"], 42);
        downloads()
            .lock()
            .expect("mutex poisoned")
            .remove(&download_id);
    }
}
