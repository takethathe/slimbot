use anyhow::Result;
use async_trait::async_trait;

use crate::runner::traits::AfterChatHook;
use crate::runner::types::{HookAction, RunSpec, RunState};

/// Accumulates token usage from the LLM response into `state.result`.
pub struct AccumulateUsageHook;

#[async_trait]
impl AfterChatHook for AccumulateUsageHook {
    async fn call(&self, _spec: &RunSpec<'_>, state: &mut RunState) -> Result<HookAction> {
        if let Some(response) = &state.response {
            state.result.accumulate(&response.usage);
        }
        Ok(HookAction::Continue)
    }
}

#[cfg(test)]
mod tests {

    use std::sync::Arc;

    use super::*;
    use crate::runner::test_support::RunFixture;

    use crate::provider::{FinishReason, LLMResponse, Usage};

    #[tokio::test]
    async fn test_accumulate_usage_adds_to_result() {
        let fixture = RunFixture::new();
        let spec = fixture.spec();
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
        let fixture = RunFixture::new();
        let spec = fixture.spec();
        let mut state = crate::runner::types::RunState::new(Arc::new(
            crate::tool::TurnContext::new("cli".into(), "c1".into()),
        ));
        AccumulateUsageHook.call(&spec, &mut state).await.unwrap();
        assert_eq!(state.result.total_tokens, 0);
    }
}
