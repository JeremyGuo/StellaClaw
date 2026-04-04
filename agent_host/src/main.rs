use agent_host::Server;
use agent_host::config::{load_server_config_file_and_upgrade, resolve_model_api_keys};
use agent_host::env::load_dotenv_files;
use agent_host::logging::init_logging;
use agent_host::sandbox::run_child_stdio;
use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use std::path::PathBuf;
use tracing::{error, info};

#[derive(Parser, Debug)]
#[command(name = "agent_host")]
struct Args {
    #[command(subcommand)]
    command: Option<AgentHostCommand>,
    #[arg(long)]
    config: Option<PathBuf>,
    #[arg(long)]
    workdir: Option<PathBuf>,
}

#[derive(Subcommand, Debug)]
enum AgentHostCommand {
    #[command(name = "run-child", hide = true)]
    RunChild,
    #[command(name = "run-tool-worker", hide = true)]
    RunToolWorker {
        #[arg(long)]
        job_file: PathBuf,
    },
}

fn main() -> Result<()> {
    let args = Args::parse();
    match args.command {
        Some(AgentHostCommand::RunChild) => return run_child_stdio(),
        Some(AgentHostCommand::RunToolWorker { job_file }) => {
            return agent_frame::tool_worker::run_job_file(&job_file);
        }
        None => {}
    }
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .context("failed to build Tokio runtime")?;
    runtime.block_on(async move { run_server(args).await })
}

async fn run_server(args: Args) -> Result<()> {
    let config_path = args
        .config
        .as_ref()
        .context("--config is required unless running run-child")?;
    let workdir = args
        .workdir
        .as_ref()
        .context("--workdir is required unless running run-child")?;
    let loaded_dotenvs = load_dotenv_files(config_path)?;
    init_logging(workdir)?;
    info!(
        log_stream = "server",
        kind = "startup",
        workdir = %workdir.display(),
        config = %config_path.display(),
        "starting agent_host"
    );
    for dotenv_path in loaded_dotenvs {
        info!(
            log_stream = "server",
            kind = "dotenv_loaded",
            path = %dotenv_path.display(),
            "loaded .env file"
        );
    }
    let (config, upgraded_config) = load_server_config_file_and_upgrade(config_path)?;
    if upgraded_config {
        info!(
            log_stream = "server",
            kind = "config_upgraded",
            config = %config_path.display(),
            version = %config.version,
            "upgraded config file to latest version"
        );
    }
    if std::env::var("DEBUG_API_KEY").ok().as_deref() == Some("1") {
        for item in resolve_model_api_keys(&config) {
            let value = item.api_key.unwrap_or_else(|| "<missing>".to_string());
            eprintln!(
                "[DEBUG_API_KEY] model={} source={} api_key={}",
                item.model_name, item.source, value
            );
        }
    }
    let server = Server::from_config(config, workdir)?;
    if let Err(error) = server.run().await {
        error!(
            log_stream = "server",
            kind = "fatal_error",
            error = %error,
            "agent_host exited with error"
        );
        return Err(error);
    }
    info!(
        log_stream = "server",
        kind = "shutdown",
        "agent_host stopped"
    );
    Ok(())
}
