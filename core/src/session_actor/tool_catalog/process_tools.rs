use std::{
    collections::{HashMap, VecDeque},
    env,
    io::{Read, Write},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::mpsc::{self, SyncSender, TrySendError},
    sync::{Arc, Mutex, OnceLock},
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use portable_pty::{native_pty_system, Child, ChildKiller, CommandBuilder, MasterPty, PtySize};
use serde_json::{json, Map, Value};

use super::{
    schema::{add_remote_property, object_schema, properties},
    PromptProtocol, ToolBackend, ToolConcurrency, ToolDefinition, ToolExecutionMode,
    ToolRemoteMode,
};
use crate::session_actor::tool_runtime::{
    shell_quote, string_arg, usize_arg_with_default, ExecutionTarget, LocalToolError,
    ToolCancellationToken, ToolExecutionContext,
};

const SHELL_EXEC_DEFAULT_YIELD_MS: usize = 10_000;
const SHELL_WRITE_DEFAULT_YIELD_MS: usize = 250;
const SHELL_MIN_YIELD_MS: usize = 250;
const SHELL_MAX_YIELD_MS: usize = 30_000;
const SHELL_DEFAULT_OUTPUT_CHARS: usize = 20_000;
const SHELL_MAX_OUTPUT_CHARS: usize = 200_000;
const SHELL_BUFFER_MAX_BYTES: usize = 1024 * 1024;
const SHELL_STDIN_QUEUE_SLOTS: usize = 128;
const SHELL_DEFAULT_COLS: u16 = 100;
const SHELL_DEFAULT_ROWS: u16 = 30;

static SHELL_MANAGER: OnceLock<Mutex<ShellManager>> = OnceLock::new();

#[derive(Default)]
struct ShellManager {
    sessions: HashMap<String, Arc<ShellSession>>,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
enum ShellBinding {
    Local,
    RemoteSsh { host: String, cwd: Option<String> },
}

struct ShellSession {
    process_id: String,
    binding: ShellBinding,
    shell: String,
    cwd: String,
    tty: bool,
    cols: u16,
    rows: u16,
    _master: Option<Mutex<Box<dyn MasterPty + Send>>>,
    writer: Option<SyncSender<Vec<u8>>>,
    stopper: ProcessStopper,
    output: Mutex<HeadTailBuffer>,
    terminal: Mutex<TerminalEmulator>,
    status: Mutex<ShellStatus>,
}

enum ProcessStopper {
    Pty(Mutex<Box<dyn ChildKiller + Send + Sync>>),
    Pid(u32),
}

#[derive(Debug)]
struct ShellStatus {
    running: bool,
    exit_code: Option<i32>,
    created_ms: u128,
    updated_ms: u128,
}

struct TerminalRender {
    plain_text: String,
    snapshot: Option<Value>,
}

struct TerminalEmulator {
    cols: usize,
    rows: usize,
    primary: Vec<Vec<char>>,
    alternate: Vec<Vec<char>>,
    use_alternate: bool,
    cursor_row: usize,
    cursor_col: usize,
    saved_cursor: Option<(usize, usize)>,
    saw_alternate_screen: bool,
    cursor_moves: usize,
    erase_ops: usize,
    carriage_returns: usize,
    line_feeds: usize,
    sgr_sequences: usize,
    non_sgr_sequences: usize,
    visible_chars: usize,
}

struct HeadTailBuffer {
    max_bytes: usize,
    head_budget: usize,
    tail_budget: usize,
    head: VecDeque<Vec<u8>>,
    tail: VecDeque<Vec<u8>>,
    head_bytes: usize,
    tail_bytes: usize,
}

impl HeadTailBuffer {
    fn new(max_bytes: usize) -> Self {
        let head_budget = max_bytes / 2;
        Self {
            max_bytes,
            head_budget,
            tail_budget: max_bytes.saturating_sub(head_budget),
            head: VecDeque::new(),
            tail: VecDeque::new(),
            head_bytes: 0,
            tail_bytes: 0,
        }
    }

    fn push(&mut self, bytes: &[u8]) {
        if self.max_bytes == 0 || bytes.is_empty() {
            return;
        }
        if self.head_bytes < self.head_budget {
            let remaining_head = self.head_budget.saturating_sub(self.head_bytes);
            if bytes.len() <= remaining_head {
                self.head_bytes += bytes.len();
                self.head.push_back(bytes.to_vec());
                return;
            }
            let (head, tail) = bytes.split_at(remaining_head);
            if !head.is_empty() {
                self.head_bytes += head.len();
                self.head.push_back(head.to_vec());
            }
            self.push_tail(tail);
            return;
        }
        self.push_tail(bytes);
    }

    fn drain(&mut self) -> Vec<u8> {
        let mut out = Vec::with_capacity(self.head_bytes.saturating_add(self.tail_bytes));
        for chunk in self.head.drain(..) {
            out.extend_from_slice(&chunk);
        }
        for chunk in self.tail.drain(..) {
            out.extend_from_slice(&chunk);
        }
        self.head_bytes = 0;
        self.tail_bytes = 0;
        out
    }

    fn push_tail(&mut self, bytes: &[u8]) {
        if self.tail_budget == 0 || bytes.is_empty() {
            return;
        }
        if bytes.len() >= self.tail_budget {
            let start = bytes.len().saturating_sub(self.tail_budget);
            self.tail.clear();
            self.tail.push_back(bytes[start..].to_vec());
            self.tail_bytes = self.tail_budget;
            return;
        }
        self.tail.push_back(bytes.to_vec());
        self.tail_bytes += bytes.len();
        self.trim_tail();
    }

    fn trim_tail(&mut self) {
        let mut excess = self.tail_bytes.saturating_sub(self.tail_budget);
        while excess > 0 {
            let Some(front) = self.tail.front_mut() else {
                break;
            };
            if excess >= front.len() {
                excess -= front.len();
                self.tail_bytes = self.tail_bytes.saturating_sub(front.len());
                self.tail.pop_front();
            } else {
                front.drain(..excess);
                self.tail_bytes = self.tail_bytes.saturating_sub(excess);
                break;
            }
        }
    }
}

fn shell_manager() -> &'static Mutex<ShellManager> {
    SHELL_MANAGER.get_or_init(|| Mutex::new(ShellManager::default()))
}

pub fn process_tool_definitions(remote_mode: &ToolRemoteMode) -> Vec<ToolDefinition> {
    let mut exec_properties = properties([
        ("command", json!({"type": "string"})),
        ("workdir", json!({"type": "string"})),
        ("shell", json!({"type": "string"})),
        (
            "login",
            json!({"type": "boolean", "description": "Run the command through a login shell, for example zsh -lc. Defaults to false."}),
        ),
        (
            "tty",
            json!({"type": "boolean", "description": "Allocate a PTY and keep stdin writable. Defaults to false."}),
        ),
        (
            "cols",
            json!({"type": "integer", "minimum": 40, "maximum": 200}),
        ),
        (
            "rows",
            json!({"type": "integer", "minimum": 10, "maximum": 80}),
        ),
        (
            "yield_time_ms",
            json!({"type": "integer", "minimum": 250, "maximum": 30000}),
        ),
        (
            "timeout_ms",
            json!({"type": "integer", "minimum": 0, "maximum": 86400000}),
        ),
        (
            "max_output_chars",
            json!({"type": "integer", "minimum": 0, "maximum": 200000}),
        ),
    ]);
    add_remote_property(&mut exec_properties, remote_mode);

    let write_properties = properties([
        ("process_id", json!({"type": "string"})),
        ("chars", json!({"type": "string"})),
        (
            "yield_time_ms",
            json!({"type": "integer", "minimum": 250, "maximum": 30000}),
        ),
        (
            "max_output_chars",
            json!({"type": "integer", "minimum": 0, "maximum": 200000}),
        ),
    ]);

    let stop_properties = properties([
        ("process_id", json!({"type": "string"})),
        (
            "signal",
            json!({"type": "string", "enum": ["interrupt", "terminate", "kill"]}),
        ),
    ]);

    vec![
        ToolDefinition::new(
            "shell_exec",
            "Execute a command as a fresh process. By default tty=false, stdin is closed, and no hidden shell is reused. If the process is still running after yield_time_ms, the result includes process_id for later shell_write_stdin polling or shell_stop. Set tty=true only for interactive terminal sessions.",
            object_schema(exec_properties.clone(), &["command"]),
            ToolExecutionMode::Interruptible,
            ToolBackend::Local,
        )
        .with_concurrency(ToolConcurrency::Serial),
        ToolDefinition::new(
            "shell_write_stdin",
            "Write chars to an existing tty=true process, or pass empty chars to observe recent output from any running process. Non-empty chars against tty=false returns stdin_closed.",
            object_schema(write_properties, &["process_id"]),
            ToolExecutionMode::Interruptible,
            ToolBackend::Local,
        )
        .with_concurrency(ToolConcurrency::Serial),
        ToolDefinition::new(
            "shell_stop",
            "Stop a running shell process by process_id. signal defaults to terminate.",
            object_schema(stop_properties, &["process_id"]),
            ToolExecutionMode::Immediate,
            ToolBackend::Local,
        )
        .with_concurrency(ToolConcurrency::Serial),
    ]
}

pub(crate) fn process_prompt_protocols() -> &'static [PromptProtocol] {
    PROCESS_PROMPT_PROTOCOLS
}

const PROCESS_PROMPT_PROTOCOLS: &[PromptProtocol] = &[PromptProtocol {
    id: "process.shell",
    priority: 200,
    required_tools: &["shell_exec", "shell_write_stdin", "shell_stop"],
    body: "When using shell for commands that dedicated tools do not cover, start a new command with shell_exec.command. Use shell_write_stdin with process_id only to poll or continue an existing process, and shell_stop with process_id to stop one. Positive example: {\"command\":\"cargo check -p stellaclaw_core\"}. Negative example: using process_id to start work, or using cmd instead of command.",
}];

pub(crate) fn execute_process_tool(
    tool_name: &str,
    arguments: &Map<String, Value>,
    context: &ToolExecutionContext<'_>,
) -> Result<Option<Value>, LocalToolError> {
    let result = match tool_name {
        "shell_exec" => shell_exec(arguments, context)?,
        "shell_write_stdin" => shell_write_stdin(arguments, context)?,
        "shell_stop" => shell_stop(arguments)?,
        _ => return Ok(None),
    };
    Ok(Some(result))
}

fn shell_exec(
    arguments: &Map<String, Value>,
    context: &ToolExecutionContext<'_>,
) -> Result<Value, LocalToolError> {
    let command = string_arg(arguments, "command")?;
    if command.trim().is_empty() {
        return Err(LocalToolError::InvalidArguments(
            "command must not be empty".to_string(),
        ));
    }
    let session = spawn_process(&command, arguments, context)?;
    let wait = yield_ms(arguments, SHELL_EXEC_DEFAULT_YIELD_MS, SHELL_MAX_YIELD_MS)?;
    let max_output_chars = shell_max_output_chars(arguments)?;
    collect_until(
        &session,
        wait,
        max_output_chars,
        &context.cancel_token,
        "shell_exec",
    )
}

fn shell_write_stdin(
    arguments: &Map<String, Value>,
    context: &ToolExecutionContext<'_>,
) -> Result<Value, LocalToolError> {
    let session = find_process(arguments)?;
    validate_remote_consistency(arguments, context, &session)?;
    let chars = optional_string(arguments, "chars").unwrap_or_default();
    if !chars.is_empty() {
        write_to_process(&session, chars.as_bytes())?;
    }
    let wait = yield_ms(arguments, SHELL_WRITE_DEFAULT_YIELD_MS, SHELL_MAX_YIELD_MS)?;
    collect_until(
        &session,
        wait,
        shell_max_output_chars(arguments)?,
        &context.cancel_token,
        "shell_write_stdin",
    )
}

fn shell_stop(arguments: &Map<String, Value>) -> Result<Value, LocalToolError> {
    let process_id = process_id_arg(arguments)
        .ok_or_else(|| LocalToolError::InvalidArguments("missing process_id".to_string()))?;
    validate_process_id(&process_id)?;
    let mut manager = shell_manager().lock().expect("mutex poisoned");
    let Some(session) = manager.sessions.remove(&process_id) else {
        return Ok(json!({
            "process_id": process_id,
            "stopped": false,
            "reason": "unknown_session",
        }));
    };
    drop(manager);

    stop_process(&session, signal_arg(arguments));
    if let Ok(mut status) = session.status.lock() {
        status.running = false;
        status.updated_ms = unix_millis();
    }
    Ok(json!({
        "process_id": process_id,
        "stopped": true,
        "remote": binding_label(&session.binding),
    }))
}

fn find_process(arguments: &Map<String, Value>) -> Result<Arc<ShellSession>, LocalToolError> {
    let process_id = process_id_arg(arguments)
        .ok_or_else(|| LocalToolError::InvalidArguments("missing process_id".to_string()))?;
    validate_process_id(&process_id)?;
    let manager = shell_manager().lock().expect("mutex poisoned");
    manager.sessions.get(&process_id).cloned().ok_or_else(|| {
        LocalToolError::InvalidArguments(format!("unknown shell process {process_id}"))
    })
}

fn spawn_process(
    command_text: &str,
    arguments: &Map<String, Value>,
    context: &ToolExecutionContext<'_>,
) -> Result<Arc<ShellSession>, LocalToolError> {
    let binding = binding_from_context(arguments, context)?;
    let shell = resolve_shell(arguments, &binding)?;
    let login = bool_arg(arguments, "login", false)?;
    let tty = bool_arg(arguments, "tty", false)?;
    let timeout_ms = usize_arg_with_default(arguments, "timeout_ms", 0)?;
    let process_id = generate_process_id();

    if tty {
        spawn_pty_process(
            process_id,
            command_text,
            arguments,
            context,
            binding,
            shell,
            login,
            timeout_ms,
        )
    } else {
        spawn_pipe_process(
            process_id,
            command_text,
            arguments,
            context,
            binding,
            shell,
            login,
            timeout_ms,
        )
    }
}

#[allow(clippy::too_many_arguments)]
fn spawn_pty_process(
    process_id: String,
    command_text: &str,
    arguments: &Map<String, Value>,
    context: &ToolExecutionContext<'_>,
    binding: ShellBinding,
    shell: String,
    login: bool,
    timeout_ms: usize,
) -> Result<Arc<ShellSession>, LocalToolError> {
    let (cols, rows) = terminal_size(arguments)?;

    let pty_system = native_pty_system();
    let pair = pty_system
        .openpty(PtySize {
            rows,
            cols,
            pixel_width: 0,
            pixel_height: 0,
        })
        .map_err(|error| LocalToolError::Io(format!("failed to open pty: {error}")))?;

    let (cwd_label, mut command) = match &binding {
        ShellBinding::Local => {
            let cwd = resolve_local_workdir(context.workspace_root, arguments)?;
            let mut command = CommandBuilder::new(&shell);
            command.arg(shell_exec_flag(login));
            command.arg(command_text);
            command.cwd(&cwd);
            (cwd.display().to_string(), command)
        }
        ShellBinding::RemoteSsh { host, cwd } => {
            let remote_cwd = resolve_remote_workdir(cwd.as_deref(), arguments);
            let remote_command = remote_exec_command(&remote_cwd, &shell, login, command_text);
            let mut command = CommandBuilder::new("ssh");
            command.arg("-tt");
            command.arg(host);
            command.arg("--");
            command.arg(remote_command);
            (remote_cwd, command)
        }
    };
    command.env("TERM", "xterm-256color");
    command.env("COLORTERM", "truecolor");
    command.env("TERM_PROGRAM", "Stellaclaw");

    let child = pair
        .slave
        .spawn_command(command)
        .map_err(|error| LocalToolError::Io(format!("failed to spawn shell: {error}")))?;
    let stopper = ProcessStopper::Pty(Mutex::new(child.clone_killer()));
    drop(pair.slave);
    let mut reader = pair
        .master
        .try_clone_reader()
        .map_err(|error| LocalToolError::Io(format!("failed to clone shell reader: {error}")))?;
    let writer = pair
        .master
        .take_writer()
        .map_err(|error| LocalToolError::Io(format!("failed to take shell writer: {error}")))?;
    let (writer_tx, writer_rx) = mpsc::sync_channel::<Vec<u8>>(SHELL_STDIN_QUEUE_SLOTS);
    thread::spawn(move || {
        let mut writer = writer;
        while let Ok(bytes) = writer_rx.recv() {
            if writer.write_all(&bytes).is_err() {
                break;
            }
            let _ = writer.flush();
        }
    });

    let session = Arc::new(ShellSession {
        process_id: process_id.clone(),
        binding: binding.clone(),
        shell,
        cwd: cwd_label,
        tty: true,
        cols,
        rows,
        _master: Some(Mutex::new(pair.master)),
        writer: Some(writer_tx),
        stopper,
        output: Mutex::new(HeadTailBuffer::new(SHELL_BUFFER_MAX_BYTES)),
        terminal: Mutex::new(TerminalEmulator::new(cols as usize, rows as usize)),
        status: Mutex::new(ShellStatus {
            running: true,
            exit_code: None,
            created_ms: unix_millis(),
            updated_ms: unix_millis(),
        }),
    });

    let reader_session = Arc::clone(&session);
    thread::spawn(move || {
        let mut buffer = [0_u8; 8192];
        loop {
            match reader.read(&mut buffer) {
                Ok(0) => break,
                Ok(read) => push_process_output(&reader_session, &buffer[..read]),
                Err(error) => {
                    push_process_output(
                        &reader_session,
                        format!("\r\n[shell read error: {error}]\r\n").as_bytes(),
                    );
                    break;
                }
            }
        }
    });

    spawn_pty_waiter(Arc::clone(&session), child);
    spawn_timeout_watcher(Arc::clone(&session), timeout_ms);

    let mut manager = shell_manager().lock().expect("mutex poisoned");
    manager.sessions.insert(process_id, Arc::clone(&session));
    Ok(session)
}

#[allow(clippy::too_many_arguments)]
fn spawn_pipe_process(
    process_id: String,
    command_text: &str,
    arguments: &Map<String, Value>,
    context: &ToolExecutionContext<'_>,
    binding: ShellBinding,
    shell: String,
    login: bool,
    timeout_ms: usize,
) -> Result<Arc<ShellSession>, LocalToolError> {
    let (cwd_label, mut command) = match &binding {
        ShellBinding::Local => {
            let cwd = resolve_local_workdir(context.workspace_root, arguments)?;
            let mut command = Command::new(&shell);
            command.arg(shell_exec_flag(login));
            command.arg(command_text);
            command.current_dir(&cwd);
            (cwd.display().to_string(), command)
        }
        ShellBinding::RemoteSsh { host, cwd } => {
            let remote_cwd = resolve_remote_workdir(cwd.as_deref(), arguments);
            let remote_command = remote_exec_command(&remote_cwd, &shell, login, command_text);
            let mut command = Command::new("ssh");
            command.arg(host);
            command.arg("--");
            command.arg(remote_command);
            (remote_cwd, command)
        }
    };
    command.env("TERM", "dumb");
    command.env("TERM_PROGRAM", "Stellaclaw");
    command.stdin(Stdio::null());
    command.stdout(Stdio::piped());
    command.stderr(Stdio::piped());
    configure_process_group(&mut command);

    let mut child = command
        .spawn()
        .map_err(|error| LocalToolError::Io(format!("failed to spawn shell process: {error}")))?;
    let pid = child.id();
    let stdout = child.stdout.take();
    let stderr = child.stderr.take();
    let session = Arc::new(ShellSession {
        process_id: process_id.clone(),
        binding: binding.clone(),
        shell,
        cwd: cwd_label,
        tty: false,
        cols: SHELL_DEFAULT_COLS,
        rows: SHELL_DEFAULT_ROWS,
        _master: None,
        writer: None,
        stopper: ProcessStopper::Pid(pid),
        output: Mutex::new(HeadTailBuffer::new(SHELL_BUFFER_MAX_BYTES)),
        terminal: Mutex::new(TerminalEmulator::new(
            SHELL_DEFAULT_COLS as usize,
            SHELL_DEFAULT_ROWS as usize,
        )),
        status: Mutex::new(ShellStatus {
            running: true,
            exit_code: None,
            created_ms: unix_millis(),
            updated_ms: unix_millis(),
        }),
    });

    if let Some(stdout) = stdout {
        spawn_pipe_reader(Arc::clone(&session), stdout);
    }
    if let Some(stderr) = stderr {
        spawn_pipe_reader(Arc::clone(&session), stderr);
    }
    spawn_pipe_waiter(Arc::clone(&session), child);
    spawn_timeout_watcher(Arc::clone(&session), timeout_ms);

    let mut manager = shell_manager().lock().expect("mutex poisoned");
    manager.sessions.insert(process_id, Arc::clone(&session));
    Ok(session)
}

fn spawn_pipe_reader<R>(session: Arc<ShellSession>, mut reader: R)
where
    R: Read + Send + 'static,
{
    thread::spawn(move || {
        let mut buffer = [0_u8; 8192];
        loop {
            match reader.read(&mut buffer) {
                Ok(0) => break,
                Ok(read) => push_process_output(&session, &buffer[..read]),
                Err(error) => {
                    push_process_output(
                        &session,
                        format!("\n[shell read error: {error}]\n").as_bytes(),
                    );
                    break;
                }
            }
        }
    });
}

fn spawn_pty_waiter(session: Arc<ShellSession>, mut child: Box<dyn Child + Send + Sync>) {
    thread::spawn(move || {
        let exit_code = child
            .wait()
            .ok()
            .map(|status| status.exit_code() as i32)
            .unwrap_or(-1);
        mark_process_exited(&session, Some(exit_code));
    });
}

fn spawn_pipe_waiter(session: Arc<ShellSession>, mut child: std::process::Child) {
    thread::spawn(move || {
        let exit_code = child
            .wait()
            .ok()
            .and_then(|status| status.code())
            .unwrap_or(-1);
        mark_process_exited(&session, Some(exit_code));
    });
}

fn spawn_timeout_watcher(session: Arc<ShellSession>, timeout_ms: usize) {
    if timeout_ms == 0 {
        return;
    }
    thread::spawn(move || {
        thread::sleep(Duration::from_millis(timeout_ms as u64));
        let running = session
            .status
            .lock()
            .map(|status| status.running)
            .unwrap_or(false);
        if running {
            stop_process(&session, "kill");
            push_process_output(&session, b"\n[shell timeout: process killed]\n");
        }
    });
}

fn push_process_output(session: &ShellSession, bytes: &[u8]) {
    if let Ok(mut output) = session.output.lock() {
        output.push(bytes);
    }
    if let Ok(mut status) = session.status.lock() {
        status.updated_ms = unix_millis();
    }
}

fn mark_process_exited(session: &ShellSession, exit_code: Option<i32>) {
    if let Ok(mut status) = session.status.lock() {
        status.running = false;
        status.exit_code = exit_code;
        status.updated_ms = unix_millis();
    }
}

fn stop_process(session: &ShellSession, signal: &str) {
    match &session.stopper {
        ProcessStopper::Pty(killer) => {
            if let Ok(mut killer) = killer.lock() {
                let _ = killer.kill();
            }
        }
        ProcessStopper::Pid(pid) => stop_pid(*pid, signal),
    }
}

#[cfg(unix)]
fn stop_pid(pid: u32, signal: &str) {
    let signal = match signal {
        "interrupt" => libc::SIGINT,
        "kill" => libc::SIGKILL,
        _ => libc::SIGTERM,
    };
    let pgid = -(pid as libc::pid_t);
    unsafe {
        if libc::kill(pgid, signal) != 0 {
            let _ = libc::kill(pid as libc::pid_t, signal);
        }
    }
}

#[cfg(not(unix))]
fn stop_pid(_pid: u32, _signal: &str) {}

#[cfg(unix)]
fn configure_process_group(command: &mut Command) {
    use std::os::unix::process::CommandExt;
    unsafe {
        command.pre_exec(|| {
            if libc::setsid() == -1 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
}

#[cfg(not(unix))]
fn configure_process_group(_command: &mut Command) {}

fn collect_until(
    session: &Arc<ShellSession>,
    wait_ms: usize,
    max_output_chars: usize,
    cancel_token: &ToolCancellationToken,
    operation: &str,
) -> Result<Value, LocalToolError> {
    let deadline = Instant::now() + Duration::from_millis(wait_ms as u64);
    let mut collected = Vec::new();
    let mut exit_code = None;
    loop {
        let chunk = drain_shell_output(session);
        if !chunk.is_empty() {
            collected.extend_from_slice(&chunk);
        }

        if !session
            .status
            .lock()
            .map(|status| status.running)
            .unwrap_or(false)
        {
            thread::sleep(Duration::from_millis(50));
            collected.extend_from_slice(&drain_shell_output(session));
            exit_code = session
                .status
                .lock()
                .ok()
                .and_then(|status| status.exit_code);
            break;
        }

        if Instant::now() >= deadline || cancel_token.is_cancelled() {
            break;
        }
        thread::sleep(Duration::from_millis(20));
    }

    let status = session.status.lock().expect("mutex poisoned");
    let running = status.running;
    if exit_code.is_none() {
        exit_code = status.exit_code;
    }
    let created_ms = status.created_ms;
    let updated_ms = status.updated_ms;
    drop(status);

    let raw_text = String::from_utf8_lossy(&collected).to_string();
    let rendered = render_session_terminal_output(session, &raw_text, max_output_chars);
    let plain = rendered.plain_text;
    let total_output_lines = plain.lines().count();
    let original_chars = plain.chars().count();
    let (output, output_truncated) = truncate_middle_chars(&plain, max_output_chars);
    let snapshot = rendered.snapshot;
    let mut result = Map::new();
    result.insert(
        "operation".to_string(),
        Value::String(operation.to_string()),
    );
    result.insert(
        "process_id".to_string(),
        Value::String(session.process_id.clone()),
    );
    result.insert("running".to_string(), Value::Bool(running));
    result.insert(
        "remote".to_string(),
        Value::String(binding_label(&session.binding)),
    );
    result.insert("cwd".to_string(), Value::String(session.cwd.clone()));
    result.insert("shell".to_string(), Value::String(session.shell.clone()));
    result.insert("tty".to_string(), Value::Bool(session.tty));
    result.insert("cols".to_string(), Value::from(session.cols));
    result.insert("rows".to_string(), Value::from(session.rows));
    result.insert("created_ms".to_string(), Value::from(created_ms as u64));
    result.insert("updated_ms".to_string(), Value::from(updated_ms as u64));
    result.insert("original_chars".to_string(), Value::from(original_chars));
    result.insert(
        "total_output_lines".to_string(),
        Value::from(total_output_lines),
    );
    if output_truncated {
        result.insert("output_truncated".to_string(), Value::Bool(true));
    }
    if !output.is_empty() {
        result.insert("output".to_string(), Value::String(output));
    }
    if let Some(code) = exit_code {
        result.insert("exit_code".to_string(), Value::from(code));
        result.insert("success".to_string(), Value::Bool(code == 0));
    }
    if let Some(snapshot) = snapshot {
        result.insert(
            "kind".to_string(),
            Value::String("terminal_snapshot".to_string()),
        );
        result.insert("terminal_snapshot".to_string(), snapshot);
    } else {
        result.insert("kind".to_string(), Value::String("text".to_string()));
    }
    if !running {
        remove_completed_process(&session.process_id);
    }
    Ok(Value::Object(result))
}

fn remove_completed_process(process_id: &str) {
    let mut manager = shell_manager().lock().expect("mutex poisoned");
    let should_remove = manager
        .sessions
        .get(process_id)
        .and_then(|session| session.status.lock().ok().map(|status| !status.running))
        .unwrap_or(false);
    if should_remove {
        manager.sessions.remove(process_id);
    }
}

fn drain_shell_output(session: &ShellSession) -> Vec<u8> {
    session
        .output
        .lock()
        .map(|mut output| output.drain())
        .unwrap_or_default()
}

fn write_to_process(session: &ShellSession, bytes: &[u8]) -> Result<(), LocalToolError> {
    let Some(writer) = &session.writer else {
        return Err(LocalToolError::InvalidArguments(
            "stdin_closed: process was started with tty=false".to_string(),
        ));
    };
    match writer.try_send(bytes.to_vec()) {
        Ok(()) => Ok(()),
        Err(TrySendError::Full(_)) => Err(LocalToolError::Io(
            "stdin_backpressure: process stdin queue is full".to_string(),
        )),
        Err(TrySendError::Disconnected(_)) => Err(LocalToolError::Io(
            "stdin_closed: process stdin writer is closed".to_string(),
        )),
    }
}

fn strip_ansi(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    let mut chars = value.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch == '\u{1b}' {
            if chars.peek() == Some(&'[') {
                let _ = chars.next();
                for next in chars.by_ref() {
                    if ('@'..='~').contains(&next) {
                        break;
                    }
                }
                continue;
            }
            if chars.peek() == Some(&']') {
                let _ = chars.next();
                while let Some(next) = chars.next() {
                    if next == '\u{7}' {
                        break;
                    }
                    if next == '\u{1b}' && chars.peek() == Some(&'\\') {
                        let _ = chars.next();
                        break;
                    }
                }
                continue;
            }
            continue;
        }
        if ch == '\r' {
            continue;
        }
        out.push(ch);
    }
    out
}

#[cfg(test)]
fn render_terminal_output(
    cols: u16,
    rows: u16,
    raw_text: &str,
    max_output_chars: usize,
) -> TerminalRender {
    let plain_text = strip_ansi(raw_text);
    let mut terminal = TerminalEmulator::new(cols as usize, rows as usize);
    terminal.feed(raw_text);
    let snapshot = terminal_snapshot_value(&terminal, cols, rows, max_output_chars);
    TerminalRender {
        plain_text,
        snapshot,
    }
}

fn render_session_terminal_output(
    session: &ShellSession,
    raw_text: &str,
    max_output_chars: usize,
) -> TerminalRender {
    let plain_text = strip_ansi(raw_text);
    let snapshot = session.terminal.lock().ok().and_then(|mut terminal| {
        terminal.feed(raw_text);
        terminal_snapshot_value(&terminal, session.cols, session.rows, max_output_chars)
    });
    TerminalRender {
        plain_text,
        snapshot,
    }
}

fn terminal_snapshot_value(
    terminal: &TerminalEmulator,
    cols: u16,
    rows: u16,
    max_output_chars: usize,
) -> Option<Value> {
    let snapshot = if terminal.should_snapshot() {
        let visible = terminal.visible_text();
        let (visible_text, truncated) = truncate_middle_chars(&visible, max_output_chars);
        Some(json!({
            "cols": cols,
            "rows": rows,
            "alternate_screen": terminal.use_alternate,
            "saw_alternate_screen": terminal.saw_alternate_screen,
            "visible_text": visible_text,
            "truncated": truncated,
        }))
    } else {
        None
    };
    snapshot
}

impl TerminalEmulator {
    fn new(cols: usize, rows: usize) -> Self {
        let cols = cols.max(1);
        let rows = rows.max(1);
        Self {
            cols,
            rows,
            primary: vec![vec![' '; cols]; rows],
            alternate: vec![vec![' '; cols]; rows],
            use_alternate: false,
            cursor_row: 0,
            cursor_col: 0,
            saved_cursor: None,
            saw_alternate_screen: false,
            cursor_moves: 0,
            erase_ops: 0,
            carriage_returns: 0,
            line_feeds: 0,
            sgr_sequences: 0,
            non_sgr_sequences: 0,
            visible_chars: 0,
        }
    }

    fn feed(&mut self, value: &str) {
        let mut chars = value.chars().peekable();
        while let Some(ch) = chars.next() {
            match ch {
                '\u{1b}' => self.consume_escape(&mut chars),
                '\r' => {
                    self.carriage_returns += 1;
                    self.cursor_col = 0;
                }
                '\n' => {
                    self.line_feeds += 1;
                    self.newline();
                }
                '\u{8}' => {
                    self.cursor_col = self.cursor_col.saturating_sub(1);
                }
                '\t' => {
                    let next_tab = ((self.cursor_col / 8) + 1) * 8;
                    while self.cursor_col < next_tab {
                        self.put_char(' ');
                    }
                }
                ch if ch.is_control() => {}
                ch => self.put_char(ch),
            }
        }
    }

    fn consume_escape(&mut self, chars: &mut std::iter::Peekable<std::str::Chars<'_>>) {
        match chars.peek().copied() {
            Some('[') => {
                let _ = chars.next();
                self.consume_csi(chars);
            }
            Some(']') => {
                let _ = chars.next();
                self.consume_osc(chars);
            }
            Some('7') => {
                let _ = chars.next();
                self.saved_cursor = Some((self.cursor_row, self.cursor_col));
            }
            Some('8') => {
                let _ = chars.next();
                if let Some((row, col)) = self.saved_cursor {
                    self.move_to(row, col);
                }
            }
            Some('c') => {
                let _ = chars.next();
                self.non_sgr_sequences += 1;
                self.erase_ops += 1;
                self.clear_screen();
            }
            Some(_) => {
                let _ = chars.next();
            }
            None => {}
        }
    }

    fn consume_csi(&mut self, chars: &mut std::iter::Peekable<std::str::Chars<'_>>) {
        let mut sequence = String::new();
        let mut final_byte = None;
        for next in chars.by_ref() {
            if ('@'..='~').contains(&next) {
                final_byte = Some(next);
                break;
            }
            sequence.push(next);
        }
        let Some(final_byte) = final_byte else {
            return;
        };
        if final_byte == 'm' {
            self.sgr_sequences += 1;
            return;
        }
        self.handle_csi(&sequence, final_byte);
    }

    fn consume_osc(&mut self, chars: &mut std::iter::Peekable<std::str::Chars<'_>>) {
        while let Some(next) = chars.next() {
            if next == '\u{7}' {
                break;
            }
            if next == '\u{1b}' && chars.peek() == Some(&'\\') {
                let _ = chars.next();
                break;
            }
        }
    }

    fn handle_csi(&mut self, sequence: &str, final_byte: char) {
        let params = parse_csi_params(sequence);
        match final_byte {
            'H' | 'f' => {
                self.non_sgr_sequences += 1;
                self.cursor_moves += 1;
                let row = csi_param(&params, 0, 1).saturating_sub(1);
                let col = csi_param(&params, 1, 1).saturating_sub(1);
                self.move_to(row, col);
            }
            'A' => self.move_relative(-(csi_param(&params, 0, 1) as isize), 0),
            'B' => self.move_relative(csi_param(&params, 0, 1) as isize, 0),
            'C' => self.move_relative(0, csi_param(&params, 0, 1) as isize),
            'D' => self.move_relative(0, -(csi_param(&params, 0, 1) as isize)),
            'E' => {
                let count = csi_param(&params, 0, 1) as isize;
                self.move_relative(count, 0);
                self.cursor_col = 0;
            }
            'F' => {
                let count = csi_param(&params, 0, 1) as isize;
                self.move_relative(-count, 0);
                self.cursor_col = 0;
            }
            'G' => {
                self.non_sgr_sequences += 1;
                self.cursor_moves += 1;
                let col = csi_param(&params, 0, 1).saturating_sub(1);
                self.move_to(self.cursor_row, col);
            }
            'J' => {
                self.non_sgr_sequences += 1;
                self.erase_ops += 1;
                self.erase_display(csi_param(&params, 0, 0));
            }
            'K' => {
                self.non_sgr_sequences += 1;
                self.erase_ops += 1;
                self.erase_line(csi_param(&params, 0, 0));
            }
            'S' => {
                self.non_sgr_sequences += 1;
                for _ in 0..csi_param(&params, 0, 1) {
                    self.scroll_up();
                }
            }
            's' => {
                self.saved_cursor = Some((self.cursor_row, self.cursor_col));
            }
            'u' => {
                if let Some((row, col)) = self.saved_cursor {
                    self.move_to(row, col);
                }
            }
            'h' | 'l' => {
                self.handle_mode_change(sequence, final_byte == 'h');
            }
            _ => {
                self.non_sgr_sequences += 1;
            }
        }
    }

    fn handle_mode_change(&mut self, sequence: &str, enabled: bool) {
        if sequence.contains("?1049") || sequence.contains("?1047") || sequence.contains("?47") {
            self.non_sgr_sequences += 1;
            self.saw_alternate_screen = true;
            self.use_alternate = enabled;
            self.cursor_row = 0;
            self.cursor_col = 0;
            if enabled {
                self.clear_screen();
            }
        }
    }

    fn should_snapshot(&self) -> bool {
        if self.saw_alternate_screen && self.use_alternate {
            return true;
        }
        if self.erase_ops > 0 && (self.cursor_moves > 0 || self.visible_chars > 0) {
            return true;
        }
        if self.cursor_moves >= 2 {
            return true;
        }
        self.carriage_returns >= 2 && self.line_feeds <= 2 && self.visible_chars >= self.cols
    }

    fn visible_text(&self) -> String {
        let screen = if self.use_alternate {
            &self.alternate
        } else {
            &self.primary
        };
        let mut lines = screen
            .iter()
            .map(|line| line.iter().collect::<String>().trim_end().to_string())
            .collect::<Vec<_>>();
        while lines.last().is_some_and(|line| line.is_empty()) {
            lines.pop();
        }
        lines.join("\n")
    }

    fn put_char(&mut self, ch: char) {
        if self.cursor_col >= self.cols {
            self.newline();
        }
        let row = self.cursor_row.min(self.rows - 1);
        let col = self.cursor_col.min(self.cols - 1);
        if self.use_alternate {
            self.alternate[row][col] = ch;
        } else {
            self.primary[row][col] = ch;
        }
        self.visible_chars += 1;
        self.cursor_col += 1;
        if self.cursor_col >= self.cols {
            self.newline();
        }
    }

    fn newline(&mut self) {
        self.cursor_col = 0;
        if self.cursor_row + 1 >= self.rows {
            self.scroll_up();
        } else {
            self.cursor_row += 1;
        }
    }

    fn scroll_up(&mut self) {
        if self.use_alternate {
            self.alternate.remove(0);
            self.alternate.push(vec![' '; self.cols]);
        } else {
            self.primary.remove(0);
            self.primary.push(vec![' '; self.cols]);
        }
        self.cursor_row = self.rows - 1;
    }

    fn clear_screen(&mut self) {
        if self.use_alternate {
            self.alternate = vec![vec![' '; self.cols]; self.rows];
        } else {
            self.primary = vec![vec![' '; self.cols]; self.rows];
        }
        self.cursor_row = 0;
        self.cursor_col = 0;
    }

    fn erase_display(&mut self, mode: usize) {
        match mode {
            2 | 3 => self.clear_screen(),
            1 => {
                for row in 0..=self.cursor_row.min(self.rows - 1) {
                    let end = if row == self.cursor_row {
                        self.cursor_col.min(self.cols - 1)
                    } else {
                        self.cols - 1
                    };
                    for col in 0..=end {
                        self.set_cell(row, col, ' ');
                    }
                }
            }
            _ => {
                for row in self.cursor_row.min(self.rows - 1)..self.rows {
                    let start = if row == self.cursor_row {
                        self.cursor_col.min(self.cols)
                    } else {
                        0
                    };
                    for col in start..self.cols {
                        self.set_cell(row, col, ' ');
                    }
                }
            }
        }
    }

    fn erase_line(&mut self, mode: usize) {
        let row = self.cursor_row.min(self.rows - 1);
        match mode {
            1 => {
                for col in 0..=self.cursor_col.min(self.cols - 1) {
                    self.set_cell(row, col, ' ');
                }
            }
            2 => {
                for col in 0..self.cols {
                    self.set_cell(row, col, ' ');
                }
            }
            _ => {
                for col in self.cursor_col.min(self.cols)..self.cols {
                    self.set_cell(row, col, ' ');
                }
            }
        }
    }

    fn set_cell(&mut self, row: usize, col: usize, ch: char) {
        if self.use_alternate {
            self.alternate[row][col] = ch;
        } else {
            self.primary[row][col] = ch;
        }
    }

    fn move_relative(&mut self, row_delta: isize, col_delta: isize) {
        self.non_sgr_sequences += 1;
        self.cursor_moves += 1;
        let row = self
            .cursor_row
            .saturating_add_signed(row_delta)
            .min(self.rows - 1);
        let col = self
            .cursor_col
            .saturating_add_signed(col_delta)
            .min(self.cols - 1);
        self.move_to(row, col);
    }

    fn move_to(&mut self, row: usize, col: usize) {
        self.cursor_row = row.min(self.rows - 1);
        self.cursor_col = col.min(self.cols - 1);
    }
}

fn parse_csi_params(sequence: &str) -> Vec<usize> {
    sequence
        .trim_start_matches('?')
        .split(';')
        .filter_map(|part| {
            let digits = part
                .chars()
                .take_while(|ch| ch.is_ascii_digit())
                .collect::<String>();
            if digits.is_empty() {
                None
            } else {
                digits.parse::<usize>().ok()
            }
        })
        .collect()
}

fn csi_param(params: &[usize], index: usize, default: usize) -> usize {
    params
        .get(index)
        .copied()
        .filter(|value| *value > 0)
        .unwrap_or(default)
}

fn truncate_middle_chars(value: &str, max_chars: usize) -> (String, bool) {
    let max_chars = max_chars.min(SHELL_MAX_OUTPUT_CHARS);
    let total_chars = value.chars().count();
    if total_chars <= max_chars {
        return (value.to_string(), false);
    }
    if max_chars == 0 {
        return (String::new(), true);
    }
    let marker_template = format!("\n...<{} chars truncated>...\n", total_chars);
    let marker_chars = marker_template.chars().count().min(max_chars);
    if marker_chars >= max_chars {
        return (value.chars().take(max_chars).collect(), true);
    }
    let available = max_chars - marker_chars;
    let head_chars = available / 2;
    let tail_chars = available - head_chars;
    let omitted = total_chars.saturating_sub(head_chars + tail_chars);
    let marker = format!("\n...<{omitted} chars truncated>...\n");
    let head = value.chars().take(head_chars).collect::<String>();
    let tail = value
        .chars()
        .rev()
        .take(tail_chars)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect::<String>();
    (format!("{head}{marker}{tail}"), true)
}

fn validate_remote_consistency(
    arguments: &Map<String, Value>,
    context: &ToolExecutionContext<'_>,
    session: &ShellSession,
) -> Result<(), LocalToolError> {
    let has_remote_arg = arguments.contains_key("remote");
    if has_remote_arg || matches!(context.remote_mode, ToolRemoteMode::FixedSsh { .. }) {
        let requested = binding_from_context(arguments, context)?;
        if requested != session.binding {
            return Err(LocalToolError::InvalidArguments(
                "process_id is bound to a different remote target".to_string(),
            ));
        }
    }
    Ok(())
}

fn binding_from_context(
    arguments: &Map<String, Value>,
    context: &ToolExecutionContext<'_>,
) -> Result<ShellBinding, LocalToolError> {
    Ok(match context.execution_target(arguments)? {
        ExecutionTarget::Local => ShellBinding::Local,
        ExecutionTarget::RemoteSsh { host, cwd } => ShellBinding::RemoteSsh { host, cwd },
    })
}

fn binding_label(binding: &ShellBinding) -> String {
    match binding {
        ShellBinding::Local => "local".to_string(),
        ShellBinding::RemoteSsh { host, .. } => host.clone(),
    }
}

fn resolve_shell(
    arguments: &Map<String, Value>,
    binding: &ShellBinding,
) -> Result<String, LocalToolError> {
    let requested = optional_string(arguments, "shell")
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty());
    let (shell, validate) = match requested {
        Some(shell) => (shell, true),
        None => match binding {
            ShellBinding::Local => (
                env::var("SHELL").unwrap_or_else(|_| "/bin/sh".to_string()),
                true,
            ),
            ShellBinding::RemoteSsh { .. } => ("${SHELL:-sh}".to_string(), false),
        },
    };
    if validate && !valid_shell_name(&shell) {
        return Err(LocalToolError::InvalidArguments(
            "shell must be a simple path or command name".to_string(),
        ));
    }
    Ok(shell)
}

fn valid_shell_name(shell: &str) -> bool {
    !shell.trim().is_empty()
        && shell
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '/' | '_' | '-' | '.' | '+'))
}

fn resolve_local_workdir(
    workspace_root: &Path,
    arguments: &Map<String, Value>,
) -> Result<PathBuf, LocalToolError> {
    let Some(workdir) = optional_string(arguments, "workdir").filter(|value| !value.is_empty())
    else {
        return Ok(workspace_root.to_path_buf());
    };
    let path = PathBuf::from(workdir);
    let path = if path.is_absolute() {
        path
    } else {
        workspace_root.join(path)
    };
    if !path.is_dir() {
        return Err(LocalToolError::InvalidArguments(format!(
            "workdir is not a directory: {}",
            path.display()
        )));
    }
    Ok(path)
}

fn resolve_remote_workdir(base: Option<&str>, arguments: &Map<String, Value>) -> String {
    let base = base
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .unwrap_or("~");
    match optional_string(arguments, "workdir") {
        Some(workdir) if !workdir.trim().is_empty() => {
            if workdir.starts_with('/') || workdir.starts_with('~') {
                workdir
            } else {
                format!("{}/{}", base.trim_end_matches('/'), workdir)
            }
        }
        _ => base.to_string(),
    }
}

fn remote_exec_command(cwd: &str, shell: &str, login: bool, command: &str) -> String {
    format!(
        "export TERM=xterm-256color COLORTERM=truecolor TERM_PROGRAM=Stellaclaw; cd {} && exec {} {} {}",
        shell_quote(cwd),
        shell,
        shell_exec_flag(login),
        shell_quote(command)
    )
}

fn shell_exec_flag(login: bool) -> &'static str {
    if login {
        "-lc"
    } else {
        "-c"
    }
}

fn terminal_size(arguments: &Map<String, Value>) -> Result<(u16, u16), LocalToolError> {
    let cols = usize_arg_with_default(arguments, "cols", SHELL_DEFAULT_COLS as usize)?
        .clamp(40, 200) as u16;
    let rows = usize_arg_with_default(arguments, "rows", SHELL_DEFAULT_ROWS as usize)?.clamp(10, 80)
        as u16;
    Ok((cols, rows))
}

fn yield_ms(
    arguments: &Map<String, Value>,
    default: usize,
    max: usize,
) -> Result<usize, LocalToolError> {
    let value = usize_arg_with_default(arguments, "yield_time_ms", default)?;
    Ok(value.clamp(SHELL_MIN_YIELD_MS, max))
}

fn shell_max_output_chars(arguments: &Map<String, Value>) -> Result<usize, LocalToolError> {
    Ok(
        usize_arg_with_default(arguments, "max_output_chars", SHELL_DEFAULT_OUTPUT_CHARS)?
            .min(SHELL_MAX_OUTPUT_CHARS),
    )
}

fn process_id_arg(arguments: &Map<String, Value>) -> Option<String> {
    optional_string(arguments, "process_id")
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

fn signal_arg(arguments: &Map<String, Value>) -> &str {
    match arguments.get("signal").and_then(Value::as_str) {
        Some("interrupt") => "interrupt",
        Some("kill") => "kill",
        _ => "terminate",
    }
}

fn bool_arg(
    arguments: &Map<String, Value>,
    key: &str,
    default: bool,
) -> Result<bool, LocalToolError> {
    match arguments.get(key) {
        Some(value) => value.as_bool().ok_or_else(|| {
            LocalToolError::InvalidArguments(format!("argument {key} must be a boolean"))
        }),
        None => Ok(default),
    }
}

fn optional_string(arguments: &Map<String, Value>, key: &str) -> Option<String> {
    arguments
        .get(key)
        .and_then(Value::as_str)
        .map(ToOwned::to_owned)
}

fn validate_process_id(process_id: &str) -> Result<(), LocalToolError> {
    if process_id.is_empty()
        || process_id.len() > 128
        || !process_id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
    {
        return Err(LocalToolError::InvalidArguments(
            "process_id must contain only ASCII letters, digits, '_' and '-'".to_string(),
        ));
    }
    Ok(())
}

fn generate_process_id() -> String {
    format!("p_{}", nonce())
}

fn nonce() -> String {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos().to_string())
        .unwrap_or_else(|_| "0".to_string())
}

fn unix_millis() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn terminal_render_keeps_colored_logs_as_plain_text() {
        let rendered = render_terminal_output(
            80,
            24,
            "\u{1b}[31merror\u{1b}[0m plain\n\u{1b}[32mok\u{1b}[0m\n",
            1_000,
        );

        assert_eq!(rendered.plain_text, "error plain\nok\n");
        assert!(rendered.snapshot.is_none());
    }

    #[test]
    fn terminal_render_snapshots_cursor_addressed_screen() {
        let rendered =
            render_terminal_output(20, 4, "\u{1b}[2J\u{1b}[Hheader\u{1b}[3;1Hfooter", 1_000);
        let snapshot = rendered.snapshot.expect("screen output should snapshot");
        let visible = snapshot["visible_text"].as_str().unwrap();

        assert!(visible.contains("header"), "{visible}");
        assert!(visible.contains("footer"), "{visible}");
        assert_eq!(snapshot["alternate_screen"], false);
    }

    #[test]
    fn terminal_render_tracks_alternate_screen() {
        let rendered = render_terminal_output(20, 4, "\u{1b}[?1049h\u{1b}[Hmenu\nitem", 1_000);
        let snapshot = rendered.snapshot.expect("alternate screen should snapshot");

        assert_eq!(snapshot["alternate_screen"], true);
        assert_eq!(snapshot["saw_alternate_screen"], true);
        assert!(snapshot["visible_text"].as_str().unwrap().contains("menu"));
    }

    #[test]
    fn terminal_render_does_not_snapshot_after_alternate_screen_exit() {
        let rendered =
            render_terminal_output(20, 4, "\u{1b}[?1049h\u{1b}[Hmenu\u{1b}[?1049l", 1_000);

        assert!(rendered.snapshot.is_none());
    }

    #[test]
    fn terminal_render_collapses_carriage_return_progress() {
        let rendered =
            render_terminal_output(30, 3, "progress 1\rprogress 2\rprogress done", 1_000);
        let snapshot = rendered
            .snapshot
            .expect("repeated carriage return should snapshot");

        assert_eq!(snapshot["visible_text"], "progress done");
    }
}
