pub mod backend;
pub mod category;
pub mod factory;
mod fs;
pub mod history;
pub mod markdown;

use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result};

use backend::{EditOutcome, MemoryBackend, MemoryBackendKind};
use category::MemoryCategory;
use factory::create_backend;
pub use history::{HistoryEntry, HistoryStore};

/// Shared access to the memory facade.
pub type SharedMemory = Arc<tokio::sync::Mutex<MemoryService>>;

/// Unified memory facade.
///
/// Combines a swappable [`MemoryBackend`] (category document reads/writes)
/// with the append-only `HistoryStore` (history.jsonl + dream cursor).
/// Category operations go through the backend trait; history operations stay
/// on the concrete store by design (see `history.rs`).
pub struct MemoryService {
    memory_dir: PathBuf,
    backend: Arc<dyn MemoryBackend>,
    history: HistoryStore,
}

impl MemoryService {
    /// Create a facade backed by the default markdown backend.
    pub fn new(workspace_dir: &Path) -> Self {
        Self::with_backend(
            workspace_dir,
            create_backend(MemoryBackendKind::Markdown, workspace_dir),
        )
    }

    /// Create a facade with an explicit backend (config-driven construction).
    pub fn with_backend(workspace_dir: &Path, backend: Arc<dyn MemoryBackend>) -> Self {
        let memory_dir = workspace_dir.join("memory");
        Self {
            memory_dir: memory_dir.clone(),
            backend,
            history: HistoryStore::new(
                memory_dir.join("history.jsonl"),
                memory_dir.join(".dream_cursor"),
            ),
        }
    }

    /// The underlying backend (for kind/introspection).
    pub fn backend(&self) -> &dyn MemoryBackend {
        self.backend.as_ref()
    }

    /// Ensure the memory directory exists.
    pub fn init(&self) -> Result<()> {
        std::fs::create_dir_all(&self.memory_dir).context("Failed to create memory directory")
    }

    // -- category operations (through the backend) --------------------------

    pub async fn get(
        &self,
        category: &MemoryCategory,
        name: Option<&str>,
    ) -> Result<Option<String>> {
        self.backend.get(category, name).await
    }

    pub async fn set(
        &self,
        category: &MemoryCategory,
        name: Option<&str>,
        content: &str,
    ) -> Result<()> {
        self.backend.set(category, name, content).await
    }

    pub async fn edit(
        &self,
        category: &MemoryCategory,
        name: Option<&str>,
        old: &str,
        new: &str,
    ) -> Result<EditOutcome> {
        self.backend.edit(category, name, old, new).await
    }

    pub async fn remove(&self, category: &MemoryCategory, name: Option<&str>) -> Result<bool> {
        self.backend.remove(category, name).await
    }

    pub async fn list_skill_names(&self) -> Result<Vec<String>> {
        self.backend.list_skill_names().await
    }

    pub async fn list_custom_names(&self) -> Result<Vec<String>> {
        self.backend.list_custom_names().await
    }

    /// Effective content for injection: returns `None` when the document is
    /// missing, empty, or identical to its bootstrap template. This preserves
    /// the previous "unmodified template not injected" behavior, including its
    /// tolerance for whitespace-only differences at either end.
    pub async fn get_effective(
        &self,
        category: &MemoryCategory,
        name: Option<&str>,
    ) -> Result<Option<String>> {
        let Some(content) = self.backend.get(category, name).await? else {
            return Ok(None);
        };
        if content.trim().is_empty() {
            return Ok(None);
        }
        if let Some(template) = self.backend.template(category)
            && template.trim() == content.trim()
        {
            return Ok(None);
        }
        Ok(Some(content))
    }

    // -- history operations (through HistoryStore) --------------------------

    pub fn append_history(&mut self, entry: &str) -> Result<()> {
        self.history.append_history(entry)
    }

    pub fn read_entries(&self) -> Vec<HistoryEntry> {
        self.history.read_entries()
    }

    pub fn read_unprocessed_history(&self, skip_lines: usize) -> Vec<HistoryEntry> {
        self.history.read_unprocessed_history(skip_lines)
    }

    pub fn read_recent_history(&self, max_entries: usize) -> Vec<HistoryEntry> {
        self.history.read_recent_history(max_entries)
    }

    pub fn get_last_dream_cursor(&mut self) -> usize {
        self.history.get_last_dream_cursor()
    }

    pub fn set_last_dream_cursor(&mut self, cursor: usize) -> Result<()> {
        self.history.set_last_dream_cursor(cursor)
    }

    pub fn compact_history_after_dream(&mut self) -> Result<()> {
        self.history.compact_history_after_dream()
    }

    // -- shutdown sync ------------------------------------------------------

    /// Sync all memory files to disk (fsync). Called during shutdown.
    ///
    /// The backend owns the document files (fsync of every fixed document plus
    /// the memory directory); the history store owns `history.jsonl` and its
    /// cursors. This keeps a future backend's durability hook reachable.
    pub async fn sync_all(&self) -> Result<()> {
        self.backend.sync().await?;
        self.history.sync()?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_store(tmp_dir: &Path) -> MemoryService {
        let store = MemoryService::new(tmp_dir);
        store.init().unwrap();
        store
    }

    #[tokio::test]
    async fn test_sync_all_writes_without_error() {
        let tmp = tempfile::tempdir().unwrap();
        let mut store = make_store(tmp.path());

        store
            .set(&MemoryCategory::Core, None, "test")
            .await
            .unwrap();
        store.append_history("entry").unwrap();

        // sync_all should not panic or error
        let result = store.sync_all().await;
        assert!(result.is_ok());
    }

    #[test]
    fn test_memory_init_creates_directory() {
        let tmp = tempfile::tempdir().unwrap();
        let workspace_dir = tmp.path().join("workspace");
        std::fs::create_dir_all(&workspace_dir).unwrap();

        let store = MemoryService::new(&workspace_dir);
        assert!(!workspace_dir.join("memory").exists());

        store.init().unwrap();
        assert!(workspace_dir.join("memory").exists());
    }

    // -- facade category API ------------------------------------------------

    #[tokio::test]
    async fn test_facade_get_effective_missing_is_none() {
        let tmp = tempfile::tempdir().unwrap();
        let store = MemoryService::new(tmp.path());
        assert_eq!(
            store
                .get_effective(&MemoryCategory::Core, None)
                .await
                .unwrap(),
            None
        );
    }

    #[tokio::test]
    async fn test_facade_get_effective_core_template_returns_none() {
        let tmp = tempfile::tempdir().unwrap();
        let store = MemoryService::new(tmp.path());
        store.init().unwrap();
        // A fresh workspace holds the exact MEMORY.md template: Core must not
        // be injected until it has been edited.
        let template = store.backend().template(&MemoryCategory::Core).unwrap();
        store
            .set(&MemoryCategory::Core, None, &template)
            .await
            .unwrap();
        assert_eq!(
            store
                .get_effective(&MemoryCategory::Core, None)
                .await
                .unwrap(),
            None
        );
    }

    #[tokio::test]
    async fn test_facade_get_effective_template_ignores_trailing_whitespace() {
        let tmp = tempfile::tempdir().unwrap();
        let store = MemoryService::new(tmp.path());
        let template = store.backend().template(&MemoryCategory::Soul).unwrap();
        store
            .set(&MemoryCategory::Soul, None, &format!("{template}\n\n  "))
            .await
            .unwrap();
        assert_eq!(
            store
                .get_effective(&MemoryCategory::Soul, None)
                .await
                .unwrap(),
            None
        );
    }

    #[tokio::test]
    async fn test_facade_get_effective_empty_is_none() {
        let tmp = tempfile::tempdir().unwrap();
        let store = MemoryService::new(tmp.path());
        store.set(&MemoryCategory::Core, None, "").await.unwrap();
        assert_eq!(
            store
                .get_effective(&MemoryCategory::Core, None)
                .await
                .unwrap(),
            None
        );
    }

    #[tokio::test]
    async fn test_facade_get_effective_template_equals_is_none() {
        let tmp = tempfile::tempdir().unwrap();
        let store = MemoryService::new(tmp.path());
        // Copy the SOUL template verbatim; should be treated as unmodified.
        let template = store.backend().template(&MemoryCategory::Soul).unwrap();
        store
            .set(&MemoryCategory::Soul, None, &template)
            .await
            .unwrap();
        assert_eq!(
            store
                .get_effective(&MemoryCategory::Soul, None)
                .await
                .unwrap(),
            None
        );
    }

    #[tokio::test]
    async fn test_facade_get_effective_custom_content_returned() {
        let tmp = tempfile::tempdir().unwrap();
        let store = MemoryService::new(tmp.path());
        store
            .set(&MemoryCategory::Soul, None, "Custom soul text")
            .await
            .unwrap();
        assert_eq!(
            store
                .get_effective(&MemoryCategory::Soul, None)
                .await
                .unwrap(),
            Some("Custom soul text".to_string())
        );
    }

    #[tokio::test]
    async fn test_facade_category_ops_delegate() {
        let tmp = tempfile::tempdir().unwrap();
        let store = MemoryService::new(tmp.path());
        store
            .set(&MemoryCategory::Custom, Some("notes"), "note body")
            .await
            .unwrap();
        assert_eq!(
            store
                .get(&MemoryCategory::Custom, Some("notes"))
                .await
                .unwrap(),
            Some("note body".to_string())
        );
        assert!(
            store
                .remove(&MemoryCategory::Custom, Some("notes"))
                .await
                .unwrap()
        );
        assert_eq!(
            store
                .get(&MemoryCategory::Custom, Some("notes"))
                .await
                .unwrap(),
            None
        );
        assert_eq!(
            store.list_custom_names().await.unwrap(),
            Vec::<String>::new()
        );
    }

    #[tokio::test]
    async fn test_facade_skill_names() {
        let tmp = tempfile::tempdir().unwrap();
        let store = MemoryService::new(tmp.path());
        store
            .set(&MemoryCategory::Skill, Some("grep"), "# Grep")
            .await
            .unwrap();
        assert_eq!(store.list_skill_names().await.unwrap(), vec!["grep"]);
    }

    #[tokio::test]
    async fn test_facade_edit_delegates() {
        let tmp = tempfile::tempdir().unwrap();
        let store = MemoryService::new(tmp.path());
        store
            .set(&MemoryCategory::Core, None, "alpha beta")
            .await
            .unwrap();
        assert_eq!(
            store
                .edit(&MemoryCategory::Core, None, "beta", "gamma")
                .await
                .unwrap(),
            EditOutcome::Replaced
        );
        assert_eq!(
            store.get(&MemoryCategory::Core, None).await.unwrap(),
            Some("alpha gamma".to_string())
        );
    }
}
