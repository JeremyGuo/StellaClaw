use super::*;

pub(super) struct IncomingDispatcher {
    server: Arc<Server>,
    conversation_workers:
        Arc<Mutex<HashMap<String, tokio::sync::mpsc::UnboundedSender<IncomingMessage>>>>,
    active_worker_count: Arc<AtomicUsize>,
    active_worker_notify: Arc<Notify>,
}

impl IncomingDispatcher {
    pub(super) fn new(server: Arc<Server>) -> Self {
        Self {
            server,
            conversation_workers: Arc::new(Mutex::new(HashMap::new())),
            active_worker_count: Arc::new(AtomicUsize::new(0)),
            active_worker_notify: Arc::new(Notify::new()),
        }
    }

    pub(super) fn has_active_workers(&self) -> bool {
        self.active_worker_count.load(Ordering::SeqCst) > 0
    }

    pub(super) async fn wait_for_worker_change(&self) {
        self.active_worker_notify.notified().await;
    }

    pub(super) async fn dispatch(&self, message: IncomingMessage) -> Result<()> {
        if self.try_send_fast_path_agent_selection(&message).await? {
            return Ok(());
        }

        let command_lane = incoming_command_lane(message.text.as_deref());
        if matches!(command_lane, Some(IncomingCommandLane::Immediate)) {
            self.spawn_immediate_command(message);
            return Ok(());
        }

        let message = if matches!(command_lane, Some(IncomingCommandLane::ConversationWorker)) {
            message
        } else {
            let Some(message) = self.prepare_regular_message(message).await? else {
                return Ok(());
            };
            message
        };
        self.enqueue_conversation_message(message)
    }

    async fn try_send_fast_path_agent_selection(&self, message: &IncomingMessage) -> Result<bool> {
        if !self
            .server
            .allows_fast_path_agent_selection(&message.address)?
        {
            return Ok(false);
        }
        let Some(outgoing) = fast_path_agent_selection_message(
            &self.server.workdir,
            &self.server.models,
            &self.server.agent,
            message,
        ) else {
            return Ok(false);
        };
        if let Some(channel) = self.server.channels.get(&message.address.channel_id)
            && let Err(error) = channel.send(&message.address, outgoing).await
        {
            error!(
                log_stream = "channel",
                log_key = %message.address.channel_id,
                kind = "fast_path_send_failed",
                conversation_id = %message.address.conversation_id,
                error = %format!("{error:#}"),
                "failed to send fast-path model selection message"
            );
        }
        Ok(true)
    }

    fn spawn_immediate_command(&self, message: IncomingMessage) {
        let server = Arc::clone(&self.server);
        tokio::spawn(async move {
            if let Err(error) = server.handle_incoming(message).await {
                error!(
                    log_stream = "server",
                    kind = "handle_out_of_band_command_failed",
                    error = %format!("{error:#}"),
                    "failed to handle out-of-band command"
                );
            }
        });
    }

    async fn prepare_regular_message(
        &self,
        message: IncomingMessage,
    ) -> Result<Option<IncomingMessage>> {
        let interrupted_followup = request_yield_for_incoming(
            &self.server.active_foreground_controls,
            &self.server.active_foreground_phases,
            &message,
        );
        if interrupted_followup.compaction_in_progress
            && let Some(channel) = self.server.channels.get(&message.address.channel_id)
            && let Err(error) = channel
                .send(
                    &message.address,
                    OutgoingMessage::text(
                        "正在压缩上下文，可能要等待压缩完毕后才能回复。".to_string(),
                    ),
                )
                .await
        {
            error!(
                log_stream = "channel",
                log_key = %message.address.channel_id,
                kind = "compaction_wait_notice_send_failed",
                conversation_id = %message.address.conversation_id,
                error = %format!("{error:#}"),
                "failed to send compaction wait notice"
            );
        }
        if interrupted_followup.interrupted {
            let mut message = message;
            message.text = tag_interrupted_followup_text(message.text);
            if let Ok(mut interrupts) = self.server.pending_foreground_interrupts.lock() {
                interrupts.insert(message.address.session_key());
            }
            Ok(Some(message))
        } else {
            Ok(Some(message))
        }
    }

    fn enqueue_conversation_message(&self, message: IncomingMessage) -> Result<()> {
        let session_key = message.address.session_key();
        let mut pending_message = Some(message);
        loop {
            let worker_sender = self
                .conversation_workers
                .lock()
                .map_err(|_| anyhow!("conversation workers lock poisoned"))?
                .get(&session_key)
                .cloned();
            let worker_sender = match worker_sender {
                Some(worker_sender) => worker_sender,
                None => self.spawn_conversation_worker(&session_key)?,
            };
            let message = pending_message
                .take()
                .expect("pending message should exist while dispatching");
            match worker_sender.send(message) {
                Ok(()) => break,
                Err(error) => {
                    if let Ok(mut workers) = self.conversation_workers.lock() {
                        workers.remove(&session_key);
                    }
                    pending_message = Some(error.0);
                }
            }
        }
        Ok(())
    }

    fn spawn_conversation_worker(
        &self,
        session_key: &str,
    ) -> Result<tokio::sync::mpsc::UnboundedSender<IncomingMessage>> {
        let (worker_tx, mut worker_rx) = tokio::sync::mpsc::unbounded_channel();
        self.conversation_workers
            .lock()
            .map_err(|_| anyhow!("conversation workers lock poisoned"))?
            .insert(session_key.to_string(), worker_tx.clone());
        self.active_worker_count.fetch_add(1, Ordering::SeqCst);

        let server = Arc::clone(&self.server);
        let conversation_workers = Arc::clone(&self.conversation_workers);
        let active_worker_count = Arc::clone(&self.active_worker_count);
        let active_worker_notify = Arc::clone(&self.active_worker_notify);
        let worker_session_key = session_key.to_string();
        tokio::spawn(async move {
            let mut local_queue = VecDeque::new();
            while let Some(message) = worker_rx.recv().await {
                local_queue.push_back(message);
                while let Ok(message) = worker_rx.try_recv() {
                    local_queue.push_back(message);
                }
                while let Some(message) = local_queue.pop_front() {
                    let merged = coalesce_buffered_conversation_messages(message, &mut local_queue);
                    if let Err(error) = server.handle_incoming(merged).await {
                        error!(
                            log_stream = "server",
                            kind = "handle_incoming_failed",
                            error = %format!("{error:#}"),
                            "failed to handle incoming message"
                        );
                    }
                    while let Ok(message) = worker_rx.try_recv() {
                        local_queue.push_back(message);
                    }
                }
            }
            if let Ok(mut workers) = conversation_workers.lock() {
                workers.remove(&worker_session_key);
            }
            active_worker_count.fetch_sub(1, Ordering::SeqCst);
            active_worker_notify.notify_waiters();
        });
        Ok(worker_tx)
    }
}
