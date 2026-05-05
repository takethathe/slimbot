use std::fs;
use std::sync::Arc;

use tempfile::TempDir;
use tokio::sync::Mutex;

use slimbot::{
    Consolidator, ContextBuilder, MemoryStore, Message,
    Provider, LLMResponse, FinishReason, Usage,
    SessionManager, SharedSessionManager, ToolManager,
    ToolDefinition,
};

// ── SessionMeta: last_summary persistence and reload ──

#[tokio::test]
async fn test_set_last_summary_in_memory() {
    let tmp = TempDir::new().unwrap();
    let session_dir = tmp.path().join("sessions");
    let mut sm = SessionManager::new(session_dir).unwrap();

    sm.get_or_create("s1").await.unwrap();
    sm.add_message("s1", Message::user("hello".to_string())).await.unwrap();

    // Initially no summary
    assert!(sm.get_last_summary("s1").await.is_none());

    sm.set_last_summary("s1", "user prefers short replies").await;
    let summary = sm.get_last_summary("s1").await;
    assert_eq!(summary.as_deref(), Some("user prefers short replies"));
}

#[tokio::test]
async fn test_last_summary_persists_and_reloads() {
    let tmp = TempDir::new().unwrap();
    let session_dir = tmp.path().join("sessions");

    // First session: set summary and persist
    {
        let mut sm = SessionManager::new(session_dir.clone()).unwrap();
        sm.get_or_create("s1").await.unwrap();
        sm.set_last_summary("s1", "user chose option B after comparing").await;
        sm.persist("s1").await.unwrap();
    }

    // Reload from a fresh SessionManager
    {
        let mut sm = SessionManager::new(session_dir).unwrap();
        sm.get_or_create("s1").await.unwrap(); // load from disk
        let summary = sm.get_last_summary("s1").await;
        assert_eq!(
            summary.as_deref(),
            Some("user chose option B after comparing")
        );
    }
}

#[tokio::test]
async fn test_last_summary_empty_is_cleared() {
    let tmp = TempDir::new().unwrap();
    let session_dir = tmp.path().join("sessions");
    let mut sm = SessionManager::new(session_dir).unwrap();

    sm.get_or_create("s1").await.unwrap();
    sm.set_last_summary("s1", "some summary").await;
    assert!(sm.get_last_summary("s1").await.is_some());

    // Setting "(nothing)" should clear
    sm.set_last_summary("s1", "(nothing)").await;
    assert!(sm.get_last_summary("s1").await.is_none());

    // Setting empty string should clear
    sm.set_last_summary("s1", "another summary").await;
    assert!(sm.get_last_summary("s1").await.is_some());
    sm.set_last_summary("s1", "").await;
    assert!(sm.get_last_summary("s1").await.is_none());
}

#[tokio::test]
async fn test_char_per_token_ratio_persists_and_reloads() {
    let tmp = TempDir::new().unwrap();
    let session_dir = tmp.path().join("sessions");

    // First session: add messages and persist
    {
        let mut sm = SessionManager::new(session_dir.clone()).unwrap();
        sm.get_or_create("s1").await.unwrap();
        sm.add_message("s1", Message::user("hello world".to_string())).await.unwrap();
        sm.persist("s1").await.unwrap();
    }

    // Reload: ratio defaults to 4.0 since no LLM call happened
    {
        let mut sm = SessionManager::new(session_dir).unwrap();
        sm.get_or_create("s1").await.unwrap(); // load from disk
        let data = sm.get_session_data("s1");
        assert!(data.is_some());
        let data = data.unwrap();
        assert_eq!(data.char_per_token_ratio, 4.0);
    }
}

// ── Consolidation summary appended to history.jsonl ──

#[tokio::test]
async fn test_consolidation_summary_appended_to_history() {
    let tmp = TempDir::new().unwrap();
    let workspace_dir = tmp.path().join("workspace");
    std::fs::create_dir_all(&workspace_dir).unwrap();

    let mut store = MemoryStore::new(&workspace_dir);
    store.init().unwrap();

    store.append_history("first historical event").unwrap();
    assert_eq!(store.read_recent_history(0).len(), 1);

    // Simulate a consolidation summary being appended
    store.append_history("- User decided to use SQLite over Postgres").unwrap();

    let entries = store.read_recent_history(10);
    assert_eq!(entries.len(), 2);
    assert!(entries[1].content.contains("SQLite"));
}

// ── ContextBuilder summary injection ──

#[tokio::test]
async fn test_context_builder_injects_summary() {
    let tmp = TempDir::new().unwrap();
    let session_dir = tmp.path().join("sessions");
    let workspace_dir = tmp.path().join("workspace");
    std::fs::create_dir_all(&session_dir).unwrap();
    std::fs::create_dir_all(&workspace_dir).unwrap();

    let mut sm = SessionManager::new(session_dir.clone()).unwrap();
    sm.get_or_create("s1").await.unwrap();
    sm.add_message("s1", Message::user("current query".to_string())).await.unwrap();

    let sm: SharedSessionManager = Arc::new(Mutex::new(sm));

    let ms = Arc::new(Mutex::new(MemoryStore::new(&workspace_dir)));
    ms.lock().await.init().unwrap();

    let tm = Arc::new(ToolManager::new(workspace_dir.clone()));

    let cb = ContextBuilder::new(sm.clone(), tm.clone(), workspace_dir.clone(), ms.clone());

    // Without summary
    let ctx = cb.build("s1", None, None, None, None).await;
    let system_text = match &ctx.messages[0] {
        Message::System { content, .. } => content,
        _ => panic!("expected system message"),
    };
    assert!(!system_text.contains("[Resumed Session]"));

    // With summary
    let ctx = cb.build("s1", None, Some("user chose SQLite"), None, None).await;
    let system_text = match &ctx.messages[0] {
        Message::System { content, .. } => content,
        _ => panic!("expected system message"),
    };
    assert!(system_text.contains("[Resumed Session]"));
    assert!(system_text.contains("user chose SQLite"));
}

#[tokio::test]
async fn test_context_builder_skips_empty_summary() {
    let tmp = TempDir::new().unwrap();
    let session_dir = tmp.path().join("sessions");
    let workspace_dir = tmp.path().join("workspace");
    std::fs::create_dir_all(&session_dir).unwrap();
    std::fs::create_dir_all(&workspace_dir).unwrap();

    let mut sm = SessionManager::new(session_dir.clone()).unwrap();
    sm.get_or_create("s1").await.unwrap();
    let sm: SharedSessionManager = Arc::new(Mutex::new(sm));

    let ms = Arc::new(Mutex::new(MemoryStore::new(&workspace_dir)));
    ms.lock().await.init().unwrap();

    let tm = Arc::new(ToolManager::new(workspace_dir.clone()));
    let cb = ContextBuilder::new(sm.clone(), tm.clone(), workspace_dir.clone(), ms.clone());

    // Empty summary should not add [Resumed Session] section
    let ctx = cb.build("s1", None, Some(""), None, None).await;
    let system_text = match &ctx.messages[0] {
        Message::System { content, .. } => content,
        _ => panic!("expected system message"),
    };
    assert!(!system_text.contains("[Resumed Session]"));
}

// ── Consolidator: summary storage and history append ──

/// Mock provider that returns a summary for consolidation.
struct SummaryMockProvider {
    summary: String,
}

#[async_trait::async_trait]
impl Provider for SummaryMockProvider {
    async fn chat(
        &self,
        _messages: &[Message],
        _tools: Option<&[ToolDefinition]>,
    ) -> anyhow::Result<LLMResponse> {
        Ok(LLMResponse {
            content: Some(self.summary.clone()),
            tool_calls: None,
            finish_reason: FinishReason::Stop,
            usage: Usage {
                prompt_tokens: 100,
                prompt_cache_hit_tokens: 0,
                completion_tokens: 10,
                total_tokens: 110,
            },
        })
    }
}

#[tokio::test]
async fn test_consolidator_archives_summary_to_history() {
    let tmp = TempDir::new().unwrap();
    let session_dir = tmp.path().join("sessions");
    let workspace_dir = tmp.path().join("workspace");
    std::fs::create_dir_all(&session_dir).unwrap();
    std::fs::create_dir_all(&workspace_dir).unwrap();

    let mut sm = SessionManager::new(session_dir.clone()).unwrap();
    sm.get_or_create("s1").await.unwrap();

    // Add enough messages to trigger consolidation
    for i in 0..5 {
        sm.add_message("s1", Message::user(format!("user msg {i}"))).await.unwrap();
        sm.add_message("s1", Message::assistant(Some(format!("reply {i}")), None)).await.unwrap();
    }

    let sm: SharedSessionManager = Arc::new(Mutex::new(sm));

    let ms = Arc::new(Mutex::new(MemoryStore::new(&workspace_dir)));
    ms.lock().await.init().unwrap();

    let provider = Arc::new(SummaryMockProvider {
        summary: "- User prefers dark mode\n- Decided to use SQLite".to_string(),
    });

    let consolidator = Consolidator::new(
        provider,
        sm.clone(),
        ms.clone(),
        8192,
        4096,
    );

    // Trigger consolidation with high prompt token count
    consolidator.maybe_consolidate("s1", 10000).await.unwrap();

    // Verify summary was appended to history
    let entries = ms.lock().await.read_recent_history(10);
    assert!(!entries.is_empty());
    let last_entry = entries.last().unwrap();
    assert!(last_entry.content.contains("dark mode"));
    assert!(last_entry.content.contains("SQLite"));
}

#[tokio::test]
async fn test_consolidator_updates_summary_in_session_meta() {
    let tmp = TempDir::new().unwrap();
    let session_dir = tmp.path().join("sessions");
    let workspace_dir = tmp.path().join("workspace");
    std::fs::create_dir_all(&session_dir).unwrap();
    std::fs::create_dir_all(&workspace_dir).unwrap();

    let mut sm = SessionManager::new(session_dir.clone()).unwrap();
    sm.get_or_create("s1").await.unwrap();

    for i in 0..5 {
        sm.add_message("s1", Message::user(format!("user msg {i}"))).await.unwrap();
        sm.add_message("s1", Message::assistant(Some(format!("reply {i}")), None)).await.unwrap();
    }

    let sm: SharedSessionManager = Arc::new(Mutex::new(sm));

    let ms = Arc::new(Mutex::new(MemoryStore::new(&workspace_dir)));
    ms.lock().await.init().unwrap();

    let summary_text = "- User uses VS Code\n- Discovered that SQLite WAL mode fixes the issue";
    let provider = Arc::new(SummaryMockProvider {
        summary: summary_text.to_string(),
    });

    let consolidator = Consolidator::new(
        provider,
        sm.clone(),
        ms.clone(),
        8192,
        4096,
    );

    // Initially no summary
    assert!({
        let guard = sm.lock().await;
        guard.get_last_summary("s1").await.is_none()
    });

    consolidator.maybe_consolidate("s1", 10000).await.unwrap();

    // After consolidation, summary should be in session meta
    let summary = {
        let guard = sm.lock().await;
        guard.get_last_summary("s1").await
    };
    assert!(summary.is_some());
    let summary = summary.unwrap();
    assert!(summary.contains("VS Code"));
    assert!(summary.contains("SQLite WAL"));
}

#[tokio::test]
async fn test_consolidator_no_consolidation_when_within_budget() {
    let tmp = TempDir::new().unwrap();
    let session_dir = tmp.path().join("sessions");
    let workspace_dir = tmp.path().join("workspace");
    std::fs::create_dir_all(&session_dir).unwrap();
    std::fs::create_dir_all(&workspace_dir).unwrap();

    let mut sm = SessionManager::new(session_dir.clone()).unwrap();
    sm.get_or_create("s1").await.unwrap();
    sm.add_message("s1", Message::user("short query".to_string())).await.unwrap();

    let sm: SharedSessionManager = Arc::new(Mutex::new(sm));

    let ms = Arc::new(Mutex::new(MemoryStore::new(&workspace_dir)));
    ms.lock().await.init().unwrap();

    // This provider should never be called since tokens are within budget
    let provider = Arc::new(SummaryMockProvider {
        summary: "should not be called".to_string(),
    });

    let consolidator = Consolidator::new(
        provider,
        sm.clone(),
        ms.clone(),
        8192,
        4096,
    );

    // Prompt tokens well within budget (8192 - 4096 - 512 = 3584 budget)
    consolidator.maybe_consolidate("s1", 100).await.unwrap();

    // No history should have been appended
    let entries = ms.lock().await.read_recent_history(10);
    assert!(entries.is_empty());
}

// ── Full flow: consolidation → reload → summary injection ──

#[tokio::test]
async fn test_consolidation_summary_survives_session_reload() {
    let tmp = TempDir::new().unwrap();
    let session_dir = tmp.path().join("sessions");
    let workspace_dir = tmp.path().join("workspace");
    std::fs::create_dir_all(&session_dir).unwrap();
    std::fs::create_dir_all(&workspace_dir).unwrap();

    // Phase 1: Run consolidation
    {
        let mut sm = SessionManager::new(session_dir.clone()).unwrap();
        sm.get_or_create("s1").await.unwrap();

        for i in 0..5 {
            sm.add_message("s1", Message::user(format!("msg {i}"))).await.unwrap();
            sm.add_message("s1", Message::assistant(Some(format!("reply {i}")), None)).await.unwrap();
        }

        sm.set_last_summary("s1", "- User runs on macOS\n- Chose async/await pattern").await;
        sm.persist("s1").await.unwrap();
    }

    // Phase 2: Reload and verify summary is accessible
    {
        let mut sm = SessionManager::new(session_dir.clone()).unwrap();
        sm.get_or_create("s1").await.unwrap(); // load from disk
        let summary = sm.get_last_summary("s1").await;
        assert!(summary.is_some());
        let summary = summary.unwrap();
        assert!(summary.contains("macOS"));
        assert!(summary.contains("async/await"));
    }

    // Phase 3: Verify the meta file on disk
    {
        let meta_path = session_dir.join("s1.meta.json");
        let meta_content = fs::read_to_string(&meta_path).unwrap();
        assert!(meta_content.contains("last_summary"));
        assert!(meta_content.contains("macOS"));
    }
}

#[tokio::test]
async fn test_context_builder_uses_summary_from_reloaded_session() {
    let tmp = TempDir::new().unwrap();
    let session_dir = tmp.path().join("sessions");
    let workspace_dir = tmp.path().join("workspace");
    std::fs::create_dir_all(&session_dir).unwrap();
    std::fs::create_dir_all(&workspace_dir).unwrap();

    // Phase 1: Create session with summary
    {
        let mut sm = SessionManager::new(session_dir.clone()).unwrap();
        sm.get_or_create("s1").await.unwrap();
        sm.add_message("s1", Message::user("original query".to_string())).await.unwrap();
        sm.set_last_summary("s1", "- User project is a chat bot").await;
        sm.persist("s1").await.unwrap();
    }

    // Phase 2: Reload and build context
    {
        let mut sm = SessionManager::new(session_dir.clone()).unwrap();
        // Trigger reload by getting the session
        sm.get_or_create("s1").await.unwrap();

        let sm: SharedSessionManager = Arc::new(Mutex::new(sm));

        let ms = Arc::new(Mutex::new(MemoryStore::new(&workspace_dir)));
        ms.lock().await.init().unwrap();

        let tm = Arc::new(ToolManager::new(workspace_dir.clone()));
        let cb = ContextBuilder::new(sm.clone(), tm.clone(), workspace_dir.clone(), ms.clone());

        // Get the summary from the reloaded session
        let summary = {
            let guard = sm.lock().await;
            guard.get_last_summary("s1").await
        };

        let ctx = cb.build("s1", None, summary.as_deref(), None, None).await;
        let system_text = match &ctx.messages[0] {
            Message::System { content, .. } => content,
            _ => panic!("expected system message"),
        };

        assert!(system_text.contains("[Resumed Session]"));
        assert!(system_text.contains("chat bot"));
    }
}
