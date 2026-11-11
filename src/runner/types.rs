use std::any::Any;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use crate::config::AgentConfig;
use crate::consolidate::Consolidator;
use crate::memory::SharedMemory;
use crate::provider::LLMResponse;
use crate::session::{Message, SharedSessionManager, TaskHook};
use crate::tool::{ToolCall, ToolDefinition, ToolManager, TurnContext};
use crate::trace::Tracer;
use tokio_util::sync::CancellationToken;

use super::{AgentResult, AgentRunner};

/// Flow control returned by phase hooks (PreTurn, PreChat, AfterChat, AfterTool, PostIteration).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HookAction {
    /// Continue to the next hook in the phase.
    Continue,
    /// Abort the entire run and jump to the after_turn phase.
    Abort { reason: String },
}

/// Flow control returned by pre-tool hooks.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ToolHookAction {
    /// Execute the tool normally.
    Execute,
    /// Skip execution and use the provided synthetic result.
    Skip { result: String },
    /// Abort the entire run and jump to the after_turn phase.
    Abort { reason: String },
}

/// Position at which a hook is registered within its phase.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HookPosition {
    /// Append to the end of the phase's hook list.
    Append,
    /// Replace the hook at the given index. Out-of-bounds indices degrade to `Append`.
    Replace(usize),
    /// Insert before the hook at the given index. Out-of-bounds indices degrade to `Append`.
    Insert(usize),
}

/// The per-turn inputs: everything a run needs that is not a standing dependency.
///
/// Assembled in one place (`AgentRunner::run`), so no field here can default to
/// a value the hooks would mistake for a real one.
pub struct RunTurn {
    pub session_id: String,
    pub content: String,
    pub task_hook: TaskHook,
    /// Origin channel, resolved from the session id / injected origin.
    pub channel: String,
    /// Origin chat id, resolved from the session id / injected origin.
    pub chat_id: String,
    /// Runtime context string generated once per turn for cache stability.
    pub runtime_ctx: String,
}

/// The hook-facing view of one run.
///
/// The run-level dependency set — managers, config, pressure anchor, stores —
/// lives on [`AgentRunner`] and is filled in exactly one place, its builder.
/// This view only *borrows* that set through the accessors below, so a new
/// dependency never gets copied per turn: it is one builder/runner edit plus one
/// forwarding accessor, with no second assignment to keep in sync.
pub struct RunSpec<'a> {
    /// The runner that assembled this turn; every dependency is read from it.
    runner: &'a AgentRunner,
    /// The session being run.
    pub session_id: String,
    /// The raw user message that started this turn.
    pub content: String,
    /// The task hook used to fire events and notify status changes.
    pub task_hook: TaskHook,
    /// Cancellation token; checked before each hook invocation.
    pub cancel_token: Option<CancellationToken>,
    /// Message count recorded before this turn, used for rollback on error/cancellation.
    pub initial_message_count: usize,
    /// Origin channel resolved from the session id / injected origin.
    pub channel: String,
    /// Origin chat id resolved from the session id / injected origin.
    pub chat_id: String,
    /// Runtime context string generated once per turn for cache stability.
    pub runtime_ctx: String,
}

impl<'a> RunSpec<'a> {
    /// Assemble the per-turn inputs; the dependency set stays on `runner`.
    pub fn new(runner: &'a AgentRunner, turn: RunTurn) -> Self {
        Self {
            runner,
            session_id: turn.session_id,
            content: turn.content,
            task_hook: turn.task_hook,
            cancel_token: None,
            initial_message_count: 0,
            channel: turn.channel,
            chat_id: turn.chat_id,
            runtime_ctx: turn.runtime_ctx,
        }
    }

    /// The runner this view borrows from (tests compare identity through it).
    #[cfg(test)]
    pub(crate) fn runner(&self) -> &'a AgentRunner {
        self.runner
    }

    /// Returns `true` if the cancellation token has been cancelled.
    pub fn is_cancelled(&self) -> bool {
        self.cancel_token
            .as_ref()
            .is_some_and(|ct| ct.is_cancelled())
    }

    // ── Run-level dependencies: borrowed from the assembly point ──

    /// Agent configuration snapshot.
    pub fn config(&self) -> &'a AgentConfig {
        self.runner.config()
    }

    /// Maximum number of ReAct iterations before the run is aborted.
    pub fn max_iterations(&self) -> u32 {
        self.runner.config().max_iterations
    }

    /// Maximum number of tokens requested per LLM response.
    pub fn max_tokens(&self) -> u32 {
        self.runner.max_tokens()
    }

    /// Context-pressure anchor for this run (window tokens + ratio table).
    pub fn pressure(&self) -> &'a crate::pressure::PressureAnchor {
        self.runner.pressure()
    }

    /// Directory for persisted tool results and workspace files.
    pub fn workspace_dir(&self) -> &'a PathBuf {
        self.runner.workspace_dir()
    }

    /// Shared session manager for message persistence.
    pub fn session_manager(&self) -> &'a SharedSessionManager {
        self.runner.session_manager()
    }

    /// Shared tool manager (read-only access; execution is a framework built-in).
    pub fn tool_manager(&self) -> &'a Arc<ToolManager> {
        self.runner.tool_manager()
    }

    /// Shared memory store used by the context builder.
    pub fn memory_service(&self) -> &'a SharedMemory {
        self.runner.memory_service()
    }

    /// Optional consolidator triggered after a successful turn.
    pub fn consolidator(&self) -> Option<Arc<Consolidator>> {
        self.runner.consolidator()
    }

    /// Optional tracer; when `None` (or with no backends) tracing is a no-op.
    pub fn tracer(&self) -> Option<Arc<Tracer>> {
        self.runner.tracer()
    }

    /// The in-memory pending-approval store shared by every run of this
    /// process (ADR 0011). Never persisted: a restart fails closed.
    pub fn approvals(&self) -> &'a Arc<crate::approval::ApprovalStore> {
        self.runner.approvals()
    }

    /// The gateway's MCP connector pool (ticket 26), when any server is
    /// configured. Scripts reach its tools through the codemode agent entry;
    /// they are never part of the model's tool schema.
    pub fn connectors(&self) -> Option<Arc<crate::mcp::McpService>> {
        self.runner.connectors()
    }
}

/// Mutable run state threaded through every phase of the hook pipeline.
pub struct RunState {
    /// Current ReAct iteration counter (incremented after each tool-call iteration).
    pub iterations: u32,
    /// Accumulated result of the run (success flag, content, token usage).
    pub result: AgentResult,
    /// Messages prepared for the LLM (built by the BuildContext + Governance hooks).
    pub messages: Vec<Message>,
    /// Tool definitions advertised to the LLM.
    pub tools: Option<Vec<ToolDefinition>>,
    /// The most recent LLM response (set by the framework between PreChat and AfterChat).
    pub response: Option<LLMResponse>,
    /// Tool results accumulated during the current turn, keyed by their tool call.
    pub tool_results: Vec<(ToolCall, String)>,
    /// Per-turn origin context, preserved for tools that depend on it.
    pub turn_context: Arc<TurnContext>,
    /// Set by after-chat hooks when the loop should exit (e.g. no tool calls).
    pub should_exit: bool,
    /// When `Some(n)`, after-turn hooks roll the session back to `n` messages instead of persisting.
    pub rollback_to: Option<usize>,
    /// True when the after-turn phase triggered session consolidation.
    pub consolidated: bool,
    /// Cross-phase custom data store with type-safe accessors.
    extensions: HashMap<String, Box<dyn Any + Send>>,
}

impl RunState {
    /// Create a fresh run state for a new turn.
    pub fn new(turn_context: Arc<TurnContext>) -> Self {
        Self {
            iterations: 0,
            result: AgentResult::default(),
            messages: Vec::new(),
            tools: None,
            response: None,
            tool_results: Vec::new(),
            turn_context,
            should_exit: false,
            rollback_to: None,
            consolidated: false,
            extensions: HashMap::new(),
        }
    }

    /// Store an arbitrary value in the run state for cross-phase communication.
    pub fn set_extension<T: Any + Send>(&mut self, key: &str, value: T) {
        self.extensions.insert(key.to_string(), Box::new(value));
    }

    /// Retrieve a previously stored extension value by type.
    pub fn get_extension<T: Any + Send>(&self, key: &str) -> Option<&T> {
        self.extensions.get(key).and_then(|b| b.downcast_ref::<T>())
    }

    /// Mark the run as failed with the given error message.
    pub(crate) fn fail(&mut self, error: String) {
        self.result.success = false;
        self.result.content = error;
        self.result.iterations = self.iterations;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runner::test_support::RunFixture;

    #[test]
    fn test_run_state_set_get_extension_roundtrip() {
        let mut state = RunState::new(Arc::new(TurnContext::new("webui".into(), "c1".into())));
        state.set_extension("budget", 42u32);
        assert_eq!(state.get_extension::<u32>("budget"), Some(&42));
        assert_eq!(state.get_extension::<String>("budget"), None);
    }

    #[test]
    fn test_run_state_fail_sets_result() {
        let mut state = RunState::new(Arc::new(TurnContext::new("cli".into(), "c1".into())));
        state.iterations = 3;
        state.fail("boom".to_string());
        assert!(!state.result.success);
        assert_eq!(state.result.content, "boom");
        assert_eq!(state.result.iterations, 3);
    }

    #[test]
    fn test_run_spec_is_cancelled() {
        let fixture = RunFixture::new();
        let spec = fixture.spec();
        assert!(!spec.is_cancelled());

        let ct = CancellationToken::new();
        let mut spec2 = fixture.spec();
        spec2.cancel_token = Some(ct.clone());
        ct.cancel();
        assert!(spec2.is_cancelled());
    }
}
