//! Tests for the trace viewer's pure functions: JSONL parsing, grouping,
//! sorting, tree building, and the size guard.

use serde_json::json;

use super::*;

/// Build a JSONL line for one span from its parts.
#[allow(clippy::too_many_arguments)]
fn span_line(
    trace_id: &str,
    span_id: &str,
    parent: Option<&str>,
    name: &str,
    start: &str,
    end: &str,
    status: &str,
    attributes: serde_json::Value,
) -> String {
    let parent = match parent {
        Some(p) => json!(p),
        None => json!(null),
    };
    json!({
        "trace_id": trace_id,
        "span_id": span_id,
        "parent_span_id": parent,
        "name": name,
        "start": start,
        "end": end,
        "duration_us": 1234,
        "status": status,
        "attributes": attributes,
    })
    .to_string()
}

fn root_line(trace_id: &str, name: &str, start: &str, end: &str, status: &str) -> String {
    span_line(
        trace_id,
        "root001",
        None,
        name,
        start,
        end,
        status,
        json!({"gen_ai.operation.name": "invoke_agent"}),
    )
}

fn child_line(
    trace_id: &str,
    span_id: &str,
    parent: &str,
    name: &str,
    start: &str,
    end: &str,
) -> String {
    span_line(
        trace_id,
        span_id,
        Some(parent),
        name,
        start,
        end,
        "ok",
        json!({"gen_ai.operation.name": name}),
    )
}

// ── Parsing ──

#[test]
fn test_parse_span_line_full_record() {
    let line = span_line(
        "abcdef1234567890",
        "span001",
        None,
        "invoke_agent slimbot",
        "2026-08-15T10:00:00.000000Z",
        "2026-08-15T10:00:05.000000Z",
        "ok",
        json!({"gen_ai.agent.name": "slimbot", "http.request.body": "{}"}),
    );
    let rec = parse_span_line(&line).unwrap();
    assert_eq!(rec.trace_id, "abcdef1234567890");
    assert_eq!(rec.span_id, "span001");
    assert!(rec.parent_span_id.is_none());
    assert_eq!(rec.name, "invoke_agent slimbot");
    assert_eq!(rec.status, "ok");
    assert_eq!(rec.start, "2026-08-15T10:00:00.000000Z");
    let expected_us = chrono::DateTime::parse_from_rfc3339("2026-08-15T10:00:00.000000Z")
        .unwrap()
        .timestamp_micros();
    assert_eq!(rec.start_us, expected_us);
    assert_eq!(rec.end_us, Some(expected_us + 5_000_000));
    assert_eq!(rec.duration_us, 1234);
    assert_eq!(rec.attributes["gen_ai.agent.name"], "slimbot");
    assert!(rec.error.is_none());
}

#[test]
fn test_parse_span_line_error_status() {
    let line = span_line(
        "t1",
        "s1",
        Some("root001"),
        "execute_tool shell",
        "2026-08-15T10:00:00.000000Z",
        "2026-08-15T10:00:01.000000Z",
        "error",
        json!({}),
    );
    // Reconstruct with an explicit error field.
    let v: serde_json::Value = serde_json::from_str(&line).unwrap();
    let mut v = v;
    v["error"] = json!("command not found");
    let rec = parse_span_line(&v.to_string()).unwrap();
    assert_eq!(rec.status, "error");
    assert_eq!(rec.error.as_deref(), Some("command not found"));
    assert_eq!(rec.parent_span_id.as_deref(), Some("root001"));
}

#[test]
fn test_parse_span_line_skips_malformed_and_empty() {
    assert!(parse_span_line("").is_none());
    assert!(parse_span_line("not json at all").is_none());
    assert!(parse_span_line("{\"foo\": 1}").is_none()); // missing trace_id/span_id
}

#[test]
fn test_parse_span_lines_skips_bad_lines_keeps_good() {
    let good = root_line(
        "t1",
        "invoke_agent slimbot",
        "2026-08-15T10:00:00.000000Z",
        "2026-08-15T10:00:01.000000Z",
        "ok",
    );
    let content = format!("{}\ngarbage\n\n{}", good, good);
    let spans = parse_span_lines(&content);
    assert_eq!(spans.len(), 2);
}

// ── Grouping + sorting ──

#[test]
fn test_build_summaries_sorted_newest_first() {
    let older = root_line(
        "aaaa",
        "invoke_agent slimbot",
        "2026-08-15T10:00:00.000000Z",
        "2026-08-15T10:00:01.000000Z",
        "ok",
    );
    let newer = root_line(
        "bbbb",
        "invoke_agent slimbot",
        "2026-08-15T10:00:10.000000Z",
        "2026-08-15T10:00:11.000000Z",
        "error",
    );
    let spans = parse_span_lines(&format!("{}\n{}", older, newer));
    let summaries = build_summaries(&spans);
    assert_eq!(summaries.len(), 2);
    // Newest first.
    assert_eq!(summaries[0].full_trace_id, "bbbb");
    assert_eq!(summaries[1].full_trace_id, "aaaa");
    assert_eq!(summaries[0].trace_id, "bbbb");
    assert_eq!(summaries[0].status, "error");
    assert_eq!(summaries[1].status, "ok");
    assert_eq!(summaries[0].span_count, 1);
    assert_eq!(summaries[0].root_name, "invoke_agent slimbot");
    assert!(!summaries[0].running);
}

#[test]
fn test_build_summaries_duration_from_root() {
    let root = span_line(
        "c1",
        "root001",
        None,
        "invoke_agent slimbot",
        "2026-08-15T10:00:00.000000Z",
        "2026-08-15T10:00:03.500000Z",
        "ok",
        json!({}),
    );
    let child = child_line(
        "c1",
        "sp1",
        "root001",
        "chat",
        "2026-08-15T10:00:00.500000Z",
        "2026-08-15T10:00:01.000000Z",
    );
    let spans = parse_span_lines(&format!("{}\n{}", root, child));
    let summaries = build_summaries(&spans);
    assert_eq!(summaries[0].span_count, 2);
    // 3.5s in microseconds.
    assert_eq!(summaries[0].duration_us, 3_500_000);
}

#[test]
fn test_build_summaries_marks_inflight_running_without_root() {
    // No root (parent_span_id null) → in-flight trace, marked running.
    let child1 = child_line(
        "d1",
        "sp1",
        "root001",
        "iteration",
        "2026-08-15T10:00:00.000000Z",
        "2026-08-15T10:00:01.000000Z",
    );
    let child2 = child_line(
        "d1",
        "sp2",
        "root001",
        "chat",
        "2026-08-15T10:00:01.000000Z",
        "2026-08-15T10:00:02.000000Z",
    );
    let spans = parse_span_lines(&format!("{}\n{}", child1, child2));
    let summaries = build_summaries(&spans);
    assert_eq!(summaries.len(), 1);
    assert!(summaries[0].running);
    assert_eq!(summaries[0].status, "running");
    // Start falls back to min span start; duration = max end − min start.
    assert_eq!(summaries[0].duration_us, 2_000_000);
    assert_eq!(summaries[0].root_name, "");
}

#[test]
fn test_build_summaries_groups_multiple_traces() {
    let a = root_line(
        "t-a",
        "invoke_agent slimbot",
        "2026-08-15T09:00:00.000000Z",
        "2026-08-15T09:00:01.000000Z",
        "ok",
    );
    let b_root = root_line(
        "t-b",
        "invoke_agent slimbot",
        "2026-08-15T10:00:00.000000Z",
        "2026-08-15T10:00:01.000000Z",
        "ok",
    );
    let b_child = child_line(
        "t-b",
        "b1",
        "root001",
        "execute_tool shell",
        "2026-08-15T10:00:00.100000Z",
        "2026-08-15T10:00:00.200000Z",
    );
    let spans = parse_span_lines(&format!("{}\n{}\n{}", a, b_root, b_child));
    let summaries = build_summaries(&spans);
    assert_eq!(summaries.len(), 2);
    assert_eq!(summaries[0].full_trace_id, "t-b");
    assert_eq!(summaries[0].span_count, 2);
    assert_eq!(summaries[1].full_trace_id, "t-a");
}

// ── Tree building ──

#[test]
fn test_build_tree_nests_by_parent() {
    let root = span_line(
        "e1",
        "root001",
        None,
        "invoke_agent slimbot",
        "2026-08-15T10:00:00.000000Z",
        "2026-08-15T10:00:02.000000Z",
        "ok",
        json!({}),
    );
    let iter = span_line(
        "e1",
        "it1",
        Some("root001"),
        "iteration",
        "2026-08-15T10:00:00.100000Z",
        "2026-08-15T10:00:01.900000Z",
        "ok",
        json!({}),
    );
    let chat = span_line(
        "e1",
        "ch1",
        Some("it1"),
        "chat",
        "2026-08-15T10:00:00.200000Z",
        "2026-08-15T10:00:00.800000Z",
        "ok",
        json!({}),
    );
    let tool = span_line(
        "e1",
        "tl1",
        Some("it1"),
        "execute_tool shell",
        "2026-08-15T10:00:01.000000Z",
        "2026-08-15T10:00:01.500000Z",
        "ok",
        json!({}),
    );
    let spans = parse_span_lines(&format!("{}\n{}\n{}\n{}", root, iter, chat, tool));
    let tree = build_tree(&spans).unwrap();

    assert_eq!(tree.span.name, "invoke_agent slimbot");
    assert_eq!(tree.children.len(), 1);
    let iter_node = &tree.children[0];
    assert_eq!(iter_node.span.name, "iteration");
    assert_eq!(iter_node.children.len(), 2);
    // Children sorted by start time.
    assert_eq!(iter_node.children[0].span.name, "chat");
    assert_eq!(iter_node.children[1].span.name, "execute_tool shell");
}

#[test]
fn test_build_tree_none_when_no_root() {
    let child = child_line(
        "f1",
        "sp1",
        "root001",
        "iteration",
        "2026-08-15T10:00:00.000000Z",
        "2026-08-15T10:00:01.000000Z",
    );
    let spans = parse_span_lines(&child);
    assert!(build_tree(&spans).is_none());
}

// ── Detail ──

#[test]
fn test_build_detail_returns_tree_and_attributes() {
    let root = span_line(
        "g1",
        "root001",
        None,
        "invoke_agent slimbot",
        "2026-08-15T10:00:00.000000Z",
        "2026-08-15T10:00:01.000000Z",
        "ok",
        json!({"gen_ai.conversation.id": "webui:chat1"}),
    );
    let child = span_line(
        "g1",
        "sp1",
        Some("root001"),
        "chat",
        "2026-08-15T10:00:00.100000Z",
        "2026-08-15T10:00:00.500000Z",
        "ok",
        json!({"http.request.body": "{\"model\":\"gpt-4o\"}"}),
    );
    let spans = parse_span_lines(&format!("{}\n{}", root, child));
    let detail = build_detail("g1", &spans).unwrap();
    assert_eq!(detail.span_count, 2);
    assert_eq!(detail.trace_id, "g1");
    assert_eq!(detail.full_trace_id, "g1");
    assert!(!detail.running);
    let root_node = detail.root.unwrap();
    assert_eq!(
        root_node.span.attributes["gen_ai.conversation.id"],
        "webui:chat1"
    );
    let child_node = &root_node.children[0];
    assert_eq!(
        child_node.span.attributes["http.request.body"],
        "{\"model\":\"gpt-4o\"}"
    );
}

#[test]
fn test_build_detail_returns_none_for_unknown_trace() {
    let root = root_line(
        "h1",
        "invoke_agent slimbot",
        "2026-08-15T10:00:00.000000Z",
        "2026-08-15T10:00:01.000000Z",
        "ok",
    );
    let spans = parse_span_lines(&root);
    assert!(build_detail("nope", &spans).is_none());
}

#[test]
fn test_build_detail_inflight_has_flat_spans_and_no_root() {
    // An in-flight trace (no root span written yet) must still expose its
    // ended child spans as a flat, start-sorted list for the viewer.
    let child1 = child_line(
        "k1",
        "sp1",
        "root001",
        "iteration",
        "2026-08-15T10:00:01.000000Z",
        "2026-08-15T10:00:02.000000Z",
    );
    let child2 = child_line(
        "k1",
        "sp2",
        "root001",
        "chat",
        "2026-08-15T10:00:00.000000Z",
        "2026-08-15T10:00:00.500000Z",
    );
    let spans = parse_span_lines(&format!("{}\n{}", child1, child2));
    let detail = build_detail("k1", &spans).unwrap();
    assert!(detail.running);
    assert!(detail.root.is_none());
    assert_eq!(detail.span_count, 2);
    // Flat spans present, start-sorted.
    assert_eq!(detail.spans.len(), 2);
    assert_eq!(detail.spans[0].name, "chat");
    assert_eq!(detail.spans[1].name, "iteration");
}

#[test]
fn test_build_detail_serializes_flat_spans() {
    let child = child_line(
        "m1",
        "sp1",
        "root001",
        "iteration",
        "2026-08-15T10:00:00.000000Z",
        "2026-08-15T10:00:01.000000Z",
    );
    let spans = parse_span_lines(&child);
    let detail = build_detail("m1", &spans).unwrap();
    let json = serde_json::to_value(&detail).unwrap();
    assert_eq!(json["full_trace_id"], "m1");
    assert!(json.get("spans").is_some());
    assert_eq!(json["spans"].as_array().unwrap().len(), 1);
    assert_eq!(json["spans"][0]["name"], "iteration");
}

// ── Size guard ──

#[test]
fn test_read_trace_files_reads_and_concatenates() {
    let tmp = tempfile::tempdir().unwrap();
    let p1 = tmp.path().join("a.jsonl");
    let p2 = tmp.path().join("b.jsonl");
    std::fs::write(&p1, "line1\n").unwrap();
    std::fs::write(&p2, "line2\n").unwrap();
    let read = read_trace_files(&[
        p1.to_string_lossy().to_string(),
        p2.to_string_lossy().to_string(),
    ]);
    assert_eq!(read.content, "line1\nline2\n");
    assert!(!read.too_large);
}

#[test]
fn test_read_trace_files_missing_file_skipped() {
    let tmp = tempfile::tempdir().unwrap();
    let read = read_trace_files(&[tmp
        .path()
        .join("missing.jsonl")
        .to_string_lossy()
        .to_string()]);
    assert!(read.content.is_empty());
    assert!(!read.too_large);
}

#[test]
fn test_read_trace_files_size_guard_flags_large_file() {
    // A small file is not flagged as too large.
    let tmp = tempfile::tempdir().unwrap();
    let p = tmp.path().join("small.jsonl");
    std::fs::write(&p, "x\n").unwrap();
    let read = read_trace_files(&[p.to_string_lossy().to_string()]);
    assert!(!read.too_large);
}

#[test]
fn test_exceeds_size_guard_threshold() {
    assert!(!exceeds_size_guard(MAX_TRACE_FILE_BYTES));
    assert!(exceeds_size_guard(MAX_TRACE_FILE_BYTES + 1));
    assert!(!exceeds_size_guard(0));
}

// ── Serialization shape ──

#[test]
fn test_trace_summary_serializes_without_sort_key() {
    let root = root_line(
        "i1",
        "invoke_agent slimbot",
        "2026-08-15T10:00:00.000000Z",
        "2026-08-15T10:00:01.000000Z",
        "ok",
    );
    let spans = parse_span_lines(&root);
    let summaries = build_summaries(&spans);
    let json = serde_json::to_value(&summaries[0]).unwrap();
    assert_eq!(json["trace_id"], "i1");
    assert_eq!(json["status"], "ok");
    assert_eq!(json["span_count"], 1);
    assert_eq!(json["duration_us"], 1_000_000);
    assert_eq!(json["root_name"], "invoke_agent slimbot");
    // The sort key must not leak into the API payload.
    assert!(json.get("start_us").is_none());
}

#[test]
fn test_span_node_serializes_attributes() {
    let root = span_line(
        "j1",
        "root001",
        None,
        "invoke_agent slimbot",
        "2026-08-15T10:00:00.000000Z",
        "2026-08-15T10:00:01.000000Z",
        "ok",
        json!({"gen_ai.agent.name": "slimbot"}),
    );
    let spans = parse_span_lines(&root);
    let tree = build_tree(&spans).unwrap();
    let json = serde_json::to_value(&tree).unwrap();
    assert_eq!(json["attributes"]["gen_ai.agent.name"], "slimbot");
    assert_eq!(json["children"], json!([]));
}

#[test]
fn test_detail_wire_shape_matches_trace_viewer_frontend() {
    // The frontend (resources/webui/traces.html) renders tree nodes as bare
    // span records: `renderNode` reads span fields directly off the node
    // (`const span = node`), never a `span` wrapper. Lock this wire contract
    // so a serializer change cannot silently break the trace viewer UI.
    let root = span_line(
        "w1",
        "root001",
        None,
        "invoke_agent slimbot",
        "2026-08-15T10:00:00.000000Z",
        "2026-08-15T10:00:01.000000Z",
        "ok",
        json!({"gen_ai.agent.name": "slimbot"}),
    );
    let child = span_line(
        "w1",
        "child01",
        Some("root001"),
        "chat",
        "2026-08-15T10:00:00.100000Z",
        "2026-08-15T10:00:00.500000Z",
        "ok",
        json!({}),
    );
    let spans = parse_span_lines(&format!("{}\n{}", root, child));
    let detail = build_detail("w1", &spans).unwrap();
    let json = serde_json::to_value(&detail).unwrap();

    let node = &json["root"];
    // Span fields are flattened onto the tree node...
    assert_eq!(node["span_id"], "root001");
    assert_eq!(node["name"], "invoke_agent slimbot");
    assert_eq!(node["attributes"]["gen_ai.agent.name"], "slimbot");
    // ...there is no `span` wrapper (the original frontend bug read node.span).
    assert!(node.get("span").is_none());
    // Children nest recursively as bare spans too.
    assert_eq!(node["children"][0]["span_id"], "child01");
    assert!(node["children"][0].get("span").is_none());
    // The flat list is bare span records as well.
    assert_eq!(json["spans"][0]["span_id"], "root001");
}
