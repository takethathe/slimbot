use anyhow::Result;
use async_trait::async_trait;

use crate::runner::traits::PreTurnHook;
use crate::runner::types::{HookAction, RunSpec, RunState};
use crate::session::{AgentEvent, TaskState};

/// Notifies the running state and fires the `TaskStarted` event.
pub struct NotifyRunningHook;

#[async_trait]
impl PreTurnHook for NotifyRunningHook {
    async fn call(&self, spec: &RunSpec<'_>, _state: &mut RunState) -> Result<HookAction> {
        let hook = &spec.task_hook;
        hook.notify_status_change(&TaskState::Running {
            current_iteration: 0,
        });
        hook.fire_event(AgentEvent::TaskStarted {
            session_id: spec.session_id.clone(),
            run_id: hook.run_id().to_string(),
        });
        Ok(HookAction::Continue)
    }
}

#[cfg(test)]
mod tests {

    use std::sync::Arc;

    use tokio::sync::{broadcast, mpsc};

    use super::*;
    use crate::runner::test_support::RunFixture;

    use crate::session::TaskHook;

    #[tokio::test]
    async fn test_notify_running_fires_task_started_and_running_state() {
        let (status_tx, mut status_rx) = mpsc::channel::<(String, TaskState)>(8);
        let (event_tx, mut event_rx) = broadcast::channel::<AgentEvent>(8);
        let hook = TaskHook::new("s1")
            .with_status_channel(status_tx)
            .with_events(event_tx);
        let fixture = RunFixture::new();
        let spec = fixture.spec_with(hook);

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
