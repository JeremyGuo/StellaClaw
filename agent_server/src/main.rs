use std::{
    io::{self, BufRead, Write},
    sync::{mpsc, Arc, Mutex},
    thread,
};

use serde::Deserialize;
use serde_json::{json, Value};
use stellaclaw_core::{
    model_config::ModelConfig,
    providers::{init_global_provider_fork_server, ForkServerProvider},
    session_actor::{
        ConversationTransport, LocalToolBatchExecutor, SessionActor, SessionActorEventSink,
        SessionActorInbox, SessionActorStep, SessionErrorDetail, SessionEvent, SessionInitial,
        SessionRequest, SessionRpcThread, ToolCatalog,
    },
};

fn main() {
    if let Err(error) = run() {
        let mut stdout = io::stdout().lock();
        let _ = write_json_line(
            &mut stdout,
            &json!({
                "jsonrpc": "2.0",
                "method": "server_error",
                "params": { "error": error },
            }),
        );
    }
}

fn run() -> Result<(), String> {
    init_global_provider_fork_server().map_err(|error| error.to_string())?;

    let output = Arc::new(JsonRpcOutput::new());
    let mut runtime: Option<AgentRuntime> = None;

    for line in io::stdin().lock().lines() {
        let line = line.map_err(|error| format!("failed to read stdin: {error}"))?;
        if line.trim().is_empty() {
            continue;
        }

        let request: JsonRpcRequest = match serde_json::from_str(&line) {
            Ok(request) => request,
            Err(error) => {
                output.write_error(None, -32700, format!("parse error: {error}"))?;
                continue;
            }
        };

        match request.method.as_str() {
            "initialize" => {
                if runtime.is_some() {
                    output.write_error(
                        request.id,
                        -32002,
                        "agent server is already initialized".to_string(),
                    )?;
                    continue;
                }
                let Some(params) = request.params else {
                    output.write_error(request.id, -32602, "missing params".to_string())?;
                    continue;
                };
                match initialize(params, output.clone()) {
                    Ok(started) => {
                        runtime = Some(started);
                        output.write_result(request.id, json!({"initialized": true}))?;
                    }
                    Err(error) => output.write_error(request.id, -32000, error)?,
                }
            }
            "session_request" => {
                let Some(runtime) = runtime.as_ref() else {
                    output.write_error(
                        request.id,
                        -32001,
                        "server is not initialized".to_string(),
                    )?;
                    continue;
                };
                let Some(params) = request.params else {
                    output.write_error(request.id, -32602, "missing params".to_string())?;
                    continue;
                };
                match serde_json::from_value::<SessionRequest>(params) {
                    Ok(session_request) => {
                        runtime.send(session_request)?;
                        output.write_result(request.id, json!({"enqueued": true}))?;
                    }
                    Err(error) => {
                        output.write_error(
                            request.id,
                            -32602,
                            format!("invalid SessionRequest: {error}"),
                        )?;
                    }
                }
            }
            "shutdown" => {
                if let Some(runtime) = runtime.take() {
                    runtime.shutdown();
                }
                output.write_result(request.id, json!({"shutdown": true}))?;
                break;
            }
            _ => {
                output.write_error(
                    request.id,
                    -32601,
                    format!("unknown method {}", request.method),
                )?;
            }
        }
    }

    if let Some(runtime) = runtime.take() {
        runtime.shutdown();
    }

    Ok(())
}

fn initialize(params: Value, output: Arc<JsonRpcOutput>) -> Result<AgentRuntime, String> {
    let params: InitializeParams = serde_json::from_value(params)
        .map_err(|error| format!("invalid initialize params: {error}"))?;
    let workspace_root =
        std::env::current_dir().map_err(|error| format!("failed to resolve cwd: {error}"))?;

    let (inbox, sender) = SessionActorInbox::channel();
    let event_sink = Arc::new(StdoutEventSink {
        output: output.clone(),
    });
    let rpc_thread = SessionRpcThread::spawn(Arc::new(sender.clone()), event_sink.clone());
    let bridge = rpc_thread.conversation_bridge();
    let tool_executor = Arc::new(
        LocalToolBatchExecutor::new(workspace_root).with_conversation_bridge(Arc::new(bridge)),
    );
    let provider: Arc<dyn stellaclaw_core::providers::Provider + Send + Sync> = Arc::new(
        ForkServerProvider::global(params.model_config.clone())
            .map_err(|error| error.to_string())?,
    );
    let catalog = ToolCatalog::from_model_config_and_initial(&params.model_config, &params.initial)
        .map_err(|error| format!("failed to build tool catalog: {error}"))?;
    let mut actor = SessionActor::new(
        params.model_config,
        provider,
        tool_executor,
        inbox,
        event_sink,
        catalog,
    );
    rpc_thread
        .enqueue_from_conversation(SessionRequest::Initial {
            initial: params.initial,
        })
        .map_err(|error| format!("failed to enqueue initial request: {error}"))?;

    let (stop_tx, stop_rx) = mpsc::channel();
    let actor_output = output.clone();
    let actor_handle = thread::spawn(move || run_actor_loop(&mut actor, stop_rx, actor_output));

    Ok(AgentRuntime {
        rpc_thread,
        actor_handle: Some(actor_handle),
        stop_tx,
    })
}

fn run_actor_loop(
    actor: &mut SessionActor,
    stop_rx: mpsc::Receiver<()>,
    output: Arc<JsonRpcOutput>,
) {
    loop {
        if stop_rx.try_recv().is_ok() {
            break;
        }
        match actor.recv_step() {
            Ok(SessionActorStep::Shutdown) => break,
            Ok(_) => {}
            Err(error) => {
                let _ = output.write_notification(
                    "session_event",
                    SessionEvent::RuntimeCrashed {
                        error: error.to_string(),
                        error_detail: SessionErrorDetail::new(
                            "agent_server.session_actor",
                            "runtime_crashed",
                            error.to_string(),
                        ),
                    },
                );
                break;
            }
        }
    }
}

struct AgentRuntime {
    rpc_thread: SessionRpcThread,
    actor_handle: Option<thread::JoinHandle<()>>,
    stop_tx: mpsc::Sender<()>,
}

impl AgentRuntime {
    fn send(&self, request: SessionRequest) -> Result<(), String> {
        self.rpc_thread
            .enqueue_from_conversation(request)
            .map_err(|error| error.to_string())
    }

    fn shutdown(mut self) {
        let _ = self
            .rpc_thread
            .enqueue_from_conversation(SessionRequest::Shutdown);
        let _ = self.stop_tx.send(());
        if let Some(handle) = self.actor_handle.take() {
            let _ = handle.join();
        }
        let _ = self.rpc_thread.shutdown();
    }
}

#[derive(Debug, Deserialize)]
struct InitializeParams {
    model_config: ModelConfig,
    initial: SessionInitial,
}

#[derive(Debug, Deserialize)]
struct JsonRpcRequest {
    id: Option<Value>,
    method: String,
    params: Option<Value>,
}

struct StdoutEventSink {
    output: Arc<JsonRpcOutput>,
}

impl SessionActorEventSink for StdoutEventSink {
    fn emit(&self, event: SessionEvent) -> Result<(), String> {
        self.output.write_notification("session_event", event)
    }
}

impl ConversationTransport for StdoutEventSink {
    fn send_event(&self, event: SessionEvent) -> Result<(), String> {
        self.output.write_notification("session_event", event)
    }
}

struct JsonRpcOutput {
    stdout: Mutex<io::Stdout>,
}

impl JsonRpcOutput {
    fn new() -> Self {
        Self {
            stdout: Mutex::new(io::stdout()),
        }
    }

    fn write_result(&self, id: Option<Value>, result: Value) -> Result<(), String> {
        let Some(id) = id else {
            return Ok(());
        };
        self.write(json!({
            "jsonrpc": "2.0",
            "id": id,
            "result": result,
        }))
    }

    fn write_error(&self, id: Option<Value>, code: i64, message: String) -> Result<(), String> {
        self.write(json!({
            "jsonrpc": "2.0",
            "id": id,
            "error": {
                "code": code,
                "message": message,
            },
        }))
    }

    fn write_notification<T: serde::Serialize>(
        &self,
        method: &str,
        params: T,
    ) -> Result<(), String> {
        self.write(json!({
            "jsonrpc": "2.0",
            "method": method,
            "params": params,
        }))
    }

    fn write(&self, value: Value) -> Result<(), String> {
        let mut stdout = self
            .stdout
            .lock()
            .map_err(|_| "stdout lock poisoned".to_string())?;
        write_json_line(&mut *stdout, &value)
    }
}

fn write_json_line(writer: &mut impl Write, value: &Value) -> Result<(), String> {
    serde_json::to_writer(&mut *writer, value)
        .map_err(|error| format!("failed to serialize json: {error}"))?;
    writer
        .write_all(b"\n")
        .map_err(|error| format!("failed to write stdout: {error}"))?;
    writer
        .flush()
        .map_err(|error| format!("failed to flush stdout: {error}"))
}
