mod agent_loop;
mod bootstrap;
mod channel;
mod cli;
mod config;
mod config_scheme;
mod context;
mod embed;
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

use std::sync::Arc;

use anyhow::Result;
use clap::Parser;

use crate::agent_loop::AgentLoop;
use crate::channel::ChannelManager;
use crate::cli::{CliArgs, Commands};
use crate::message_bus::MessageBus;
use crate::path::PathManager;

#[tokio::main]
async fn main() -> Result<()> {
    let args = CliArgs::parse();

    // Handle setup subcommand
    if let Some(Commands::Setup { config }) = &args.command {
        let config_path = config
            .as_ref()
            .map(|p| p.to_str().unwrap())
            .or_else(|| args.config_path());
        let data_dir = args.data_dir().unwrap_or("~/.slimbot");
        return setup::run_setup(config_path, data_dir, args.workspace_dir());
    }

    let paths = PathManager::resolve(
        args.config_path(),
        args.data_dir(),
        args.workspace_dir(),
    )?;

    eprintln!("SlimBot starting... config file: {}", paths.config_path().display());

    // Initialize MessageBus (pure channel endpoints, no background tasks)
    let message_bus = Arc::new(MessageBus::new());

    // Initialize AgentLoop (spawns inbound listener task)
    let agent_loop = AgentLoop::from_config(&paths, message_bus.clone()).await?;
    agent_loop.start_inbound();

    // Initialize ChannelManager (auto-registers built-in channel factories)
    let mut channel_manager = ChannelManager::new(message_bus);

    // Load channels from config
    let config = crate::config::Config::load(paths.config_path().to_str().unwrap())?;
    channel_manager.init_from_config(&config.channels).await?;

    // Run outbound routing loop — blocks until all channels close
    channel_manager.run().await;

    Ok(())
}
