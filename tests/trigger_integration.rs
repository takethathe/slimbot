//! End-to-end tests for the trigger layer.
//!
//! Everything here runs the real thing: real directories under a temp
//! workspace, the real QuickJS entries (`collect`, `judge`), the real
//! dispatcher, the real subagent runtime and the real message bus. Only three
//! edges are doubled — the provider (a canned reply), the MCP server (a child
//! process of this test binary) and the keychain (a map) — because the tests
//! must never reach the network. See `docs/trigger-test-plan.md`.
//!
//! Determinism rules:
//! - the tick advances by calling `tick()`, never by waiting for a clock;
//! - the consumer advances by `drain()`, never by sleeping;
//! - the only waiting helper is `wait_until`, and it always has an upper bound.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};

use serde_json::{Value, json};

use slimbot::agent::{Origin, SubagentRuntime, ToolCatalog, default_tool_set};
use slimbot::trigger::dispatch::{Dispatcher, read_history};
use slimbot::trigger::source::SourceLayer;
use slimbot::trigger::source::TickReport;
use slimbot::trigger::source::meta as source_meta;
use slimbot::trigger::subscription::meta as rule_meta;
use slimbot::trigger::subscription::meta::{
    ActionKind, TriggerAction, TriggerMeta, load_all as load_rules,
};
use slimbot::trigger::subscription::state::TriggerState;
use slimbot::trigger::tool::{DEFAULT_MAX_ACTIVE_SOURCES, TriggerSourceTool};
use slimbot::trigger::{TickOutcome, TriggerService, assemble, build_layers, history_path};
use slimbot::{
    AgentConfig, CronSchedule, FinishReason, LLMResponse, McpServerConfig, McpService,
    MemoryService, Message, Provider, SecretEntry, SessionManager, SharedMemory, SpanContext,
    SpanRecorder, SpanStatus, Tool, ToolDefinition, TraceBackend, Tracer, TriggersConfig, Usage,
};
use slimbot::{BusRequest, MessageBus, MessageBusReceivers};

/// Where `build_layers` looks for sources and rules. Spelling it here keeps the
/// tests honest about the on-disk contract.
fn source_root(workspace: &Path) -> PathBuf {
    workspace.join("triggers").join("sources")
}

fn rule_root(workspace: &Path) -> PathBuf {
    workspace.join("triggers").join("triggers")
}

// ── Scripts the model would have written ──

/// A local channel: every non-empty line of a file becomes one item.
const COLLECT_FEED: &str = r#"
function collect(ctx) {
  const text = ctx.fs.read('feed.txt');
  return {items: text.split('\n').filter(l => l.trim()).map(l => ({title: l.trim()}))};
}
"#;

const JUDGE_DEPLOY: &str = r#"
function judge(ctx) {
  const view = ctx.sources['src-0001'] || {items: []};
  const hit = view.items.filter(i => String(i.title).indexOf('deploy') >= 0);
  return {
    fire: hit.length > 0,
    detail: hit.length + ' deploy line(s)',
    evidence: hit.map(i => ({source_id: 'src-0001', text: i.title})),
  };
}
"#;

/// Fires on any item, so a boundary test is about storage rather than matching.
const JUDGE_ANY_ITEM: &str = r#"
function judge(ctx) {
  const view = ctx.sources['src-0001'] || {items: []};
  return {fire: view.items.length > 0, detail: view.items.length + ' item(s)',
          evidence: view.items.map(i => ({source_id: 'src-0001', text: i.title}))};
}
"#;

/// One boolean derived from a file, so a test can turn the world on and off
/// without touching the script.
const COLLECT_FLAG: &str = r#"
function collect(ctx) {
  const on = ctx.fs.read('flag.txt').trim() === 'on';
  return {items: [{on: on}]};
}
"#;

const JUDGE_FLAG: &str = r#"
function judge(ctx) {
  const view = ctx.sources['src-0001'] || {items: []};
  const on = view.items.some(i => i.on === true);
  return {fire: on, detail: on ? 'flag is on' : 'flag is off',
          evidence: on ? [{source_id: 'src-0001', text: 'flag.txt reads on'}] : []};
}
"#;

const ALWAYS_TRUE: &str = "function judge() { return {fire: true, detail: 'fired'}; }";

// ── Span capture ──

/// A backend that keeps every span, so a test can assert on the trace the
/// contract promises (`trigger.tick`, `trigger.fire`, `subagent.run`).
#[derive(Default)]
struct Spans {
    open: Mutex<BTreeMap<String, String>>,
    closed: Mutex<Vec<Recorded>>,
}

struct Recorded {
    name: String,
    status: SpanStatus,
    attributes: BTreeMap<String, Value>,
}

impl TraceBackend for Spans {
    fn on_span_start(&self, ctx: &SpanContext, name: &str, _start: SystemTime) {
        self.open
            .lock()
            .unwrap()
            .insert(ctx.span_id.clone(), name.to_string());
    }

    fn on_span_end(
        &self,
        ctx: &SpanContext,
        _end: SystemTime,
        status: &SpanStatus,
        attributes: &BTreeMap<String, Value>,
    ) {
        let name = self
            .open
            .lock()
            .unwrap()
            .remove(&ctx.span_id)
            .unwrap_or_default();
        self.closed.lock().unwrap().push(Recorded {
            name,
            status: status.clone(),
            attributes: attributes.clone(),
        });
    }
}

impl Spans {
    fn named(&self, name: &str) -> Vec<(SpanStatus, BTreeMap<String, Value>)> {
        self.closed
            .lock()
            .unwrap()
            .iter()
            .filter(|r| r.name == name)
            .map(|r| (r.status.clone(), r.attributes.clone()))
            .collect()
    }

    fn names(&self) -> Vec<String> {
        self.closed
            .lock()
            .unwrap()
            .iter()
            .map(|r| r.name.clone())
            .collect()
    }

    fn one(&self, name: &str) -> (SpanStatus, BTreeMap<String, Value>) {
        let mut found = self.named(name);
        assert_eq!(
            found.len(),
            1,
            "expected exactly one {name} span: {:?}",
            self.names()
        );
        found.remove(0)
    }
}

// ── The harness ──

/// One process worth of trigger wiring over a temp workspace.
///
/// The temp dir sits behind an `Arc` so a test can rebuild the wiring over the
/// same workspace and act out a restart.
struct Harness {
    _tmp: Arc<tempfile::TempDir>,
    /// Only `with_mcp` fills this in: the seam tests read server status.
    pool: std::cell::RefCell<Option<Arc<McpService>>>,
    workspace: PathBuf,
    config: TriggersConfig,
    sources: Arc<SourceLayer>,
    rules: Arc<slimbot::trigger::subscription::SubscriptionLayer>,
    /// What the last `with_mcp` harness was wired with, for the restart test.
    servers: Vec<McpServerConfig>,
    service: Arc<TriggerService>,
    dispatch: Arc<Dispatcher>,
    consumers: MessageBusReceivers,
    spans: Arc<Spans>,
}

impl Harness {
    fn new() -> Self {
        Self::with(TriggersConfig {
            enabled: true,
            ..Default::default()
        })
    }

    fn with(config: TriggersConfig) -> Self {
        let tmp = Arc::new(tempfile::tempdir().unwrap());
        Self::over(tmp, config, None, None)
    }

    /// A harness whose dispatcher carries a real subagent runtime, so an
    /// `agent` action runs to completion over a canned provider.
    fn with_subagent(config: TriggersConfig) -> Self {
        let tmp = Arc::new(tempfile::tempdir().unwrap());
        Self::over(tmp, config, None, Some(Arc::new(CannedProvider)))
    }

    /// A harness whose collectors can read connectors.
    fn with_connectors(config: TriggersConfig, pool: Arc<McpService>) -> Self {
        let tmp = Arc::new(tempfile::tempdir().unwrap());
        Self::over(tmp, config, Some(pool), None)
    }

    /// A harness wired against an MCP section that declares virtual sources.
    fn with_mcp(
        config: TriggersConfig,
        pool: Arc<McpService>,
        servers: Vec<McpServerConfig>,
    ) -> Self {
        let h = {
            let tmp = Arc::new(tempfile::tempdir().unwrap());
            Self::over_with(tmp, config, Some(pool.clone()), None, servers)
        };
        *h.pool.borrow_mut() = Some(pool);
        h
    }

    /// Same workspace, brand new wiring: what a process restart looks like to
    /// this layer. Nothing but the files on disk carries over.
    fn restart(&self) -> Self {
        Self::over(self._tmp.clone(), self.config.clone(), None, None)
    }

    fn over(
        tmp: Arc<tempfile::TempDir>,
        config: TriggersConfig,
        connectors: Option<Arc<McpService>>,
        subagent: Option<Arc<dyn Provider>>,
    ) -> Self {
        Self::over_with(tmp, config, connectors, subagent, Vec::new())
    }

    fn over_with(
        tmp: Arc<tempfile::TempDir>,
        config: TriggersConfig,
        connectors: Option<Arc<McpService>>,
        subagent: Option<Arc<dyn Provider>>,
        servers: Vec<McpServerConfig>,
    ) -> Self {
        let workspace = tmp.path().join("workspace");
        std::fs::create_dir_all(source_root(&workspace)).unwrap();
        std::fs::create_dir_all(rule_root(&workspace)).unwrap();
        std::fs::create_dir_all(workspace.join("agents")).unwrap();

        let (bus, consumers) = MessageBus::new();
        let spans = Arc::new(Spans::default());
        let tracer = Tracer::new(vec![spans.clone() as Arc<dyn TraceBackend>]);
        let layers = build_layers(&config, &workspace);
        let sources = layers.sources.clone();
        let rules = layers.rules.clone();
        // The pool comes up with the loop, so the layer is handed it after it is
        // built — the same order the gateway uses.
        if let Some(pool) = connectors {
            sources.attach_connectors(Some(pool));
        }
        let runtime = subagent.map(|provider| {
            let workspace = workspace.clone();
            std::fs::create_dir_all(workspace.join("agents")).unwrap();
            std::fs::write(
                workspace.join("agents").join("echo.md"),
                "---\nname: echo\ndescription: test agent\ndeliver: true\ntimeout_secs: 30\n---\n\nYou are a test agent.\n",
            )
            .unwrap();
            let session_manager = Arc::new(tokio::sync::Mutex::new(
                SessionManager::new(tmp.path().join("sessions")).unwrap(),
            ));
            let memory: SharedMemory =
                Arc::new(tokio::sync::Mutex::new(MemoryService::new(&workspace)));
            let mut catalog = ToolCatalog::new();
            catalog.register_builtin("file_reader", &workspace);
            Arc::new(SubagentRuntime::new(
                Arc::new(catalog),
                workspace,
                provider,
                session_manager,
                memory,
                AgentConfig::default(),
                default_tool_set(&[]),
                512,
                2,
                Some(Arc::new(tracer.clone())),
                bus.clone(),
                Default::default(),
                None,
            ))
        });
        let wiring = assemble(layers, &config, &workspace, tracer, runtime, bus, &servers);
        Self {
            pool: std::cell::RefCell::new(None),
            workspace,
            config,
            sources,
            rules,
            servers,
            service: wiring.service,
            dispatch: wiring.dispatch,
            consumers,
            spans,
            _tmp: tmp,
        }
    }

    fn workspace(&self) -> &Path {
        &self.workspace
    }

    fn borrow_pool(&self) -> Arc<McpService> {
        self.pool
            .borrow()
            .clone()
            .expect("only `with_mcp` sets a pool")
    }

    fn pool_online(&self) -> bool {
        self.pool
            .borrow()
            .as_ref()
            .is_some_and(|pool| pool.any_online())
    }

    /// The process directory: workspace, session store and anything else the
    /// wiring puts on disk.
    fn tmp_root(&self) -> &Path {
        self._tmp.path()
    }

    fn write_file(&self, name: &str, content: &str) {
        std::fs::write(self.workspace.join(name), content).unwrap();
    }

    fn source(&self, id: &str, script: &str) {
        let root = source_root(&self.workspace);
        let mut meta = source_meta::SourceMeta::new(id, id);
        meta.timeout_s = 5;
        source_meta::save_meta(&root, &meta).unwrap();
        source_meta::save_script(&root, id, script).unwrap();
    }

    fn rule(&self, meta: &TriggerMeta, script: Option<&str>) {
        let root = rule_root(&self.workspace);
        rule_meta::save_meta(&root, meta).unwrap();
        if let Some(script) = script {
            rule_meta::save_script(&root, &meta.id, script).unwrap();
        }
    }

    async fn tick(&self) -> TickOutcome {
        self.service.tick().await
    }

    /// A round with a sampled source behind it: the judge reads the frame
    /// frozen before collection, so the first round can only sample.
    async fn warm(&self) -> TickOutcome {
        let cold = self.tick().await;
        assert!(
            cold.fired.is_empty(),
            "a cold round must not fire: {cold:?}"
        );
        self.tick().await
    }

    async fn drain(&self) -> usize {
        self.dispatch.drain().await
    }

    fn rows(&self) -> Vec<Value> {
        read_history(&history_path(&self.workspace)).unwrap_or_default()
    }

    fn state(&self, id: &str) -> TriggerState {
        TriggerState::load(&rule_root(&self.workspace).join(id)).0
    }

    fn rules(&self) -> Vec<TriggerMeta> {
        load_rules(&rule_root(&self.workspace))
            .into_iter()
            .map(|r| r.meta)
            .collect()
    }

    async fn inbound(&mut self) -> Option<BusRequest> {
        tokio::time::timeout(Duration::from_secs(5), self.consumers.inbound.recv())
            .await
            .ok()
            .flatten()
    }
}

fn notify(template: &str) -> TriggerAction {
    TriggerAction {
        kind: ActionKind::Notify,
        text_template: Some(template.to_string()),
        ..Default::default()
    }
}

fn rule_with(id: &str, sources: &[&str], action: TriggerAction) -> TriggerMeta {
    let mut meta = TriggerMeta::new(id, id);
    meta.sources = sources.iter().map(|s| s.to_string()).collect();
    meta.origin = Some(Origin::new("webui", "chat-1"));
    meta.action = action;
    meta
}

/// Every file under the workspace, concatenated — used to prove a boundary
/// ("this text never lands on disk") rather than a shape.
fn all_text_on_disk(root: &Path) -> String {
    let mut out = String::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else if let Ok(text) = std::fs::read_to_string(&path) {
                out.push_str(&text);
                out.push('\n');
            }
        }
    }
    out
}

// ── Source → condition → notice ──

#[tokio::test]
async fn test_a_collect_script_reading_a_real_channel_ends_in_a_notice_at_the_origin() {
    let mut h = Harness::new();
    h.write_file("feed.txt", "quiet morning\ndeploy failed on prod\n");
    h.source("src-0001", COLLECT_FEED);
    h.rule(
        &rule_with("trg-a", &["src-0001"], notify("{{detail}} | {{evidence}}")),
        Some(JUDGE_DEPLOY),
    );

    let warm = h.warm().await;
    assert_eq!(warm.fired.len(), 1, "{warm:?}");
    assert_eq!(warm.queued, 1, "the fire must reach the queue");
    assert_eq!(h.drain().await, 1);

    let inbound = h.inbound().await.expect("one notice");
    assert_eq!(inbound.session_id, "webui:chat-1");
    assert!(
        inbound.content.starts_with("[Trigger notice]"),
        "{}",
        inbound.content
    );
    assert!(
        inbound.content.contains("1 deploy line(s)"),
        "{}",
        inbound.content
    );
    assert!(
        inbound.content.contains("- [src-0001 @"),
        "evidence carries its source and time: {}",
        inbound.content
    );
    assert!(
        inbound.content.contains("deploy failed on prod"),
        "{}",
        inbound.content
    );
    assert!(
        inbound.content.contains("not a message from the user"),
        "the main loop must not read a notice as the user talking: {}",
        inbound.content
    );

    let rows = h.rows();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0]["action_kind"], json!("notify"));
    assert_eq!(rows[0]["ok"], json!(true));
    assert_eq!(rows[0]["trigger_id"], json!("trg-a"));

    // The source really produced the items, in order, inside the snapshot.
    let view = h.sources.view("src-0001").expect("snapshot");
    assert_eq!(view.count, 2);
    assert_eq!(view.items[1]["title"], json!("deploy failed on prod"));
}

#[tokio::test]
async fn test_a_rising_edge_is_announced_once_and_rearms_after_it_falls() {
    let mut h = Harness::new();
    h.write_file("flag.txt", "on");
    h.source("src-0001", COLLECT_FLAG);
    h.rule(
        &rule_with("trg-a", &["src-0001"], notify("{{detail}}")),
        Some(JUDGE_FLAG),
    );

    assert!(h.tick().await.fired.is_empty(), "sampling round");

    // Turning the world on: the judge reads the frame frozen before this
    // round's collection, so the rise lands one round after the file changed.
    let rise = h.tick().await;
    assert_eq!(rise.fired.len(), 1, "a new rise must fire: {rise:?}");

    // Same world again: `true -> true` is not a rise.
    let steady = h.tick().await;
    assert!(
        steady.fired.is_empty(),
        "a steady condition must not repeat"
    );

    // Falling edge, then a fresh rise.
    h.write_file("flag.txt", "off");
    assert!(h.tick().await.fired.is_empty(), "judges the old frame");
    assert!(h.tick().await.fired.is_empty(), "the edge falls");
    h.write_file("flag.txt", "on");
    assert!(h.tick().await.fired.is_empty(), "judges the off frame");
    let second = h.tick().await;
    assert_eq!(second.fired.len(), 1, "a fresh rise after a fall must fire");

    assert_eq!(h.drain().await, 2, "two notices, one per rise");
    assert!(h.inbound().await.is_some());
    assert!(h.inbound().await.is_some());
    assert_eq!(h.rows().len(), 2);

    // The trace says what happened, one fire span per rise, and each names the
    // action it carried.
    let fires = h.spans.named("trigger.fire");
    assert_eq!(fires.len(), 2, "{:?}", h.spans.names());
    for (status, attributes) in fires {
        assert!(matches!(status, SpanStatus::Ok), "{status:?}");
        assert_eq!(attributes["trigger_id"], json!("trg-a"));
        assert_eq!(attributes["action_kind"], json!("notify"));
        assert_eq!(attributes["ok"], json!(true));
    }
    assert!(
        h.spans.named("trigger.tick").len() >= 2,
        "every working round is traced: {:?}",
        h.spans.names()
    );
}

// ── Restart, durability ──

#[tokio::test]
async fn test_a_restart_resumes_the_state_and_never_back_fires() {
    let h = Harness::new();
    h.rule(
        &rule_with("trg-a", &[], notify("{{detail}}")),
        Some(ALWAYS_TRUE),
    );
    assert_eq!(h.tick().await.fired.len(), 1);

    // A brand new wiring over the same workspace: only files carry over.
    let restarted = h.restart();
    assert!(
        restarted.tick().await.fired.is_empty(),
        "a restart must not fire again on state it already acted on"
    );
    let state = restarted.state("trg-a");
    assert!(state.prev, "the edge survives the restart");
    assert_eq!(state.fire_count, 1);

    // A one-shot whose moment passed while the process was down is retired on
    // reload, not back-fired.
    let mut once = TriggerMeta::new("trg-once", "once");
    once.schedule = Some(CronSchedule::at(source_meta::now_ms() - 60_000));
    restarted.rule(&once, None);
    assert!(restarted.tick().await.fired.is_empty());
    let once = restarted
        .rules()
        .into_iter()
        .find(|m| m.id == "trg-once")
        .expect("the definition stays on disk");
    assert!(!once.enabled, "a missed one-shot is disabled, not fired");
}

// ── Queue bounds ──

#[tokio::test]
async fn test_a_full_queue_drops_the_newest_fire_and_books_it_failed() {
    let mut h = Harness::with(TriggersConfig {
        enabled: true,
        max_queue: 1,
        ..Default::default()
    });
    // Two destinations: a merge would hide the queue's own bound, and this test
    // is about the queue.
    let mut first = rule_with("trg-1", &[], notify("first"));
    first.origin = Some(Origin::new("webui", "chat-1"));
    let mut second = rule_with("trg-2", &[], notify("second"));
    second.origin = Some(Origin::new("webui", "chat-2"));
    h.rule(&first, Some(ALWAYS_TRUE));
    h.rule(&second, Some(ALWAYS_TRUE));

    let tick = h.tick().await;
    assert_eq!(tick.fired.len(), 2);
    assert_eq!(tick.queued, 1, "one slot, one queue entry");
    assert_eq!(h.dispatch.dropped_count(), 1);

    // The dropped fire is already in the ledger, marked as not delivered: a
    // late reminder is worse than none, so it is never queued for later.
    let rows = h.rows();
    assert_eq!(rows.len(), 1);
    assert_eq!(
        rows[0]["trigger_id"],
        json!("trg-2"),
        "the newest fire is the one dropped"
    );
    assert_eq!(rows[0]["ok"], json!(false));
    assert_eq!(h.drain().await, 1, "the accepted fire still runs");
    let inbound = h.inbound().await.expect("the first fire is delivered");
    assert!(
        inbound.content.contains("first") && !inbound.content.contains("second"),
        "{}",
        inbound.content
    );
}

// ── External content stays off the disk ──

#[tokio::test]
async fn test_collected_content_never_lands_on_the_disk() {
    let mut h = Harness::new();
    // The marker is assembled inside the script, so the literal never exists on
    // disk by construction: if the test finds it, the layer persisted collected
    // content somewhere it promised not to.
    h.write_file("feed.txt", "one line\n");
    h.source(
        "src-0001",
        r#"
function collect(ctx) {
  const text = ctx.fs.read('feed.txt');
  const marker = 'MARKER' + '-only-in-memory';
  return {items: text.split('\n').filter(l => l.trim()).map(() => ({title: marker}))};
}
"#,
    );
    h.rule(
        &rule_with("trg-a", &["src-0001"], notify("{{detail}}")),
        Some(JUDGE_ANY_ITEM),
    );

    h.warm().await;
    h.drain().await;
    assert!(h.inbound().await.is_some());

    assert!(
        h.sources.view("src-0001").is_some_and(|v| v.count == 1),
        "the item is in the in-memory snapshot"
    );
    // The whole process directory: a session file or a log landing outside the
    // workspace would be just as much of a leak as one inside it.
    let on_disk = all_text_on_disk(h.tmp_root());
    assert!(
        !on_disk.contains("MARKER-only-in-memory"),
        "external content must have no path on disk (ADR 0009)"
    );
}

// ── Secrets ──

#[tokio::test]
async fn test_a_notice_and_its_evidence_never_carry_a_secret() {
    let mut h = Harness::with(TriggersConfig {
        enabled: true,
        secrets: vec![SecretEntry {
            name: "token".to_string(),
            value: "s3cr3t-value-42".to_string(),
            ..Default::default()
        }],
        ..Default::default()
    });
    h.write_file("feed.txt", "nothing\n");
    h.source(
        "src-0001",
        r#"
function collect(ctx) {
  const token = ctx.secret('token');
  ctx.log('using ' + token);
  return {items: [{title: token}]};
}
"#,
    );
    h.rule(
        &rule_with("trg-a", &["src-0001"], notify("{{evidence}} | {{detail}}")),
        Some(JUDGE_ANY_ITEM),
    );

    h.warm().await;
    h.drain().await;
    let inbound = h.inbound().await.expect("one notice");

    assert!(
        !inbound.content.contains("s3cr3t-value-42"),
        "the delivered text must not carry the value: {}",
        inbound.content
    );
    assert!(
        inbound.content.contains("token was ***") || inbound.content.contains("***"),
        "the value reached the evidence and was masked there: {}",
        inbound.content
    );
    let ledger = std::fs::read_to_string(history_path(h.workspace())).unwrap();
    assert!(
        !ledger.contains("s3cr3t-value-42"),
        "nor the ledger: {ledger}"
    );
    assert_eq!(h.rows().len(), 1);
}

// ── Doubles ──

/// The provider an `agent` fire runs against: one final answer, so the subagent
/// run settles in a single iteration.
struct CannedProvider;

#[async_trait::async_trait]
impl Provider for CannedProvider {
    async fn chat(
        &self,
        messages: &[&Message],
        _tools: Option<&[ToolDefinition]>,
        _recorder: Option<&dyn SpanRecorder>,
    ) -> slimbot::ProviderResult<LLMResponse> {
        // The rendered task is what the subagent was started with; if the
        // template stops reaching it, the run has nothing to act on.
        let last = match messages.last() {
            Some(Message::User { content, .. }) => content.as_text(),
            Some(Message::System { content, .. }) => content.as_str(),
            _ => "",
        };
        assert!(
            last.contains("Look at the feed: fired"),
            "the task template must reach the run: {last:?}"
        );
        Ok(LLMResponse {
            content: Some("the feed says: deploy failed on prod".to_string()),
            tool_calls: None,
            finish_reason: FinishReason::Stop,
            usage: Usage::default(),
        })
    }
}

/// A collector that spends 120ms of CPU, so a round's wall time says how many
/// of them ran at once. `ctx.now()` is the only clock a script has.
const SLOW_COLLECT: &str = r#"
function collect(ctx) {
  const t0 = ctx.now().epoch_ms;
  while (ctx.now().epoch_ms - t0 < 120) {}
  return {items: [{n: 1}]};
}
"#;

/// Same, long enough to outlast a one-second tick budget.
const SPIN_COLLECT: &str = r#"
function collect(ctx) {
  const t0 = ctx.now().epoch_ms;
  while (ctx.now().epoch_ms - t0 < 1500) {}
  return {items: [{n: 1}]};
}
"#;

/// Probes the collect entry's capability surface from inside the script: what it
/// can see, what it can call, and what is structurally absent.
const CONNECTOR_PROBE: &str = r#"
async function collect(ctx) {
  // `searchTools` is an async host binding in every entry: await it.
  const found = JSON.parse(await ctx.searchTools('feed'));
  const read = await ctx.tools.mcp__feed__read_feed({q: 'all'});
  return {items: [{
    searchable: found.map(t => t.name),
    has_write: typeof ctx.tools.mcp__feed__post_note,
    read: String(read),
    primitives: typeof ctx.fs.read === 'function' && typeof ctx.http.get === 'function',
  }]};
}
"#;

// ── An agent fire runs a subagent and reports back ──

#[tokio::test]
async fn test_an_agent_fire_runs_a_subagent_and_reports_back_to_the_origin() {
    let mut h = Harness::with_subagent(TriggersConfig {
        enabled: true,
        ..Default::default()
    });
    h.rule(
        &rule_with(
            "trg-a",
            &[],
            TriggerAction {
                kind: ActionKind::Agent,
                agent: Some("echo".to_string()),
                task_template: Some("Look at the feed: {{detail}}".to_string()),
                ..Default::default()
            },
        ),
        Some(ALWAYS_TRUE),
    );

    let tick = h.tick().await;
    assert_eq!(tick.fired.len(), 1);
    assert_eq!(
        h.drain().await,
        1,
        "the consumer runs the agent, not the tick"
    );

    let inbound = h.inbound().await.expect("the result comes back to origin");
    assert_eq!(inbound.session_id, "webui:chat-1");
    assert!(
        inbound.content.starts_with("[Subagent result]"),
        "{}",
        inbound.content
    );
    assert!(
        inbound.content.contains("agent=echo"),
        "{}",
        inbound.content
    );
    assert!(inbound.content.contains("ok=true"), "{}", inbound.content);
    assert!(
        inbound.content.contains("deploy failed on prod"),
        "the run's own words reach the origin: {}",
        inbound.content
    );

    // Two spans, related by run id, neither parented to the other: the tick is
    // long finished by the time the run settles.
    let (_, fire) = h.spans.one("trigger.fire");
    assert_eq!(fire["action_kind"], json!("agent"));
    assert_eq!(fire["agent"], json!("echo"));
    let run_id = fire["run_id"].as_str().expect("a fire span names its run");
    let (run_status, run) = h.spans.one("subagent.run");
    assert!(matches!(run_status, SpanStatus::Ok), "{run_status:?}");
    assert_eq!(run["run_id"], json!(run_id));
    assert_eq!(run["agent"], json!("echo"));

    let rows = h.rows();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0]["action_kind"], json!("agent"));
    assert_eq!(rows[0]["ok"], json!(true));
}

// ── The tick budget and the concurrency cap ──

#[tokio::test]
async fn test_a_slow_collect_set_never_exceeds_the_configured_concurrency() {
    let h = Harness::with(TriggersConfig {
        enabled: true,
        max_concurrent_collect: 1,
        ..Default::default()
    });
    let ids: Vec<String> = (1..=4).map(|n| format!("src-000{n}")).collect();
    for id in &ids {
        h.source(id, SLOW_COLLECT);
    }
    // Unreferenced sources are not collected any more, so the rule that reads
    // them is what puts them in the round.
    let sources: Vec<&str> = ids.iter().map(String::as_str).collect();
    h.rule(
        &rule_with("trg-a", &sources, notify("{{detail}}")),
        Some("function judge() { return {fire: false}; }"),
    );

    let started = std::time::Instant::now();
    let outcome = h.tick().await;
    let elapsed = started.elapsed();

    assert_eq!(
        outcome.sources.succeeded.len(),
        4,
        "{:?}",
        outcome.sources.failed
    );
    // A lower bound cannot flake on a slow machine: it only makes the round
    // longer. Four 120ms collectors on one permit must take at least ~360ms.
    assert!(
        elapsed >= Duration::from_millis(300),
        "the cap must serialize the collectors, took {elapsed:?}"
    );
    let (status, attributes) = h.spans.one("trigger.tick");
    assert!(matches!(status, SpanStatus::Ok), "{status:?}");
    assert_eq!(attributes["max_concurrent"], json!(1));
    assert_eq!(attributes["sources_due"], json!(4));
}

#[tokio::test]
async fn test_a_tick_that_exceeds_its_budget_books_nothing_and_says_so() {
    let h = Harness::with(TriggersConfig {
        enabled: true,
        tick_timeout_s: 1,
        ..Default::default()
    });
    h.source("src-0001", SPIN_COLLECT);
    // The rule names the source, so the spinning collector is scheduled and the
    // deadline has something to cut.
    h.rule(
        &rule_with("trg-a", &["src-0001"], notify("{{detail}}")),
        Some(ALWAYS_TRUE),
    );

    let started = std::time::Instant::now();
    let outcome = h.tick().await;
    let elapsed = started.elapsed();

    assert!(
        elapsed < Duration::from_millis(1400),
        "the deadline must cut the round, took {elapsed:?}"
    );
    assert!(outcome.fired.is_empty(), "a cut round fires nothing");
    assert_eq!(outcome.queued, 0);
    assert_eq!(outcome.judged, 0);
    assert!(
        !history_path(h.workspace()).exists(),
        "a cut round books nothing"
    );

    let (status, _) = h.spans.one("trigger.tick");
    assert!(
        matches!(status, SpanStatus::Error(_)),
        "the trace says the round was cut: {status:?}"
    );
}

// ── The collect entry's connector half is read-only ──

const CHILD_ENV: &str = "SLIMBOT_TRIGGER_MCP_CHILD";

/// The child half of the stdio transport: this binary re-invoked. A normal
/// `cargo test` run returns immediately.
#[test]
fn stdio_child_server() {
    if std::env::var(CHILD_ENV).is_err() {
        return;
    }
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    runtime.block_on(async {
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
        let mut lines = BufReader::new(tokio::io::stdin()).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            let Ok(frame) = serde_json::from_str::<Value>(line.trim()) else {
                continue;
            };
            let Some(id) = frame.get("id").cloned() else {
                continue; // Notification: nothing to answer.
            };
            let method = frame.get("method").and_then(Value::as_str).unwrap_or("");
            let result = match method {
                "initialize" => json!({
                    "protocolVersion": "2025-03-26",
                    "capabilities": {"tools": {}},
                    "serverInfo": {"name": "fake-feed", "version": "0"},
                }),
                "tools/list" => json!({"tools": [
                    {"name": "read_feed",
                     "description": "Read the feed",
                     "annotations": {"readOnlyHint": true, "destructiveHint": false}},
                    {"name": "post_note", "description": "Post a note"}
                ]}),
                "tools/call" => {
                    json!({"content": [{"type": "text", "text": "connector says hi"}]})
                }
                _ => json!({}),
            };
            let reply = json!({"jsonrpc": "2.0", "id": id, "result": result});
            let mut out = tokio::io::stdout();
            let _ = out.write_all(format!("\n{reply}\n").as_bytes()).await;
            let _ = out.flush().await;
        }
    });
}

/// A connector pool with one fake server: one tool that claims read-only, one
/// that claims nothing.
fn fake_pool() -> Arc<McpService> {
    let exe = std::env::current_exe()
        .unwrap()
        .to_string_lossy()
        .to_string();
    let file = tempfile::NamedTempFile::new().unwrap();
    let config = json!({
        "agent": {"provider": "default"},
        "providers": {"default": {"api_key": "sk-test", "model": "gpt-4o"}},
        "tools": [],
        "channels": [],
        "mcp": {
            "servers": [{
                "name": "feed",
                "transport": "stdio",
                "command": exe,
                "args": ["--exact", "stdio_child_server", "--nocapture", "--test-threads=1"],
                "env": {CHILD_ENV: "1"},
            }],
            "connect_timeout_seconds": 30,
            "call_timeout_seconds": 10,
            "backoff_initial_ms": 200,
        }
    });
    std::fs::write(file.path(), serde_json::to_string(&config).unwrap()).unwrap();
    let loaded = slimbot::Config::load_resolved_with(
        file.path().to_str().unwrap(),
        &slimbot::secrets::SecretStore::new(vec![]),
    )
    .unwrap();
    let pool = Arc::new(McpService::new(&loaded.mcp));
    pool.start();
    pool
}

async fn wait_until(probe: impl Fn() -> bool, what: &str) {
    for _ in 0..300 {
        if probe() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    panic!("timed out waiting for {what}");
}

#[tokio::test]
async fn test_the_collect_entry_sees_only_the_read_only_connector_half() {
    let pool = fake_pool();
    wait_until(|| pool.any_online(), "the fake feed server").await;

    let h = Harness::with_connectors(
        TriggersConfig {
            enabled: true,
            ..Default::default()
        },
        pool,
    );
    h.source("src-0001", CONNECTOR_PROBE);
    h.rule(
        &rule_with("trg-a", &["src-0001"], notify("{{detail}}")),
        Some("function judge() { return {fire: false}; }"),
    );

    let outcome = h.tick().await;
    assert_eq!(
        outcome.sources.succeeded.len(),
        1,
        "{:?}",
        outcome.sources.failed
    );
    let view = h.sources.view("src-0001").expect("snapshot");
    let item = &view.items[0];

    assert_eq!(
        item["searchable"],
        json!(["mcp__feed__read_feed"]),
        "only the read half is discoverable"
    );
    assert_eq!(
        item["has_write"],
        json!("undefined"),
        "the write half is structurally absent, not merely refused"
    );
    assert!(
        item["read"]
            .as_str()
            .unwrap_or_default()
            .contains("connector says hi"),
        "the read tool answers: {}",
        item["read"]
    );
    assert_eq!(
        item["primitives"],
        json!(true),
        "the connector edge is additive: fs and http stay"
    );
}

// ── The capability is an agent, not a skill ──

#[tokio::test]
async fn test_the_main_loop_schema_carries_none_of_the_trigger_tools() {
    let tmp = tempfile::tempdir().unwrap();
    let workspace = tmp.path().join("workspace");
    std::fs::create_dir_all(&workspace).unwrap();

    let mut manager = slimbot::ToolManager::new(workspace.clone());
    manager.init_from_config(&[]);
    let schema: Vec<String> = manager
        .to_openai_functions()
        .into_iter()
        .map(|d| d.name)
        .collect();
    assert!(!schema.is_empty(), "the default tool set is not empty");

    for name in ["trigger", "trigger_source", "executor"] {
        assert!(
            !schema.iter().any(|n| n == name),
            "`{name}` must stay out of the main loop's schema: {schema:?}"
        );
        assert!(
            !slimbot::agent::default_tool_set(&[])
                .iter()
                .any(|n| n == name),
            "`{name}` must not be part of the default agent tool set"
        );
    }
}

#[tokio::test]
async fn test_the_trigger_capability_ships_as_an_agent_and_not_a_skill() {
    let tmp = tempfile::tempdir().unwrap();
    let data_dir = tmp.path().to_string_lossy().to_string();
    slimbot::run_setup(None, &data_dir, None).expect("setup");

    let workspace = tmp.path().join("workspace");
    assert!(
        workspace.join("agents").join("trigger.md").is_file(),
        "the trigger agent is the user-facing entry"
    );

    let skills = workspace.join("skills");
    if skills.is_dir() {
        for entry in std::fs::read_dir(&skills).unwrap().flatten() {
            let name = entry.file_name().to_string_lossy().to_string();
            assert!(
                !name.contains("trigger"),
                "a trigger skill must not exist: {name}"
            );
        }
    }
}

// ── Virtual sources ──

/// A stdio MCP server that declares one tool, and can be launched in a mode
/// where every call fails, so an offline server is a deterministic case rather
/// than a race with a supervisor's probe.
const VIRTUAL_SERVER: &str = r#"
import json, sys
mode = sys.argv[sys.argv.index('--mode') + 1] if '--mode' in sys.argv else 'online'
for line in sys.stdin:
    msg = json.loads(line)
    if 'id' not in msg:
        continue
    method = msg.get('method')
    if method == 'initialize':
        out = {'jsonrpc': '2.0', 'id': msg['id'], 'result': {'protocolVersion': '2025-03-26', 'capabilities': {}, 'serverInfo': {'name': 'ci', 'version': '1'}}}
    elif method == 'tools/list':
        out = {'jsonrpc': '2.0', 'id': msg['id'], 'result': {'tools': [
            {'name': 'list_runs', 'description': 'List CI runs', 'inputSchema': {'type': 'object'}},
            {'name': 'push', 'description': 'Push one build notification', 'inputSchema': {'type': 'object'}},
        ]}}
    elif method == 'tools/call' and mode == 'failing':
        out = {'jsonrpc': '2.0', 'id': msg['id'], 'error': {'code': -32000, 'message': 'peer is down'}}
    elif method == 'tools/call':
        name = msg['params'].get('name')
        if name == 'push':
            sys.stdout.write(json.dumps({'jsonrpc': '2.0', 'method': 'notifications/builds', 'params': {'status': 'failed', 'run': 7}}) + '\n')
            sys.stdout.flush()
            text = 'pushed'
        else:
            text = 'run 7 failed'
        out = {'jsonrpc': '2.0', 'id': msg['id'], 'result': {'content': [{'type': 'text', 'text': text}]}}
    else:
        out = {'jsonrpc': '2.0', 'id': msg['id'], 'result': {}}
    sys.stdout.write(json.dumps(out) + '\n')
    sys.stdout.flush()
"#;

/// A loaded config whose one server declares a notification and a tool.
fn virtual_config(mode: &str) -> slimbot::Config {
    let raw = serde_json::json!({
        "agent": {"provider": "default"},
        "providers": {"default": {"api_key": "sk-test", "model": "gpt-4o"}},
        "tools": [],
        "channels": [],
        "mcp": {
            "servers": [{
                "name": "ci",
                "transport": "stdio",
                "command": "python3",
                "args": ["-c", VIRTUAL_SERVER, "--mode", mode],
                "triggers": [
                    {"type": "notification", "uri": "notifications/builds", "source_id": "builds"},
                    {"type": "tool", "tool": "list_runs", "source_id": "ci-runs"}
                ]
            }],
            "connect_timeout_seconds": 30,
            "call_timeout_seconds": 10,
            "backoff_initial_ms": 200,
        }
    });
    let file = tempfile::NamedTempFile::new().unwrap();
    std::fs::write(file.path(), serde_json::to_string(&raw).unwrap()).unwrap();
    slimbot::Config::load_resolved_with(
        file.path().to_str().unwrap(),
        &slimbot::secrets::SecretStore::new(vec![]),
    )
    .unwrap()
}

fn virtual_harness(mode: &str) -> Harness {
    let loaded = virtual_config(mode);
    let pool = Arc::new(McpService::new(&loaded.mcp));
    pool.start();
    Harness::with_mcp(
        TriggersConfig {
            enabled: true,
            ..Default::default()
        },
        pool,
        loaded.mcp.servers,
    )
}

#[tokio::test]
async fn test_a_tick_collects_a_virtual_tool_source_the_rule_reads() {
    // End to end: the config declares the subscription, the pool connects, and
    // a tick whose rule reads the source calls the tool and keeps the answer.
    let h = virtual_harness("online");
    wait_until(|| h.pool_online(), "the fake ci server").await;
    h.rule(
        &TriggerMeta {
            sources: vec!["ci-runs".to_string()],
            ..TriggerMeta::new("trg-ci", "ci runs")
        },
        Some("function judge(ctx) { return {fire: false}; }"),
    );

    let outcome = h.tick().await;
    assert_eq!(
        outcome.sources.succeeded,
        vec!["ci-runs".to_string()],
        "{:?}",
        outcome.sources.failed
    );
    let view = h.sources.view("ci-runs").expect("the tool answer");
    assert!(
        view.items[0]
            .as_str()
            .unwrap_or_default()
            .contains("run 7 failed"),
        "{:?}",
        view.items
    );

    // The listing names the source and its origin, so an agent can find it.
    let listed = h.sources.status();
    let row = |id: &str| listed.iter().find(|s| s.meta.id == id).unwrap();
    assert_eq!(
        row("ci-runs").origin,
        slimbot::trigger::source::Origin::Tool
    );
    assert_eq!(
        row("builds").origin,
        slimbot::trigger::source::Origin::Notification
    );
}

#[tokio::test]
async fn test_a_virtual_tool_source_records_a_failure_when_the_call_fails() {
    // A server that connects but refuses the call is one failed source, booked
    // against that source alone and visible in `list`.
    let h = virtual_harness("failing");
    wait_until(|| h.pool_online(), "the fake ci server").await;
    h.rule(
        &TriggerMeta {
            sources: vec!["ci-runs".to_string()],
            ..TriggerMeta::new("trg-ci", "ci runs")
        },
        Some("function judge(ctx) { return {fire: false}; }"),
    );

    let outcome = h.tick().await;
    assert!(outcome.sources.succeeded.is_empty(), "{outcome:?}");
    assert_eq!(outcome.sources.failed.len(), 1, "{outcome:?}");
    assert_eq!(outcome.sources.failed[0].id, "ci-runs");
    let listed = h.sources.status();
    let row = listed.iter().find(|s| s.meta.id == "ci-runs").unwrap();
    assert_eq!(row.status, "degraded");
    assert!(row.last_error.as_deref().unwrap_or("").contains("ci"));
}

#[tokio::test]
async fn test_a_rule_fires_from_a_notification_virtual_source() {
    // The passive path end to end: the server pushes a notification, the pool
    // buffers it under the declared source id, and a rule that reads only that
    // source fires from the frozen frame. Nothing was collected from disk.
    let h = virtual_harness("online");
    wait_until(|| h.pool_online(), "the fake ci server").await;
    h.rule(
        &TriggerMeta {
            sources: vec!["builds".to_string()],
            ..TriggerMeta::new("trg-build", "failed build")
        },
        Some(
            "function judge(ctx) { return {fire: ctx.sources.builds.items[0].params.status === 'failed', \
                        detail: 'build failed'}; }",
        ),
    );

    // Warm pass: the rule sees the source exists and leaves it unsampled.
    let cold = h.tick().await;
    assert!(cold.fired.is_empty(), "{cold:?}");

    // Push through the server: `notifications/push` is a client request the
    // launcher answers with a notification frame on the same stream.
    let pushed = h
        .borrow_pool()
        .call("mcp__ci__push", serde_json::json!({}))
        .await;
    assert!(pushed.is_ok(), "{pushed:?}");
    // The frame travels down the reader task, so give the sink a moment.
    wait_until(
        || !h.borrow_pool().notification_snapshot().is_empty(),
        "the pushed notification",
    )
    .await;

    let warm = h.tick().await;
    assert_eq!(warm.fired.len(), 1, "{warm:?}");
    assert_eq!(warm.fired[0].id, "trg-build");
    assert_eq!(warm.fired[0].detail, "build failed");
}

// ── End to end: notifications, lifecycle, the source tool, mixed frames ──
//
// These drive the same wiring the gateway does (`trigger::assemble`), and they
// cross the edges the unit tests cannot: the real `trigger` and
// `trigger_source` tools, the real config-declared subscriptions, and the real
// tick moving between them.

/// The virtual source the fake ci server pushes notifications under.
const NOTIFICATION_SOURCE: &str = "builds";
/// The virtual source that is one connector tool call.
const TOOL_SOURCE: &str = "ci-runs";

/// Reads both halves of a notification item, so a frame that lost either the
/// payload or its receive time is a failure rather than a quiet pass.
const JUDGE_NOTIFICATION: &str = r#"
function judge(ctx) {
  const item = (ctx.sources['builds'] || {items: []}).items[0];
  if (!item) return {fire: false};
  return {
    fire: item.params.status === 'failed',
    detail: 'run ' + item.params.run + ' at ' + item.received_at,
    evidence: [{source_id: 'builds', text: item.params.status}],
  };
}
"#;

/// A condition that reads the disk source and the tool source in the same
/// round, and says where each half of its evidence came from.
const JUDGE_MIXED: &str = r#"
function judge(ctx) {
  const disk = ctx.sources['src-0001'] || {items: []};
  const tool = ctx.sources['ci-runs'] || {items: []};
  const line = disk.items.map(i => i.title).join(', ');
  const runs = tool.items.map(i => String(i)).join(', ');
  const hit = line.indexOf('deploy') >= 0 && runs.indexOf('run 7 failed') >= 0;
  return {
    fire: hit,
    detail: 'disk says ' + line + '; ci says ' + runs,
    evidence: [
      {source_id: 'src-0001', text: line},
      {source_id: 'ci-runs', text: runs},
    ],
  };
}
"#;

/// A collector that succeeds, then throws once the file says so.
const COLLECT_THEN_THROW: &str = r#"
function collect(ctx) {
  if (ctx.fs.read('mode.txt').trim() === 'boom') throw new Error('feed is unreadable');
  return {items: [{title: 'ok'}]};
}
"#;

/// One call of a trigger-layer tool, answered the way the model sees it: the
/// tool's JSON result, parsed.
async fn call_tool(tool: &dyn Tool, args: Value) -> Value {
    let text = tool
        .execute(args)
        .await
        .unwrap_or_else(|e| panic!("{} refused to run: {e:#}", tool.name()));
    serde_json::from_str(&text)
        .unwrap_or_else(|e| panic!("{} answered with non-JSON {text:?}: {e}", tool.name()))
}

impl Harness {
    /// The `trigger` tool over this harness's own rule layer — the same
    /// construction the wiring hands the trigger agent.
    fn rule_tool(&self) -> slimbot::trigger::subscription::tool::TriggerTool {
        slimbot::trigger::subscription::tool::TriggerTool::new(
            self.rules.clone(),
            self.sources.clone(),
            self.workspace.clone(),
            self.sources.secrets(),
        )
    }

    fn source_tool(&self) -> TriggerSourceTool {
        TriggerSourceTool::new(self.sources.clone(), DEFAULT_MAX_ACTIVE_SOURCES)
    }

    /// One collection round scheduled the way the gateway's tick schedules it:
    /// reload both layers, settle the reference set, then collect — without
    /// judging, so the returned frame is the one the round froze.
    async fn collect_round(&self) -> TickReport {
        self.sources.reload();
        self.sources
            .set_referenced_sources(self.rules.referenced_sources());
        let index = self.sources.begin_tick();
        self.sources.collect(index, 1).await
    }

    fn observed_at(&self, id: &str) -> Option<i64> {
        self.sources.view(id).and_then(|v| v.observed_at)
    }

    /// What the layer booked for a source, which is the only trace a collector
    /// that stopped being scheduled leaves behind.
    fn last_error(&self, id: &str) -> Option<String> {
        self.sources
            .status()
            .into_iter()
            .find(|s| s.meta.id == id)
            .and_then(|s| s.last_error)
    }

    fn source_dirs(&self) -> Vec<String> {
        let mut names: Vec<String> = std::fs::read_dir(source_root(&self.workspace))
            .map(|entries| {
                entries
                    .flatten()
                    .map(|e| e.file_name().to_string_lossy().to_string())
                    .filter(|n| n.starts_with("src-"))
                    .collect()
            })
            .unwrap_or_default();
        names.sort();
        names
    }
}

// ── 1. A notification virtual source inside a tick's frame ──

#[tokio::test]
async fn test_a_tick_freezes_a_notification_source_as_an_item_with_its_params() {
    // Config declares the subscription, the server pushes one notification,
    // and the tick's frame carries it as `{params, received_at}` — the exact
    // shape a condition is documented to read.
    let h = virtual_harness("online");
    wait_until(|| h.pool_online(), "the fake ci server").await;
    h.rule(
        &rule_with("trg-build", &[NOTIFICATION_SOURCE], notify("{{detail}}")),
        Some(JUDGE_NOTIFICATION),
    );

    let cold = h.tick().await;
    assert!(cold.fired.is_empty(), "{cold:?}");
    let empty = cold
        .sources
        .frame
        .view(NOTIFICATION_SOURCE)
        .expect("a declared notification source is always in the frame");
    assert_eq!(empty.count, 0, "nothing pushed yet");
    assert!(empty.observed_at.is_none(), "{empty:?}");

    let pushed = h
        .borrow_pool()
        .call("mcp__ci__push", serde_json::json!({}))
        .await;
    assert!(pushed.is_ok(), "{pushed:?}");
    wait_until(
        || !h.borrow_pool().notification_snapshot().is_empty(),
        "the pushed notification",
    )
    .await;

    let warm = h.tick().await;
    let view = warm
        .sources
        .frame
        .view(NOTIFICATION_SOURCE)
        .expect("the notification source is in the frame")
        .clone();
    assert_eq!(view.count, 1, "{view:?}");
    assert_eq!(view.items.len(), 1, "{view:?}");
    let item = &view.items[0];
    assert_eq!(item["params"]["status"], json!("failed"), "{item}");
    assert_eq!(item["params"]["run"], json!(7), "{item}");
    let received_at = item["received_at"]
        .as_i64()
        .unwrap_or_else(|| panic!("the item carries when it arrived: {item}"));
    assert_eq!(
        view.observed_at,
        Some(received_at),
        "the view's freshness is the push's own timestamp"
    );
    // The frame is what the condition reads, so a rule waiting on this source
    // fires from the push and its `{{now}}`-free template names the arrival.
    assert_eq!(warm.fired.len(), 1, "{warm:?}");
    assert_eq!(
        warm.fired[0].detail,
        format!("run 7 at {received_at}"),
        "the script read both halves of the item"
    );
}

// ── 2. A tool virtual source inside a tick's frame ──

#[tokio::test]
async fn test_a_tick_freezes_a_tool_source_as_the_tool_result() {
    // One declared `type=tool` subscription: the frame holds exactly the
    // connector's answer, as the single item of the view.
    let h = virtual_harness("online");
    wait_until(|| h.pool_online(), "the fake ci server").await;
    // The rule has to have judged once: the reference set the round obeys is
    // pushed by a judging tick, not by the round itself.
    h.rule(
        &rule_with("trg-ci", &[], notify("{{detail}}")),
        Some("function judge() { return {fire: false}; }"),
    );
    h.tick().await;
    let rule = call_tool(
        &h.rule_tool(),
        json!({"action": "update", "id": "trg-ci", "sources": [TOOL_SOURCE]}),
    )
    .await;
    assert_eq!(rule["updated"], true, "{rule}");

    // Round one samples the source; the frame it freezes is the world before
    // that sample, so it is the *next* round's frame that carries the answer.
    let sampled = h.collect_round().await;
    assert_eq!(
        sampled.succeeded,
        vec![TOOL_SOURCE.to_string()],
        "{:?}",
        sampled.failed
    );
    let after_sample = h.collect_round().await;
    let view = after_sample
        .frame
        .view(TOOL_SOURCE)
        .expect("the tool source is in the frame")
        .clone();
    assert_eq!(view.count, 1, "{view:?}");
    let item = view.items[0].as_str().unwrap_or_default();
    assert_eq!(
        item,
        "<codemode_output server=\"ci\" tool=\"list_runs\">\nrun 7 failed\n</codemode_output>",
        "the item is the connector's answer, wrapped as codemode hands it on"
    );
    assert!(view.observed_at.is_some(), "the frame says when it sampled");

    // The next frame carries the same answer with a sample at least as fresh,
    // which is what makes "the tool was called again" observable at all.
    let next = h.collect_round().await;
    let again = next.frame.view(TOOL_SOURCE).expect("still in the frame");
    assert_eq!(again.items, view.items);
    assert!(
        again.observed_at >= view.observed_at,
        "{again:?} vs {view:?}"
    );
    // The stamp the last *successful* round left behind, which is what the
    // failures below must preserve. Read from the source rather than from a
    // frame: a round freezes the world *before* it samples, so any frame lags
    // the sample by one round and would make the comparison below race on a
    // millisecond.
    let sampled_at = h.observed_at(TOOL_SOURCE);
    assert!(
        sampled_at.is_some(),
        "the successful rounds stamped the source"
    );

    // Stop the connector pool and read the next frame. The scheduled round
    // can only fail now, so the answer standing in the frame is the one from
    // the last call — a frozen sample, not a fresh one taken behind our back.
    h.borrow_pool().shutdown().await;
    let unread = h.collect_round().await;
    assert_eq!(
        unread.failed.len(),
        1,
        "with every server down the call itself fails: {:?}",
        unread.succeeded
    );
    assert_eq!(unread.failed[0].id, TOOL_SOURCE);
    let frozen = unread
        .frame
        .view(TOOL_SOURCE)
        .expect("a failed sample keeps the previous answer in the frame")
        .clone();
    assert_eq!(frozen.items, view.items);
    // A failure keeps the entries and re-stamps nothing: the source's sample
    // time is the last *successful* round's, and `unread` saw the entries that
    // round left behind. Comparing stamps here would be a millisecond race — a
    // round with no due samples still reads `now` when it freezes its frame, so
    // the frame time moves without anything being sampled. What the claim pins
    // is the source's own stamp, so read that, after the clock has moved far
    // enough that a re-stamp could not hide.
    tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    let later = h.collect_round().await;
    assert_eq!(
        later.failed.len(),
        1,
        "the pool stays down: {:?}",
        later.succeeded
    );
    assert_eq!(
        h.observed_at(TOOL_SOURCE),
        sampled_at,
        "a failed round does not re-stamp the source: {sampled_at:?}"
    );
    assert_eq!(
        later.frame.view(TOOL_SOURCE).map(|v| v.items.clone()),
        Some(frozen.items.clone()),
        "and it still carries the last successful answer"
    );
}

// ── 3. Lifecycle: deleting the rule retires the source, not the files ──

/// Where a source's definition lives, so a test can say "still on disk".
fn source_files(h: &Harness, id: &str) -> Vec<PathBuf> {
    vec![
        source_root(h.workspace()).join(id).join("meta.json"),
        source_root(h.workspace()).join(id).join("collect.js"),
    ]
}

#[tokio::test]
async fn test_deleting_the_rule_stops_collecting_its_sources_and_keeps_their_files() {
    let h = Harness::new();
    h.write_file("feed.txt", "a line\n");
    std::fs::write(h.workspace().join("mode.txt"), "calm").unwrap();

    // Both definitions are created through the real tool, so the ids are the
    // tool's own (`src-xxxx`), not ones the test wrote by hand.
    let sources = h.source_tool();
    let created = call_tool(
        &sources,
        json!({"action": "create", "name": "feed", "script": COLLECT_FEED}),
    )
    .await;
    assert_eq!(created["created"], true, "{created}");
    let collected = created["id"].as_str().unwrap().to_string();
    let created = call_tool(
        &sources,
        json!({"action": "create", "name": "breaks", "script": COLLECT_THEN_THROW}),
    )
    .await;
    let flaky = created["id"].as_str().unwrap().to_string();
    assert_eq!(h.source_dirs().len(), 2, "{:?}", h.source_dirs());
    for id in [&collected, &flaky] {
        for path in source_files(&h, id) {
            assert!(path.is_file(), "the tool wrote {path:?}");
        }
    }

    let rule = call_tool(
        &h.rule_tool(),
        json!({"action": "create", "name": "watches both",
               "sources": [collected.clone(), flaky.clone()],
               "on_fire": "record",
               "script": "function judge() { return {fire: false}; }"}),
    )
    .await;
    assert_eq!(rule["created"], true, "{rule}");
    let rule_id = rule["id"].as_str().unwrap().to_string();

    // Collected: both sources are worked on and the frame says when.
    let first = h.tick().await;
    assert_eq!(
        first.sources.succeeded.len(),
        2,
        "{:?}",
        first.sources.failed
    );
    assert!(h.observed_at(&collected).is_some(), "{first:?}");
    assert!(
        h.last_error(&collected).is_none() && h.last_error(&flaky).is_none(),
        "a healthy round books no error"
    );

    // The rule goes away through the tool, which removes the directory and
    // drops the definition from the layer in the same call.
    let deleted = call_tool(&h.rule_tool(), json!({"action": "delete", "id": rule_id})).await;
    assert_eq!(deleted["deleted"], true, "{deleted}");
    assert!(
        !rule_root(h.workspace()).join(&rule_id).exists(),
        "the rule directory is gone"
    );

    // Next round: neither source is scheduled any more. The collector that
    // used to throw proves it by staying clean — had it run, it would have
    // booked a failure.
    std::fs::write(h.workspace().join("mode.txt"), "boom").unwrap();
    let after = h.tick().await;
    assert!(after.sources.due.is_empty(), "{:?}", after.sources.due);
    assert!(after.sources.succeeded.is_empty(), "{after:?}");
    assert!(after.sources.failed.is_empty(), "{after:?}");
    assert_eq!(
        h.last_error(&flaky),
        None,
        "an unreferenced collector must not run"
    );

    // The definitions themselves are untouched: a deleted rule never deletes
    // the sources it used to read.
    assert_eq!(h.source_dirs().len(), 2, "{:?}", h.source_dirs());
    for id in [&collected, &flaky] {
        for path in source_files(&h, id) {
            assert!(path.is_file(), "the rule's deletion removed {path:?}");
        }
    }

    // A fresh process over the same files still lists both.
    let restarted = h.restart();
    let listed = call_tool(&restarted.source_tool(), json!({"action": "list"})).await;
    let ids: Vec<&str> = listed["sources"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|s| s["id"].as_str())
        .collect();
    assert!(ids.contains(&collected.as_str()), "{listed}");
    assert!(ids.contains(&flaky.as_str()), "{listed}");

    // And a new rule reading one of them brings it straight back: the files
    // were never the thing that was retired.
    h.rule(
        &rule_with("trg-again", &[&collected], notify("{{detail}}")),
        Some(JUDGE_ANY_ITEM),
    );
    let revived = h.tick().await;
    assert_eq!(
        revived.sources.succeeded,
        vec![collected.clone()],
        "{:?}",
        revived.sources.failed
    );
}

#[tokio::test]
async fn test_a_deleted_source_is_no_longer_collected_and_its_directory_is_gone() {
    // The other direction of the same edge: `trigger_source delete` removes
    // the definition, the tick stops scheduling it, and the rule that named it
    // is told the source is missing rather than shown stale data.
    let h = Harness::new();
    h.write_file("feed.txt", "a line\n");
    let created = call_tool(
        &h.source_tool(),
        json!({"action": "create", "name": "feed", "script": COLLECT_FEED}),
    )
    .await;
    let id = created["id"].as_str().unwrap().to_string();
    h.rule(
        &rule_with("trg-a", &[&id], notify("{{detail}}")),
        Some(JUDGE_ANY_ITEM),
    );

    assert_eq!(h.tick().await.sources.succeeded, vec![id.clone()]);

    let deleted = call_tool(&h.source_tool(), json!({"action": "delete", "id": id})).await;
    assert_eq!(deleted["deleted"], true, "{deleted}");
    let after = h.tick().await;
    assert!(after.sources.due.is_empty(), "{:?}", after.sources.due);
    assert!(
        after.sources.frame.view(&id).is_none(),
        "a deleted source leaves the frame"
    );
    assert!(
        h.rules.status().iter().any(|s| s.meta.id == "trg-a"
            && s.state
                .last_error
                .as_deref()
                .is_some_and(|e| e.contains("not found"))),
        "the rule says its source is gone: {:?}",
        h.rules.status().len()
    );
    assert!(!source_root(h.workspace()).join(&id).exists(), "{id}");
    assert!(h.source_dirs().is_empty(), "{:?}", h.source_dirs());
}

#[tokio::test]
async fn test_removing_a_subscription_retires_the_virtual_source_at_the_next_tick() {
    // A virtual source has no directory to delete: its lifecycle is the MCP
    // config. Dropping the subscription must therefore retire it as completely
    // as deleting a rule retires a disk source — no collection, no frame entry.
    let h = virtual_harness("online");
    wait_until(|| h.pool_online(), "the fake ci server").await;
    h.rule(
        &rule_with("trg-ci", &[], notify("{{detail}}")),
        Some("function judge() { return {fire: false}; }"),
    );
    h.tick().await;
    let rule = call_tool(
        &h.rule_tool(),
        json!({"action": "update", "id": "trg-ci", "sources": [TOOL_SOURCE]}),
    )
    .await;
    assert_eq!(rule["updated"], true, "{rule}");
    h.collect_round().await;
    let sampled = h.collect_round().await;
    assert_eq!(sampled.succeeded, vec![TOOL_SOURCE.to_string()]);
    assert!(
        sampled
            .frame
            .view(TOOL_SOURCE)
            .is_some_and(|view| view.observed_at.is_some()),
        "{:?}",
        sampled.frame.view(TOOL_SOURCE)
    );
    let row = h
        .sources
        .status()
        .into_iter()
        .find(|s| s.meta.id == TOOL_SOURCE)
        .expect("declared");
    assert!(row.last_collected_at.is_some(), "{row:?}");

    // The server restarts with the subscription gone from its config.
    let emptied: Vec<McpServerConfig> = h
        .servers
        .clone()
        .into_iter()
        .map(|mut server| {
            server.triggers.retain(|t| t.source_id != TOOL_SOURCE);
            server
        })
        .collect();
    assert!(
        emptied
            .iter()
            .all(|s| s.triggers.iter().all(|t| t.source_id != TOOL_SOURCE)),
        "the fixture really dropped the subscription"
    );
    let restarted = Harness::over_with(
        h._tmp.clone(),
        h.config.clone(),
        h.pool.borrow().clone(),
        None,
        emptied,
    );
    let before = h.sources.status().len();
    assert_eq!(before, 2, "the fixture starts with both subscriptions");
    assert!(
        restarted
            .sources
            .status()
            .iter()
            .all(|s| s.meta.id != TOOL_SOURCE),
        "the restart does not know the removed subscription at all: {:?}",
        restarted
            .sources
            .status()
            .iter()
            .map(|s| s.meta.id.clone())
            .collect::<Vec<_>>()
    );
    // The first judgment round samples what is left of the world; the second
    // is the one that can still fail cleanly.
    restarted.tick().await;
    let after = restarted.tick().await;
    assert!(
        !after.sources.due.contains(&TOOL_SOURCE.to_string()),
        "{:?}",
        after.sources.due
    );
    assert!(
        after.sources.frame.view(TOOL_SOURCE).is_none(),
        "an undeclared source leaves the frame: {:?}",
        after.sources.frame.view(TOOL_SOURCE)
    );
    assert!(after.sources.failed.is_empty(), "{after:?}");
    assert!(
        h.sources.status().iter().any(|s| s.meta.id == TOOL_SOURCE),
        "the fixture removed it, rather than the source never existing"
    );
}

// ── 4. Embedded trial run through the real tool ──

#[tokio::test]
async fn test_creating_a_source_through_the_tool_runs_it_and_a_broken_one_writes_nothing() {
    let h = Harness::new();
    h.write_file("feed.txt", "deploy failed\n");
    let tool = h.source_tool();

    let good = call_tool(
        &tool,
        json!({"action": "create", "name": "feed", "script": COLLECT_FEED}),
    )
    .await;
    assert_eq!(good["created"], true, "{good}");
    assert_eq!(good["test"]["ok"], true, "{good}");
    assert_eq!(good["test"]["items"], 1, "{good}");
    assert!(good["test"]["elapsed_ms"].is_u64(), "{good}");
    let id = good["id"].as_str().expect("the response names the id");
    let dir = source_root(h.workspace()).join(id);
    assert!(dir.join("meta.json").is_file(), "{}, {id}", dir.display());
    assert!(dir.join("collect.js").is_file(), "{id}");

    // The very same definition, with a collector that cannot run.
    let broken = call_tool(
        &tool,
        json!({"action": "create", "name": "broken",
               "script": "function collect(ctx) { throw new Error('nope'); }"}),
    )
    .await;
    assert_eq!(broken["created"], false, "{broken}");
    assert_eq!(broken["test"]["ok"], false, "{broken}");
    assert!(
        broken["error"].as_str().unwrap_or("").contains("nope"),
        "the refusal says why: {broken}"
    );
    assert!(
        broken["error"]
            .as_str()
            .unwrap_or("")
            .contains("nothing was written"),
        "{broken}"
    );
    assert_eq!(
        h.source_dirs(),
        vec![id.to_string()],
        "a refused create leaves no directory behind"
    );
    // And no half-written sibling either: the sources root holds exactly the
    // one source directory and nothing else.
    let mut root_entries: Vec<String> = std::fs::read_dir(source_root(h.workspace()))
        .unwrap()
        .flatten()
        .map(|e| e.file_name().to_string_lossy().to_string())
        .collect();
    root_entries.sort();
    assert_eq!(root_entries, vec![id.to_string()], "{root_entries:?}");
}

// ── 5. The active-source cap, through the tool ──

#[tokio::test]
async fn test_a_create_past_the_cap_is_refused_and_lists_the_sources() {
    // The cap counts what the enabled rules read, and one rule exists here,
    // so the reference set is real rather than "every source".
    let h = Harness::new();
    h.write_file("feed.txt", "one line\n");
    h.source("src-0001", COLLECT_FEED);
    // The rule must have judged at least once: only a judging tick pushes the
    // reference set, and that set is what decides which sources count.
    h.rule(
        &rule_with("trg-a", &[], notify("{{detail}}")),
        Some("function judge() { return {fire: false}; }"),
    );
    h.tick().await;
    assert_eq!(
        h.sources.active_source_ids(),
        Some(std::collections::HashSet::new()),
        "one tick settles the reference set"
    );
    let rule = call_tool(
        &h.rule_tool(),
        json!({"action": "update", "id": "trg-a", "sources": ["src-0001"]}),
    )
    .await;
    assert_eq!(rule["updated"], true, "{rule}");
    assert_eq!(
        h.source_dirs(),
        vec!["src-0001".to_string()],
        "the fixture's source stays on disk"
    );
    h.tick().await;
    assert_eq!(
        h.sources.active_source_ids(),
        Some(["src-0001".to_string()].into()),
        "the cap must count what the rule reads"
    );

    let tool = TriggerSourceTool::new(h.sources.clone(), 1);
    let refused = call_tool(
        &tool,
        json!({"action": "create", "name": "another", "script": COLLECT_FEED}),
    )
    .await;
    assert_eq!(refused["created"], false, "{refused}");
    assert_eq!(refused["count"], 1, "{refused}");
    assert_eq!(refused["max_active_sources"], 1, "{refused}");
    let listed = refused["sources"].as_array().expect("the source list");
    assert_eq!(listed.len(), 1, "{refused}");
    assert_eq!(listed[0]["id"], json!("src-0001"), "{refused}");
    assert_eq!(listed[0]["referenced"], json!(true), "{refused}");
    assert_eq!(listed[0]["origin"], json!("disk"), "{refused}");
    assert!(
        refused["error"]
            .as_str()
            .unwrap_or("")
            .contains("max_active_sources"),
        "the refusal names the setting it obeyed: {refused}"
    );
    assert_eq!(h.source_dirs(), vec!["src-0001".to_string()]);

    // The refusal is about the cap, not the script: with the rule gone the
    // same call goes through, through the tool the trigger agent holds (its
    // cap is the configured one).
    call_tool(&h.rule_tool(), json!({"action": "delete", "id": "trg-a"})).await;
    h.tick().await;
    let allowed = call_tool(
        &TriggerSourceTool::new(h.sources.clone(), DEFAULT_MAX_ACTIVE_SOURCES),
        json!({"action": "create", "name": "another", "script": COLLECT_FEED}),
    )
    .await;
    assert_eq!(allowed["created"], true, "{allowed}");
    assert_eq!(
        allowed["test"]["ok"], true,
        "the script itself is fine: {allowed}"
    );
    assert_eq!(
        h.source_dirs().len(),
        2,
        "the allowed create wrote its directory: {:?}",
        h.source_dirs()
    );
}

// ── 6. A disk source and a tool source in the same frame ──

#[tokio::test]
async fn test_a_tick_freezes_a_disk_and_a_tool_source_and_the_judge_reads_both() {
    let h = virtual_harness("online");
    wait_until(|| h.pool_online(), "the fake ci server").await;
    h.write_file("feed.txt", "all quiet\ndeploy failed on prod\n");
    h.source("src-0001", COLLECT_FEED);
    h.rule(
        &rule_with(
            "trg-mixed",
            &["src-0001", "ci-runs"],
            notify("{{detail}} | {{evidence}}"),
        ),
        Some(JUDGE_MIXED),
    );

    // The rule reads both, so both halves of the world are sampled together.
    let first = h.tick().await;
    assert_eq!(
        first.sources.succeeded.len(),
        2,
        "one round, both sources: {:?}",
        first.sources.failed
    );
    let frame = &first.sources.frame;
    assert!(
        frame.view("src-0001").is_some() && frame.view("ci-runs").is_some(),
        "both live in the same frame: {:?}",
        frame.view("src-0001")
    );

    let warm = h.tick().await;
    assert_eq!(warm.fired.len(), 1, "{warm:?}");
    let fire = &warm.fired[0];
    assert_eq!(fire.id, "trg-mixed");
    assert!(
        fire.evidence
            .iter()
            .any(|e| e["source_id"] == json!("src-0001")),
        "the disk half: {:?}",
        fire.evidence
    );
    assert!(
        fire.evidence
            .iter()
            .any(|e| e["source_id"] == json!("ci-runs")),
        "the virtual half: {:?}",
        fire.evidence
    );
    assert_eq!(warm.queued, 1, "{warm:?}");
    assert_eq!(h.drain().await, 1);

    let mut h = h;
    let inbound = h.inbound().await.expect("one notice");
    assert!(
        inbound
            .content
            .contains("disk says all quiet, deploy failed on prod"),
        "{}",
        inbound.content
    );
    // The connector's answer is wrapped by codemode on its way into a
    // condition, so what the script saw is the envelope around the text.
    assert!(
        inbound
            .content
            .contains(r#"ci says <codemode_output server="ci" tool="list_runs">"#)
            && inbound.content.contains("run 7 failed"),
        "{}",
        inbound.content
    );
    assert!(
        inbound.content.contains("[src-0001 @") && inbound.content.contains("[ci-runs @"),
        "evidence from both sides reaches the notice: {}",
        inbound.content
    );
    let rows = h.rows();
    assert_eq!(rows.len(), 1, "{rows:?}");
    assert_eq!(rows[0]["ok"], json!(true));
}
