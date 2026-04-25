use std::{
    collections::HashMap,
    io::{BufRead, BufReader, Write},
    path::Path,
    process::{Child, ChildStdin, ChildStdout, Stdio},
    sync::{
        atomic::{AtomicU64, Ordering},
        mpsc, Arc, Mutex,
    },
    thread,
};

use serde::Deserialize;
use serde_json::{json, Value};
use stellaclaw_core::{
    model_config::ModelConfig,
    session_actor::{SessionEvent, SessionInitial, SessionRequest},
};

use crate::{config::SandboxConfig, sandbox::build_agent_server_command};

pub struct AgentServerClient {
    child: Child,
    stdin: Mutex<ChildStdin>,
    pending: Arc<Mutex<HashMap<u64, mpsc::Sender<Result<Value, String>>>>>,
    next_id: AtomicU64,
    reader_handle: Option<thread::JoinHandle<()>>,
}

impl AgentServerClient {
    pub fn spawn(
        binary_path: &Path,
        current_dir: &Path,
        session_root: &Path,
        sandbox: &SandboxConfig,
    ) -> Result<(Self, mpsc::Receiver<SessionEvent>), String> {
        let mut command =
            build_agent_server_command(sandbox, binary_path, current_dir, session_root)
                .map_err(|error| format!("failed to build agent_server command: {error:#}"))?;
        let mut child = command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .map_err(|error| format!("failed to spawn {}: {error}", binary_path.display()))?;
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| "agent_server stdin was not piped".to_string())?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| "agent_server stdout was not piped".to_string())?;
        let pending = Arc::new(Mutex::new(HashMap::new()));
        let (event_tx, event_rx) = mpsc::channel();
        let reader_handle = Some(spawn_reader_thread(stdout, pending.clone(), event_tx));

        Ok((
            Self {
                child,
                stdin: Mutex::new(stdin),
                pending,
                next_id: AtomicU64::new(1),
                reader_handle,
            },
            event_rx,
        ))
    }

    pub fn initialize(
        &self,
        model_config: &ModelConfig,
        initial: &SessionInitial,
    ) -> Result<(), String> {
        self.request(
            "initialize",
            json!({
                "model_config": model_config,
                "initial": initial,
            }),
        )
        .map(|_| ())
    }

    pub fn send_session_request(&self, request: &SessionRequest) -> Result<(), String> {
        self.request(
            "session_request",
            serde_json::to_value(request).map_err(|error| error.to_string())?,
        )
        .map(|_| ())
    }

    pub fn shutdown(mut self) -> Result<(), String> {
        let _ = self.request("shutdown", json!({}));
        let _ = self.child.wait();
        if let Some(handle) = self.reader_handle.take() {
            let _ = handle.join();
        }
        Ok(())
    }

    fn request(&self, method: &str, params: Value) -> Result<Value, String> {
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        let (response_tx, response_rx) = mpsc::channel();
        self.pending
            .lock()
            .map_err(|_| "pending request lock poisoned".to_string())?
            .insert(id, response_tx);

        let payload = json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params,
        });
        {
            let mut stdin = self
                .stdin
                .lock()
                .map_err(|_| "agent_server stdin lock poisoned".to_string())?;
            serde_json::to_writer(&mut *stdin, &payload)
                .map_err(|error| format!("failed to encode request: {error}"))?;
            stdin
                .write_all(b"\n")
                .map_err(|error| format!("failed to write request: {error}"))?;
            stdin
                .flush()
                .map_err(|error| format!("failed to flush request: {error}"))?;
        }

        response_rx
            .recv()
            .map_err(|_| "agent_server response stream closed".to_string())?
    }
}

fn spawn_reader_thread(
    stdout: ChildStdout,
    pending: Arc<Mutex<HashMap<u64, mpsc::Sender<Result<Value, String>>>>>,
    event_tx: mpsc::Sender<SessionEvent>,
) -> thread::JoinHandle<()> {
    thread::spawn(move || {
        let reader = BufReader::new(stdout);
        for line in reader.lines() {
            let Ok(line) = line else {
                break;
            };
            if line.trim().is_empty() {
                continue;
            }
            let message = match serde_json::from_str::<JsonRpcMessage>(&line) {
                Ok(message) => message,
                Err(_) => continue,
            };

            if let Some(id) = message.id {
                if let Some(sender) = pending.lock().ok().and_then(|mut map| map.remove(&id)) {
                    let result = match (message.result, message.error) {
                        (Some(result), None) => Ok(result),
                        (_, Some(error)) => Err(error.message),
                        _ => Err("agent_server returned an empty response".to_string()),
                    };
                    let _ = sender.send(result);
                }
                continue;
            }

            match message.method.as_deref() {
                Some("session_event") => {
                    if let Some(params) = message.params {
                        if let Ok(event) = serde_json::from_value::<SessionEvent>(params) {
                            let _ = event_tx.send(event);
                        }
                    }
                }
                Some("server_error") => {}
                _ => {}
            }
        }
    })
}

#[derive(Debug, Deserialize)]
struct JsonRpcMessage {
    #[serde(default)]
    id: Option<u64>,
    #[serde(default)]
    method: Option<String>,
    #[serde(default)]
    params: Option<Value>,
    #[serde(default)]
    result: Option<Value>,
    #[serde(default)]
    error: Option<JsonRpcError>,
}

#[derive(Debug, Deserialize)]
struct JsonRpcError {
    message: String,
}
