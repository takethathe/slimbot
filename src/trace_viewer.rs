//! Pure, unit-testable functions that power the WebUI trace viewer.
//!
//! This module is the service side of `/traces`: it parses the trace JSONL
//! file(s), groups spans by trace id, sorts traces by start time (newest
//! first), and builds the span tree for a single trace. The HTTP handlers in
//! `channel/webui.rs` are thin shells over these functions.
//!
//! The module deliberately knows nothing about HTTP, the session layer, or the
//! config singleton. Parsing / grouping / sorting / tree-building work on
//! parsed JSON records so they are verifiable without starting a server;
//! the small file-reading helpers are isolated and unit-tested with tempdirs.

use std::collections::HashMap;

use serde::Serialize;

/// Size guard threshold: files larger than this still get read (a warning is
/// surfaced), but we refuse to silently load unbounded data.
pub const MAX_TRACE_FILE_BYTES: u64 = 50 * 1024 * 1024; // 50 MiB

/// A single parsed span record from one JSONL line.
#[derive(Debug, Clone, Serialize)]
pub struct SpanRecord {
    pub trace_id: String,
    pub span_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parent_span_id: Option<String>,
    pub name: String,
    /// RFC3339 start timestamp as written by the file backend.
    pub start: String,
    #[serde(skip)]
    pub start_us: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub end: Option<String>,
    #[serde(skip)]
    pub end_us: Option<i64>,
    pub duration_us: u64,
    /// "ok" | "error"
    pub status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    pub attributes: serde_json::Map<String, serde_json::Value>,
}

/// A trace as shown in the list: one row per trace, newest first.
#[derive(Debug, Clone, Serialize)]
pub struct TraceSummary {
    /// Short prefix of the full trace id (e.g. 12 hex chars).
    pub trace_id: String,
    /// Full trace id (used to fetch the detail view).
    pub full_trace_id: String,
    /// Local display time of the trace start (root start; min span start if no root).
    pub start: String,
    /// "ok" | "error" | "running"
    pub status: String,
    pub span_count: usize,
    /// Root end − root start; falls back to max end − min start.
    pub duration_us: u64,
    /// Root span name (e.g. `invoke_agent slimbot`), empty when no root.
    pub root_name: String,
    pub running: bool,
    /// Exact start micros used for sorting; not serialized.
    #[serde(skip)]
    pub start_us: i64,
}

/// A node in a trace's span tree.
#[derive(Debug, Clone, Serialize)]
pub struct SpanNode {
    #[serde(flatten)]
    pub span: SpanRecord,
    pub children: Vec<SpanNode>,
}

/// Full detail for one trace: the span tree plus summary fields.
#[derive(Debug, Clone, Serialize)]
pub struct TraceDetail {
    pub trace_id: String,
    pub start: String,
    pub status: String,
    pub span_count: usize,
    pub duration_us: u64,
    pub running: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub root: Option<SpanNode>,
    /// Flat list of all spans (start-sorted) — lets the frontend render an
    /// in-flight trace's ended child spans even before a root completes.
    pub spans: Vec<SpanRecord>,
}

/// Result of reading the trace file(s) with a size guard applied.
#[derive(Debug, Default)]
pub struct TraceFileRead {
    pub content: String,
    /// True when any file exceeded `MAX_TRACE_FILE_BYTES` (still read fully).
    pub too_large: bool,
}

/// True when a file of `byte_len` bytes exceeds the size guard threshold.
///
/// Kept as a pure function so both branches are directly testable; a large
/// file is still read fully (with a warning), not refused.
pub fn exceeds_size_guard(byte_len: u64) -> bool {
    byte_len > MAX_TRACE_FILE_BYTES
}

/// Read all enabled trace files, concatenating their contents.
///
/// Files larger than `MAX_TRACE_FILE_BYTES` are still read (with a warning
/// surfaced via `too_large`), so a big trace file degrades gracefully instead
/// of blowing up the server. Missing files are skipped.
pub fn read_trace_files(paths: &[String]) -> TraceFileRead {
    let mut out = TraceFileRead::default();
    for path in paths {
        let expanded = crate::path::expand_home(path);
        let too_large = std::fs::metadata(&expanded)
            .map(|m| exceeds_size_guard(m.len()))
            .unwrap_or(false);
        if too_large {
            out.too_large = true;
        }
        if let Ok(content) = std::fs::read_to_string(&expanded) {
            if content.is_empty() {
                continue;
            }
            if !out.content.is_empty() && !out.content.ends_with('\n') {
                out.content.push('\n');
            }
            out.content.push_str(&content);
        }
    }
    out
}

/// Earliest modification time (epoch ms) across the enabled trace files.
///
/// With `reset_on_start` (the default) the files are truncated at Gateway
/// startup, so this is the current process's startup time. Returns `None`
/// when no file exists yet.
pub fn trace_file_startup_ms(paths: &[String]) -> Option<i64> {
    paths
        .iter()
        .filter_map(|p| {
            let expanded = crate::path::expand_home(p);
            std::fs::metadata(&expanded)
                .and_then(|m| m.modified())
                .ok()
                .and_then(|t| {
                    t.duration_since(std::time::UNIX_EPOCH)
                        .ok()
                        .map(|d| d.as_millis() as i64)
                })
        })
        .min()
}

/// Parse every JSONL line into a [`SpanRecord`], skipping malformed lines.
pub fn parse_span_lines(content: &str) -> Vec<SpanRecord> {
    content.lines().filter_map(parse_span_line).collect()
}

/// Parse a single JSONL line into a [`SpanRecord`], or `None` when malformed.
pub fn parse_span_line(line: &str) -> Option<SpanRecord> {
    let line = line.trim();
    if line.is_empty() {
        return None;
    }
    let v: serde_json::Value = serde_json::from_str(line).ok()?;
    let trace_id = v.get("trace_id")?.as_str()?.to_string();
    let span_id = v.get("span_id")?.as_str()?.to_string();
    let name = v
        .get("name")
        .and_then(|n| n.as_str())
        .unwrap_or("")
        .to_string();
    let parent_span_id = v
        .get("parent_span_id")
        .and_then(|p| p.as_str())
        .map(String::from);
    let start_str = v
        .get("start")
        .and_then(|s| s.as_str())
        .unwrap_or("")
        .to_string();
    let end_str = v
        .get("end")
        .and_then(|s| s.as_str())
        .unwrap_or("")
        .to_string();
    let duration_us = v.get("duration_us").and_then(|d| d.as_u64()).unwrap_or(0);
    let status = v
        .get("status")
        .and_then(|s| s.as_str())
        .unwrap_or("ok")
        .to_string();
    let error = v.get("error").and_then(|e| e.as_str()).map(String::from);
    let attributes = v
        .get("attributes")
        .and_then(|a| a.as_object())
        .cloned()
        .unwrap_or_default();

    let start_us = parse_rfc3339_micros(&start_str);
    let end_us = if end_str.is_empty() {
        None
    } else {
        Some(parse_rfc3339_micros(&end_str))
    };

    Some(SpanRecord {
        trace_id,
        span_id,
        parent_span_id,
        name,
        start: start_str,
        start_us,
        end: if end_str.is_empty() {
            None
        } else {
            Some(end_str)
        },
        end_us,
        duration_us,
        status,
        error,
        attributes,
    })
}

/// Parse an RFC3339 timestamp (as written by the file backend, with
/// microseconds) into epoch microseconds. Falls back to 0 when unparseable.
fn parse_rfc3339_micros(s: &str) -> i64 {
    chrono::DateTime::parse_from_rfc3339(s)
        .map(|dt| dt.timestamp_micros())
        .unwrap_or(0)
}

/// Build the summary rows for all traces, sorted by start time descending
/// (newest first).
pub fn build_summaries(spans: &[SpanRecord]) -> Vec<TraceSummary> {
    let mut by_trace: HashMap<String, Vec<&SpanRecord>> = HashMap::new();
    for span in spans {
        by_trace
            .entry(span.trace_id.clone())
            .or_default()
            .push(span);
    }

    let mut summaries: Vec<TraceSummary> = by_trace
        .into_values()
        .map(|trace_spans| summarize_trace(&trace_spans))
        .collect();
    summaries.sort_by(|a, b| {
        // Start time desc (newest first); tie-break on the full id for
        // determinism.
        b.start_us
            .cmp(&a.start_us)
            .then_with(|| b.full_trace_id.cmp(&a.full_trace_id))
    });
    summaries
}

/// Summarize a single trace's spans (already grouped by trace id).
fn summarize_trace(spans: &[&SpanRecord]) -> TraceSummary {
    let roots: Vec<&&SpanRecord> = spans
        .iter()
        .filter(|s| s.parent_span_id.is_none())
        .collect();

    let (start_us, status, duration_us, root_name, running) = match roots.first() {
        Some(root) => {
            // Root ended → completed trace. Duration = root end − root start.
            let running = root.end.is_none();
            let start_us = root.start_us;
            let end_us = root.end_us.unwrap_or_else(|| max_end_us(spans));
            let status = if running {
                "running".to_string()
            } else {
                root.status.clone()
            };
            let duration_us = (end_us - start_us).max(0) as u64;
            (start_us, status, duration_us, root.name.clone(), running)
        }
        None => {
            // No root span yet → in-flight trace. Start = min span start,
            // duration = max end − min start.
            let start_us = spans.iter().map(|s| s.start_us).min().unwrap_or(0);
            let end_us = max_end_us(spans);
            (
                start_us,
                "running".to_string(),
                (end_us - start_us).max(0) as u64,
                String::new(),
                true,
            )
        }
    };

    TraceSummary {
        trace_id: short_trace_id(&spans[0].trace_id),
        full_trace_id: spans[0].trace_id.clone(),
        start: format_local_time(start_us),
        status,
        span_count: spans.len(),
        duration_us,
        root_name,
        running,
        start_us,
    }
}

fn max_end_us(spans: &[&SpanRecord]) -> i64 {
    spans
        .iter()
        .filter_map(|s| s.end_us)
        .max()
        .unwrap_or_else(|| spans.iter().map(|s| s.start_us).max().unwrap_or(0))
}

/// Short prefix of a trace id (first 12 hex chars), matching common tracing UIs.
fn short_trace_id(full: &str) -> String {
    full.chars().take(12).collect()
}

/// Format an epoch-micros timestamp as a local display string.
fn format_local_time(us: i64) -> String {
    let secs = us.div_euclid(1_000_000);
    let nanos = (us.rem_euclid(1_000_000) * 1000) as u32;
    let dt = chrono::DateTime::from_timestamp(secs, nanos)
        .unwrap_or_default()
        .with_timezone(&chrono::Local);
    dt.format("%Y-%m-%d %H:%M:%S%.3f").to_string()
}

/// Build the full span tree for one trace, nested by `parent_span_id`.
///
/// The root is the span with `parent_span_id == None`. When the trace is
/// in-flight (no root present), returns `None` and callers render the flat
/// span list instead.
pub fn build_tree(spans: &[SpanRecord]) -> Option<SpanNode> {
    let roots: Vec<&SpanRecord> = spans
        .iter()
        .filter(|s| s.parent_span_id.is_none())
        .collect();
    let root = roots.first()?;

    let mut children: HashMap<&str, Vec<&SpanRecord>> = HashMap::new();
    for s in spans {
        if let Some(parent) = &s.parent_span_id {
            children.entry(parent.as_str()).or_default().push(s);
        }
    }

    fn build(node: &SpanRecord, children: &HashMap<&str, Vec<&SpanRecord>>) -> SpanNode {
        let kids: Vec<SpanNode> = children
            .get(node.span_id.as_str())
            .map(|list| {
                let mut sorted = list.clone();
                // Sort children once by start so sibling order is deterministic.
                sorted.sort_by_key(|s| s.start_us);
                sorted.iter().map(|c| build(c, children)).collect()
            })
            .unwrap_or_default();
        SpanNode {
            span: node.clone(),
            children: kids,
        }
    }

    Some(build(root, &children))
}

/// Build the detail payload for a single trace id, or `None` when no such
/// trace exists in the parsed spans.
pub fn build_detail(trace_id: &str, spans: &[SpanRecord]) -> Option<TraceDetail> {
    let trace_spans: Vec<&SpanRecord> = spans.iter().filter(|s| s.trace_id == trace_id).collect();
    if trace_spans.is_empty() {
        return None;
    }
    let summary = summarize_trace(&trace_spans);
    let mut spans: Vec<SpanRecord> = trace_spans.into_iter().cloned().collect();
    spans.sort_by_key(|s| s.start_us);
    let root = build_tree(&spans);
    Some(TraceDetail {
        trace_id: summary.trace_id,
        start: summary.start,
        status: summary.status,
        span_count: summary.span_count,
        duration_us: summary.duration_us,
        running: summary.running,
        root,
        spans,
    })
}

#[cfg(test)]
mod tests;
