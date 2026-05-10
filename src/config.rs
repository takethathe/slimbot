use std::collections::{BTreeMap, HashMap};
use std::path::Path;
use std::sync::Arc;

use anyhow::{Context, Result};
use notify::Watcher;
use parking_lot::RwLock;
use serde::{Deserialize, Serialize};

use crate::config_normalize::apply_normalize;

// Re-export for backward compatibility (other modules import these from crate::config)
pub use crate::config_defs::{AgentConfig, ProviderConfig};

// ── Root Config ──

#[derive(Clone, Debug, Serialize, Deserialize)]
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

#[derive(Debug, Clone, Deserialize, Serialize, Default)]
pub struct GatewayConfig {
    #[serde(default)]
    pub cron: CronConfig,
    #[serde(default)]
    pub heartbeat: HeartbeatConfig,
}

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
pub struct ChannelConfig {
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(flatten)]
    pub extra: HashMap<String, serde_json::Value>,
}

impl Default for ChannelConfig {
    fn default() -> Self {
        ChannelConfig {
            enabled: true,
            extra: HashMap::new(),
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ToolEntry {
    pub name: String,
    #[serde(default = "default_true")]
    pub enabled: bool,
}

fn default_true() -> bool { true }
fn default_heartbeat_interval() -> u64 { 1800 }

// ── Backward compat: Vec<ChannelEntry> → HashMap ──

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
            let mut idx = 0;
            while let Some(entry) = seq.next_element::<ChannelEntryCompat>()? {
                let key = if entry.r#type.is_empty() {
                    format!("legacy_{idx}")
                } else {
                    entry.r#type.clone()
                };
                m.insert(key, entry.into_channel_config());
                idx += 1;
            }
            Ok(m)
        }
    }

    deserializer.deserialize_any(ChannelsVisitor)
}

#[derive(Debug, Deserialize)]
struct ChannelEntryCompat {
    #[serde(default)]
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

impl Config {
    pub fn clamp(&mut self) {
        self.agent.clamp();
        for provider in self.providers.values_mut() {
            provider.clamp();
        }
    }

    pub fn normalize(&mut self) {
        apply_normalize(self);
        // Remove entries with empty name/type
        self.tools.retain(|t| !t.name.is_empty());
        self.channels.retain(|name, _| !name.is_empty());
    }

    pub fn validate(&self) -> Result<()> {
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

    pub fn load(path: &str) -> Result<Self> {
        let content = std::fs::read_to_string(path).context("Failed to read config file")?;
        let mut config: Config = serde_json::from_str(&content).context("Invalid config JSON")?;
        config.clamp();
        config.normalize();
        config.validate()?;
        Ok(config)
    }

    /// Load config without validation (for setup preview where api_key may be empty).
    pub fn load_for_preview(path: &str) -> Result<Self> {
        let content = std::fs::read_to_string(path).context("Failed to read config file")?;
        let mut config: Config = serde_json::from_str(&content).context("Invalid config JSON")?;
        config.clamp();
        config.normalize();
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
}

// ── ConfigChange, ConfigValue ──

#[derive(Debug, Clone)]
pub enum ConfigValue {
    Str(String),
    Num(f64),
    Bool(bool),
    Object(serde_json::Map<String, serde_json::Value>),
}

#[derive(Debug, Clone)]
pub struct ConfigChange {
    pub paths: Vec<String>,
    pub old_values: BTreeMap<String, ConfigValue>,
    pub new_values: BTreeMap<String, ConfigValue>,
}

fn config_value_from_json(v: &serde_json::Value) -> ConfigValue {
    match v {
        serde_json::Value::String(s) => ConfigValue::Str(s.clone()),
        serde_json::Value::Number(n) => ConfigValue::Num(n.as_f64().unwrap_or(0.0)),
        serde_json::Value::Bool(b) => ConfigValue::Bool(*b),
        serde_json::Value::Object(m) => ConfigValue::Object(m.clone()),
        _ => ConfigValue::Str(String::new()),
    }
}

// ── Global Singleton ──

use std::sync::OnceLock;

static CONFIG_INSTANCE: OnceLock<Arc<RwLock<ConfigInner>>> = OnceLock::new();

struct ConfigInner {
    config: Config,
    config_path: String,
    subscribers: Vec<(String, Arc<dyn Fn(ConfigChange) + Send + Sync>)>,
    watcher: Option<notify::RecommendedWatcher>,
}

impl Config {
    pub fn init(path: &str) -> Result<()> {
        let config = Self::load(path)?;
        let inner = Arc::new(RwLock::new(ConfigInner {
            config,
            config_path: path.to_string(),
            subscribers: Vec::new(),
            watcher: None,
        }));

        // Start file watcher
        {
            let inner_clone = inner.clone();
            let path_clone = path.to_string();
            let mut watcher = notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
                if let Ok(event) = res {
                    if matches!(event.kind, notify::EventKind::Modify(_)) {
                        if let Err(e) = Self::reload_with_inner(&inner_clone) {
                            crate::error!("[Config] Reload failed: {}", e);
                        }
                    }
                }
            }).context("Failed to create config watcher")?;

            watcher.watch(
                Path::new(&path_clone),
                notify::RecursiveMode::NonRecursive,
            ).context("Failed to watch config file")?;

            inner.write().watcher = Some(watcher);
        }

        CONFIG_INSTANCE.set(inner).map_err(|_| anyhow::anyhow!("Config already initialized"))?;
        Ok(())
    }

    pub fn get() -> Arc<Config> {
        let inner = CONFIG_INSTANCE
            .get()
            .expect("Config not initialized. Call Config::init() first.");
        Arc::new(inner.read().config.clone())
    }

    pub fn subscribe<F>(path: &str, callback: F)
    where
        F: Fn(ConfigChange) + Send + Sync + 'static,
    {
        let inner = CONFIG_INSTANCE
            .get()
            .expect("Config not initialized");
        inner.write().subscribers.push((path.to_string(), Arc::new(callback)));
    }

    fn reload_with_inner(inner: &Arc<RwLock<ConfigInner>>) -> Result<()> {
        let path = {
            let guard = inner.read();
            guard.config_path.clone()
        };

        let new_config = Self::load(&path)?;

        let change = {
            let guard = inner.read();
            Self::compute_diff(&guard.config, &new_config)
        };

        if change.paths.is_empty() {
            return Ok(());
        }

        // Swap
        {
            let mut guard = inner.write();
            guard.config = new_config;
        }

        // Notify subscribers
        let subscribers: Vec<_> = {
            let guard = inner.read();
            guard.subscribers.clone()
        };

        for (prefix, callback) in subscribers {
            let matching: Vec<_> = change.paths.iter()
                .filter(|p| p.starts_with(&prefix) || prefix.starts_with(p.as_str()))
                .cloned()
                .collect();
            if !matching.is_empty() {
                callback(ConfigChange {
                    paths: matching,
                    old_values: change.old_values.clone(),
                    new_values: change.new_values.clone(),
                });
            }
        }

        crate::info!("[Config] Reloaded config with {} changed paths", change.paths.len());
        Ok(())
    }

    fn compute_diff(old: &Config, new: &Config) -> ConfigChange {
        let mut paths = Vec::new();
        let mut old_values = BTreeMap::new();
        let mut new_values = BTreeMap::new();

        // Diff agent fields
        let old_agent = serde_json::to_value(&old.agent).unwrap_or_default();
        let new_agent = serde_json::to_value(&new.agent).unwrap_or_default();
        if old_agent != new_agent {
            if let (Some(old_obj), Some(new_obj)) = (old_agent.as_object(), new_agent.as_object()) {
                for (key, new_val) in new_obj {
                    if !old_obj.get(key).map(|v| v == new_val).unwrap_or(false) {
                        let path = format!("agent.{}", key);
                        paths.push(path.clone());
                        old_values.insert(path.clone(), config_value_from_json(old_obj.get(key).unwrap_or(&serde_json::Value::Null)));
                        new_values.insert(path, config_value_from_json(new_val));
                    }
                }
            }
        }

        // Diff providers
        for (name, new_p) in &new.providers {
            if let Some(old_p) = old.providers.get(name) {
                let old_j = serde_json::to_value(old_p).unwrap_or_default();
                let new_j = serde_json::to_value(new_p).unwrap_or_default();
                if old_j != new_j {
                    if let (Some(old_obj), Some(new_obj)) = (old_j.as_object(), new_j.as_object()) {
                        for (key, new_val) in new_obj {
                            if !old_obj.get(key).map(|v| v == new_val).unwrap_or(false) {
                                let path = format!("providers.{}.{}", name, key);
                                paths.push(path.clone());
                                old_values.insert(path.clone(), config_value_from_json(old_obj.get(key).unwrap_or(&serde_json::Value::Null)));
                                new_values.insert(path, config_value_from_json(new_val));
                            }
                        }
                    }
                }
            }
        }

        ConfigChange { paths, old_values, new_values }
    }
}

// ── Tests ──

#[cfg(test)]
mod tests {
    use super::*;

    fn make_test_config() -> Config {
        let mut providers = HashMap::new();
        providers.insert("default".to_string(), ProviderConfig {
            r#type: "openai".to_string(),
            api_url: String::new(),
            base_url: String::new(),
            api_key: "sk-test".to_string(),
            model: "gpt-4o".to_string(),
            temperature: 0.7,
            max_tokens: 4096,
            prompt_cache_enabled: true,
            unknown: Default::default(),
        });
        Config {
            agent: AgentConfig {
                provider: "default".to_string(),
                max_iterations: 40,
                timeout_seconds: 120,
                max_tool_result_chars: 10000,
                persist_tool_results: false,
                context_window_tokens: 8192,
                unknown: Default::default(),
            },
            providers,
            tools: vec![],
            channels: HashMap::new(),
            gateway: GatewayConfig::default(),
        }
    }

    #[test]
    fn test_config_validate_empty_provider() {
        let mut cfg = make_test_config();
        cfg.agent.provider = String::new();
        assert!(cfg.validate().is_err());
    }

    #[test]
    fn test_config_validate_missing_provider_ref() {
        let mut cfg = make_test_config();
        cfg.agent.provider = "nonexistent".to_string();
        assert!(cfg.validate().is_err());
    }

    #[test]
    fn test_config_validate_empty_api_key() {
        let mut cfg = make_test_config();
        cfg.providers.get_mut("default").unwrap().api_key = String::new();
        assert!(cfg.validate().is_err());
    }

    #[test]
    fn test_config_validate_pass() {
        let cfg = make_test_config();
        assert!(cfg.validate().is_ok());
    }

    #[test]
    fn test_config_clamp() {
        let mut cfg = make_test_config();
        cfg.agent.max_iterations = 9999;
        cfg.providers.get_mut("default").unwrap().temperature = 5.0;
        cfg.clamp();
        assert_eq!(cfg.agent.max_iterations, 200);
        assert!((cfg.providers["default"].temperature - 2.0).abs() < f32::EPSILON);
    }

    #[test]
    fn test_config_normalize() {
        let mut cfg = make_test_config();
        cfg.agent.provider = String::new();
        cfg.normalize();
        assert_eq!(cfg.agent.provider, "default");
    }

    #[test]
    fn test_config_diff() {
        let old = make_test_config();
        let mut new = make_test_config();
        new.agent.max_iterations = 50;

        let diff = Config::compute_diff(&old, &new);
        assert_eq!(diff.paths.len(), 1);
        assert!(diff.paths.iter().any(|p| p.contains("max_iterations")));
    }

    #[test]
    fn test_config_diff_no_change() {
        let cfg = make_test_config();
        let diff = Config::compute_diff(&cfg, &cfg);
        assert!(diff.paths.is_empty());
    }
}
