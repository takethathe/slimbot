//! MCP trigger subscriptions and the active-source cap, exercised through the
//! two load seams the rest of the system uses: the effective load
//! (`Config::load_resolved_with`, which clamps then validates) and validation
//! of a parsed config.

use tempfile::NamedTempFile;

use slimbot::Config;
use slimbot::secrets::SecretStore;

/// Write `value` to a temp config file and return (guard, path).
fn config_file(value: serde_json::Value) -> (NamedTempFile, String) {
    let tmp = NamedTempFile::new().unwrap();
    let path = tmp.path().to_str().unwrap().to_string();
    std::fs::write(&path, serde_json::to_string_pretty(&value).unwrap()).unwrap();
    (tmp, path)
}

fn base(extra: serde_json::Value) -> serde_json::Value {
    let mut root = serde_json::json!({
        "agent": { "provider": "default" },
        "providers": { "default": { "api_key": "sk-key", "model": "gpt-4o" } }
    });
    let (root_map, extra_map) = match (&mut root, extra) {
        (serde_json::Value::Object(r), serde_json::Value::Object(e)) => (r, e),
        _ => unreachable!(),
    };
    for (key, value) in extra_map {
        root_map.insert(key, value);
    }
    root
}

// ── Old configs load unchanged ──

#[test]
fn test_config_load_applies_subscription_and_source_cap_defaults_to_legacy_file() {
    // A config written before this feature: no `triggers` array on the server,
    // no `max_active_sources` on the triggers section.
    let (_tmp, path) = config_file(base(serde_json::json!({
        "mcp": { "servers": [{ "name": "gh", "transport": "stdio", "command": "npx" }] },
        "triggers": { "tick_interval_s": 30 }
    })));

    let config = Config::load_resolved_with(&path, &SecretStore::empty()).unwrap();

    assert!(
        config.mcp.servers[0].triggers.is_empty(),
        "a server that declares no subscriptions must load with an empty list"
    );
    assert_eq!(
        config.triggers.max_active_sources, 8,
        "the active-source cap must default to 8"
    );
    assert_eq!(
        config.triggers.tick_interval_s, 30,
        "the surrounding hand-edited values must be untouched"
    );
}

#[test]
fn test_config_load_keeps_custom_max_active_sources() {
    let (_tmp, path) = config_file(base(serde_json::json!({
        "triggers": { "max_active_sources": 25 }
    })));

    let config = Config::load_resolved_with(&path, &SecretStore::empty()).unwrap();
    assert_eq!(config.triggers.max_active_sources, 25);
}

// ── Parsing both subscription kinds through the effective load ──

#[test]
fn test_config_load_parses_notification_and_tool_subscriptions() {
    let (_tmp, path) = config_file(base(serde_json::json!({
        "mcp": {
            "servers": [{
                "name": "gh",
                "transport": "stdio",
                "command": "npx",
                "triggers": [
                    {
                        "type": "notification",
                        "uri": "notifications/tools/list_changed",
                        "description": "Tool list changed",
                        "source_id": "gh-tools-changed"
                    },
                    {
                        "type": "tool",
                        "tool": "list_builds",
                        "args": { "branch": "main" },
                        "description": "Latest CI builds",
                        "source_id": "gh-builds"
                    }
                ]
            }]
        }
    })));

    let config = Config::load_resolved_with(&path, &SecretStore::empty()).unwrap();
    let subs = &config.mcp.servers[0].triggers;
    assert_eq!(subs.len(), 2);

    assert_eq!(subs[0].r#type, "notification");
    assert_eq!(subs[0].uri, "notifications/tools/list_changed");
    assert_eq!(subs[0].source_id, "gh-tools-changed");
    assert_eq!(subs[0].args, None);

    assert_eq!(subs[1].r#type, "tool");
    assert_eq!(subs[1].tool, "list_builds");
    assert_eq!(subs[1].args, Some(serde_json::json!({ "branch": "main" })));
    assert_eq!(subs[1].description, "Latest CI builds");
}

#[test]
fn test_config_load_clamps_subscription_fields() {
    let (_tmp, path) = config_file(base(serde_json::json!({
        "mcp": {
            "servers": [{
                "name": "gh",
                "transport": "stdio",
                "command": "npx",
                "triggers": [{
                    "type": "notification",
                    "uri": "u".repeat(300),
                    "tool": "t".repeat(200),
                    "description": "d".repeat(300),
                    "source_id": "s".repeat(100)
                }]
            }]
        }
    })));

    let config = Config::load_resolved_with(&path, &SecretStore::empty()).unwrap();
    let sub = &config.mcp.servers[0].triggers[0];
    assert_eq!(sub.uri.len(), 256, "uri must honour str_max(256)");
    assert_eq!(sub.tool.len(), 128, "tool must honour str_max(128)");
    assert_eq!(sub.description.len(), 256);
    assert_eq!(sub.source_id.len(), 64, "source_id must honour str_max(64)");
}

#[test]
fn test_config_load_falls_back_for_unknown_subscription_type() {
    let (_tmp, path) = config_file(base(serde_json::json!({
        "mcp": {
            "servers": [{
                "name": "gh",
                "transport": "stdio",
                "command": "npx",
                "triggers": [{
                    "type": "webhook",
                    "uri": "notifications/progress",
                    "source_id": "gh-progress"
                }]
            }]
        }
    })));

    let config = Config::load_resolved_with(&path, &SecretStore::empty()).unwrap();
    assert_eq!(
        config.mcp.servers[0].triggers[0].r#type, "notification",
        "an unknown subscription type must fall back to the declared default"
    );
    assert_eq!(
        config.mcp.servers[0].triggers[0].source_id, "gh-progress",
        "only the offending field falls back"
    );
}

// ── Validation ──

fn parsed(extra: serde_json::Value) -> Config {
    serde_json::from_value(base(extra)).expect("config must parse")
}

#[test]
fn test_config_validate_rejects_notification_subscription_without_uri() {
    let config = parsed(serde_json::json!({
        "mcp": {
            "servers": [{
                "name": "gh",
                "transport": "stdio",
                "command": "npx",
                "triggers": [{ "type": "notification", "uri": "", "source_id": "gh-x" }]
            }]
        }
    }));

    let err = config
        .validate()
        .expect_err("a uri-less notification must fail");
    let message = err.to_string();
    assert!(
        message.starts_with("mcp.servers.gh:") || message.starts_with("mcp.servers.gh."),
        "the error must name the server: {message}"
    );
    assert!(
        message.contains("uri"),
        "the error must name the field: {message}"
    );
}

#[test]
fn test_config_validate_rejects_tool_subscription_without_tool_name() {
    let config = parsed(serde_json::json!({
        "mcp": {
            "servers": [{
                "name": "gh",
                "transport": "stdio",
                "command": "npx",
                "triggers": [{ "type": "tool", "tool": "", "source_id": "gh-builds" }]
            }]
        }
    }));

    let err = config
        .validate()
        .expect_err("a tool-less tool subscription must fail");
    let message = err.to_string();
    assert!(
        message.starts_with("mcp.servers.gh:") || message.starts_with("mcp.servers.gh."),
        "{message}"
    );
    assert!(message.contains("tool"), "{message}");
}

#[test]
fn test_config_validate_rejects_subscription_without_source_id() {
    let config = parsed(serde_json::json!({
        "mcp": {
            "servers": [{
                "name": "gh",
                "transport": "stdio",
                "command": "npx",
                "triggers": [
                    { "type": "notification", "uri": "notifications/x", "source_id": "" },
                    { "type": "tool", "tool": "list_builds", "source_id": "" }
                ]
            }]
        }
    }));

    let err = config
        .validate()
        .expect_err("a source_id-less subscription must fail");
    let message = err.to_string();
    assert!(
        message.starts_with("mcp.servers.gh:") || message.starts_with("mcp.servers.gh."),
        "{message}"
    );
    assert!(message.contains("source_id"), "{message}");
}

#[test]
fn test_config_validate_accepts_complete_subscriptions() {
    let config = parsed(serde_json::json!({
        "mcp": {
            "servers": [{
                "name": "gh",
                "transport": "stdio",
                "command": "npx",
                "triggers": [
                    { "type": "notification", "uri": "notifications/x", "source_id": "gh-x" },
                    { "type": "tool", "tool": "list_builds", "source_id": "gh-builds" }
                ]
            }]
        }
    }));

    assert!(config.validate().is_ok());
}

#[test]
fn test_config_validate_ignores_subscriptions_of_a_disabled_server() {
    // Same stance as a half-filled transport: a toggle-off server is never
    // connected to, so its subscriptions cannot strand a tick either.
    let config = parsed(serde_json::json!({
        "mcp": {
            "servers": [{
                "name": "gh",
                "transport": "stdio",
                "enabled": false,
                "command": "",
                "triggers": [{ "type": "notification", "uri": "", "source_id": "" }]
            }]
        }
    }));

    assert!(config.validate().is_ok());
}
