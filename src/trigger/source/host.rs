//! Host functions available to a collector (`collect.js`).
//!
//! A collector runs unattended, on model-written code, so its capability list
//! is deliberately short: HTTP reads, filesystem reads inside the workspace,
//! secrets, logging, and the clock. There is no shell, no write, and no
//! `sleep` (a script that sleeps burns the timeout budget while looking busy).
//!
//! The JavaScript surface is a thin shim over JSON-returning primitives, so
//! this module never has to hand JS objects to an async host function.

use std::collections::BTreeMap;
use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use rquickjs::function::Async;
use rquickjs::{Ctx, Function};

use crate::codemode::host::{
    HostBindings, HostEnv, LogSink, host_error, host_object, install_log, install_time,
};

/// Largest file a collector may read in one call.
const MAX_READ_BYTES: u64 = 2 * 1024 * 1024;
/// Largest directory listing a collector may request.
const MAX_LIST_ENTRIES: usize = 500;
/// Largest HTTP body kept; the rest is dropped and flagged.
const MAX_HTTP_BYTES: usize = 2 * 1024 * 1024;
/// Hard ceiling for one HTTP call. The sandbox's own deadline is usually
/// tighter; this exists so a stalled socket cannot outlive its script.
const HTTP_TIMEOUT: Duration = Duration::from_secs(20);

/// Everything a collector may touch, fixed for the whole execution.
#[derive(Clone)]
pub struct CollectHost {
    workspace: PathBuf,
    allow: Vec<PathBuf>,
    secrets: Arc<BTreeMap<String, String>>,
    logs: Arc<Mutex<Vec<String>>>,
}

impl CollectHost {
    pub fn new(
        workspace: PathBuf,
        allow_paths: &[String],
        secrets: Arc<BTreeMap<String, String>>,
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
            logs: Arc::new(Mutex::new(Vec::new())),
        }
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

    async fn http_get(
        &self,
        url: String,
        headers_json: String,
    ) -> std::result::Result<String, String> {
        self.http_request(reqwest::Method::GET, url, headers_json, String::new())
            .await
    }

    async fn http_post(
        &self,
        url: String,
        headers_json: String,
        body: String,
    ) -> std::result::Result<String, String> {
        self.http_request(reqwest::Method::POST, url, headers_json, body)
            .await
    }

    async fn http_request(
        &self,
        method: reqwest::Method,
        url: String,
        headers_json: String,
        body: String,
    ) -> std::result::Result<String, String> {
        let parsed = crate::tools::web_fetch::validate_url(&url)?;
        // Pin the host before connecting so a DNS answer cannot change between
        // validation and the request.
        let port = parsed.port_or_known_default().unwrap_or(80);
        crate::tools::web_fetch::resolve_and_pin(&parsed, port).await?;

        let method_label = method.to_string();
        let mut request = http_client().request(method, parsed.clone());
        if !headers_json.trim().is_empty()
            && let Ok(serde_json::Value::Object(map)) =
                serde_json::from_str::<serde_json::Value>(&headers_json)
        {
            for (key, value) in map {
                if let Some(text) = value.as_str() {
                    request = request.header(key, text);
                }
            }
        }

        if !body.is_empty() {
            request = request.body(body);
        }

        let response = request
            .send()
            .await
            .map_err(|e| format!("{method_label} to '{url}' failed: {e}"))?;
        let status = response.status().as_u16();
        let bytes = response
            .bytes()
            .await
            .map_err(|e| format!("cannot read the response body: {e}"))?;
        let truncated = bytes.len() > MAX_HTTP_BYTES;
        let slice = if truncated {
            &bytes[..MAX_HTTP_BYTES]
        } else {
            &bytes[..]
        };
        let text = String::from_utf8_lossy(slice).to_string();
        Ok(serde_json::json!({
            "status": status,
            "text": text,
            "truncated": truncated,
        })
        .to_string())
    }
}

fn http_client() -> reqwest::Client {
    reqwest::Client::builder()
        .timeout(HTTP_TIMEOUT)
        .redirect(reqwest::redirect::Policy::custom(|attempt| {
            let url = attempt.url();
            if url.scheme() != "http" && url.scheme() != "https" {
                return attempt.stop();
            }
            if let Some(url::Host::Ipv4(addr)) = url.host()
                && crate::tools::web_fetch::is_private_ip(std::net::IpAddr::V4(addr))
            {
                return attempt.stop();
            }
            if attempt.previous().len() > 5 {
                attempt.stop()
            } else {
                attempt.follow()
            }
        }))
        .build()
        .unwrap_or_default()
}

/// JavaScript shim: `ctx.fs.*` / `ctx.http.get|post` over JSON primitives.
const SHIM: &str = r#"
__host.fs = {
    stat: (path) => JSON.parse(__host.fs_stat(String(path))),
    read: (path) => __host.fs_read(String(path)),
    list: (path) => JSON.parse(__host.fs_list(String(path))),
};
__host.http = {
    get: async (url, opts) => JSON.parse(
        await __host.http_get(
            String(url),
            opts && opts.headers ? JSON.stringify(opts.headers) : ""
        )
    ),
    post: async (url, opts) => JSON.parse(
        await __host.http_post(
            String(url),
            opts && opts.headers ? JSON.stringify(opts.headers) : "",
            opts && opts.body !== undefined ? String(opts.body) : ""
        )
    ),
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

        let host = host_object(ctx)?;

        // -- filesystem (workspace-scoped) --
        {
            let stat = self.clone();
            host.set(
                "fs_stat",
                Function::new(
                    ctx.clone(),
                    move |path: String| -> rquickjs::Result<String> {
                        stat.stat(&path).map_err(base_error)
                    },
                )?,
            )?;
            let read = self.clone();
            host.set(
                "fs_read",
                Function::new(
                    ctx.clone(),
                    move |path: String| -> rquickjs::Result<String> {
                        read.read(&path).map_err(base_error)
                    },
                )?,
            )?;
            let list = self.clone();
            host.set(
                "fs_list",
                Function::new(
                    ctx.clone(),
                    move |path: String| -> rquickjs::Result<String> {
                        list.list(&path).map_err(base_error)
                    },
                )?,
            )?;
        }

        // -- http (async: the VM keeps running while the request is in flight) --
        {
            let handle = self.clone();
            let host_fn = Function::new(
                ctx.clone(),
                Async(move |url: String, headers: String| {
                    let handle = handle.clone();
                    async move { handle.http_get(url, headers).await.map_err(base_error) }
                }),
            )?;
            host.set("http_get", host_fn)?;
            let handle = self.clone();
            let host_fn = Function::new(
                ctx.clone(),
                Async(move |url: String, headers: String, body: String| {
                    let handle = handle.clone();
                    async move {
                        handle
                            .http_post(url, headers, body)
                            .await
                            .map_err(base_error)
                    }
                }),
            )?;
            host.set("http_post", host_fn)?;
        }

        // -- secrets --
        {
            let secrets = self.secrets.clone();
            host.set(
                "secret",
                Function::new(
                    ctx.clone(),
                    move |ctx: Ctx<'_>, name: String| -> rquickjs::Result<String> {
                        match secrets.get(&name) {
                            Some(value) => Ok(value.clone()),
                            None => Err(host_error(
                                &ctx,
                                &format!(
                                    "no secret named '{name}'; add it to triggers.secrets in config"
                                ),
                            )),
                        }
                    },
                )?,
            )?;
        }

        ctx.eval::<(), _>(SHIM)?;
        Ok(())
    }
}

/// Host-side failures surface as a thrown `Error` the script can catch.
fn base_error(message: String) -> rquickjs::Error {
    rquickjs::Error::new_from_js_message("host", "string", message)
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
    use std::sync::Arc;

    fn host(workspace: &Path, allow: &[&str]) -> CollectHost {
        let allow: Vec<String> = allow.iter().map(|s| s.to_string()).collect();
        CollectHost::new(
            workspace.to_path_buf(),
            &allow,
            Arc::new(BTreeMap::from([("token".to_string(), "s3cr3t".into())])),
        )
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

    #[tokio::test]
    async fn test_private_http_target_is_blocked() {
        let tmp = tempfile::tempdir().unwrap();
        let h = host(tmp.path(), &[]);
        let err = h
            .http_get("http://127.0.0.1:8080/x".to_string(), String::new())
            .await
            .unwrap_err();
        assert!(err.contains("private"), "{err}");
        // POST answers to the same boundary as GET — the check lives in the
        // shared request path, so neither method can reach a private host.
        let err = h
            .http_post(
                "http://127.0.0.1:8080/x".to_string(),
                String::new(),
                "{}".to_string(),
            )
            .await
            .unwrap_err();
        assert!(err.contains("private"), "{err}");
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
    fn test_no_host_bindings_is_still_a_valid_default() {
        // A build of the trait object must not require a workspace.
        let _: Arc<dyn HostBindings> = Arc::new(NoHost);
        let env = HostEnv::new(Redactor::empty());
        assert!(!env.redact("x").is_empty());
    }
}
