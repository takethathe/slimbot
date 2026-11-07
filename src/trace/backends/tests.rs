//! Tests for the file and OTel backends.

use std::collections::BTreeMap;
use std::time::SystemTime;

use opentelemetry::trace::{SpanId, Status};
use opentelemetry_sdk::trace::InMemorySpanExporter;

use super::{FileBackend, OtelBackend};
use crate::trace::span::SpanContext;
use crate::trace::{SpanRecorder, SpanStatus, TraceBackend};

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

// ── OTLP HTTP export (end to end) ──

/// (request line, body) pairs captured by the fake collector.
type RecordedRequests = std::sync::Arc<std::sync::Mutex<Vec<(String, Vec<u8>)>>>;

/// Minimal OTLP/HTTP collector: records every request it receives and replies
/// 200. Enough to prove the exporter really speaks HTTP, what path it uses, and
/// that full bodies survive the hop.
struct FakeCollector {
    addr: String,
    requests: RecordedRequests,
    stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
    handle: Option<std::thread::JoinHandle<()>>,
}

impl FakeCollector {
    fn start() -> Self {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let addr = listener.local_addr().unwrap().to_string();
        let requests = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let sink = requests.clone();
        let stop_flag = stop.clone();
        let handle = std::thread::spawn(move || {
            while !stop_flag.load(std::sync::atomic::Ordering::Relaxed) {
                match listener.accept() {
                    Ok((stream, _)) => {
                        let _ = read_request(stream, &sink);
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(std::time::Duration::from_millis(20));
                    }
                    Err(_) => break,
                }
            }
        });
        Self {
            addr,
            requests,
            stop,
            handle: Some(handle),
        }
    }

    fn requests(&self) -> Vec<(String, Vec<u8>)> {
        self.requests.lock().unwrap().clone()
    }

    fn post_bodies(&self) -> Vec<(String, Vec<u8>)> {
        self.requests()
            .into_iter()
            .filter(|(line, _)| line.starts_with("POST "))
            .collect()
    }
}

impl Drop for FakeCollector {
    fn drop(&mut self) {
        self.stop.store(true, std::sync::atomic::Ordering::Relaxed);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

fn read_request(
    mut stream: std::net::TcpStream,
    sink: &std::sync::Mutex<Vec<(String, Vec<u8>)>>,
) -> std::io::Result<()> {
    use std::io::{Read, Write};

    stream.set_read_timeout(Some(std::time::Duration::from_secs(2)))?;
    let mut buf = Vec::new();
    let mut chunk = [0u8; 8192];
    let header_end = loop {
        let n = stream.read(&mut chunk)?;
        if n == 0 {
            return Ok(());
        }
        buf.extend_from_slice(&chunk[..n]);
        if let Some(pos) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            break pos + 4;
        }
    };
    let head = String::from_utf8_lossy(&buf[..header_end]).to_string();
    let content_length = head
        .lines()
        .find_map(|line| {
            let (key, value) = line.split_once(':')?;
            if key.eq_ignore_ascii_case("content-length") {
                value.trim().parse::<usize>().ok()
            } else {
                None
            }
        })
        .unwrap_or(0);
    while buf.len() < header_end + content_length {
        let n = stream.read(&mut chunk)?;
        if n == 0 {
            break;
        }
        buf.extend_from_slice(&chunk[..n]);
    }
    let end = (header_end + content_length).min(buf.len());
    let body = buf[header_end..end].to_vec();
    let request_line = head.lines().next().unwrap_or("").to_string();
    sink.lock().unwrap().push((request_line, body));

    stream.write_all(b"HTTP/1.1 200 OK\r\ncontent-length: 0\r\nconnection: close\r\n\r\n")?;
    stream.flush()
}

#[test]
fn test_otel_backend_exports_full_request_and_reply_over_otlp_http() {
    // Ticket 04: the only claim that matters for local observability is that a
    // span carrying the full request body and reply survives the real OTLP HTTP
    // hop. Parent linkage / error status are covered by the in-memory exporter
    // tests above; here we check transport, path and payload.
    let collector = FakeCollector::start();
    let backend = std::sync::Arc::new(
        OtelBackend::new(&format!("http://{}", collector.addr), "slimbot").unwrap(),
    );
    let tracer = crate::trace::Tracer::new(vec![backend.clone()]);

    let root = tracer.start_span("invoke_agent slimbot", None);
    let root_ctx = root.context();
    let child = tracer.start_span("chat", Some(&root_ctx));
    child.set_attribute(
        "http.request.body",
        serde_json::json!(r#"{"messages":[{"role":"user","content":"REQ-MARKER-9f3a"}]}"#),
    );
    child.set_attribute(
        "gen_ai.response.content",
        serde_json::json!("REPLY-MARKER-7c1d"),
    );
    child.set_status(SpanStatus::Ok);
    drop(child);
    drop(root);

    backend.force_flush();

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while collector.post_bodies().is_empty() {
        assert!(
            std::time::Instant::now() < deadline,
            "no OTLP export arrived; requests seen: {:?}",
            collector
                .requests()
                .iter()
                .map(|(line, body)| (line.clone(), body.len()))
                .collect::<Vec<_>>()
        );
        std::thread::sleep(std::time::Duration::from_millis(50));
    }

    let (request_line, body) = collector.post_bodies().remove(0);
    assert!(
        request_line.ends_with(" /v1/traces HTTP/1.1"),
        "the exporter must target the OTLP traces path: {request_line}"
    );
    // Protobuf carries strings verbatim, so a marker search is a faithful check
    // that the attribute values were exported unmodified.
    let body_text = String::from_utf8_lossy(&body);
    assert!(
        body_text.contains("REQ-MARKER-9f3a"),
        "the full request body must reach the collector"
    );
    assert!(
        body_text.contains("REPLY-MARKER-7c1d"),
        "the full reply must reach the collector"
    );
}

#[test]
fn test_otel_backend_normalizes_a_bare_endpoint_to_the_traces_path() {
    let normalize = OtelBackend::normalize_endpoint;
    assert_eq!(
        normalize("http://localhost:4318"),
        "http://localhost:4318/v1/traces"
    );
    assert_eq!(
        normalize("http://localhost:4318/"),
        "http://localhost:4318/v1/traces"
    );
    assert_eq!(
        normalize("http://collector.internal:4318/v1/traces"),
        "http://collector.internal:4318/v1/traces"
    );
    assert_eq!(
        normalize("https://gateway.example/otlp/v1/traces"),
        "https://gateway.example/otlp/v1/traces"
    );
}
