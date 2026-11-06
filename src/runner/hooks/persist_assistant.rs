use anyhow::{Result, anyhow};
use async_trait::async_trait;

use crate::runner::hooks::has_tool_calls;
use crate::runner::traits::AfterChatHook;
use crate::runner::types::{HookAction, RunSpec, RunState};
use crate::session::Message;

/// Persists the assistant message (with content and tool calls) to the session.
/// On a final (no tool call) response it also updates the token ratio.
/// Actual session persistence happens in the AfterTurn phase.
pub struct PersistAssistantMsgHook;

#[async_trait]
impl AfterChatHook for PersistAssistantMsgHook {
    async fn call(&self, spec: &RunSpec, state: &mut RunState) -> Result<HookAction> {
        let response = state
            .response
            .as_ref()
            .ok_or_else(|| anyhow!("PersistAssistantMsgHook: no response in state"))?;

        let mut sm = spec.session_manager.lock().await;
        sm.add_message(
            &spec.session_id,
            Message::assistant(
                response.content.clone(),
                response.tool_calls.clone(),
                None,
                None,
            ),
        )
        .await
        .map_err(|e| anyhow!("Failed to write assistant message: {}", e))?;

        // Update token ratio so persist() saves it with the meta (final responses only).
        if !has_tool_calls(response) {
            sm.update_token_ratio(&spec.session_id, response.usage.prompt_tokens);
        }
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
    use crate::provider::{FinishReason, LLMResponse, Usage};
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
            approvals: Default::default(),
            connectors: None,
            tracer: None,
        }
    }

    #[tokio::test]
    async fn test_persist_assistant_message_writes_message() {
        let spec = make_spec();
        let sm: SharedSessionManager = spec.session_manager.clone();
        sm.lock().await.get_or_create("s1").await.unwrap();

        let mut state = crate::runner::types::RunState::new(Arc::new(
            crate::tool::TurnContext::new("cli".into(), "c1".into()),
        ));
        state.response = Some(LLMResponse {
            content: Some("hello there".into()),
            tool_calls: None,
            finish_reason: FinishReason::Stop,
            usage: Usage {
                prompt_tokens: 25,
                ..Default::default()
            },
        });

        PersistAssistantMsgHook
            .call(&spec, &mut state)
            .await
            .unwrap();

        let msgs = sm.lock().await.get_messages("s1").await;
        assert_eq!(msgs.len(), 1);
        assert!(matches!(
            &msgs[0],
            Message::Assistant { content: Some(c), tool_calls: None, .. } if c == "hello there"
        ));
    }

    #[tokio::test]
    async fn test_persist_assistant_message_updates_token_ratio_on_final() {
        let spec = make_spec();
        let sm: SharedSessionManager = spec.session_manager.clone();
        sm.lock().await.get_or_create("s1").await.unwrap();

        let mut state = crate::runner::types::RunState::new(Arc::new(
            crate::tool::TurnContext::new("cli".into(), "c1".into()),
        ));
        state.response = Some(LLMResponse {
            content: Some("done".into()),
            tool_calls: None,
            finish_reason: FinishReason::Stop,
            usage: Usage {
                prompt_tokens: 100,
                ..Default::default()
            },
        });

        PersistAssistantMsgHook
            .call(&spec, &mut state)
            .await
            .unwrap();
        let ratio = sm
            .lock()
            .await
            .get_session_data("s1")
            .map(|d| d.char_per_token_ratio);
        assert!(
            ratio.is_some(),
            "token ratio should be updated on final response"
        );
    }
}
