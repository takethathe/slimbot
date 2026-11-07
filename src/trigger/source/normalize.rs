//! One normalization path for every collector.
//!
//! A collector always returns `{items: [...]}`; the source's `mode` decides
//! what happens next. Conditions therefore never have to know whether they are
//! reading weather, a mailbox or a file listing.
//!
//! Limits are truncation, never failure: a day with unusually much news must
//! not turn a source into a silently dead one. Everything that was cut is
//! reported as a warning so a tick can log it.

use serde_json::Value;

use super::meta::SourceMode;

/// Ceiling for a single item, after which its longest strings are shortened.
pub const ITEM_MAX_BYTES: usize = 4 * 1024;
/// Ceiling for one collection's committed payload.
pub const TOTAL_MAX_BYTES: usize = 64 * 1024;
/// How many trim passes an oversized item gets before it is dropped.
const TRIM_PASSES: usize = 8;

/// One committed data point.
#[derive(Debug, Clone, PartialEq)]
pub struct Entry {
    /// Dedupe key in `append` mode: a script-declared `fingerprint` field, or
    /// a hash of the whole item.
    pub fingerprint: String,
    /// When the item was observed (epoch ms).
    pub at: i64,
    pub item: Value,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Normalized {
    pub entries: Vec<Entry>,
    pub warnings: Vec<String>,
}

impl Normalized {
    pub fn bytes(&self) -> usize {
        self.entries
            .iter()
            .map(|e| serde_json::to_string(&e.item).map(|s| s.len()).unwrap_or(0))
            .sum()
    }
}

/// Turn one collector result into committed entries.
///
/// `existing` is the current window (oldest first) and is only consulted in
/// `append` mode.
pub fn normalize(
    raw: &Value,
    mode: SourceMode,
    max_items: usize,
    existing: &[Entry],
    now: i64,
) -> Result<Normalized, String> {
    let raw_items = raw
        .get("items")
        .and_then(|v| v.as_array())
        .ok_or_else(|| "collector must return {items: [...]}".to_string())?;

    let max_items = max_items.max(1);
    let mut warnings = Vec::new();
    let mut fresh: Vec<Entry> = Vec::with_capacity(raw_items.len());
    let mut not_object = 0usize;
    let mut trimmed = 0usize;
    let mut dropped_oversize = 0usize;

    for raw_item in raw_items {
        if !raw_item.is_object() {
            not_object += 1;
            continue;
        }
        let mut item = raw_item.clone();
        if serialized_len(&item) > ITEM_MAX_BYTES {
            if trim_to_budget(&mut item, ITEM_MAX_BYTES) {
                trimmed += 1;
            } else {
                dropped_oversize += 1;
                continue;
            }
        }
        fresh.push(Entry {
            fingerprint: fingerprint_of(&item),
            at: now,
            item,
        });
    }

    if not_object > 0 {
        warnings.push(format!(
            "skipped {not_object} item(s) that were not objects"
        ));
    }
    if trimmed > 0 {
        warnings.push(format!(
            "shortened {trimmed} item(s) to the {}KB item limit",
            ITEM_MAX_BYTES / 1024
        ));
    }
    if dropped_oversize > 0 {
        warnings.push(format!(
            "dropped {dropped_oversize} item(s) that stayed over the {}KB item limit",
            ITEM_MAX_BYTES / 1024
        ));
    }

    let mut entries = match mode {
        SourceMode::Snapshot => dedupe_newest_first(fresh),
        SourceMode::Append => {
            let mut all = existing.to_vec();
            all.extend(fresh);
            dedupe_newest_first(all)
        }
    };

    if entries.len() > max_items {
        let dropped = entries.len() - max_items;
        // Entries are oldest-first, so the oldest live at the front.
        entries.drain(..dropped);
        warnings.push(format!(
            "dropped {dropped} oldest item(s) beyond the max_items={max_items} limit"
        ));
    }

    // Whole-collection budget: drop from the oldest end, never from the newest
    // data the caller just produced.
    let mut total: usize = entries.iter().map(|e| serialized_len(&e.item)).sum();
    if total > TOTAL_MAX_BYTES {
        let before = entries.len();
        while total > TOTAL_MAX_BYTES && entries.len() > 1 {
            let removed = entries.remove(0);
            total -= serialized_len(&removed.item);
        }
        warnings.push(format!(
            "dropped {} oldest item(s) to stay within the {}KB collection limit",
            before - entries.len(),
            TOTAL_MAX_BYTES / 1024
        ));
    }

    Ok(Normalized { entries, warnings })
}

/// Keep the newest occurrence of each fingerprint and return oldest-first.
fn dedupe_newest_first(entries: Vec<Entry>) -> Vec<Entry> {
    let mut seen = std::collections::HashSet::new();
    let mut kept: Vec<Entry> = Vec::with_capacity(entries.len());
    for entry in entries.into_iter().rev() {
        if seen.insert(entry.fingerprint.clone()) {
            kept.push(entry);
        }
    }
    kept.reverse();
    kept
}

/// Drop entries older than `ttl`. Used on commit and again on every read, so
/// an expired item can never be presented as the current state.
pub fn prune_expired(entries: &mut Vec<Entry>, ttl_ms: i64, now: i64) {
    entries.retain(|e| now.saturating_sub(e.at) <= ttl_ms);
}

fn fingerprint_of(item: &Value) -> String {
    if let Some(declared) = item.get("fingerprint").and_then(|v| v.as_str())
        && !declared.is_empty()
    {
        return declared.to_string();
    }
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    serde_json::to_string(item)
        .unwrap_or_default()
        .hash(&mut hasher);
    format!("{:016x}", hasher.finish())
}

pub(crate) fn serialized_len(value: &Value) -> usize {
    serde_json::to_string(value).map(|s| s.len()).unwrap_or(0)
}

/// Shrink the longest strings until the item fits. Shape and keys survive;
/// only text is cut, which is what "truncate" can honestly mean for JSON.
///
/// Shared with `executor::result`, which applies the same 4KB item budget to an
/// action's `evidence`: the executor transmits external content too, and the
/// evidence boundary does not stop applying because the caller is an action.
pub(crate) fn trim_to_budget(item: &mut Value, budget: usize) -> bool {
    for _ in 0..TRIM_PASSES {
        if serialized_len(item) <= budget {
            return true;
        }
        let longest = longest_string_len(item);
        if longest < 2 {
            return false;
        }
        let target = (longest / 2).max(1);
        shorten_strings_to(item, longest, target);
    }
    serialized_len(item) <= budget
}

fn longest_string_len(value: &Value) -> usize {
    match value {
        Value::String(text) => text.chars().count(),
        Value::Array(items) => items.iter().map(longest_string_len).max().unwrap_or(0),
        Value::Object(map) => map.values().map(longest_string_len).max().unwrap_or(0),
        _ => 0,
    }
}

fn shorten_strings_to(value: &mut Value, from: usize, to: usize) {
    match value {
        Value::String(text) => {
            if text.chars().count() == from {
                let mut kept: String = text.chars().take(to).collect();
                kept.push('…');
                *text = kept;
            }
        }
        Value::Array(items) => {
            for item in items {
                shorten_strings_to(item, from, to);
            }
        }
        Value::Object(map) => {
            for item in map.values_mut() {
                shorten_strings_to(item, from, to);
            }
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn normalize_raw(raw: Value, mode: SourceMode, max_items: usize) -> Normalized {
        normalize(&raw, mode, max_items, &[], 1_000).expect("valid collector output")
    }

    #[test]
    fn test_missing_items_field_is_an_error() {
        let err = normalize(&json!({"rows": []}), SourceMode::Snapshot, 10, &[], 0).unwrap_err();
        assert!(err.contains("items"));
    }

    #[test]
    fn test_snapshot_replaces_previous_items() {
        let existing = vec![Entry {
            fingerprint: "old".into(),
            at: 1,
            item: json!({"v": 1}),
        }];
        let out = normalize(
            &json!({"items": [{"v": 2}]}),
            SourceMode::Snapshot,
            10,
            &existing,
            2_000,
        )
        .unwrap();
        assert_eq!(out.entries.len(), 1);
        assert_eq!(out.entries[0].item["v"], 2);
        assert_eq!(out.entries[0].at, 2_000);
    }

    #[test]
    fn test_append_dedupes_by_fingerprint() {
        let existing = vec![Entry {
            fingerprint: "a".into(),
            at: 1,
            item: json!({"fingerprint": "a", "v": 1}),
        }];
        let out = normalize(
            &json!({"items": [
                {"fingerprint": "a", "v": 2},
                {"fingerprint": "b", "v": 3}
            ]}),
            SourceMode::Append,
            10,
            &existing,
            2_000,
        )
        .unwrap();
        assert_eq!(out.entries.len(), 2);
        // The newest occurrence of "a" wins and keeps ordering oldest-first.
        assert_eq!(out.entries[0].item["v"], 2);
        assert_eq!(out.entries[1].item["v"], 3);
    }

    #[test]
    fn test_append_falls_back_to_content_hash() {
        let out = normalize_raw(
            json!({"items": [{"v": 1}, {"v": 1}, {"v": 2}]}),
            SourceMode::Append,
            10,
        );
        assert_eq!(out.entries.len(), 2);
    }

    #[test]
    fn test_max_items_drops_the_oldest() {
        let out = normalize_raw(
            json!({"items": [{"i": 1}, {"i": 2}, {"i": 3}]}),
            SourceMode::Snapshot,
            2,
        );
        assert_eq!(out.entries.len(), 2);
        assert_eq!(out.entries[0].item["i"], 2);
        assert_eq!(out.entries[1].item["i"], 3);
        assert!(out.warnings.iter().any(|w| w.contains("max_items=2")));
    }

    #[test]
    fn test_oversized_item_is_shortened_not_failed() {
        let big = "x".repeat(10_000);
        let out = normalize_raw(
            json!({"items": [{"title": "kept", "body": big}]}),
            SourceMode::Snapshot,
            10,
        );
        assert_eq!(out.entries.len(), 1);
        assert_eq!(out.entries[0].item["title"], "kept");
        assert!(serialized_len(&out.entries[0].item) <= ITEM_MAX_BYTES);
        assert!(out.warnings.iter().any(|w| w.contains("4KB")));
    }

    #[test]
    fn test_total_budget_drops_oldest() {
        let items: Vec<Value> = (0..80)
            .map(|i| json!({"i": i, "body": "y".repeat(3_000)}))
            .collect();
        let out = normalize_raw(json!({"items": items}), SourceMode::Append, 200);
        assert!(out.bytes() <= TOTAL_MAX_BYTES);
        assert!(!out.entries.is_empty());
        // The newest item survived the trimming.
        let last = out.entries.last().unwrap();
        assert_eq!(last.item["i"], 79);
        assert!(out.warnings.iter().any(|w| w.contains("64KB")));
    }

    #[test]
    fn test_non_object_items_are_skipped_with_a_warning() {
        let out = normalize_raw(
            json!({"items": [1, "two", {"ok": true}]}),
            SourceMode::Snapshot,
            10,
        );
        assert_eq!(out.entries.len(), 1);
        assert!(out.warnings.iter().any(|w| w.contains("2 item(s)")));
    }

    #[test]
    fn test_prune_expired_drops_only_stale_entries() {
        let mut entries = vec![
            Entry {
                fingerprint: "old".into(),
                at: 1_000,
                item: json!({}),
            },
            Entry {
                fingerprint: "new".into(),
                at: 9_000,
                item: json!({}),
            },
        ];
        prune_expired(&mut entries, 5_000, 10_000);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].fingerprint, "new");
    }
}
