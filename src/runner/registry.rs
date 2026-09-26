use std::fmt;
use std::sync::Arc;

use crate::tool::ToolCall;
use crate::warn_log;

use super::traits::{
    AfterChatHook, AfterToolHook, AfterTurnHook, PostIterationHook, PreChatHook, PreToolHook,
    PreTurnHook,
};
use super::types::{HookAction, HookPosition, RunSpec, RunState, ToolHookAction};

/// Ordered execution for hooks that take `(&RunSpec, &mut RunState)`.
macro_rules! run_ordered {
    ($hooks:expr, $spec:expr, $state:expr) => {{
        let mut action = HookAction::Continue;
        for hook in $hooks {
            if $spec.is_cancelled() {
                return Err(HookRunError::Cancelled);
            }
            match hook.call($spec, $state).await {
                Ok(HookAction::Continue) => {}
                Ok(abort @ HookAction::Abort { .. }) => {
                    action = abort;
                    break;
                }
                Err(e) => return Err(HookRunError::Hook(e)),
            }
        }
        Ok(action)
    }};
}

/// Ordered execution for AfterTool hooks.
macro_rules! run_ordered_with {
    ($hooks:expr, $spec:expr, $state:expr, $tool_call:expr, $result:expr) => {{
        let mut action = HookAction::Continue;
        for hook in $hooks {
            if $spec.is_cancelled() {
                return Err(HookRunError::Cancelled);
            }
            match hook.call($spec, $state, $tool_call, $result).await {
                Ok(HookAction::Continue) => {}
                Ok(abort @ HookAction::Abort { .. }) => {
                    action = abort;
                    break;
                }
                Err(e) => return Err(HookRunError::Hook(e)),
            }
        }
        Ok(action)
    }};
}

/// Error raised while running a phase of hooks.
#[derive(Debug)]
pub enum HookRunError {
    /// The run was cancelled (checked before each hook invocation).
    Cancelled,
    /// An individual hook returned an error.
    Hook(anyhow::Error),
}

impl fmt::Display for HookRunError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            HookRunError::Cancelled => write!(f, "run cancelled"),
            HookRunError::Hook(e) => write!(f, "hook error: {}", e),
        }
    }
}

impl std::error::Error for HookRunError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            HookRunError::Cancelled => None,
            HookRunError::Hook(e) => e.source(),
        }
    }
}

/// Stores all registered hooks per phase and encapsulates iteration, cancellation
/// checking, and action handling. `AgentRunner::run()` only calls the high-level
/// `run_*` methods and never manipulates the hook lists directly.
#[derive(Default)]
pub struct HookRegistry {
    pre_turn: Vec<Arc<dyn PreTurnHook>>,
    pre_chat: Vec<Arc<dyn PreChatHook>>,
    after_chat: Vec<Arc<dyn AfterChatHook>>,
    pre_tool: Vec<Arc<dyn PreToolHook>>,
    after_tool: Vec<Arc<dyn AfterToolHook>>,
    post_iteration: Vec<Arc<dyn PostIterationHook>>,
    after_turn: Vec<Arc<dyn AfterTurnHook>>,
}

impl HookRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Returns `true` when no hooks have been registered in any phase.
    pub fn is_empty(&self) -> bool {
        self.pre_turn.is_empty()
            && self.pre_chat.is_empty()
            && self.after_chat.is_empty()
            && self.pre_tool.is_empty()
            && self.after_tool.is_empty()
            && self.post_iteration.is_empty()
            && self.after_turn.is_empty()
    }

    /// Apply a `HookPosition` to a list of the given length, returning the resolved
    /// operation as an index and whether it replaces or inserts.
    fn apply_position(len: usize, pos: HookPosition) -> PositionOp {
        match pos {
            HookPosition::Append => PositionOp::Push,
            HookPosition::Replace(i) if i < len => PositionOp::Replace(i),
            HookPosition::Insert(i) if i < len => PositionOp::Insert(i),
            // Out-of-bounds Replace/Insert degrade to Append.
            HookPosition::Replace(_) | HookPosition::Insert(_) => PositionOp::Push,
        }
    }

    pub fn add_pre_turn(&mut self, hook: Arc<dyn PreTurnHook>, pos: HookPosition) {
        let op = Self::apply_position(self.pre_turn.len(), pos);
        insert_at(&mut self.pre_turn, hook, op);
    }

    pub fn add_pre_chat(&mut self, hook: Arc<dyn PreChatHook>, pos: HookPosition) {
        let op = Self::apply_position(self.pre_chat.len(), pos);
        insert_at(&mut self.pre_chat, hook, op);
    }

    pub fn add_after_chat(&mut self, hook: Arc<dyn AfterChatHook>, pos: HookPosition) {
        let op = Self::apply_position(self.after_chat.len(), pos);
        insert_at(&mut self.after_chat, hook, op);
    }

    pub fn add_pre_tool(&mut self, hook: Arc<dyn PreToolHook>, pos: HookPosition) {
        let op = Self::apply_position(self.pre_tool.len(), pos);
        insert_at(&mut self.pre_tool, hook, op);
    }

    pub fn add_after_tool(&mut self, hook: Arc<dyn AfterToolHook>, pos: HookPosition) {
        let op = Self::apply_position(self.after_tool.len(), pos);
        insert_at(&mut self.after_tool, hook, op);
    }

    pub fn add_post_iteration(&mut self, hook: Arc<dyn PostIterationHook>, pos: HookPosition) {
        let op = Self::apply_position(self.post_iteration.len(), pos);
        insert_at(&mut self.post_iteration, hook, op);
    }

    pub fn add_after_turn(&mut self, hook: Arc<dyn AfterTurnHook>, pos: HookPosition) {
        let op = Self::apply_position(self.after_turn.len(), pos);
        insert_at(&mut self.after_turn, hook, op);
    }

    /// Run all PreTurn hooks in order. Checks cancellation before each hook.
    pub async fn run_pre_turn(
        &self,
        spec: &RunSpec,
        state: &mut RunState,
    ) -> Result<HookAction, HookRunError> {
        run_ordered!(&self.pre_turn, spec, state)
    }

    /// Run all PreChat hooks in order. Checks cancellation before each hook.
    pub async fn run_pre_chat(
        &self,
        spec: &RunSpec,
        state: &mut RunState,
    ) -> Result<HookAction, HookRunError> {
        run_ordered!(&self.pre_chat, spec, state)
    }

    /// Run all AfterChat hooks in order. Checks cancellation before each hook.
    pub async fn run_after_chat(
        &self,
        spec: &RunSpec,
        state: &mut RunState,
    ) -> Result<HookAction, HookRunError> {
        run_ordered!(&self.after_chat, spec, state)
    }

    /// Run all PreTool hooks in order, returning the resolved action.
    /// Checks cancellation before each hook.
    pub async fn run_pre_tool(
        &self,
        spec: &RunSpec,
        state: &mut RunState,
        tool_call: &ToolCall,
    ) -> Result<ToolHookAction, HookRunError> {
        for hook in &self.pre_tool {
            if spec.is_cancelled() {
                return Err(HookRunError::Cancelled);
            }
            match hook.call(spec, state, tool_call).await {
                Ok(ToolHookAction::Execute) => {}
                Ok(action @ (ToolHookAction::Skip { .. } | ToolHookAction::Abort { .. })) => {
                    return Ok(action);
                }
                Err(e) => return Err(HookRunError::Hook(e)),
            }
        }
        Ok(ToolHookAction::Execute)
    }

    /// Run all AfterTool hooks in order. Checks cancellation before each hook.
    pub async fn run_after_tool(
        &self,
        spec: &RunSpec,
        state: &mut RunState,
        tool_call: &ToolCall,
        result: &str,
    ) -> Result<HookAction, HookRunError> {
        run_ordered_with!(&self.after_tool, spec, state, tool_call, result)
    }

    /// Run all PostIteration hooks in order. Checks cancellation before each hook.
    pub async fn run_post_iteration(
        &self,
        spec: &RunSpec,
        state: &mut RunState,
    ) -> Result<HookAction, HookRunError> {
        run_ordered!(&self.post_iteration, spec, state)
    }

    /// Run all AfterTurn hooks. Cancellation is NOT checked: cleanup always runs.
    /// Errors from individual hooks are logged and the remaining hooks still execute.
    pub async fn run_after_turn(&self, spec: &RunSpec, state: &mut RunState) {
        for hook in &self.after_turn {
            if let Err(e) = hook.call(spec, state).await {
                warn_log!("[HookRegistry] after_turn hook error: {}", e);
            }
        }
    }
}

/// Where a hook should be placed in its phase list.
enum PositionOp {
    Push,
    Replace(usize),
    Insert(usize),
}

fn insert_at<T: ?Sized>(list: &mut Vec<Arc<T>>, hook: Arc<T>, op: PositionOp) {
    match op {
        PositionOp::Push => list.push(hook),
        PositionOp::Replace(i) => list[i] = hook,
        PositionOp::Insert(i) => list.insert(i, hook),
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use async_trait::async_trait;

    use super::*;
    use crate::runner::types::RunState;
    use crate::tool::TurnContext;

    struct RecordingHook {
        order: Arc<AtomicUsize>,
    }

    #[async_trait]
    impl PreChatHook for RecordingHook {
        async fn call(&self, _spec: &RunSpec, _state: &mut RunState) -> anyhow::Result<HookAction> {
            self.order.fetch_add(1, Ordering::SeqCst);
            Ok(HookAction::Continue)
        }
    }

    struct AbortingHook;

    #[async_trait]
    impl PreChatHook for AbortingHook {
        async fn call(&self, _spec: &RunSpec, _state: &mut RunState) -> anyhow::Result<HookAction> {
            Ok(HookAction::Abort {
                reason: "stop now".to_string(),
            })
        }
    }

    struct SkipHook;

    #[async_trait]
    impl PreToolHook for SkipHook {
        async fn call(
            &self,
            _spec: &RunSpec,
            _state: &mut RunState,
            _tool_call: &ToolCall,
        ) -> anyhow::Result<ToolHookAction> {
            Ok(ToolHookAction::Skip {
                result: "cached".to_string(),
            })
        }
    }

    fn dummy_spec() -> RunSpec {
        RunSpec {
            session_id: "s".into(),
            content: "c".into(),
            task_hook: crate::session::TaskHook::new("s"),
            max_iterations: 10,
            max_tokens: 100,
            cancel_token: None,
            workspace_dir: std::path::PathBuf::from("/tmp"),
            session_manager: Arc::new(tokio::sync::Mutex::new(
                crate::session::SessionManager::new(std::path::PathBuf::from("/tmp/sess")).unwrap(),
            )),
            tool_manager: Arc::new(crate::tool::ToolManager::new(std::path::PathBuf::from(
                "/tmp",
            ))),
            config: crate::config::AgentConfig::default(),
            memory_service: Arc::new(tokio::sync::Mutex::new(crate::memory::MemoryService::new(
                std::path::Path::new("/tmp"),
            ))),
            consolidator: None,
            initial_message_count: 0,
            channel: "cli".into(),
            chat_id: "c1".into(),
            runtime_ctx: "ctx".into(),
            tracer: None,
        }
    }

    fn dummy_state() -> RunState {
        RunState::new(Arc::new(TurnContext::new("cli".into(), "c1".into())))
    }

    #[tokio::test]
    async fn test_append_runs_in_registration_order() {
        let order = Arc::new(AtomicUsize::new(0));
        let mut reg = HookRegistry::new();
        reg.add_pre_chat(
            Arc::new(RecordingHook {
                order: order.clone(),
            }),
            HookPosition::Append,
        );
        reg.add_pre_chat(
            Arc::new(RecordingHook {
                order: order.clone(),
            }),
            HookPosition::Append,
        );
        reg.add_pre_chat(
            Arc::new(RecordingHook {
                order: order.clone(),
            }),
            HookPosition::Append,
        );

        let spec = dummy_spec();
        let mut state = dummy_state();
        let action = reg.run_pre_chat(&spec, &mut state).await.unwrap();
        assert_eq!(action, HookAction::Continue);
        assert_eq!(order.load(Ordering::SeqCst), 3);
    }

    #[tokio::test]
    async fn test_run_pre_chat_with_abort() {
        let order = Arc::new(AtomicUsize::new(0));
        let mut reg = HookRegistry::new();
        reg.add_pre_chat(
            Arc::new(RecordingHook {
                order: order.clone(),
            }),
            HookPosition::Append,
        );
        reg.add_pre_chat(Arc::new(AbortingHook), HookPosition::Append);
        // A hook registered after the aborting one must not run.
        reg.add_pre_chat(
            Arc::new(RecordingHook {
                order: order.clone(),
            }),
            HookPosition::Append,
        );

        let spec = dummy_spec();
        let mut state = dummy_state();
        let action = reg.run_pre_chat(&spec, &mut state).await.unwrap();
        assert_eq!(
            action,
            HookAction::Abort {
                reason: "stop now".to_string()
            }
        );
        assert_eq!(
            order.load(Ordering::SeqCst),
            1,
            "hooks after abort must not run"
        );
    }

    #[tokio::test]
    async fn test_run_pre_tool_with_skip() {
        let mut reg = HookRegistry::new();
        reg.add_pre_tool(Arc::new(SkipHook), HookPosition::Append);

        let spec = dummy_spec();
        let mut state = dummy_state();
        let action = reg
            .run_pre_tool(
                &spec,
                &mut state,
                &ToolCall {
                    id: "tc".into(),
                    name: "echo".into(),
                    args: serde_json::json!({}),
                },
            )
            .await
            .unwrap();
        assert_eq!(
            action,
            ToolHookAction::Skip {
                result: "cached".to_string()
            }
        );
    }

    #[tokio::test]
    async fn test_run_pre_chat_with_empty_registry_returns_continue() {
        let reg = HookRegistry::new();
        let spec = dummy_spec();
        let mut state = dummy_state();
        let action = reg.run_pre_chat(&spec, &mut state).await.unwrap();
        assert_eq!(action, HookAction::Continue);
    }

    #[tokio::test]
    async fn test_replace_degrades_to_append_when_out_of_bounds() {
        let order = Arc::new(AtomicUsize::new(0));
        let mut reg = HookRegistry::new();
        reg.add_pre_chat(
            Arc::new(RecordingHook {
                order: order.clone(),
            }),
            HookPosition::Append,
        );
        // Replace index 5 (out of bounds) -> append.
        reg.add_pre_chat(
            Arc::new(RecordingHook {
                order: order.clone(),
            }),
            HookPosition::Replace(5),
        );
        assert_eq!(order.load(Ordering::SeqCst), 0);
        let _ = (order, reg);
    }

    #[tokio::test]
    async fn test_insert_degrades_to_append_when_out_of_bounds() {
        let mut reg = HookRegistry::new();
        reg.add_pre_chat(Arc::new(AbortingHook), HookPosition::Append);
        // Insert index 5 (out of bounds) -> append.
        reg.add_pre_chat(Arc::new(AbortingHook), HookPosition::Insert(5));
        let spec = dummy_spec();
        let mut state = dummy_state();
        let action = reg.run_pre_chat(&spec, &mut state).await.unwrap();
        assert_eq!(
            action,
            HookAction::Abort {
                reason: "stop now".to_string()
            }
        );
    }

    #[tokio::test]
    async fn test_cancel_check_stops_before_hooks() {
        let order = Arc::new(AtomicUsize::new(0));
        let mut reg = HookRegistry::new();
        reg.add_pre_chat(
            Arc::new(RecordingHook {
                order: order.clone(),
            }),
            HookPosition::Append,
        );

        let ct = tokio_util::sync::CancellationToken::new();
        ct.cancel();
        let mut spec = dummy_spec();
        spec.cancel_token = Some(ct);
        let mut state = dummy_state();
        let err = reg.run_pre_chat(&spec, &mut state).await.unwrap_err();
        assert!(matches!(err, HookRunError::Cancelled));
        assert_eq!(
            order.load(Ordering::SeqCst),
            0,
            "no hook should run when cancelled"
        );
    }
}
