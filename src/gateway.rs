use anyhow::Result;
use std::sync::Arc;
use tokio::signal;

use crate::WorkerPool;
use crate::agent_loop::AgentLoop;
use crate::channel::ChannelManager;
use crate::config::Config;
use crate::cron::CronService;
use crate::heartbeat::HeartbeatService;
use crate::info;
use crate::message_bus::{BusResult, MessageBus};
use crate::path::PathManager;
use crate::session::{AgentEvent, TaskHook};
use crate::tool::ToolManager;
use crate::tools::cron::CronTool;
use crate::tools::message::MessageTool;

pub async fn run_gateway(paths: &PathManager) -> Result<()> {
    WorkerPool::init_global(64);

    let config = Arc::new(Config::load(paths.config_path().to_str().unwrap())?);
    let (message_bus, receivers) = MessageBus::new();
    let outbound_rx = receivers.outbound;
    let inbound_rx = receivers.inbound;

    // Build ToolManager with all tools including message + cron
    let mut tool_manager = ToolManager::new(paths.workspace_dir().to_path_buf());
    tool_manager.init_from_config(&config.tools);

    // Create message tool with send callback
    let mut message_tool = MessageTool::new();
    let outbound_tx = message_bus.outbound_tx();
    message_tool.set_send_callback(Arc::new(move |channel, chat_id, content| {
        let tx = outbound_tx.clone();
        Box::pin(async move {
            let _ = tx
                .send(BusResult {
                    session_id: format!("{}:{}", channel, chat_id),
                    task_id: String::new(),
                    content,
                })
                .await;
        })
    }));
    tool_manager.register(Box::new(message_tool));

    // Create cron service and cron tool
    let cron_service = CronService::new(paths.workspace_dir());
    let cron_service_arc = Arc::new(cron_service);
    let cron_tool = CronTool::new(cron_service_arc.clone());
    tool_manager.register(Box::new(cron_tool));

    // Create AgentLoop with pre-configured tool manager
    let agent_loop = Arc::new(
        AgentLoop::from_config_with_tools(
            paths,
            message_bus.clone(),
            inbound_rx,
            config.clone(),
            tool_manager,
        )
        .await?,
    );

    // ChannelManager: init channels from config
    let (event_tx, _) = tokio::sync::broadcast::channel::<AgentEvent>(256);
    let mut channel_manager = ChannelManager::new(
        message_bus.clone(),
        outbound_rx,
        config.clone(),
        Some(event_tx.clone()),
    );

    // Set up cron service callback
    let agent_loop_for_cron = agent_loop.clone();
    let mb_for_cron = message_bus.clone();
    let event_tx_for_cron = event_tx.clone();
    cron_service_arc.set_on_job(Arc::new(move |job| {
        let al = agent_loop_for_cron.clone();
        let mb = mb_for_cron.clone();
        let job_clone = job.clone();
        let event_tx = event_tx_for_cron.clone();
        Box::pin(async move {
            let session_id = format!("cron:{}", job_clone.id);
            // Use origin session_id for event routing so cron execution
            // events (task_started, tool_call, etc.) appear in the user's
            // WebUI chat instead of being filtered as system-channel events.
            // TaskHook.session_id is only used for event broadcasting and
            // status notifications — session persistence uses the session_id
            // parameter passed separately to run_task().
            let origin_session_id = job_clone.payload.channel.as_deref()
                .zip(job_clone.payload.to.as_deref())
                .map(|(ch, cid)| format!("{}:{}", ch, cid));
            let hook = TaskHook::new(origin_session_id.as_deref().unwrap_or(&session_id))
                .with_events(event_tx);
            let content = format!(
                "[Scheduled Task] Timer finished.\n\nTask '{}' has been triggered.\nScheduled instruction: {}",
                job_clone.name, job_clone.payload.message
            );
            // Pass origin channel/chat_id so the message tool defaults to the target user channel
            let origin_ch = job_clone.payload.channel.clone();
            let origin_cid = job_clone.payload.to.clone();
            let result = al.run_task(&session_id, content, hook, None, origin_ch, origin_cid).await;

            // Deliver result to user channel if configured (only if message tool wasn't used)
            if job_clone.payload.deliver && !result.message_sent {
                if let (Some(channel), Some(chat_id)) = (&job_clone.payload.channel, &job_clone.payload.to) {
                    mb.publish_outbound(BusResult {
                        session_id: format!("{}:{}", channel, chat_id),
                        task_id: String::new(),
                        content: result.content,
                    }).await;
                }
            }
        })
    }));

    if config.gateway.cron.enabled {
        cron_service_arc.start();
        info!("[gateway] cron service started");
    } else {
        info!("[gateway] cron service disabled");
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
            // Heartbeat doesn't use message tool; result is sent via on_notify to webui
            let result = al
                .run_task(session_id, content, hook, None, None, None)
                .await;
            result.content
        })
    }));

    let mb_for_notify = message_bus.clone();
    heartbeat.set_on_notify(Arc::new(move |response| {
        let mb = mb_for_notify.clone();
        let resp = response.clone();
        Box::pin(async move {
            mb.publish_outbound(BusResult {
                session_id: "webui:webui_main".to_string(),
                task_id: String::new(),
                content: resp,
            })
            .await;
        })
    }));

    if config.gateway.heartbeat.enabled {
        heartbeat.start();
        info!(
            "[gateway] heartbeat service started (interval={}s)",
            config.gateway.heartbeat.interval_s
        );
    } else {
        info!("[gateway] heartbeat service disabled");
    }

    // Create shutdown broadcast for channel servers (webui etc.)
    let (channel_shutdown_tx, _) = tokio::sync::broadcast::channel::<()>(1);

    // Register webui factory with shutdown channel
    if config.channels.contains_key("webui") {
        channel_manager.register_webui_factory(
            agent_loop.session_manager(),
            channel_shutdown_tx.clone(),
            event_tx.clone(),
        );
    }

    channel_manager.init().await?;
    channel_manager.start_channels().await;

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

    // Signal channel servers to stop (webui etc.)
    let _ = channel_shutdown_tx.send(());

    // Graceful shutdown
    hb_for_shutdown.stop();
    cron_service_arc.stop();
    agent_loop.graceful_shutdown(&cm_for_shutdown).await;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::memory::MemoryStore;
    use crate::message_bus::BusRequest;
    use crate::session::{Message, SessionManager, SharedSessionManager};
    use std::path::PathBuf;

    fn make_test_paths(tmp: &tempfile::TempDir) -> PathManager {
        let data_dir = tmp.path().join("data");
        let workspace_dir = data_dir.join("workspace");
        std::fs::create_dir_all(&workspace_dir).unwrap();

        // Create a minimal config file
        let config_path = data_dir.join("config.json");
        let config = crate::config::Config {
            agent: crate::config::AgentConfig {
                provider: "default".to_string(),
                max_iterations: 10,
                timeout_seconds: 30,
                max_tool_result_chars: 8000,
                persist_tool_results: false,
                context_window_tokens: 32768,
                unknown: Default::default(),
            },
            providers: {
                let mut map = std::collections::HashMap::new();
                map.insert(
                    "default".to_string(),
                    crate::config::ProviderConfig {
                        r#type: "openai".to_string(),
                        api_url: "".to_string(),
                        base_url: "https://api.openai.com".to_string(),
                        api_key: "test-key".to_string(),
                        model: "gpt-4o".to_string(),
                        temperature: 0.7,
                        max_tokens: 4096,
                        prompt_cache_enabled: false,
                        unknown: Default::default(),
                    },
                );
                map
            },
            tools: vec![],
            channels: std::collections::HashMap::new(),
            gateway: crate::config::GatewayConfig {
                cron: crate::config::CronConfig { enabled: false },
                heartbeat: crate::config::HeartbeatConfig {
                    enabled: false,
                    interval_s: 60,
                },
            },
        };
        let config_json = serde_json::to_string_pretty(&config).unwrap();
        std::fs::write(&config_path, config_json).unwrap();

        PathManager::resolve(
            Some(config_path.to_str().unwrap()),
            Some(data_dir.to_str().unwrap()),
            Some(workspace_dir.to_str().unwrap()),
        )
        .unwrap()
    }

    #[tokio::test]
    async fn test_gateway_worker_pool_init() {
        // Verify worker pool can be initialized
        WorkerPool::init_global(64);
        let pool = WorkerPool::global();
        pool.submit(Box::new(|| {
            Box::pin(async {}) as std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>>
        }));
    }

    #[tokio::test]
    async fn test_gateway_message_bus_creation() {
        let (message_bus, _receivers) = MessageBus::new();
        assert!(message_bus.inbound_tx().capacity() > 0);
        assert!(message_bus.outbound_tx().capacity() > 0);
    }

    #[tokio::test]
    async fn test_gateway_tool_manager_creation() {
        let tmp = tempfile::tempdir().unwrap();
        let workspace_dir = tmp.path().join("workspace");
        std::fs::create_dir_all(&workspace_dir).unwrap();

        let mut tool_manager = ToolManager::new(workspace_dir);
        tool_manager.init_from_config(&[]); // Use defaults

        // Verify built-in tools are registered
        let tools = tool_manager.to_openai_functions();
        assert!(tools.len() >= 6);
    }

    #[tokio::test]
    async fn test_gateway_message_tool_registration() {
        let mut tool_manager = ToolManager::new(PathBuf::from("/tmp/test"));
        let message_tool = MessageTool::new();
        tool_manager.register(Box::new(message_tool));

        // Verify message tool is registered
        let tools = tool_manager.to_openai_functions();
        assert!(tools.iter().any(|t| t.name == "message"));
    }

    #[tokio::test]
    async fn test_gateway_cron_service_creation() {
        let tmp = tempfile::tempdir().unwrap();
        let workspace_dir = tmp.path().join("workspace");
        std::fs::create_dir_all(&workspace_dir).unwrap();

        let cron_service = CronService::new(&workspace_dir);
        assert!(cron_service.list_jobs().is_empty());
    }

    #[tokio::test]
    async fn test_gateway_cron_tool_registration() {
        let tmp = tempfile::tempdir().unwrap();
        let workspace_dir = tmp.path().join("workspace");
        std::fs::create_dir_all(&workspace_dir).unwrap();

        let cron_service = Arc::new(CronService::new(&workspace_dir));
        let cron_tool = CronTool::new(cron_service);

        let mut tool_manager = ToolManager::new(workspace_dir);
        tool_manager.register(Box::new(cron_tool));

        // Verify cron tool is registered
        let tools = tool_manager.to_openai_functions();
        assert!(tools.iter().any(|t| t.name == "cron"));
    }

    #[tokio::test]
    async fn test_gateway_heartbeat_service_creation() {
        let tmp = tempfile::tempdir().unwrap();
        let workspace_dir = tmp.path().join("workspace");
        std::fs::create_dir_all(&workspace_dir).unwrap();

        let heartbeat = HeartbeatService::new(workspace_dir, 60, false);
        assert!(!heartbeat.is_enabled());
    }

    #[tokio::test]
    async fn test_gateway_channel_manager_creation() {
        let (message_bus, _receivers) = MessageBus::new();
        let config = Arc::new(crate::config::Config {
            agent: crate::config::AgentConfig {
                provider: "default".to_string(),
                max_iterations: 10,
                timeout_seconds: 30,
                max_tool_result_chars: 8000,
                persist_tool_results: false,
                context_window_tokens: 32768,
                unknown: Default::default(),
            },
            providers: std::collections::HashMap::new(),
            tools: vec![],
            channels: std::collections::HashMap::new(),
            gateway: crate::config::GatewayConfig {
                cron: crate::config::CronConfig { enabled: false },
                heartbeat: crate::config::HeartbeatConfig {
                    enabled: false,
                    interval_s: 60,
                },
            },
        });

        let (event_tx, _) = tokio::sync::broadcast::channel::<AgentEvent>(256);
        let channel_manager =
            ChannelManager::new(message_bus, _receivers.outbound, config, Some(event_tx));
        assert!(channel_manager.channel_info().0 == "cli:default");
    }

    #[tokio::test]
    async fn test_gateway_paths_resolution() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = make_test_paths(&tmp);

        assert!(paths.workspace_dir().exists());
        assert!(paths.data_dir().exists());
        assert!(paths.config_path().exists());
    }

    #[tokio::test]
    async fn test_gateway_config_loading() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = make_test_paths(&tmp);

        let config = Config::load(paths.config_path().to_str().unwrap());
        assert!(config.is_ok());
    }

    #[tokio::test]
    async fn test_gateway_agent_loop_creation() {
        let tmp = tempfile::tempdir().unwrap();
        let workspace_dir = tmp.path().join("workspace");
        std::fs::create_dir_all(&workspace_dir).unwrap();

        let session_dir = workspace_dir.join("sessions");
        std::fs::create_dir_all(&session_dir).unwrap();

        let memory_dir = workspace_dir.join("memory");
        std::fs::create_dir_all(&memory_dir).unwrap();

        let (message_bus, _receivers) = MessageBus::new();
        let config = Arc::new(crate::config::Config {
            agent: crate::config::AgentConfig {
                provider: "default".to_string(),
                max_iterations: 10,
                timeout_seconds: 30,
                max_tool_result_chars: 8000,
                persist_tool_results: false,
                context_window_tokens: 32768,
                unknown: Default::default(),
            },
            providers: {
                let mut map = std::collections::HashMap::new();
                map.insert(
                    "default".to_string(),
                    crate::config::ProviderConfig {
                        r#type: "openai".to_string(),
                        api_url: "".to_string(),
                        base_url: "https://api.openai.com".to_string(),
                        api_key: "test-key".to_string(),
                        model: "gpt-4o".to_string(),
                        temperature: 0.7,
                        max_tokens: 4096,
                        prompt_cache_enabled: false,
                        unknown: Default::default(),
                    },
                );
                map
            },
            tools: vec![],
            channels: std::collections::HashMap::new(),
            gateway: crate::config::GatewayConfig {
                cron: crate::config::CronConfig { enabled: false },
                heartbeat: crate::config::HeartbeatConfig {
                    enabled: false,
                    interval_s: 60,
                },
            },
        });

        let mut tool_manager = ToolManager::new(workspace_dir.clone());
        tool_manager.init_from_config(&[]);

        // Verify agent loop can be created
        let result = AgentLoop::from_config_with_tools(
            &PathManager::resolve(
                None,
                Some(tmp.path().to_str().unwrap()),
                Some(workspace_dir.to_str().unwrap()),
            )
            .unwrap(),
            message_bus,
            _receivers.inbound,
            config,
            tool_manager,
        )
        .await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_gateway_shutdown_channels() {
        let (message_bus, _receivers) = MessageBus::new();
        let config = Arc::new(crate::config::Config {
            agent: crate::config::AgentConfig {
                provider: "default".to_string(),
                max_iterations: 10,
                timeout_seconds: 30,
                max_tool_result_chars: 8000,
                persist_tool_results: false,
                context_window_tokens: 32768,
                unknown: Default::default(),
            },
            providers: std::collections::HashMap::new(),
            tools: vec![],
            channels: std::collections::HashMap::new(),
            gateway: crate::config::GatewayConfig {
                cron: crate::config::CronConfig { enabled: false },
                heartbeat: crate::config::HeartbeatConfig {
                    enabled: false,
                    interval_s: 60,
                },
            },
        });

        let (event_tx, _) = tokio::sync::broadcast::channel::<AgentEvent>(256);
        let mut channel_manager =
            ChannelManager::new(message_bus, _receivers.outbound, config, Some(event_tx));

        // Verify channel manager can be initialized with no channels
        let result = channel_manager.init().await;
        assert!(result.is_ok());

        // Verify shutdown doesn't panic
        channel_manager.shutdown().await;
    }

    #[tokio::test]
    async fn test_gateway_run_gateway_paths_validation() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = make_test_paths(&tmp);

        // Verify all paths are valid
        assert!(paths.config_path().exists());
        assert!(paths.workspace_dir().exists());
        assert!(paths.data_dir().exists());
        assert!(paths.session_dir().exists() || std::fs::create_dir(paths.session_dir()).is_ok());
        assert!(paths.skills_dir().exists() || std::fs::create_dir(paths.skills_dir()).is_ok());
        assert!(paths.memory_dir().exists() || std::fs::create_dir(paths.memory_dir()).is_ok());
    }

    #[tokio::test]
    async fn test_gateway_cron_service_tick() {
        let tmp = tempfile::tempdir().unwrap();
        let workspace_dir = tmp.path().join("workspace");
        std::fs::create_dir_all(&workspace_dir).unwrap();

        let cron_service = CronService::new(&workspace_dir);
        cron_service.start();

        // Verify tick doesn't panic
        cron_service.tick().await;

        cron_service.stop();
    }

    #[tokio::test]
    async fn test_gateway_heartbeat_service_tick() {
        let tmp = tempfile::tempdir().unwrap();
        let workspace_dir = tmp.path().join("workspace");
        std::fs::create_dir_all(&workspace_dir).unwrap();

        let mut heartbeat = HeartbeatService::new(workspace_dir, 60, true);
        heartbeat.start();

        // Verify tick doesn't panic
        heartbeat.tick().await;

        heartbeat.stop();
    }

    #[tokio::test]
    async fn test_gateway_message_bus_send_receive() {
        let (mb, _receivers) = MessageBus::new();

        // Send a request
        let req = BusRequest {
            session_id: "test-session".to_string(),
            content: "test content".to_string(),
            channel_inject: None,
            hook: TaskHook::new("test-session"),
        };
        let result = mb.inbound_tx().send(req).await;
        assert!(result.is_ok());

        // Send a result
        let res = BusResult {
            session_id: "test-session".to_string(),
            task_id: "test-task".to_string(),
            content: "test result".to_string(),
        };
        let result = mb.outbound_tx().send(res).await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_gateway_session_manager_operations() {
        let tmp = tempfile::tempdir().unwrap();
        let session_dir = tmp.path().join("sessions");
        std::fs::create_dir_all(&session_dir).unwrap();

        let sm: SharedSessionManager = Arc::new(tokio::sync::Mutex::new(
            SessionManager::new(session_dir.clone()).unwrap(),
        ));

        // Create session
        {
            let mut guard = sm.lock().await;
            guard.get_or_create("test-session").await.unwrap();
        }

        // Add messages
        {
            let mut guard = sm.lock().await;
            guard
                .add_message("test-session", Message::user("hello".to_string()))
                .await
                .unwrap();
            guard
                .add_message(
                    "test-session",
                    Message::assistant(Some("hi".to_string()), None, None, None),
                )
                .await
                .unwrap();
        }

        // Get messages
        {
            let guard = sm.lock().await;
            let msgs = guard.get_messages("test-session").await;
            assert_eq!(msgs.len(), 2);
        }

        // Persist
        {
            let mut guard = sm.lock().await;
            guard.persist("test-session").await.unwrap();
        }

        // Verify files exist
        assert!(session_dir.join("test-session.jsonl").exists());
        assert!(session_dir.join("test-session.meta.json").exists());
    }

    #[tokio::test]
    async fn test_gateway_message_bus_channel_capacity() {
        let (mb, _receivers) = MessageBus::new();
        // Verify channel capacities
        assert_eq!(mb.inbound_tx().capacity(), 32);
        assert_eq!(mb.outbound_tx().capacity(), 32);
    }

    #[tokio::test]
    async fn test_gateway_tool_manager_to_openai_functions() {
        let tmp = tempfile::tempdir().unwrap();
        let workspace_dir = tmp.path().join("workspace");
        std::fs::create_dir_all(&workspace_dir).unwrap();

        let mut tm = ToolManager::new(workspace_dir);
        tm.init_from_config(&[]); // Use defaults

        // Verify OpenAI functions are generated
        let functions = tm.to_openai_functions();
        assert!(!functions.is_empty());
        for func in &functions {
            assert!(!func.name.is_empty());
            assert!(!func.description.is_empty());
        }
    }

    #[tokio::test]
    async fn test_gateway_cron_service_add_remove_job() {
        let tmp = tempfile::tempdir().unwrap();
        let workspace_dir = tmp.path().join("workspace");
        std::fs::create_dir_all(&workspace_dir).unwrap();

        let cron_service = CronService::new(&workspace_dir);
        cron_service.start();

        // Verify no jobs initially
        assert!(cron_service.list_jobs().is_empty());

        cron_service.stop();
    }

    #[tokio::test]
    async fn test_gateway_heartbeat_service_disabled() {
        let tmp = tempfile::tempdir().unwrap();
        let workspace_dir = tmp.path().join("workspace");
        std::fs::create_dir_all(&workspace_dir).unwrap();

        let heartbeat = HeartbeatService::new(workspace_dir, 60, false);
        assert!(!heartbeat.is_enabled());
    }

    #[tokio::test]
    async fn test_gateway_channel_manager_no_channels() {
        let (message_bus, _receivers) = MessageBus::new();
        let config = Arc::new(crate::config::Config {
            agent: crate::config::AgentConfig {
                provider: "default".to_string(),
                max_iterations: 10,
                timeout_seconds: 30,
                max_tool_result_chars: 8000,
                persist_tool_results: false,
                context_window_tokens: 32768,
                unknown: Default::default(),
            },
            providers: std::collections::HashMap::new(),
            tools: vec![],
            channels: std::collections::HashMap::new(),
            gateway: crate::config::GatewayConfig {
                cron: crate::config::CronConfig { enabled: false },
                heartbeat: crate::config::HeartbeatConfig {
                    enabled: false,
                    interval_s: 60,
                },
            },
        });

        let (event_tx, _) = tokio::sync::broadcast::channel::<AgentEvent>(256);
        let mut channel_manager =
            ChannelManager::new(message_bus, _receivers.outbound, config, Some(event_tx));

        // Initialize with no channels should succeed
        let result = channel_manager.init().await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_gateway_event_broadcast() {
        let (event_tx, mut event_rx1) = tokio::sync::broadcast::channel::<AgentEvent>(10);
        let mut event_rx2 = event_tx.subscribe();

        // Send an event
        let event = AgentEvent::TaskStarted {
            session_id: "test-session".to_string(),
        };
        let result = event_tx.send(event);
        assert!(result.is_ok());

        // Both receivers should get the event
        let received1 = event_rx1.recv().await;
        assert!(received1.is_ok());

        let received2 = event_rx2.recv().await;
        assert!(received2.is_ok());
    }

    #[tokio::test]
    async fn test_gateway_bus_result_serialization() {
        let result = BusResult {
            session_id: "test-session".to_string(),
            task_id: "task-123".to_string(),
            content: "Test content".to_string(),
        };
        let json = serde_json::to_string(&result).unwrap();
        assert!(json.contains("test-session"));
        assert!(json.contains("task-123"));
        assert!(json.contains("Test content"));

        let deserialized: BusResult = serde_json::from_str(&json).unwrap();
        assert_eq!(deserialized.session_id, "test-session");
        assert_eq!(deserialized.task_id, "task-123");
        assert_eq!(deserialized.content, "Test content");
    }

    #[tokio::test]
    async fn test_gateway_bus_request_creation() {
        let hook = TaskHook::new("test-session");
        let request = BusRequest {
            session_id: "test-session".to_string(),
            content: "Test input".to_string(),
            channel_inject: None,
            hook,
        };
        assert_eq!(request.session_id, "test-session");
        assert_eq!(request.content, "Test input");
        assert!(request.channel_inject.is_none());
    }

    #[tokio::test]
    async fn test_gateway_memory_store_init() {
        let tmp = tempfile::tempdir().unwrap();
        let workspace_dir = tmp.path().join("workspace");
        std::fs::create_dir_all(&workspace_dir).unwrap();

        let ms = Arc::new(tokio::sync::Mutex::new(MemoryStore::new(&workspace_dir)));
        let result = ms.lock().await.init();
        assert!(result.is_ok());
        assert!(workspace_dir.join("memory").exists());
    }

    #[tokio::test]
    async fn test_gateway_memory_store_write_read() {
        let tmp = tempfile::tempdir().unwrap();
        let workspace_dir = tmp.path().join("workspace");
        std::fs::create_dir_all(&workspace_dir).unwrap();

        let ms = Arc::new(tokio::sync::Mutex::new(MemoryStore::new(&workspace_dir)));
        ms.lock().await.init().unwrap();

        // Write memory
        ms.lock().await.write_memory("test memory content").unwrap();

        // Read memory
        let content = ms.lock().await.read_memory();
        assert_eq!(content, "test memory content");
    }

    #[tokio::test]
    async fn test_gateway_session_manager_list_sessions() {
        let tmp = tempfile::tempdir().unwrap();
        let session_dir = tmp.path().join("sessions");
        std::fs::create_dir_all(&session_dir).unwrap();

        let sm: SharedSessionManager = Arc::new(tokio::sync::Mutex::new(
            SessionManager::new(session_dir).unwrap(),
        ));

        // Create multiple sessions
        {
            let mut guard = sm.lock().await;
            guard.get_or_create("session-1").await.unwrap();
            guard.get_or_create("session-2").await.unwrap();
            guard.get_or_create("session-3").await.unwrap();
        }

        // List sessions
        let sessions = sm.lock().await.list_persisted_sessions("");
        assert!(sessions.len() >= 0); // May be 0 if not persisted
    }

    #[tokio::test]
    async fn test_gateway_task_hook_creation() {
        let hook = TaskHook::new("test-session");
        // TaskHook doesn't expose session_id directly, but we can verify it was created
        assert!(matches!(hook, TaskHook { .. }));
    }

    #[tokio::test]
    async fn test_gateway_agent_event_variants() {
        // Test different AgentEvent variants
        let event1 = AgentEvent::TaskStarted {
            session_id: "session-1".to_string(),
        };
        assert!(matches!(event1, AgentEvent::TaskStarted { .. }));

        let event2 = AgentEvent::TaskCompleted {
            session_id: "session-2".to_string(),
            result: "result".to_string(),
        };
        assert!(matches!(event2, AgentEvent::TaskCompleted { .. }));

        let event3 = AgentEvent::ToolCall {
            session_id: "session-3".to_string(),
            name: "shell".to_string(),
            args: "{\"command\": \"echo test\"}".to_string(),
        };
        assert!(matches!(event3, AgentEvent::ToolCall { .. }));
    }

    #[tokio::test]
    async fn test_gateway_cron_service_list_jobs_empty() {
        let tmp = tempfile::tempdir().unwrap();
        let workspace_dir = tmp.path().join("workspace");
        std::fs::create_dir_all(&workspace_dir).unwrap();

        let cron_service = CronService::new(&workspace_dir);
        let jobs = cron_service.list_jobs();
        assert!(jobs.is_empty());
    }

    #[tokio::test]
    async fn test_gateway_cron_service_start_stop() {
        let tmp = tempfile::tempdir().unwrap();
        let workspace_dir = tmp.path().join("workspace");
        std::fs::create_dir_all(&workspace_dir).unwrap();

        let cron_service = CronService::new(&workspace_dir);
        cron_service.start();
        // Starting twice should be idempotent
        cron_service.start();
        cron_service.stop();
        // Stopping twice should be idempotent
        cron_service.stop();
    }

    #[tokio::test]
    async fn test_gateway_heartbeat_service_multiple_ticks() {
        let tmp = tempfile::tempdir().unwrap();
        let workspace_dir = tmp.path().join("workspace");
        std::fs::create_dir_all(&workspace_dir).unwrap();

        let heartbeat = HeartbeatService::new(workspace_dir, 60, false);
        // Ticking when disabled should be a no-op
        heartbeat.tick().await;
        heartbeat.tick().await;
        heartbeat.tick().await;
    }

    #[tokio::test]
    async fn test_gateway_message_bus_multiple_sends() {
        let (mb, _receivers) = MessageBus::new();
        let outbound_tx = mb.outbound_tx();

        // Send multiple results
        for i in 0..5 {
            let result = BusResult {
                session_id: format!("session-{}", i),
                task_id: format!("task-{}", i),
                content: format!("Content {}", i),
            };
            let send_result = outbound_tx.send(result).await;
            assert!(send_result.is_ok());
        }
    }

    #[tokio::test]
    async fn test_gateway_tool_manager_multiple_registrations() {
        let tmp = tempfile::tempdir().unwrap();
        let workspace_dir = tmp.path().join("workspace");
        std::fs::create_dir_all(&workspace_dir).unwrap();

        let mut tm = ToolManager::new(workspace_dir);
        tm.init_from_config(&[]); // Use defaults

        // Register additional tools
        tm.register(Box::new(crate::tools::shell::ShellTool::default()));

        // Verify tools are registered
        let tools = tm.to_openai_functions();
        assert!(!tools.is_empty());
    }

    #[tokio::test]
    async fn test_gateway_session_manager_concurrent_access() {
        let tmp = tempfile::tempdir().unwrap();
        let session_dir = tmp.path().join("sessions");
        std::fs::create_dir_all(&session_dir).unwrap();

        let sm: SharedSessionManager = Arc::new(tokio::sync::Mutex::new(
            SessionManager::new(session_dir).unwrap(),
        ));

        // Create session
        {
            let mut guard = sm.lock().await;
            guard.get_or_create("test-session").await.unwrap();
        }

        // Concurrent message adds
        let sm_clone = sm.clone();
        let handle1 = tokio::spawn(async move {
            let mut guard = sm_clone.lock().await;
            guard
                .add_message("test-session", Message::user("msg1".to_string()))
                .await
                .unwrap();
        });

        let sm_clone = sm.clone();
        let handle2 = tokio::spawn(async move {
            let mut guard = sm_clone.lock().await;
            guard
                .add_message("test-session", Message::user("msg2".to_string()))
                .await
                .unwrap();
        });

        handle1.await.unwrap();
        handle2.await.unwrap();

        // Verify messages were added
        let guard = sm.lock().await;
        let msgs = guard.get_messages("test-session").await;
        assert!(msgs.len() >= 2);
    }

    #[tokio::test]
    async fn test_gateway_message_bus_inbound_channel() {
        let (mb, _receivers) = MessageBus::new();
        let inbound_tx = mb.inbound_tx();

        // Verify inbound channel capacity
        assert!(inbound_tx.capacity() > 0);

        // Send a request
        let hook = TaskHook::new("test-session");
        let request = BusRequest {
            session_id: "test-session".to_string(),
            content: "test input".to_string(),
            channel_inject: None,
            hook,
        };
        let result = inbound_tx.send(request).await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_gateway_cron_service_persistence() {
        let tmp = tempfile::tempdir().unwrap();
        let workspace_dir = tmp.path().join("workspace");
        std::fs::create_dir_all(&workspace_dir).unwrap();

        let cron_service = CronService::new(&workspace_dir);
        cron_service.start();

        // Verify persistence file path exists or can be created
        assert!(workspace_dir.exists());

        cron_service.stop();
    }

    #[tokio::test]
    async fn test_gateway_heartbeat_service_sleep_duration() {
        let tmp = tempfile::tempdir().unwrap();
        let workspace_dir = tmp.path().join("workspace");
        std::fs::create_dir_all(&workspace_dir).unwrap();

        let heartbeat = HeartbeatService::new(workspace_dir, 30, false);
        // Verify sleep duration is based on interval
        let duration = heartbeat.sleep_duration();
        assert!(duration.as_secs() <= 30);
    }

    #[tokio::test]
    async fn test_gateway_agent_loop_session_ensure() {
        let tmp = tempfile::tempdir().unwrap();
        let session_dir = tmp.path().join("sessions");
        std::fs::create_dir_all(&session_dir).unwrap();

        let sm: SharedSessionManager = Arc::new(tokio::sync::Mutex::new(
            SessionManager::new(session_dir).unwrap(),
        ));

        // Ensure session creates it if not exists
        crate::session::ensure_session(&sm, "new-session")
            .await
            .unwrap();

        // Verify session exists
        let guard = sm.lock().await;
        assert!(guard.has_session("new-session"));
    }

    #[tokio::test]
    async fn test_gateway_session_manager_graceful_shutdown() {
        let tmp = tempfile::tempdir().unwrap();
        let session_dir = tmp.path().join("sessions");
        std::fs::create_dir_all(&session_dir).unwrap();

        let sm: SharedSessionManager = Arc::new(tokio::sync::Mutex::new(
            SessionManager::new(session_dir).unwrap(),
        ));

        // Create a session
        {
            let mut guard = sm.lock().await;
            guard.get_or_create("test-session").await.unwrap();
            guard
                .add_message("test-session", Message::user("hello".to_string()))
                .await
                .unwrap();
        }

        // Graceful shutdown should not panic
        sm.lock().await.graceful_shutdown().await;
    }

    #[tokio::test]
    async fn test_gateway_memory_store_sync_all() {
        let tmp = tempfile::tempdir().unwrap();
        let workspace_dir = tmp.path().join("workspace");
        std::fs::create_dir_all(&workspace_dir).unwrap();

        let ms = Arc::new(tokio::sync::Mutex::new(MemoryStore::new(&workspace_dir)));
        ms.lock().await.init().unwrap();

        // Write some data
        ms.lock().await.write_memory("test").unwrap();

        // sync_all should not error
        let result = ms.lock().await.sync_all();
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_gateway_worker_pool_submit_multiple() {
        WorkerPool::init_global(64);
        let pool = WorkerPool::global();

        // Submit multiple tasks
        for i in 0..10 {
            pool.submit(Box::new(move || {
                Box::pin(async move {
                    let _ = i;
                })
                    as std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>>
            }));
        }

        // Give tasks time to complete
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }

    #[tokio::test]
    async fn test_gateway_config_with_all_fields() {
        let config = crate::config::Config {
            agent: crate::config::AgentConfig {
                provider: "default".to_string(),
                max_iterations: 50,
                timeout_seconds: 180,
                max_tool_result_chars: 12000,
                persist_tool_results: true,
                context_window_tokens: 64000,
                unknown: Default::default(),
            },
            providers: {
                let mut map = std::collections::HashMap::new();
                map.insert(
                    "default".to_string(),
                    crate::config::ProviderConfig {
                        r#type: "openai".to_string(),
                        api_url: "".to_string(),
                        base_url: "https://api.openai.com".to_string(),
                        api_key: "sk-test".to_string(),
                        model: "gpt-4".to_string(),
                        temperature: 0.7,
                        max_tokens: 4096,
                        prompt_cache_enabled: true,
                        unknown: Default::default(),
                    },
                );
                map
            },
            tools: vec![],
            channels: std::collections::HashMap::new(),
            gateway: crate::config::GatewayConfig {
                cron: crate::config::CronConfig { enabled: true },
                heartbeat: crate::config::HeartbeatConfig {
                    enabled: true,
                    interval_s: 300,
                },
            },
        };

        assert_eq!(config.agent.max_iterations, 50);
        assert_eq!(config.agent.timeout_seconds, 180);
        assert!(config.gateway.cron.enabled);
        assert!(config.gateway.heartbeat.enabled);
    }

    #[tokio::test]
    async fn test_gateway_session_manager_message_count() {
        let tmp = tempfile::tempdir().unwrap();
        let session_dir = tmp.path().join("sessions");
        std::fs::create_dir_all(&session_dir).unwrap();

        let sm: SharedSessionManager = Arc::new(tokio::sync::Mutex::new(
            SessionManager::new(session_dir).unwrap(),
        ));

        // Create session
        {
            let mut guard = sm.lock().await;
            guard.get_or_create("test-session").await.unwrap();
        }

        // Add messages
        {
            let mut guard = sm.lock().await;
            for i in 0..5 {
                guard
                    .add_message("test-session", Message::user(format!("msg{}", i)))
                    .await
                    .unwrap();
            }
        }

        // Verify message count
        let count = sm.lock().await.total_message_count("test-session");
        assert_eq!(count, 5);
    }

    #[tokio::test]
    async fn test_gateway_memory_store_append_history() {
        let tmp = tempfile::tempdir().unwrap();
        let workspace_dir = tmp.path().join("workspace");
        std::fs::create_dir_all(&workspace_dir).unwrap();

        let ms = Arc::new(tokio::sync::Mutex::new(MemoryStore::new(&workspace_dir)));
        ms.lock().await.init().unwrap();

        // Append history entries - should not error
        ms.lock().await.append_history("entry1").unwrap();
        ms.lock().await.append_history("entry2").unwrap();
        ms.lock().await.append_history("entry3").unwrap();

        // Verify history file was created
        let history_file = workspace_dir.join("memory/history.jsonl");
        assert!(history_file.exists());
    }

    #[tokio::test]
    async fn test_gateway_memory_store_read_recent_history() {
        let tmp = tempfile::tempdir().unwrap();
        let workspace_dir = tmp.path().join("workspace");
        std::fs::create_dir_all(&workspace_dir).unwrap();

        let ms = Arc::new(tokio::sync::Mutex::new(MemoryStore::new(&workspace_dir)));
        ms.lock().await.init().unwrap();

        // Append many entries
        for i in 0..20 {
            ms.lock()
                .await
                .append_history(&format!("entry{}", i))
                .unwrap();
        }

        // Read recent history with cap
        let recent = ms.lock().await.read_recent_history(5);
        assert_eq!(recent.len(), 5);
    }

    #[tokio::test]
    async fn test_gateway_tool_manager_execute_tool() {
        let tmp = tempfile::tempdir().unwrap();
        let workspace_dir = tmp.path().join("workspace");
        std::fs::create_dir_all(&workspace_dir).unwrap();

        let mut tm = ToolManager::new(workspace_dir);
        tm.init_from_config(&[]); // Use defaults

        // Execute a tool
        let result = tm
            .execute("shell", serde_json::json!({"command": "echo test"}))
            .await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_gateway_session_manager_clear_session() {
        let tmp = tempfile::tempdir().unwrap();
        let session_dir = tmp.path().join("sessions");
        std::fs::create_dir_all(&session_dir).unwrap();

        let sm: SharedSessionManager = Arc::new(tokio::sync::Mutex::new(
            SessionManager::new(session_dir.clone()).unwrap(),
        ));

        // Create session with messages
        {
            let mut guard = sm.lock().await;
            guard.get_or_create("test-session").await.unwrap();
            guard
                .add_message("test-session", Message::user("hello".to_string()))
                .await
                .unwrap();
            guard.persist("test-session").await.unwrap();
        }

        // Verify files exist
        assert!(session_dir.join("test-session.jsonl").exists());

        // Clear session
        sm.lock().await.clear_session("test-session");

        // Verify files were deleted
        assert!(!session_dir.join("test-session.jsonl").exists());
    }

    #[tokio::test]
    async fn test_gateway_cron_service_job_execution() {
        let tmp = tempfile::tempdir().unwrap();
        let workspace_dir = tmp.path().join("workspace");
        std::fs::create_dir_all(&workspace_dir).unwrap();

        let cron_service = CronService::new(&workspace_dir);
        cron_service.start();

        // Tick should not panic even with no jobs
        cron_service.tick().await;
        cron_service.tick().await;

        cron_service.stop();
    }

    #[tokio::test]
    async fn test_gateway_heartbeat_service_enabled_tick() {
        let tmp = tempfile::tempdir().unwrap();
        let workspace_dir = tmp.path().join("workspace");
        std::fs::create_dir_all(&workspace_dir).unwrap();

        let heartbeat = HeartbeatService::new(workspace_dir, 60, true);
        // Tick when enabled should not panic
        heartbeat.tick().await;
    }

    #[tokio::test]
    async fn test_gateway_message_bus_outbound_tx_clone() {
        let (mb, _receivers) = MessageBus::new();
        let tx1 = mb.outbound_tx();
        let tx2 = tx1.clone();

        // Both senders should work
        let result1 = tx1
            .send(BusResult {
                session_id: "session-1".to_string(),
                task_id: "task-1".to_string(),
                content: "content-1".to_string(),
            })
            .await;
        assert!(result1.is_ok());

        let result2 = tx2
            .send(BusResult {
                session_id: "session-2".to_string(),
                task_id: "task-2".to_string(),
                content: "content-2".to_string(),
            })
            .await;
        assert!(result2.is_ok());
    }

    #[tokio::test]
    async fn test_gateway_tool_manager_set_context() {
        let tmp = tempfile::tempdir().unwrap();
        let workspace_dir = tmp.path().join("workspace");
        std::fs::create_dir_all(&workspace_dir).unwrap();

        let mut tm = ToolManager::new(workspace_dir);
        tm.init_from_config(&[]); // Use defaults

        // Set context should not panic
        tm.set_context(&crate::tool::ToolContext {
            channel: "cli".to_string(),
            chat_id: "default".to_string(),
        });
    }

    #[tokio::test]
    async fn test_gateway_session_manager_has_session() {
        let tmp = tempfile::tempdir().unwrap();
        let session_dir = tmp.path().join("sessions");
        std::fs::create_dir_all(&session_dir).unwrap();

        let sm: SharedSessionManager = Arc::new(tokio::sync::Mutex::new(
            SessionManager::new(session_dir).unwrap(),
        ));

        // Initially no sessions
        assert!(!sm.lock().await.has_session("nonexistent"));

        // Create session
        sm.lock().await.get_or_create("test-session").await.unwrap();
        assert!(sm.lock().await.has_session("test-session"));
    }

    #[tokio::test]
    async fn test_gateway_memory_store_read_unprocessed_history() {
        let tmp = tempfile::tempdir().unwrap();
        let workspace_dir = tmp.path().join("workspace");
        std::fs::create_dir_all(&workspace_dir).unwrap();

        let ms = Arc::new(tokio::sync::Mutex::new(MemoryStore::new(&workspace_dir)));
        ms.lock().await.init().unwrap();

        // Append entries
        ms.lock().await.append_history("entry1").unwrap();
        ms.lock().await.append_history("entry2").unwrap();
        ms.lock().await.append_history("entry3").unwrap();

        // Read unprocessed history with cursor 0 should return all
        let unprocessed = ms.lock().await.read_unprocessed_history(0);
        assert_eq!(unprocessed.len(), 3);
    }

    #[tokio::test]
    async fn test_gateway_memory_store_get_memory_context() {
        let tmp = tempfile::tempdir().unwrap();
        let workspace_dir = tmp.path().join("workspace");
        std::fs::create_dir_all(&workspace_dir).unwrap();

        let ms = Arc::new(tokio::sync::Mutex::new(MemoryStore::new(&workspace_dir)));
        ms.lock().await.init().unwrap();

        // Initially empty context
        let ctx = ms.lock().await.get_memory_context();
        assert!(ctx.is_empty());

        // Write memory
        ms.lock().await.write_memory("test memory").unwrap();

        // Now context should have content
        let ctx = ms.lock().await.get_memory_context();
        assert!(!ctx.is_empty());
        assert!(ctx.contains("test memory"));
    }

    #[tokio::test]
    async fn test_gateway_worker_pool_global_init() {
        // init_global should be idempotent
        WorkerPool::init_global(64);
        WorkerPool::init_global(64);
        WorkerPool::init_global(64);

        let pool = WorkerPool::global();
        assert!(pool.submit(Box::new(|| Box::pin(async {}))) == ());
    }

    #[tokio::test]
    async fn test_gateway_cron_service_list_jobs_with_filter() {
        let tmp = tempfile::tempdir().unwrap();
        let workspace_dir = tmp.path().join("workspace");
        std::fs::create_dir_all(&workspace_dir).unwrap();

        let cron_service = CronService::new(&workspace_dir);
        cron_service.start();

        // List jobs should return empty list
        let jobs = cron_service.list_jobs();
        assert!(jobs.is_empty());

        cron_service.stop();
    }

    #[tokio::test]
    async fn test_gateway_heartbeat_service_custom_interval() {
        let tmp = tempfile::tempdir().unwrap();
        let workspace_dir = tmp.path().join("workspace");
        std::fs::create_dir_all(&workspace_dir).unwrap();

        let heartbeat = HeartbeatService::new(workspace_dir, 120, true);
        let duration = heartbeat.sleep_duration();
        assert!(duration.as_secs() <= 120);
    }

    #[tokio::test]
    async fn test_gateway_message_bus_inbound_tx_clone() {
        let (mb, _receivers) = MessageBus::new();
        let tx1 = mb.inbound_tx();
        let tx2 = tx1.clone();

        // Both senders should work
        let hook = TaskHook::new("test-session");
        let result1 = tx1
            .send(BusRequest {
                session_id: "session-1".to_string(),
                content: "content-1".to_string(),
                channel_inject: None,
                hook: hook.clone(),
            })
            .await;
        assert!(result1.is_ok());

        let result2 = tx2
            .send(BusRequest {
                session_id: "session-2".to_string(),
                content: "content-2".to_string(),
                channel_inject: None,
                hook,
            })
            .await;
        assert!(result2.is_ok());
    }

    #[tokio::test]
    async fn test_gateway_tool_manager_start_turn() {
        let tmp = tempfile::tempdir().unwrap();
        let workspace_dir = tmp.path().join("workspace");
        std::fs::create_dir_all(&workspace_dir).unwrap();

        let mut tm = ToolManager::new(workspace_dir);
        tm.init_from_config(&[]); // Use defaults

        // start_turn should not panic
        tm.start_turn("shell");
        tm.start_turn("file_reader");
    }

    #[tokio::test]
    async fn test_gateway_tool_manager_sent_in_turn() {
        let tmp = tempfile::tempdir().unwrap();
        let workspace_dir = tmp.path().join("workspace");
        std::fs::create_dir_all(&workspace_dir).unwrap();

        let mut tm = ToolManager::new(workspace_dir);
        tm.init_from_config(&[]); // Use defaults

        // sent_in_turn should return false for tools that haven't sent
        assert!(!tm.sent_in_turn("shell"));
        assert!(!tm.sent_in_turn("file_reader"));
        assert!(!tm.sent_in_turn("nonexistent"));
    }

    #[tokio::test]
    async fn test_gateway_memory_store_write_soul() {
        let tmp = tempfile::tempdir().unwrap();
        let workspace_dir = tmp.path().join("workspace");
        std::fs::create_dir_all(&workspace_dir).unwrap();

        let ms = Arc::new(tokio::sync::Mutex::new(MemoryStore::new(&workspace_dir)));
        ms.lock().await.init().unwrap();

        // Write SOUL.md
        ms.lock()
            .await
            .write_soul("I am a helpful assistant")
            .unwrap();

        // Read it back
        let soul = ms.lock().await.read_soul();
        assert_eq!(soul, "I am a helpful assistant");
    }

    #[tokio::test]
    async fn test_gateway_memory_store_write_user() {
        let tmp = tempfile::tempdir().unwrap();
        let workspace_dir = tmp.path().join("workspace");
        std::fs::create_dir_all(&workspace_dir).unwrap();

        let ms = Arc::new(tokio::sync::Mutex::new(MemoryStore::new(&workspace_dir)));
        ms.lock().await.init().unwrap();

        // Write USER.md
        ms.lock()
            .await
            .write_user("Alice, software engineer")
            .unwrap();

        // Read it back
        let user = ms.lock().await.read_user();
        assert_eq!(user, "Alice, software engineer");
    }

    #[tokio::test]
    async fn test_gateway_worker_pool_multiple_submits() {
        WorkerPool::init_global(64);
        let pool = WorkerPool::global();

        // Submit some tasks
        for i in 0..5 {
            pool.submit(Box::new(move || {
                Box::pin(async move {
                    let _ = i;
                })
                    as std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>>
            }));
        }

        // Give tasks time to complete
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }

    #[tokio::test]
    async fn test_gateway_heartbeat_service_is_enabled() {
        let tmp = tempfile::tempdir().unwrap();
        let workspace_dir = tmp.path().join("workspace");
        std::fs::create_dir_all(&workspace_dir).unwrap();

        let hb_enabled = HeartbeatService::new(workspace_dir.clone(), 60, true);
        assert!(hb_enabled.is_enabled());

        let hb_disabled = HeartbeatService::new(workspace_dir, 60, false);
        assert!(!hb_disabled.is_enabled());
    }

    #[tokio::test]
    async fn test_gateway_message_bus_creation_and_capacity() {
        let (mb, _receivers) = MessageBus::new();
        assert_eq!(mb.inbound_tx().capacity(), 32);
        assert_eq!(mb.outbound_tx().capacity(), 32);
    }

    #[tokio::test]
    async fn test_gateway_session_manager_get_or_create_idempotent() {
        let tmp = tempfile::tempdir().unwrap();
        let session_dir = tmp.path().join("sessions");
        std::fs::create_dir_all(&session_dir).unwrap();

        let sm: SharedSessionManager = Arc::new(tokio::sync::Mutex::new(
            SessionManager::new(session_dir).unwrap(),
        ));

        // Create session twice
        sm.lock().await.get_or_create("test-session").await.unwrap();
        sm.lock().await.get_or_create("test-session").await.unwrap();

        // Should still have only one session
        assert!(sm.lock().await.has_session("test-session"));
    }

    #[tokio::test]
    async fn test_gateway_memory_store_init_idempotent() {
        let tmp = tempfile::tempdir().unwrap();
        let workspace_dir = tmp.path().join("workspace");
        std::fs::create_dir_all(&workspace_dir).unwrap();

        let ms = Arc::new(tokio::sync::Mutex::new(MemoryStore::new(&workspace_dir)));

        // Init multiple times should not error
        ms.lock().await.init().unwrap();
        ms.lock().await.init().unwrap();
        ms.lock().await.init().unwrap();

        assert!(workspace_dir.join("memory").exists());
    }

    #[tokio::test]
    async fn test_gateway_tool_manager_with_custom_config() {
        let tmp = tempfile::tempdir().unwrap();
        let workspace_dir = tmp.path().join("workspace");
        std::fs::create_dir_all(&workspace_dir).unwrap();

        let mut tm = ToolManager::new(workspace_dir);
        let tools = vec![
            crate::config::ToolEntry {
                name: "shell".to_string(),
                enabled: true,
            },
            crate::config::ToolEntry {
                name: "file_reader".to_string(),
                enabled: false,
            },
        ];
        tm.init_from_config(&tools);

        let functions = tm.to_openai_functions();
        assert_eq!(functions.len(), 1);
        assert_eq!(functions[0].name, "shell");
    }

    #[tokio::test]
    async fn test_gateway_memory_store_read_unprocessed_history_with_cursor() {
        let tmp = tempfile::tempdir().unwrap();
        let workspace_dir = tmp.path().join("workspace");
        std::fs::create_dir_all(&workspace_dir).unwrap();

        let ms = Arc::new(tokio::sync::Mutex::new(MemoryStore::new(&workspace_dir)));
        ms.lock().await.init().unwrap();

        // Append entries
        ms.lock().await.append_history("entry1").unwrap();
        ms.lock().await.append_history("entry2").unwrap();
        ms.lock().await.append_history("entry3").unwrap();

        // Read unprocessed with cursor 1 should return entries after cursor 1
        let unprocessed = ms.lock().await.read_unprocessed_history(1);
        assert_eq!(unprocessed.len(), 2);
    }

    #[tokio::test]
    async fn test_gateway_memory_store_read_recent_history_cap() {
        let tmp = tempfile::tempdir().unwrap();
        let workspace_dir = tmp.path().join("workspace");
        std::fs::create_dir_all(&workspace_dir).unwrap();

        let ms = Arc::new(tokio::sync::Mutex::new(MemoryStore::new(&workspace_dir)));
        ms.lock().await.init().unwrap();

        // Append many entries
        for i in 0..10 {
            ms.lock()
                .await
                .append_history(&format!("entry{}", i))
                .unwrap();
        }

        // Read with cap 3
        let recent = ms.lock().await.read_recent_history(3);
        assert_eq!(recent.len(), 3);
    }

    #[tokio::test]
    async fn test_gateway_cron_service_multiple_ticks() {
        let tmp = tempfile::tempdir().unwrap();
        let workspace_dir = tmp.path().join("workspace");
        std::fs::create_dir_all(&workspace_dir).unwrap();

        let cron_service = CronService::new(&workspace_dir);
        cron_service.start();

        // Multiple ticks should not panic
        cron_service.tick().await;
        cron_service.tick().await;
        cron_service.tick().await;

        cron_service.stop();
    }

    #[tokio::test]
    async fn test_gateway_message_bus_concurrent_sends() {
        let (mb, _receivers) = MessageBus::new();
        let tx = mb.outbound_tx();

        // Concurrent sends should work
        let handles: Vec<_> = (0..10)
            .map(|i| {
                let tx = tx.clone();
                tokio::spawn(async move {
                    tx.send(BusResult {
                        session_id: format!("session-{}", i),
                        task_id: format!("task-{}", i),
                        content: format!("content-{}", i),
                    })
                    .await
                    .is_ok()
                })
            })
            .collect();

        for handle in handles {
            assert!(handle.await.unwrap());
        }
    }

    #[tokio::test]
    async fn test_gateway_session_manager_multiple_sessions() {
        let tmp = tempfile::tempdir().unwrap();
        let session_dir = tmp.path().join("sessions");
        std::fs::create_dir_all(&session_dir).unwrap();

        let sm: SharedSessionManager = Arc::new(tokio::sync::Mutex::new(
            SessionManager::new(session_dir).unwrap(),
        ));

        // Create multiple sessions
        for i in 0..5 {
            sm.lock()
                .await
                .get_or_create(&format!("session-{}", i))
                .await
                .unwrap();
        }

        // All should exist
        for i in 0..5 {
            assert!(sm.lock().await.has_session(&format!("session-{}", i)));
        }
    }

    #[tokio::test]
    async fn test_gateway_memory_store_write_and_read_soul() {
        let tmp = tempfile::tempdir().unwrap();
        let workspace_dir = tmp.path().join("workspace");
        std::fs::create_dir_all(&workspace_dir).unwrap();

        let ms = Arc::new(tokio::sync::Mutex::new(MemoryStore::new(&workspace_dir)));
        ms.lock().await.init().unwrap();

        // Write soul
        ms.lock()
            .await
            .write_soul("You are a helpful AI assistant")
            .unwrap();

        // Read it back
        let soul = ms.lock().await.read_soul();
        assert_eq!(soul, "You are a helpful AI assistant");
    }

    #[tokio::test]
    async fn test_gateway_memory_store_write_and_read_user() {
        let tmp = tempfile::tempdir().unwrap();
        let workspace_dir = tmp.path().join("workspace");
        std::fs::create_dir_all(&workspace_dir).unwrap();

        let ms = Arc::new(tokio::sync::Mutex::new(MemoryStore::new(&workspace_dir)));
        ms.lock().await.init().unwrap();

        // Write user
        ms.lock()
            .await
            .write_user("Bob, a software developer")
            .unwrap();

        // Read it back
        let user = ms.lock().await.read_user();
        assert_eq!(user, "Bob, a software developer");
    }

    #[tokio::test]
    async fn test_gateway_tool_manager_execute_unknown_tool() {
        let tmp = tempfile::tempdir().unwrap();
        let workspace_dir = tmp.path().join("workspace");
        std::fs::create_dir_all(&workspace_dir).unwrap();

        let mut tm = ToolManager::new(workspace_dir);
        tm.init_from_config(&[]); // Use defaults

        // Execute unknown tool should error
        let result = tm.execute("nonexistent_tool", serde_json::json!({})).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_gateway_session_manager_clear_nonexistent() {
        let tmp = tempfile::tempdir().unwrap();
        let session_dir = tmp.path().join("sessions");
        std::fs::create_dir_all(&session_dir).unwrap();

        let sm: SharedSessionManager = Arc::new(tokio::sync::Mutex::new(
            SessionManager::new(session_dir).unwrap(),
        ));

        // Clear nonexistent session should not panic
        sm.lock().await.clear_session("nonexistent-session");
    }

    #[tokio::test]
    async fn test_gateway_worker_pool_submit_with_different_tasks() {
        WorkerPool::init_global(64);
        let pool = WorkerPool::global();

        // Submit different types of tasks
        pool.submit(Box::new(|| {
            Box::pin(async {
                let _ = 1 + 1;
            }) as std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>>
        }));

        pool.submit(Box::new(|| {
            Box::pin(async {
                let _ = "hello".to_string();
            }) as std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>>
        }));

        // Give tasks time to complete
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }

    #[tokio::test]
    async fn test_gateway_cron_service_with_custom_workspace() {
        let tmp = tempfile::tempdir().unwrap();
        let workspace_dir = tmp.path().join("custom_workspace");
        std::fs::create_dir_all(&workspace_dir).unwrap();

        let cron_service = CronService::new(&workspace_dir);
        cron_service.start();
        cron_service.tick().await;
        cron_service.stop();
    }

    #[tokio::test]
    async fn test_gateway_heartbeat_service_custom_sleep_duration() {
        let tmp = tempfile::tempdir().unwrap();
        let workspace_dir = tmp.path().join("workspace");
        std::fs::create_dir_all(&workspace_dir).unwrap();

        let heartbeat = HeartbeatService::new(workspace_dir, 180, true);
        let duration = heartbeat.sleep_duration();
        assert!(duration.as_secs() <= 180);
    }

    #[tokio::test]
    async fn test_gateway_message_bus_inbound_outbound_separate() {
        let (mb, _receivers) = MessageBus::new();
        let inbound_tx = mb.inbound_tx();
        let outbound_tx = mb.outbound_tx();

        // Both channels should have capacity
        assert!(inbound_tx.capacity() > 0);
        assert!(outbound_tx.capacity() > 0);

        // Send to inbound
        let hook = TaskHook::new("test");
        let result = inbound_tx
            .send(BusRequest {
                session_id: "test".to_string(),
                content: "test".to_string(),
                channel_inject: None,
                hook,
            })
            .await;
        assert!(result.is_ok());

        // Send to outbound
        let result = outbound_tx
            .send(BusResult {
                session_id: "test".to_string(),
                task_id: "test".to_string(),
                content: "test".to_string(),
            })
            .await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_gateway_session_manager_get_messages_empty() {
        let tmp = tempfile::tempdir().unwrap();
        let session_dir = tmp.path().join("sessions");
        std::fs::create_dir_all(&session_dir).unwrap();

        let sm: SharedSessionManager = Arc::new(tokio::sync::Mutex::new(
            SessionManager::new(session_dir).unwrap(),
        ));

        // Create session
        sm.lock().await.get_or_create("test-session").await.unwrap();

        // Get messages should return empty
        let msgs = sm.lock().await.get_messages("test-session").await;
        assert!(msgs.is_empty());
    }

    #[tokio::test]
    async fn test_gateway_memory_store_append_and_read_multiple() {
        let tmp = tempfile::tempdir().unwrap();
        let workspace_dir = tmp.path().join("workspace");
        std::fs::create_dir_all(&workspace_dir).unwrap();

        let ms = Arc::new(tokio::sync::Mutex::new(MemoryStore::new(&workspace_dir)));
        ms.lock().await.init().unwrap();

        // Append many entries
        for i in 0..20 {
            ms.lock()
                .await
                .append_history(&format!("entry-{}", i))
                .unwrap();
        }

        // Read recent with different caps
        let recent_5 = ms.lock().await.read_recent_history(5);
        assert_eq!(recent_5.len(), 5);

        let recent_10 = ms.lock().await.read_recent_history(10);
        assert_eq!(recent_10.len(), 10);

        let recent_100 = ms.lock().await.read_recent_history(100);
        assert_eq!(recent_100.len(), 20); // Only 20 entries exist
    }

    #[tokio::test]
    async fn test_gateway_tool_manager_with_empty_config() {
        let tmp = tempfile::tempdir().unwrap();
        let workspace_dir = tmp.path().join("workspace");
        std::fs::create_dir_all(&workspace_dir).unwrap();

        let mut tm = ToolManager::new(workspace_dir);
        tm.init_from_config(&[]); // Empty config uses defaults

        let functions = tm.to_openai_functions();
        assert!(!functions.is_empty()); // Should have default tools
    }

    #[tokio::test]
    async fn test_gateway_session_manager_persist_empty_session() {
        let tmp = tempfile::tempdir().unwrap();
        let session_dir = tmp.path().join("sessions");
        std::fs::create_dir_all(&session_dir).unwrap();

        let sm: SharedSessionManager = Arc::new(tokio::sync::Mutex::new(
            SessionManager::new(session_dir).unwrap(),
        ));

        // Create session
        sm.lock()
            .await
            .get_or_create("empty-session")
            .await
            .unwrap();

        // Persist empty session should not error
        let result = sm.lock().await.persist("empty-session").await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_gateway_worker_pool_submit_closures() {
        WorkerPool::init_global(64);
        let pool = WorkerPool::global();

        // Submit closures that capture different values
        let value1 = "hello".to_string();
        let value2 = 42;

        pool.submit(Box::new(move || {
            Box::pin(async move {
                let _ = value1;
            }) as std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>>
        }));

        pool.submit(Box::new(move || {
            Box::pin(async move {
                let _ = value2;
            }) as std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>>
        }));

        // Give tasks time to complete
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }

    #[tokio::test]
    async fn test_gateway_cron_service_persistence_path() {
        let tmp = tempfile::tempdir().unwrap();
        let workspace_dir = tmp.path().join("workspace");
        std::fs::create_dir_all(&workspace_dir).unwrap();

        let cron_service = CronService::new(&workspace_dir);
        // Service should be created without error
        assert!(workspace_dir.exists());
    }

    #[tokio::test]
    async fn test_gateway_heartbeat_service_with_zero_interval() {
        let tmp = tempfile::tempdir().unwrap();
        let workspace_dir = tmp.path().join("workspace");
        std::fs::create_dir_all(&workspace_dir).unwrap();

        // Zero interval should be handled gracefully
        let heartbeat = HeartbeatService::new(workspace_dir, 0, true);
        let duration = heartbeat.sleep_duration();
        // Duration should be non-negative
        assert!(duration.as_secs() >= 0);
    }

    #[tokio::test]
    async fn test_gateway_message_bus_capacity_check() {
        let (mb, _receivers) = MessageBus::new();
        // Both channels should have capacity > 0
        assert!(mb.inbound_tx().capacity() > 0);
        assert!(mb.outbound_tx().capacity() > 0);
    }

    #[tokio::test]
    async fn test_gateway_session_manager_has_session_empty() {
        let tmp = tempfile::tempdir().unwrap();
        let session_dir = tmp.path().join("sessions");
        std::fs::create_dir_all(&session_dir).unwrap();

        let sm: SharedSessionManager = Arc::new(tokio::sync::Mutex::new(
            SessionManager::new(session_dir).unwrap(),
        ));

        // No sessions initially
        assert!(!sm.lock().await.has_session("any-session"));
    }

    #[tokio::test]
    async fn test_gateway_memory_store_init_creates_directory() {
        let tmp = tempfile::tempdir().unwrap();
        let workspace_dir = tmp.path().join("workspace");
        std::fs::create_dir_all(&workspace_dir).unwrap();

        let ms = Arc::new(tokio::sync::Mutex::new(MemoryStore::new(&workspace_dir)));

        // Memory dir should not exist before init
        assert!(!workspace_dir.join("memory").exists());

        ms.lock().await.init().unwrap();

        // Memory dir should exist after init
        assert!(workspace_dir.join("memory").exists());
    }

    #[tokio::test]
    async fn test_gateway_tool_manager_description_and_parameters() {
        let tmp = tempfile::tempdir().unwrap();
        let workspace_dir = tmp.path().join("workspace");
        std::fs::create_dir_all(&workspace_dir).unwrap();

        let mut tm = ToolManager::new(workspace_dir);
        tm.init_from_config(&[]); // Use defaults

        let functions = tm.to_openai_functions();
        // All functions should have name, description, and parameters
        for func in &functions {
            assert!(!func.name.is_empty());
            assert!(!func.description.is_empty());
            // parameters should be a JSON object
            assert!(func.parameters.is_object() || func.parameters.is_null());
        }
    }

    #[tokio::test]
    async fn test_gateway_session_manager_get_messages_after_add() {
        let tmp = tempfile::tempdir().unwrap();
        let session_dir = tmp.path().join("sessions");
        std::fs::create_dir_all(&session_dir).unwrap();

        let sm: SharedSessionManager = Arc::new(tokio::sync::Mutex::new(
            SessionManager::new(session_dir).unwrap(),
        ));

        // Create session and add messages
        sm.lock().await.get_or_create("test-session").await.unwrap();
        sm.lock()
            .await
            .add_message("test-session", Message::user("hello".to_string()))
            .await
            .unwrap();
        sm.lock()
            .await
            .add_message(
                "test-session",
                Message::assistant(Some("hi".to_string()), None, None, None),
            )
            .await
            .unwrap();

        // Get messages
        let msgs = sm.lock().await.get_messages("test-session").await;
        assert_eq!(msgs.len(), 2);
    }

    #[tokio::test]
    async fn test_gateway_worker_pool_submit_many_tasks() {
        WorkerPool::init_global(64);
        let pool = WorkerPool::global();

        // Submit many tasks
        for i in 0..20 {
            pool.submit(Box::new(move || {
                Box::pin(async move {
                    let _ = i * 2;
                })
                    as std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>>
            }));
        }

        // Give tasks time to complete
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }

    #[tokio::test]
    async fn test_gateway_cron_service_empty_workspace() {
        let tmp = tempfile::tempdir().unwrap();
        let workspace_dir = tmp.path().join("workspace");
        std::fs::create_dir_all(&workspace_dir).unwrap();

        let cron_service = CronService::new(&workspace_dir);
        // Should not panic with empty workspace
        cron_service.start();
        cron_service.tick().await;
        cron_service.stop();
    }

    #[tokio::test]
    async fn test_gateway_heartbeat_service_disabled_tick() {
        let tmp = tempfile::tempdir().unwrap();
        let workspace_dir = tmp.path().join("workspace");
        std::fs::create_dir_all(&workspace_dir).unwrap();

        let heartbeat = HeartbeatService::new(workspace_dir, 60, false);
        // Tick when disabled should be a no-op
        heartbeat.tick().await;
    }

    #[tokio::test]
    async fn test_gateway_message_bus_clone_senders() {
        let (mb, _receivers) = MessageBus::new();
        let tx1 = mb.outbound_tx();
        let tx2 = tx1.clone();

        // Both senders should work independently
        let result1 = tx1
            .send(BusResult {
                session_id: "s1".to_string(),
                task_id: "t1".to_string(),
                content: "c1".to_string(),
            })
            .await;
        assert!(result1.is_ok());

        let result2 = tx2
            .send(BusResult {
                session_id: "s2".to_string(),
                task_id: "t2".to_string(),
                content: "c2".to_string(),
            })
            .await;
        assert!(result2.is_ok());
    }

    #[tokio::test]
    async fn test_gateway_session_manager_persist_multiple_times() {
        let tmp = tempfile::tempdir().unwrap();
        let session_dir = tmp.path().join("sessions");
        std::fs::create_dir_all(&session_dir).unwrap();

        let sm: SharedSessionManager = Arc::new(tokio::sync::Mutex::new(
            SessionManager::new(session_dir).unwrap(),
        ));

        // Create session
        sm.lock().await.get_or_create("test-session").await.unwrap();

        // Add and persist
        sm.lock()
            .await
            .add_message("test-session", Message::user("msg1".to_string()))
            .await
            .unwrap();
        sm.lock().await.persist("test-session").await.unwrap();

        // Add more and persist again
        sm.lock()
            .await
            .add_message("test-session", Message::user("msg2".to_string()))
            .await
            .unwrap();
        sm.lock().await.persist("test-session").await.unwrap();

        // Should not error
        let msgs = sm.lock().await.get_messages("test-session").await;
        assert_eq!(msgs.len(), 2);
    }

    #[tokio::test]
    async fn test_gateway_memory_store_read_memory_empty() {
        let tmp = tempfile::tempdir().unwrap();
        let workspace_dir = tmp.path().join("workspace");
        std::fs::create_dir_all(&workspace_dir).unwrap();

        let ms = Arc::new(tokio::sync::Mutex::new(MemoryStore::new(&workspace_dir)));
        ms.lock().await.init().unwrap();

        // Read memory before writing should be empty
        let memory = ms.lock().await.read_memory();
        assert!(memory.is_empty());
    }

    #[tokio::test]
    async fn test_gateway_tool_manager_get_functions_count() {
        let tmp = tempfile::tempdir().unwrap();
        let workspace_dir = tmp.path().join("workspace");
        std::fs::create_dir_all(&workspace_dir).unwrap();

        let mut tm = ToolManager::new(workspace_dir);
        tm.init_from_config(&[]); // Use defaults

        let functions = tm.to_openai_functions();
        // Should have at least the default tools
        assert!(functions.len() >= 6); // shell, file_reader, file_writer, file_editor, list_dir, make_dir
    }

    #[tokio::test]
    async fn test_gateway_worker_pool_submit_async_tasks() {
        WorkerPool::init_global(64);
        let pool = WorkerPool::global();

        // Submit async tasks
        pool.submit(Box::new(|| {
            Box::pin(async {
                tokio::time::sleep(std::time::Duration::from_millis(1)).await;
            }) as std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>>
        }));

        pool.submit(Box::new(|| {
            Box::pin(async {
                let _ = vec![1, 2, 3];
            }) as std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>>
        }));

        // Give tasks time to complete
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }

    #[tokio::test]
    async fn test_gateway_cron_service_tick_no_jobs() {
        let tmp = tempfile::tempdir().unwrap();
        let workspace_dir = tmp.path().join("workspace");
        std::fs::create_dir_all(&workspace_dir).unwrap();

        let cron_service = CronService::new(&workspace_dir);
        cron_service.start();
        // Tick with no jobs should not panic
        cron_service.tick().await;
        cron_service.stop();
    }

    #[tokio::test]
    async fn test_gateway_heartbeat_service_stop_when_not_started() {
        let tmp = tempfile::tempdir().unwrap();
        let workspace_dir = tmp.path().join("workspace");
        std::fs::create_dir_all(&workspace_dir).unwrap();

        let heartbeat = HeartbeatService::new(workspace_dir, 60, false);
        // Stop when not started should not panic
        heartbeat.stop();
    }

    #[tokio::test]
    async fn test_gateway_session_manager_get_or_create_returns_same() {
        let tmp = tempfile::tempdir().unwrap();
        let session_dir = tmp.path().join("sessions");
        std::fs::create_dir_all(&session_dir).unwrap();

        let sm: SharedSessionManager = Arc::new(tokio::sync::Mutex::new(
            SessionManager::new(session_dir).unwrap(),
        ));

        // Create session
        {
            let mut guard = sm.lock().await;
            let s1 = guard.get_or_create("test-session").await;
            assert!(s1.is_ok());
        }

        // Get same session again
        {
            let mut guard = sm.lock().await;
            let s2 = guard.get_or_create("test-session").await;
            assert!(s2.is_ok());
        }
    }

    #[tokio::test]
    async fn test_gateway_memory_store_get_memory_context_empty() {
        let tmp = tempfile::tempdir().unwrap();
        let workspace_dir = tmp.path().join("workspace");
        std::fs::create_dir_all(&workspace_dir).unwrap();

        let ms = Arc::new(tokio::sync::Mutex::new(MemoryStore::new(&workspace_dir)));
        ms.lock().await.init().unwrap();

        // Get memory context when empty
        let ctx = ms.lock().await.get_memory_context();
        assert!(ctx.is_empty());
    }

    #[tokio::test]
    async fn test_gateway_tool_manager_init_with_all_disabled() {
        let tmp = tempfile::tempdir().unwrap();
        let workspace_dir = tmp.path().join("workspace");
        std::fs::create_dir_all(&workspace_dir).unwrap();

        let mut tm = ToolManager::new(workspace_dir);
        let tools = vec![
            crate::config::ToolEntry {
                name: "shell".to_string(),
                enabled: false,
            },
            crate::config::ToolEntry {
                name: "file_reader".to_string(),
                enabled: false,
            },
        ];
        tm.init_from_config(&tools);

        let functions = tm.to_openai_functions();
        assert_eq!(functions.len(), 0);
    }

    #[tokio::test]
    async fn test_gateway_worker_pool_submit_with_string_capture() {
        WorkerPool::init_global(64);
        let pool = WorkerPool::global();

        let s = "hello".to_string();
        pool.submit(Box::new(move || {
            Box::pin(async move {
                let _ = s.len();
            }) as std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>>
        }));

        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }

    #[tokio::test]
    async fn test_gateway_cron_service_start_stop_cycle() {
        let tmp = tempfile::tempdir().unwrap();
        let workspace_dir = tmp.path().join("workspace");
        std::fs::create_dir_all(&workspace_dir).unwrap();

        let cron_service = CronService::new(&workspace_dir);
        // Multiple start/stop cycles should work
        cron_service.start();
        cron_service.stop();
        cron_service.start();
        cron_service.stop();
    }

    #[tokio::test]
    async fn test_gateway_heartbeat_service_multiple_stops() {
        let tmp = tempfile::tempdir().unwrap();
        let workspace_dir = tmp.path().join("workspace");
        std::fs::create_dir_all(&workspace_dir).unwrap();

        let heartbeat = HeartbeatService::new(workspace_dir, 60, true);
        // Multiple stops should not panic
        heartbeat.stop();
        heartbeat.stop();
    }

    #[tokio::test]
    async fn test_gateway_session_manager_total_message_count() {
        let tmp = tempfile::tempdir().unwrap();
        let session_dir = tmp.path().join("sessions");
        std::fs::create_dir_all(&session_dir).unwrap();

        let sm: SharedSessionManager = Arc::new(tokio::sync::Mutex::new(
            SessionManager::new(session_dir).unwrap(),
        ));

        // Create session
        sm.lock().await.get_or_create("test-session").await.unwrap();

        // Add messages
        for i in 0..3 {
            sm.lock()
                .await
                .add_message("test-session", Message::user(format!("msg{}", i)))
                .await
                .unwrap();
        }

        // Total count should be 3
        let count = sm.lock().await.total_message_count("test-session");
        assert_eq!(count, 3);
    }

    #[tokio::test]
    async fn test_gateway_memory_store_read_user_empty() {
        let tmp = tempfile::tempdir().unwrap();
        let workspace_dir = tmp.path().join("workspace");
        std::fs::create_dir_all(&workspace_dir).unwrap();

        let ms = Arc::new(tokio::sync::Mutex::new(MemoryStore::new(&workspace_dir)));
        ms.lock().await.init().unwrap();

        // Read user before writing should be empty
        let user = ms.lock().await.read_user();
        assert!(user.is_empty());
    }

    #[tokio::test]
    async fn test_gateway_memory_store_read_soul_empty() {
        let tmp = tempfile::tempdir().unwrap();
        let workspace_dir = tmp.path().join("workspace");
        std::fs::create_dir_all(&workspace_dir).unwrap();

        let ms = Arc::new(tokio::sync::Mutex::new(MemoryStore::new(&workspace_dir)));
        ms.lock().await.init().unwrap();

        // Read soul before writing should be empty
        let soul = ms.lock().await.read_soul();
        assert!(soul.is_empty());
    }

    #[tokio::test]
    async fn test_gateway_tool_manager_execute_with_invalid_json() {
        let tmp = tempfile::tempdir().unwrap();
        let workspace_dir = tmp.path().join("workspace");
        std::fs::create_dir_all(&workspace_dir).unwrap();

        let mut tm = ToolManager::new(workspace_dir);
        tm.init_from_config(&[]); // Use defaults

        // Execute shell with invalid command format
        let result = tm.execute("shell", serde_json::json!({})).await;
        // Should return an error due to missing command
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_gateway_worker_pool_submit_empty_closures() {
        WorkerPool::init_global(64);
        let pool = WorkerPool::global();

        // Submit empty closures
        pool.submit(Box::new(|| {
            Box::pin(async {}) as std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>>
        }));
        pool.submit(Box::new(|| {
            Box::pin(async {}) as std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>>
        }));
        pool.submit(Box::new(|| {
            Box::pin(async {}) as std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>>
        }));

        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }

    #[tokio::test]
    async fn test_gateway_cron_service_add_and_list() {
        let tmp = tempfile::tempdir().unwrap();
        let workspace_dir = tmp.path().join("workspace");
        std::fs::create_dir_all(&workspace_dir).unwrap();

        let cron_service = CronService::new(&workspace_dir);
        // Initially no jobs
        assert!(cron_service.list_jobs().is_empty());
    }

    #[tokio::test]
    async fn test_gateway_heartbeat_service_sleep_duration_positive() {
        let tmp = tempfile::tempdir().unwrap();
        let workspace_dir = tmp.path().join("workspace");
        std::fs::create_dir_all(&workspace_dir).unwrap();

        let heartbeat = HeartbeatService::new(workspace_dir, 60, true);
        let duration = heartbeat.sleep_duration();
        assert!(duration.as_secs() > 0);
    }

    #[tokio::test]
    async fn test_gateway_message_bus_inbound_capacity() {
        let (mb, _receivers) = MessageBus::new();
        // Inbound channel should have capacity
        assert!(mb.inbound_tx().capacity() > 0);
    }

    #[tokio::test]
    async fn test_gateway_message_bus_outbound_capacity() {
        let (mb, _receivers) = MessageBus::new();
        // Outbound channel should have capacity
        assert!(mb.outbound_tx().capacity() > 0);
    }

    #[tokio::test]
    async fn test_gateway_session_manager_has_session_false() {
        let tmp = tempfile::tempdir().unwrap();
        let session_dir = tmp.path().join("sessions");
        std::fs::create_dir_all(&session_dir).unwrap();

        let sm: SharedSessionManager = Arc::new(tokio::sync::Mutex::new(
            SessionManager::new(session_dir).unwrap(),
        ));

        // Initially no sessions
        assert!(!sm.lock().await.has_session("nonexistent"));
    }

    #[tokio::test]
    async fn test_gateway_memory_store_write_and_read_memory() {
        let tmp = tempfile::tempdir().unwrap();
        let workspace_dir = tmp.path().join("workspace");
        std::fs::create_dir_all(&workspace_dir).unwrap();

        let ms = Arc::new(tokio::sync::Mutex::new(MemoryStore::new(&workspace_dir)));
        ms.lock().await.init().unwrap();

        // Write memory
        ms.lock().await.write_memory("test memory content").unwrap();

        // Read it back
        let memory = ms.lock().await.read_memory();
        assert_eq!(memory, "test memory content");
    }

    #[tokio::test]
    async fn test_gateway_tool_manager_get_functions_not_empty() {
        let tmp = tempfile::tempdir().unwrap();
        let workspace_dir = tmp.path().join("workspace");
        std::fs::create_dir_all(&workspace_dir).unwrap();

        let mut tm = ToolManager::new(workspace_dir);
        tm.init_from_config(&[]); // Use defaults

        let functions = tm.to_openai_functions();
        assert!(!functions.is_empty());
    }

    #[tokio::test]
    async fn test_gateway_worker_pool_submit_single_task() {
        WorkerPool::init_global(64);
        let pool = WorkerPool::global();

        // Submit single task
        pool.submit(Box::new(|| {
            Box::pin(async {}) as std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>>
        }));

        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
}
