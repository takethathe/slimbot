//! Integration tests for context builder runtime context stability.

use std::sync::Arc;

use slimbot::{
    ContextBuilder, MemoryStore, Message, SessionManager, SharedSessionManager, ToolCall,
    ToolManager, build_runtime_context,
};
use tempfile::TempDir;
use tokio::sync::Mutex;

/// Helper to extract runtime_content from the first User message in current_turn.
fn extract_runtime_content(ctx: &slimbot::RunContext) -> Option<String> {
    for msg in &ctx.current_turn {
        if let Message::User {
            runtime_content, ..
        } = msg
        {
            return runtime_content.clone();
        }
    }
    None
}

/// Test that runtime_context remains stable across multiple build_messages calls
/// within the same turn when the same runtime_ctx is provided.
///
/// This simulates a multi-iteration ReAct loop where:
/// 1. User sends a message
/// 2. First build_messages call (iteration 1)
/// 3. Tool is called, assistant + tool messages added to current_turn
/// 4. Second build_messages call (iteration 2)
/// 5. Verify runtime_content is identical in both calls
#[tokio::test]
async fn test_runtime_context_stable_across_iterations() {
    let tmp = TempDir::new().unwrap();
    let session_dir = tmp.path().join("sessions");
    let workspace_dir = tmp.path().join("workspace");
    std::fs::create_dir_all(&session_dir).unwrap();
    std::fs::create_dir_all(&workspace_dir).unwrap();

    // Setup session with a user message
    let mut sm = SessionManager::new(session_dir.clone()).unwrap();
    sm.get_or_create("s1").await.unwrap();
    sm.add_message(
        "s1",
        Message::user("What is the weather today?".to_string()),
    )
    .await
    .unwrap();
    let sm: SharedSessionManager = Arc::new(Mutex::new(sm));

    let ms = Arc::new(Mutex::new(MemoryStore::new(&workspace_dir)));
    ms.lock().await.init().unwrap();

    let tm = Arc::new(ToolManager::new(workspace_dir.clone()));
    let cb = ContextBuilder::new(sm.clone(), tm.clone(), workspace_dir.clone(), ms.clone());

    // Generate runtime_ctx ONCE before the loop (simulating AgentRunner behavior)
    let runtime_ctx = build_runtime_context("webui", "chat-123");

    // ── Iteration 1: First build_messages call ──
    let ctx1 = cb
        .build_messages("s1", "webui", "chat-123", None, Some(runtime_ctx.clone()))
        .await;
    let runtime_content_1 = extract_runtime_content(&ctx1);
    assert!(
        runtime_content_1.is_some(),
        "iteration 1 should have runtime_content"
    );
    let rc1 = runtime_content_1.unwrap();
    assert!(rc1.contains("Current Time:"));
    assert!(rc1.contains("Channel: webui"));
    assert!(rc1.contains("Chat ID: chat-123"));

    // ── Simulate tool call: add assistant + tool messages to current_turn ──
    {
        let mut sm_guard = sm.lock().await;
        // Add assistant message with tool call (simulating LLM response)
        sm_guard
            .add_message(
                "s1",
                Message::assistant(
                    Some("Let me check the weather.".to_string()),
                    Some(vec![ToolCall {
                        id: "call_1".to_string(),
                        name: "shell".to_string(),
                        args: serde_json::json!({"command": "curl wttr.in"}),
                    }]),
                    None,
                    None,
                ),
            )
            .await
            .unwrap();
        // Add tool result
        sm_guard
            .add_message(
                "s1",
                Message::tool(
                    "Weather: Sunny, 25°C".to_string(),
                    "call_1".to_string(),
                    Some("shell".to_string()),
                ),
            )
            .await
            .unwrap();
    }

    // ── Iteration 2: Second build_messages call (simulating next ReAct iteration) ──
    // Small delay to ensure time would be different if regenerated
    tokio::time::sleep(tokio::time::Duration::from_millis(10)).await;

    let ctx2 = cb
        .build_messages("s1", "webui", "chat-123", None, Some(runtime_ctx.clone()))
        .await;
    let runtime_content_2 = extract_runtime_content(&ctx2);
    assert!(
        runtime_content_2.is_some(),
        "iteration 2 should have runtime_content"
    );
    let rc2 = runtime_content_2.unwrap();

    // ── Verify: runtime_content should be IDENTICAL across iterations ──
    assert_eq!(
        rc1, rc2,
        "runtime_content should be stable across iterations within the same turn"
    );

    // Also verify current_turn has more messages now (user + assistant + tool)
    assert_eq!(
        ctx2.current_turn.len(),
        3,
        "current_turn should have user + assistant + tool messages"
    );
}

/// Test that WITHOUT providing runtime_ctx, each build_messages call generates
/// a NEW runtime_content (demonstrating the original problem).
#[tokio::test]
async fn test_runtime_context_changes_without_pregeneration() {
    let tmp = TempDir::new().unwrap();
    let session_dir = tmp.path().join("sessions");
    let workspace_dir = tmp.path().join("workspace");
    std::fs::create_dir_all(&session_dir).unwrap();
    std::fs::create_dir_all(&workspace_dir).unwrap();

    let mut sm = SessionManager::new(session_dir.clone()).unwrap();
    sm.get_or_create("s1").await.unwrap();
    sm.add_message("s1", Message::user("test query".to_string()))
        .await
        .unwrap();
    let sm: SharedSessionManager = Arc::new(Mutex::new(sm));

    let ms = Arc::new(Mutex::new(MemoryStore::new(&workspace_dir)));
    ms.lock().await.init().unwrap();

    let tm = Arc::new(ToolManager::new(workspace_dir.clone()));
    let cb = ContextBuilder::new(sm.clone(), tm.clone(), workspace_dir.clone(), ms.clone());

    // First call without pre-generated runtime_ctx (None)
    let ctx1 = cb.build_messages("s1", "cli", "test", None, None).await;
    let rc1 = extract_runtime_content(&ctx1).unwrap();

    // Wait to ensure time difference
    tokio::time::sleep(tokio::time::Duration::from_millis(1100)).await; // 1.1 seconds

    // Second call without pre-generated runtime_ctx
    let ctx2 = cb.build_messages("s1", "cli", "test", None, None).await;
    let rc2 = extract_runtime_content(&ctx2).unwrap();

    // The timestamps should be DIFFERENT (demonstrating the original problem)
    // Note: This test verifies that None causes regeneration
    assert_ne!(
        rc1, rc2,
        "without pre-generated runtime_ctx, each call should generate new content"
    );
}

/// Test that runtime_context preserves the timestamp from the pre-generated value.
#[tokio::test]
async fn test_runtime_context_uses_pregenerated_timestamp() {
    let tmp = TempDir::new().unwrap();
    let session_dir = tmp.path().join("sessions");
    let workspace_dir = tmp.path().join("workspace");
    std::fs::create_dir_all(&session_dir).unwrap();
    std::fs::create_dir_all(&workspace_dir).unwrap();

    let mut sm = SessionManager::new(session_dir.clone()).unwrap();
    sm.get_or_create("s1").await.unwrap();
    sm.add_message("s1", Message::user("test".to_string()))
        .await
        .unwrap();
    let sm: SharedSessionManager = Arc::new(Mutex::new(sm));

    let ms = Arc::new(Mutex::new(MemoryStore::new(&workspace_dir)));
    ms.lock().await.init().unwrap();

    let tm = Arc::new(ToolManager::new(workspace_dir.clone()));
    let cb = ContextBuilder::new(sm.clone(), tm.clone(), workspace_dir.clone(), ms.clone());

    // Generate runtime_ctx and extract its timestamp
    let runtime_ctx = build_runtime_context("cli", "default");
    let timestamp = runtime_ctx
        .lines()
        .find(|l| l.contains("Current Time:"))
        .map(|l| l.replace("Current Time: ", ""))
        .unwrap();

    // Wait a bit
    tokio::time::sleep(tokio::time::Duration::from_millis(1100)).await;

    // Call build_messages with the pre-generated runtime_ctx
    let ctx = cb
        .build_messages("s1", "cli", "default", None, Some(runtime_ctx.clone()))
        .await;
    let rc = extract_runtime_content(&ctx).unwrap();

    // The timestamp in runtime_content should match the original
    let extracted_timestamp = rc
        .lines()
        .find(|l| l.contains("Current Time:"))
        .map(|l| l.replace("Current Time: ", ""))
        .unwrap();

    assert_eq!(
        timestamp, extracted_timestamp,
        "timestamp should be preserved from pre-generated runtime_ctx"
    );
}
