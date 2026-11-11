use std::collections::HashMap;
use std::fs;

use tempfile::NamedTempFile;

use slimbot::secrets::{MapSource, SecretStore, SourceKind, provider_key};
use slimbot::{AgentConfig, ChannelConfig, Config, ProviderConfig, ToolEntry};

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

    let config = Config::load_resolved_with(path, &SecretStore::empty()).unwrap();
    assert_eq!(config.agent.provider, "default");
    assert_eq!(config.agent.max_iterations, 20);
    assert!(config.providers.contains_key("default"));
    assert_eq!(config.providers["default"].api_key, "sk-test-key");
}

#[test]
fn test_load_missing_file() {
    let result = Config::load_resolved_with("/nonexistent/path/config.json", &SecretStore::empty());
    assert!(result.is_err());
    let err = result.unwrap_err().to_string();
    assert!(err.contains("Failed to read config file"));
}

#[test]
fn test_load_invalid_json() {
    let tmp = NamedTempFile::new().unwrap();
    let path = tmp.path().to_str().unwrap();
    fs::write(path, "{ invalid json }").unwrap();

    let result = Config::load_resolved_with(path, &SecretStore::empty());
    assert!(result.is_err());
    let err = result.unwrap_err().to_string();
    assert!(err.contains("Invalid config JSON"));
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

    let result = Config::load_resolved_with(path, &SecretStore::empty());
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

    let result = Config::load_resolved_with(path, &SecretStore::empty());
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

    let result = Config::load_resolved_with(path, &SecretStore::empty());
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

    let config = Config::load_resolved_with(path, &SecretStore::empty()).unwrap();
    let provider = &config.providers["default"];
    assert_eq!(provider.r#type, "openai");
    assert!((provider.temperature - 0.7).abs() < f32::EPSILON);
    assert_eq!(provider.max_tokens, 4096);
    // api_url is derived from base_url during normalize
    assert_eq!(
        provider.api_url,
        "https://api.openai.com/v1/chat/completions"
    );
    assert!(!provider.base_url.is_empty());
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

    let config = Config::load_resolved_with(path, &SecretStore::empty()).unwrap();
    assert_eq!(config.agent.max_iterations, 40);
    assert_eq!(config.agent.timeout_seconds, 120);
    // `agent.max_tool_result_chars` is deprecated: limits now derive from the
    // provider window plus the `pressure` ratio table.
    assert_eq!(config.pressure.single_tool_result_ratio, 0.25);
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
            context_window_tokens: 32768,
            prompt_cache_enabled: true,
            disable_thinking: false,
            timeout_seconds: 180,
            unknown: Default::default(),
        },
    );

    let mut channels = HashMap::new();
    channels.insert(
        "cli".to_string(),
        ChannelConfig {
            enabled: true,
            extra: {
                let mut m = HashMap::new();
                m.insert("chat_id".to_string(), serde_json::json!("default"));
                m
            },
        },
    );

    // Name only the fields this scenario varies. A new Config field must not
    // require an edit here (ticket 02).
    let config = Config {
        agent: AgentConfig {
            provider: "my-provider".to_string(),
            max_iterations: 10,
            timeout_seconds: 60,
            persist_tool_results: false,
            ..Default::default()
        },
        providers,
        tools: slimbot::ToolsConfig {
            entries: vec![ToolEntry {
                name: "shell".to_string(),
                enabled: true,
            }],
            ..Default::default()
        },
        channels,
        ..Default::default()
    };

    config.save(path).unwrap();
    let reloaded = Config::load_resolved_with(path, &SecretStore::empty()).unwrap();

    assert_eq!(reloaded.agent.provider, "my-provider");
    assert_eq!(reloaded.agent.max_iterations, 10);
    assert_eq!(reloaded.providers["my-provider"].model, "gpt-4");
    assert_eq!(reloaded.tools.entries.len(), 1);
    assert_eq!(reloaded.channels.len(), 1);
}

// ── Clamp coverage for sections missing from Config::clamp ──
// These three sections declare real constraints in define_config!, but the
// hand-written clamp list never reached them. Assert observable behaviour via
// the public load entry rather than metadata consistency (ticket 01).

fn load_config(value: serde_json::Value) -> Config {
    let tmp = NamedTempFile::new().unwrap();
    let path = tmp.path().to_str().unwrap();
    fs::write(path, serde_json::to_string_pretty(&value).unwrap()).unwrap();
    Config::load_resolved_with(path, &SecretStore::empty()).unwrap()
}

#[test]
fn test_config_load_clamps_gateway_owner_id() {
    let config = load_config(serde_json::json!({
        "agent": { "provider": "default" },
        "providers": { "default": { "api_key": "sk-key", "model": "gpt-4o" } },
        "gateway": { "owner": { "id": "x".repeat(128) } }
    }));
    assert_eq!(
        config.gateway.owner.id.len(),
        64,
        "gateway.owner.id must honour str_max(64)"
    );
}

#[test]
fn test_config_load_clamps_trace_backend_type() {
    let config = load_config(serde_json::json!({
        "agent": { "provider": "default" },
        "providers": { "default": { "api_key": "sk-key", "model": "gpt-4o" } },
        "tracing": { "backends": [{ "type": "bogus", "enabled": true }] }
    }));
    assert_eq!(config.tracing.backends.len(), 1);
    assert_eq!(
        config.tracing.backends[0].r#type, "file",
        "an unknown backend type must fall back to the declared default"
    );
}

#[test]
fn test_config_load_clamps_trigger_secret_name() {
    let config = load_config(serde_json::json!({
        "agent": { "provider": "default" },
        "providers": { "default": { "api_key": "sk-key", "model": "gpt-4o" } },
        "triggers": { "secrets": [{ "name": "s".repeat(200), "value": "v" }] }
    }));
    assert_eq!(
        config.triggers.secrets[0].name.len(),
        128,
        "triggers.secrets[].name must honour str_max(128)"
    );
}

// ── The single effective load path ──

fn config_file(value: serde_json::Value) -> (NamedTempFile, String) {
    let tmp = NamedTempFile::new().unwrap();
    let path = tmp.path().to_str().unwrap().to_string();
    fs::write(&path, serde_json::to_string_pretty(&value).unwrap()).unwrap();
    (tmp, path)
}

#[test]
fn test_load_resolved_clamps_and_validates_in_order() {
    let (_tmp, path) = config_file(serde_json::json!({
        "agent": { "provider": "default" },
        "providers": { "default": { "api_key": "", "model": "gpt-4o" } },
        "gateway": { "owner": { "id": "x".repeat(128) } },
        "tracing": { "backends": [{ "type": "bogus", "enabled": true }] },
        "triggers": { "secrets": [{ "name": "s".repeat(200), "value": "" }] }
    }));
    let store = SecretStore::new(vec![(
        SourceKind::Keychain,
        Box::new(MapSource::new([(
            provider_key("default"),
            "sk-from-keychain",
        )])),
    )]);

    let config = Config::load_resolved_with(&path, &store).unwrap();

    assert_eq!(config.gateway.owner.id.len(), 64, "clamp runs on read");
    assert_eq!(
        config.tracing.backends[0].r#type, "file",
        "clamp runs on read"
    );
    assert_eq!(
        config.triggers.secrets[0].name.len(),
        128,
        "clamp runs on read"
    );
    assert_eq!(
        config.providers["default"].api_key, "sk-from-keychain",
        "secrets resolve after the file has been read and clamped"
    );
    assert!(
        config.validate().is_ok(),
        "validate must see the resolved key, not the empty value on disk"
    );
}

#[cfg(unix)]
#[test]
fn test_load_resolved_enforces_secret_file_mode() {
    use std::os::unix::fs::PermissionsExt;

    let (_tmp, path) = config_file(serde_json::json!({
        "agent": { "provider": "default" },
        "providers": { "default": { "api_key": "sk-inline", "model": "gpt-4o" } }
    }));
    fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();

    Config::load_resolved_with(&path, &SecretStore::empty()).unwrap();

    assert_eq!(
        fs::metadata(&path).unwrap().permissions().mode() & 0o777,
        0o600,
        "a config holding secrets must be tightened on every effective load"
    );
}

#[test]
fn test_preview_entry_accepts_unvalidated_disk_state() {
    let (_tmp, path) = config_file(serde_json::json!({
        "agent": { "provider": "default" },
        "providers": { "default": { "api_key": "", "model": "" } }
    }));

    let preview = Config::load_for_preview(&path).unwrap();
    assert!(
        preview.validate().is_err(),
        "the preview reads disk state the effective entry would refuse"
    );
    assert_eq!(preview.providers["default"].api_key, "");
    assert!(
        Config::load_resolved_with(&path, &SecretStore::empty()).is_err(),
        "the effective entry validates the same file"
    );
}

#[test]
fn test_config_fixture_needs_no_full_struct_literal() {
    // Only the fields this scenario varies are named; a new Config field is
    // absorbed by the struct update syntax instead of breaking every fixture.
    let config = Config {
        agent: AgentConfig {
            provider: "default".to_string(),
            ..Default::default()
        },
        ..Default::default()
    };

    assert_eq!(config.agent.provider, "default");
    assert!(config.providers.is_empty());
    assert_eq!(config.pressure.single_tool_result_ratio, 0.25);
}
