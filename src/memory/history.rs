use std::io::Write;
use std::path::PathBuf;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use super::fs::sync_if_exists;

/// History entry in history.jsonl.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HistoryEntry {
    pub timestamp: String,
    pub content: String,
}

/// Episodic history subsystem: append-only JSONL + Dream processing cursor.
///
/// Deliberately separate from the [`MemoryBackend`](super::backend::MemoryBackend)
/// trait: append-only logs with a processing cursor have little in common with
/// category document reads/writes, and forcing them into one interface would
/// complicate both. Kept as a concrete store inside the memory module.
pub struct HistoryStore {
    history_file: PathBuf,
    dream_cursor_file: PathBuf,
    // In-memory cache for dream cursor, lazily initialized.
    dream_cursor_cache: Option<usize>,
}

impl HistoryStore {
    pub fn new(history_file: PathBuf, dream_cursor_file: PathBuf) -> Self {
        Self {
            history_file,
            dream_cursor_file,
            dream_cursor_cache: None,
        }
    }

    /// Append an entry to history.jsonl.
    pub fn append_history(&mut self, entry: &str) -> Result<()> {
        let now = chrono::Local::now().format("%Y-%m-%d %H:%M");
        let record = HistoryEntry {
            timestamp: now.to_string(),
            content: entry.to_string(),
        };
        let line = serde_json::to_string(&record).context("Failed to serialize history entry")?;
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.history_file)
            .context("Failed to open history file")?;
        writeln!(file, "{}", line).context("Failed to write history entry")?;
        Ok(())
    }

    /// Read history entries, skipping the first `skip_lines` entries.
    pub fn read_unprocessed_history(&self, skip_lines: usize) -> Vec<HistoryEntry> {
        self.read_entries().into_iter().skip(skip_lines).collect()
    }

    /// Read all entries from history.jsonl, capped to `max_entries` if > 0.
    pub fn read_recent_history(&self, max_entries: usize) -> Vec<HistoryEntry> {
        let entries = self.read_entries();
        if max_entries > 0 && entries.len() > max_entries {
            entries[entries.len() - max_entries..].to_vec()
        } else {
            entries
        }
    }

    pub fn get_last_dream_cursor(&mut self) -> usize {
        if let Some(cached) = self.dream_cursor_cache {
            return cached;
        }
        if let Ok(text) = std::fs::read_to_string(&self.dream_cursor_file)
            && let Ok(val) = text.trim().parse::<usize>()
        {
            self.dream_cursor_cache = Some(val);
            return val;
        }
        // No dream cursor file exists, start from 0
        self.dream_cursor_cache = Some(0);
        0
    }

    pub fn set_last_dream_cursor(&mut self, cursor: usize) -> Result<()> {
        self.dream_cursor_cache = Some(cursor);
        std::fs::write(&self.dream_cursor_file, cursor.to_string())
            .context("Failed to write dream cursor")
    }

    /// Read all entries from history.jsonl.
    pub fn read_entries(&self) -> Vec<HistoryEntry> {
        let mut entries = Vec::new();
        if let Ok(file) = std::fs::File::open(&self.history_file) {
            let reader = std::io::BufReader::new(file);
            for line in std::io::BufRead::lines(reader).map_while(Result::ok) {
                let trimmed = line.trim();
                if trimmed.is_empty() {
                    continue;
                }
                if let Ok(entry) = serde_json::from_str::<HistoryEntry>(trimmed) {
                    entries.push(entry);
                }
            }
        }
        entries
    }

    /// Minimum number of recent entries to keep during compaction.
    const DREAM_KEEP_MINIMUM: usize = 50;

    /// Maximum number of history entries before triggering compaction.
    const HISTORY_MAX_ENTRIES: usize = 2000;

    /// Write history entries to file atomically via temp file.
    fn write_entries(&self, entries: Vec<HistoryEntry>) -> Result<()> {
        let tmp_file = self.history_file.with_extension("jsonl.tmp");
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(&tmp_file)
            .context("Failed to open temp file")?;

        for entry in entries {
            let line =
                serde_json::to_string(&entry).context("Failed to serialize history entry")?;
            writeln!(file, "{}", line).context("Failed to write history entry")?;
        }

        file.sync_all().context("Failed to sync temp file")?;
        std::fs::rename(&tmp_file, &self.history_file)
            .context("Failed to rename temp file to history file")?;
        Ok(())
    }

    /// Compact history entries after Dream processing.
    /// Only compacts when history exceeds HISTORY_MAX_ENTRIES.
    /// Keeps the most recent DREAM_KEEP_MINIMUM entries.
    /// Adjusts dream_cursor after compaction based on deleted line count.
    pub fn compact_history_after_dream(&mut self) -> Result<()> {
        let entries = self.read_entries();

        if entries.is_empty() {
            return Ok(());
        }

        // Only compact when history exceeds the limit
        if entries.len() < Self::HISTORY_MAX_ENTRIES {
            return Ok(());
        }

        // Keep only the most recent DREAM_KEEP_MINIMUM entries
        let keep_start = entries.len().saturating_sub(Self::DREAM_KEEP_MINIMUM);
        let deleted_count = keep_start;
        let filtered: Vec<HistoryEntry> = entries.into_iter().skip(keep_start).collect();

        // Adjust dream_cursor: subtract the number of deleted lines
        let old_cursor = self.get_last_dream_cursor();
        let new_cursor = old_cursor.saturating_sub(deleted_count);
        self.set_last_dream_cursor(new_cursor)?;

        self.write_entries(filtered)
    }

    /// fsync history and cursor files if they exist.
    pub fn sync(&self) -> Result<()> {
        sync_if_exists(&self.history_file, true)?;
        sync_if_exists(&self.dream_cursor_file, false)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_store(tmp_dir: &std::path::Path) -> HistoryStore {
        HistoryStore::new(tmp_dir.join("history.jsonl"), tmp_dir.join(".dream_cursor"))
    }

    #[test]
    fn test_history_append_and_read() {
        let tmp = tempfile::tempdir().unwrap();
        let mut store = make_store(tmp.path());

        store.append_history("first entry").unwrap();
        store.append_history("second entry").unwrap();

        let entries = store.read_entries();
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].content, "first entry");
        assert_eq!(entries[1].content, "second entry");
    }

    #[test]
    fn test_read_unprocessed_history() {
        let tmp = tempfile::tempdir().unwrap();
        let mut store = make_store(tmp.path());

        store.append_history("entry 1").unwrap();
        store.append_history("entry 2").unwrap();
        store.append_history("entry 3").unwrap();

        let unprocessed = store.read_unprocessed_history(1);
        assert_eq!(unprocessed.len(), 2);
        assert_eq!(unprocessed[0].content, "entry 2");
        assert_eq!(unprocessed[1].content, "entry 3");
    }

    #[test]
    fn test_read_recent_history_cap() {
        let tmp = tempfile::tempdir().unwrap();
        let mut store = make_store(tmp.path());

        for i in 1..=10 {
            store.append_history(&format!("entry {}", i)).unwrap();
        }

        let recent = store.read_recent_history(3);
        assert_eq!(recent.len(), 3);
        assert_eq!(recent[0].content, "entry 8");
    }

    #[test]
    fn test_dream_cursor() {
        let tmp = tempfile::tempdir().unwrap();
        let mut store = make_store(tmp.path());

        assert_eq!(store.get_last_dream_cursor(), 0usize);
        store.set_last_dream_cursor(42usize).unwrap();
        assert_eq!(store.get_last_dream_cursor(), 42usize);
    }

    #[test]
    fn test_read_entries_empty() {
        let tmp = tempfile::tempdir().unwrap();
        let store = make_store(tmp.path());

        let entries = store.read_entries();
        assert!(entries.is_empty());
    }

    #[test]
    fn test_read_entries_with_data() {
        let tmp = tempfile::tempdir().unwrap();
        let mut store = make_store(tmp.path());

        store.append_history("entry1").unwrap();
        store.append_history("entry2").unwrap();

        let entries = store.read_entries();
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].content, "entry1");
        assert_eq!(entries[1].content, "entry2");
    }

    #[test]
    fn test_read_recent_history_with_cap() {
        let tmp = tempfile::tempdir().unwrap();
        let mut store = make_store(tmp.path());

        for i in 1..=10 {
            store.append_history(&format!("entry {}", i)).unwrap();
        }

        let recent = store.read_recent_history(5);
        assert_eq!(recent.len(), 5);
        assert_eq!(recent[0].content, "entry 6");
        assert_eq!(recent[4].content, "entry 10");
    }

    #[test]
    fn test_read_recent_history_zero_cap() {
        let tmp = tempfile::tempdir().unwrap();
        let mut store = make_store(tmp.path());

        for i in 1..=5 {
            store.append_history(&format!("entry {}", i)).unwrap();
        }

        // max_entries = 0 means no cap
        let all = store.read_recent_history(0);
        assert_eq!(all.len(), 5);
    }

    #[test]
    fn test_history_entry_serialization() {
        let entry = HistoryEntry {
            timestamp: "2024-01-15 10:30".to_string(),
            content: "Test content".to_string(),
        };

        let json = serde_json::to_string(&entry).unwrap();
        assert!(json.contains("2024-01-15 10:30"));
        assert!(json.contains("Test content"));

        let deserialized: HistoryEntry = serde_json::from_str(&json).unwrap();
        assert_eq!(deserialized.timestamp, "2024-01-15 10:30");
        assert_eq!(deserialized.content, "Test content");
    }

    #[test]
    fn test_compact_history_after_dream_no_compaction_under_limit() {
        let tmp = tempfile::tempdir().unwrap();
        let mut store = make_store(tmp.path());

        // Create 100 entries (under HISTORY_MAX_ENTRIES limit)
        for i in 1..=100 {
            store.append_history(&format!("entry {}", i)).unwrap();
        }

        // Set dream_cursor to 60
        store.set_last_dream_cursor(60).unwrap();

        // Call compact_history_after_dream - should NOT compact
        store.compact_history_after_dream().unwrap();

        // Verify: all 100 entries remain (no compaction)
        let entries = store.read_entries();
        assert_eq!(entries.len(), 100, "Should not compact when under limit");
        assert_eq!(
            store.get_last_dream_cursor(),
            60,
            "Dream cursor should not change"
        );
    }

    #[test]
    fn test_compact_history_after_dream_compacts_over_limit() {
        let tmp = tempfile::tempdir().unwrap();
        let mut store = make_store(tmp.path());

        // Create 2100 entries (over HISTORY_MAX_ENTRIES limit of 2000)
        for i in 1..=2100 {
            store.append_history(&format!("entry {}", i)).unwrap();
        }

        // Set dream_cursor to 2000 (meaning 2000 lines processed)
        store.set_last_dream_cursor(2000).unwrap();

        // Call compact_history_after_dream - should compact
        store.compact_history_after_dream().unwrap();

        // Verify: only 50 entries remain (DREAM_KEEP_MINIMUM)
        let entries = store.read_entries();
        assert_eq!(entries.len(), 50, "Should compact to DREAM_KEEP_MINIMUM");

        // Dream cursor should be adjusted: 2000 - 2050 deleted = 0 (saturating)
        assert_eq!(
            store.get_last_dream_cursor(),
            0,
            "Dream cursor should be adjusted to 0"
        );

        // First entry should be "entry 2051"
        assert_eq!(entries[0].content, "entry 2051");
        assert_eq!(entries[49].content, "entry 2100");
    }

    #[test]
    fn test_sync_writes_without_error() {
        let tmp = tempfile::tempdir().unwrap();
        let mut store = make_store(tmp.path());

        store.append_history("entry").unwrap();

        // sync should not panic or error
        let result = store.sync();
        assert!(result.is_ok());
    }
}
