//! The `run` action: one stored `act.js`, executed in the sandbox.
//!
//! This is the only action of the `executor` tool that is async, the only one
//! that touches [`crate::codemode`], and the only one that reads the turn's
//! task-local. The other four actions are host-owned file work (scan, cap,
//! atomic writes), so they live in [`super::tool`]; everything about
//! *executing* a stored script — the four gates, the connector wiring, the
//! in-band report — lives here.
//!
//! The seam is also the test surface: [`run_stored`] takes a root, the secret
//! map and the call arguments, and reports every refusal as a value instead of
//! an error, so a gate can be exercised without a `Tool::execute` around it.

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use serde_json::{Value, json};

use crate::codemode::ActHost;
use crate::codemode::host::HostEnv;
use crate::codemode::redact::Redactor;
use crate::codemode::sandbox::{AbortHandle, ENTRY_ACT, Limits, RunRequest, execute_async};

use super::args as call_args_schema;
use super::meta;
use super::result::{self, ActFailure};
use super::tool::text;

/// Run one executor's stored `act.js` and report the action in-band.
///
/// The order of the gates is the contract:
///
/// 1. the executor has to exist and be runnable — a broken one never starts;
/// 2. `args` are checked against `args_schema` **before** the sandbox is
///    involved, so a malformed call runs nothing at all;
/// 3. the sandbox's numeric gates apply: `meta.timeout_s` as the wall clock
///    and [`Limits::act`] as memory and the 16KB result ceiling;
/// 4. what the script returned is validated as `{ok, summary, evidence?}`
///    by [`result::outcome_to_json`], which also trims evidence to 4KB per
///    item and classifies every failure.
pub(crate) async fn run_stored(
    root: &Path,
    secrets: &Arc<BTreeMap<String, String>>,
    args: &Value,
) -> Value {
    let id = text(args, "id");
    if id.is_empty() {
        return not_started(
            &id,
            ActFailure::rejected("`id` is required: name the executor to run"),
        );
    }
    let Some(loaded) = meta::load_all(root)
        .into_iter()
        .find(|loaded| loaded.meta.id == id)
    else {
        return not_started(
            &id,
            ActFailure::rejected(format!("no executor with id '{id}'")),
        );
    };
    if let Some(problem) = &loaded.problem {
        return not_started(
            &id,
            ActFailure::rejected(format!("executor '{id}' is not runnable: {problem}")),
        );
    }
    let Some(script) = loaded.script else {
        return not_started(
            &id,
            ActFailure::rejected(format!("executor '{id}' has no act.js")),
        );
    };
    // An absent `args` is an empty object: an executor without an args
    // schema takes no arguments, which is the common case.
    let call_args = match args.get("args") {
        None | Some(Value::Null) => json!({}),
        Some(value) => value.clone(),
    };
    if let Err(problem) = call_args_schema::check(loaded.meta.args_schema.as_ref(), &call_args) {
        return not_started(&id, ActFailure::rejected(problem));
    }

    let abort = Arc::new(AbortHandle::new());
    let mut host = ActHost::new(secrets.clone()).with_abort(abort.clone());
    // The connector surface is installed only when the call has a gate to
    // route it through: without one there is nothing to authorize, so no
    // connector is callable at all (fail-closed). In production `run` always
    // executes inside a runner turn, and both facts are read from it once.
    let turn = crate::tool::current_turn();
    if let Some(exec) = turn.as_ref().and_then(|turn| turn.tool_exec()) {
        host = host.with_connectors(exec, turn, loaded.meta.allow_connectors.clone());
    }
    let host = Arc::new(host);
    let limits = Limits::act().with_timeout(Duration::from_secs(loaded.meta.timeout_s));
    let request = RunRequest::new(script, ENTRY_ACT)
        .with_ctx(json!({"args": call_args}))
        .with_limits(limits)
        .with_abort(abort.clone());
    let env = HostEnv::new(Redactor::new(secrets.values().cloned()));

    let outcome = execute_async(request, host.clone(), env).await;

    // Host facts first: taking them drains the host, and they are wanted on
    // every path, including a failure.
    let outward = host.take_outward_failure();
    let calls = host.external_calls();
    let logs = host.drain_logs();
    let report = match &outcome {
        Ok(value) => result::outcome_to_json(Ok(value), &abort, outward),
        Err(error) => result::outcome_to_json(Err(error), &abort, outward),
    };
    started(&id, calls, logs, report)
}

/// The report for a call that never reached the sandbox: nothing ran, so there
/// are no logs and no outward calls.
fn not_started(id: &str, failure: ActFailure) -> Value {
    let mut report = failure.into_json();
    let map = report.as_object_mut().expect("failure json is an object");
    map.insert("executor".to_string(), json!(id));
    map.insert("ran".to_string(), json!(false));
    map.insert("external_calls".to_string(), json!(0));
    report
}

/// The report for a run that reached the sandbox: the `act()` outcome plus the
/// host facts a reader needs to reconstruct what happened.
fn started(id: &str, external_calls: usize, logs: Vec<String>, outcome: Value) -> Value {
    let mut report = outcome;
    let map = report.as_object_mut().expect("outcome json is an object");
    map.insert("executor".to_string(), json!(id));
    map.insert("ran".to_string(), json!(true));
    map.insert("external_calls".to_string(), json!(external_calls));
    if !logs.is_empty() {
        map.insert("logs".to_string(), json!(logs));
    }
    report
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::{Path, PathBuf};

    use crate::executor::meta::ExecutorMeta;
    use crate::executor::tool::ExecutorTool;
    use crate::tool::Tool;

    fn workspace() -> (tempfile::TempDir, PathBuf) {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("executors");
        (tmp, root)
    }

    fn tool(root: &Path) -> ExecutorTool {
        ExecutorTool::new(root.to_path_buf(), Arc::new(BTreeMap::new()), 0)
    }

    async fn run(root: &Path, args: Value) -> Value {
        let text = tool(root).execute(args).await.unwrap();
        serde_json::from_str(&text).unwrap()
    }

    fn on_disk(root: &Path, id: &str) -> ExecutorMeta {
        let text = std::fs::read_to_string(root.join(id).join("meta.json")).unwrap();
        serde_json::from_str(&text).unwrap()
    }

    // ---- run -----------------------------------------------------------------

    /// Create one executor with a script and return its id.
    async fn create(root: &Path, extra: Value) -> String {
        let mut args = json!({"action": "create", "name": "act"});
        for (key, value) in extra.as_object().unwrap() {
            args[key] = value.clone();
        }
        let out = run(root, args).await;
        out["id"].as_str().unwrap().to_string()
    }

    const SCHEMA: &str =
        r#"{"type":"object","required":["title"],"properties":{"title":{"type":"string"}}}"#;

    #[tokio::test]
    async fn test_run_returns_the_contract_with_args_and_logs() {
        let (_tmp, root) = workspace();
        let id = create(
            &root,
            json!({
                "args_schema": serde_json::from_str::<Value>(SCHEMA).unwrap(),
                "script": "async function act(ctx) { \
                     ctx.log('acting on ' + ctx.args.title); \
                     return { ok: true, summary: 'filed ' + ctx.args.title, evidence: [{title: 'kept'}] }; }"
            }),
        )
        .await;

        let out = run(
            &root,
            json!({"action": "run", "id": id, "args": {"title": "the bug"}}),
        )
        .await;

        assert_eq!(out["ok"], true, "{out}");
        assert_eq!(out["summary"], "filed the bug", "{out}");
        assert_eq!(out["evidence"][0]["title"], "kept", "{out}");
        assert_eq!(out["ran"], true);
        assert_eq!(out["failure"], Value::Null, "{out}");
        assert_eq!(out["executor"], id);
        // The script's own log line is the run's progress record.
        assert_eq!(out["logs"][0], "acting on the bug", "{out}");
    }

    #[tokio::test]
    async fn test_run_treats_omitted_args_as_an_empty_object() {
        let (_tmp, root) = workspace();
        let id = create(
            &root,
            json!({"script": "async function act(ctx) { return {ok: true, summary: String(Object.keys(ctx.args).length)}; }"}),
        )
        .await;

        let out = run(&root, json!({"action": "run", "id": id})).await;
        assert_eq!(out["summary"], "0", "{out}");
    }

    #[tokio::test]
    async fn test_run_refuses_malformed_args_without_starting_the_script() {
        let (_tmp, root) = workspace();
        let id = create(
            &root,
            json!({
                "args_schema": serde_json::from_str::<Value>(SCHEMA).unwrap(),
                // A script that would leave a trace if it ever ran.
                "script": "async function act(ctx) { ctx.log('the script ran'); return {ok: true, summary: 'ran'}; }"
            }),
        )
        .await;

        let out = run(
            &root,
            json!({"action": "run", "id": id, "args": {"name": "no title"}}),
        )
        .await;

        assert_eq!(out["ok"], false, "{out}");
        assert_eq!(out["failure"], "rejected", "{out}");
        assert_eq!(out["ran"], false, "the sandbox must never see this call");
        assert_eq!(out["external_calls"], 0);
        assert!(out.get("logs").is_none(), "no script log can exist: {out}");
        let summary = out["summary"].as_str().unwrap();
        assert!(summary.contains("`args.title` is required"), "{summary}");
        assert!(summary.contains("Next: "), "{summary}");
    }

    #[tokio::test]
    async fn test_run_refuses_args_of_the_wrong_type_by_field_name() {
        let (_tmp, root) = workspace();
        let id = create(
            &root,
            json!({
                "args_schema": serde_json::from_str::<Value>(SCHEMA).unwrap(),
                "script": "async function act(ctx) { return {ok: true, summary: 'ran'}; }"
            }),
        )
        .await;

        let out = run(
            &root,
            json!({"action": "run", "id": id, "args": {"title": 7}}),
        )
        .await;
        assert_eq!(out["ran"], false, "{out}");
        assert!(
            out["summary"]
                .as_str()
                .unwrap()
                .contains("`args.title` must be string, got integer"),
            "{out}"
        );
    }

    #[tokio::test]
    async fn test_run_without_an_id_or_with_an_unknown_id_does_not_run() {
        let (_tmp, root) = workspace();
        let out = run(&root, json!({"action": "run"})).await;
        assert_eq!(out["ran"], false);
        assert!(
            out["summary"]
                .as_str()
                .unwrap()
                .contains("`id` is required")
        );

        let out = run(&root, json!({"action": "run", "id": "exec-0000"})).await;
        assert_eq!(out["ran"], false);
        assert!(
            out["summary"]
                .as_str()
                .unwrap()
                .contains("no executor with id 'exec-0000'")
        );
    }

    #[tokio::test]
    async fn test_run_refuses_a_broken_executor_before_anything_runs() {
        let (_tmp, root) = workspace();
        // Half-built: meta only, no `act.js`.
        let id = create(&root, json!({})).await;
        let out = run(&root, json!({"action": "run", "id": id})).await;
        assert_eq!(out["ran"], false, "{out}");
        assert_eq!(out["failure"], "rejected");
        assert!(out["summary"].as_str().unwrap().contains("act.js"), "{out}");
    }

    #[tokio::test]
    async fn test_run_reports_a_timeout_as_a_host_stop() {
        let (_tmp, root) = workspace();
        let id = create(
            &root,
            json!({
                "timeout_s": 1,
                "script": "async function act(ctx) { while (true) {} }"
            }),
        )
        .await;

        let out = run(&root, json!({"action": "run", "id": id})).await;
        assert_eq!(out["ok"], false, "{out}");
        assert_eq!(out["failure"], "host_stopped", "{out}");
        assert_eq!(out["ran"], true);
        assert!(out["summary"].as_str().unwrap().contains("budget"), "{out}");
    }

    #[tokio::test]
    async fn test_run_stops_the_script_when_the_call_budget_is_spent() {
        let (_tmp, root) = workspace();
        let id = create(
            &root,
            json!({
                "timeout_s": 30,
                // Every attempt is refused by the shared SSRF guard before it
                // connects, but each one still counts as an outward attempt.
                "script": "async function act(ctx) { \
                     for (let i = 0; i < 200; i++) { \
                         try { await ctx.http.get('http://127.0.0.1:9/x'); } catch (e) {} \
                     } \
                     return { ok: true, summary: 'kept going' }; }"
            }),
        )
        .await;

        let out = run(&root, json!({"action": "run", "id": id})).await;
        assert_eq!(out["ok"], false, "{out}");
        assert_eq!(out["failure"], "host_stopped", "{out}");
        let summary = out["summary"].as_str().unwrap();
        assert!(summary.contains("more than 20 external calls"), "{summary}");
        assert!(
            out["external_calls"].as_u64().unwrap() >= 21,
            "the counter must show the attempts: {out}"
        );
    }

    /// Without network a test can only provoke the outward failure the shared
    /// guard raises before connecting (a private target). The `answered HTTP
    /// 4xx/5xx` wording of the same class is pinned in `result.rs`.
    #[tokio::test]
    async fn test_run_reports_a_failed_outward_call_in_band() {
        let (_tmp, root) = workspace();
        let id = create(
            &root,
            json!({
                "script": "async function act(ctx) { \
                     const r = await ctx.http.get('http://127.0.0.1:9/x'); \
                     return { ok: true, summary: String(r.status) }; }"
            }),
        )
        .await;

        let out = run(&root, json!({"action": "run", "id": id})).await;
        assert_eq!(out["ok"], false, "{out}");
        assert_eq!(out["failure"], "external_error", "{out}");
        // Root cause, not just "it failed".
        assert!(
            out["summary"]
                .as_str()
                .unwrap()
                .contains("http://127.0.0.1:9/x"),
            "{out}"
        );
    }

    #[tokio::test]
    async fn test_run_refuses_a_result_over_the_act_output_ceiling() {
        let (_tmp, root) = workspace();
        let id = create(
            &root,
            json!({
                "script": "async function act(ctx) { return {ok: true, summary: 'x'.repeat(20000)}; }"
            }),
        )
        .await;

        let out = run(&root, json!({"action": "run", "id": id})).await;
        assert_eq!(out["ok"], false, "{out}");
        assert_eq!(out["failure"], "host_stopped", "{out}");
        assert!(out["summary"].as_str().unwrap().contains("16384"), "{out}");
    }

    #[tokio::test]
    async fn test_run_trims_each_evidence_item_to_the_item_budget() {
        let (_tmp, root) = workspace();
        let id = create(
            &root,
            json!({
                "script": "async function act(ctx) { \
                     return { ok: true, summary: 'read it', \
                              evidence: [{url: 'https://example.test/a', text: 'x'.repeat(9000)}] }; }"
            }),
        )
        .await;

        let out = run(&root, json!({"action": "run", "id": id})).await;
        let item = &out["evidence"][0];
        let bytes = serde_json::to_string(item).unwrap().len();
        assert!(bytes <= 4 * 1024, "item kept {bytes} bytes");
        assert_eq!(item["url"], "https://example.test/a", "{out}");
        assert!(out["summary"].as_str().unwrap().contains("4KB"), "{out}");
    }

    #[tokio::test]
    async fn test_run_reports_a_broken_result_shape_as_a_script_error() {
        let (_tmp, root) = workspace();
        let id = create(
            &root,
            json!({"script": "async function act(ctx) { return {summary: 'forgot ok'}; }"}),
        )
        .await;

        let out = run(&root, json!({"action": "run", "id": id})).await;
        assert_eq!(out["failure"], "script_error", "{out}");
        assert!(out["summary"].as_str().unwrap().contains("`ok`"), "{out}");
    }

    #[tokio::test]
    async fn test_run_reads_the_executor_fresh_from_disk() {
        let (_tmp, root) = workspace();
        let id = create(
            &root,
            json!({"script": "async function act(ctx) { return {ok: true, summary: 'first'}; }"}),
        )
        .await;
        let first = run(&root, json!({"action": "run", "id": id.clone()})).await;
        assert_eq!(first["summary"], "first");

        // A hand-edited script is what the next run executes: scan-on-call.
        crate::executor::meta::save_script(
            &root,
            &id,
            "async function act(ctx) { return {ok: true, summary: 'second'}; }",
        )
        .unwrap();
        let second = run(&root, json!({"action": "run", "id": id})).await;
        assert_eq!(second["summary"], "second");
    }

    // ---- connector writes (ticket 15) ---------------------------------------

    use serde_json::json as j;
    use std::sync::Arc;

    use crate::approval::{ApprovalConfig, ApprovalStore, ControlLevel};
    use crate::mcp::McpService;
    use crate::mcp::service::test_support as mcp_test;
    use crate::runner::gate::{GateContext, RunToolExec};
    use crate::session::{TaskHook, TurnKind};
    use crate::tool::{ToolManager, with_turn_context};

    /// A started pool over one fake server; the runtime must outlive the test so
    /// the supervisor keeps running.
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

    /// The gate an executor run always has in production: an unattended
    /// subagent's own `RunToolExec`, whose pool the act surface shares.
    fn gate(
        pool: Option<Arc<McpService>>,
        controls: Vec<(&str, ControlLevel)>,
    ) -> (Arc<RunToolExec>, Arc<ApprovalStore>) {
        let manager = ToolManager::new(PathBuf::from("/tmp/slimbot-executor-tool-test"));
        let mut config = ApprovalConfig::default();
        for (name, level) in controls {
            config.tools.insert(name.to_string(), level);
        }
        let approvals = Arc::new(ApprovalStore::new(config));
        let exec = Arc::new(RunToolExec::new(GateContext {
            session_id: "cli:c1".to_string(),
            task_hook: TaskHook::new("cli:c1").with_turn_kind(TurnKind::Subagent),
            tool_manager: Arc::new(manager),
            approvals: approvals.clone(),
            connectors: pool,
            tracer: None,
            cancel_token: None,
        }));
        (exec, approvals)
    }

    /// `executor run`, executed inside the turn scope the runner opens for a tool
    /// call — the task-local is how the tool reaches its gate.
    async fn run_in_turn(root: &Path, args: Value, exec: Arc<RunToolExec>) -> Value {
        let turn = Arc::new(
            crate::tool::TurnContext::new("cli".to_string(), "c1".to_string()).with_tool_exec(exec),
        );
        let text = with_turn_context(turn, tool(root).execute(args))
            .await
            .expect("the tool never fails on an in-band refusal");
        serde_json::from_str(&text).unwrap()
    }

    /// One unannotated connector tool: no read-only claim, so it is a write.
    fn write_tool() -> Vec<Value> {
        vec![j!({"name": "create_issue", "description": "file an issue"})]
    }

    /// Build the receipt out of the connector's own answer, the way a real
    /// action script would: the `<codemode_output>` envelope carries the body.
    const RECEIPT_SCRIPT: &str = "async function act(ctx) { \
         const raw = await ctx.tools.mcp__github__create_issue({ title: ctx.args.title }); \
         const body = raw.slice(raw.indexOf('>') + 1, raw.lastIndexOf('</codemode_output')); \
         const created = JSON.parse(body.trim()); \
         return { ok: true, summary: 'filed issue #' + created.number, \
                  evidence: [{ url: created.html_url }] }; \
     }";

    #[test]
    fn test_run_creates_an_external_object_through_an_allowed_connector_write() {
        // Demo (a): the executor writes through a connector whose server its
        // `allow_connectors` lists, the owner's policy allows the callable, and
        // the receipt the connector answered with reaches `{ok, summary,
        // evidence?}`.
        let (pool, peer, rt) = online_pool("github", write_tool());
        peer.set_answer(r#"{"number": 42, "html_url": "https://github.test/acme/repo/issues/42"}"#);
        let (exec, _approvals) = gate(
            Some(pool),
            vec![("mcp__github__create_issue", ControlLevel::Allow)],
        );
        let (_tmp, root) = workspace();
        let id = rt.block_on(create(
            &root,
            j!({"allow_connectors": ["github"], "script": RECEIPT_SCRIPT}),
        ));

        let out = rt.block_on(run_in_turn(
            &root,
            j!({"action": "run", "id": id, "args": {"title": "the bug"}}),
            exec,
        ));

        assert_eq!(out["ok"], true, "{out}");
        assert_eq!(out["summary"], "filed issue #42", "{out}");
        assert_eq!(
            out["evidence"][0]["url"], "https://github.test/acme/repo/issues/42",
            "the receipt travels into evidence: {out}"
        );
        assert_eq!(out["failure"], Value::Null, "{out}");
        assert_eq!(out["ran"], true);
        assert_eq!(out["external_calls"], 1, "{out}");
        assert_eq!(peer.calls(), 1, "the connector was asked exactly once");
    }

    #[test]
    fn test_run_reports_an_unattended_approval_refusal_and_leaves_no_pending() {
        // Demo (b): the same write, but the policy is the default (`Ask` for
        // `ExternalSend`). An executor run is unattended, so the call is refused,
        // the script stops, and — the part a text assertion cannot prove — no
        // pending approval is stored for a next turn that will never come.
        let (pool, peer, rt) = online_pool("github", write_tool());
        let (exec, approvals) = gate(Some(pool), vec![]);
        let (_tmp, root) = workspace();
        let id = rt.block_on(create(
            &root,
            j!({"allow_connectors": ["github"], "script": RECEIPT_SCRIPT}),
        ));

        let out = rt.block_on(run_in_turn(
            &root,
            j!({"action": "run", "id": id, "args": {"title": "the bug"}}),
            exec,
        ));

        assert_eq!(out["ok"], false, "{out}");
        assert_eq!(out["failure"], "approval_refused", "{out}");
        assert_eq!(out["ran"], true);
        let summary = out["summary"].as_str().unwrap();
        assert!(summary.contains("needs owner authorization"), "{summary}");
        assert!(summary.contains("unattended"), "{summary}");
        assert!(summary.contains("no confirmation was queued"), "{summary}");
        assert_eq!(peer.calls(), 0, "nothing was sent: {out}");
        assert!(
            !approvals.has_pending("cli:c1"),
            "an unattended run must not queue an approval"
        );
    }

    #[test]
    fn test_run_without_a_gate_has_no_connector_surface() {
        // Fail-closed: outside a runner turn there is no gate, so no connector is
        // installed at all. (Production always runs inside one.)
        let (pool, peer, rt) = online_pool("github", write_tool());
        let (_exec, _approvals) = gate(Some(pool), vec![]);
        let (_tmp, root) = workspace();
        let id = rt.block_on(create(
            &root,
            j!({"allow_connectors": ["github"], "script": "async function act(ctx) { \
                 return { ok: true, summary: typeof ctx.tools.mcp__github__create_issue }; }"}),
        ));

        let out = rt.block_on(run(&root, j!({"action": "run", "id": id})));
        assert_eq!(out["summary"], "undefined", "{out}");
        assert_eq!(out["ok"], true, "{out}");
        assert_eq!(peer.calls(), 0);
    }

    #[test]
    fn test_a_misspelt_connector_name_is_stored_and_fails_closed_at_run_time() {
        // `create` cannot check `allow_connectors` against the live pool (see
        // `replace_given`), so the typo has to be reported by the run instead:
        // nothing is sent, and the ceiling names the server it could not match.
        let (pool, peer, rt) = online_pool("github", write_tool());
        let (exec, _approvals) = gate(
            Some(pool),
            vec![("mcp__github__create_issue", ControlLevel::Allow)],
        );
        let (_tmp, root) = workspace();
        let id = rt.block_on(create(
            &root,
            j!({"allow_connectors": ["githbu"], "script": RECEIPT_SCRIPT}),
        ));
        assert_eq!(
            on_disk(&root, &id).allow_connectors,
            vec!["githbu".to_string()]
        );

        let out = rt.block_on(run_in_turn(
            &root,
            j!({"action": "run", "id": id, "args": {"title": "x"}}),
            exec,
        ));

        assert_eq!(out["failure"], "host_stopped", "{out}");
        let summary = out["summary"].as_str().unwrap();
        // The reason names the server the callable really belongs to, so a typo
        // in `allow_connectors` shows up as a mismatch the reader can see,
        // instead of being silently allowed or reported as an unknown tool.
        assert!(summary.contains("`github`"), "{summary}");
        assert!(summary.contains("allow_connectors"), "{summary}");
        assert_eq!(peer.calls(), 0, "nothing was sent: {out}");
    }

    #[test]
    fn test_run_trims_connector_content_like_any_other_evidence() {
        // ADR 0009: external content is context, not memory, and enters the
        // executor's report one item at a time under the 4KB budget. A connector
        // answer is external content — the same trim applies, no exemption.
        let (pool, peer, rt) = online_pool("github", write_tool());
        peer.set_answer("x".repeat(8 * 1024));
        let (exec, _approvals) = gate(
            Some(pool),
            vec![("mcp__github__create_issue", ControlLevel::Allow)],
        );
        let (_tmp, root) = workspace();
        let id = rt.block_on(create(
            &root,
            j!({"allow_connectors": ["github"], "script": "async function act(ctx) { \
                 const raw = await ctx.tools.mcp__github__create_issue({title: 'x'}); \
                 return { ok: true, summary: 'filed', evidence: [{ receipt: raw }] }; }"}),
        ));

        let out = rt.block_on(run_in_turn(&root, j!({"action": "run", "id": id}), exec));

        assert_eq!(out["ok"], true, "{out}");
        let receipt = out["evidence"][0]["receipt"].as_str().unwrap();
        assert!(
            receipt.len() <= 4096,
            "the connector's 8KB answer must be trimmed to the item budget, got {}",
            receipt.len()
        );
        assert!(!receipt.is_empty());
    }
}
