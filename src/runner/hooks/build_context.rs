use anyhow::Result;
use async_trait::async_trait;

use crate::context::ContextBuilder;
use crate::runner::traits::PreChatHook;
use crate::runner::types::{HookAction, RunSpec, RunState};
use crate::session::AgentEvent;

/// Builds the message list for the LLM via `ContextBuilder::build_messages()`
/// and stores the result (plus tool definitions) in `state`.
///
/// Also fires the `PreIteration` event, which in the original loop fired at the
/// start of each iteration before context construction.
pub struct BuildContextHook;

#[async_trait]
impl PreChatHook for BuildContextHook {
    async fn call(&self, spec: &RunSpec<'_>, state: &mut RunState) -> Result<HookAction> {
        // Pre-iteration event (preserves original behavior).
        spec.task_hook.fire_event(AgentEvent::PreIteration {
            session_id: spec.session_id.clone(),
            iteration: state.iterations,
        });

        // Build context (with session summary from last consolidation).
        let session_summary = {
            let sm = spec.session_manager().lock().await;
            sm.get_last_summary(&spec.session_id).await
        };

        let context_builder = ContextBuilder::new(
            spec.session_manager().clone(),
            spec.tool_manager().clone(),
            spec.workspace_dir().clone(),
            spec.memory_service().clone(),
            spec.pressure().clone(),
        )
        .with_connectors(spec.connectors().clone());
        let ctx = context_builder
            .build_messages(
                &spec.session_id,
                &spec.channel,
                &spec.chat_id,
                session_summary.as_deref(),
                Some(spec.runtime_ctx.clone()),
            )
            .await;

        let history_len = ctx.history.len();
        let current_turn_len = ctx.current_turn.len();
        let tools_len = ctx.tools.as_ref().map(|t| t.len()).unwrap_or(0);
        crate::debug!(
            "[AgentRunner] Context built: history={}, current_turn={}, tools={}",
            history_len,
            current_turn_len,
            tools_len
        );

        state.messages = std::iter::once(ctx.system_message)
            .chain(ctx.history.iter().cloned())
            .chain(ctx.current_turn)
            .collect();
        state.tools = ctx.tools;
        Ok(HookAction::Continue)
    }
}

#[cfg(test)]
mod tests {

    use std::sync::Arc;

    use super::*;
    use crate::runner::test_support::RunFixture;

    use crate::session::{Message, SharedSessionManager};

    #[tokio::test]
    async fn test_build_context_populates_messages_and_tools() {
        let fixture = RunFixture::new();
        let spec = fixture.spec();
        let sm: SharedSessionManager = spec.session_manager().clone();
        sm.lock().await.get_or_create("s1").await.unwrap();

        let mut state = crate::runner::types::RunState::new(Arc::new(
            crate::tool::TurnContext::new("cli".into(), "c1".into()),
        ));
        let action = BuildContextHook.call(&spec, &mut state).await.unwrap();
        assert_eq!(action, HookAction::Continue);
        assert!(!state.messages.is_empty());
        assert!(state.tools.is_some());
        // First message should be the system message.
        assert!(matches!(state.messages[0], Message::System { .. }));
    }
}
