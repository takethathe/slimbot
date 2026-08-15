//! Multi-backend span instrumentation for tracing a single run.
//!
//! One run (`AgentRunner::run()`) maps to one trace: a root span plus its
//! child spans (`iteration`, `provider.chat`, `tool.{name}`), all sharing the
//! same trace id. Term definitions live in `CONTEXT.md`; design narrative in
//! `docs/trace.md`; the decision to build a self-contained span model instead
//! of adopting the `tracing` crate is recorded in `docs/adr/0001-self-built-span-model.md`.
//!
//! This module is a leaf: it depends on nothing but std / serde / uuid / rand.
//! It does not reference the provider, the runner, the session layer, the log
//! system, `AgentEvent`, or the global config singleton. The tracer is built
//! from configuration by the wiring layer and injected downward.

pub mod backends;
mod span;

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::SystemTime;

pub use span::{SpanContext, SpanGuard};

/// Outcome of a traced operation, recorded on the span when it ends.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum SpanStatus {
    /// The operation completed successfully.
    #[default]
    Ok,
    /// The operation failed; the payload is a human-readable error message.
    Error(String),
}

/// A backend that receives span lifecycle events.
///
/// Both methods are **synchronous and infallible**: span end runs inside
/// `Drop`, so a backend must never surface an error to the run loop. Backends
/// buffer and flush internally; a slow or failing backend must not block or
/// break other backends (the tracer contains each backend's panics).
pub trait TraceBackend: Send + Sync {
    /// Called when a span starts.
    fn on_span_start(&self, ctx: &SpanContext, name: &str, start: SystemTime);

    /// Called when a span ends, carrying the accumulated status and attributes.
    fn on_span_end(
        &self,
        ctx: &SpanContext,
        end: SystemTime,
        status: &SpanStatus,
        attributes: &BTreeMap<String, serde_json::Value>,
    );
}

/// Minimal interface for writing a single attribute to a live span.
///
/// The provider uses this to record the serialized request body without
/// depending on the full span machinery.
pub trait SpanRecorder: Sync {
    fn set_attribute(&self, key: &str, value: serde_json::Value);
}

/// Fan-out registry of backends.
///
/// Every span start/end is broadcast to all configured backends. Each backend
/// is isolated: a panicking or failing backend cannot affect other backends or
/// the run itself. When no backends are configured, `start_span` returns a
/// cheap no-op guard so call sites need no `if let` branching.
pub struct Tracer {
    backends: Arc<Vec<Arc<dyn TraceBackend>>>,
}

impl Clone for Tracer {
    fn clone(&self) -> Self {
        Self {
            backends: self.backends.clone(),
        }
    }
}

impl Tracer {
    /// Create a tracer that fans out to the given backends.
    pub fn new(backends: Vec<Arc<dyn TraceBackend>>) -> Self {
        Self {
            backends: Arc::new(backends),
        }
    }

    /// Whether this tracer has at least one backend.
    pub fn is_enabled(&self) -> bool {
        !self.backends.is_empty()
    }

    /// Start a new span.
    ///
    /// `parent` establishes explicit parent-child linkage (restore): the new
    /// span inherits the parent's trace id and records it as `parent_span_id`.
    /// When `parent` is `None` (or a no-op context) the span becomes a root of
    /// a new trace.
    pub fn start_span(&self, name: &str, parent: Option<&SpanContext>) -> SpanGuard {
        if self.backends.is_empty() {
            return SpanGuard::noop();
        }
        let ctx = SpanContext::new_child(parent);
        SpanGuard::new(
            self.backends.clone(),
            ctx,
            name.to_string(),
            SystemTime::now(),
        )
    }
}

/// Broadcast a span-start event to all backends, containing panics per backend.
pub(crate) fn broadcast_start(
    backends: &[Arc<dyn TraceBackend>],
    ctx: &SpanContext,
    name: &str,
    start: SystemTime,
) {
    for backend in backends {
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            backend.on_span_start(ctx, name, start)
        }));
    }
}

/// Broadcast a span-end event to all backends, containing panics per backend.
pub(crate) fn broadcast_end(
    backends: &[Arc<dyn TraceBackend>],
    ctx: &SpanContext,
    end: SystemTime,
    status: &SpanStatus,
    attributes: &BTreeMap<String, serde_json::Value>,
) {
    for backend in backends {
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            backend.on_span_end(ctx, end, status, attributes)
        }));
    }
}

#[cfg(test)]
mod tests;
