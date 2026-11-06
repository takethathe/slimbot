use anyhow::{Result, anyhow};
use async_trait::async_trait;

use crate::runner::hooks::has_tool_calls;
use crate::runner::traits::AfterChatHook;
use crate::runner::types::{HookAction, RunSpec, RunState};
use crate::session::AgentEvent;

/// Inspects the LLM response: fires the `AssistantMessage` event when the
/// response has content, and marks the run as complete (no more iterations)
/// when the response contains no tool calls.
pub struct HandleResponseHook;

#[async_trait]
impl AfterChatHook for HandleResponseHook {
    async fn call(&self, spec: &RunSpec, state: &mut RunState) -> Result<HookAction> {
        let response = state
            .response
            .as_ref()
            .ok_or_else(|| anyhow!("HandleResponseHook: no response in state"))?;

        // Emit assistant message content if present.
        if let Some(ref content) = response.content
            && !content.is_empty()
        {
            spec.task_hook.fire_event(AgentEvent::AssistantMessage {
                session_id: spec.session_id.clone(),
                content: content.clone(),
            });
        }

        // No tool calls -> done.
        if !has_tool_calls(response) {
            let text = response.content.clone().unwrap_or_default();
            state.should_exit = true;
            state.result.success = true;
            state.result.content = text;
            state.result.iterations = state.iterations;
        }
        Ok(HookAction::Continue)
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::sync::Arc;

    use tokio::sync::{Mutex, broadcast};

    use super::*;
    use crate::memory::MemoryService;
    use crate::provider::{FinishReason, LLMResponse, Usage};
    use crate::runner::types::RunSpec;
    use crate::session::{SessionManager, TaskHook};
    use crate::tool::{ToolCall, ToolManager};

    fn make_spec() -> RunSpec {
        let tmp = tempfile::TempDir::new().unwrap();
        let session_dir = tmp.path().join("sessions");
        let workspace_dir = tmp.path().join("workspace");
        std::fs::create_dir_all(&workspace_dir).unwrap();
        RunSpec {
            session_id: "s1".into(),
            content: "hello".into(),
            task_hook: TaskHook::new("s1"),
            max_iterations: 10,
            max_tokens: 100,
            cancel_token: None,
            workspace_dir,
            session_manager: Arc::new(Mutex::new(SessionManager::new(session_dir).unwrap())),
            tool_manager: Arc::new(ToolManager::new(PathBuf::from("/tmp"))),
            config: crate::config::AgentConfig::default(),
            pressure: Default::default(),
            memory_service: Arc::new(Mutex::new(MemoryService::new(std::path::Path::new("/tmp")))),
            consolidator: None,
            initial_message_count: 0,
            channel: "cli".into(),
            chat_id: "c1".into(),
            runtime_ctx: "ctx".into(),
            approvals: Default::default(),
            connectors: None,
            tracer: None,
        }
    }

    #[tokio::test]
    async fn test_handle_response_no_tool_calls_sets_should_exit() {
        let (event_tx, _) = broadcast::channel::<AgentEvent>(8);
        let mut spec = make_spec();
        spec.task_hook = TaskHook::new("s1").with_events(event_tx);
        let mut state = crate::runner::types::RunState::new(Arc::new(
            crate::tool::TurnContext::new("cli".into(), "c1".into()),
        ));
        state.response = Some(LLMResponse {
            content: Some("final answer".into()),
            tool_calls: None,
            finish_reason: FinishReason::Stop,
            usage: Usage::default(),
        });

        HandleResponseHook.call(&spec, &mut state).await.unwrap();
        assert!(state.should_exit);
        assert!(state.result.success);
        assert_eq!(state.result.content, "final answer");
    }

    #[tokio::test]
    async fn test_handle_response_with_tool_calls_does_not_exit() {
        let spec = make_spec();
        let mut state = crate::runner::types::RunState::new(Arc::new(
            crate::tool::TurnContext::new("cli".into(), "c1".into()),
        ));
        state.response = Some(LLMResponse {
            content: Some("calling".into()),
            tool_calls: Some(vec![ToolCall {
                id: "tc-1".into(),
                name: "echo".into(),
                args: serde_json::json!({}),
            }]),
            finish_reason: FinishReason::ToolCalls,
            usage: Usage::default(),
        });

        HandleResponseHook.call(&spec, &mut state).await.unwrap();
        assert!(!state.should_exit);
        assert!(!state.result.success);
    }
}
