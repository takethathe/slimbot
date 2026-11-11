use anyhow::Result;
use async_trait::async_trait;

use crate::runner::traits::AfterTurnHook;
use crate::runner::types::{RunSpec, RunState};
use crate::session::{AgentEvent, TaskState};

/// Fires the `TaskCompleted` or `TaskFailed` event based on the run result.
///
/// Also persists the session (unless the run is rolling back to a previous
/// message count, e.g. on cancellation or LLM error) and notifies the final
/// task state. This hook runs on every exit path.
pub struct FireTaskCompletedHook;

#[async_trait]
impl AfterTurnHook for FireTaskCompletedHook {
    async fn call(&self, spec: &RunSpec<'_>, state: &mut RunState) -> Result<()> {
        let mut sm = spec.session_manager().lock().await;
        if let Some(limit) = state.rollback_to {
            // Roll back messages added during this turn; do not persist.
            sm.truncate_messages(&spec.session_id, limit).await;
        } else {
            let _ = sm.persist(&spec.session_id).await;
        }

        let hook = &spec.task_hook;
        if state.result.success {
            let text = state.result.content.clone();
            hook.notify_status_change(&TaskState::Completed {
                result: text.clone(),
            });
            hook.fire_event(AgentEvent::TaskCompleted {
                session_id: spec.session_id.clone(),
                run_id: hook.run_id().to_string(),
                result: text,
            });
        } else {
            let error = state.result.content.clone();
            hook.notify_status_change(&TaskState::Failed {
                error: error.clone(),
            });
            hook.fire_event(AgentEvent::TaskFailed {
                session_id: spec.session_id.clone(),
                run_id: hook.run_id().to_string(),
                error,
            });
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {

    use std::sync::Arc;

    use tokio::sync::{broadcast, mpsc};

    use super::*;
    use crate::runner::test_support::RunFixture;

    use crate::session::{Message, SharedSessionManager, TaskHook};

    #[tokio::test]
    async fn test_fire_task_completed_on_success() {
        let (status_tx, mut status_rx) = mpsc::channel::<(String, TaskState)>(8);
        let (event_tx, mut event_rx) = broadcast::channel::<AgentEvent>(8);
        let fixture = RunFixture::new();
        let mut spec = fixture.spec();
        spec.task_hook = TaskHook::new("s1")
            .with_status_channel(status_tx)
            .with_events(event_tx);
        let sm: SharedSessionManager = spec.session_manager().clone();
        sm.lock().await.get_or_create("s1").await.unwrap();

        let mut state = crate::runner::types::RunState::new(Arc::new(
            crate::tool::TurnContext::new("cli".into(), "c1".into()),
        ));
        state.result.success = true;
        state.result.content = "done".into();

        FireTaskCompletedHook.call(&spec, &mut state).await.unwrap();

        let (_, task_state) = status_rx.try_recv().unwrap();
        assert!(matches!(task_state, TaskState::Completed { .. }));
        let event = event_rx.try_recv().unwrap();
        assert!(matches!(event, AgentEvent::TaskCompleted { .. }));
    }

    #[tokio::test]
    async fn test_fire_task_completed_persists_session() {
        let fixture = RunFixture::new();
        let spec = fixture.spec();
        let sm: SharedSessionManager = spec.session_manager().clone();
        sm.lock().await.get_or_create("s1").await.unwrap();
        sm.lock()
            .await
            .add_message("s1", Message::user("hi".into()))
            .await
            .unwrap();

        let mut state = crate::runner::types::RunState::new(Arc::new(
            crate::tool::TurnContext::new("cli".into(), "c1".into()),
        ));
        state.result.success = true;

        FireTaskCompletedHook.call(&spec, &mut state).await.unwrap();

        // Session manager persisted the session to its configured session dir.
        let jsonl = spec.session_manager().lock().await.get_messages("s1").await;
        assert_eq!(jsonl.len(), 1);
    }

    #[tokio::test]
    async fn test_fire_task_completed_rolls_back_when_requested() {
        let fixture = RunFixture::new();
        let spec = fixture.spec();
        let sm: SharedSessionManager = spec.session_manager().clone();
        sm.lock().await.get_or_create("s1").await.unwrap();
        // Two pre-existing messages; initial_message_count = 2.
        sm.lock()
            .await
            .add_message("s1", Message::user("prev1".into()))
            .await
            .unwrap();
        sm.lock()
            .await
            .add_message("s1", Message::user("prev2".into()))
            .await
            .unwrap();

        let mut state = crate::runner::types::RunState::new(Arc::new(
            crate::tool::TurnContext::new("cli".into(), "c1".into()),
        ));
        state.result.success = false;
        state.result.content = "cancelled".into();
        state.rollback_to = Some(2);

        FireTaskCompletedHook.call(&spec, &mut state).await.unwrap();

        let msgs = sm.lock().await.get_messages("s1").await;
        assert_eq!(msgs.len(), 2, "should roll back to initial message count");
    }
}
