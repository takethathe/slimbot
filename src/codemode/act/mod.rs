//! The act entry's capability set (tickets 12 and 15).
//!
//! An executor's action script (`act.js`, ticket 12/13) runs under
//! [`ENTRY_ACT`](super::sandbox::ENTRY_ACT) with a deliberately narrow surface:
//! the shared time trio, `log`, `secret`, the read-side HTTP primitive
//! (`ctx.http.get` / `ctx.http.post`), and the connector tools (`ctx.tools` /
//! `ctx.searchTools`, ticket 15).
//!
//! What is missing is the point. There is no shell (the action domain faces
//! outward only), no `ctx.fs` (an action's local data arrives as `ctx.args`,
//! read by the calling subagent with its own file tools), and no raw HTTP write
//! verb: an unvetted `ctx.http.put` carries no intrinsic risk level, so the
//! approval gate could never hold it — it would be a backdoor around the
//! "every outward change is a gated connector tool call" rule. Reads and
//! `post`-as-query stay because they are what the collect entry already ships.
//!
//! **The connector surface is connectors only.** `ctx.tools.*` never carries a
//! built-in tool, whatever the run's `ToolManager` holds: an act script reaches
//! the outside world through a connector or not at all. The write side is
//! limited to the servers named in the executor's `allow_connectors`; a write
//! outside that ceiling is installed as a tripwire that stops the run. That
//! ceiling is a *capability* — it says which servers this executor was created
//! to touch — and it is orthogonal to the approval gate, which decides per call
//! from the tool's own risk class (`ExternalSend`, always) whether the action
//! may run at all (ADR 0011). Neither gate feeds the other.
//!
//! The capability list is the only safety boundary, as everywhere in this
//! module: the sandbox never inspects the script text.
//!
//! Two numeric gates ride the outward-call path rather than the script text:
//! the per-run call budget (`MAX_EXTERNAL_CALLS`, connector calls and
//! `ctx.http` together, one counter) and the provenance of an outward call that
//! did not answer as needed. Both are accounted for below the shims, so a
//! script cannot skip them by calling the raw `ctx.http_get` primitive.
//!
//! **Any refusal stops the whole run.** An executor runs in an ephemeral
//! unattended subagent: there is no next turn in which to redeem an approval,
//! so `Ask` is a refusal (the gate says so) and `Deny` is a refusal, and both
//! end the script through the same uncatchable [`AbortHandle`] the ceiling and
//! the call budget use. The agent entry deliberately does the opposite — it
//! answers the refusal text and keeps going — because that entry *does* have a
//! next turn. This is the one place the two entries' post-decision behaviour
//! differs, and it lives here, not in the shared gate.
//!
//! What `act()` returns is *not* validated here: the sandbox guarantees only
//! parseable JSON within [`Limits::act`](super::sandbox::Limits::act)'s 16KB
//! output ceiling. The `{ok, summary, evidence?}` contract, its 4KB per-item
//! evidence trim and the failure classification live in
//! [`crate::executor::result`], on the caller side, right after
//! `execute_async`.
//!
//! The module is split by change reason. The fixed call budget and the abort
//! wiring it shares with `ctx.http` live in [`budget`]; the connector write
//! side — catalog, `allow_connectors` ceiling, tripwire — in [`connectors`].
//! What stays here is what every act run has in common: the primitives and the
//! HTTP hooks.

mod budget;
mod connectors;

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use rquickjs::Ctx;

use super::host::{HostBindings, HostEnv, LogSink, install_log, install_secret, install_time};
use super::http::{HttpCallHooks, install_http_with};
use super::sandbox::AbortHandle;
use crate::runner::gate::RunToolExec;
use crate::tool::TurnContext;

use self::budget::CallBudget;

/// Bindings for one act-entry execution.
pub(crate) struct ActHost {
    secrets: Arc<BTreeMap<String, String>>,
    logs: Arc<Mutex<Vec<String>>>,
    /// Every outward call of the run: `ctx.http` and the connector surface
    /// charge the same counter, so the budget is one gate rather than two.
    budget: CallBudget,
    /// `None` when the run has no gate to route connector calls through, which
    /// also means no connector surface (there would be nothing to authorize).
    connectors: Option<connectors::ActConnectors>,
    /// Root cause of the last outward call that did not answer as needed, for
    /// the caller's external-failure classification. Not a security signal: it
    /// only decides how a failure is reported.
    outward_failure: Arc<Mutex<Option<String>>>,
}

impl ActHost {
    pub(crate) fn new(secrets: Arc<BTreeMap<String, String>>) -> Self {
        Self {
            secrets,
            logs: Arc::new(Mutex::new(Vec::new())),
            budget: CallBudget::new(),
            connectors: None,
            outward_failure: Arc::new(Mutex::new(None)),
        }
    }

    /// Share the run's abort switch, so a spent budget or a refused call stops
    /// the script uncatchably.
    pub(crate) fn with_abort(mut self, abort: Arc<AbortHandle>) -> Self {
        self.budget = self.budget.with_abort(abort);
        self
    }

    /// Install the connector surface: every online connector is reachable as
    /// `ctx.tools.mcp__*` (and searchable), reads on every server, writes only
    /// on the servers in `allow` (ticket 15).
    ///
    /// `allow` is the executor's `allow_connectors`, a **capability** ceiling
    /// fixed before the run. It never decides an approval level: every call
    /// still goes through the gate, whose class comes from the tool's own risk
    /// (always `ExternalSend` for a connector).
    pub(crate) fn with_connectors(
        mut self,
        exec: Arc<RunToolExec>,
        turn: Option<Arc<TurnContext>>,
        allow: Vec<String>,
    ) -> Self {
        self.connectors = Some(connectors::ActConnectors::new(exec, turn, allow));
        self
    }

    /// Take the lines the script logged, oldest first.
    pub(crate) fn drain_logs(&self) -> Vec<String> {
        std::mem::take(&mut *self.logs.lock().unwrap_or_else(|p| p.into_inner()))
    }

    /// Take the recorded external root cause, if an outward call failed.
    pub(crate) fn take_outward_failure(&self) -> Option<String> {
        self.outward_failure
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .take()
    }

    /// How many outward calls the run has attempted.
    pub(crate) fn external_calls(&self) -> usize {
        self.budget.external_calls()
    }
}

/// The accounting half of [`ActHost`], handed to the shared HTTP primitive as a
/// plain `Arc<dyn HttpCallHooks>` so the disposable borrow does not leak into
/// `install`.
struct OutwardCalls {
    budget: CallBudget,
    failure: Arc<Mutex<Option<String>>>,
}

impl HttpCallHooks for OutwardCalls {
    fn before(&self, _method: &str, _url: &str) -> Option<String> {
        self.budget.charge()
    }

    fn after(&self, method: &str, url: &str, outcome: Result<u16, &str>) {
        let Some(failure) = describe_outward_failure(method, url, outcome) else {
            return;
        };
        *self.failure.lock().unwrap_or_else(|p| p.into_inner()) = Some(failure);
    }
}

/// How to describe an outward call that did not answer as needed: an error
/// status or a transport failure, both of which make the action fail for a
/// reason the executor itself did not cause.
fn describe_outward_failure(method: &str, url: &str, outcome: Result<u16, &str>) -> Option<String> {
    match outcome {
        Ok(status) if status >= 400 => Some(format!("{method} {url} answered HTTP {status}")),
        Ok(_) => None,
        Err(error) => Some(format!("{method} {url} failed: {error}")),
    }
}

impl HostBindings for ActHost {
    fn install(&self, ctx: &Ctx<'_>, env: &HostEnv) -> rquickjs::Result<()> {
        install_time(ctx, env)?;
        let logs: LogSink = {
            let logs = self.logs.clone();
            Arc::new(move |line: &str| {
                logs.lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .push(line.to_string());
            })
        };
        install_log(ctx, env, logs)?;
        install_secret(ctx, self.secrets.clone())?;
        install_http_with(
            ctx,
            Some(Arc::new(OutwardCalls {
                budget: self.budget.clone(),
                failure: self.outward_failure.clone(),
            })),
        )?;
        connectors::install(ctx, self.connectors.as_ref(), self.budget.clone())?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::sync::Arc;
    use std::time::Duration;

    use serde_json::Value;

    use super::*;
    use crate::codemode::error::ScriptError;
    use crate::codemode::host::HostEnv;
    use crate::codemode::redact::Redactor;
    use crate::codemode::sandbox::{AbortKind, ENTRY_ACT, Limits, RunRequest, execute};

    use super::budget::MAX_EXTERNAL_CALLS;

    fn secrets() -> Arc<BTreeMap<String, String>> {
        Arc::new(BTreeMap::from([(
            "token".to_string(),
            "s3cr3t-token".to_string(),
        )]))
    }

    fn run_act_with(script: &str, host: Arc<ActHost>) -> Result<Value, ScriptError> {
        execute(
            RunRequest::new(script, ENTRY_ACT)
                .with_limits(Limits::act().with_timeout(Duration::from_secs(5))),
            host,
            HostEnv::new(Redactor::new(["s3cr3t-token"])),
        )
    }

    fn run_act(script: &str) -> Result<Value, ScriptError> {
        run_act_with(script, Arc::new(ActHost::new(secrets())))
    }

    fn run_act_with_abort(script: &str) -> (Result<Value, ScriptError>, Arc<AbortHandle>) {
        let abort = Arc::new(AbortHandle::new());
        let host = Arc::new(ActHost::new(secrets()).with_abort(abort.clone()));
        let outcome = execute(
            RunRequest::new(script, ENTRY_ACT)
                .with_limits(Limits::act().with_timeout(Duration::from_secs(5)))
                .with_abort(abort.clone()),
            host,
            HostEnv::new(Redactor::new(["s3cr3t-token"])),
        );
        (outcome, abort)
    }

    /// A target the shared SSRF guard refuses before connecting: every attempt
    /// is counted, but no test touches the network.
    const REFUSED_URL: &str = "http://127.0.0.1:9/x";

    #[test]
    fn test_act_entry_allows_exactly_the_call_budget() {
        let script = format!(
            r#"async function act(ctx) {{
                let refused = 0;
                for (let i = 0; i < {MAX_EXTERNAL_CALLS}; i++) {{
                    try {{ await ctx.http.get("{REFUSED_URL}"); }} catch (e) {{ refused++; }}
                }}
                return {{ ok: true, summary: 'tried ' + refused }};
            }}"#
        );
        let (outcome, abort) = run_act_with_abort(&script);
        let value = outcome.expect("the budget must allow exactly its own size");
        assert_eq!(value["summary"], format!("tried {MAX_EXTERNAL_CALLS}"));
        assert!(!abort.is_aborted());
    }

    #[test]
    fn test_act_entry_stops_the_script_when_the_call_budget_is_spent() {
        // One attempt past the ceiling: the run is stopped, not merely answered
        // with an error the script could swallow and keep going.
        let script = format!(
            r#"async function act(ctx) {{
                for (let i = 0; i < {}; i++) {{
                    try {{ await ctx.http.get("{REFUSED_URL}"); }} catch (e) {{}}
                }}
                return {{ ok: true, summary: 'kept going' }};
            }}"#,
            MAX_EXTERNAL_CALLS + 5
        );
        let (outcome, abort) = run_act_with_abort(&script);
        let err = outcome.unwrap_err();
        assert_eq!(err.code(), "aborted", "{err}");
        assert!(
            err.message
                .contains(&format!("more than {MAX_EXTERNAL_CALLS} external calls")),
            "{err}"
        );
        // The kind is what lets the caller report a host stop, not a refusal.
        assert_eq!(abort.kind(), Some(AbortKind::Stopped));
    }

    #[test]
    fn test_act_entry_records_no_root_cause_for_an_answered_call() {
        let host = Arc::new(ActHost::new(secrets()));
        // The guard refuses before any response exists, so this one *is* a
        // root cause; a 2xx/3xx would not be.
        run_act_with(
            &format!(
                r#"async function act(ctx) {{
                    try {{ await ctx.http.get("{REFUSED_URL}"); }} catch (e) {{}}
                    return {{ ok: true, summary: 'ignored' }};
                }}"#
            ),
            host.clone(),
        )
        .expect("a caught failure must not stop the script");
        let failure = host
            .take_outward_failure()
            .expect("a root cause is recorded");
        assert!(failure.contains(REFUSED_URL), "{failure}");
        assert!(host.take_outward_failure().is_none(), "taking drains it");
    }

    #[test]
    fn test_describe_outward_failure_marks_error_statuses_and_transport_errors() {
        assert_eq!(
            describe_outward_failure("POST", "https://api.test/items", Ok(503)).as_deref(),
            Some("POST https://api.test/items answered HTTP 503")
        );
        assert_eq!(
            describe_outward_failure("GET", "https://api.test/items", Ok(200)),
            None
        );
        assert_eq!(
            describe_outward_failure("GET", "https://api.test/items", Ok(204)),
            None
        );
        assert_eq!(
            describe_outward_failure("GET", "https://api.test/x", Err("connection refused"))
                .as_deref(),
            Some("GET https://api.test/x failed: connection refused")
        );
    }

    #[test]
    fn test_act_entry_installs_time_log_and_secret() {
        let host = Arc::new(ActHost::new(secrets()));
        let value = run_act_with(
            r#"async function act(ctx) {
                const t = ctx.now();
                ctx.log("act logged this with " + ctx.secret("token"));
                return {
                    epoch: t.epoch_ms,
                    boot: ctx.boot_at(),
                    uptime: ctx.uptime_s(),
                    // The real value reaches the script; echoing it back is
                    // redacted on the way out.
                    match: ctx.secret("token") === "s3cr3t-token",
                    echoed: ctx.secret("token"),
                };
            }"#,
            host.clone(),
        )
        .expect("probe must run");

        assert!(value["epoch"].as_i64().unwrap() > 0, "{value}");
        assert!(value["boot"].as_i64().unwrap() > 0, "{value}");
        assert!(value["uptime"].as_i64().unwrap() >= 0, "{value}");
        assert_eq!(
            value["match"], true,
            "the secret value must reach the script"
        );
        assert_eq!(value["echoed"], "***", "an echoed secret is redacted");

        // The log sink received the line, with the secret already redacted.
        let logs = host.drain_logs().join("\n");
        assert!(logs.contains("act logged this"), "{logs}");
        assert!(!logs.contains("s3cr3t-token"), "log leaked the secret");
    }

    #[test]
    fn test_act_entry_unknown_secret_is_an_error_not_an_empty_string() {
        let err = run_act(
            r#"async function act(ctx) {
                return { ok: true, summary: ctx.secret("nope") };
            }"#,
        )
        .unwrap_err();
        assert_eq!(err.code(), "exception");
        assert!(err.message.contains("no secret named 'nope'"), "{err}");
    }

    #[test]
    fn test_act_entry_http_surface_is_read_side_get_and_post() {
        // `get` and `post` are the collect entry's existing HTTP surface, wired
        // to the same implementation: a call reaches the shared SSRF guard and
        // is refused before any socket opens.
        let value = run_act(
            r#"async function act(ctx) {
                const attempts = {};
                for (const [name, call] of [
                    ["get", () => ctx.http.get("http://127.0.0.1:8080/x")],
                    ["post", () => ctx.http.post("http://127.0.0.1:8080/x", {body: "{}"})],
                ]) {
                    try { await call(); attempts[name] = "reached"; }
                    catch (e) { attempts[name] = String(e && e.message || e); }
                }
                return { keys: Object.keys(ctx.http).sort(), attempts };
            }"#,
        )
        .expect("probe must run");

        assert_eq!(value["keys"], serde_json::json!(["get", "post"]));
        for method in ["get", "post"] {
            let outcome = value["attempts"][method].as_str().unwrap();
            assert!(outcome.contains("private"), "{method}: {outcome}");
        }
    }

    #[test]
    fn test_act_entry_capability_surface_is_exact() {
        // The capability list is the whole safety boundary, so it is pinned to
        // an exact set — and every name in it is exercised, not just listed.
        // `http_get` / `http_post` are the JSON primitives `ctx.http` is built
        // from; the collect entry exposes them the same way.
        // `tools` / `searchTools` are part of the surface but not exercised
        // here beyond existence: the connector tests own their contents, and
        // with no pool the namespace is empty.
        let host = Arc::new(ActHost::new(secrets()));
        let value = run_act_with(
            r#"async function act(ctx) {
                const usable = {};
                ctx.log("surface probe");
                usable.log = true;
                usable.now = ctx.now().epoch_ms > 0;
                usable.boot_at = ctx.boot_at() > 0;
                usable.uptime_s = ctx.uptime_s() >= 0;
                usable.secret = ctx.secret("token") === "s3cr3t-token";
                usable.http = typeof ctx.http.get === "function"
                    && typeof ctx.http.post === "function";
                usable.http_get = typeof ctx.http_get === "function";
                usable.http_post = typeof ctx.http_post === "function";
                usable.tools = Object.keys(ctx.tools).length === 0;
                usable.searchTools = JSON.parse(await ctx.searchTools("anything")).length === 0;
                const keys = Object.keys(ctx).sort();
                return { keys, usable, every: keys.every((k) => usable[k] === true) };
            }"#,
            host.clone(),
        )
        .expect("probe must run");

        assert_eq!(
            value["keys"],
            serde_json::json!([
                "boot_at",
                "http",
                "http_get",
                "http_post",
                "log",
                "now",
                "searchTools",
                "secret",
                "tools",
                "uptime_s"
            ]),
            "{value}"
        );
        assert_eq!(
            value["every"], true,
            "every installed name must work: {value}"
        );
        assert!(
            host.drain_logs().iter().any(|line| line == "surface probe"),
            "log must reach the sink"
        );
    }

    #[test]
    fn test_act_entry_forbidden_capabilities_are_unreachable() {
        // The reverse pin: local writes, a shell, raw HTTP writes and built-in
        // tools are absent by name *and* throw when called. Nothing here sniffs
        // the script text; a capability is unreachable because it was never
        // installed. `tools` / `searchTools` exist as namespaces (their contents
        // are pinned by the connector tests), so they are not in this list.
        let value = run_act(
            r#"async function act(ctx) {
                const present = [];
                for (const name of [
                    "fs", "fs_stat", "fs_read", "fs_list", "shell", "exec",
                    "store", "subagent", "message",
                    "file_reader", "file_writer", "file_editor", "dir_list",
                    "grep", "find_files", "web_fetch", "memory_search",
                    "executor",
                ]) {
                    if (typeof ctx[name] !== "undefined") present.push(name);
                }

                const reached = [];
                for (const [label, call] of [
                    ["fs.read", () => ctx.fs.read("/etc/passwd")],
                    ["fs.write", () => ctx.fs.write("note.txt", "x")],
                    ["shell", () => ctx.shell("id")],
                    ["tools.message", () => ctx.tools.message({})],
                    ["tools.file_editor", () => ctx.tools.file_editor({})],
                    ["mcp__github__get_pull", () => ctx.mcp__github__get_pull({})],
                    ["http.put", () => ctx.http.put("http://example.com/x", {})],
                    ["http.delete", () => ctx.http.delete("http://example.com/x")],
                    ["http.patch", () => ctx.http.patch("http://example.com/x", {})],
                    ["http.request", () => ctx.http.request("GET", "http://example.com/x")],
                ]) {
                    try { await call(); reached.push(label); }
                    catch (e) { /* refuses: unreachable, as required */ }
                }

                const escapes = [];
                for (const name of ["require", "process", "fetch", "Bun", "Deno", "console"]) {
                    if (typeof globalThis[name] !== "undefined") escapes.push(name);
                }
                const evalReached = [];
                for (const code of ["require('fs')", "process.env", "fetch('http://x')"]) {
                    try { eval(code); evalReached.push(code); } catch (e) { }
                }

                return { present, reached, escapes, evalReached };
            }"#,
        )
        .expect("probe must run");

        assert_eq!(value["present"], serde_json::json!([]), "{value}");
        assert_eq!(value["reached"], serde_json::json!([]), "{value}");
        assert_eq!(value["escapes"], serde_json::json!([]), "{value}");
        assert_eq!(value["evalReached"], serde_json::json!([]), "{value}");
    }
}
