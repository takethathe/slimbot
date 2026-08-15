use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::SystemTime;

use parking_lot::Mutex;

use super::{SpanRecorder, SpanStatus, TraceBackend};

/// Identifies a span's position in the trace tree.
///
/// Formats are W3C/OTel-compatible: `trace_id` is 32 hex chars (16 bytes from
/// a UUID v4), `span_id` is 16 hex chars (8 random bytes), and
/// `parent_span_id` links the span to its parent within the same trace.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SpanContext {
    pub trace_id: String,
    pub span_id: String,
    pub parent_span_id: Option<String>,
}

impl SpanContext {
    /// Create a context from explicit parts.
    pub fn new(trace_id: String, span_id: String, parent_span_id: Option<String>) -> Self {
        Self {
            trace_id,
            span_id,
            parent_span_id,
        }
    }

    /// Generate a fresh root context: a new trace id and no parent.
    pub fn new_root() -> Self {
        Self {
            trace_id: new_trace_id(),
            span_id: new_span_id(),
            parent_span_id: None,
        }
    }

    /// Derive a child context from an optional parent.
    ///
    /// A missing parent (or a no-op context with an empty trace id) yields a
    /// fresh root trace.
    pub(crate) fn new_child(parent: Option<&SpanContext>) -> Self {
        match parent {
            Some(p) if !p.trace_id.is_empty() && !p.span_id.is_empty() => Self {
                trace_id: p.trace_id.clone(),
                span_id: new_span_id(),
                parent_span_id: Some(p.span_id.clone()),
            },
            _ => Self::new_root(),
        }
    }
}

/// Generate a 32-hex trace id (16 random bytes from a UUID v4).
fn new_trace_id() -> String {
    uuid::Uuid::new_v4().simple().to_string()
}

/// Generate a 16-hex span id (8 random bytes).
fn new_span_id() -> String {
    let bytes: [u8; 8] = rand::random();
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Accumulated state for a live span: attributes + status.
#[derive(Default)]
struct GuardState {
    attributes: BTreeMap<String, serde_json::Value>,
    status: SpanStatus,
}

/// RAII handle for a span.
///
/// Constructing a guard emits span-start to all backends; dropping it emits
/// span-end carrying the accumulated attributes, status and end timestamp.
/// Attributes and status accumulate through `&self` methods, and the interior
/// state lives behind a mutex, so the guard is `Send + Sync` and can be held
/// across `.await` points.
pub struct SpanGuard {
    inner: Option<Arc<SpanInner>>,
}

struct SpanInner {
    backends: Arc<Vec<Arc<dyn TraceBackend>>>,
    ctx: SpanContext,
    state: Mutex<GuardState>,
}

impl SpanGuard {
    /// Create a live guard: broadcasts span-start to every backend.
    pub(crate) fn new(
        backends: Arc<Vec<Arc<dyn TraceBackend>>>,
        ctx: SpanContext,
        name: String,
        start: SystemTime,
    ) -> Self {
        super::broadcast_start(&backends, &ctx, &name, start);
        let inner = Arc::new(SpanInner {
            backends,
            ctx,
            state: Mutex::new(GuardState::default()),
        });
        Self { inner: Some(inner) }
    }

    /// Cheap no-op guard: every method is a no-op and drop emits nothing.
    pub(crate) fn noop() -> Self {
        Self { inner: None }
    }

    /// Capture the context of this span (for restore as an explicit parent).
    pub fn context(&self) -> SpanContext {
        match &self.inner {
            Some(inner) => inner.ctx.clone(),
            // A no-op guard has no meaningful identity; keep the empty marker
            // so a parent derived from it falls back to a fresh root trace.
            None => SpanContext::new(String::new(), String::new(), None),
        }
    }

    /// Set the span's status. The default status is `Ok`.
    pub fn set_status(&self, status: SpanStatus) {
        if let Some(inner) = &self.inner {
            inner.state.lock().status = status;
        }
    }
}

impl SpanRecorder for SpanGuard {
    fn set_attribute(&self, key: &str, value: serde_json::Value) {
        if let Some(inner) = &self.inner {
            inner.state.lock().attributes.insert(key.to_string(), value);
        }
    }
}

impl Drop for SpanGuard {
    fn drop(&mut self) {
        if let Some(inner) = &self.inner {
            let state = inner.state.lock();
            super::broadcast_end(
                &inner.backends,
                &inner.ctx,
                SystemTime::now(),
                &state.status,
                &state.attributes,
            );
        }
    }
}
