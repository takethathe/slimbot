use anyhow::Result;
use async_trait::async_trait;

use crate::runner::traits::PostIterationHook;
use crate::runner::types::{HookAction, RunSpec, RunState};
use crate::session::AgentEvent;

/// Fires the `PostIteration` event after all tool calls in an iteration complete.
pub struct FirePostIterationEventHook;

#[async_trait]
impl PostIterationHook for FirePostIterationEventHook {
    async fn call(&self, spec: &RunSpec, state: &mut RunState) -> Result<HookAction> {
        spec.task_hook.fire_event(AgentEvent::PostIteration {
            session_id: spec.session_id.clone(),
            iteration: state.iterations,
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
            tracer: None,
        }
    }

    #[tokio::test]
    async fn test_fire_post_iteration_event() {
        let (event_tx, mut event_rx) = broadcast::channel::<AgentEvent>(8);
        let mut spec = make_spec();
        spec.task_hook = TaskHook::new("s1").with_events(event_tx);

        let mut state = crate::runner::types::RunState::new(Arc::new(
            crate::tool::TurnContext::new("cli".into(), "c1".into()),
        ));
        state.iterations = 2;
        FirePostIterationEventHook
            .call(&spec, &mut state)
            .await
            .unwrap();

        let event = event_rx.try_recv().unwrap();
        match event {
            AgentEvent::PostIteration { iteration, .. } => {
                assert_eq!(iteration, 2);
            }
            other => panic!("expected PostIteration event, got {:?}", other),
        }
    }
}
