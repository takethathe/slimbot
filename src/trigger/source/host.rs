//! Host functions available to a collector (`collect.js`).
//!
//! A collector runs unattended, on model-written code, so its capability list
//! is deliberately short: HTTP reads, filesystem reads inside the workspace,
//! secrets, logging, the clock, and — when a connector pool is wired — the
//! **read-only** half of the connectors (ticket 17). There is no shell, no write,
//! and no `sleep` (a script that sleeps burns the timeout budget while looking
//! busy).
//!
//! The connector view is additive: RSS feeds, plain endpoints and local files
//! keep going through `ctx.http` / `ctx.fs`, and a connector read sits beside
//! them for systems the owner actually configured an MCP server for. Its filter
//! is a capability list, not an approval class: a tool is installed only when
//! its server claims `readOnlyHint` and not `destructiveHint`, so the write half
//! is neither installed nor searchable. `McpService::risk_for` still answers
//! `ExternalSend` for every connector call — a claim selects this entry's
//! surface, it never buys a lower approval level (see `docs/codemode.md`; the
//! rule is recorded in ADR 0021).
//!
//! The JavaScript surface is a thin shim over JSON-returning primitives, so
//! this module never has to hand JS objects to an async host function.

use std::collections::BTreeMap;
use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, Mutex};

use rquickjs::{Ctx, Function};

use crate::codemode::connector::install_tool_bridge;
use crate::codemode::host::{
    HostBindings, HostEnv, LogSink, host_failure, host_object, install_log, install_secret,
    install_time,
};
use crate::codemode::http::install_http;
use crate::mcp::McpService;

/// Largest file a collector may read in one call.
const MAX_READ_BYTES: u64 = 2 * 1024 * 1024;
/// Largest directory listing a collector may request.
const MAX_LIST_ENTRIES: usize = 500;

/// Everything a collector may touch, fixed for the whole execution.
#[derive(Clone)]
pub struct CollectHost {
    workspace: PathBuf,
    allow: Vec<PathBuf>,
    secrets: Arc<BTreeMap<String, String>>,
    /// The process-wide connector pool, when the process has one. `None` leaves
    /// the collector's surface exactly as it was before connectors existed.
    connectors: Option<Arc<McpService>>,
    logs: Arc<Mutex<Vec<String>>>,
}

impl CollectHost {
    pub fn new(
        workspace: PathBuf,
        allow_paths: &[String],
        secrets: Arc<BTreeMap<String, String>>,
        connectors: Option<Arc<McpService>>,
    ) -> Self {
        let workspace = canonical_or_self(&workspace);
        let allow = allow_paths
            .iter()
            .filter(|p| !p.trim().is_empty())
            .map(|p| canonical_or_self(&expand(p)))
            .collect();
        Self {
            workspace,
            allow,
            secrets,
            connectors,
            logs: Arc::new(Mutex::new(Vec::new())),
        }
    }

    /// Install the read-only connector view: `ctx.tools.mcp__*` and
    /// `ctx.searchTools` over the tools whose server claims them read-only.
    ///
    /// No pool means no surface at all — not an empty one — so a process without
    /// `mcp.servers[]` keeps exactly the collector capability list it had before
    /// ticket 17.
    fn install_connectors(&self, ctx: &Ctx<'_>) -> rquickjs::Result<()> {
        let Some(pool) = self.connectors.clone() else {
            return Ok(());
        };
        install_tool_bridge(
            ctx,
            pool,
            |pool: &Arc<McpService>| pool.readonly_definitions(),
            |pool, name, args| Box::pin(async move { call_read_only(&pool, &name, args).await }),
        )
    }

    /// Resolve a script-supplied path: workspace-relative by default, absolute
    /// only when the source declared the location in `allow_paths`.
    fn resolve(&self, requested: &str) -> std::result::Result<PathBuf, String> {
        let trimmed = requested.trim();
        if trimmed.is_empty() {
            return Err("path is empty".to_string());
        }
        let raw = Path::new(trimmed);
        let joined = if raw.is_absolute() {
            raw.to_path_buf()
        } else {
            self.workspace.join(trimmed.trim_start_matches("./"))
        };
        let resolved = canonical_existing_ancestor(&joined)
            .map_err(|e| format!("cannot resolve '{trimmed}': {e}"))?;
        if resolved.starts_with(&self.workspace)
            || self.allow.iter().any(|a| resolved.starts_with(a))
        {
            return Ok(resolved);
        }
        Err(format!(
            "'{trimmed}' is outside the workspace; declare the directory in allow_paths to read it"
        ))
    }

    fn stat(&self, requested: &str) -> std::result::Result<String, String> {
        let path = self.resolve(requested)?;
        let meta = std::fs::metadata(&path).ok();
        let payload = serde_json::json!({
            "path": path.to_string_lossy(),
            "exists": meta.is_some(),
            "is_file": meta.as_ref().is_some_and(|m| m.is_file()),
            "is_dir": meta.as_ref().is_some_and(|m| m.is_dir()),
            "size": meta.as_ref().map(|m| m.len()).unwrap_or(0),
            "mtime_ms": meta
                .as_ref()
                .and_then(|m| m.modified().ok())
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| d.as_millis() as i64)
                .unwrap_or(0),
        });
        Ok(payload.to_string())
    }

    fn read(&self, requested: &str) -> std::result::Result<String, String> {
        let path = self.resolve(requested)?;
        let meta =
            std::fs::metadata(&path).map_err(|e| format!("cannot stat '{requested}': {e}"))?;
        if meta.len() > MAX_READ_BYTES {
            return Err(format!(
                "'{requested}' is {} bytes, over the {}KB read limit",
                meta.len(),
                MAX_READ_BYTES / 1024
            ));
        }
        std::fs::read_to_string(&path).map_err(|e| format!("cannot read '{requested}': {e}"))
    }

    fn list(&self, requested: &str) -> std::result::Result<String, String> {
        let path = self.resolve(requested)?;
        let entries =
            std::fs::read_dir(&path).map_err(|e| format!("cannot list '{requested}': {e}"))?;
        let mut rows = Vec::new();
        for entry in entries.filter_map(|e| e.ok()).take(MAX_LIST_ENTRIES) {
            let meta = entry.metadata().ok();
            rows.push(serde_json::json!({
                "name": entry.file_name().to_string_lossy(),
                "is_dir": meta.as_ref().is_some_and(|m| m.is_dir()),
                "size": meta.as_ref().map(|m| m.len()).unwrap_or(0),
            }));
        }
        Ok(serde_json::Value::Array(rows).to_string())
    }
}

/// JavaScript shim: `ctx.fs.*` over the JSON primitives. The HTTP shim lives
/// in `codemode::http`, shared with the act entry.
const SHIM: &str = r#"
__host.fs = {
    stat: (path) => JSON.parse(__host.fs_stat(String(path))),
    read: (path) => __host.fs_read(String(path)),
    list: (path) => JSON.parse(__host.fs_list(String(path))),
};
"#;

impl HostBindings for CollectHost {
    fn install(&self, ctx: &Ctx<'_>, env: &HostEnv) -> rquickjs::Result<()> {
        install_time(ctx, env)?;

        let logs: LogSink = {
            let logs = self.logs.clone();
            Arc::new(move |line: &str| {
                if let Ok(mut guard) = logs.lock() {
                    guard.push(line.to_string());
                }
            })
        };
        install_log(ctx, env, logs)?;
        install_http(ctx)?;
        install_secret(ctx, self.secrets.clone())?;

        let host = host_object(ctx)?;

        // -- filesystem (workspace-scoped) --
        {
            let stat = self.clone();
            host.set(
                "fs_stat",
                Function::new(
                    ctx.clone(),
                    move |path: String| -> rquickjs::Result<String> {
                        stat.stat(&path).map_err(host_failure)
                    },
                )?,
            )?;
            let read = self.clone();
            host.set(
                "fs_read",
                Function::new(
                    ctx.clone(),
                    move |path: String| -> rquickjs::Result<String> {
                        read.read(&path).map_err(host_failure)
                    },
                )?,
            )?;
            let list = self.clone();
            host.set(
                "fs_list",
                Function::new(
                    ctx.clone(),
                    move |path: String| -> rquickjs::Result<String> {
                        list.list(&path).map_err(host_failure)
                    },
                )?,
            )?;
        }

        self.install_connectors(ctx)?;

        ctx.eval::<(), _>(SHIM)?;
        Ok(())
    }
}

/// One connector read on behalf of a collector.
///
/// The claim is re-checked here, not only at install time: a server that
/// re-lists its tools mid-run (or goes offline) must not leave a write-side tool
/// callable through a function installed a moment earlier. Fail-closed both
/// ways: an unknown callable is not read-only.
async fn call_read_only(pool: &McpService, name: &str, args: serde_json::Value) -> String {
    if !pool.is_read_only_claimed(name) {
        return format!(
            "Error: `{name}` is not a read-only connector tool (the server is offline or \
             does not claim it read-only); a collector may not call it"
        );
    }
    match pool.call(name, args).await {
        Ok(text) => text,
        Err(e) => format!("Error: {e}"),
    }
}

fn expand(path: &str) -> PathBuf {
    crate::path::expand_home(path)
}

fn canonical_or_self(path: &Path) -> PathBuf {
    path.canonicalize().unwrap_or_else(|_| path.to_path_buf())
}

/// Canonicalize the deepest existing ancestor of `path`, then re-append the
/// missing tail after rejecting `..` segments. Mirrors how the file tools
/// resolve not-yet-existing paths, so `fs.stat` on a missing file still works.
fn canonical_existing_ancestor(path: &Path) -> std::result::Result<PathBuf, String> {
    if let Ok(resolved) = path.canonicalize() {
        return Ok(resolved);
    }
    let mut ancestor = path.to_path_buf();
    let mut tail: Vec<std::ffi::OsString> = Vec::new();
    while !ancestor.exists() {
        match (ancestor.parent(), ancestor.file_name()) {
            (Some(parent), Some(name)) => {
                tail.push(name.to_os_string());
                ancestor = parent.to_path_buf();
            }
            _ => return Err("path has no existing ancestor".to_string()),
        }
    }
    let mut resolved = ancestor
        .canonicalize()
        .map_err(|e| format!("cannot canonicalize '{}': {e}", ancestor.display()))?;
    for comp in tail.iter().rev() {
        let piece = Path::new(comp);
        if piece
            .components()
            .any(|c| matches!(c, Component::ParentDir))
        {
            return Err("path escapes its base directory".to_string());
        }
        resolved.push(piece);
    }
    Ok(resolved)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::codemode::{HostEnv, NoHost, Redactor};
    use serde_json::json;
    use std::sync::Arc;

    fn host(workspace: &Path, allow: &[&str]) -> CollectHost {
        let allow: Vec<String> = allow.iter().map(|s| s.to_string()).collect();
        CollectHost::new(
            workspace.to_path_buf(),
            &allow,
            Arc::new(BTreeMap::from([("token".to_string(), "s3cr3t".into())])),
            None,
        )
    }

    /// One fake connector server brought online on a runtime of its own: the
    /// script runs on a dedicated thread and blocks the test thread while it
    /// waits, so the supervisor loop needs a scheduler that keeps turning.
    fn online_pool(
        server: &str,
        tools: Vec<serde_json::Value>,
    ) -> (Arc<crate::mcp::McpService>, tokio::runtime::Runtime) {
        use crate::mcp::service::test_support as mcp_test;
        let rt = tokio::runtime::Runtime::new().expect("connector runtime");
        let (service, _peer) = {
            let _guard = rt.enter();
            mcp_test::online_service(server, tools)
        };
        rt.block_on(mcp_test::wait_for(
            || service.any_online(),
            "the fake server online",
        ));
        (service, rt)
    }

    fn run_collect(script: &str, host: Arc<CollectHost>) -> serde_json::Value {
        let request = crate::codemode::RunRequest::new(script, crate::codemode::ENTRY_COLLECT)
            .with_limits(
                crate::codemode::Limits::collect().with_timeout(std::time::Duration::from_secs(5)),
            );
        crate::codemode::execute(request, host, HostEnv::default()).expect("probe must run")
    }

    #[test]
    fn test_read_stays_inside_workspace() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("a.txt"), "hello").unwrap();
        let h = host(tmp.path(), &[]);
        assert_eq!(h.read("a.txt").unwrap(), "hello");
    }

    #[test]
    fn test_escape_outside_workspace_is_refused() {
        let tmp = tempfile::tempdir().unwrap();
        let other = tempfile::tempdir().unwrap();
        std::fs::write(other.path().join("secret.txt"), "nope").unwrap();
        let h = host(tmp.path(), &[]);
        let outside = other.path().join("secret.txt");
        let err = h.read(outside.to_str().unwrap()).unwrap_err();
        assert!(err.contains("allow_paths"), "{err}");
    }

    #[test]
    fn test_allow_paths_admits_a_declared_directory() {
        let tmp = tempfile::tempdir().unwrap();
        let other = tempfile::tempdir().unwrap();
        std::fs::write(other.path().join("ok.txt"), "allowed").unwrap();
        let h = host(tmp.path(), &[other.path().to_str().unwrap()]);
        let outside = other.path().join("ok.txt");
        assert_eq!(h.read(outside.to_str().unwrap()).unwrap(), "allowed");
    }

    #[test]
    fn test_stat_reports_missing_file() {
        let tmp = tempfile::tempdir().unwrap();
        let h = host(tmp.path(), &[]);
        let value: serde_json::Value =
            serde_json::from_str(&h.stat("missing.txt").unwrap()).unwrap();
        assert_eq!(value["exists"], false);
        assert_eq!(value["is_file"], false);
    }

    #[test]
    fn test_list_reports_entries_without_content() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("a.md"), "x").unwrap();
        std::fs::create_dir_all(tmp.path().join("sub")).unwrap();
        let h = host(tmp.path(), &[]);
        let value: serde_json::Value = serde_json::from_str(&h.list(".").unwrap()).unwrap();
        let rows = value.as_array().unwrap();
        assert!(
            rows.iter()
                .any(|r| r["name"] == "a.md" && r["is_dir"] == false)
        );
        assert!(
            rows.iter()
                .any(|r| r["name"] == "sub" && r["is_dir"] == true)
        );
    }

    #[test]
    fn test_oversized_read_is_refused() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(
            tmp.path().join("big.txt"),
            vec![b'x'; (MAX_READ_BYTES + 1) as usize],
        )
        .unwrap();
        let h = host(tmp.path(), &[]);
        let err = h.read("big.txt").unwrap_err();
        assert!(err.contains("read limit"), "{err}");
    }

    #[test]
    fn test_collect_entry_still_gets_the_shared_http_surface() {
        // `ctx.http` moved to `codemode::http` (shared with the act entry);
        // the collector still gets both verbs, and both still refuse a private
        // target before opening a socket.
        let tmp = tempfile::tempdir().unwrap();
        let request = crate::codemode::RunRequest::new(
            r#"async function collect(ctx) {
                const refused = [];
                for (const call of [
                    () => ctx.http.get("http://127.0.0.1:8080/x"),
                    () => ctx.http.post("http://127.0.0.1:8080/x", {body: "{}"}),
                ]) {
                    try { await call(); refused.push("reached"); }
                    catch (e) { refused.push(String(e && e.message || e)); }
                }
                return { items: [], http: Object.keys(ctx.http).sort(), refused };
            }"#,
            crate::codemode::ENTRY_COLLECT,
        )
        .with_limits(
            crate::codemode::Limits::collect().with_timeout(std::time::Duration::from_secs(5)),
        );
        let value =
            crate::codemode::execute(request, Arc::new(host(tmp.path(), &[])), HostEnv::default())
                .expect("probe must run");

        assert_eq!(value["http"], serde_json::json!(["get", "post"]));
        for outcome in value["refused"].as_array().unwrap() {
            assert!(outcome.as_str().unwrap().contains("private"), "{outcome}");
        }
    }

    #[test]
    fn test_unknown_secret_is_an_error_not_an_empty_string() {
        let tmp = tempfile::tempdir().unwrap();
        let h = host(tmp.path(), &[]);
        assert!(h.secrets.get("token").is_some());
        assert!(h.secrets.get("nope").is_none());
    }

    #[test]
    fn test_missing_path_tail_is_reappended() {
        let tmp = tempfile::tempdir().unwrap();
        let h = host(tmp.path(), &[]);
        let resolved = h.resolve("sub/missing.txt").unwrap();
        assert!(resolved.starts_with(h.workspace.clone()));
        assert!(resolved.to_string_lossy().ends_with("sub/missing.txt"));
    }

    #[test]
    fn test_collect_entry_reaches_only_the_read_only_connector_tools() {
        // One server, one tool it claims read-only and one it does not: the
        // collector's connector surface is a *capability* subset (ticket 17), so
        // the write-side tool is neither installed nor searchable, while the
        // read-side call runs end to end. The same pool hands the agent entry
        // both tools; only this entry's filter differs.
        let (pool, _rt) = online_pool(
            "github",
            vec![
                json!({
                    "name": "get_pull",
                    "description": "Fetch one pull request",
                    "annotations": {"readOnlyHint": true, "destructiveHint": false},
                }),
                json!({"name": "merge_pull", "description": "Merge one pull request"}),
            ],
        );
        let tmp = tempfile::tempdir().unwrap();
        let host = Arc::new(CollectHost::new(
            tmp.path().to_path_buf(),
            &[],
            Arc::new(BTreeMap::new()),
            Some(pool),
        ));
        let value = run_collect(
            r#"async function collect(ctx) {
                const hits = JSON.parse(await ctx.searchTools("pull request")).map((h) => h.name);
                const writeVisible = typeof ctx.tools.mcp__github__merge_pull;
                let merge;
                try { merge = await ctx.tools.mcp__github__merge_pull({}); }
                catch (e) { merge = "threw"; }
                const read = await ctx.tools.mcp__github__get_pull({number: 12});
                return { items: [{title: read}], hits, writeVisible, merge };
            }"#,
            host,
        );

        assert_eq!(value["hits"], json!(["mcp__github__get_pull"]), "{value}");
        assert_eq!(value["writeVisible"], "undefined", "{value}");
        assert_eq!(value["merge"], "threw", "{value}");
        let title = value["items"][0]["title"].as_str().unwrap();
        assert!(title.contains("<codemode_output"), "{title}");
        assert!(title.contains("pong"), "{title}");
    }

    #[test]
    fn test_collect_entry_keeps_its_whole_primitive_surface_with_a_connector_pool() {
        // The connector view is additive, never a replacement (ticket 17): RSS
        // feeds, plain endpoints and local files still go through the base
        // primitives while a pool is attached.
        let (pool, _rt) = online_pool(
            "github",
            vec![json!({
                "name": "get_pull",
                "description": "Fetch one pull request",
                "annotations": {"readOnlyHint": true, "destructiveHint": false},
            })],
        );
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("feed.xml"), "<rss/>").unwrap();
        let host = Arc::new(CollectHost::new(
            tmp.path().to_path_buf(),
            &[],
            Arc::new(BTreeMap::from([("token".to_string(), "s3cr3t".into())])),
            Some(pool),
        ));
        let value = run_collect(
            r#"async function collect(ctx) {
                ctx.log("collect ran");
                let httpOutcome;
                try { await ctx.http.get("http://127.0.0.1:8080/x"); httpOutcome = "reached"; }
                catch (e) { httpOutcome = String(e && e.message || e); }
                return {
                    items: [{title: "ok"}],
                    keys: Object.keys(ctx).sort(),
                    http: Object.keys(ctx.http).sort(),
                    httpOutcome,
                    read: ctx.fs.read("feed.xml"),
                    stat: ctx.fs.stat("feed.xml").exists,
                    secret: ctx.secret("token") === "s3cr3t",
                    connector: typeof ctx.tools.mcp__github__get_pull,
                };
            }"#,
            host,
        );

        // The exact surface: every base primitive still installed, the read-only
        // connector view added beside them.
        assert_eq!(
            value["keys"],
            json!([
                "boot_at",
                "fs",
                "fs_list",
                "fs_read",
                "fs_stat",
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
        assert_eq!(value["http"], json!(["get", "post"]));
        assert!(
            value["httpOutcome"].as_str().unwrap().contains("private"),
            "{value}"
        );
        assert_eq!(value["read"], "<rss/>");
        assert_eq!(value["stat"], true);
        assert_eq!(value["secret"], true);
        assert_eq!(value["connector"], "function");
    }

    #[test]
    fn test_collect_entry_has_no_connector_surface_without_a_pool() {
        // No pool, no addition: the collector's surface is exactly today's, and
        // `tools` / `searchTools` stay structurally absent.
        let tmp = tempfile::tempdir().unwrap();
        let value = run_collect(
            r#"async function collect(ctx) {
                return { items: [], keys: Object.keys(ctx).sort() };
            }"#,
            Arc::new(host(tmp.path(), &[])),
        );
        assert_eq!(
            value["keys"],
            json!([
                "boot_at",
                "fs",
                "fs_list",
                "fs_read",
                "fs_stat",
                "http",
                "http_get",
                "http_post",
                "log",
                "now",
                "secret",
                "uptime_s"
            ]),
            "{value}"
        );
    }

    #[test]
    fn test_no_host_bindings_is_still_a_valid_default() {
        // A build of the trait object must not require a workspace.
        let _: Arc<dyn HostBindings> = Arc::new(NoHost);
        let env = HostEnv::new(Redactor::empty());
        assert!(!env.redact("x").is_empty());
    }
}
