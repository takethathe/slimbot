use anyhow::Result;
use async_trait::async_trait;

use crate::runner::traits::PostIterationHook;
use crate::runner::types::{HookAction, RunSpec, RunState};
use crate::session::AgentEvent;

/// Fires the `PostIteration` event after all tool calls in an iteration complete.
pub struct FirePostIterationEventHook;

#[async_trait]
impl PostIterationHook for FirePostIterationEventHook {
    async fn call(&self, spec: &RunSpec<'_>, state: &mut RunState) -> Result<HookAction> {
        spec.task_hook.fire_event(AgentEvent::PostIteration {
            session_id: spec.session_id.clone(),
            iteration: state.iterations,
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
    async fn test_fire_post_iteration_event() {
        let (event_tx, mut event_rx) = broadcast::channel::<AgentEvent>(8);
        let fixture = RunFixture::new();
        let mut spec = fixture.spec();
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
