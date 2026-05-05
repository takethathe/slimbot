use std::collections::HashMap;
use std::fs;

use tempfile::NamedTempFile;

use slimbot::{Config, AgentConfig, ProviderConfig, ToolEntry, ChannelConfig, GatewayConfig};

// ── Config loading and validation ──

#[test]
fn test_load_valid_config() {
    let tmp = NamedTempFile::new().unwrap();
    let path = tmp.path().to_str().unwrap();

    let config_json = serde_json::json!({
        "agent": {
            "provider": "default",
            "max_iterations": 20,
        },
        "providers": {
            "default": {
                "api_key": "sk-test-key",
                "model": "gpt-4o",
            }
        },
        "tools": [],
        "channels": []
    });
    fs::write(path, serde_json::to_string_pretty(&config_json).unwrap()).unwrap();

    let config = Config::load(path).unwrap();
    assert_eq!(config.agent.provider, "default");
    assert_eq!(config.agent.max_iterations, 20);
    assert!(config.providers.contains_key("default"));
    assert_eq!(config.providers["default"].api_key, "sk-test-key");
}

#[test]
fn test_load_missing_file() {
    let result = Config::load("/nonexistent/path/config.json");
    assert!(result.is_err());
    let err = result.unwrap_err().to_string();
    assert!(err.contains("Failed to read config.json"));
}

#[test]
fn test_load_invalid_json() {
    let tmp = NamedTempFile::new().unwrap();
    let path = tmp.path().to_str().unwrap();
    fs::write(path, "{ invalid json }").unwrap();

    let result = Config::load(path);
    assert!(result.is_err());
    let err = result.unwrap_err().to_string();
    assert!(err.contains("Invalid config.json"));
}

#[test]
fn test_validate_missing_provider_ref() {
    let tmp = NamedTempFile::new().unwrap();
    let path = tmp.path().to_str().unwrap();

    let config_json = serde_json::json!({
        "agent": { "provider": "nonexistent" },
        "providers": {
            "default": { "api_key": "sk-key", "model": "gpt-4o" }
        }
    });
    fs::write(path, serde_json::to_string_pretty(&config_json).unwrap()).unwrap();

    let result = Config::load(path);
    assert!(result.is_err());
    let err = result.unwrap_err().to_string();
    assert!(err.contains("referenced by agent not found"));
}

#[test]
fn test_validate_empty_api_key() {
    let tmp = NamedTempFile::new().unwrap();
    let path = tmp.path().to_str().unwrap();

    let config_json = serde_json::json!({
        "agent": { "provider": "default" },
        "providers": {
            "default": { "api_key": "", "model": "gpt-4o" }
        }
    });
    fs::write(path, serde_json::to_string_pretty(&config_json).unwrap()).unwrap();

    let result = Config::load(path);
    assert!(result.is_err());
    let err = result.unwrap_err().to_string();
    assert!(err.contains("api_key must not be empty"));
}

#[test]
fn test_validate_empty_model() {
    let tmp = NamedTempFile::new().unwrap();
    let path = tmp.path().to_str().unwrap();

    let config_json = serde_json::json!({
        "agent": { "provider": "default" },
        "providers": {
            "default": { "api_key": "sk-key", "model": "" }
        }
    });
    fs::write(path, serde_json::to_string_pretty(&config_json).unwrap()).unwrap();

    let result = Config::load(path);
    assert!(result.is_err());
    let err = result.unwrap_err().to_string();
    assert!(err.contains("model must not be empty"));
}

// ── Config defaults via serde ──

#[test]
fn test_provider_defaults() {
    let tmp = NamedTempFile::new().unwrap();
    let path = tmp.path().to_str().unwrap();

    let config_json = serde_json::json!({
        "agent": { "provider": "default" },
        "providers": {
            "default": { "api_key": "sk-key", "model": "gpt-4o" }
        }
    });
    fs::write(path, serde_json::to_string_pretty(&config_json).unwrap()).unwrap();

    let config = Config::load(path).unwrap();
    let provider = &config.providers["default"];
    assert_eq!(provider.r#type, "openai");
    assert!((provider.temperature - 0.7).abs() < f32::EPSILON);
    assert_eq!(provider.max_tokens, 4096);
    assert!(provider.api_url.is_empty());
    assert!(provider.base_url.is_empty());
}

#[test]
fn test_agent_defaults() {
    let tmp = NamedTempFile::new().unwrap();
    let path = tmp.path().to_str().unwrap();

    let config_json = serde_json::json!({
        "agent": { "provider": "default" },
        "providers": {
            "default": { "api_key": "sk-key", "model": "gpt-4o" }
        }
    });
    fs::write(path, serde_json::to_string_pretty(&config_json).unwrap()).unwrap();

    let config = Config::load(path).unwrap();
    assert_eq!(config.agent.max_iterations, 40);
    assert_eq!(config.agent.timeout_seconds, 120);
    assert_eq!(config.agent.max_tool_result_chars, 8000);
    assert!(config.agent.persist_tool_results);
}

// ── Config save and round-trip ──

#[test]
fn test_save_and_reload_round_trip() {
    let tmp = NamedTempFile::new().unwrap();
    let path = tmp.path().to_str().unwrap();

    let mut providers = HashMap::new();
    providers.insert(
        "my-provider".to_string(),
        ProviderConfig {
            r#type: "openai".to_string(),
            api_url: String::new(),
            base_url: "https://api.openai.com".to_string(),
            api_key: "sk-test".to_string(),
            model: "gpt-4".to_string(),
            temperature: 0.5,
            max_tokens: 2048,
            prompt_cache_enabled: true,
        },
    );

    let mut channels = HashMap::new();
    channels.insert("cli".to_string(), ChannelConfig {
        enabled: true,
        extra: {
            let mut m = HashMap::new();
            m.insert("chat_id".to_string(), serde_json::json!("default"));
            m
        },
    });

    let config = Config {
        agent: AgentConfig {
            provider: "my-provider".to_string(),
            max_iterations: 10,
            timeout_seconds: 60,
            max_tool_result_chars: 4000,
            persist_tool_results: false,
            context_window_tokens: 8192,
        },
        providers,
        tools: vec![ToolEntry {
            name: "shell".to_string(),
            enabled: true,
        }],
        channels,
        gateway: GatewayConfig::default(),
    };

    config.save(path).unwrap();
    let reloaded = Config::load(path).unwrap();

    assert_eq!(reloaded.agent.provider, "my-provider");
    assert_eq!(reloaded.agent.max_iterations, 10);
    assert_eq!(reloaded.providers["my-provider"].model, "gpt-4");
    assert_eq!(reloaded.tools.len(), 1);
    assert_eq!(reloaded.channels.len(), 1);
}
