use std::{
    collections::BTreeMap,
    env,
    io::{Read, Write},
    path::{Component, Path, PathBuf},
    sync::{Arc, Mutex},
    thread,
    time::{SystemTime, UNIX_EPOCH},
};

use anyhow::{anyhow, Context};
use crossbeam_channel::{bounded, Receiver, Sender, TrySendError};
use portable_pty::{native_pty_system, CommandBuilder, MasterPty, PtySize};
use serde::{Deserialize, Serialize};
use stellaclaw_core::session_actor::ToolRemoteMode;

use crate::{conversation::ConversationState, workspace::ensure_workspace_for_remote_mode};

const DEFAULT_COLS: u16 = 120;
const DEFAULT_ROWS: u16 = 30;
const MIN_COLS: u16 = 20;
const MIN_ROWS: u16 = 5;
const MAX_COLS: u16 = 400;
const MAX_ROWS: u16 = 120;
const MAX_OUTPUT_BUFFER_BYTES: usize = 2 * 1024 * 1024;
#[cfg(test)]
const DEFAULT_OUTPUT_LIMIT_BYTES: usize = 256 * 1024;
#[cfg(test)]
const MAX_OUTPUT_LIMIT_BYTES: usize = 1024 * 1024;
const MAX_TERMINALS_PER_CONVERSATION: usize = 8;
const MAX_TERMINALS_TOTAL: usize = 128;
const TERMINAL_STREAM_CHANNEL_CAPACITY: usize = 256;
const TERMINAL_STREAM_REPLAY_CHUNK_BYTES: usize = 16 * 1024;

#[derive(Debug, Default, Deserialize)]
pub struct TerminalCreateRequest {
    #[serde(default)]
    pub shell: Option<String>,
    #[serde(default)]
    pub cwd: Option<String>,
    #[serde(default)]
    pub cols: Option<u16>,
    #[serde(default)]
    pub rows: Option<u16>,
}

#[derive(Debug, Deserialize)]
pub struct TerminalResizeRequest {
    pub cols: u16,
    pub rows: u16,
}

#[derive(Debug, Serialize)]
pub struct TerminalSummary {
    pub terminal_id: String,
    pub conversation_id: String,
    pub mode: TerminalMode,
    pub remote: Option<TerminalRemote>,
    pub shell: String,
    pub cwd: String,
    pub cols: u16,
    pub rows: u16,
    pub running: bool,
    pub created_ms: u128,
    pub updated_ms: u128,
    pub next_offset: u64,
}

#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TerminalMode {
    Local,
    FixedSsh,
}

#[derive(Debug, Clone, Serialize)]
pub struct TerminalRemote {
    pub host: String,
    pub cwd: Option<String>,
}

#[cfg(test)]
#[derive(Debug, Serialize)]
pub struct TerminalOutput {
    pub terminal_id: String,
    pub offset: u64,
    pub next_offset: u64,
    pub buffer_start_offset: u64,
    pub dropped_bytes: u64,
    pub encoding: &'static str,
    pub data: String,
    pub running: bool,
}

#[derive(Debug, Clone)]
pub struct TerminalOutputChunk {
    pub bytes: Vec<u8>,
}

pub struct TerminalReplay {
    pub terminal_id: String,
    pub requested_offset: u64,
    pub replay_start_offset: u64,
    pub buffer_start_offset: u64,
    pub next_offset: u64,
    pub dropped_bytes: u64,
    pub chunks: Vec<TerminalOutputChunk>,
    pub running: bool,
}

pub struct TerminalAttach {
    pub replay: TerminalReplay,
    pub receiver: Receiver<TerminalOutputChunk>,
    pub subscriber_id: Option<u64>,
}

#[derive(Debug)]
pub enum WebTerminalError {
    InvalidRequest(String),
    NotFound,
    LimitExceeded(String),
    Internal(anyhow::Error),
}

impl std::fmt::Display for WebTerminalError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidRequest(message) | Self::LimitExceeded(message) => {
                formatter.write_str(message)
            }
            Self::NotFound => formatter.write_str("terminal_not_found"),
            Self::Internal(error) => write!(formatter, "{error:#}"),
        }
    }
}

impl std::error::Error for WebTerminalError {}

impl From<anyhow::Error> for WebTerminalError {
    fn from(error: anyhow::Error) -> Self {
        Self::Internal(error)
    }
}

#[derive(Default)]
pub struct TerminalManager {
    inner: Mutex<TerminalManagerInner>,
}

#[derive(Default)]
struct TerminalManagerInner {
    next_index: u64,
    sessions: BTreeMap<String, Arc<TerminalSession>>,
}

struct TerminalSession {
    terminal_id: String,
    conversation_id: String,
    runtime_key: TerminalRuntimeKey,
    mode: TerminalMode,
    remote: Option<TerminalRemote>,
    shell: String,
    cwd: String,
    size: Mutex<TerminalSize>,
    master: Mutex<Box<dyn MasterPty + Send>>,
    writer: Mutex<Box<dyn Write + Send>>,
    child: Arc<Mutex<Box<dyn portable_pty::Child + Send>>>,
    output: Arc<Mutex<OutputBuffer>>,
    output_subscribers: Arc<Mutex<OutputSubscribers>>,
    status: Arc<Mutex<TerminalStatus>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum TerminalRuntimeKey {
    Local,
    FixedSsh { host: String, cwd: Option<String> },
}

#[derive(Debug, Clone, Copy)]
struct TerminalSize {
    cols: u16,
    rows: u16,
}

#[derive(Debug)]
struct TerminalStatus {
    running: bool,
    created_ms: u128,
    updated_ms: u128,
}

#[derive(Debug, Default)]
struct OutputBuffer {
    start_offset: u64,
    next_offset: u64,
    bytes: Vec<u8>,
}

#[derive(Debug, Default)]
struct OutputSubscribers {
    next_id: u64,
    entries: Vec<OutputSubscriber>,
}

#[derive(Debug)]
struct OutputSubscriber {
    id: u64,
    sender: Sender<TerminalOutputChunk>,
}

impl TerminalManager {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn list(&self, state: &ConversationState) -> Vec<TerminalSummary> {
        let Ok(mut inner) = self.inner.lock() else {
            return Vec::new();
        };
        reset_stale_terminals(&mut inner, state);
        inner
            .sessions
            .values()
            .filter(|session| session.conversation_id == state.conversation_id)
            .map(|session| session.summary())
            .collect()
    }

    pub fn get(
        &self,
        state: &ConversationState,
        terminal_id: &str,
    ) -> Result<TerminalSummary, WebTerminalError> {
        Ok(self.lookup(state, terminal_id)?.summary())
    }

    pub fn create(
        &self,
        workdir: &Path,
        state: &ConversationState,
        request: TerminalCreateRequest,
    ) -> Result<TerminalSummary, WebTerminalError> {
        let cols = clamp_dimension(request.cols.unwrap_or(DEFAULT_COLS), MIN_COLS, MAX_COLS);
        let rows = clamp_dimension(request.rows.unwrap_or(DEFAULT_ROWS), MIN_ROWS, MAX_ROWS);
        let shell = resolve_shell(request.shell.as_deref(), &state.tool_remote_mode)?;
        let cwd_request = normalize_relative_cwd(request.cwd.as_deref())?;

        let mut inner = self
            .inner
            .lock()
            .map_err(|_| WebTerminalError::Internal(anyhow!("terminal manager lock poisoned")))?;
        reset_stale_terminals(&mut inner, state);
        if inner.sessions.len() >= MAX_TERMINALS_TOTAL {
            return Err(WebTerminalError::LimitExceeded(format!(
                "web terminal limit exceeded: at most {MAX_TERMINALS_TOTAL} terminals total"
            )));
        }
        let conversation_count = inner
            .sessions
            .values()
            .filter(|session| session.conversation_id == state.conversation_id)
            .count();
        if conversation_count >= MAX_TERMINALS_PER_CONVERSATION {
            return Err(WebTerminalError::LimitExceeded(format!(
                "web terminal limit exceeded: at most {MAX_TERMINALS_PER_CONVERSATION} terminals per conversation"
            )));
        }

        let terminal_id = format!("terminal_{:04}", inner.next_index);
        inner.next_index = inner.next_index.saturating_add(1);
        let session = Arc::new(spawn_terminal_session(
            workdir,
            state,
            terminal_id.clone(),
            shell,
            cwd_request,
            TerminalSize { cols, rows },
        )?);
        let summary = session.summary();
        inner.sessions.insert(terminal_id, session);
        Ok(summary)
    }

    #[cfg(test)]
    pub fn output(
        &self,
        state: &ConversationState,
        terminal_id: &str,
        offset: u64,
        limit_bytes: usize,
    ) -> Result<TerminalOutput, WebTerminalError> {
        let session = self.lookup(state, terminal_id)?;
        Ok(session.output(offset, limit_bytes))
    }

    #[cfg(test)]
    pub fn input(
        &self,
        state: &ConversationState,
        terminal_id: &str,
        data: &str,
    ) -> Result<TerminalSummary, WebTerminalError> {
        self.input_bytes(state, terminal_id, data.as_bytes())
    }

    pub fn input_bytes(
        &self,
        state: &ConversationState,
        terminal_id: &str,
        data: &[u8],
    ) -> Result<TerminalSummary, WebTerminalError> {
        let session = self.lookup(state, terminal_id)?;
        if !session.is_running() {
            return Err(WebTerminalError::InvalidRequest(
                "terminal is not running".to_string(),
            ));
        }
        session
            .writer
            .lock()
            .map_err(|_| WebTerminalError::Internal(anyhow!("terminal writer lock poisoned")))?
            .write_all(data)
            .context("failed to write terminal input")?;
        session.touch();
        Ok(session.summary())
    }

    pub fn resize(
        &self,
        state: &ConversationState,
        terminal_id: &str,
        request: TerminalResizeRequest,
    ) -> Result<TerminalSummary, WebTerminalError> {
        let session = self.lookup(state, terminal_id)?;
        let cols = clamp_dimension(request.cols, MIN_COLS, MAX_COLS);
        let rows = clamp_dimension(request.rows, MIN_ROWS, MAX_ROWS);
        session
            .master
            .lock()
            .map_err(|_| WebTerminalError::Internal(anyhow!("terminal master lock poisoned")))?
            .resize(PtySize {
                rows,
                cols,
                pixel_width: 0,
                pixel_height: 0,
            })
            .context("failed to resize terminal")?;
        *session
            .size
            .lock()
            .map_err(|_| WebTerminalError::Internal(anyhow!("terminal size lock poisoned")))? =
            TerminalSize { cols, rows };
        session.touch();
        Ok(session.summary())
    }

    pub fn terminate(
        &self,
        state: &ConversationState,
        terminal_id: &str,
    ) -> Result<TerminalSummary, WebTerminalError> {
        let session = self.lookup(state, terminal_id)?;
        if session.is_running() {
            kill_terminal_session(&session)?;
            session.finish();
        }
        let summary = session.summary();
        if let Ok(mut inner) = self.inner.lock() {
            inner.sessions.remove(terminal_id);
        }
        Ok(summary)
    }

    pub fn terminate_conversation(&self, conversation_id: &str) -> usize {
        let Ok(mut inner) = self.inner.lock() else {
            return 0;
        };
        let terminal_ids = inner
            .sessions
            .iter()
            .filter(|(_, session)| session.conversation_id == conversation_id)
            .map(|(terminal_id, _)| terminal_id.clone())
            .collect::<Vec<_>>();
        let mut terminated = 0;
        for terminal_id in terminal_ids {
            if let Some(session) = inner.sessions.remove(&terminal_id) {
                if session.is_running() {
                    let _ = kill_terminal_session(&session);
                }
                session.finish();
                terminated += 1;
            }
        }
        terminated
    }

    pub fn replay(
        &self,
        state: &ConversationState,
        terminal_id: &str,
        offset: u64,
    ) -> Result<TerminalReplay, WebTerminalError> {
        let session = self.lookup(state, terminal_id)?;
        Ok(session.replay(offset))
    }

    pub fn attach(
        &self,
        state: &ConversationState,
        terminal_id: &str,
        offset: u64,
    ) -> Result<TerminalAttach, WebTerminalError> {
        let session = self.lookup(state, terminal_id)?;
        Ok(session.attach(offset))
    }

    pub fn detach(
        &self,
        state: &ConversationState,
        terminal_id: &str,
        subscriber_id: u64,
    ) -> Result<(), WebTerminalError> {
        let session = self.lookup(state, terminal_id)?;
        session.detach(subscriber_id);
        Ok(())
    }

    fn lookup(
        &self,
        state: &ConversationState,
        terminal_id: &str,
    ) -> Result<Arc<TerminalSession>, WebTerminalError> {
        let mut inner = self
            .inner
            .lock()
            .map_err(|_| WebTerminalError::Internal(anyhow!("terminal manager lock poisoned")))?;
        reset_stale_terminals(&mut inner, state);
        let Some(session) = inner.sessions.get(terminal_id) else {
            return Err(WebTerminalError::NotFound);
        };
        if session.conversation_id != state.conversation_id {
            return Err(WebTerminalError::NotFound);
        }
        Ok(session.clone())
    }
}

fn reset_stale_terminals(inner: &mut TerminalManagerInner, state: &ConversationState) {
    let runtime_key = terminal_runtime_key(state);
    let stale_ids = inner
        .sessions
        .iter()
        .filter(|(_, session)| {
            session.conversation_id == state.conversation_id && session.runtime_key != runtime_key
        })
        .map(|(terminal_id, _)| terminal_id.clone())
        .collect::<Vec<_>>();
    for terminal_id in stale_ids {
        if let Some(session) = inner.sessions.remove(&terminal_id) {
            let _ = kill_terminal_session(&session);
            session.finish();
        }
    }
}

fn kill_terminal_session(session: &TerminalSession) -> Result<(), WebTerminalError> {
    let _ = session
        .child
        .lock()
        .map_err(|_| WebTerminalError::Internal(anyhow!("terminal child lock poisoned")))?
        .kill();
    Ok(())
}

fn terminal_runtime_key(state: &ConversationState) -> TerminalRuntimeKey {
    match &state.tool_remote_mode {
        ToolRemoteMode::Selectable => TerminalRuntimeKey::Local,
        ToolRemoteMode::FixedSsh { host, cwd } => TerminalRuntimeKey::FixedSsh {
            host: host.clone(),
            cwd: cwd.clone(),
        },
    }
}

impl TerminalSession {
    fn summary(&self) -> TerminalSummary {
        let size = *self.size.lock().expect("terminal size lock poisoned");
        let status = self.status.lock().expect("terminal status lock poisoned");
        let output = self.output.lock().expect("terminal output lock poisoned");
        TerminalSummary {
            terminal_id: self.terminal_id.clone(),
            conversation_id: self.conversation_id.clone(),
            mode: self.mode,
            remote: self.remote.clone(),
            shell: self.shell.clone(),
            cwd: self.cwd.clone(),
            cols: size.cols,
            rows: size.rows,
            running: status.running,
            created_ms: status.created_ms,
            updated_ms: status.updated_ms,
            next_offset: output.next_offset,
        }
    }

    #[cfg(test)]
    fn output(&self, offset: u64, limit_bytes: usize) -> TerminalOutput {
        let output = self.output.lock().expect("terminal output lock poisoned");
        let limit = limit_bytes
            .clamp(1, MAX_OUTPUT_LIMIT_BYTES)
            .min(output.bytes.len().max(1));
        let start = offset.max(output.start_offset);
        let available_start = start.saturating_sub(output.start_offset) as usize;
        let mut available_end = available_start
            .saturating_add(limit)
            .min(output.bytes.len());
        // Avoid splitting a multi-byte UTF-8 sequence at the boundary.
        // Walk back up to 3 bytes to find a char-aligned cut point.
        if available_end < output.bytes.len() {
            let mut retreat = 0;
            while retreat < 4 && available_end > available_start {
                let byte = output.bytes[available_end - 1];
                if byte < 0x80 || byte >= 0xC0 {
                    // ASCII or the start of a multi-byte sequence.
                    // Check if this start byte's full sequence fits.
                    if byte >= 0xC0 {
                        let seq_len = match byte {
                            0xC0..=0xDF => 2,
                            0xE0..=0xEF => 3,
                            0xF0..=0xF7 => 4,
                            _ => 1,
                        };
                        let seq_start = available_end - 1;
                        if seq_start + seq_len > available_end {
                            // Incomplete sequence at end — exclude it.
                            available_end -= 1;
                        }
                    }
                    break;
                }
                // Continuation byte (0x80..0xBF) — keep retreating.
                available_end -= 1;
                retreat += 1;
            }
        }
        let data = if available_start < output.bytes.len() && available_start < available_end {
            String::from_utf8_lossy(&output.bytes[available_start..available_end]).to_string()
        } else {
            String::new()
        };
        let next_offset = output.start_offset.saturating_add(available_end as u64);
        let status = self.status.lock().expect("terminal status lock poisoned");
        TerminalOutput {
            terminal_id: self.terminal_id.clone(),
            offset,
            next_offset,
            buffer_start_offset: output.start_offset,
            dropped_bytes: output.start_offset.saturating_sub(offset),
            encoding: "utf8_lossy",
            data,
            running: status.running,
        }
    }

    fn replay(&self, offset: u64) -> TerminalReplay {
        let status = self.status.lock().expect("terminal status lock poisoned");
        let output = self.output.lock().expect("terminal output lock poisoned");
        replay_from_output(&self.terminal_id, offset, &output, status.running)
    }

    fn attach(&self, offset: u64) -> TerminalAttach {
        let (sender, receiver) = bounded(TERMINAL_STREAM_CHANNEL_CAPACITY);
        let status = self.status.lock().expect("terminal status lock poisoned");
        let output = self.output.lock().expect("terminal output lock poisoned");
        let replay = replay_from_output(&self.terminal_id, offset, &output, status.running);
        let subscriber_id = if status.running {
            let mut subscribers = self
                .output_subscribers
                .lock()
                .expect("terminal output subscriber lock poisoned");
            let id = subscribers.next_id;
            subscribers.next_id = subscribers.next_id.saturating_add(1);
            subscribers.entries.push(OutputSubscriber { id, sender });
            Some(id)
        } else {
            None
        };
        TerminalAttach {
            replay,
            receiver,
            subscriber_id,
        }
    }

    fn detach(&self, subscriber_id: u64) {
        if let Ok(mut subscribers) = self.output_subscribers.lock() {
            subscribers
                .entries
                .retain(|subscriber| subscriber.id != subscriber_id);
        }
    }

    fn is_running(&self) -> bool {
        self.status
            .lock()
            .map(|status| status.running)
            .unwrap_or(false)
    }

    fn touch(&self) {
        if let Ok(mut status) = self.status.lock() {
            status.updated_ms = unix_millis();
        }
    }

    fn finish(&self) {
        if let Ok(mut status) = self.status.lock() {
            status.running = false;
            status.updated_ms = unix_millis();
        }
    }
}

fn replay_from_output(
    terminal_id: &str,
    offset: u64,
    output: &OutputBuffer,
    running: bool,
) -> TerminalReplay {
    let replay_start_offset = offset.max(output.start_offset);
    let start_index = replay_start_offset.saturating_sub(output.start_offset) as usize;
    let start_index = start_index.min(output.bytes.len());
    let mut chunks = Vec::new();
    let mut index = start_index;
    while index < output.bytes.len() {
        let end = index
            .saturating_add(TERMINAL_STREAM_REPLAY_CHUNK_BYTES)
            .min(output.bytes.len());
        chunks.push(TerminalOutputChunk {
            bytes: output.bytes[index..end].to_vec(),
        });
        index = end;
    }
    TerminalReplay {
        terminal_id: terminal_id.to_string(),
        requested_offset: offset,
        replay_start_offset,
        buffer_start_offset: output.start_offset,
        next_offset: output.next_offset,
        dropped_bytes: output.start_offset.saturating_sub(offset),
        chunks,
        running,
    }
}

fn publish_output_chunk(subscribers: &Arc<Mutex<OutputSubscribers>>, chunk: TerminalOutputChunk) {
    let Ok(mut subscribers) = subscribers.lock() else {
        return;
    };
    subscribers.entries.retain(
        |subscriber| match subscriber.sender.try_send(chunk.clone()) {
            Ok(()) => true,
            Err(TrySendError::Full(_)) | Err(TrySendError::Disconnected(_)) => false,
        },
    );
}

impl OutputBuffer {
    fn append(&mut self, data: &[u8]) {
        self.bytes.extend_from_slice(data);
        self.next_offset = self.next_offset.saturating_add(data.len() as u64);
        if self.bytes.len() > MAX_OUTPUT_BUFFER_BYTES {
            let drop_len = self.bytes.len() - MAX_OUTPUT_BUFFER_BYTES;
            self.bytes.drain(..drop_len);
            self.start_offset = self.start_offset.saturating_add(drop_len as u64);
        }
    }
}

fn spawn_terminal_session(
    workdir: &Path,
    state: &ConversationState,
    terminal_id: String,
    shell: String,
    cwd_request: Option<String>,
    size: TerminalSize,
) -> Result<TerminalSession, WebTerminalError> {
    let pty_system = native_pty_system();
    let pair = pty_system
        .openpty(PtySize {
            rows: size.rows,
            cols: size.cols,
            pixel_width: 0,
            pixel_height: 0,
        })
        .context("failed to open terminal pty")?;

    let conversation_root = workdir.join("conversations").join(&state.conversation_id);
    let runtime_key = terminal_runtime_key(state);
    let (mode, remote, cwd_label, mut command) = match &state.tool_remote_mode {
        ToolRemoteMode::Selectable => {
            let workspace_root = ensure_workspace_for_remote_mode(
                workdir,
                &conversation_root,
                &state.conversation_id,
                &state.tool_remote_mode,
            )?;
            let cwd = resolve_local_cwd(&workspace_root, cwd_request.as_deref())?;
            let mut command = CommandBuilder::new(&shell);
            command.cwd(&cwd);
            (
                TerminalMode::Local,
                None,
                cwd.display().to_string(),
                command,
            )
        }
        ToolRemoteMode::FixedSsh { host, cwd } => {
            let remote_cwd = resolve_remote_cwd(cwd.as_deref(), cwd_request.as_deref());
            let remote_command = remote_shell_command(&remote_cwd, &shell);
            let mut command = CommandBuilder::new("ssh");
            command.arg("-tt");
            command.arg(host);
            command.arg("--");
            command.arg(remote_command);
            (
                TerminalMode::FixedSsh,
                Some(TerminalRemote {
                    host: host.clone(),
                    cwd: cwd.clone(),
                }),
                remote_cwd,
                command,
            )
        }
    };
    command.env("TERM", "xterm-256color");
    command.env("COLORTERM", "truecolor");
    command.env("TERM_PROGRAM", "Stellacode");

    let child = pair
        .slave
        .spawn_command(command)
        .context("failed to spawn terminal command")?;
    drop(pair.slave);
    let mut reader = pair
        .master
        .try_clone_reader()
        .context("failed to clone terminal reader")?;
    let writer = pair
        .master
        .take_writer()
        .context("failed to take terminal writer")?;

    let now = unix_millis();
    let session = TerminalSession {
        terminal_id,
        conversation_id: state.conversation_id.clone(),
        runtime_key,
        mode,
        remote,
        shell,
        cwd: cwd_label,
        size: Mutex::new(size),
        master: Mutex::new(pair.master),
        writer: Mutex::new(writer),
        child: Arc::new(Mutex::new(child)),
        output: Arc::new(Mutex::new(OutputBuffer::default())),
        output_subscribers: Arc::new(Mutex::new(OutputSubscribers::default())),
        status: Arc::new(Mutex::new(TerminalStatus {
            running: true,
            created_ms: now,
            updated_ms: now,
        })),
    };

    let output = session.output.clone();
    let output_subscribers = session.output_subscribers.clone();
    let status = session.status.clone();
    let child = session.child.clone();
    thread::spawn(move || {
        let mut buffer = [0_u8; 8192];
        loop {
            match reader.read(&mut buffer) {
                Ok(0) => break,
                Ok(read) => {
                    let mut chunk = None;
                    if let Ok(mut output) = output.lock() {
                        output.append(&buffer[..read]);
                        chunk = Some(TerminalOutputChunk {
                            bytes: buffer[..read].to_vec(),
                        });
                    }
                    if let Some(chunk) = chunk {
                        publish_output_chunk(&output_subscribers, chunk);
                    }
                    if let Ok(mut status) = status.lock() {
                        status.updated_ms = unix_millis();
                    }
                }
                Err(error) => {
                    let message = format!("\r\n[terminal read error: {error}]\r\n");
                    let mut chunk = None;
                    if let Ok(mut output) = output.lock() {
                        output.append(message.as_bytes());
                        chunk = Some(TerminalOutputChunk {
                            bytes: message.into_bytes(),
                        });
                    }
                    if let Some(chunk) = chunk {
                        publish_output_chunk(&output_subscribers, chunk);
                    }
                    break;
                }
            }
        }
        if let Ok(mut child) = child.lock() {
            let _ = child.wait();
        }
        if let Ok(mut status) = status.lock() {
            status.running = false;
            status.updated_ms = unix_millis();
        }
        if let Ok(mut subscribers) = output_subscribers.lock() {
            subscribers.entries.clear();
        }
    });

    Ok(session)
}

fn resolve_shell(
    requested: Option<&str>,
    remote_mode: &ToolRemoteMode,
) -> Result<String, WebTerminalError> {
    let Some(shell) = requested.map(str::trim).filter(|value| !value.is_empty()) else {
        return Ok(match remote_mode {
            ToolRemoteMode::Selectable => env::var("SHELL")
                .ok()
                .filter(|value| valid_shell_name(value))
                .unwrap_or_else(|| "/bin/sh".to_string()),
            ToolRemoteMode::FixedSsh { .. } => "${SHELL:-sh}".to_string(),
        });
    };
    if !valid_shell_name(shell) {
        return Err(WebTerminalError::InvalidRequest(
            "terminal shell must be a simple path or command name".to_string(),
        ));
    }
    Ok(shell.to_string())
}

fn valid_shell_name(shell: &str) -> bool {
    !shell.trim().is_empty()
        && shell
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '/' | '_' | '-' | '.' | '+'))
}

fn normalize_relative_cwd(value: Option<&str>) -> Result<Option<String>, WebTerminalError> {
    let Some(raw) = value.map(str::trim).filter(|value| !value.is_empty()) else {
        return Ok(None);
    };
    let path = Path::new(raw);
    if path.is_absolute() {
        return Err(WebTerminalError::InvalidRequest(
            "terminal cwd must be relative to the conversation workspace".to_string(),
        ));
    }
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::Normal(value) => normalized.push(value),
            Component::ParentDir => {
                return Err(WebTerminalError::InvalidRequest(
                    "terminal cwd must not contain parent components".to_string(),
                ));
            }
            Component::RootDir | Component::Prefix(_) => {
                return Err(WebTerminalError::InvalidRequest(
                    "terminal cwd must be relative to the conversation workspace".to_string(),
                ));
            }
        }
    }
    let normalized = normalized
        .components()
        .filter_map(|component| match component {
            Component::Normal(value) => Some(value.to_string_lossy().to_string()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("/");
    Ok((!normalized.is_empty()).then_some(normalized))
}

fn resolve_local_cwd(
    workspace_root: &Path,
    relative: Option<&str>,
) -> Result<PathBuf, WebTerminalError> {
    let cwd = relative
        .map(|relative| workspace_root.join(relative))
        .unwrap_or_else(|| workspace_root.to_path_buf());
    if !cwd.exists() {
        return Err(WebTerminalError::InvalidRequest(format!(
            "terminal cwd does not exist: {}",
            cwd.display()
        )));
    }
    if !cwd.is_dir() {
        return Err(WebTerminalError::InvalidRequest(format!(
            "terminal cwd is not a directory: {}",
            cwd.display()
        )));
    }
    Ok(cwd)
}

fn resolve_remote_cwd(base: Option<&str>, relative: Option<&str>) -> String {
    let base = base
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .unwrap_or("~");
    match relative {
        Some(relative) if !relative.is_empty() => {
            format!("{}/{}", base.trim_end_matches('/'), relative)
        }
        _ => base.to_string(),
    }
}

fn remote_shell_command(cwd: &str, shell: &str) -> String {
    format!(
        "export TERM=xterm-256color COLORTERM=truecolor TERM_PROGRAM=Stellacode; cd {} && exec {} -l",
        shell_quote(cwd),
        shell
    )
}

fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

fn clamp_dimension(value: u16, min: u16, max: u16) -> u16 {
    value.clamp(min, max)
}

fn unix_millis() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{collections::BTreeMap, fs, thread, time::Duration};

    use crate::{
        config::{ModelSelection, SessionProfile},
        conversation::ConversationSessionBinding,
    };

    fn test_workdir(name: &str) -> PathBuf {
        let path = env::temp_dir().join(format!(
            "stellaclaw-web-terminal-{name}-{}-{}",
            std::process::id(),
            unix_millis()
        ));
        fs::create_dir_all(&path).expect("create temp workdir");
        path
    }

    fn test_state(conversation_id: &str, tool_remote_mode: ToolRemoteMode) -> ConversationState {
        ConversationState {
            version: 1,
            conversation_id: conversation_id.to_string(),
            nickname: conversation_id.to_string(),
            channel_id: "web-main".to_string(),
            platform_chat_id: "test-chat".to_string(),
            session_profile: SessionProfile {
                main_model: ModelSelection::alias("main"),
            },
            model_selection_pending: false,
            tool_remote_mode,
            sandbox: None,
            reasoning_effort: None,
            session_binding: ConversationSessionBinding {
                foreground_session_id: format!("{conversation_id}.foreground"),
                next_background_index: 1,
                next_subagent_index: 1,
                background_sessions: BTreeMap::new(),
                subagent_sessions: BTreeMap::new(),
            },
        }
    }

    fn wait_for_output(
        manager: &TerminalManager,
        state: &ConversationState,
        terminal_id: &str,
        needle: &str,
    ) -> TerminalOutput {
        let mut output = manager
            .output(state, terminal_id, 0, DEFAULT_OUTPUT_LIMIT_BYTES)
            .expect("read terminal output");
        for _ in 0..50 {
            if output.data.contains(needle) {
                return output;
            }
            thread::sleep(Duration::from_millis(20));
            output = manager
                .output(state, terminal_id, 0, DEFAULT_OUTPUT_LIMIT_BYTES)
                .expect("read terminal output");
        }
        output
    }

    #[test]
    fn terminal_create_sets_term_and_keeps_session_across_list() {
        let workdir = test_workdir("term-env");
        let state = test_state("web-main-test-term-env", ToolRemoteMode::Selectable);
        fs::create_dir_all(workdir.join("conversations").join(&state.conversation_id))
            .expect("create conversation root");
        let manager = TerminalManager::new();

        let terminal = manager
            .create(
                &workdir,
                &state,
                TerminalCreateRequest {
                    shell: Some("/bin/sh".to_string()),
                    cwd: None,
                    cols: Some(81),
                    rows: Some(22),
                },
            )
            .expect("create terminal");
        assert_eq!(terminal.cols, 81);
        assert_eq!(terminal.rows, 22);

        manager
            .input(
                &state,
                &terminal.terminal_id,
                "printf 'TERM=%s\\n' \"$TERM\"\n",
            )
            .expect("write terminal input");
        let output = wait_for_output(&manager, &state, &terminal.terminal_id, "xterm-256color");
        assert!(
            output.data.contains("TERM=xterm-256color"),
            "{}",
            output.data
        );
        assert!(output.next_offset > output.offset);

        let listed = manager.list(&state);
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].terminal_id, terminal.terminal_id);

        let _ = manager.terminate(&state, &terminal.terminal_id);
        let _ = fs::remove_dir_all(workdir);
    }

    #[test]
    fn terminal_list_resets_sessions_after_remote_mode_changes() {
        let workdir = test_workdir("remote-reset");
        let state = test_state("web-main-test-remote-reset", ToolRemoteMode::Selectable);
        fs::create_dir_all(workdir.join("conversations").join(&state.conversation_id))
            .expect("create conversation root");
        let manager = TerminalManager::new();
        let terminal = manager
            .create(
                &workdir,
                &state,
                TerminalCreateRequest {
                    shell: Some("/bin/sh".to_string()),
                    cwd: None,
                    cols: None,
                    rows: None,
                },
            )
            .expect("create terminal");

        let remote_state = test_state(
            &state.conversation_id,
            ToolRemoteMode::FixedSsh {
                host: "example-host".to_string(),
                cwd: Some("~/repo".to_string()),
            },
        );
        assert!(manager.list(&remote_state).is_empty());
        assert!(matches!(
            manager.get(&remote_state, &terminal.terminal_id),
            Err(WebTerminalError::NotFound)
        ));

        let _ = fs::remove_dir_all(workdir);
    }

    #[test]
    fn terminal_attach_streams_raw_bytes_and_replays_by_offset() {
        let workdir = test_workdir("stream-attach");
        let state = test_state("web-main-test-stream-attach", ToolRemoteMode::Selectable);
        fs::create_dir_all(workdir.join("conversations").join(&state.conversation_id))
            .expect("create conversation root");
        let manager = TerminalManager::new();
        let terminal = manager
            .create(
                &workdir,
                &state,
                TerminalCreateRequest {
                    shell: Some("/bin/sh".to_string()),
                    cwd: None,
                    cols: None,
                    rows: None,
                },
            )
            .expect("create terminal");
        let attach = manager
            .attach(&state, &terminal.terminal_id, terminal.next_offset)
            .expect("attach terminal");
        let subscriber_id = attach.subscriber_id.expect("running subscriber");

        manager
            .input_bytes(
                &state,
                &terminal.terminal_id,
                b"printf 'raw-stream-ok\\n'\n",
            )
            .expect("write bytes");
        let mut streamed = Vec::new();
        for _ in 0..50 {
            if let Ok(chunk) = attach.receiver.recv_timeout(Duration::from_millis(20)) {
                streamed.extend_from_slice(&chunk.bytes);
                if streamed
                    .windows(b"raw-stream-ok".len())
                    .any(|window| window == b"raw-stream-ok")
                {
                    break;
                }
            }
        }
        assert!(
            streamed
                .windows(b"raw-stream-ok".len())
                .any(|window| window == b"raw-stream-ok"),
            "{}",
            String::from_utf8_lossy(&streamed)
        );

        let replay = manager
            .replay(&state, &terminal.terminal_id, terminal.next_offset)
            .expect("replay terminal");
        let replayed = replay
            .chunks
            .iter()
            .flat_map(|chunk| chunk.bytes.iter().copied())
            .collect::<Vec<_>>();
        assert!(
            replayed
                .windows(b"raw-stream-ok".len())
                .any(|window| window == b"raw-stream-ok"),
            "{}",
            String::from_utf8_lossy(&replayed)
        );

        manager
            .detach(&state, &terminal.terminal_id, subscriber_id)
            .expect("detach terminal");
        let _ = manager.terminate(&state, &terminal.terminal_id);
        let _ = fs::remove_dir_all(workdir);
    }
}
