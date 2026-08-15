//! Default phase hooks that replicate the current `AgentRunner::run()` behavior.
//!
//! Each default hook is a stateless struct that reads everything it needs from
//! the immutable [`RunSpec`] (session manager, tool manager, config, etc.) at
//! call time, so the same hook instance can be reused across runs.

use crate::provider::LLMResponse;

mod accumulate_usage;
mod build_context;
mod fire_post_iteration;
mod fire_task_completed;
mod fire_tool_call;
mod fire_tool_result;
mod governance;
mod handle_response;
mod notify_running;
mod persist_assistant;
mod persist_tool_message;
mod process_tool_result;
mod trigger_consolidation;
mod write_user_message;

pub use accumulate_usage::AccumulateUsageHook;
pub use build_context::BuildContextHook;
pub use fire_post_iteration::FirePostIterationEventHook;
pub use fire_task_completed::FireTaskCompletedHook;
pub use fire_tool_call::FireToolCallEventHook;
pub use fire_tool_result::FireToolResultEventHook;
pub use governance::GovernanceHook;
pub use handle_response::HandleResponseHook;
pub use notify_running::NotifyRunningHook;
pub use persist_assistant::PersistAssistantMsgHook;
pub use persist_tool_message::PersistToolMessageHook;
pub use process_tool_result::ProcessToolResultHook;
pub use trigger_consolidation::TriggerConsolidationHook;
pub use write_user_message::WriteUserMessageHook;

/// Returns `true` when the response declares at least one tool call.
pub(crate) fn has_tool_calls(response: &LLMResponse) -> bool {
    response
        .tool_calls
        .as_ref()
        .is_some_and(|calls| !calls.is_empty())
}
