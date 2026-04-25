use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

fn default_data_dir() -> String {
    let home = dirs::home_dir().map(|p| p.to_string_lossy().to_string())
        .unwrap_or_else(|| ".".to_string());
    format!("{}/.slimbot", home)
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

#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct ProviderConfig {
    pub api_url: String,
    pub api_key: String,
    pub model: String,
    #[serde(default = "default_temperature")]
    pub temperature: f32,
    #[serde(default = "default_max_tokens")]
    pub max_tokens: u32,
}

#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct AgentConfig {
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
    pub provider: ProviderConfig,
    pub agent: AgentConfig,
    #[serde(default)]
    pub tools: Vec<ToolEntry>,
    #[serde(default)]
    pub channels: Vec<ChannelEntry>,
}

impl Config {
    pub fn load(path: &str) -> Result<Self> {
        let content = std::fs::read_to_string(path)
            .context("Failed to read config.json")?;
        let config: Config = serde_json::from_str(&content)
            .context("Invalid config.json format")?;
        config.validate()?;

        let data_dir = Path::new(&config.data_dir);
        std::fs::create_dir_all(data_dir.join("workspace/sessions"))?;

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

    pub fn session_dir(&self) -> PathBuf {
        Path::new(&self.data_dir).join("workspace/sessions")
    }

    fn validate(&self) -> Result<()> {
        if self.provider.api_key.is_empty() {
            anyhow::bail!("provider.api_key must not be empty");
        }
        if self.provider.model.is_empty() {
            anyhow::bail!("provider.model must not be empty");
        }
        if self.agent.max_iterations == 0 {
            anyhow::bail!("agent.max_iterations must be greater than 0");
        }
        Ok(())
    }
}
