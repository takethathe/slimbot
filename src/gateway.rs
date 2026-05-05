use std::sync::Arc;
use anyhow::Result;
use tokio::signal;

use crate::agent_loop::AgentLoop;
use crate::config::Config;
use crate::message_bus::{BusResult, MessageBus};
use crate::path::PathManager;
use crate::channel::ChannelManager;
use crate::cron::CronService;
use crate::heartbeat::HeartbeatService;
use crate::tools::message::MessageTool;
use crate::tools::cron::CronTool;
use crate::tool::ToolManager;
use crate::session::TaskHook;
use crate::WorkerPool;
use crate::info;

pub async fn run_gateway(paths: &PathManager) -> Result<()> {
    WorkerPool::init_global(64);

    let config = Arc::new(Config::load(paths.config_path().to_str().unwrap())?);
    let message_bus = Arc::new(MessageBus::new());

    // Build ToolManager with all tools including message + cron
    let mut tool_manager = ToolManager::new(paths.workspace_dir().to_path_buf());
    tool_manager.init_from_config(&config.tools);

    // Create message tool with send callback
    let mut message_tool = MessageTool::new();
    let outbound_tx = message_bus.outbound_tx();
    message_tool.set_send_callback(Arc::new(move |channel, chat_id, content| {
        let tx = outbound_tx.clone();
        Box::pin(async move {
            let _ = tx.send(BusResult {
                session_id: format!("{}:{}", channel, chat_id),
                task_id: String::new(),
                content,
            }).await;
        })
    }));
    tool_manager.register(Box::new(message_tool));

    // Create cron service and cron tool
    let cron_service = CronService::new(paths.workspace_dir());
    let cron_service_arc = Arc::new(cron_service);
    let cron_tool = CronTool::new(cron_service_arc.clone());
    tool_manager.register(Box::new(cron_tool));

    // Create AgentLoop with pre-configured tool manager
    let agent_loop = Arc::new(AgentLoop::from_config_with_tools(
        paths,
        message_bus.clone(),
        config.clone(),
        tool_manager,
    ).await?);

    // Set up cron service callback (now takes &self via interior mutability)
    let agent_loop_for_cron = agent_loop.clone();
    cron_service_arc.set_on_job(Arc::new(move |job| {
        let al = agent_loop_for_cron.clone();
        let job_clone = job.clone();
        Box::pin(async move {
            if job.name == "dream" {
                return;
            }
            let session_id = format!("cron:{}", job_clone.id);
            let hook = TaskHook::new(&session_id);
            let content = format!(
                "[Scheduled Task] Timer finished.\n\nTask '{}' has been triggered.\nScheduled instruction: {}",
                job_clone.name, job_clone.payload.message
            );
            let _ = al.run_task(&session_id, content, hook, None).await;
        })
    }));

    if config.gateway.cron.enabled {
        cron_service_arc.start();
    }

    // Set up heartbeat service
    let mut heartbeat = HeartbeatService::new(
        paths.workspace_dir().to_path_buf(),
        config.gateway.heartbeat.interval_s,
        config.gateway.heartbeat.enabled,
    );

    let agent_loop_for_hb = agent_loop.clone();
    heartbeat.set_on_execute(Arc::new(move |content| {
        let al = agent_loop_for_hb.clone();
        Box::pin(async move {
            let session_id = "heartbeat:system";
            let hook = TaskHook::new(session_id);
            let result = al.run_task(session_id, content, hook, None).await;
            result.content
        })
    }));

    let mb_for_notify = message_bus.clone();
    heartbeat.set_on_notify(Arc::new(move |response| {
        let mb = mb_for_notify.clone();
        let resp = response.clone();
        Box::pin(async move {
            let _ = mb.outbound_tx().send(BusResult {
                session_id: "webui:webui_main".to_string(),
                task_id: String::new(),
                content: resp,
            }).await;
        })
    }));

    if config.gateway.heartbeat.enabled {
        heartbeat.start();
    }

    // ChannelManager: init channels from config
    let mut channel_manager = ChannelManager::new(message_bus.clone(), config.clone());

    // Register webui factory if webui channel is in config
    if config.channels.contains_key("webui") {
        channel_manager.register_webui_factory(agent_loop.session_manager());
    }

    channel_manager.init().await?;

    // Start inbound listener
    agent_loop.start_inbound(None);

    // Set up shutdown broadcast for channel-tier commands
    let (shutdown_tx, _) = tokio::sync::broadcast::channel::<()>(1);
    channel_manager.set_quit_broadcast(shutdown_tx.clone());

    // Spawn outbound router
    let cm = Arc::new(channel_manager);
    let cm_for_shutdown = cm.clone();
    let shutdown_rx = shutdown_tx.subscribe();
    tokio::spawn(async move {
        cm.run_with_shutdown(shutdown_rx).await;
    });

    // Spawn heartbeat tick loop
    let hb_arc = Arc::new(heartbeat);
    let hb_for_shutdown = hb_arc.clone();
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(hb_arc.sleep_duration()).await;
            hb_arc.tick().await;
        }
    });

    // Spawn cron tick loop
    tokio::spawn({
        let cron = cron_service_arc.clone();
        async move {
            loop {
                tokio::time::sleep(std::time::Duration::from_secs(1)).await;
                cron.tick().await;
            }
        }
    });

    info!("[gateway] Running, press Ctrl+C to stop");

    // Wait for Ctrl+C
    if let Err(e) = signal::ctrl_c().await {
        crate::error!("[gateway] Ctrl+C handler error: {}", e);
    }
    info!("[gateway] Shutdown signal received");

    // Graceful shutdown
    hb_for_shutdown.stop();
    cron_service_arc.stop();
    agent_loop.graceful_shutdown(&cm_for_shutdown).await;

    Ok(())
}
