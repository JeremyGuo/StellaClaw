mod cache;
mod channels;
mod config;
mod conversation;
mod conversation_id_manager;
mod cron;
mod logger;
mod remote_actor;
mod sandbox;
mod session_client;
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
    types::{IncomingDispatch, OutgoingDispatch},
    Channel, TelegramChannel, WebChannel,
};
use config::{ChannelConfig, StellaclawConfig};
use conversation::{
    load_or_create_conversation_state, push_configured_skill_sync_on_startup, spawn_conversation,
    ConversationCommand,
};
use conversation_id_manager::ConversationIdManager;
use cron::CronManager;
use crossbeam_channel::{unbounded, Receiver, Sender};
use logger::StellaclawLogger;
use sandbox::bubblewrap_support_error;
use upgrade::upgrade_workdir;

fn main() {
    if let Err(error) = run() {
        eprintln!("stellaclaw: {error:#}");
        std::process::exit(1);
    }
}

fn run() -> Result<()> {
    let args = parse_args()?;
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
    let cron_manager = Arc::new(CronManager::load_under(&args.workdir)?);
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

    let send_channels = channels.clone();
    let outgoing_logger = logger.clone();
    thread::spawn(move || {
        if let Err(error) = run_outgoing_loop(outgoing_rx, send_channels, outgoing_logger) {
            eprintln!("stellaclaw outgoing loop failed: {error:#}");
        }
    });

    run_dispatcher_loop(
        args.workdir,
        config,
        agent_server_path,
        cron_manager,
        incoming_rx,
        outgoing_tx,
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

fn run_dispatcher_loop(
    workdir: PathBuf,
    config: Arc<StellaclawConfig>,
    agent_server_path: PathBuf,
    cron_manager: Arc<CronManager>,
    incoming_rx: Receiver<IncomingDispatch>,
    outgoing_tx: Sender<OutgoingDispatch>,
    logger: Arc<StellaclawLogger>,
) -> Result<()> {
    let mut conversations: HashMap<String, Sender<ConversationCommand>> = HashMap::new();
    loop {
        match incoming_rx.recv_timeout(std::time::Duration::from_secs(1)) {
            Ok(IncomingDispatch::Message(dispatch)) => {
                if let Err(error) = send_conversation_command(
                    &mut conversations,
                    &workdir,
                    &config,
                    &agent_server_path,
                    &cron_manager,
                    &outgoing_tx,
                    &logger,
                    &dispatch.conversation_id,
                    &dispatch.channel_id,
                    &dispatch.platform_chat_id,
                    ConversationCommand::Incoming(dispatch.message),
                ) {
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
            Ok(IncomingDispatch::DeleteConversation {
                channel_id,
                platform_chat_id,
                conversation_id,
                response_tx,
            }) => {
                let result = shutdown_active_conversation(
                    &mut conversations,
                    &conversation_id,
                    &channel_id,
                    &platform_chat_id,
                    &logger,
                )
                .and_then(|_| {
                    cron_manager
                        .remove_tasks_for_conversation(&conversation_id)
                        .map(|_| ())
                })
                .map_err(|error| format!("{error:#}"));
                let _ = response_tx.send(result);
            }
            Err(crossbeam_channel::RecvTimeoutError::Timeout) => {}
            Err(crossbeam_channel::RecvTimeoutError::Disconnected) => break,
        }

        for task in cron_manager.collect_due_tasks(chrono::Utc::now())? {
            logger.info(
                "cron_task_due",
                serde_json::json!({
                    "id": task.id,
                    "conversation_id": task.conversation_id,
                    "name": task.name,
                    "next_run_at": task.next_run_at,
                }),
            );
            let conversation_id = task.conversation_id.clone();
            let channel_id = task.channel_id.clone();
            let platform_chat_id = task.platform_chat_id.clone();
            if let Err(error) = send_conversation_command(
                &mut conversations,
                &workdir,
                &config,
                &agent_server_path,
                &cron_manager,
                &outgoing_tx,
                &logger,
                &conversation_id,
                &channel_id,
                &platform_chat_id,
                ConversationCommand::RunCronTask { task },
            ) {
                logger.warn(
                    "cron_dispatch_failed",
                    serde_json::json!({
                        "conversation_id": conversation_id,
                        "channel_id": channel_id,
                        "platform_chat_id": platform_chat_id,
                        "error": format!("{error:#}"),
                    }),
                );
            }
        }
    }
    Ok(())
}

fn shutdown_active_conversation(
    conversations: &mut HashMap<String, Sender<ConversationCommand>>,
    conversation_id: &str,
    channel_id: &str,
    platform_chat_id: &str,
    logger: &Arc<StellaclawLogger>,
) -> Result<()> {
    let Some(sender) = conversations.remove(conversation_id) else {
        return Ok(());
    };
    let (ack_tx, ack_rx) = crossbeam_channel::bounded(1);
    if sender
        .send(ConversationCommand::Shutdown {
            reason: "conversation deleted",
            ack_tx,
        })
        .is_err()
    {
        return Ok(());
    }
    ack_rx
        .recv_timeout(std::time::Duration::from_secs(5))
        .with_context(|| {
            format!("timed out waiting for conversation {conversation_id} to stop before delete")
        })?;
    logger.info(
        "conversation_shutdown_for_delete",
        serde_json::json!({
            "conversation_id": conversation_id,
            "channel_id": channel_id,
            "platform_chat_id": platform_chat_id,
        }),
    );
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn send_conversation_command(
    conversations: &mut HashMap<String, Sender<ConversationCommand>>,
    workdir: &PathBuf,
    config: &Arc<StellaclawConfig>,
    agent_server_path: &PathBuf,
    cron_manager: &Arc<CronManager>,
    outgoing_tx: &Sender<OutgoingDispatch>,
    logger: &Arc<StellaclawLogger>,
    conversation_id: &str,
    channel_id: &str,
    platform_chat_id: &str,
    command: ConversationCommand,
) -> Result<()> {
    let mut last_error = None;
    for attempt in 0..2 {
        let sender = ensure_conversation_sender(
            conversations,
            workdir,
            config,
            agent_server_path,
            cron_manager,
            outgoing_tx,
            logger,
            conversation_id,
            channel_id,
            platform_chat_id,
        )?;
        match sender.send(command.clone()) {
            Ok(()) => return Ok(()),
            Err(_) => {
                conversations.remove(conversation_id);
                let error = anyhow!("conversation thread stopped before command delivery");
                logger.warn(
                    "conversation_command_send_failed",
                    serde_json::json!({
                        "conversation_id": conversation_id,
                        "channel_id": channel_id,
                        "platform_chat_id": platform_chat_id,
                        "attempt": attempt + 1,
                        "will_retry": attempt == 0,
                        "error": format!("{error:#}"),
                    }),
                );
                last_error = Some(error);
            }
        }
    }
    Err(last_error.unwrap_or_else(|| anyhow!("conversation command was not delivered")))
}

#[allow(clippy::too_many_arguments)]
fn ensure_conversation_sender(
    conversations: &mut HashMap<String, Sender<ConversationCommand>>,
    workdir: &PathBuf,
    config: &Arc<StellaclawConfig>,
    agent_server_path: &PathBuf,
    cron_manager: &Arc<CronManager>,
    outgoing_tx: &Sender<OutgoingDispatch>,
    logger: &Arc<StellaclawLogger>,
    conversation_id: &str,
    channel_id: &str,
    platform_chat_id: &str,
) -> Result<Sender<ConversationCommand>> {
    if let Some(sender) = conversations.get(conversation_id) {
        return Ok(sender.clone());
    }
    let state = load_or_create_conversation_state(
        workdir,
        conversation_id,
        channel_id,
        platform_chat_id,
        config,
    )?;
    let sender = spawn_conversation(
        workdir.clone(),
        state,
        config.clone(),
        agent_server_path.clone(),
        cron_manager.clone(),
        outgoing_tx.clone(),
        logger.clone(),
    );
    conversations.insert(conversation_id.to_string(), sender.clone());
    Ok(sender)
}

struct Args {
    config: PathBuf,
    workdir: PathBuf,
}

fn parse_args() -> Result<Args> {
    let mut args = env::args().skip(1);
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
            other => return Err(anyhow!("unknown argument {other}")),
        }
    }
    Ok(Args {
        config: config.ok_or_else(|| anyhow!("missing --config"))?,
        workdir: workdir.ok_or_else(|| anyhow!("missing --workdir"))?,
    })
}
