use std::collections::BTreeMap;
use crate::define_config;

define_config! {
    agent => AgentConfig {
        provider: String = "".to_string(), str_max(64), desc: "References a provider name in providers map",
        max_iterations: u32 = 40, range(1, 200), desc: "Max agent loop iterations before termination",
        timeout_seconds: u64 = 120, range(1, 600), desc: "Per-request timeout in seconds",
        max_tool_result_chars: u32 = 8000, range(100, 100_000), desc: "Max characters for tool result before truncation",
        persist_tool_results: bool = true, none, desc: "Whether to persist oversized tool results to disk",
        context_window_tokens: u32 = 8192, range(1024, 200_000), desc: "LLM context window size in tokens",
    }
}

define_config! {
    provider => ProviderConfig {
        r#type: String = "openai".to_string(), allowed(["openai", "anthropic", "custom"]), desc: "Provider type identifier",
        api_url: String = "".to_string(), str_max(512), desc: "Full API endpoint URL",
        base_url: String = "https://api.openai.com".to_string(), str_max(512), desc: "Base URL for deriving api_url",
        api_key: String = "".to_string(), str_max(512), desc: "API authentication key",
        model: String = "gpt-4o".to_string(), str_max(128), desc: "Model identifier",
        temperature: f32 = 0.7, range(0.0, 2.0), desc: "LLM sampling temperature",
        max_tokens: u32 = 4096, range(1, 100_000), desc: "Max response tokens",
        prompt_cache_enabled: bool = true, none, desc: "Enable prompt caching via cache_control",
    }
}
