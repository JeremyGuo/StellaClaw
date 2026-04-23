#[cfg(unix)]
mod unix_impl {
    use std::{
        collections::HashMap,
        io::{BufRead, BufReader, Write},
        os::{
            fd::{AsRawFd, FromRawFd, RawFd},
            unix::net::UnixStream,
        },
        process,
        sync::{
            atomic::{AtomicU64, Ordering},
            mpsc, Arc, Mutex, OnceLock,
        },
        thread,
    };

    use serde::{Deserialize, Serialize};

    use crate::{
        model_config::ModelConfig,
        session_actor::{ChatMessage, ToolDefinition},
    };

    use crate::providers::{provider_from_model_config, Provider, ProviderError, ProviderRequest};

    static FORK_SERVER: OnceLock<Arc<ProviderRequestForkServer>> = OnceLock::new();

    #[derive(Debug, Clone)]
    pub struct ForkServerProvider {
        fork_server: Arc<ProviderRequestForkServer>,
    }

    impl ForkServerProvider {
        pub fn global() -> Result<Self, ProviderError> {
            Ok(Self {
                fork_server: global_provider_fork_server()?,
            })
        }
    }

    impl Provider for ForkServerProvider {
        fn send(
            &self,
            model_config: &ModelConfig,
            request: ProviderRequest<'_>,
        ) -> Result<ChatMessage, ProviderError> {
            self.fork_server
                .start(
                    model_config.clone(),
                    ProviderRequestOwned::from_provider_request(&request),
                )?
                .wait()
        }
    }

    pub fn init_global_provider_fork_server(
    ) -> Result<Arc<ProviderRequestForkServer>, ProviderError> {
        if let Some(fork_server) = FORK_SERVER.get() {
            return Ok(fork_server.clone());
        }

        let fork_server = Arc::new(ProviderRequestForkServer::spawn()?);
        let _ = FORK_SERVER.set(fork_server.clone());
        Ok(FORK_SERVER.get().expect("fork server initialized").clone())
    }

    pub fn global_provider_fork_server() -> Result<Arc<ProviderRequestForkServer>, ProviderError> {
        FORK_SERVER.get().cloned().ok_or_else(|| {
            ProviderError::Subprocess("provider request runtime is not initialized".to_string())
        })
    }

    #[derive(Debug)]
    pub struct ProviderRequestForkServer {
        pid: libc::pid_t,
        writer: Mutex<UnixStream>,
        pending: Arc<Mutex<HashMap<String, mpsc::Sender<Result<ChatMessage, ProviderError>>>>>,
        next_request_id: AtomicU64,
    }

    impl ProviderRequestForkServer {
        fn spawn() -> Result<Self, ProviderError> {
            let (parent_stream, forkserver_stream) = UnixStream::pair().map_err(|error| {
                ProviderError::Subprocess(format!(
                    "failed to create forkserver socketpair: {error}"
                ))
            })?;
            let pid = unsafe { libc::fork() };
            if pid < 0 {
                return Err(ProviderError::Subprocess(format!(
                    "failed to fork provider request forkserver: {}",
                    std::io::Error::last_os_error()
                )));
            }

            if pid == 0 {
                drop(parent_stream);
                run_forkserver_process(forkserver_stream);
                unsafe {
                    libc::_exit(0);
                }
            }

            drop(forkserver_stream);
            let reader = parent_stream.try_clone().map_err(|error| {
                ProviderError::Subprocess(format!("failed to clone forkserver stream: {error}"))
            })?;
            let pending = Arc::new(Mutex::new(HashMap::new()));
            spawn_parent_event_reader(reader, pending.clone());

            Ok(Self {
                pid,
                writer: Mutex::new(parent_stream),
                pending,
                next_request_id: AtomicU64::new(1),
            })
        }

        pub fn start(
            &self,
            model_config: ModelConfig,
            request: ProviderRequestOwned,
        ) -> Result<ProviderRequestHandle, ProviderError> {
            let request_id = format!(
                "provider_request_{}_{}",
                process::id(),
                self.next_request_id.fetch_add(1, Ordering::SeqCst)
            );
            let (result_tx, result_rx) = mpsc::channel();
            self.pending
                .lock()
                .expect("mutex poisoned")
                .insert(request_id.clone(), result_tx);

            if let Err(error) = self.send_command(ForkServerCommand::Start {
                request_id: request_id.clone(),
                model_config,
                request,
            }) {
                self.pending
                    .lock()
                    .expect("mutex poisoned")
                    .remove(&request_id);
                return Err(error);
            }

            Ok(ProviderRequestHandle {
                request_id,
                result_rx,
            })
        }

        pub fn abort(&self, request_id: &str) -> Result<(), ProviderError> {
            self.send_command(ForkServerCommand::Cancel {
                request_id: request_id.to_string(),
            })
        }

        fn send_command(&self, command: ForkServerCommand) -> Result<(), ProviderError> {
            let mut writer = self.writer.lock().expect("mutex poisoned");
            serde_json::to_writer(&mut *writer, &command).map_err(|error| {
                ProviderError::Subprocess(format!("failed to encode forkserver command: {error}"))
            })?;
            writer.write_all(b"\n").map_err(|error| {
                ProviderError::Subprocess(format!("failed to write forkserver command: {error}"))
            })?;
            writer.flush().map_err(|error| {
                ProviderError::Subprocess(format!("failed to flush forkserver command: {error}"))
            })
        }

        pub fn pid(&self) -> libc::pid_t {
            self.pid
        }
    }

    #[derive(Debug)]
    pub struct ProviderRequestHandle {
        request_id: String,
        result_rx: mpsc::Receiver<Result<ChatMessage, ProviderError>>,
    }

    impl ProviderRequestHandle {
        pub fn abort_handle(&self) -> ProviderRequestAbortHandle {
            ProviderRequestAbortHandle {
                request_id: self.request_id.clone(),
            }
        }

        pub fn wait(self) -> Result<ChatMessage, ProviderError> {
            self.result_rx.recv().map_err(|_| {
                ProviderError::Subprocess("provider request runtime disconnected".to_string())
            })?
        }
    }

    #[derive(Debug, Clone)]
    pub struct ProviderRequestAbortHandle {
        request_id: String,
    }

    impl ProviderRequestAbortHandle {
        pub fn abort(&self) -> Result<(), ProviderError> {
            global_provider_fork_server()?.abort(&self.request_id)
        }
    }

    #[derive(Debug, Clone, Serialize, Deserialize)]
    pub struct ProviderRequestOwned {
        pub system_prompt: Option<String>,
        pub messages: Vec<ChatMessage>,
        pub tools: Vec<ToolDefinition>,
    }

    impl ProviderRequestOwned {
        pub fn new(messages: Vec<ChatMessage>) -> Self {
            Self {
                system_prompt: None,
                messages,
                tools: Vec::new(),
            }
        }

        pub fn from_provider_request(request: &ProviderRequest<'_>) -> Self {
            Self {
                system_prompt: request.system_prompt.map(str::to_string),
                messages: request.messages.to_vec(),
                tools: request.tools.iter().map(|tool| (*tool).clone()).collect(),
            }
        }

        fn as_provider_request(&self) -> ProviderRequest<'_> {
            ProviderRequest {
                system_prompt: self.system_prompt.as_deref(),
                messages: &self.messages,
                tools: self.tools.iter().collect(),
            }
        }
    }

    #[derive(Debug, Serialize, Deserialize)]
    #[serde(tag = "type", rename_all = "snake_case")]
    enum ForkServerCommand {
        Start {
            request_id: String,
            model_config: ModelConfig,
            request: ProviderRequestOwned,
        },
        Cancel {
            request_id: String,
        },
        Shutdown,
    }

    #[derive(Debug, Serialize, Deserialize)]
    #[serde(tag = "type", rename_all = "snake_case")]
    enum ForkServerEvent {
        Completed {
            request_id: String,
            result: Result<ChatMessage, String>,
        },
    }

    #[derive(Debug)]
    struct RunningRequest {
        request_id: String,
        pid: libc::pid_t,
        result_fd: RawFd,
        buffer: Vec<u8>,
        cancelled: bool,
    }

    fn spawn_parent_event_reader(
        reader: UnixStream,
        pending: Arc<Mutex<HashMap<String, mpsc::Sender<Result<ChatMessage, ProviderError>>>>>,
    ) {
        thread::spawn(move || {
            let mut reader = BufReader::new(reader);
            loop {
                let mut line = String::new();
                match reader.read_line(&mut line) {
                    Ok(0) => break,
                    Ok(_) => match serde_json::from_str::<ForkServerEvent>(&line) {
                        Ok(ForkServerEvent::Completed { request_id, result }) => {
                            if let Some(sender) =
                                pending.lock().expect("mutex poisoned").remove(&request_id)
                            {
                                let result = result.map_err(ProviderError::Subprocess);
                                let _ = sender.send(result);
                            }
                        }
                        Err(error) => fail_all_pending(
                            &pending,
                            format!("failed to decode forkserver event: {error}"),
                        ),
                    },
                    Err(error) => {
                        fail_all_pending(
                            &pending,
                            format!("failed to read forkserver event: {error}"),
                        );
                        break;
                    }
                }
            }

            fail_all_pending(&pending, "provider request runtime closed".to_string());
        });
    }

    fn fail_all_pending(
        pending: &Arc<Mutex<HashMap<String, mpsc::Sender<Result<ChatMessage, ProviderError>>>>>,
        error: String,
    ) {
        let mut pending = pending.lock().expect("mutex poisoned");
        for (_, sender) in pending.drain() {
            let _ = sender.send(Err(ProviderError::Subprocess(error.clone())));
        }
    }

    fn run_forkserver_process(mut stream: UnixStream) {
        let command_fd = stream.as_raw_fd();
        let mut command_buffer = Vec::new();
        let mut running = Vec::<RunningRequest>::new();

        loop {
            let mut poll_fds = Vec::with_capacity(1 + running.len());
            let polled_running_len = running.len();
            poll_fds.push(libc::pollfd {
                fd: command_fd,
                events: libc::POLLIN,
                revents: 0,
            });
            for request in &running {
                poll_fds.push(libc::pollfd {
                    fd: request.result_fd,
                    events: libc::POLLIN | libc::POLLHUP,
                    revents: 0,
                });
            }

            let poll_result =
                unsafe { libc::poll(poll_fds.as_mut_ptr(), poll_fds.len() as libc::nfds_t, -1) };
            if poll_result < 0 {
                continue;
            }

            if poll_fds[0].revents & libc::POLLIN != 0 {
                match read_available(command_fd, &mut command_buffer) {
                    ReadStatus::Open => {
                        while let Some(line) = take_line(&mut command_buffer) {
                            if !handle_forkserver_command(&line, &mut stream, &mut running) {
                                return;
                            }
                        }
                    }
                    ReadStatus::Closed => return,
                }
            }

            let mut completed_indexes = Vec::new();
            for index in 0..polled_running_len {
                let revents = poll_fds[index + 1].revents;
                if revents & (libc::POLLIN | libc::POLLHUP) == 0 {
                    continue;
                }
                let status = read_available(running[index].result_fd, &mut running[index].buffer);
                if matches!(status, ReadStatus::Closed) {
                    completed_indexes.push(index);
                }
            }

            for index in completed_indexes.into_iter().rev() {
                let request = running.swap_remove(index);
                complete_running_request(&mut stream, request);
            }
        }
    }

    fn handle_forkserver_command(
        line: &[u8],
        stream: &mut UnixStream,
        running: &mut Vec<RunningRequest>,
    ) -> bool {
        let command = match serde_json::from_slice::<ForkServerCommand>(line) {
            Ok(command) => command,
            Err(error) => {
                let _ = write_event(
                    stream,
                    ForkServerEvent::Completed {
                        request_id: "invalid".to_string(),
                        result: Err(format!("invalid forkserver command: {error}")),
                    },
                );
                return true;
            }
        };

        match command {
            ForkServerCommand::Start {
                request_id,
                model_config,
                request,
            } => start_request_child(request_id, model_config, request, running),
            ForkServerCommand::Cancel { request_id } => {
                cancel_running_request(&request_id, running);
            }
            ForkServerCommand::Shutdown => return false,
        }

        true
    }

    fn start_request_child(
        request_id: String,
        model_config: ModelConfig,
        request: ProviderRequestOwned,
        running: &mut Vec<RunningRequest>,
    ) {
        let mut pipe_fds = [0; 2];
        if unsafe { libc::pipe(pipe_fds.as_mut_ptr()) } != 0 {
            return;
        }

        let pid = unsafe { libc::fork() };
        if pid < 0 {
            unsafe {
                libc::close(pipe_fds[0]);
                libc::close(pipe_fds[1]);
            }
            return;
        }

        if pid == 0 {
            unsafe {
                libc::close(pipe_fds[0]);
            }
            run_request_child(pipe_fds[1], model_config, request);
            unsafe {
                libc::_exit(0);
            }
        }

        unsafe {
            libc::close(pipe_fds[1]);
        }
        running.push(RunningRequest {
            request_id,
            pid,
            result_fd: pipe_fds[0],
            buffer: Vec::new(),
            cancelled: false,
        });
    }

    fn cancel_running_request(request_id: &str, running: &mut [RunningRequest]) {
        if let Some(request) = running
            .iter_mut()
            .find(|request| request.request_id == request_id)
        {
            request.cancelled = true;
            unsafe {
                libc::kill(request.pid, libc::SIGKILL);
            }
        }
    }

    fn run_request_child(
        result_fd: RawFd,
        model_config: ModelConfig,
        request: ProviderRequestOwned,
    ) {
        let mut writer = unsafe { std::fs::File::from_raw_fd(result_fd) };
        let provider = provider_from_model_config(&model_config);
        let result = provider
            .send(&model_config, request.as_provider_request())
            .map_err(|error| error.to_string());
        let event = ForkServerEvent::Completed {
            request_id: String::new(),
            result,
        };
        let _ = serde_json::to_writer(&mut writer, &event);
        let _ = writer.write_all(b"\n");
        let _ = writer.flush();
    }

    fn complete_running_request(stream: &mut UnixStream, request: RunningRequest) {
        unsafe {
            libc::close(request.result_fd);
        }
        let mut status = 0;
        unsafe {
            libc::waitpid(request.pid, &mut status, 0);
        }

        if request.cancelled {
            let _ = write_event(
                stream,
                ForkServerEvent::Completed {
                    request_id: request.request_id,
                    result: Err("provider request cancelled".to_string()),
                },
            );
            return;
        }

        let Some(line) = first_line(&request.buffer) else {
            let _ = write_event(
                stream,
                ForkServerEvent::Completed {
                    request_id: request.request_id,
                    result: Err("provider request child exited without a response".to_string()),
                },
            );
            return;
        };

        let child_event = match serde_json::from_slice::<ForkServerEvent>(line) {
            Ok(ForkServerEvent::Completed { result, .. }) => ForkServerEvent::Completed {
                request_id: request.request_id,
                result,
            },
            Err(error) => ForkServerEvent::Completed {
                request_id: request.request_id,
                result: Err(format!(
                    "failed to decode provider request child response: {error}"
                )),
            },
        };
        let _ = write_event(stream, child_event);
    }

    fn write_event(stream: &mut UnixStream, event: ForkServerEvent) -> std::io::Result<()> {
        serde_json::to_writer(&mut *stream, &event)?;
        stream.write_all(b"\n")?;
        stream.flush()
    }

    enum ReadStatus {
        Open,
        Closed,
    }

    fn read_available(fd: RawFd, buffer: &mut Vec<u8>) -> ReadStatus {
        let mut chunk = [0_u8; 8192];
        loop {
            let read = unsafe { libc::read(fd, chunk.as_mut_ptr().cast(), chunk.len()) };
            if read == 0 {
                return ReadStatus::Closed;
            }
            if read < 0 {
                return ReadStatus::Open;
            }
            buffer.extend_from_slice(&chunk[..read as usize]);
            if read < chunk.len() as isize {
                return ReadStatus::Open;
            }
        }
    }

    fn take_line(buffer: &mut Vec<u8>) -> Option<Vec<u8>> {
        let position = buffer.iter().position(|byte| *byte == b'\n')?;
        let mut line = buffer.drain(..=position).collect::<Vec<_>>();
        if line.last() == Some(&b'\n') {
            line.pop();
        }
        Some(line)
    }

    fn first_line(buffer: &[u8]) -> Option<&[u8]> {
        let end = buffer.iter().position(|byte| *byte == b'\n')?;
        Some(&buffer[..end])
    }
}

#[cfg(unix)]
pub use unix_impl::*;

#[cfg(not(unix))]
mod portable_impl {
    use std::{
        collections::HashMap,
        process,
        sync::{
            atomic::{AtomicBool, AtomicU64, Ordering},
            mpsc, Arc, Mutex, OnceLock,
        },
        thread,
    };

    use serde::{Deserialize, Serialize};

    use crate::{
        model_config::ModelConfig,
        providers::{provider_from_model_config, Provider, ProviderError, ProviderRequest},
        session_actor::{ChatMessage, ToolDefinition},
    };

    static FORK_SERVER: OnceLock<Arc<ProviderRequestForkServer>> = OnceLock::new();

    #[derive(Debug, Clone)]
    pub struct ForkServerProvider {
        fork_server: Arc<ProviderRequestForkServer>,
    }

    impl ForkServerProvider {
        pub fn global() -> Result<Self, ProviderError> {
            Ok(Self {
                fork_server: global_provider_fork_server()?,
            })
        }
    }

    impl Provider for ForkServerProvider {
        fn send(
            &self,
            model_config: &ModelConfig,
            request: ProviderRequest<'_>,
        ) -> Result<ChatMessage, ProviderError> {
            self.fork_server
                .start(
                    model_config.clone(),
                    ProviderRequestOwned::from_provider_request(&request),
                )?
                .wait()
        }
    }

    pub fn init_global_provider_fork_server(
    ) -> Result<Arc<ProviderRequestForkServer>, ProviderError> {
        if let Some(fork_server) = FORK_SERVER.get() {
            return Ok(fork_server.clone());
        }

        let fork_server = Arc::new(ProviderRequestForkServer::spawn()?);
        let _ = FORK_SERVER.set(fork_server.clone());
        Ok(FORK_SERVER.get().expect("fork server initialized").clone())
    }

    pub fn global_provider_fork_server() -> Result<Arc<ProviderRequestForkServer>, ProviderError> {
        FORK_SERVER.get().cloned().ok_or_else(|| {
            ProviderError::Subprocess("provider request runtime is not initialized".to_string())
        })
    }

    #[derive(Debug)]
    pub struct ProviderRequestForkServer {
        pending: Arc<Mutex<HashMap<String, RunningRequest>>>,
        next_request_id: AtomicU64,
    }

    #[derive(Debug)]
    struct RunningRequest {
        cancelled: Arc<AtomicBool>,
        result_tx: mpsc::Sender<Result<ChatMessage, ProviderError>>,
    }

    impl ProviderRequestForkServer {
        fn spawn() -> Result<Self, ProviderError> {
            Ok(Self {
                pending: Arc::new(Mutex::new(HashMap::new())),
                next_request_id: AtomicU64::new(1),
            })
        }

        pub fn start(
            &self,
            model_config: ModelConfig,
            request: ProviderRequestOwned,
        ) -> Result<ProviderRequestHandle, ProviderError> {
            let request_id = format!(
                "provider_request_{}_{}",
                process::id(),
                self.next_request_id.fetch_add(1, Ordering::SeqCst)
            );
            let (result_tx, result_rx) = mpsc::channel();
            let cancelled = Arc::new(AtomicBool::new(false));
            self.pending.lock().expect("mutex poisoned").insert(
                request_id.clone(),
                RunningRequest {
                    cancelled: cancelled.clone(),
                    result_tx,
                },
            );

            let pending = self.pending.clone();
            let thread_request_id = request_id.clone();
            thread::spawn(move || {
                let provider = provider_from_model_config(&model_config);
                let result = provider.send(&model_config, request.as_provider_request());
                let Some(running) = pending
                    .lock()
                    .expect("mutex poisoned")
                    .remove(&thread_request_id)
                else {
                    return;
                };
                if !cancelled.load(Ordering::SeqCst) {
                    let _ = running.result_tx.send(result);
                }
            });

            Ok(ProviderRequestHandle {
                request_id,
                result_rx,
            })
        }

        pub fn abort(&self, request_id: &str) -> Result<(), ProviderError> {
            if let Some(running) = self
                .pending
                .lock()
                .expect("mutex poisoned")
                .remove(request_id)
            {
                running.cancelled.store(true, Ordering::SeqCst);
                let _ = running.result_tx.send(Err(ProviderError::Subprocess(
                    "provider request cancelled".to_string(),
                )));
            }
            Ok(())
        }
    }

    #[derive(Debug)]
    pub struct ProviderRequestHandle {
        request_id: String,
        result_rx: mpsc::Receiver<Result<ChatMessage, ProviderError>>,
    }

    impl ProviderRequestHandle {
        pub fn abort_handle(&self) -> ProviderRequestAbortHandle {
            ProviderRequestAbortHandle {
                request_id: self.request_id.clone(),
            }
        }

        pub fn wait(self) -> Result<ChatMessage, ProviderError> {
            self.result_rx.recv().map_err(|_| {
                ProviderError::Subprocess("provider request runtime disconnected".to_string())
            })?
        }
    }

    #[derive(Debug, Clone)]
    pub struct ProviderRequestAbortHandle {
        request_id: String,
    }

    impl ProviderRequestAbortHandle {
        pub fn abort(&self) -> Result<(), ProviderError> {
            global_provider_fork_server()?.abort(&self.request_id)
        }
    }

    #[derive(Debug, Clone, Serialize, Deserialize)]
    pub struct ProviderRequestOwned {
        pub system_prompt: Option<String>,
        pub messages: Vec<ChatMessage>,
        pub tools: Vec<ToolDefinition>,
    }

    impl ProviderRequestOwned {
        pub fn new(messages: Vec<ChatMessage>) -> Self {
            Self {
                system_prompt: None,
                messages,
                tools: Vec::new(),
            }
        }

        pub fn from_provider_request(request: &ProviderRequest<'_>) -> Self {
            Self {
                system_prompt: request.system_prompt.map(str::to_string),
                messages: request.messages.to_vec(),
                tools: request.tools.iter().map(|tool| (*tool).clone()).collect(),
            }
        }

        fn as_provider_request(&self) -> ProviderRequest<'_> {
            ProviderRequest {
                system_prompt: self.system_prompt.as_deref(),
                messages: &self.messages,
                tools: self.tools.iter().collect(),
            }
        }
    }
}

#[cfg(not(unix))]
pub use portable_impl::*;
