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

use super::AgentResult;

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

/// Immutable run configuration handed to every hook invocation.
///
/// All fields are fixed for the duration of a single `AgentRunner::run()` call.
#[derive(Clone)]
pub struct RunSpec {
    /// The session being run.
    pub session_id: String,
    /// The raw user message that started this turn.
    pub content: String,
    /// The task hook used to fire events and notify status changes.
    pub task_hook: TaskHook,
    /// Maximum number of ReAct iterations before the run is aborted.
    pub max_iterations: u32,
    /// Maximum number of tokens requested per LLM response.
    pub max_tokens: u32,
    /// Cancellation token; checked before each hook invocation.
    pub cancel_token: Option<CancellationToken>,
    /// Directory for persisted tool results and workspace files.
    pub workspace_dir: PathBuf,
    /// Shared session manager for message persistence.
    pub session_manager: SharedSessionManager,
    /// Shared tool manager (read-only access; execution is a framework built-in).
    pub tool_manager: Arc<ToolManager>,
    /// Agent configuration snapshot.
    pub config: AgentConfig,
    /// Context-pressure anchor for this run (window tokens + ratio table).
    pub pressure: crate::pressure::PressureAnchor,
    /// Shared memory store used by the context builder.
    pub memory_service: SharedMemory,
    /// Optional consolidator triggered after a successful turn.
    pub consolidator: Option<Arc<Consolidator>>,
    /// Optional tracer; when `None` (or with no backends) tracing is a no-op.
    pub tracer: Option<Arc<Tracer>>,
    /// Message count recorded before this turn, used for rollback on error/cancellation.
    pub initial_message_count: usize,
    /// Origin channel resolved from the session id / injected origin.
    pub channel: String,
    /// Origin chat id resolved from the session id / injected origin.
    pub chat_id: String,
    /// Runtime context string generated once per turn for cache stability.
    pub runtime_ctx: String,
    /// The in-memory pending-approval store shared by every run of this
    /// process (ADR 0011). Never persisted: a restart fails closed.
    pub approvals: Arc<crate::approval::ApprovalStore>,
    /// The gateway's MCP connector pool (ticket 26), when any server is
    /// configured. Scripts reach its tools through the codemode agent entry;
    /// they are never part of the model's tool schema.
    pub connectors: Option<Arc<crate::mcp::McpService>>,
}

impl RunSpec {
    /// Returns `true` if the cancellation token has been cancelled.
    pub fn is_cancelled(&self) -> bool {
        self.cancel_token
            .as_ref()
            .is_some_and(|ct| ct.is_cancelled())
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
        let spec = make_spec();
        assert!(!spec.is_cancelled());

        let ct = CancellationToken::new();
        let mut spec2 = spec.clone();
        spec2.cancel_token = Some(ct.clone());
        ct.cancel();
        assert!(spec2.is_cancelled());
    }

    fn make_spec() -> RunSpec {
        let tmp = tempfile::TempDir::new().unwrap();
        let session_dir = tmp.path().join("sessions");
        let workspace_dir = tmp.path().join("workspace");
        std::fs::create_dir_all(&workspace_dir).unwrap();
        RunSpec {
            session_id: "s1".into(),
            content: "hi".into(),
            task_hook: TaskHook::new("s1"),
            max_iterations: 10,
            max_tokens: 100,
            cancel_token: None,
            workspace_dir: workspace_dir.clone(),
            session_manager: Arc::new(tokio::sync::Mutex::new(
                crate::session::SessionManager::new(session_dir).unwrap(),
            )),
            tool_manager: Arc::new(ToolManager::new(workspace_dir.clone())),
            config: AgentConfig::default(),
            pressure: Default::default(),
            memory_service: Arc::new(tokio::sync::Mutex::new(crate::memory::MemoryService::new(
                &workspace_dir,
            ))),
            consolidator: None,
            tracer: None,
            initial_message_count: 0,
            channel: "webui".into(),
            chat_id: "c1".into(),
            runtime_ctx: "ctx".into(),
            approvals: Default::default(),
            connectors: None,
        }
    }
}
