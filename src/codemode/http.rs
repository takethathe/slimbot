//! The read-side HTTP primitive shared by the collect and act entries.
//!
//! Both entries get the same `ctx.http.get` / `ctx.http.post` surface out of
//! this one implementation: fetch a URL, keep at most 2MB of the body, and
//! answer `{status, text, truncated}`. There is no write verb (`put` /
//! `delete` / `patch`) and never will be here — a raw HTTP write carries no
//! intrinsic risk level, so the approval gate cannot hold it; the only way to
//! change an external system is a gated connector tool call (ticket 12/15).
//!
//! Safety lives in the shared request path: only `http`/`https`, private
//! targets refused before connecting, host resolved and pinned, redirects
//! capped. Every entry that installs this primitive inherits all of it.
//!
//! An entry that needs to account for outward calls ([`HttpCallHooks`]) gets
//! both hooks *below* the `ctx.http` shim, so a script calling the raw
//! `ctx.http_get` / `ctx.http_post` primitives cannot skip its own budget.

use std::sync::Arc;
use std::time::Duration;

use rquickjs::function::Async;
use rquickjs::{Ctx, Function};

use super::host::{host_failure, host_object};

/// Largest HTTP body kept; the rest is dropped and flagged.
const MAX_HTTP_BYTES: usize = 2 * 1024 * 1024;
/// Hard ceiling for one HTTP call. The sandbox's own deadline is usually
/// tighter; this exists so a stalled socket cannot outlive its script.
const HTTP_TIMEOUT: Duration = Duration::from_secs(20);

/// Per-entry accounting for outward HTTP calls.
///
/// The act entry uses it for two facts the caller has to report accurately: the
/// per-run call budget (spec D) and the root cause of an external failure. Both
/// live at this layer rather than in the JS shim, so the whole call surface is
/// covered, not just `ctx.http.*`.
pub(crate) trait HttpCallHooks: Send + Sync + 'static {
    /// Consulted before the request is even validated. `Some(reason)` refuses
    /// the call; a caller that counts calls is expected to have stopped the run
    /// before returning a reason.
    fn before(&self, method: &str, url: &str) -> Option<String>;
    /// Called once the call settled: the status when it was answered, or the
    /// transport error when no response arrived.
    fn after(&self, method: &str, url: &str, outcome: Result<u16, &str>);
}

/// One `GET`, answered as JSON text `{status, text, truncated}`.
pub(crate) async fn http_get(
    url: String,
    headers_json: String,
    hooks: Option<&dyn HttpCallHooks>,
) -> std::result::Result<String, String> {
    http_request(
        reqwest::Method::GET,
        url,
        headers_json,
        String::new(),
        hooks,
    )
    .await
}

/// One `POST`, answered exactly like [`http_get`]. The verb exists for query
/// endpoints that only accept POST, not to write.
pub(crate) async fn http_post(
    url: String,
    headers_json: String,
    body: String,
    hooks: Option<&dyn HttpCallHooks>,
) -> std::result::Result<String, String> {
    http_request(reqwest::Method::POST, url, headers_json, body, hooks).await
}

/// One answered HTTP call.
struct Answer {
    status: u16,
    text: String,
    truncated: bool,
}

async fn http_request(
    method: reqwest::Method,
    url: String,
    headers_json: String,
    body: String,
    hooks: Option<&dyn HttpCallHooks>,
) -> std::result::Result<String, String> {
    let method_label = method.to_string();
    // Before validation: an attempt the call budget refuses must still be
    // counted, and a target the guard blocks was still an outward attempt. A
    // refused attempt is *not* reported as an outward failure: the outside was
    // never contacted, so it is a host gate, not an external one.
    if let Some(hooks) = hooks
        && let Some(reason) = hooks.before(&method_label, &url)
    {
        return Err(reason);
    }

    let outcome = attempt(method, &url, &headers_json, body).await;
    if let Some(hooks) = hooks {
        match &outcome {
            Ok(answer) => hooks.after(&method_label, &url, Ok(answer.status)),
            Err(error) => hooks.after(&method_label, &url, Err(error)),
        }
    }
    let answer = outcome?;
    Ok(serde_json::json!({
        "status": answer.status,
        "text": answer.text,
        "truncated": answer.truncated,
    })
    .to_string())
}

/// Everything from URL validation to the response body. Failures here mean the
/// call was made but did not answer: the caller's external-failure root cause.
async fn attempt(
    method: reqwest::Method,
    url: &str,
    headers_json: &str,
    body: String,
) -> std::result::Result<Answer, String> {
    let method_label = method.to_string();
    let parsed = crate::tools::web_fetch::validate_url(url)?;
    // Pin the host before connecting so a DNS answer cannot change between
    // validation and the request.
    let port = parsed.port_or_known_default().unwrap_or(80);
    crate::tools::web_fetch::resolve_and_pin(&parsed, port).await?;

    let mut request = http_client().request(method, parsed.clone());
    if !headers_json.trim().is_empty()
        && let Ok(serde_json::Value::Object(map)) =
            serde_json::from_str::<serde_json::Value>(headers_json)
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
    Ok(Answer {
        status,
        text: String::from_utf8_lossy(slice).to_string(),
        truncated,
    })
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

/// JavaScript shim: `ctx.http.get|post` over the JSON primitives.
const SHIM: &str = r#"
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

/// Install the read-side HTTP primitive: `ctx.http.get` and `ctx.http.post`.
pub fn install_http(ctx: &Ctx<'_>) -> rquickjs::Result<()> {
    install_http_with(ctx, None)
}

/// The same surface with an entry-specific [`HttpCallHooks`].
pub(crate) fn install_http_with(
    ctx: &Ctx<'_>,
    hooks: Option<Arc<dyn HttpCallHooks>>,
) -> rquickjs::Result<()> {
    let host = host_object(ctx)?;

    let host_fn = Function::new(ctx.clone(), {
        let hooks = hooks.clone();
        Async(move |url: String, headers: String| {
            let hooks = hooks.clone();
            async move {
                http_get(url, headers, hooks.as_deref())
                    .await
                    .map_err(host_failure)
            }
        })
    })?;
    host.set("http_get", host_fn)?;

    let host_fn = Function::new(ctx.clone(), {
        let hooks = hooks.clone();
        Async(move |url: String, headers: String, body: String| {
            let hooks = hooks.clone();
            async move {
                http_post(url, headers, body, hooks.as_deref())
                    .await
                    .map_err(host_failure)
            }
        })
    })?;
    host.set("http_post", host_fn)?;

    ctx.eval::<(), _>(SHIM)?;
    Ok(())
}
