use std::collections::HashMap;
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
        let default_provider_name = "default".to_string();
        let mut providers = HashMap::new();
        providers.insert(
            default_provider_name.clone(),
            self.default_provider_config(),
        );

        Config {
            data_dir: Self::default_data_dir(),
            workspace_dir: String::new(), // resolved by Config::load
            agent: self.default_agent_config(&default_provider_name),
            providers,
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
        // workspace_dir
        if config.workspace_dir.is_empty() {
            config.workspace_dir = format!("{}/workspace", config.data_dir);
        }

        // agent
        if config.agent.provider.is_empty() {
            config.agent.provider = "default".to_string();
        }
        if config.agent.max_iterations == 0 {
            config.agent.max_iterations = Self::DEFAULT_MAX_ITERATIONS;
        }
        if config.agent.timeout_seconds == 0 {
            config.agent.timeout_seconds = Self::DEFAULT_TIMEOUT;
        }

        // providers
        for provider in config.providers.values_mut() {
            if provider.r#type.is_empty() {
                provider.r#type = Self::DEFAULT_PROVIDER_TYPE.to_string();
            }
            self.normalize_provider_url(provider);
            if provider.model.is_empty() {
                provider.model = Self::DEFAULT_MODEL.to_string();
            }
            if provider.temperature <= 0.0 || provider.temperature > 2.0 {
                provider.temperature = Self::DEFAULT_TEMPERATURE;
            }
            if provider.max_tokens == 0 {
                provider.max_tokens = Self::DEFAULT_MAX_TOKENS;
            }
            // api_key is NOT normalized — it must be set by the user
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

    /// Derive the full API URL from api_url or base_url.
    /// Priority: api_url > base_url + suffix > default.
    fn normalize_provider_url(&self, provider: &mut crate::config::ProviderConfig) {
        if !provider.api_url.is_empty() {
            return; // api_url takes priority
        }
        if !provider.base_url.is_empty() {
            let base = provider.base_url.trim_end_matches('/');
            provider.api_url = format!("{}/v1/chat/completions", base);
        } else {
            provider.api_url = Self::DEFAULT_API_URL.to_string();
        }
    }

    // ── Default value constants ──

    pub const DEFAULT_API_URL: &str = "https://api.openai.com/v1/chat/completions";
    pub const DEFAULT_PROVIDER_TYPE: &str = "openai";
    pub const DEFAULT_MODEL: &str = "gpt-4o";
    pub const DEFAULT_TEMPERATURE: f32 = 0.7;
    pub const DEFAULT_MAX_TOKENS: u32 = 4096;
    pub const DEFAULT_MAX_ITERATIONS: u32 = 40;
    pub const DEFAULT_TIMEOUT: u64 = 120;

    fn default_data_dir() -> String {
        let home = dirs::home_dir()
            .map(|p| p.to_string_lossy().to_string())
            .unwrap_or_else(|| ".".to_string());
        format!("{}/.slimbot", home)
    }

    fn default_provider_config(&self) -> ProviderConfig {
        ProviderConfig {
            r#type: Self::DEFAULT_PROVIDER_TYPE.to_string(),
            api_url: Self::DEFAULT_API_URL.to_string(),
            base_url: String::new(),
            api_key: String::new(), // must be set by user
            model: Self::DEFAULT_MODEL.to_string(),
            temperature: Self::DEFAULT_TEMPERATURE,
            max_tokens: Self::DEFAULT_MAX_TOKENS,
        }
    }

    fn default_agent_config(&self, provider_ref: &str) -> AgentConfig {
        AgentConfig {
            provider: provider_ref.to_string(),
            max_iterations: Self::DEFAULT_MAX_ITERATIONS,
            timeout_seconds: Self::DEFAULT_TIMEOUT,
            max_tool_result_chars: 8000,
            persist_tool_results: true,
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

    fn default_provider(config: &Config) -> &ProviderConfig {
        config
            .providers
            .get("default")
            .expect("default provider should exist")
    }

    fn default_provider_mut(config: &mut Config) -> &mut ProviderConfig {
        config
            .providers
            .get_mut("default")
            .expect("default provider should exist")
    }

    #[test]
    fn test_default_config_has_all_fields() {
        let mut config = scheme().default_config();
        scheme().normalize(&mut config);
        assert!(!config.data_dir.is_empty());
        assert!(!config.workspace_dir.is_empty());
        assert_eq!(config.agent.provider, "default");
        let provider = default_provider(&config);
        assert_eq!(provider.r#type, "openai");
        assert!(!provider.api_url.is_empty());
        assert!(provider.api_key.is_empty()); // intentional
        assert!(!provider.model.is_empty());
        assert!((provider.temperature - 0.7).abs() < f32::EPSILON);
        assert_eq!(provider.max_tokens, 4096);
        assert_eq!(config.agent.max_iterations, 40);
        assert_eq!(config.agent.timeout_seconds, 120);
        assert!(config.tools.is_empty());
        assert!(config.channels.is_empty());
    }

    #[test]
    fn test_normalize_fills_missing_provider() {
        let mut config = scheme().default_config();
        default_provider_mut(&mut config).api_url.clear();
        default_provider_mut(&mut config).model.clear();
        default_provider_mut(&mut config).temperature = 0.0;
        default_provider_mut(&mut config).max_tokens = 0;

        scheme().normalize(&mut config);

        let provider = default_provider(&config);
        assert_eq!(provider.api_url, ConfigScheme::DEFAULT_API_URL);
        assert_eq!(provider.model, ConfigScheme::DEFAULT_MODEL);
        assert_eq!(provider.temperature, 0.7);
        assert_eq!(provider.max_tokens, 4096);
    }

    #[test]
    fn test_normalize_derives_url_from_base_url() {
        let mut config = scheme().default_config();
        default_provider_mut(&mut config).api_url.clear();
        default_provider_mut(&mut config).base_url = "https://api.example.com".to_string();

        scheme().normalize(&mut config);

        assert_eq!(
            default_provider(&config).api_url,
            "https://api.example.com/v1/chat/completions"
        );
    }

    #[test]
    fn test_normalize_api_url_takes_priority_over_base_url() {
        let mut config = scheme().default_config();
        default_provider_mut(&mut config).api_url =
            "https://existing.com/v1/chat/completions".to_string();
        default_provider_mut(&mut config).base_url = "https://ignored.com".to_string();

        scheme().normalize(&mut config);

        assert_eq!(
            default_provider(&config).api_url,
            "https://existing.com/v1/chat/completions"
        );
    }

    #[test]
    fn test_normalize_custom_provider_base_url() {
        let mut config = scheme().default_config();
        default_provider_mut(&mut config).r#type = "custom".to_string();
        default_provider_mut(&mut config).api_url.clear();
        default_provider_mut(&mut config).base_url =
            "https://my-provider.example.com/api".to_string();

        scheme().normalize(&mut config);

        assert_eq!(default_provider(&config).r#type, "custom");
        assert_eq!(
            default_provider(&config).api_url,
            "https://my-provider.example.com/api/v1/chat/completions"
        );
    }

    #[test]
    fn test_normalize_preserves_valid_api_key() {
        let mut config = scheme().default_config();
        default_provider_mut(&mut config).api_key = "sk-test-key".to_string();

        scheme().normalize(&mut config);

        assert_eq!(default_provider(&config).api_key, "sk-test-key");
    }

    #[test]
    fn test_normalize_fixes_invalid_temperature() {
        let mut config = scheme().default_config();
        default_provider_mut(&mut config).temperature = 5.0; // out of range

        scheme().normalize(&mut config);

        assert_eq!(default_provider(&config).temperature, 0.7);
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
        let mut parsed: Config = serde_json::from_str(&content).unwrap();
        scheme().normalize(&mut parsed);
        assert!(!parsed.data_dir.is_empty());
        assert!(!parsed.workspace_dir.is_empty());
        assert_eq!(default_provider(&parsed).model, "gpt-4o");
    }

    #[test]
    fn test_data_dir_fallback_without_home() {
        // default_config should always produce a non-empty data_dir
        let config = scheme().default_config();
        assert!(config.data_dir.ends_with(".slimbot"));
    }

    #[test]
    fn test_workspace_dir_derived_from_data_dir() {
        let mut config = scheme().default_config();
        // Config::load resolves workspace_dir, but default_config leaves it empty
        // ConfigScheme::normalize should also fill it
        scheme().normalize(&mut config);
        assert_eq!(
            config.workspace_dir,
            format!("{}/workspace", config.data_dir)
        );
    }

    #[test]
    fn test_workspace_dir_override() {
        let mut config = scheme().default_config();
        config.workspace_dir = "/custom/workspace".to_string();
        scheme().normalize(&mut config);
        assert_eq!(config.workspace_dir, "/custom/workspace");
    }

    #[test]
    fn test_multiple_providers() {
        let mut config = scheme().default_config();
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
            },
        );
        config.agent.provider = "siliconflow".to_string();

        scheme().normalize(&mut config);

        let sf = config.providers.get("siliconflow").unwrap();
        assert_eq!(sf.api_url, "https://api.siliconflow.cn/v1/chat/completions");
        assert_eq!(sf.api_key, "sk-sf-key");
    }
}
