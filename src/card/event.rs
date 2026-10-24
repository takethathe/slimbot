//! Card lifecycle events.
//!
//! A card generation run emits a small sequence of events that travel over a
//! dedicated mpsc (never the `AgentEvent` broadcast, which silently drops on
//! lag and would bother unrelated channels). The `ChannelManager` routes each
//! event to the channel named by its `session_id` prefix; channels that cannot
//! render cards inherit the default no-op `write_card` and simply drop them.
//!
//! A generation emits `Started`, then a `Delta` per batch of assembled ops,
//! then the authoritative `Completed`. Dropped deltas only cost smoothness —
//! the final spec is always complete — and an empty or failed generation
//! retracts with `Failed`.

use serde::Serialize;
use serde_json::Value;

/// One card event for a specific Run, addressed by `session_id`
/// (`"{channel}:{chat}"`) so the manager can route by channel prefix.
#[derive(Clone, Debug, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum CardEvent {
    /// Generation kicked off (frontend may show a placeholder).
    Started { session_id: String, run_id: String },
    /// Incremental patch operations applied so far (streaming).
    Delta {
        session_id: String,
        run_id: String,
        ops: Vec<Value>,
    },
    /// Authoritative full spec; the frontend renders this as final.
    Completed {
        session_id: String,
        run_id: String,
        spec: Value,
    },
    /// Generation failed or produced an empty spec (frontend ignores).
    Failed { session_id: String, run_id: String },
}

impl CardEvent {
    /// The session this event belongs to (`"{channel}:{chat}"`).
    pub fn session_id(&self) -> &str {
        match self {
            CardEvent::Started { session_id, .. }
            | CardEvent::Delta { session_id, .. }
            | CardEvent::Completed { session_id, .. }
            | CardEvent::Failed { session_id, .. } => session_id,
        }
    }

    /// The Run this event is attributed to.
    pub fn run_id(&self) -> &str {
        match self {
            CardEvent::Started { run_id, .. }
            | CardEvent::Delta { run_id, .. }
            | CardEvent::Completed { run_id, .. }
            | CardEvent::Failed { run_id, .. } => run_id,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn test_card_event_accessors() {
        let e = CardEvent::Completed {
            session_id: "webui:c1".to_string(),
            run_id: "r1".to_string(),
            spec: json!({"root": "x"}),
        };
        assert_eq!(e.session_id(), "webui:c1");
        assert_eq!(e.run_id(), "r1");
    }

    #[test]
    fn test_card_event_serializes_with_type_tag() {
        let e = CardEvent::Completed {
            session_id: "webui:c1".to_string(),
            run_id: "r1".to_string(),
            spec: json!({"root": "x", "elements": {"x": {"type": "Card"}}}),
        };
        let v = serde_json::to_value(&e).unwrap();
        assert_eq!(v["type"], json!("completed"));
        assert_eq!(v["run_id"], json!("r1"));
        assert_eq!(v["spec"]["root"], json!("x"));
    }
}
