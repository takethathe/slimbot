use anyhow::Result;

use crate::config::{AgentConfig, Config, ProviderConfig};
use crate::config_scheme::ConfigScheme;

fn default_fallback_provider() -> ProviderConfig {
    ProviderConfig {
        r#type: "custom".to_string(),
        api_url: String::new(),
        base_url: String::new(),
        api_key: String::new(),
        model: String::new(),
        temperature: ConfigScheme::DEFAULT_TEMPERATURE,
        max_tokens: ConfigScheme::DEFAULT_MAX_TOKENS,
    }
}

/// Load an existing config file, using serde defaults for missing fields.
/// If deserialization fails entirely, falls back to the scheme's default config.
fn load_config_with_defaults(path: &str, scheme: &ConfigScheme) -> Result<Config> {
    let content = std::fs::read_to_string(path)?;

    // Try normal deserialization first
    if let Ok(config) = serde_json::from_str::<Config>(&content) {
        return Ok(config);
    }

    // Partial config — try to parse as serde_json::Value and merge into defaults
    let value: serde_json::Value = serde_json::from_str(&content)
        .map_err(|e| anyhow::anyhow!("Invalid config JSON: {}", e))?;

    let mut config = scheme.default_config();

    // Merge top-level fields
    if let Some(v) = value.get("data_dir").and_then(|v| v.as_str()) {
        config.data_dir = v.to_string();
    }
    if let Some(v) = value.get("workspace_dir").and_then(|v| v.as_str()) {
        config.workspace_dir = v.to_string();
    }
    if let Some(obj) = value.get("agent").and_then(|v| v.as_object()) {
        merge_agent(&mut config.agent, obj);
    }
    if let Some(providers) = value.get("providers").and_then(|v| v.as_object()) {
        for (key, val) in providers {
            if let Some(obj) = val.as_object() {
                if let Some(existing) = config.providers.get_mut(key) {
                    merge_provider(existing, obj);
                } else {
                    let mut provider = config
                        .providers
                        .values()
                        .next()
                        .cloned()
                        .unwrap_or_else(default_fallback_provider);
                    merge_provider(&mut provider, obj);
                    config.providers.insert(key.clone(), provider);
                }
            }
        }
    }
    // Backward compat: merge legacy "provider" object into the default provider
    if let Some(obj) = value.get("provider").and_then(|v| v.as_object()) {
        if let Some(default_provider) = config.providers.get_mut("default") {
            merge_provider(default_provider, obj);
        }
    }
    if let Some(arr) = value.get("tools").and_then(|v| v.as_array()) {
        config.tools = arr
            .iter()
            .filter_map(|v| serde_json::from_value(v.clone()).ok())
            .collect();
    }
    if let Some(arr) = value.get("channels").and_then(|v| v.as_array()) {
        config.channels = arr
            .iter()
            .filter_map(|v| serde_json::from_value(v.clone()).ok())
            .collect();
    }

    Ok(config)
}

fn merge_provider(target: &mut ProviderConfig, src: &serde_json::Map<String, serde_json::Value>) {
    if let Some(v) = src.get("type").and_then(|v| v.as_str()) {
        target.r#type = v.to_string();
    }
    if let Some(v) = src.get("api_url").and_then(|v| v.as_str()) {
        target.api_url = v.to_string();
    }
    if let Some(v) = src.get("base_url").and_then(|v| v.as_str()) {
        target.base_url = v.to_string();
    }
    if let Some(v) = src.get("api_key").and_then(|v| v.as_str()) {
        target.api_key = v.to_string();
    }
    if let Some(v) = src.get("model").and_then(|v| v.as_str()) {
        target.model = v.to_string();
    }
    if let Some(v) = src.get("temperature").and_then(|v| v.as_f64()) {
        target.temperature = v as f32;
    }
    if let Some(v) = src.get("max_tokens").and_then(|v| v.as_u64()) {
        target.max_tokens = v as u32;
    }
}

fn merge_agent(target: &mut AgentConfig, src: &serde_json::Map<String, serde_json::Value>) {
    if let Some(v) = src.get("provider").and_then(|v| v.as_str()) {
        target.provider = v.to_string();
    }
    if let Some(v) = src.get("max_iterations").and_then(|v| v.as_u64()) {
        target.max_iterations = v as u32;
    }
    if let Some(v) = src.get("timeout_seconds").and_then(|v| v.as_u64()) {
        target.timeout_seconds = v as u64;
    }
}

/// Run the setup command: write default config or normalize existing one.
pub fn run_setup(config_path: Option<&str>) -> Result<()> {
    let path = config_path.unwrap_or_else(|| {
        let home = dirs::home_dir()
            .map(|p| p.to_string_lossy().to_string())
            .unwrap_or_else(|| ".".to_string());
        Box::leak(format!("{}/.slimbot/config.json", home).into_boxed_str())
    });

    let scheme = ConfigScheme::new();

    if scheme.config_exists(path) {
        // Config exists — load, normalize missing fields, and write back
        eprintln!("Config file found at: {}", path);
        let raw_content = std::fs::read_to_string(path)?;
        let mut config = load_config_with_defaults(path, &scheme)?;
        scheme.normalize(&mut config);
        let normalized = serde_json::to_string_pretty(&config)?;

        // Compare normalized output against the raw file content
        // (raw may differ if it was missing defaults or had formatting differences)
        let needs_save = raw_content.trim() != normalized.trim();
        if needs_save {
            std::fs::write(path, &normalized)?;
            eprintln!("Missing default fields have been filled in.");
        } else {
            eprintln!("Config is already complete, no changes needed.");
        }
    } else {
        // Config doesn't exist — write full default config
        eprintln!("Writing default config to: {}", path);
        scheme.write_default_config(path)?;
        eprintln!("Default config created successfully.");
    }

    // Print config summary (skip validation — api_key may be empty)
    let config = load_config_with_defaults(path, &scheme)?;
    eprintln!("\nConfig summary:");
    eprintln!("  data_dir: {}", config.data_dir);
    eprintln!("  workspace_dir: {}", config.workspace_dir);
    eprintln!("  agent.provider: {}", config.agent.provider);
    eprintln!("  agent.max_iterations: {}", config.agent.max_iterations);
    eprintln!("  agent.timeout_seconds: {}", config.agent.timeout_seconds);
    eprintln!("  providers:");
    for (name, provider) in &config.providers {
        eprintln!(
            "    {}: type={} model={} api_key={}",
            name,
            provider.r#type,
            provider.model,
            mask_api_key(&provider.api_key)
        );
    }
    eprintln!("  tools: {} entries", config.tools.len());
    eprintln!("  channels: {} entries", config.channels.len());
    eprintln!("\nEdit {} to configure your API key and channels.", path);

    Ok(())
}

fn mask_api_key(key: &str) -> String {
    if key.is_empty() {
        return "<not set>".to_string();
    }
    if key.len() <= 8 {
        return "****".to_string();
    }
    format!("{}****{}", &key[..4], &key[key.len() - 4..])
}
