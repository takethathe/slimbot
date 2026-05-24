use std::path::PathBuf;
use std::sync::Arc;

use tempfile::TempDir;
use tokio::sync::{Mutex, broadcast};

use slimbot::{
    AgentConfig, AgentEvent, AgentRunner, FinishReason, LLMResponse, MemoryStore, Message,
    Provider, SessionManager, SharedSessionManager, TaskHook, Tool, ToolCall, ToolDefinition,
    ToolManager, Usage,
};

// ── Mock Provider ──

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
}

#[async_trait::async_trait]
impl Provider for MockProvider {
    async fn chat(
        &self,
        _messages: &[&Message],
        _tools: Option<&[ToolDefinition]>,
    ) -> anyhow::Result<LLMResponse> {
        let idx = self
            .call_count
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let responses = self.responses.lock().unwrap();
        let resp = responses
            .get(idx as usize)
            .unwrap_or_else(|| panic!("MockProvider: no more responses for call {idx}"));
        Ok(resp.clone())
    }
}

// ── Mock Echo Tool ──

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

#[async_trait::async_trait]
impl Tool for MockEchoTool {
    fn name(&self) -> &str {
        &self.name
    }

    fn description(&self) -> &str {
        "Mock echo tool"
    }

    fn parameters(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {},
            "required": [],
        })
    }

    async fn execute(&self, _args: serde_json::Value) -> anyhow::Result<String> {
        Ok(self.return_value.clone())
    }
}

// ── Helpers ──

fn tool_call(id: &str, name: &str) -> ToolCall {
    ToolCall {
        id: id.to_string(),
        name: name.to_string(),
        args: serde_json::json!({}),
    }
}

fn test_config() -> AgentConfig {
    AgentConfig {
        provider: "test".to_string(),
        max_iterations: 40,
        timeout_seconds: 120,
        max_tool_result_chars: 8000,
        persist_tool_results: false,
        context_window_tokens: 32768,
        unknown: Default::default(),
    }
}

async fn make_test_env(
    tmp: &TempDir,
    tools: &[(&str, &str)],
) -> (
    SharedSessionManager,
    Arc<ToolManager>,
    PathBuf,
    Arc<Mutex<MemoryStore>>,
) {
    let session_dir = tmp.path().join("sessions");
    let workspace_dir = tmp.path().join("workspace");
    std::fs::create_dir_all(&workspace_dir).unwrap();

    let sm: SharedSessionManager = Arc::new(Mutex::new(SessionManager::new(session_dir).unwrap()));
    let mut tm = ToolManager::new(workspace_dir.clone());
    for (name, ret) in tools {
        tm.register(Box::new(MockEchoTool::new(name, ret)));
    }

    let ms = Arc::new(Mutex::new(MemoryStore::new(&workspace_dir)));
    ms.lock().await.init().unwrap();

    sm.lock().await.get_or_create("test-session").await.unwrap();

    (sm, Arc::new(tm), workspace_dir, ms)
}

fn make_runner(
    sm: SharedSessionManager,
    tm: Arc<ToolManager>,
    provider: Arc<dyn Provider>,
    workspace_dir: PathBuf,
    ms: Arc<Mutex<MemoryStore>>,
) -> AgentRunner {
    AgentRunner::new(tm, provider, sm, test_config(), workspace_dir, ms, None)
}

// ── Test: Full event sequence with single tool call ──

#[tokio::test]
async fn test_event_sequence_single_tool() {
    let tmp = TempDir::new().unwrap();
    let (sm, tm, wd, ms) = make_test_env(&tmp, &[("tool1", "result1")]).await;

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

    let runner = make_runner(sm, tm, provider, wd, ms);
    let hook = TaskHook::new("test-session").with_events(event_tx);

    let result = runner
        .run(
            "run tool".to_string(),
            hook,
            "test-session",
            None,
            None,
            None,
            None,
        )
        .await;

    assert!(result.success, "run should succeed");

    let mut events = Vec::new();
    while let Ok(evt) = event_rx.try_recv() {
        events.push(evt);
    }

    assert!(!events.is_empty(), "should have emitted events");

    // Verify order
    assert!(
        matches!(&events[0], AgentEvent::TaskStarted { .. }),
        "first should be TaskStarted, got {:?}",
        events[0]
    );

    // Find indices of key events
    let tool_call_idx = events
        .iter()
        .position(|e| matches!(e, AgentEvent::ToolCall { name, .. } if name == "tool1"))
        .expect("should have ToolCall for tool1");
    let tool_result_idx = events
        .iter()
        .position(|e| matches!(e, AgentEvent::ToolResult { name, .. } if name == "tool1"))
        .expect("should have ToolResult for tool1");
    let completed_idx = events
        .iter()
        .position(|e| matches!(e, AgentEvent::TaskCompleted { .. }))
        .expect("should have TaskCompleted");

    assert!(
        tool_call_idx < tool_result_idx,
        "ToolCall should come before ToolResult"
    );
    assert!(
        tool_result_idx < completed_idx,
        "ToolResult should come before TaskCompleted"
    );

    // Verify PreIteration/PostIteration
    assert!(
        events
            .iter()
            .any(|e| matches!(e, AgentEvent::PreIteration { iteration: 0, .. })),
        "should have PreIteration for iteration 0"
    );
}

// ── Test: Multi-iteration with multiple tools per iteration ──

#[tokio::test]
async fn test_event_sequence_multi_iteration() {
    let tmp = TempDir::new().unwrap();
    let (sm, tm, wd, ms) = make_test_env(&tmp, &[("tool_a", "A"), ("tool_b", "B")]).await;

    let (event_tx, mut event_rx) = broadcast::channel::<AgentEvent>(64);

    let provider = Arc::new(MockProvider::new(vec![
        // Iteration 0: call tool_a
        LLMResponse {
            content: Some("calling A".to_string()),
            tool_calls: Some(vec![tool_call("tc-1", "tool_a")]),
            finish_reason: FinishReason::ToolCalls,
            usage: Usage::default(),
        },
        // Iteration 1: call tool_b
        LLMResponse {
            content: Some("calling B".to_string()),
            tool_calls: Some(vec![tool_call("tc-2", "tool_b")]),
            finish_reason: FinishReason::ToolCalls,
            usage: Usage::default(),
        },
        // Iteration 2: done
        LLMResponse {
            content: Some("all done".to_string()),
            tool_calls: None,
            finish_reason: FinishReason::Stop,
            usage: Usage::default(),
        },
    ]));

    let runner = make_runner(sm, tm, provider, wd, ms);
    let hook = TaskHook::new("test-session").with_events(event_tx);

    let result = runner
        .run(
            "multi tool".to_string(),
            hook,
            "test-session",
            None,
            None,
            None,
            None,
        )
        .await;

    assert!(result.success);

    let mut events = Vec::new();
    while let Ok(evt) = event_rx.try_recv() {
        events.push(evt);
    }

    // Both tools should appear
    let tool_a_calls = events
        .iter()
        .filter(|e| matches!(e, AgentEvent::ToolCall { name, .. } if name == "tool_a"))
        .count();
    let tool_b_calls = events
        .iter()
        .filter(|e| matches!(e, AgentEvent::ToolCall { name, .. } if name == "tool_b"))
        .count();
    assert_eq!(tool_a_calls, 1, "tool_a should be called once");
    assert_eq!(tool_b_calls, 1, "tool_b should be called once");

    // Should have PreIteration for iterations 0, 1, 2
    for i in 0..=2u32 {
        assert!(
            events.iter().any(
                |e| matches!(e, AgentEvent::PreIteration { iteration, .. } if *iteration == i)
            ),
            "should have PreIteration for iteration {i}"
        );
    }

    // PostIteration fires with the same iteration number as the matching PreIteration
    // PreIteration { iteration: 0 } → tools run → PostIteration { iteration: 0 }
    // PreIteration { iteration: 1 } → tools run → PostIteration { iteration: 1 }
    for i in 0..=1u32 {
        assert!(
            events.iter().any(
                |e| matches!(e, AgentEvent::PostIteration { iteration, .. } if *iteration == i)
            ),
            "should have PostIteration for iteration {i}"
        );
    }

    // Last event should be TaskCompleted
    assert!(
        matches!(events.last().unwrap(), AgentEvent::TaskCompleted { .. }),
        "last event should be TaskCompleted, got {:?}",
        events.last().unwrap()
    );
}

// ── Test: TaskFailed on provider error ──

#[tokio::test]
async fn test_event_task_failed_on_error() {
    let tmp = TempDir::new().unwrap();
    let (sm, tm, wd, ms) = make_test_env(&tmp, &[]).await;

    let (event_tx, mut event_rx) = broadcast::channel::<AgentEvent>(16);

    struct ErrorProvider;

    #[async_trait::async_trait]
    impl Provider for ErrorProvider {
        async fn chat(
            &self,
            _messages: &[&Message],
            _tools: Option<&[ToolDefinition]>,
        ) -> anyhow::Result<LLMResponse> {
            Err(anyhow::anyhow!("Simulated LLM failure"))
        }
    }

    let runner = make_runner(sm, tm, Arc::new(ErrorProvider), wd, ms);
    let hook = TaskHook::new("test-session").with_events(event_tx);

    let result = runner
        .run(
            "fail me".to_string(),
            hook,
            "test-session",
            None,
            None,
            None,
            None,
        )
        .await;

    assert!(!result.success, "run should fail");

    let mut events = Vec::new();
    while let Ok(evt) = event_rx.try_recv() {
        events.push(evt);
    }

    assert!(
        matches!(events.last().unwrap(), AgentEvent::TaskFailed { .. }),
        "last event should be TaskFailed, got {:?}",
        events.last().unwrap()
    );

    // Verify error message is propagated
    let last = events.last().unwrap();
    if let AgentEvent::TaskFailed { error, .. } = last {
        assert!(
            error.contains("LLM failure"),
            "error should contain 'LLM failure', got: {error}"
        );
    } else {
        panic!("expected TaskFailed");
    }
}

// ── Test: TaskCompleted with correct result text ──

#[tokio::test]
async fn test_task_completed_has_result() {
    let tmp = TempDir::new().unwrap();
    let (sm, tm, wd, ms) = make_test_env(&tmp, &[]).await;

    let (event_tx, mut event_rx) = broadcast::channel::<AgentEvent>(16);

    let provider = Arc::new(MockProvider::new(vec![LLMResponse {
        content: Some("Final answer".to_string()),
        tool_calls: None,
        finish_reason: FinishReason::Stop,
        usage: Usage::default(),
    }]));

    let runner = make_runner(sm, tm, provider, wd, ms);
    let hook = TaskHook::new("test-session").with_events(event_tx);

    let _ = runner
        .run(
            "hello".to_string(),
            hook,
            "test-session",
            None,
            None,
            None,
            None,
        )
        .await;

    let mut events = Vec::new();
    while let Ok(evt) = event_rx.try_recv() {
        events.push(evt);
    }

    let completed = events
        .iter()
        .find_map(|e| match e {
            AgentEvent::TaskCompleted { result, .. } => Some(result),
            _ => None,
        })
        .expect("should have TaskCompleted event");

    assert_eq!(completed, "Final answer");
}

// ── Test: Direct reply (no tools) has minimal events ──

#[tokio::test]
async fn test_direct_reply_no_tool_events() {
    let tmp = TempDir::new().unwrap();
    let (sm, tm, wd, ms) = make_test_env(&tmp, &[]).await;

    let (event_tx, mut event_rx) = broadcast::channel::<AgentEvent>(16);

    let provider = Arc::new(MockProvider::new(vec![LLMResponse {
        content: Some("Hello back!".to_string()),
        tool_calls: None,
        finish_reason: FinishReason::Stop,
        usage: Usage {
            prompt_tokens: 10,
            prompt_cache_hit_tokens: 0,
            completion_tokens: 5,
            total_tokens: 15,
        },
    }]));

    let runner = make_runner(sm, tm, provider, wd, ms);
    let hook = TaskHook::new("test-session").with_events(event_tx);

    let _ = runner
        .run(
            "hi".to_string(),
            hook,
            "test-session",
            None,
            None,
            None,
            None,
        )
        .await;

    let mut events = Vec::new();
    while let Ok(evt) = event_rx.try_recv() {
        events.push(evt);
    }

    // Should only have: TaskStarted, PreIteration, (maybe AssistantMessage), TaskCompleted
    // No ToolCall or ToolResult events
    let has_tool_call = events
        .iter()
        .any(|e| matches!(e, AgentEvent::ToolCall { .. }));
    let has_tool_result = events
        .iter()
        .any(|e| matches!(e, AgentEvent::ToolResult { .. }));

    assert!(!has_tool_call, "should not have ToolCall for direct reply");
    assert!(
        !has_tool_result,
        "should not have ToolResult for direct reply"
    );
}

// ── Test: ToolResult contains actual tool output ──

#[tokio::test]
async fn test_tool_result_contains_output() {
    let tmp = TempDir::new().unwrap();
    let (sm, tm, wd, ms) = make_test_env(&tmp, &[("greet", "Hello from tool")]).await;

    let (event_tx, mut event_rx) = broadcast::channel::<AgentEvent>(32);

    let provider = Arc::new(MockProvider::new(vec![
        LLMResponse {
            content: Some("greeting".to_string()),
            tool_calls: Some(vec![tool_call("tc-1", "greet")]),
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

    let runner = make_runner(sm, tm, provider, wd, ms);
    let hook = TaskHook::new("test-session").with_events(event_tx);

    let _ = runner
        .run(
            "greet me".to_string(),
            hook,
            "test-session",
            None,
            None,
            None,
            None,
        )
        .await;

    let mut events = Vec::new();
    while let Ok(evt) = event_rx.try_recv() {
        events.push(evt);
    }

    let tool_result_output = events.iter().find_map(|e| match e {
        AgentEvent::ToolResult { name, output, .. } if name == "greet" => Some(output),
        _ => None,
    });

    assert!(
        tool_result_output.is_some(),
        "should have ToolResult for greet tool"
    );
    assert!(
        tool_result_output.unwrap().contains("Hello from tool"),
        "ToolResult output should contain tool's return value"
    );
}

// ── Test: Event serialization for SSE (WebUI contract) ──

#[test]
fn test_agent_event_serializes_correctly() {
    // Verify the serde tag produces expected JSON for the frontend SSE contract
    let event = AgentEvent::TaskStarted {
        session_id: "webui:chat1".to_string(),
    };
    let json = serde_json::to_string(&event).unwrap();
    assert!(json.contains(r#""type":"task_started""#));
    assert!(json.contains(r#""session_id":"webui:chat1""#));

    let event = AgentEvent::ToolCall {
        session_id: "webui:chat1".to_string(),
        name: "shell".to_string(),
        args: r#"{"command": "ls"}"#.to_string(),
    };
    let json = serde_json::to_string(&event).unwrap();
    assert!(json.contains(r#""type":"tool_call""#));
    assert!(json.contains(r#""name":"shell""#));

    let event = AgentEvent::TaskCompleted {
        session_id: "webui:chat1".to_string(),
        result: "All done".to_string(),
    };
    let json = serde_json::to_string(&event).unwrap();
    assert!(json.contains(r#""type":"task_completed""#));
    assert!(json.contains(r#""result":"All done""#));
}

// ── Test: Broadcast capacity handles burst events ──

#[tokio::test]
async fn test_broadcast_no_lagged_events() {
    let tmp = TempDir::new().unwrap();
    // Register many tools to create many tool calls per iteration
    let tools: Vec<_> = (0..5)
        .map(|i| (format!("tool_{i}"), format!("result_{i}")))
        .collect();
    let tool_refs: Vec<_> = tools
        .iter()
        .map(|(n, r)| (n.as_str(), r.as_str()))
        .collect();
    let (sm, tm, wd, ms) = make_test_env(&tmp, &tool_refs).await;

    // Large enough buffer for all events: 5 ToolCalls + 5 ToolResults + PreIteration + PostIteration + AssistantMessage + TaskStarted + TaskCompleted = ~15
    let (event_tx, mut event_rx) = broadcast::channel::<AgentEvent>(64);

    // Build a response that calls all 5 tools in one iteration
    let tool_calls: Vec<_> = (0..5)
        .map(|i| tool_call(&format!("tc-{i}"), &format!("tool_{i}")))
        .collect();

    let mut responses = vec![LLMResponse {
        content: Some("calling tools".to_string()),
        tool_calls: Some(tool_calls),
        finish_reason: FinishReason::ToolCalls,
        usage: Usage::default(),
    }];
    // Final response to stop
    responses.push(LLMResponse {
        content: Some("done".to_string()),
        tool_calls: None,
        finish_reason: FinishReason::Stop,
        usage: Usage::default(),
    });

    let provider = Arc::new(MockProvider::new(responses));
    let runner = make_runner(sm, tm, provider, wd, ms);
    let hook = TaskHook::new("test-session").with_events(event_tx);

    let _ = runner
        .run(
            "bulk test".to_string(),
            hook,
            "test-session",
            None,
            None,
            None,
            None,
        )
        .await;

    let mut events = Vec::new();
    while let Ok(evt) = event_rx.try_recv() {
        events.push(evt);
    }

    // All 5 tools should have been called
    for i in 0..5 {
        let name = format!("tool_{i}");
        assert!(
            events
                .iter()
                .any(|e| matches!(e, AgentEvent::ToolCall { name: n, .. } if n == &name)),
            "should have ToolCall for {name}"
        );
    }
}
