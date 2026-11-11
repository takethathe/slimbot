use anyhow::Result;
use async_trait::async_trait;

use crate::runner::traits::PreToolHook;
use crate::runner::types::{RunSpec, RunState, ToolHookAction};
use crate::session::AgentEvent;
use crate::tool::ToolCall;

/// Fires the `ToolCall` event before the tool executes.
pub struct FireToolCallEventHook;

#[async_trait]
impl PreToolHook for FireToolCallEventHook {
    async fn call(
        &self,
        spec: &RunSpec<'_>,
        _state: &mut RunState,
        tool_call: &ToolCall,
    ) -> Result<ToolHookAction> {
        spec.task_hook.fire_event(AgentEvent::ToolCall {
            session_id: spec.session_id.clone(),
            name: tool_call.name.clone(),
            args: tool_call.args.to_string(),
        });
        Ok(ToolHookAction::Execute)
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
    async fn test_fire_tool_call_event_and_execute_action() {
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
            args: serde_json::json!({ "x": 1 }),
        };
        let action = FireToolCallEventHook
            .call(&spec, &mut state, &call)
            .await
            .unwrap();
        assert_eq!(action, ToolHookAction::Execute);

        let event = event_rx.try_recv().unwrap();
        match event {
            AgentEvent::ToolCall { name, args, .. } => {
                assert_eq!(name, "echo");
                assert!(args.contains("x"));
            }
            other => panic!("expected ToolCall event, got {:?}", other),
        }
    }
}
