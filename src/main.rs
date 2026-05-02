mod agent_loop;
mod bootstrap;
mod channel;
mod config;
mod config_scheme;
mod context;
mod embed;
mod memory;
mod message_bus;
mod provider;
mod runner;
mod session;
mod setup;
mod tool;
mod tools;
mod utils;

use std::sync::Arc;

use anyhow::Result;

use crate::agent_loop::AgentLoop;
use crate::channel::{ChannelManager, CliChannelFactory};
use crate::message_bus::MessageBus;

#[tokio::main]
async fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().collect();

    // Handle setup subcommand
    if args.len() > 1 && args[1] == "setup" {
        let config_path = if args.len() > 2 {
            Some(args[2].as_str())
        } else {
            None
        };
        return setup::run_setup(config_path);
    }

    let config_path = if args.len() > 1 {
        args[1].clone()
    } else {
        // Default: ~/.slimbot/config.json
        let home = dirs::home_dir()
            .map(|p| p.to_string_lossy().to_string())
            .unwrap_or_else(|| ".".to_string());
        format!("{}/.slimbot/config.json", home)
    };

    eprintln!("SlimBot starting... config file: {}", config_path);

    // Initialize AgentLoop
    let agent_loop = AgentLoop::from_config(&config_path).await?;
    let agent_loop = Arc::new(agent_loop);

    // Initialize MessageBus
    let message_bus = Arc::new(MessageBus::new(agent_loop));

    // Initialize ChannelManager
    let mut channel_manager = ChannelManager::new(message_bus);

    // Load channels from config
    // Need to reload config to get channel entries
    let config = crate::config::Config::load(&config_path)?;
    channel_manager.init_from_config(&config.channels, &[Box::new(CliChannelFactory)])?;

    // Start channel loop
    channel_manager.run().await?;

    Ok(())
}
