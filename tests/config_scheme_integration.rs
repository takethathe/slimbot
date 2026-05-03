use std::fs;

use tempfile::NamedTempFile;

use slimbot::{ConfigScheme, ProviderConfig, ToolEntry, ChannelEntry};

// ── Default config generation ──

#[test]
fn test_default_config_structure() {
    let scheme = ConfigScheme::new();
    let config = scheme.default_config();

    assert_eq!(config.agent.provider, "default");
    assert!(config.providers.contains_key("default"));
    assert!(config.tools.is_empty());
    assert!(config.channels.is_empty());

    let provider = &config.providers["default"];
    assert_eq!(provider.r#type, ConfigScheme::DEFAULT_PROVIDER_TYPE);
    assert_eq!(provider.base_url, ConfigScheme::DEFAULT_BASE_URL);
    assert!(provider.api_url.is_empty()); // derived during normalize
    assert!(provider.api_key.is_empty()); // intentional, user must set
    assert_eq!(provider.model, ConfigScheme::DEFAULT_MODEL);
}

// ── Normalization ──

#[test]
fn test_normalize_fills_empty_agent_provider() {
    let scheme = ConfigScheme::new();
    let mut config = scheme.default_config();
    config.agent.provider.clear();

    scheme.normalize(&mut config);
    assert_eq!(config.agent.provider, "default");
}

#[test]
fn test_normalize_derives_url_from_base_url() {
    let scheme = ConfigScheme::new();
    let mut config = scheme.default_config();

    let provider = config.providers.get_mut("default").unwrap();
    provider.api_url.clear();
    provider.base_url = "https://api.example.com".to_string();

    scheme.normalize(&mut config);

    let provider = config.providers.get("default").unwrap();
    assert_eq!(
        provider.api_url,
        "https://api.example.com/v1/chat/completions"
    );
}

#[test]
fn test_normalize_preserves_existing_api_url() {
    let scheme = ConfigScheme::new();
    let mut config = scheme.default_config();

    let provider = config.providers.get_mut("default").unwrap();
    provider.api_url = "https://custom.api.com/v1/chat".to_string();
    provider.base_url = "https://should-be-ignored.com".to_string();

    scheme.normalize(&mut config);

    assert_eq!(
        config.providers["default"].api_url,
        "https://custom.api.com/v1/chat"
    );
}

#[test]
fn test_normalize_falls_back_to_default_url_when_no_base() {
    let scheme = ConfigScheme::new();
    let mut config = scheme.default_config();

    let provider = config.providers.get_mut("default").unwrap();
    provider.api_url.clear();
    provider.base_url.clear();

    scheme.normalize(&mut config);

    assert_eq!(
        config.providers["default"].api_url,
        ConfigScheme::DEFAULT_API_URL
    );
}

#[test]
fn test_normalize_trailing_slash_stripped_from_base_url() {
    let scheme = ConfigScheme::new();
    let mut config = scheme.default_config();

    let provider = config.providers.get_mut("default").unwrap();
    provider.api_url.clear();
    provider.base_url = "https://api.example.com/".to_string();

    scheme.normalize(&mut config);

    assert_eq!(
        config.providers["default"].api_url,
        "https://api.example.com/v1/chat/completions"
    );
}

#[test]
fn test_normalize_removes_empty_tools_and_channels() {
    let scheme = ConfigScheme::new();
    let mut config = scheme.default_config();

    config.tools.push(ToolEntry {
        name: String::new(),
        enabled: true,
    });
    config.tools.push(ToolEntry {
        name: "valid".to_string(),
        enabled: true,
    });
    config.channels.push(ChannelEntry {
        r#type: String::new(),
        enabled: true,
        config: serde_json::json!({}),
    });

    scheme.normalize(&mut config);

    assert_eq!(config.tools.len(), 1);
    assert_eq!(config.tools[0].name, "valid");
    assert!(config.channels.is_empty());
}

#[test]
fn test_normalize_fixes_out_of_range_temperature() {
    let scheme = ConfigScheme::new();
    let mut config = scheme.default_config();

    config.providers.get_mut("default").unwrap().temperature = 5.0;
    scheme.normalize(&mut config);

    assert_eq!(config.providers["default"].temperature, 0.7);
}

#[test]
fn test_normalize_does_not_touch_api_key() {
    let scheme = ConfigScheme::new();
    let mut config = scheme.default_config();

    config.providers.get_mut("default").unwrap().api_key = "sk-secret".to_string();
    scheme.normalize(&mut config);

    assert_eq!(config.providers["default"].api_key, "sk-secret");
}

#[test]
fn test_normalize_max_iterations_and_timeout() {
    let scheme = ConfigScheme::new();
    let mut config = scheme.default_config();
    config.agent.max_iterations = 0;
    config.agent.timeout_seconds = 0;

    scheme.normalize(&mut config);

    assert_eq!(config.agent.max_iterations, ConfigScheme::DEFAULT_MAX_ITERATIONS);
    assert_eq!(config.agent.timeout_seconds, ConfigScheme::DEFAULT_TIMEOUT);
}

// ── Write and reload ──

#[test]
fn test_write_default_config_creates_file() {
    let tmp = NamedTempFile::new().unwrap();
    let path = tmp.path().to_str().unwrap();

    let scheme = ConfigScheme::new();
    scheme.write_default_config(path).unwrap();

    assert!(scheme.config_exists(path));
    assert!(fs::read_to_string(path).unwrap().contains("default"));
}

#[test]
fn test_written_config_is_valid_json() {
    let tmp = NamedTempFile::new().unwrap();
    let path = tmp.path().to_str().unwrap();

    let scheme = ConfigScheme::new();
    scheme.write_default_config(path).unwrap();

    let content = fs::read_to_string(path).unwrap();
    let parsed: serde_json::Value = serde_json::from_str(&content).unwrap();
    assert!(parsed.get("agent").is_some());
    assert!(parsed.get("providers").is_some());
}

// ── Multiple providers ──

#[test]
fn test_normalize_multiple_providers() {
    let scheme = ConfigScheme::new();
    let mut config = scheme.default_config();

    config.providers.insert(
        "siliconflow".to_string(),
        ProviderConfig {
            r#type: "custom".to_string(),
            api_url: String::new(),
            base_url: "https://api.siliconflow.cn".to_string(),
            api_key: "sk-sf-key".to_string(),
            model: "Qwen/Qwen2.5-72B-Instruct".to_string(),
            temperature: 0.7,
            max_tokens: 4096,
            prompt_cache_enabled: true,
        },
    );
    config.agent.provider = "siliconflow".to_string();

    scheme.normalize(&mut config);

    let sf = config.providers.get("siliconflow").unwrap();
    assert_eq!(sf.api_url, "https://api.siliconflow.cn/v1/chat/completions");
    assert_eq!(sf.api_key, "sk-sf-key");
}

// ── Constants ──

#[test]
fn test_config_scheme_constants() {
    assert_eq!(ConfigScheme::DEFAULT_PROVIDER_TYPE, "openai");
    assert_eq!(ConfigScheme::DEFAULT_MODEL, "gpt-4o");
    assert_eq!(ConfigScheme::DEFAULT_TEMPERATURE, 0.7);
    assert_eq!(ConfigScheme::DEFAULT_MAX_TOKENS, 4096);
    assert_eq!(ConfigScheme::DEFAULT_MAX_ITERATIONS, 40);
    assert_eq!(ConfigScheme::DEFAULT_TIMEOUT, 120);
    assert_eq!(
        ConfigScheme::DEFAULT_API_URL,
        "https://api.openai.com/v1/chat/completions"
    );
    assert_eq!(ConfigScheme::DEFAULT_BASE_URL, "https://api.openai.com");
}
