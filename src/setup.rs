use anyhow::Result;

use crate::bootstrap::{bootstrap_files, skill_files};
use crate::config::Config;
use crate::config_defs::{AgentConfig, ProviderConfig};
use crate::debug;
use crate::path::PathManager;

/// Build a default Config for initial setup. All values come from macro defaults.
fn default_config() -> Config {
    let mut providers = std::collections::HashMap::new();
    providers.insert("default".to_string(), ProviderConfig::default());
    Config {
        agent: AgentConfig::default(),
        providers,
        tools: vec![],
        channels: std::collections::HashMap::new(),
        gateway: Default::default(),
    }
}

/// Run the setup command: write default config or normalize existing one.
pub fn run_setup(
    config_path: Option<&str>,
    data_dir: &str,
    workspace_dir: Option<&str>,
) -> Result<()> {
    debug!("[setup] starting setup with data_dir={}", data_dir);
    let paths = PathManager::resolve(config_path, Some(data_dir), workspace_dir)?;
    let path = paths.config_path().to_str().unwrap();

    if std::path::Path::new(path).exists() {
        // Config exists — clamp, normalize, and write back (skip validation for preview)
        eprintln!("Config file found at: {}", path);
        let raw_content = std::fs::read_to_string(path)?;
        let config = Config::load_for_preview(path)?;
        let normalized = serde_json::to_string_pretty(&config)?;

        let needs_save = raw_content.trim() != normalized.trim();
        if needs_save {
            std::fs::write(path, &normalized)?;
            eprintln!("Missing default fields have been filled in.");
        } else {
            eprintln!("Config is already complete, no changes needed.");
        }

        // Create directories and bootstrap files
        if let Err(e) = create_directories_and_bootstrap(&paths) {
            eprintln!("Warning: failed to create workspace directories: {}", e);
        }

        // Use already-loaded config for summary
        print_config_summary(&config, &paths);
    } else {
        // Config doesn't exist — write full default config
        eprintln!("Writing default config to: {}", path);
        let config = default_config();
        config.save(path)?;
        eprintln!("Default config created successfully.");

        // Create directories and bootstrap files
        if let Err(e) = create_directories_and_bootstrap(&paths) {
            eprintln!("Warning: failed to create workspace directories: {}", e);
        }

        print_config_summary(&config, &paths);
    }

    Ok(())
}

fn print_config_summary(config: &Config, paths: &PathManager) {
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
    eprintln!("\nEdit {} to configure your API key and channels.", paths.config_path().display());
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
