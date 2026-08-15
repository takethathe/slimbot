//! JSONL file backend: appends one record per finished span.

use std::collections::{BTreeMap, HashMap};
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::time::SystemTime;

use parking_lot::Mutex;

use crate::trace::{SpanContext, SpanStatus, TraceBackend};

struct StartedSpan {
    name: String,
    start: SystemTime,
}

/// Writes one JSONL line per finished span.
///
/// Writes are serialized under a mutex; a completed span is appended
/// synchronously, so a process crash loses only in-flight spans. Errors are
/// swallowed: the backend must never surface a failure to the run loop.
pub struct FileBackend {
    inner: Mutex<Inner>,
}

struct Inner {
    file: File,
    started: HashMap<String, StartedSpan>,
}

impl FileBackend {
    /// Open (creating if needed) a JSONL file at `path`, expanding `~`.
    pub fn new(path: &str) -> anyhow::Result<Self> {
        let expanded = crate::path::expand_home(path);
        if let Some(parent) = expanded.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&expanded)?;
        Ok(Self {
            inner: Mutex::new(Inner {
                file,
                started: HashMap::new(),
            }),
        })
    }
}

impl TraceBackend for FileBackend {
    fn on_span_start(&self, ctx: &SpanContext, name: &str, start: SystemTime) {
        self.inner.lock().started.insert(
            ctx.span_id.clone(),
            StartedSpan {
                name: name.to_string(),
                start,
            },
        );
    }

    fn on_span_end(
        &self,
        ctx: &SpanContext,
        end: SystemTime,
        status: &SpanStatus,
        attributes: &BTreeMap<String, serde_json::Value>,
    ) {
        let mut inner = self.inner.lock();
        let (name, start) = match inner.started.remove(&ctx.span_id) {
            Some(s) => (s.name, s.start),
            // A span we never saw start (e.g. a backend added mid-flight):
            // still record it, with a zero duration.
            None => (String::new(), end),
        };
        let duration_us = end
            .duration_since(start)
            .map(|d| u64::try_from(d.as_micros()).unwrap_or(u64::MAX))
            .unwrap_or(0);

        let mut record = serde_json::Map::new();
        record.insert("trace_id".to_string(), serde_json::json!(ctx.trace_id));
        record.insert("span_id".to_string(), serde_json::json!(ctx.span_id));
        record.insert(
            "parent_span_id".to_string(),
            match &ctx.parent_span_id {
                Some(p) => serde_json::json!(p),
                None => serde_json::Value::Null,
            },
        );
        record.insert("name".to_string(), serde_json::json!(name));
        record.insert("start".to_string(), serde_json::json!(format_time(start)));
        record.insert("end".to_string(), serde_json::json!(format_time(end)));
        record.insert("duration_us".to_string(), serde_json::json!(duration_us));
        match status {
            SpanStatus::Ok => {
                record.insert("status".to_string(), serde_json::json!("ok"));
            }
            SpanStatus::Error(msg) => {
                record.insert("status".to_string(), serde_json::json!("error"));
                record.insert("error".to_string(), serde_json::json!(msg));
            }
        }
        record.insert(
            "attributes".to_string(),
            serde_json::Value::Object(attributes.clone().into_iter().collect()),
        );

        let line = serde_json::Value::Object(record).to_string();
        // `File` is unbuffered: the write lands in the OS page cache, so a
        // completed span survives a process crash without an explicit flush.
        let _ = writeln!(inner.file, "{line}");
    }
}

fn format_time(t: SystemTime) -> String {
    let dt: chrono::DateTime<chrono::Utc> = t.into();
    dt.to_rfc3339_opts(chrono::SecondsFormat::Micros, true)
}
