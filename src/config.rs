use std::collections::HashMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

fn default_data_dir() -> String {
    let home = dirs::home_dir()
        .map(|p| p.to_string_lossy().to_string())
        .unwrap_or_else(|| ".".to_string());
    format!("{}/.slimbot", home)
}

fn default_workspace_dir_for_serde() -> String {
    String::new()
}

fn default_temperature() -> f32 {
    0.7
}

fn default_max_tokens() -> u32 {
    4096
}

fn default_max_iterations() -> u32 {
    40
}

fn default_timeout() -> u64 {
    120
}

fn default_true() -> bool {
    true
}

fn default_provider_type() -> String {
    "openai".to_string()
}

#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct ProviderConfig {
    #[serde(default = "default_provider_type")]
    pub r#type: String,
    /// Full API endpoint URL (e.g. "https://api.openai.com/v1/chat/completions").
    /// If empty, derived from `base_url` + "/v1/chat/completions".
    #[serde(default)]
    pub api_url: String,
    /// Base URL for the provider (e.g. "https://api.openai.com").
    /// Used when `api_url` is not set — the full URL is derived from this.
    #[serde(default)]
    pub base_url: String,
    pub api_key: String,
    pub model: String,
    #[serde(default = "default_temperature")]
    pub temperature: f32,
    #[serde(default = "default_max_tokens")]
    pub max_tokens: u32,
}

#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct AgentConfig {
    /// References a key in `Config.providers`.
    pub provider: String,
    #[serde(default = "default_max_iterations")]
    pub max_iterations: u32,
    #[serde(default = "default_timeout")]
    pub timeout_seconds: u64,
}

#[derive(Debug, Deserialize, Serialize)]
pub struct ToolEntry {
    pub name: String,
    #[serde(default = "default_true")]
    pub enabled: bool,
}

#[derive(Debug, Deserialize, Serialize)]
pub struct ChannelEntry {
    pub r#type: String,
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default)]
    pub config: serde_json::Value,
}

#[derive(Debug, Deserialize, Serialize)]
pub struct Config {
    #[serde(default = "default_data_dir")]
    pub data_dir: String,
    /// Directory for workspace files (agent.md, skills, sessions, etc.).
    /// Defaults to `{data_dir}/workspace` if not set.
    #[serde(default = "default_workspace_dir_for_serde")]
    pub workspace_dir: String,
    pub agent: AgentConfig,
    pub providers: HashMap<String, ProviderConfig>,
    #[serde(default)]
    pub tools: Vec<ToolEntry>,
    #[serde(default)]
    pub channels: Vec<ChannelEntry>,
}

impl Config {
    pub fn load(path: &str) -> Result<Self> {
        let content = std::fs::read_to_string(path).context("Failed to read config.json")?;
        let mut config: Config =
            serde_json::from_str(&content).context("Invalid config.json format")?;
        config.validate()?;

        // Derive workspace_dir from data_dir if not explicitly set
        if config.workspace_dir.is_empty() {
            config.workspace_dir = format!("{}/workspace", config.data_dir);
        }

        std::fs::create_dir_all(config.session_dir())?;

        Ok(config)
    }

    pub fn save(&self, path: &str) -> Result<()> {
        if let Some(parent) = Path::new(path).parent() {
            std::fs::create_dir_all(parent)?;
        }
        let content = serde_json::to_string_pretty(self)?;
        std::fs::write(path, content)?;
        Ok(())
    }

    /// Returns the workspace directory path.
    pub fn workspace_dir(&self) -> PathBuf {
        Path::new(&self.workspace_dir).to_path_buf()
    }

    /// Returns the sessions directory path.
    pub fn session_dir(&self) -> PathBuf {
        Path::new(&self.workspace_dir).join("sessions")
    }

    fn validate(&self) -> Result<()> {
        if self.agent.provider.is_empty() {
            anyhow::bail!("agent.provider must not be empty");
        }
        let provider = self.providers.get(&self.agent.provider).ok_or_else(|| {
            anyhow::anyhow!(
                "Provider '{}' referenced by agent not found in providers",
                self.agent.provider
            )
        })?;
        if provider.api_key.is_empty() {
            anyhow::bail!(
                "provider '{}'.api_key must not be empty",
                self.agent.provider
            );
        }
        if provider.model.is_empty() {
            anyhow::bail!("provider '{}'.model must not be empty", self.agent.provider);
        }
        Ok(())
    }
}
