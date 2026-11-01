use anyhow::Result;
use async_trait::async_trait;

use crate::runner::traits::AfterChatHook;
use crate::runner::types::{HookAction, RunSpec, RunState};

/// Accumulates token usage from the LLM response into `state.result`.
pub struct AccumulateUsageHook;

#[async_trait]
impl AfterChatHook for AccumulateUsageHook {
    async fn call(&self, _spec: &RunSpec, state: &mut RunState) -> Result<HookAction> {
        if let Some(response) = &state.response {
            state.result.accumulate(&response.usage);
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
    async fn test_accumulate_usage_adds_to_result() {
        let spec = make_spec();
        let mut state = crate::runner::types::RunState::new(Arc::new(
            crate::tool::TurnContext::new("cli".into(), "c1".into()),
        ));
        state.response = Some(LLMResponse {
            content: None,
            tool_calls: None,
            finish_reason: FinishReason::Stop,
            usage: Usage {
                prompt_tokens: 10,
                prompt_cache_hit_tokens: 2,
                completion_tokens: 5,
                total_tokens: 15,
            },
        });

        AccumulateUsageHook.call(&spec, &mut state).await.unwrap();
        assert_eq!(state.result.total_tokens, 15);
        assert_eq!(state.result.prompt_tokens, 10);
        assert_eq!(state.result.prompt_cache_hit_tokens, 2);
        assert_eq!(state.result.completion_tokens, 5);
    }

    #[tokio::test]
    async fn test_accumulate_usage_without_response_is_noop() {
        let spec = make_spec();
        let mut state = crate::runner::types::RunState::new(Arc::new(
            crate::tool::TurnContext::new("cli".into(), "c1".into()),
        ));
        AccumulateUsageHook.call(&spec, &mut state).await.unwrap();
        assert_eq!(state.result.total_tokens, 0);
    }
}
