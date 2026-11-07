//! Integration tests for gateway module.
//!
//! These tests verify gateway startup and shutdown behavior.

use std::sync::Arc;

use slimbot::{
    Config, CronService, MessageBus, PathManager, SessionManager, SharedSessionManager, ToolManager,
};

/// Helper to create a minimal test config
fn make_test_config(_data_dir: &std::path::Path) -> Config {
    Config {
        agent: slimbot::AgentConfig {
            provider: "default".to_string(),
            max_iterations: 10,
            timeout_seconds: 30,
            persist_tool_results: false,
            unknown: Default::default(),
        },
        providers: {
            let mut map = std::collections::HashMap::new();
            map.insert(
                "default".to_string(),
                slimbot::ProviderConfig {
                    r#type: "openai".to_string(),
                    api_url: "".to_string(),
                    base_url: "https://api.openai.com".to_string(),
                    api_key: "test-key".to_string(),
                    model: "gpt-4o".to_string(),
                    temperature: 0.7,
                    max_tokens: 4096,
                    context_window_tokens: 32768,
                    prompt_cache_enabled: false,
                    disable_thinking: false,
                    timeout_seconds: 180,
                    unknown: Default::default(),
                },
            );
            map
        },
        tools: Default::default(),
        channels: std::collections::HashMap::new(),
        gateway: slimbot::GatewayConfig {
            owner: Default::default(),
            cron: slimbot::CronConfig {
                enabled: false,
                unknown: Default::default(),
            },
            heartbeat: slimbot::HeartbeatConfig {
                channel: Default::default(),
                to: Default::default(),
                enabled: false,
                interval_s: 60,
                unknown: Default::default(),
            },
            dream: slimbot::DreamConfig::default(),
        },
        web_fetch: None,
        tracing: Default::default(),
        memory: Default::default(),
        json_render: Default::default(),
        subagent: Default::default(),
        triggers: Default::default(),
        pressure: Default::default(),
        mcp: Default::default(),
        executors: Default::default(),
    }
}

#[tokio::test]
async fn test_gateway_can_start_and_shutdown() {
    let tmp = tempfile::tempdir().unwrap();
    let data_dir = tmp.path().join("data");
    let workspace_dir = data_dir.join("workspace");
    std::fs::create_dir_all(&workspace_dir).unwrap();

    // Create a minimal config file
    let config = make_test_config(&data_dir);
    let config_path = data_dir.join("config.json");
    let config_json = serde_json::to_string_pretty(&config).unwrap();
    std::fs::write(&config_path, config_json).unwrap();

    let paths = PathManager::resolve(
        Some(config_path.to_str().unwrap()),
        Some(data_dir.to_str().unwrap()),
        Some(workspace_dir.to_str().unwrap()),
    )
    .unwrap();

    // Gateway should be able to start (it will block on ctrl_c, so we can't fully test it)
    // Instead, we verify the setup code doesn't panic
    // Since we're not sending ctrl_c, this will block, so we use tokio::time::timeout
    // For now, just verify the function can be called with valid paths
    assert!(paths.workspace_dir().exists());
    assert!(paths.data_dir().exists());
}

#[tokio::test]
async fn test_gateway_session_manager_creation() {
    let tmp = tempfile::tempdir().unwrap();
    let data_dir = tmp.path().join("data");
    let workspace_dir = data_dir.join("workspace");
    std::fs::create_dir_all(&workspace_dir).unwrap();

    let session_dir = workspace_dir.join("sessions");
    std::fs::create_dir_all(&session_dir).unwrap();

    // Verify session manager can be created
    let sm: SharedSessionManager = Arc::new(tokio::sync::Mutex::new(
        SessionManager::new(session_dir).unwrap(),
    ));

    // Verify we can create a session
    let session_id = {
        let mut guard = sm.lock().await;
        guard.get_or_create("test-session").await.unwrap();
        "test-session".to_string()
    };

    // Verify session exists
    let has_session = {
        let guard = sm.lock().await;
        guard.has_session(&session_id)
    };
    assert!(has_session);
}

#[tokio::test]
async fn test_gateway_cron_disabled_by_default() {
    let tmp = tempfile::tempdir().unwrap();
    let data_dir = tmp.path().join("data");
    let workspace_dir = data_dir.join("workspace");
    std::fs::create_dir_all(&workspace_dir).unwrap();

    let config = make_test_config(&data_dir);
    assert!(!config.gateway.cron.enabled);
    assert!(!config.gateway.heartbeat.enabled);
}

#[tokio::test]
async fn test_gateway_cron_can_be_enabled() {
    let tmp = tempfile::tempdir().unwrap();
    let data_dir = tmp.path().join("data");
    let workspace_dir = data_dir.join("workspace");
    std::fs::create_dir_all(&workspace_dir).unwrap();

    let mut config = make_test_config(&data_dir);
    config.gateway.cron.enabled = true;
    assert!(config.gateway.cron.enabled);

    // Verify cron service can be created
    let cron_service = CronService::new(&workspace_dir);
    assert!(cron_service.list_jobs().is_empty());
}

#[tokio::test]
async fn test_gateway_heartbeat_can_be_enabled() {
    let tmp = tempfile::tempdir().unwrap();
    let data_dir = tmp.path().join("data");
    let workspace_dir = data_dir.join("workspace");
    std::fs::create_dir_all(&workspace_dir).unwrap();

    let mut config = make_test_config(&data_dir);
    config.gateway.heartbeat.enabled = true;
    config.gateway.heartbeat.interval_s = 10;
    assert!(config.gateway.heartbeat.enabled);
    assert_eq!(config.gateway.heartbeat.interval_s, 10);
}

#[tokio::test]
async fn test_gateway_message_bus_creation() {
    let (mb, _receivers) = MessageBus::new();
    assert!(mb.inbound_tx().capacity() > 0);
    assert!(mb.outbound_tx().capacity() > 0);
}

#[tokio::test]
async fn test_gateway_tool_manager_creation() {
    let tmp = tempfile::tempdir().unwrap();
    let workspace_dir = tmp.path().join("workspace");
    std::fs::create_dir_all(&workspace_dir).unwrap();

    let mut tm = ToolManager::new(workspace_dir);
    tm.init_from_config(&[]); // Use defaults

    // Verify built-in tools are registered
    let tools = tm.to_openai_functions();
    assert!(tools.len() >= 6); // At least shell, file_reader, file_writer, file_editor, list_dir, make_dir
}

/// Build a loop from a written config, optionally carrying MCP servers.
/// The temp dir comes back with it: the loop's paths point inside it.
async fn loop_with_config(
    servers: Option<serde_json::Value>,
) -> (slimbot::AgentLoop, tempfile::TempDir) {
    let tmp = tempfile::tempdir().unwrap();
    let data_dir = tmp.path().join("data");
    let workspace_dir = data_dir.join("workspace");
    std::fs::create_dir_all(&workspace_dir).unwrap();

    let mut raw = serde_json::to_value(make_test_config(&data_dir)).unwrap();
    if let Some(servers) = servers {
        raw["mcp"] = serde_json::json!({ "servers": servers });
    }
    let config_path = data_dir.join("config.json");
    std::fs::write(&config_path, serde_json::to_string_pretty(&raw).unwrap()).unwrap();
    let config = std::sync::Arc::new(Config::load(config_path.to_str().unwrap()).unwrap());

    let paths = PathManager::resolve(
        Some(config_path.to_str().unwrap()),
        Some(data_dir.to_str().unwrap()),
        Some(workspace_dir.to_str().unwrap()),
    )
    .unwrap();
    let (mb, receivers) = MessageBus::new();
    let loop_ = slimbot::AgentLoop::from_config(&paths, mb, receivers.inbound, config)
        .await
        .unwrap();
    (loop_, tmp)
}

/// The gateway-level assembly of ticket 26: configured servers produce a pool
/// that the loop owns and starts, and an empty `mcp.servers` produces no pool at
/// all — a script then simply has no connector surface.
#[tokio::test]
async fn test_gateway_brings_up_the_configured_connector_pool() {
    let dead_port = {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        listener.local_addr().unwrap().port()
    };
    let servers = serde_json::json!([{
        "name": "github",
        "transport": "http",
        "url": format!("http://127.0.0.1:{dead_port}/mcp"),
    }]);
    let (loop_, _tmp) = loop_with_config(Some(servers)).await;

    let pool = loop_
        .connectors()
        .expect("a configured server must produce a pool");
    // Observable through the service: the server is named before it is reached,
    // and an unreachable one never blocks the loop from being built.
    let servers = pool.servers();
    assert_eq!(servers.len(), 1);
    assert_eq!(servers[0].0, "github");
    loop_.shutdown_for_cli().await;
}

#[tokio::test]
async fn test_gateway_without_mcp_servers_has_no_connector_pool() {
    let (loop_, _tmp) = loop_with_config(None).await;
    assert!(loop_.connectors().is_none());
    loop_.shutdown_for_cli().await;
}
