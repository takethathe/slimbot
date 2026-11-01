//! One-shot config migrations for breaking config changes.
//!
//! Both migrations below move or drop a key that used to control context
//! pressure. Skipping them would silently reset limits to defaults, so they run
//! on every load and each one returns a human-readable note. The note fires
//! only while the legacy key is still in the file: the rewritten config no
//! longer carries it, so it is a genuinely one-time message.
//!
//! See `docs/config.md` ("迁移说明") and `docs/adr/0013`.

use serde_json::Value;

/// Prefix of every migration note, so the log line is unmistakable.
pub const MIGRATION_NOTE_PREFIX: &str = "[config migration]";

/// Apply every legacy-key migration to a raw config document.
///
/// Returns the notes describing what was moved or dropped; an empty vec means
/// the config was already in the new shape.
pub fn migrate_legacy_keys(raw: &mut Value) -> Vec<String> {
    let mut notes = Vec::new();
    let Some(root) = raw.as_object_mut() else {
        return notes;
    };

    let provider_name = root
        .get("agent")
        .and_then(|agent| agent.get("provider"))
        .and_then(|provider| provider.as_str())
        .filter(|name| !name.is_empty())
        .unwrap_or("default")
        .to_string();

    // (a) `context_window_tokens` moved from `agent` to the provider section:
    // the window is a property of the model, not of the agent's policy.
    let legacy_window = root
        .get_mut("agent")
        .and_then(|agent| agent.as_object_mut())
        .and_then(|agent| agent.remove("context_window_tokens"));

    if let Some(legacy) = legacy_window {
        if !legacy.is_number() {
            notes.push(format!(
                "{MIGRATION_NOTE_PREFIX} agent.context_window_tokens was not a number ({legacy}); dropped, \
                 falling back to providers.{provider_name}.context_window_tokens"
            ));
        } else {
            let providers = root
                .entry("providers")
                .or_insert_with(|| Value::Object(Default::default()));
            let provider = providers.as_object_mut().map(|providers| {
                providers
                    .entry(provider_name.clone())
                    .or_insert_with(|| Value::Object(Default::default()))
            });
            if let Some(provider) = provider.and_then(|provider| provider.as_object_mut()) {
                if provider.contains_key("context_window_tokens") {
                    notes.push(format!(
                        "{MIGRATION_NOTE_PREFIX} agent.context_window_tokens is deprecated; \
                         providers.{provider_name}.context_window_tokens is already set, so the \
                         agent value ({legacy}) was dropped"
                    ));
                } else {
                    provider.insert("context_window_tokens".to_string(), legacy.clone());
                    notes.push(format!(
                        "{MIGRATION_NOTE_PREFIX} agent.context_window_tokens moved to \
                         providers.{provider_name}.context_window_tokens (value kept: {legacy})"
                    ));
                }
            }
        }
    }

    // (b) `max_tool_result_chars` is deprecated: it carried two meanings at
    // once (persist threshold and truncation length). Limits now derive from
    // the window anchor and the `pressure.*` ratios.
    if let Some(agent) = root.get_mut("agent").and_then(|a| a.as_object_mut())
        && let Some(legacy) = agent.remove("max_tool_result_chars")
    {
        notes.push(format!(
            "{MIGRATION_NOTE_PREFIX} agent.max_tool_result_chars is deprecated (value {legacy} ignored); \
             tool-result limits now derive from providers.{provider_name}.context_window_tokens × \
             pressure.single_tool_result_ratio / pressure.persisted_preview_ratio"
        ));
    }

    notes
}

/// Emit one migration note.
///
/// Uses the logger when it is already active, and falls back to stderr so an
/// upgrade performed before logging is initialised still shows the note.
pub fn emit(note: &str) {
    if crate::log::should_log(crate::log::LogLevel::Warning) {
        crate::warn_log!("{}", note);
    } else {
        eprintln!("{note}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn test_migrate_agent_window_moves_to_provider() {
        let mut raw = json!({
            "agent": { "provider": "main", "context_window_tokens": 64000 },
            "providers": { "main": { "model": "gpt-4o" } }
        });
        let notes = migrate_legacy_keys(&mut raw);

        assert_eq!(
            raw["providers"]["main"]["context_window_tokens"],
            json!(64_000)
        );
        assert!(raw["agent"].get("context_window_tokens").is_none());
        assert_eq!(notes.len(), 1);
        assert!(notes[0].contains(MIGRATION_NOTE_PREFIX));
    }

    #[test]
    fn test_migrate_agent_window_note_mentions_old_and_new_key() {
        let mut raw = json!({
            "agent": { "provider": "main", "context_window_tokens": 64000 },
            "providers": { "main": {} }
        });
        let notes = migrate_legacy_keys(&mut raw);
        assert!(notes[0].contains("agent.context_window_tokens"));
        assert!(notes[0].contains("providers.main.context_window_tokens"));
    }

    #[test]
    fn test_migrate_agent_window_keeps_existing_provider_value() {
        let mut raw = json!({
            "agent": { "provider": "main", "context_window_tokens": 64000 },
            "providers": { "main": { "context_window_tokens": 128000 } }
        });
        let notes = migrate_legacy_keys(&mut raw);
        assert_eq!(
            raw["providers"]["main"]["context_window_tokens"],
            json!(128_000)
        );
        assert!(notes[0].contains("already set"));
    }

    #[test]
    fn test_migrate_agent_window_defaults_to_default_provider() {
        // No agent.provider: the migrated value lands on the implicit provider.
        let mut raw = json!({
            "agent": { "context_window_tokens": 32000 },
            "providers": { "default": {} }
        });
        migrate_legacy_keys(&mut raw);
        assert_eq!(
            raw["providers"]["default"]["context_window_tokens"],
            json!(32_000)
        );
    }

    #[test]
    fn test_migrate_agent_window_creates_missing_provider_entry() {
        let mut raw = json!({
            "agent": { "provider": "main", "context_window_tokens": 32000 },
            "providers": {}
        });
        migrate_legacy_keys(&mut raw);
        assert_eq!(
            raw["providers"]["main"]["context_window_tokens"],
            json!(32_000)
        );
    }

    #[test]
    fn test_migrate_max_tool_result_chars_dropped_with_note() {
        let mut raw = json!({ "agent": { "max_tool_result_chars": 8000 } });
        let notes = migrate_legacy_keys(&mut raw);
        assert!(raw["agent"].get("max_tool_result_chars").is_none());
        assert_eq!(notes.len(), 1);
        assert!(notes[0].contains("agent.max_tool_result_chars"));
        assert!(notes[0].contains("pressure."));
    }

    #[test]
    fn test_migrate_both_keys_at_once_yields_two_notes() {
        let mut raw = json!({
            "agent": { "provider": "main", "context_window_tokens": 64000, "max_tool_result_chars": 8000 },
            "providers": { "main": {} }
        });
        let notes = migrate_legacy_keys(&mut raw);
        assert_eq!(notes.len(), 2);
    }

    #[test]
    fn test_migrate_already_migrated_config_is_noop() {
        let mut raw = json!({
            "agent": { "provider": "main" },
            "providers": { "main": { "context_window_tokens": 64000 } }
        });
        let before = raw.clone();
        let notes = migrate_legacy_keys(&mut raw);
        assert!(notes.is_empty());
        assert_eq!(raw, before);
    }

    #[test]
    fn test_migrate_non_numeric_window_is_dropped_with_note() {
        let mut raw = json!({
            "agent": { "provider": "main", "context_window_tokens": "64k" },
            "providers": { "main": {} }
        });
        let notes = migrate_legacy_keys(&mut raw);
        assert!(raw["agent"].get("context_window_tokens").is_none());
        assert!(
            raw["providers"]["main"]
                .get("context_window_tokens")
                .is_none()
        );
        assert!(notes[0].contains("not a number"));
    }
}
