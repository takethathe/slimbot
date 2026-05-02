use anyhow::Result;

use crate::bootstrap::{bootstrap_files, skill_files};
use crate::config::{AgentConfig, Config, ProviderConfig};
use crate::config_scheme::ConfigScheme;
use crate::path::PathManager;

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
pub fn run_setup(
    config_path: Option<&str>,
    data_dir: &str,
    workspace_dir: Option<&str>,
) -> Result<()> {
    let paths = PathManager::resolve(config_path, Some(data_dir), workspace_dir)?;

    // Use the config path from PathManager for setup
    let path = paths.config_path().to_str().unwrap();

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
    let mut config = load_config_with_defaults(path, &scheme)?;
    scheme.normalize(&mut config);

    // Create directories and bootstrap files
    if let Err(e) = create_directories_and_bootstrap(&paths) {
        eprintln!("Warning: failed to create workspace directories: {}", e);
    }

    eprintln!("\nConfig preview:");
    eprintln!("  data_dir: {}", paths.data_dir().display());
    eprintln!("  workspace_dir: {}", paths.workspace_dir().display());
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

/// Create workspace directories and write bootstrap files.
/// Skips files that already exist.
fn create_directories_and_bootstrap(paths: &PathManager) -> Result<()> {
    std::fs::create_dir_all(paths.data_dir())?;
    std::fs::create_dir_all(paths.workspace_dir())?;
    std::fs::create_dir_all(paths.skills_dir())?;
    std::fs::create_dir_all(paths.memory_dir())?;

    // Write bootstrap files (skip if already exists)
    for (filename, template) in bootstrap_files() {
        let path = paths.bootstrap_file(filename);
        if path.exists() {
            eprintln!("  {} already exists, skipping.", filename);
        } else {
            std::fs::write(&path, template)?;
            eprintln!("  Created {}.", filename);
        }
    }

    // Write skill files (skip if already exists)
    for (_filename, content, dest) in skill_files() {
        let path = paths.workspace_dir().join(dest);
        if path.exists() {
            eprintln!("  {} already exists, skipping.", dest);
        } else {
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::write(&path, content)?;
            eprintln!("  Created {}.", dest);
        }
    }

    Ok(())
}
