use crate::backend::{AgentBackendKind, BackendExecutionOptions};
use crate::child_rpc::{
    ChildToParentMessage, ChildTurnPayload, ParentToChildMessage, RemoteToolDefinition,
};
use crate::config::{SandboxConfig, SandboxMode};
use agent_frame::{
    ExecutionSignal, PersistentSessionRuntime, SessionExecutionControl, SessionState, Tool,
    run_session_state_controlled_persistent,
};
use anyhow::{Context, Result, anyhow};
use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::Value;
use std::collections::HashMap;
use std::env;
use std::fs;
use std::io::{BufRead, BufReader, BufWriter, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStderr, ChildStdin, ChildStdout, Command, Stdio};
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

pub fn bubblewrap_support_error(sandbox: &SandboxConfig) -> Option<String> {
    if !cfg!(target_os = "linux") {
        return Some(
            "sandbox mode 'bubblewrap' requires Linux with bubblewrap installed".to_string(),
        );
    }
    let binary = sandbox.bubblewrap_binary.trim();
    if binary.is_empty() {
        return Some("sandbox.bubblewrap_binary must not be empty".to_string());
    }
    if !binary_in_path(binary) {
        return Some(format!(
            "sandbox.bubblewrap_binary '{}' was not found in PATH",
            binary
        ));
    }
    None
}

pub fn bubblewrap_is_available(sandbox: &SandboxConfig) -> bool {
    bubblewrap_support_error(sandbox).is_none()
}

fn binary_in_path(binary: &str) -> bool {
    if binary.contains('/') {
        return Path::new(binary).is_file();
    }
    let Some(paths) = env::var_os("PATH") else {
        return false;
    };
    for dir in env::split_paths(&paths) {
        if dir.join(binary).is_file() {
            return true;
        }
    }
    false
}

/// Spawn a child process from a dedicated thread that remains alive (parked)
/// until the child exits. This prevents `PR_SET_PDEATHSIG` (used by
/// `bwrap --die-with-parent`) from firing when the original spawning thread
/// is reaped by a thread pool (e.g. tokio's blocking pool).
fn spawn_with_persistent_parent_thread(mut command: Command) -> Result<Child> {
    let (child_sender, child_receiver) = std::sync::mpsc::channel::<Result<Child>>();
    std::thread::Builder::new()
        .name("bwrap-parent-keeper".to_string())
        .spawn(move || {
            let result = command.spawn();
            let child_id = result.as_ref().ok().map(|c| c.id());
            let _ = child_sender.send(result.map_err(|e| anyhow::anyhow!(e)));
            // Keep this thread alive so PR_SET_PDEATHSIG does not fire.
            // Park until the child process exits. We detect this by periodically
            // checking if the child PID is still alive via a zero-signal kill.
            if let Some(pid) = child_id {
                loop {
                    std::thread::park_timeout(std::time::Duration::from_secs(30));
                    // Check if child is still alive via kill(pid, 0).
                    // SAFETY: kill with signal 0 performs an existence check
                    // without sending any signal.
                    let alive = unsafe { libc::kill(pid as libc::pid_t, 0) } == 0;
                    if !alive {
                        break;
                    }
                }
            }
        })
        .context("failed to spawn bwrap parent keeper thread")?;
    child_receiver
        .recv()
        .map_err(|_| anyhow!("bwrap parent keeper thread terminated before sending child"))?
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

enum ChildCommand {
    RunTurn(ChildTurnPayload),
    Shutdown,
}

pub struct PersistentChildRuntime {
    child: Child,
    writer: Arc<Mutex<BufWriter<ChildStdin>>>,
    reader: BufReader<ChildStdout>,
    child_stderr: Option<ChildStderr>,
}

pub fn is_child_run_turn_request_send_error(error: &anyhow::Error) -> bool {
    error.chain().any(|cause| {
        cause
            .to_string()
            .contains("failed to send child run turn request")
    })
}

pub fn is_child_transport_error(error: &anyhow::Error) -> bool {
    error.chain().any(|cause| {
        let message = cause.to_string();
        message.contains("failed to send child run turn request")
            || message.contains("failed to send child tool response")
            || message.contains("child RPC stream closed unexpectedly")
            || message.contains("failed to decode child RPC stream")
    })
}

pub fn run_child_stdio() -> Result<()> {
    let stdin = std::io::stdin();
    let stdout = std::io::stdout();
    let mut reader = BufReader::new(stdin);
    let writer = Arc::new(Mutex::new(BufWriter::new(stdout)));
    {
        let mut writer = writer.lock().map_err(|_| anyhow!("RPC writer poisoned"))?;
        write_json_line(&mut *writer, &ChildToParentMessage::Started)?;
    }

    let pending: Arc<Mutex<HashMap<String, mpsc::Sender<Result<Value, String>>>>> =
        Arc::new(Mutex::new(HashMap::new()));
    let current_control: Arc<Mutex<Option<SessionExecutionControl>>> = Arc::new(Mutex::new(None));
    let pending_map = Arc::clone(&pending);
    let signal_control = Arc::clone(&current_control);
    let (command_sender, command_receiver) = mpsc::channel::<ChildCommand>();
    let _inbound_thread = std::thread::spawn(move || -> Result<()> {
        while let Some(message) = read_json_line::<ParentToChildMessage>(&mut reader)? {
            match message {
                ParentToChildMessage::RunTurn(payload) => {
                    command_sender
                        .send(ChildCommand::RunTurn(payload))
                        .map_err(|_| anyhow!("child command channel closed"))?;
                }
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
                    if let Ok(guard) = signal_control.lock()
                        && let Some(control) = guard.as_ref()
                    {
                        control.request_timeout_observation();
                    }
                }
                ParentToChildMessage::Yield => {
                    if let Ok(guard) = signal_control.lock()
                        && let Some(control) = guard.as_ref()
                    {
                        control.request_yield();
                    }
                }
                ParentToChildMessage::Cancel => {
                    if let Ok(guard) = signal_control.lock()
                        && let Some(control) = guard.as_ref()
                    {
                        control.request_cancel();
                    }
                }
                ParentToChildMessage::Shutdown => {
                    let _ = command_sender.send(ChildCommand::Shutdown);
                    break;
                }
            }
        }
        Ok(())
    });

    let mut agent_frame_runtime = PersistentSessionRuntime::new();
    while let Ok(command) = command_receiver.recv() {
        match command {
            ChildCommand::RunTurn(payload) => {
                let control = SessionExecutionControl::new().with_event_callback({
                    let writer = Arc::clone(&writer);
                    move |event| {
                        if let Ok(mut writer) = writer.lock() {
                            let _ = write_json_line(
                                &mut *writer,
                                &ChildToParentMessage::SessionEvent(event),
                            );
                        }
                    }
                });
                {
                    let mut active = current_control
                        .lock()
                        .map_err(|_| anyhow!("current child control lock poisoned"))?;
                    *active = Some(control.clone());
                }

                let mut extra_tools = Vec::with_capacity(payload.extra_tools.len());
                for definition in &payload.extra_tools {
                    extra_tools.push(build_proxy_tool(
                        definition,
                        Arc::clone(&writer),
                        Arc::clone(&pending),
                    ));
                }

                let result = match payload.backend {
                    AgentBackendKind::AgentFrame => run_session_state_controlled_persistent(
                        payload.previous_messages,
                        payload.prompt,
                        payload.config,
                        extra_tools,
                        Some(control),
                        &mut agent_frame_runtime,
                    ),
                    AgentBackendKind::Zgent => {
                        if !crate::zgent::zgent_runtime_available() {
                            Err(anyhow!(
                                "the zgent backend is unavailable because the local ./zgent runtime directory is unavailable"
                            ))
                        } else {
                            crate::zgent::kernel::run_session_state_controlled(
                                payload.previous_messages,
                                payload.prompt,
                                payload.config,
                                extra_tools,
                                Some(control),
                                payload.execution_options,
                            )
                        }
                    }
                };

                {
                    let mut active = current_control
                        .lock()
                        .map_err(|_| anyhow!("current child control lock poisoned"))?;
                    *active = None;
                }

                let mut writer = writer.lock().map_err(|_| anyhow!("RPC writer poisoned"))?;
                match result {
                    Ok(report) => {
                        write_json_line(&mut *writer, &ChildToParentMessage::Completed(report))?
                    }
                    Err(error) => write_json_line(
                        &mut *writer,
                        &ChildToParentMessage::Failed {
                            error: format!("{error:#}"),
                        },
                    )?,
                }
            }
            ChildCommand::Shutdown => {
                break;
            }
        }
    }

    Ok(())
}

impl PersistentChildRuntime {
    pub fn spawn(
        sandbox: &SandboxConfig,
        workspace_root: &Path,
        runtime_state_root: &Path,
        global_install_root: PathBuf,
        skill_memory_source: PathBuf,
        skills_source_root: PathBuf,
        skills_dirs: &[std::path::PathBuf],
    ) -> Result<Self> {
        fs::create_dir_all(runtime_state_root).with_context(|| {
            format!(
                "failed to prepare runtime state root {}",
                runtime_state_root.display()
            )
        })?;
        let current_exe = resolve_spawnable_current_exe()?;
        let mut command = match sandbox.mode {
            SandboxMode::Subprocess => Command::new(&current_exe),
            SandboxMode::Bubblewrap => build_bubblewrap_command(
                sandbox,
                &current_exe,
                workspace_root,
                runtime_state_root,
                &global_install_root,
                &workspace_root.join(".skill_memory"),
                &skill_memory_source,
                &skills_source_root,
                skills_dirs,
            )?,
        };
        command.arg("run-child");
        command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        // IMPORTANT: When using bubblewrap with --die-with-parent, bwrap sets
        // PR_SET_PDEATHSIG(SIGKILL) which is tracked per-thread on Linux, not
        // per-process. If the spawning thread exits (e.g. a tokio blocking pool
        // thread being reaped after idle timeout), the child receives SIGKILL
        // even though the parent process is still alive. To prevent this, we
        // spawn the child from a dedicated thread that stays alive (parked)
        // until the child process exits.
        let mut child = if matches!(sandbox.mode, SandboxMode::Bubblewrap) {
            spawn_with_persistent_parent_thread(command)
                .context("failed to spawn child agent runner")?
        } else {
            command
                .spawn()
                .context("failed to spawn child agent runner")?
        };
        let child_stdin = child.stdin.take().context("child stdin unavailable")?;
        let child_stdout = child.stdout.take().context("child stdout unavailable")?;
        let child_stderr = child.stderr.take().context("child stderr unavailable")?;
        let mut runtime = Self {
            child,
            writer: Arc::new(Mutex::new(BufWriter::new(child_stdin))),
            reader: BufReader::new(child_stdout),
            child_stderr: Some(child_stderr),
        };
        match read_json_line::<ChildToParentMessage>(&mut runtime.reader)? {
            Some(ChildToParentMessage::Started) => Ok(runtime),
            Some(other) => Err(anyhow!(
                "expected child startup acknowledgement, received {:?}",
                other
            )),
            None => Err(anyhow!(
                "child RPC stream closed before startup acknowledgement"
            )),
        }
    }

    pub fn run_turn(
        &mut self,
        backend: AgentBackendKind,
        previous_messages: Vec<agent_frame::ChatMessage>,
        prompt: String,
        config: agent_frame::config::AgentConfig,
        execution_options: BackendExecutionOptions,
        extra_tools: Vec<Tool>,
        control: Option<SessionExecutionControl>,
    ) -> Result<SessionState> {
        {
            let payload = ParentToChildMessage::RunTurn(ChildTurnPayload {
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
                execution_options,
            });
            let mut writer_guard = self
                .writer
                .lock()
                .map_err(|_| anyhow!("child stdin poisoned"))?;
            write_json_line(&mut *writer_guard, &payload)
                .context("failed to send child run turn request")?;
        }

        if let Some(control) = &control {
            let signal_receiver = control.signal_receiver();
            let writer = Arc::clone(&self.writer);
            std::thread::spawn(move || {
                while let Ok(signal) = signal_receiver.recv() {
                    let message = match signal {
                        ExecutionSignal::Cancel => ParentToChildMessage::Cancel,
                        ExecutionSignal::TimeoutObservation => ParentToChildMessage::SoftTimeout,
                        ExecutionSignal::Yield => ParentToChildMessage::Yield,
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

        loop {
            let message = self.read_child_message()?;
            match message {
                ChildToParentMessage::Started => {}
                ChildToParentMessage::SessionEvent(event) => {
                    if let Some(control) = &control {
                        control.emit_event_external(event);
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
                    let mut writer_guard = self
                        .writer
                        .lock()
                        .map_err(|_| anyhow!("child stdin poisoned"))?;
                    write_json_line(&mut *writer_guard, &response)
                        .context("failed to send child tool response")?;
                }
                ChildToParentMessage::Completed(report) => return Ok(report),
                ChildToParentMessage::Failed { error } => return Err(anyhow!(error)),
            }
        }
    }

    fn read_child_message(&mut self) -> Result<ChildToParentMessage> {
        match read_json_line::<ChildToParentMessage>(&mut self.reader) {
            Ok(Some(message)) => Ok(message),
            Ok(None) => {
                let status = self.child.wait().ok();
                let stderr_output = self.read_stderr_lossy();
                Err(anyhow!(
                    "child RPC stream closed unexpectedly; exit_status={}; stderr={}",
                    status
                        .map(|status| status.to_string())
                        .unwrap_or_else(|| "<unknown>".to_string()),
                    if stderr_output.is_empty() {
                        "<empty>"
                    } else {
                        stderr_output.as_str()
                    }
                ))
            }
            Err(error) => {
                let status = self.child.wait().ok();
                let stderr_output = self.read_stderr_lossy();
                Err(anyhow!(
                    "failed to decode child RPC stream: {error:#}; exit_status={}; stderr={}",
                    status
                        .map(|status| status.to_string())
                        .unwrap_or_else(|| "<unknown>".to_string()),
                    if stderr_output.is_empty() {
                        "<empty>"
                    } else {
                        stderr_output.as_str()
                    }
                ))
            }
        }
    }

    fn read_stderr_lossy(&mut self) -> String {
        let Some(stderr) = self.child_stderr.take() else {
            return String::new();
        };
        let mut stderr_reader = BufReader::new(stderr);
        let mut stderr_output = String::new();
        let _ = std::io::Read::read_to_string(&mut stderr_reader, &mut stderr_output);
        stderr_output.trim().to_string()
    }

    pub fn shutdown(&mut self) -> Result<()> {
        if let Ok(mut writer) = self.writer.lock() {
            let _ = write_json_line(&mut *writer, &ParentToChildMessage::Shutdown);
        }
        let status = self
            .child
            .wait()
            .context("failed to wait for child agent runner")?;
        if !status.success() {
            return Err(anyhow!(
                "child agent runner exited unsuccessfully: {status}"
            ));
        }
        Ok(())
    }
}

pub fn run_one_shot_child_turn(
    sandbox: &SandboxConfig,
    backend: AgentBackendKind,
    previous_messages: Vec<agent_frame::ChatMessage>,
    prompt: String,
    config: agent_frame::config::AgentConfig,
    execution_options: BackendExecutionOptions,
    global_install_root: PathBuf,
    skill_memory_source: PathBuf,
    skills_source_root: PathBuf,
    extra_tools: Vec<Tool>,
    control: Option<SessionExecutionControl>,
) -> Result<SessionState> {
    let mut runtime = PersistentChildRuntime::spawn(
        sandbox,
        &config.workspace_root,
        &config.runtime_state_root,
        global_install_root,
        skill_memory_source,
        skills_source_root,
        &config.skills_dirs,
    )?;
    let result = runtime.run_turn(
        backend,
        previous_messages,
        prompt,
        config,
        execution_options,
        extra_tools,
        control,
    );
    let shutdown_result = runtime.shutdown();
    match (result, shutdown_result) {
        (Ok(report), Ok(())) => Ok(report),
        (Err(error), _) => Err(error),
        (Ok(_), Err(error)) => Err(error),
    }
}

fn build_bubblewrap_command(
    sandbox: &SandboxConfig,
    current_exe: &Path,
    workspace_root: &Path,
    runtime_state_root: &Path,
    global_install_root: &Path,
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
    command.args(["--dev", "/dev"]);
    command.args(["--proc", "/proc"]);
    command.args(["--tmpfs", "/tmp"]);
    command.args(["--tmpfs", "/var/tmp"]);
    command.args(["--dir", "/__agent_host"]);
    command.args(["--dir", "/__agent_host/bin"]);
    bind_path_to(
        &mut command,
        current_exe,
        Path::new("/__agent_host/bin/partyclaw"),
        true,
    )?;
    if let Some(home_dir) = discover_home_dir() {
        let home_skeleton = prepare_sandbox_home_skeleton(runtime_state_root, &home_dir)?;
        ensure_home_skeleton_parent_for_target(&home_skeleton, &home_dir, workspace_root)?;
        ensure_home_skeleton_parent_for_target(&home_skeleton, &home_dir, runtime_state_root)?;
        bind_path_to(&mut command, &home_skeleton, &home_dir, true)?;
    }
    bind_path(&mut command, workspace_root, false)?;
    bind_path(&mut command, runtime_state_root, false)?;
    if !global_install_root.as_os_str().is_empty() {
        fs::create_dir_all(global_install_root).with_context(|| {
            format!(
                "failed to prepare global install root {}",
                global_install_root.display()
            )
        })?;
        bind_path(&mut command, global_install_root, false)?;
    }
    if let Some(home_ssh_dir) = discover_home_ssh_dir() {
        bind_path(&mut command, &home_ssh_dir, false)?;
    }
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
    command.arg("/__agent_host/bin/partyclaw");
    Ok(command)
}

pub(crate) fn resolve_spawnable_current_exe() -> Result<PathBuf> {
    let current_exe = std::env::current_exe().context("failed to resolve current executable")?;
    if current_exe.exists() {
        return Ok(current_exe);
    }

    let current_exe_text = current_exe.to_string_lossy();
    if let Some(stripped) = current_exe_text.strip_suffix(" (deleted)") {
        let candidate = PathBuf::from(stripped);
        if candidate.exists() {
            return Ok(candidate);
        }
    }

    Err(anyhow!(
        "resolved current executable does not exist: {}",
        current_exe.display()
    ))
}

fn discover_home_dir() -> Option<PathBuf> {
    env::var_os("HOME").map(PathBuf::from)
}

fn discover_home_ssh_dir() -> Option<PathBuf> {
    let home_dir = discover_home_dir()?;
    let ssh_dir = home_dir.join(".ssh");
    ssh_dir.exists().then_some(ssh_dir)
}

fn prepare_sandbox_home_skeleton(runtime_state_root: &Path, home_dir: &Path) -> Result<PathBuf> {
    let home_name = home_dir
        .file_name()
        .ok_or_else(|| anyhow!("HOME path has no final component: {}", home_dir.display()))?;
    let parent_name = home_dir
        .parent()
        .and_then(Path::file_name)
        .ok_or_else(|| anyhow!("HOME path has no parent directory: {}", home_dir.display()))?;
    let skeleton_root = runtime_state_root.join(".sandbox-home");
    let home_parent = skeleton_root.join(parent_name);
    let home_target = home_parent.join(home_name);
    fs::create_dir_all(&home_target).with_context(|| {
        format!(
            "failed to prepare sandbox home skeleton at {}",
            home_target.display()
        )
    })?;
    fs::create_dir_all(home_target.join(".ssh")).with_context(|| {
        format!(
            "failed to prepare sandbox home ssh placeholder at {}",
            home_target.join(".ssh").display()
        )
    })?;
    Ok(home_target)
}

fn ensure_home_skeleton_parent_for_target(
    home_skeleton: &Path,
    home_dir: &Path,
    target: &Path,
) -> Result<()> {
    let Ok(relative_target) = target.strip_prefix(home_dir) else {
        return Ok(());
    };
    fs::create_dir_all(home_skeleton.join(relative_target)).with_context(|| {
        format!(
            "failed to prepare sandbox home mountpoint for {}",
            target.display()
        )
    })?;
    Ok(())
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

#[cfg(test)]
mod tests {
    use super::build_bubblewrap_command;
    use crate::config::{SandboxConfig, SandboxMode};
    use std::fs;
    use std::path::PathBuf;
    use tempfile::TempDir;

    #[test]
    fn bubblewrap_mounts_global_install_root_as_writable_bind() {
        let temp_dir = TempDir::new().unwrap();
        let current_exe = temp_dir.path().join("partyclaw");
        let workspace_root = temp_dir.path().join("workspace");
        let runtime_state_root = temp_dir.path().join("runtime");
        let global_install_root = temp_dir.path().join("global");
        let skill_memory_source = temp_dir.path().join("skill_memory");
        let skills_source_root = temp_dir.path().join("skills-source");
        let workspace_skills_dir = workspace_root.join(".skills");

        fs::write(&current_exe, b"binary").unwrap();
        fs::create_dir_all(&workspace_root).unwrap();
        fs::create_dir_all(&runtime_state_root).unwrap();
        fs::create_dir_all(&global_install_root).unwrap();
        fs::create_dir_all(&skill_memory_source).unwrap();
        fs::create_dir_all(&skills_source_root).unwrap();
        fs::create_dir_all(&workspace_skills_dir).unwrap();

        let command = build_bubblewrap_command(
            &SandboxConfig {
                mode: SandboxMode::Bubblewrap,
                bubblewrap_binary: "bwrap".to_string(),
            },
            &current_exe,
            &workspace_root,
            &runtime_state_root,
            &global_install_root,
            &workspace_root.join(".skill_memory"),
            &skill_memory_source,
            &skills_source_root,
            &[workspace_skills_dir],
        )
        .unwrap();

        let args = command
            .get_args()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect::<Vec<_>>();

        let expected = vec![
            "--bind".to_string(),
            global_install_root.to_string_lossy().into_owned(),
            global_install_root.to_string_lossy().into_owned(),
        ];
        assert!(
            args.windows(expected.len())
                .any(|window| window == expected),
            "bubblewrap args did not include writable global_install_root bind: {:?}",
            args
        );
    }

    #[test]
    fn bubblewrap_creates_and_mounts_missing_global_install_root() {
        let temp_dir = TempDir::new().unwrap();
        let current_exe = temp_dir.path().join("partyclaw");
        let workspace_root = temp_dir.path().join("workspace");
        let runtime_state_root = temp_dir.path().join("runtime");
        let missing_global_install_root = temp_dir.path().join("missing-global");

        fs::write(&current_exe, b"binary").unwrap();
        fs::create_dir_all(&workspace_root).unwrap();
        fs::create_dir_all(&runtime_state_root).unwrap();

        let command = build_bubblewrap_command(
            &SandboxConfig {
                mode: SandboxMode::Bubblewrap,
                bubblewrap_binary: "bwrap".to_string(),
            },
            &current_exe,
            &workspace_root,
            &runtime_state_root,
            &missing_global_install_root,
            &workspace_root.join(".skill_memory"),
            &temp_dir.path().join("skill_memory"),
            &temp_dir.path().join("skills-source"),
            &[PathBuf::from("/tmp/unused-skills-dir")],
        )
        .unwrap();

        let args = command
            .get_args()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect::<Vec<_>>();

        assert!(missing_global_install_root.is_dir());
        assert!(
            args.iter()
                .any(|arg| arg == &missing_global_install_root.to_string_lossy()),
            "bubblewrap args did not reference created global_install_root: {:?}",
            args
        );
    }
}
