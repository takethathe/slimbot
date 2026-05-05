use std::sync::Arc;

use anyhow::Result;

use crate::agent_loop::AgentLoop;
use crate::config::Config;
use crate::message_bus::MessageBus;
use crate::path::PathManager;
use crate::channel::ChannelManager;
use crate::worker::WorkerPool;
use crate::{debug, info};

/// Run the gateway: start cron, heartbeat, enabled channels, and block on outbound routing.
pub async fn run_gateway(paths: &PathManager) -> Result<()> {
    WorkerPool::init_global(64);

    let config = Arc::new(Config::load(paths.config_path().to_str().unwrap())?);
    let message_bus = Arc::new(MessageBus::new());

    let agent_loop = AgentLoop::from_config(paths, message_bus.clone(), config.clone()).await?;

    let mut channel_manager = ChannelManager::new(message_bus.clone(), config.clone());
    channel_manager.init().await?;

    // Start channels with scheduler
    // TODO: start cron, heartbeat, channels

    // Start inbound listener
    agent_loop.start_inbound(None);

    info!("[gateway] Running, press Ctrl+C to stop");

    // Wait for Ctrl+C
    tokio::signal::ctrl_c().await?;
    info!("[gateway] Shutdown signal received");

    // Graceful shutdown
    agent_loop.graceful_shutdown(&Arc::new(channel_manager)).await;
    Ok(())
}
