use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::path::PathBuf;
use std::process::{Child, ChildStderr, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;

use anyhow::{Context, Result, anyhow, bail};
use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};

use crate::zgent::detect::{
    zgent_root_dir, zgent_runtime_available, zgent_server_binary_candidates,
};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ZgentInstallation {
    pub root_dir: PathBuf,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ZgentServerEntry {
    Binary(PathBuf),
    CargoManifest(PathBuf),
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ZgentServerLaunchConfig {
    pub workspace_root: Option<PathBuf>,
    pub data_root: Option<PathBuf>,
    pub model: Option<String>,
    pub api_base: Option<String>,
    pub api_key: Option<String>,
    pub subagent_models_path: Option<PathBuf>,
    pub no_persist: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ZgentSessionCreateResult {
    pub session_id: String,
    pub created_at: String,
    pub workspace_path: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ZgentSessionSummary {
    pub session_id: String,
    pub profile: Option<String>,
    pub workspace_path: String,
    pub context_window_current: Option<u32>,
    pub context_window_size: Option<u32>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ZgentProfileSummary {
    pub name: String,
    pub version: String,
    pub description: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ZgentProfileDetails {
    pub name: String,
    pub version: String,
    pub description: Option<String>,
    pub process_command: Option<String>,
}

#[derive(Debug)]
pub struct ZgentRpcClient {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
    stderr: BufReader<ChildStderr>,
    next_request_id: u64,
}

impl ZgentInstallation {
    pub fn detect() -> Result<Self> {
        if !zgent_runtime_available() {
            return Err(anyhow!(
                "local ./zgent runtime directory is unavailable; unpack zgent into the repository root to enable the zgent backend"
            ));
        }
        Ok(Self {
            root_dir: zgent_root_dir(),
        })
    }

    pub fn native_server_binary(&self) -> Option<PathBuf> {
        for candidate in zgent_server_binary_candidates(&self.root_dir) {
            if candidate.is_file() {
                return Some(candidate);
            }
        }
        None
    }

    pub fn native_kernel_runtime_ready(&self) -> bool {
        self.native_server_binary().is_some()
    }

    pub fn ensure_native_server_binary(&self) -> Result<PathBuf> {
        self.native_server_binary().ok_or_else(|| {
            anyhow!(
                "no built zgent-server binary was found under {}; build ./zgent externally before enabling the zgent native kernel path",
                self.root_dir.display()
            )
        })
    }

    pub fn server_entry(&self) -> Result<ZgentServerEntry> {
        if let Some(binary) = self.native_server_binary() {
            return Ok(ZgentServerEntry::Binary(binary));
        }

        Err(anyhow!(
            "could not find a built zgent-server binary under {}; expected target/release/zgent-server or target/debug/zgent-server",
            self.root_dir.display()
        ))
    }

    pub fn build_server_command(&self, config: &ZgentServerLaunchConfig) -> Result<Command> {
        let entry = ZgentServerEntry::Binary(self.ensure_native_server_binary()?);
        let mut command = match entry {
            ZgentServerEntry::Binary(path) => {
                let cmd = Command::new(path);
                cmd
            }
            ZgentServerEntry::CargoManifest(_) => unreachable!("cargo-manifest launch is disabled"),
        };

        command.current_dir(&self.root_dir);
        command.stdin(Stdio::piped());
        command.stdout(Stdio::piped());
        command.stderr(Stdio::piped());

        if let Some(workspace_root) = &config.workspace_root {
            command.arg("--workspace").arg(workspace_root);
        }
        if let Some(data_root) = &config.data_root {
            command.arg("--root").arg(data_root);
        }
        if let Some(model) = &config.model {
            command.arg("--model").arg(model);
        }
        if let Some(api_base) = &config.api_base {
            command.arg("--api-base").arg(api_base);
        }
        if let Some(api_key) = &config.api_key {
            command.arg("--api-key").arg(api_key);
        }
        if let Some(subagent_models_path) = &config.subagent_models_path {
            command.arg("--subagent-models").arg(subagent_models_path);
        }
        if config.no_persist {
            command.arg("--no-persist");
        }

        Ok(command)
    }
}

impl ZgentRpcClient {
    pub fn spawn_stdio(
        installation: &ZgentInstallation,
        config: &ZgentServerLaunchConfig,
    ) -> Result<Self> {
        let mut command = installation.build_server_command(config)?;
        let mut child = command
            .spawn()
            .context("failed to spawn zgent-server stdio process")?;
        let stdin = child
            .stdin
            .take()
            .context("spawned zgent-server missing stdin pipe")?;
        let stdout = child
            .stdout
            .take()
            .context("spawned zgent-server missing stdout pipe")?;
        let stderr = child
            .stderr
            .take()
            .context("spawned zgent-server missing stderr pipe")?;
        Ok(Self {
            child,
            stdin,
            stdout: BufReader::new(stdout),
            stderr: BufReader::new(stderr),
            next_request_id: 1,
        })
    }

    pub fn request_value(&mut self, method: &str, params: Value) -> Result<Value> {
        let request_id = self.next_request_id;
        self.next_request_id += 1;

        let payload = json!({
            "jsonrpc": "2.0",
            "id": request_id,
            "method": method,
            "params": params,
        });
        write_framed_message(&mut self.stdin, &payload)?;

        loop {
            let message = match read_framed_message(&mut self.stdout) {
                Ok(message) => message,
                Err(error) => return Err(self.decorate_transport_error(error)),
            };
            let Some(object) = message.as_object() else {
                continue;
            };

            let same_id = object
                .get("id")
                .and_then(Value::as_u64)
                .map(|id| id == request_id)
                .unwrap_or(false);
            if !same_id {
                continue;
            }

            if let Some(error) = object.get("error") {
                let message = error
                    .get("message")
                    .and_then(Value::as_str)
                    .map(ToOwned::to_owned)
                    .unwrap_or_else(|| error.to_string());
                bail!("zgent RPC {method} failed: {message}");
            }

            return Ok(object.get("result").cloned().unwrap_or(Value::Null));
        }
    }

    pub fn request<P, R>(&mut self, method: &str, params: &P) -> Result<R>
    where
        P: Serialize,
        R: DeserializeOwned,
    {
        let value = serde_json::to_value(params)
            .with_context(|| format!("failed to serialize zgent RPC params for {method}"))?;
        let result = self.request_value(method, value)?;
        serde_json::from_value(result)
            .with_context(|| format!("failed to decode zgent RPC result for {method}"))
    }

    pub fn tool_call(&mut self, session_id: &str, name: &str, arguments: Value) -> Result<Value> {
        let result = self.request_value(
            "tool/call",
            json!({
                "session_id": session_id,
                "name": name,
                "arguments": arguments,
            }),
        )?;
        Ok(result.get("result").cloned().unwrap_or(result))
    }

    pub fn chat_send(&mut self, session_id: &str, message: &str, mode: &str) -> Result<Value> {
        self.request_value(
            "chat/send",
            json!({
                "session_id": session_id,
                "message": message,
                "mode": mode,
            }),
        )
    }

    pub fn chat_send_immediate(&mut self, session_id: &str, message: &str) -> Result<Value> {
        self.chat_send(session_id, message, "immediate")
    }

    pub fn chat_send_queue(&mut self, session_id: &str, message: &str) -> Result<Value> {
        self.chat_send(session_id, message, "queue")
    }

    pub fn chat_send_steer(&mut self, session_id: &str, message: &str) -> Result<Value> {
        self.chat_send(session_id, message, "steer")
    }

    pub fn session_create_minimal(
        &mut self,
        title: Option<&str>,
    ) -> Result<ZgentSessionCreateResult> {
        self.session_create(title, None)
    }

    pub fn session_create(
        &mut self,
        title: Option<&str>,
        profile: Option<&str>,
    ) -> Result<ZgentSessionCreateResult> {
        let result = self.request_value(
            "session/create",
            json!({
                "title": title,
                "profile": profile,
            }),
        )?;
        Ok(ZgentSessionCreateResult {
            session_id: result
                .get("session_id")
                .and_then(Value::as_str)
                .ok_or_else(|| anyhow!("zgent session/create result missing session_id"))?
                .to_string(),
            created_at: result
                .get("created_at")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
            workspace_path: result
                .get("workspace_path")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
        })
    }

    pub fn profile_list(&mut self) -> Result<Vec<ZgentProfileSummary>> {
        let result = self.request_value("profile/list", json!({}))?;
        let profiles = result
            .get("profiles")
            .and_then(Value::as_array)
            .ok_or_else(|| anyhow!("zgent profile/list result missing profiles array"))?;
        profiles
            .iter()
            .map(|profile| {
                Ok(ZgentProfileSummary {
                    name: profile
                        .get("name")
                        .and_then(Value::as_str)
                        .ok_or_else(|| anyhow!("zgent profile summary missing name"))?
                        .to_string(),
                    version: profile
                        .get("version")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string(),
                    description: profile
                        .get("description")
                        .and_then(Value::as_str)
                        .map(ToOwned::to_owned),
                })
            })
            .collect()
    }

    pub fn profile_get(&mut self, name: &str) -> Result<ZgentProfileDetails> {
        let result = self.request_value("profile/get", json!({ "name": name }))?;
        let process_command = result
            .get("process")
            .and_then(Value::as_object)
            .and_then(|process| process.get("command"))
            .and_then(Value::as_str)
            .map(ToOwned::to_owned);
        Ok(ZgentProfileDetails {
            name: result
                .get("name")
                .and_then(Value::as_str)
                .ok_or_else(|| anyhow!("zgent profile/get result missing name"))?
                .to_string(),
            version: result
                .get("version")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
            description: result
                .get("description")
                .and_then(Value::as_str)
                .map(ToOwned::to_owned),
            process_command,
        })
    }

    pub fn session_get(&mut self, session_id: &str) -> Result<ZgentSessionSummary> {
        let result = self.request_value("session/get", json!({ "session_id": session_id }))?;
        Ok(ZgentSessionSummary {
            session_id: result
                .get("id")
                .and_then(Value::as_str)
                .ok_or_else(|| anyhow!("zgent session/get result missing id"))?
                .to_string(),
            profile: result
                .get("profile")
                .and_then(Value::as_str)
                .map(ToOwned::to_owned),
            workspace_path: result
                .get("workspace_root")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
            context_window_current: result
                .get("context_window_current")
                .and_then(Value::as_u64)
                .and_then(|value| u32::try_from(value).ok()),
            context_window_size: result
                .get("context_window_size")
                .and_then(Value::as_u64)
                .and_then(|value| u32::try_from(value).ok()),
        })
    }

    fn decorate_transport_error(&mut self, error: anyhow::Error) -> anyhow::Error {
        let status = self.child.try_wait().ok().flatten();
        let mut stderr_text = String::new();
        let _ = self.stderr.read_to_string(&mut stderr_text);
        if status.is_none() && stderr_text.trim().is_empty() {
            return error.context("failed to read zgent JSON-RPC response");
        }

        let mut detail = String::from("failed to read zgent JSON-RPC response");
        if let Some(status) = status {
            detail.push_str(&format!("; zgent-server exited with status {status}"));
        }
        if !stderr_text.trim().is_empty() {
            detail.push_str(&format!("; stderr: {}", stderr_text.trim()));
        }
        error.context(detail)
    }
}

impl Drop for ZgentRpcClient {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

struct PendingRequest {
    method: String,
    sender: std::sync::mpsc::Sender<std::result::Result<Value, String>>,
}

struct ZgentSharedRpcClientInner {
    child: Mutex<Child>,
    stdin: Mutex<ChildStdin>,
    next_request_id: AtomicU64,
    pending: Mutex<HashMap<u64, PendingRequest>>,
    stderr_text: Arc<Mutex<String>>,
    transport_closed: AtomicBool,
}

#[derive(Clone)]
pub struct ZgentSharedRpcClient {
    inner: Arc<ZgentSharedRpcClientInner>,
}

impl ZgentSharedRpcClientInner {
    fn fail_pending(&self, detail: String) {
        let pending = {
            let mut pending = self.pending.lock().expect("pending lock poisoned");
            std::mem::take(&mut *pending)
        };
        for request in pending.into_values() {
            let _ = request.sender.send(Err(detail.clone()));
        }
    }

    fn transport_error_detail(&self, cause: &str) -> String {
        let status = self
            .child
            .lock()
            .ok()
            .and_then(|mut child| child.try_wait().ok().flatten());
        let stderr_text = self
            .stderr_text
            .lock()
            .map(|text| text.trim().to_string())
            .unwrap_or_default();
        if status.is_none() && stderr_text.is_empty() {
            return cause.to_string();
        }

        let mut detail = cause.to_string();
        if let Some(status) = status {
            detail.push_str(&format!("; zgent-server exited with status {status}"));
        }
        if !stderr_text.is_empty() {
            detail.push_str(&format!("; stderr: {stderr_text}"));
        }
        detail
    }
}

impl Drop for ZgentSharedRpcClientInner {
    fn drop(&mut self) {
        self.transport_closed.store(true, Ordering::SeqCst);
        self.fail_pending("zgent shared RPC client closed".to_string());
        if let Ok(mut child) = self.child.lock() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

impl ZgentSharedRpcClient {
    pub fn spawn_stdio(
        installation: &ZgentInstallation,
        config: &ZgentServerLaunchConfig,
    ) -> Result<Self> {
        let mut command = installation.build_server_command(config)?;
        let mut child = command
            .spawn()
            .context("failed to spawn zgent-server stdio process")?;
        let stdin = child
            .stdin
            .take()
            .context("spawned zgent-server missing stdin pipe")?;
        let stdout = child
            .stdout
            .take()
            .context("spawned zgent-server missing stdout pipe")?;
        let stderr = child
            .stderr
            .take()
            .context("spawned zgent-server missing stderr pipe")?;

        let stderr_text = Arc::new(Mutex::new(String::new()));
        let inner = Arc::new(ZgentSharedRpcClientInner {
            child: Mutex::new(child),
            stdin: Mutex::new(stdin),
            next_request_id: AtomicU64::new(1),
            pending: Mutex::new(HashMap::new()),
            stderr_text: Arc::clone(&stderr_text),
            transport_closed: AtomicBool::new(false),
        });

        {
            let stderr_text = Arc::clone(&stderr_text);
            thread::spawn(move || {
                let mut reader = BufReader::new(stderr);
                let mut captured = String::new();
                let _ = reader.read_to_string(&mut captured);
                if let Ok(mut sink) = stderr_text.lock() {
                    sink.push_str(&captured);
                }
            });
        }

        {
            let inner = Arc::clone(&inner);
            thread::spawn(move || {
                let mut reader = BufReader::new(stdout);
                loop {
                    match read_framed_message(&mut reader) {
                        Ok(message) => {
                            let Some(object) = message.as_object() else {
                                continue;
                            };
                            let Some(request_id) = object.get("id").and_then(Value::as_u64) else {
                                continue;
                            };
                            let pending = {
                                let mut pending =
                                    inner.pending.lock().expect("pending lock poisoned");
                                pending.remove(&request_id)
                            };
                            let Some(pending) = pending else {
                                continue;
                            };
                            if let Some(error) = object.get("error") {
                                let message = error
                                    .get("message")
                                    .and_then(Value::as_str)
                                    .map(ToOwned::to_owned)
                                    .unwrap_or_else(|| error.to_string());
                                let _ = pending.sender.send(Err(format!(
                                    "zgent RPC {} failed: {}",
                                    pending.method, message
                                )));
                                continue;
                            }
                            let _ = pending
                                .sender
                                .send(Ok(object.get("result").cloned().unwrap_or(Value::Null)));
                        }
                        Err(error) => {
                            if inner.transport_closed.load(Ordering::SeqCst) {
                                break;
                            }
                            let detail = inner.transport_error_detail(&format!(
                                "failed to read zgent JSON-RPC response: {error:#}"
                            ));
                            inner.transport_closed.store(true, Ordering::SeqCst);
                            inner.fail_pending(detail);
                            break;
                        }
                    }
                }
            });
        }

        Ok(Self { inner })
    }

    pub fn request_value(&self, method: &str, params: Value) -> Result<Value> {
        let request_id = self.inner.next_request_id.fetch_add(1, Ordering::SeqCst);
        let (sender, receiver) = std::sync::mpsc::channel();
        {
            let mut pending = self.inner.pending.lock().expect("pending lock poisoned");
            pending.insert(
                request_id,
                PendingRequest {
                    method: method.to_string(),
                    sender,
                },
            );
        }

        let payload = json!({
            "jsonrpc": "2.0",
            "id": request_id,
            "method": method,
            "params": params,
        });
        if let Err(error) = self
            .inner
            .stdin
            .lock()
            .map_err(|_| anyhow!("stdin lock poisoned"))
            .and_then(|mut stdin| write_framed_message(&mut stdin, &payload))
        {
            let detail = self.inner.transport_error_detail(&format!(
                "failed to write zgent JSON-RPC request {}: {error:#}",
                method
            ));
            let mut pending = self.inner.pending.lock().expect("pending lock poisoned");
            pending.remove(&request_id);
            return Err(anyhow!(detail));
        }

        match receiver.recv() {
            Ok(Ok(result)) => Ok(result),
            Ok(Err(error)) => Err(anyhow!(error)),
            Err(_) => Err(anyhow!(self.inner.transport_error_detail(&format!(
                "zgent RPC response channel closed for {method}"
            )))),
        }
    }

    pub fn profile_list(&self) -> Result<Vec<ZgentProfileSummary>> {
        let result = self.request_value("profile/list", json!({}))?;
        let profiles = result
            .get("profiles")
            .and_then(Value::as_array)
            .ok_or_else(|| anyhow!("zgent profile/list result missing profiles array"))?;
        profiles
            .iter()
            .map(|profile| {
                Ok(ZgentProfileSummary {
                    name: profile
                        .get("name")
                        .and_then(Value::as_str)
                        .ok_or_else(|| anyhow!("zgent profile summary missing name"))?
                        .to_string(),
                    version: profile
                        .get("version")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string(),
                    description: profile
                        .get("description")
                        .and_then(Value::as_str)
                        .map(ToOwned::to_owned),
                })
            })
            .collect()
    }

    pub fn profile_get(&self, name: &str) -> Result<ZgentProfileDetails> {
        let result = self.request_value("profile/get", json!({ "name": name }))?;
        let process_command = result
            .get("process")
            .and_then(Value::as_object)
            .and_then(|process| process.get("command"))
            .and_then(Value::as_str)
            .map(ToOwned::to_owned);
        Ok(ZgentProfileDetails {
            name: result
                .get("name")
                .and_then(Value::as_str)
                .ok_or_else(|| anyhow!("zgent profile/get result missing name"))?
                .to_string(),
            version: result
                .get("version")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
            description: result
                .get("description")
                .and_then(Value::as_str)
                .map(ToOwned::to_owned),
            process_command,
        })
    }

    pub fn session_create(
        &self,
        title: Option<&str>,
        profile: Option<&str>,
    ) -> Result<ZgentSessionCreateResult> {
        let result = self.request_value(
            "session/create",
            json!({
                "title": title,
                "profile": profile,
            }),
        )?;
        Ok(ZgentSessionCreateResult {
            session_id: result
                .get("session_id")
                .and_then(Value::as_str)
                .ok_or_else(|| anyhow!("zgent session/create result missing session_id"))?
                .to_string(),
            created_at: result
                .get("created_at")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
            workspace_path: result
                .get("workspace_path")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
        })
    }

    pub fn session_get(&self, session_id: &str) -> Result<ZgentSessionSummary> {
        let result = self.request_value("session/get", json!({ "session_id": session_id }))?;
        Ok(ZgentSessionSummary {
            session_id: result
                .get("id")
                .and_then(Value::as_str)
                .ok_or_else(|| anyhow!("zgent session/get result missing id"))?
                .to_string(),
            profile: result
                .get("profile")
                .and_then(Value::as_str)
                .map(ToOwned::to_owned),
            workspace_path: result
                .get("workspace_root")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
            context_window_current: result
                .get("context_window_current")
                .and_then(Value::as_u64)
                .and_then(|value| u32::try_from(value).ok()),
            context_window_size: result
                .get("context_window_size")
                .and_then(Value::as_u64)
                .and_then(|value| u32::try_from(value).ok()),
        })
    }

    pub fn chat_send(&self, session_id: &str, message: &str, mode: &str) -> Result<Value> {
        self.request_value(
            "chat/send",
            json!({
                "session_id": session_id,
                "message": message,
                "mode": mode,
            }),
        )
    }

    pub fn chat_send_immediate(&self, session_id: &str, message: &str) -> Result<Value> {
        self.chat_send(session_id, message, "immediate")
    }

    pub fn chat_send_queue(&self, session_id: &str, message: &str) -> Result<Value> {
        self.chat_send(session_id, message, "queue")
    }

    pub fn chat_send_steer(&self, session_id: &str, message: &str) -> Result<Value> {
        self.chat_send(session_id, message, "steer")
    }
}

fn write_framed_message(writer: &mut ChildStdin, value: &Value) -> Result<()> {
    let body =
        serde_json::to_vec(value).context("failed to serialize zgent JSON-RPC request body")?;
    let header = format!("Content-Length: {}\r\n\r\n", body.len());
    writer
        .write_all(header.as_bytes())
        .context("failed to write zgent JSON-RPC header")?;
    writer
        .write_all(&body)
        .context("failed to write zgent JSON-RPC body")?;
    writer
        .flush()
        .context("failed to flush zgent JSON-RPC body")
}

fn read_framed_message(reader: &mut BufReader<ChildStdout>) -> Result<Value> {
    let content_length = read_content_length(reader)?;
    let mut body = vec![0_u8; content_length];
    reader
        .read_exact(&mut body)
        .context("failed to read zgent JSON-RPC body")?;
    serde_json::from_slice(&body).context("failed to parse zgent JSON-RPC message body")
}

fn read_content_length(reader: &mut BufReader<ChildStdout>) -> Result<usize> {
    let mut line = String::new();
    let mut content_length = None;
    loop {
        line.clear();
        let bytes = reader
            .read_line(&mut line)
            .context("failed to read zgent JSON-RPC header line")?;
        if bytes == 0 {
            return Err(anyhow!(
                "unexpected EOF while reading zgent JSON-RPC headers"
            ));
        }
        if line == "\r\n" {
            break;
        }
        if let Some(value) = line.strip_prefix("Content-Length: ") {
            let trimmed = value.trim();
            content_length = Some(
                trimmed
                    .parse::<usize>()
                    .with_context(|| format!("invalid zgent Content-Length header: {trimmed}"))?,
            );
        }
    }
    content_length.context("missing Content-Length header in zgent JSON-RPC message")
}

#[cfg(test)]
mod tests {
    use super::{ZgentInstallation, ZgentServerEntry};
    use std::fs;
    use std::path::PathBuf;
    use tempfile::TempDir;

    #[test]
    fn detect_prefers_manifest_when_source_tree_exists() {
        let installation = ZgentInstallation {
            root_dir: PathBuf::from("/tmp/example-zgent"),
        };
        let entry = installation.server_entry();
        assert!(entry.is_err());
    }

    #[test]
    fn native_server_binary_prefers_release_then_debug() {
        let temp_dir = TempDir::new().unwrap();
        let root_dir = temp_dir.path().to_path_buf();
        let installation = ZgentInstallation {
            root_dir: root_dir.clone(),
        };

        fs::create_dir_all(root_dir.join("target/debug")).unwrap();
        fs::write(root_dir.join("target/debug/zgent-server"), b"debug").unwrap();
        assert_eq!(
            installation.native_server_binary(),
            Some(root_dir.join("target/debug/zgent-server"))
        );
        assert!(installation.native_kernel_runtime_ready());

        fs::create_dir_all(root_dir.join("target/release")).unwrap();
        fs::write(root_dir.join("target/release/zgent-server"), b"release").unwrap();
        assert_eq!(
            installation.native_server_binary(),
            Some(root_dir.join("target/release/zgent-server"))
        );
    }

    #[test]
    fn server_entry_requires_built_binary() {
        let temp_dir = TempDir::new().unwrap();
        let root_dir = temp_dir.path().to_path_buf();
        fs::create_dir_all(root_dir.join("target/debug")).unwrap();
        fs::write(root_dir.join("target/debug/zgent-server"), b"debug").unwrap();
        fs::write(
            root_dir.join("Cargo.toml"),
            b"[package]\nname=\"zgent\"\nversion=\"0.1.0\"\n",
        )
        .unwrap();

        let installation = ZgentInstallation { root_dir };
        let entry = installation.server_entry().unwrap();
        assert_eq!(
            entry,
            ZgentServerEntry::Binary(installation.root_dir.join("target/debug/zgent-server"))
        );
    }
}
