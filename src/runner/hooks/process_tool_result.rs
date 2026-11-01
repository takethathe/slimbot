use anyhow::Result;
use async_trait::async_trait;

use crate::runner::traits::AfterToolHook;
use crate::runner::types::{HookAction, RunSpec, RunState};
use crate::tool::{
    ToolCall, ensure_nonempty_tool_result, persist_tool_result, truncate_text_head_tail,
};

/// Processes a tool result: ensures it is non-empty, optionally persists it to
/// disk, and truncates it to the configured budget. The final content is stored
/// in `state.tool_results` for the `PersistToolMessageHook`.
pub struct ProcessToolResultHook;

#[async_trait]
impl AfterToolHook for ProcessToolResultHook {
    async fn call(
        &self,
        spec: &RunSpec,
        state: &mut RunState,
        tool_call: &ToolCall,
        result: &str,
    ) -> Result<HookAction> {
        let ensured = ensure_nonempty_tool_result(&tool_call.name, result);

        // Limits come from the window anchor: the persist threshold and the
        // preview length are now two separate percentages instead of one field
        // that meant both things at once.
        let limits = spec
            .pressure
            .limits(super::char_per_token_ratio(spec).await);

        let processed = if spec.config.persist_tool_results && limits.single_tool_result_chars > 0 {
            persist_tool_result(
                &spec.workspace_dir,
                &tool_call.id,
                &ensured,
                limits.single_tool_result_chars,
                limits.persisted_preview_chars,
            )
        } else {
            ensured
        };

        let final_content = if limits.single_tool_result_chars > 0 {
            truncate_text_head_tail(&processed, limits.single_tool_result_chars)
        } else {
            processed
        };
        state.tool_results.push((tool_call.clone(), final_content));
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
            approvals: Default::default(),
            tracer: None,
        }
    }

    #[tokio::test]
    async fn test_process_tool_result_ensures_nonempty() {
        let spec = make_spec();
        let mut state = crate::runner::types::RunState::new(Arc::new(
            crate::tool::TurnContext::new("cli".into(), "c1".into()),
        ));
        let call = ToolCall {
            id: "tc-1".into(),
            name: "echo".into(),
            args: serde_json::json!({}),
        };
        ProcessToolResultHook
            .call(&spec, &mut state, &call, "")
            .await
            .unwrap();
        assert_eq!(state.tool_results.len(), 1);
        let (saved_call, content) = &state.tool_results[0];
        assert_eq!(saved_call.id, "tc-1");
        assert!(content.contains("no output"));
    }

    #[tokio::test]
    async fn test_process_tool_result_truncates_long_output() {
        let mut spec = make_spec();
        // Disable persistence so truncation is exercised directly.
        spec.config.persist_tool_results = false;
        // 4000-token window → 1000 tokens per result → 3000 chars.
        spec.pressure = crate::pressure::PressureAnchor::from_window(4_000);
        let mut state = crate::runner::types::RunState::new(Arc::new(
            crate::tool::TurnContext::new("cli".into(), "c1".into()),
        ));
        let call = ToolCall {
            id: "tc-1".into(),
            name: "echo".into(),
            args: serde_json::json!({}),
        };
        let long = "A".repeat(20_000);
        ProcessToolResultHook
            .call(&spec, &mut state, &call, &long)
            .await
            .unwrap();
        let (_, content) = &state.tool_results[0];
        assert!(content.contains("truncated"));
        assert!(content.len() < long.len());
    }
}
