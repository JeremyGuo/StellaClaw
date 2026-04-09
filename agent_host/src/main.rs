mod config_editor;
mod setup;

use agent_host::Server;
use agent_host::config::{
    SandboxMode, load_server_config_file_and_upgrade, resolve_model_api_keys,
};
use agent_host::env::load_dotenv_files;
use agent_host::logging::init_logging;
use agent_host::sandbox::{bubblewrap_support_error, run_child_stdio};
use agent_host::zgent::app_bridge::run_zgent_app_bridge_stdio;
use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use config_editor::run_config_editor;
use setup::run_setup;
use std::io::{self, IsTerminal, Write};
use std::path::PathBuf;
use tracing::{error, info, warn};

#[derive(Parser, Debug)]
#[command(name = "partyclaw")]
struct Args {
    #[command(subcommand)]
    command: Option<AgentHostCommand>,
    #[arg(long)]
    config: Option<PathBuf>,
    #[arg(long)]
    workdir: Option<PathBuf>,
    #[arg(long)]
    sandbox_auto: bool,
}

#[derive(Subcommand, Debug)]
enum AgentHostCommand {
    #[command(name = "config")]
    Config { path: PathBuf },
    #[command(name = "setup")]
    Setup {
        config: PathBuf,
        workdir: PathBuf,
        service_name: Option<String>,
    },
    #[command(name = "run-child", hide = true)]
    RunChild,
    #[command(name = "run-tool-worker", hide = true)]
    RunToolWorker {
        #[arg(long)]
        job_file: PathBuf,
    },
    #[command(name = "run-zgent-app-bridge", hide = true)]
    RunZgentAppBridge {
        #[arg(long)]
        tools_file: PathBuf,
        #[arg(long)]
        bridge_address: String,
        #[arg(long)]
        bridge_token: String,
    },
}

fn main() -> Result<()> {
    let args = Args::parse();
    match args.command {
        Some(AgentHostCommand::Config { path }) => return run_config_editor(&path),
        Some(AgentHostCommand::Setup {
            config,
            workdir,
            service_name,
        }) => return run_setup(&config, &workdir, service_name.as_deref()),
        Some(AgentHostCommand::RunChild) => return run_child_stdio(),
        Some(AgentHostCommand::RunToolWorker { job_file }) => {
            return agent_frame::tool_worker::run_job_file(&job_file);
        }
        Some(AgentHostCommand::RunZgentAppBridge {
            tools_file,
            bridge_address,
            bridge_token,
        }) => {
            return run_zgent_app_bridge_stdio(&tools_file, &bridge_address, &bridge_token);
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
        "starting partyclaw"
    );
    for dotenv_path in loaded_dotenvs {
        info!(
            log_stream = "server",
            kind = "dotenv_loaded",
            path = %dotenv_path.display(),
            "loaded .env file"
        );
    }
    let (mut config, upgraded_config) = load_server_config_file_and_upgrade(config_path)?;
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
    if config.sandbox.mode == SandboxMode::Bubblewrap {
        if let Some(reason) = bubblewrap_support_error(&config.sandbox) {
            if args.sandbox_auto || !io::stdin().is_terminal() || !io::stdout().is_terminal() {
                warn!(
                    log_stream = "server",
                    kind = "sandbox_auto_fallback",
                    reason = %reason,
                    "sandbox mode 'bubblewrap' is not supported; falling back to subprocess"
                );
                config.sandbox.mode = SandboxMode::Subprocess;
            } else {
                let mut stdout = io::stdout();
                writeln!(
                    stdout,
                    "Sandbox mode 'bubblewrap' is not supported on this system: {reason}"
                )
                .ok();
                write!(
                    stdout,
                    "Continue with sandbox mode 'subprocess' instead? [y/N]: "
                )
                .ok();
                let _ = stdout.flush();
                let mut line = String::new();
                let _ = io::stdin().read_line(&mut line);
                let answer = line.trim().to_ascii_lowercase();
                if answer == "y" || answer == "yes" {
                    warn!(
                        log_stream = "server",
                        kind = "sandbox_prompt_fallback",
                        reason = %reason,
                        "sandbox mode 'bubblewrap' is not supported; falling back to subprocess"
                    );
                    config.sandbox.mode = SandboxMode::Subprocess;
                } else {
                    return Err(anyhow::anyhow!(
                        "sandbox mode 'bubblewrap' is not supported on this system: {reason}"
                    ));
                }
            }
        }
    }
    let server = Server::from_config(config, workdir)?;
    if let Err(error) = server.run().await {
        error!(
            log_stream = "server",
            kind = "fatal_error",
            error = %error,
            "partyclaw exited with error"
        );
        return Err(error);
    }
    info!(
        log_stream = "server",
        kind = "shutdown",
        "partyclaw stopped"
    );
    Ok(())
}
