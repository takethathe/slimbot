use anyhow::Result;
use async_trait::async_trait;

use crate::tool::ToolCall;

use super::types::{HookAction, RunSpec, RunState, ToolHookAction};

/// Hook executed before the turn begins (after the user message is written).
#[async_trait]
pub trait PreTurnHook: Send + Sync {
    async fn call(&self, spec: &RunSpec<'_>, state: &mut RunState) -> Result<HookAction>;
}

/// Hook executed before each LLM chat call (after context is built).
#[async_trait]
pub trait PreChatHook: Send + Sync {
    async fn call(&self, spec: &RunSpec<'_>, state: &mut RunState) -> Result<HookAction>;
}

/// Hook executed after each LLM chat call (before processing the response).
#[async_trait]
pub trait AfterChatHook: Send + Sync {
    async fn call(&self, spec: &RunSpec<'_>, state: &mut RunState) -> Result<HookAction>;
}

/// Hook executed before each tool call.
#[async_trait]
pub trait PreToolHook: Send + Sync {
    async fn call(
        &self,
        spec: &RunSpec<'_>,
        state: &mut RunState,
        tool_call: &ToolCall,
    ) -> Result<ToolHookAction>;
}

/// Hook executed after each tool call.
#[async_trait]
pub trait AfterToolHook: Send + Sync {
    async fn call(
        &self,
        spec: &RunSpec<'_>,
        state: &mut RunState,
        tool_call: &ToolCall,
        result: &str,
    ) -> Result<HookAction>;
}

/// Hook executed after the loop exits, regardless of success or failure.
/// `after_turn` hooks always run, even on cancellation.
#[async_trait]
pub trait AfterTurnHook: Send + Sync {
    async fn call(&self, spec: &RunSpec<'_>, state: &mut RunState) -> Result<()>;
}
