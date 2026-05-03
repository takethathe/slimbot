use std::path::PathBuf;
use std::sync::Arc;

use crate::config::AgentConfig;
use crate::consolidate::Consolidator;
use crate::context::ContextBuilder;
use crate::memory::SharedMemoryStore;
use crate::provider::{Provider, Usage};
use crate::session::{Message, SessionManager, SharedSessionManager, TaskHook, TaskState};
use crate::tool::{ToolManager, ensure_nonempty_tool_result, format_tool_error, persist_tool_result, truncate_text_head_tail};
use crate::{debug, warn_log};

/// Result returned by AgentRunner after a ReAct loop completes.
#[derive(Debug, Clone, Default)]
pub struct AgentResult {
    pub success: bool,
    pub content: String,
    pub total_tokens: u32,
    pub prompt_tokens: u32,
    pub prompt_cache_hit_tokens: u32,
    pub completion_tokens: u32,
    pub iterations: u32,
}

impl AgentResult {
    fn accumulate(&mut self, usage: &Usage) {
        self.total_tokens += usage.total_tokens;
        self.prompt_tokens += usage.prompt_tokens;
        self.prompt_cache_hit_tokens += usage.prompt_cache_hit_tokens;
        self.completion_tokens += usage.completion_tokens;
    }
}

/// Builder for constructing an `AgentRunner`.
/// Encapsulates the composition of `ContextBuilder` and `AgentRunner`,
/// shielding callers from internal construction details.
pub struct AgentRunnerBuilder {
    session_manager: Option<SharedSessionManager>,
    tool_manager: Option<Arc<ToolManager>>,
    provider: Option<Arc<dyn Provider>>,
    config: Option<AgentConfig>,
    workspace_dir: Option<PathBuf>,
    memory_store: Option<SharedMemoryStore>,
    channel_inject: Option<String>,
    consolidator: Option<Arc<Consolidator>>,
}

impl AgentRunnerBuilder {
    pub fn new() -> Self {
        Self {
            session_manager: None,
            tool_manager: None,
            provider: None,
            config: None,
            workspace_dir: None,
            memory_store: None,
            channel_inject: None,
            consolidator: None,
        }
    }

    pub fn session_manager(mut self, sm: SharedSessionManager) -> Self {
        self.session_manager = Some(sm);
        self
    }

    pub fn tool_manager(mut self, tm: Arc<ToolManager>) -> Self {
        self.tool_manager = Some(tm);
        self
    }

    pub fn provider(mut self, p: Arc<dyn Provider>) -> Self {
        self.provider = Some(p);
        self
    }

    pub fn config(mut self, c: AgentConfig) -> Self {
        self.config = Some(c);
        self
    }

    pub fn workspace_dir(mut self, wd: PathBuf) -> Self {
        self.workspace_dir = Some(wd);
        self
    }

    pub fn memory_store(mut self, ms: SharedMemoryStore) -> Self {
        self.memory_store = Some(ms);
        self
    }

    pub fn channel_inject(mut self, ci: Option<String>) -> Self {
        self.channel_inject = ci;
        self
    }

    pub fn consolidator(mut self, c: Option<Arc<Consolidator>>) -> Self {
        self.consolidator = c;
        self
    }

    pub fn build(self) -> AgentRunner {
        let session_manager = self.session_manager.expect("session_manager required");
        let tool_manager = self.tool_manager.expect("tool_manager required");
        let provider = self.provider.expect("provider required");
        let config = self.config.expect("config required");
        let workspace_dir = self.workspace_dir.expect("workspace_dir required");
        let memory_store = self.memory_store.expect("memory_store required");

        AgentRunner::new(
            tool_manager,
            provider,
            session_manager,
            config,
            workspace_dir,
            memory_store,
            self.channel_inject,
            self.consolidator,
        )
    }}

pub struct AgentRunner {
    tool_manager: Arc<ToolManager>,
    provider: Arc<dyn Provider>,
    session_manager: SharedSessionManager,
    config: AgentConfig,
    workspace_dir: PathBuf,
    memory_store: SharedMemoryStore,
    channel_inject: Option<String>,
    consolidator: Option<Arc<Consolidator>>,
}

impl AgentRunner {
    /// Create a new builder for constructing an `AgentRunner`.
    pub fn builder() -> AgentRunnerBuilder {
        AgentRunnerBuilder::new()
    }

    pub fn new(
        tool_manager: Arc<ToolManager>,
        provider: Arc<dyn Provider>,
        session_manager: SharedSessionManager,
        config: AgentConfig,
        workspace_dir: PathBuf,
        memory_store: SharedMemoryStore,
        channel_inject: Option<String>,
        consolidator: Option<Arc<Consolidator>>,
    ) -> Self {
        Self {
            tool_manager,
            provider,
            session_manager,
            config,
            workspace_dir,
            memory_store,
            channel_inject,
            consolidator,
        }
    }

    pub async fn run(
        &self,
        content: String,
        hook: TaskHook,
        session_id: &str,
    ) -> AgentResult {
        debug!("[AgentRunner] Starting run for session={}", session_id);

        // 1. Write user message
        {
            let mut sm: tokio::sync::MutexGuard<'_, SessionManager> =
                self.session_manager.lock().await;
            let add_result = sm
                .add_message(
                    session_id,
                    Message::user(content),
                )
                .await;
            if let Err(e) = add_result
            {
                return self.fail_result(&hook, session_id, &format!("Failed to write user message: {}", e), 0).await;
            }
        }

        // 2. Notify running state
        let running_state = TaskState::Running {
            current_iteration: 0,
        };
        hook.notify_status_change(&running_state);

        let max_iterations = self.config.max_iterations;
        let mut iterations: u32 = 0;
        let mut result = AgentResult::default();

        loop {
            // Exceeded max iterations
            if iterations >= max_iterations {
                let err_msg = format!("Reached max iterations {}", max_iterations);
                warn_log!("[AgentRunner] {}", err_msg);
                result.success = false;
                result.content = err_msg.clone();
                result.iterations = iterations;
                let failed_state = TaskState::Failed {
                    error: err_msg.clone(),
                };
                {
                    let mut sm: tokio::sync::MutexGuard<'_, SessionManager> =
                        self.session_manager.lock().await;
                    let _ = sm.persist(session_id).await;
                }
                hook.notify_status_change(&failed_state);
                return result;
            }

            // Build context (with session summary from last consolidation)
            let session_summary = {
                let sm = self.session_manager.lock().await;
                sm.get_last_summary(session_id).await
            };
            let session_summary_ref = session_summary.as_deref();
            let context_builder = ContextBuilder::new(
                self.session_manager.clone(),
                self.tool_manager.clone(),
                self.workspace_dir.clone(),
                self.memory_store.clone(),
            );
            let mut ctx = context_builder
                .build(session_id, self.channel_inject.clone(), session_summary_ref)
                .await;
            debug!("[AgentRunner] Context built: {} messages, tools={}", ctx.messages.len(), ctx.tools.as_ref().map(|t| t.len()).unwrap_or(0));

            // History governance: clean orphans and backfill missing tool results
            Self::drop_orphan_tool_results(&mut ctx.messages);
            Self::backfill_missing_tool_results(&mut ctx.messages);

            // Request model
            debug!("[AgentRunner] Sending chat request to provider");
            let response = match self
                .provider
                .chat(&ctx.messages, ctx.tools.as_deref())
                .await
            {
                Ok(r) => r,
                Err(e) => {
                    let err_msg = format!("LLM API request failed - {}", e);
                    warn_log!("[AgentRunner] {}", err_msg);
                    result.success = false;
                    result.content = err_msg.clone();
                    result.iterations = iterations;
                    let failed_state = TaskState::Failed { error: err_msg };
                    {
                        let mut sm: tokio::sync::MutexGuard<'_, SessionManager> =
                            self.session_manager.lock().await;
                        let _ = sm.persist(session_id).await;
                    }
                    hook.notify_status_change(&failed_state);
                    return result;
                }
            };
            debug!(
                "[AgentRunner] Got response: content_len={}, tool_calls={}, prompt_tokens={}, prompt_cache_hit={}, completion_tokens={}, total_tokens={}",
                response.content.as_ref().map(|s| s.len()).unwrap_or(0),
                response.tool_calls.as_ref().map(|c| c.len()).unwrap_or(0),
                response.usage.prompt_tokens,
                response.usage.prompt_cache_hit_tokens,
                response.usage.completion_tokens,
                response.usage.total_tokens,
            );

            // No tool calls → done
            let has_tool_calls = response
                .tool_calls
                .as_ref()
                .is_some_and(|calls| !calls.is_empty());

            if !has_tool_calls {
                let text = response.content.clone().unwrap_or_default();
                let prompt_tokens = response.usage.prompt_tokens;
                result.accumulate(&response.usage);
                result.success = true;
                result.content = text.clone();
                result.iterations = iterations;
                debug!("[AgentRunner] No tool calls, final response (len={})", text.len());
                {
                    let mut sm: tokio::sync::MutexGuard<'_, SessionManager> =
                        self.session_manager.lock().await;
                    if let Err(e) = sm
                        .add_message(
                            session_id,
                            Message::assistant(Some(text.clone()), None),
                        )
                        .await
                    {
                        return self.fail_result(&hook, session_id, &format!("Failed to write assistant message: {}", e), iterations).await;
                    }

                    // Update token ratio so persist() saves it with the meta.
                    sm.update_token_ratio(session_id, prompt_tokens);

                    let _completed_state = TaskState::Completed {
                        result: text.clone(),
                    };
                    let _ = sm.persist(session_id).await;
                }
                hook.notify_status_change(&TaskState::Completed {
                    result: text.clone(),
                });
                // Trigger consolidation after successful turn.
                if let Some(ref consolidator) = self.consolidator {
                    let _ = consolidator.maybe_consolidate(session_id, prompt_tokens).await;
                }
                debug!("[AgentRunner] Run complete: success={}, iterations={}", result.success, result.iterations);
                return result;
            }

            // Has tool calls → write ONE assistant message with content + all tool_calls,
            // then execute each tool and append its Tool message.
            if let Some(calls) = &response.tool_calls {
                result.accumulate(&response.usage);
                debug!("[AgentRunner] Executing {} tool calls", calls.len());

                // 1. Persist the assistant reply (preserves model's content text)
                {
                    let mut sm: tokio::sync::MutexGuard<'_, SessionManager> =
                        self.session_manager.lock().await;
                    if let Err(e) = sm
                        .add_message(
                            session_id,
                            Message::assistant(response.content.clone(), Some(calls.clone())),
                        )
                        .await
                    {
                        return self.fail_result(&hook, session_id, &format!("Failed to write assistant message: {}", e), iterations).await;
                    }
                }

                // 2. Execute tools and write Tool messages
                for call in calls {
                    let raw_result = match self
                        .tool_manager
                        .execute(&call.name, call.args.clone())
                        .await
                    {
                        Ok(r) => r,
                        Err(e) => format_tool_error(&e.to_string()),
                    };

                    let ensured = ensure_nonempty_tool_result(&call.name, &raw_result);

                    let processed = if self.config.persist_tool_results {
                        persist_tool_result(
                            &self.workspace_dir,
                            &call.id,
                            &ensured,
                            self.config.max_tool_result_chars as usize,
                        )
                    } else {
                        ensured
                    };

                    let final_content =
                        truncate_text_head_tail(&processed, self.config.max_tool_result_chars as usize);

                    {
                        let mut sm: tokio::sync::MutexGuard<'_, SessionManager> =
                            self.session_manager.lock().await;
                        if let Err(e) = sm
                            .add_message(
                                session_id,
                                Message::tool(final_content, call.id.clone(), Some(call.name.clone())),
                            )
                            .await
                        {
                            return self.fail_result(&hook, session_id, &format!("Failed to write tool message: {}", e), iterations).await;
                        }
                    }
                }

                iterations += 1;
                let running_state = TaskState::Running {
                    current_iteration: iterations,
                };
                hook.notify_status_change(&running_state);
            }
        }
    }

    async fn fail_result(
        &self,
        hook: &TaskHook,
        session_id: &str,
        error: &str,
        iterations: u32,
    ) -> AgentResult {
        let mut result = AgentResult::default();
        result.success = false;
        result.content = error.to_string();
        result.iterations = iterations;
        let failed_state = TaskState::Failed { error: error.to_string() };
        {
            let mut sm: tokio::sync::MutexGuard<'_, SessionManager> =
                self.session_manager.lock().await;
            let _ = sm.persist(session_id).await;
        }
        hook.notify_status_change(&failed_state);
        result
    }
}

impl AgentRunner {
    /// Drop tool result messages that have no matching assistant tool_call.
    fn drop_orphan_tool_results(messages: &mut Vec<Message>) {
        let mut declared: std::collections::HashSet<String> = std::collections::HashSet::new();
        let mut orphan_indices: Vec<usize> = Vec::new();
        for (i, msg) in messages.iter().enumerate() {
            match msg {
                Message::Assistant { tool_calls, .. } => {
                    if let Some(calls) = tool_calls {
                        for call in calls {
                            declared.insert(call.id.clone());
                        }
                    }
                }
                Message::Tool { tool_call_id, .. } => {
                    if !declared.contains(tool_call_id) {
                        orphan_indices.push(i);
                    }
                }
                _ => {}
            }
        }
        // Remove orphans in reverse order to preserve indices
        for i in orphan_indices.into_iter().rev() {
            messages.remove(i);
        }
    }

    /// Insert synthetic error Tool messages for assistant tool_calls that lack a result.
    fn backfill_missing_tool_results(messages: &mut Vec<Message>) {
        let backfill_content = "[Tool result unavailable — call was interrupted or lost]";

        // Collect all declared tool calls and fulfilled tool_call_ids
        let mut declared: Vec<(usize, String, String)> = Vec::new(); // (assistant_idx, call_id, tool_name)
        let mut fulfilled: std::collections::HashSet<String> = std::collections::HashSet::new();

        for (i, msg) in messages.iter().enumerate() {
            match msg {
                Message::Assistant { tool_calls, .. } => {
                    if let Some(calls) = tool_calls {
                        for call in calls {
                            declared.push((i, call.id.clone(), call.name.clone()));
                        }
                    }
                }
                Message::Tool { tool_call_id, .. } => {
                    fulfilled.insert(tool_call_id.clone());
                }
                _ => {}
            }
        }

        let missing: Vec<_> = declared
            .into_iter()
            .filter(|(_, id, _)| !fulfilled.contains(id))
            .collect();

        if missing.is_empty() {
            return;
        }

        // Group by assistant index, then insert each group in reverse to avoid index shifting.
        let mut by_assistant: std::collections::BTreeMap<usize, Vec<(String, String)>> =
            std::collections::BTreeMap::new();
        for (asst_idx, call_id, tool_name) in missing {
            by_assistant.entry(asst_idx).or_default().push((call_id, tool_name));
        }

        for (assistant_idx, calls) in by_assistant.into_iter().rev() {
            // Insert each call in reverse so they end up in original order
            for (call_id, tool_name) in calls.into_iter().rev() {
                messages.insert(assistant_idx + 1, Message::tool(
                    backfill_content.to_string(),
                    call_id,
                    Some(tool_name),
                ));
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
    use crate::provider::{LLMResponse, Provider, Usage, FinishReason};
    use crate::session::{Message, SessionTask, TaskHook, TaskState};
    use crate::tool::{Tool, ToolCall, ToolDefinition};
    use crate::memory::MemoryStore;

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
            _messages: &[Message],
            _tools: Option<&[ToolDefinition]>,
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
    ) -> (SharedSessionManager, Arc<ToolManager>, PathBuf, Arc<tokio::sync::Mutex<MemoryStore>>) {
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

        let ms = Arc::new(tokio::sync::Mutex::new(MemoryStore::new(&workspace_dir)));
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
        ms: Arc<tokio::sync::Mutex<MemoryStore>>,
    ) -> AgentRunner {
        AgentRunner::builder()
            .session_manager(sm)
            .tool_manager(tm)
            .provider(provider)
            .config(AgentConfig {
                provider: "test".to_string(),
                max_iterations: 40,
                timeout_seconds: 120,
                max_tool_result_chars: 8000,
                persist_tool_results: false,
                context_window_tokens: 8192,
            })
            .workspace_dir(workspace_dir)
            .memory_store(ms)
            .build()
    }

    #[tokio::test]
    async fn test_direct_reply_no_tool_calls() {
        let tmp = tempfile::TempDir::new().unwrap();
        let (sm, tm, wd, ms) = make_test_env(&tmp, &[]).await;
        let provider = Arc::new(MockProvider::new(vec![LLMResponse {
            content: Some("Hello, world!".to_string()),
            tool_calls: None,
            finish_reason: FinishReason::Stop,
            usage: Usage { prompt_tokens: 10, prompt_cache_hit_tokens: 0, completion_tokens: 5, total_tokens: 15 },
        }]));
        let runner = make_runner(sm.clone(), tm, provider.clone(), wd, ms);
        let task = make_task("Say hi");

        let result = runner.run(task.content.clone(), task.hook.clone(), "test-session").await;
        assert_eq!(result.content, "Hello, world!");
        assert_eq!(result.success, true);
        assert_eq!(result.total_tokens, 15);
        assert_eq!(provider.call_count(), 1);

        let msgs = sm.lock().await.get_messages("test-session").await;
        assert_eq!(msgs.len(), 2); // user + assistant (system prompt is not stored in session)
        assert!(
            matches!(&msgs[0], Message::User { content, .. } if content == "Say hi")
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
                usage: Usage { prompt_tokens: 20, prompt_cache_hit_tokens: 0, completion_tokens: 10, total_tokens: 30 },
            },
            LLMResponse {
                content: Some("Done!".to_string()),
                tool_calls: None,
                finish_reason: FinishReason::Stop,
                usage: Usage { prompt_tokens: 40, prompt_cache_hit_tokens: 0, completion_tokens: 5, total_tokens: 45 },
            },
        ]));
        let runner = make_runner(sm.clone(), tm, provider.clone(), wd, ms);
        let task = make_task("Run echo");

        let result = runner.run(task.content.clone(), task.hook.clone(), "test-session").await;
        assert_eq!(result.content, "Done!");
        assert_eq!(result.success, true);
        assert_eq!(result.total_tokens, 75); // 30 + 45
        assert_eq!(result.prompt_cache_hit_tokens, 0);
        assert_eq!(provider.call_count(), 2);

        let msgs = sm.lock().await.get_messages("test-session").await;
        // user + assistant(content+tool_calls) + tool + assistant(final)
        assert_eq!(msgs.len(), 4);
        match &msgs[1] {
            Message::Assistant { content, tool_calls, .. } => {
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
            Message::Tool { content, tool_call_id, .. } => {
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
            &[
                ("list", "listing-contents"),
                ("read", "file-contents"),
            ],
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
                usage: Usage { prompt_tokens: 50, prompt_cache_hit_tokens: 0, completion_tokens: 20, total_tokens: 70 },
            },
            LLMResponse {
                content: Some(
                    "Here are the results: listing-contents and file-contents.".to_string(),
                ),
                tool_calls: None,
                finish_reason: FinishReason::Stop,
                usage: Usage { prompt_tokens: 80, prompt_cache_hit_tokens: 0, completion_tokens: 15, total_tokens: 95 },
            },
        ]));
        let runner = make_runner(sm.clone(), tm, provider.clone(), wd, ms);
        let task = make_task("List and read");

        let result = runner.run(task.content.clone(), task.hook.clone(), "test-session").await;
        assert_eq!(
            result.content,
            "Here are the results: listing-contents and file-contents."
        );
        assert_eq!(result.success, true);
        assert_eq!(result.total_tokens, 165); // 70 + 95
        assert_eq!(provider.call_count(), 2);

        let msgs = sm.lock().await.get_messages("test-session").await;
        // user + assistant(content+2tool_calls) + tool + tool + assistant(final)
        assert_eq!(msgs.len(), 5);

        // Verify assistant message has both content and both tool_calls
        match &msgs[1] {
            Message::Assistant { content, tool_calls, .. } => {
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
                tool_calls: Some(vec![tool_call("tc-a", "tool_a"), tool_call("tc-b", "tool_b")]),
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

        let result = runner.run(task.content.clone(), task.hook.clone(), "test-session").await;
        assert_eq!(result.content, "Final answer");
        assert_eq!(result.success, true);

        let msgs = sm.lock().await.get_messages("test-session").await;
        match &msgs[1] {
            Message::Assistant { content, tool_calls, .. } => {
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
            std::iter::repeat(base_response).take(50).collect(),
        ));
        let runner = make_runner(sm.clone(), tm, provider.clone(), wd, ms);
        let task = make_task("Loop forever");

        let result = runner.run(task.content.clone(), task.hook.clone(), "test-session").await;
        assert_eq!(result.success, false);
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
        let (status_tx, mut status_rx) =
            tokio::sync::mpsc::channel::<(String, TaskState)>(32);

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

        let _ = runner.run(content, hook, "test-session").await;

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
    async fn test_tool_error_continues_loop() {
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

        let ms = Arc::new(tokio::sync::Mutex::new(MemoryStore::new(&workspace_dir)));
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

        let result = runner.run(task.content.clone(), task.hook.clone(), "test-session").await;
        assert_eq!(result.content, "Got the error, stopping.");

        let msgs = sm.lock().await.get_messages("test-session").await;
        // user + assistant(tool_calls) + tool(error) + assistant(final)
        assert_eq!(msgs.len(), 4);
        assert!(
            matches!(&msgs[2], Message::Tool { content, .. } if content.contains("Error:"))
        );
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

        let result = runner.run(task.content.clone(), task.hook.clone(), "test-session").await;
        assert_eq!(result.content, "persist me");

        // Create a new SessionManager and reload from disk
        let session_dir = tmp.path().join("sessions");
        let sm2: SharedSessionManager =
            Arc::new(Mutex::new(SessionManager::new(session_dir).unwrap()));
        sm2.lock().await.get_or_create("test-session").await.unwrap();

        let msgs = sm2.lock().await.get_messages("test-session").await;
        assert_eq!(msgs.len(), 2); // user + assistant
        assert!(
            matches!(&msgs[0], Message::User { content, .. } if content == "hello")
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
        assert_eq!(
            ensure_nonempty_tool_result("echo", "hello"),
            "hello"
        );
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
        let result = persist_tool_result(tmp.path(), "call-123", &long_content, 8000);
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
            Message::assistant(Some("calling".to_string()), Some(vec![tool_call("tc-1", "echo")])),
            Message::tool("orphan".to_string(), "tc-999".to_string(), Some("echo".to_string())),
            Message::tool("valid".to_string(), "tc-1".to_string(), Some("echo".to_string())),
        ];

        AgentRunner::drop_orphan_tool_results(&mut messages);

        assert_eq!(messages.len(), 3);
        assert!(matches!(&messages[2], Message::Tool { content, .. } if content == "valid"));
    }

    #[test]
    fn test_backfill_missing_tool_results() {
        let mut messages = vec![
            Message::user("test".to_string()),
            Message::assistant(Some("calling".to_string()), Some(vec![tool_call("tc-1", "echo"), tool_call("tc-2", "read")])),
            Message::tool("result-1".to_string(), "tc-1".to_string(), Some("echo".to_string())),
            // tc-2 has no result
        ];

        AgentRunner::backfill_missing_tool_results(&mut messages);

        // tc-2 is backfilled right after the assistant, before tc-1's result
        assert_eq!(messages.len(), 4);
        assert!(
            matches!(&messages[2], Message::Tool { content, tool_call_id, .. }
                if tool_call_id == "tc-2" && content.contains("interrupted or lost"))
        );
        assert!(
            matches!(&messages[3], Message::Tool { tool_call_id, .. } if tool_call_id == "tc-1")
        );
    }

    #[test]
    fn test_backfill_missing_multiple_assistants() {
        // Two assistants, each with missing tool calls
        let mut messages = vec![
            Message::user("first".to_string()),
            Message::assistant(Some("a1".to_string()), Some(vec![tool_call("tc-1", "echo"), tool_call("tc-2", "read")])),
            Message::tool("r1".to_string(), "tc-1".to_string(), Some("echo".to_string())),
            Message::user("second".to_string()),
            Message::assistant(Some("a2".to_string()), Some(vec![tool_call("tc-3", "write"), tool_call("tc-4", "list")])),
            Message::tool("r4".to_string(), "tc-4".to_string(), Some("list".to_string())),
            // tc-2 and tc-3 are missing
        ];

        AgentRunner::backfill_missing_tool_results(&mut messages);

        assert_eq!(messages.len(), 8);
        // After A1 (index 1): tc-2 should be at index 2
        assert!(
            matches!(&messages[2], Message::Tool { tool_call_id, .. } if tool_call_id == "tc-2")
        );
        // After A2 (originally index 4, now 5): tc-3 should be at index 6
        assert!(
            matches!(&messages[6], Message::Tool { tool_call_id, .. } if tool_call_id == "tc-3")
        );
    }

    #[tokio::test]
    async fn test_runner_persist_long_output() {
        let tmp = tempfile::TempDir::new().unwrap();
        let path = tmp.path();
        let session_dir = path.join("sessions");
        let workspace_dir = path.join("workspace");
        std::fs::create_dir_all(&workspace_dir).unwrap();

        let long_result = "X".repeat(12_000);

        let sm: SharedSessionManager =
            Arc::new(Mutex::new(SessionManager::new(session_dir).unwrap()));
        let mut tm = ToolManager::new(workspace_dir.clone());
        tm.register(Box::new(MockEchoTool::new("big", &long_result)));
        let tm = Arc::new(tm);
        sm.lock().await.get_or_create("test-session").await.unwrap();

        let ms = Arc::new(tokio::sync::Mutex::new(MemoryStore::new(&workspace_dir)));
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
        let runner = AgentRunner::builder()
            .session_manager(sm.clone())
            .tool_manager(tm)
            .provider(provider)
            .config(AgentConfig {
                provider: "test".to_string(),
                max_iterations: 40,
                timeout_seconds: 120,
                max_tool_result_chars: 8000,
                persist_tool_results: true,
                context_window_tokens: 8192,
            })
            .workspace_dir(workspace_dir.clone())
            .memory_store(ms)
            .build();
        let task = make_task("test persist");

        let result = runner.run(task.content.clone(), task.hook.clone(), "test-session").await;
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
}
