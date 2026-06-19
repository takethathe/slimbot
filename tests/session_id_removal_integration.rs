use slimbot::{Content, Message, SessionManager};
use std::fs;
use tempfile::TempDir;

// ── total_persisted alignment from disk ──

#[tokio::test]
async fn test_total_persisted_aligned_from_disk_on_reload() {
    let tmp = TempDir::new().unwrap();
    let session_dir = tmp.path().join("sessions");

    // Phase 1: create and persist messages
    {
        let mut sm = SessionManager::new(session_dir.clone()).unwrap();
        let session = sm.get_or_create("test:chat1").await.unwrap();
        assert_eq!(session.id, "test:chat1");
        sm.add_message("test:chat1", Message::user("msg1".to_string()))
            .await
            .unwrap();
        sm.add_message("test:chat1", Message::user("msg2".to_string()))
            .await
            .unwrap();
        sm.add_message("test:chat1", Message::user("msg3".to_string()))
            .await
            .unwrap();
        sm.persist("test:chat1").await.unwrap();

        let session = sm.get_or_create("test:chat1").await.unwrap();
        assert_eq!(session.total_persisted(), 3);
    }

    // Phase 2: reload and verify total_persisted aligned from disk
    {
        let mut sm = SessionManager::new(session_dir).unwrap();
        let session = sm.get_or_create("test:chat1").await.unwrap();
        assert_eq!(session.total_persisted(), 3);
        assert_eq!(session.history.len(), 3);
    }
}

// ── consolidated_lines persists and survives reload ──

#[tokio::test]
async fn test_consolidated_lines_persists_and_reloads() {
    let tmp = TempDir::new().unwrap();
    let session_dir = tmp.path().join("sessions");

    // Phase 1: create, consolidate, persist
    {
        let mut sm = SessionManager::new(session_dir.clone()).unwrap();
        sm.get_or_create("s1").await.unwrap();
        sm.add_message("s1", Message::user("a".to_string()))
            .await
            .unwrap();
        sm.add_message("s1", Message::user("b".to_string()))
            .await
            .unwrap();
        sm.add_message("s1", Message::user("c".to_string()))
            .await
            .unwrap();
        sm.persist("s1").await.unwrap();

        sm.update_consolidated_lines("s1", 2).await;
    }

    // Phase 2: reload and verify consolidation cursor
    {
        let mut sm = SessionManager::new(session_dir).unwrap();
        let session = sm.get_or_create("s1").await.unwrap();
        assert_eq!(session.consolidated_lines(), 2);
        // Only 1 message should be loaded (3 on disk - 2 consolidated = 1)
        assert_eq!(session.history.len(), 1);
        let msgs = sm.get_messages("s1").await;
        assert_eq!(msgs.len(), 1);
    }
}

// ── JSONL no longer contains id field ──

#[tokio::test]
async fn test_persisted_jsonl_does_not_contain_id_field() {
    let tmp = TempDir::new().unwrap();
    let session_dir = tmp.path().join("sessions");
    let mut sm = SessionManager::new(session_dir.clone()).unwrap();

    sm.get_or_create("s1").await.unwrap();
    sm.add_message("s1", Message::user("hello".to_string()))
        .await
        .unwrap();
    sm.persist("s1").await.unwrap();

    let jsonl = fs::read_to_string(session_dir.join("s1.jsonl")).unwrap();
    // The serialized message should NOT have an "id" field
    assert!(
        !jsonl.contains("\"id\":"),
        "JSONL should not contain id field, got: {jsonl}"
    );
    assert!(jsonl.contains("hello"));
}

// ── Meta file contains new fields ──

#[tokio::test]
async fn test_meta_file_contains_new_fields() {
    let tmp = TempDir::new().unwrap();
    let session_dir = tmp.path().join("sessions");
    let mut sm = SessionManager::new(session_dir.clone()).unwrap();

    sm.get_or_create("s1").await.unwrap();
    sm.add_message("s1", Message::user("test".to_string()))
        .await
        .unwrap();
    sm.persist("s1").await.unwrap();

    let meta_content = fs::read_to_string(session_dir.join("s1.meta.json")).unwrap();
    assert!(meta_content.contains("consolidated_lines"));
    assert!(meta_content.contains("total_persisted"));
    assert!(!meta_content.contains("last_consolidated_id"));
    assert!(!meta_content.contains("next_message_id"));
}

// ── Multiple consolidate rounds increment consolidated_lines correctly ──

#[tokio::test]
async fn test_multiple_consolidate_rounds_increment_correctly() {
    let tmp = TempDir::new().unwrap();
    let session_dir = tmp.path().join("sessions");
    let mut sm = SessionManager::new(session_dir.clone()).unwrap();
    sm.get_or_create("s1").await.unwrap();

    // Add 6 messages and persist
    for i in 0..6 {
        sm.add_message("s1", Message::user(format!("msg{i}")))
            .await
            .unwrap();
    }
    sm.persist("s1").await.unwrap();

    {
        let session = sm.get_or_create("s1").await.unwrap();
        assert_eq!(session.total_persisted(), 6);
    }

    // First consolidate: remove 2
    sm.update_consolidated_lines("s1", 2).await;
    {
        let session = sm.get_or_create("s1").await.unwrap();
        assert_eq!(session.consolidated_lines(), 2);
        assert_eq!(session.history.len(), 4);
    }

    // Second consolidate: remove 2 more
    sm.update_consolidated_lines("s1", 2).await;
    {
        let session = sm.get_or_create("s1").await.unwrap();
        assert_eq!(session.consolidated_lines(), 2); // rewrite resets to 0, then +2
        assert_eq!(session.history.len(), 2);
    }

    // Persist and reload
    sm.persist("s1").await.unwrap();

    let mut sm2 = SessionManager::new(session_dir).unwrap();
    let session = sm2.get_or_create("s1").await.unwrap();
    // Rewrite resets consolidated_lines to 0, total_persisted to merged len
    assert_eq!(session.consolidated_lines(), 0);
    assert_eq!(session.total_persisted(), 2); // only msg4, msg5 on disk
    assert_eq!(session.history.len(), 2);
    let msgs = sm2.get_messages("s1").await;
    assert_eq!(msgs.len(), 2);
}

// ── Clear session resets all counters ──

#[tokio::test]
async fn test_clear_session_resets_counters() {
    let tmp = TempDir::new().unwrap();
    let session_dir = tmp.path().join("sessions");
    let mut sm = SessionManager::new(session_dir).unwrap();
    sm.get_or_create("s1").await.unwrap();

    sm.add_message("s1", Message::user("a".to_string()))
        .await
        .unwrap();
    sm.persist("s1").await.unwrap();
    sm.update_consolidated_lines("s1", 1).await;

    sm.clear_session("s1");

    let session = sm.get_or_create("s1").await.unwrap();
    assert_eq!(session.consolidated_lines(), 0);
    assert_eq!(session.total_persisted(), 0);
    assert!(session.history.is_empty());
}

// ── Timestamp is still set on messages ──

#[tokio::test]
async fn test_timestamp_still_set_without_id() {
    let tmp = TempDir::new().unwrap();
    let session_dir = tmp.path().join("sessions");
    let mut sm = SessionManager::new(session_dir.clone()).unwrap();
    sm.get_or_create("s1").await.unwrap();

    let before = chrono::Local::now().to_rfc3339();
    sm.add_message("s1", Message::user("hello".to_string()))
        .await
        .unwrap();
    let after = chrono::Local::now().to_rfc3339();

    let msgs = sm.get_messages("s1").await;
    assert_eq!(msgs.len(), 1);
    let ts = msgs[0].timestamp();
    assert!(!ts.is_empty(), "timestamp should not be empty");
    assert!(
        ts >= before.as_str() && ts <= after.as_str(),
        "timestamp {} should be in [{}, {}]",
        ts,
        before,
        after
    );
}
