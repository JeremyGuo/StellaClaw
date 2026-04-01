use agent_frame::tooling::terminate_all_managed_processes;
use agent_host::Server;
use agent_host::config::{load_server_config_file, resolve_model_api_keys};
use agent_host::env::load_dotenv_files;
use agent_host::logging::init_logging;
use anyhow::Result;
use clap::Parser;
use std::path::PathBuf;
use tracing::{error, info};

#[derive(Parser, Debug)]
#[command(name = "agent_host")]
struct Args {
    #[arg(long)]
    config: PathBuf,
    #[arg(long)]
    workdir: PathBuf,
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();
    let loaded_dotenvs = load_dotenv_files(&args.config)?;
    init_logging(&args.workdir)?;
    info!(
        log_stream = "server",
        kind = "startup",
        workdir = %args.workdir.display(),
        config = %args.config.display(),
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
    let config = load_server_config_file(&args.config)?;
    if std::env::var("DEBUG_API_KEY").ok().as_deref() == Some("1") {
        for item in resolve_model_api_keys(&config) {
            let value = item.api_key.unwrap_or_else(|| "<missing>".to_string());
            eprintln!(
                "[DEBUG_API_KEY] model={} source={} api_key={}",
                item.model_name, item.source, value
            );
        }
    }
    let server = Server::from_config(config, &args.workdir)?;
    if let Err(error) = server.run().await {
        let _ = terminate_all_managed_processes();
        error!(
            log_stream = "server",
            kind = "fatal_error",
            error = %error,
            "agent_host exited with error"
        );
        return Err(error);
    }
    let _ = terminate_all_managed_processes();
    info!(
        log_stream = "server",
        kind = "shutdown",
        "agent_host stopped"
    );
    Ok(())
}
