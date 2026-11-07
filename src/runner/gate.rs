//! The shared tool-call gate (ADR 0011), extracted so the runner's PreTool
//! [`ApprovalHook`](crate::runner::hooks::approval::ApprovalHook) and the
//! codemode agent entry (ticket 25) consult one decision core.
//!
//! A nested `tools.<name>()` call inside a codemode script is a tool call like
//! any other: same risk class, same control level, same redemption and veto
//! rules, same refusal text. The only difference is what happens after the
//! decision — the runner hook can end the turn, while a nested call hands the
//! refusal or pending-guidance text back to the script as the call's resolved
//! value (no mid-script popups). The act entry is the exception (ticket 15): it
//! stops the whole script on any verdict other than "it ran", because an
//! unattended ephemeral subagent has no next turn in which to redeem a pending
//! approval — see [`NestedCall`].

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use serde_json::Value;
use tokio_util::sync::CancellationToken;

use crate::approval::{
    ControlLevel, PendingApproval, RiskLevel, args_hash, denied_text, pending_guidance_text,
    unattended_denied_text, vetoed_text,
};
use crate::session::{AgentEvent, TaskHook};
use crate::tool::{ToolCall, ToolDefinition, ToolManager, format_tool_error};
use crate::trace::{SpanRecorder, SpanStatus, Tracer};

/// Everything the gate needs about one run, independent of `RunSpec`/`RunState`
/// so callers outside the runner loop (codemode) can reach the same decision.
#[derive(Clone)]
pub(crate) struct GateContext {
    pub session_id: String,
    pub task_hook: TaskHook,
    pub tool_manager: Arc<ToolManager>,
    pub approvals: Arc<crate::approval::ApprovalStore>,
    pub tracer: Option<Arc<Tracer>>,
    pub cancel_token: Option<CancellationToken>,
    /// The gateway's connector pool (ticket 26). `None` when no MCP server is
    /// configured, which reads exactly like a pool with no tools in it.
    pub connectors: Option<Arc<crate::mcp::McpService>>,
}

impl GateContext {
    fn is_cancelled(&self) -> bool {
        self.cancel_token
            .as_ref()
            .is_some_and(|ct| ct.is_cancelled())
    }

    /// The risk class of one call, looked up in both tool sources: the run's
    /// registered tools first, then the connectors. Registered tools win, so a
    /// connector can never shadow or reinterpret a tool the run owns.
    fn risk_of(&self, name: &str) -> Option<RiskLevel> {
        match self.tool_manager.risk_level(name) {
            Some(risk) => Some(risk),
            None => self
                .connectors
                .as_ref()
                .and_then(|pool| pool.risk_for(name)),
        }
    }

    /// Execute one call against whichever source owns the name.
    async fn dispatch(&self, name: &str, args: Value) -> anyhow::Result<String> {
        // A name the registered tools do not know is the connector's to answer;
        // no pool configured means it simply does not exist.
        if self.tool_manager.risk_level(name).is_none()
            && let Some(pool) = &self.connectors
        {
            return pool
                .call(name, args)
                .await
                .map_err(|e| anyhow::anyhow!(e.to_string()));
        }
        self.tool_manager.execute(name, args).await
    }
}

/// The gate's verdict for one tool call, with all store mutations (redemption,
/// veto, proposal) and audits already applied. Run-loop side effects that need
/// `RunState` (ending the turn, marking the tool message) stay in the hook.
pub(crate) enum GateDecision {
    /// Execute now: allowed outright, or a redeemed replay.
    Execute,
    /// Do not execute; answer the call with this text and continue.
    Refuse { result: String },
    /// Do not execute; a pending approval was recorded (attended `Ask`).
    Pending { result: String, approval_id: String },
    /// A redeemed call found the run cancelled: abort like the hook does.
    Cancelled { reason: String },
}

/// Write one audit event to the trace `approval` span.
///
/// Decisions go to the trace and not to the session JSONL: conversation history
/// is trimmed by snip/consolidate, so an audit there would eventually vanish.
pub(crate) fn audit(tracer: Option<&Arc<Tracer>>, decision: &str, record: &PendingApproval) {
    let Some(tracer) = tracer else { return };
    let span = tracer.start_span("approval", None);
    span.set_attribute("approval.decision", serde_json::json!(decision));
    span.set_attribute(
        "approval.decided_by",
        serde_json::json!(decided_by(decision)),
    );
    span.set_attribute("approval.id", serde_json::json!(record.approval_id));
    span.set_attribute("approval.owner_id", serde_json::json!(record.owner_id));
    span.set_attribute("approval.session_id", serde_json::json!(record.session_id));
    span.set_attribute("approval.run_id", serde_json::json!(record.run_id));
    span.set_attribute(
        "approval.turn_kind",
        serde_json::json!(record.turn_kind.as_str()),
    );
    span.set_attribute("approval.tool", serde_json::json!(record.tool));
    span.set_attribute("approval.args", serde_json::json!(record.trace_args()));
    span.set_attribute("approval.args_hash", serde_json::json!(record.args_hash));
    span.set_attribute("approval.risk", serde_json::json!(record.risk.as_str()));
    span.set_attribute("approval.ts_ms", serde_json::json!(record.created_ms));
    span.set_status(SpanStatus::Ok);
}

/// Who/what decided: the audit needs to distinguish "the policy refused" from
/// "the owner redeemed" from "the clock ran out" without decoding the decision.
fn decided_by(decision: &str) -> &'static str {
    match decision {
        "redeemed" => "redemption",
        "expired" => "ttl",
        "cleared" => "round_end",
        _ => "policy",
    }
}

/// Build the record a decision would produce. Only `Ask` stores it.
fn proposal(
    ctx: &GateContext,
    tool_call: &ToolCall,
    risk: RiskLevel,
    hash: &str,
    control: ControlLevel,
) -> PendingApproval {
    PendingApproval {
        approval_id: new_approval_id(control),
        owner_id: ctx.task_hook.owner_id().to_string(),
        session_id: ctx.session_id.clone(),
        run_id: ctx.task_hook.run_id().to_string(),
        turn_kind: ctx.task_hook.turn_kind(),
        tool: tool_call.name.clone(),
        args: tool_call.args.clone(),
        args_hash: hash.to_string(),
        risk,
        created_ms: ctx.approvals.now_ms(),
    }
}

/// Deterministic-ish id, prefixed with the decision so a trace is readable.
fn new_approval_id(control: ControlLevel) -> String {
    format!(
        "appr-{}-{}",
        control.as_str(),
        uuid::Uuid::new_v4().simple()
    )
}

/// The single decision point: allow, refuse, or defer to the owner.
pub(crate) async fn decide(ctx: &GateContext, tool_call: &ToolCall) -> GateDecision {
    // An unregistered tool cannot execute, so there is nothing to authorize.
    let Some(risk) = ctx.risk_of(&tool_call.name) else {
        return GateDecision::Execute;
    };
    let control = ctx.approvals.control_for(&tool_call.name, risk);
    let hash = args_hash(&tool_call.args);

    // 1. Redemption: a replayed call with the same tool + args hash is the
    //    approval the owner gave on the previous turn.
    if let Some(record) = ctx
        .approvals
        .matching(&ctx.session_id, &tool_call.name, &hash)
    {
        return redeem(ctx, record, risk, &hash).await;
    }

    // 2. Veto: a carried proposal for this tool with different args means the
    //    model replays something else than what was approved. Fail closed.
    let mismatched = ctx
        .approvals
        .mismatching(&ctx.session_id, &tool_call.name, &hash);
    if !mismatched.is_empty() {
        for record in &mismatched {
            ctx.approvals.take(&ctx.session_id, &record.approval_id);
            audit(ctx.tracer.as_ref(), "denied_by_policy", record);
        }
        return GateDecision::Refuse {
            result: vetoed_text(&tool_call.name, &mismatched[0].approval_id),
        };
    }

    match control {
        ControlLevel::Allow => GateDecision::Execute,
        ControlLevel::Deny => {
            let record = proposal(ctx, tool_call, risk, &hash, control);
            audit(ctx.tracer.as_ref(), "denied_by_policy", &record);
            GateDecision::Refuse {
                result: denied_text(&tool_call.name, risk),
            }
        }
        ControlLevel::Ask => {
            // Nobody is online to redeem an unattended run, so `Ask` is a
            // refusal there: fail closed and never write a pending record.
            if !ctx.task_hook.turn_kind().is_attended() {
                let record = proposal(ctx, tool_call, risk, &hash, control);
                audit(ctx.tracer.as_ref(), "denied_by_policy", &record);
                return GateDecision::Refuse {
                    result: unattended_denied_text(
                        &tool_call.name,
                        risk,
                        ctx.task_hook.turn_kind(),
                    ),
                };
            }

            let record = ctx
                .approvals
                .propose(proposal(ctx, tool_call, risk, &hash, control));
            audit(ctx.tracer.as_ref(), "proposed", &record);
            GateDecision::Pending {
                result: pending_guidance_text(&record),
                approval_id: record.approval_id.clone(),
            }
        }
    }
}

/// Redeem a matching pending record: broadcast, re-validate, then execute.
async fn redeem(
    ctx: &GateContext,
    record: PendingApproval,
    risk: RiskLevel,
    hash: &str,
) -> GateDecision {
    // Pre-execution re-validation of the two code-side gates (ADR 0011/6): the
    // risk class and the args hash are recomputed against the live call.
    let revalidated = record.risk == risk && record.args_hash == hash;
    if !revalidated {
        ctx.approvals.take(&ctx.session_id, &record.approval_id);
        audit(ctx.tracer.as_ref(), "denied_by_policy", &record);
        return GateDecision::Refuse {
            result: vetoed_text(&record.tool, &record.approval_id),
        };
    }

    // Consume first, then announce: a redeemed approval is never redeemable twice.
    ctx.approvals.take(&ctx.session_id, &record.approval_id);
    audit(ctx.tracer.as_ref(), "redeemed", &record);

    // Compensation control: tell the origin channel what is about to run, then
    // give the owner a scheduling window to `/stop` before it does.
    ctx.task_hook.fire_event(AgentEvent::ApprovalRedeemed {
        session_id: ctx.session_id.clone(),
        approval_id: record.approval_id.clone(),
        tool: record.tool.clone(),
        args: record.args_text(),
        risk: record.risk.as_str().to_string(),
    });
    tokio::task::yield_now().await;

    if ctx.is_cancelled() {
        return GateDecision::Cancelled {
            reason: format!(
                "Task cancelled: /stop arrived before `{}` executed.",
                record.tool
            ),
        };
    }
    GateDecision::Execute
}

/// The run's nested tool-call authority (ticket 25). Shared through
/// [`TurnContext`](crate::tool::TurnContext) so the codemode agent entry can
/// execute `tools.<name>()` calls under exactly the gate above.
pub(crate) struct RunToolExec {
    context: GateContext,
    /// Set when a nested call produced a pending approval, so the codemode
    /// tool can append the owner-facing pending list to its result.
    pending_seen: AtomicBool,
}

impl RunToolExec {
    pub(crate) fn new(context: GateContext) -> Self {
        Self {
            context,
            pending_seen: AtomicBool::new(false),
        }
    }

    /// The tool catalog this run exposes to the model. Connector tools are
    /// deliberately absent: a script discovers them through `searchTools` and
    /// reaches them as `tools.mcp__<server>__<tool>`, so their (untrusted,
    /// server-authored) schemas never widen the model's action space.
    pub(crate) fn tool_definitions(&self) -> Vec<ToolDefinition> {
        self.context.tool_manager.to_openai_functions()
    }

    /// What a script may search and call: the run's tools plus every online
    /// connector's tools, in that order.
    pub(crate) fn callable_definitions(&self) -> Vec<ToolDefinition> {
        let mut defs = self.tool_definitions();
        if let Some(pool) = &self.context.connectors {
            defs.extend(pool.definitions());
        }
        defs
    }

    pub(crate) fn took_pending(&self) -> bool {
        self.pending_seen.load(Ordering::Acquire)
    }

    /// The owner-facing plain-text pending list for this session (ADR 0011).
    pub(crate) fn pending_list_text(&self) -> String {
        self.context
            .approvals
            .pending_list_text(&self.context.session_id)
    }

    /// The run's connector pool, so an entry can install a connector surface
    /// over the same pool this gate dispatches to (ticket 15).
    pub(crate) fn connectors(&self) -> Option<Arc<crate::mcp::McpService>> {
        self.context.connectors.clone()
    }

    /// Gate and execute one nested call, keeping *how* it ended.
    ///
    /// Entries that must react to a refusal rather than read it as a value need
    /// the verdict as a fact; the text prefixes (`DENIED:`, `NOT EXECUTED:`) are
    /// prose and may be reworded, so they are never the control flow. The act
    /// entry stops its script on any non-`Ran` answer (ticket 15); the agent
    /// entry answers the text and keeps going.
    pub(crate) async fn call_gated(&self, name: &str, args: Value) -> NestedCall {
        let call = ToolCall {
            id: format!("codemode-{}", uuid::Uuid::new_v4().simple()),
            name: name.to_string(),
            args,
        };
        match decide(&self.context, &call).await {
            GateDecision::Execute => match self.context.dispatch(name, call.args).await {
                Ok(text) => NestedCall::Ran(text),
                Err(e) => NestedCall::Ran(format_tool_error(&e.to_string())),
            },
            GateDecision::Refuse { result } => NestedCall::Refused(result),
            GateDecision::Pending { result, .. } => NestedCall::Pending(result),
            GateDecision::Cancelled { reason } => NestedCall::Cancelled(format_tool_error(&reason)),
        }
    }

    /// Gate and execute one nested call. Never fails the caller: every outcome
    /// (result, refusal, pending guidance) comes back as text, mirroring how
    /// the runner answers tool messages for the model.
    pub(crate) async fn call(&self, name: &str, args: Value) -> String {
        match self.call_gated(name, args).await {
            NestedCall::Ran(text) | NestedCall::Refused(text) => text,
            NestedCall::Pending(text) => {
                // The codemode tool appends the owner-facing pending list from
                // this flag, so the attended hand-off stays observable.
                self.pending_seen.store(true, Ordering::Release);
                text
            }
            NestedCall::Cancelled(reason) => reason,
        }
    }
}

/// How one nested `tools.<name>()` call ended.
///
/// The gate's verdict, not the answer text: `Refused` and `Pending` both mean
/// "nothing executed", and the difference is whether an approval was recorded.
pub(crate) enum NestedCall {
    /// The tool ran, or answered a clean tool error of its own.
    Ran(String),
    /// The policy refused the call; nothing executed.
    Refused(String),
    /// An attended `Ask` recorded a pending approval; nothing executed.
    Pending(String),
    /// A redeemed replay found the run cancelled by `/stop`.
    Cancelled(String),
}
