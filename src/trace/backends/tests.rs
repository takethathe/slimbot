//! Tests for the file and OTel backends.

use std::collections::BTreeMap;
use std::time::SystemTime;

use opentelemetry::trace::{SpanId, Status};
use opentelemetry_sdk::trace::InMemorySpanExporter;

use super::{FileBackend, OtelBackend};
use crate::trace::span::SpanContext;
use crate::trace::{SpanStatus, TraceBackend};

// ── File backend ──

#[test]
fn test_file_backend_appends_one_jsonl_line_per_span() {
    let tmp = tempfile::TempDir::new().unwrap();
    let path = tmp.path().join("traces.jsonl");
    let backend = FileBackend::new(path.to_str().unwrap()).unwrap();

    let ctx = SpanContext::new_root();
    let mut attributes = BTreeMap::new();
    attributes.insert(
        "gen_ai.conversation.id".to_string(),
        serde_json::json!("s1"),
    );

    backend.on_span_start(&ctx, "invoke_agent slimbot", SystemTime::now());
    backend.on_span_end(&ctx, SystemTime::now(), &SpanStatus::Ok, &attributes);

    let content = std::fs::read_to_string(&path).unwrap();
    let lines: Vec<&str> = content.lines().collect();
    assert_eq!(
        lines.len(),
        1,
        "one finished span must produce exactly one line"
    );

    let record: serde_json::Value = serde_json::from_str(lines[0]).unwrap();
    assert_eq!(record["trace_id"], serde_json::json!(ctx.trace_id));
    assert_eq!(record["span_id"], serde_json::json!(ctx.span_id));
    assert_eq!(record["parent_span_id"], serde_json::Value::Null);
    assert_eq!(record["name"], "invoke_agent slimbot");
    assert_eq!(record["status"], "ok");
    assert!(record["start"].is_string());
    assert!(record["end"].is_string());
    assert!(record["duration_us"].is_number());
    assert_eq!(record["attributes"]["gen_ai.conversation.id"], "s1");
}

#[test]
fn test_file_backend_writes_error_status_and_parent_link() {
    let tmp = tempfile::TempDir::new().unwrap();
    let path = tmp.path().join("traces.jsonl");
    let backend = FileBackend::new(path.to_str().unwrap()).unwrap();

    let parent = SpanContext::new_root();
    let child = SpanContext::new_child(Some(&parent));
    let mut attributes = BTreeMap::new();
    attributes.insert("gen_ai.tool.call.id".to_string(), serde_json::json!("tc-1"));

    backend.on_span_start(&child, "execute_tool shell", SystemTime::now());
    backend.on_span_end(
        &child,
        SystemTime::now(),
        &SpanStatus::Error("command not found".to_string()),
        &attributes,
    );

    let content = std::fs::read_to_string(&path).unwrap();
    let record: serde_json::Value = serde_json::from_str(content.lines().next().unwrap()).unwrap();
    assert_eq!(record["status"], "error");
    assert_eq!(record["error"], "command not found");
    assert_eq!(record["parent_span_id"], serde_json::json!(parent.span_id));
    assert_eq!(record["attributes"]["gen_ai.tool.call.id"], "tc-1");
}

#[test]
fn test_file_backend_creates_parent_dirs_and_expands_home() {
    let tmp = tempfile::TempDir::new().unwrap();
    let path = tmp.path().join("nested/dir/traces.jsonl");
    let backend = FileBackend::new(path.to_str().unwrap()).unwrap();

    let ctx = SpanContext::new_root();
    backend.on_span_start(&ctx, "iteration", SystemTime::now());
    backend.on_span_end(&ctx, SystemTime::now(), &SpanStatus::Ok, &BTreeMap::new());

    assert!(path.exists());
}

// ── OTel backend ──

#[test]
fn test_otel_backend_exports_span_with_our_ids() {
    let exporter = InMemorySpanExporter::default();
    let backend = OtelBackend::with_exporter(exporter.clone(), "test-service");

    let ctx = SpanContext::new_root();
    let mut attributes = BTreeMap::new();
    attributes.insert(
        "gen_ai.conversation.id".to_string(),
        serde_json::json!("s1"),
    );
    attributes.insert("total_tokens".to_string(), serde_json::json!(42));
    attributes.insert("ok".to_string(), serde_json::json!(true));

    backend.on_span_start(&ctx, "invoke_agent slimbot", SystemTime::now());
    backend.on_span_end(&ctx, SystemTime::now(), &SpanStatus::Ok, &attributes);
    backend.force_flush();

    let spans = exporter.get_finished_spans().unwrap();
    assert_eq!(spans.len(), 1);
    let span = &spans[0];
    assert_eq!(span.name.as_ref(), "invoke_agent slimbot");
    assert_eq!(span.span_context.trace_id().to_string(), ctx.trace_id);
    assert_eq!(span.span_context.span_id().to_string(), ctx.span_id);
    assert_eq!(span.parent_span_id, SpanId::INVALID);
    assert_eq!(span.status, Status::Ok);

    let attr: Vec<(String, String)> = span
        .attributes
        .iter()
        .map(|kv| (kv.key.as_str().to_string(), kv.value.to_string()))
        .collect();
    assert!(attr.contains(&("gen_ai.conversation.id".to_string(), "s1".to_string())));
    assert!(attr.contains(&("total_tokens".to_string(), "42".to_string())));
    assert!(attr.contains(&("ok".to_string(), "true".to_string())));
}

#[test]
fn test_otel_backend_preserves_parent_linkage_and_error_status() {
    let exporter = InMemorySpanExporter::default();
    let backend = OtelBackend::with_exporter(exporter.clone(), "test");

    let root = SpanContext::new_root();
    backend.on_span_start(&root, "invoke_agent slimbot", SystemTime::now());
    backend.on_span_end(&root, SystemTime::now(), &SpanStatus::Ok, &BTreeMap::new());

    let child = SpanContext::new_child(Some(&root));
    backend.on_span_start(&child, "iteration", SystemTime::now());
    backend.on_span_end(
        &child,
        SystemTime::now(),
        &SpanStatus::Error("boom".to_string()),
        &BTreeMap::new(),
    );
    backend.force_flush();

    let spans = exporter.get_finished_spans().unwrap();
    assert_eq!(spans.len(), 2);
    let child_span = spans
        .iter()
        .find(|s| s.name.as_ref() == "iteration")
        .unwrap();
    assert_eq!(
        child_span.span_context.trace_id().to_string(),
        root.trace_id
    );
    assert_eq!(child_span.parent_span_id.to_string(), root.span_id);
    match &child_span.status {
        Status::Error { description } => assert!(description.contains("boom")),
        other => panic!("expected error status, got {other:?}"),
    }
}

#[test]
fn test_otel_backend_exports_array_attribute() {
    let exporter = InMemorySpanExporter::default();
    let backend = OtelBackend::with_exporter(exporter.clone(), "test");

    let ctx = SpanContext::new_root();
    let mut attributes = BTreeMap::new();
    // `gen_ai.response.finish_reasons` is a string array in the spec.
    attributes.insert(
        "gen_ai.response.finish_reasons".to_string(),
        serde_json::json!(["stop"]),
    );

    backend.on_span_start(&ctx, "chat", SystemTime::now());
    backend.on_span_end(&ctx, SystemTime::now(), &SpanStatus::Ok, &attributes);
    backend.force_flush();

    let spans = exporter.get_finished_spans().unwrap();
    assert_eq!(spans.len(), 1);
    let span = &spans[0];
    let kv = span
        .attributes
        .iter()
        .find(|kv| kv.key.as_str() == "gen_ai.response.finish_reasons")
        .expect("finish_reasons attribute");
    assert!(matches!(
        &kv.value,
        opentelemetry::Value::Array(opentelemetry::Array::String(items))
            if items.len() == 1 && items[0].as_str() == "stop"
    ));
}

#[test]
fn test_otel_backend_otlp_construction_smoke() {
    // Building the real OTLP HTTP exporter must succeed without a collector;
    // nothing is exported until spans end.
    let backend = OtelBackend::new("http://localhost:4318", "slimbot").unwrap();
    drop(backend);
}
