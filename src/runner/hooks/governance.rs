use anyhow::Result;
use async_trait::async_trait;

use crate::runner::AgentRunner;
use crate::runner::traits::PreChatHook;
use crate::runner::types::{HookAction, RunSpec, RunState};

/// Applies message governance to the built context: drops orphan tool results,
/// backfills missing tool results, microcompacts, applies the tool-result budget,
/// and snips messages to fit the token window.
pub struct GovernanceHook;

#[async_trait]
impl PreChatHook for GovernanceHook {
    async fn call(&self, spec: &RunSpec, state: &mut RunState) -> Result<HookAction> {
        let mut all_messages = std::mem::take(&mut state.messages);

        // History governance: clean orphans, backfill, microcompact, budget tool results.
        AgentRunner::drop_orphan_tool_results(&mut all_messages);
        AgentRunner::backfill_missing_tool_results(&mut all_messages);
        all_messages = crate::snip::microcompact(all_messages);
        all_messages = crate::snip::apply_tool_result_budget(all_messages);

        // Snip: truncate messages to fit within token budget (stateless, no persistence).
        let char_per_token_ratio = {
            let sm = spec.session_manager.lock().await;
            sm.get_session_data(&spec.session_id)
                .map(|d| d.char_per_token_ratio)
                .unwrap_or(4.0)
        };
        all_messages = crate::snip::snip_messages(
            all_messages,
            spec.config.context_window_tokens,
            spec.max_tokens,
            char_per_token_ratio,
        );

        // Post-snip cleanup: snipping may have created new orphans.
        AgentRunner::drop_orphan_tool_results(&mut all_messages);
        AgentRunner::backfill_missing_tool_results(&mut all_messages);

        state.messages = all_messages;
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
    use crate::session::{Message, SessionManager, TaskHook};
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
    async fn test_governance_preserves_user_and_system_messages() {
        let spec = make_spec();
        let mut state = crate::runner::types::RunState::new(Arc::new(
            crate::tool::TurnContext::new("cli".into(), "c1".into()),
        ));
        // Simulate a built context: system + user + orphan tool result.
        state.messages = vec![
            Message::system("sys".into()),
            Message::user("u".into()),
            Message::tool("orphan".into(), "tc-999".into(), Some("echo".into())),
        ];

        let action = GovernanceHook.call(&spec, &mut state).await.unwrap();
        assert_eq!(action, HookAction::Continue);
        // The orphan tool result should have been dropped.
        assert_eq!(state.messages.len(), 2);
        assert!(matches!(state.messages[1], Message::User { .. }));
    }
}
