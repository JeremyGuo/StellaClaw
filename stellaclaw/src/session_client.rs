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
    time::Duration,
};

use serde::Deserialize;
use serde_json::{json, Value};
use stellaclaw_core::{
    model_config::ModelConfig,
    session_actor::{SessionEvent, SessionInitial, SessionRequest},
};

use crate::{config::SandboxConfig, sandbox::build_agent_server_command};

const AGENT_SERVER_REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

pub struct AgentServerClient {
    child: Child,
    write_tx: crossbeam_channel::Sender<AgentServerWriteCommand>,
    pending: Arc<Mutex<HashMap<u64, mpsc::Sender<Result<Value, String>>>>>,
    next_id: AtomicU64,
    reader_handle: Option<thread::JoinHandle<()>>,
    writer_handle: Option<thread::JoinHandle<()>>,
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
        let (write_tx, write_rx) = crossbeam_channel::unbounded();
        let writer_handle = Some(spawn_writer_thread(stdin, pending.clone(), write_rx));
        let reader_handle = Some(spawn_reader_thread(stdout, pending.clone(), event_tx));

        Ok((
            Self {
                child,
                write_tx,
                pending,
                next_id: AtomicU64::new(1),
                reader_handle,
                writer_handle,
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
        self.notify(
            "session_request",
            serde_json::to_value(request).map_err(|error| error.to_string())?,
        )
    }

    pub fn shutdown(mut self) -> Result<(), String> {
        let _ = self.request("shutdown", json!({}));
        let _ = self.child.wait();
        let _ = self.write_tx.send(AgentServerWriteCommand::Shutdown);
        if let Some(handle) = self.writer_handle.take() {
            let _ = handle.join();
        }
        if let Some(handle) = self.reader_handle.take() {
            let _ = handle.join();
        }
        Ok(())
    }

    fn notify(&self, method: &str, params: Value) -> Result<(), String> {
        let payload = json!({
            "jsonrpc": "2.0",
            "method": method,
            "params": params,
        });
        self.write_tx
            .send(AgentServerWriteCommand::Write {
                id: None,
                payload,
                response_tx: None,
            })
            .map_err(|_| "agent_server writer stopped".to_string())
    }

    fn request(&self, method: &str, params: Value) -> Result<Value, String> {
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        let (response_tx, response_rx) = mpsc::channel();
        self.pending
            .lock()
            .map_err(|_| "pending request lock poisoned".to_string())?
            .insert(id, response_tx.clone());
        let payload = json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params,
        });
        self.write_tx
            .send(AgentServerWriteCommand::Write {
                id: Some(id),
                payload,
                response_tx: Some(response_tx),
            })
            .map_err(|_| {
                if let Ok(mut pending) = self.pending.lock() {
                    pending.remove(&id);
                }
                "agent_server writer stopped".to_string()
            })?;

        match response_rx.recv_timeout(AGENT_SERVER_REQUEST_TIMEOUT) {
            Ok(result) => result,
            Err(mpsc::RecvTimeoutError::Timeout) => {
                if let Ok(mut pending) = self.pending.lock() {
                    pending.remove(&id);
                }
                Err(format!(
                    "agent_server request `{method}` timed out after {}s",
                    AGENT_SERVER_REQUEST_TIMEOUT.as_secs()
                ))
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                Err("agent_server response stream closed".to_string())
            }
        }
    }
}

enum AgentServerWriteCommand {
    Write {
        id: Option<u64>,
        payload: Value,
        response_tx: Option<mpsc::Sender<Result<Value, String>>>,
    },
    Shutdown,
}

fn spawn_writer_thread(
    mut stdin: ChildStdin,
    pending: Arc<Mutex<HashMap<u64, mpsc::Sender<Result<Value, String>>>>>,
    command_rx: crossbeam_channel::Receiver<AgentServerWriteCommand>,
) -> thread::JoinHandle<()> {
    thread::spawn(move || loop {
        crossbeam_channel::select! {
            recv(command_rx) -> command => {
                let Ok(command) = command else {
                    break;
                };
                match command {
                    AgentServerWriteCommand::Write {
                        id,
                        payload,
                        response_tx,
                    } => {
                        if let Err(error) = write_json_line(&mut stdin, &payload) {
                            if let Some(id) = id {
                                if let Ok(mut pending) = pending.lock() {
                                    pending.remove(&id);
                                }
                            }
                            if let Some(response_tx) = response_tx {
                                let _ = response_tx.send(Err(error));
                            }
                        }
                    }
                    AgentServerWriteCommand::Shutdown => break,
                }
            }
        }
    })
}

fn write_json_line(writer: &mut impl Write, value: &Value) -> Result<(), String> {
    serde_json::to_writer(&mut *writer, value)
        .map_err(|error| format!("failed to encode request: {error}"))?;
    writer
        .write_all(b"\n")
        .map_err(|error| format!("failed to write request: {error}"))?;
    writer
        .flush()
        .map_err(|error| format!("failed to flush request: {error}"))
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
