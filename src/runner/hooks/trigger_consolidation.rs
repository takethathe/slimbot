use anyhow::Result;
use async_trait::async_trait;

use crate::runner::traits::AfterTurnHook;
use crate::runner::types::{RunSpec, RunState};

/// Triggers session consolidation after a successful turn, if a consolidator
/// is configured.
pub struct TriggerConsolidationHook;

#[async_trait]
impl AfterTurnHook for TriggerConsolidationHook {
    async fn call(&self, spec: &RunSpec<'_>, state: &mut RunState) -> Result<()> {
        if !state.result.success {
            return Ok(());
        }
        if let (Some(consolidator), Some(response)) =
            (spec.consolidator().as_ref(), state.response.as_ref())
            && let Ok(consolidated) = consolidator
                .maybe_consolidate_protecting(
                    &spec.session_id,
                    response.usage.prompt_tokens,
                    &spec.approvals().live_ids(&spec.session_id),
                )
                .await
        {
            state.consolidated = consolidated;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {

    use std::sync::Arc;

    use tokio::sync::Mutex;

    use super::*;
    use crate::memory::MemoryService;
    use crate::provider::{FinishReason, LLMResponse, Usage};
    use crate::runner::test_support::RunFixture;

    use crate::session::SessionManager;

    #[tokio::test]
    async fn test_trigger_consolidation_noop_without_consolidator() {
        let fixture = RunFixture::new();
        let spec = fixture.spec();
        let mut state = crate::runner::types::RunState::new(Arc::new(
            crate::tool::TurnContext::new("cli".into(), "c1".into()),
        ));
        state.result.success = true;
        state.response = Some(LLMResponse {
            content: Some("done".into()),
            tool_calls: None,
            finish_reason: FinishReason::Stop,
            usage: Usage {
                prompt_tokens: 10,
                ..Default::default()
            },
        });
        // Should not panic when no consolidator is configured.
        TriggerConsolidationHook
            .call(&spec, &mut state)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn test_trigger_consolidation_skipped_on_failure() {
        // A consolidator is configured but the run failed -> no consolidation attempt.
        let provider = Arc::new(crate::provider::MockProvider::new());
        let sm = spec_session();
        let ms = Arc::new(Mutex::new(MemoryService::new(std::path::Path::new("/tmp"))));
        let consolidator = Arc::new(crate::consolidate::Consolidator::new(
            provider, sm, ms, 32768,
        ));
        let fixture = RunFixture::builder().consolidator(consolidator).build();
        let spec = fixture.spec();
        let mut state = crate::runner::types::RunState::new(Arc::new(
            crate::tool::TurnContext::new("cli".into(), "c1".into()),
        ));
        state.result.success = false;
        TriggerConsolidationHook
            .call(&spec, &mut state)
            .await
            .unwrap();
    }

    fn spec_session() -> crate::session::SharedSessionManager {
        let tmp = tempfile::TempDir::new().unwrap();
        let session_dir = tmp.path().join("sessions");
        Arc::new(Mutex::new(SessionManager::new(session_dir).unwrap()))
    }
}
