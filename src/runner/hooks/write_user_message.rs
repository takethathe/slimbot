use anyhow::{Result, anyhow};
use async_trait::async_trait;

use crate::runner::traits::PreTurnHook;
use crate::runner::types::{HookAction, RunSpec, RunState};
use crate::session::Message;

/// Writes the user message that started this turn into the session.
pub struct WriteUserMessageHook;

#[async_trait]
impl PreTurnHook for WriteUserMessageHook {
    async fn call(&self, spec: &RunSpec, _state: &mut RunState) -> Result<HookAction> {
        let mut sm = spec.session_manager.lock().await;
        sm.add_message(&spec.session_id, Message::user(spec.content.clone()))
            .await
            .map_err(|e| anyhow!("Failed to write user message: {}", e))?;
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
    use crate::session::{SessionManager, SharedSessionManager, TaskHook};
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
    async fn test_write_user_message_writes_to_session() {
        let spec = make_spec();
        let sm: SharedSessionManager = spec.session_manager.clone();
        sm.lock().await.get_or_create("s1").await.unwrap();

        let mut state = crate::runner::types::RunState::new(Arc::new(
            crate::tool::TurnContext::new("cli".into(), "c1".into()),
        ));
        let action = WriteUserMessageHook.call(&spec, &mut state).await.unwrap();
        assert_eq!(action, HookAction::Continue);

        let msgs = sm.lock().await.get_messages("s1").await;
        assert_eq!(msgs.len(), 1);
        assert!(matches!(&msgs[0], Message::User { .. }));
    }
}
