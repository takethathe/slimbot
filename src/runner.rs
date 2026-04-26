use std::sync::Arc;

use anyhow::Result;

use crate::config::AgentConfig;
use crate::context::ContextBuilder;
use crate::provider::Provider;
use crate::session::{Message, SessionManager, SessionTask, SharedSessionManager, TaskState};
use crate::tool::ToolManager;

pub struct AgentRunner {
    context_builder: ContextBuilder,
    tool_manager: Arc<ToolManager>,
    provider: Arc<dyn Provider>,
    session_manager: SharedSessionManager,
    config: AgentConfig,
}

impl AgentRunner {
    pub fn new(
        context_builder: ContextBuilder,
        tool_manager: Arc<ToolManager>,
        provider: Arc<dyn Provider>,
        session_manager: SharedSessionManager,
        config: AgentConfig,
    ) -> Self {
        Self {
            context_builder,
            tool_manager,
            provider,
            session_manager,
            config,
        }
    }

    pub async fn run(
        &self,
        task: &mut SessionTask,
        session_id: &str,
        channel_inject: Option<String>,
    ) -> Result<String> {
        // 1. Write user message
        {
            let mut sm: tokio::sync::MutexGuard<'_, SessionManager> =
                self.session_manager.lock().await;
            sm.add_message(
                session_id,
                Message::User {
                    content: task.content.clone(),
                },
            )
            .await?;
        }

        // 2. Update task state to Running
        let running_state = TaskState::Running {
            current_iteration: 0,
        };
        {
            let mut sm: tokio::sync::MutexGuard<'_, SessionManager> =
                self.session_manager.lock().await;
            sm.update_task_state(session_id, &task.id, running_state.clone())
                .await;
        }
        task.hook.notify_status_change(&running_state);

        let max_iterations = self.config.max_iterations;
        let mut iterations: u32 = 0;

        loop {
            // Exceeded max iterations
            if iterations >= max_iterations {
                let err_msg = format!("Reached max iterations {}", max_iterations);
                let failed_state = TaskState::Failed {
                    error: err_msg.clone(),
                };
                {
                    let mut sm: tokio::sync::MutexGuard<'_, SessionManager> =
                        self.session_manager.lock().await;
                    sm.update_task_state(session_id, &task.id, failed_state.clone())
                        .await;
                }
                task.hook.notify_status_change(&failed_state);
                {
                    let sm: tokio::sync::MutexGuard<'_, SessionManager> =
                        self.session_manager.lock().await;
                    sm.persist(session_id).await?;
                }
                return Err(anyhow::anyhow!(err_msg));
            }

            // Build context
            let ctx = self
                .context_builder
                .build(session_id, channel_inject.clone())
                .await;

            // Request model
            let response = self
                .provider
                .chat(&ctx.messages, ctx.tools.as_deref())
                .await?;

            // No tool calls → done
            let has_tool_calls = response
                .tool_calls
                .as_ref()
                .is_some_and(|calls| !calls.is_empty());

            if !has_tool_calls {
                let text = response.content.clone().unwrap_or_default();
                {
                    let mut sm: tokio::sync::MutexGuard<'_, SessionManager> =
                        self.session_manager.lock().await;
                    sm.add_message(
                        session_id,
                        Message::Assistant {
                            content: Some(text.clone()),
                            tool_calls: None,
                        },
                    )
                    .await?;

                    let completed_state = TaskState::Completed {
                        result: text.clone(),
                    };
                    sm.update_task_state(session_id, &task.id, completed_state.clone())
                        .await;
                    sm.persist(session_id).await?;
                }
                task.hook.notify_status_change(&TaskState::Completed {
                    result: text.clone(),
                });
                return Ok(text);
            }

            // Has tool calls → write ONE assistant message with content + all tool_calls,
            // then execute each tool and append its Tool message.
            if let Some(calls) = &response.tool_calls {
                // 1. Persist the assistant reply (preserves model's content text)
                {
                    let mut sm: tokio::sync::MutexGuard<'_, SessionManager> =
                        self.session_manager.lock().await;
                    sm.add_message(
                        session_id,
                        Message::Assistant {
                            content: response.content.clone(),
                            tool_calls: Some(calls.clone()),
                        },
                    )
                    .await?;
                }

                // 2. Execute tools and write Tool messages
                for call in calls {
                    let raw_result = self
                        .tool_manager
                        .execute(&call.name, call.args.clone())
                        .await?;
                    let processed_result =
                        task.hook.process_tool_result(&call.name, &raw_result)?;

                    {
                        let mut sm: tokio::sync::MutexGuard<'_, SessionManager> =
                            self.session_manager.lock().await;
                        sm.add_message(
                            session_id,
                            Message::Tool {
                                content: processed_result,
                                tool_call_id: call.id.clone(),
                            },
                        )
                        .await?;
                    }
                }

                iterations += 1;
                let running_state = TaskState::Running {
                    current_iteration: iterations,
                };
                {
                    let mut sm: tokio::sync::MutexGuard<'_, SessionManager> =
                        self.session_manager.lock().await;
                    sm.update_task_state(session_id, &task.id, running_state.clone())
                        .await;
                }
                task.hook.notify_status_change(&running_state);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::sync::Arc;

    use anyhow::Result;
    use async_trait::async_trait;
    use tokio::sync::Mutex;

    use super::*;
    use crate::provider::{ChatResponse, FinishReason};
    use crate::session::{Message, SessionTask, TaskHook, TaskState};
    use crate::tool::{Tool, ToolCall, ToolDefinition};

    /// Mock provider that returns predefined responses in order.
    struct MockProvider {
        responses: std::sync::Mutex<Vec<ChatResponse>>,
        call_count: std::sync::atomic::AtomicU32,
    }

    impl MockProvider {
        fn new(responses: Vec<ChatResponse>) -> Self {
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
            _messages: &[Message],
            _tools: Option<&[ToolDefinition]>,
        ) -> Result<ChatResponse> {
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
    ) -> (SharedSessionManager, Arc<ToolManager>, PathBuf) {
        let path = tmp_dir.path();
        let session_dir = path.join("sessions");
        let workspace_dir = path.join("workspace");
        std::fs::create_dir_all(&workspace_dir).unwrap();

        let sm: SharedSessionManager =
            Arc::new(Mutex::new(SessionManager::new(session_dir).unwrap()));
        let mut tm = ToolManager::new(workspace_dir.clone());
        for (name, ret) in tool_names {
            tm.register(Box::new(MockEchoTool::new(name, ret)));
        }

        // Pre-create the default test session
        sm.lock().await.get_or_create("test-session").await.unwrap();

        (sm, Arc::new(tm), workspace_dir)
    }

    fn make_task(content: &str) -> SessionTask {
        SessionTask {
            id: "task-1".to_string(),
            content: content.to_string(),
            hook: TaskHook::new("test-session"),
            state: TaskState::Pending,
        }
    }

    fn make_runner(
        sm: SharedSessionManager,
        tm: Arc<ToolManager>,
        provider: Arc<dyn Provider>,
        workspace_dir: PathBuf,
    ) -> AgentRunner {
        AgentRunner::new(
            ContextBuilder::new(sm.clone(), tm.clone(), workspace_dir),
            tm,
            provider,
            sm,
            AgentConfig {
                provider: "test".to_string(),
                max_iterations: 40,
                timeout_seconds: 120,
            },
        )
    }

    #[tokio::test]
    async fn test_direct_reply_no_tool_calls() {
        let tmp = tempfile::TempDir::new().unwrap();
        let (sm, tm, wd) = make_test_env(&tmp, &[]).await;
        let provider = Arc::new(MockProvider::new(vec![ChatResponse {
            content: Some("Hello, world!".to_string()),
            tool_calls: None,
            finish_reason: FinishReason::Stop,
        }]));
        let runner = make_runner(sm.clone(), tm, provider.clone(), wd);
        let mut task = make_task("Say hi");

        let result = runner.run(&mut task, "test-session", None).await.unwrap();
        assert_eq!(result, "Hello, world!");
        assert_eq!(provider.call_count(), 1);

        let msgs = sm.lock().await.get_messages("test-session").await;
        assert_eq!(msgs.len(), 2); // user + assistant (system prompt is not stored in session)
        assert!(
            matches!(&msgs[0], Message::User { content } if content == "Say hi")
        );
        assert!(
            matches!(&msgs[1], Message::Assistant { content: Some(c), tool_calls: None } if c == "Hello, world!")
        );
    }

    #[tokio::test]
    async fn test_single_tool_call_with_content() {
        let tmp = tempfile::TempDir::new().unwrap();
        let (sm, tm, wd) = make_test_env(&tmp, &[("echo", "echo-result")]).await;
        let provider = Arc::new(MockProvider::new(vec![
            ChatResponse {
                content: Some("Let me echo that for you.".to_string()),
                tool_calls: Some(vec![tool_call("tc-1", "echo")]),
                finish_reason: FinishReason::ToolCalls,
            },
            ChatResponse {
                content: Some("Done!".to_string()),
                tool_calls: None,
                finish_reason: FinishReason::Stop,
            },
        ]));
        let runner = make_runner(sm.clone(), tm, provider.clone(), wd);
        let mut task = make_task("Run echo");

        let result = runner.run(&mut task, "test-session", None).await.unwrap();
        assert_eq!(result, "Done!");
        assert_eq!(provider.call_count(), 2);

        let msgs = sm.lock().await.get_messages("test-session").await;
        // user + assistant(content+tool_calls) + tool + assistant(final)
        assert_eq!(msgs.len(), 4);
        match &msgs[1] {
            Message::Assistant { content, tool_calls } => {
                assert_eq!(
                    content.as_deref(),
                    Some("Let me echo that for you.")
                );
                let calls = tool_calls.as_ref().unwrap();
                assert_eq!(calls.len(), 1);
                assert_eq!(calls[0].id, "tc-1");
            }
            other => panic!("Expected Assistant with content and tool_calls, got {:?}", other),
        }
        match &msgs[2] {
            Message::Tool { content, tool_call_id } => {
                assert_eq!(content, "echo-result");
                assert_eq!(tool_call_id, "tc-1");
            }
            other => panic!("Expected Tool message, got {:?}", other),
        }
    }

    #[tokio::test]
    async fn test_multiple_tool_calls_content_preserved() {
        let tmp = tempfile::TempDir::new().unwrap();
        let (sm, tm, wd) = make_test_env(
            &tmp,
            &[
                ("list", "listing-contents"),
                ("read", "file-contents"),
            ],
        )
        .await;
        let provider = Arc::new(MockProvider::new(vec![
            ChatResponse {
                content: Some("I'll check both the list and the file for you.".to_string()),
                tool_calls: Some(vec![
                    tool_call("tc-list", "list"),
                    tool_call("tc-read", "read"),
                ]),
                finish_reason: FinishReason::ToolCalls,
            },
            ChatResponse {
                content: Some(
                    "Here are the results: listing-contents and file-contents.".to_string(),
                ),
                tool_calls: None,
                finish_reason: FinishReason::Stop,
            },
        ]));
        let runner = make_runner(sm.clone(), tm, provider.clone(), wd);
        let mut task = make_task("List and read");

        let result = runner.run(&mut task, "test-session", None).await.unwrap();
        assert_eq!(
            result,
            "Here are the results: listing-contents and file-contents."
        );
        assert_eq!(provider.call_count(), 2);

        let msgs = sm.lock().await.get_messages("test-session").await;
        // user + assistant(content+2tool_calls) + tool + tool + assistant(final)
        assert_eq!(msgs.len(), 5);

        // Verify assistant message has both content and both tool_calls
        match &msgs[1] {
            Message::Assistant { content, tool_calls } => {
                assert_eq!(
                    content.as_deref(),
                    Some("I'll check both the list and the file for you.")
                );
                let calls = tool_calls.as_ref().unwrap();
                assert_eq!(calls.len(), 2);
                assert_eq!(calls[0].name, "list");
                assert_eq!(calls[1].name, "read");
            }
            other => panic!("Expected Assistant with content and 2 tool_calls, got {:?}", other),
        }

        // Verify both tool results are recorded
        assert!(
            matches!(&msgs[2], Message::Tool { content, tool_call_id } if content == "listing-contents" && tool_call_id == "tc-list")
        );
        assert!(
            matches!(&msgs[3], Message::Tool { content, tool_call_id } if content == "file-contents" && tool_call_id == "tc-read")
        );
    }

    #[tokio::test]
    async fn test_multiple_tool_calls_with_empty_content() {
        let tmp = tempfile::TempDir::new().unwrap();
        let (sm, tm, wd) =
            make_test_env(&tmp, &[("tool_a", "result-a"), ("tool_b", "result-b")]).await;
        let provider = Arc::new(MockProvider::new(vec![
            ChatResponse {
                content: None,
                tool_calls: Some(vec![tool_call("tc-a", "tool_a"), tool_call("tc-b", "tool_b")]),
                finish_reason: FinishReason::ToolCalls,
            },
            ChatResponse {
                content: Some("Final answer".to_string()),
                tool_calls: None,
                finish_reason: FinishReason::Stop,
            },
        ]));
        let runner = make_runner(sm.clone(), tm, provider.clone(), wd);
        let mut task = make_task("Run two tools");

        let result = runner.run(&mut task, "test-session", None).await.unwrap();
        assert_eq!(result, "Final answer");

        let msgs = sm.lock().await.get_messages("test-session").await;
        match &msgs[1] {
            Message::Assistant { content, tool_calls } => {
                assert!(content.is_none());
                assert_eq!(tool_calls.as_ref().unwrap().len(), 2);
            }
            other => panic!("Expected Assistant, got {:?}", other),
        }
    }

    #[tokio::test]
    async fn test_max_iterations_exceeded() {
        let tmp = tempfile::TempDir::new().unwrap();
        let (sm, tm, wd) = make_test_env(&tmp, &[("loop_tool", "x")]).await;
        // Provider always returns tool_calls, forcing the loop to continue
        let base_response = ChatResponse {
            content: None,
            tool_calls: Some(vec![tool_call("tc", "loop_tool")]),
            finish_reason: FinishReason::ToolCalls,
        };
        let provider = Arc::new(MockProvider::new(
            std::iter::repeat(base_response).take(50).collect(),
        ));
        let runner = make_runner(sm.clone(), tm, provider.clone(), wd);
        let mut task = make_task("Loop forever");

        let err = runner
            .run(&mut task, "test-session", None)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("Reached max iterations 40"));

        // Verify session was persisted
        let msgs = sm.lock().await.get_messages("test-session").await;
        assert!(!msgs.is_empty());

        let jsonl_path = tmp.path().join("sessions/test-session.jsonl");
        assert!(jsonl_path.exists());
    }

    #[tokio::test]
    async fn test_task_state_transitions() {
        let tmp = tempfile::TempDir::new().unwrap();
        let (sm, tm, wd) = make_test_env(&tmp, &[("tool1", "r1")]).await;

        // Capture state changes via TaskHook
        let (status_tx, mut status_rx) =
            tokio::sync::mpsc::channel::<(String, TaskState)>(32);

        let provider = Arc::new(MockProvider::new(vec![
            ChatResponse {
                content: Some("running tool".to_string()),
                tool_calls: Some(vec![tool_call("tc-1", "tool1")]),
                finish_reason: FinishReason::ToolCalls,
            },
            ChatResponse {
                content: Some("done".to_string()),
                tool_calls: None,
                finish_reason: FinishReason::Stop,
            },
        ]));
        let runner = make_runner(sm.clone(), tm, provider, wd);
        let mut task = make_task("test");
        task.hook = TaskHook::new("test-session").with_status_channel(status_tx);

        let _ = runner.run(&mut task, "test-session", None).await.unwrap();

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
        assert!(matches!(states.last().unwrap(), TaskState::Completed { .. }));
    }

    #[tokio::test]
    async fn test_tool_error_stops_loop() {
        let tmp = tempfile::TempDir::new().unwrap();
        let path = tmp.path();
        let session_dir = path.join("sessions");
        let workspace_dir = path.join("workspace");
        std::fs::create_dir_all(&workspace_dir).unwrap();

        let sm: SharedSessionManager =
            Arc::new(Mutex::new(SessionManager::new(session_dir).unwrap()));
        let mut tm = ToolManager::new(workspace_dir.clone());
        tm.register(Box::new(FailingTool));
        let tm = Arc::new(tm);
        sm.lock().await.get_or_create("test-session").await.unwrap();

        let provider = Arc::new(MockProvider::new(vec![ChatResponse {
            content: Some("trying...".to_string()),
            tool_calls: Some(vec![tool_call("tc-fail", "fail_tool")]),
            finish_reason: FinishReason::ToolCalls,
        }]));
        let runner = make_runner(sm.clone(), tm, provider, workspace_dir);
        let mut task = make_task("test");

        let err = runner
            .run(&mut task, "test-session", None)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("Tool execution failed"));
    }

    #[tokio::test]
    async fn test_persistence_and_reload() {
        let tmp = tempfile::TempDir::new().unwrap();
        let (sm, tm, wd) = make_test_env(&tmp, &[]).await;

        let provider = Arc::new(MockProvider::new(vec![ChatResponse {
            content: Some("persist me".to_string()),
            tool_calls: None,
            finish_reason: FinishReason::Stop,
        }]));
        let runner = make_runner(sm.clone(), tm, provider, wd);
        let mut task = make_task("hello");

        let result = runner.run(&mut task, "test-session", None).await.unwrap();
        assert_eq!(result, "persist me");

        // Create a new SessionManager and reload from disk
        let session_dir = tmp.path().join("sessions");
        let sm2: SharedSessionManager =
            Arc::new(Mutex::new(SessionManager::new(session_dir).unwrap()));
        sm2.lock().await.get_or_create("test-session").await.unwrap();

        let msgs = sm2.lock().await.get_messages("test-session").await;
        assert_eq!(msgs.len(), 2); // user + assistant
        assert!(
            matches!(&msgs[0], Message::User { content } if content == "hello")
        );
        assert!(
            matches!(&msgs[1], Message::Assistant { content: Some(c), tool_calls: None } if c == "persist me")
        );
    }
}
