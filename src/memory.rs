use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

/// History entry in history.jsonl.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HistoryEntry {
    pub cursor: u64,
    pub timestamp: String,
    pub content: String,
}

/// Shared access to the memory store.
pub type SharedMemoryStore = Arc<tokio::sync::Mutex<MemoryStore>>;

/// Pure file I/O layer for memory files.
pub struct MemoryStore {
    workspace_dir: PathBuf,
    memory_dir: PathBuf,
    memory_file: PathBuf,
    history_file: PathBuf,
    cursor_file: PathBuf,
    dream_cursor_file: PathBuf,
    // In-memory cache for cursors, lazily initialized.
    cursor_cache: Option<u64>,
    dream_cursor_cache: Option<u64>,
}

impl MemoryStore {
    pub fn new(workspace_dir: &Path) -> Self {
        let memory_dir = workspace_dir.join("memory");
        Self {
            workspace_dir: workspace_dir.to_path_buf(),
            memory_dir: memory_dir.clone(),
            memory_file: memory_dir.join("MEMORY.md"),
            history_file: memory_dir.join("history.jsonl"),
            cursor_file: memory_dir.join(".cursor"),
            dream_cursor_file: memory_dir.join(".dream_cursor"),
            cursor_cache: None,
            dream_cursor_cache: None,
        }
    }

    /// Ensure the memory directory exists.
    pub fn init(&self) -> Result<()> {
        std::fs::create_dir_all(&self.memory_dir).context("Failed to create memory directory")
    }

    // -- MEMORY.md (long-term facts) ----------------------------------------

    pub fn read_memory(&self) -> String {
        read_file_or_empty(&self.memory_file)
    }

    pub fn write_memory(&self, content: &str) -> Result<()> {
        std::fs::write(&self.memory_file, content).context("Failed to write MEMORY.md")
    }

    // -- SOUL.md and USER.md (workspace root) --------------------------------

    pub fn read_soul(&self) -> String {
        read_file_or_empty(&self.workspace_dir.join("SOUL.md"))
    }

    pub fn write_soul(&self, content: &str) -> Result<()> {
        std::fs::write(self.workspace_dir.join("SOUL.md"), content)
            .context("Failed to write SOUL.md")
    }

    pub fn read_user(&self) -> String {
        read_file_or_empty(&self.workspace_dir.join("USER.md"))
    }

    pub fn write_user(&self, content: &str) -> Result<()> {
        std::fs::write(self.workspace_dir.join("USER.md"), content)
            .context("Failed to write USER.md")
    }

    // -- context injection helper -------------------------------------------

    /// Returns the memory content formatted for system prompt injection,
    /// or empty string if the memory is empty.
    pub fn get_memory_context(&self) -> String {
        let long_term = self.read_memory();
        if long_term.is_empty() {
            String::new()
        } else {
            format!("## Long-term Memory\n{long_term}")
        }
    }

    // -- shutdown sync ------------------------------------------------------

    /// Sync all memory files to disk (fsync). Called during shutdown.
    pub fn sync_all(&self) -> Result<()> {
        sync_if_exists(&self.history_file, true)?;
        sync_if_exists(&self.cursor_file, false)?;
        sync_if_exists(&self.dream_cursor_file, false)?;
        sync_if_exists(&self.memory_file, false)?;
        // Sync the memory directory
        if let Ok(dir) = std::fs::OpenOptions::new()
            .read(true)
            .open(&self.memory_dir)
        {
            let _ = dir.sync_all();
        }
        Ok(())
    }

    // -- history.jsonl (append-only JSONL) -----------------------------------

    /// Append an entry to history.jsonl and return its auto-incrementing cursor.
    pub fn append_history(&mut self, entry: &str) -> Result<u64> {
        let cursor = self.next_cursor();
        let now = chrono::Local::now().format("%Y-%m-%d %H:%M");
        let record = HistoryEntry {
            cursor,
            timestamp: now.to_string(),
            content: entry.to_string(),
        };
        let line = serde_json::to_string(&record).context("Failed to serialize history entry")?;
        std::fs::write(&self.cursor_file, cursor.to_string())
            .context("Failed to write history cursor")?;
        self.cursor_cache = Some(cursor);
        use std::io::Write;
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.history_file)
            .context("Failed to open history file")?;
        writeln!(file, "{}", line).context("Failed to write history entry")?;
        Ok(cursor)
    }

    /// Read history entries with cursor > `since_cursor`.
    pub fn read_unprocessed_history(&mut self, since_cursor: u64) -> Vec<HistoryEntry> {
        self.read_entries()
            .into_iter()
            .filter(|e| e.cursor > since_cursor)
            .collect()
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

    // -- cursor management --------------------------------------------------

    fn next_cursor(&mut self) -> u64 {
        if let Some(cached) = self.cursor_cache {
            return cached + 1;
        }
        if let Ok(text) = std::fs::read_to_string(&self.cursor_file) {
            if let Ok(val) = text.trim().parse::<u64>() {
                self.cursor_cache = Some(val);
                return val + 1;
            }
        }
        let entries = self.read_entries();
        let cursor = entries.last().map(|e| e.cursor + 1).unwrap_or(1);
        self.cursor_cache = Some(cursor - 1);
        cursor
    }

    pub fn get_last_dream_cursor(&mut self) -> u64 {
        if let Some(cached) = self.dream_cursor_cache {
            return cached;
        }
        if let Ok(text) = std::fs::read_to_string(&self.dream_cursor_file) {
            if let Ok(val) = text.trim().parse::<u64>() {
                self.dream_cursor_cache = Some(val);
                return val;
            }
        }
        let entries = self.read_entries();
        let cursor = entries.last().map(|e| e.cursor).unwrap_or(0);
        self.dream_cursor_cache = Some(cursor);
        cursor
    }

    pub fn set_last_dream_cursor(&mut self, cursor: u64) -> Result<()> {
        self.dream_cursor_cache = Some(cursor);
        std::fs::write(&self.dream_cursor_file, cursor.to_string())
            .context("Failed to write dream cursor")
    }

    // -- internal helpers ----------------------------------------------------

    fn read_entries(&self) -> Vec<HistoryEntry> {
        let mut entries = Vec::new();
        if let Ok(file) = std::fs::File::open(&self.history_file) {
            let reader = std::io::BufReader::new(file);
            for line in std::io::BufRead::lines(reader) {
                if let Ok(line) = line {
                    let trimmed = line.trim();
                    if trimmed.is_empty() {
                        continue;
                    }
                    if let Ok(entry) = serde_json::from_str::<HistoryEntry>(trimmed) {
                        entries.push(entry);
                    }
                }
            }
        }
        entries
    }

    fn read_last_entry(&self) -> Option<HistoryEntry> {
        use std::io::{Read, Seek, SeekFrom};
        let Ok(mut file) = std::fs::File::open(&self.history_file) else {
            return None;
        };
        let Ok(size) = file.metadata().map(|m| m.len()) else {
            return None;
        };
        if size == 0 {
            return None;
        }
        // Try progressively larger windows to handle long entries.
        for window_kb in [4, 16, 64, 256] {
            let read_size = (window_kb * 1024).min(size as usize);
            if file.seek(SeekFrom::End(-(read_size as i64))).is_err() {
                continue;
            }
            let mut buf = vec![0u8; read_size];
            if file.read_exact(&mut buf).is_err() {
                continue;
            }
            let Ok(text) = String::from_utf8(buf) else {
                continue;
            };
            let last_line = text.lines().filter(|l| !l.trim().is_empty()).last()?;
            if let Ok(entry) = serde_json::from_str::<HistoryEntry>(last_line) {
                return Some(entry);
            }
        }
        None
    }
}

/// Open a file if it exists and call fsync.
fn sync_if_exists(path: &Path, append: bool) -> std::io::Result<()> {
    if path.exists() {
        let file = std::fs::OpenOptions::new()
            .write(true)
            .append(append)
            .open(path)?;
        let _ = file.sync_all();
    }
    Ok(())
}

fn read_file_or_empty(path: &Path) -> String {
    std::fs::read_to_string(path).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_store(tmp_dir: &Path) -> MemoryStore {
        let store = MemoryStore::new(tmp_dir);
        store.init().unwrap();
        store
    }

    #[test]
    fn test_memory_read_write() {
        let tmp = tempfile::tempdir().unwrap();
        let store = make_store(tmp.path());

        assert!(store.read_memory().is_empty());
        store.write_memory("test memory").unwrap();
        assert_eq!(store.read_memory(), "test memory");
    }

    #[test]
    fn test_history_append_and_read() {
        let tmp = tempfile::tempdir().unwrap();
        let mut store = make_store(tmp.path());

        let c1 = store.append_history("first entry").unwrap();
        let c2 = store.append_history("second entry").unwrap();
        assert_eq!(c1, 1);
        assert_eq!(c2, 2);

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

        assert_eq!(store.get_last_dream_cursor(), 0);
        store.set_last_dream_cursor(42).unwrap();
        assert_eq!(store.get_last_dream_cursor(), 42);
    }

    #[test]
    fn test_cursor_cache_updates() {
        let tmp = tempfile::tempdir().unwrap();
        let mut store = make_store(tmp.path());

        let c1 = store.append_history("first").unwrap();
        assert_eq!(c1, 1);

        // Second call uses cached cursor.
        let c2 = store.append_history("second").unwrap();
        assert_eq!(c2, 2);

        // Third call confirms cache is still correct.
        let c3 = store.append_history("third").unwrap();
        assert_eq!(c3, 3);
    }

    #[test]
    fn test_get_memory_context_empty() {
        let tmp = tempfile::tempdir().unwrap();
        let store = make_store(tmp.path());

        assert!(store.get_memory_context().is_empty());
    }

    #[test]
    fn test_get_memory_context_formatted() {
        let tmp = tempfile::tempdir().unwrap();
        let store = make_store(tmp.path());

        store.write_memory("User prefers Rust").unwrap();
        let ctx = store.get_memory_context();
        assert!(ctx.contains("## Long-term Memory"));
        assert!(ctx.contains("User prefers Rust"));
    }

    #[test]
    fn test_soul_read_write() {
        let tmp = tempfile::tempdir().unwrap();
        let store = make_store(tmp.path());

        assert!(store.read_soul().is_empty());
        store.write_soul("You are a helpful assistant").unwrap();
        assert_eq!(store.read_soul(), "You are a helpful assistant");
    }

    #[test]
    fn test_user_read_write() {
        let tmp = tempfile::tempdir().unwrap();
        let store = make_store(tmp.path());

        assert!(store.read_user().is_empty());
        store.write_user("Alice, software engineer").unwrap();
        assert_eq!(store.read_user(), "Alice, software engineer");
    }

    #[test]
    fn test_sync_all() {
        let tmp = tempfile::tempdir().unwrap();
        let mut store = make_store(tmp.path());

        store.write_memory("test").unwrap();
        store.append_history("entry").unwrap();

        // sync_all should not panic or error
        let result = store.sync_all();
        assert!(result.is_ok());
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
    fn test_next_cursor_starts_at_zero() {
        let tmp = tempfile::tempdir().unwrap();
        let mut store = make_store(tmp.path());

        assert_eq!(store.next_cursor(), 1);
    }

    #[test]
    fn test_next_cursor_increments() {
        let tmp = tempfile::tempdir().unwrap();
        let mut store = make_store(tmp.path());

        let c1 = store.append_history("entry1").unwrap();
        let c2 = store.append_history("entry2").unwrap();
        let c3 = store.append_history("entry3").unwrap();

        assert_eq!(c1, 1);
        assert_eq!(c2, 2);
        assert_eq!(c3, 3);
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
            cursor: 42,
            timestamp: "2024-01-15 10:30".to_string(),
            content: "Test content".to_string(),
        };

        let json = serde_json::to_string(&entry).unwrap();
        assert!(json.contains("42"));
        assert!(json.contains("2024-01-15 10:30"));
        assert!(json.contains("Test content"));

        let deserialized: HistoryEntry = serde_json::from_str(&json).unwrap();
        assert_eq!(deserialized.cursor, 42);
        assert_eq!(deserialized.timestamp, "2024-01-15 10:30");
        assert_eq!(deserialized.content, "Test content");
    }

    #[test]
    fn test_memory_init_creates_directory() {
        let tmp = tempfile::tempdir().unwrap();
        let workspace_dir = tmp.path().join("workspace");
        std::fs::create_dir_all(&workspace_dir).unwrap();

        let store = MemoryStore::new(&workspace_dir);
        assert!(!workspace_dir.join("memory").exists());

        store.init().unwrap();
        assert!(workspace_dir.join("memory").exists());
    }
}
