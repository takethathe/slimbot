use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Result;
use async_trait::async_trait;
use tokio::sync::Mutex;

use super::*;
use crate::memory::MemoryService;
use crate::provider::{FinishReason, LLMResponse, Provider, Usage};
use crate::session::AgentEvent;
use crate::session::Content;
use crate::session::{Message, SessionManager, SessionTask, TaskHook, TaskState};
use crate::tool::{
    Tool, ToolCall, ToolDefinition, ensure_nonempty_tool_result, format_tool_error,
    persist_tool_result, truncate_text_head_tail,
};
use crate::trace::SpanRecorder;
use tokio::sync::broadcast;

/// Mock provider that returns predefined responses in order.
struct MockProvider {
    responses: std::sync::Mutex<Vec<LLMResponse>>,
    call_count: std::sync::atomic::AtomicU32,
}

impl MockProvider {
    fn new(responses: Vec<LLMResponse>) -> Self {
        Self {
            responses: std::sync::Mutex::new(responses),
            call_count: std::sync::atomic::AtomicU32::new(0),
        }
    }

    fn call_count(&self) -> u32 {
        self.call_count.load(std::sync::atomic::Ordering::SeqCst)
    }
}

#[async_trait]
impl Provider for MockProvider {
    async fn chat(
        &self,
        _messages: &[&Message],
        _tools: Option<&[ToolDefinition]>,
        _recorder: Option<&dyn SpanRecorder>,
    ) -> Result<LLMResponse> {
        let idx = self
            .call_count
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let responses = self.responses.lock().unwrap();
        let resp = responses
            .get(idx as usize)
            .unwrap_or_else(|| panic!("MockProvider: no more responses for call {}", idx));
        Ok(resp.clone())
    }
}

/// Mock tool that returns a fixed string.
struct MockEchoTool {
    name: String,
    return_value: String,
}

impl MockEchoTool {
    fn new(name: &str, return_value: &str) -> Self {
        Self {
            name: name.to_string(),
            return_value: return_value.to_string(),
        }
    }
}

#[async_trait]
impl Tool for MockEchoTool {
    fn name(&self) -> &str {
        &self.name
    }

    fn description(&self) -> &str {
        "A mock tool for testing"
    }

    fn parameters(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {},
        })
    }

    async fn execute(&self, _args: serde_json::Value) -> Result<String> {
        Ok(self.return_value.clone())
    }
}

/// Tool that records the turn-context channel it observes during execute.
struct TurnCaptureTool {
    seen: Arc<std::sync::Mutex<Option<String>>>,
}

#[async_trait]
impl Tool for TurnCaptureTool {
    fn name(&self) -> &str {
        "capture"
    }
    fn description(&self) -> &str {
        "capture turn context for tests"
    }
    fn parameters(&self) -> serde_json::Value {
        serde_json::json!({ "type": "object", "properties": {} })
    }
    async fn execute(&self, _args: serde_json::Value) -> Result<String> {
        let ch = crate::tool::current_turn().map(|t| t.channel().to_string());
        *self.seen.lock().unwrap() = ch;
        Ok("captured".to_string())
    }
}

#[tokio::test]
async fn test_runner_wires_turn_context_to_tools() {
    let tmp = tempfile::TempDir::new().unwrap();
    let path = tmp.path();
    let session_dir = path.join("sessions");
    let workspace_dir = path.join("workspace");
    std::fs::create_dir_all(&workspace_dir).unwrap();

    let sm: SharedSessionManager = Arc::new(Mutex::new(SessionManager::new(session_dir).unwrap()));
    let seen = Arc::new(std::sync::Mutex::new(None));
    let mut tm = ToolManager::new(workspace_dir.clone());
    tm.register(Box::new(TurnCaptureTool { seen: seen.clone() }));
    let tm = Arc::new(tm);
    sm.lock().await.get_or_create("webui:chat-7").await.unwrap();

    let ms = Arc::new(tokio::sync::Mutex::new(MemoryService::new(&workspace_dir)));
    ms.lock().await.init().unwrap();

    let provider = Arc::new(MockProvider::new(vec![
        LLMResponse {
            content: Some("call it".to_string()),
            tool_calls: Some(vec![tool_call("tc-1", "capture")]),
            finish_reason: FinishReason::ToolCalls,
            usage: Usage::default(),
        },
        LLMResponse {
            content: Some("done".to_string()),
            tool_calls: None,
            finish_reason: FinishReason::Stop,
            usage: Usage::default(),
        },
    ]));
    let runner = make_runner(sm.clone(), tm, provider, workspace_dir, ms);

    let result = runner
        .run(
            "hi".to_string(),
            TaskHook::new("webui:chat-7"),
            "webui:chat-7",
            None,
            None,
            None,
            None,
        )
        .await;
    assert!(result.success);

    let captured = seen.lock().unwrap().clone();
    assert_eq!(
        captured.as_deref(),
        Some("webui"),
        "tool must observe the turn context channel from session_id"
    );
}

/// Tool that marks the turn as having delivered output to its origin
/// (mirrors MessageTool's origin-marking behavior).
struct MarkingTool;

#[async_trait]
impl Tool for MarkingTool {
    fn name(&self) -> &str {
        "marker"
    }

    fn description(&self) -> &str {
        "mark the turn as sent for tests"
    }

    fn parameters(&self) -> serde_json::Value {
        serde_json::json!({ "type": "object", "properties": {} })
    }

    async fn execute(&self, _args: serde_json::Value) -> Result<String> {
        if let Some(t) = crate::tool::current_turn() {
            t.mark_sent_if_target(t.channel(), t.chat_id());
        }
        Ok("marked".to_string())
    }
}

#[tokio::test]
async fn test_runner_message_sent_on_max_iterations_exit() {
    let tmp = tempfile::TempDir::new().unwrap();
    let path = tmp.path();
    let session_dir = path.join("sessions");
    let workspace_dir = path.join("workspace");
    std::fs::create_dir_all(&workspace_dir).unwrap();

    let sm: SharedSessionManager = Arc::new(Mutex::new(SessionManager::new(session_dir).unwrap()));
    let mut tm = ToolManager::new(workspace_dir.clone());
    tm.register(Box::new(MarkingTool));
    let tm = Arc::new(tm);
    let ms = Arc::new(tokio::sync::Mutex::new(MemoryService::new(&workspace_dir)));
    ms.lock().await.init().unwrap();
    sm.lock().await.get_or_create("webui:chat-7").await.unwrap();

    // Provider always returns tool calls, forcing the max-iterations exit.
    let base_response = LLMResponse {
        content: None,
        tool_calls: Some(vec![tool_call("tc", "marker")]),
        finish_reason: FinishReason::ToolCalls,
        usage: Usage::default(),
    };
    let provider = Arc::new(MockProvider::new(
        std::iter::repeat_n(base_response, 50).collect(),
    ));
    let runner = make_runner(sm.clone(), tm, provider.clone(), workspace_dir, ms);

    let result = runner
        .run(
            "Loop forever".to_string(),
            TaskHook::new("webui:chat-7"),
            "webui:chat-7",
            None,
            None,
            None,
            None,
        )
        .await;
    assert!(!result.success);
    assert!(result.content.contains("Reached max iterations"));
    assert!(
        result.message_sent,
        "message_sent must reflect delivery even on the max-iterations exit"
    );
}

/// Failing tool that always returns an error.
struct FailingTool;

#[async_trait]
impl Tool for FailingTool {
    fn name(&self) -> &str {
        "fail_tool"
    }

    fn description(&self) -> &str {
        "Always fails"
    }

    fn parameters(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {},
        })
    }

    async fn execute(&self, _args: serde_json::Value) -> Result<String> {
        Err(anyhow::anyhow!("Tool execution failed"))
    }
}

/// Provider wrapper that cancels after the first call.
struct CancellingProvider {
    inner: Arc<dyn Provider>,
    cancel_token: CancellationToken,
    call_count: std::sync::atomic::AtomicU32,
}

impl CancellingProvider {
    fn new(inner: Arc<dyn Provider>, cancel_token: CancellationToken) -> Self {
        Self {
            inner,
            cancel_token,
            call_count: std::sync::atomic::AtomicU32::new(0),
        }
    }
}

#[async_trait]
impl Provider for CancellingProvider {
    async fn chat(
        &self,
        messages: &[&Message],
        tools: Option<&[ToolDefinition]>,
        recorder: Option<&dyn SpanRecorder>,
    ) -> Result<LLMResponse> {
        let count = self
            .call_count
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        // Cancel on the first call so the next cancellation check detects it.
        if count == 0 {
            self.cancel_token.cancel();
        }
        self.inner.chat(messages, tools, recorder).await
    }
}

/// Provider that always returns an error.
struct FailingProvider;

#[async_trait]
impl Provider for FailingProvider {
    async fn chat(
        &self,
        _messages: &[&Message],
        _tools: Option<&[ToolDefinition]>,
        _recorder: Option<&dyn SpanRecorder>,
    ) -> Result<LLMResponse> {
        Err(anyhow::anyhow!("Simulated LLM API failure"))
    }
}

fn tool_call(id: &str, name: &str) -> ToolCall {
    ToolCall {
        id: id.to_string(),
        name: name.to_string(),
        args: serde_json::json!({}),
    }
}

/// Set up test environment: session manager, tool manager, workspace dir.
/// Pre-creates the "test-session" session.
async fn make_test_env(
    tmp_dir: &tempfile::TempDir,
    tool_names: &[(&str, &str)],
) -> (
    SharedSessionManager,
    Arc<ToolManager>,
    PathBuf,
    Arc<tokio::sync::Mutex<MemoryService>>,
) {
    let path = tmp_dir.path();
    let session_dir = path.join("sessions");
    let workspace_dir = path.join("workspace");
    std::fs::create_dir_all(&workspace_dir).unwrap();

    let sm: SharedSessionManager = Arc::new(Mutex::new(SessionManager::new(session_dir).unwrap()));
    let mut tm = ToolManager::new(workspace_dir.clone());
    for (name, ret) in tool_names {
        tm.register(Box::new(MockEchoTool::new(name, ret)));
    }

    let ms = Arc::new(tokio::sync::Mutex::new(MemoryService::new(&workspace_dir)));
    ms.lock().await.init().unwrap();

    // Pre-create the default test session
    sm.lock().await.get_or_create("test-session").await.unwrap();

    (sm, Arc::new(tm), workspace_dir, ms)
}

fn make_task(content: &str) -> SessionTask {
    SessionTask {
        id: "task-1".to_string(),
        session_id: "test-session".to_string(),
        content: content.to_string(),
        hook: TaskHook::new("test-session"),
        state: TaskState::Pending,
        closure: None,
    }
}

fn make_runner(
    sm: SharedSessionManager,
    tm: Arc<ToolManager>,
    provider: Arc<dyn Provider>,
    workspace_dir: PathBuf,
    ms: Arc<tokio::sync::Mutex<MemoryService>>,
) -> AgentRunner {
    make_runner_with_tracer(sm, tm, provider, workspace_dir, ms, None)
}

/// Like [`make_runner`], but with an explicit tracer (used by trace tests).
fn make_runner_with_tracer(
    sm: SharedSessionManager,
    tm: Arc<ToolManager>,
    provider: Arc<dyn Provider>,
    workspace_dir: PathBuf,
    ms: Arc<tokio::sync::Mutex<MemoryService>>,
    tracer: Option<Arc<crate::trace::Tracer>>,
) -> AgentRunner {
    AgentRunner::new(
        tm,
        provider,
        sm,
        AgentConfig {
            provider: "test".to_string(),
            max_iterations: 40,
            timeout_seconds: 120,
            persist_tool_results: false,
            unknown: Default::default(),
        },
        workspace_dir,
        ms,
        None,
        4096,
        tracer,
    )
}

#[tokio::test]
async fn test_direct_reply_no_tool_calls() {
    let tmp = tempfile::TempDir::new().unwrap();
    let (sm, tm, wd, ms) = make_test_env(&tmp, &[]).await;
    let provider = Arc::new(MockProvider::new(vec![LLMResponse {
        content: Some("Hello, world!".to_string()),
        tool_calls: None,
        finish_reason: FinishReason::Stop,
        usage: Usage {
            prompt_tokens: 10,
            prompt_cache_hit_tokens: 0,
            completion_tokens: 5,
            total_tokens: 15,
        },
    }]));
    let runner = make_runner(sm.clone(), tm, provider.clone(), wd, ms);
    let task = make_task("Say hi");

    let result = runner
        .run(
            task.content.clone(),
            task.hook.clone(),
            "test-session",
            None,
            None,
            None,
            None,
        )
        .await;
    assert_eq!(result.content, "Hello, world!");
    assert!(result.success);
    assert_eq!(result.total_tokens, 15);
    assert_eq!(provider.call_count(), 1);

    let msgs = sm.lock().await.get_messages("test-session").await;
    assert_eq!(msgs.len(), 2); // user + assistant (system prompt is not stored in session)
    assert!(
        matches!(&msgs[0], Message::User { content, .. } if matches!(content, Content::Plain(c) if c == "Say hi"))
    );
    assert!(
        matches!(&msgs[1], Message::Assistant { content: Some(c), tool_calls: None, .. } if c == "Hello, world!")
    );
}

#[tokio::test]
async fn test_single_tool_call_with_content() {
    let tmp = tempfile::TempDir::new().unwrap();
    let (sm, tm, wd, ms) = make_test_env(&tmp, &[("echo", "echo-result")]).await;
    let provider = Arc::new(MockProvider::new(vec![
        LLMResponse {
            content: Some("Let me echo that for you.".to_string()),
            tool_calls: Some(vec![tool_call("tc-1", "echo")]),
            finish_reason: FinishReason::ToolCalls,
            usage: Usage {
                prompt_tokens: 20,
                prompt_cache_hit_tokens: 0,
                completion_tokens: 10,
                total_tokens: 30,
            },
        },
        LLMResponse {
            content: Some("Done!".to_string()),
            tool_calls: None,
            finish_reason: FinishReason::Stop,
            usage: Usage {
                prompt_tokens: 40,
                prompt_cache_hit_tokens: 0,
                completion_tokens: 5,
                total_tokens: 45,
            },
        },
    ]));
    let runner = make_runner(sm.clone(), tm, provider.clone(), wd, ms);
    let task = make_task("Run echo");

    let result = runner
        .run(
            task.content.clone(),
            task.hook.clone(),
            "test-session",
            None,
            None,
            None,
            None,
        )
        .await;
    assert_eq!(result.content, "Done!");
    assert!(result.success);
    assert_eq!(result.total_tokens, 75); // 30 + 45
    assert_eq!(result.prompt_cache_hit_tokens, 0);
    assert_eq!(provider.call_count(), 2);

    let msgs = sm.lock().await.get_messages("test-session").await;
    // user + assistant(content+tool_calls) + tool + assistant(final)
    assert_eq!(msgs.len(), 4);
    match &msgs[1] {
        Message::Assistant {
            content,
            tool_calls,
            ..
        } => {
            assert_eq!(content.as_deref(), Some("Let me echo that for you."));
            let calls = tool_calls.as_ref().unwrap();
            assert_eq!(calls.len(), 1);
            assert_eq!(calls[0].id, "tc-1");
        }
        other => panic!(
            "Expected Assistant with content and tool_calls, got {:?}",
            other
        ),
    }
    match &msgs[2] {
        Message::Tool {
            content,
            tool_call_id,
            ..
        } => {
            assert_eq!(content, "echo-result");
            assert_eq!(tool_call_id, "tc-1");
        }
        other => panic!("Expected Tool message, got {:?}", other),
    }
}

#[tokio::test]
async fn test_multiple_tool_calls_content_preserved() {
    let tmp = tempfile::TempDir::new().unwrap();
    let (sm, tm, wd, ms) = make_test_env(
        &tmp,
        &[("list", "listing-contents"), ("read", "file-contents")],
    )
    .await;
    let provider = Arc::new(MockProvider::new(vec![
        LLMResponse {
            content: Some("I'll check both the list and the file for you.".to_string()),
            tool_calls: Some(vec![
                tool_call("tc-list", "list"),
                tool_call("tc-read", "read"),
            ]),
            finish_reason: FinishReason::ToolCalls,
            usage: Usage {
                prompt_tokens: 50,
                prompt_cache_hit_tokens: 0,
                completion_tokens: 20,
                total_tokens: 70,
            },
        },
        LLMResponse {
            content: Some("Here are the results: listing-contents and file-contents.".to_string()),
            tool_calls: None,
            finish_reason: FinishReason::Stop,
            usage: Usage {
                prompt_tokens: 80,
                prompt_cache_hit_tokens: 0,
                completion_tokens: 15,
                total_tokens: 95,
            },
        },
    ]));
    let runner = make_runner(sm.clone(), tm, provider.clone(), wd, ms);
    let task = make_task("List and read");

    let result = runner
        .run(
            task.content.clone(),
            task.hook.clone(),
            "test-session",
            None,
            None,
            None,
            None,
        )
        .await;
    assert_eq!(
        result.content,
        "Here are the results: listing-contents and file-contents."
    );
    assert!(result.success);
    assert_eq!(result.total_tokens, 165); // 70 + 95
    assert_eq!(provider.call_count(), 2);

    let msgs = sm.lock().await.get_messages("test-session").await;
    // user + assistant(content+2tool_calls) + tool + tool + assistant(final)
    assert_eq!(msgs.len(), 5);

    // Verify assistant message has both content and both tool_calls
    match &msgs[1] {
        Message::Assistant {
            content,
            tool_calls,
            ..
        } => {
            assert_eq!(
                content.as_deref(),
                Some("I'll check both the list and the file for you.")
            );
            let calls = tool_calls.as_ref().unwrap();
            assert_eq!(calls.len(), 2);
            assert_eq!(calls[0].name, "list");
            assert_eq!(calls[1].name, "read");
        }
        other => panic!(
            "Expected Assistant with content and 2 tool_calls, got {:?}",
            other
        ),
    }

    // Verify both tool results are recorded
    assert!(
        matches!(&msgs[2], Message::Tool { content, tool_call_id, .. } if content == "listing-contents" && tool_call_id == "tc-list")
    );
    assert!(
        matches!(&msgs[3], Message::Tool { content, tool_call_id, .. } if content == "file-contents" && tool_call_id == "tc-read")
    );
}

#[tokio::test]
async fn test_multiple_tool_calls_with_empty_content() {
    let tmp = tempfile::TempDir::new().unwrap();
    let (sm, tm, wd, ms) =
        make_test_env(&tmp, &[("tool_a", "result-a"), ("tool_b", "result-b")]).await;
    let provider = Arc::new(MockProvider::new(vec![
        LLMResponse {
            content: None,
            tool_calls: Some(vec![
                tool_call("tc-a", "tool_a"),
                tool_call("tc-b", "tool_b"),
            ]),
            finish_reason: FinishReason::ToolCalls,
            usage: Usage::default(),
        },
        LLMResponse {
            content: Some("Final answer".to_string()),
            tool_calls: None,
            finish_reason: FinishReason::Stop,
            usage: Usage::default(),
        },
    ]));
    let runner = make_runner(sm.clone(), tm, provider.clone(), wd, ms);
    let task = make_task("Run two tools");

    let result = runner
        .run(
            task.content.clone(),
            task.hook.clone(),
            "test-session",
            None,
            None,
            None,
            None,
        )
        .await;
    assert_eq!(result.content, "Final answer");
    assert!(result.success);

    let msgs = sm.lock().await.get_messages("test-session").await;
    match &msgs[1] {
        Message::Assistant {
            content,
            tool_calls,
            ..
        } => {
            assert!(content.is_none());
            assert_eq!(tool_calls.as_ref().unwrap().len(), 2);
        }
        other => panic!("Expected Assistant, got {:?}", other),
    }
}

#[tokio::test]
async fn test_max_iterations_exceeded() {
    let tmp = tempfile::TempDir::new().unwrap();
    let (sm, tm, wd, ms) = make_test_env(&tmp, &[("loop_tool", "x")]).await;
    // Provider always returns tool_calls, forcing the loop to continue
    let base_response = LLMResponse {
        content: None,
        tool_calls: Some(vec![tool_call("tc", "loop_tool")]),
        finish_reason: FinishReason::ToolCalls,
        usage: Usage::default(),
    };
    let provider = Arc::new(MockProvider::new(
        std::iter::repeat_n(base_response, 50).collect(),
    ));
    let runner = make_runner(sm.clone(), tm, provider.clone(), wd, ms);
    let task = make_task("Loop forever");

    let result = runner
        .run(
            task.content.clone(),
            task.hook.clone(),
            "test-session",
            None,
            None,
            None,
            None,
        )
        .await;
    assert!(!result.success);
    assert!(result.content.contains("Reached max iterations 40"));

    // Verify session was persisted
    let msgs = sm.lock().await.get_messages("test-session").await;
    assert!(!msgs.is_empty());

    let jsonl_path = tmp.path().join("sessions/test-session.jsonl");
    assert!(jsonl_path.exists());
}

#[tokio::test]
async fn test_task_state_transitions() {
    let tmp = tempfile::TempDir::new().unwrap();
    let (sm, tm, wd, ms) = make_test_env(&tmp, &[("tool1", "r1")]).await;

    // Capture state changes via TaskHook
    let (status_tx, mut status_rx) = tokio::sync::mpsc::channel::<(String, TaskState)>(32);

    let provider = Arc::new(MockProvider::new(vec![
        LLMResponse {
            content: Some("running tool".to_string()),
            tool_calls: Some(vec![tool_call("tc-1", "tool1")]),
            finish_reason: FinishReason::ToolCalls,
            usage: Usage::default(),
        },
        LLMResponse {
            content: Some("done".to_string()),
            tool_calls: None,
            finish_reason: FinishReason::Stop,
            usage: Usage::default(),
        },
    ]));
    let runner = make_runner(sm.clone(), tm, provider, wd, ms);
    let task = make_task("test");
    let hook = TaskHook::new("test-session").with_status_channel(status_tx);
    let content = task.content.clone();

    let _ = runner
        .run(content, hook, "test-session", None, None, None, None)
        .await;

    // Collect all status events
    let mut states = Vec::new();
    while let Ok((_, state)) = status_rx.try_recv() {
        states.push(state);
    }

    // Should have: Running(0), Running(1), Completed
    assert!(states.len() >= 2);
    assert!(
        matches!(&states[0], TaskState::Running { current_iteration } if *current_iteration == 0)
    );
    // Last state should be Completed
    assert!(matches!(
        states.last().unwrap(),
        TaskState::Completed { .. }
    ));
}

#[tokio::test]
async fn test_agent_event_sequence() {
    let tmp = tempfile::TempDir::new().unwrap();
    let (sm, tm, wd, ms) = make_test_env(&tmp, &[("tool1", "r1")]).await;

    let (event_tx, mut event_rx) = broadcast::channel::<AgentEvent>(32);

    let provider = Arc::new(MockProvider::new(vec![
        LLMResponse {
            content: Some("running tool".to_string()),
            tool_calls: Some(vec![tool_call("tc-1", "tool1")]),
            finish_reason: FinishReason::ToolCalls,
            usage: Usage::default(),
        },
        LLMResponse {
            content: Some("done".to_string()),
            tool_calls: None,
            finish_reason: FinishReason::Stop,
            usage: Usage::default(),
        },
    ]));
    let runner = make_runner(sm.clone(), tm, provider, wd, ms);
    let task = make_task("test");
    let hook = TaskHook::new("test-session").with_events(event_tx);
    let content = task.content.clone();

    let result = runner
        .run(content, hook, "test-session", None, None, None, None)
        .await;

    assert!(result.success);

    // Collect all events
    let mut events = Vec::new();
    while let Ok(evt) = event_rx.try_recv() {
        events.push(evt);
    }

    assert!(!events.is_empty(), "should have emitted events");
    assert!(
        matches!(events[0], AgentEvent::TaskStarted { .. }),
        "first event should be TaskStarted, got {:?}",
        events[0]
    );
    assert!(
        matches!(events.last().unwrap(), AgentEvent::TaskCompleted { .. }),
        "last event should be TaskCompleted, got {:?}",
        events.last().unwrap()
    );
    assert!(
        events
            .iter()
            .any(|e| matches!(e, AgentEvent::ToolCall { name, .. } if name == "tool1")),
        "should contain ToolCall for tool1"
    );
    assert!(
        events
            .iter()
            .any(|e| matches!(e, AgentEvent::ToolResult { name, .. } if name == "tool1")),
        "should contain ToolResult for tool1"
    );
}

#[tokio::test]
async fn test_tool_error_continues_loop() {
    let tmp = tempfile::TempDir::new().unwrap();
    let path = tmp.path();
    let session_dir = path.join("sessions");
    let workspace_dir = path.join("workspace");
    std::fs::create_dir_all(&workspace_dir).unwrap();

    let sm: SharedSessionManager = Arc::new(Mutex::new(SessionManager::new(session_dir).unwrap()));
    let mut tm = ToolManager::new(workspace_dir.clone());
    tm.register(Box::new(FailingTool));
    let tm = Arc::new(tm);
    sm.lock().await.get_or_create("test-session").await.unwrap();

    let ms = Arc::new(tokio::sync::Mutex::new(MemoryService::new(&workspace_dir)));
    ms.lock().await.init().unwrap();

    // Provider returns tool_error first, then a final reply after seeing the error
    let provider = Arc::new(MockProvider::new(vec![
        LLMResponse {
            content: Some("trying...".to_string()),
            tool_calls: Some(vec![tool_call("tc-fail", "fail_tool")]),
            finish_reason: FinishReason::ToolCalls,
            usage: Usage::default(),
        },
        LLMResponse {
            content: Some("Got the error, stopping.".to_string()),
            tool_calls: None,
            finish_reason: FinishReason::Stop,
            usage: Usage::default(),
        },
    ]));
    let runner = make_runner(sm.clone(), tm, provider, workspace_dir, ms);
    let task = make_task("test");

    let result = runner
        .run(
            task.content.clone(),
            task.hook.clone(),
            "test-session",
            None,
            None,
            None,
            None,
        )
        .await;
    assert_eq!(result.content, "Got the error, stopping.");

    let msgs = sm.lock().await.get_messages("test-session").await;
    // user + assistant(tool_calls) + tool(error) + assistant(final)
    assert_eq!(msgs.len(), 4);
    assert!(matches!(&msgs[2], Message::Tool { content, .. } if content.contains("Error:")));
    assert!(
        matches!(&msgs[2], Message::Tool { content, .. } if content.contains("Analyze the error"))
    );
}

#[tokio::test]
async fn test_persistence_and_reload() {
    let tmp = tempfile::TempDir::new().unwrap();
    let (sm, tm, wd, ms) = make_test_env(&tmp, &[]).await;

    let provider = Arc::new(MockProvider::new(vec![LLMResponse {
        content: Some("persist me".to_string()),
        tool_calls: None,
        finish_reason: FinishReason::Stop,
        usage: Usage::default(),
    }]));
    let runner = make_runner(sm.clone(), tm, provider, wd, ms);
    let task = make_task("hello");

    let result = runner
        .run(
            task.content.clone(),
            task.hook.clone(),
            "test-session",
            None,
            None,
            None,
            None,
        )
        .await;
    assert_eq!(result.content, "persist me");

    // Create a new SessionManager and reload from disk
    let session_dir = tmp.path().join("sessions");
    let sm2: SharedSessionManager = Arc::new(Mutex::new(SessionManager::new(session_dir).unwrap()));
    sm2.lock()
        .await
        .get_or_create("test-session")
        .await
        .unwrap();

    let msgs = sm2.lock().await.get_messages("test-session").await;
    assert_eq!(msgs.len(), 2); // user + assistant
    assert!(
        matches!(&msgs[0], Message::User { content, .. } if matches!(content, Content::Plain(c) if c == "hello"))
    );
    assert!(
        matches!(&msgs[1], Message::Assistant { content: Some(c), tool_calls: None, .. } if c == "persist me")
    );
}

#[test]
fn test_ensure_nonempty_tool_result() {
    assert_eq!(
        ensure_nonempty_tool_result("echo", ""),
        "(echo completed with no output)"
    );
    assert_eq!(
        ensure_nonempty_tool_result("shell", "   "),
        "(shell completed with no output)"
    );
    assert_eq!(ensure_nonempty_tool_result("echo", "hello"), "hello");
}

#[test]
fn test_truncate_text_head_tail() {
    let short = "hello world";
    assert_eq!(truncate_text_head_tail(short, 100), short);

    // Long text: head(2000) + tail(2000)
    let long = "A".repeat(10_000);
    let truncated = truncate_text_head_tail(&long, 8000);
    assert!(truncated.contains("... (truncated,"));
    assert!(truncated.contains("chars omitted)"));
    assert!(truncated.len() < long.len());
    assert!(truncated.starts_with("A"));
    assert!(truncated.ends_with("A"));
}

#[test]
fn test_truncate_text_head_tail_utf8() {
    // CJK: each char is 3 bytes. 10000 chars = 30000 bytes.
    let cjk = "测试文本".repeat(2500);
    assert!(std::str::from_utf8(cjk.as_bytes()).is_ok());

    let truncated = truncate_text_head_tail(&cjk, 8000);
    // Verify valid UTF-8 — no panics or broken boundaries
    assert!(std::str::from_utf8(truncated.as_bytes()).is_ok());
    assert!(truncated.starts_with("测试"));
    assert!(truncated.ends_with("文本"));
}

#[test]
fn test_format_tool_error() {
    let formatted = format_tool_error("Connection refused");
    assert!(formatted.contains("Error: Connection refused"));
    assert!(formatted.contains("Analyze the error above"));
    assert!(formatted.contains("try a different approach"));
}

#[test]
fn test_persist_tool_result() {
    let tmp = tempfile::tempdir().unwrap();
    let long_content = "X".repeat(20_000);
    let result = persist_tool_result(tmp.path(), "call-123", &long_content, 8000, 1200);
    assert!(result.contains("[tool output persisted]"));
    assert!(result.contains("call-123.txt"));
    assert!(result.contains("Original size: 20000"));
    assert!(result.contains("Preview:"));

    let file_path = tmp.path().join("tool-results/call-123.txt");
    assert!(file_path.exists());
    let saved = std::fs::read_to_string(&file_path).unwrap();
    assert_eq!(saved, long_content);
}

#[test]
fn test_drop_orphan_tool_results() {
    let mut messages = vec![
        Message::user("test".to_string()),
        Message::assistant(
            Some("calling".to_string()),
            Some(vec![tool_call("tc-1", "echo")]),
            None,
            None,
        ),
        Message::tool(
            "orphan".to_string(),
            "tc-999".to_string(),
            Some("echo".to_string()),
        ),
        Message::tool(
            "valid".to_string(),
            "tc-1".to_string(),
            Some("echo".to_string()),
        ),
    ];

    AgentRunner::drop_orphan_tool_results(&mut messages);

    assert_eq!(messages.len(), 3);
    assert!(matches!(&messages[2], Message::Tool { content, .. } if content == "valid"));
}

#[test]
fn test_backfill_missing_tool_results() {
    let mut messages = vec![
        Message::user("test".to_string()),
        Message::assistant(
            Some("calling".to_string()),
            Some(vec![tool_call("tc-1", "echo"), tool_call("tc-2", "read")]),
            None,
            None,
        ),
        Message::tool(
            "result-1".to_string(),
            "tc-1".to_string(),
            Some("echo".to_string()),
        ),
        // tc-2 has no result
    ];

    AgentRunner::backfill_missing_tool_results(&mut messages);

    // tc-2 is backfilled right after the assistant, before tc-1's result
    assert_eq!(messages.len(), 4);
    assert!(
        matches!(&messages[2], Message::Tool { content, tool_call_id, .. }
                if tool_call_id == "tc-2" && content.contains("interrupted or lost"))
    );
    assert!(matches!(&messages[3], Message::Tool { tool_call_id, .. } if tool_call_id == "tc-1"));
}

#[test]
fn test_backfill_missing_multiple_assistants() {
    // Two assistants, each with missing tool calls
    let mut messages = vec![
        Message::user("first".to_string()),
        Message::assistant(
            Some("a1".to_string()),
            Some(vec![tool_call("tc-1", "echo"), tool_call("tc-2", "read")]),
            None,
            None,
        ),
        Message::tool(
            "r1".to_string(),
            "tc-1".to_string(),
            Some("echo".to_string()),
        ),
        Message::user("second".to_string()),
        Message::assistant(
            Some("a2".to_string()),
            Some(vec![tool_call("tc-3", "write"), tool_call("tc-4", "list")]),
            None,
            None,
        ),
        Message::tool(
            "r4".to_string(),
            "tc-4".to_string(),
            Some("list".to_string()),
        ),
        // tc-2 and tc-3 are missing
    ];

    AgentRunner::backfill_missing_tool_results(&mut messages);

    assert_eq!(messages.len(), 8);
    // After A1 (index 1): tc-2 should be at index 2
    assert!(matches!(&messages[2], Message::Tool { tool_call_id, .. } if tool_call_id == "tc-2"));
    // After A2 (originally index 4, now 5): tc-3 should be at index 6
    assert!(matches!(&messages[6], Message::Tool { tool_call_id, .. } if tool_call_id == "tc-3"));
}

#[tokio::test]
async fn test_runner_persist_long_output() {
    let tmp = tempfile::TempDir::new().unwrap();
    let path = tmp.path();
    let session_dir = path.join("sessions");
    let workspace_dir = path.join("workspace");
    std::fs::create_dir_all(&workspace_dir).unwrap();

    let long_result = "X".repeat(12_000);

    let sm: SharedSessionManager = Arc::new(Mutex::new(SessionManager::new(session_dir).unwrap()));
    let mut tm = ToolManager::new(workspace_dir.clone());
    tm.register(Box::new(MockEchoTool::new("big", &long_result)));
    let tm = Arc::new(tm);
    sm.lock().await.get_or_create("test-session").await.unwrap();

    let ms = Arc::new(tokio::sync::Mutex::new(MemoryService::new(&workspace_dir)));
    ms.lock().await.init().unwrap();

    let provider = Arc::new(MockProvider::new(vec![
        LLMResponse {
            content: Some("calling tool".to_string()),
            tool_calls: Some(vec![tool_call("tc-big", "big")]),
            finish_reason: FinishReason::ToolCalls,
            usage: Usage::default(),
        },
        LLMResponse {
            content: Some("done".to_string()),
            tool_calls: None,
            finish_reason: FinishReason::Stop,
            usage: Usage::default(),
        },
    ]));
    let runner = AgentRunner::new(
        tm,
        provider,
        sm.clone(),
        AgentConfig {
            provider: "test".to_string(),
            max_iterations: 40,
            timeout_seconds: 120,
            persist_tool_results: true,
            unknown: Default::default(),
        },
        workspace_dir.clone(),
        ms,
        None,
        4096,
        None,
    )
    // 4000-token window → 3000 chars per result, so this 20k-char result persists.
    .with_pressure(crate::pressure::PressureAnchor::from_window(4_000));
    let task = make_task("test persist");

    let result = runner
        .run(
            task.content.clone(),
            task.hook.clone(),
            "test-session",
            None,
            None,
            None,
            None,
        )
        .await;
    assert_eq!(result.content, "done");

    let msgs = sm.lock().await.get_messages("test-session").await;
    match &msgs[2] {
        Message::Tool { content, .. } => {
            // Long content persisted to file → reference string + truncation
            assert!(content.contains("[tool output persisted]"));
        }
        other => panic!("Expected Tool message, got {:?}", other),
    }
}

#[tokio::test]
async fn test_stop_sentinel_returns_cancelled() {
    let tmp = tempfile::TempDir::new().unwrap();
    let (sm, tm, wd, ms) = make_test_env(&tmp, &[]).await;

    // Create a runner with a cancellation token that is already cancelled.
    let ct = CancellationToken::new();
    ct.cancel();

    let runner = AgentRunner::new(
        tm,
        Arc::new(MockProvider::new(vec![])),
        sm.clone(),
        AgentConfig {
            provider: "test".to_string(),
            max_iterations: 40,
            timeout_seconds: 120,
            persist_tool_results: false,
            unknown: Default::default(),
        },
        wd,
        ms,
        None,
        4096,
        None,
    );

    // Sentinel /stop message should be detected immediately.
    let result = runner
        .run(
            "/stop".to_string(),
            TaskHook::new("test-session"),
            "test-session",
            None,
            Some(ct),
            None,
            None,
        )
        .await;
    assert!(!result.success);
    assert_eq!(result.content, "Task cancelled.");
}

#[tokio::test]
async fn test_cancelled_token_before_run() {
    let tmp = tempfile::TempDir::new().unwrap();
    let (sm, tm, wd, ms) = make_test_env(&tmp, &[]).await;

    // Create a runner with a cancelled token but non-sentinel content.
    let ct = CancellationToken::new();
    ct.cancel();

    let runner = AgentRunner::new(
        tm,
        Arc::new(MockProvider::new(vec![])),
        sm.clone(),
        AgentConfig {
            provider: "test".to_string(),
            max_iterations: 40,
            timeout_seconds: 120,
            persist_tool_results: false,
            unknown: Default::default(),
        },
        wd,
        ms,
        None,
        4096,
        None,
    );

    // Should return cancelled before making any LLM call.
    let result = runner
        .run(
            "hello".to_string(),
            TaskHook::new("test-session"),
            "test-session",
            None,
            Some(ct),
            None,
            None,
        )
        .await;
    assert!(!result.success);
    assert_eq!(result.content, "Task cancelled.");
}

#[tokio::test]
async fn test_cancel_in_loop_rolls_back_messages() {
    let tmp = tempfile::TempDir::new().unwrap();
    let path = tmp.path();
    let session_dir = path.join("sessions");
    let workspace_dir = path.join("workspace");
    std::fs::create_dir_all(&workspace_dir).unwrap();

    let sm: SharedSessionManager = Arc::new(Mutex::new(SessionManager::new(session_dir).unwrap()));
    let mut tm = ToolManager::new(workspace_dir.clone());
    tm.register(Box::new(MockEchoTool::new("echo", "ok")));
    let tm = Arc::new(tm);
    sm.lock().await.get_or_create("test-session").await.unwrap();

    let ms = Arc::new(tokio::sync::Mutex::new(MemoryService::new(&workspace_dir)));
    ms.lock().await.init().unwrap();

    // Pre-existing messages in session
    sm.lock()
        .await
        .add_message("test-session", Message::user("prev".to_string()))
        .await
        .unwrap();
    sm.lock()
        .await
        .add_message(
            "test-session",
            Message::assistant(Some("prev reply".to_string()), None, None, None),
        )
        .await
        .unwrap();
    let pre_count = sm.lock().await.message_count("test-session");
    assert_eq!(pre_count, 2);

    let ct = CancellationToken::new();
    let ct_clone = ct.clone();

    // Mock provider: returns tool_calls on first call, cancels token during that call
    // so the second loop iteration hits cancellation.
    let provider = Arc::new(MockProvider::new(vec![
        LLMResponse {
            content: Some("calling tool".to_string()),
            tool_calls: Some(vec![tool_call("tc-1", "echo")]),
            finish_reason: FinishReason::ToolCalls,
            usage: Usage::default(),
        },
        LLMResponse {
            content: Some("should never reach".to_string()),
            tool_calls: None,
            finish_reason: FinishReason::Stop,
            usage: Usage::default(),
        },
    ]));

    let runner = AgentRunner::new(
        tm,
        Arc::new(CancellingProvider::new(provider, ct_clone)),
        sm.clone(),
        AgentConfig {
            provider: "test".to_string(),
            max_iterations: 40,
            timeout_seconds: 120,
            persist_tool_results: false,
            unknown: Default::default(),
        },
        workspace_dir,
        ms,
        None,
        4096,
        None,
    );

    let result = runner
        .run(
            "test cancel".to_string(),
            TaskHook::new("test-session"),
            "test-session",
            None,
            Some(ct),
            None,
            None,
        )
        .await;
    assert!(!result.success);
    assert_eq!(result.content, "Task cancelled.");

    // All messages added during this turn should be rolled back.
    // Session should still have only the 2 pre-existing messages.
    let msgs = sm.lock().await.get_messages("test-session").await;
    assert_eq!(
        msgs.len(),
        2,
        "expected pre-count messages, got {}",
        msgs.len()
    );
}

#[tokio::test]
async fn test_cancel_in_pre_turn_rolls_back_user_message() {
    let tmp = tempfile::TempDir::new().unwrap();
    let (sm, tm, wd, ms) = make_test_env(&tmp, &[]).await;

    // Pre-existing messages: user + assistant reply, so the session ends on an
    // assistant message once this turn's user message is rolled back.
    sm.lock()
        .await
        .add_message("test-session", Message::user("prev".to_string()))
        .await
        .unwrap();
    sm.lock()
        .await
        .add_message(
            "test-session",
            Message::assistant(Some("prev reply".to_string()), None, None, None),
        )
        .await
        .unwrap();
    let pre_count = sm.lock().await.message_count("test-session");
    assert_eq!(pre_count, 2);

    let ct = CancellationToken::new();
    let ct_clone = ct.clone();
    let provider = Arc::new(MockProvider::new(vec![LLMResponse {
        content: Some("should never be reached".to_string()),
        tool_calls: None,
        finish_reason: FinishReason::Stop,
        usage: Usage::default(),
    }]));

    // Cancel mid-PreTurn: the hook is inserted after WriteUserMessageHook
    // (index 0), so the user message IS written, then the registry's next
    // cancellation check (before NotifyRunningHook) aborts the phase.
    let runner = make_runner_with(sm.clone(), tm, provider.clone(), wd, ms, |b| {
        b.add_pre_turn(
            Arc::new(CancellingPreTurnHook { ct: ct_clone }),
            HookPosition::Insert(1),
        )
    });

    let result = runner
        .run(
            "hello".to_string(),
            TaskHook::new("test-session"),
            "test-session",
            None,
            Some(ct),
            None,
            None,
        )
        .await;

    assert!(!result.success);
    assert_eq!(result.content, "Task cancelled.");
    assert_eq!(
        provider.call_count(),
        0,
        "no LLM call should be made after pre-turn cancellation"
    );

    // This turn's user message must be rolled back by the after_turn
    // governance: the session keeps only the pre-existing messages and ends
    // on the assistant reply.
    let msgs = sm.lock().await.get_messages("test-session").await;
    assert_eq!(
        msgs.len(),
        pre_count,
        "user message written during PreTurn must be rolled back"
    );
    assert!(
        matches!(msgs.last(), Some(Message::Assistant { .. })),
        "session must not end on an unanswered user message"
    );
}

#[tokio::test]
async fn test_cancel_before_chat_rolls_back_without_llm_call() {
    let tmp = tempfile::TempDir::new().unwrap();
    let (sm, tm, wd, ms) = make_test_env(&tmp, &[]).await;

    // Pre-existing messages: user + assistant reply.
    sm.lock()
        .await
        .add_message("test-session", Message::user("prev".to_string()))
        .await
        .unwrap();
    sm.lock()
        .await
        .add_message(
            "test-session",
            Message::assistant(Some("prev reply".to_string()), None, None, None),
        )
        .await
        .unwrap();
    let pre_count = sm.lock().await.message_count("test-session");
    assert_eq!(pre_count, 2);

    let ct = CancellationToken::new();
    let ct_clone = ct.clone();
    let provider = Arc::new(MockProvider::new(vec![LLMResponse {
        content: Some("should never be reached".to_string()),
        tool_calls: None,
        finish_reason: FinishReason::Stop,
        usage: Usage::default(),
    }]));

    // Cancel in the last PreChat hook: the token is already cancelled when the
    // skeleton reaches the LLM call, so no request must be fired and the turn
    // must be rolled back.
    let runner = make_runner_with(sm.clone(), tm, provider.clone(), wd, ms, |b| {
        b.add_pre_chat(
            Arc::new(CancellingPreChatHook { ct: ct_clone }),
            HookPosition::Append,
        )
    });

    let result = runner
        .run(
            "hello".to_string(),
            TaskHook::new("test-session"),
            "test-session",
            None,
            Some(ct),
            None,
            None,
        )
        .await;

    assert!(!result.success);
    assert_eq!(result.content, "Task cancelled.");
    assert_eq!(
        provider.call_count(),
        0,
        "a cancelled run must not fire an LLM call"
    );

    // The turn's user message must be rolled back; session ends on the
    // assistant reply.
    let msgs = sm.lock().await.get_messages("test-session").await;
    assert_eq!(
        msgs.len(),
        pre_count,
        "turn messages must be rolled back on pre-chat cancellation"
    );
    assert!(matches!(msgs.last(), Some(Message::Assistant { .. })));
}

#[tokio::test]
async fn test_llm_error_rolls_back_messages() {
    let tmp = tempfile::TempDir::new().unwrap();
    let path = tmp.path();
    let session_dir = path.join("sessions");
    let workspace_dir = path.join("workspace");
    std::fs::create_dir_all(&workspace_dir).unwrap();

    let sm: SharedSessionManager = Arc::new(Mutex::new(SessionManager::new(session_dir).unwrap()));
    let mut tm = ToolManager::new(workspace_dir.clone());
    tm.register(Box::new(MockEchoTool::new("echo", "ok")));
    let tm = Arc::new(tm);
    sm.lock().await.get_or_create("test-session").await.unwrap();

    let ms = Arc::new(tokio::sync::Mutex::new(MemoryService::new(&workspace_dir)));
    ms.lock().await.init().unwrap();

    // Pre-existing messages
    sm.lock()
        .await
        .add_message("test-session", Message::user("prev".to_string()))
        .await
        .unwrap();
    let pre_count = sm.lock().await.message_count("test-session");
    assert_eq!(pre_count, 1);

    // Provider that always fails
    let provider = Arc::new(FailingProvider);

    let runner = AgentRunner::new(
        tm,
        provider,
        sm.clone(),
        AgentConfig {
            provider: "test".to_string(),
            max_iterations: 40,
            timeout_seconds: 120,
            persist_tool_results: false,
            unknown: Default::default(),
        },
        workspace_dir,
        ms,
        None,
        4096,
        None,
    );

    let result = runner
        .run(
            "test error".to_string(),
            TaskHook::new("test-session"),
            "test-session",
            None,
            None,
            None,
            None,
        )
        .await;
    assert!(!result.success);
    assert!(result.content.contains("LLM API request failed"));

    // User message should be rolled back.
    let msgs = sm.lock().await.get_messages("test-session").await;
    assert_eq!(
        msgs.len(),
        pre_count,
        "expected pre-count messages, got {}",
        msgs.len()
    );
}

#[tokio::test]
async fn test_agent_result_accumulate() {
    // Test that AgentResult.accumulate correctly combines Usage stats
    let mut total = AgentResult {
        success: false,
        content: String::new(),
        total_tokens: 0,
        prompt_tokens: 0,
        prompt_cache_hit_tokens: 0,
        completion_tokens: 0,
        iterations: 0,
        message_sent: false,
    };

    let usage1 = Usage {
        prompt_tokens: 100,
        completion_tokens: 50,
        total_tokens: 150,
        prompt_cache_hit_tokens: 20,
    };
    total.accumulate(&usage1);
    assert_eq!(total.total_tokens, 150);
    assert_eq!(total.prompt_tokens, 100);
    assert_eq!(total.prompt_cache_hit_tokens, 20);
    assert_eq!(total.completion_tokens, 50);

    let usage2 = Usage {
        prompt_tokens: 200,
        completion_tokens: 80,
        total_tokens: 280,
        prompt_cache_hit_tokens: 40,
    };
    total.accumulate(&usage2);
    assert_eq!(total.total_tokens, 430);
    assert_eq!(total.prompt_tokens, 300);
    assert_eq!(total.prompt_cache_hit_tokens, 60);
    assert_eq!(total.completion_tokens, 130);
}

#[tokio::test]
async fn test_snip_truncates_context_before_chat() {
    let tmp = tempfile::TempDir::new().unwrap();
    let (sm, tm, wd, ms) = make_test_env(&tmp, &[]).await;

    // Pre-fill session with messages to exceed budget
    // budget = 700 - 500 - 128 (min safety floor) = 72 tokens
    // At 4 tokens per message (15-20 chars), 50 pairs = ~400 tokens > 72, will snip
    {
        let mut sm_lock = sm.lock().await;
        for i in 0..50 {
            sm_lock
                .add_message("test-session", Message::user(format!("old user msg {}", i)))
                .await
                .unwrap();
            sm_lock
                .add_message(
                    "test-session",
                    Message::assistant(
                        Some(format!("old assistant reply {}", i)),
                        None,
                        None,
                        None,
                    ),
                )
                .await
                .unwrap();
        }
    }

    // Provider that captures the messages it receives
    let received_messages = Arc::new(std::sync::Mutex::new(Vec::new()));
    let received_clone = received_messages.clone();

    struct CapturingProvider {
        received: Arc<std::sync::Mutex<Vec<usize>>>,
    }

    #[async_trait]
    impl Provider for CapturingProvider {
        async fn chat(
            &self,
            messages: &[&Message],
            _tools: Option<&[ToolDefinition]>,
            _recorder: Option<&dyn crate::trace::SpanRecorder>,
        ) -> Result<LLMResponse> {
            self.received.lock().unwrap().push(messages.len());
            Ok(LLMResponse {
                content: Some("done".to_string()),
                tool_calls: None,
                finish_reason: FinishReason::Stop,
                usage: Usage::default(),
            })
        }
    }

    // Create runner to force snipping with limited budget.
    // window=700, output=500 -> budget = 700 - 500 - 128 = 72 tokens, so the
    // kept window is a ~18 messages: well under the 50 asserted below.
    let provider = Arc::new(CapturingProvider {
        received: received_clone,
    });
    let runner = AgentRunner::new(
        tm,
        provider,
        sm.clone(),
        AgentConfig {
            provider: "test".to_string(),
            max_iterations: 40,
            timeout_seconds: 120,
            persist_tool_results: false,
            unknown: Default::default(),
        },
        wd,
        ms,
        None,
        500,
        None,
    )
    .with_pressure(crate::pressure::PressureAnchor::from_window(700));

    let task = make_task("new user message");
    let result = runner
        .run(
            task.content,
            task.hook,
            "test-session",
            None,
            None,
            None,
            None,
        )
        .await;

    assert!(result.success);
    // Provider should have received fewer messages than the 100+ in session
    let counts = received_messages.lock().unwrap();
    assert!(!counts.is_empty());
    // The number of messages sent to provider should be much less than 100+
    assert!(
        counts[0] < 50,
        "Expected snip to reduce messages, but provider received {}",
        counts[0]
    );
}

// ---------------------------------------------------------------------------
// Issue 04: Hook behavior integration tests (through AgentRunner::run())
// ---------------------------------------------------------------------------

/// Records each invocation id in order (for ordering tests).
struct OrderRecorder {
    order: Arc<Mutex<Vec<usize>>>,
    id: usize,
}

#[async_trait]
impl PreChatHook for OrderRecorder {
    async fn call(&self, _spec: &RunSpec, _state: &mut RunState) -> Result<HookAction> {
        self.order.lock().await.push(self.id);
        Ok(HookAction::Continue)
    }
}

/// Aborts the run when called (PreChat phase).
struct AbortPreChatHook {
    reason: String,
}

#[async_trait]
impl PreChatHook for AbortPreChatHook {
    async fn call(&self, _spec: &RunSpec, _state: &mut RunState) -> Result<HookAction> {
        Ok(HookAction::Abort {
            reason: self.reason.clone(),
        })
    }
}

/// Stores an extension in run state (cross-phase communication).
struct SetExtensionPreChatHook {
    key: String,
    value: String,
}

#[async_trait]
impl PreChatHook for SetExtensionPreChatHook {
    async fn call(&self, _spec: &RunSpec, state: &mut RunState) -> Result<HookAction> {
        state.set_extension(&self.key, self.value.clone());
        Ok(HookAction::Continue)
    }
}

/// Reads an extension from run state (AfterChat phase).
struct ReadExtensionAfterChatHook {
    key: String,
    seen: Arc<Mutex<Option<String>>>,
}

#[async_trait]
impl AfterChatHook for ReadExtensionAfterChatHook {
    async fn call(&self, _spec: &RunSpec, state: &mut RunState) -> Result<HookAction> {
        *self.seen.lock().await = state.get_extension::<String>(&self.key).cloned();
        Ok(HookAction::Continue)
    }
}

/// Skips tool execution and returns a synthetic result.
struct SkipPreToolHook {
    result: String,
}

#[async_trait]
impl PreToolHook for SkipPreToolHook {
    async fn call(
        &self,
        _spec: &RunSpec,
        _state: &mut RunState,
        _tool_call: &ToolCall,
    ) -> Result<ToolHookAction> {
        Ok(ToolHookAction::Skip {
            result: self.result.clone(),
        })
    }
}

/// Aborts the run from the PreTool phase.
struct AbortPreToolHook {
    reason: String,
}

#[async_trait]
impl PreToolHook for AbortPreToolHook {
    async fn call(
        &self,
        _spec: &RunSpec,
        _state: &mut RunState,
        _tool_call: &ToolCall,
    ) -> Result<ToolHookAction> {
        Ok(ToolHookAction::Abort {
            reason: self.reason.clone(),
        })
    }
}

/// Records whether the AfterTurn phase executed.
struct RecordingAfterTurnHook {
    invoked: Arc<std::sync::atomic::AtomicBool>,
}

#[async_trait]
impl AfterTurnHook for RecordingAfterTurnHook {
    async fn call(&self, _spec: &RunSpec, _state: &mut RunState) -> Result<()> {
        self.invoked
            .store(true, std::sync::atomic::Ordering::SeqCst);
        Ok(())
    }
}

/// Cancels the run token when invoked (PreTurn phase).
///
/// Runs after `WriteUserMessageHook`, so the user message is already written
/// when the registry's next cancellation check aborts the phase.
struct CancellingPreTurnHook {
    ct: CancellationToken,
}

#[async_trait]
impl PreTurnHook for CancellingPreTurnHook {
    async fn call(&self, _spec: &RunSpec, _state: &mut RunState) -> Result<HookAction> {
        self.ct.cancel();
        Ok(HookAction::Continue)
    }
}

/// Cancels the run token when invoked (PreChat phase).
///
/// Runs after the default PreChat hooks, so the token is cancelled in the
/// window between the last PreChat hook and the LLM call.
struct CancellingPreChatHook {
    ct: CancellationToken,
}

#[async_trait]
impl PreChatHook for CancellingPreChatHook {
    async fn call(&self, _spec: &RunSpec, _state: &mut RunState) -> Result<HookAction> {
        self.ct.cancel();
        Ok(HookAction::Continue)
    }
}

/// Tool that counts executions (to verify skips prevent execution).
struct CountingTool {
    name: String,
    executions: Arc<std::sync::atomic::AtomicU32>,
}

impl CountingTool {
    fn new(name: &str, executions: Arc<std::sync::atomic::AtomicU32>) -> Self {
        Self {
            name: name.to_string(),
            executions,
        }
    }
}

#[async_trait]
impl Tool for CountingTool {
    fn name(&self) -> &str {
        &self.name
    }

    fn description(&self) -> &str {
        "A mock tool that counts executions"
    }

    fn parameters(&self) -> serde_json::Value {
        serde_json::json!({ "type": "object", "properties": {} })
    }

    async fn execute(&self, _args: serde_json::Value) -> Result<String> {
        self.executions
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Ok("executed".to_string())
    }
}

/// Build a runner with default hooks plus custom hooks applied via `configure`.
fn make_runner_with<F>(
    sm: SharedSessionManager,
    tm: Arc<ToolManager>,
    provider: Arc<dyn Provider>,
    workspace_dir: PathBuf,
    ms: Arc<tokio::sync::Mutex<MemoryService>>,
    configure: F,
) -> AgentRunner
where
    F: FnOnce(AgentRunnerBuilder) -> AgentRunnerBuilder,
{
    let builder = AgentRunnerBuilder::new()
        .session_manager(sm)
        .tool_manager(tm)
        .provider(provider)
        .config(AgentConfig {
            provider: "test".to_string(),
            max_iterations: 40,
            timeout_seconds: 120,
            persist_tool_results: false,
            unknown: Default::default(),
        })
        .workspace_dir(workspace_dir)
        .memory_service(ms)
        .max_tokens(4096)
        .with_defaults();
    configure(builder).build()
}

/// Run a task against a runner and return the result.
async fn run_task(runner: &AgentRunner, content: &str) -> AgentResult {
    runner
        .run(
            content.to_string(),
            TaskHook::new("test-session"),
            "test-session",
            None,
            None,
            None,
            None,
        )
        .await
}

#[tokio::test]
async fn test_pre_chat_hook_aborts_run() {
    let tmp = tempfile::TempDir::new().unwrap();
    let (sm, tm, wd, ms) = make_test_env(&tmp, &[]).await;
    let provider = Arc::new(MockProvider::new(vec![LLMResponse {
        content: Some("should never be reached".to_string()),
        tool_calls: None,
        finish_reason: FinishReason::Stop,
        usage: Usage::default(),
    }]));
    let runner = make_runner_with(sm.clone(), tm, provider.clone(), wd, ms, |b| {
        b.add_pre_chat(
            Arc::new(AbortPreChatHook {
                reason: "budget exceeded".to_string(),
            }),
            HookPosition::Append,
        )
    });

    let result = run_task(&runner, "hello").await;
    assert!(!result.success, "aborted run must fail");
    assert_eq!(result.content, "budget exceeded");
    assert_eq!(
        provider.call_count(),
        0,
        "no chat call should be made after abort"
    );
}

#[tokio::test]
async fn test_pre_tool_hook_skips_execution() {
    let tmp = tempfile::TempDir::new().unwrap();
    let (sm, _tm, wd, ms) = make_test_env(&tmp, &[]).await;
    let executions = Arc::new(std::sync::atomic::AtomicU32::new(0));
    let mut tmm = ToolManager::new(wd.clone());
    tmm.register(Box::new(CountingTool::new("echo", executions.clone())));
    let tm = Arc::new(tmm);

    let provider = Arc::new(MockProvider::new(vec![
        LLMResponse {
            content: Some("calling".to_string()),
            tool_calls: Some(vec![tool_call("tc-1", "echo")]),
            finish_reason: FinishReason::ToolCalls,
            usage: Usage::default(),
        },
        LLMResponse {
            content: Some("done".to_string()),
            tool_calls: None,
            finish_reason: FinishReason::Stop,
            usage: Usage::default(),
        },
    ]));
    let runner = make_runner_with(sm.clone(), tm, provider, wd, ms, |b| {
        b.add_pre_tool(
            Arc::new(SkipPreToolHook {
                result: "cached-result".to_string(),
            }),
            HookPosition::Append,
        )
    });

    let result = run_task(&runner, "run echo").await;
    assert!(result.success);
    assert_eq!(
        executions.load(std::sync::atomic::Ordering::SeqCst),
        0,
        "tool must not execute when skipped"
    );

    // The synthetic result should have been persisted as the tool message.
    let msgs = sm.lock().await.get_messages("test-session").await;
    assert!(msgs.iter().any(|m| matches!(
        m,
        Message::Tool { content, .. } if content == "cached-result"
    )));
}

#[tokio::test]
async fn test_pre_tool_hook_aborts_run() {
    let tmp = tempfile::TempDir::new().unwrap();
    let (sm, tm, wd, ms) = make_test_env(&tmp, &[]).await;
    let provider = Arc::new(MockProvider::new(vec![LLMResponse {
        content: Some("calling".to_string()),
        tool_calls: Some(vec![tool_call("tc-1", "echo")]),
        finish_reason: FinishReason::ToolCalls,
        usage: Usage::default(),
    }]));
    let runner = make_runner_with(sm.clone(), tm, provider, wd, ms, |b| {
        b.add_pre_tool(
            Arc::new(AbortPreToolHook {
                reason: "unsafe tool call".to_string(),
            }),
            HookPosition::Append,
        )
    });

    let result = run_task(&runner, "run echo").await;
    assert!(!result.success);
    assert_eq!(result.content, "unsafe tool call");
}

#[tokio::test]
async fn test_hook_extensions_cross_phase() {
    let tmp = tempfile::TempDir::new().unwrap();
    let (sm, tm, wd, ms) = make_test_env(&tmp, &[]).await;
    let provider = Arc::new(MockProvider::new(vec![LLMResponse {
        content: Some("final".to_string()),
        tool_calls: None,
        finish_reason: FinishReason::Stop,
        usage: Usage::default(),
    }]));
    let seen = Arc::new(Mutex::new(None));
    let seen_clone = seen.clone();
    let runner = make_runner_with(sm.clone(), tm, provider, wd, ms, |b| {
        b.add_pre_chat(
            Arc::new(SetExtensionPreChatHook {
                key: "flag".to_string(),
                value: "set-in-pre-chat".to_string(),
            }),
            HookPosition::Append,
        )
        .add_after_chat(
            Arc::new(ReadExtensionAfterChatHook {
                key: "flag".to_string(),
                seen: seen_clone,
            }),
            HookPosition::Append,
        )
    });

    let result = run_task(&runner, "hello").await;
    assert!(result.success);
    let observed = seen.lock().await.clone();
    assert_eq!(
        observed.as_deref(),
        Some("set-in-pre-chat"),
        "after-chat hook must read the extension written by the pre-chat hook"
    );
}

#[tokio::test]
async fn test_after_turn_hooks_execute_on_cancellation() {
    let tmp = tempfile::TempDir::new().unwrap();
    let (sm, tm, wd, ms) = make_test_env(&tmp, &[]).await;

    let ct = CancellationToken::new();
    let ct_clone = ct.clone();
    let provider = Arc::new(CancellingProvider::new(
        Arc::new(MockProvider::new(vec![LLMResponse {
            content: Some("cancelled".to_string()),
            tool_calls: None,
            finish_reason: FinishReason::Stop,
            usage: Usage::default(),
        }])),
        ct_clone,
    ));
    let invoked = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let invoked_clone = invoked.clone();
    let runner = make_runner_with(sm.clone(), tm, provider, wd, ms, |b| {
        b.add_after_turn(
            Arc::new(RecordingAfterTurnHook {
                invoked: invoked_clone,
            }),
            HookPosition::Append,
        )
    });

    let result = runner
        .run(
            "hello".to_string(),
            TaskHook::new("test-session"),
            "test-session",
            None,
            Some(ct),
            None,
            None,
        )
        .await;
    assert!(!result.success);
    assert!(invoked.load(std::sync::atomic::Ordering::SeqCst));
}

#[tokio::test]
async fn test_multiple_hooks_per_phase_execute_in_order() {
    let tmp = tempfile::TempDir::new().unwrap();
    let (sm, tm, wd, ms) = make_test_env(&tmp, &[]).await;
    let order = Arc::new(Mutex::new(Vec::new()));
    let runner = make_runner_with(sm.clone(), tm, provider_for_direct_reply(), wd, ms, |b| {
        let order_clone = order.clone();
        let mut builder = b.add_pre_chat(
            Arc::new(OrderRecorder {
                order: order_clone.clone(),
                id: 1,
            }),
            HookPosition::Append,
        );
        builder = builder.add_pre_chat(
            Arc::new(OrderRecorder {
                order: order_clone.clone(),
                id: 2,
            }),
            HookPosition::Append,
        );
        builder.add_pre_chat(
            Arc::new(OrderRecorder {
                order: order_clone.clone(),
                id: 3,
            }),
            HookPosition::Append,
        )
    });

    let result = run_task(&runner, "hello").await;
    assert!(result.success);
    let observed = order.lock().await.clone();
    assert_eq!(
        observed,
        vec![1, 2, 3],
        "hooks must run in registration order"
    );
}

fn provider_for_direct_reply() -> Arc<dyn Provider> {
    Arc::new(MockProvider::new(vec![LLMResponse {
        content: Some("ok".to_string()),
        tool_calls: None,
        finish_reason: FinishReason::Stop,
        usage: Usage::default(),
    }]))
}

#[tokio::test]
async fn test_hook_position_replace() {
    let tmp = tempfile::TempDir::new().unwrap();
    let (sm, tm, wd, ms) = make_test_env(&tmp, &[]).await;
    let provider = Arc::new(MockProvider::new(vec![LLMResponse {
        content: Some("never".to_string()),
        tool_calls: None,
        finish_reason: FinishReason::Stop,
        usage: Usage::default(),
    }]));
    // Replace the first pre_chat hook (BuildContext) with an aborting hook.
    let runner = make_runner_with(sm.clone(), tm, provider.clone(), wd, ms, |b| {
        b.add_pre_chat(
            Arc::new(AbortPreChatHook {
                reason: "replaced".to_string(),
            }),
            HookPosition::Replace(0),
        )
    });

    let result = run_task(&runner, "hello").await;
    assert!(!result.success);
    assert_eq!(result.content, "replaced");
    assert_eq!(
        provider.call_count(),
        0,
        "BuildContext was replaced so no chat happens"
    );
}

#[tokio::test]
async fn test_hook_position_insert() {
    let tmp = tempfile::TempDir::new().unwrap();
    let (sm, tm, wd, ms) = make_test_env(&tmp, &[]).await;
    let order = Arc::new(Mutex::new(Vec::new()));
    let runner = make_runner_with(sm.clone(), tm, provider_for_direct_reply(), wd, ms, |b| {
        b.add_pre_chat(
            Arc::new(OrderRecorder {
                order: order.clone(),
                id: 99,
            }),
            HookPosition::Insert(0),
        )
    });

    let result = run_task(&runner, "hello").await;
    assert!(
        result.success,
        "inserting a hook must not break the default pipeline"
    );
    let observed = order.lock().await.clone();
    assert_eq!(
        observed,
        vec![99],
        "inserted hook should run (first in phase)"
    );
}

#[tokio::test]
async fn test_with_defaults_produces_default_behavior() {
    let tmp = tempfile::TempDir::new().unwrap();
    let (sm, tm, wd, ms) = make_test_env(&tmp, &[("echo", "echo-result")]).await;

    // Direct reply scenario.
    let runner = make_runner_with(
        sm.clone(),
        tm.clone(),
        provider_for_direct_reply(),
        wd.clone(),
        ms.clone(),
        |b| b,
    );
    let result = run_task(&runner, "hello").await;
    assert!(result.success);
    assert_eq!(result.content, "ok");

    // Tool call scenario (default hooks execute the tool).
    let provider = Arc::new(MockProvider::new(vec![
        LLMResponse {
            content: Some("calling".to_string()),
            tool_calls: Some(vec![tool_call("tc-1", "echo")]),
            finish_reason: FinishReason::ToolCalls,
            usage: Usage::default(),
        },
        LLMResponse {
            content: Some("done".to_string()),
            tool_calls: None,
            finish_reason: FinishReason::Stop,
            usage: Usage::default(),
        },
    ]));
    let runner = make_runner_with(sm.clone(), tm, provider, wd, ms, |b| b);
    let result = run_task(&runner, "run echo").await;
    assert!(result.success);
    assert_eq!(result.content, "done");
}

/// Aborts the run from the PreTurn phase (before the loop starts).
struct AbortPreTurnHook {
    reason: String,
}

#[async_trait]
impl PreTurnHook for AbortPreTurnHook {
    async fn call(&self, _spec: &RunSpec, _state: &mut RunState) -> Result<HookAction> {
        Ok(HookAction::Abort {
            reason: self.reason.clone(),
        })
    }
}

#[tokio::test]
async fn test_pre_turn_hook_aborts_run_before_loop() {
    let tmp = tempfile::TempDir::new().unwrap();
    let (sm, tm, wd, ms) = make_test_env(&tmp, &[]).await;
    let provider = Arc::new(MockProvider::new(vec![LLMResponse {
        content: Some("should never be reached".to_string()),
        tool_calls: None,
        finish_reason: FinishReason::Stop,
        usage: Usage::default(),
    }]));
    let runner = make_runner_with(sm.clone(), tm, provider.clone(), wd, ms, |b| {
        b.add_pre_turn(
            Arc::new(AbortPreTurnHook {
                reason: "pre-turn abort".to_string(),
            }),
            HookPosition::Append,
        )
    });

    let result = run_task(&runner, "hello").await;
    assert!(!result.success, "pre-turn abort must fail the run");
    assert_eq!(result.content, "pre-turn abort");
    assert_eq!(
        provider.call_count(),
        0,
        "no chat call should be made when pre-turn aborts"
    );

    // AfterTurn must still run: the session is persisted (no rollback set).
    let msgs = sm.lock().await.get_messages("test-session").await;
    assert!(
        !msgs.is_empty(),
        "after_turn should persist the turn even when pre-turn aborts"
    );
}

// ── Trace integration ──

/// Recording trace backend for runner-level span-tree assertions.
#[derive(Default)]
struct RecordingTraceBackend {
    events: Arc<std::sync::Mutex<Vec<TraceEvent>>>,
}

#[derive(Clone)]
struct TraceEvent {
    kind: &'static str,
    span_id: String,
    trace_id: String,
    parent_span_id: Option<String>,
    name: String,
    status: Option<crate::trace::SpanStatus>,
    attributes: std::collections::BTreeMap<String, serde_json::Value>,
}

impl crate::trace::TraceBackend for RecordingTraceBackend {
    fn on_span_start(
        &self,
        ctx: &crate::trace::SpanContext,
        name: &str,
        _start: std::time::SystemTime,
    ) {
        self.events.lock().unwrap().push(TraceEvent {
            kind: "start",
            span_id: ctx.span_id.clone(),
            trace_id: ctx.trace_id.clone(),
            parent_span_id: ctx.parent_span_id.clone(),
            name: name.to_string(),
            status: None,
            attributes: Default::default(),
        });
    }

    fn on_span_end(
        &self,
        ctx: &crate::trace::SpanContext,
        _end: std::time::SystemTime,
        status: &crate::trace::SpanStatus,
        attributes: &std::collections::BTreeMap<String, serde_json::Value>,
    ) {
        self.events.lock().unwrap().push(TraceEvent {
            kind: "end",
            span_id: ctx.span_id.clone(),
            trace_id: ctx.trace_id.clone(),
            parent_span_id: ctx.parent_span_id.clone(),
            name: String::new(),
            status: Some(status.clone()),
            attributes: attributes.clone(),
        });
    }
}

impl RecordingTraceBackend {
    fn starts(&self) -> Vec<TraceEvent> {
        self.events
            .lock()
            .unwrap()
            .iter()
            .filter(|e| e.kind == "start")
            .cloned()
            .collect()
    }

    fn start_named(&self, name: &str) -> TraceEvent {
        self.starts()
            .into_iter()
            .find(|e| e.name == name)
            .unwrap_or_else(|| panic!("no start event for span {name}"))
    }

    fn end_for(&self, start: &TraceEvent) -> TraceEvent {
        self.events
            .lock()
            .unwrap()
            .iter()
            .find(|e| e.kind == "end" && e.span_id == start.span_id)
            .cloned()
            .expect("missing end event")
    }
}

/// Builds a runner over a fresh "webui:chat-7" session with a tracer backed by
/// the given recording backend.
async fn make_traced_runner(
    tmp: &tempfile::TempDir,
    provider: Arc<dyn Provider>,
    backend: Arc<RecordingTraceBackend>,
) -> AgentRunner {
    let (sm, tm, wd, ms) = make_test_env(tmp, &[("echo", "echo-result")]).await;
    sm.lock().await.get_or_create("webui:chat-7").await.unwrap();
    let tracer = Arc::new(crate::trace::Tracer::new(vec![backend]));
    make_runner_with_tracer(sm, tm, provider, wd, ms, Some(tracer))
}

#[tokio::test]
async fn test_runner_emits_full_span_tree() {
    let tmp = tempfile::TempDir::new().unwrap();
    let provider = Arc::new(MockProvider::new(vec![
        LLMResponse {
            content: Some("calling".to_string()),
            tool_calls: Some(vec![tool_call("tc-1", "echo")]),
            finish_reason: FinishReason::ToolCalls,
            usage: Usage {
                prompt_tokens: 10,
                prompt_cache_hit_tokens: 2,
                completion_tokens: 5,
                total_tokens: 15,
            },
        },
        LLMResponse {
            content: Some("done".to_string()),
            tool_calls: None,
            finish_reason: FinishReason::Stop,
            usage: Usage {
                prompt_tokens: 20,
                prompt_cache_hit_tokens: 3,
                completion_tokens: 7,
                total_tokens: 27,
            },
        },
    ]));
    let backend = Arc::new(RecordingTraceBackend::default());
    let runner = make_traced_runner(&tmp, provider, backend.clone()).await;

    let result = runner
        .run(
            "hi".to_string(),
            TaskHook::new("webui:chat-7"),
            "webui:chat-7",
            None,
            None,
            None,
            None,
        )
        .await;
    assert!(result.success);

    // ── Hierarchy: invoke_agent slimbot -> iteration -> chat / execute_tool echo ──
    let root = backend.start_named("invoke_agent slimbot");
    assert_eq!(root.parent_span_id, None);

    let iterations = backend
        .starts()
        .into_iter()
        .filter(|e| e.name == "iteration")
        .collect::<Vec<_>>();
    assert_eq!(iterations.len(), 2);
    for it in &iterations {
        assert_eq!(it.parent_span_id.as_deref(), Some(root.span_id.as_str()));
        assert_eq!(it.trace_id, root.trace_id);
    }

    let chats = backend
        .starts()
        .into_iter()
        .filter(|e| e.name == "chat")
        .collect::<Vec<_>>();
    assert_eq!(chats.len(), 2);
    for chat in &chats {
        assert!(
            iterations
                .iter()
                .any(|it| it.span_id == chat.parent_span_id.as_deref().unwrap())
        );
    }

    let tools = backend
        .starts()
        .into_iter()
        .filter(|e| e.name == "execute_tool echo")
        .collect::<Vec<_>>();
    assert_eq!(tools.len(), 1);
    assert!(
        iterations
            .iter()
            .any(|it| it.span_id == tools[0].parent_span_id.as_deref().unwrap())
    );

    // All spans share one trace.
    let trace_ids: std::collections::HashSet<_> = backend
        .events
        .lock()
        .unwrap()
        .iter()
        .map(|e| e.trace_id.clone())
        .collect();
    assert_eq!(trace_ids.len(), 1);

    // ── Root span attributes (GenAI semantic conventions) ──
    let root_end = backend.end_for(&root);
    assert_eq!(root_end.status, Some(crate::trace::SpanStatus::Ok));
    let attrs = &root_end.attributes;
    assert_eq!(
        attrs.get("gen_ai.operation.name"),
        Some(&serde_json::json!("invoke_agent"))
    );
    assert_eq!(
        attrs.get("gen_ai.agent.name"),
        Some(&serde_json::json!("slimbot"))
    );
    assert_eq!(
        attrs.get("gen_ai.conversation.id"),
        Some(&serde_json::json!("webui:chat-7"))
    );
    assert_eq!(attrs.get("channel"), Some(&serde_json::json!("webui")));
    assert_eq!(attrs.get("chat_id"), Some(&serde_json::json!("chat-7")));
    assert_eq!(attrs.get("max_iterations"), Some(&serde_json::json!(40)));
    assert_eq!(attrs.get("max_tokens"), Some(&serde_json::json!(4096)));
    assert_eq!(
        attrs.get("gen_ai.usage.input_tokens"),
        Some(&serde_json::json!(30))
    );
    assert_eq!(
        attrs.get("gen_ai.usage.output_tokens"),
        Some(&serde_json::json!(12))
    );
    assert_eq!(attrs.get("iterations"), Some(&serde_json::json!(1)));
    assert_eq!(
        attrs.get("gen_ai.conversation.compacted"),
        Some(&serde_json::json!(false))
    );

    // ── Iteration attributes ──
    let it1 = backend.end_for(&iterations[0]);
    assert_eq!(it1.attributes.get("iteration"), Some(&serde_json::json!(1)));
    let it2 = backend.end_for(&iterations[1]);
    assert_eq!(it2.attributes.get("iteration"), Some(&serde_json::json!(2)));

    // ── chat attributes (GenAI inference) ──
    for chat in &chats {
        let end = backend.end_for(chat);
        assert!(end.attributes.contains_key("gen_ai.operation.name"));
        assert!(end.attributes.contains_key("gen_ai.usage.input_tokens"));
        assert!(end.attributes.contains_key("gen_ai.usage.output_tokens"));
        assert!(
            end.attributes
                .contains_key("gen_ai.usage.cache_read.input_tokens")
        );
        assert!(
            end.attributes
                .contains_key("gen_ai.response.finish_reasons")
        );
    }

    // ── execute_tool echo attributes ──
    let tool_end = backend.end_for(&tools[0]);
    assert_eq!(
        tool_end.attributes.get("gen_ai.operation.name"),
        Some(&serde_json::json!("execute_tool"))
    );
    assert_eq!(
        tool_end.attributes.get("gen_ai.tool.name"),
        Some(&serde_json::json!("echo"))
    );
    assert_eq!(
        tool_end.attributes.get("gen_ai.tool.call.id"),
        Some(&serde_json::json!("tc-1"))
    );
    assert!(
        tool_end
            .attributes
            .contains_key("gen_ai.tool.call.arguments")
    );
}

#[tokio::test]
async fn test_runner_root_span_error_on_llm_failure() {
    let tmp = tempfile::TempDir::new().unwrap();
    let backend = Arc::new(RecordingTraceBackend::default());
    let runner = make_traced_runner(&tmp, Arc::new(FailingProvider), backend.clone()).await;

    let result = runner
        .run(
            "boom".to_string(),
            TaskHook::new("webui:chat-7"),
            "webui:chat-7",
            None,
            None,
            None,
            None,
        )
        .await;
    assert!(!result.success);

    let root = backend.start_named("invoke_agent slimbot");
    let root_end = backend.end_for(&root);
    match &root_end.status {
        Some(crate::trace::SpanStatus::Error(msg)) => {
            assert!(msg.contains("LLM API request failed"));
        }
        other => panic!("expected error status, got {other:?}"),
    }
    assert_eq!(
        root_end.attributes.get("error.type"),
        Some(&serde_json::json!("provider_error"))
    );
    // The failing chat span is also marked error (with the raw error).
    let chat = backend.start_named("chat");
    let chat_end = backend.end_for(&chat);
    match &chat_end.status {
        Some(crate::trace::SpanStatus::Error(msg)) => {
            assert!(msg.contains("Simulated LLM API failure"));
        }
        other => panic!("expected chat error status, got {other:?}"),
    }
    assert_eq!(
        chat_end.attributes.get("error.type"),
        Some(&serde_json::json!("provider_error"))
    );
}

#[tokio::test]
async fn test_runner_root_span_error_on_max_iterations() {
    let tmp = tempfile::TempDir::new().unwrap();
    let base_response = LLMResponse {
        content: None,
        tool_calls: Some(vec![tool_call("tc", "echo")]),
        finish_reason: FinishReason::ToolCalls,
        usage: Usage::default(),
    };
    let provider = Arc::new(MockProvider::new(
        std::iter::repeat_n(base_response, 50).collect(),
    ));
    let backend = Arc::new(RecordingTraceBackend::default());
    let runner = make_traced_runner(&tmp, provider, backend.clone()).await;

    let result = runner
        .run(
            "loop".to_string(),
            TaskHook::new("webui:chat-7"),
            "webui:chat-7",
            None,
            None,
            None,
            None,
        )
        .await;
    assert!(!result.success);

    let root = backend.start_named("invoke_agent slimbot");
    let root_end = backend.end_for(&root);
    match &root_end.status {
        Some(crate::trace::SpanStatus::Error(msg)) => {
            assert!(msg.contains("Reached max iterations"));
        }
        other => panic!("expected error status, got {other:?}"),
    }
    assert_eq!(
        root_end.attributes.get("error.type"),
        Some(&serde_json::json!("max_iterations"))
    );
}

#[tokio::test]
async fn test_runner_file_backend_writes_root_span_jsonl() {
    let tmp = tempfile::TempDir::new().unwrap();
    let path = tmp.path().join("traces.jsonl");
    let file_backend = crate::FileBackend::new(path.to_str().unwrap()).unwrap();
    let backend = Arc::new(file_backend);
    let tracer = Arc::new(crate::trace::Tracer::new(vec![backend]));

    let provider = Arc::new(MockProvider::new(vec![LLMResponse {
        content: Some("done".to_string()),
        tool_calls: None,
        finish_reason: FinishReason::Stop,
        usage: Usage::default(),
    }]));
    let (sm, tm, wd, ms) = make_test_env(&tmp, &[("echo", "echo-result")]).await;
    let runner = make_runner_with_tracer(sm, tm, provider, wd, ms, Some(tracer));

    let result = runner
        .run(
            "hi".to_string(),
            TaskHook::new("test-session"),
            "test-session",
            None,
            None,
            None,
            None,
        )
        .await;
    assert!(result.success);

    // Every finished span (root + iteration + chat) got a JSONL line; the
    // root line carries the session/channel attributes.
    let content = std::fs::read_to_string(&path).unwrap();
    let records: Vec<serde_json::Value> = content
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    assert_eq!(records.len(), 3, "root + iteration + chat spans");
    let root_record = records
        .iter()
        .find(|r| r["name"] == "invoke_agent slimbot")
        .expect("root span record");
    assert_eq!(root_record["status"], "ok");
    assert!(root_record["duration_us"].as_u64().is_some());
}

/// Provider wrapper that writes a request body through the recorder before
/// delegating; proves the runner forwards the recorder into `chat`.
struct BodyRecordingProvider {
    inner: Arc<dyn Provider>,
}

#[async_trait]
impl Provider for BodyRecordingProvider {
    async fn chat(
        &self,
        messages: &[&Message],
        tools: Option<&[ToolDefinition]>,
        recorder: Option<&dyn SpanRecorder>,
    ) -> Result<LLMResponse> {
        if let Some(rec) = recorder {
            rec.set_attribute(
                "http.request.body",
                serde_json::json!("{\"model\":\"gpt-4\",\"messages\":[]}"),
            );
        }
        self.inner.chat(messages, tools, recorder).await
    }
}

#[tokio::test]
async fn test_runner_chat_span_carries_provider_recorded_body() {
    let tmp = tempfile::TempDir::new().unwrap();
    let provider = Arc::new(BodyRecordingProvider {
        inner: Arc::new(MockProvider::new(vec![LLMResponse {
            content: Some("done".to_string()),
            tool_calls: None,
            finish_reason: FinishReason::Stop,
            usage: Usage::default(),
        }])),
    });
    let backend = Arc::new(RecordingTraceBackend::default());
    let runner = make_traced_runner(&tmp, provider, backend.clone()).await;

    let result = runner
        .run(
            "hi".to_string(),
            TaskHook::new("webui:chat-7"),
            "webui:chat-7",
            None,
            None,
            None,
            None,
        )
        .await;
    assert!(result.success);

    let chat = backend.start_named("chat");
    let chat_end = backend.end_for(&chat);
    assert_eq!(
        chat_end.attributes.get("http.request.body"),
        Some(&serde_json::json!("{\"model\":\"gpt-4\",\"messages\":[]}"))
    );
}

#[tokio::test]
async fn test_runner_chat_span_records_response_content() {
    let tmp = tempfile::TempDir::new().unwrap();
    let provider = Arc::new(MockProvider::new(vec![
        LLMResponse {
            content: Some("first reply".to_string()),
            tool_calls: Some(vec![tool_call("tc-1", "echo")]),
            finish_reason: FinishReason::ToolCalls,
            usage: Usage::default(),
        },
        LLMResponse {
            content: Some("second reply".to_string()),
            tool_calls: None,
            finish_reason: FinishReason::Stop,
            usage: Usage::default(),
        },
    ]));
    let backend = Arc::new(RecordingTraceBackend::default());
    let runner = make_traced_runner(&tmp, provider, backend.clone()).await;

    let result = runner
        .run(
            "hi".to_string(),
            TaskHook::new("webui:chat-7"),
            "webui:chat-7",
            None,
            None,
            None,
            None,
        )
        .await;
    assert!(result.success);

    let chats = backend
        .starts()
        .into_iter()
        .filter(|e| e.name == "chat")
        .collect::<Vec<_>>();
    assert_eq!(chats.len(), 2);
    assert_eq!(
        backend
            .end_for(&chats[0])
            .attributes
            .get("gen_ai.response.content"),
        Some(&serde_json::json!("first reply"))
    );
    assert_eq!(
        backend
            .end_for(&chats[1])
            .attributes
            .get("gen_ai.response.content"),
        Some(&serde_json::json!("second reply"))
    );
}

#[tokio::test]
async fn test_runner_chat_span_omits_content_when_absent() {
    let tmp = tempfile::TempDir::new().unwrap();
    // Tool-call-only responses carry no content, so the attribute must not
    // be recorded on those chat spans.
    let provider = Arc::new(MockProvider::new(vec![
        LLMResponse {
            content: None,
            tool_calls: Some(vec![tool_call("tc-1", "echo")]),
            finish_reason: FinishReason::ToolCalls,
            usage: Usage::default(),
        },
        LLMResponse {
            content: None,
            tool_calls: None,
            finish_reason: FinishReason::Stop,
            usage: Usage::default(),
        },
    ]));
    let backend = Arc::new(RecordingTraceBackend::default());
    let runner = make_traced_runner(&tmp, provider, backend.clone()).await;

    let result = runner
        .run(
            "hi".to_string(),
            TaskHook::new("webui:chat-7"),
            "webui:chat-7",
            None,
            None,
            None,
            None,
        )
        .await;
    assert!(result.success);

    let chats = backend
        .starts()
        .into_iter()
        .filter(|e| e.name == "chat")
        .collect::<Vec<_>>();
    assert_eq!(chats.len(), 2);
    for chat in &chats {
        assert!(
            !backend
                .end_for(chat)
                .attributes
                .contains_key("gen_ai.response.content"),
            "chat span must not record content when the response has none"
        );
    }
}

#[tokio::test]
async fn test_runner_tool_span_records_error_and_truncates_args() {
    let tmp = tempfile::TempDir::new().unwrap();
    let path = tmp.path();
    let session_dir = path.join("sessions");
    let workspace_dir = path.join("workspace");
    std::fs::create_dir_all(&workspace_dir).unwrap();

    let sm: SharedSessionManager = Arc::new(Mutex::new(SessionManager::new(session_dir).unwrap()));
    let mut tool_manager = ToolManager::new(workspace_dir.clone());
    tool_manager.register(Box::new(FailingTool));
    let tm = Arc::new(tool_manager);
    sm.lock().await.get_or_create("webui:chat-7").await.unwrap();
    let ms = Arc::new(tokio::sync::Mutex::new(MemoryService::new(&workspace_dir)));
    ms.lock().await.init().unwrap();

    // A tool call with oversized args exercises the 200-char truncation.
    let long_args = serde_json::json!({ "payload": "x".repeat(500) });
    let provider = Arc::new(MockProvider::new(vec![
        LLMResponse {
            content: Some("calling".to_string()),
            tool_calls: Some(vec![ToolCall {
                id: "tc-fail".to_string(),
                name: "fail_tool".to_string(),
                args: long_args,
            }]),
            finish_reason: FinishReason::ToolCalls,
            usage: Usage::default(),
        },
        LLMResponse {
            content: Some("done".to_string()),
            tool_calls: None,
            finish_reason: FinishReason::Stop,
            usage: Usage::default(),
        },
    ]));
    let backend = Arc::new(RecordingTraceBackend::default());
    let tracer = Arc::new(crate::trace::Tracer::new(vec![backend.clone()]));
    let runner = make_runner_with_tracer(sm, tm, provider, workspace_dir, ms, Some(tracer));

    let result = runner
        .run(
            "fail".to_string(),
            TaskHook::new("webui:chat-7"),
            "webui:chat-7",
            None,
            None,
            None,
            None,
        )
        .await;
    assert!(
        result.success,
        "tool errors surface as results, not run failures"
    );

    let tool = backend.start_named("execute_tool fail_tool");
    let tool_end = backend.end_for(&tool);
    let args = tool_end
        .attributes
        .get("gen_ai.tool.call.arguments")
        .and_then(|v| v.as_str());
    let args = args.expect("gen_ai.tool.call.arguments attribute");
    assert!(args.len() <= 200, "args must be truncated to 200 chars");
    assert_eq!(
        tool_end.attributes.get("error.type"),
        Some(&serde_json::json!("tool_error"))
    );
    match &tool_end.status {
        Some(crate::trace::SpanStatus::Error(msg)) => {
            assert!(msg.contains("Tool execution failed"));
            assert!(
                msg.len() <= 500,
                "error message must be truncated to 500 chars"
            );
        }
        other => panic!("expected error status, got {other:?}"),
    }
}

#[test]
fn test_truncate_chars_limits_and_char_boundary() {
    assert_eq!(truncate_chars("abc", 5), "abc");
    assert_eq!(truncate_chars("abcde", 3), "abc");
    // Multi-byte chars must not be split mid-character.
    assert_eq!(truncate_chars("héllo", 3), "hél");
    assert_eq!(truncate_chars("日本語", 2), "日本");
    assert_eq!(truncate_chars("", 10), "");
}

#[tokio::test]
async fn test_runner_root_span_error_on_cancelled_run() {
    let tmp = tempfile::TempDir::new().unwrap();
    let (sm, tm, wd, ms) = make_test_env(&tmp, &[]).await;
    let provider = Arc::new(MockProvider::new(vec![LLMResponse {
        content: Some("done".to_string()),
        tool_calls: None,
        finish_reason: FinishReason::Stop,
        usage: Usage::default(),
    }]));
    let backend = Arc::new(RecordingTraceBackend::default());
    let tracer = Arc::new(crate::trace::Tracer::new(vec![backend.clone()]));
    let runner = make_runner_with_tracer(sm, tm, provider, wd, ms, Some(tracer));

    // Pre-cancelled run: the root span must still be emitted with Error status.
    let cancel_token = tokio_util::sync::CancellationToken::new();
    cancel_token.cancel();
    let result = runner
        .run(
            "hi".to_string(),
            TaskHook::new("webui:chat-7"),
            "webui:chat-7",
            None,
            Some(cancel_token),
            None,
            None,
        )
        .await;
    assert!(!result.success);

    let root = backend.start_named("invoke_agent slimbot");
    let root_end = backend.end_for(&root);
    match &root_end.status {
        Some(crate::trace::SpanStatus::Error(msg)) => {
            assert!(msg.contains("Task cancelled"));
        }
        other => panic!("expected error status, got {other:?}"),
    }
    assert_eq!(
        root_end.attributes.get("error.type"),
        Some(&serde_json::json!("cancelled"))
    );
}

// ---- run wall-clock deadline (ADR 0018) ----

/// Provider that never answers: only the run deadline can end the turn.
struct HangingProvider {
    call_count: std::sync::atomic::AtomicU32,
}

impl HangingProvider {
    fn new() -> Self {
        Self {
            call_count: std::sync::atomic::AtomicU32::new(0),
        }
    }

    fn call_count(&self) -> u32 {
        self.call_count.load(std::sync::atomic::Ordering::SeqCst)
    }
}

#[async_trait]
impl Provider for HangingProvider {
    async fn chat(
        &self,
        _messages: &[&Message],
        _tools: Option<&[ToolDefinition]>,
        _recorder: Option<&dyn SpanRecorder>,
    ) -> Result<LLMResponse> {
        self.call_count
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        // Longer than any deadline used by the tests, so only the deadline can
        // end the turn.
        tokio::time::sleep(std::time::Duration::from_secs(5)).await;
        unreachable!("the run deadline must cut this call before it returns");
    }
}

/// A runner whose `agent.timeout_seconds` is `timeout_seconds`.
fn make_runner_with_run_deadline(
    sm: SharedSessionManager,
    tm: Arc<ToolManager>,
    provider: Arc<dyn Provider>,
    workspace_dir: PathBuf,
    ms: Arc<tokio::sync::Mutex<MemoryService>>,
    timeout_seconds: u64,
) -> AgentRunner {
    make_runner_with(sm, tm, provider, workspace_dir, ms, |b| {
        b.config(AgentConfig {
            provider: "test".to_string(),
            max_iterations: 40,
            timeout_seconds,
            persist_tool_results: false,
            unknown: Default::default(),
        })
    })
}

#[tokio::test]
async fn test_run_exceeding_deadline_cancels_via_stop_path_and_rolls_back() {
    let tmp = tempfile::TempDir::new().unwrap();
    let (sm, tm, wd, ms) = make_test_env(&tmp, &[]).await;

    // A turn that never finishes must not survive its messages.
    sm.lock()
        .await
        .add_message("test-session", Message::user("prev".to_string()))
        .await
        .unwrap();
    sm.lock()
        .await
        .add_message(
            "test-session",
            Message::assistant(Some("prev reply".to_string()), None, None, None),
        )
        .await
        .unwrap();

    let provider = Arc::new(HangingProvider::new());
    let runner = make_runner_with_run_deadline(sm.clone(), tm, provider.clone(), wd, ms, 1);

    let (event_tx, mut event_rx) = broadcast::channel::<AgentEvent>(16);
    let hook = TaskHook::new("test-session").with_events(event_tx);
    let cancel = CancellationToken::new();

    let result = runner
        .run(
            "hang please".to_string(),
            hook,
            "test-session",
            None,
            Some(cancel.clone()),
            None,
            None,
        )
        .await;

    assert!(!result.success, "an expired deadline must fail the run");
    assert!(
        result.content.contains("deadline"),
        "the failure must name the deadline, got: {}",
        result.content
    );
    assert_eq!(
        provider.call_count(),
        1,
        "the stuck call was attempted once"
    );

    // Reuses the /stop path: after-turn rollback still runs, so the turn leaves
    // no half-written messages behind.
    let msgs = sm.lock().await.get_messages("test-session").await;
    assert_eq!(
        msgs.len(),
        2,
        "rollback must restore the pre-turn message count, got {}",
        msgs.len()
    );

    // The failure is reported through the normal after-turn event path.
    let mut failed = None;
    while let Ok(event) = event_rx.try_recv() {
        if let AgentEvent::TaskFailed { error, .. } = event {
            failed = Some(error);
        }
    }
    let failed = failed.expect("a failed run must emit TaskFailed");
    assert!(
        failed.contains("deadline"),
        "TaskFailed must carry the deadline error, got: {failed}"
    );

    // The deadline cancels this run only: the session-wide /stop token stays
    // usable so queued sibling turns of the same session are not collateral.
    assert!(
        !cancel.is_cancelled(),
        "the deadline must not cancel the session-wide cancellation token"
    );
}
