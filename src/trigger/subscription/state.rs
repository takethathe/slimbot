//! Runtime state for one rule: `state.json`, written by the host and no one else.

use std::collections::BTreeMap;
use std::path::Path;

use anyhow::Result;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::meta::STATE_FILE;

/// Bumped when the shape of `state.json` changes incompatibly.
pub const STATE_VERSION: u32 = 1;
/// Ceiling for the fired-key ledger; older entries are evicted first.
pub const FIRED_KEYS_MAX: usize = 1000;
/// A fired key nobody has seen in a week is dropped.
pub const FIRED_KEY_TTL_MS: i64 = 7 * 24 * 60 * 60 * 1000;
/// Ceiling for the script-private store.
pub const STORE_MAX_BYTES: usize = 16 * 1024;

/// One entry of the fired-key ledger.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FiredKey {
    pub key: String,
    pub at: i64,
}

/// Everything the host remembers about one rule.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TriggerState {
    #[serde(default)]
    pub version: u32,
    /// Last judged result, the edge the next round compares against.
    #[serde(default)]
    pub prev: bool,
    #[serde(default)]
    pub next_run_at: Option<i64>,
    #[serde(default)]
    pub last_fired_at: Option<i64>,
    #[serde(default)]
    pub fire_count: u64,
    #[serde(default)]
    pub consecutive_failures: u32,
    #[serde(default)]
    pub last_error: Option<String>,
    /// Alert dedupe: one alert per failure streak, not one per tick.
    #[serde(default)]
    pub alerted: bool,
    #[serde(default)]
    pub fired_keys: Vec<FiredKey>,
    #[serde(default)]
    pub store: BTreeMap<String, Value>,
}

impl Default for TriggerState {
    fn default() -> Self {
        Self {
            version: STATE_VERSION,
            prev: false,
            next_run_at: None,
            last_fired_at: None,
            fire_count: 0,
            consecutive_failures: 0,
            last_error: None,
            alerted: false,
            fired_keys: Vec::new(),
            store: BTreeMap::new(),
        }
    }
}

impl TriggerState {
    /// Read a rule's state. A missing file is a fresh rule; an unreadable or
    /// wrong-version file is **reset**, because guessing which half of a torn
    /// state to trust is how a rule starts re-firing things it already told
    /// the user about. The returned note is the warning the caller should log.
    pub fn load(dir: &Path) -> (Self, Option<String>) {
        let path = dir.join(STATE_FILE);
        let Ok(text) = std::fs::read_to_string(&path) else {
            return (Self::default(), None);
        };
        match serde_json::from_str::<TriggerState>(&text) {
            Ok(state) if state.version == STATE_VERSION => {
                let mut state = state;
                state.prune_fired_keys(crate::trigger::source::meta::now_ms());
                (state, None)
            }
            Ok(_) => (
                Self::default(),
                Some(format!(
                    "state.json has version {}, expected {}; state was reset",
                    {
                        serde_json::from_str::<serde_json::Value>(&text)
                            .ok()
                            .and_then(|v| v.get("version").and_then(|x| x.as_u64()))
                            .unwrap_or(0)
                    },
                    STATE_VERSION
                )),
            ),
            Err(e) => (
                Self::default(),
                Some(format!("state.json is damaged and was reset: {e}")),
            ),
        }
    }

    /// Atomic replace, tmp + rename: a tick reading a half-written state would
    /// either double-fire or swallow a fire.
    pub fn save(&self, path: &Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let text = serde_json::to_string_pretty(self)?;
        crate::trigger::source::meta::write_atomic(path, text.as_bytes())
    }

    /// Record what a fire consumed. Newest-first eviction: the keys that keep
    /// being refreshed are the keys that matter.
    pub fn touch_fired_keys(&mut self, keys: &[String], now_ms: i64) {
        for key in keys {
            let trimmed = key.trim();
            if trimmed.is_empty() {
                continue;
            }
            if let Some(existing) = self.fired_keys.iter_mut().find(|f| f.key == trimmed) {
                existing.at = now_ms;
                continue;
            }
            self.fired_keys.push(FiredKey {
                key: trimmed.to_string(),
                at: now_ms,
            });
        }
        self.prune_fired_keys(now_ms);
    }

    /// Drop expired entries, then evict the least recently used until the cap.
    pub fn prune_fired_keys(&mut self, now_ms: i64) {
        self.fired_keys
            .retain(|entry| now_ms.saturating_sub(entry.at) <= FIRED_KEY_TTL_MS);
        if self.fired_keys.len() <= FIRED_KEYS_MAX {
            return;
        }
        self.fired_keys.sort_by_key(|entry| entry.at);
        let drop = self.fired_keys.len() - FIRED_KEYS_MAX;
        self.fired_keys.drain(..drop);
    }

    pub fn fired_key_list(&self) -> Vec<String> {
        self.fired_keys
            .iter()
            .map(|entry| entry.key.clone())
            .collect()
    }

    pub fn has_fired_key(&self, key: &str) -> bool {
        self.fired_keys.iter().any(|entry| entry.key == key)
    }

    /// Serialized size of the private store, as the 16KB ceiling sees it.
    pub fn store_bytes(&self) -> usize {
        serde_json::to_string(&self.store)
            .map(|s| s.len())
            .unwrap_or(0)
    }

    /// Whether `key = value` still fits under the store ceiling. Reported to the
    /// script as a failure so it can log and move on instead of silently losing
    /// the write on the next commit.
    pub fn store_would_fit(&self, key: &str, value: &Value) -> bool {
        let mut probe = self.store.clone();
        probe.insert(key.to_string(), value.clone());
        serde_json::to_string(&probe)
            .map(|s| s.len())
            .unwrap_or(usize::MAX)
            <= STORE_MAX_BYTES
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn test_missing_state_is_a_fresh_rule() {
        let tmp = tempfile::tempdir().unwrap();
        let (state, note) = TriggerState::load(tmp.path());
        assert_eq!(state, TriggerState::default());
        assert!(note.is_none());
    }

    #[test]
    fn test_round_trip_keeps_the_edge_and_the_store() {
        let tmp = tempfile::tempdir().unwrap();
        let mut state = TriggerState {
            prev: true,
            fire_count: 3,
            ..Default::default()
        };
        state.store.insert("cursor".into(), json!("abc"));
        state.save(&tmp.path().join(STATE_FILE)).unwrap();

        let (reloaded, note) = TriggerState::load(tmp.path());
        assert!(note.is_none());
        assert!(reloaded.prev);
        assert_eq!(reloaded.fire_count, 3);
        assert_eq!(reloaded.store["cursor"], json!("abc"));
    }

    #[test]
    fn test_damaged_state_is_reset_with_a_warning_not_guessed() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join(STATE_FILE), "{ not json").unwrap();
        let (state, note) = TriggerState::load(tmp.path());
        assert_eq!(state, TriggerState::default());
        assert!(note.unwrap().contains("damaged"));
    }

    #[test]
    fn test_wrong_version_resets_and_says_so() {
        let tmp = tempfile::tempdir().unwrap();
        let text = serde_json::to_string(&json!({"version": 99, "prev": true})).unwrap();
        std::fs::write(tmp.path().join(STATE_FILE), text).unwrap();
        let (state, note) = TriggerState::load(tmp.path());
        assert!(!state.prev);
        let note = note.expect("version mismatch must be reported");
        assert!(note.contains("version"), "{note}");
    }

    #[test]
    fn test_fired_keys_are_capped_and_evict_the_oldest() {
        let mut state = TriggerState::default();
        for i in 0..(FIRED_KEYS_MAX + 50) {
            state.touch_fired_keys(&[format!("k{i}")], 1_000 + i as i64);
        }
        assert_eq!(state.fired_keys.len(), FIRED_KEYS_MAX);
        assert!(state.has_fired_key("k1049"));
        assert!(!state.has_fired_key("k0"));

        // Refreshing an existing key moves it forward instead of growing the set.
        let before = state.fired_keys.len();
        state.touch_fired_keys(&["k1049".to_string()], 5_000);
        assert_eq!(state.fired_keys.len(), before);
    }

    #[test]
    fn test_fired_keys_past_their_week_are_dropped() {
        let mut state = TriggerState::default();
        state.touch_fired_keys(&["old".to_string()], 0);
        state.prune_fired_keys(FIRED_KEY_TTL_MS + 1);
        assert!(state.fired_keys.is_empty());
    }

    #[test]
    fn test_store_ceiling_is_measured_on_the_merged_map() {
        let mut state = TriggerState::default();
        let big = "x".repeat(8 * 1024);
        assert!(state.store_would_fit("a", &json!(big)));
        state.store.insert("a".into(), json!(big));
        // Second write of the same size crosses 16KB and must be refused.
        assert!(!state.store_would_fit("b", &json!(big)));
        assert!(state.store_bytes() <= STORE_MAX_BYTES);
    }

    #[test]
    fn test_state_file_has_no_tmp_leftovers_after_save() {
        let tmp = tempfile::tempdir().unwrap();
        let state = TriggerState::default();
        state.save(&tmp.path().join(STATE_FILE)).unwrap();
        let leftovers: Vec<_> = std::fs::read_dir(tmp.path())
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().ends_with(".tmp"))
            .collect();
        assert!(leftovers.is_empty(), "{leftovers:?}");
    }
}
