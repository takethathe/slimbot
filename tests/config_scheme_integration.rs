use std::collections::HashMap;
use std::fs;

use tempfile::NamedTempFile;

use slimbot::{AgentConfig, ChannelConfig, Config, PressureConfig, ProviderConfig, ToolEntry};

// ── Default config generation ──

#[test]
fn test_default_config_structure() {
    let mut providers = HashMap::new();
    providers.insert("default".to_string(), ProviderConfig::default());
    let mut config = Config {
        pressure: Default::default(),
        agent: AgentConfig::default(),
        providers,
        tools: Default::default(),
        channels: HashMap::new(),
        gateway: Default::default(),
        web_fetch: None,
        tracing: Default::default(),
        memory: Default::default(),
        json_render: Default::default(),
        subagent: Default::default(),
        triggers: Default::default(),
    };

    // Normalize fills in default provider name
    config.normalize();

    assert_eq!(config.agent.provider, "default");
    assert!(config.providers.contains_key("default"));
    assert!(config.tools.entries.is_empty());
    assert!(config.channels.is_empty());

    let provider = &config.providers["default"];
    assert_eq!(provider.r#type, "openai");
    assert_eq!(provider.base_url, "https://api.openai.com");
    assert!(provider.api_key.is_empty()); // intentional, user must set
    assert_eq!(provider.model, "gpt-4o");
}

// ── Normalization ──

#[test]
fn test_normalize_fills_empty_agent_provider() {
    let mut providers = HashMap::new();
    providers.insert("default".to_string(), ProviderConfig::default());
    let mut config = Config {
        pressure: Default::default(),
        agent: AgentConfig::default(),
        providers,
        tools: Default::default(),
        channels: HashMap::new(),
        gateway: Default::default(),
        web_fetch: None,
        tracing: Default::default(),
        memory: Default::default(),
        json_render: Default::default(),
        subagent: Default::default(),
        triggers: Default::default(),
    };
    config.agent.provider.clear();

    config.normalize();
    assert_eq!(config.agent.provider, "default");
}

#[test]
fn test_normalize_derives_url_from_base_url() {
    let mut providers = HashMap::new();
    providers.insert("default".to_string(), ProviderConfig::default());
    let mut config = Config {
        pressure: Default::default(),
        agent: AgentConfig::default(),
        providers,
        tools: Default::default(),
        channels: HashMap::new(),
        gateway: Default::default(),
        web_fetch: None,
        tracing: Default::default(),
        memory: Default::default(),
        json_render: Default::default(),
        subagent: Default::default(),
        triggers: Default::default(),
    };

    let provider = config.providers.get_mut("default").unwrap();
    provider.api_url.clear();
    provider.base_url = "https://api.example.com".to_string();

    config.normalize();

    let provider = config.providers.get("default").unwrap();
    assert_eq!(
        provider.api_url,
        "https://api.example.com/v1/chat/completions"
    );
}

#[test]
fn test_normalize_preserves_existing_api_url() {
    let mut providers = HashMap::new();
    providers.insert("default".to_string(), ProviderConfig::default());
    let mut config = Config {
        pressure: Default::default(),
        agent: AgentConfig::default(),
        providers,
        tools: Default::default(),
        channels: HashMap::new(),
        gateway: Default::default(),
        web_fetch: None,
        tracing: Default::default(),
        memory: Default::default(),
        json_render: Default::default(),
        subagent: Default::default(),
        triggers: Default::default(),
    };

    let provider = config.providers.get_mut("default").unwrap();
    provider.api_url = "https://custom.api.com/v1/chat".to_string();
    provider.base_url = "https://should-be-ignored.com".to_string();

    config.normalize();

    assert_eq!(
        config.providers["default"].api_url,
        "https://custom.api.com/v1/chat"
    );
}

#[test]
fn test_normalize_falls_back_to_default_url_when_no_base() {
    let mut providers = HashMap::new();
    providers.insert("default".to_string(), ProviderConfig::default());
    let mut config = Config {
        pressure: Default::default(),
        agent: AgentConfig::default(),
        providers,
        tools: Default::default(),
        channels: HashMap::new(),
        gateway: Default::default(),
        web_fetch: None,
        tracing: Default::default(),
        memory: Default::default(),
        json_render: Default::default(),
        subagent: Default::default(),
        triggers: Default::default(),
    };

    let provider = config.providers.get_mut("default").unwrap();
    provider.api_url.clear();
    provider.base_url.clear();

    config.normalize();

    assert_eq!(
        config.providers["default"].api_url,
        "https://api.openai.com/v1/chat/completions"
    );
}

#[test]
fn test_normalize_trailing_slash_stripped_from_base_url() {
    let mut providers = HashMap::new();
    providers.insert("default".to_string(), ProviderConfig::default());
    let mut config = Config {
        pressure: Default::default(),
        agent: AgentConfig::default(),
        providers,
        tools: Default::default(),
        channels: HashMap::new(),
        gateway: Default::default(),
        web_fetch: None,
        tracing: Default::default(),
        memory: Default::default(),
        json_render: Default::default(),
        subagent: Default::default(),
        triggers: Default::default(),
    };

    let provider = config.providers.get_mut("default").unwrap();
    provider.api_url.clear();
    provider.base_url = "https://api.example.com/".to_string();

    config.normalize();

    assert_eq!(
        config.providers["default"].api_url,
        "https://api.example.com/v1/chat/completions"
    );
}

#[test]
fn test_normalize_removes_empty_tools_and_channels() {
    let mut providers = HashMap::new();
    providers.insert("default".to_string(), ProviderConfig::default());
    let mut config = Config {
        pressure: Default::default(),
        agent: AgentConfig::default(),
        providers,
        tools: Default::default(),
        channels: HashMap::new(),
        gateway: Default::default(),
        web_fetch: None,
        tracing: Default::default(),
        memory: Default::default(),
        json_render: Default::default(),
        subagent: Default::default(),
        triggers: Default::default(),
    };

    config.tools.entries.push(ToolEntry {
        name: String::new(),
        enabled: true,
    });
    config.tools.entries.push(ToolEntry {
        name: "valid".to_string(),
        enabled: true,
    });
    config.channels.insert(
        "".to_string(),
        ChannelConfig {
            enabled: true,
            extra: std::collections::HashMap::new(),
        },
    );

    config.normalize();

    assert_eq!(config.tools.entries.len(), 1);
    assert_eq!(config.tools.entries[0].name, "valid");
    // Empty-named channels are removed during normalize
    assert!(!config.channels.contains_key(""));
}

#[test]
fn test_normalize_does_not_touch_api_key() {
    let mut providers = HashMap::new();
    providers.insert("default".to_string(), ProviderConfig::default());
    let mut config = Config {
        pressure: Default::default(),
        agent: AgentConfig::default(),
        providers,
        tools: Default::default(),
        channels: HashMap::new(),
        gateway: Default::default(),
        web_fetch: None,
        tracing: Default::default(),
        memory: Default::default(),
        json_render: Default::default(),
        subagent: Default::default(),
        triggers: Default::default(),
    };

    config.providers.get_mut("default").unwrap().api_key = "sk-secret".to_string();
    config.normalize();

    assert_eq!(config.providers["default"].api_key, "sk-secret");
}

#[test]
fn test_normalize_max_iterations_and_timeout_via_clamp() {
    // The new system uses clamp for range-based validation
    let mut providers = HashMap::new();
    providers.insert("default".to_string(), ProviderConfig::default());
    let mut config = Config {
        pressure: Default::default(),
        agent: AgentConfig::default(),
        providers,
        tools: Default::default(),
        channels: HashMap::new(),
        gateway: Default::default(),
        web_fetch: None,
        tracing: Default::default(),
        memory: Default::default(),
        json_render: Default::default(),
        subagent: Default::default(),
        triggers: Default::default(),
    };
    config.agent.max_iterations = 0; // below range
    config.agent.timeout_seconds = 0; // below range

    config.clamp();

    assert_eq!(config.agent.max_iterations, 1); // clamped to min of range(1, 200)
    assert_eq!(config.agent.timeout_seconds, 1); // clamped to min of range(1, 600)
}

// ── Write and reload ──

#[test]
fn test_write_default_config_creates_file() {
    let tmp = NamedTempFile::new().unwrap();
    let path = tmp.path().to_str().unwrap();

    let mut providers = HashMap::new();
    providers.insert("default".to_string(), ProviderConfig::default());
    let config = Config {
        pressure: Default::default(),
        agent: AgentConfig::default(),
        providers,
        tools: Default::default(),
        channels: HashMap::new(),
        gateway: Default::default(),
        web_fetch: None,
        tracing: Default::default(),
        memory: Default::default(),
        json_render: Default::default(),
        subagent: Default::default(),
        triggers: Default::default(),
    };
    config.save(path).unwrap();

    assert!(std::path::Path::new(path).exists());
    assert!(fs::read_to_string(path).unwrap().contains("default"));
}

#[test]
fn test_written_config_is_valid_json() {
    let tmp = NamedTempFile::new().unwrap();
    let path = tmp.path().to_str().unwrap();

    let mut providers = HashMap::new();
    providers.insert("default".to_string(), ProviderConfig::default());
    let config = Config {
        pressure: Default::default(),
        agent: AgentConfig::default(),
        providers,
        tools: Default::default(),
        channels: HashMap::new(),
        gateway: Default::default(),
        web_fetch: None,
        tracing: Default::default(),
        memory: Default::default(),
        json_render: Default::default(),
        subagent: Default::default(),
        triggers: Default::default(),
    };
    config.save(path).unwrap();

    let content = fs::read_to_string(path).unwrap();
    let parsed: serde_json::Value = serde_json::from_str(&content).unwrap();
    assert!(parsed.get("agent").is_some());
    assert!(parsed.get("providers").is_some());
}

// ── Multiple providers ──

#[test]
fn test_normalize_multiple_providers() {
    let mut providers = HashMap::new();
    let default_p = ProviderConfig {
        api_key: "sk-key".to_string(),
        ..Default::default()
    };
    providers.insert("default".to_string(), default_p);

    providers.insert(
        "siliconflow".to_string(),
        ProviderConfig {
            r#type: "custom".to_string(),
            api_url: String::new(),
            base_url: "https://api.siliconflow.cn".to_string(),
            api_key: "sk-sf-key".to_string(),
            model: "Qwen/Qwen2.5-72B-Instruct".to_string(),
            temperature: 0.7,
            max_tokens: 4096,
            context_window_tokens: 32768,
            prompt_cache_enabled: true,
            disable_thinking: false,
            timeout_seconds: 180,
            unknown: Default::default(),
        },
    );
    let mut config = Config {
        pressure: Default::default(),
        agent: AgentConfig {
            provider: "siliconflow".to_string(),
            max_iterations: 40,
            timeout_seconds: 120,
            persist_tool_results: true,
            unknown: Default::default(),
        },
        providers,
        tools: Default::default(),
        channels: HashMap::new(),
        gateway: Default::default(),
        web_fetch: None,
        tracing: Default::default(),
        memory: Default::default(),
        json_render: Default::default(),
        subagent: Default::default(),
        triggers: Default::default(),
    };

    config.normalize();

    let sf = config.providers.get("siliconflow").unwrap();
    assert_eq!(sf.api_url, "https://api.siliconflow.cn/v1/chat/completions");
    assert_eq!(sf.api_key, "sk-sf-key");
}

// ── Constants from macro defaults ──

#[test]
fn test_config_defaults_match_expected_values() {
    let agent_defaults = AgentConfig::defaults();
    let provider_defaults = ProviderConfig::defaults();
    let pressure_defaults = PressureConfig::defaults();

    assert_eq!(agent_defaults["provider"], "");
    assert_eq!(agent_defaults["max_iterations"], 40);
    assert_eq!(agent_defaults["timeout_seconds"], 120);
    assert!(agent_defaults["persist_tool_results"].as_bool().unwrap());
    // Length limits moved out of `agent`: the window lives on the provider and
    // the percentages live in the `pressure` section.
    assert!(!agent_defaults.contains_key("max_tool_result_chars"));
    assert!(!agent_defaults.contains_key("context_window_tokens"));

    assert_eq!(provider_defaults["r#type"], "openai");
    assert_eq!(provider_defaults["context_window_tokens"], 32768);
    assert_eq!(pressure_defaults["single_tool_result_ratio"], 0.25);
    assert_eq!(pressure_defaults["consolidate_trigger_ratio"], 0.5);
    assert_eq!(pressure_defaults["tool_results_ttl_days"], 7);
    assert_eq!(provider_defaults["base_url"], "https://api.openai.com");
    assert_eq!(provider_defaults["model"], "gpt-4o");
    assert_eq!(provider_defaults["temperature"], 0.7);
    assert_eq!(provider_defaults["max_tokens"], 4096);
    assert!(provider_defaults["prompt_cache_enabled"].as_bool().unwrap());
}
