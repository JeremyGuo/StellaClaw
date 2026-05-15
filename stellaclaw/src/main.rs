mod cache;
mod channels;
mod config;
mod conversation_host;
mod conversation_id_manager;
mod conversation_metadata;
mod conversation_new;
mod conversation_state;
mod logger;
mod memory;
mod sandbox;
mod service_protos;
mod services;
mod session_client;
mod setup;
mod tool_binary_manager;
mod upgrade;
mod workspace;

use std::{
    collections::HashMap,
    env, fs,
    path::PathBuf,
    sync::{Arc, Mutex},
    thread,
};

use anyhow::{anyhow, Context, Result};
use channels::{
    types::{
        ChannelEvent, ConversationControl, IncomingConversationMessage, IncomingDispatch,
        OutgoingDispatch, OutgoingError, OutgoingErrorScope, OutgoingErrorSeverity,
        OutgoingMessageAppended, OutgoingProcessing, OutgoingSessionStream, ProcessingState,
    },
    Channel, TelegramChannel, WebChannel,
};
use config::{ChannelConfig, ModelSelection, SessionProfile, StellaclawConfig};
use conversation_host::ConversationHostRuntime;
use conversation_id_manager::ConversationIdManager;
use conversation_metadata::ConversationMetadataStore;
use crossbeam_channel::{unbounded, Receiver, Sender};
use logger::StellaclawLogger;
use sandbox::bubblewrap_support_error;
use service_protos::{
    agent_session::{AgentMessageOrigin, AgentSessionEvent},
    channel::ChannelIngress,
    kernel::KernelRuntimeConfigPatch,
};
use services::skill_sync::push_configured_skill_sync_on_startup;
use stellaclaw_core::session_actor::{
    ChatMessage, ChatMessageItem, ChatRole, ContextItem, ToolRemoteMode,
};
use upgrade::upgrade_workdir;

fn main() {
    if let Err(error) = run() {
        eprintln!("stellaclaw: {error:#}");
        std::process::exit(1);
    }
}

fn run() -> Result<()> {
    let args = match parse_args()? {
        Command::Serve(args) => args,
        Command::Setup(args) => return setup::run(args),
    };
    fs::create_dir_all(&args.workdir)
        .with_context(|| format!("failed to create {}", args.workdir.display()))?;
    let logger = Arc::new(
        StellaclawLogger::open_under(&args.workdir, "host.log").map_err(anyhow::Error::msg)?,
    );
    logger.info(
        "stellaclaw_starting",
        serde_json::json!({
            "config_path": args.config.display().to_string(),
            "workdir": args.workdir.display().to_string(),
        }),
    );

    let (mut loaded_config, config_upgraded) =
        config::loaders::load_config_file_and_upgrade(&args.config).map_err(anyhow::Error::msg)?;
    if config_upgraded {
        logger.info(
            "config_upgraded",
            serde_json::json!({"config_path": args.config.display().to_string()}),
        );
    }
    if matches!(loaded_config.sandbox.mode, config::SandboxMode::Bubblewrap) {
        if let Some(reason) = bubblewrap_support_error(&loaded_config.sandbox) {
            logger.warn(
                "sandbox_fallback",
                serde_json::json!({"reason": reason, "mode": "subprocess"}),
            );
            loaded_config.sandbox.mode = config::SandboxMode::Subprocess;
        }
    }

    let workdir_upgraded = upgrade_workdir(&args.workdir, &loaded_config)?;
    if workdir_upgraded {
        logger.info(
            "workdir_upgraded",
            serde_json::json!({"workdir": args.workdir.display().to_string()}),
        );
    }
    let startup_skill_sync =
        push_configured_skill_sync_on_startup(&loaded_config.skill_sync, &args.workdir, &logger);
    if !startup_skill_sync.is_empty() {
        logger.info(
            "skill_sync_startup_finished",
            serde_json::json!({"skills": startup_skill_sync}),
        );
    }

    let config = Arc::new(loaded_config);
    let agent_server_path = config.resolve_agent_server_path(&args.config);
    let id_manager = Arc::new(Mutex::new(
        ConversationIdManager::load_under(&args.workdir).map_err(anyhow::Error::msg)?,
    ));
    let conversation_host_runtime = Arc::new(ConversationHostRuntime::start_existing(
        args.workdir.clone(),
        config.clone(),
        agent_server_path.clone(),
        logger.clone(),
    )?);
    let (incoming_tx, incoming_rx) = unbounded::<IncomingDispatch>();
    let (outgoing_tx, outgoing_rx) = unbounded::<OutgoingDispatch>();

    let mut channels: HashMap<String, Arc<dyn Channel>> = HashMap::new();
    for channel in &config.channels {
        match channel {
            ChannelConfig::Telegram(telegram) => {
                let instance = Arc::new(TelegramChannel::new(
                    telegram.id.clone(),
                    telegram.resolve_bot_token().map_err(anyhow::Error::msg)?,
                    telegram.api_base_url.clone(),
                    telegram.poll_timeout_seconds,
                    telegram.poll_interval_ms,
                    telegram.admin_user_ids.clone(),
                    &args.workdir,
                )?);
                instance.clone().spawn_ingress(
                    incoming_tx.clone(),
                    id_manager.clone(),
                    logger.clone(),
                );
                channels.insert(instance.id().to_string(), instance);
            }
            ChannelConfig::Web(web) => {
                let instance = Arc::new(WebChannel::new(
                    web.id.clone(),
                    web.bind_addr.clone(),
                    web.resolve_token().map_err(anyhow::Error::msg)?,
                    args.workdir.clone(),
                    config.clone(),
                    conversation_host_runtime.clone(),
                    logger.clone(),
                ));
                instance.clone().spawn_ingress(
                    incoming_tx.clone(),
                    id_manager.clone(),
                    logger.clone(),
                );
                channels.insert(instance.id().to_string(), instance);
            }
        }
    }

    let bridge_runtime = conversation_host_runtime.clone();
    let bridge_workdir = args.workdir.clone();
    let bridge_outgoing_tx = outgoing_tx.clone();
    let bridge_logger = logger.clone();
    thread::spawn(move || {
        run_conversation_event_bridge(
            bridge_workdir,
            bridge_runtime,
            bridge_outgoing_tx,
            bridge_logger,
        );
    });

    let send_channels = channels.clone();
    let outgoing_logger = logger.clone();
    thread::spawn(move || {
        if let Err(error) = run_outgoing_loop(outgoing_rx, send_channels, outgoing_logger) {
            eprintln!("stellaclaw outgoing loop failed: {error:#}");
        }
    });

    run_channel_ingress_loop(
        args.workdir,
        config,
        conversation_host_runtime,
        incoming_rx,
        logger,
    )
}

fn run_outgoing_loop(
    rx: Receiver<OutgoingDispatch>,
    channels: HashMap<String, Arc<dyn Channel>>,
    logger: Arc<StellaclawLogger>,
) -> Result<()> {
    while let Ok(dispatch) = rx.recv() {
        match dispatch {
            OutgoingDispatch::Event(event) => {
                let channel_id = event.channel_id().to_string();
                let platform_chat_id = event.platform_chat_id().to_string();
                let Some(channel) = channels.get(&channel_id) else {
                    logger.warn(
                        "outgoing_event_failed",
                        serde_json::json!({"channel_id": channel_id, "error": "unknown channel"}),
                    );
                    continue;
                };
                if let Err(error) = channel.send_event(&event) {
                    logger.warn(
                        "outgoing_event_failed",
                        serde_json::json!({
                            "channel_id": channel_id,
                            "platform_chat_id": platform_chat_id,
                            "error": format!("{error:#}"),
                        }),
                    );
                }
            }
        }
    }
    Ok(())
}

fn run_channel_ingress_loop(
    workdir: PathBuf,
    config: Arc<StellaclawConfig>,
    conversation_runtime: Arc<ConversationHostRuntime>,
    incoming_rx: Receiver<IncomingDispatch>,
    logger: Arc<StellaclawLogger>,
) -> Result<()> {
    while let Ok(dispatch) = incoming_rx.recv() {
        match dispatch {
            IncomingDispatch::Message(dispatch) => {
                if let Err(error) =
                    handle_incoming_message(&workdir, &config, &conversation_runtime, &dispatch)
                {
                    logger.warn(
                        "incoming_dispatch_failed",
                        serde_json::json!({
                            "conversation_id": dispatch.conversation_id,
                            "channel_id": dispatch.channel_id,
                            "platform_chat_id": dispatch.platform_chat_id,
                            "error": format!("{error:#}"),
                        }),
                    );
                }
            }
            IncomingDispatch::DeleteConversation {
                channel_id,
                platform_chat_id,
                conversation_id,
                response_tx,
            } => {
                let result = conversation_runtime
                    .stop_conversation(&conversation_id, "conversation deleted")
                    .map_err(|error| format!("{error:#}"));
                logger.info(
                    "conversation_shutdown_for_delete",
                    serde_json::json!({
                        "conversation_id": conversation_id,
                        "channel_id": channel_id,
                        "platform_chat_id": platform_chat_id,
                    }),
                );
                let _ = response_tx.send(result);
            }
        }
    }
    Ok(())
}

fn handle_incoming_message(
    workdir: &PathBuf,
    config: &Arc<StellaclawConfig>,
    conversation_runtime: &Arc<ConversationHostRuntime>,
    dispatch: &channels::types::IncomingMessageDispatch,
) -> Result<()> {
    ensure_conversation_state(workdir, config, dispatch)?;
    conversation_runtime.ensure_conversation_started(&dispatch.conversation_id)?;
    let ingress = incoming_message_to_channel_ingress(config, &dispatch.message)?;
    conversation_runtime.send_main_channel_ingress(&dispatch.conversation_id, ingress)
}

fn ensure_conversation_state(
    workdir: &PathBuf,
    _config: &Arc<StellaclawConfig>,
    dispatch: &channels::types::IncomingMessageDispatch,
) -> Result<()> {
    let store = ConversationMetadataStore::new(workdir);
    let mut metadata = store.load_or_create(
        &dispatch.conversation_id,
        &dispatch.channel_id,
        &dispatch.platform_chat_id,
    )?;
    metadata.channel_id = dispatch.channel_id.clone();
    metadata.platform_chat_id = dispatch.platform_chat_id.clone();
    store.persist(&metadata)
}

fn incoming_message_to_channel_ingress(
    config: &StellaclawConfig,
    message: &IncomingConversationMessage,
) -> Result<ChannelIngress> {
    if let Some(control) = &message.control {
        return control_to_channel_ingress(config, control);
    }

    let mut items = Vec::new();
    if let Some(text) = message.text.as_ref().filter(|text| !text.is_empty()) {
        items.push(ChatMessageItem::Context(ContextItem { text: text.clone() }));
    }
    items.extend(
        message
            .selection_references
            .iter()
            .cloned()
            .map(ChatMessageItem::SelectionReference),
    );
    items.extend(message.files.iter().cloned().map(ChatMessageItem::File));
    if items.is_empty() {
        return Err(anyhow!("incoming message is empty"));
    }

    Ok(ChannelIngress::IncomingMessage {
        platform_message_id: Some(message.remote_message_id.clone()),
        origin: Some(AgentMessageOrigin::User),
        message: ChatMessage::new(ChatRole::User, items)
            .with_user_name_option(message.user_name.clone())
            .with_message_time_option(message.message_time.clone()),
        metadata: serde_json::json!({}),
    })
}

fn control_to_channel_ingress(
    config: &StellaclawConfig,
    control: &ConversationControl,
) -> Result<ChannelIngress> {
    match control {
        ConversationControl::Continue => Ok(ChannelIngress::ContinueForegroundTurn {
            reason: Some("user requested continue".to_string()),
        }),
        ConversationControl::Cancel => Ok(ChannelIngress::CancelForegroundTurn {
            reason: Some("user requested cancel".to_string()),
        }),
        ConversationControl::Compact => Ok(ChannelIngress::CompactForegroundNow),
        ConversationControl::ShowStatus => Ok(ChannelIngress::QueryForegroundStatus),
        ConversationControl::SwitchModel { model_name } => {
            if !config.models.contains_key(model_name) {
                return Err(anyhow!("unknown model alias {model_name}"));
            }
            Ok(ChannelIngress::UpdateRuntimeConfig {
                patch: KernelRuntimeConfigPatch {
                    session_profile: Some(Some(SessionProfile {
                        main_model: ModelSelection::alias(model_name.clone()),
                    })),
                    ..Default::default()
                },
            })
        }
        ConversationControl::SetReasoning { effort } => Ok(ChannelIngress::UpdateRuntimeConfig {
            patch: KernelRuntimeConfigPatch {
                reasoning_effort: Some(effort.clone()),
                ..Default::default()
            },
        }),
        ConversationControl::SetRemote { host, path } => Ok(ChannelIngress::UpdateRuntimeConfig {
            patch: KernelRuntimeConfigPatch {
                tool_remote_mode: Some(ToolRemoteMode::FixedSsh {
                    host: host.clone(),
                    cwd: Some(path.clone()),
                }),
                ..Default::default()
            },
        }),
        ConversationControl::DisableRemote => Ok(ChannelIngress::UpdateRuntimeConfig {
            patch: KernelRuntimeConfigPatch {
                tool_remote_mode: Some(ToolRemoteMode::Selectable),
                ..Default::default()
            },
        }),
        ConversationControl::ShowModel
        | ConversationControl::ShowReasoning
        | ConversationControl::ShowRemote
        | ConversationControl::ShowSandbox => Ok(ChannelIngress::QueryForegroundStatus),
        ConversationControl::SetSandbox { .. } => Err(anyhow!(
            "sandbox runtime switching is not exposed through the new channel protocol yet"
        )),
        ConversationControl::InvalidReasoning { reason }
        | ConversationControl::InvalidRemote { reason }
        | ConversationControl::InvalidSandbox { reason } => Err(anyhow!(reason.clone())),
    }
}

fn run_conversation_event_bridge(
    workdir: PathBuf,
    conversation_runtime: Arc<ConversationHostRuntime>,
    outgoing_tx: Sender<OutgoingDispatch>,
    logger: Arc<StellaclawLogger>,
) {
    let mut subscriptions = HashMap::new();
    loop {
        for conversation_id in conversation_runtime.conversation_ids() {
            if !subscriptions.contains_key(&conversation_id) {
                match conversation_runtime.subscribe_main_channel_events(&conversation_id) {
                    Ok(rx) => {
                        subscriptions.insert(conversation_id.clone(), rx);
                    }
                    Err(error) => {
                        logger.warn(
                            "conversation_event_bridge_subscribe_failed",
                            serde_json::json!({
                                "conversation_id": conversation_id,
                                "error": format!("{error:#}"),
                            }),
                        );
                        continue;
                    }
                }
            }
            let Some(rx) = subscriptions.get(&conversation_id) else {
                continue;
            };
            for event in rx.try_iter() {
                match project_channel_event(&workdir, &conversation_id, event) {
                    Ok(events) => {
                        for event in events {
                            let _ = outgoing_tx.send(OutgoingDispatch::Event(event));
                        }
                    }
                    Err(error) => {
                        logger.warn(
                            "conversation_event_bridge_project_failed",
                            serde_json::json!({
                                "conversation_id": conversation_id,
                                "error": format!("{error:#}"),
                            }),
                        );
                    }
                }
            }
        }
        thread::sleep(std::time::Duration::from_millis(100));
    }
}

fn project_channel_event(
    workdir: &PathBuf,
    conversation_id: &str,
    event: service_protos::channel::ChannelEvent,
) -> Result<Vec<ChannelEvent>> {
    let metadata = ConversationMetadataStore::new(workdir).load(conversation_id)?;
    let mut events = Vec::new();
    match event {
        service_protos::channel::ChannelEvent::SessionEvent {
            session_addr: _,
            event,
        } => match event {
            AgentSessionEvent::MessageAppended { index, message } => {
                events.push(ChannelEvent::MessageAppended(OutgoingMessageAppended {
                    channel_id: metadata.channel_id.clone(),
                    platform_chat_id: metadata.platform_chat_id.clone(),
                    conversation_id: metadata.conversation_id.clone(),
                    session_id: metadata.foreground_session_id.clone(),
                    index,
                    message,
                }));
            }
            AgentSessionEvent::TurnStarted { .. } => {
                events.push(ChannelEvent::Processing(OutgoingProcessing {
                    channel_id: metadata.channel_id.clone(),
                    platform_chat_id: metadata.platform_chat_id.clone(),
                    state: ProcessingState::Typing,
                }));
            }
            event @ (AgentSessionEvent::StreamAssistantMessageDelta { .. }
            | AgentSessionEvent::StreamToolCallDelta { .. }
            | AgentSessionEvent::StreamReasoningSummaryDelta { .. }
            | AgentSessionEvent::StreamReasoningSummaryPartAdded { .. }
            | AgentSessionEvent::StreamError { .. }) => {
                events.push(ChannelEvent::SessionStream(OutgoingSessionStream {
                    channel_id: metadata.channel_id.clone(),
                    platform_chat_id: metadata.platform_chat_id.clone(),
                    conversation_id: metadata.conversation_id.clone(),
                    session_id: metadata.foreground_session_id.clone(),
                    event: serde_json::to_value(event)?,
                }));
            }
            AgentSessionEvent::TurnCompleted { .. } => {
                events.push(ChannelEvent::Processing(OutgoingProcessing {
                    channel_id: metadata.channel_id.clone(),
                    platform_chat_id: metadata.platform_chat_id.clone(),
                    state: ProcessingState::Idle,
                }));
            }
            AgentSessionEvent::TurnFailed {
                error,
                error_detail,
                can_continue,
            } => {
                events.push(ChannelEvent::Processing(OutgoingProcessing {
                    channel_id: metadata.channel_id.clone(),
                    platform_chat_id: metadata.platform_chat_id.clone(),
                    state: ProcessingState::Idle,
                }));
                events.push(ChannelEvent::Error(OutgoingError {
                    channel_id: metadata.channel_id.clone(),
                    platform_chat_id: metadata.platform_chat_id.clone(),
                    conversation_id: metadata.conversation_id.clone(),
                    scope: OutgoingErrorScope::Runtime,
                    severity: OutgoingErrorSeverity::Error,
                    code: "agent_session_failed".to_string(),
                    message: error,
                    detail: Some(serde_json::to_value(error_detail)?),
                    can_continue,
                    suggested_action: None,
                }));
            }
            AgentSessionEvent::RuntimeCrashed {
                error,
                error_detail,
            } => {
                events.push(ChannelEvent::Processing(OutgoingProcessing {
                    channel_id: metadata.channel_id.clone(),
                    platform_chat_id: metadata.platform_chat_id.clone(),
                    state: ProcessingState::Idle,
                }));
                events.push(ChannelEvent::Error(OutgoingError {
                    channel_id: metadata.channel_id.clone(),
                    platform_chat_id: metadata.platform_chat_id.clone(),
                    conversation_id: metadata.conversation_id.clone(),
                    scope: OutgoingErrorScope::Runtime,
                    severity: OutgoingErrorSeverity::Error,
                    code: "agent_session_crashed".to_string(),
                    message: error,
                    detail: Some(serde_json::to_value(error_detail)?),
                    can_continue: false,
                    suggested_action: None,
                }));
            }
            _ => {}
        },
        service_protos::channel::ChannelEvent::Delivery { delivery, text } => {
            events.push(ChannelEvent::Delivery(channels::types::OutgoingDelivery {
                channel_id: metadata.channel_id.clone(),
                platform_chat_id: metadata.platform_chat_id.clone(),
                conversation_id: metadata.conversation_id.clone(),
                session_id: delivery.session_addr.map(|addr| addr.to_string()),
                message: delivery.message,
                text,
                attachments: Vec::new(),
                options: None,
            }));
        }
        service_protos::channel::ChannelEvent::Error {
            code,
            message,
            detail,
        } => {
            events.push(ChannelEvent::Error(OutgoingError {
                channel_id: metadata.channel_id.clone(),
                platform_chat_id: metadata.platform_chat_id.clone(),
                conversation_id: metadata.conversation_id.clone(),
                scope: OutgoingErrorScope::Runtime,
                severity: OutgoingErrorSeverity::Error,
                code,
                message,
                detail: detail.map(serde_json::Value::String),
                can_continue: true,
                suggested_action: None,
            }));
        }
        _ => {}
    }
    Ok(events)
}

struct Args {
    config: PathBuf,
    workdir: PathBuf,
}

enum Command {
    Serve(Args),
    Setup(setup::SetupArgs),
}

fn parse_args() -> Result<Command> {
    let mut args = env::args().skip(1);
    let first = args.next().ok_or_else(|| anyhow!(usage()))?;
    if first == "setup" {
        return parse_setup_args(args);
    }
    let mut args = std::iter::once(first).chain(args);
    let mut config = None;
    let mut workdir = None;
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--config" => {
                config = Some(PathBuf::from(
                    args.next()
                        .ok_or_else(|| anyhow!("--config requires a path"))?,
                ));
            }
            "--workdir" => {
                workdir = Some(PathBuf::from(
                    args.next()
                        .ok_or_else(|| anyhow!("--workdir requires a path"))?,
                ));
            }
            "--help" | "-h" => return Err(anyhow!(usage())),
            other => return Err(anyhow!("unknown argument {other}")),
        }
    }
    Ok(Command::Serve(Args {
        config: config.ok_or_else(|| anyhow!("missing --config"))?,
        workdir: workdir.ok_or_else(|| anyhow!("missing --workdir"))?,
    }))
}

fn parse_setup_args(args: impl Iterator<Item = String>) -> Result<Command> {
    let mut positionals = Vec::new();
    let mut install_systemd = false;
    let mut systemd_user = false;
    let mut dry_run = false;
    for arg in args {
        match arg.as_str() {
            "--systemd" => install_systemd = true,
            "--user" => systemd_user = true,
            "--dry-run" => dry_run = true,
            "--help" | "-h" => return Err(anyhow!(setup_usage())),
            other if other.starts_with('-') => {
                return Err(anyhow!("unknown setup argument {other}"))
            }
            _ => positionals.push(arg),
        }
    }
    if positionals.len() > 2 {
        return Err(anyhow!(
            "too many setup positional arguments\n\n{}",
            setup_usage()
        ));
    }
    let config = positionals
        .first()
        .ok_or_else(|| anyhow!("missing setup <config>\n\n{}", setup_usage()))?;
    let workdir = positionals
        .get(1)
        .ok_or_else(|| anyhow!("missing setup <workdir>\n\n{}", setup_usage()))?;
    if systemd_user && !install_systemd {
        return Err(anyhow!("--user requires --systemd"));
    }
    Ok(Command::Setup(setup::SetupArgs {
        config: PathBuf::from(config),
        workdir: PathBuf::from(workdir),
        install_systemd,
        systemd_user,
        dry_run,
    }))
}

fn usage() -> &'static str {
    "usage:\n  stellaclaw --config <config> --workdir <workdir>\n  stellaclaw setup <config> <workdir> [--systemd [--user]] [--dry-run]"
}

fn setup_usage() -> &'static str {
    "usage: stellaclaw setup <config> <workdir> [--systemd [--user]] [--dry-run]"
}
