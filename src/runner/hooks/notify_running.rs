use anyhow::Result;
use async_trait::async_trait;

use crate::runner::traits::PreTurnHook;
use crate::runner::types::{HookAction, RunSpec, RunState};
use crate::session::{AgentEvent, TaskState};

/// Notifies the running state and fires the `TaskStarted` event.
pub struct NotifyRunningHook;

#[async_trait]
impl PreTurnHook for NotifyRunningHook {
    async fn call(&self, spec: &RunSpec, _state: &mut RunState) -> Result<HookAction> {
        let hook = &spec.task_hook;
        hook.notify_status_change(&TaskState::Running {
            current_iteration: 0,
        });
        hook.fire_event(AgentEvent::TaskStarted {
            session_id: spec.session_id.clone(),
        });
        Ok(HookAction::Continue)
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::sync::Arc;

    use tokio::sync::{Mutex, broadcast, mpsc};

    use super::*;
    use crate::memory::MemoryStore;
    use crate::runner::types::RunSpec;
    use crate::session::{SessionManager, TaskHook};
    use crate::tool::ToolManager;

    fn make_spec(hook: TaskHook) -> RunSpec {
        let tmp = tempfile::TempDir::new().unwrap();
        let session_dir = tmp.path().join("sessions");
        let workspace_dir = tmp.path().join("workspace");
        std::fs::create_dir_all(&workspace_dir).unwrap();
        RunSpec {
            session_id: "s1".into(),
            content: "hello".into(),
            task_hook: hook,
            max_iterations: 10,
            max_tokens: 100,
            cancel_token: None,
            workspace_dir,
            session_manager: Arc::new(Mutex::new(SessionManager::new(session_dir).unwrap())),
            tool_manager: Arc::new(ToolManager::new(PathBuf::from("/tmp"))),
            config: crate::config::AgentConfig::default(),
            memory_store: Arc::new(Mutex::new(MemoryStore::new(std::path::Path::new("/tmp")))),
            consolidator: None,
            initial_message_count: 0,
            channel: "cli".into(),
            chat_id: "c1".into(),
            runtime_ctx: "ctx".into(),
        }
    }

    #[tokio::test]
    async fn test_notify_running_fires_task_started_and_running_state() {
        let (status_tx, mut status_rx) = mpsc::channel::<(String, TaskState)>(8);
        let (event_tx, mut event_rx) = broadcast::channel::<AgentEvent>(8);
        let hook = TaskHook::new("s1")
            .with_status_channel(status_tx)
            .with_events(event_tx);
        let spec = make_spec(hook);

        let mut state = crate::runner::types::RunState::new(Arc::new(
            crate::tool::TurnContext::new("cli".into(), "c1".into()),
        ));
        NotifyRunningHook.call(&spec, &mut state).await.unwrap();

        let (sid, task_state) = status_rx.try_recv().unwrap();
        assert_eq!(sid, "s1");
        assert!(matches!(
            task_state,
            TaskState::Running {
                current_iteration: 0
            }
        ));

        let event = event_rx.try_recv().unwrap();
        assert!(matches!(event, AgentEvent::TaskStarted { .. }));
    }
}
