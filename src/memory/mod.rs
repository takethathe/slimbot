pub mod backend;
pub mod category;
pub mod factory;
pub(crate) mod frontmatter;
mod fs;
pub mod history;
mod index;
pub mod markdown;

use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result};

use backend::{EditOutcome, MemoryBackend, MemoryBackendKind};
use category::MemoryCategory;
use factory::create_backend;
pub use history::{HistoryEntry, HistoryStore};
pub use index::{DEFAULT_LIMIT, MAX_LIMIT, ScoredEntry};
use index::{DocumentSnapshot, KeywordIndex, MemoryIndex, SEARCH_CATEGORIES, source_path};

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
    /// Lazily built derived entry index; `None` until the first search.
    index: Option<KeywordIndex>,
    /// Content fingerprint of the documents the cached index was built from.
    index_fingerprint: Option<u64>,
    /// How many times the index has been (re)built. Test-only observation
    /// hook used to prove that unchanged documents are not re-parsed; it is
    /// deliberately not part of the public API and not present in release
    /// builds.
    #[cfg(test)]
    index_build_count: usize,
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
            index: None,
            index_fingerprint: None,
            #[cfg(test)]
            index_build_count: 0,
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

    // -- derived entry search (read-only) -----------------------------------

    /// Search memory documents for entries relevant to `query`.
    ///
    /// The index is built lazily on first use and rebuilt whenever the
    /// fingerprint of the enumerable documents changes, so search never
    /// returns stale content. Takes `&mut self` because of that in-memory
    /// index cache, matching the `&mut self` precedent of the history API;
    /// callers hold an `Arc<Mutex<MemoryService>>`, so a mutable borrow is
    /// always available.
    pub async fn search(
        &mut self,
        query: &str,
        limit: Option<usize>,
        category: Option<&MemoryCategory>,
    ) -> Result<Vec<ScoredEntry>> {
        let limit = limit.unwrap_or(DEFAULT_LIMIT).min(MAX_LIMIT);
        let documents = self.enumerable_documents().await?;
        let fingerprint = index::fingerprint(&documents);

        if self.index.is_none() || self.index_fingerprint != Some(fingerprint) {
            let mut index = KeywordIndex::new();
            index.rebuild(&documents).await?;
            self.index = Some(index);
            self.index_fingerprint = Some(fingerprint);
            #[cfg(test)]
            {
                self.index_build_count += 1;
            }
        }

        let index = self
            .index
            .as_ref()
            .expect("index was built in the branch above");
        Ok(index.search(query, limit, category))
    }

    /// Documents that make up the search corpus: the five fixed documents plus
    /// every enumerated skill and custom document. `Daily` has no writer yet,
    /// so it is naturally absent. Everything is read through the backend, so a
    /// future sqlite backend needs no changes here.
    async fn enumerable_documents(&self) -> Result<Vec<DocumentSnapshot>> {
        let mut documents = Vec::new();

        for category in SEARCH_CATEGORIES {
            if let Some(content) = self.backend.get(&category, None).await? {
                documents.push(DocumentSnapshot {
                    source: source_path(&category, None),
                    category,
                    name: None,
                    content,
                });
            }
        }

        self.push_named_documents(
            MemoryCategory::Skill,
            self.backend.list_skill_names().await?,
            &mut documents,
        )
        .await?;
        self.push_named_documents(
            MemoryCategory::Custom,
            self.backend.list_custom_names().await?,
            &mut documents,
        )
        .await?;

        Ok(documents)
    }

    /// Append one snapshot per existing named document of `category`.
    async fn push_named_documents(
        &self,
        category: MemoryCategory,
        names: Vec<String>,
        documents: &mut Vec<DocumentSnapshot>,
    ) -> Result<()> {
        for name in names {
            if let Some(content) = self.backend.get(&category, Some(&name)).await? {
                documents.push(DocumentSnapshot {
                    source: source_path(&category, Some(&name)),
                    category,
                    name: Some(name),
                    content,
                });
            }
        }
        Ok(())
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

    // -- derived entry search (main seam) -----------------------------------

    /// Sorted list of every file path under `root`, for read-only assertions.
    fn snapshot(root: &Path) -> Vec<PathBuf> {
        let mut paths = Vec::new();
        let mut stack = vec![root.to_path_buf()];
        while let Some(dir) = stack.pop() {
            let Ok(entries) = std::fs::read_dir(&dir) else {
                continue;
            };
            for entry in entries.flatten() {
                let path = entry.path();
                if path.is_dir() {
                    stack.push(path.clone());
                }
                paths.push(path);
            }
        }
        paths.sort();
        paths
    }

    #[tokio::test]
    async fn test_search_returns_entries_with_source_and_category() {
        let tmp = tempfile::tempdir().unwrap();
        let mut store = make_store(tmp.path());
        store
            .set(
                &MemoryCategory::Core,
                None,
                "# Deployment\n\nThe nightly deploy runs at 02:00.\n\nUnrelated fact.\n",
            )
            .await
            .unwrap();

        let hits = store.search("deploy", None, None).await.unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].entry.category, MemoryCategory::Core);
        assert_eq!(hits[0].entry.source, "memory/MEMORY.md");
        assert_eq!(hits[0].entry.heading.as_deref(), Some("Deployment"));
    }

    #[tokio::test]
    async fn test_search_enumerates_skills_and_customs() {
        let tmp = tempfile::tempdir().unwrap();
        let mut store = make_store(tmp.path());
        store
            .set(
                &MemoryCategory::Skill,
                Some("release"),
                "---\nname: release\ndescription: Ship it.\n---\n\nTag and push the release.\n",
            )
            .await
            .unwrap();
        store
            .set(
                &MemoryCategory::Custom,
                Some("glossary"),
                "Widget means a UI part.",
            )
            .await
            .unwrap();

        let skill = store.search("release", None, None).await.unwrap();
        assert_eq!(skill.len(), 1);
        assert_eq!(skill[0].entry.category, MemoryCategory::Skill);
        assert_eq!(skill[0].entry.source, "skills/release/SKILL.md");

        let custom = store.search("widget", None, None).await.unwrap();
        assert_eq!(custom.len(), 1);
        assert_eq!(custom[0].entry.source, "memory/glossary.md");
    }

    #[tokio::test]
    async fn test_search_category_filter_narrows() {
        let tmp = tempfile::tempdir().unwrap();
        let mut store = make_store(tmp.path());
        store
            .set(&MemoryCategory::Core, None, "shared keyword")
            .await
            .unwrap();
        store
            .set(&MemoryCategory::User, None, "shared keyword")
            .await
            .unwrap();

        let hits = store
            .search("shared", None, Some(&MemoryCategory::User))
            .await
            .unwrap();
        assert!(
            hits.iter()
                .all(|h| h.entry.category == MemoryCategory::User)
        );
        assert_eq!(hits.len(), 1);
    }

    #[tokio::test]
    async fn test_search_chinese_query_matches_chinese_content() {
        let tmp = tempfile::tempdir().unwrap();
        let mut store = make_store(tmp.path());
        store
            .set(
                &MemoryCategory::User,
                None,
                "部署流程需要先构建镜像。\n\n无关内容。",
            )
            .await
            .unwrap();

        let hits = store.search("部署", None, None).await.unwrap();
        assert_eq!(hits.len(), 1);
        assert!(hits[0].entry.content.contains("部署"));
    }

    #[tokio::test]
    async fn test_search_limit_defaults_and_caps() {
        let tmp = tempfile::tempdir().unwrap();
        let mut store = make_store(tmp.path());
        let body: String = (0..60).map(|i| format!("keyword {i}\n\n")).collect();
        store.set(&MemoryCategory::Core, None, &body).await.unwrap();

        assert_eq!(
            store.search("keyword", None, None).await.unwrap().len(),
            DEFAULT_LIMIT
        );
        assert_eq!(
            store
                .search("keyword", Some(1000), None)
                .await
                .unwrap()
                .len(),
            MAX_LIMIT
        );
        assert_eq!(
            store.search("keyword", Some(3), None).await.unwrap().len(),
            3
        );
    }

    #[tokio::test]
    async fn test_search_reflects_document_edits() {
        let tmp = tempfile::tempdir().unwrap();
        let mut store = make_store(tmp.path());
        store
            .set(&MemoryCategory::Core, None, "alpha fact")
            .await
            .unwrap();
        assert_eq!(store.search("alpha", None, None).await.unwrap().len(), 1);

        store
            .set(&MemoryCategory::Core, None, "bravo fact")
            .await
            .unwrap();
        assert!(
            store.search("alpha", None, None).await.unwrap().is_empty(),
            "edited-away content must disappear"
        );
        assert_eq!(store.search("bravo", None, None).await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn test_search_reflects_document_deletion() {
        let tmp = tempfile::tempdir().unwrap();
        let mut store = make_store(tmp.path());
        store
            .set(&MemoryCategory::Custom, Some("notes"), "obsolete keyword")
            .await
            .unwrap();
        assert_eq!(store.search("obsolete", None, None).await.unwrap().len(), 1);

        assert!(
            store
                .remove(&MemoryCategory::Custom, Some("notes"))
                .await
                .unwrap()
        );
        assert!(
            store
                .search("obsolete", None, None)
                .await
                .unwrap()
                .is_empty()
        );
    }

    #[tokio::test]
    async fn test_search_sees_newly_created_document() {
        let tmp = tempfile::tempdir().unwrap();
        let mut store = make_store(tmp.path());
        assert!(
            store
                .search("freshly", None, None)
                .await
                .unwrap()
                .is_empty()
        );
        store
            .set(
                &MemoryCategory::Skill,
                Some("newskill"),
                "freshly added workflow",
            )
            .await
            .unwrap();
        assert_eq!(store.search("freshly", None, None).await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn test_search_reuses_index_when_documents_unchanged() {
        let tmp = tempfile::tempdir().unwrap();
        let mut store = make_store(tmp.path());
        store
            .set(&MemoryCategory::Core, None, "stable content")
            .await
            .unwrap();

        store.search("stable", None, None).await.unwrap();
        assert_eq!(store.index_build_count, 1, "first search builds the index");
        store.search("stable", None, None).await.unwrap();
        store.search("content", None, None).await.unwrap();
        assert_eq!(
            store.index_build_count, 1,
            "unchanged documents must not be re-parsed"
        );

        store
            .set(&MemoryCategory::Core, None, "changed content")
            .await
            .unwrap();
        store.search("changed", None, None).await.unwrap();
        assert_eq!(store.index_build_count, 2, "a change triggers one rebuild");
    }

    #[tokio::test]
    async fn test_search_does_not_write_any_file() {
        let tmp = tempfile::tempdir().unwrap();
        let mut store = make_store(tmp.path());
        store
            .set(&MemoryCategory::Core, None, "read only searchable")
            .await
            .unwrap();
        let before = snapshot(tmp.path());

        store.search("searchable", None, None).await.unwrap();
        store.search("nonexistent", None, None).await.unwrap();

        assert_eq!(
            before,
            snapshot(tmp.path()),
            "search must not create an index file or any other artifact"
        );
    }

    #[tokio::test]
    async fn test_search_corpus_excludes_daily_documents_and_history() {
        // The retrieval corpus is memory documents only: `Daily` has no writer
        // and is not a search category, and the append-only history log is an
        // event log, not a fact store (ADR 0012 / ticket 05).
        let tmp = tempfile::tempdir().unwrap();
        let mut store = make_store(tmp.path());
        store
            .set(&MemoryCategory::Core, None, "deploywindow closes at 18:00")
            .await
            .unwrap();
        std::fs::write(
            tmp.path().join("memory").join("2026-11-02.md"),
            "dailyonlytoken was logged today",
        )
        .unwrap();
        store
            .append_history("historyonlytoken was said in a turn")
            .unwrap();

        assert!(
            store
                .search("dailyonlytoken", None, None)
                .await
                .unwrap()
                .is_empty(),
            "date-named Daily documents must not be searchable"
        );
        assert!(
            store
                .search("historyonlytoken", None, None)
                .await
                .unwrap()
                .is_empty(),
            "session history must not be part of the retrieval corpus"
        );

        let hits = store.search("deploywindow", None, None).await.unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].entry.source, "memory/MEMORY.md");

        assert!(
            !SEARCH_CATEGORIES.contains(&MemoryCategory::Daily),
            "Daily is deliberately not a search category"
        );
    }

    #[tokio::test]
    async fn test_search_empty_query_and_no_match_are_empty() {
        let tmp = tempfile::tempdir().unwrap();
        let mut store = make_store(tmp.path());
        store
            .set(&MemoryCategory::Core, None, "some content")
            .await
            .unwrap();
        assert!(store.search("", None, None).await.unwrap().is_empty());
        assert!(store.search("   ", None, None).await.unwrap().is_empty());
        assert!(store.search("absent", None, None).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn test_search_empty_and_heading_only_documents_do_not_error() {
        let tmp = tempfile::tempdir().unwrap();
        let mut store = make_store(tmp.path());
        store.set(&MemoryCategory::Core, None, "").await.unwrap();
        store
            .set(&MemoryCategory::User, None, "# Only a heading\n")
            .await
            .unwrap();
        assert!(
            store
                .search("anything", None, None)
                .await
                .unwrap()
                .is_empty()
        );
    }
}
