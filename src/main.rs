mod agent_loop;
mod bootstrap;
mod channel;
mod cli;
mod config;
mod config_scheme;
mod context;
mod embed;
mod io_scheduler;
mod log;
mod macros;
mod memory;
mod message_bus;
mod path;
mod provider;
mod runner;
mod session;
mod setup;
mod tool;
mod tools;
mod utils;
mod worker;

use std::sync::Arc;

use anyhow::Result;
use clap::Parser;

use crate::agent_loop::AgentLoop;
use crate::cli::{CliArgs, Commands};
use crate::log::LogLevel;
use crate::message_bus::MessageBus;
use crate::path::PathManager;

#[tokio::main]
async fn main() -> Result<()> {
    let args = CliArgs::parse();

    let log_level = LogLevel::from_u8(args.log).unwrap_or_else(|| {
        LogLevel::Info
    });
    let log_file_path = args.log_file.as_ref().and_then(|p| {
        p.to_str().map(|s| crate::path::expand_home(s))
    });
    crate::log::init(log_level, log_file_path.as_deref())?;

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
            return setup::run_setup(config_path, data_dir, args.workspace_dir());
        }
        Some(Commands::Agent { ref session_id }) => {
            let paths = PathManager::resolve(
                args.config_path(),
                args.data_dir(),
                args.workspace_dir(),
            )?;
            return run_cli_agent(&paths, session_id.as_deref()).await;
        }
        None => unreachable!(),
    }
}

/// Run a CLI-only agent session: load config, start AgentLoop, prompt user, run tasks.
async fn run_cli_agent(paths: &PathManager, session_id: Option<&str>) -> Result<()> {
    let message_bus = Arc::new(MessageBus::new());
    crate::worker::WorkerPool::init_global(64);

    let config = Arc::new(crate::config::Config::load(paths.config_path().to_str().unwrap())?);

    let agent_loop = AgentLoop::from_config(paths, message_bus.clone(), config.clone()).await?;
    agent_loop.start_inbound();

    crate::cli::run_agent_session(&agent_loop, session_id).await
}
