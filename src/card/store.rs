//! Card sidecar store.
//!
//! Completed Card specs are appended to a per-chat sidecar file
//! (`{session_dir}/{chat_id}.cards.jsonl`), one record per line. Cards are
//! deliberately kept out of the session JSONL so patch text never enters the
//! agent context or consolidation summaries. Each record is anchored to the
//! timestamp of the Run's last assistant message so it can be reattached on
//! replay; the anchor survives consolidation's full rewrite of the session
//! file (message indices do not).
//!
//! Writes happen only on `Completed`. Reads skip corrupt lines rather than
//! failing the whole history load.

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// One appended card record. `ts` is the anchor (last assistant message's
/// timestamp); `created_at_ms` is wall-clock generation time.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CardRecord {
    pub ts: String,
    pub created_at_ms: i64,
    pub spec: Value,
}

/// Append-only card sidecar reader/writer, rooted at a session directory.
pub struct CardStore {
    session_dir: PathBuf,
}

impl CardStore {
    pub fn new(session_dir: impl Into<PathBuf>) -> Self {
        Self {
            session_dir: session_dir.into(),
        }
    }

    /// Resolve the sidecar path for a chat, sanitizing the chat id so it is a
    /// safe single-path-component file name (guards against traversal).
    fn path_for(&self, chat_id: &str) -> PathBuf {
        sidecar_path(&self.session_dir, chat_id)
    }

    /// Append one record as a JSON line. Creates the parent directory if needed.
    pub fn append(&self, chat_id: &str, record: &CardRecord) -> Result<()> {
        let path = self.path_for(chat_id);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("create card dir {}", parent.display()))?;
        }
        let line = serde_json::to_string(record).context("serialize card record")?;
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .with_context(|| format!("open card sidecar {}", path.display()))?;
        writeln!(file, "{line}").with_context(|| format!("append card {}", path.display()))?;
        Ok(())
    }

    /// Load all records for a chat. A missing file yields an empty vec; corrupt
    /// or blank lines are skipped without failing the rest.
    pub fn load(&self, chat_id: &str) -> Vec<CardRecord> {
        let path = self.path_for(chat_id);
        let Ok(content) = fs::read_to_string(&path) else {
            return Vec::new();
        };
        content
            .lines()
            .filter(|l| !l.trim().is_empty())
            .filter_map(|l| serde_json::from_str::<CardRecord>(l).ok())
            .collect()
    }

    /// Find the spec whose anchor `ts` equals the given message timestamp.
    /// Exact match only — no guessing by index.
    pub fn find_by_ts(&self, chat_id: &str, ts: &str) -> Option<Value> {
        if ts.is_empty() {
            return None;
        }
        self.load(chat_id)
            .into_iter()
            .find(|r| r.ts == ts)
            .map(|r| r.spec)
    }
}

/// Keep only path-safe characters, replacing the rest with `_`. Empty input maps
/// to `_` so the file name is never just the extension.
fn sanitize(chat_id: &str) -> String {
    let out: String = chat_id
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.' {
                c
            } else {
                '_'
            }
        })
        .collect();
    if out.is_empty() { "_".to_string() } else { out }
}

/// Path to a chat's sidecar file — the shape behind `CardStore::path_for`, kept
/// private so there is a single place spelling `{chat_id}.cards.jsonl`.
fn sidecar_path(session_dir: &Path, chat_id: &str) -> PathBuf {
    session_dir.join(format!("{}.cards.jsonl", sanitize(chat_id)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn record(ts: &str) -> CardRecord {
        CardRecord {
            ts: ts.to_string(),
            created_at_ms: 123,
            spec: json!({"root": "r", "elements": {"r": {"type": "Card"}}}),
        }
    }

    #[test]
    fn test_store_appends_and_loads_records() {
        let dir = tempfile::tempdir().unwrap();
        let store = CardStore::new(dir.path());
        store.append("c1", &record("t1")).unwrap();
        store.append("c1", &record("t2")).unwrap();
        let recs = store.load("c1");
        assert_eq!(recs.len(), 2);
        assert_eq!(recs[0].ts, "t1");
        assert_eq!(recs[1].ts, "t2");
    }

    #[test]
    fn test_store_skips_corrupt_lines() {
        let dir = tempfile::tempdir().unwrap();
        let store = CardStore::new(dir.path());
        store.append("c1", &record("good1")).unwrap();
        // Hand-append a corrupt line and a blank line.
        let path = store.path_for("c1");
        fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .and_then(|mut f| {
                f.write_all(b"{ not json }\n")?;
                f.write_all(b"\n")?;
                Ok(())
            })
            .unwrap();
        store.append("c1", &record("good2")).unwrap();
        let recs = store.load("c1");
        assert_eq!(recs.len(), 2, "corrupt + blank lines skipped");
        assert_eq!(recs[0].ts, "good1");
        assert_eq!(recs[1].ts, "good2");
    }

    #[test]
    fn test_store_missing_file_returns_empty() {
        let dir = tempfile::tempdir().unwrap();
        let store = CardStore::new(dir.path());
        assert!(store.load("nonexistent").is_empty());
        assert!(store.find_by_ts("nonexistent", "t").is_none());
    }

    #[test]
    fn test_store_find_by_ts_exact_match() {
        let dir = tempfile::tempdir().unwrap();
        let store = CardStore::new(dir.path());
        store.append("c1", &record("t1")).unwrap();
        store.append("c1", &record("t2")).unwrap();
        assert!(store.find_by_ts("c1", "t2").is_some());
        assert!(store.find_by_ts("c1", "nope").is_none());
        assert!(
            store.find_by_ts("c1", "").is_none(),
            "empty ts never matches"
        );
    }

    #[test]
    fn test_sanitize_blocks_path_traversal() {
        assert_eq!(sanitize("../../etc/passwd"), ".._.._etc_passwd");
        assert_eq!(sanitize("ok-chat_1.2"), "ok-chat_1.2");
    }
}
