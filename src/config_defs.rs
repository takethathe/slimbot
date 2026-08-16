use crate::define_config;
use std::collections::BTreeMap;

define_config! {
    agent => AgentConfig {
        provider: String = "".to_string(), str_max(64), desc: "References a provider name in providers map",
        max_iterations: u32 = 40, range(1, 200), desc: "Max agent loop iterations before termination",
        timeout_seconds: u64 = 120, range(1, 600), desc: "Per-request timeout in seconds",
        max_tool_result_chars: u32 = 8000, range(100, 100_000), desc: "Max characters for tool result before truncation",
        persist_tool_results: bool = true, none, desc: "Whether to persist oversized tool results to disk",
        context_window_tokens: u32 = 32768, range(1024, 200_000), desc: "LLM context window size in tokens",
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

define_config! {
    tracing => TraceConfig {
        enabled: bool = false, none, desc: "Master switch for trace recording (default off)",
        reset_on_start: bool = true, none, desc: "Truncate trace file(s) on Gateway startup (CLI never truncates)",
        backends: Vec<TraceBackendEntry> = Vec::<TraceBackendEntry>::new(), none, desc: "Trace backends (file / otel), each with its own enable switch",
    }
}

define_config! {
    tracing_backend => TraceBackendEntry {
        r#type: String = "file".to_string(), allowed(["file", "otel"]), desc: "Backend type (file | otel)",
        enabled: bool = true, none, desc: "Per-backend enable switch",
        path: String = "".to_string(), str_max(512), desc: "File backend JSONL output path",
        endpoint: String = "http://localhost:4318".to_string(), str_max(512), desc: "OTLP HTTP exporter endpoint",
        service_name: String = "slimbot".to_string(), str_max(128), desc: "OpenTelemetry service name",
    }
}
