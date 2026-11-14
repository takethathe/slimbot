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

use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{Value, json};

use crate::codemode::{
    ENTRY_COLLECT, HostEnv, Limits, Redactor, RunRequest, ScriptError, execute_async,
};
use crate::config::{McpServerConfig, McpTriggerSubscription};
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

/// Where a source comes from.
///
/// A `Disk` source owns a directory with a collector. The virtual kinds are
/// declared in the MCP config instead and hold no files: a `Notification`
/// source carries whatever the server last pushed, and a `Tool` source is one
/// connector tool called on every due tick. The origin is what `reload` uses to
/// keep virtual entries alive and what `trigger_source list` reports so an
/// agent can tell a collector it can edit from a subscription it cannot.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Origin {
    #[default]
    Disk,
    Notification,
    Tool,
}

impl Origin {
    pub fn as_str(self) -> &'static str {
        match self {
            Origin::Disk => "disk",
            Origin::Notification => "notification",
            Origin::Tool => "tool",
        }
    }

    /// True for the sources synthesized from config rather than loaded from a
    /// directory.
    pub fn is_virtual(self) -> bool {
        self != Origin::Disk
    }
}

/// Health of one source, as `trigger_source list` reports it.
#[derive(Debug, Clone)]
pub struct SourceStatus {
    pub meta: SourceMeta,
    pub origin: Origin,
    pub status: &'static str,
    pub last_collected_at: Option<i64>,
    pub items_count: usize,
    pub shape: Value,
    pub last_error: Option<String>,
}

struct Runtime {
    meta: SourceMeta,
    origin: Origin,
    /// How a tool source is reached, and what it is called with. `None` for
    /// disk and notification sources.
    call: Option<ToolCall>,
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
            origin: Origin::Disk,
            call: None,
            script: loaded.script,
            entries: Vec::new(),
            observed_at: None,
            failures: 0,
            last_error: None,
            problem: loaded.problem,
        }
    }

    /// A source synthesized from an MCP subscription: config is the whole
    /// truth, so a reload replaces the meta and the call without touching the
    /// committed window or the failure streak.
    fn virtual_source(meta: SourceMeta, origin: Origin, call: Option<ToolCall>) -> Self {
        Self {
            meta,
            origin,
            call,
            script: None,
            entries: Vec::new(),
            observed_at: None,
            failures: 0,
            last_error: None,
            problem: None,
        }
    }

    fn refresh(&mut self, meta: SourceMeta, origin: Origin, call: Option<ToolCall>) {
        self.meta = meta;
        self.origin = origin;
        self.call = call;
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
    origin: Origin,
    call: Option<ToolCall>,
    script: Option<String>,
    problem: Option<String>,
    /// Current window, needed by `append` mode for fingerprint dedupe.
    existing: Vec<Entry>,
}

/// One `type=tool` subscription, resolved to the callable it names.
#[derive(Debug, Clone, PartialEq)]
struct ToolCall {
    /// `mcp__<server>__<tool>`, the name the pool resolves to one server.
    callable: String,
    /// Arguments handed to every call; `null` when the config gave none.
    args: Value,
}

impl ToolCall {
    fn of(server: &McpServerConfig, subscription: &McpTriggerSubscription) -> Self {
        Self {
            callable: format!(
                "{}{}",
                crate::mcp::server_prefix(&server.name),
                crate::mcp::js_safe_ident(subscription.tool.trim())
            ),
            args: subscription.args.clone().unwrap_or(Value::Null),
        }
    }
}

/// The virtual sources one MCP config declares, keyed by `source_id`.
///
/// A server's subscriptions are the config's declaration of what it feeds the
/// trigger layer, so this is the only place the mapping from config to a source
/// runtime lives. Service ids always win over the general map.
fn virtual_sources(
    servers: &[McpServerConfig],
) -> BTreeMap<String, (SourceMeta, Origin, Option<ToolCall>)> {
    let mut out = BTreeMap::new();
    for server in servers {
        for subscription in &server.triggers {
            let id = subscription.source_id.trim();
            if id.is_empty() {
                continue;
            }
            let (origin, call, summary) = match subscription.r#type.as_str() {
                "tool" if !subscription.tool.trim().is_empty() => (
                    Origin::Tool,
                    Some(ToolCall::of(server, subscription)),
                    format!("mcp {} tool {}", server.name, subscription.tool.trim()),
                ),
                "notification" if !subscription.uri.trim().is_empty() => (
                    Origin::Notification,
                    None,
                    format!("mcp {} {}", server.name, subscription.uri.trim()),
                ),
                // Anything else is an incomplete subscription; `validate_mcp`
                // refuses to start on one, so there is nothing to declare.
                _ => continue,
            };
            let mut meta = SourceMeta::new(id, server.name.clone());
            meta.description = if subscription.description.trim().is_empty() {
                summary
            } else {
                subscription.description.trim().to_string()
            };
            // Service ids win: the first declaration owns the id.
            out.entry(id.to_string()).or_insert((meta, origin, call));
        }
    }
    out
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
    /// The MCP servers whose subscriptions declare virtual sources. Also
    /// attached after construction, because config reaches the trigger layer
    /// through the wiring that builds it.
    mcp_servers: Mutex<Vec<McpServerConfig>>,
    /// What the enabled rules read, pushed in after the rules reload and
    /// before the collect. `None` means "nobody has told me", which collects
    /// everything: a layer built on its own (a test, a wired-up layer before
    /// its first rule reload) must behave exactly as it did before reference
    /// counting existed. An *empty* set is the opposite and means "no rule
    /// refers to any source".
    referenced: Mutex<Option<HashSet<String>>>,
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
            mcp_servers: Mutex::new(Vec::new()),
            referenced: Mutex::new(None),
        };
        layer.reload();
        layer
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn workspace(&self) -> &Path {
        &self.workspace
    }

    /// The secret map the tick's collectors resolve `ctx.secret(name)` against.
    /// Handed out so a caller that runs a collector outside the tick (the
    /// `trigger_source` trial run) binds exactly the same credentials.
    pub fn secrets(&self) -> Arc<BTreeMap<String, String>> {
        self.secrets.clone()
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

    /// The connector pool the tick's collectors reach through `ctx.tools`, if
    /// one has been attached.
    pub fn connectors(&self) -> Option<Arc<McpService>> {
        self.connectors
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }

    /// Hand the layer the MCP servers whose subscriptions are virtual sources.
    ///
    /// A setter for the same reason as [`Self::attach_connectors`]: the layers
    /// are built from `triggers` alone, and the MCP section arrives with the
    /// wiring. The next `reload` reconciles against whatever is set here, so a
    /// removed subscription disappears at the next tick like a deleted
    /// directory does.
    pub fn attach_mcp_servers(&self, servers: &[McpServerConfig]) {
        *self
            .mcp_servers
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = servers.to_vec();
        self.reload();
    }

    /// Tell the layer which sources the enabled rules read.
    ///
    /// Pushed by the tick right after the rules reload and before the collect,
    /// so one reload of the rules is what decides which collectors run — a rule
    /// paused this tick stops its exclusive source on the same tick.
    pub fn set_referenced_sources(&self, ids: HashSet<String>) {
        *self
            .referenced
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(ids);
    }

    /// The ids the enabled rules read — i.e. the sources this tick will work
    /// on. `None` while nobody has pushed a reference set, which reads as "every
    /// source is active" (the same rule [`Self::is_active`] applies).
    pub fn active_source_ids(&self) -> Option<HashSet<String>> {
        self.referenced()
    }

    /// Whether a source should be worked on at all.
    fn is_active(&self, id: &str, referenced: &Option<HashSet<String>>) -> bool {
        match referenced {
            None => true,
            Some(ids) => ids.contains(id),
        }
    }

    fn referenced(&self) -> Option<HashSet<String>> {
        self.referenced
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }

    fn mcp_servers(&self) -> Vec<McpServerConfig> {
        self.mcp_servers
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }

    /// Re-read the sources directory. New sources appear, deleted ones vanish,
    /// and edited metadata or scripts take effect on the next tick — no
    /// restart, because the write-test-fix loop is the normal workflow.
    ///
    /// The virtual sources are reconciled in the same pass: every subscription
    /// in the MCP config is one entry, and an entry whose subscription is gone
    /// is dropped. Keeping them here rather than only in `freeze` is what lets
    /// a tick with nothing due still describe them.
    pub fn reload(&self) {
        let loaded = meta::load_all(&self.root);
        let declared = virtual_sources(&self.mcp_servers());
        let mut inner = match self.inner.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        let mut seen: Vec<String> = Vec::with_capacity(loaded.len() + declared.len());
        // A disk directory that claims a declared source id would otherwise
        // shadow the subscription; the config declaration wins.
        inner.retain(|id, runtime| {
            (runtime.origin.is_virtual() && declared.contains_key(id)) || !declared.contains_key(id)
        });
        for source in loaded {
            let id = source.meta.id.clone();
            if declared.contains_key(&id) {
                warn_log!(
                    "[trigger] source {id} is also declared by an MCP subscription; the subscription wins"
                );
                continue;
            }
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
        for (id, (meta, origin, call)) in declared {
            match inner.get_mut(&id) {
                Some(runtime) if runtime.origin.is_virtual() => {
                    runtime.refresh(meta, origin, call);
                }
                Some(_) => continue,
                None => {
                    inner.insert(id.clone(), Runtime::virtual_source(meta, origin, call));
                }
            }
            seen.push(id);
        }
        // Virtual entries are never re-appended from the directory scan, so
        // dropping one here is exactly what a removed subscription means.
        inner.retain(|id, _| seen.contains(id));
    }

    /// Advance the tick counter and return the new index. The index drives
    /// `every_n_ticks`, so it must be a counter and never a wall clock.
    pub fn begin_tick(&self) -> u64 {
        self.tick_index.fetch_add(1, Ordering::SeqCst) + 1
    }

    /// Ids that are due at `tick_index`: due by cadence *and* read by an
    /// enabled rule (always, when no reference set has been pushed).
    pub fn due(&self, tick_index: u64) -> Vec<String> {
        let inner = self.lock();
        let referenced = self.referenced();
        inner
            .values()
            .filter(|runtime| tick_index.is_multiple_of(runtime.every_n_ticks()))
            .filter(|runtime| self.is_active(&runtime.meta.id, &referenced))
            .map(|runtime| runtime.meta.id.clone())
            .collect()
    }

    fn due_sources(&self, tick_index: u64) -> Vec<DueSource> {
        let inner = self.lock();
        let referenced = self.referenced();
        inner
            .values()
            .filter(|runtime| tick_index.is_multiple_of(runtime.every_n_ticks()))
            .filter(|runtime| self.is_active(&runtime.meta.id, &referenced))
            .map(|runtime| DueSource {
                id: runtime.meta.id.clone(),
                meta: runtime.meta.clone(),
                origin: runtime.origin,
                call: runtime.call.clone(),
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
        let referenced = self.referenced();
        let mut views = BTreeMap::new();
        for (id, runtime) in inner.iter() {
            // An inactive source is absent from the frame, so a rule that names
            // one (a paused rule re-enabled against a source it no longer
            // reads) is told "not found" instead of judging stale data.
            if !self.is_active(id, &referenced) {
                continue;
            }
            match runtime.origin {
                // A pushed notification is not stored in the runtime: the pool
                // owns the last value and a freeze reads it straight through,
                // so the frame always holds the newest push and never a copy
                // that outlives the subscription.
                Origin::Notification => {
                    views.insert(id.clone(), self.notification_view(id, now, ttl_ms));
                }
                Origin::Disk | Origin::Tool => {
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
            }
        }
        TickFrame { views }
    }

    /// The view a notification source carries right now, or an empty, never
    /// sampled view when the pool holds nothing for it — a rule waiting on it
    /// then stays unready instead of judging an empty world.
    fn notification_view(&self, id: &str, now: i64, ttl_ms: i64) -> SourceView {
        let Some(pool) = self.connectors() else {
            return SourceView::default();
        };
        let Some((params, received_at)) = pool.notification_snapshot().get(id).cloned() else {
            return SourceView::default();
        };
        if now.saturating_sub(received_at) > ttl_ms {
            return SourceView::default();
        }
        SourceView {
            items: vec![json!({"params": params, "received_at": received_at})],
            count: 1,
            observed_at: Some(received_at),
        }
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
        let mut tasks = Vec::with_capacity(due.len());
        for source in due {
            let permits = permits.clone();
            let workspace = self.workspace.clone();
            let secrets = self.secrets.clone();
            let connectors = self.connectors();
            tasks.push(tokio::spawn(async move {
                let _permit = match permits.acquire().await {
                    Ok(permit) => permit,
                    Err(_) => return (source.id.clone(), Err("collector cancelled".to_string())),
                };
                let outcome = match source.origin {
                    Origin::Tool => collect_tool(&source, connectors.clone()).await,
                    // A notification source is never collected: the pool's
                    // buffer is its data and `freeze` reads it directly.
                    Origin::Notification => {
                        return (source.id, Ok(normalize::Normalized::default()));
                    }
                    Origin::Disk => collect_one(workspace, secrets, connectors, &source).await,
                };
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
                    origin: runtime.origin,
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

/// Collect one `type=tool` source: call the connector tool and wrap its answer
/// as a single item.
///
/// A missing pool is a failure with a readable reason rather than silence: the
/// subscription is a promise to call something, and "nothing configured the
/// connectors" is the most likely reason it was not kept.
async fn collect_tool(
    source: &DueSource,
    connectors: Option<Arc<McpService>>,
) -> Result<normalize::Normalized, String> {
    let Some(call) = source.call.as_ref() else {
        return Err("subscription declares no tool to call".to_string());
    };
    let Some(pool) = connectors else {
        return Err(format!(
            "no connector pool is attached; cannot call `{}`",
            call.callable
        ));
    };
    let answer = pool
        .call(&call.callable, call.args.clone())
        .await
        .map_err(|e| format!("{}: {e}", call.callable))?;
    let item = Value::String(answer);
    let entry = Entry {
        fingerprint: normalize::fingerprint_of(&item),
        at: meta::now_ms(),
        item,
    };
    Ok(normalize::Normalized {
        entries: vec![entry],
        warnings: Vec::new(),
    })
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
    trial_collect(
        workspace,
        secrets,
        connectors,
        &source.meta,
        &script,
        &source.existing,
    )
    .await
}

/// Run one collector script against a meta that is not necessarily on disk yet.
///
/// This is the single home of "what a collection is": a script, a meta, and the
/// environment the tick binds for them. The tick calls it through
/// [`collect_one`]; `trigger_source create` / `update` call it *before* they
/// write anything, so a source that would fail at the next tick is refused at
/// the call that defined it instead of landing on disk broken. Keeping one
/// implementation is the point: a create that passed its test must behave
/// exactly like the tick that follows it, not like a second, near-identical
/// sandbox that drifted from the first.
///
/// `existing` is the window `append` mode deduplicates against; a trial usually
/// passes an empty slice.
pub(crate) async fn trial_collect(
    workspace: PathBuf,
    secrets: Arc<BTreeMap<String, String>>,
    connectors: Option<Arc<McpService>>,
    meta: &SourceMeta,
    script: &str,
    existing: &[Entry],
) -> Result<normalize::Normalized, String> {
    let redactor = Redactor::new(secrets.values().cloned());
    let env = HostEnv::new(redactor);
    let host = Arc::new(CollectHost::new(
        workspace,
        &meta.allow_paths,
        secrets.clone(),
        connectors,
    ));
    // Collect limits, never the judge default: a trial has to be judged by the
    // same budget the tick will give it, or a slow-but-valid source is refused.
    let limits = Limits::collect().with_timeout(Duration::from_secs(meta.timeout_s.max(1)));
    let request = RunRequest::new(script.to_string(), ENTRY_COLLECT).with_limits(limits);

    let raw = execute_async(request, host, env)
        .await
        .map_err(|e: ScriptError| e.to_string())?;
    normalize::normalize(&raw, meta.mode, meta.max_items, existing, meta::now_ms())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::trigger::source::meta::{SourceMode, save_meta, save_script};
    use std::collections::HashSet;

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

    /// One MCP server that declares a single subscription.
    fn server(name: &str, subscription: McpTriggerSubscription) -> McpServerConfig {
        McpServerConfig {
            name: name.to_string(),
            triggers: vec![subscription],
            ..Default::default()
        }
    }

    fn notification_subscription(uri: &str, source_id: &str) -> McpTriggerSubscription {
        McpTriggerSubscription {
            r#type: "notification".to_string(),
            uri: uri.to_string(),
            source_id: source_id.to_string(),
            ..Default::default()
        }
    }

    fn tool_subscription(tool: &str, source_id: &str) -> McpTriggerSubscription {
        McpTriggerSubscription {
            r#type: "tool".to_string(),
            tool: tool.to_string(),
            source_id: source_id.to_string(),
            ..Default::default()
        }
    }

    #[tokio::test]
    async fn test_an_unreferenced_source_is_not_collected() {
        // A layer that was told what the rules read collects only that: an
        // orphaned collector costs nothing until a rule names it.
        let tmp = tempfile::tempdir().unwrap();
        write_source(tmp.path(), "src-0001", 1, Some(ONE_ITEM));
        write_source(tmp.path(), "src-0002", 1, Some(ONE_ITEM));
        let layer = layer(tmp.path(), DEFAULT_TTL);
        layer.set_referenced_sources(HashSet::from(["src-0001".to_string()]));

        let report = layer.tick(2).await;
        assert_eq!(report.succeeded, vec!["src-0001".to_string()]);
        assert_eq!(report.due, vec!["src-0001".to_string()]);
        assert_eq!(layer.view("src-0001").unwrap().count, 2);
        assert!(
            layer.view("src-0002").is_none(),
            "an unreferenced source is absent from the frame, not just empty"
        );
        // It is still listed: the definition exists, it just is not used.
        assert_eq!(layer.status().len(), 2);
    }

    #[tokio::test]
    async fn test_a_layer_that_was_never_told_what_is_referenced_collects_everything() {
        // Backward compatibility: every caller that builds a `SourceLayer` on
        // its own (tests, a wired-up layer before the first rule reload) sees
        // no filter at all. An empty set is *not* the same thing — that one is
        // an explicit "nothing is referenced".
        let tmp = tempfile::tempdir().unwrap();
        write_source(tmp.path(), "src-0001", 1, Some(ONE_ITEM));
        write_source(tmp.path(), "src-0002", 1, Some(ONE_ITEM));
        let layer = layer(tmp.path(), DEFAULT_TTL);

        let report = layer.tick(2).await;
        assert_eq!(
            report.succeeded,
            vec!["src-0001".to_string(), "src-0002".to_string()]
        );

        // An explicit empty set skips everything.
        let other = self::layer(tmp.path(), DEFAULT_TTL);
        other.set_referenced_sources(HashSet::new());
        let report = other.tick(3).await;
        assert!(report.succeeded.is_empty(), "{report:?}");
        assert!(report.due.is_empty());
    }

    #[tokio::test]
    async fn test_an_unreferenced_virtual_source_is_neither_called_nor_injected() {
        // A subscription is a declaration, not a demand: until a rule reads the
        // source the tick neither calls the tool nor hands the push to a
        // condition — while `list` still shows both so an agent can find them.
        use crate::mcp::service::test_support as mcp_test;

        let tmp = tempfile::tempdir().unwrap();
        let layer = layer(tmp.path(), DEFAULT_TTL);
        let (pool, peer) = mcp_test::online_service(
            "ci",
            vec![json!({"name": "list_runs", "description": "List CI runs"})],
        );
        mcp_test::wait_for(|| pool.any_online(), "the fake server online").await;
        pool.ingest_notification("ci", "notifications/builds", json!({"status": "failed"}));
        layer.attach_connectors(Some(pool));
        layer.attach_mcp_servers(&[McpServerConfig {
            name: "ci".to_string(),
            triggers: vec![
                notification_subscription("notifications/builds", "builds"),
                tool_subscription("list_runs", "ci-runs"),
            ],
            ..Default::default()
        }]);

        layer.set_referenced_sources(HashSet::new());
        let report = layer.tick(2).await;
        assert!(report.due.is_empty(), "{:?}", report.due);
        assert_eq!(peer.calls(), 0, "no rule reads ci-runs, so nobody calls it");
        assert!(layer.view("builds").is_none());

        // Both remain discoverable.
        let statuses = layer.status();
        let origins: Vec<(&str, &str)> = statuses
            .iter()
            .map(|s| (s.meta.id.as_str(), s.origin.as_str()))
            .collect();
        assert_eq!(
            origins,
            vec![("builds", "notification"), ("ci-runs", "tool")]
        );
    }

    #[tokio::test]
    async fn test_a_notification_source_without_a_pool_is_present_but_unsampled() {
        // No pool means nothing can have been pushed. The source still exists
        // in the frame — a rule naming it is not told "not found" — but it
        // carries no observation, so the ready gate keeps the rule quiet rather
        // than letting it judge an empty world.
        let tmp = tempfile::tempdir().unwrap();
        let layer = layer(tmp.path(), DEFAULT_TTL);
        layer.attach_mcp_servers(&[server(
            "ci",
            notification_subscription("notifications/builds", "builds"),
        )]);

        let frame = layer.freeze();
        let view = frame.view("builds").expect("declared, so listed");
        assert_eq!(view.count, 0);
        assert_eq!(view.observed_at, None);
        // A notification source is never "collected", so a tick leaves it
        // neither succeeded nor failed.
        let report = layer.tick(2).await;
        assert!(report.failed.is_empty(), "{:?}", report.failed);
        assert_eq!(report.succeeded, vec!["builds".to_string()]);
    }

    #[tokio::test]
    async fn test_freeze_carries_a_notification_virtual_source_with_its_payload() {
        // A notification is pushed, not collected: the frame the condition
        // reads must already hold the last push, with no tick behind it.
        use crate::mcp::service::test_support as mcp_test;

        let tmp = tempfile::tempdir().unwrap();
        let layer = layer(tmp.path(), DEFAULT_TTL);
        layer.attach_mcp_servers(&[server(
            "ci",
            notification_subscription("notifications/builds", "builds"),
        )]);

        // Before any push the view exists but has nothing observed, so a rule
        // waiting on it stays unready instead of judging an empty world.
        let cold = layer.freeze();
        let view = cold
            .view("builds")
            .expect("a declared source is always in the frame");
        assert_eq!(view.count, 0);
        assert_eq!(view.observed_at, None);

        // The pool only keeps a push some server's config declares, so the
        // subscription has to be in the pool's spec as well as in the layer's.
        let service = Arc::new(McpService::with_connector(
            vec![server(
                "ci",
                notification_subscription("notifications/builds", "builds"),
            )],
            mcp_test::FakeConnector::new(vec![]),
            mcp_test::timing(),
            Duration::from_secs(2),
        ));
        service.ingest_notification(
            "ci",
            "notifications/builds",
            json!({"status": "failed", "branch": "main"}),
        );
        layer.attach_connectors(Some(service));

        let frame = layer.freeze();
        let view = frame.view("builds").expect("the subscription is a source");
        assert_eq!(view.count, 1);
        let item = &view.items[0];
        assert_eq!(item["params"]["status"], "failed");
        assert_eq!(item["params"]["branch"], "main");
        let received_at = item["received_at"].as_i64().expect("a receive timestamp");
        assert!(received_at > 0);
        assert_eq!(view.observed_at, Some(received_at));
    }

    #[tokio::test]
    async fn test_tool_virtual_source_calls_the_connector_and_commits_its_answer() {
        // A `type=tool` subscription is an active collection: the tick calls
        // the named tool with the configured arguments and keeps whatever text
        // comes back as one item.
        use crate::mcp::service::test_support as mcp_test;

        let tmp = tempfile::tempdir().unwrap();
        let layer = layer(tmp.path(), DEFAULT_TTL);
        layer.attach_mcp_servers(&[server(
            "ci",
            McpTriggerSubscription {
                args: Some(json!({"branch": "main"})),
                ..tool_subscription("list_runs", "ci-runs")
            },
        )]);

        let (pool, peer) = mcp_test::online_service(
            "ci",
            vec![json!({"name": "list_runs", "description": "List CI runs"})],
        );
        mcp_test::wait_for(|| pool.any_online(), "the fake server online").await;
        peer.set_answer("3 runs, one failed");
        layer.attach_connectors(Some(pool));

        let report = layer.tick(4).await;
        assert_eq!(report.succeeded, vec!["ci-runs".to_string()]);
        assert!(report.failed.is_empty(), "{:?}", report.failed);
        assert_eq!(peer.calls(), 1, "the tick must call the tool once");

        let status = layer
            .status()
            .into_iter()
            .find(|s| s.meta.id == "ci-runs")
            .expect("the subscription is a source");
        assert_eq!(status.origin, Origin::Tool);
        assert_eq!(status.status, "ready");
        let view = layer.view("ci-runs").expect("view after collect");
        assert_eq!(view.count, 1);
        // The pool answers with the quoted `<codemode_output>` envelope so
        // connector text stays data; the item keeps the answer verbatim.
        let text = view.items[0].as_str().expect("the answer is one text item");
        assert!(text.contains("<codemode_output"), "{text}");
        assert!(text.contains("3 runs, one failed"), "{text}");
        assert!(view.observed_at.is_some());
    }

    #[tokio::test]
    async fn test_tool_virtual_source_fails_when_the_connector_is_offline() {
        // A dead server is one failed source, not a failed tick, and the reason
        // must name the server so `trigger_source list` explains itself.
        use crate::mcp::service::test_support as mcp_test;

        let tmp = tempfile::tempdir().unwrap();
        write_source(tmp.path(), "src-0001", 1, Some(ONE_ITEM));
        let layer = layer(tmp.path(), DEFAULT_TTL);
        layer.attach_mcp_servers(&[server("ci", tool_subscription("list_runs", "ci-runs"))]);

        let (pool, peer) = mcp_test::online_service(
            "ci",
            vec![json!({"name": "list_runs", "description": "List CI runs"})],
        );
        mcp_test::wait_for(|| pool.any_online(), "the fake server online").await;
        peer.set_online(false);
        // The supervisor notices the death on its own clock; wait for it rather
        // than assuming one probe interval has passed.
        mcp_test::wait_for(
            || {
                pool.status_of("ci")
                    .is_some_and(|status| !status.is_online())
            },
            "the fake server offline",
        )
        .await;
        layer.attach_connectors(Some(pool));

        let report = layer.tick(4).await;
        assert_eq!(report.succeeded, vec!["src-0001".to_string()]);
        assert_eq!(report.failed.len(), 1, "{:?}", report.failed);
        assert_eq!(report.failed[0].id, "ci-runs");
        assert!(
            report.failed[0].error.contains("ci"),
            "the failure must name the server: {}",
            report.failed[0].error
        );

        let status = layer
            .status()
            .into_iter()
            .find(|s| s.meta.id == "ci-runs")
            .expect("the subscription is a source");
        assert_eq!(status.status, "degraded");
        assert!(status.last_error.is_some());
        // Nothing was committed, so a condition sees an unsampled source.
        assert_eq!(layer.view("ci-runs").unwrap().observed_at, None);
    }

    #[tokio::test]
    async fn test_tool_virtual_source_without_a_pool_records_a_readable_failure() {
        let tmp = tempfile::tempdir().unwrap();
        let layer = layer(tmp.path(), DEFAULT_TTL);
        layer.attach_mcp_servers(&[server("ci", tool_subscription("list_runs", "ci-runs"))]);

        let report = layer.tick(4).await;
        assert_eq!(report.failed.len(), 1, "{:?}", report.failed);
        assert_eq!(report.failed[0].id, "ci-runs");
        assert!(
            report.failed[0].error.contains("no connector pool"),
            "{}",
            report.failed[0].error
        );
    }

    #[tokio::test]
    async fn test_tool_virtual_source_is_called_once_per_due_tick() {
        // Health tracking and cadence are the disk collector's, not a special
        // case: the tick counter decides, so a second tick collects again.
        use crate::mcp::service::test_support as mcp_test;

        let tmp = tempfile::tempdir().unwrap();
        let layer = layer(tmp.path(), DEFAULT_TTL);
        let (pool, peer) = mcp_test::online_service(
            "ci",
            vec![json!({"name": "list_runs", "description": "List CI runs"})],
        );
        mcp_test::wait_for(|| pool.any_online(), "the fake server online").await;
        layer.attach_connectors(Some(pool));
        layer.attach_mcp_servers(&[server("ci", tool_subscription("list_runs", "ci-runs"))]);

        assert_eq!(layer.due(1), vec!["ci-runs".to_string()]);
        layer.tick(4).await;
        layer.tick(4).await;
        assert_eq!(peer.calls(), 2, "one call per due tick");
    }

    #[tokio::test]
    async fn test_reload_synthesizes_a_virtual_source_per_mcp_subscription() {
        // A subscription in the MCP config is a source declaration: the tick
        // must see it without a directory on disk, and it must survive the
        // reload that prunes everything not found under the sources root.
        let tmp = tempfile::tempdir().unwrap();
        let layer = layer(tmp.path(), DEFAULT_TTL);
        layer.attach_mcp_servers(&[
            McpServerConfig {
                name: "ci".to_string(),
                triggers: vec![
                    notification_subscription("notifications/builds", "builds"),
                    tool_subscription("list_runs", "ci-runs"),
                ],
                ..Default::default()
            },
            server(
                "mail",
                notification_subscription("notifications/mail", "inbox"),
            ),
        ]);

        let statuses = layer.status();
        let origins: Vec<(&str, &str)> = statuses
            .iter()
            .map(|s| (s.meta.id.as_str(), s.origin.as_str()))
            .collect();
        assert_eq!(
            origins,
            vec![
                ("builds", "notification"),
                ("ci-runs", "tool"),
                ("inbox", "notification"),
            ]
        );
        // Nothing was written to disk to make them appear.
        assert!(
            !tmp.path()
                .join("triggers")
                .join("sources")
                .join("builds")
                .exists()
        );

        // The config is the whole truth: dropping a subscription drops the
        // virtual source, keeping the others intact.
        layer.attach_mcp_servers(&[server(
            "ci",
            notification_subscription("notifications/builds", "builds"),
        )]);
        let ids: Vec<String> = layer.status().iter().map(|s| s.meta.id.clone()).collect();
        assert_eq!(ids, vec!["builds".to_string()]);
    }

    #[tokio::test]
    async fn test_notification_virtual_source_survives_reload() {
        // The reload prunes everything the directory scan did not find; a
        // virtual source has no directory, so keeping it is the whole point of
        // tracking its origin.
        let tmp = tempfile::tempdir().unwrap();
        let layer = layer(tmp.path(), DEFAULT_TTL);
        layer.attach_mcp_servers(&[server(
            "ci",
            notification_subscription("notifications/builds", "builds"),
        )]);

        layer.reload();
        layer.reload();

        let ids: Vec<String> = layer.status().iter().map(|s| s.meta.id.clone()).collect();
        assert_eq!(ids, vec!["builds".to_string()]);
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
