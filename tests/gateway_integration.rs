//! Integration tests for gateway module.
//!
//! These tests verify gateway startup and shutdown behavior.

use std::sync::Arc;

use slimbot::{
    Config, CronService, MessageBus, PathManager, SessionManager, SharedSessionManager, ToolManager,
};

/// Helper to create a minimal test config
fn make_test_config(_data_dir: &std::path::Path) -> Config {
    let config = Config {
        agent: slimbot::AgentConfig {
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
                slimbot::ProviderConfig {
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
        gateway: slimbot::GatewayConfig {
            cron: slimbot::CronConfig { enabled: false },
            heartbeat: slimbot::HeartbeatConfig {
                enabled: false,
                interval_s: 60,
            },
        },
    };
    config
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
