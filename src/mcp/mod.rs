//! MCP connectors: the external capabilities a codemode script can call.
//!
//! Three layers, each testable on its own:
//!
//! - [`rpc`] — the minimal client: JSON-RPC frames plus `initialize`,
//!   `tools/list`, `tools/call`.
//! - [`transport`] — how a frame becomes an answer: a stdio child process or a
//!   streamable-HTTP endpoint.
//! - [`service`] — [`McpService`], the gateway-level owner of every connection,
//!   the per-server tool cache, and the backoff that keeps a dead server from
//!   wedging anything.
//!
//! What this module deliberately is not: a tool provider for the model. A
//! connector's tools never reach the model's tool schema or the built-in
//! `ToolManager` — they are discovered inside codemode with `searchTools()` and
//! called as `tools.mcp__<server>__<tool>()` (ADR 0016), which is why the only
//! consumer of [`McpService::definitions`] is the codemode agent host.

pub mod rpc;
pub mod service;
pub mod transport;

pub use service::{MCP_TOOL_PREFIX, McpService, ServerStatus};

/// Characters a JS property access can carry: `ctx.tools.<name>` must be
/// reachable with the name we hand out, so everything else becomes `_`.
pub fn js_safe_ident(name: &str) -> String {
    name.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

/// The callable prefix one server owns: `mcp__<server>__`. Two servers whose
/// names differ only by punctuation would otherwise address the same tools.
pub fn server_prefix(server: &str) -> String {
    format!("{}{}__", MCP_TOOL_PREFIX, js_safe_ident(server))
}
