//! The single enforcement point for tool risk (ADR 0011).
//!
//! Three hooks, one lifecycle:
//!
//! - [`ApprovalTurnStartHook`] (PreTurn) opens the policy turn: records past
//!   their TTL expire here, and only what was already pending may be redeemed.
//! - [`ApprovalHook`] (PreTool) is the only decision point. `Allow` executes,
//!   `Deny` returns a fixed refusal and the run continues, `Ask` aborts the
//!   call, records a pending approval, answers the tool call with guidance text
//!   and ends the turn with the plain-text pending list.
//! - [`ApprovalTurnEndHook`] (AfterTurn) closes the policy turn: carried records
//!   that were never redeemed are cleared.
//!
//! [`ApprovalContextHook`] (PreChat) applies the build-time transform that folds
//! a redeemed round onto its original call and rewrites unredeemed guidance to
//! failure text. Nothing here ever writes a decision into the session: the
//! audit goes to the trace `approval` span.
//!
//! The decision itself lives in [`crate::runner::gate`], shared verbatim with
//! the codemode agent entry (ticket 25) so a nested `tools.*` call meets the
//! same policy as a top-level one. This file keeps only the `RunState` side
//! effects: marking the tool message, ending the turn, answering the hook.

use anyhow::Result;
use async_trait::async_trait;

use crate::approval::{PendingApproval, merge_approval_rounds};
use crate::runner::gate::{GateContext, GateDecision};
use crate::runner::traits::{AfterTurnHook, PreChatHook, PreToolHook, PreTurnHook};
use crate::runner::types::{HookAction, RunSpec, RunState, ToolHookAction};
use crate::tool::ToolCall;

/// Run-state extension key: `HashMap<tool_call_id, approval_id>` for the calls
/// this run turned into pending approvals. `PersistToolMessageHook` reads it to
/// mark the guidance tool result, which is what the build-time transform keys on.
pub(crate) const MARKS_KEY: &str = "approval.marks";

/// Mark the tool result of `call_id` as approval-related.
pub(crate) fn mark_call(state: &mut RunState, call_id: &str, approval_id: &str) {
    let mut marks = state
        .get_extension::<std::collections::HashMap<String, String>>(MARKS_KEY)
        .cloned()
        .unwrap_or_default();
    marks.insert(call_id.to_string(), approval_id.to_string());
    state.set_extension(MARKS_KEY, marks);
}

/// The approval marker attached to the tool result of `call_id`, if any.
pub(crate) fn mark_for(state: &RunState, call_id: &str) -> Option<String> {
    state
        .get_extension::<std::collections::HashMap<String, String>>(MARKS_KEY)
        .and_then(|marks| marks.get(call_id).cloned())
}

/// Write one audit event to the trace `approval` span (see `runner::gate`).
fn audit(spec: &RunSpec, decision: &str, record: &PendingApproval) {
    crate::runner::gate::audit(spec.tracer.as_ref(), decision, record);
}

/// Snapshot the run's gate inputs; cheap Arc clones, no `RunState` access.
pub(crate) fn gate_context(spec: &RunSpec) -> GateContext {
    GateContext {
        session_id: spec.session_id.clone(),
        task_hook: spec.task_hook.clone(),
        tool_manager: spec.tool_manager.clone(),
        approvals: spec.approvals.clone(),
        tracer: spec.tracer.clone(),
        cancel_token: spec.cancel_token.clone(),
    }
}

/// Open the policy turn: expire past-TTL records, then arm redemption.
pub struct ApprovalTurnStartHook;

#[async_trait]
impl PreTurnHook for ApprovalTurnStartHook {
    async fn call(&self, spec: &RunSpec, _state: &mut RunState) -> Result<HookAction> {
        for record in spec.approvals.begin_turn(&spec.session_id) {
            audit(spec, "expired", &record);
        }
        Ok(HookAction::Continue)
    }
}

/// Close the policy turn: whatever was carried and not redeemed rolls off.
pub struct ApprovalTurnEndHook;

#[async_trait]
impl AfterTurnHook for ApprovalTurnEndHook {
    async fn call(&self, spec: &RunSpec, _state: &mut RunState) -> Result<()> {
        for record in spec.approvals.end_turn(&spec.session_id) {
            audit(spec, "cleared", &record);
        }
        Ok(())
    }
}

/// Apply the build-time approval transform to the provider-bound messages.
pub struct ApprovalContextHook;

#[async_trait]
impl PreChatHook for ApprovalContextHook {
    async fn call(&self, spec: &RunSpec, state: &mut RunState) -> Result<HookAction> {
        let live = spec.approvals.live_ids(&spec.session_id);
        let messages = std::mem::take(&mut state.messages);
        state.messages = merge_approval_rounds(&messages, &live);
        Ok(HookAction::Continue)
    }
}

/// The only place a top-level tool call is allowed, refused or deferred.
pub struct ApprovalHook;

#[async_trait]
impl PreToolHook for ApprovalHook {
    async fn call(
        &self,
        spec: &RunSpec,
        state: &mut RunState,
        tool_call: &ToolCall,
    ) -> Result<ToolHookAction> {
        let ctx = gate_context(spec);
        match crate::runner::gate::decide(&ctx, tool_call).await {
            GateDecision::Execute => Ok(ToolHookAction::Execute),
            GateDecision::Refuse { result } => Ok(ToolHookAction::Skip { result }),
            GateDecision::Cancelled { reason } => Ok(ToolHookAction::Abort { reason }),
            GateDecision::Pending {
                result,
                approval_id,
            } => {
                mark_call(state, &tool_call.id, &approval_id);

                // End the turn: the owner reads a plain-text list, and the same
                // call replays next turn to redeem it.
                state.should_exit = true;
                state.result.success = true;
                state.result.iterations = state.iterations;
                state.result.content = spec.approvals.pending_list_text(&spec.session_id);
                Ok(ToolHookAction::Skip { result })
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};

    use serde_json::json;
    use tokio::sync::broadcast;

    use super::*;
    use crate::approval::{
        ApprovalConfig, ApprovalStore, ControlLevel, DEFAULT_APPROVAL_TIMEOUT_SECONDS, RiskLevel,
        args_hash,
    };
    use crate::memory::MemoryService;
    use crate::provider::{FinishReason, LLMResponse, Provider, Usage};
    use crate::runner::AgentRunnerBuilder;
    use crate::session::{
        AgentEvent, Message, SessionManager, SharedSessionManager, TaskHook, TurnKind,
    };
    use crate::tool::{Tool, ToolDefinition, ToolManager, TurnContext};
    use crate::trace::Tracer;

    /// Tool whose risk class and side effects the test controls.
    struct WireTool {
        name: &'static str,
        risk: RiskLevel,
        runs: Arc<AtomicUsize>,
        /// Padded result length, so a test can fill the context window.
        body_chars: usize,
    }

    #[async_trait]
    impl Tool for WireTool {
        fn name(&self) -> &str {
            self.name
        }
        fn description(&self) -> &str {
            "test tool"
        }
        fn parameters(&self) -> serde_json::Value {
            json!({})
        }
        fn risk_level(&self) -> RiskLevel {
            self.risk
        }
        async fn execute(&self, args: serde_json::Value) -> Result<String> {
            self.runs.fetch_add(1, Ordering::SeqCst);
            let mut out = format!("ran {args}");
            while out.chars().count() < self.body_chars {
                out.push('·');
            }
            Ok(out)
        }
    }

    struct Harness {
        _tmp: tempfile::TempDir,
        spec: RunSpec,
        state: RunState,
        runs: Arc<AtomicUsize>,
        store: Arc<ApprovalStore>,
        events: broadcast::Receiver<AgentEvent>,
        trace_path: PathBuf,
    }

    impl Harness {
        fn new(config: ApprovalConfig, kind: TurnKind) -> Self {
            Self::with_clock(config, kind, Arc::new(AtomicU64::new(1_000)))
        }

        fn with_clock(config: ApprovalConfig, kind: TurnKind, now: Arc<AtomicU64>) -> Self {
            let tmp = tempfile::TempDir::new().unwrap();
            let workspace = tmp.path().join("workspace");
            std::fs::create_dir_all(&workspace).unwrap();
            let trace_path = tmp.path().join("trace.jsonl");

            let runs = Arc::new(AtomicUsize::new(0));
            let mut tools = ToolManager::new(workspace.clone());
            tools.register(Box::new(WireTool {
                name: "shell",
                risk: RiskLevel::Exec,
                runs: runs.clone(),
                body_chars: 0,
            }));
            tools.register(Box::new(WireTool {
                name: "publish",
                risk: RiskLevel::ExternalSend,
                runs: runs.clone(),
                body_chars: 0,
            }));
            tools.register(Box::new(WireTool {
                name: "read",
                risk: RiskLevel::ReadOnly,
                runs: runs.clone(),
                body_chars: 900,
            }));

            let store = Arc::new(ApprovalStore::with_clock(
                config,
                Arc::new(move || now.load(Ordering::SeqCst)),
            ));
            let session_manager: SharedSessionManager = Arc::new(tokio::sync::Mutex::new(
                SessionManager::new(tmp.path().join("sessions")).unwrap(),
            ));
            let (event_tx, events) = broadcast::channel(64);
            let tracer = Tracer::new(vec![Arc::new(
                crate::trace::backends::FileBackend::new(trace_path.to_str().unwrap()).unwrap(),
            )]);

            let spec = RunSpec {
                session_id: "s1".into(),
                content: "hi".into(),
                task_hook: TaskHook::new("s1")
                    .with_events(event_tx)
                    .with_turn_kind(kind),
                max_iterations: 10,
                max_tokens: 1024,
                cancel_token: None,
                workspace_dir: workspace.clone(),
                session_manager,
                tool_manager: Arc::new(tools),
                config: crate::config::AgentConfig::default(),
                pressure: Default::default(),
                memory_service: Arc::new(tokio::sync::Mutex::new(MemoryService::new(&workspace))),
                consolidator: None,
                tracer: Some(Arc::new(tracer)),
                initial_message_count: 0,
                channel: "cli".into(),
                chat_id: "c1".into(),
                runtime_ctx: "ctx".into(),
                approvals: store.clone(),
            };
            let state = RunState::new(Arc::new(TurnContext::new("cli".into(), "c1".into())));
            Self {
                _tmp: tmp,
                spec,
                state,
                runs,
                store,
                events,
                trace_path,
            }
        }

        fn call(&self, id: &str, tool: &str, args: serde_json::Value) -> ToolCall {
            ToolCall {
                id: id.into(),
                name: tool.into(),
                args,
            }
        }

        async fn decide(&mut self, call: &ToolCall) -> ToolHookAction {
            ApprovalHook
                .call(&self.spec, &mut self.state, call)
                .await
                .unwrap()
        }

        fn runs(&self) -> usize {
            self.runs.load(Ordering::SeqCst)
        }

        /// Decisions recorded on `approval` spans, in order.
        fn audit_decisions(&self) -> Vec<String> {
            let text = std::fs::read_to_string(&self.trace_path).unwrap_or_default();
            text.lines()
                .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
                .filter(|span| span["name"] == json!("approval"))
                .filter_map(|span| {
                    span["attributes"]["approval.decision"]
                        .as_str()
                        .map(|s| s.to_string())
                })
                .collect()
        }
    }

    // ── policy decisions ──

    #[tokio::test]
    async fn test_approval_hook_allow_executes_without_any_record() {
        let mut h = Harness::new(ApprovalConfig::default(), TurnKind::User);
        let call = h.call("tc-1", "read_nothing", json!({}));
        // An unregistered tool has no declared risk and cannot execute.
        assert_eq!(h.decide(&call).await, ToolHookAction::Execute);

        // Read-only class defaults to `allow`.
        let mut h = Harness::new(ApprovalConfig::default(), TurnKind::User);
        let call = h.call("tc-1", "wire_read", json!({}));
        assert_eq!(h.decide(&call).await, ToolHookAction::Execute);
        assert!(!h.store.has_pending("s1"));
        assert!(h.audit_decisions().is_empty(), "allow writes no audit span");
    }

    #[tokio::test]
    async fn test_approval_hook_deny_returns_refusal_and_never_executes() {
        let mut config = ApprovalConfig::default();
        config.tools.insert("shell".into(), ControlLevel::Deny);
        let mut h = Harness::new(config, TurnKind::User);

        let call = h.call("tc-1", "shell", json!({"command": "rm -rf /"}));
        match h.decide(&call).await {
            ToolHookAction::Skip { result } => {
                assert!(result.starts_with("DENIED"), "got: {result}");
                assert!(result.contains("shell"), "got: {result}");
                assert!(
                    result.contains("Do not retry it with different phrasing"),
                    "deny carries the no-rephrase nudge: {result}"
                );
            }
            other => panic!("expected Skip, got {other:?}"),
        }
        // The run is not ended by a denial: no turn-exit, no pending list.
        assert!(!h.state.should_exit);
        assert!(!h.store.has_pending("s1"));
        assert_eq!(h.audit_decisions(), vec!["denied_by_policy"]);
    }

    #[tokio::test]
    async fn test_approval_hook_ask_proposes_and_ends_turn_with_pending_list() {
        let mut h = Harness::new(ApprovalConfig::default(), TurnKind::User);
        let call = h.call("tc-1", "shell", json!({"command": "make deploy"}));

        match h.decide(&call).await {
            ToolHookAction::Skip { result } => {
                assert!(result.starts_with("NOT EXECUTED"), "got: {result}");
                assert!(result.contains("make deploy"), "args travel with it");
                assert!(result.contains("Approval id"), "got: {result}");
            }
            other => panic!("expected Skip, got {other:?}"),
        }

        // The turn ends with the plain-text list, not with a tool execution.
        assert!(h.state.should_exit);
        assert!(h.state.result.success);
        let list = h.state.result.content.clone();
        assert!(list.starts_with("Pending approval"), "got: {list}");
        assert!(list.contains("shell"), "got: {list}");
        assert!(list.contains("make deploy"), "the exact args are shown");
        assert!(!list.contains("button"), "plain text, no affordances");

        // The record holds the full args plus their hash, and the call is marked
        // so the persisted guidance result carries the approval id.
        let pending = h.store.pending_for("s1");
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].args, json!({"command": "make deploy"}));
        assert_eq!(pending[0].args_hash, args_hash(&pending[0].args));
        assert_eq!(pending[0].risk, RiskLevel::Exec);
        assert_eq!(pending[0].turn_kind, TurnKind::User);
        assert_eq!(pending[0].run_id, h.spec.task_hook.run_id());
        assert_eq!(
            mark_for(&h.state, "tc-1").as_deref(),
            Some(pending[0].approval_id.as_str())
        );
        assert_eq!(h.runs(), 0);
        assert_eq!(h.audit_decisions(), vec!["proposed"]);
    }

    #[tokio::test]
    async fn test_approval_hook_unattended_ask_is_denied_without_a_record() {
        for kind in [
            TurnKind::Cron,
            TurnKind::Heartbeat,
            TurnKind::Trigger,
            TurnKind::Subagent,
        ] {
            let mut h = Harness::new(ApprovalConfig::default(), kind);
            let call = h.call("tc-1", "shell", json!({"command": "touch /tmp/x"}));
            match h.decide(&call).await {
                ToolHookAction::Skip { result } => {
                    assert!(result.starts_with("DENIED"), "{kind:?}: {result}");
                    assert!(
                        result.contains("needs owner authorization"),
                        "{kind:?}: {result}"
                    );
                }
                other => panic!("{kind:?}: expected Skip, got {other:?}"),
            }
            assert_eq!(
                h.store.pending_for("s1").len(),
                0,
                "{kind:?} must never write a pending list"
            );
            assert!(!h.state.should_exit, "{kind:?} keeps the run going");
            assert_eq!(h.audit_decisions(), vec!["denied_by_policy"]);
        }
    }

    // ── redemption ──

    /// Propose `shell make deploy` on one turn, then return the store so the
    /// next turn can redeem it.
    async fn propose(h: &mut Harness) -> PendingApproval {
        let call = h.call("tc-1", "shell", json!({"command": "make deploy"}));
        h.decide(&call).await;
        h.spec.approvals.pending_for("s1")[0].clone()
    }

    #[tokio::test]
    async fn test_approval_hook_same_turn_replay_does_not_redeem() {
        // Two identical calls inside one turn (a tool-call batch, or a codemode
        // script retrying) must not approve themselves: the proposal belongs to
        // the *next* turn's replay.
        let mut h = Harness::new(ApprovalConfig::default(), TurnKind::User);
        propose(&mut h).await;

        let replay = h.call("tc-9", "shell", json!({"command": "make deploy"}));
        match h.decide(&replay).await {
            ToolHookAction::Skip { result } => {
                assert!(result.starts_with("NOT EXECUTED"), "{result}");
            }
            other => panic!("expected a second pending Skip, got {other:?}"),
        }
        assert_eq!(h.runs(), 0, "the tool never ran");
        assert_eq!(
            h.store.pending_for("s1").len(),
            1,
            "the duplicate proposal collapses onto the same record"
        );
    }

    #[tokio::test]
    async fn test_approval_hook_replaying_the_same_call_redeems_it() {
        let mut h = Harness::new(ApprovalConfig::default(), TurnKind::User);
        let record = propose(&mut h).await;

        // Next turn opens, then the model replays the exact same call.
        h.spec.approvals.begin_turn("s1");
        let replay = h.call("tc-9", "shell", json!({"command": "make deploy"}));
        assert_eq!(h.decide(&replay).await, ToolHookAction::Execute);

        assert_eq!(
            h.runs(),
            0,
            "the hook decides; the executor does the running"
        );
        assert!(
            h.spec.approvals.pending_for("s1").is_empty(),
            "a redeemed record is consumed"
        );
        assert_eq!(h.audit_decisions(), vec!["proposed", "redeemed"]);
        assert_eq!(
            h.store.pending_for("s1").len(),
            0,
            "no record may be redeemed twice"
        );
        // The broadcast names the approval that is about to run.
        let event = h.events.try_recv().unwrap();
        match event {
            AgentEvent::ApprovalRedeemed {
                approval_id,
                tool,
                args,
                risk,
                ..
            } => {
                assert_eq!(approval_id, record.approval_id);
                assert_eq!(tool, "shell");
                assert!(args.contains("make deploy"));
                assert_eq!(risk, "exec");
            }
            other => panic!("expected ApprovalRedeemed, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn test_approval_hook_replay_with_other_args_vetoes_and_deletes() {
        let mut h = Harness::new(ApprovalConfig::default(), TurnKind::User);
        propose(&mut h).await;

        h.spec.approvals.begin_turn("s1");
        let replay = h.call("tc-9", "shell", json!({"command": "make destroy"}));
        match h.decide(&replay).await {
            ToolHookAction::Skip { result } => {
                assert!(result.starts_with("DENIED"), "got: {result}");
                assert!(
                    result.contains("does not match the pending approval"),
                    "got: {result}"
                );
            }
            other => panic!("expected Skip, got {other:?}"),
        }
        assert!(
            h.spec.approvals.pending_for("s1").is_empty(),
            "a mismatch deletes the record instead of executing anything"
        );
        assert_eq!(h.audit_decisions(), vec!["proposed", "denied_by_policy"]);
    }

    #[tokio::test]
    async fn test_approval_hook_expired_record_cannot_be_redeemed() {
        let now = Arc::new(AtomicU64::new(1_000));
        let mut h = Harness::with_clock(ApprovalConfig::default(), TurnKind::User, now.clone());
        propose(&mut h).await;

        // Past the TTL: the next turn opening expires it before anything reads it.
        now.store(
            1_000 + DEFAULT_APPROVAL_TIMEOUT_SECONDS * 1000 + 1,
            Ordering::SeqCst,
        );
        let expired = h.spec.approvals.begin_turn("s1");
        assert_eq!(expired.len(), 1);

        let replay = h.call("tc-9", "shell", json!({"command": "make deploy"}));
        match h.decide(&replay).await {
            // No live record: the policy applies again and proposes a new round.
            ToolHookAction::Skip { result } => assert!(result.starts_with("NOT EXECUTED")),
            other => panic!("expected Skip, got {other:?}"),
        }
        assert_eq!(h.store.pending_for("s1").len(), 1, "a fresh proposal");
        assert_ne!(
            h.store.pending_for("s1")[0].created_ms,
            1_000,
            "the new record is stamped now, not with the expired timestamp"
        );
    }

    #[tokio::test]
    async fn test_approval_hook_stop_in_the_redeem_window_prevents_the_call() {
        let mut h = Harness::new(ApprovalConfig::default(), TurnKind::User);
        propose(&mut h).await;
        h.spec.approvals.begin_turn("s1");

        // The owner reacts to the broadcast by pressing /stop.
        let mut events = h.events.resubscribe();
        let cancel = tokio_util::sync::CancellationToken::new();
        let token = cancel.clone();
        tokio::spawn(async move {
            if let Ok(AgentEvent::ApprovalRedeemed { .. }) = events.recv().await {
                token.cancel();
            }
        });
        h.spec.cancel_token = Some(cancel);

        let replay = h.call("tc-9", "shell", json!({"command": "make deploy"}));
        match h.decide(&replay).await {
            ToolHookAction::Abort { reason } => {
                assert!(
                    reason.contains("stopped") || reason.contains("/stop"),
                    "{reason}"
                );
            }
            other => panic!("expected Abort, got {other:?}"),
        }
        assert_eq!(h.runs(), 0, "/stop in the window prevents execution");
    }

    #[tokio::test]
    async fn test_approval_hook_turn_end_clears_unredeemed_records() {
        let mut h = Harness::new(ApprovalConfig::default(), TurnKind::User);
        h.spec.approvals.begin_turn("s1");
        propose(&mut h).await;

        // Proposed this turn: it rolls over, it does not vanish.
        ApprovalTurnEndHook
            .call(&h.spec, &mut h.state)
            .await
            .unwrap();
        assert_eq!(h.store.pending_for("s1").len(), 1);
        assert!(!h.audit_decisions().contains(&"cleared".to_string()));

        // Carried and not redeemed: it is cleared at turn end.
        ApprovalTurnStartHook
            .call(&h.spec, &mut h.state)
            .await
            .unwrap();
        ApprovalTurnEndHook
            .call(&h.spec, &mut h.state)
            .await
            .unwrap();
        assert!(h.store.pending_for("s1").is_empty());
        assert_eq!(h.audit_decisions(), vec!["proposed", "cleared"]);
    }

    // ── runner seam ──

    /// Scripted provider that also records the message list it was handed.
    struct ScriptedProvider {
        responses: Mutex<Vec<LLMResponse>>,
        seen: Mutex<Vec<Vec<Message>>>,
    }

    impl ScriptedProvider {
        fn new(responses: Vec<LLMResponse>) -> Self {
            Self {
                responses: Mutex::new(responses),
                seen: Mutex::new(Vec::new()),
            }
        }

        fn last_messages(&self) -> Vec<Message> {
            self.seen
                .lock()
                .unwrap()
                .last()
                .cloned()
                .unwrap_or_default()
        }
    }

    #[async_trait]
    impl Provider for ScriptedProvider {
        async fn chat(
            &self,
            messages: &[&Message],
            _tools: Option<&[ToolDefinition]>,
            _recorder: Option<&dyn crate::trace::SpanRecorder>,
        ) -> Result<LLMResponse> {
            self.seen
                .lock()
                .unwrap()
                .push(messages.iter().map(|m| (*m).clone()).collect());
            let mut responses = self.responses.lock().unwrap();
            assert!(
                !responses.is_empty(),
                "provider ran out of scripted answers"
            );
            Ok(responses.remove(0))
        }
    }

    fn test_agent_config() -> crate::config::AgentConfig {
        crate::config::AgentConfig {
            provider: "test".into(),
            max_iterations: 10,
            timeout_seconds: 120,
            persist_tool_results: false,
            unknown: Default::default(),
        }
    }

    fn tool_call_response(id: &str, tool: &str, args: serde_json::Value) -> LLMResponse {
        LLMResponse {
            content: Some("calling".into()),
            tool_calls: Some(vec![ToolCall {
                id: id.into(),
                name: tool.into(),
                args,
            }]),
            finish_reason: FinishReason::ToolCalls,
            usage: Usage::default(),
        }
    }

    fn final_response(text: &str) -> LLMResponse {
        LLMResponse {
            content: Some(text.into()),
            tool_calls: None,
            finish_reason: FinishReason::Stop,
            usage: Usage::default(),
        }
    }

    struct RunnerHarness {
        runner: Option<crate::runner::AgentRunner>,
        provider: Arc<ScriptedProvider>,
        store: Arc<ApprovalStore>,
        session_manager: SharedSessionManager,
        _tmp: tempfile::TempDir,
        trace_path: PathBuf,
        runs: Arc<AtomicUsize>,
    }

    async fn runner_harness(
        responses: Vec<LLMResponse>,
        store: Arc<ApprovalStore>,
        pressure: crate::pressure::PressureAnchor,
    ) -> RunnerHarness {
        let tmp = tempfile::TempDir::new().unwrap();
        let workspace = tmp.path().join("workspace");
        std::fs::create_dir_all(&workspace).unwrap();
        let trace_path = tmp.path().join("trace.jsonl");

        let runs = Arc::new(AtomicUsize::new(0));
        let mut tools = ToolManager::new(workspace.clone());
        tools.register(Box::new(WireTool {
            name: "shell",
            risk: RiskLevel::Exec,
            runs: runs.clone(),
            body_chars: 0,
        }));
        tools.register(Box::new(WireTool {
            name: "read",
            risk: RiskLevel::ReadOnly,
            runs: runs.clone(),
            body_chars: 900,
        }));

        let session_manager: SharedSessionManager = Arc::new(tokio::sync::Mutex::new(
            SessionManager::new(tmp.path().join("sessions")).unwrap(),
        ));
        // The session exists before the first run, as it does in production.
        session_manager
            .lock()
            .await
            .get_or_create("s1")
            .await
            .unwrap();

        let provider = Arc::new(ScriptedProvider::new(responses));
        let tracer = Tracer::new(vec![Arc::new(
            crate::trace::backends::FileBackend::new(trace_path.to_str().unwrap()).unwrap(),
        )]);

        let runner = AgentRunnerBuilder::new()
            .session_manager(session_manager.clone())
            .tool_manager(Arc::new(tools))
            .provider(provider.clone())
            .config(test_agent_config())
            .workspace_dir(workspace.clone())
            .memory_service(Arc::new(tokio::sync::Mutex::new(MemoryService::new(
                &workspace,
            ))))
            .max_tokens(1024)
            .pressure(pressure)
            .tracer(Some(Arc::new(tracer)))
            .approvals(store.clone())
            .with_defaults()
            .build();

        RunnerHarness {
            runner: Some(runner),
            provider,
            store,
            session_manager,
            _tmp: tmp,
            trace_path,
            runs,
        }
    }

    impl RunnerHarness {
        async fn run(&mut self, content: &str, kind: TurnKind) -> crate::runner::AgentResult {
            let runner = self.runner.take().expect("runner");
            let (event_tx, _events) = broadcast::channel(64);
            let hook = TaskHook::new("s1")
                .with_events(event_tx)
                .with_turn_kind(kind);
            let result = runner
                .run(content.to_string(), hook, "s1", None, None, None, None)
                .await;
            self.runner = Some(runner);
            result
        }

        /// Simulate a process restart: the pending store is rebuilt empty
        /// because nothing about approvals is ever persisted.
        fn restart(&mut self, store: Arc<ApprovalStore>) {
            let runner = self.runner.take().expect("runner");
            self.runner = Some(runner.with_approvals(store.clone()));
            self.store = store;
        }

        async fn messages(&self) -> Vec<Message> {
            self.session_manager.lock().await.get_messages("s1").await
        }

        fn audit_decisions(&self) -> Vec<String> {
            let text = std::fs::read_to_string(&self.trace_path).unwrap_or_default();
            text.lines()
                .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
                .filter(|span| span["name"] == json!("approval"))
                .filter_map(|span| {
                    span["attributes"]["approval.decision"]
                        .as_str()
                        .map(|s| s.to_string())
                })
                .collect()
        }
    }

    #[tokio::test]
    async fn test_runner_ask_ends_the_turn_with_the_pending_list() {
        let store = Arc::new(ApprovalStore::default());
        let mut h = runner_harness(
            vec![
                tool_call_response("tc-1", "shell", json!({"command": "deploy"})),
                final_response("unused"),
            ],
            store.clone(),
            Default::default(),
        )
        .await;

        let result = h.run("deploy it", TurnKind::User).await;

        assert!(result.success, "got: {:?}", result.content);
        assert_eq!(
            result.iterations, 0,
            "no second model call: the turn ends on the proposal"
        );
        assert!(
            result.content.starts_with("Pending approval"),
            "{}",
            result.content
        );
        assert!(result.content.contains("deploy"));
        assert_eq!(h.provider.seen.lock().unwrap().len(), 1);
        assert_eq!(h.runs.load(Ordering::SeqCst), 0, "nothing executed");

        // The guidance tool result is persisted and marked with the approval id.
        let messages = h.messages().await;
        let marked: Vec<&Message> = messages
            .iter()
            .filter(|m| m.approval_id().is_some())
            .collect();
        assert_eq!(marked.len(), 1, "exactly one marked guidance result");
        assert!(
            matches!(marked[0], Message::Tool { content, .. } if content.starts_with("NOT EXECUTED"))
        );
        assert_eq!(store.pending_for("s1").len(), 1);
    }

    #[tokio::test]
    async fn test_runner_deny_keeps_the_run_going() {
        let mut config = ApprovalConfig::default();
        config.tools.insert("shell".into(), ControlLevel::Deny);
        let store = Arc::new(ApprovalStore::new(config));
        let mut h = runner_harness(
            vec![
                tool_call_response("tc-1", "shell", json!({"command": "deploy"})),
                final_response("I could not deploy; the action was refused."),
            ],
            store.clone(),
            Default::default(),
        )
        .await;

        let result = h.run("deploy it", TurnKind::User).await;

        assert!(result.success, "run failed: {}", result.content);
        assert_eq!(
            result.iterations, 1,
            "a denial does not end the turn: the model gets to answer"
        );
        assert_eq!(
            result.content,
            "I could not deploy; the action was refused."
        );
        assert_eq!(h.provider.seen.lock().unwrap().len(), 2);
        assert_eq!(h.runs.load(Ordering::SeqCst), 0);
        assert!(!store.has_pending("s1"));
        assert_eq!(h.audit_decisions(), vec!["denied_by_policy"]);
    }

    #[tokio::test]
    async fn test_runner_redeems_the_replay_and_folds_the_round_in_context() {
        let store = Arc::new(ApprovalStore::default());
        let mut h = runner_harness(
            vec![
                tool_call_response("tc-1", "shell", json!({"command": "deploy"})),
                tool_call_response("tc-2", "shell", json!({"command": "deploy"})),
                final_response("deployed"),
            ],
            store.clone(),
            Default::default(),
        )
        .await;

        let first = h.run("deploy it", TurnKind::User).await;
        assert!(first.content.starts_with("Pending approval"));
        let second = h.run("yes, go ahead", TurnKind::User).await;

        assert!(second.success, "{}", second.content);
        assert_eq!(h.runs.load(Ordering::SeqCst), 1, "the replay executed once");
        assert!(!store.has_pending("s1"), "the record was redeemed");

        // The third provider call saw one completed round: the real result sits
        // in the original call's slot and the confirmation round is gone.
        let seen = h.provider.last_messages();
        assert!(
            !seen.iter().any(|m| m.approval_id().is_some()),
            "no marked message survives a redeemed round: {seen:#?}"
        );
        assert!(
            !seen.iter().any(|m| matches!(m, Message::User { content: crate::session::Content::Plain(t), .. } if t == "yes, go ahead")),
            "the confirmation line is folded away"
        );
        let tool_results: Vec<&Message> = seen
            .iter()
            .filter(|m| matches!(m, Message::Tool { .. }))
            .collect();
        assert_eq!(
            tool_results.len(),
            1,
            "exactly one result for the call: {seen:#?}"
        );
        assert!(
            matches!(tool_results[0], Message::Tool { content, .. } if content.contains("ran"))
        );

        // The JSONL is append-only: both rounds are physically still there.
        let persisted = h.messages().await;
        assert_eq!(
            persisted
                .iter()
                .filter(|m| matches!(m, Message::Tool { .. }))
                .count(),
            2,
            "the append-only session keeps both rounds"
        );
        assert_eq!(h.audit_decisions(), vec!["proposed", "redeemed"]);
        assert!(
            !std::fs::read_to_string(h.session_path())
                .unwrap()
                .contains("decision"),
            "audit never lands in the session JSONL"
        );
    }

    impl RunnerHarness {
        fn session_path(&self) -> PathBuf {
            // The session manager names the file after the session id.
            std::fs::read_dir(self._tmp.path().join("sessions"))
                .unwrap()
                .filter_map(|e| e.ok())
                .map(|e| e.path())
                .find(|p| p.extension().is_some_and(|e| e == "jsonl"))
                .expect("session jsonl")
        }
    }

    #[tokio::test]
    async fn test_runner_cancelled_confirmation_turn_leaves_no_dangling_tool_call() {
        let store = Arc::new(ApprovalStore::default());
        let mut h = runner_harness(
            vec![
                tool_call_response("tc-1", "shell", json!({"command": "deploy"})),
                // The owner moved on instead of confirming.
                final_response("ok, dropping it"),
                final_response("still here"),
            ],
            store.clone(),
            Default::default(),
        )
        .await;

        h.run("deploy it", TurnKind::User).await;
        h.run("never mind", TurnKind::User).await;
        assert!(!store.has_pending("s1"), "the unredeemed record rolled off");

        h.run("hello again", TurnKind::User).await;

        // Every tool_call in the built context has a real result, and the
        // guidance was rewritten to a failure instead of vanishing.
        let seen = h.provider.last_messages();
        let mut answers: std::collections::HashMap<String, String> =
            std::collections::HashMap::new();
        for message in &seen {
            if let Message::Tool {
                tool_call_id,
                content,
                ..
            } = message
            {
                answers.insert(tool_call_id.clone(), content.clone());
            }
        }
        let mut declared = Vec::new();
        for message in &seen {
            if let Message::Assistant {
                tool_calls: Some(calls),
                ..
            } = message
            {
                for call in calls {
                    declared.push(call.id.clone());
                }
            }
        }
        assert!(!declared.is_empty());
        for id in &declared {
            assert!(
                answers.contains_key(id),
                "tool_call {id} has no result: {seen:#?}"
            );
        }
        assert!(
            answers["tc-1"].contains("not redeemed"),
            "unredeemed guidance becomes a failure result: {}",
            answers["tc-1"]
        );
    }

    #[tokio::test]
    async fn test_runner_unattended_turn_never_writes_a_pending_list() {
        let store = Arc::new(ApprovalStore::default());
        let mut h = runner_harness(
            vec![
                tool_call_response("tc-1", "shell", json!({"command": "deploy"})),
                final_response("needs owner authorization"),
            ],
            store.clone(),
            Default::default(),
        )
        .await;

        let result = h.run("deploy it", TurnKind::Cron).await;

        assert!(result.success, "{}", result.content);
        assert_eq!(result.content, "needs owner authorization");
        assert_eq!(result.iterations, 1, "the run continues after the refusal");
        assert!(!store.has_pending("s1"));
        assert_eq!(h.runs.load(Ordering::SeqCst), 0);
        assert_eq!(h.audit_decisions(), vec!["denied_by_policy"]);
    }

    #[tokio::test]
    async fn test_runner_audit_lives_in_the_trace_not_in_the_session() {
        let store = Arc::new(ApprovalStore::default());
        let mut h = runner_harness(
            vec![
                tool_call_response("tc-1", "shell", json!({"command": "deploy"})),
                tool_call_response("tc-2", "shell", json!({"command": "deploy"})),
                final_response("deployed"),
            ],
            store.clone(),
            Default::default(),
        )
        .await;

        h.run("deploy it", TurnKind::User).await;
        h.run("yes", TurnKind::User).await;

        let trace = std::fs::read_to_string(&h.trace_path).unwrap();
        assert!(
            trace.contains("\"name\":\"approval\""),
            "approval span missing"
        );
        assert!(trace.contains("\"approval.decision\":\"proposed\""));
        assert!(trace.contains("\"approval.decision\":\"redeemed\""));
        assert!(trace.contains("\"approval.args_hash\""));
        // The audit says who decided, not just what was decided.
        assert!(trace.contains("\"approval.decided_by\":\"policy\""));
        assert!(trace.contains("\"approval.decided_by\":\"redemption\""));

        let session = std::fs::read_to_string(h.session_path()).unwrap();
        for decision in [
            "proposed",
            "redeemed",
            "cleared",
            "expired",
            "denied_by_policy",
        ] {
            assert!(
                !session.contains(decision),
                "decision {decision} must not be written into session JSONL: {session}"
            );
        }
    }

    #[tokio::test]
    async fn test_runner_restart_forgets_pending_approvals() {
        let store = Arc::new(ApprovalStore::default());
        let mut h = runner_harness(
            vec![
                tool_call_response("tc-1", "shell", json!({"command": "deploy"})),
                // After the restart the replay is a *new* proposal, not a redemption.
                tool_call_response("tc-2", "shell", json!({"command": "deploy"})),
            ],
            store.clone(),
            Default::default(),
        )
        .await;
        h.run("deploy it", TurnKind::User).await;
        assert_eq!(store.pending_for("s1").len(), 1);

        // A new process has an empty store (nothing is persisted).
        let revived = Arc::new(ApprovalStore::default());
        h.restart(revived.clone());

        let second = h.run("yes", TurnKind::User).await;
        assert!(
            second.content.starts_with("Pending approval"),
            "{}",
            second.content
        );
        assert_eq!(
            h.runs.load(Ordering::SeqCst),
            0,
            "nothing ran after a restart"
        );
        assert_eq!(revived.pending_for("s1").len(), 1);
    }

    #[tokio::test]
    async fn test_runner_live_approval_is_pinned_against_snipping() {
        let store = Arc::new(ApprovalStore::default());
        let mut responses = vec![tool_call_response(
            "tc-1",
            "shell",
            json!({"command": "deploy"}),
        )];
        // Turn two keeps calling a harmless tool, so the pending turn drifts
        // away from the tail while the record is still live.
        for i in 0..4 {
            responses.push(tool_call_response(
                &format!("r-{i}"),
                "read",
                json!({"path": "big"}),
            ));
        }
        responses.push(final_response("done"));
        let mut h = runner_harness(
            responses,
            store.clone(),
            // A window the read results alone will exceed.
            crate::pressure::PressureAnchor::from_window(2_048),
        )
        .await;

        let proposed = h.run("deploy it", TurnKind::User).await;
        assert!(
            proposed.content.starts_with("Pending approval"),
            "{}",
            proposed.content
        );
        assert_eq!(store.pending_for("s1").len(), 1, "still pending");

        // The record stays live for the whole second run: the guidance result
        // must survive every build, even with the window full of later traffic.
        let second = h.run("what is happening?", TurnKind::User).await;
        assert!(second.success, "{}", second.content);
        // Never redeemed: the record rolls off when the turn closes.
        assert!(store.pending_for("s1").is_empty());

        let seen = h.provider.last_messages();
        let marked: Vec<&Message> = seen
            .iter()
            .filter(|m| m.approval_id().is_some() && matches!(m, Message::Tool { .. }))
            .collect();
        assert_eq!(
            marked.len(),
            1,
            "the pending guidance must stay in the window: {seen:#?}"
        );
        assert!(matches!(
            marked[0],
            Message::Tool { content, .. } if content.starts_with("NOT EXECUTED")
        ));
    }
}
