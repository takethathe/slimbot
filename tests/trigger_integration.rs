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
use slimbot::trigger::source::meta as source_meta;
use slimbot::trigger::subscription::meta as rule_meta;
use slimbot::trigger::subscription::meta::{
    ActionKind, TriggerAction, TriggerMeta, load_all as load_rules,
};
use slimbot::trigger::subscription::state::TriggerState;
use slimbot::trigger::{TickOutcome, TriggerService, assemble, build_layers, history_path};
use slimbot::{
    AgentConfig, CronSchedule, FinishReason, LLMResponse, McpService, MemoryService, Message,
    Provider, SecretEntry, SessionManager, SharedMemory, SpanContext, SpanRecorder, SpanStatus,
    ToolDefinition, TraceBackend, Tracer, TriggersConfig, Usage,
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
    workspace: PathBuf,
    config: TriggersConfig,
    sources: Arc<SourceLayer>,
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
        let workspace = tmp.path().join("workspace");
        std::fs::create_dir_all(source_root(&workspace)).unwrap();
        std::fs::create_dir_all(rule_root(&workspace)).unwrap();
        std::fs::create_dir_all(workspace.join("agents")).unwrap();

        let (bus, consumers) = MessageBus::new();
        let spans = Arc::new(Spans::default());
        let tracer = Tracer::new(vec![spans.clone() as Arc<dyn TraceBackend>]);
        let layers = build_layers(&config, &workspace);
        let sources = layers.sources.clone();
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
        let wiring = assemble(layers, &config, &workspace, tracer, runtime, bus);
        Self {
            workspace,
            config,
            sources,
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
    ) -> anyhow::Result<LLMResponse> {
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
    for n in 1..=4 {
        h.source(&format!("src-000{n}"), SLOW_COLLECT);
    }

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
    h.rule(
        &rule_with("trg-a", &[], notify("{{detail}}")),
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
