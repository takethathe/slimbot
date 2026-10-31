//! The script execution layer.
//!
//! Two very different scripts — a data-source collector and a trigger
//! condition — share one evaluator. Each execution gets:
//!
//! * a dedicated OS thread per execution, which exits with the script: the
//!   result travels back over a channel, so no thread ever outlives its run,
//!   and each thread carries its own current-thread tokio runtime, a `LocalSet`
//!   and a fresh QuickJS VM (building one costs ~0.1ms, so there is nothing to
//!   pool);
//! * a bare global scope: no modules, no timers, no `process`, no `console`.
//!   `eval` and `Function` stay available on purpose — dynamic code inside the
//!   VM is not an escape, and static script scanning must never be the security
//!   boundary;
//! * two independent deadlines: a watchdog thread flips the VM's interrupt
//!   flag so a spinning script is cut off mid-instruction, and an outer
//!   `tokio::time::timeout` catches a host call that never settles;
//! * a hard memory ceiling and a hard output ceiling.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use rquickjs::function::Async;
use rquickjs::{AsyncContext, AsyncRuntime, CaughtError, Ctx, Promise};
use serde_json::Value;

use super::error::{ScriptError, classify};
use super::host::{HostBindings, HostEnv};
use super::redact::Redactor;

/// Name of the function the caller expects the script to define. `collect`
/// returns `{items: [...]}`, `judge` returns `{fire, evidence?, detail?}`.
pub const ENTRY_COLLECT: &str = "collect";
pub const ENTRY_JUDGE: &str = "judge";

/// Budgets for one execution.
#[derive(Debug, Clone, Copy)]
pub struct Limits {
    pub timeout: Duration,
    pub memory_bytes: usize,
    pub max_output_bytes: usize,
}

impl Limits {
    /// Data-source collection: up to 10s (per-source `timeout_s` overrides the
    /// duration), 64MB, and a 64KB result ceiling.
    pub fn collect() -> Self {
        Self {
            timeout: Duration::from_secs(10),
            memory_bytes: 64 * 1024 * 1024,
            max_output_bytes: 64 * 1024,
        }
    }

    /// Conditions must be millisecond work: half a second, 16MB, and a small
    /// result. The trigger layer enforces the tighter 4KB evidence budget.
    pub fn judge() -> Self {
        Self {
            timeout: Duration::from_millis(500),
            memory_bytes: 16 * 1024 * 1024,
            max_output_bytes: 8 * 1024,
        }
    }

    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }
}

/// One script to evaluate.
pub struct RunRequest {
    /// Script source. Defines `collect(ctx)` or `judge(ctx)`.
    pub script: String,
    /// Entry function name, see [`ENTRY_COLLECT`] / [`ENTRY_JUDGE`].
    pub entry: String,
    /// Plain data merged into `ctx` (source views, previous store values...).
    pub ctx: Value,
    pub limits: Limits,
}

impl RunRequest {
    pub fn new(script: impl Into<String>, entry: &str) -> Self {
        Self {
            script: script.into(),
            entry: entry.to_string(),
            ctx: Value::Null,
            limits: Limits::judge(),
        }
    }

    pub fn with_ctx(mut self, ctx: Value) -> Self {
        self.ctx = ctx;
        self
    }

    pub fn with_limits(mut self, limits: Limits) -> Self {
        self.limits = limits;
        self
    }
}

/// Run a script to completion on the calling thread.
///
/// Blocks for at most `limits.timeout` (plus a small teardown margin). Never
/// panics and never takes the host process down: a panic inside the script
/// thread is reported as an `internal` error.
///
/// Production callers are async and use [`execute_async`]; this is the sync half,
/// kept for callers that have no runtime to hand and exercised by the tests.
#[cfg_attr(not(test), allow(dead_code))]
pub fn execute(
    request: RunRequest,
    host: Arc<dyn HostBindings>,
    env: HostEnv,
) -> Result<Value, ScriptError> {
    let (tx, rx) = std::sync::mpsc::channel();
    spawn(request, host, env, move |outcome| {
        let _ = tx.send(outcome);
    })?;
    rx.recv()
        .map_err(|_| ScriptError::internal("script thread vanished without a result"))?
}

/// Run a script without blocking the async runtime's worker threads.
pub async fn execute_async(
    request: RunRequest,
    host: Arc<dyn HostBindings>,
    env: HostEnv,
) -> Result<Value, ScriptError> {
    let (tx, rx) = tokio::sync::oneshot::channel();
    spawn(request, host, env, move |outcome| {
        let _ = tx.send(outcome);
    })?;
    rx.await
        .map_err(|_| ScriptError::internal("script thread vanished without a result"))?
}

/// Spawn the dedicated execution thread. The thread is detached: its result
/// travels through `done`, so the caller does not need the join handle.
fn spawn(
    request: RunRequest,
    host: Arc<dyn HostBindings>,
    env: HostEnv,
    done: impl FnOnce(Result<Value, ScriptError>) + Send + 'static,
) -> Result<(), ScriptError> {
    std::thread::Builder::new()
        .name("codemode".to_string())
        .spawn(move || {
            let outcome = run_on_own_thread(request, host, env);
            done(outcome);
        })
        .map(|_| ())
        .map_err(|e| ScriptError::internal(format!("could not start a script thread: {e}")))
}

fn run_on_own_thread(
    request: RunRequest,
    host: Arc<dyn HostBindings>,
    env: HostEnv,
) -> Result<Value, ScriptError> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| ScriptError::internal(format!("could not start the script runtime: {e}")))?;
    let local = tokio::task::LocalSet::new();
    local.block_on(&runtime, run_in_runtime(request, host, env))
}

async fn run_in_runtime(
    request: RunRequest,
    host: Arc<dyn HostBindings>,
    env: HostEnv,
) -> Result<Value, ScriptError> {
    let budget = request.limits.timeout;
    let budget_ms = budget.as_millis();
    let redactor = env.redactor().clone();
    let timed_out = Arc::new(AtomicBool::new(false));

    let js = AsyncRuntime::new()
        .map_err(|e| ScriptError::internal(format!("could not start the script engine: {e}")))?;

    // The interrupt flag: a script that never yields is cut off inside the VM,
    // which is the only way to stop `while (true) {}`.
    {
        let flag = timed_out.clone();
        js.set_interrupt_handler(Some(Box::new(move || flag.load(Ordering::SeqCst))))
            .await;
    }
    if request.limits.memory_bytes > 0 {
        js.set_memory_limit(request.limits.memory_bytes).await;
    }

    // Watchdog: a plain thread, because a spinning script keeps the runtime's
    // own timers from ever firing.
    let (stop_tx, stop_rx) = std::sync::mpsc::channel::<()>();
    {
        let flag = timed_out.clone();
        std::thread::spawn(move || {
            if stop_rx.recv_timeout(budget).is_err() {
                flag.store(true, Ordering::SeqCst);
            }
        });
    }

    let ctx = AsyncContext::full(&js)
        .await
        .map_err(|e| ScriptError::internal(format!("could not create a script context: {e}")))?;

    let work = ctx.async_with(async move |ctx: Ctx| {
        run_script(ctx, request, host, env, timed_out, budget_ms, redactor).await
    });

    let outcome = tokio::select! {
        // Completes only when the runtime is dropped, which cannot happen
        // while `js` is alive: this arm is here to keep the VM's job queue
        // driven so awaited host promises actually settle.
        _ = js.drive() => Err(ScriptError::internal("script engine stopped unexpectedly")),
        // Outer deadline: catches a host call that never settles, where the
        // interrupt flag can do nothing because no script code is running.
        result = tokio::time::timeout(budget, work) => match result {
            Ok(outcome) => outcome,
            Err(_) => Err(ScriptError::timeout(budget_ms)),
        },
    };

    let _ = stop_tx.send(());
    outcome
}

async fn run_script<'js>(
    ctx: Ctx<'js>,
    request: RunRequest,
    host: Arc<dyn HostBindings>,
    env: HostEnv,
    timed_out: Arc<AtomicBool>,
    budget_ms: u128,
    redactor: Redactor,
) -> Result<Value, ScriptError> {
    let fail = |error: CaughtError<'js>, ctx: &Ctx<'js>| -> ScriptError {
        classify(
            &describe_exception(&error, ctx),
            timed_out.load(Ordering::SeqCst),
            budget_ms,
            &redactor,
        )
    };

    host.install(&ctx, &env)
        .map_err(|e| ScriptError::internal(format!("could not install host functions: {e}")))?;

    // QuickJS ships `queueMicrotask`. Dropping it keeps the surface at "compute
    // plus whitelisted host calls": nothing in the VM schedules work on its own.
    let _ = ctx.eval::<(), _>("delete globalThis.queueMicrotask");

    // `ctx` is plain data; `__host` carries the functions. The script sees one
    // merged object so `ctx.log(...)`, `ctx.secret(...)` work uniformly.
    let payload = serde_json::to_string(&request.ctx)
        .map_err(|e| ScriptError::internal(format!("could not encode the script input: {e}")))?;
    ctx.globals()
        .set("__ctx_json", payload)
        .map_err(|e| ScriptError::internal(format!("could not seed the script context: {e}")))?;

    // Compile and bind the script. A syntax error surfaces here, before any
    // script code runs.
    if let Err(error) = ctx.eval::<(), _>(request.script.as_str()) {
        return Err(fail(CaughtError::from_error(&ctx, error), &ctx));
    }

    if ctx
        .globals()
        .get::<_, rquickjs::Function>(request.entry.as_str())
        .is_err()
    {
        return Err(ScriptError::missing_entry(&request.entry));
    }

    let wrapper = format!(
        "(async () => {{ const __ctx = Object.assign({{}}, JSON.parse(__ctx_json), __host); \
         const __r = await {}(__ctx); return __r === undefined ? \"null\" : JSON.stringify(__r); }})()",
        request.entry
    );
    let promise: Promise<'js> = match ctx.eval(wrapper.as_str()) {
        Ok(promise) => promise,
        Err(error) => return Err(fail(CaughtError::from_error(&ctx, error), &ctx)),
    };

    let text = match promise.into_future::<String>().await {
        Ok(text) => text,
        Err(error) => return Err(fail(CaughtError::from_error(&ctx, error), &ctx)),
    };

    if text.len() > request.limits.max_output_bytes {
        return Err(ScriptError::output(
            text.len(),
            request.limits.max_output_bytes,
        ));
    }

    let mut value: Value = serde_json::from_str(&text)
        .map_err(|e| ScriptError::internal(format!("script returned invalid JSON: {e}")))?;
    redact_json(&mut value, &redactor);
    Ok(value)
}

/// Render a caught exception as `Name: message`, falling back to the thrown
/// value when the script threw something that is not an `Error`.
fn describe_exception(error: &CaughtError<'_>, ctx: &Ctx<'_>) -> String {
    match error {
        CaughtError::Exception(exception) => {
            let object = exception.as_object();
            let name = object
                .get::<_, String>("name")
                .unwrap_or_else(|_| "Error".to_string());
            let message = exception.message().unwrap_or_default();
            format!("{name}: {message}")
        }
        CaughtError::Error(err) => err.to_string(),
        CaughtError::Value(value) => match value.as_string() {
            Some(text) => format!("script threw: {}", text.to_string().unwrap_or_default()),
            None => {
                let _ = ctx;
                "script threw a non-Error value".to_string()
            }
        },
    }
}

/// Redact every string in the result, keys included, so a secret echoed into
/// the return value cannot reach the caller.
fn redact_json(value: &mut Value, redactor: &Redactor) {
    if redactor.is_empty() {
        return;
    }
    match value {
        Value::String(text) => *text = redactor.redact(text),
        Value::Array(items) => {
            for item in items {
                redact_json(item, redactor);
            }
        }
        Value::Object(map) => {
            let previous = std::mem::take(map);
            for (key, mut item) in previous {
                redact_json(&mut item, redactor);
                map.insert(redactor.redact(&key), item);
            }
        }
        _ => {}
    }
}

/// Install an async host function that resolves after `delay_ms`, used by the
/// sandbox tests (and a template for real host functions).
#[cfg_attr(not(test), allow(dead_code))]
pub fn install_slow_fake<'js>(
    ctx: &Ctx<'js>,
    name: &'static str,
    delay: Duration,
) -> rquickjs::Result<()> {
    let host = super::host::host_object(ctx)?;
    let function = rquickjs::Function::new(
        ctx.clone(),
        Async(move |label: String| {
            let delay = delay;
            async move {
                tokio::time::sleep(delay).await;
                Ok::<String, rquickjs::Error>(format!("{name}:{label}"))
            }
        }),
    )?;
    host.set(name, function)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;
    use std::time::Instant;

    use rquickjs::{Function, Object};

    use crate::codemode::host::{LogSink, host_error, host_object, install_log, install_time};

    /// Host functions a test script can reach: `echo`, `slow`, `secret`,
    /// `boom`, plus `log` and the time trio.
    struct FakeHost {
        slow: Duration,
        logs: Arc<Mutex<Vec<String>>>,
    }

    impl FakeHost {
        fn new(slow: Duration) -> Self {
            Self {
                slow,
                logs: Arc::new(Mutex::new(Vec::new())),
            }
        }
    }

    impl HostBindings for FakeHost {
        fn install(&self, ctx: &Ctx<'_>, env: &HostEnv) -> rquickjs::Result<()> {
            install_time(ctx, env)?;
            let logs: LogSink = {
                let logs = self.logs.clone();
                Arc::new(move |line: &str| logs.lock().unwrap().push(line.to_string()))
            };
            install_log(ctx, env, logs)?;
            install_slow_fake(ctx, "slow", self.slow)?;

            let host: Object = host_object(ctx)?;
            host.set(
                "echo",
                Function::new(ctx.clone(), |text: String| -> String { text })?,
            )?;
            host.set(
                "secret",
                Function::new(ctx.clone(), |_name: String| -> String {
                    "s3cr3t-value".to_string()
                })?,
            )?;
            host.set(
                "boom",
                Function::new(ctx.clone(), |ctx: Ctx<'_>| -> rquickjs::Result<()> {
                    Err(host_error(&ctx, "host refused s3cr3t-value"))
                })?,
            )?;
            Ok(())
        }
    }

    fn env() -> HostEnv {
        HostEnv::new(Redactor::new(["s3cr3t-value"]))
    }

    fn run(script: &str, entry: &str, limits: Limits) -> Result<Value, ScriptError> {
        run_with(
            script,
            entry,
            limits,
            Arc::new(FakeHost::new(Duration::ZERO)),
        )
    }

    fn run_with(
        script: &str,
        entry: &str,
        limits: Limits,
        host: Arc<dyn HostBindings>,
    ) -> Result<Value, ScriptError> {
        execute(
            RunRequest::new(script, entry).with_limits(limits),
            host,
            env(),
        )
    }

    fn budget(ms: u64) -> Limits {
        Limits::judge().with_timeout(Duration::from_millis(ms))
    }

    #[test]
    fn test_return_value_crosses_back_as_json() {
        let value = run(
            "async function judge(ctx) { return { fire: true, detail: 'hi' }; }",
            ENTRY_JUDGE,
            Limits::judge(),
        )
        .expect("script must succeed");
        assert_eq!(value["fire"], true);
        assert_eq!(value["detail"], "hi");
    }

    #[test]
    fn test_syntax_error_is_structured() {
        let err = run("function judge( {", ENTRY_JUDGE, Limits::judge()).unwrap_err();
        assert_eq!(err.code(), "syntax");
    }

    #[test]
    fn test_uncaught_exception_is_structured() {
        let err = run(
            "async function judge(ctx) { throw new Error('kaput'); }",
            ENTRY_JUDGE,
            Limits::judge(),
        )
        .unwrap_err();
        assert_eq!(err.code(), "exception");
        assert!(err.message.contains("kaput"));
    }

    #[test]
    fn test_missing_entry_is_structured() {
        let err = run("const x = 1;", ENTRY_JUDGE, Limits::judge()).unwrap_err();
        assert_eq!(err.code(), "missing_entry");
    }

    #[test]
    fn test_dangerous_globals_are_absent() {
        // eval/Function are deliberately available; everything else is not.
        let script = r#"
            async function judge(ctx) {
                const found = [];
                for (const name of ["require", "import", "process", "module",
                                    "console", "setTimeout", "setInterval",
                                    "setImmediate", "queueMicrotask", "fetch",
                                    "Deno", "Bun", "XMLHttpRequest"]) {
                    if (typeof globalThis[name] !== "undefined") found.push(name);
                }
                return { found, eval: typeof eval, ctor: typeof Function };
            }
        "#;
        let value = run(script, ENTRY_JUDGE, Limits::judge()).expect("probe must run");
        assert_eq!(value["found"].as_array().unwrap().len(), 0, "{value}");
        assert_eq!(value["eval"], "function");
        assert_eq!(value["ctor"], "function");
    }

    #[test]
    fn test_eval_and_function_cannot_reach_unregistered_capabilities() {
        let script = r#"
            async function judge(ctx) {
                const attempts = [];
                for (const code of ["require('fs')", "process.env",
                                    "setTimeout(() => {}, 1)", "fetch('http://x')",
                                    "new (Function('return process'))()"]) {
                    let outcome;
                    try { outcome = typeof eval(code) === "undefined" ? "undefined" : "reached"; }
                    catch (e) { outcome = "threw"; }
                    attempts.push(outcome);
                }
                return { attempts };
            }
        "#;
        let value = run(script, ENTRY_JUDGE, Limits::judge()).expect("probe must run");
        for attempt in value["attempts"].as_array().unwrap() {
            assert_eq!(attempt, "threw", "{value}");
        }
    }

    #[test]
    fn test_host_async_function_awaits_and_settles() {
        let script = r#"
            async function judge(ctx) {
                const first = await ctx.slow("a");
                const second = await ctx.slow("b");
                return { first, second };
            }
        "#;
        let value = run(script, ENTRY_JUDGE, budget(3_000)).expect("awaits must settle");
        assert_eq!(value["first"], "slow:a");
        assert_eq!(value["second"], "slow:b");
    }

    #[test]
    fn test_spinning_script_is_interrupted() {
        let started = Instant::now();
        let err = run(
            "async function judge(ctx) { while (true) {} }",
            ENTRY_JUDGE,
            budget(200),
        )
        .unwrap_err();
        assert_eq!(err.code(), "timeout");
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "spin was not cut off, took {:?}",
            started.elapsed()
        );
    }

    #[test]
    fn test_outer_deadline_catches_host_call_in_flight() {
        let started = Instant::now();
        let host = Arc::new(FakeHost::new(Duration::from_secs(10)));
        let err = run_with(
            "async function judge(ctx) { await ctx.slow('hang'); return {}; }",
            ENTRY_JUDGE,
            budget(200),
            host,
        )
        .unwrap_err();
        assert_eq!(err.code(), "timeout");
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "host call was not cut off, took {:?}",
            started.elapsed()
        );
    }

    #[test]
    fn test_memory_limit_is_enforced_and_host_still_usable() {
        let err = run(
            "async function judge(ctx) { const a = []; for (;;) { a.push(new Array(4096).fill(1)); } }",
            ENTRY_JUDGE,
            Limits {
                memory_bytes: 8 * 1024 * 1024,
                ..Limits::judge().with_timeout(Duration::from_secs(5))
            },
        )
        .unwrap_err();
        assert_eq!(err.code(), "memory", "{err}");

        // The process, and the sandbox, still work.
        let value = run(
            "async function judge(ctx) { return { ok: true }; }",
            ENTRY_JUDGE,
            Limits::judge(),
        )
        .unwrap();
        assert_eq!(value["ok"], true);
    }

    #[test]
    fn test_output_limit_rejects_oversized_result() {
        let err = run(
            "async function judge(ctx) { return { blob: 'x'.repeat(5000) }; }",
            ENTRY_JUDGE,
            Limits {
                max_output_bytes: 1024,
                ..Limits::judge()
            },
        )
        .unwrap_err();
        assert_eq!(err.code(), "output");
    }

    #[test]
    fn test_secret_is_redacted_in_value_log_and_exception() {
        let host = Arc::new(FakeHost::new(Duration::ZERO));
        let logs = host.logs.clone();

        let value = run_with(
            r#"
            async function judge(ctx) {
                const s = ctx.secret("api");
                ctx.log("using " + s);
                return { echoed: s };
            }
            "#,
            ENTRY_JUDGE,
            budget(3_000),
            host.clone(),
        )
        .expect("script must run");
        assert_eq!(value["echoed"], "***");

        let logged = logs.lock().unwrap().join("\n");
        assert!(!logged.contains("s3cr3t-value"), "log leaked the secret");
        assert!(logged.contains("***"));

        let err = run_with(
            r#"
            async function judge(ctx) {
                const s = ctx.secret("api");
                throw new Error("token was " + s);
            }
            "#,
            ENTRY_JUDGE,
            budget(3_000),
            host,
        )
        .unwrap_err();
        assert!(!err.message.contains("s3cr3t-value"), "{}", err.message);
        assert!(err.message.contains("***"));

        let host_err = run(
            "async function judge(ctx) { return await ctx.boom(); }",
            ENTRY_JUDGE,
            budget(3_000),
        )
        .unwrap_err();
        assert!(
            !host_err.message.contains("s3cr3t-value"),
            "{}",
            host_err.message
        );
    }

    #[test]
    fn test_concurrent_runs_are_isolated() {
        let spinning = std::thread::spawn(|| {
            run(
                "async function judge(ctx) { while (true) {} }",
                ENTRY_JUDGE,
                budget(200),
            )
        });
        let healthy = std::thread::spawn(|| {
            run(
                "async function judge(ctx) { return { ok: 1 }; }",
                ENTRY_JUDGE,
                budget(3_000),
            )
        });

        let healthy = healthy.join().unwrap().expect("healthy run must succeed");
        assert_eq!(healthy["ok"], 1);
        let spinning = spinning.join().unwrap().unwrap_err();
        assert_eq!(spinning.code(), "timeout");
    }

    #[test]
    fn test_repeated_runs_do_not_accumulate_threads() {
        let before = thread_count();
        for _ in 0..100 {
            let value = run(
                "async function judge(ctx) { return { ok: 1 }; }",
                ENTRY_JUDGE,
                budget(5_000),
            )
            .expect("run must succeed");
            assert_eq!(value["ok"], 1);
        }
        let after = thread_count();
        if let (Some(before), Some(after)) = (before, after) {
            assert!(
                after <= before + 2,
                "threads accumulated: {before} -> {after}"
            );
        }
    }

    #[test]
    fn test_time_functions_are_available() {
        let script = r#"
            async function judge(ctx) {
                const t = ctx.now();
                return {
                    epoch: t.epoch_ms,
                    has_date: typeof t.date === "string" && t.date.length === 10,
                    weekday: typeof t.weekday,
                    boot: ctx.boot_at(),
                    uptime: ctx.uptime_s(),
                };
            }
        "#;
        let value = run(script, ENTRY_JUDGE, budget(3_000)).unwrap();
        assert!(value["epoch"].as_i64().unwrap() > 0);
        assert_eq!(value["has_date"], true);
        assert_eq!(value["weekday"], "string");
        assert!(value["boot"].as_i64().unwrap() > 0);
        // Uptime is process-wide: other tests may have booted the clock first,
        // so only "a non-negative whole number of seconds" is guaranteed here.
        assert!(value["uptime"].as_i64().unwrap() >= 0);
    }

    #[test]
    fn test_ctx_payload_is_merged_into_the_argument() {
        let value = execute(
            RunRequest::new(
                "async function judge(ctx) { return { n: ctx.sources.length, ok: ctx.flag }; }",
                ENTRY_JUDGE,
            )
            .with_ctx(serde_json::json!({ "sources": [1, 2, 3], "flag": true }))
            .with_limits(budget(3_000)),
            Arc::new(FakeHost::new(Duration::ZERO)),
            env(),
        )
        .unwrap();
        assert_eq!(value["n"], 3);
        assert_eq!(value["ok"], true);
    }

    #[test]
    fn test_output_is_json_null_when_script_returns_nothing() {
        let value = run("async function judge(ctx) {}", ENTRY_JUDGE, budget(3_000)).unwrap();
        assert_eq!(value, Value::Null);
    }

    /// Best-effort thread count: `/proc` on Linux, `ps -M` on macOS. Returns
    /// `None` when neither is available, which skips the assertion.
    fn thread_count() -> Option<usize> {
        if let Ok(entries) = std::fs::read_dir("/proc/self/task") {
            return Some(entries.count());
        }
        let pid = std::process::id().to_string();
        let output = std::process::Command::new("ps")
            .args(["-M", &pid])
            .output()
            .ok()?;
        if !output.status.success() {
            return None;
        }
        let text = String::from_utf8_lossy(&output.stdout);
        Some(text.lines().count().saturating_sub(1))
    }

    #[test]
    fn test_limits_have_expected_defaults() {
        assert_eq!(Limits::collect().timeout, Duration::from_secs(10));
        assert_eq!(Limits::judge().timeout, Duration::from_millis(500));
        assert_eq!(Limits::judge().memory_bytes, 16 * 1024 * 1024);
        assert!(Limits::judge().max_output_bytes <= 8 * 1024);
    }
}
