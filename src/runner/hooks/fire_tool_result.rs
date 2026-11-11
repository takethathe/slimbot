use anyhow::Result;
use async_trait::async_trait;

use crate::runner::traits::AfterToolHook;
use crate::runner::types::{HookAction, RunSpec, RunState};
use crate::session::AgentEvent;
use crate::tool::ToolCall;

/// Fires the `ToolResult` event after the tool executes.
pub struct FireToolResultEventHook;

#[async_trait]
impl AfterToolHook for FireToolResultEventHook {
    async fn call(
        &self,
        spec: &RunSpec<'_>,
        _state: &mut RunState,
        tool_call: &ToolCall,
        result: &str,
    ) -> Result<HookAction> {
        spec.task_hook.fire_event(AgentEvent::ToolResult {
            session_id: spec.session_id.clone(),
            name: tool_call.name.clone(),
            output: result.to_string(),
        });
        Ok(HookAction::Continue)
    }
}

#[cfg(test)]
mod tests {

    use std::sync::Arc;

    use tokio::sync::broadcast;

    use super::*;
    use crate::runner::test_support::RunFixture;

    use crate::session::TaskHook;

    #[tokio::test]
    async fn test_fire_tool_result_event() {
        let (event_tx, mut event_rx) = broadcast::channel::<AgentEvent>(8);
        let fixture = RunFixture::new();
        let mut spec = fixture.spec();
        spec.task_hook = TaskHook::new("s1").with_events(event_tx);

        let mut state = crate::runner::types::RunState::new(Arc::new(
            crate::tool::TurnContext::new("cli".into(), "c1".into()),
        ));
        let call = ToolCall {
            id: "tc-1".into(),
            name: "echo".into(),
            args: serde_json::json!({}),
        };
        let action = FireToolResultEventHook
            .call(&spec, &mut state, &call, "output")
            .await
            .unwrap();
        assert_eq!(action, HookAction::Continue);

        let event = event_rx.try_recv().unwrap();
        match event {
            AgentEvent::ToolResult { name, output, .. } => {
                assert_eq!(name, "echo");
                assert_eq!(output, "output");
            }
            other => panic!("expected ToolResult event, got {:?}", other),
        }
    }
}
