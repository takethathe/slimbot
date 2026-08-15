//! OpenTelemetry backend: mirrors each span into an OTel SDK span and exports
//! it over OTLP HTTP.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use opentelemetry::trace::{
    Span as OtelSpan, SpanContext as OtelSpanContext, SpanId, Status, TraceContextExt, TraceFlags,
    TraceId, TraceState, Tracer, TracerProvider,
};
use opentelemetry::{Context, KeyValue, Value};
use opentelemetry_otlp::WithExportConfig;
use opentelemetry_sdk::Resource;
use opentelemetry_sdk::trace::{
    BatchSpanProcessor, IdGenerator, SdkTracer, SdkTracerProvider, Span as SdkSpan, SpanExporter,
};
use parking_lot::Mutex;

use crate::trace::{SpanContext, SpanStatus, TraceBackend};

/// Bridges our span lifecycle into OpenTelemetry SDK spans exported over
/// OTLP HTTP (default endpoint `http://localhost:4318`).
///
/// `on_span_start` creates a live SDK span (visible to the batch processor);
/// `on_span_end` writes attributes + status and ends it. The batch processor
/// exports on a background thread; dropping the backend flushes and shuts the
/// provider down (best-effort, timeout-bounded).
pub struct OtelBackend {
    tracer: SdkTracer,
    provider: SdkTracerProvider,
    /// Serializes the whole start sequence so the pending id slots below can
    /// never be clobbered by a concurrent span creation.
    start_lock: Mutex<()>,
    /// Pending ids armed for the next SDK span; consumed by [`PendingIdGenerator`].
    pending_trace: Arc<Mutex<Option<TraceId>>>,
    pending_span: Arc<Mutex<Option<SpanId>>>,
    /// Open (not yet ended) SDK spans keyed by our span id.
    active_spans: Mutex<BTreeMap<String, SdkSpan>>,
}

impl OtelBackend {
    /// Flush buffered spans to the exporter without shutting the provider down.
    pub fn force_flush(&self) {
        let _ = self.provider.force_flush();
    }

    /// Build a backend exporting over OTLP HTTP to `endpoint`, advertising
    /// `service_name` on every exported resource.
    pub fn new(endpoint: &str, service_name: &str) -> anyhow::Result<Self> {
        let exporter = opentelemetry_otlp::SpanExporter::builder()
            .with_http()
            .with_endpoint(endpoint)
            .with_timeout(Duration::from_secs(10))
            .build()?;
        Ok(Self::with_exporter(exporter, service_name))
    }

    /// Build a backend over an arbitrary exporter (used by tests with an
    /// in-memory exporter). Span ids are preserved from [`SpanContext`].
    pub fn with_exporter<E: SpanExporter + Send + 'static>(
        exporter: E,
        service_name: &str,
    ) -> Self {
        let pending_trace = Arc::new(Mutex::new(None));
        let pending_span = Arc::new(Mutex::new(None));
        let provider = Self::build_provider(
            exporter,
            service_name,
            pending_trace.clone(),
            pending_span.clone(),
        );
        let tracer = provider.tracer("slimbot");
        Self {
            tracer,
            provider,
            start_lock: Mutex::new(()),
            pending_trace,
            pending_span,
            active_spans: Mutex::new(BTreeMap::new()),
        }
    }

    fn build_provider<E: SpanExporter + Send + 'static>(
        exporter: E,
        service_name: &str,
        pending_trace: Arc<Mutex<Option<TraceId>>>,
        pending_span: Arc<Mutex<Option<SpanId>>>,
    ) -> SdkTracerProvider {
        let processor = BatchSpanProcessor::builder(exporter).build();
        let resource = Resource::builder()
            .with_attribute(KeyValue::new("service.name", service_name.to_string()))
            .build();
        SdkTracerProvider::builder()
            .with_span_processor(processor)
            .with_id_generator(PendingIdGenerator {
                trace: pending_trace,
                span: pending_span,
            })
            .with_resource(resource)
            .build()
    }
}

impl TraceBackend for OtelBackend {
    fn on_span_start(&self, ctx: &SpanContext, name: &str, start: SystemTime) {
        // Serialize so the armed pending ids belong to the span we are about
        // to create, even across concurrent runs.
        let _serialize = self.start_lock.lock();

        let Ok(trace_id) = TraceId::from_hex(&ctx.trace_id) else {
            return;
        };
        let Ok(span_id) = SpanId::from_hex(&ctx.span_id) else {
            return;
        };

        // Arm the id generator so the SDK span reuses OUR trace/span ids.
        *self.pending_trace.lock() = Some(trace_id);
        *self.pending_span.lock() = Some(span_id);

        // Build the parent context from the recorded parent_span_id. The
        // parent is a remote-flagged context (the SDK does not hold the live
        // parent span), which keeps the exported parent_span_id intact.
        let parent_cx = match &ctx.parent_span_id {
            Some(parent_hex) => {
                let Ok(parent_span_id) = SpanId::from_hex(parent_hex) else {
                    return;
                };
                let parent_span_ctx = OtelSpanContext::new(
                    trace_id,
                    parent_span_id,
                    TraceFlags::SAMPLED,
                    true,
                    TraceState::default(),
                );
                Context::new().with_remote_span_context(parent_span_ctx)
            }
            None => Context::new(),
        };

        let builder = self
            .tracer
            .span_builder(name.to_string())
            .with_start_time(start);
        let span = self.tracer.build_with_context(builder, &parent_cx);
        self.active_spans.lock().insert(ctx.span_id.clone(), span);
    }

    fn on_span_end(
        &self,
        ctx: &SpanContext,
        end: SystemTime,
        status: &SpanStatus,
        attributes: &BTreeMap<String, serde_json::Value>,
    ) {
        let Some(mut span) = self.active_spans.lock().remove(&ctx.span_id) else {
            return;
        };
        for (key, value) in attributes {
            span.set_attribute(KeyValue::new(key.clone(), otel_value(value)));
        }
        span.set_status(match status {
            SpanStatus::Ok => Status::Ok,
            SpanStatus::Error(msg) => Status::error(msg.clone()),
        });
        span.end_with_timestamp(end);
    }
}

impl Drop for OtelBackend {
    fn drop(&mut self) {
        // Flush buffered spans and stop the batch processor. Timeout-bounded
        // and best-effort: a slow collector must not block the process.
        let _ = self.provider.shutdown_with_timeout(Duration::from_secs(2));
    }
}

/// Maps a serde_json value to an OTel attribute value; structured values are
/// stringified since OTel attributes are scalar.
fn otel_value(v: &serde_json::Value) -> Value {
    match v {
        serde_json::Value::String(s) => s.clone().into(),
        serde_json::Value::Bool(b) => (*b).into(),
        serde_json::Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                i.into()
            } else if let Some(f) = n.as_f64() {
                f.into()
            } else {
                n.to_string().into()
            }
        }
        other => other.to_string().into(),
    }
}

/// Id generator that replays ids armed by the backend, falling back to random
/// ids for spans created outside our instrumentation.
///
/// Invariant: the SDK calls `new_trace_id` then `new_span_id` exactly once
/// per span creation, and `on_span_start` arms both slots while holding
/// `start_lock`, so the replayed ids always belong to the span being created.
/// If a future SDK version changes that contract, ids silently fall back to
/// random ones — the fallback keeps tracing working, at the cost of the
/// exported ids no longer matching [`SpanContext`].
#[derive(Debug)]
struct PendingIdGenerator {
    trace: Arc<Mutex<Option<TraceId>>>,
    span: Arc<Mutex<Option<SpanId>>>,
}

impl IdGenerator for PendingIdGenerator {
    fn new_trace_id(&self) -> TraceId {
        self.trace
            .lock()
            .take()
            .unwrap_or_else(|| TraceId::from(uuid::Uuid::new_v4().as_u128()))
    }

    fn new_span_id(&self) -> SpanId {
        self.span
            .lock()
            .take()
            .unwrap_or_else(|| SpanId::from(rand::random::<u64>()))
    }
}
