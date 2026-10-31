use crate::define_config;
use std::collections::BTreeMap;

define_config! {
    "agent" => AgentConfig {
        provider: String = "".to_string(), str_max(64), desc: "References a provider name in providers map",
        max_iterations: u32 = 40, range(1, 200), desc: "Max agent loop iterations before termination",
        timeout_seconds: u64 = 120, range(1, 600), desc: "Per-request timeout in seconds",
        max_tool_result_chars: u32 = 8000, range(100, 100_000), desc: "Max characters for tool result before truncation",
        persist_tool_results: bool = true, none, desc: "Whether to persist oversized tool results to disk",
        context_window_tokens: u32 = 32768, range(1024, 200_000), desc: "LLM context window size in tokens",
    }
}

define_config! {
    "provider" => ProviderConfig {
        r#type: String = "openai".to_string(), allowed(["openai", "anthropic", "custom"]), desc: "Provider type identifier",
        api_url: String = "".to_string(), str_max(512), desc: "Full API endpoint URL",
        base_url: String = "https://api.openai.com".to_string(), str_max(512), desc: "Base URL for deriving api_url",
        api_key: String = "".to_string(), secret, desc: "API authentication key (leave empty to keep unchanged)",
        model: String = "gpt-4o".to_string(), str_max(128), desc: "Model identifier",
        temperature: f32 = 0.7, range(0.0, 2.0), desc: "LLM sampling temperature",
        max_tokens: u32 = 4096, range(1, 100_000), desc: "Max response tokens",
        prompt_cache_enabled: bool = true, none, desc: "Enable prompt caching via cache_control",
        disable_thinking: bool = false, none, desc: "Force-disable model thinking/reasoning mode for calls on this provider (injects enable_thinking=false)",
    }
}

define_config! {
    "tracing" => TraceConfig {
        enabled: bool = false, none, desc: "Master switch for trace recording (default off)",
        reset_on_start: bool = true, none, desc: "Truncate trace file(s) on Gateway startup (CLI never truncates)",
        backends: Vec<TraceBackendEntry> = Vec::<TraceBackendEntry>::new(), none, desc: "Trace backends (file / otel), each with its own enable switch",
    }
}

define_config! {
    "tracing.backends" => TraceBackendEntry {
        r#type: String = "file".to_string(), allowed(["file", "otel"]), desc: "Backend type (file | otel)",
        enabled: bool = true, none, desc: "Per-backend enable switch",
        path: String = "".to_string(), str_max(512), desc: "File backend JSONL output path",
        endpoint: String = "http://localhost:4318".to_string(), str_max(512), desc: "OTLP HTTP exporter endpoint",
        service_name: String = "slimbot".to_string(), str_max(128), desc: "OpenTelemetry service name",
    }
}

// ── Flat config sections migrated from src/config.rs ──
// These were previously hand-written structs without field metadata; moving
// them into `define_config!` gives them desc/constraint metadata so the
// WebUI config editor can render every editable item from FieldMeta alone.
// Constraint is `none` for every field so clamp behavior stays identical to
// the pre-migration hand-written structs (they were never clamped before).

define_config! {
    "gateway.cron" => CronConfig {
        enabled: bool = true, none, desc: "Whether cron scheduling is enabled",
    }
}

define_config! {
    "gateway.heartbeat" => HeartbeatConfig {
        enabled: bool = true, none, desc: "Whether periodic heartbeat checks are enabled",
        interval_s: u64 = crate::config::default_heartbeat_interval(), none, desc: "Heartbeat check interval in seconds (default 30 minutes)",
    }
}

define_config! {
    "gateway.dream" => DreamConfig {
        enabled: bool = true, none, desc: "Whether the Dream memory consolidation service is enabled",
        interval_ms: i64 = crate::config::default_dream_interval_ms(), none, desc: "Dream trigger interval in milliseconds (default 2 hours)",
        model_override: Option<String> = None::<String>, none, desc: "Optional provider name override for Dream (defaults to the main agent provider)",
        max_entries: usize = crate::config::default_dream_max_entries(), none, desc: "Max history entries processed per Dream run",
    }
}

define_config! {
    "web_fetch" => WebFetchConfig {
        max_chars: usize = crate::config::default_max_chars(), none, desc: "Max characters fetched per URL before truncation",
        timeout_s: u64 = crate::config::default_timeout(), none, desc: "Fetch timeout in seconds",
        user_agent: String = crate::config::default_user_agent(), none, desc: "User-Agent header used for fetches",
    }
}

define_config! {
    "json_render" => JsonRenderConfig {
        enabled: bool = true, none, desc: "Master switch for WebUI card generation (default on; the real gate is Catalog registration)",
        model_override: Option<String> = None::<String>, none, desc: "Optional provider name override for card generation (defaults to the main agent provider)",
        max_tokens: u32 = 2048, range(64, 32768), desc: "Max output tokens per card generation call",
        timeout_seconds: u64 = 30, range(1, 300), desc: "No-progress window for one card generation call in seconds (a living stream is never cut off)",
    }
}

define_config! {
    "subagent" => SubagentConfig {
        max_concurrent: usize = 3, range(1, 16), desc: "Max subagent runs executing at the same time (shared by the main loop and triggers)",
    }
}

define_config! {
    "triggers" => TriggersConfig {
        enabled: bool = true, none, desc: "Master switch for the trigger tick (sources are not collected when off)",
        tick_interval_s: u64 = 60, range(1, 3600), desc: "Fixed trigger tick interval in seconds",
        tick_timeout_s: u64 = 30, range(1, 600), desc: "Hard timeout for one tick (collect + judge + enqueue) in seconds",
        max_concurrent_collect: usize = 4, range(1, 16), desc: "Max sources collected concurrently within one tick",
        max_queue: usize = 20, range(1, 200), desc: "Max pending fires in the dispatch queue (overflow is dropped)",
        max_merge: usize = 3, range(1, 20), desc: "Max notify messages merged into one inbound input per origin per tick",
        history_lines: usize = 5000, range(100, 100_000), desc: "Fire history lines kept before the file is rolled to one generation",
        secrets: Vec<SecretEntry> = Vec::<SecretEntry>::new(), none, desc: "Secrets available to scripts via ctx.secret(name); values are masked",
    }
}

define_config! {
    "triggers.secrets" => SecretEntry {
        name: String = "".to_string(), str_max(128), desc: "Secret name used by ctx.secret(name)",
        value: String = "".to_string(), secret, desc: "Secret value (masked in the config editor)",
    }
}

define_config! {
    "memory" => MemoryConfig {
        backend: String = "markdown".to_string(), allowed(["markdown"]), desc: "Memory backend type (markdown | future: sqlite)",
    }
}
