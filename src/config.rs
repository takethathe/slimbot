use std::collections::HashMap;
use std::path::Path;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

fn default_context_window_tokens() -> u32 {
    8192
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

fn default_max_tool_result_chars() -> u32 {
    8000
}

fn default_persist_tool_results() -> bool {
    true
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

fn default_heartbeat_interval() -> u64 { 1800 }

// ── Gateway config types ──

#[derive(Debug, Clone, Deserialize, Serialize, Default)]
pub struct CronConfig {
    #[serde(default = "default_true")]
    pub enabled: bool,
}

#[derive(Debug, Clone, Deserialize, Serialize, Default)]
pub struct HeartbeatConfig {
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default = "default_heartbeat_interval")]
    pub interval_s: u64,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct GatewayConfig {
    #[serde(default)]
    pub cron: CronConfig,
    #[serde(default)]
    pub heartbeat: HeartbeatConfig,
}

impl Default for GatewayConfig {
    fn default() -> Self {
        Self {
            cron: CronConfig { enabled: true },
            heartbeat: HeartbeatConfig { enabled: true, interval_s: 1800 },
        }
    }
}

// ── Channel config ──

#[derive(Debug, Clone, Deserialize, Serialize, Default)]
pub struct ChannelConfig {
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(flatten)]
    pub extra: HashMap<String, serde_json::Value>,
}

// ── Backward compatibility: old Vec<ChannelEntry> → HashMap<String, ChannelConfig> ──

fn deserialize_channels<'de, D>(deserializer: D) -> std::result::Result<HashMap<String, ChannelConfig>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    use serde::de::{self, Visitor};

    struct ChannelsVisitor;

    impl<'de> Visitor<'de> for ChannelsVisitor {
        type Value = HashMap<String, ChannelConfig>;

        fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
            f.write_str("a map or a list of channel entries")
        }

        fn visit_map<M>(self, mut map: M) -> std::result::Result<Self::Value, M::Error>
        where
            M: de::MapAccess<'de>,
        {
            let mut m = HashMap::new();
            while let Some((key, value)) = map.next_entry::<String, ChannelConfig>()? {
                m.insert(key, value);
            }
            Ok(m)
        }

        fn visit_seq<A>(self, mut seq: A) -> std::result::Result<Self::Value, A::Error>
        where
            A: de::SeqAccess<'de>,
        {
            let mut m = HashMap::new();
            while let Some(entry) = seq.next_element::<ChannelEntryCompat>()? {
                let r#type = entry.r#type.clone();
                m.insert(r#type, entry.into_channel_config());
            }
            Ok(m)
        }
    }

    deserializer.deserialize_any(ChannelsVisitor)
}

#[derive(Debug, Deserialize)]
struct ChannelEntryCompat {
    r#type: String,
    #[serde(default = "default_true")]
    enabled: bool,
    #[serde(default)]
    config: serde_json::Value,
}

impl ChannelEntryCompat {
    fn into_channel_config(self) -> ChannelConfig {
        let mut extra = match self.config {
            serde_json::Value::Object(m) => m.into_iter().collect::<HashMap<_, _>>(),
            _ => HashMap::new(),
        };
        extra.insert("enabled".to_string(), serde_json::json!(self.enabled));
        ChannelConfig { enabled: self.enabled, extra }
    }
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
    /// Whether to enable prompt caching via `cache_control` annotation on the
    /// last message. Defaults to true.
    #[serde(default = "default_true")]
    pub prompt_cache_enabled: bool,
}

#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct AgentConfig {
    /// References a key in `Config.providers`.
    pub provider: String,
    #[serde(default = "default_max_iterations")]
    pub max_iterations: u32,
    #[serde(default = "default_timeout")]
    pub timeout_seconds: u64,
    /// Maximum characters for a tool result before truncation.
    #[serde(default = "default_max_tool_result_chars")]
    pub max_tool_result_chars: u32,
    /// Whether to persist oversized tool results to disk.
    #[serde(default = "default_persist_tool_results")]
    pub persist_tool_results: bool,
    /// LLM context window size in tokens. Used by consolidate to trigger
    /// summarization when prompt usage exceeds this budget.
    #[serde(default = "default_context_window_tokens")]
    pub context_window_tokens: u32,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ToolEntry {
    pub name: String,
    #[serde(default = "default_true")]
    pub enabled: bool,
}

#[allow(dead_code)]
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ChannelEntry {
    pub r#type: String,
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default)]
    pub config: serde_json::Value,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Config {
    pub agent: AgentConfig,
    pub providers: HashMap<String, ProviderConfig>,
    #[serde(default)]
    pub tools: Vec<ToolEntry>,
    #[serde(default, deserialize_with = "deserialize_channels")]
    pub channels: HashMap<String, ChannelConfig>,
    #[serde(default)]
    pub gateway: GatewayConfig,
}

impl Config {
    pub fn load(path: &str) -> Result<Self> {
        let content = std::fs::read_to_string(path).context("Failed to read config.json")?;
        let config: Config =
            serde_json::from_str(&content).context("Invalid config.json format")?;
        config.validate()?;
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
                "provider '{}'.api_key must not be empty. Set it in your config file.",
                self.agent.provider
            );
        }
        if provider.model.is_empty() {
            anyhow::bail!("provider '{}'.model must not be empty", self.agent.provider);
        }
        Ok(())
    }
}

#[cfg(test)]
mod config_tests {
    use super::*;

    #[test]
    fn test_channels_hashmap_deserialize() {
        let json = r#"{
            "agent": {"provider": "test", "max_iterations": 40, "timeout_seconds": 120, "max_tool_result_chars": 8000, "persist_tool_results": false, "context_window_tokens": 8192},
            "providers": {"test": {"api_key": "k", "model": "m"}},
            "channels": {
                "webui": {"enabled": true, "host": "0.0.0.0", "port": 8080}
            }
        }"#;
        let config: Config = serde_json::from_str(json).unwrap();
        assert!(config.channels.contains_key("webui"));
        assert!(config.channels["webui"].enabled);
    }

    #[test]
    fn test_channels_vec_compat_deserialize() {
        let json = r#"{
            "agent": {"provider": "test", "max_iterations": 40, "timeout_seconds": 120, "max_tool_result_chars": 8000, "persist_tool_results": false, "context_window_tokens": 8192},
            "providers": {"test": {"api_key": "k", "model": "m"}},
            "channels": [
                {"type": "cli", "enabled": true, "config": {"prompt": "> "}}
            ]
        }"#;
        let config: Config = serde_json::from_str(json).unwrap();
        assert!(config.channels.contains_key("cli"));
        assert!(config.channels["cli"].enabled);
    }

    #[test]
    fn test_gateway_config_defaults() {
        let json = r#"{
            "agent": {"provider": "test", "max_iterations": 40, "timeout_seconds": 120, "max_tool_result_chars": 8000, "persist_tool_results": false, "context_window_tokens": 8192},
            "providers": {"test": {"api_key": "k", "model": "m"}}
        }"#;
        let config: Config = serde_json::from_str(json).unwrap();
        assert!(config.gateway.cron.enabled);
        assert!(config.gateway.heartbeat.enabled);
        assert_eq!(config.gateway.heartbeat.interval_s, 1800);
    }
}
