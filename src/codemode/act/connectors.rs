//! The connector write side of an act run (ticket 15).
//!
//! Three things meet here and nowhere else: the catalog the script can see, the
//! `allow_connectors` ceiling that turns a write outside it into a tripwire, and
//! the wiring of both to the run's call budget and abort switch. A change to the
//! capability table, to the ceiling, or to how a refusal is reported lands in
//! this file alone; the rest of [`super::ActHost`] is primitives.
//!
//! The catalog is every online connector tool, reads and writes alike, because
//! `searchTools` is meant to find the write side too — "searchable" and
//! "callable" are different questions. A callable outside the write ceiling is
//! installed as a **tripwire**: calling it stops the whole run through the abort
//! switch instead of reaching the connector. A bare absence would be a
//! `TypeError` the script can catch and continue past, which is exactly the
//! "treat a refusal as a value" mistake the act entry must not make.
//!
//! A refusal is the end of the run, not a value the script may carry on from: an
//! unattended ephemeral subagent has no next turn in which to redeem an
//! approval. The agent entry deliberately does the opposite — this is the one
//! place the two entries' post-decision behaviour differs.

use std::collections::{BTreeSet, HashMap};
use std::sync::Arc;

use rquickjs::Ctx;

use super::budget::CallBudget;
use crate::codemode::connector::{call_in_turn, install_tool_bridge};
use crate::mcp::McpService;
use crate::runner::gate::{NestedCall, RunToolExec};
use crate::tool::TurnContext;

/// The connector half of an act run: the pool to reach, the gate every call
/// passes, and the servers the write side is limited to.
pub(super) struct ActConnectors {
    /// The run's approval gate. `RunToolExec` owns the pool, so the surface and
    /// the gate can never disagree about which connectors exist.
    exec: Arc<RunToolExec>,
    /// The turn nested calls run in: tools that resolve the turn's origin
    /// (message, cron, trigger) see the same target the subagent does.
    turn: Option<Arc<TurnContext>>,
    /// MCP **server** names whose write tools this executor may call. Read-only
    /// claims need no entry here; everything else does (fail-closed).
    allow: BTreeSet<String>,
}

impl ActConnectors {
    /// `allow` is the executor's `allow_connectors`, a **capability** ceiling
    /// fixed before the run. It never decides an approval level: every call
    /// still goes through the gate, whose class comes from the tool's own risk
    /// (always `ExternalSend` for a connector).
    pub(super) fn new(
        exec: Arc<RunToolExec>,
        turn: Option<Arc<TurnContext>>,
        allow: Vec<String>,
    ) -> Self {
        Self {
            exec,
            turn,
            allow: allow.into_iter().collect(),
        }
    }
}

/// Install the connector surface: one shared bridge (ticket 17's), with this
/// entry's catalog and its own invoke path.
pub(super) fn install(
    ctx: &Ctx<'_>,
    connectors: Option<&ActConnectors>,
    budget: CallBudget,
) -> rquickjs::Result<()> {
    let pool = connectors.and_then(|conn| conn.exec.connectors());
    let blocked = pool
        .as_ref()
        .zip(connectors)
        .map(|(pool, conn)| Arc::new(blocked_writes(pool, &conn.allow)))
        .unwrap_or_default();

    let call = connectors.map(|conn| ActCall {
        exec: conn.exec.clone(),
        turn: conn.turn.clone(),
        blocked,
    });
    install_tool_bridge(
        ctx,
        (pool, call, budget),
        // No pool: an empty namespace, not a missing one, so the act surface
        // keeps one shape whatever the process is configured with.
        |(pool, _, _): &ConnectorState| {
            pool.as_ref()
                .map(|pool| pool.definitions())
                .unwrap_or_default()
        },
        |(_, call, budget), name, args| {
            Box::pin(async move {
                let Some(call) = call else {
                    return format!(
                        "Error: connector `{name}` is not available in this run (no authorization \
                         context is bound, so nothing could be gated)"
                    );
                };
                // The ceiling is checked before the budget: nothing was
                // attempted outward, so a blocked write must not spend an
                // external call the script still needs.
                if let Some(server) = call.blocked.get(&name) {
                    let reason = format!(
                        "connector write `{name}` is outside this executor's ceiling: server \
                         `{server}` is not in `allow_connectors`, so nothing was sent and the run \
                         was stopped"
                    );
                    if let Some(abort) = budget.abort() {
                        abort.stop(reason.clone());
                    }
                    return format!("Error: {reason}");
                }
                if let Some(reason) = budget.charge() {
                    return format!("Error: {reason}");
                }
                let outcome = call_in_turn(call.turn.clone(), move || {
                    let exec = call.exec.clone();
                    Box::pin(async move { exec.call_gated(&name, args).await })
                })
                .await;
                match outcome {
                    NestedCall::Ran(text) => text,
                    // A refusal stops the run: `Ask` in an unattended subagent
                    // has no next turn to redeem it, and `Deny` is final.
                    NestedCall::Refused(text) => {
                        if let Some(abort) = budget.abort() {
                            abort.refuse(text.clone());
                        }
                        text
                    }
                    NestedCall::Pending(text) => {
                        if let Some(abort) = budget.abort() {
                            abort.refuse(text.clone());
                        }
                        text
                    }
                    NestedCall::Cancelled(reason) => {
                        if let Some(abort) = budget.abort() {
                            abort.stop(reason.clone());
                        }
                        reason
                    }
                }
            })
        },
    )
}

/// What the bridge needs from this entry: the pool (for the catalog), the call
/// path, and the budget both paths charge.
type ConnectorState = (Option<Arc<McpService>>, Option<ActCall>, CallBudget);

/// What the connector surface does with one callable during install: reach the
/// connector, or trip the run.
///
/// The split is decided from the **install-time** catalog snapshot, for the same
/// reason [`install_tool_bridge`] installs from a snapshot: a server that
/// re-lists mid-run must not reclassify a name the script already holds. Writes
/// are the fail-closed complement of the read-only claim, so an unannotated tool
/// is a write.
fn blocked_writes(pool: &McpService, allow: &BTreeSet<String>) -> HashMap<String, String> {
    let mut blocked = HashMap::new();
    for def in pool.definitions() {
        if pool.is_read_only_claimed(&def.name) {
            continue;
        }
        let server = pool.server_of(&def.name).unwrap_or_default();
        if allow.contains(&server) {
            continue;
        }
        blocked.insert(def.name, server);
    }
    blocked
}

/// The call path of one act run's connector surface, cloned per call.
#[derive(Clone)]
struct ActCall {
    exec: Arc<RunToolExec>,
    turn: Option<Arc<TurnContext>>,
    /// Callables that must never reach the connector, with the server behind
    /// each (for the reason text).
    blocked: Arc<HashMap<String, String>>,
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::sync::Arc;
    use std::time::Duration;

    use async_trait::async_trait;
    use serde_json::{Value, json};

    use super::super::budget::MAX_EXTERNAL_CALLS;
    use super::*;
    use crate::approval::{ApprovalConfig, ApprovalStore, ControlLevel, RiskLevel};
    use crate::codemode::ActHost;
    use crate::codemode::error::ScriptError;
    use crate::codemode::host::HostEnv;
    use crate::codemode::redact::Redactor;
    use crate::codemode::sandbox::{
        AbortHandle, AbortKind, ENTRY_ACT, Limits, RunRequest, execute,
    };
    use crate::mcp::service::test_support as mcp_test;
    use crate::runner::gate::GateContext;
    use crate::session::{TaskHook, TurnKind};
    use crate::tool::{Tool, ToolManager};

    fn secrets() -> Arc<BTreeMap<String, String>> {
        Arc::new(BTreeMap::from([(
            "token".to_string(),
            "s3cr3t-token".to_string(),
        )]))
    }

    /// A tool the run owns. The act entry must never expose one of these, no
    /// matter what the run's tool manager carries — `tools.*` is the connector's
    /// namespace only.
    struct BuiltinProbe {
        name: &'static str,
        description: &'static str,
    }

    #[async_trait]
    impl Tool for BuiltinProbe {
        fn name(&self) -> &str {
            self.name
        }
        fn description(&self) -> &str {
            self.description
        }
        fn parameters(&self) -> Value {
            json!({"type": "object"})
        }
        fn risk_level(&self) -> RiskLevel {
            RiskLevel::ReadOnly
        }
        async fn execute(&self, _args: Value) -> anyhow::Result<String> {
            Ok(format!("{}:ran", self.name))
        }
    }

    /// A started pool over one fake server. The runtime stays alive for the
    /// test: the pool's supervisor runs on it.
    fn online_pool(
        server: &str,
        tools: Vec<Value>,
    ) -> (
        Arc<McpService>,
        Arc<mcp_test::FakePeer>,
        tokio::runtime::Runtime,
    ) {
        let rt = tokio::runtime::Runtime::new().expect("connector runtime");
        let (service, peer) = {
            let _guard = rt.enter();
            mcp_test::online_service(server, tools)
        };
        rt.block_on(mcp_test::wait_for(
            || service.any_online(),
            "the fake server online",
        ));
        (service, peer, rt)
    }

    /// The run's gate over a tool manager that carries the built-ins an act
    /// script must never see, plus whatever connector pool the test wires.
    fn gate(
        pool: Option<Arc<McpService>>,
        controls: Vec<(&str, ControlLevel)>,
    ) -> (Arc<RunToolExec>, Arc<ApprovalStore>) {
        let mut manager =
            ToolManager::new(std::path::PathBuf::from("/tmp/slimbot-codemode-act-test"));
        for (name, description) in [
            ("file_reader", "read a local file"),
            ("file_writer", "write a local file"),
            ("shell", "run a shell command"),
            ("message", "send a message to the owner"),
            ("memory_search", "search memory"),
        ] {
            manager.register(Box::new(BuiltinProbe { name, description }));
        }
        let mut config = ApprovalConfig::default();
        for (name, level) in controls {
            config.tools.insert(name.to_string(), level);
        }
        let approvals = Arc::new(ApprovalStore::new(config));
        let exec = Arc::new(RunToolExec::new(GateContext {
            session_id: "cli:c1".to_string(),
            // An executor runs inside an ephemeral subagent: unattended.
            task_hook: TaskHook::new("cli:c1").with_turn_kind(TurnKind::Subagent),
            tool_manager: Arc::new(manager),
            approvals: approvals.clone(),
            connectors: pool,
            tracer: None,
            cancel_token: None,
        }));
        (exec, approvals)
    }

    /// Run one act script with the connector surface of `allow` over `exec`.
    fn run_act_connectors(
        script: &str,
        exec: Arc<RunToolExec>,
        allow: &[&str],
    ) -> (Result<Value, ScriptError>, Arc<AbortHandle>) {
        let abort = Arc::new(AbortHandle::new());
        let host = Arc::new(
            ActHost::new(secrets())
                .with_abort(abort.clone())
                .with_connectors(
                    exec.clone(),
                    None,
                    allow.iter().map(|s| s.to_string()).collect(),
                ),
        );
        let outcome = execute(
            RunRequest::new(script, ENTRY_ACT)
                .with_limits(Limits::act().with_timeout(Duration::from_secs(5)))
                .with_abort(abort.clone()),
            host,
            HostEnv::new(Redactor::new(["s3cr3t-token"])),
        );
        (outcome, abort)
    }

    /// One read-only-claimed tool and one unannotated (⇒ write) tool.
    fn read_and_write_tools() -> Vec<Value> {
        vec![
            json!({
                "name": "get_pull",
                "description": "fetch a pull request",
                "annotations": {"readOnlyHint": true, "destructiveHint": false},
            }),
            json!({
                "name": "create_issue",
                "description": "file an issue",
            }),
        ]
    }

    #[test]
    fn test_act_entry_connector_surface_excludes_builtin_tools() {
        // The one substantial difference from the agent entry: `tools.*` serves
        // connectors only. Both directions are pinned — the connector names are
        // there, every built-in name is not, and `searchTools` cannot surface
        // one either.
        let (pool, _peer, _rt) = online_pool("github", read_and_write_tools());
        let (exec, _approvals) = gate(Some(pool), vec![]);
        let (outcome, _abort) = run_act_connectors(
            r#"async function act(ctx) {
                const names = async (query) =>
                    JSON.parse(await ctx.searchTools(query)).map((h) => h.name);
                return {
                    keys: Object.keys(ctx.tools).sort(),
                    shellHits: await names("run a shell command"),
                    memoryHits: await names("search memory"),
                    connectorHits: await names("file an issue"),
                    connector: typeof ctx.tools.mcp__github__get_pull,
                    builtins: ["file_reader", "file_writer", "shell", "message", "memory_search"]
                        .filter((n) => typeof ctx.tools[n] !== "undefined"),
                };
            }"#,
            exec,
            &["github"],
        );
        let value = outcome.expect("probe must run");
        assert_eq!(
            value["keys"],
            json!(["mcp__github__create_issue", "mcp__github__get_pull"]),
            "{value}"
        );
        assert_eq!(value["connector"], "function", "{value}");
        assert_eq!(value["builtins"], json!([]), "{value}");
        // The searchable catalog is the connectors', not the run's tools: a
        // query that only a built-in description would match finds nothing, and
        // every hit is a connector name.
        assert_eq!(value["shellHits"], json!([]), "{value}");
        assert_eq!(value["memoryHits"], json!([]), "{value}");
        assert_eq!(
            value["connectorHits"],
            json!(["mcp__github__create_issue"]),
            "{value}"
        );
    }

    #[test]
    fn test_act_entry_write_outside_the_ceiling_is_searchable_but_stops_the_script() {
        // `github` is not in `allow_connectors`, so its write tool is outside the
        // ceiling. Two facts, both required: the script can still *find* it (the
        // searchable catalog is every connector), and calling it terminates the
        // run before the connector hears anything.
        let (pool, peer, _rt) = online_pool("github", read_and_write_tools());
        let (exec, _approvals) = gate(
            Some(pool),
            // The owner even allowed the callable: the ceiling holds anyway,
            // because a capability the executor was not created with is not the
            // approval policy's to grant.
            vec![("mcp__github__create_issue", ControlLevel::Allow)],
        );
        let (outcome, _abort) = run_act_connectors(
            r#"async function act(ctx) {
                return { hits: JSON.parse(await ctx.searchTools("file an issue")).map((h) => h.name) };
            }"#,
            exec.clone(),
            &[],
        );
        assert_eq!(
            outcome.expect("searching is not calling")["hits"],
            json!(["mcp__github__create_issue"]),
            "a write outside the ceiling stays searchable"
        );

        let (outcome, abort) = run_act_connectors(
            r#"async function act(ctx) {
                let swallowed = false;
                try { await ctx.tools.mcp__github__create_issue({title: "x"}); }
                catch (e) { swallowed = true; }
                return { ok: true, summary: "kept going, caught=" + swallowed };
            }"#,
            exec,
            &[],
        );
        let err = outcome.unwrap_err();
        assert_eq!(
            err.code(),
            "aborted",
            "a caught refusal may not continue: {err}"
        );
        assert!(err.message.contains("allow_connectors"), "{err}");
        assert_eq!(
            abort.kind(),
            Some(AbortKind::Stopped),
            "a host gate stopped it"
        );
        assert_eq!(
            peer.calls(),
            0,
            "the ceiling is enforced before the connector is asked"
        );
    }

    #[test]
    fn test_act_entry_reads_are_callable_without_allow_connectors() {
        // The ceiling constrains writes only: a read-only-claimed tool works with
        // an empty `allow_connectors`, and it still passes the approval gate.
        let (pool, peer, _rt) = online_pool("github", read_and_write_tools());
        let (exec, _approvals) = gate(
            Some(pool),
            vec![("mcp__github__get_pull", ControlLevel::Allow)],
        );
        let (outcome, abort) = run_act_connectors(
            r#"async function act(ctx) {
                return { ok: true, summary: String(await ctx.tools.mcp__github__get_pull({number: 7})) };
            }"#,
            exec,
            &[],
        );
        let value = outcome.expect("a read needs no ceiling entry");
        assert!(
            value["summary"].as_str().unwrap().contains("pong"),
            "the connector's answer must arrive: {value}"
        );
        assert_eq!(peer.calls(), 1);
        assert!(!abort.is_aborted());
    }

    #[test]
    fn test_act_entry_stops_the_script_when_a_call_needs_owner_approval() {
        // An executor runs in an ephemeral subagent: nobody can redeem an
        // approval after the run, so `Ask` is a refusal, and the script stops
        // where it stands instead of continuing as if nothing happened. No
        // pending record is written either — there is no next turn to replay it.
        let (pool, peer, _rt) = online_pool("github", read_and_write_tools());
        let (exec, approvals) = gate(Some(pool), vec![]);
        let (outcome, abort) = run_act_connectors(
            r#"async function act(ctx) {
                const out = await ctx.tools.mcp__github__create_issue({title: "x"});
                return { ok: true, summary: "kept going with " + out };
            }"#,
            exec,
            &["github"],
        );
        let err = outcome.unwrap_err();
        assert_eq!(err.code(), "aborted", "{err}");
        assert!(err.message.contains("needs owner authorization"), "{err}");
        assert!(err.message.contains("unattended"), "{err}");
        // The class the gate decided on is the tool's own, `external_send`:
        // naming the server in `allow_connectors` did not earn it a lighter one.
        assert!(err.message.contains("risk: external_send"), "{err}");
        assert_eq!(abort.kind(), Some(AbortKind::Refused), "policy refused it");
        assert_eq!(peer.calls(), 0, "nothing was sent");
        assert!(
            !approvals.has_pending("cli:c1"),
            "an unattended run must not queue a pending approval"
        );
    }

    #[test]
    fn test_act_entry_stops_the_script_on_a_denied_call() {
        // A `Deny` is the same full stop, with its own reason: the executor must
        // not report a refused action as a successful one.
        let (pool, peer, _rt) = online_pool("github", read_and_write_tools());
        let (exec, approvals) = gate(
            Some(pool),
            vec![("mcp__github__create_issue", ControlLevel::Deny)],
        );
        let (outcome, abort) = run_act_connectors(
            r#"async function act(ctx) {
                try { await ctx.tools.mcp__github__create_issue({title: "x"}); } catch (e) {}
                return { ok: true, summary: "kept going" };
            }"#,
            exec,
            &["github"],
        );
        let err = outcome.unwrap_err();
        assert_eq!(err.code(), "aborted", "{err}");
        assert!(
            err.message
                .contains("not allowed by the current approval policy"),
            "{err}"
        );
        assert_eq!(abort.kind(), Some(AbortKind::Refused));
        assert_eq!(peer.calls(), 0);
        assert!(!approvals.has_pending("cli:c1"));
    }

    #[test]
    fn test_act_entry_counts_connector_calls_against_the_same_budget() {
        // One budget for every outward call: a connector call spends the same
        // counter `ctx.http` does, and the run stops at the same ceiling.
        let (pool, peer, _rt) = online_pool("github", read_and_write_tools());
        let (exec, _approvals) = gate(
            Some(pool),
            vec![("mcp__github__get_pull", ControlLevel::Allow)],
        );
        let (outcome, abort) = run_act_connectors(
            &format!(
                r#"async function act(ctx) {{
                    for (let i = 0; i <= {MAX_EXTERNAL_CALLS}; i++) {{
                        try {{ await ctx.tools.mcp__github__get_pull({{number: i}}); }} catch (e) {{}}
                    }}
                    return {{ ok: true, summary: 'kept going' }};
                }}"#
            ),
            exec,
            &[],
        );
        let err = outcome.unwrap_err();
        assert_eq!(err.code(), "aborted", "{err}");
        assert!(
            err.message
                .contains(&format!("more than {MAX_EXTERNAL_CALLS} external calls")),
            "{err}"
        );
        assert_eq!(abort.kind(), Some(AbortKind::Stopped));
        assert_eq!(
            peer.calls(),
            MAX_EXTERNAL_CALLS,
            "the spent budget is what stops it, not the connector"
        );
    }
}
