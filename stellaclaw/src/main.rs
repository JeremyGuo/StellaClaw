mod channels;
mod config;
mod conversation;
mod conversation_id_manager;
mod cron;
mod logger;
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
    types::{IncomingDispatch, OutgoingDelivery},
    Channel, TelegramChannel,
};
use config::{ChannelConfig, StellaclawConfig};
use conversation::{load_or_create_conversation_state, spawn_conversation, ConversationCommand};
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

    let config = Arc::new(loaded_config);
    let agent_server_path = config.resolve_agent_server_path(&args.config);
    let id_manager = Arc::new(Mutex::new(
        ConversationIdManager::load_under(&args.workdir).map_err(anyhow::Error::msg)?,
    ));
    let cron_manager = Arc::new(CronManager::load_under(&args.workdir)?);
    let (incoming_tx, incoming_rx) = unbounded::<IncomingDispatch>();
    let (outgoing_tx, outgoing_rx) = unbounded::<OutgoingDelivery>();

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
        }
    }

    let send_channels = channels.clone();
    thread::spawn(move || {
        if let Err(error) = run_outgoing_loop(outgoing_rx, send_channels) {
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
    rx: Receiver<OutgoingDelivery>,
    channels: HashMap<String, Arc<dyn Channel>>,
) -> Result<()> {
    while let Ok(delivery) = rx.recv() {
        let channel = channels
            .get(&delivery.channel_id)
            .ok_or_else(|| anyhow!("unknown channel {}", delivery.channel_id))?;
        channel.send_delivery(&delivery)?;
    }
    Ok(())
}

fn run_dispatcher_loop(
    workdir: PathBuf,
    config: Arc<StellaclawConfig>,
    agent_server_path: PathBuf,
    cron_manager: Arc<CronManager>,
    incoming_rx: Receiver<IncomingDispatch>,
    outgoing_tx: Sender<OutgoingDelivery>,
    logger: Arc<StellaclawLogger>,
) -> Result<()> {
    let mut conversations: HashMap<String, Sender<ConversationCommand>> = HashMap::new();
    loop {
        match incoming_rx.recv_timeout(std::time::Duration::from_secs(1)) {
            Ok(dispatch) => {
                let sender = ensure_conversation_sender(
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
                )?;
                sender
                    .send(ConversationCommand::Incoming(dispatch.message))
                    .map_err(|_| anyhow!("conversation thread stopped"))?;
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
            let sender = ensure_conversation_sender(
                &mut conversations,
                &workdir,
                &config,
                &agent_server_path,
                &cron_manager,
                &outgoing_tx,
                &logger,
                &task.conversation_id,
                &task.channel_id,
                &task.platform_chat_id,
            )?;
            sender
                .send(ConversationCommand::RunCronTask { task })
                .map_err(|_| anyhow!("conversation thread stopped"))?;
        }
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn ensure_conversation_sender(
    conversations: &mut HashMap<String, Sender<ConversationCommand>>,
    workdir: &PathBuf,
    config: &Arc<StellaclawConfig>,
    agent_server_path: &PathBuf,
    cron_manager: &Arc<CronManager>,
    outgoing_tx: &Sender<OutgoingDelivery>,
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
