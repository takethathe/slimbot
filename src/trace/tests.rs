//! Tests for the trace core: span lifecycle, RAII, tracer composition.
//!
//! These tests observe only external behavior — the span events a recording
//! backend receives — never internal structure.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::SystemTime;

use super::{SpanGuard, SpanRecorder, SpanStatus, TraceBackend, Tracer};
use crate::trace::span::SpanContext;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RecordedKind {
    Start,
    End,
}

#[derive(Debug, Clone)]
struct RecordedSpan {
    kind: RecordedKind,
    ctx: SpanContext,
    name: String,
    start: Option<SystemTime>,
    end: Option<SystemTime>,
    status: Option<SpanStatus>,
    attributes: BTreeMap<String, serde_json::Value>,
}

/// Recording mock backend: collects every lifecycle event.
#[derive(Default)]
struct RecordingBackend {
    records: Arc<parking_lot::Mutex<Vec<RecordedSpan>>>,
}

impl RecordingBackend {
    fn all(&self) -> Vec<RecordedSpan> {
        self.records.lock().clone()
    }

    fn starts(&self) -> Vec<RecordedSpan> {
        self.all()
            .into_iter()
            .filter(|r| r.kind == RecordedKind::Start)
            .collect()
    }

    fn ends(&self) -> Vec<RecordedSpan> {
        self.all()
            .into_iter()
            .filter(|r| r.kind == RecordedKind::End)
            .collect()
    }
}

impl TraceBackend for RecordingBackend {
    fn on_span_start(&self, ctx: &SpanContext, name: &str, start: SystemTime) {
        self.records.lock().push(RecordedSpan {
            kind: RecordedKind::Start,
            ctx: ctx.clone(),
            name: name.to_string(),
            start: Some(start),
            end: None,
            status: None,
            attributes: BTreeMap::new(),
        });
    }

    fn on_span_end(
        &self,
        ctx: &SpanContext,
        end: SystemTime,
        status: &SpanStatus,
        attributes: &BTreeMap<String, serde_json::Value>,
    ) {
        self.records.lock().push(RecordedSpan {
            kind: RecordedKind::End,
            ctx: ctx.clone(),
            name: String::new(),
            start: None,
            end: Some(end),
            status: Some(status.clone()),
            attributes: attributes.clone(),
        });
    }
}

/// Backend that panics on every event.
struct PanickingBackend;

impl TraceBackend for PanickingBackend {
    fn on_span_start(&self, _ctx: &SpanContext, _name: &str, _start: SystemTime) {
        panic!("backend start boom");
    }

    fn on_span_end(
        &self,
        _ctx: &SpanContext,
        _end: SystemTime,
        _status: &SpanStatus,
        _attributes: &BTreeMap<String, serde_json::Value>,
    ) {
        panic!("backend end boom");
    }
}

fn assert_send_sync<T: Send + Sync>() {}

// ── ID formats ──

#[test]
fn test_span_context_id_hex_formats() {
    let ctx = SpanContext::new_root();
    assert_eq!(ctx.trace_id.len(), 32);
    assert!(ctx.trace_id.chars().all(|c| c.is_ascii_hexdigit()));
    assert_eq!(ctx.span_id.len(), 16);
    assert!(ctx.span_id.chars().all(|c| c.is_ascii_hexdigit()));
    assert_eq!(ctx.parent_span_id, None);
}

#[test]
fn test_span_context_parent_linkage() {
    let parent = SpanContext::new_root();
    let child = SpanContext::new_child(Some(&parent));
    assert_eq!(child.trace_id, parent.trace_id);
    assert_eq!(
        child.parent_span_id.as_deref(),
        Some(parent.span_id.as_str())
    );
    assert_ne!(child.span_id, parent.span_id);
}

#[test]
fn test_span_context_empty_parent_falls_back_to_root() {
    let noop = SpanContext::new(String::new(), String::new(), None);
    let child = SpanContext::new_child(Some(&noop));
    assert_eq!(child.parent_span_id, None);
    assert_eq!(child.trace_id.len(), 32);
}

// ── RAII lifecycle ──

#[test]
fn test_guard_construction_emits_start_and_drop_emits_end() {
    let backend = Arc::new(RecordingBackend::default());
    let tracer = Tracer::new(vec![backend.clone()]);

    {
        let _guard = tracer.start_span("agent.run", None);
        assert_eq!(backend.starts().len(), 1);
        assert_eq!(backend.ends().len(), 0);
    }

    assert_eq!(backend.starts().len(), 1);
    let ends = backend.ends();
    assert_eq!(ends.len(), 1);
    assert_eq!(ends[0].ctx.trace_id, backend.starts()[0].ctx.trace_id);
    assert!(ends[0].end.unwrap() >= backend.starts()[0].start.unwrap());
    assert_eq!(ends[0].status, Some(SpanStatus::Ok));
}

#[test]
fn test_guard_drop_emits_end_with_accumulated_attributes() {
    let backend = Arc::new(RecordingBackend::default());
    let tracer = Tracer::new(vec![backend.clone()]);

    let guard = tracer.start_span("provider.chat", None);
    guard.set_attribute("model", serde_json::json!("gpt-4o"));
    guard.set_attribute("total_tokens", serde_json::json!(123));
    drop(guard);

    // The recording backend stores the name on the start event; locate the
    // matching end event by span id linkage.
    let start = backend.starts().into_iter().next().unwrap();
    let end = backend
        .ends()
        .into_iter()
        .find(|e| e.ctx.span_id == start.ctx.span_id)
        .unwrap();
    assert_eq!(start.name, "provider.chat");
    assert_eq!(
        end.attributes.get("model"),
        Some(&serde_json::json!("gpt-4o"))
    );
    assert_eq!(
        end.attributes.get("total_tokens"),
        Some(&serde_json::json!(123))
    );
}

#[test]
fn test_guard_set_status_error_recorded_on_end() {
    let backend = Arc::new(RecordingBackend::default());
    let tracer = Tracer::new(vec![backend.clone()]);

    let guard = tracer.start_span("iteration", None);
    guard.set_status(SpanStatus::Error("boom".to_string()));
    drop(guard);

    let start = backend.starts()[0].clone();
    let end = backend
        .ends()
        .into_iter()
        .find(|e| e.ctx.span_id == start.ctx.span_id)
        .unwrap();
    assert_eq!(end.status, Some(SpanStatus::Error("boom".to_string())));
}

#[test]
fn test_guard_is_send_and_sync() {
    assert_send_sync::<SpanGuard>();
}

#[tokio::test]
async fn test_guard_held_across_await() {
    let backend = Arc::new(RecordingBackend::default());
    let tracer = Tracer::new(vec![backend.clone()]);

    let guard = tracer.start_span("provider.chat", None);
    tokio::task::yield_now().await;
    guard.set_attribute("k", serde_json::json!("v"));
    drop(guard);

    assert_eq!(backend.ends().len(), 1);
}

#[test]
fn test_guard_implements_span_recorder_trait_object() {
    let backend = Arc::new(RecordingBackend::default());
    let tracer = Tracer::new(vec![backend.clone()]);

    let guard = tracer.start_span("provider.chat", None);
    let recorder: &dyn SpanRecorder = &guard;
    recorder.set_attribute("http.request.body", serde_json::json!("{...}"));
    drop(guard);

    let end = backend.ends().into_iter().next().unwrap();
    assert_eq!(
        end.attributes.get("http.request.body"),
        Some(&serde_json::json!("{...}"))
    );
}

// ── Tracer composition ──

#[test]
fn test_tracer_parent_linkage_via_capture_restore() {
    let backend = Arc::new(RecordingBackend::default());
    let tracer = Tracer::new(vec![backend.clone()]);

    let root = tracer.start_span("agent.run", None);
    let root_ctx = root.context();
    let child = tracer.start_span("iteration", Some(&root_ctx));
    let child_ctx = child.context();
    drop(child);
    drop(root);

    let child_start = backend
        .starts()
        .into_iter()
        .find(|s| s.ctx.span_id == child_ctx.span_id)
        .unwrap();
    assert_eq!(child_start.ctx.trace_id, root_ctx.trace_id);
    assert_eq!(
        child_start.ctx.parent_span_id.as_deref(),
        Some(root_ctx.span_id.as_str())
    );
}

#[test]
fn test_tracer_zero_backends_returns_cheap_noop_guard() {
    let tracer = Tracer::new(vec![]);
    assert!(!tracer.is_enabled());

    let guard = tracer.start_span("agent.run", None);
    guard.set_attribute("k", serde_json::json!("v"));
    guard.set_status(SpanStatus::Error("x".to_string()));
    let ctx = guard.context();
    drop(guard);
    // No panics, empty context, and everything is a no-op.
    assert_eq!(ctx.trace_id, "");
    assert_eq!(ctx.span_id, "");
    assert_eq!(ctx.parent_span_id, None);
}

#[test]
fn test_tracer_fan_out_all_backends_receive_start_and_end() {
    let backend_a = Arc::new(RecordingBackend::default());
    let backend_b = Arc::new(RecordingBackend::default());
    let tracer = Tracer::new(vec![backend_a.clone(), backend_b.clone()]);

    let guard = tracer.start_span("iteration", None);
    drop(guard);

    assert_eq!(backend_a.starts().len(), 1);
    assert_eq!(backend_a.ends().len(), 1);
    assert_eq!(backend_b.starts().len(), 1);
    assert_eq!(backend_b.ends().len(), 1);
}

#[test]
fn test_backend_panic_does_not_break_other_backends_or_guard() {
    let panicking = Arc::new(PanickingBackend);
    let recording = Arc::new(RecordingBackend::default());
    let tracer = Tracer::new(vec![panicking, recording.clone()]);

    // Must not panic even though the first backend panics on start and end.
    let guard = tracer.start_span("agent.run", None);
    guard.set_attribute("k", serde_json::json!("v"));
    drop(guard);

    assert_eq!(recording.starts().len(), 1);
    assert_eq!(recording.ends().len(), 1);
}

// ── Cross-thread capture/restore ──

#[tokio::test(flavor = "multi_thread")]
async fn test_capture_restore_across_tokio_task() {
    let backend = Arc::new(RecordingBackend::default());
    let tracer = Tracer::new(vec![backend.clone()]);

    let root = tracer.start_span("agent.run", None);
    let root_ctx = root.context();

    // Move the captured context into a new task (potentially another OS
    // thread); no ambient thread-local state is involved.
    let tracer_for_task = tracer.clone();
    let root_ctx_for_task = root_ctx.clone();
    let handle = tokio::spawn(async move {
        let child = tracer_for_task.start_span("iteration", Some(&root_ctx_for_task));
        let child_ctx = child.context();
        drop(child);
        child_ctx
    });

    let child_ctx = handle.await.unwrap();
    drop(root);

    let child_start = backend
        .starts()
        .into_iter()
        .find(|s| s.ctx.span_id == child_ctx.span_id)
        .unwrap();
    assert_eq!(child_start.ctx.trace_id, root_ctx.trace_id);
    assert_eq!(
        child_start.ctx.parent_span_id.as_deref(),
        Some(root_ctx.span_id.as_str())
    );
    // Both spans landed in the same trace.
    let trace_ids: std::collections::HashSet<_> = backend
        .all()
        .iter()
        .map(|r| r.ctx.trace_id.clone())
        .collect();
    assert_eq!(trace_ids.len(), 1);
}

/// ADR 0001: the trace module is a leaf — it knows nothing about the runner,
/// the hooks, or `AgentEvent`. That is what lets any layer emit spans without a
/// dependency edge back into it, and what keeps trace and events independent
/// (neither derives from the other).
#[test]
fn test_trace_module_stays_a_leaf() {
    for (name, src) in [
        ("mod.rs", include_str!("mod.rs")),
        ("span.rs", include_str!("span.rs")),
    ] {
        let code: String = src
            .lines()
            .filter(|line| !line.trim_start().starts_with("//"))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            !code.contains("crate::"),
            "trace/{name} must not reach back into the crate: {code}"
        );
    }
}
