//! Config schema serialization + path-patch engine for the WebUI config editor.
//!
//! Two pure, unit-testable pieces that never touch the HTTP layer:
//!
//! - [`serialize_schema`] turns the `define_config!` field metadata plus a live
//!   [`Config`] into a schema tree the frontend renders as a form. Secret
//!   fields are masked (value is `null`) so keys are never sent back as
//!   plaintext.
//! - [`apply_config_patch`] applies `[{op, path, value}]` patches to a
//!   [`Config`] with the same clamp → normalize → validate pipeline as
//!   `Config::load`. Validation failures are returned as per-field errors and
//!   the caller must not persist the mutated config in that case.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use crate::config::{
    AgentConfig, ChannelConfig, Config, CronConfig, DreamConfig, HeartbeatConfig, JsonRenderConfig,
    MemoryConfig, OwnerConfig, PressureConfig, ProviderConfig, SecretEntry, SubagentConfig,
    ToolEntry, TraceBackendEntry, TraceConfig, TriggersConfig, WebFetchConfig,
};
use crate::config_macro::FieldMeta;

// ── Patch types ──

/// Patch operation kind. `Set` targets scalars (or whole collection elements),
/// `Add`/`Remove` target collection elements (providers map, tools Vec,
/// channels map, tracing.backends Vec).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PatchOpKind {
    Set,
    Add,
    Remove,
}

/// One config patch: `{op, path, value}` with dot-paths like
/// `agent.max_iterations`, `providers.<name>.temperature`, `gateway.cron.enabled`.
/// `from` is used by `add` ops to inherit unmasked fields (e.g. `api_key`)
/// from an existing map entry — required for lossless provider renames.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PatchOp {
    pub op: PatchOpKind,
    pub path: String,
    #[serde(default)]
    pub value: serde_json::Value,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub from: Option<String>,
}

/// Per-field validation error, surfaced to the UI for inline highlighting.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FieldError {
    pub path: String,
    pub message: String,
}

// ── Schema tree ──

/// A single editable config field, rendered from `FieldMeta` + current value.
#[derive(Debug, Clone, Serialize)]
pub struct SchemaField {
    pub name: String,
    /// Full dot-path used as the patch path for scalar sections and collection
    /// items; relative (field-name only) for collection `item_fields` templates.
    pub path: String,
    /// `bool` | `int` | `float` | `string` | `select` | `secret` |
    /// `optional_string` | `list` | `json`
    pub kind: String,
    pub desc: String,
    pub default: serde_json::Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub min: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub allowed: Vec<String>,
    #[serde(skip_serializing_if = "is_false")]
    pub secret: bool,
    /// Current value; always `null` for secret fields (never plaintext).
    pub value: serde_json::Value,
}

/// One element of a collection section (providers/channels entries, a tool
/// entry, or a trace backend).
#[derive(Debug, Clone, Serialize)]
pub struct SchemaItem {
    /// Map key for `providers`/`channels`; absent for Vec-based collections.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub key: Option<String>,
    pub fields: Vec<SchemaField>,
    /// Channels: raw JSON of the channel-specific `extra` config.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub extra: Option<serde_json::Value>,
}

/// A rendered section of the config editor form.
#[derive(Debug, Clone, Serialize)]
pub struct SchemaSection {
    /// Dot-path prefix (e.g. `agent`, `gateway.cron`, `providers`, `tracing.backends`).
    pub name: String,
    pub label: String,
    /// Scalar fields (empty for collection sections).
    pub fields: Vec<SchemaField>,
    /// Collection element form template (values = defaults), for the "add" UI.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub item_fields: Option<Vec<SchemaField>>,
    /// Current collection elements.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub items: Option<Vec<SchemaItem>>,
}

#[derive(Debug, Clone, Serialize)]
pub struct SchemaTree {
    pub sections: Vec<SchemaSection>,
}

// ── Schema serialization ──

/// Serialize a config into the schema tree the WebUI config editor renders.
/// Pure: no I/O, no HTTP.
pub fn serialize_schema(config: &Config) -> SchemaTree {
    let sections = vec![
        scalar_section(
            "agent",
            "Agent",
            AgentConfig::fields(),
            &to_value_map(&config.agent),
        ),
        scalar_section(
            "gateway.owner",
            "Gateway · Owner",
            OwnerConfig::fields(),
            &to_value_map(&config.gateway.owner),
        ),
        scalar_section(
            "gateway.cron",
            "Gateway · Cron",
            CronConfig::fields(),
            &to_value_map(&config.gateway.cron),
        ),
        scalar_section(
            "gateway.heartbeat",
            "Gateway · Heartbeat",
            HeartbeatConfig::fields(),
            &to_value_map(&config.gateway.heartbeat),
        ),
        scalar_section(
            "gateway.dream",
            "Gateway · Dream",
            DreamConfig::fields(),
            &to_value_map(&config.gateway.dream),
        ),
        scalar_section(
            "web_fetch",
            "Web Fetch",
            WebFetchConfig::fields(),
            &to_value_map(&config.web_fetch.clone().unwrap_or_default()),
        ),
        scalar_section(
            "json_render",
            "JSON Render · Cards",
            JsonRenderConfig::fields(),
            &to_value_map(&config.json_render),
        ),
        scalar_section(
            "tracing",
            "Tracing",
            TraceConfig::fields(),
            &to_value_map(&config.tracing),
        ),
        scalar_section(
            "memory",
            "Memory",
            MemoryConfig::fields(),
            &to_value_map(&config.memory),
        ),
        scalar_section(
            "subagent",
            "Subagent",
            SubagentConfig::fields(),
            &to_value_map(&config.subagent),
        ),
        scalar_section(
            "triggers",
            "Triggers",
            TriggersConfig::fields(),
            &to_value_map(&config.triggers),
        ),
        scalar_section(
            "pressure",
            "Pressure · Context",
            PressureConfig::fields(),
            &to_value_map(&config.pressure),
        ),
        providers_section(config),
        tools_section(config),
        channels_section(config),
        tracing_backends_section(config),
        trigger_secrets_section(config),
    ];
    SchemaTree { sections }
}

/// Derive the form control kind from a field's type + constraints.
fn kind_for(meta: &FieldMeta) -> &'static str {
    if meta.secret {
        return "secret";
    }
    match meta.ty {
        "bool" => "bool",
        "u32" | "u64" | "i64" | "usize" => "int",
        "f32" => "float",
        "Option<String>" => "optional_string",
        t if t.starts_with("Vec<") => "list",
        _ if !meta.allowed.is_empty() => "select",
        _ => "string",
    }
}

/// Normalize a raw-identifier field name (`r#type` → `type`) so schema names
/// match both the serialized JSON keys and the patch paths.
fn meta_name(meta: &FieldMeta) -> &str {
    meta.name.trim_start_matches("r#")
}

/// Build a [`SchemaField`] from metadata + a value, masking secrets.
fn field_from_meta(meta: &FieldMeta, value: serde_json::Value, path: &str) -> SchemaField {
    SchemaField {
        name: meta_name(meta).to_string(),
        path: path.to_string(),
        kind: kind_for(meta).to_string(),
        desc: meta.desc.to_string(),
        default: meta.default.clone(),
        min: meta.min.clone(),
        max: meta.max.clone(),
        allowed: meta.allowed.iter().map(|s| s.to_string()).collect(),
        secret: meta.secret,
        value: if meta.secret {
            serde_json::Value::Null
        } else {
            value
        },
    }
}

/// Map a macro struct's field metas onto schema fields, looking each value up
/// in `value_map` (falling back to the field default). `prefix` is prepended to
/// every field path.
fn fields_from_metas(
    metas: &[FieldMeta],
    value_map: &serde_json::Map<String, serde_json::Value>,
    prefix: &str,
) -> Vec<SchemaField> {
    metas
        .iter()
        .map(|m| {
            let value = value_map
                .get(meta_name(m))
                .cloned()
                .unwrap_or_else(|| m.default.clone());
            field_from_meta(m, value, &format!("{}.{}", prefix, meta_name(m)))
        })
        .collect()
}

/// Serialize a struct to its JSON object map (for value lookup by field name).
fn to_value_map<T: serde::Serialize>(value: &T) -> serde_json::Map<String, serde_json::Value> {
    serde_json::to_value(value)
        .ok()
        .and_then(|v| v.as_object().cloned())
        .unwrap_or_default()
}

/// A scalar section: one field per macro `FieldMeta`, values from `value_map`.
fn scalar_section(
    name: &str,
    label: &str,
    metas: &[FieldMeta],
    value_map: &serde_json::Map<String, serde_json::Value>,
) -> SchemaSection {
    let fields = fields_from_metas(metas, value_map, name);
    SchemaSection {
        name: name.to_string(),
        label: label.to_string(),
        fields,
        item_fields: None,
        items: None,
    }
}

fn providers_section(config: &Config) -> SchemaSection {
    let metas = ProviderConfig::fields();
    let mut names: Vec<&String> = config.providers.keys().collect();
    names.sort();

    let items = names
        .iter()
        .map(|name| {
            let fields = fields_from_metas(
                metas,
                &to_value_map(&config.providers[*name]),
                &format!("providers.{}", name),
            );
            SchemaItem {
                key: Some((*name).clone()),
                fields,
                extra: None,
            }
        })
        .collect();

    // Element form template for the "add provider" flow (paths are relative).
    let item_fields = metas
        .iter()
        .map(|m| field_from_meta(m, m.default.clone(), meta_name(m)))
        .collect();

    SchemaSection {
        name: "providers".to_string(),
        label: "Providers".to_string(),
        fields: Vec::new(),
        item_fields: Some(item_fields),
        items: Some(items),
    }
}

/// Built-in tool identifiers (mirrors `tools::create_tool`).
const BUILTIN_TOOLS: &[&str] = &[
    "shell",
    "file_reader",
    "file_writer",
    "file_editor",
    "list_dir",
    "make_dir",
    "grep",
    "find_files",
    "web_fetch",
];

fn tools_section(config: &Config) -> SchemaSection {
    let name_meta = SchemaField {
        name: "name".to_string(),
        path: "name".to_string(),
        kind: "select".to_string(),
        desc: "Tool identifier".to_string(),
        default: serde_json::json!("shell"),
        min: None,
        max: None,
        allowed: BUILTIN_TOOLS.iter().map(|s| s.to_string()).collect(),
        secret: false,
        value: serde_json::json!("shell"),
    };
    let enabled_meta = SchemaField {
        name: "enabled".to_string(),
        path: "enabled".to_string(),
        kind: "bool".to_string(),
        desc: "Whether this tool is available to the agent".to_string(),
        default: serde_json::json!(true),
        min: None,
        max: None,
        allowed: Vec::new(),
        secret: false,
        value: serde_json::json!(true),
    };

    let items = config
        .tools
        .iter()
        .enumerate()
        .map(|(i, t)| SchemaItem {
            key: None,
            fields: vec![
                SchemaField {
                    value: serde_json::json!(t.name),
                    path: format!("tools.{}.name", i),
                    ..name_meta.clone()
                },
                SchemaField {
                    value: serde_json::json!(t.enabled),
                    path: format!("tools.{}.enabled", i),
                    ..enabled_meta.clone()
                },
            ],
            extra: None,
        })
        .collect();

    SchemaSection {
        name: "tools".to_string(),
        label: "Tools".to_string(),
        fields: Vec::new(),
        item_fields: Some(vec![name_meta, enabled_meta]),
        items: Some(items),
    }
}

fn channels_section(config: &Config) -> SchemaSection {
    let enabled_meta = SchemaField {
        name: "enabled".to_string(),
        path: "enabled".to_string(),
        kind: "bool".to_string(),
        desc: "Whether this channel is enabled".to_string(),
        default: serde_json::json!(true),
        min: None,
        max: None,
        allowed: Vec::new(),
        secret: false,
        value: serde_json::json!(true),
    };
    let extra_meta = SchemaField {
        name: "extra".to_string(),
        path: "extra".to_string(),
        kind: "json".to_string(),
        desc: "Channel-specific raw JSON config (e.g. host, port)".to_string(),
        default: serde_json::json!({}),
        min: None,
        max: None,
        allowed: Vec::new(),
        secret: false,
        value: serde_json::json!({}),
    };

    let mut names: Vec<&String> = config.channels.keys().collect();
    names.sort();
    let items = names
        .iter()
        .map(|name| {
            let ch = &config.channels[*name];
            SchemaItem {
                key: Some((*name).clone()),
                fields: vec![SchemaField {
                    value: serde_json::json!(ch.enabled),
                    path: format!("channels.{}.enabled", name),
                    ..enabled_meta.clone()
                }],
                extra: Some(serde_json::json!(ch.extra)),
            }
        })
        .collect();

    SchemaSection {
        name: "channels".to_string(),
        label: "Channels".to_string(),
        fields: Vec::new(),
        item_fields: Some(vec![enabled_meta, extra_meta]),
        items: Some(items),
    }
}

fn tracing_backends_section(config: &Config) -> SchemaSection {
    let metas = TraceBackendEntry::fields();
    let item_fields = metas
        .iter()
        .map(|m| field_from_meta(m, m.default.clone(), meta_name(m)))
        .collect();

    let items = config
        .tracing
        .backends
        .iter()
        .enumerate()
        .map(|(i, b)| {
            let fields =
                fields_from_metas(metas, &to_value_map(b), &format!("tracing.backends.{}", i));
            SchemaItem {
                key: None,
                fields,
                extra: None,
            }
        })
        .collect();

    SchemaSection {
        name: "tracing.backends".to_string(),
        label: "Tracing Backends".to_string(),
        fields: Vec::new(),
        item_fields: Some(item_fields),
        items: Some(items),
    }
}

fn is_false(b: &bool) -> bool {
    !*b
}

/// `triggers.secrets`: a list of `{name, value}` entries, edited like
/// `tracing.backends`. The value stays masked by the `secret` flag on the
/// field, so the editor never receives a plaintext key.
fn trigger_secrets_section(config: &Config) -> SchemaSection {
    let metas = SecretEntry::fields();
    let item_fields = metas
        .iter()
        .map(|m| field_from_meta(m, m.default.clone(), meta_name(m)))
        .collect();

    let items = config
        .triggers
        .secrets
        .iter()
        .enumerate()
        .map(|(i, entry)| {
            let fields = fields_from_metas(
                metas,
                &to_value_map(entry),
                &format!("triggers.secrets.{}", i),
            );
            SchemaItem {
                key: None,
                fields,
                extra: None,
            }
        })
        .collect();

    SchemaSection {
        name: "triggers.secrets".to_string(),
        label: "Trigger Secrets".to_string(),
        fields: Vec::new(),
        item_fields: Some(item_fields),
        items: Some(items),
    }
}

// ── Patch engine ──

/// Apply patches to a config with the same clamp → normalize → validate
/// pipeline as `Config::load`. Returns `Err` for malformed ops / unknown
/// paths; returns `Ok(errors)` with per-field validation errors when the
/// resulting config fails `Config::validate` (the caller must not persist in
/// that case). A successful apply returns an empty error list.
pub fn apply_config_patch(config: &mut Config, ops: &[PatchOp]) -> Result<Vec<FieldError>, String> {
    for op in ops {
        apply_op(config, op)?;
    }
    config.clamp();
    config.normalize();
    match config.validate() {
        Ok(()) => Ok(Vec::new()),
        Err(e) => Ok(validate_to_field_errors(&e.to_string())),
    }
}

fn apply_op(config: &mut Config, op: &PatchOp) -> Result<(), String> {
    let segs: Vec<&str> = op.path.split('.').filter(|s| !s.is_empty()).collect();
    match op.op {
        PatchOpKind::Set => set_path(config, &segs, &op.value),
        PatchOpKind::Add => add_path(config, op, &segs),
        PatchOpKind::Remove => remove_path(config, &segs),
    }
}

fn parse_index(s: &str) -> Result<usize, String> {
    s.parse::<usize>()
        .map_err(|_| format!("Invalid collection index: {}", s))
}

// ── Set ──

fn set_path(config: &mut Config, segs: &[&str], value: &serde_json::Value) -> Result<(), String> {
    match segs {
        ["agent", field] => set_agent(config, field, value),
        ["providers", name, field] => set_provider_field(config, name, field, value),
        ["tools", idx, field] => set_tool_field(config, idx, field, value),
        ["channels", name, "enabled"] => set_channel_enabled(config, name, value),
        ["channels", name, "extra"] => set_channel_extra(config, name, value),
        ["gateway", sub, field] if matches!(*sub, "owner" | "cron" | "heartbeat" | "dream") => {
            set_gateway(config, sub, field, value)
        }
        ["web_fetch", field] => set_web_fetch(config, field, value),
        ["json_render", field] => set_json_render(config, field, value),
        ["memory", field] => set_memory(config, field, value),
        ["tracing", "enabled"] => {
            config.tracing.enabled = value_bool(value, "tracing.enabled")?;
            Ok(())
        }
        ["tracing", "reset_on_start"] => {
            config.tracing.reset_on_start = value_bool(value, "tracing.reset_on_start")?;
            Ok(())
        }
        ["tracing", "backends", idx, field] => set_backend_field(config, idx, field, value),
        ["subagent", field] => set_subagent(config, field, value),
        ["triggers", field] => set_triggers(config, field, value),
        ["triggers", "secrets", idx, field] => set_secret_field(config, idx, field, value),
        ["pressure", field] => set_pressure(config, field, value),
        _ => Err(format!("Unsupported patch path: {}", segs.join("."))),
    }
}

fn set_subagent(config: &mut Config, field: &str, value: &serde_json::Value) -> Result<(), String> {
    match field {
        "max_concurrent" => {
            config.subagent.max_concurrent = value_u32(value, "subagent.max_concurrent")? as usize
        }
        _ => return Err(format!("Unknown subagent field: {}", field)),
    }
    Ok(())
}

fn set_triggers(config: &mut Config, field: &str, value: &serde_json::Value) -> Result<(), String> {
    match field {
        "enabled" => config.triggers.enabled = value_bool(value, "triggers.enabled")?,
        "tick_interval_s" => {
            config.triggers.tick_interval_s = value_u64(value, "triggers.tick_interval_s")?
        }
        "tick_timeout_s" => {
            config.triggers.tick_timeout_s = value_u64(value, "triggers.tick_timeout_s")?
        }
        "max_concurrent_collect" => {
            config.triggers.max_concurrent_collect =
                value_u32(value, "triggers.max_concurrent_collect")? as usize
        }
        "max_queue" => config.triggers.max_queue = value_u32(value, "triggers.max_queue")? as usize,
        "max_merge" => config.triggers.max_merge = value_u32(value, "triggers.max_merge")? as usize,
        "history_lines" => {
            config.triggers.history_lines = value_u32(value, "triggers.history_lines")? as usize
        }
        _ => return Err(format!("Unknown triggers field: {}", field)),
    }
    Ok(())
}

fn set_secret_field(
    config: &mut Config,
    idx: &str,
    field: &str,
    value: &serde_json::Value,
) -> Result<(), String> {
    let i = parse_index(idx)?;
    let prefix = format!("triggers.secrets.{}", idx);
    let entry = config
        .triggers
        .secrets
        .get_mut(i)
        .ok_or_else(|| format!("{}: index out of range", prefix))?;
    match field {
        "name" => entry.name = value_str(value, &format!("{}.name", prefix))?,
        "value" => entry.value = value_str(value, &format!("{}.value", prefix))?,
        _ => return Err(format!("Unknown secret field: {}", field)),
    }
    Ok(())
}

fn set_pressure(config: &mut Config, field: &str, value: &serde_json::Value) -> Result<(), String> {
    match field {
        "single_tool_result_ratio" => {
            config.pressure.single_tool_result_ratio =
                value_f32(value, "pressure.single_tool_result_ratio")?
        }
        "persisted_preview_ratio" => {
            config.pressure.persisted_preview_ratio =
                value_f32(value, "pressure.persisted_preview_ratio")?
        }
        "snip_safety_ratio" => {
            config.pressure.snip_safety_ratio = value_f32(value, "pressure.snip_safety_ratio")?
        }
        "consolidate_trigger_ratio" => {
            config.pressure.consolidate_trigger_ratio =
                value_f32(value, "pressure.consolidate_trigger_ratio")?
        }
        "consolidate_target_ratio" => {
            config.pressure.consolidate_target_ratio =
                value_f32(value, "pressure.consolidate_target_ratio")?
        }
        "summary_ratio" => {
            config.pressure.summary_ratio = value_f32(value, "pressure.summary_ratio")?
        }
        "min_keep_messages" => {
            config.pressure.min_keep_messages =
                value_u32(value, "pressure.min_keep_messages")? as usize
        }
        "keep_recent_tool_results" => {
            config.pressure.keep_recent_tool_results =
                value_u32(value, "pressure.keep_recent_tool_results")? as usize
        }
        "tool_results_ttl_days" => {
            config.pressure.tool_results_ttl_days =
                value_u64(value, "pressure.tool_results_ttl_days")?
        }
        _ => return Err(format!("Unknown pressure field: {}", field)),
    }
    Ok(())
}

fn set_agent(config: &mut Config, field: &str, value: &serde_json::Value) -> Result<(), String> {
    match field {
        "provider" => config.agent.provider = value_str(value, "agent.provider")?,
        "max_iterations" => config.agent.max_iterations = value_u32(value, "agent.max_iterations")?,
        "timeout_seconds" => {
            config.agent.timeout_seconds = value_u64(value, "agent.timeout_seconds")?
        }
        "persist_tool_results" => {
            config.agent.persist_tool_results = value_bool(value, "agent.persist_tool_results")?
        }
        _ => return Err(format!("Unknown agent field: {}", field)),
    }
    Ok(())
}

fn set_provider_field(
    config: &mut Config,
    name: &str,
    field: &str,
    value: &serde_json::Value,
) -> Result<(), String> {
    let prefix = format!("providers.{}", name);
    let provider = config
        .providers
        .get_mut(name)
        .ok_or_else(|| format!("{}: provider not found", prefix))?;
    match field {
        "type" => provider.r#type = value_str(value, &format!("{}.type", prefix))?,
        "api_url" => provider.api_url = value_str(value, &format!("{}.api_url", prefix))?,
        "base_url" => provider.base_url = value_str(value, &format!("{}.base_url", prefix))?,
        "api_key" => provider.api_key = value_str(value, &format!("{}.api_key", prefix))?,
        "model" => provider.model = value_str(value, &format!("{}.model", prefix))?,
        "temperature" => {
            provider.temperature = value_f32(value, &format!("{}.temperature", prefix))?
        }
        "max_tokens" => provider.max_tokens = value_u32(value, &format!("{}.max_tokens", prefix))?,
        "context_window_tokens" => {
            provider.context_window_tokens =
                value_u32(value, &format!("{}.context_window_tokens", prefix))?
        }
        "prompt_cache_enabled" => {
            provider.prompt_cache_enabled =
                value_bool(value, &format!("{}.prompt_cache_enabled", prefix))?
        }
        "disable_thinking" => {
            provider.disable_thinking = value_bool(value, &format!("{}.disable_thinking", prefix))?
        }
        _ => return Err(format!("Unknown provider field: {}", field)),
    }
    Ok(())
}

fn set_tool_field(
    config: &mut Config,
    idx: &str,
    field: &str,
    value: &serde_json::Value,
) -> Result<(), String> {
    let i = parse_index(idx)?;
    let tool = config
        .tools
        .get_mut(i)
        .ok_or_else(|| format!("tools.{}: index out of range", idx))?;
    match field {
        "name" => tool.name = value_str(value, &format!("tools.{}.name", idx))?,
        "enabled" => tool.enabled = value_bool(value, &format!("tools.{}.enabled", idx))?,
        _ => return Err(format!("Unknown tool field: {}", field)),
    }
    Ok(())
}

fn set_channel_enabled(
    config: &mut Config,
    name: &str,
    value: &serde_json::Value,
) -> Result<(), String> {
    let enabled = value_bool(value, &format!("channels.{}.enabled", name))?;
    config.channels.entry(name.to_string()).or_default().enabled = enabled;
    Ok(())
}

fn set_channel_extra(
    config: &mut Config,
    name: &str,
    value: &serde_json::Value,
) -> Result<(), String> {
    let obj = value
        .as_object()
        .ok_or_else(|| format!("channels.{}.extra: expected a JSON object", name))?;
    let extra: HashMap<String, serde_json::Value> =
        obj.iter().map(|(k, v)| (k.clone(), v.clone())).collect();
    config.channels.entry(name.to_string()).or_default().extra = extra;
    Ok(())
}

fn set_gateway(
    config: &mut Config,
    sub: &str,
    field: &str,
    value: &serde_json::Value,
) -> Result<(), String> {
    match (sub, field) {
        ("owner", "id") => config.gateway.owner.id = value_str(value, "gateway.owner.id")?,
        ("owner", "display_name") => {
            config.gateway.owner.display_name = value_opt_str(value, "gateway.owner.display_name")?
        }
        ("cron", "enabled") => {
            config.gateway.cron.enabled = value_bool(value, "gateway.cron.enabled")?
        }
        ("heartbeat", "enabled") => {
            config.gateway.heartbeat.enabled = value_bool(value, "gateway.heartbeat.enabled")?
        }
        ("heartbeat", "interval_s") => {
            config.gateway.heartbeat.interval_s = value_u64(value, "gateway.heartbeat.interval_s")?
        }
        ("heartbeat", "channel") => {
            config.gateway.heartbeat.channel = value_opt_str(value, "gateway.heartbeat.channel")?
        }
        ("heartbeat", "to") => {
            config.gateway.heartbeat.to = value_opt_str(value, "gateway.heartbeat.to")?
        }
        ("dream", "enabled") => {
            config.gateway.dream.enabled = value_bool(value, "gateway.dream.enabled")?
        }
        ("dream", "interval_ms") => {
            config.gateway.dream.interval_ms = value_i64(value, "gateway.dream.interval_ms")?
        }
        ("dream", "model_override") => {
            config.gateway.dream.model_override =
                value_opt_str(value, "gateway.dream.model_override")?
        }
        ("dream", "max_entries") => {
            config.gateway.dream.max_entries = value_usize(value, "gateway.dream.max_entries")?
        }
        _ => return Err(format!("Unknown gateway field: {}.{}", sub, field)),
    }
    Ok(())
}

fn set_web_fetch(
    config: &mut Config,
    field: &str,
    value: &serde_json::Value,
) -> Result<(), String> {
    let wf = config.web_fetch.get_or_insert_with(Default::default);
    match field {
        "max_chars" => wf.max_chars = value_usize(value, "web_fetch.max_chars")?,
        "timeout_s" => wf.timeout_s = value_u64(value, "web_fetch.timeout_s")?,
        "user_agent" => wf.user_agent = value_str(value, "web_fetch.user_agent")?,
        _ => return Err(format!("Unknown web_fetch field: {}", field)),
    }
    Ok(())
}

fn set_memory(config: &mut Config, field: &str, value: &serde_json::Value) -> Result<(), String> {
    match field {
        "backend" => config.memory.backend = value_str(value, "memory.backend")?,
        _ => return Err(format!("Unknown memory field: {}", field)),
    }
    Ok(())
}

fn set_json_render(
    config: &mut Config,
    field: &str,
    value: &serde_json::Value,
) -> Result<(), String> {
    match field {
        "enabled" => config.json_render.enabled = value_bool(value, "json_render.enabled")?,
        "model_override" => {
            config.json_render.model_override = value_opt_str(value, "json_render.model_override")?
        }
        "max_tokens" => config.json_render.max_tokens = value_u32(value, "json_render.max_tokens")?,
        "timeout_seconds" => {
            config.json_render.timeout_seconds = value_u64(value, "json_render.timeout_seconds")?
        }
        _ => return Err(format!("Unknown json_render field: {}", field)),
    }
    Ok(())
}

fn set_backend_field(
    config: &mut Config,
    idx: &str,
    field: &str,
    value: &serde_json::Value,
) -> Result<(), String> {
    let i = parse_index(idx)?;
    let backend = config
        .tracing
        .backends
        .get_mut(i)
        .ok_or_else(|| format!("tracing.backends.{}: index out of range", idx))?;
    let prefix = format!("tracing.backends.{}", idx);
    match field {
        "type" => backend.r#type = value_str(value, &format!("{}.type", prefix))?,
        "enabled" => backend.enabled = value_bool(value, &format!("{}.enabled", prefix))?,
        "path" => backend.path = value_str(value, &format!("{}.path", prefix))?,
        "endpoint" => backend.endpoint = value_str(value, &format!("{}.endpoint", prefix))?,
        "service_name" => {
            backend.service_name = value_str(value, &format!("{}.service_name", prefix))?
        }
        _ => return Err(format!("Unknown backend field: {}", field)),
    }
    Ok(())
}

// ── Add / Remove ──

fn add_path(config: &mut Config, op: &PatchOp, segs: &[&str]) -> Result<(), String> {
    match segs {
        ["providers", name] => {
            let mut provider: ProviderConfig = if op.value.is_null() {
                ProviderConfig::default()
            } else {
                serde_json::from_value(op.value.clone())
                    .map_err(|e| format!("providers.{}: invalid provider object: {}", name, e))?
            };
            // A rename (or re-add) can inherit unmasked fields from the
            // previous entry via `from` — required because secret fields are
            // masked in the schema and the frontend can never read them back.
            if let Some(from) = &op.from {
                let inherited = config.providers.get(from).ok_or_else(|| {
                    format!("providers.{}: 'from' provider '{}' not found", name, from)
                })?;
                if !op
                    .value
                    .as_object()
                    .map(|o| o.contains_key("api_key"))
                    .unwrap_or(false)
                {
                    provider.api_key = inherited.api_key.clone();
                }
                provider.unknown.extend(inherited.unknown.clone());
            }
            config.providers.insert(name.to_string(), provider);
            Ok(())
        }
        ["tools"] => {
            let name = op
                .value
                .get("name")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let enabled = op
                .value
                .get("enabled")
                .and_then(|v| v.as_bool())
                .unwrap_or(true);
            config.tools.push(ToolEntry { name, enabled });
            Ok(())
        }
        ["channels", name] => {
            let channel: ChannelConfig = if op.value.is_null() {
                ChannelConfig::default()
            } else {
                serde_json::from_value(op.value.clone())
                    .map_err(|e| format!("channels.{}: invalid channel object: {}", name, e))?
            };
            config.channels.insert(name.to_string(), channel);
            Ok(())
        }
        ["tracing", "backends"] => {
            let backend: TraceBackendEntry = serde_json::from_value(op.value.clone())
                .map_err(|e| format!("tracing.backends: invalid backend object: {}", e))?;
            config.tracing.backends.push(backend);
            Ok(())
        }
        ["triggers", "secrets"] => {
            let entry: SecretEntry = serde_json::from_value(op.value.clone())
                .map_err(|e| format!("triggers.secrets: invalid secret object: {}", e))?;
            config.triggers.secrets.push(entry);
            Ok(())
        }
        _ => Err(format!("add not supported for path: {}", segs.join("."))),
    }
}

fn remove_path(config: &mut Config, segs: &[&str]) -> Result<(), String> {
    match segs {
        ["providers", name] => {
            if config.providers.remove(*name).is_none() {
                Err(format!("providers.{}: not found", name))
            } else {
                Ok(())
            }
        }
        ["tools", idx] => {
            let i = parse_index(idx)?;
            if i >= config.tools.len() {
                Err(format!("tools.{}: index out of range", idx))
            } else {
                config.tools.remove(i);
                Ok(())
            }
        }
        ["channels", name] => {
            if config.channels.remove(*name).is_none() {
                Err(format!("channels.{}: not found", name))
            } else {
                Ok(())
            }
        }
        ["tracing", "backends", idx] => {
            let i = parse_index(idx)?;
            if i >= config.tracing.backends.len() {
                Err(format!("tracing.backends.{}: index out of range", idx))
            } else {
                config.tracing.backends.remove(i);
                Ok(())
            }
        }
        ["triggers", "secrets", idx] => {
            let i = parse_index(idx)?;
            if i >= config.triggers.secrets.len() {
                Err(format!("triggers.secrets.{}: index out of range", idx))
            } else {
                config.triggers.secrets.remove(i);
                Ok(())
            }
        }
        _ => Err(format!("remove not supported for path: {}", segs.join("."))),
    }
}

// ── Value coercion helpers ──

fn value_str(v: &serde_json::Value, path: &str) -> Result<String, String> {
    v.as_str()
        .map(|s| s.to_string())
        .ok_or_else(|| format!("{}: expected a string", path))
}

fn value_opt_str(v: &serde_json::Value, path: &str) -> Result<Option<String>, String> {
    if v.is_null() {
        Ok(None)
    } else {
        value_str(v, path).map(Some)
    }
}

fn value_bool(v: &serde_json::Value, path: &str) -> Result<bool, String> {
    v.as_bool()
        .ok_or_else(|| format!("{}: expected a boolean", path))
}

fn value_u32(v: &serde_json::Value, path: &str) -> Result<u32, String> {
    value_u64(v, path)
        .and_then(|n| u32::try_from(n).map_err(|_| format!("{}: integer out of range", path)))
}

fn value_u64(v: &serde_json::Value, path: &str) -> Result<u64, String> {
    v.as_u64()
        .ok_or_else(|| format!("{}: expected a non-negative integer", path))
}

fn value_i64(v: &serde_json::Value, path: &str) -> Result<i64, String> {
    v.as_i64()
        .ok_or_else(|| format!("{}: expected an integer", path))
}

fn value_usize(v: &serde_json::Value, path: &str) -> Result<usize, String> {
    value_u64(v, path)
        .and_then(|n| usize::try_from(n).map_err(|_| format!("{}: integer out of range", path)))
}

fn value_f32(v: &serde_json::Value, path: &str) -> Result<f32, String> {
    v.as_f64()
        .map(|f| f as f32)
        .ok_or_else(|| format!("{}: expected a number", path))
}

// ── Validation error mapping ──

/// Map the single `Config::validate()` error onto the affected field(s).
fn validate_to_field_errors(message: &str) -> Vec<FieldError> {
    if message.starts_with("agent.provider") {
        return vec![FieldError {
            path: "agent.provider".to_string(),
            message: message.to_string(),
        }];
    }
    if let Some(name) = provider_name_from(message) {
        let path = if message.contains("api_key") {
            format!("providers.{}.api_key", name)
        } else if message.contains("model") {
            format!("providers.{}.model", name)
        } else {
            "agent.provider".to_string()
        };
        return vec![FieldError {
            path,
            message: message.to_string(),
        }];
    }
    vec![FieldError {
        path: String::new(),
        message: message.to_string(),
    }]
}

/// Extract the provider name from a message like `provider 'x'.api_key ...`.
fn provider_name_from(message: &str) -> Option<String> {
    let start = message.find('\'')? + 1;
    let rel = &message[start..];
    let len = rel.find('\'')?;
    Some(rel[..len].to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn op(kind: PatchOpKind, path: &str, value: serde_json::Value) -> PatchOp {
        PatchOp {
            op: kind,
            path: path.to_string(),
            value,
            from: None,
        }
    }

    fn make_config() -> Config {
        let mut providers = HashMap::new();
        providers.insert(
            "default".to_string(),
            ProviderConfig {
                r#type: "openai".to_string(),
                api_url: String::new(),
                base_url: "https://api.openai.com".to_string(),
                api_key: "sk-super-secret".to_string(),
                model: "gpt-4o".to_string(),
                temperature: 0.7,
                max_tokens: 4096,
                context_window_tokens: 32768,
                prompt_cache_enabled: true,
                disable_thinking: false,
                unknown: Default::default(),
            },
        );
        let mut extra = HashMap::new();
        extra.insert("port".to_string(), json!(8080));
        let mut channels = HashMap::new();
        channels.insert(
            "webui".to_string(),
            ChannelConfig {
                enabled: true,
                extra,
            },
        );
        Config {
            agent: AgentConfig {
                provider: "default".to_string(),
                max_iterations: 40,
                timeout_seconds: 120,
                persist_tool_results: true,
                unknown: Default::default(),
            },
            providers,
            tools: vec![ToolEntry {
                name: "shell".to_string(),
                enabled: true,
            }],
            channels,
            gateway: crate::config::GatewayConfig {
                owner: Default::default(),
                cron: CronConfig {
                    enabled: true,
                    unknown: Default::default(),
                },
                heartbeat: HeartbeatConfig {
                    channel: Default::default(),
                    to: Default::default(),
                    enabled: true,
                    interval_s: 1800,
                    unknown: Default::default(),
                },
                dream: DreamConfig::default(),
            },
            web_fetch: Some(WebFetchConfig::default()),
            tracing: Default::default(),
            memory: Default::default(),
            json_render: Default::default(),
            subagent: Default::default(),
            triggers: Default::default(),
            pressure: Default::default(),
        }
    }

    fn section<'a>(tree: &'a SchemaTree, name: &str) -> &'a SchemaSection {
        tree.sections
            .iter()
            .find(|s| s.name == name)
            .unwrap_or_else(|| panic!("section {} missing", name))
    }

    fn field<'a>(sec: &'a SchemaSection, name: &str) -> &'a SchemaField {
        sec.fields
            .iter()
            .find(|f| f.name == name)
            .unwrap_or_else(|| panic!("field {} missing", name))
    }

    // ── Schema shape ──

    #[test]
    fn test_schema_has_all_sections() {
        let tree = serialize_schema(&make_config());
        let names: Vec<&str> = tree.sections.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(
            names,
            vec![
                "agent",
                "gateway.owner",
                "gateway.cron",
                "gateway.heartbeat",
                "gateway.dream",
                "web_fetch",
                "json_render",
                "tracing",
                "memory",
                "subagent",
                "triggers",
                "pressure",
                "providers",
                "tools",
                "channels",
                "tracing.backends",
                "triggers.secrets",
            ]
        );
    }

    #[test]
    fn test_schema_triggers_section_marks_secret_values() {
        let mut config = make_config();
        config.triggers.secrets.push(SecretEntry {
            name: "weather".to_string(),
            value: "plaintext-key".to_string(),
            unknown: Default::default(),
        });

        let tree = serialize_schema(&config);
        let secrets_section = section(&tree, "triggers.secrets");
        let item = &secrets_section.items.as_ref().unwrap()[0];

        let name = item.fields.iter().find(|f| f.name == "name").unwrap();
        assert_eq!(name.kind, "string");
        assert_eq!(name.value, json!("weather"));
        assert_eq!(name.path, "triggers.secrets.0.name");

        // The value is a secret: the editor only sees the flag, never the key.
        let value = item.fields.iter().find(|f| f.name == "value").unwrap();
        assert_eq!(value.kind, "secret");
        assert!(value.secret);
        assert_eq!(value.value, serde_json::Value::Null);

        // The scalar section advertises the collection instead of inlining it.
        let triggers = section(&tree, "triggers");
        let secrets = field(triggers, "secrets");
        assert_eq!(secrets.kind, "list");
        assert_eq!(secrets.path, "triggers.secrets");
    }

    #[test]
    fn test_patch_add_set_remove_trigger_secret() {
        let mut config = make_config();
        let ops = vec![
            op(
                PatchOpKind::Add,
                "triggers.secrets",
                json!({"name": "api", "value": "v1"}),
            ),
            op(PatchOpKind::Set, "triggers.secrets.0.value", json!("v2")),
        ];
        let errors = apply_config_patch(&mut config, &ops).unwrap();
        assert!(errors.is_empty());
        assert_eq!(config.triggers.secrets.len(), 1);
        assert_eq!(config.triggers.secrets[0].name, "api");
        assert_eq!(config.triggers.secrets[0].value, "v2");

        let ops = vec![op(PatchOpKind::Remove, "triggers.secrets.0", json!(null))];
        let errors = apply_config_patch(&mut config, &ops).unwrap();
        assert!(errors.is_empty());
        assert!(config.triggers.secrets.is_empty());
    }

    #[test]
    fn test_patch_triggers_and_subagent_scalars() {
        let mut config = make_config();
        let ops = vec![
            op(PatchOpKind::Set, "subagent.max_concurrent", json!(5)),
            op(PatchOpKind::Set, "triggers.enabled", json!(false)),
            op(PatchOpKind::Set, "triggers.tick_interval_s", json!(120)),
            op(PatchOpKind::Set, "triggers.max_queue", json!(30)),
        ];
        let errors = apply_config_patch(&mut config, &ops).unwrap();
        assert!(errors.is_empty(), "{errors:?}");
        assert_eq!(config.subagent.max_concurrent, 5);
        assert!(!config.triggers.enabled);
        assert_eq!(config.triggers.tick_interval_s, 120);
        assert_eq!(config.triggers.max_queue, 30);
    }

    #[test]
    fn test_schema_memory_section() {
        let tree = serialize_schema(&make_config());
        let memory = section(&tree, "memory");
        let backend = field(memory, "backend");
        assert_eq!(backend.kind, "select");
        assert_eq!(backend.value, json!("markdown"));
        assert_eq!(backend.allowed, vec!["markdown"]);
        assert_eq!(backend.path, "memory.backend");
        assert!(!backend.desc.is_empty());
    }

    #[test]
    fn test_schema_pressure_section_exposes_ratios() {
        let tree = serialize_schema(&make_config());
        let pressure = section(&tree, "pressure");
        let single = field(pressure, "single_tool_result_ratio");
        assert_eq!(single.kind, "float");
        assert_eq!(single.path, "pressure.single_tool_result_ratio");
        assert!((single.value.as_f64().unwrap() - 0.25).abs() < 1e-6);
        let summary = field(pressure, "summary_ratio").value.as_f64().unwrap();
        assert!((summary - 0.02).abs() < 1e-6);
        assert_eq!(field(pressure, "min_keep_messages").kind, "int");
        assert_eq!(field(pressure, "tool_results_ttl_days").value, json!(7));
    }

    #[test]
    fn test_schema_agent_section_drops_deprecated_limit_fields() {
        let tree = serialize_schema(&make_config());
        let agent = section(&tree, "agent");
        assert!(
            agent
                .fields
                .iter()
                .all(|f| f.name != "max_tool_result_chars")
        );
        assert!(
            agent
                .fields
                .iter()
                .all(|f| f.name != "context_window_tokens")
        );
    }

    #[test]
    fn test_schema_providers_section_exposes_context_window() {
        let tree = serialize_schema(&make_config());
        let providers = section(&tree, "providers");
        let item_fields = providers.item_fields.as_ref().unwrap();
        let window = item_fields
            .iter()
            .find(|f| f.name == "context_window_tokens")
            .expect("provider context window must be editable");
        assert_eq!(window.kind, "int");
        assert_eq!(window.value, json!(32_768));
    }

    #[test]
    fn test_patch_provider_context_window_tokens() {
        let mut config = make_config();
        let errors = apply_config_patch(
            &mut config,
            &[op(
                PatchOpKind::Set,
                "providers.default.context_window_tokens",
                json!(128_000),
            )],
        )
        .unwrap();
        assert!(errors.is_empty());
        assert_eq!(config.providers["default"].context_window_tokens, 128_000);
    }

    #[test]
    fn test_patch_pressure_ratio_applies_and_clamps() {
        let mut config = make_config();
        let errors = apply_config_patch(
            &mut config,
            &[op(
                PatchOpKind::Set,
                "pressure.single_tool_result_ratio",
                json!(0.5),
            )],
        )
        .unwrap();
        assert!(errors.is_empty());
        assert!((config.pressure.single_tool_result_ratio - 0.5).abs() < f32::EPSILON);

        // Out-of-range values are clamped, not rejected.
        apply_config_patch(
            &mut config,
            &[op(
                PatchOpKind::Set,
                "pressure.single_tool_result_ratio",
                json!(9.0),
            )],
        )
        .unwrap();
        assert!((config.pressure.single_tool_result_ratio - 0.90).abs() < 1e-6);
    }

    #[test]
    fn test_schema_scalar_kind_and_constraints() {
        let tree = serialize_schema(&make_config());
        let agent = section(&tree, "agent");
        let max_iterations = field(agent, "max_iterations");
        assert_eq!(max_iterations.kind, "int");
        assert_eq!(max_iterations.min, Some(json!(1)));
        assert_eq!(max_iterations.max, Some(json!(200)));
        assert_eq!(max_iterations.value, json!(40));
        assert!(!max_iterations.secret);
        assert_eq!(max_iterations.path, "agent.max_iterations");
        assert!(!max_iterations.desc.is_empty());

        let persist = field(agent, "persist_tool_results");
        assert_eq!(persist.kind, "bool");
        assert_eq!(persist.value, json!(true));
    }

    #[test]
    fn test_schema_kind_mapping() {
        let tree = serialize_schema(&make_config());
        let providers = section(&tree, "providers");
        let item_fields = providers.item_fields.as_ref().unwrap();
        let by_name = |n: &str| item_fields.iter().find(|f| f.name == n).unwrap();
        assert_eq!(by_name("type").kind, "select");
        assert_eq!(
            by_name("type").allowed,
            vec!["openai", "anthropic", "custom"]
        );
        assert_eq!(by_name("type").value, json!("openai"));
        assert_eq!(by_name("api_url").kind, "string");
        assert_eq!(by_name("max_tokens").kind, "int");
        assert_eq!(by_name("temperature").kind, "float");
        assert_eq!(by_name("prompt_cache_enabled").kind, "bool");

        let dream = section(&tree, "gateway.dream");
        assert_eq!(field(dream, "model_override").kind, "optional_string");
        assert_eq!(field(dream, "model_override").value, json!(null));

        let tracing = section(&tree, "tracing");
        assert_eq!(field(tracing, "backends").kind, "list");
    }

    #[test]
    fn test_schema_secret_fields_masked() {
        let tree = serialize_schema(&make_config());
        let providers = section(&tree, "providers");
        let items = providers.items.as_ref().unwrap();
        let api_key = items[0]
            .fields
            .iter()
            .find(|f| f.name == "api_key")
            .unwrap();
        assert!(api_key.secret);
        assert_eq!(api_key.value, json!(null));
        // Non-secret fields keep their values.
        let model = items[0].fields.iter().find(|f| f.name == "model").unwrap();
        assert!(!model.secret);
        assert_eq!(model.value, json!("gpt-4o"));
        // The element template also masks the secret.
        let template = providers.item_fields.as_ref().unwrap();
        let tmpl_key = template.iter().find(|f| f.name == "api_key").unwrap();
        assert!(tmpl_key.secret);
        assert_eq!(tmpl_key.value, json!(null));
    }

    #[test]
    fn test_schema_collection_structure() {
        let tree = serialize_schema(&make_config());
        let providers = section(&tree, "providers");
        let items = providers.items.as_ref().unwrap();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].key.as_deref(), Some("default"));
        let model = items[0].fields.iter().find(|f| f.name == "model").unwrap();
        assert_eq!(model.path, "providers.default.model");

        let tools = section(&tree, "tools");
        let tool_items = tools.items.as_ref().unwrap();
        assert_eq!(tool_items.len(), 1);
        let name = tool_items[0]
            .fields
            .iter()
            .find(|f| f.name == "name")
            .unwrap();
        assert_eq!(name.kind, "select");
        assert_eq!(name.path, "tools.0.name");
        assert!(name.allowed.iter().any(|t| t == "shell"));

        let channels = section(&tree, "channels");
        let ch_items = channels.items.as_ref().unwrap();
        assert_eq!(ch_items[0].key.as_deref(), Some("webui"));
        assert_eq!(ch_items[0].extra, Some(json!({"port": 8080})));
        let enabled = ch_items[0]
            .fields
            .iter()
            .find(|f| f.name == "enabled")
            .unwrap();
        assert_eq!(enabled.path, "channels.webui.enabled");

        let backends = section(&tree, "tracing.backends");
        assert!(backends.items.as_ref().unwrap().is_empty());
        let bt = backends.item_fields.as_ref().unwrap();
        assert_eq!(bt.len(), 5);
    }

    #[test]
    fn test_schema_json_render_section_shows_defaults() {
        let mut config = make_config();
        config.json_render = crate::config::JsonRenderConfig::default();
        let tree = serialize_schema(&config);
        let jr = section(&tree, "json_render");
        assert_eq!(field(jr, "enabled").value, json!(true));
        assert_eq!(field(jr, "max_tokens").value, json!(2048));
        assert_eq!(field(jr, "timeout_seconds").value, json!(30));
        assert_eq!(field(jr, "model_override").kind, "optional_string");
        assert_eq!(field(jr, "model_override").value, json!(null));
    }

    #[test]
    fn test_patch_json_render_fields() {
        let mut config = make_config();
        let ops = vec![
            op(PatchOpKind::Set, "json_render.enabled", json!(false)),
            op(PatchOpKind::Set, "json_render.max_tokens", json!(1024)),
            op(PatchOpKind::Set, "json_render.timeout_seconds", json!(10)),
            op(
                PatchOpKind::Set,
                "json_render.model_override",
                json!("cheap"),
            ),
        ];
        let errors = apply_config_patch(&mut config, &ops).unwrap();
        assert!(errors.is_empty());
        assert!(!config.json_render.enabled);
        assert_eq!(config.json_render.max_tokens, 1024);
        assert_eq!(config.json_render.timeout_seconds, 10);
        assert_eq!(config.json_render.model_override, Some("cheap".to_string()));
    }

    #[test]
    fn test_schema_web_fetch_defaults_when_absent() {
        let mut config = make_config();
        config.web_fetch = None;
        let tree = serialize_schema(&config);
        let wf = section(&tree, "web_fetch");
        assert_eq!(field(wf, "max_chars").value, json!(50000));
        assert_eq!(field(wf, "timeout_s").value, json!(30));
    }

    #[test]
    fn test_schema_tracing_backends_items() {
        let mut config = make_config();
        config.tracing.backends.push(TraceBackendEntry {
            r#type: "file".to_string(),
            enabled: true,
            path: "traces.jsonl".to_string(),
            endpoint: String::new(),
            service_name: "slimbot".to_string(),
            unknown: Default::default(),
        });
        let tree = serialize_schema(&config);
        let backends = section(&tree, "tracing.backends");
        let items = backends.items.as_ref().unwrap();
        assert_eq!(items.len(), 1);
        let path = items[0].fields.iter().find(|f| f.name == "path").unwrap();
        assert_eq!(path.value, json!("traces.jsonl"));
        assert_eq!(path.path, "tracing.backends.0.path");
    }

    // ── Patch engine: set ──

    #[test]
    fn test_patch_set_scalar() {
        let mut config = make_config();
        let errors = apply_config_patch(
            &mut config,
            &[op(PatchOpKind::Set, "agent.max_iterations", json!(99))],
        )
        .unwrap();
        assert!(errors.is_empty());
        assert_eq!(config.agent.max_iterations, 99);
    }

    #[test]
    fn test_patch_set_provider_field() {
        let mut config = make_config();
        let errors = apply_config_patch(
            &mut config,
            &[op(
                PatchOpKind::Set,
                "providers.default.model",
                json!("gpt-4-turbo"),
            )],
        )
        .unwrap();
        assert!(errors.is_empty());
        assert_eq!(config.providers["default"].model, "gpt-4-turbo");
    }

    #[test]
    fn test_patch_set_secret_field_rotates_key() {
        let mut config = make_config();
        let errors = apply_config_patch(
            &mut config,
            &[op(
                PatchOpKind::Set,
                "providers.default.api_key",
                json!("sk-new-key"),
            )],
        )
        .unwrap();
        assert!(errors.is_empty());
        assert_eq!(config.providers["default"].api_key, "sk-new-key");
    }

    #[test]
    fn test_patch_set_gateway_nested() {
        let mut config = make_config();
        let ops = vec![
            op(PatchOpKind::Set, "gateway.cron.enabled", json!(false)),
            op(PatchOpKind::Set, "gateway.heartbeat.interval_s", json!(60)),
            op(
                PatchOpKind::Set,
                "gateway.dream.model_override",
                json!(null),
            ),
        ];
        let errors = apply_config_patch(&mut config, &ops).unwrap();
        assert!(errors.is_empty());
        assert!(!config.gateway.cron.enabled);
        assert_eq!(config.gateway.heartbeat.interval_s, 60);
        assert_eq!(config.gateway.dream.model_override, None);
    }

    #[test]
    fn test_patch_set_web_fetch_creates_section() {
        let mut config = make_config();
        config.web_fetch = None;
        let errors = apply_config_patch(
            &mut config,
            &[op(PatchOpKind::Set, "web_fetch.max_chars", json!(20000))],
        )
        .unwrap();
        assert!(errors.is_empty());
        let wf = config.web_fetch.as_ref().unwrap();
        assert_eq!(wf.max_chars, 20000);
        assert_eq!(wf.timeout_s, 30);
    }

    #[test]
    fn test_patch_set_tracing_flags() {
        let mut config = make_config();
        let errors = apply_config_patch(
            &mut config,
            &[op(PatchOpKind::Set, "tracing.enabled", json!(true))],
        )
        .unwrap();
        assert!(errors.is_empty());
        assert!(config.tracing.enabled);
    }

    #[test]
    fn test_patch_set_memory_backend() {
        let mut config = make_config();
        let errors = apply_config_patch(
            &mut config,
            &[op(PatchOpKind::Set, "memory.backend", json!("markdown"))],
        )
        .unwrap();
        assert!(errors.is_empty());
        assert_eq!(config.memory.backend, "markdown");
    }

    #[test]
    fn test_patch_set_memory_backend_unknown_field() {
        let mut config = make_config();
        let result = apply_config_patch(
            &mut config,
            &[op(PatchOpKind::Set, "memory.nope", json!("x"))],
        );
        assert!(result.is_err());
    }

    #[test]
    fn test_patch_set_memory_backend_invalid_value_clamps() {
        let mut config = make_config();
        let errors = apply_config_patch(
            &mut config,
            &[op(PatchOpKind::Set, "memory.backend", json!("sqlite"))],
        )
        .unwrap();
        assert!(errors.is_empty());
        // `clamp()` inside apply_config_patch falls back to the default.
        assert_eq!(config.memory.backend, "markdown");
    }

    #[test]
    fn test_patch_set_channel_enabled_and_extra() {
        let mut config = make_config();
        let ops = vec![
            op(PatchOpKind::Set, "channels.webui.enabled", json!(false)),
            op(
                PatchOpKind::Set,
                "channels.webui.extra",
                json!({"host": "127.0.0.1", "port": 9090}),
            ),
        ];
        let errors = apply_config_patch(&mut config, &ops).unwrap();
        assert!(errors.is_empty());
        let webui = &config.channels["webui"];
        assert!(!webui.enabled);
        assert_eq!(webui.extra["port"], json!(9090));
        assert_eq!(webui.extra["host"], json!("127.0.0.1"));
    }

    #[test]
    fn test_patch_set_tool_fields() {
        let mut config = make_config();
        let errors = apply_config_patch(
            &mut config,
            &[op(PatchOpKind::Set, "tools.0.enabled", json!(false))],
        )
        .unwrap();
        assert!(errors.is_empty());
        assert!(!config.tools[0].enabled);
    }

    // ── Patch engine: add / remove ──

    #[test]
    fn test_patch_add_provider() {
        let mut config = make_config();
        let errors = apply_config_patch(
            &mut config,
            &[op(PatchOpKind::Add, "providers.cheap", json!({"type": "custom", "base_url": "https://x.example", "api_key": "sk-x", "model": "qwen"}))],
        )
        .unwrap();
        assert!(errors.is_empty());
        let cheap = config.providers.get("cheap").unwrap();
        assert_eq!(cheap.r#type, "custom");
        assert_eq!(cheap.model, "qwen");
        assert_eq!(cheap.max_tokens, 4096); // default filled
    }

    #[test]
    fn test_patch_remove_provider() {
        let mut config = make_config();
        // Removing the only provider breaks validation → per-field errors,
        // but the removal itself must have succeeded.
        let errors = apply_config_patch(
            &mut config,
            &[op(PatchOpKind::Remove, "providers.default", json!(null))],
        )
        .unwrap();
        assert!(config.providers.is_empty());
        assert!(!errors.is_empty());
    }

    #[test]
    fn test_patch_add_remove_tool() {
        let mut config = make_config();
        let ops = vec![
            op(
                PatchOpKind::Add,
                "tools",
                json!({"name": "grep", "enabled": true}),
            ),
            op(PatchOpKind::Remove, "tools.0", json!(null)),
        ];
        let errors = apply_config_patch(&mut config, &ops).unwrap();
        assert!(errors.is_empty());
        assert_eq!(config.tools.len(), 1);
        assert_eq!(config.tools[0].name, "grep");
    }

    #[test]
    fn test_patch_add_remove_backend() {
        let mut config = make_config();
        let ops = vec![
            op(
                PatchOpKind::Add,
                "tracing.backends",
                json!({"type": "file", "path": "traces.jsonl"}),
            ),
            op(PatchOpKind::Set, "tracing.backends.0.enabled", json!(false)),
        ];
        let errors = apply_config_patch(&mut config, &ops).unwrap();
        assert!(errors.is_empty());
        assert_eq!(config.tracing.backends.len(), 1);
        assert_eq!(config.tracing.backends[0].path, "traces.jsonl");
        assert!(!config.tracing.backends[0].enabled);

        let ops = vec![op(PatchOpKind::Remove, "tracing.backends.0", json!(null))];
        let errors = apply_config_patch(&mut config, &ops).unwrap();
        assert!(errors.is_empty());
        assert!(config.tracing.backends.is_empty());
    }

    #[test]
    fn test_patch_add_remove_channel() {
        let mut config = make_config();
        let errors = apply_config_patch(
            &mut config,
            &[op(
                PatchOpKind::Add,
                "channels.telegram",
                json!({"enabled": false, "token": "abc"}),
            )],
        )
        .unwrap();
        assert!(errors.is_empty());
        let tg = config.channels.get("telegram").unwrap();
        assert!(!tg.enabled);
        assert_eq!(tg.extra["token"], json!("abc"));
    }

    #[test]
    fn test_patch_rename_provider_preserves_secret() {
        let mut config = make_config();
        config
            .providers
            .get_mut("default")
            .unwrap()
            .unknown
            .insert("future".to_string(), json!(1));
        let mut rename = op(
            PatchOpKind::Add,
            "providers.main",
            json!({"type": "openai", "model": "gpt-4-turbo", "temperature": 0.5}),
        );
        rename.from = Some("default".to_string());
        // add-with-`from` must run before the remove so the source entry still
        // exists; agent.provider must follow the rename or validation fails.
        let ops = vec![
            rename,
            op(PatchOpKind::Remove, "providers.default", json!(null)),
            op(PatchOpKind::Set, "agent.provider", json!("main")),
        ];
        let errors = apply_config_patch(&mut config, &ops).unwrap();
        assert!(errors.is_empty());
        assert!(!config.providers.contains_key("default"));
        let main = config.providers.get("main").unwrap();
        // Unmasked fields inherited from the removed entry.
        assert_eq!(main.api_key, "sk-super-secret");
        assert_eq!(main.unknown["future"], json!(1));
        // Edits from the add object applied.
        assert_eq!(main.model, "gpt-4-turbo");
        assert_eq!(main.temperature, 0.5);
    }

    #[test]
    fn test_patch_rename_typed_key_wins_over_inherited() {
        let mut config = make_config();
        let mut rename = op(
            PatchOpKind::Add,
            "providers.main",
            json!({"api_key": "sk-rotated"}),
        );
        rename.from = Some("default".to_string());
        let ops = vec![
            rename,
            op(PatchOpKind::Remove, "providers.default", json!(null)),
            op(PatchOpKind::Set, "agent.provider", json!("main")),
        ];
        let errors = apply_config_patch(&mut config, &ops).unwrap();
        assert!(errors.is_empty());
        assert_eq!(config.providers["main"].api_key, "sk-rotated");
    }

    #[test]
    fn test_patch_rename_missing_from_errors() {
        let mut config = make_config();
        let mut op = op(
            PatchOpKind::Add,
            "providers.main",
            json!({"type": "openai"}),
        );
        op.from = Some("ghost".to_string());
        assert!(apply_config_patch(&mut config, &[op]).is_err());
    }

    #[test]
    fn test_patch_invalid_path_errors() {
        let mut config = make_config();
        assert!(
            apply_config_patch(&mut config, &[op(PatchOpKind::Set, "agent.nope", json!(1))])
                .is_err()
        );
        assert!(
            apply_config_patch(
                &mut config,
                &[op(PatchOpKind::Set, "totally.unknown", json!(1))]
            )
            .is_err()
        );
        assert!(
            apply_config_patch(
                &mut config,
                &[op(PatchOpKind::Remove, "tools.5", json!(null))]
            )
            .is_err()
        );
    }

    #[test]
    fn test_patch_type_mismatch_errors() {
        let mut config = make_config();
        assert!(
            apply_config_patch(
                &mut config,
                &[op(PatchOpKind::Set, "agent.max_iterations", json!("many"))]
            )
            .is_err()
        );
        assert!(
            apply_config_patch(
                &mut config,
                &[op(
                    PatchOpKind::Set,
                    "providers.default.temperature",
                    json!(true)
                )]
            )
            .is_err()
        );
    }

    // ── Patch pipeline ──

    #[test]
    fn test_patch_clamp_applied() {
        let mut config = make_config();
        let errors = apply_config_patch(
            &mut config,
            &[op(PatchOpKind::Set, "agent.max_iterations", json!(9999))],
        )
        .unwrap();
        assert!(errors.is_empty());
        assert_eq!(config.agent.max_iterations, 200);
    }

    #[test]
    fn test_patch_normalize_applied() {
        let mut config = make_config();
        let errors = apply_config_patch(
            &mut config,
            &[op(PatchOpKind::Set, "agent.provider", json!(""))],
        )
        .unwrap();
        assert!(errors.is_empty());
        assert_eq!(config.agent.provider, "default");
    }

    #[test]
    fn test_patch_validate_failure_returns_field_errors() {
        let mut config = make_config();
        let errors = apply_config_patch(
            &mut config,
            &[op(PatchOpKind::Set, "agent.provider", json!("nonexistent"))],
        )
        .unwrap();
        assert!(!errors.is_empty());
        assert_eq!(errors[0].path, "agent.provider");
    }

    #[test]
    fn test_patch_validate_failure_empty_api_key_path() {
        let mut config = make_config();
        let errors = apply_config_patch(
            &mut config,
            &[op(PatchOpKind::Set, "providers.default.api_key", json!(""))],
        )
        .unwrap();
        assert!(!errors.is_empty());
        assert_eq!(errors[0].path, "providers.default.api_key");
    }

    #[test]
    fn test_patch_unknown_fields_preserved() {
        let mut config = make_config();
        config
            .agent
            .unknown
            .insert("future_field".to_string(), json!(42));
        let errors = apply_config_patch(
            &mut config,
            &[op(PatchOpKind::Set, "agent.max_iterations", json!(50))],
        )
        .unwrap();
        assert!(errors.is_empty());
        assert_eq!(config.agent.unknown["future_field"], json!(42));
        assert_eq!(config.agent.max_iterations, 50);
    }

    #[test]
    fn test_patch_accepts_all_schema_scalar_paths() {
        // Guard against drift between the macro FieldMeta (rendered in the
        // form) and the patch engine's hand-written field matches: every path
        // the schema renders must be accepted by `set` (re-applying the
        // current value is a no-op, so the config must be unchanged).
        let mut config = make_config();
        // Mirror Config::load_for_preview so schema values match the
        // clamp+normalize pipeline (api_url gets derived, etc.).
        config.clamp();
        config.normalize();
        let tree = serialize_schema(&config);
        let mut ops = Vec::new();
        for sec in &tree.sections {
            for f in &sec.fields {
                if matches!(f.kind.as_str(), "list" | "json" | "secret") {
                    continue; // display-only kinds / masked values
                }
                ops.push(op(PatchOpKind::Set, &f.path, f.value.clone()));
            }
            if let Some(items) = &sec.items {
                for item in items {
                    for f in &item.fields {
                        if f.kind == "secret" {
                            continue;
                        }
                        ops.push(op(PatchOpKind::Set, &f.path, f.value.clone()));
                    }
                }
            }
        }
        assert!(
            !ops.is_empty(),
            "schema must render at least one scalar path"
        );
        let mut patched = config.clone();
        let errors = apply_config_patch(&mut patched, &ops).unwrap();
        assert!(errors.is_empty());
        // Re-applying current values is a semantic no-op (Config has no
        // PartialEq — compare serialized forms instead).
        assert_eq!(
            serde_json::to_value(&patched).unwrap(),
            serde_json::to_value(&config).unwrap()
        );
    }

    #[test]
    fn test_patch_serialize_roundtrip_consistent() {
        let mut config = make_config();
        let ops = vec![
            op(PatchOpKind::Set, "agent.max_iterations", json!(55)),
            op(
                PatchOpKind::Set,
                "gateway.dream.model_override",
                json!("cheap"),
            ),
            op(
                PatchOpKind::Add,
                "tools",
                json!({"name": "list_dir", "enabled": false}),
            ),
        ];
        let errors = apply_config_patch(&mut config, &ops).unwrap();
        assert!(errors.is_empty());

        // Re-serializing reflects the patched values.
        let tree = serialize_schema(&config);
        let agent = section(&tree, "agent");
        assert_eq!(field(agent, "max_iterations").value, json!(55));
        let dream = section(&tree, "gateway.dream");
        assert_eq!(field(dream, "model_override").value, json!("cheap"));
        let tools = section(&tree, "tools");
        assert_eq!(tools.items.as_ref().unwrap().len(), 2);
    }
}
