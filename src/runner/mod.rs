use std::path::PathBuf;
use std::sync::Arc;

use crate::config::AgentConfig;
use crate::consolidate::Consolidator;
use crate::memory::SharedMemory;
use crate::provider::{Provider, ProviderErrorKind, Usage};
use crate::session::{
    AgentEvent, Message, SharedSessionManager, TaskHook, TaskState, parse_session_origin,
};
use crate::tool::{ToolManager, TurnContext, format_tool_error, with_turn_context};
use crate::trace::{SpanGuard, SpanRecorder, SpanStatus, Tracer};
use crate::{debug, warn_log};
use tokio_util::sync::CancellationToken;

/// Agent name reported on GenAI spans via `gen_ai.agent.name`.
const AGENT_NAME: &str = "slimbot";

/// Message every cancelled exit reports, so the exits cannot drift apart.
const CANCELLED_MESSAGE: &str = "Task cancelled.";

pub(crate) mod gate;
mod hooks;
mod registry;
#[cfg(test)]
mod test_support;
mod traits;
mod types;

pub use registry::{HookRegistry, HookRunError};
pub use traits::{
    AfterChatHook, AfterToolHook, AfterTurnHook, PreChatHook, PreToolHook, PreTurnHook,
};
pub use types::{HookAction, HookPosition, RunSpec, RunState, RunTurn, ToolHookAction};

/// Max characters for the `args` attribute on tool spans.
const ARGS_MAX_CHARS: usize = 200;
/// Max characters for the `error` attribute on tool spans.
const ERROR_MAX_CHARS: usize = 500;

/// Truncate a string at a char boundary to at most `max` characters.
fn truncate_chars(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        s.chars().take(max).collect()
    }
}

/// Map a failed run to a low-cardinality `error.type` per the GenAI semantic
/// conventions (fallback `_OTHER`). A classified provider failure maps to
/// `provider_<kind>`; other failures are keyed on their message.
fn classify_run_error(result: &AgentResult) -> String {
    if let Some(pe) = &result.provider_error {
        return format!("provider_{}", pe.kind);
    }
    let message = &result.content;
    if message.contains("Reached max iterations") {
        "max_iterations".to_string()
    } else if message.contains("Task cancelled") {
        "cancelled".to_string()
    } else {
        "_OTHER".to_string()
    }
}

/// The one mapping from a hook-phase error to run state.
///
/// Cancellation fails the task and marks the turn for rollback (executed by
/// `close_turn()` before the AfterTurn hooks); any other hook error only fails
/// the task. The three control-flow exits (`false` / `break` / `break 'outer`)
/// stay at their call sites — only the two lines they share live here.
fn absorb_hook_error(state: &mut RunState, spec: &RunSpec<'_>, error: HookRunError) {
    match error {
        HookRunError::Cancelled => fail_cancelled(state, spec),
        HookRunError::Hook(e) => state.fail(e.to_string()),
    }
}

/// Fail the run as cancelled and mark the turn for rollback.
fn fail_cancelled(state: &mut RunState, spec: &RunSpec<'_>) {
    state.fail(CANCELLED_MESSAGE.to_string());
    state.rollback_to = Some(spec.initial_message_count);
}

/// The per-phase cancellation checkpoint the hook registry used to provide.
///
/// Each inlined behavior sat behind a `run_*` method that checked the token
/// before its first hook. With the hook gone the check has to be explicit, or a
/// cancelled run would announce work it never did (an unpaired `ToolCall`, a
/// `PostIteration` for an iteration that will not advance).
///
/// Returns `true` when the run was cancelled and the caller must exit.
fn cancel_here(state: &mut RunState, spec: &RunSpec<'_>) -> bool {
    if spec.is_cancelled() {
        fail_cancelled(state, spec);
        return true;
    }
    false
}

/// Close a turn: persist the session (or roll the turn back) and report the
/// final task state.
///
/// Runs on every exit path and before the AfterTurn hooks: consolidation must
/// see the settled session, and the approval turn close comes after both. It
/// is the loop's own finalization, not a policy seam.
///
/// Takes `&mut RunState` to match the hook signature it replaces and the call
/// sites, which already hold a mutable borrow of the run state.
async fn close_turn(spec: &RunSpec<'_>, state: &mut RunState) {
    let mut sm = spec.session_manager().lock().await;
    if let Some(limit) = state.rollback_to {
        // Roll back messages added during this turn; do not persist.
        sm.truncate_messages(&spec.session_id, limit).await;
    } else {
        let _ = sm.persist(&spec.session_id).await;
    }

    let hook = &spec.task_hook;
    if state.result.success {
        let text = state.result.content.clone();
        hook.notify_status_change(&TaskState::Completed {
            result: text.clone(),
        });
        hook.fire_event(AgentEvent::TaskCompleted {
            session_id: spec.session_id.clone(),
            run_id: hook.run_id().to_string(),
            result: text,
        });
    } else {
        let error = state.result.content.clone();
        hook.notify_status_change(&TaskState::Failed {
            error: error.clone(),
        });
        hook.fire_event(AgentEvent::TaskFailed {
            session_id: spec.session_id.clone(),
            run_id: hook.run_id().to_string(),
            error,
        });
    }
}

/// Result returned by AgentRunner after a ReAct loop completes.
#[derive(Debug, Clone, Default)]
pub struct AgentResult {
    pub success: bool,
    pub content: String,
    pub total_tokens: u32,
    pub prompt_tokens: u32,
    pub prompt_cache_hit_tokens: u32,
    pub completion_tokens: u32,
    pub iterations: u32,
    /// True if the message tool was used to deliver output this turn.
    /// When true, the caller should suppress the final response to avoid duplicates.
    pub message_sent: bool,
    /// Set when the run ended on a provider failure, carrying the classified
    /// kind and the provider's own (safe) message. The caller uses it to route
    /// context-length failures into pressure and to alert the owner on hard
    /// auth/quota failures — never the raw response body (ticket 19 / ADR 0018).
    pub provider_error: Option<crate::provider::ProviderError>,
}

impl AgentResult {
    fn accumulate(&mut self, usage: &Usage) {
        self.total_tokens += usage.total_tokens;
        self.prompt_tokens += usage.prompt_tokens;
        self.prompt_cache_hit_tokens += usage.prompt_cache_hit_tokens;
        self.completion_tokens += usage.completion_tokens;
    }

    /// Where a classified provider failure should go once the turn is over.
    ///
    /// The decision lives next to the result it reads, so it is one function to
    /// drive and assert instead of a branch buried in the run closure. Carrying
    /// out the answer is the caller's job: this module does not know about the
    /// outbound bus or the owner's primary channel.
    pub fn provider_failure_route(&self) -> ProviderFailureRoute {
        match self.provider_error.as_ref().map(|pe| pe.kind) {
            Some(ProviderErrorKind::AuthQuota) => ProviderFailureRoute::AlertOwner,
            Some(ProviderErrorKind::ContextLength) => ProviderFailureRoute::RelievePressure,
            _ => ProviderFailureRoute::FailInPlace,
        }
    }
}

/// What a finished run should do about a provider failure.
///
/// Three answers cover the five kinds: only auth/quota needs the owner, only a
/// context-length refusal can be relieved by compressing, and everything else
/// (transient, content filter, unclassified) simply fails the turn in place.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProviderFailureRoute {
    /// Alert the owner through the delivery seam, once per failure streak.
    AlertOwner,
    /// Run one forced consolidation round so the next turn fits.
    RelievePressure,
    /// Nothing beyond the failed turn itself.
    FailInPlace,
}

/// Builder for constructing an `AgentRunner`.
///
/// Encapsulates the composition of `ContextBuilder` and `AgentRunner` plus the
/// hook pipeline. Call [`AgentRunnerBuilder::with_defaults`] to get the stock
/// behavior, then append/replace/insert custom hooks as needed.
pub struct AgentRunnerBuilder {
    session_manager: Option<SharedSessionManager>,
    tool_manager: Option<Arc<ToolManager>>,
    provider: Option<Arc<dyn Provider>>,
    config: Option<AgentConfig>,
    workspace_dir: Option<PathBuf>,
    memory_service: Option<SharedMemory>,
    consolidator: Option<Arc<Consolidator>>,
    tracer: Option<Arc<Tracer>>,
    max_tokens: Option<u32>,
    pressure: Option<crate::pressure::PressureAnchor>,
    approvals: Option<Arc<crate::approval::ApprovalStore>>,
    connectors: Option<Arc<crate::mcp::McpService>>,
    hooks: HookRegistry,
}

impl AgentRunnerBuilder {
    pub fn new() -> Self {
        Self {
            session_manager: None,
            tool_manager: None,
            provider: None,
            config: None,
            workspace_dir: None,
            memory_service: None,
            consolidator: None,
            tracer: None,
            max_tokens: None,
            pressure: None,
            approvals: None,
            connectors: None,
            hooks: HookRegistry::new(),
        }
    }

    pub fn session_manager(mut self, sm: SharedSessionManager) -> Self {
        self.session_manager = Some(sm);
        self
    }

    pub fn tool_manager(mut self, tm: Arc<ToolManager>) -> Self {
        self.tool_manager = Some(tm);
        self
    }

    pub fn provider(mut self, p: Arc<dyn Provider>) -> Self {
        self.provider = Some(p);
        self
    }

    pub fn config(mut self, c: AgentConfig) -> Self {
        self.config = Some(c);
        self
    }

    pub fn workspace_dir(mut self, wd: PathBuf) -> Self {
        self.workspace_dir = Some(wd);
        self
    }

    pub fn memory_service(mut self, ms: SharedMemory) -> Self {
        self.memory_service = Some(ms);
        self
    }

    pub fn consolidator(mut self, c: Option<Arc<Consolidator>>) -> Self {
        self.consolidator = c;
        self
    }

    /// Set the optional tracer used to emit spans for each run.
    pub fn tracer(mut self, t: Option<Arc<Tracer>>) -> Self {
        self.tracer = t;
        self
    }

    pub fn max_tokens(mut self, mt: u32) -> Self {
        self.max_tokens = Some(mt);
        self
    }

    /// Set the context-pressure anchor (window tokens + ratio table).
    ///
    /// The window is a provider property, so callers pass
    /// [`crate::pressure::PressureAnchor::from_config`] rather than an agent
    /// setting. When unset, the default anchor (32k window) is used.
    pub fn pressure(mut self, anchor: crate::pressure::PressureAnchor) -> Self {
        self.pressure = Some(anchor);
        self
    }

    /// Attach the process-wide pending-approval store (ADR 0011).
    ///
    /// It must be shared by every run (and every entry point) so a call
    /// proposed on one turn can be redeemed on the next. Unset means a fresh
    /// default-policy store, which gates each run on its own — fine for tests,
    /// wrong for a gateway.
    pub fn approvals(mut self, store: Arc<crate::approval::ApprovalStore>) -> Self {
        self.approvals = Some(store);
        self
    }

    /// Attach the gateway's MCP connector pool (ticket 26).
    ///
    /// Unset means the run has no connector tools at all: the script's
    /// `searchTools` and `tools.*` surface is just the run's own tools. Shared
    /// across runs like the approval store, because connections are per process.
    pub fn connectors(mut self, pool: Arc<crate::mcp::McpService>) -> Self {
        self.connectors = Some(pool);
        self
    }

    /// Register the stock hooks that replicate the current `run()` behavior.
    /// Returns self so it can be chained with the dependency setters.
    pub fn with_defaults(mut self) -> Self {
        use crate::runner::hooks::*;

        // The five shallow behaviors (the ToolCall / ToolResult / PostIteration
        // events, usage accumulation, task-completed finalization) are the
        // loop's own bookkeeping and live in `run()` itself. What is registered
        // here is the policy that genuinely varies per run.
        // PreTurn
        // Approval turn opening runs first: the lifecycle must be armed (TTL
        // expiry + carried set) before anything reads it.
        self.hooks
            .add_pre_turn(Arc::new(ApprovalTurnStartHook), HookPosition::Append);
        self.hooks
            .add_pre_turn(Arc::new(WriteUserMessageHook), HookPosition::Append);
        self.hooks
            .add_pre_turn(Arc::new(NotifyRunningHook), HookPosition::Append);
        // PreChat
        self.hooks
            .add_pre_chat(Arc::new(BuildContextHook), HookPosition::Append);
        self.hooks
            .add_pre_chat(Arc::new(GovernanceHook), HookPosition::Append);
        // Approval transform last: it folds redeemed rounds onto their original
        // call (and rewrites unredeemed guidance) on the copy handed to the model.
        self.hooks
            .add_pre_chat(Arc::new(ApprovalContextHook), HookPosition::Append);
        // AfterChat
        self.hooks
            .add_after_chat(Arc::new(HandleResponseHook), HookPosition::Append);
        self.hooks
            .add_after_chat(Arc::new(PersistAssistantMsgHook), HookPosition::Append);
        // PreTool
        // The single enforcement point (ADR 0011). `run()` fires the `ToolCall`
        // event before this phase, so the channel has already been told which
        // call the model is attempting.
        self.hooks
            .add_pre_tool(Arc::new(ApprovalHook), HookPosition::Append);
        // AfterTool
        self.hooks
            .add_after_tool(Arc::new(ProcessToolResultHook), HookPosition::Append);
        self.hooks
            .add_after_tool(Arc::new(PersistToolMessageHook), HookPosition::Append);
        // AfterTurn
        self.hooks
            .add_after_turn(Arc::new(TriggerConsolidationHook), HookPosition::Append);
        // Close the approval turn last: consolidation has already run, and any
        // record that was still carried now rolls off (audit `cleared`).
        self.hooks
            .add_after_turn(Arc::new(ApprovalTurnEndHook), HookPosition::Append);
        self
    }

    /// Append a hook to the PreTurn phase.
    pub fn add_pre_turn(mut self, hook: Arc<dyn PreTurnHook>, pos: HookPosition) -> Self {
        self.hooks.add_pre_turn(hook, pos);
        self
    }

    /// Append a hook to the PreChat phase.
    pub fn add_pre_chat(mut self, hook: Arc<dyn PreChatHook>, pos: HookPosition) -> Self {
        self.hooks.add_pre_chat(hook, pos);
        self
    }

    /// Append a hook to the AfterChat phase.
    pub fn add_after_chat(mut self, hook: Arc<dyn AfterChatHook>, pos: HookPosition) -> Self {
        self.hooks.add_after_chat(hook, pos);
        self
    }

    /// Append a hook to the PreTool phase.
    pub fn add_pre_tool(mut self, hook: Arc<dyn PreToolHook>, pos: HookPosition) -> Self {
        self.hooks.add_pre_tool(hook, pos);
        self
    }

    /// Append a hook to the AfterTool phase.
    pub fn add_after_tool(mut self, hook: Arc<dyn AfterToolHook>, pos: HookPosition) -> Self {
        self.hooks.add_after_tool(hook, pos);
        self
    }

    /// Append a hook to the AfterTurn phase.
    pub fn add_after_turn(mut self, hook: Arc<dyn AfterTurnHook>, pos: HookPosition) -> Self {
        self.hooks.add_after_turn(hook, pos);
        self
    }

    pub fn build(self) -> AgentRunner {
        let session_manager = self.session_manager.expect("session_manager required");
        let tool_manager = self.tool_manager.expect("tool_manager required");
        let provider = self.provider.expect("provider required");
        let config = self.config.expect("config required");
        let workspace_dir = self.workspace_dir.expect("workspace_dir required");
        let memory_service = self.memory_service.expect("memory_service required");
        let max_tokens = self.max_tokens.unwrap_or(4096);
        let pressure = self.pressure.unwrap_or_default();
        let approvals = self
            .approvals
            .unwrap_or_else(|| Arc::new(crate::approval::ApprovalStore::default()));
        let connectors = self.connectors;

        // Builders that never called `with_defaults` (or added no custom hooks)
        // still get the stock behavior out of the box.
        let hooks = if self.hooks.is_empty() {
            let mut builder = AgentRunnerBuilder::new();
            builder = builder.with_defaults();
            builder.hooks
        } else {
            self.hooks
        };

        AgentRunner {
            tool_manager,
            provider,
            session_manager,
            config,
            workspace_dir,
            memory_service,
            consolidator: self.consolidator,
            tracer: self.tracer,
            max_tokens,
            pressure,
            approvals,
            connectors,
            hooks,
        }
    }
}

pub struct AgentRunner {
    tool_manager: Arc<ToolManager>,
    provider: Arc<dyn Provider>,
    session_manager: SharedSessionManager,
    config: AgentConfig,
    workspace_dir: PathBuf,
    memory_service: SharedMemory,
    consolidator: Option<Arc<Consolidator>>,
    tracer: Option<Arc<Tracer>>,
    max_tokens: u32,
    pressure: crate::pressure::PressureAnchor,
    /// Shared pending-approval state (in-memory, never persisted).
    approvals: Arc<crate::approval::ApprovalStore>,
    /// Gateway connector pool, when any MCP server is configured (ticket 26).
    /// Never a model-facing tool source: only scripts reach it, and then through
    /// the same approval gate as every other tool call.
    connectors: Option<Arc<crate::mcp::McpService>>,
    hooks: HookRegistry,
}

impl AgentRunner {
    /// Create a new builder for constructing an `AgentRunner`.
    pub fn builder() -> AgentRunnerBuilder {
        AgentRunnerBuilder::new()
    }

    /// Replace the context-pressure anchor after construction.
    ///
    /// `new` keeps its established signature; callers that know the provider
    /// window (the agent loop, Dream) override the default anchor here.
    pub fn with_pressure(mut self, pressure: crate::pressure::PressureAnchor) -> Self {
        self.pressure = pressure;
        self
    }

    /// Replace the pending-approval store after construction.
    pub fn with_approvals(mut self, approvals: Arc<crate::approval::ApprovalStore>) -> Self {
        self.approvals = approvals;
        self
    }

    /// Replace the connector pool after construction (see `with_approvals`).
    /// `None` is the honest "no connectors configured" and is what a caller
    /// holding an `Option<Arc<McpService>>` hands over.
    pub fn with_connectors(mut self, connectors: Option<Arc<crate::mcp::McpService>>) -> Self {
        self.connectors = connectors;
        self
    }

    // ── Run-level dependencies ──
    //
    // These are the set a `RunSpec` borrows and hooks read: assembled once by
    // the builder, never copied per turn.

    pub(crate) fn config(&self) -> &AgentConfig {
        &self.config
    }

    pub(crate) fn max_tokens(&self) -> u32 {
        self.max_tokens
    }

    pub(crate) fn pressure(&self) -> &crate::pressure::PressureAnchor {
        &self.pressure
    }

    pub(crate) fn workspace_dir(&self) -> &PathBuf {
        &self.workspace_dir
    }

    pub(crate) fn session_manager(&self) -> &SharedSessionManager {
        &self.session_manager
    }

    pub(crate) fn tool_manager(&self) -> &Arc<ToolManager> {
        &self.tool_manager
    }

    pub(crate) fn memory_service(&self) -> &SharedMemory {
        &self.memory_service
    }

    pub(crate) fn approvals(&self) -> &Arc<crate::approval::ApprovalStore> {
        &self.approvals
    }

    pub(crate) fn consolidator(&self) -> Option<Arc<Consolidator>> {
        self.consolidator.clone()
    }

    pub(crate) fn tracer(&self) -> Option<Arc<Tracer>> {
        self.tracer.clone()
    }

    pub(crate) fn connectors(&self) -> Option<Arc<crate::mcp::McpService>> {
        self.connectors.clone()
    }

    /// Construct a runner with the stock default hooks.
    ///
    /// Existing callers that use `AgentRunner::new` keep working unchanged;
    /// they automatically get the default hook pipeline.
    //
    // The 9-parameter signature is part of the public API (see
    // .scratch/runner-phase-hooks/issues/03: "keep AgentRunner::new()
    // signature"); a struct would change every call site. New callers should
    // use `builder()`, which is the single assembly point for a run.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        tool_manager: Arc<ToolManager>,
        provider: Arc<dyn Provider>,
        session_manager: SharedSessionManager,
        config: AgentConfig,
        workspace_dir: PathBuf,
        memory_service: SharedMemory,
        consolidator: Option<Arc<Consolidator>>,
        max_tokens: u32,
        tracer: Option<Arc<Tracer>>,
    ) -> Self {
        let builder = AgentRunnerBuilder::new()
            .session_manager(session_manager)
            .tool_manager(tool_manager)
            .provider(provider)
            .config(config)
            .workspace_dir(workspace_dir)
            .memory_service(memory_service)
            .consolidator(consolidator)
            .tracer(tracer)
            .max_tokens(max_tokens)
            .with_defaults();
        builder.build()
    }

    /// Run a ReAct loop turn against the phase-hook pipeline.
    //
    // The signature (7 parameters plus `&self`) is part of the public API used
    // by AgentLoop and the gateway; per-turn inputs are grouped into `RunSpec`.
    #[allow(clippy::too_many_arguments)]
    pub async fn run(
        &self,
        content: String,
        hook: TaskHook,
        session_id: &str,
        _channel_inject: Option<String>,
        cancel_token: Option<CancellationToken>,
        origin_channel: Option<String>,
        origin_chat_id: Option<String>,
    ) -> AgentResult {
        debug!("[AgentRunner] Starting run for session={}", session_id);

        // Resolve turn context once, before anything else, so the root span
        // can carry it even on the sentinel / cancellation exits.
        // Prefer origin channel/chat_id (e.g. from cron payload) over session_id-parsed values.
        let (tool_channel, tool_chat_id) = parse_session_origin(
            session_id,
            (origin_channel.as_deref(), origin_chat_id.as_deref()),
        );

        // Root trace span covering the whole run (held until after after-turn
        // hooks, so consolidation/cancellation outcomes can be recorded).
        let root_span = self.tracer.as_ref().map(|tracer| {
            let span = tracer.start_span(&format!("invoke_agent {AGENT_NAME}"), None);
            span.set_attribute("gen_ai.operation.name", serde_json::json!("invoke_agent"));
            span.set_attribute("gen_ai.agent.name", serde_json::json!(AGENT_NAME));
            span.set_attribute("gen_ai.conversation.id", serde_json::json!(session_id));
            span.set_attribute("channel", serde_json::json!(tool_channel));
            span.set_attribute("chat_id", serde_json::json!(tool_chat_id));
            span.set_attribute(
                "max_iterations",
                serde_json::json!(self.config.max_iterations),
            );
            span.set_attribute("max_tokens", serde_json::json!(self.max_tokens));
            span
        });

        // Sentinel /stop message: all prior tasks were cancelled and drained.
        if cancel_token.is_some() && content == "/stop" {
            return self.cancelled_result(&hook, session_id, &root_span).await;
        }

        // Check cancellation before starting.
        if let Some(ref ct) = cancel_token
            && ct.is_cancelled()
        {
            return self.cancelled_result(&hook, session_id, &root_span).await;
        }

        // Record message count before this turn, for rollback on cancellation/error.
        let initial_message_count = {
            let sm = self.session_manager.lock().await;
            sm.message_count(session_id)
        };

        // Generate runtime context once per turn for cache stability.
        // This ensures the timestamp doesn't change between iterations within the same turn.
        let runtime_ctx = crate::context::build_runtime_context(&tool_channel, &tool_chat_id);

        // Derive a per-run cancellation token. The run still observes the
        // caller's `/stop` cancellation (a child is cancelled with its parent),
        // but cancelling this token — which the run deadline does — never touches
        // the session-wide token that queued sibling turns of the same session
        // hold, so one stuck turn cannot cancel its neighbours.
        let run_cancel_token = match cancel_token.as_ref() {
            Some(outer) => outer.child_token(),
            None => CancellationToken::new(),
        };

        // One borrow of the runner plus this turn's inputs: the dependency set
        // is never re-projected, so a new Run-level dependency is a builder edit.
        let mut spec = RunSpec::new(
            self,
            RunTurn {
                session_id: session_id.to_string(),
                content,
                task_hook: hook,
                channel: tool_channel,
                chat_id: tool_chat_id,
                runtime_ctx,
            },
        );
        spec.cancel_token = Some(run_cancel_token);
        spec.initial_message_count = initial_message_count;

        // Scope per-turn state (context + sent_in_turn) via a task-local so that
        // concurrent runs sharing one Arc<ToolManager> each see their own state.
        // The turn also carries the run's nested tool-call authority (ticket 25):
        // the codemode agent entry executes `tools.*` calls through the same
        // approval gate this loop's hook chain uses.
        let tool_exec = Arc::new(crate::runner::gate::RunToolExec::new(
            hooks::approval::gate_context(&spec),
        ));
        let turn = Arc::new(
            TurnContext::new(spec.channel.clone(), spec.chat_id.clone()).with_tool_exec(tool_exec),
        );
        with_turn_context(turn.clone(), async {
            // Every exit must carry the sent_in_turn flag: callers suppress the
            // final response only when the message tool already delivered to the
            // origin channel, regardless of how the loop ends.
            let finalize = |mut result: AgentResult| {
                result.message_sent = turn.sent_in_turn();
                result
            };

            let mut state = RunState::new(turn.clone());

            // A whole run is bounded by a wall-clock deadline
            // (`agent.timeout_seconds`), so a stuck provider or tool call can no
            // longer hang the turn forever. Only the run body is wrapped: the
            // AfterTurn phase below stays outside so rollback/consolidation still
            // happen when the deadline fires.
            let deadline_secs = spec.config().timeout_seconds;
            let body = async {
                // PreTurn hooks: write the user message and notify the running state.
                // On abort/error/cancel the entire loop is skipped and we jump to AfterTurn.
                let pre_turn_ok = match self.hooks.run_pre_turn(&spec, &mut state).await {
                    Ok(HookAction::Continue) => true,
                    Ok(HookAction::Abort { reason }) => {
                        state.fail(reason);
                        false
                    }
                    Err(e) => {
                        absorb_hook_error(&mut state, &spec, e);
                        false
                    }
                };

                if pre_turn_ok {
                    'outer: loop {
                        // Exceeded max iterations.
                        if state.iterations >= spec.max_iterations() {
                            let err_msg = format!("Reached max iterations {}", spec.max_iterations());
                            warn_log!("[AgentRunner] {}", err_msg);
                            state.fail(err_msg);
                            break;
                        }

                        // Iteration span wrapping the chat call + tool loop.
                        let iteration_span = if let Some(root) = &root_span {
                            let span = self.start_span(Some(&root.context()), "iteration");
                            span.set_attribute(
                                "iteration",
                                serde_json::json!(state.iterations + 1),
                            );
                            Some(span)
                        } else {
                            None
                        };

                        // PreChat hooks: build context and apply governance.
                        match self.hooks.run_pre_chat(&spec, &mut state).await {
                            Ok(HookAction::Continue) => {}
                            Ok(HookAction::Abort { reason }) => {
                                state.fail(reason);
                                break;
                            }
                            Err(e) => {
                                absorb_hook_error(&mut state, &spec, e);
                                break;
                            }
                        }

                        // Request model. Cancellation may arrive after the last PreChat
                        // hook; check here so a cancelled run skips the LLM call and
                        // rolls back instead of firing a wasted request.
                        if spec.is_cancelled() {
                            fail_cancelled(&mut state, &spec);
                            break;
                        }
                        debug!("[AgentRunner] Sending chat request to provider");
                        let messages_ref: Vec<&Message> = state.messages.iter().collect();

                        // chat span: an inference call. The provider records request
                        // details (provider/model/body) through the recorder, the runner
                        // records usage and the outcome afterwards.
                        let chat_span = iteration_span.as_ref().map(|iteration| {
                            let span = self.start_span(Some(&iteration.context()), "chat");
                            span.set_attribute("gen_ai.operation.name", serde_json::json!("chat"));
                            span.set_attribute(
                                "gen_ai.request.max_tokens",
                                serde_json::json!(self.max_tokens),
                            );
                            span
                        });
                        let recorder: Option<&dyn SpanRecorder> =
                            chat_span.as_ref().map(|s| s as &dyn SpanRecorder);
                        let response = match self
                            .provider
                            .chat(&messages_ref, state.tools.as_deref(), recorder)
                            .await
                        {
                            Ok(r) => r,
                            Err(provider_err) => {
                                // The kind crosses the provider seam as itself:
                                // there is no type recovery that can fail and
                                // silently swallow an unclassified failure.
                                let kind = provider_err.kind;
                                let safe_msg = provider_err.message.clone();
                                if let Some(span) = &chat_span {
                                    span.set_attribute(
                                        "error.type",
                                        serde_json::json!(format!("provider_{kind}")),
                                    );
                                    span.set_status(SpanStatus::Error(safe_msg.clone()));
                                }
                                warn_log!(
                                    "[AgentRunner] provider call failed ({kind}): {safe_msg}"
                                );
                                // Fail the turn with the provider's own message
                                // verbatim (privacy: no raw response body).
                                state.result.provider_error = Some(provider_err);
                                state.fail(safe_msg);
                                state.rollback_to = Some(spec.initial_message_count);
                                break;
                            }
                        };
                        if let Some(span) = &chat_span {
                            span.set_attribute(
                                "gen_ai.usage.input_tokens",
                                serde_json::json!(response.usage.prompt_tokens),
                            );
                            span.set_attribute(
                                "gen_ai.usage.output_tokens",
                                serde_json::json!(response.usage.completion_tokens),
                            );
                            span.set_attribute(
                                "gen_ai.usage.cache_read.input_tokens",
                                serde_json::json!(response.usage.prompt_cache_hit_tokens),
                            );
                            span.set_attribute(
                                "gen_ai.response.finish_reasons",
                                serde_json::json!([response.finish_reason.to_string()]),
                            );
                            if let Some(content) = &response.content {
                                span.set_attribute(
                                    "gen_ai.response.content",
                                    serde_json::json!(content),
                                );
                            }
                        }
                        debug!(
                            "[AgentRunner] Got response: content_len={}, tool_calls={}, prompt_tokens={}, cached_tokens={}, completion_tokens={}, total_tokens={}",
                            response.content.as_ref().map(|s| s.len()).unwrap_or(0),
                            response.tool_calls.as_ref().map(|c| c.len()).unwrap_or(0),
                            response.usage.prompt_tokens,
                            response.usage.prompt_cache_hit_tokens,
                            response.usage.completion_tokens,
                            response.usage.total_tokens,
                        );
                        state.response = Some(response);

                        // AfterChat hooks: inspect the response and persist the
                        // assistant message.
                        match self.hooks.run_after_chat(&spec, &mut state).await {
                            Ok(HookAction::Continue) => {}
                            Ok(HookAction::Abort { reason }) => {
                                state.fail(reason);
                                break;
                            }
                            Err(e) => {
                                absorb_hook_error(&mut state, &spec, e);
                                break;
                            }
                        }

                        // Usage for this call is accounted for once, after the
                        // AfterChat phase has inspected the response.
                        if let Some(response) = &state.response {
                            state.result.accumulate(&response.usage);
                        }

                        if state.should_exit {
                            break;
                        }

                        // Execute tool calls.
                        let calls = state
                            .response
                            .as_ref()
                            .and_then(|r| r.tool_calls.clone())
                            .unwrap_or_default();
                        for call in &calls {
                            debug!("[tool_call] name={}, args={}", call.name, call.args);
                            // execute_tool span carrying the call id, arguments and (on
                            // failure) the error, keyed under the iteration span.
                            let tool_span = if let Some(iteration) = &iteration_span {
                                let span = self.start_span(
                                    Some(&iteration.context()),
                                    &format!("execute_tool {}", call.name),
                                );
                                span.set_attribute(
                                    "gen_ai.operation.name",
                                    serde_json::json!("execute_tool"),
                                );
                                span.set_attribute(
                                    "gen_ai.tool.name",
                                    serde_json::json!(call.name),
                                );
                                span.set_attribute(
                                    "gen_ai.agent.name",
                                    serde_json::json!(AGENT_NAME),
                                );
                                span.set_attribute(
                                    "gen_ai.tool.call.id",
                                    serde_json::json!(call.id),
                                );
                                span.set_attribute(
                                    "gen_ai.tool.call.arguments",
                                    serde_json::json!(truncate_chars(
                                        &serde_json::to_string(&call.args).unwrap_or_default(),
                                        ARGS_MAX_CHARS,
                                    )),
                                );
                                Some(span)
                            } else {
                                None
                            };
                            // The PreTool phase used to check the token before
                            // its first hook; the inlined event sits in that
                            // slot, so the check moves here with it.
                            if cancel_here(&mut state, &spec) {
                                break 'outer;
                            }
                            // The call the model is attempting reaches the
                            // channel before any policy hook sees it.
                            spec.task_hook.fire_event(AgentEvent::ToolCall {
                                session_id: spec.session_id.clone(),
                                name: call.name.clone(),
                                args: call.args.to_string(),
                            });
                            let action = match self
                                .hooks
                                .run_pre_tool(&spec, &mut state, call)
                                .await
                            {
                                Ok(a) => a,
                                Err(e) => {
                                    absorb_hook_error(&mut state, &spec, e);
                                    break 'outer;
                                }
                            };
                            let raw_result = match action {
                                ToolHookAction::Execute => {
                                    match self
                                        .tool_manager
                                        .execute(&call.name, call.args.clone())
                                        .await
                                    {
                                        Ok(r) => r,
                                        Err(e) => {
                                            if let Some(span) = &tool_span {
                                                span.set_attribute(
                                                    "error.type",
                                                    serde_json::json!("tool_error"),
                                                );
                                                span.set_status(SpanStatus::Error(truncate_chars(
                                                    &e.to_string(),
                                                    ERROR_MAX_CHARS,
                                                )));
                                            }
                                            format_tool_error(&e.to_string())
                                        }
                                    }
                                }
                                ToolHookAction::Skip { result } => result,
                                ToolHookAction::Abort { reason } => {
                                    state.fail(reason);
                                    break 'outer;
                                }
                            };

                            // Same checkpoint, now guarding the result event.
                            if cancel_here(&mut state, &spec) {
                                break 'outer;
                            }
                            // The channel sees the result before the AfterTool
                            // policy hooks rewrite or persist it.
                            spec.task_hook.fire_event(AgentEvent::ToolResult {
                                session_id: spec.session_id.clone(),
                                name: call.name.clone(),
                                output: raw_result.clone(),
                            });

                            match self
                                .hooks
                                .run_after_tool(&spec, &mut state, call, &raw_result)
                                .await
                            {
                                Ok(HookAction::Continue) => {}
                                Ok(HookAction::Abort { reason }) => {
                                    state.fail(reason);
                                    break 'outer;
                                }
                                Err(e) => {
                                    absorb_hook_error(&mut state, &spec, e);
                                    break 'outer;
                                }
                            }
                        }

                        // This was the iteration's last cancellation checkpoint
                        // while PostIteration was a phase; keep it.
                        if cancel_here(&mut state, &spec) {
                            break;
                        }
                        // The iteration's tool work is done: tell the channel.
                        spec.task_hook.fire_event(AgentEvent::PostIteration {
                            session_id: spec.session_id.clone(),
                            iteration: state.iterations,
                        });
                        // A PreTool hook may end the turn without another LLM
                        // call (approval `Ask`): the tool results for this
                        // iteration are written, and the run result is the
                        // plain-text pending list the hook already set.
                        if state.should_exit {
                            break;
                        }
                        state.iterations += 1;
                        spec.task_hook.notify_status_change(&TaskState::Running {
                            current_iteration: state.iterations,
                        });
                    }
                }
            };
            let deadline_expired = if deadline_secs == 0 {
                body.await;
                false
            } else {
                tokio::time::timeout(std::time::Duration::from_secs(deadline_secs), body)
                    .await
                    .is_err()
            };

            if deadline_expired {
                // Expiry reuses the `/stop` cancellation path: cancel this run's
                // token and roll the turn back, so AfterTurn below persists or
                // consolidates exactly as it does for a user `/stop`.
                warn_log!(
                    "[AgentRunner] run deadline of {}s exceeded (session={}); cancelling",
                    deadline_secs,
                    session_id
                );
                if let Some(ct) = spec.cancel_token.as_ref() {
                    ct.cancel();
                }
                state.fail(format!(
                    "Task cancelled: run deadline of {deadline_secs}s exceeded."
                ));
                state.rollback_to = Some(spec.initial_message_count);
            }

            // Close the turn (persist or roll back, then report the final
            // state) before consolidation and the approval turn close, which
            // both need a settled session. Runs on every exit path.
            close_turn(&spec, &mut state).await;

            // AfterTurn hooks: consolidate, then close the approval turn.
            // Always executes, even on cancellation or error.
            self.hooks.run_after_turn(&spec, &mut state).await;

            debug!(
                "[AgentRunner] Run complete: success={}, iterations={}",
                state.result.success, state.result.iterations
            );

            let result = finalize(state.result);

            // Finalize the root span: aggregate usage, iteration count, and the
            // outcome status mapped from the run result (low-cardinality
            // error.type + detailed message in the status).
            if let Some(root) = &root_span {
                root.set_attribute(
                    "gen_ai.usage.input_tokens",
                    serde_json::json!(result.prompt_tokens),
                );
                root.set_attribute(
                    "gen_ai.usage.output_tokens",
                    serde_json::json!(result.completion_tokens),
                );
                root.set_attribute("iterations", serde_json::json!(result.iterations));
                root.set_attribute(
                    "gen_ai.conversation.compacted",
                    serde_json::json!(state.consolidated),
                );
                root.set_status(if result.success {
                    SpanStatus::Ok
                } else {
                    root.set_attribute(
                        "error.type",
                        serde_json::json!(classify_run_error(&result)),
                    );
                    SpanStatus::Error(result.content.clone())
                });
            }

            result
        })
        .await
    }

    /// Build a cancelled result (used for the /stop sentinel and pre-run cancellation).
    async fn cancelled_result(
        &self,
        hook: &TaskHook,
        session_id: &str,
        root_span: &Option<SpanGuard>,
    ) -> AgentResult {
        let msg = CANCELLED_MESSAGE.to_string();
        // Record the cancellation on the root span, matching the run status
        // mapping (cancellation -> Error).
        if let Some(root) = root_span {
            root.set_attribute("error.type", serde_json::json!("cancelled"));
            root.set_status(SpanStatus::Error(msg.clone()));
        }
        let result = AgentResult {
            success: false,
            content: msg.clone(),
            ..Default::default()
        };
        let failed_state = TaskState::Failed { error: msg.clone() };
        {
            let mut sm = self.session_manager.lock().await;
            let _ = sm.persist(session_id).await;
        }
        hook.notify_status_change(&failed_state);
        hook.fire_event(AgentEvent::TaskFailed {
            session_id: hook.session_id().to_string(),
            run_id: hook.run_id().to_string(),
            error: msg.clone(),
        });
        result
    }

    /// Start a child span under `parent`, or a cheap no-op guard when tracing
    /// is disabled (no tracer configured).
    fn start_span(&self, parent: Option<&crate::trace::SpanContext>, name: &str) -> SpanGuard {
        match &self.tracer {
            Some(tracer) => tracer.start_span(name, parent),
            None => SpanGuard::noop(),
        }
    }
}

impl AgentRunner {
    /// Drop tool result messages that have no matching assistant tool_call.
    fn drop_orphan_tool_results(messages: &mut Vec<Message>) {
        let mut declared: std::collections::HashSet<String> = std::collections::HashSet::new();
        let mut orphan_indices: Vec<usize> = Vec::new();
        for (i, msg) in messages.iter().enumerate() {
            match msg {
                Message::Assistant {
                    tool_calls: Some(calls),
                    ..
                } => {
                    for call in calls {
                        declared.insert(call.id.clone());
                    }
                }
                Message::Tool { tool_call_id, .. } if !declared.contains(tool_call_id) => {
                    orphan_indices.push(i);
                }
                _ => {}
            }
        }
        // Remove orphans in reverse order to preserve indices
        for i in orphan_indices.into_iter().rev() {
            messages.remove(i);
        }
    }

    /// Insert synthetic error Tool messages for assistant tool_calls that lack a result.
    fn backfill_missing_tool_results(messages: &mut Vec<Message>) {
        let backfill_content = "[Tool result unavailable — call was interrupted or lost]";

        // Collect all declared tool calls and fulfilled tool_call_ids
        let mut declared: Vec<(usize, String, String)> = Vec::new(); // (assistant_idx, call_id, tool_name)
        let mut fulfilled: std::collections::HashSet<String> = std::collections::HashSet::new();

        for (i, msg) in messages.iter().enumerate() {
            match msg {
                Message::Assistant {
                    tool_calls: Some(calls),
                    ..
                } => {
                    for call in calls {
                        declared.push((i, call.id.clone(), call.name.clone()));
                    }
                }
                Message::Tool { tool_call_id, .. } => {
                    fulfilled.insert(tool_call_id.clone());
                }
                _ => {}
            }
        }

        let missing: Vec<_> = declared
            .into_iter()
            .filter(|(_, id, _)| !fulfilled.contains(id))
            .collect();

        if missing.is_empty() {
            return;
        }

        // Group by assistant index, then insert each group in reverse to avoid index shifting.
        let mut by_assistant: std::collections::BTreeMap<usize, Vec<(String, String)>> =
            std::collections::BTreeMap::new();
        for (asst_idx, call_id, tool_name) in missing {
            by_assistant
                .entry(asst_idx)
                .or_default()
                .push((call_id, tool_name));
        }

        for (assistant_idx, calls) in by_assistant.into_iter().rev() {
            // Insert each call in reverse so they end up in original order
            for (call_id, tool_name) in calls.into_iter().rev() {
                messages.insert(
                    assistant_idx + 1,
                    Message::tool(backfill_content.to_string(), call_id, Some(tool_name)),
                );
            }
        }
    }
}

#[cfg(test)]
mod tests;
