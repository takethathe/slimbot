use std::path::PathBuf;
use std::sync::Arc;

use crate::config::AgentConfig;
use crate::consolidate::Consolidator;
use crate::memory::SharedMemory;
use crate::provider::{Provider, Usage};
use crate::session::{
    AgentEvent, Message, SharedSessionManager, TaskHook, TaskState, parse_session_origin,
};
use crate::tool::{ToolManager, TurnContext, format_tool_error, with_turn_context};
use crate::trace::{SpanGuard, SpanRecorder, SpanStatus, Tracer};
use crate::{debug, warn_log};
use tokio_util::sync::CancellationToken;

/// Agent name reported on GenAI spans via `gen_ai.agent.name`.
const AGENT_NAME: &str = "slimbot";

mod hooks;
mod registry;
mod traits;
mod types;

pub use registry::{HookRegistry, HookRunError};
pub use traits::{
    AfterChatHook, AfterToolHook, AfterTurnHook, PostIterationHook, PreChatHook, PreToolHook,
    PreTurnHook,
};
pub use types::{HookAction, HookPosition, RunSpec, RunState, ToolHookAction};

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

/// Map a failed run's message to a low-cardinality `error.type` per the
/// GenAI semantic conventions (fallback `_OTHER`).
fn classify_run_error(message: &str) -> &'static str {
    if message.contains("LLM API request failed") {
        "provider_error"
    } else if message.contains("Reached max iterations") {
        "max_iterations"
    } else if message.contains("Task cancelled") {
        "cancelled"
    } else {
        "_OTHER"
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
}

impl AgentResult {
    fn accumulate(&mut self, usage: &Usage) {
        self.total_tokens += usage.total_tokens;
        self.prompt_tokens += usage.prompt_tokens;
        self.prompt_cache_hit_tokens += usage.prompt_cache_hit_tokens;
        self.completion_tokens += usage.completion_tokens;
    }
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

    /// Register the stock hooks that replicate the current `run()` behavior.
    /// Returns self so it can be chained with the dependency setters.
    pub fn with_defaults(mut self) -> Self {
        use crate::runner::hooks::*;

        // PreTurn
        self.hooks
            .add_pre_turn(Arc::new(WriteUserMessageHook), HookPosition::Append);
        self.hooks
            .add_pre_turn(Arc::new(NotifyRunningHook), HookPosition::Append);
        // PreChat
        self.hooks
            .add_pre_chat(Arc::new(BuildContextHook), HookPosition::Append);
        self.hooks
            .add_pre_chat(Arc::new(GovernanceHook), HookPosition::Append);
        // AfterChat
        self.hooks
            .add_after_chat(Arc::new(HandleResponseHook), HookPosition::Append);
        self.hooks
            .add_after_chat(Arc::new(PersistAssistantMsgHook), HookPosition::Append);
        self.hooks
            .add_after_chat(Arc::new(AccumulateUsageHook), HookPosition::Append);
        // PreTool
        self.hooks
            .add_pre_tool(Arc::new(FireToolCallEventHook), HookPosition::Append);
        // AfterTool
        self.hooks
            .add_after_tool(Arc::new(FireToolResultEventHook), HookPosition::Append);
        self.hooks
            .add_after_tool(Arc::new(ProcessToolResultHook), HookPosition::Append);
        self.hooks
            .add_after_tool(Arc::new(PersistToolMessageHook), HookPosition::Append);
        // PostIteration
        self.hooks
            .add_post_iteration(Arc::new(FirePostIterationEventHook), HookPosition::Append);
        // AfterTurn
        self.hooks
            .add_after_turn(Arc::new(FireTaskCompletedHook), HookPosition::Append);
        self.hooks
            .add_after_turn(Arc::new(TriggerConsolidationHook), HookPosition::Append);
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

    /// Append a hook to the PostIteration phase.
    pub fn add_post_iteration(
        mut self,
        hook: Arc<dyn PostIterationHook>,
        pos: HookPosition,
    ) -> Self {
        self.hooks.add_post_iteration(hook, pos);
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
    hooks: HookRegistry,
}

impl AgentRunner {
    /// Create a new builder for constructing an `AgentRunner`.
    pub fn builder() -> AgentRunnerBuilder {
        AgentRunnerBuilder::new()
    }

    /// Construct a runner with the stock default hooks.
    ///
    /// Existing callers that use `AgentRunner::new` keep working unchanged;
    /// they automatically get the default hook pipeline.
    //
    // The 8-argument signature is part of the public API (see
    // .scratch/runner-phase-hooks/issues/03: "keep AgentRunner::new()
    // signature"); a `RunSpec`-style struct would change every call site.
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
    // The 8-argument signature is part of the public API used by AgentLoop
    // and the gateway; grouped params live in `RunSpec` internally.
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

        let spec = RunSpec {
            session_id: session_id.to_string(),
            content,
            task_hook: hook,
            max_iterations: self.config.max_iterations,
            max_tokens: self.max_tokens,
            cancel_token: Some(run_cancel_token),
            workspace_dir: self.workspace_dir.clone(),
            session_manager: self.session_manager.clone(),
            tool_manager: self.tool_manager.clone(),
            config: self.config.clone(),
            memory_service: self.memory_service.clone(),
            consolidator: self.consolidator.clone(),
            tracer: self.tracer.clone(),
            initial_message_count,
            channel: tool_channel.clone(),
            chat_id: tool_chat_id.clone(),
            runtime_ctx,
        };

        // Scope per-turn state (context + sent_in_turn) via a task-local so that
        // concurrent runs sharing one Arc<ToolManager> each see their own state.
        let turn = Arc::new(TurnContext::new(tool_channel, tool_chat_id));
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
            let deadline_secs = self.config.timeout_seconds;
            let body = async {
                // PreTurn hooks: write the user message and notify the running state.
                // On abort/error/cancel the entire loop is skipped and we jump to AfterTurn.
                let pre_turn_ok = match self.hooks.run_pre_turn(&spec, &mut state).await {
                    Ok(HookAction::Continue) => true,
                    Ok(HookAction::Abort { reason }) => {
                        state.fail(reason);
                        false
                    }
                    Err(HookRunError::Cancelled) => {
                        state.fail("Task cancelled.".to_string());
                        state.rollback_to = Some(spec.initial_message_count);
                        false
                    }
                    Err(HookRunError::Hook(e)) => {
                        state.fail(e.to_string());
                        false
                    }
                };

                if pre_turn_ok {
                    'outer: loop {
                        // Exceeded max iterations.
                        if state.iterations >= spec.max_iterations {
                            let err_msg = format!("Reached max iterations {}", spec.max_iterations);
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
                            Err(HookRunError::Cancelled) => {
                                state.fail("Task cancelled.".to_string());
                                state.rollback_to = Some(spec.initial_message_count);
                                break;
                            }
                            Err(HookRunError::Hook(e)) => {
                                state.fail(e.to_string());
                                break;
                            }
                        }

                        // Request model. Cancellation may arrive after the last PreChat
                        // hook; check here so a cancelled run skips the LLM call and
                        // rolls back instead of firing a wasted request.
                        if spec.is_cancelled() {
                            state.fail("Task cancelled.".to_string());
                            state.rollback_to = Some(spec.initial_message_count);
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
                            Err(e) => {
                                if let Some(span) = &chat_span {
                                    span.set_attribute(
                                        "error.type",
                                        serde_json::json!("provider_error"),
                                    );
                                    span.set_status(SpanStatus::Error(e.to_string()));
                                }
                                let err_msg = format!("LLM API request failed - {}", e);
                                warn_log!("[AgentRunner] {}", err_msg);
                                state.fail(err_msg);
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

                        // AfterChat hooks: inspect response, persist assistant
                        // message, accumulate usage.
                        match self.hooks.run_after_chat(&spec, &mut state).await {
                            Ok(HookAction::Continue) => {}
                            Ok(HookAction::Abort { reason }) => {
                                state.fail(reason);
                                break;
                            }
                            Err(HookRunError::Cancelled) => {
                                state.fail("Task cancelled.".to_string());
                                state.rollback_to = Some(spec.initial_message_count);
                                break;
                            }
                            Err(HookRunError::Hook(e)) => {
                                state.fail(e.to_string());
                                break;
                            }
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
                            let action = match self
                                .hooks
                                .run_pre_tool(&spec, &mut state, call)
                                .await
                            {
                                Ok(a) => a,
                                Err(HookRunError::Cancelled) => {
                                    state.fail("Task cancelled.".to_string());
                                    state.rollback_to = Some(spec.initial_message_count);
                                    break 'outer;
                                }
                                Err(HookRunError::Hook(e)) => {
                                    state.fail(e.to_string());
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
                                Err(HookRunError::Cancelled) => {
                                    state.fail("Task cancelled.".to_string());
                                    state.rollback_to = Some(spec.initial_message_count);
                                    break 'outer;
                                }
                                Err(HookRunError::Hook(e)) => {
                                    state.fail(e.to_string());
                                    break 'outer;
                                }
                            }
                        }

                        // PostIteration hooks: fire the PostIteration event.
                        match self.hooks.run_post_iteration(&spec, &mut state).await {
                            Ok(HookAction::Continue) => {}
                            Ok(HookAction::Abort { reason }) => {
                                state.fail(reason);
                                break;
                            }
                            Err(HookRunError::Cancelled) => {
                                state.fail("Task cancelled.".to_string());
                                state.rollback_to = Some(spec.initial_message_count);
                                break;
                            }
                            Err(HookRunError::Hook(e)) => {
                                state.fail(e.to_string());
                                break;
                            }
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

            // AfterTurn hooks: persist/rollback + fire completed/failed + consolidate.
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
                        serde_json::json!(classify_run_error(&result.content)),
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
        let msg = "Task cancelled.".to_string();
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
