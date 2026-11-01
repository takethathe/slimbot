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
        spec: &RunSpec,
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
    use std::path::PathBuf;
    use std::sync::Arc;

    use tokio::sync::{Mutex, broadcast};

    use super::*;
    use crate::memory::MemoryService;
    use crate::runner::types::RunSpec;
    use crate::session::{SessionManager, TaskHook};
    use crate::tool::ToolManager;

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
            tracer: None,
        }
    }

    #[tokio::test]
    async fn test_fire_tool_result_event() {
        let (event_tx, mut event_rx) = broadcast::channel::<AgentEvent>(8);
        let mut spec = make_spec();
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
