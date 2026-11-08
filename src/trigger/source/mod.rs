//! Data sources: collection, snapshots and health.
//!
//! A source is a directory with a collector. Once per tick the due sources run
//! concurrently, their `{items: [...]}` output is normalized, and the result is
//! committed into an in-memory snapshot. Nothing is written back to disk — the
//! snapshot is the only place external content ever lives, which is why there
//! is no path to leak it later.
//!
//! Two properties matter more than throughput:
//!
//! * **One instant per tick.** The snapshot is frozen when the tick starts and
//!   committed when it ends, so every condition in one tick reads the same
//!   world. A source that is mid-write can never produce a half-new view.
//! * **Failure is local.** One source timing out, throwing or being broken on
//!   disk never affects the others; it is booked against that source alone and
//!   the tick still commits what succeeded.

pub mod host;
pub mod meta;
pub mod normalize;
pub mod shape;

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{Value, json};

use crate::codemode::{
    ENTRY_COLLECT, HostEnv, Limits, Redactor, RunRequest, ScriptError, execute_async,
};
use crate::debug;
use crate::mcp::McpService;
use crate::warn_log;

use host::CollectHost;
use meta::{LoadedSource, SourceMeta};
use normalize::Entry;

/// How long a collected item stays visible. Older items are dropped from the
/// view, so a stale answer can never be mistaken for the current one.
pub const DEFAULT_TTL: Duration = Duration::from_secs(24 * 60 * 60);

/// Consecutive failures after which a source is collected less often.
const DEGRADED_AFTER_FAILURES: u32 = 3;
/// Cadence for a degraded source, measured in ticks.
const DEGRADED_EVERY_N_TICKS: u64 = 5;

/// One frozen view of a source as a condition sees it.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct SourceView {
    pub items: Vec<Value>,
    pub count: usize,
    pub observed_at: Option<i64>,
}

impl SourceView {
    /// The `ctx.sources[id]` payload handed to a condition script.
    pub fn to_json(&self) -> Value {
        json!({
            "items": self.items,
            "count": self.count,
            "observed_at": self.observed_at,
        })
    }
}

/// The world as it looked when a tick started.
#[derive(Debug, Clone, Default)]
pub struct TickFrame {
    views: BTreeMap<String, SourceView>,
}

impl TickFrame {
    pub fn view(&self, id: &str) -> Option<&SourceView> {
        self.views.get(id)
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Failure {
    pub id: String,
    pub error: String,
}

/// What one tick did. Empty (`due` empty) means nothing was scheduled, which
/// is also the signal that the tick must not produce a trace span.
#[derive(Debug, Clone, Default)]
pub struct TickReport {
    pub tick_index: u64,
    pub due: Vec<String>,
    pub succeeded: Vec<String>,
    pub failed: Vec<Failure>,
    pub warnings: Vec<String>,
    pub frame: TickFrame,
}

impl TickReport {
    pub fn is_idle(&self) -> bool {
        self.due.is_empty()
    }
}

/// Health of one source, as `trigger_source list` reports it.
#[derive(Debug, Clone)]
pub struct SourceStatus {
    pub meta: SourceMeta,
    pub status: &'static str,
    pub last_collected_at: Option<i64>,
    pub items_count: usize,
    pub shape: Value,
    pub last_error: Option<String>,
}

struct Runtime {
    meta: SourceMeta,
    script: Option<String>,
    /// Committed items, oldest first, already TTL-pruned.
    entries: Vec<Entry>,
    observed_at: Option<i64>,
    failures: u32,
    last_error: Option<String>,
    /// A disk-level problem (missing script, unreadable meta).
    problem: Option<String>,
}

impl Runtime {
    fn new(loaded: LoadedSource) -> Self {
        Self {
            meta: loaded.meta,
            script: loaded.script,
            entries: Vec::new(),
            observed_at: None,
            failures: 0,
            last_error: None,
            problem: loaded.problem,
        }
    }

    /// Cadence in ticks: normal, or slowed down while degraded.
    fn every_n_ticks(&self) -> u64 {
        let base = u64::from(self.meta.every_n_ticks.max(1));
        if self.failures >= DEGRADED_AFTER_FAILURES {
            base.max(DEGRADED_EVERY_N_TICKS)
        } else {
            base
        }
    }
}

/// A source that is due in the current tick, copied out of the lock.
#[derive(Debug, Clone)]
struct DueSource {
    id: String,
    meta: SourceMeta,
    script: Option<String>,
    problem: Option<String>,
    /// Current window, needed by `append` mode for fingerprint dedupe.
    existing: Vec<Entry>,
}

/// In-memory snapshot store plus the collect driver.
pub struct SourceLayer {
    root: PathBuf,
    workspace: PathBuf,
    secrets: Arc<BTreeMap<String, String>>,
    ttl: Duration,
    interval: Duration,
    inner: Mutex<BTreeMap<String, Runtime>>,
    tick_index: AtomicU64,
    /// The process-wide connector pool, attached by the wiring layer once the
    /// loop has brought it up (the layers are built first, the pool second). A
    /// layer without a pool collects exactly as it did before ticket 17.
    connectors: Mutex<Option<Arc<McpService>>>,
}

impl SourceLayer {
    pub fn new(
        root: PathBuf,
        workspace: PathBuf,
        secrets: Arc<BTreeMap<String, String>>,
        interval: Duration,
        ttl: Duration,
    ) -> Self {
        let layer = Self {
            root,
            workspace,
            secrets,
            ttl,
            interval,
            inner: Mutex::new(BTreeMap::new()),
            tick_index: AtomicU64::new(0),
            connectors: Mutex::new(None),
        };
        layer.reload();
        layer
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Hand the collector the process-wide connector pool (ticket 17).
    ///
    /// A setter rather than a constructor parameter because the pool comes up
    /// with the agent loop, after these layers are built; `None` (or never
    /// calling this) leaves the collector's surface untouched.
    pub fn attach_connectors(&self, connectors: Option<Arc<McpService>>) {
        *self
            .connectors
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = connectors;
    }

    fn connectors(&self) -> Option<Arc<McpService>> {
        self.connectors
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }

    /// Re-read the sources directory. New sources appear, deleted ones vanish,
    /// and edited metadata or scripts take effect on the next tick — no
    /// restart, because the write-test-fix loop is the normal workflow.
    pub fn reload(&self) {
        let loaded = meta::load_all(&self.root);
        let mut inner = match self.inner.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        let mut seen = Vec::with_capacity(loaded.len());
        for source in loaded {
            let id = source.meta.id.clone();
            seen.push(id.clone());
            match inner.get_mut(&id) {
                Some(runtime) => {
                    runtime.meta = source.meta;
                    runtime.script = source.script;
                    runtime.problem = source.problem;
                }
                None => {
                    inner.insert(id, Runtime::new(source));
                }
            }
        }
        inner.retain(|id, _| seen.contains(id));
    }

    /// Advance the tick counter and return the new index. The index drives
    /// `every_n_ticks`, so it must be a counter and never a wall clock.
    pub fn begin_tick(&self) -> u64 {
        self.tick_index.fetch_add(1, Ordering::SeqCst) + 1
    }

    /// Ids that are due at `tick_index`.
    pub fn due(&self, tick_index: u64) -> Vec<String> {
        let inner = self.lock();
        inner
            .values()
            .filter(|runtime| tick_index.is_multiple_of(runtime.every_n_ticks()))
            .map(|runtime| runtime.meta.id.clone())
            .collect()
    }

    fn due_sources(&self, tick_index: u64) -> Vec<DueSource> {
        let inner = self.lock();
        inner
            .values()
            .filter(|runtime| tick_index.is_multiple_of(runtime.every_n_ticks()))
            .map(|runtime| DueSource {
                id: runtime.meta.id.clone(),
                meta: runtime.meta.clone(),
                script: runtime.script.clone(),
                problem: runtime.problem.clone(),
                existing: runtime.entries.clone(),
            })
            .collect()
    }

    /// Freeze the current snapshots. Everything read from this frame keeps
    /// describing the tick's start even after the tick commits.
    pub fn freeze(&self) -> TickFrame {
        let now = meta::now_ms();
        let ttl_ms = self.ttl.as_millis() as i64;
        let inner = self.lock();
        let mut views = BTreeMap::new();
        for (id, runtime) in inner.iter() {
            let items: Vec<Value> = runtime
                .entries
                .iter()
                .filter(|e| now.saturating_sub(e.at) <= ttl_ms)
                .map(|e| e.item.clone())
                .collect();
            let observed_at = runtime.observed_at;
            views.insert(
                id.clone(),
                SourceView {
                    count: items.len(),
                    items,
                    observed_at,
                },
            );
        }
        TickFrame { views }
    }

    /// Run one collection round over the due sources.
    pub async fn collect(&self, tick_index: u64, max_concurrent: usize) -> TickReport {
        let frame = self.freeze();
        let due = self.due_sources(tick_index);
        let due_ids: Vec<String> = due.iter().map(|d| d.id.clone()).collect();
        if due.is_empty() {
            return TickReport {
                tick_index,
                frame,
                ..Default::default()
            };
        }

        let permits = Arc::new(tokio::sync::Semaphore::new(max_concurrent.max(1)));
        let connectors = self.connectors();
        let mut tasks = Vec::with_capacity(due.len());
        for source in due {
            let permits = permits.clone();
            let workspace = self.workspace.clone();
            let secrets = self.secrets.clone();
            let connectors = connectors.clone();
            tasks.push(tokio::spawn(async move {
                let _permit = match permits.acquire().await {
                    Ok(permit) => permit,
                    Err(_) => return (source.id.clone(), Err("collector cancelled".to_string())),
                };
                let outcome = collect_one(workspace, secrets, connectors, &source).await;
                (source.id, outcome)
            }));
        }

        let mut report = TickReport {
            tick_index,
            due: due_ids,
            frame,
            ..Default::default()
        };
        for (index, task) in tasks.into_iter().enumerate() {
            match task.await {
                Ok((id, Ok(normalized))) => {
                    if normalized.entries.is_empty() {
                        debug!("[trigger] source {} returned no items", id);
                    }
                    for warning in &normalized.warnings {
                        warn_log!("[trigger] source {}: {}", id, warning);
                        report.warnings.push(format!("{id}: {warning}"));
                    }
                    self.commit(&id, normalized.entries);
                    report.succeeded.push(id);
                }
                Ok((id, Err(error))) => {
                    self.record_failure(&id, &error);
                    report.failed.push(Failure { id, error });
                }
                Err(join_error) => {
                    // A panicking collector must not take the tick down.
                    let id = report
                        .due
                        .get(index)
                        .cloned()
                        .unwrap_or_else(|| "unknown".to_string());
                    let error = format!("collector did not return: {join_error}");
                    self.record_failure(&id, &error);
                    report.failed.push(Failure { id, error });
                }
            }
        }
        report
    }

    /// Convenience: advance the counter, then collect.
    pub async fn tick(&self, max_concurrent: usize) -> TickReport {
        self.reload();
        let index = self.begin_tick();
        self.collect(index, max_concurrent).await
    }

    fn commit(&self, id: &str, entries: Vec<Entry>) {
        let now = meta::now_ms();
        let ttl_ms = self.ttl.as_millis() as i64;
        let mut inner = self.lock();
        if let Some(runtime) = inner.get_mut(id) {
            let mut entries = entries;
            normalize::prune_expired(&mut entries, ttl_ms, now);
            runtime.entries = entries;
            runtime.observed_at = Some(now);
            runtime.failures = 0;
            runtime.last_error = None;
            runtime.problem = None;
        }
    }

    fn record_failure(&self, id: &str, error: &str) {
        let mut inner = self.lock();
        if let Some(runtime) = inner.get_mut(id) {
            runtime.failures += 1;
            runtime.last_error = Some(error.to_string());
            if runtime.failures >= DEGRADED_AFTER_FAILURES {
                debug!(
                    "[trigger] source {} failed {} times in a row; collecting every {} ticks",
                    id, runtime.failures, DEGRADED_EVERY_N_TICKS
                );
            } else {
                warn_log!("[trigger] source {} collection failed: {}", id, error);
            }
        }
    }

    /// Health and shape for every known source, for `trigger_source list`.
    pub fn status(&self) -> Vec<SourceStatus> {
        let now = meta::now_ms();
        let ttl_ms = self.ttl.as_millis() as i64;
        let inner = self.lock();
        inner
            .values()
            .map(|runtime| {
                let live: Vec<&Entry> = runtime
                    .entries
                    .iter()
                    .filter(|e| now.saturating_sub(e.at) <= ttl_ms)
                    .collect();
                let problems = runtime.problem.is_some();
                let status = if runtime.failures > 0 || problems {
                    "degraded"
                } else if runtime.observed_at.is_none() {
                    "pending"
                } else {
                    "ready"
                };
                let declared = runtime.meta.output_schema.as_ref();
                SourceStatus {
                    meta: runtime.meta.clone(),
                    status,
                    last_collected_at: runtime.observed_at,
                    items_count: live.len(),
                    shape: {
                        let entries: Vec<Entry> = live.into_iter().cloned().collect();
                        shape::report(declared, &entries, runtime.observed_at, entries.len())
                    },
                    last_error: runtime
                        .last_error
                        .clone()
                        .or_else(|| runtime.problem.clone()),
                }
            })
            .collect()
    }

    pub fn get(&self, id: &str) -> Option<SourceMeta> {
        self.lock().get(id).map(|runtime| runtime.meta.clone())
    }

    /// A fresh view of one source, for `test` on a single rule.
    pub fn view(&self, id: &str) -> Option<SourceView> {
        self.freeze().view(id).cloned()
    }

    /// When this source next collects, as epoch milliseconds.
    pub fn next_collection_at(&self, id: &str) -> Option<i64> {
        let inner = self.lock();
        let runtime = inner.get(id)?;
        let every = runtime.every_n_ticks();
        let index = self.tick_index.load(Ordering::SeqCst);
        let wait = every - (index % every);
        Some(meta::now_ms() + (wait * self.interval.as_millis() as u64) as i64)
    }

    // -- writes (used by the tool layer) --

    pub fn add(&self, loaded: LoadedSource) {
        let id = loaded.meta.id.clone();
        let script = loaded.script.clone();
        let mut inner = self.lock();
        inner.insert(id.clone(), Runtime::new(loaded));
        if let Some(runtime) = inner.get_mut(&id) {
            runtime.script = script;
        }
    }

    pub fn forget(&self, id: &str) {
        self.lock().remove(id);
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, BTreeMap<String, Runtime>> {
        match self.inner.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        }
    }
}

async fn collect_one(
    workspace: PathBuf,
    secrets: Arc<BTreeMap<String, String>>,
    connectors: Option<Arc<McpService>>,
    source: &DueSource,
) -> Result<normalize::Normalized, String> {
    if let Some(problem) = &source.problem {
        return Err(problem.clone());
    }
    let Some(script) = source.script.clone() else {
        return Err(format!("{} is missing", meta::SCRIPT_FILE));
    };

    let redactor = Redactor::new(secrets.values().cloned());
    let env = HostEnv::new(redactor);
    let host = Arc::new(CollectHost::new(
        workspace,
        &source.meta.allow_paths,
        secrets.clone(),
        connectors,
    ));
    let limits = Limits::collect().with_timeout(Duration::from_secs(source.meta.timeout_s));
    let request = RunRequest::new(script, ENTRY_COLLECT).with_limits(limits);

    let raw = execute_async(request, host, env)
        .await
        .map_err(|e: ScriptError| e.to_string())?;
    normalize::normalize(
        &raw,
        source.meta.mode,
        source.meta.max_items,
        &source.existing,
        meta::now_ms(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::trigger::source::meta::{SourceMode, save_meta, save_script};

    const ONE_ITEM: &str = "function collect() { return {items: [{title: 'a'}, {title: 'b'}]}; }";

    fn layer(workspace: &Path, ttl: Duration) -> SourceLayer {
        SourceLayer::new(
            workspace.join("triggers").join("sources"),
            workspace.to_path_buf(),
            Arc::new(BTreeMap::new()),
            Duration::from_secs(60),
            ttl,
        )
    }

    /// Write one source directory. `script: None` leaves it half-built.
    fn write_source(workspace: &Path, id: &str, every: u32, script: Option<&str>) {
        let root = workspace.join("triggers").join("sources");
        let mut source = SourceMeta::new(id, id);
        source.every_n_ticks = every;
        source.timeout_s = 1;
        save_meta(&root, &source).unwrap();
        if let Some(script) = script {
            save_script(&root, id, script).unwrap();
        }
    }

    fn collect_script(workspace: &Path, id: &str, script: &str) {
        save_script(&workspace.join("triggers").join("sources"), id, script).unwrap();
    }

    #[tokio::test]
    async fn test_due_follows_the_tick_counter_not_the_clock() {
        let tmp = tempfile::tempdir().unwrap();
        write_source(tmp.path(), "src-0001", 3, Some(ONE_ITEM));
        let layer = layer(tmp.path(), DEFAULT_TTL);

        assert!(layer.due(1).is_empty());
        assert!(layer.due(2).is_empty());
        assert_eq!(layer.due(3), vec!["src-0001".to_string()]);
        assert!(layer.due(4).is_empty());
        assert_eq!(layer.due(6), vec!["src-0001".to_string()]);
    }

    #[tokio::test]
    async fn test_reload_keeps_the_snapshot_and_forgets_a_deleted_source() {
        let tmp = tempfile::tempdir().unwrap();
        write_source(tmp.path(), "src-0001", 1, Some(ONE_ITEM));
        let layer = layer(tmp.path(), DEFAULT_TTL);
        layer.collect(1, 1).await;
        assert_eq!(layer.view("src-0001").unwrap().count, 2);

        // Editing a definition is the normal repair loop: the new script is
        // picked up, and what was already collected stays visible. A reload
        // that reset the runtime would make every script fix cost a refetch.
        write_source(
            tmp.path(),
            "src-0001",
            1,
            Some("function collect() { return {items: [{title: 'c'}]}; }"),
        );
        layer.reload();
        assert_eq!(
            layer.view("src-0001").unwrap().count,
            2,
            "a reload must not wipe the snapshot"
        );
        assert_eq!(layer.due(1), vec!["src-0001".to_string()]);

        // Deleting the directory drops the source outright: no stale runtime,
        // no stale view, nothing left to collect.
        meta::remove_source(layer.root(), "src-0001").unwrap();
        layer.reload();
        assert!(layer.status().is_empty());
        assert!(layer.due(1).is_empty());
        assert!(layer.view("src-0001").is_none());
    }

    #[tokio::test]
    async fn test_collect_commits_items_into_the_snapshot() {
        let tmp = tempfile::tempdir().unwrap();
        write_source(tmp.path(), "src-0001", 1, Some(ONE_ITEM));
        let layer = layer(tmp.path(), DEFAULT_TTL);

        let report = layer.tick(2).await;
        assert_eq!(report.succeeded, vec!["src-0001".to_string()]);
        assert!(report.failed.is_empty());

        let view = layer.view("src-0001").expect("view after collect");
        assert_eq!(view.count, 2);
        assert!(view.observed_at.is_some());
    }

    #[tokio::test]
    async fn test_collector_reads_through_the_attached_connector_pool() {
        // The pool is handed to the layer after it is built (it comes up with
        // the loop), so this pins the hand-off: a collector reaches a read-only
        // connector through `ctx.tools`, and its item lands in the snapshot.
        use crate::mcp::service::test_support as mcp_test;

        let tmp = tempfile::tempdir().unwrap();
        let mut source = SourceMeta::new("src-0001", "src-0001");
        source.timeout_s = 5;
        let root = tmp.path().join("triggers").join("sources");
        save_meta(&root, &source).unwrap();
        save_script(
            &root,
            "src-0001",
            r#"async function collect(ctx) {
                const read = await ctx.tools.mcp__github__get_pull({number: 7});
                return { items: [{title: read}] };
            }"#,
        )
        .unwrap();

        let (pool, _peer) = mcp_test::online_service(
            "github",
            vec![json!({
                "name": "get_pull",
                "description": "Fetch one pull request",
                "annotations": {"readOnlyHint": true, "destructiveHint": false},
            })],
        );
        mcp_test::wait_for(|| pool.any_online(), "the fake server online").await;

        let layer = layer(tmp.path(), DEFAULT_TTL);
        layer.attach_connectors(Some(pool));
        let report = layer.tick(2).await;
        assert_eq!(report.succeeded, vec!["src-0001".to_string()]);
        assert!(report.failed.is_empty(), "{:?}", report.failed);

        let view = layer.view("src-0001").expect("view after collect");
        let title = view.items[0]["title"].as_str().unwrap_or_default();
        assert!(title.contains("pong"), "{title}");
        assert!(title.contains("<codemode_output"), "{title}");
    }

    #[tokio::test]
    async fn test_frame_describes_the_tick_start_not_its_end() {
        let tmp = tempfile::tempdir().unwrap();
        write_source(tmp.path(), "src-0001", 1, Some(ONE_ITEM));
        let layer = layer(tmp.path(), DEFAULT_TTL);

        let first = layer.tick(2).await;
        assert_eq!(first.frame.view("src-0001").unwrap().count, 0);

        let second = layer.tick(2).await;
        assert_eq!(second.frame.view("src-0001").unwrap().count, 2);
    }

    #[tokio::test]
    async fn test_expired_entries_leave_the_view() {
        let tmp = tempfile::tempdir().unwrap();
        write_source(tmp.path(), "src-0001", 1, Some(ONE_ITEM));
        // A 1ms TTL expires between the commit and the next read.
        let layer = layer(tmp.path(), Duration::from_millis(1));

        layer.tick(2).await;
        std::thread::sleep(Duration::from_millis(20));
        let view = layer.view("src-0001").unwrap();
        assert_eq!(view.count, 0);
        assert_eq!(layer.status()[0].items_count, 0);
    }

    #[tokio::test]
    async fn test_a_hanging_source_does_not_take_the_tick_down() {
        let tmp = tempfile::tempdir().unwrap();
        write_source(
            tmp.path(),
            "src-0001",
            1,
            Some("function collect() { while (true) {} }"),
        );
        write_source(tmp.path(), "src-0002", 1, Some(ONE_ITEM));
        let layer = layer(tmp.path(), DEFAULT_TTL);

        let report = layer.tick(4).await;

        assert_eq!(report.succeeded, vec!["src-0002".to_string()]);
        assert_eq!(report.failed.len(), 1);
        assert_eq!(report.failed[0].id, "src-0001");
        assert!(
            report.failed[0].error.contains("budget"),
            "{}",
            report.failed[0].error
        );
        // The healthy source still holds its data.
        assert_eq!(layer.view("src-0002").unwrap().count, 2);
    }

    #[tokio::test]
    async fn test_half_built_source_fails_alone() {
        let tmp = tempfile::tempdir().unwrap();
        write_source(tmp.path(), "src-0001", 1, None);
        write_source(tmp.path(), "src-0002", 1, Some(ONE_ITEM));
        let layer = layer(tmp.path(), DEFAULT_TTL);

        let report = layer.tick(2).await;

        assert_eq!(report.succeeded, vec!["src-0002".to_string()]);
        assert_eq!(report.failed[0].id, "src-0001");
        assert!(report.failed[0].error.contains("collect.js"));
    }

    #[tokio::test]
    async fn test_three_failures_degrade_the_cadence_and_one_success_restores_it() {
        let tmp = tempfile::tempdir().unwrap();
        write_source(tmp.path(), "src-0001", 1, Some("function collect( {"));
        let layer = layer(tmp.path(), DEFAULT_TTL);

        for expected_tick in 1..=3u64 {
            let report = layer.tick(2).await;
            assert_eq!(report.tick_index, expected_tick);
            assert_eq!(report.failed.len(), 1);
        }
        // Degraded: collected every fifth tick, so 4 and 5 are the next look.
        assert!(layer.due(4).is_empty());
        assert_eq!(layer.due(5), vec!["src-0001".to_string()]);
        assert_eq!(layer.status()[0].status, "degraded");
        assert!(layer.status()[0].last_error.is_some());

        // Fixing the script restores both the data and the cadence.
        collect_script(tmp.path(), "src-0001", ONE_ITEM);
        // Tick 4 is skipped entirely: the degraded cadence has not come round.
        let skipped = layer.tick(2).await;
        assert_eq!(skipped.tick_index, 4);
        assert!(skipped.is_idle());

        let report = layer.tick(2).await;
        assert_eq!(report.tick_index, 5);
        assert_eq!(report.succeeded, vec!["src-0001".to_string()]);
        assert_eq!(layer.due(6), vec!["src-0001".to_string()]);
        assert_eq!(layer.status()[0].status, "ready");
    }

    #[tokio::test]
    async fn test_collected_data_never_touches_the_disk() {
        let tmp = tempfile::tempdir().unwrap();
        write_source(tmp.path(), "src-0001", 1, Some(ONE_ITEM));
        let layer = layer(tmp.path(), DEFAULT_TTL);

        layer.tick(2).await;
        layer.tick(2).await;

        let dir = tmp.path().join("triggers").join("sources").join("src-0001");
        let mut names: Vec<String> = std::fs::read_dir(&dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().to_string())
            .collect();
        names.sort();
        assert_eq!(names, vec!["collect.js", "meta.json"]);
        // Nothing appeared at the workspace root either.
        let root_entries: Vec<String> = std::fs::read_dir(tmp.path())
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().to_string())
            .collect();
        assert_eq!(root_entries, vec!["triggers"]);
    }

    #[tokio::test]
    async fn test_snapshot_mode_replaces_and_append_mode_grows() {
        let tmp = tempfile::tempdir().unwrap();
        write_source(tmp.path(), "src-0001", 1, Some(ONE_ITEM));
        let root = tmp.path().join("triggers").join("sources");
        let mut meta = SourceMeta::new("src-0001", "src-0001");
        meta.mode = SourceMode::Append;
        save_meta(&root, &meta).unwrap();
        let layer = layer(tmp.path(), DEFAULT_TTL);

        layer.tick(2).await;
        assert_eq!(layer.view("src-0001").unwrap().count, 2);

        // Re-running the same collector in append mode dedupes by fingerprint,
        // so the window does not grow with every tick.
        layer.tick(2).await;
        assert_eq!(layer.view("src-0001").unwrap().count, 2);

        collect_script(
            tmp.path(),
            "src-0001",
            "function collect() { return {items: [{title: 'c'}]}; }",
        );
        layer.tick(2).await;
        assert_eq!(layer.view("src-0001").unwrap().count, 3);
    }
}
