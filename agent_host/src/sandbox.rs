use crate::backend::{AgentBackendKind, run_session_with_report_controlled as run_backend_session};
use crate::child_rpc::{
    ChildInitPayload, ChildToParentMessage, ParentToChildMessage, RemoteToolDefinition,
};
use crate::config::{SandboxConfig, SandboxMode};
use agent_frame::{ExecutionSignal, SessionExecutionControl, SessionRunReport, Tool};
use anyhow::{Context, Result, anyhow};
use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::Value;
use std::collections::HashMap;
use std::io::{BufRead, BufReader, BufWriter, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, mpsc};

fn write_json_line<W: Write, T: Serialize>(writer: &mut W, value: &T) -> Result<()> {
    serde_json::to_writer(&mut *writer, value).context("failed to serialize RPC message")?;
    writer
        .write_all(b"\n")
        .context("failed to write RPC newline")?;
    writer.flush().context("failed to flush RPC message")?;
    Ok(())
}

fn read_json_line<T: DeserializeOwned>(reader: &mut impl BufRead) -> Result<Option<T>> {
    let mut line = String::new();
    let bytes = reader
        .read_line(&mut line)
        .context("failed to read RPC line")?;
    if bytes == 0 {
        return Ok(None);
    }
    let parsed = serde_json::from_str(line.trim_end()).context("failed to parse RPC message")?;
    Ok(Some(parsed))
}

fn next_request_id() -> String {
    static COUNTER: AtomicU64 = AtomicU64::new(1);
    format!("tool-req-{}", COUNTER.fetch_add(1, Ordering::Relaxed))
}

fn build_proxy_tool(
    definition: &RemoteToolDefinition,
    writer: Arc<Mutex<BufWriter<std::io::Stdout>>>,
    pending: Arc<Mutex<HashMap<String, mpsc::Sender<Result<Value, String>>>>>,
) -> Tool {
    let tool_name = definition.name.clone();
    let request_name = definition.name.clone();
    let request_parameters = definition.parameters.clone();
    let request_description = definition.description.clone();
    Tool::new(
        tool_name,
        request_description,
        request_parameters,
        move |arguments| {
            let request_id = next_request_id();
            let (sender, receiver) = mpsc::channel();
            pending
                .lock()
                .map_err(|_| anyhow!("RPC pending map poisoned"))?
                .insert(request_id.clone(), sender);
            {
                let mut writer = writer.lock().map_err(|_| anyhow!("RPC writer poisoned"))?;
                write_json_line(
                    &mut *writer,
                    &ChildToParentMessage::ToolRequest {
                        request_id: request_id.clone(),
                        tool_name: request_name.clone(),
                        arguments,
                    },
                )?;
            }
            match receiver.recv() {
                Ok(Ok(result)) => Ok(result),
                Ok(Err(error)) => Err(anyhow!(error)),
                Err(_) => Err(anyhow!("tool response channel closed unexpectedly")),
            }
        },
    )
}

pub fn run_child_stdio() -> Result<()> {
    let stdin = std::io::stdin();
    let stdout = std::io::stdout();
    let mut reader = BufReader::new(stdin);
    let writer = Arc::new(Mutex::new(BufWriter::new(stdout)));
    let init = match read_json_line::<ParentToChildMessage>(&mut reader)? {
        Some(ParentToChildMessage::Init(payload)) => payload,
        Some(other) => {
            return Err(anyhow!(
                "expected initial init RPC message, received {:?}",
                other
            ));
        }
        None => return Err(anyhow!("stdin closed before init message")),
    };

    {
        let mut writer = writer.lock().map_err(|_| anyhow!("RPC writer poisoned"))?;
        write_json_line(&mut *writer, &ChildToParentMessage::Started)?;
    }

    let control = SessionExecutionControl::with_checkpoint_callback({
        let writer = Arc::clone(&writer);
        move |report| {
            if let Ok(mut writer) = writer.lock() {
                let _ = write_json_line(&mut *writer, &ChildToParentMessage::Checkpoint(report));
            }
        }
    })
    .with_event_callback({
        let writer = Arc::clone(&writer);
        move |event| {
            if let Ok(mut writer) = writer.lock() {
                let _ = write_json_line(&mut *writer, &ChildToParentMessage::SessionEvent(event));
            }
        }
    });

    let pending: Arc<Mutex<HashMap<String, mpsc::Sender<Result<Value, String>>>>> =
        Arc::new(Mutex::new(HashMap::new()));
    let mut extra_tools = Vec::with_capacity(init.extra_tools.len());
    for definition in &init.extra_tools {
        extra_tools.push(build_proxy_tool(
            definition,
            Arc::clone(&writer),
            Arc::clone(&pending),
        ));
    }

    let signal_control = control.clone();
    let pending_map = Arc::clone(&pending);
    let _inbound_thread = std::thread::spawn(move || -> Result<()> {
        while let Some(message) = read_json_line::<ParentToChildMessage>(&mut reader)? {
            match message {
                ParentToChildMessage::ToolResponse {
                    request_id,
                    ok,
                    result,
                    error,
                } => {
                    if let Some(sender) = pending_map
                        .lock()
                        .map_err(|_| anyhow!("RPC pending map poisoned"))?
                        .remove(&request_id)
                    {
                        let _ = sender.send(if ok {
                            Ok(result.unwrap_or(Value::Null))
                        } else {
                            Err(error.unwrap_or_else(|| "tool request failed".to_string()))
                        });
                    }
                }
                ParentToChildMessage::SoftTimeout => {
                    signal_control.request_timeout_observation();
                }
                ParentToChildMessage::Cancel => {
                    signal_control.request_cancel();
                    break;
                }
                ParentToChildMessage::Init(_) => {
                    return Err(anyhow!("received duplicate init message"));
                }
            }
        }
        Ok(())
    });

    let result = run_backend_session(
        init.backend,
        init.previous_messages,
        init.prompt,
        init.config,
        extra_tools,
        Some(control),
    );

    {
        let mut writer = writer.lock().map_err(|_| anyhow!("RPC writer poisoned"))?;
        match result {
            Ok(report) => write_json_line(&mut *writer, &ChildToParentMessage::Completed(report))?,
            Err(error) => write_json_line(
                &mut *writer,
                &ChildToParentMessage::Failed {
                    error: format!("{error:#}"),
                },
            )?,
        }
    }

    let _ = agent_frame::tooling::terminate_all_managed_processes();
    Ok(())
}

pub fn run_turn_in_child_process(
    sandbox: &SandboxConfig,
    backend: AgentBackendKind,
    previous_messages: Vec<agent_frame::ChatMessage>,
    prompt: String,
    config: agent_frame::config::AgentConfig,
    skill_memory_source: PathBuf,
    skills_source_root: PathBuf,
    extra_tools: Vec<Tool>,
    control: Option<SessionExecutionControl>,
) -> Result<SessionRunReport> {
    let current_exe = std::env::current_exe().context("failed to resolve current executable")?;
    let mut command = match sandbox.mode {
        SandboxMode::Disabled => Command::new(&current_exe),
        SandboxMode::Subprocess => Command::new(&current_exe),
        SandboxMode::Bubblewrap => build_bubblewrap_command(
            sandbox,
            &current_exe,
            &config.workspace_root,
            &config.runtime_state_root,
            &config.workspace_root.join(".skill_memory"),
            &skill_memory_source,
            &skills_source_root,
            &config.skills_dirs,
        )?,
    };
    command.arg("run-child");
    command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = command
        .spawn()
        .context("failed to spawn child agent runner")?;

    let child_stdin = child.stdin.take().context("child stdin unavailable")?;
    let child_stdout = child.stdout.take().context("child stdout unavailable")?;
    let mut child_stderr = Some(child.stderr.take().context("child stderr unavailable")?);
    let writer = Arc::new(Mutex::new(BufWriter::new(child_stdin)));
    {
        let init = ParentToChildMessage::Init(ChildInitPayload {
            backend,
            previous_messages,
            prompt,
            config,
            extra_tools: extra_tools
                .iter()
                .map(|tool| RemoteToolDefinition {
                    name: tool.name.clone(),
                    description: tool.description.clone(),
                    parameters: tool.parameters.clone(),
                })
                .collect(),
        });
        let mut writer_guard = writer.lock().map_err(|_| anyhow!("child stdin poisoned"))?;
        write_json_line(&mut *writer_guard, &init)?;
    }

    if let Some(control) = &control {
        let signal_receiver = control.signal_receiver();
        let writer = Arc::clone(&writer);
        std::thread::spawn(move || {
            while let Ok(signal) = signal_receiver.recv() {
                let message = match signal {
                    ExecutionSignal::Cancel => ParentToChildMessage::Cancel,
                    ExecutionSignal::TimeoutObservation => ParentToChildMessage::SoftTimeout,
                };
                if let Ok(mut writer) = writer.lock() {
                    let _ = write_json_line(&mut *writer, &message);
                }
                if matches!(signal, ExecutionSignal::Cancel) {
                    break;
                }
            }
        });
    }

    let mut reader = BufReader::new(child_stdout);
    loop {
        let message = match read_json_line::<ChildToParentMessage>(&mut reader) {
            Ok(Some(message)) => message,
            Ok(None) => {
                let mut stderr_reader = BufReader::new(
                    child_stderr
                        .take()
                        .ok_or_else(|| anyhow!("child stderr already consumed"))?,
                );
                let mut stderr_output = String::new();
                let _ = std::io::Read::read_to_string(&mut stderr_reader, &mut stderr_output);
                let status = child.wait().ok();
                let stderr_output = stderr_output.trim();
                return Err(anyhow!(
                    "child RPC stream closed unexpectedly; exit_status={}; stderr={}",
                    status
                        .map(|status| status.to_string())
                        .unwrap_or_else(|| "<unknown>".to_string()),
                    if stderr_output.is_empty() {
                        "<empty>"
                    } else {
                        stderr_output
                    }
                ));
            }
            Err(error) => {
                let mut stderr_reader = BufReader::new(
                    child_stderr
                        .take()
                        .ok_or_else(|| anyhow!("child stderr already consumed"))?,
                );
                let mut stderr_output = String::new();
                let _ = std::io::Read::read_to_string(&mut stderr_reader, &mut stderr_output);
                let status = child.wait().ok();
                let stderr_output = stderr_output.trim();
                return Err(anyhow!(
                    "failed to decode child RPC stream: {error:#}; exit_status={}; stderr={}",
                    status
                        .map(|status| status.to_string())
                        .unwrap_or_else(|| "<unknown>".to_string()),
                    if stderr_output.is_empty() {
                        "<empty>"
                    } else {
                        stderr_output
                    }
                ));
            }
        };
        match message {
            ChildToParentMessage::Started => {}
            ChildToParentMessage::SessionEvent(event) => {
                if let Some(control) = &control {
                    control.emit_event_external(event);
                }
            }
            ChildToParentMessage::Checkpoint(report) => {
                if let Some(control) = &control {
                    control.emit_checkpoint_report(report);
                }
            }
            ChildToParentMessage::ToolRequest {
                request_id,
                tool_name,
                arguments,
            } => {
                let response = match extra_tools.iter().find(|tool| tool.name == tool_name) {
                    Some(tool) => match tool.invoke(arguments) {
                        Ok(result) => ParentToChildMessage::ToolResponse {
                            request_id,
                            ok: true,
                            result: Some(result),
                            error: None,
                        },
                        Err(error) => ParentToChildMessage::ToolResponse {
                            request_id,
                            ok: false,
                            result: None,
                            error: Some(format!("{error:#}")),
                        },
                    },
                    None => ParentToChildMessage::ToolResponse {
                        request_id,
                        ok: false,
                        result: None,
                        error: Some(format!("unknown host tool: {tool_name}")),
                    },
                };
                let mut writer_guard =
                    writer.lock().map_err(|_| anyhow!("child stdin poisoned"))?;
                write_json_line(&mut *writer_guard, &response)?;
            }
            ChildToParentMessage::Completed(report) => {
                let status = child
                    .wait()
                    .context("failed to wait for child agent runner")?;
                if !status.success() {
                    return Err(anyhow!(
                        "child agent runner exited unsuccessfully: {status}"
                    ));
                }
                return Ok(report);
            }
            ChildToParentMessage::Failed { error } => {
                let status = child.wait().ok();
                return Err(anyhow!(
                    "{}{}",
                    error,
                    status
                        .map(|status| format!("; exit_status={status}"))
                        .unwrap_or_default()
                ));
            }
        }
    }
}

fn build_bubblewrap_command(
    sandbox: &SandboxConfig,
    current_exe: &Path,
    workspace_root: &Path,
    runtime_state_root: &Path,
    skill_memory_target: &Path,
    skill_memory_source: &Path,
    skills_source_root: &Path,
    skills_dirs: &[std::path::PathBuf],
) -> Result<Command> {
    if !cfg!(target_os = "linux") {
        return Err(anyhow!(
            "sandbox mode 'bubblewrap' requires Linux with bubblewrap installed"
        ));
    }
    let mut command = Command::new(&sandbox.bubblewrap_binary);
    command.arg("--die-with-parent").arg("--new-session");
    if Path::new("/usr").exists() {
        command.args(["--ro-bind", "/usr", "/usr"]);
    }
    if Path::new("/bin").exists() {
        command.args(["--ro-bind", "/bin", "/bin"]);
    }
    if Path::new("/sbin").exists() {
        command.args(["--ro-bind", "/sbin", "/sbin"]);
    }
    if Path::new("/lib").exists() {
        command.args(["--ro-bind", "/lib", "/lib"]);
    }
    if Path::new("/lib64").exists() {
        command.args(["--ro-bind", "/lib64", "/lib64"]);
    }
    if Path::new("/etc").exists() {
        command.args(["--ro-bind", "/etc", "/etc"]);
    }
    if Path::new("/run").exists() {
        command.args(["--ro-bind", "/run", "/run"]);
    }
    let system_resolv = Path::new("/run/systemd/resolve/resolv.conf");
    if system_resolv.exists() && Path::new("/etc/resolv.conf").exists() {
        bind_path_to(
            &mut command,
            system_resolv,
            Path::new("/etc/resolv.conf"),
            true,
        )?;
    }
    command.args(["--dev", "/dev"]);
    command.args(["--proc", "/proc"]);
    command.args(["--tmpfs", "/tmp"]);
    command.args(["--tmpfs", "/var/tmp"]);
    command.args(["--dir", "/__agent_host"]);
    command.args(["--dir", "/__agent_host/bin"]);
    bind_path_to(
        &mut command,
        current_exe,
        Path::new("/__agent_host/bin/agent_host"),
        true,
    )?;
    bind_path(&mut command, workspace_root, false)?;
    bind_path(&mut command, runtime_state_root, false)?;
    if skill_memory_source.exists() {
        bind_path_to(
            &mut command,
            skill_memory_source,
            skill_memory_target,
            false,
        )?;
    }
    for skill_dir in skills_dirs {
        if skills_source_root.exists() {
            bind_path_to(&mut command, skills_source_root, skill_dir, false)?;
        }
    }
    command.arg("/__agent_host/bin/agent_host");
    Ok(command)
}

fn bind_path(command: &mut Command, path: &Path, read_only: bool) -> Result<()> {
    bind_path_to(command, path, path, read_only)
}

fn bind_path_to(
    command: &mut Command,
    source: &Path,
    target: &Path,
    read_only: bool,
) -> Result<()> {
    let source_str = source
        .to_str()
        .ok_or_else(|| anyhow!("path is not valid UTF-8: {}", source.display()))?;
    let target_str = target
        .to_str()
        .ok_or_else(|| anyhow!("path is not valid UTF-8: {}", target.display()))?;
    if read_only {
        command.args(["--ro-bind", source_str, target_str]);
    } else {
        command.args(["--bind", source_str, target_str]);
    }
    Ok(())
}
