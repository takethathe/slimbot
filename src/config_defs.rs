use crate::define_config;
use std::collections::BTreeMap;

define_config! {
    "agent" => AgentConfig {
        provider: String = "".to_string(), str_max(64), desc: "References a provider name in providers map",
        max_iterations: u32 = 40, range(1, 200), desc: "Max agent loop iterations before termination",
        timeout_seconds: u64 = 120, range(1, 600), desc: "Whole-run wall-clock deadline in seconds",
        persist_tool_results: bool = true, none, desc: "Whether to persist oversized tool results to disk",
    }
}

define_config! {
    "pressure" => PressureConfig {
        single_tool_result_ratio: f32 = 0.25, range(0.01, 0.90), desc: "Share of the context window one tool result may occupy (tokens, converted to chars)",
        persisted_preview_ratio: f32 = 0.04, range(0.001, 0.50), desc: "Share of the context window kept as preview for a persisted tool result",
        snip_safety_ratio: f32 = 0.03, range(0.001, 0.50), desc: "Share of the context window reserved for tokenizer estimation drift",
        consolidate_trigger_ratio: f32 = 0.50, range(0.05, 0.95), desc: "Share of the context window that triggers consolidation",
        consolidate_target_ratio: f32 = 0.60, range(0.05, 1.0), desc: "Share of the trigger point consolidation aims to reduce the prompt to",
        summary_ratio: f32 = 0.02, range(0.001, 0.50), desc: "Share of the context window allowed for one consolidation summary",
        min_keep_messages: usize = 4, range(1, 500), desc: "Floor on how many non-system messages snip keeps",
        keep_recent_tool_results: usize = 10, range(1, 500), desc: "How many recent tool results residual cleanup leaves untouched",
        tool_results_ttl_days: u64 = 7, range(1, 365), desc: "Retention for persisted tool results before Dream sweeps them",
        memory_doc_ratio: f32 = 0.075, range(0.001, 0.50), desc: "Share of the primary model window one memory document may occupy (tokens, converted to chars)",
        memory_snapshot_recent_days: u64 = 30, range(1, 3650), desc: "Keep every pre-write memory snapshot younger than this many days",
        memory_snapshot_daily_days: u64 = 365, range(1, 3650), desc: "Beyond the recent window, thin memory snapshots to one per day until this age, then one per month",
        memory_snapshot_max: usize = 120, range(1, 10_000), desc: "Hard bound on retained pre-write snapshots per memory document",
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
        context_window_tokens: u32 = 32768, range(1024, 200_000), desc: "Model context window in tokens (the single anchor all context limits derive from)",
        timeout_seconds: u64 = 180, range(1, 3600), desc: "Single provider call timeout in seconds (wall-clock bound on one HTTP request)",
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
    "gateway.owner" => OwnerConfig {
        id: String = "owner".to_string(), str_max(64), desc: "Stable identifier of the single principal this gateway serves",
        display_name: Option<String> = None::<String>, none, desc: "Human-readable owner name (optional, display only)",
    }
}

define_config! {
    "gateway.cron" => CronConfig {
        enabled: bool = true, none, desc: "Whether cron scheduling is enabled",
    }
}

define_config! {
    "gateway.heartbeat" => HeartbeatConfig {
        enabled: bool = true, none, desc: "Whether periodic heartbeat checks are enabled",
        interval_s: u64 = crate::config::default_heartbeat_interval(), none, desc: "Heartbeat check interval in seconds (default 30 minutes)",
        channel: Option<String> = None::<String>, none, desc: "Optional delivery channel for heartbeat notices (default: the owner's primary channel)",
        to: Option<String> = None::<String>, none, desc: "Optional chat id paired with `channel` for heartbeat notices",
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

// ── `tools.approval` field metadata (hand-written) ──
//
// The approval policy lives in `src/approval/mod.rs` (it carries behavior:
// levels, per-tool overrides, TTL). Only its editor metadata lives here, so the
// WebUI can render one control per knob from `FieldMeta` alone. Field names are
// dotted (`levels.exec`) because the nested levels map is flattened for editing.

/// Editor metadata for the `tools.approval` section (ADR 0011).
pub fn approval_field_metas() -> &'static [crate::config_macro::FieldMeta] {
    use crate::config_macro::FieldMeta;

    const LEVELS: &[&str] = &["allow", "ask", "deny"];
    let level = |name: &'static str, default: &'static str, desc: &'static str| -> FieldMeta {
        FieldMeta {
            name,
            path: "",
            ty: "String",
            default: serde_json::json!(default),
            min: None,
            max: None,
            allowed: LEVELS,
            desc,
            secret: false,
        }
    };

    Box::leak(Box::new([
        level(
            "levels.read_only",
            "allow",
            "Control level for read-only tools (file_reader, list_dir, grep, ...)",
        ),
        level(
            "levels.workspace_write",
            "allow",
            "Control level for workspace writes (file_writer, file_editor, make_dir)",
        ),
        level(
            "levels.exec",
            "ask",
            "Control level for command execution (`shell`)",
        ),
        level(
            "levels.network_egress",
            "allow",
            "Control level for network fetches (`web_fetch`)",
        ),
        level(
            "levels.external_send",
            "ask",
            "Control level for outbound sends (`message`, third-party writes)",
        ),
        FieldMeta {
            name: "tools",
            path: "",
            ty: "json",
            default: serde_json::json!({}),
            min: None,
            max: None,
            allowed: &[],
            desc: "Per-tool overrides, e.g. {\"shell\": \"ask\"}; a tool name wins over its risk level",
            secret: false,
        },
        FieldMeta {
            name: "timeout_seconds",
            path: "",
            ty: "u64",
            default: serde_json::json!(1800),
            min: Some(serde_json::json!(1)),
            max: Some(serde_json::json!(86400)),
            allowed: &[],
            desc: "How long a pending approval stays redeemable (clamped to 24h)",
            secret: false,
        },
    ]))
}
