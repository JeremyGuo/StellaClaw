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

    use crate::providers::{
        provider_from_model_config, send_provider_request_with_retry, Provider, ProviderError,
        ProviderErrorReport, ProviderRequest,
    };

    static FORK_SERVER: OnceLock<Arc<ProviderRequestForkServer>> = OnceLock::new();

    #[derive(Debug, Clone)]
    pub struct ForkServerProvider {
        model_config: ModelConfig,
        fork_server: Arc<ProviderRequestForkServer>,
        worker: Arc<Mutex<Option<ProviderWorkerBinding>>>,
    }

    impl ForkServerProvider {
        pub fn global(model_config: ModelConfig) -> Result<Self, ProviderError> {
            Ok(Self {
                model_config,
                fork_server: global_provider_fork_server()?,
                worker: Arc::new(Mutex::new(None)),
            })
        }

        fn ensure_worker(&self) -> Result<String, ProviderError> {
            let signature = provider_worker_signature(&self.model_config);
            let mut worker = self.worker.lock().expect("mutex poisoned");
            if let Some(binding) = worker.as_ref() {
                if binding.signature == signature {
                    return Ok(binding.worker_id.clone());
                }
                let _ = self.fork_server.shutdown_worker(&binding.worker_id);
            }

            let worker_id = self.fork_server.start_worker(self.model_config.clone())?;
            *worker = Some(ProviderWorkerBinding {
                worker_id: worker_id.clone(),
                signature,
            });
            Ok(worker_id)
        }

        fn clear_worker(&self, worker_id: &str) {
            let mut worker = self.worker.lock().expect("mutex poisoned");
            if worker
                .as_ref()
                .is_some_and(|binding| binding.worker_id == worker_id)
            {
                *worker = None;
            }
        }
    }

    impl Provider for ForkServerProvider {
        fn model_config(&self) -> &ModelConfig {
            &self.model_config
        }

        fn send(&self, request: ProviderRequest<'_>) -> Result<ChatMessage, ProviderError> {
            let request = ProviderRequestOwned::from_provider_request(&request);
            let mut retried_worker = false;
            loop {
                let worker_id = self.ensure_worker()?;
                let result = self
                    .fork_server
                    .start_on_worker(worker_id.clone(), request.clone())?
                    .wait();
                if should_recreate_provider_worker(&result) && !retried_worker {
                    retried_worker = true;
                    self.clear_worker(&worker_id);
                    continue;
                }
                return result;
            }
        }
    }

    impl Drop for ForkServerProvider {
        fn drop(&mut self) {
            if let Ok(worker) = self.worker.lock() {
                if let Some(binding) = worker.as_ref() {
                    let _ = self.fork_server.shutdown_worker(&binding.worker_id);
                }
            }
        }
    }

    #[derive(Debug, Clone)]
    struct ProviderWorkerBinding {
        worker_id: String,
        signature: String,
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
        next_worker_id: AtomicU64,
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
                next_worker_id: AtomicU64::new(1),
            })
        }

        pub fn start_worker(&self, model_config: ModelConfig) -> Result<String, ProviderError> {
            let worker_id = format!(
                "provider_worker_{}_{}",
                process::id(),
                self.next_worker_id.fetch_add(1, Ordering::SeqCst)
            );
            self.send_command(ForkServerCommand::StartWorker {
                worker_id: worker_id.clone(),
                model_config,
                temporary: false,
            })?;
            Ok(worker_id)
        }

        pub fn shutdown_worker(&self, worker_id: &str) -> Result<(), ProviderError> {
            self.send_command(ForkServerCommand::ShutdownWorker {
                worker_id: worker_id.to_string(),
            })
        }

        pub fn start_on_worker(
            &self,
            worker_id: String,
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

            if let Err(error) = self.send_command(ForkServerCommand::StartOnWorker {
                request_id: request_id.clone(),
                worker_id,
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

            let worker_id = format!(
                "provider_worker_{}_one_shot_{}",
                process::id(),
                self.next_worker_id.fetch_add(1, Ordering::SeqCst)
            );
            let start_request = self
                .send_command(ForkServerCommand::StartWorker {
                    worker_id: worker_id.clone(),
                    model_config,
                    temporary: true,
                })
                .and_then(|_| {
                    self.send_command(ForkServerCommand::StartOnWorker {
                        request_id: request_id.clone(),
                        worker_id,
                        request,
                    })
                });

            if let Err(error) = start_request {
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
        StartWorker {
            worker_id: String,
            model_config: ModelConfig,
            temporary: bool,
        },
        StartOnWorker {
            request_id: String,
            worker_id: String,
            request: ProviderRequestOwned,
        },
        Cancel {
            request_id: String,
        },
        ShutdownWorker {
            worker_id: String,
        },
        Shutdown,
    }

    #[derive(Debug, Serialize, Deserialize)]
    #[serde(tag = "type", rename_all = "snake_case")]
    enum ForkServerEvent {
        Completed {
            request_id: String,
            result: Result<ChatMessage, ProviderErrorReport>,
        },
    }

    #[derive(Debug, Serialize, Deserialize)]
    #[serde(tag = "type", rename_all = "snake_case")]
    enum ProviderWorkerCommand {
        Start {
            request_id: String,
            request: ProviderRequestOwned,
        },
        Shutdown,
    }

    #[derive(Debug, Serialize, Deserialize)]
    #[serde(tag = "type", rename_all = "snake_case")]
    enum ProviderWorkerEvent {
        Completed {
            request_id: String,
            result: Result<ChatMessage, ProviderErrorReport>,
        },
    }

    #[derive(Debug)]
    struct ProviderWorkerProcess {
        worker_id: String,
        pid: libc::pid_t,
        command_fd: RawFd,
        result_fd: RawFd,
        buffer: Vec<u8>,
        active_request_id: Option<String>,
        temporary: bool,
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
                                let result =
                                    result.map_err(ProviderErrorReport::into_provider_error);
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
        let mut workers = Vec::<ProviderWorkerProcess>::new();

        loop {
            let mut poll_fds = Vec::with_capacity(1 + workers.len());
            let polled_worker_len = workers.len();
            poll_fds.push(libc::pollfd {
                fd: command_fd,
                events: libc::POLLIN,
                revents: 0,
            });
            for worker in &workers {
                poll_fds.push(libc::pollfd {
                    fd: worker.result_fd,
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
                            if !handle_forkserver_command(&line, &mut stream, &mut workers) {
                                return;
                            }
                        }
                    }
                    ReadStatus::Closed => return,
                }
            }

            let worker_offset = 1;
            let mut completed_worker_indexes = Vec::new();
            for index in 0..polled_worker_len {
                let revents = poll_fds[worker_offset + index].revents;
                if revents & (libc::POLLIN | libc::POLLHUP) == 0 {
                    continue;
                }
                let status = read_available(workers[index].result_fd, &mut workers[index].buffer);
                let mut remove_after_event = false;
                while let Some(line) = take_line(&mut workers[index].buffer) {
                    if complete_worker_event(&mut stream, &mut workers[index], &line) {
                        remove_after_event = true;
                    }
                }
                if matches!(status, ReadStatus::Closed) || remove_after_event {
                    completed_worker_indexes.push(index);
                }
            }

            for index in completed_worker_indexes.into_iter().rev() {
                let worker = workers.swap_remove(index);
                complete_worker_exit(&mut stream, worker);
            }
        }
    }

    fn handle_forkserver_command(
        line: &[u8],
        stream: &mut UnixStream,
        workers: &mut Vec<ProviderWorkerProcess>,
    ) -> bool {
        let command = match serde_json::from_slice::<ForkServerCommand>(line) {
            Ok(command) => command,
            Err(error) => {
                let _ = write_event(
                    stream,
                    ForkServerEvent::Completed {
                        request_id: "invalid".to_string(),
                        result: Err(subprocess_report(format!(
                            "invalid forkserver command: {error}"
                        ))),
                    },
                );
                return true;
            }
        };

        match command {
            ForkServerCommand::StartWorker {
                worker_id,
                model_config,
                temporary,
            } => start_provider_worker(worker_id, model_config, temporary, workers),
            ForkServerCommand::StartOnWorker {
                request_id,
                worker_id,
                request,
            } => start_request_on_worker(request_id, &worker_id, request, stream, workers),
            ForkServerCommand::Cancel { request_id } => {
                cancel_worker_request(&request_id, stream, workers);
            }
            ForkServerCommand::ShutdownWorker { worker_id } => {
                shutdown_provider_worker(&worker_id, workers);
            }
            ForkServerCommand::Shutdown => return false,
        }

        true
    }

    fn start_provider_worker(
        worker_id: String,
        model_config: ModelConfig,
        temporary: bool,
        workers: &mut Vec<ProviderWorkerProcess>,
    ) {
        if workers.iter().any(|worker| worker.worker_id == worker_id) {
            return;
        }

        let mut command_pipe = [0; 2];
        let mut result_pipe = [0; 2];
        if unsafe { libc::pipe(command_pipe.as_mut_ptr()) } != 0 {
            return;
        }
        if unsafe { libc::pipe(result_pipe.as_mut_ptr()) } != 0 {
            unsafe {
                libc::close(command_pipe[0]);
                libc::close(command_pipe[1]);
            }
            return;
        }

        let pid = unsafe { libc::fork() };
        if pid < 0 {
            unsafe {
                libc::close(command_pipe[0]);
                libc::close(command_pipe[1]);
                libc::close(result_pipe[0]);
                libc::close(result_pipe[1]);
            }
            return;
        }

        if pid == 0 {
            unsafe {
                libc::close(command_pipe[1]);
                libc::close(result_pipe[0]);
            }
            run_provider_worker(command_pipe[0], result_pipe[1], model_config);
            unsafe {
                libc::_exit(0);
            }
        }

        unsafe {
            libc::close(command_pipe[0]);
            libc::close(result_pipe[1]);
        }
        workers.push(ProviderWorkerProcess {
            worker_id,
            pid,
            command_fd: command_pipe[1],
            result_fd: result_pipe[0],
            buffer: Vec::new(),
            active_request_id: None,
            temporary,
        });
    }

    fn start_request_on_worker(
        request_id: String,
        worker_id: &str,
        request: ProviderRequestOwned,
        stream: &mut UnixStream,
        workers: &mut [ProviderWorkerProcess],
    ) {
        let Some(worker) = workers
            .iter_mut()
            .find(|worker| worker.worker_id == worker_id)
        else {
            let _ = write_event(
                stream,
                ForkServerEvent::Completed {
                    request_id,
                    result: Err(subprocess_report(format!(
                        "unknown provider worker {worker_id}"
                    ))),
                },
            );
            return;
        };

        if let Some(active_request_id) = &worker.active_request_id {
            let _ = write_event(
                stream,
                ForkServerEvent::Completed {
                    request_id,
                    result: Err(subprocess_report(format!(
                        "provider worker {worker_id} is busy with {active_request_id}"
                    ))),
                },
            );
            return;
        }

        let command = ProviderWorkerCommand::Start {
            request_id: request_id.clone(),
            request,
        };
        if let Err(error) = write_fd_json_line(worker.command_fd, &command) {
            let _ = write_event(
                stream,
                ForkServerEvent::Completed {
                    request_id,
                    result: Err(subprocess_report(format!(
                        "failed to write provider worker command: {error}"
                    ))),
                },
            );
            return;
        }

        worker.active_request_id = Some(request_id);
    }

    fn cancel_worker_request(
        request_id: &str,
        stream: &mut UnixStream,
        workers: &mut Vec<ProviderWorkerProcess>,
    ) {
        let Some(index) = workers
            .iter()
            .position(|worker| worker.active_request_id.as_deref() == Some(request_id))
        else {
            return;
        };
        let worker = workers.swap_remove(index);
        unsafe {
            libc::kill(worker.pid, libc::SIGKILL);
        }
        close_provider_worker_fds(&worker);
        let _ = write_event(
            stream,
            ForkServerEvent::Completed {
                request_id: request_id.to_string(),
                result: Err(subprocess_report("provider request cancelled")),
            },
        );
    }

    fn shutdown_provider_worker(worker_id: &str, workers: &mut Vec<ProviderWorkerProcess>) {
        let Some(index) = workers
            .iter()
            .position(|worker| worker.worker_id == worker_id)
        else {
            return;
        };
        let worker = workers.swap_remove(index);
        let _ = write_fd_json_line(worker.command_fd, &ProviderWorkerCommand::Shutdown);
        unsafe {
            libc::kill(worker.pid, libc::SIGTERM);
        }
        close_provider_worker_fds(&worker);
    }

    fn complete_worker_event(
        stream: &mut UnixStream,
        worker: &mut ProviderWorkerProcess,
        line: &[u8],
    ) -> bool {
        let event = match serde_json::from_slice::<ProviderWorkerEvent>(line) {
            Ok(ProviderWorkerEvent::Completed { request_id, result }) => {
                worker.active_request_id = None;
                ForkServerEvent::Completed { request_id, result }
            }
            Err(error) => {
                let request_id = worker
                    .active_request_id
                    .take()
                    .unwrap_or_else(|| format!("{}_decode_error", worker.worker_id));
                ForkServerEvent::Completed {
                    request_id,
                    result: Err(subprocess_report(format!(
                        "failed to decode provider worker response: {error}"
                    ))),
                }
            }
        };
        let _ = write_event(stream, event);
        if worker.temporary {
            let _ = write_fd_json_line(worker.command_fd, &ProviderWorkerCommand::Shutdown);
            unsafe {
                libc::kill(worker.pid, libc::SIGTERM);
            }
            return true;
        }
        false
    }

    fn complete_worker_exit(stream: &mut UnixStream, worker: ProviderWorkerProcess) {
        unsafe {
            libc::close(worker.command_fd);
            libc::close(worker.result_fd);
        }
        let mut status = 0;
        unsafe {
            libc::waitpid(worker.pid, &mut status, 0);
        }
        if let Some(request_id) = worker.active_request_id {
            let _ = write_event(
                stream,
                ForkServerEvent::Completed {
                    request_id,
                    result: Err(subprocess_report(format!(
                        "provider worker {} exited before completing request",
                        worker.worker_id
                    ))),
                },
            );
        }
    }

    fn close_provider_worker_fds(worker: &ProviderWorkerProcess) {
        unsafe {
            libc::close(worker.command_fd);
            libc::close(worker.result_fd);
        }
    }

    fn run_provider_worker(command_fd: RawFd, result_fd: RawFd, model_config: ModelConfig) {
        let command_stream = unsafe { std::fs::File::from_raw_fd(command_fd) };
        let mut writer = unsafe { std::fs::File::from_raw_fd(result_fd) };
        let mut reader = BufReader::new(command_stream);
        let provider = provider_from_model_config(model_config);

        loop {
            let mut line = String::new();
            match reader.read_line(&mut line) {
                Ok(0) => break,
                Ok(_) => {}
                Err(error) => {
                    let _ = write_worker_event(
                        &mut writer,
                        ProviderWorkerEvent::Completed {
                            request_id: "worker_read_error".to_string(),
                            result: Err(subprocess_report(format!(
                                "failed to read provider worker command: {error}"
                            ))),
                        },
                    );
                    break;
                }
            }

            let command = match serde_json::from_str::<ProviderWorkerCommand>(&line) {
                Ok(command) => command,
                Err(error) => {
                    let _ = write_worker_event(
                        &mut writer,
                        ProviderWorkerEvent::Completed {
                            request_id: "invalid_worker_command".to_string(),
                            result: Err(subprocess_report(format!(
                                "invalid provider worker command: {error}"
                            ))),
                        },
                    );
                    continue;
                }
            };

            match command {
                ProviderWorkerCommand::Start {
                    request_id,
                    request,
                } => {
                    let result = send_provider_request_with_retry(
                        provider.as_ref(),
                        request.as_provider_request(),
                        |_| {},
                    )
                    .map_err(ProviderErrorReport::from_provider_error);
                    let _ = write_worker_event(
                        &mut writer,
                        ProviderWorkerEvent::Completed { request_id, result },
                    );
                }
                ProviderWorkerCommand::Shutdown => break,
            }
        }
    }

    fn write_event(stream: &mut UnixStream, event: ForkServerEvent) -> std::io::Result<()> {
        serde_json::to_writer(&mut *stream, &event)?;
        stream.write_all(b"\n")?;
        stream.flush()
    }

    fn write_worker_event(
        writer: &mut std::fs::File,
        event: ProviderWorkerEvent,
    ) -> std::io::Result<()> {
        serde_json::to_writer(&mut *writer, &event)?;
        writer.write_all(b"\n")?;
        writer.flush()
    }

    fn write_fd_json_line<T: Serialize>(fd: RawFd, value: &T) -> std::io::Result<()> {
        let rendered = serde_json::to_vec(value)?;
        let mut written = 0usize;
        while written < rendered.len() {
            let result = unsafe {
                libc::write(
                    fd,
                    rendered[written..].as_ptr().cast(),
                    rendered.len() - written,
                )
            };
            if result < 0 {
                return Err(std::io::Error::last_os_error());
            }
            written += result as usize;
        }
        let newline = [b'\n'];
        let result = unsafe { libc::write(fd, newline.as_ptr().cast(), newline.len()) };
        if result < 0 {
            return Err(std::io::Error::last_os_error());
        }
        Ok(())
    }

    fn subprocess_report(message: impl Into<String>) -> ProviderErrorReport {
        ProviderErrorReport::Subprocess {
            message: message.into(),
        }
    }

    fn provider_worker_signature(model_config: &ModelConfig) -> String {
        serde_json::to_string(model_config).unwrap_or_else(|_| {
            format!(
                "{:?}:{}:{}",
                model_config.provider_type, model_config.model_name, model_config.url
            )
        })
    }

    fn should_recreate_provider_worker(result: &Result<ChatMessage, ProviderError>) -> bool {
        matches!(
            result,
            Err(ProviderError::Subprocess(message))
                if message.contains("unknown provider worker")
                    || message.contains("provider worker")
                        && message.contains("exited before completing request")
        )
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
        model_config: ModelConfig,
        fork_server: Arc<ProviderRequestForkServer>,
    }

    impl ForkServerProvider {
        pub fn global(model_config: ModelConfig) -> Result<Self, ProviderError> {
            Ok(Self {
                model_config,
                fork_server: global_provider_fork_server()?,
            })
        }
    }

    impl Provider for ForkServerProvider {
        fn model_config(&self) -> &ModelConfig {
            &self.model_config
        }

        fn send(&self, request: ProviderRequest<'_>) -> Result<ChatMessage, ProviderError> {
            self.fork_server
                .start(
                    self.model_config.clone(),
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
                let provider = provider_from_model_config(model_config);
                let result = send_provider_request_with_retry(
                    provider.as_ref(),
                    request.as_provider_request(),
                    |_| {},
                );
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
