use std::fs;
use std::sync::Arc;

use tempfile::TempDir;
use tokio::sync::Mutex;

use slimbot::{
    Message, SessionManager, TaskHook, TaskState,
    SharedSessionManager,
};

// ── Session creation and ID generation ──

#[test]
fn test_create_id_generates_uuid() {
    let id1 = SessionManager::create_id();
    let id2 = SessionManager::create_id();
    assert_ne!(id1, id2);
    assert!(!id1.is_empty());
    assert!(id1.len() > 20); // UUID format
}

#[tokio::test]
async fn test_new_creates_session_dir() {
    let tmp = TempDir::new().unwrap();
    let session_dir = tmp.path().join("sessions");
    assert!(!session_dir.exists());

    let _sm = SessionManager::new(session_dir.clone()).unwrap();
    assert!(session_dir.exists());
}

// ── Get or create session ──

#[tokio::test]
async fn test_get_or_create_new_session() {
    let tmp = TempDir::new().unwrap();
    let session_dir = tmp.path().join("sessions");
    let mut sm = SessionManager::new(session_dir).unwrap();

    let session = sm.get_or_create("test:chat1").await.unwrap();
    assert_eq!(session.id, "test:chat1");
}

#[tokio::test]
async fn test_get_or_create_existing_returns_same() {
    let tmp = TempDir::new().unwrap();
    let session_dir = tmp.path().join("sessions");
    let mut sm = SessionManager::new(session_dir).unwrap();

    sm.get_or_create("test:chat1").await.unwrap();
    let session2 = sm.get_or_create("test:chat1").await.unwrap();
    assert_eq!(session2.id, "test:chat1");
}

// ── Message management ──

#[tokio::test]
async fn test_add_and_get_messages() {
    let tmp = TempDir::new().unwrap();
    let session_dir = tmp.path().join("sessions");
    let mut sm = SessionManager::new(session_dir).unwrap();

    sm.get_or_create("test:chat1").await.unwrap();
    sm.add_message("test:chat1", Message::user("hello".to_string())).await.unwrap();
    sm.add_message("test:chat1", Message::assistant(Some("hi".to_string()), None)).await.unwrap();

    let messages = sm.get_messages("test:chat1").await;
    assert_eq!(messages.len(), 2);
    assert!(matches!(&messages[0], Message::User { content, .. } if content == "hello"));
}

#[tokio::test]
async fn test_get_messages_unknown_session() {
    let tmp = TempDir::new().unwrap();
    let session_dir = tmp.path().join("sessions");
    let sm = SessionManager::new(session_dir).unwrap();

    let messages = sm.get_messages("nonexistent").await;
    assert!(messages.is_empty());
}

#[tokio::test]
async fn test_add_message_unknown_session_fails() {
    let tmp = TempDir::new().unwrap();
    let session_dir = tmp.path().join("sessions");
    let mut sm = SessionManager::new(session_dir).unwrap();

    let result = sm.add_message("nonexistent", Message::user("x".to_string())).await;
    assert!(result.is_err());
}

// ── JSONL persistence ──

#[tokio::test]
async fn test_persist_creates_jsonl_file() {
    let tmp = TempDir::new().unwrap();
    let session_dir = tmp.path().join("sessions");
    let mut sm = SessionManager::new(session_dir.clone()).unwrap();

    sm.get_or_create("test:chat1").await.unwrap();
    sm.add_message("test:chat1", Message::user("persist me".to_string())).await.unwrap();
    sm.persist("test:chat1").await.unwrap();

    let jsonl_path = session_dir.join("test:chat1.jsonl");
    assert!(jsonl_path.exists());
    let content = fs::read_to_string(&jsonl_path).unwrap();
    assert!(content.contains("persist me"));
}

#[tokio::test]
async fn test_reload_from_jsonl() {
    let tmp = TempDir::new().unwrap();
    let session_dir = tmp.path().join("sessions");

    // First session: add messages and persist
    {
        let mut sm = SessionManager::new(session_dir.clone()).unwrap();
        sm.get_or_create("test:chat1").await.unwrap();
        sm.add_message("test:chat1", Message::user("saved".to_string())).await.unwrap();
        sm.persist("test:chat1").await.unwrap();
    }

    // New SessionManager: load from disk
    {
        let mut sm = SessionManager::new(session_dir).unwrap();
        let session = sm.get_or_create("test:chat1").await.unwrap();
        assert_eq!(session.messages.len(), 1);
        assert!(matches!(&session.messages[0], Message::User { content, .. } if content == "saved"));
    }
}

#[tokio::test]
async fn test_persist_nonexistent_session_fails() {
    let tmp = TempDir::new().unwrap();
    let session_dir = tmp.path().join("sessions");
    let mut sm = SessionManager::new(session_dir).unwrap();

    let result = sm.persist("nonexistent").await;
    assert!(result.is_err());
}

// ── Consolidation cursor ──

#[tokio::test]
async fn test_consolidation_cursor_skips_messages() {
    let tmp = TempDir::new().unwrap();
    let session_dir = tmp.path().join("sessions");
    let mut sm = SessionManager::new(session_dir).unwrap();

    sm.get_or_create("s1").await.unwrap();
    sm.add_message("s1", Message::user("a".to_string())).await.unwrap();    // id=1
    sm.add_message("s1", Message::assistant(Some("b".to_string()), None)).await.unwrap(); // id=2
    sm.add_message("s1", Message::user("c".to_string())).await.unwrap();    // id=3

    // Set cursor to skip first 2 messages (by id)
    sm.update_consolidation_cursor("s1", 2).await;

    let messages = sm.get_messages("s1").await;
    assert_eq!(messages.len(), 1);
    assert!(matches!(&messages[0], Message::User { content, .. } if content == "c"));
}

#[tokio::test]
async fn test_consolidation_persists_and_reloads_cursor() {
    let tmp = TempDir::new().unwrap();
    let session_dir = tmp.path().join("sessions");

    {
        let mut sm = SessionManager::new(session_dir.clone()).unwrap();
        sm.get_or_create("s1").await.unwrap();
        sm.add_message("s1", Message::user("a".to_string())).await.unwrap(); // id=1
        sm.add_message("s1", Message::user("b".to_string())).await.unwrap(); // id=2
        sm.persist("s1").await.unwrap();

        // Consolidate first message (by id) — physically removed from memory
        sm.update_consolidation_cursor("s1", 1).await;
    }

    {
        let mut sm = SessionManager::new(session_dir).unwrap();
        let session = sm.get_or_create("s1").await.unwrap();
        assert_eq!(session.last_consolidated_id(), 1);
        // get_messages should skip consolidated ones
        let msgs = sm.get_messages("s1").await;
        assert_eq!(msgs.len(), 1);
    }
}

// ── SharedSessionManager ──

#[tokio::test]
async fn test_shared_session_manager_concurrent_access() {
    let tmp = TempDir::new().unwrap();
    let session_dir = tmp.path().join("sessions");
    let sm = SessionManager::new(session_dir).unwrap();
    let shared: SharedSessionManager = Arc::new(Mutex::new(sm));

    // Clone references and access from "concurrent" tasks
    let sm1 = shared.clone();
    let sm2 = shared.clone();

    let h1 = tokio::spawn(async move {
        let mut guard = sm1.lock().await;
        guard.get_or_create("shared:s1").await.unwrap();
        guard.add_message("shared:s1", Message::user("from task1".to_string())).await.unwrap();
    });

    let h2 = tokio::spawn(async move {
        tokio::time::sleep(tokio::time::Duration::from_millis(10)).await;
        let mut guard = sm2.lock().await;
        guard.add_message("shared:s1", Message::user("from task2".to_string())).await.unwrap();
    });

    h1.await.unwrap();
    h2.await.unwrap();

    let guard = shared.lock().await;
    let msgs = guard.get_messages("shared:s1").await;
    assert_eq!(msgs.len(), 2);
}

// ── TaskHook ──

#[test]
fn test_task_hook_without_channel() {
    let hook = TaskHook::new("test:chat1");
    // Should not panic when no channel is set
    hook.notify_status_change(&TaskState::Pending);
}

#[tokio::test]
async fn test_task_hook_with_channel() {
    let (tx, mut rx) = tokio::sync::mpsc::channel(10);
    let hook = TaskHook::new("test:chat1").with_status_channel(tx);

    hook.notify_status_change(&TaskState::Running { current_iteration: 0 });

    let received = rx.recv().await.unwrap();
    assert_eq!(received.0, "test:chat1");
    assert!(matches!(received.1, TaskState::Running { current_iteration: 0 }));
}

// ── Message serialization ──

#[test]
fn test_message_serde_round_trip() {
    let msg = Message::user("hello".to_string());
    let json = serde_json::to_string(&msg).unwrap();
    let deserialized: Message = serde_json::from_str(&json).unwrap();
    assert!(matches!(deserialized, Message::User { content, .. } if content == "hello"));
}

#[test]
fn test_message_assistant_with_tool_calls() {
    use slimbot::ToolCall;
    let tool_call = ToolCall {
        id: "call1".to_string(),
        name: "shell".to_string(),
        args: serde_json::json!({"command": "ls"}),
    };
    let msg = Message::assistant(Some("running ls".to_string()), Some(vec![tool_call]));
    let json = serde_json::to_string(&msg).unwrap();
    let deserialized: Message = serde_json::from_str(&json).unwrap();
    assert!(matches!(deserialized, Message::Assistant { tool_calls: Some(calls), .. } if calls.len() == 1));
}

#[test]
fn test_message_tool_serialization() {
    let msg = Message::tool(
        "result".to_string(),
        "call1".to_string(),
        Some("shell".to_string()),
    );
    let json = serde_json::to_string(&msg).unwrap();
    assert!(json.contains("\"role\":\"tool\""));
    let deserialized: Message = serde_json::from_str(&json).unwrap();
    assert!(matches!(deserialized, Message::Tool { content, .. } if content == "result"));
}

// ── Append-only persistence ──

#[tokio::test]
async fn test_append_only_does_not_duplicate_messages() {
    let tmp = TempDir::new().unwrap();
    let session_dir = tmp.path().join("sessions");
    let mut sm = SessionManager::new(session_dir.clone()).unwrap();

    sm.get_or_create("s1").await.unwrap();
    sm.add_message("s1", Message::user("msg1".to_string())).await.unwrap();
    sm.persist("s1").await.unwrap();

    // First persist: 1 line in JSONL
    let jsonl = fs::read_to_string(session_dir.join("s1.jsonl")).unwrap();
    assert_eq!(jsonl.lines().count(), 1);

    sm.add_message("s1", Message::user("msg2".to_string())).await.unwrap();
    sm.persist("s1").await.unwrap();

    // Second persist: 2 lines (appended, not rewritten)
    let jsonl = fs::read_to_string(session_dir.join("s1.jsonl")).unwrap();
    assert_eq!(jsonl.lines().count(), 2);
}

#[tokio::test]
async fn test_meta_file_separate_from_messages() {
    let tmp = TempDir::new().unwrap();
    let session_dir = tmp.path().join("sessions");
    let mut sm = SessionManager::new(session_dir.clone()).unwrap();

    sm.get_or_create("s1").await.unwrap();
    sm.add_message("s1", Message::user("test".to_string())).await.unwrap();
    sm.persist("s1").await.unwrap();

    // Meta file should exist
    assert!(session_dir.join("s1.meta.json").exists());
    let meta_content = fs::read_to_string(session_dir.join("s1.meta.json")).unwrap();
    assert!(meta_content.contains("last_consolidated_id"));
    assert!(meta_content.contains("next_message_id"));

    // JSONL should have no metadata lines — just the message
    let jsonl = fs::read_to_string(session_dir.join("s1.jsonl")).unwrap();
    let lines: Vec<&str> = jsonl.lines().collect();
    assert_eq!(lines.len(), 1);
    assert!(!lines[0].contains("_type"));
}
