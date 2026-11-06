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
    async fn call(&self, spec: &RunSpec, state: &mut RunState) -> Result<HookAction> {
        // Pre-iteration event (preserves original behavior).
        spec.task_hook.fire_event(AgentEvent::PreIteration {
            session_id: spec.session_id.clone(),
            iteration: state.iterations,
        });

        // Build context (with session summary from last consolidation).
        let session_summary = {
            let sm = spec.session_manager.lock().await;
            sm.get_last_summary(&spec.session_id).await
        };

        let context_builder = ContextBuilder::new(
            spec.session_manager.clone(),
            spec.tool_manager.clone(),
            spec.workspace_dir.clone(),
            spec.memory_service.clone(),
        )
        .with_connectors(spec.connectors.clone());
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
    use std::path::PathBuf;
    use std::sync::Arc;

    use tokio::sync::Mutex;

    use super::*;
    use crate::memory::MemoryService;
    use crate::runner::types::RunSpec;
    use crate::session::{Message, SessionManager, SharedSessionManager, TaskHook};
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
            connectors: None,
            tracer: None,
        }
    }

    #[tokio::test]
    async fn test_build_context_populates_messages_and_tools() {
        let spec = make_spec();
        let sm: SharedSessionManager = spec.session_manager.clone();
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
