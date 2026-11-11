use anyhow::{Result, anyhow};
use async_trait::async_trait;

use crate::runner::traits::AfterToolHook;
use crate::runner::types::{HookAction, RunSpec, RunState};
use crate::session::Message;
use crate::tool::ToolCall;

/// Persists the processed tool message to the session.
///
/// Reads the processed content produced by the `ProcessToolResultHook` from
/// `state.tool_results` (matched by tool call id), falling back to the raw
/// result if no processed entry is present.
pub struct PersistToolMessageHook;

#[async_trait]
impl AfterToolHook for PersistToolMessageHook {
    async fn call(
        &self,
        spec: &RunSpec<'_>,
        state: &mut RunState,
        tool_call: &ToolCall,
        result: &str,
    ) -> Result<HookAction> {
        let content = state
            .tool_results
            .iter()
            .rev()
            .find(|(call, _)| call.id == tool_call.id)
            .map(|(_, c)| c.clone())
            .unwrap_or_else(|| result.to_string());

        // A result that answered a proposal carries its approval id, which is
        // what the build-time transform keys on later (ADR 0011).
        let message = match super::approval::mark_for(state, &tool_call.id) {
            Some(approval_id) => Message::tool_marked(
                content,
                tool_call.id.clone(),
                Some(tool_call.name.clone()),
                approval_id,
            ),
            None => Message::tool(content, tool_call.id.clone(), Some(tool_call.name.clone())),
        };

        let mut sm = spec.session_manager().lock().await;
        sm.add_message(&spec.session_id, message)
            .await
            .map_err(|e| anyhow!("Failed to write tool message: {}", e))?;
        Ok(HookAction::Continue)
    }
}

#[cfg(test)]
mod tests {

    use std::sync::Arc;

    use super::*;
    use crate::runner::test_support::RunFixture;

    use crate::session::SharedSessionManager;

    #[tokio::test]
    async fn test_persist_tool_message_uses_processed_content() {
        let fixture = RunFixture::new();
        let spec = fixture.spec();
        let sm: SharedSessionManager = spec.session_manager().clone();
        sm.lock().await.get_or_create("s1").await.unwrap();

        let mut state = crate::runner::types::RunState::new(Arc::new(
            crate::tool::TurnContext::new("cli".into(), "c1".into()),
        ));
        let call = ToolCall {
            id: "tc-1".into(),
            name: "echo".into(),
            args: serde_json::json!({}),
        };
        state.tool_results.push((call.clone(), "processed".into()));

        PersistToolMessageHook
            .call(&spec, &mut state, &call, "raw")
            .await
            .unwrap();

        let msgs = sm.lock().await.get_messages("s1").await;
        assert_eq!(msgs.len(), 1);
        match &msgs[0] {
            Message::Tool {
                content,
                tool_call_id,
                name,
                ..
            } => {
                assert_eq!(content, "processed");
                assert_eq!(tool_call_id, "tc-1");
                assert_eq!(name.as_deref(), Some("echo"));
            }
            other => panic!("expected Tool message, got {:?}", other),
        }
    }

    #[tokio::test]
    async fn test_persist_tool_message_falls_back_to_raw() {
        let fixture = RunFixture::new();
        let spec = fixture.spec();
        let sm: SharedSessionManager = spec.session_manager().clone();
        sm.lock().await.get_or_create("s1").await.unwrap();

        let mut state = crate::runner::types::RunState::new(Arc::new(
            crate::tool::TurnContext::new("cli".into(), "c1".into()),
        ));
        let call = ToolCall {
            id: "tc-1".into(),
            name: "echo".into(),
            args: serde_json::json!({}),
        };
        // No processed entry -> falls back to raw result.
        PersistToolMessageHook
            .call(&spec, &mut state, &call, "raw-result")
            .await
            .unwrap();

        let msgs = sm.lock().await.get_messages("s1").await;
        assert!(matches!(&msgs[0], Message::Tool { content, .. } if content == "raw-result"));
    }
}
