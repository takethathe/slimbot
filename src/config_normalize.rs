use crate::config_defs::{AgentConfig, ProviderConfig};
use crate::config_macro::Normalizable;

/// Normalizes all config sections.
pub fn apply_normalize(config: &mut crate::config::Config) {
    config.agent.normalize();
    for provider in config.providers.values_mut() {
        provider.normalize();
    }
}

impl Normalizable for AgentConfig {
    fn normalize(&mut self) {
        if self.provider.is_empty() {
            self.provider = "default".to_string();
        }
    }
}

impl Normalizable for ProviderConfig {
    fn normalize(&mut self) {
        // Derive api_url from base_url if api_url is empty
        if self.api_url.is_empty() && !self.base_url.is_empty() {
            let base = self.base_url.trim_end_matches('/');
            self.api_url = if base.ends_with("/chat/completions") {
                base.to_string()
            } else if base.ends_with("/v1") {
                format!("{}/chat/completions", base)
            } else {
                format!("{}/v1/chat/completions", base)
            };
        }
        // Fall back to default OpenAI URL if both are empty
        if self.api_url.is_empty() {
            self.api_url = "https://api.openai.com/v1/chat/completions".to_string();
        }
    }
}
