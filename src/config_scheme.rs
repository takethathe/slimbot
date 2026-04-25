use std::path::Path;

use anyhow::Result;

use crate::config::{AgentConfig, Config, ProviderConfig};

/// ConfigScheme holds all default values and validation rules
/// for the application configuration.
pub struct ConfigScheme;

impl ConfigScheme {
    pub fn new() -> Self {
        Self
    }

    /// Generate a complete default Config with all fields populated.
    pub fn default_config(&self) -> Config {
        Config {
            data_dir: Self::default_data_dir(),
            provider: self.default_provider_config(),
            agent: self.default_agent_config(),
            tools: vec![],
            channels: vec![],
        }
    }

    /// Normalize an existing Config: fill missing defaults,
    /// and correct invalid values.
    pub fn normalize(&self, config: &mut Config) {
        // data_dir
        if config.data_dir.is_empty() {
            config.data_dir = Self::default_data_dir();
        }

        // provider
        if config.provider.api_url.is_empty() {
            config.provider.api_url = Self::DEFAULT_API_URL.to_string();
        }
        if config.provider.model.is_empty() {
            config.provider.model = Self::DEFAULT_MODEL.to_string();
        }
        if config.provider.temperature <= 0.0 || config.provider.temperature > 2.0 {
            config.provider.temperature = Self::DEFAULT_TEMPERATURE;
        }
        if config.provider.max_tokens == 0 {
            config.provider.max_tokens = Self::DEFAULT_MAX_TOKENS;
        }
        // api_key is NOT normalized — it must be set by the user

        // agent
        if config.agent.max_iterations == 0 {
            config.agent.max_iterations = Self::DEFAULT_MAX_ITERATIONS;
        }
        if config.agent.timeout_seconds == 0 {
            config.agent.timeout_seconds = Self::DEFAULT_TIMEOUT;
        }

        // tools & channels: remove entries with empty name/type
        config.tools.retain(|t| !t.name.is_empty());
        config.channels.retain(|c| !c.r#type.is_empty());
    }

    /// Write the default config.json to the given path.
    /// Returns Ok(()) even if the file already exists (caller should check first).
    pub fn write_default_config(&self, path: &str) -> Result<()> {
        if let Some(parent) = Path::new(path).parent() {
            std::fs::create_dir_all(parent)?;
        }
        let config = self.default_config();
        let content = serde_json::to_string_pretty(&config)?;
        std::fs::write(path, content)?;
        Ok(())
    }

    /// Check whether a config file exists at the given path.
    pub fn config_exists(&self, path: &str) -> bool {
        Path::new(path).exists()
    }

    // ── Default value constants ──

    const DEFAULT_API_URL: &str = "https://api.openai.com/v1/chat/completions";
    const DEFAULT_MODEL: &str = "gpt-4o";
    const DEFAULT_TEMPERATURE: f32 = 0.7;
    const DEFAULT_MAX_TOKENS: u32 = 4096;
    const DEFAULT_MAX_ITERATIONS: u32 = 40;
    const DEFAULT_TIMEOUT: u64 = 120;

    fn default_data_dir() -> String {
        let home = dirs::home_dir()
            .map(|p| p.to_string_lossy().to_string())
            .unwrap_or_else(|| ".".to_string());
        format!("{}/.slimbot", home)
    }

    fn default_provider_config(&self) -> ProviderConfig {
        ProviderConfig {
            api_url: Self::DEFAULT_API_URL.to_string(),
            api_key: String::new(), // must be set by user
            model: Self::DEFAULT_MODEL.to_string(),
            temperature: Self::DEFAULT_TEMPERATURE,
            max_tokens: Self::DEFAULT_MAX_TOKENS,
        }
    }

    fn default_agent_config(&self) -> AgentConfig {
        AgentConfig {
            max_iterations: Self::DEFAULT_MAX_ITERATIONS,
            timeout_seconds: Self::DEFAULT_TIMEOUT,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::ToolEntry;
    use tempfile::NamedTempFile;

    fn scheme() -> ConfigScheme {
        ConfigScheme::new()
    }

    #[test]
    fn test_default_config_has_all_fields() {
        let config = scheme().default_config();
        assert!(!config.data_dir.is_empty());
        assert!(!config.provider.api_url.is_empty());
        assert!(config.provider.api_key.is_empty()); // intentional
        assert!(!config.provider.model.is_empty());
        assert!((config.provider.temperature - 0.7).abs() < f32::EPSILON);
        assert_eq!(config.provider.max_tokens, 4096);
        assert_eq!(config.agent.max_iterations, 40);
        assert_eq!(config.agent.timeout_seconds, 120);
        assert!(config.tools.is_empty());
        assert!(config.channels.is_empty());
    }

    #[test]
    fn test_normalize_fills_missing_provider() {
        let mut config = scheme().default_config();
        config.provider.api_url.clear();
        config.provider.model.clear();
        config.provider.temperature = 0.0;
        config.provider.max_tokens = 0;

        scheme().normalize(&mut config);

        assert_eq!(config.provider.api_url, ConfigScheme::DEFAULT_API_URL);
        assert_eq!(config.provider.model, ConfigScheme::DEFAULT_MODEL);
        assert_eq!(config.provider.temperature, 0.7);
        assert_eq!(config.provider.max_tokens, 4096);
    }

    #[test]
    fn test_normalize_preserves_valid_api_key() {
        let mut config = scheme().default_config();
        config.provider.api_key = "sk-test-key".to_string();

        scheme().normalize(&mut config);

        assert_eq!(config.provider.api_key, "sk-test-key");
    }

    #[test]
    fn test_normalize_fixes_invalid_temperature() {
        let mut config = scheme().default_config();
        config.provider.temperature = 5.0; // out of range

        scheme().normalize(&mut config);

        assert_eq!(config.provider.temperature, 0.7);
    }

    #[test]
    fn test_normalize_removes_empty_tools() {
        let mut config = scheme().default_config();
        config.tools.push(ToolEntry {
            name: String::new(),
            enabled: true,
        });
        config.tools.push(ToolEntry {
            name: "valid-tool".to_string(),
            enabled: true,
        });

        scheme().normalize(&mut config);

        assert_eq!(config.tools.len(), 1);
        assert_eq!(config.tools[0].name, "valid-tool");
    }

    #[test]
    fn test_write_and_reload() {
        let tmp = NamedTempFile::new().unwrap();
        let path = tmp.path().to_str().unwrap().to_string();

        scheme().write_default_config(&path).unwrap();
        assert!(scheme().config_exists(&path));

        let content = std::fs::read_to_string(&path).unwrap();
        let parsed: Config = serde_json::from_str(&content).unwrap();
        assert!(!parsed.data_dir.is_empty());
        assert_eq!(parsed.provider.model, "gpt-4o");
    }

    #[test]
    fn test_data_dir_fallback_without_home() {
        // default_config should always produce a non-empty data_dir
        let config = scheme().default_config();
        assert!(config.data_dir.ends_with(".slimbot"));
    }
}
