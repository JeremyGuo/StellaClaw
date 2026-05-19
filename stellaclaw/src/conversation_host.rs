#![allow(dead_code)]

use std::{
    collections::HashMap,
    path::PathBuf,
    sync::{Arc, Mutex},
    thread,
};

use anyhow::{anyhow, Context, Result};
use crossbeam_channel::{Receiver, Sender};

use crate::{
    config::StellaclawConfig,
    conversation_metadata::{ConversationMetadata, ConversationMetadataStore, WorkdirLayout},
    conversation_new::{
        ChannelServiceEndpoint, ConversationKernel, ConversationKernelHandle, ConversationRef,
        ConversationRuntimeConfig, ServiceAddr, ServiceRefs,
    },
    logger::StellaclawLogger,
    tool_binary_manager::shared_tool_binary_client,
};

pub struct ConversationHostRuntime {
    workdir: PathBuf,
    default_runtime_config: ConversationRuntimeConfig,
    logger: Arc<StellaclawLogger>,
    conversations: Mutex<HashMap<String, HostedConversation>>,
}

pub struct HostedConversation {
    pub handle: ConversationKernelHandle,
    pub main_channel_ingress_tx: Sender<crate::service_protos::channel::ChannelIngress>,
    pub main_channel_event_subscribers:
        Arc<Mutex<Vec<Sender<crate::service_protos::channel::ChannelEvent>>>>,
}

impl ConversationHostRuntime {
    pub fn start_existing(
        workdir: PathBuf,
        config: Arc<StellaclawConfig>,
        agent_server_path: PathBuf,
        logger: Arc<StellaclawLogger>,
    ) -> Result<Self> {
        start_global_services();
        let default_runtime_config = runtime_config_from_host_defaults(&config, agent_server_path)?;
        let runtime = Self {
            workdir,
            default_runtime_config,
            logger,
            conversations: Mutex::new(HashMap::new()),
        };

        let store = ConversationMetadataStore::new(&runtime.workdir);
        for metadata_path in store.list_metadata_paths()? {
            let metadata = read_conversation_metadata(&metadata_path)?;
            runtime.ensure_conversation_started(&metadata.conversation_id)?;
        }

        runtime.logger.info(
            "conversation_kernels_started",
            serde_json::json!({ "count": runtime.conversation_count() }),
        );
        Ok(runtime)
    }

    pub fn conversation_count(&self) -> usize {
        self.conversations
            .lock()
            .map(|conversations| conversations.len())
            .unwrap_or(0)
    }

    pub fn conversation_ids(&self) -> Vec<String> {
        let Ok(conversations) = self.conversations.lock() else {
            return Vec::new();
        };
        conversations.keys().cloned().collect()
    }

    pub fn ensure_conversation_started(&self, conversation_id: &str) -> Result<()> {
        if self
            .conversations
            .lock()
            .map_err(|_| anyhow!("conversation registry lock poisoned"))?
            .contains_key(conversation_id)
        {
            return Ok(());
        }

        let layout = WorkdirLayout::new(&self.workdir);
        let conversation_root = layout.conversation_root(conversation_id);
        let (main_channel_ingress_tx, main_channel_ingress_rx) = crossbeam_channel::unbounded();
        let (main_channel_event_tx, main_channel_event_rx) = crossbeam_channel::unbounded();
        let main_channel_event_subscribers = Arc::new(Mutex::new(Vec::new()));
        spawn_channel_event_fanout(
            conversation_id.to_string(),
            main_channel_event_rx,
            main_channel_event_subscribers.clone(),
            self.logger.clone(),
        );
        let refs = ServiceRefs::default().with_channel_endpoint(
            ServiceAddr::channel(),
            ChannelServiceEndpoint {
                ingress_rx: main_channel_ingress_rx,
                event_tx: main_channel_event_tx,
            },
        );
        let conversation = ConversationRef {
            conversation_id: conversation_id.to_string(),
            workdir: self.workdir.clone(),
            conversation_root,
        };
        let kernel = ConversationKernel::open_or_bootstrap(
            conversation,
            refs,
            self.default_runtime_config.clone(),
        )
        .with_context(|| format!("failed to open conversation kernel {conversation_id}"))?;
        let handle = kernel
            .spawn()
            .with_context(|| format!("failed to spawn conversation kernel {conversation_id}"))?;

        self.conversations
            .lock()
            .map_err(|_| anyhow!("conversation registry lock poisoned"))?
            .insert(
                conversation_id.to_string(),
                HostedConversation {
                    handle,
                    main_channel_ingress_tx,
                    main_channel_event_subscribers,
                },
            );
        self.logger.info(
            "conversation_kernel_started",
            serde_json::json!({ "conversation_id": conversation_id }),
        );
        Ok(())
    }

    pub fn main_channel_ingress(
        &self,
        conversation_id: &str,
    ) -> Option<Sender<crate::service_protos::channel::ChannelIngress>> {
        self.conversations
            .lock()
            .ok()?
            .get(conversation_id)
            .map(|conversation| conversation.main_channel_ingress_tx.clone())
    }

    pub fn send_main_channel_ingress(
        &self,
        conversation_id: &str,
        ingress: crate::service_protos::channel::ChannelIngress,
    ) -> Result<()> {
        let sender = self
            .main_channel_ingress(conversation_id)
            .ok_or_else(|| anyhow!("unknown conversation {conversation_id}"))?;
        match sender.send(ingress) {
            Ok(()) => Ok(()),
            Err(crossbeam_channel::SendError(ingress)) => {
                self.logger.warn(
                    "conversation_channel_ingress_closed",
                    serde_json::json!({
                        "conversation_id": conversation_id,
                        "action": "restart_and_retry",
                    }),
                );
                self.restart_conversation(conversation_id, "main channel ingress closed")?;
                let sender = self.main_channel_ingress(conversation_id).ok_or_else(|| {
                    anyhow!("unknown conversation {conversation_id} after restart")
                })?;
                sender.send(ingress).map_err(|_| {
                    anyhow!(
                        "conversation {conversation_id} channel ingress is closed after restart"
                    )
                })
            }
        }
    }

    fn restart_conversation(&self, conversation_id: &str, reason: &str) -> Result<()> {
        let conversation = self
            .conversations
            .lock()
            .map_err(|_| anyhow!("conversation registry lock poisoned"))?
            .remove(conversation_id);
        if let Some(conversation) = conversation {
            if let Err(error) = conversation
                .handle
                .shutdown(format!("restarting: {reason}"))
            {
                self.logger.warn(
                    "conversation_kernel_shutdown_after_closed_ingress_failed",
                    serde_json::json!({
                        "conversation_id": conversation_id,
                        "error": error.to_string(),
                    }),
                );
            }
        }
        self.ensure_conversation_started(conversation_id)
    }

    pub fn stop_conversation(
        &self,
        conversation_id: &str,
        reason: impl Into<String>,
    ) -> Result<()> {
        let conversation = self
            .conversations
            .lock()
            .map_err(|_| anyhow!("conversation registry lock poisoned"))?
            .remove(conversation_id);
        let Some(conversation) = conversation else {
            return Ok(());
        };
        conversation.handle.shutdown(reason)
    }

    pub fn subscribe_main_channel_events(
        &self,
        conversation_id: &str,
    ) -> Result<Receiver<crate::service_protos::channel::ChannelEvent>> {
        let (_, rx) = self.main_channel_sender_and_subscription(conversation_id)?;
        Ok(rx)
    }

    pub fn send_main_channel_ingress_subscribed(
        &self,
        conversation_id: &str,
        ingress: crate::service_protos::channel::ChannelIngress,
    ) -> Result<Receiver<crate::service_protos::channel::ChannelEvent>> {
        self.ensure_conversation_started(conversation_id)?;
        let (sender, rx) = self.main_channel_sender_and_subscription(conversation_id)?;
        match sender.send(ingress) {
            Ok(()) => Ok(rx),
            Err(crossbeam_channel::SendError(ingress)) => {
                self.logger.warn(
                    "conversation_channel_ingress_closed",
                    serde_json::json!({
                        "conversation_id": conversation_id,
                        "action": "restart_resubscribe_and_retry",
                    }),
                );
                self.restart_conversation(conversation_id, "main channel ingress closed")?;
                let (sender, rx) = self.main_channel_sender_and_subscription(conversation_id)?;
                sender.send(ingress).map_err(|_| {
                    anyhow!(
                        "conversation {conversation_id} channel ingress is closed after restart"
                    )
                })?;
                Ok(rx)
            }
        }
    }

    fn main_channel_sender_and_subscription(
        &self,
        conversation_id: &str,
    ) -> Result<(
        Sender<crate::service_protos::channel::ChannelIngress>,
        Receiver<crate::service_protos::channel::ChannelEvent>,
    )> {
        let conversations = self
            .conversations
            .lock()
            .map_err(|_| anyhow!("conversation registry lock poisoned"))?;
        let conversation = conversations
            .get(conversation_id)
            .ok_or_else(|| anyhow!("unknown conversation {conversation_id}"))?;
        let (tx, rx) = crossbeam_channel::unbounded();
        conversation
            .main_channel_event_subscribers
            .lock()
            .map_err(|_| anyhow!("conversation event subscriber lock poisoned"))?
            .push(tx);
        Ok((conversation.main_channel_ingress_tx.clone(), rx))
    }
}

impl Drop for ConversationHostRuntime {
    fn drop(&mut self) {
        let Ok(mut conversations) = self.conversations.lock() else {
            return;
        };
        for (conversation_id, conversation) in conversations.drain() {
            let _ = conversation.handle.shutdown(format!(
                "host runtime stopping conversation {conversation_id}"
            ));
        }
    }
}

fn start_global_services() {
    let _ = shared_tool_binary_client();
}

fn spawn_channel_event_fanout(
    conversation_id: String,
    event_rx: Receiver<crate::service_protos::channel::ChannelEvent>,
    subscribers: Arc<Mutex<Vec<Sender<crate::service_protos::channel::ChannelEvent>>>>,
    logger: Arc<StellaclawLogger>,
) {
    thread::Builder::new()
        .name(format!("conversation-channel-events-{conversation_id}"))
        .spawn(move || {
            while let Ok(event) = event_rx.recv() {
                let Ok(mut subscribers) = subscribers.lock() else {
                    logger.warn(
                        "conversation_channel_event_subscriber_lock_poisoned",
                        serde_json::json!({"conversation_id": conversation_id}),
                    );
                    break;
                };
                subscribers.retain(|sender| sender.send(event.clone()).is_ok());
            }
        })
        .expect("failed to spawn conversation channel event fanout");
}

fn runtime_config_from_host_defaults(
    config: &StellaclawConfig,
    agent_server_path: PathBuf,
) -> Result<ConversationRuntimeConfig> {
    Ok(ConversationRuntimeConfig {
        agent_server_path: Some(agent_server_path),
        session_profile: Some(
            config
                .initial_session_profile()
                .map_err(anyhow::Error::msg)?,
        ),
        models: config.models.clone(),
        session_defaults: config.session_defaults.clone(),
        memory_enabled: config.memory.enabled,
        tool_remote_mode: stellaclaw_core::session_actor::ToolRemoteMode::Selectable,
        sandbox: Some(config.sandbox.clone()),
        reasoning_effort: None,
    })
}

fn read_conversation_metadata(path: &std::path::Path) -> Result<ConversationMetadata> {
    let raw = std::fs::read_to_string(path)
        .with_context(|| format!("failed to read {}", path.display()))?;
    serde_json::from_str(&raw).with_context(|| format!("failed to parse {}", path.display()))
}

#[cfg(test)]
mod tests {
    use std::{
        collections::HashMap,
        env, fs,
        sync::{Arc, Mutex},
        time::SystemTime,
    };

    use super::*;
    use crate::{
        conversation_new::ConversationRuntimeConfig,
        service_protos::channel::{ChannelEvent, ChannelIngress},
    };

    #[test]
    fn send_main_channel_ingress_restarts_closed_ingress() {
        let workdir = test_workdir("closed_ingress");
        fs::create_dir_all(&workdir).expect("test workdir can be created");
        let conversation_id = "web-main-test";
        let conversation_ref = ConversationRef {
            conversation_id: conversation_id.to_string(),
            workdir: workdir.clone(),
            conversation_root: workdir.join("conversations").join(conversation_id),
        };
        let runtime = ConversationHostRuntime {
            workdir: workdir.clone(),
            default_runtime_config: ConversationRuntimeConfig::for_conversation(&conversation_ref),
            logger: Arc::new(
                StellaclawLogger::open_under(&workdir, "test.log").expect("logger opens"),
            ),
            conversations: Mutex::new(HashMap::new()),
        };
        runtime
            .ensure_conversation_started(conversation_id)
            .expect("conversation starts");

        let (closed_tx, closed_rx) = crossbeam_channel::unbounded();
        drop(closed_rx);
        runtime
            .conversations
            .lock()
            .expect("registry lock")
            .get_mut(conversation_id)
            .expect("conversation is hosted")
            .main_channel_ingress_tx = closed_tx;

        runtime
            .send_main_channel_ingress(
                conversation_id,
                ChannelIngress::QueryForegroundStatus {
                    foreground_session_id: None,
                },
            )
            .expect("closed ingress is restarted and retried");
        runtime
            .stop_conversation(conversation_id, "test finished")
            .expect("conversation stops");
    }

    #[test]
    fn subscribed_ingress_resubscribes_after_restart() {
        let workdir = test_workdir("closed_ingress_subscribed");
        fs::create_dir_all(&workdir).expect("test workdir can be created");
        let conversation_id = "web-main-test";
        let conversation_ref = ConversationRef {
            conversation_id: conversation_id.to_string(),
            workdir: workdir.clone(),
            conversation_root: workdir.join("conversations").join(conversation_id),
        };
        let runtime = ConversationHostRuntime {
            workdir: workdir.clone(),
            default_runtime_config: ConversationRuntimeConfig::for_conversation(&conversation_ref),
            logger: Arc::new(
                StellaclawLogger::open_under(&workdir, "test.log").expect("logger opens"),
            ),
            conversations: Mutex::new(HashMap::new()),
        };
        runtime
            .ensure_conversation_started(conversation_id)
            .expect("conversation starts");

        let (closed_tx, closed_rx) = crossbeam_channel::unbounded();
        drop(closed_rx);
        runtime
            .conversations
            .lock()
            .expect("registry lock")
            .get_mut(conversation_id)
            .expect("conversation is hosted")
            .main_channel_ingress_tx = closed_tx;

        let rx = runtime
            .send_main_channel_ingress_subscribed(
                conversation_id,
                ChannelIngress::QueryForegroundStatus {
                    foreground_session_id: None,
                },
            )
            .expect("closed ingress is restarted and request is resubscribed");
        let event = ChannelEvent::Status {
            label: "probe".to_string(),
            detail: serde_json::json!({}),
        };
        let subscriber = {
            let conversations = runtime.conversations.lock().expect("registry lock");
            let conversation = conversations
                .get(conversation_id)
                .expect("conversation is hosted after restart");
            let subscriber = conversation
                .main_channel_event_subscribers
                .lock()
                .expect("subscriber lock")
                .last()
                .cloned()
                .expect("new subscriber exists");
            subscriber
        };
        subscriber.send(event).expect("probe event sends");
        assert!(matches!(
            rx.recv_timeout(std::time::Duration::from_millis(100)),
            Ok(ChannelEvent::Status { label, .. }) if label == "probe"
        ));
        runtime
            .stop_conversation(conversation_id, "test finished")
            .expect("conversation stops");
    }

    fn test_workdir(name: &str) -> PathBuf {
        let unique = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .expect("clock works")
            .as_nanos();
        env::temp_dir().join(format!("stellaclaw-host-{name}-{unique}"))
    }
}
