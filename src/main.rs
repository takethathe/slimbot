use std::sync::Arc;

use anyhow::Result;
use clap::Parser;

use slimbot::AgentLoop;
use slimbot::CliArgs;
use slimbot::Commands;
use slimbot::Config;
use slimbot::expand_home;
use slimbot::log_init;
use slimbot::LogLevel;
use slimbot::MessageBus;
use slimbot::PathManager;
use slimbot::run_agent_session;
use slimbot::run_setup;
use slimbot::WorkerPool;

#[tokio::main]
async fn main() -> Result<()> {
    let args = CliArgs::parse();

    let log_level = LogLevel::from_u8(args.log).unwrap_or_else(|| {
        LogLevel::Info
    });
    let log_file_path = args.log_file.as_ref().and_then(|p| {
        p.to_str().map(|s| expand_home(s))
    });
    log_init(log_level, log_file_path.as_deref())?;

    // No subcommand: print help and exit
    if args.command.is_none() {
        let _ = CliArgs::parse_from(["slimbot", "--help"]);
        return Ok(());
    }

    match args.command {
        Some(Commands::Setup { ref config }) => {
            let config_path = config
                .as_ref()
                .map(|p| p.to_str().unwrap())
                .or_else(|| args.config_path());
            let data_dir = args.data_dir().unwrap_or("~/.slimbot");
            return run_setup(config_path, data_dir, args.workspace_dir());
        }
        Some(Commands::Agent { ref session_id, ref query }) => {
            let paths = PathManager::resolve(
                args.config_path(),
                args.data_dir(),
                args.workspace_dir(),
            )?;
            return run_cli_agent(&paths, session_id.as_deref(), query.as_deref()).await;
        }
        None => unreachable!(),
    }
}

/// Run a CLI-only agent session: load config, start AgentLoop, prompt user, run tasks.
async fn run_cli_agent(
    paths: &PathManager,
    session_id: Option<&str>,
    query: Option<&str>,
) -> Result<()> {
    let message_bus = Arc::new(MessageBus::new());
    WorkerPool::init_global(64);

    let config = Arc::new(Config::load(paths.config_path().to_str().unwrap())?);

    let agent_loop = AgentLoop::from_config(paths, message_bus.clone(), config.clone()).await?;
    agent_loop.start_inbound();

    run_agent_session(&agent_loop, session_id, query).await
}
