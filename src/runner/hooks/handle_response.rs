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
    async fn call(&self, spec: &RunSpec<'_>, state: &mut RunState) -> Result<HookAction> {
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

    use std::sync::Arc;

    use tokio::sync::broadcast;

    use super::*;
    use crate::runner::test_support::RunFixture;

    use crate::provider::{FinishReason, LLMResponse, Usage};

    use crate::session::TaskHook;
    use crate::tool::ToolCall;

    #[tokio::test]
    async fn test_handle_response_no_tool_calls_sets_should_exit() {
        let (event_tx, _) = broadcast::channel::<AgentEvent>(8);
        let fixture = RunFixture::new();
        let mut spec = fixture.spec();
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
        let fixture = RunFixture::new();
        let spec = fixture.spec();
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
