//! Acting on a fire: the queue, the actions, the delivery and the history.
//!
//! This layer sits *outside* the tick. A tick decides whether something is a
//! fire and hands it over; everything that can take longer than a few
//! milliseconds — running a subagent, sending a message to a conversation —
//! happens here, so a hard tick deadline never cuts an in-flight run in half.
//!
//! Two halves live in this file:
//!
//! - the history ledger, one appended line per booked fire, never carrying an
//!   evidence body, and a write failure that logs and moves on;
//! - [`Dispatcher`], the bounded FIFO queue and its single consumer, which
//!   turns a [`Fire`] into work: start a run, inject a notice into the
//!   originating conversation, or book the fire silently.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use anyhow::{Context, Result};
use serde_json::{Value, json};
use tokio::sync::mpsc;

use crate::agent::{AcquireMode, StartReject, StartRequest, SubagentRuntime};
use crate::debug;
use crate::delivery::{DeliveryTarget, Resolution};
use crate::message_bus::{BusRequest, BusResult, MessageBus};
use crate::session::TaskHook;
use crate::trace::{SpanRecorder, SpanStatus, Tracer};

use super::subscription::evidence::{iso_local, render_template};
use super::subscription::meta::ActionKind;
use super::subscription::{Alert, Fire};

/// Fire ledger next to the rule directories. Append-only, one JSON object per
/// line, never carrying an evidence body.
pub const HISTORY_FILE: &str = "history.jsonl";

/// Append one history line, then keep only the newest `max_lines` of the file.
///
/// Rolling rewrites the file in place, so the ledger holds exactly one
/// generation — a reader never has to stitch an older file back together.
pub fn append_history(path: &Path, row: &Value, max_lines: usize) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("cannot create {}", parent.display()))?;
    }
    let line = serde_json::to_string(row).context("history row is not serializable")?;
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .with_context(|| format!("cannot open {}", path.display()))?;
    std::io::Write::write_all(&mut file, line.as_bytes())
        .with_context(|| format!("cannot append to {}", path.display()))?;
    std::io::Write::write_all(&mut file, b"\n")?;

    roll_history(path, max_lines)?;
    Ok(())
}

/// Read the ledger back, newest last. Unparsable lines are skipped: a torn
/// final line must not make the whole history unreadable.
pub fn read_history(path: &Path) -> Result<Vec<Value>> {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e).with_context(|| format!("cannot read {}", path.display())),
    };
    Ok(text
        .lines()
        .filter(|line| !line.trim().is_empty())
        .filter_map(|line| serde_json::from_str(line).ok())
        .collect())
}

/// Truncate the file to its last `max_lines` lines. Cheap enough to run on
/// every append: the ledger is a few hundred kilobytes at most.
fn roll_history(path: &Path, max_lines: usize) -> Result<()> {
    let text = std::fs::read_to_string(path)
        .with_context(|| format!("cannot read {} for rollover", path.display()))?;
    let lines: Vec<&str> = text
        .lines()
        .filter(|line| !line.trim().is_empty())
        .collect();
    if lines.len() <= max_lines {
        return Ok(());
    }
    let kept = &lines[lines.len() - max_lines..];
    let rolled = kept.join("\n") + "\n";
    crate::trigger::source::meta::write_atomic(path, rolled.as_bytes())
}

// ── Work items ──

/// One unit of work handed over by a tick.
#[derive(Debug)]
pub enum Work {
    /// `notify` fires from one tick that share a destination, carried to that
    /// conversation as a single input. `overflow` holds the fires that could not
    /// join it: booked, never sent.
    Notice {
        dest: String,
        fires: Vec<Fire>,
        overflow: Vec<Fire>,
    },
    /// One `agent` fire: start the run, whose result the runtime injects.
    Run { fire: Fire },
    /// A fire that is booked and sends nothing — a `record` rule, or a fire with
    /// nowhere to go. `error` says why nothing was sent.
    Book { fire: Fire, error: Option<String> },
    /// Operations alert: fixed text, straight to the channel. The one delivery
    /// in this chain that skips the main loop — it summarizes nothing, and
    /// paying a model to relay "a rule broke" would buy noise.
    Alert { alert: Alert },
}

/// Group a tick's fires into work.
///
/// Only `notify` merges, and only within one tick and one destination: `agent`
/// results finish at different moments, and holding a finished one back to pair
/// it with a slower one is a defect rather than a feature. Across ticks nothing
/// merges either — the queue cannot see how many inputs a session is already
/// holding.
pub fn group_fires(fires: Vec<Fire>, max_merge: usize) -> Vec<Work> {
    let max_merge = max_merge.max(1);
    let mut works: Vec<Work> = Vec::new();
    for fire in fires {
        // Whether a fire can reach anybody at all was decided by the judgment;
        // dispatch honours that answer instead of re-deriving it.
        if !fire.delivers {
            works.push(Work::Book { fire, error: None });
            continue;
        }
        match fire.action.kind {
            ActionKind::Record => works.push(Work::Book { fire, error: None }),
            ActionKind::Agent => works.push(Work::Run { fire }),
            ActionKind::Notify => match destination(&fire) {
                Resolution::Deliver(target) => {
                    push_notice(&mut works, target.session_id(), fire, max_merge)
                }
                Resolution::Nowhere => {
                    crate::warn_log!(
                        "[trigger] rule {} has nowhere to notify: it declares no channel/to and \
                         was not created in a conversation; the fire is booked only",
                        fire.id
                    );
                    works.push(Work::Book {
                        fire,
                        error: Some("notify has no destination".to_string()),
                    });
                }
            },
        }
    }
    works
}

fn push_notice(works: &mut Vec<Work>, dest: String, fire: Fire, max_merge: usize) {
    for work in works.iter_mut() {
        if let Work::Notice {
            dest: existing,
            fires,
            overflow,
        } = work
        {
            if *existing != dest {
                continue;
            }
            if fires.len() < max_merge {
                fires.push(fire);
            } else {
                overflow.push(fire);
            }
            return;
        }
    }
    works.push(Work::Notice {
        dest,
        fires: vec![fire],
        overflow: Vec::new(),
    });
}

/// Where a `notify` goes, resolved by the shared delivery seam: an explicit
/// channel and target, else the conversation the rule was created in.
///
/// Triggers never fall back to the owner's primary channel — they always carry
/// an origin, and an unreachable origin stays book-only rather than being
/// misdelivered (`delivery::resolve_target`).
fn destination(fire: &Fire) -> Resolution {
    let action = &fire.action;
    crate::delivery::resolve_target(
        DeliveryTarget::from_pair(action.channel.as_deref(), action.to.as_deref()),
        fire.origin
            .as_ref()
            .and_then(|origin| DeliveryTarget::from_session_id(&origin.session_id())),
        None,
    )
}

/// The fixed text of an operations alert: which rule broke, how often, and the
/// promise that it will stay quiet now. Nothing here is worth a model call.
pub fn alert_text(alert: &Alert) -> String {
    format!(
        "[Trigger alert] id={} name={} — the rule failed {} time(s) in a row and is now judged \
         once every {} ticks. Last error: {}. Nothing more will be said about it until it recovers.",
        alert.id,
        alert.name,
        alert.failures,
        super::subscription::DEGRADED_EVERY_N_TICKS,
        alert.error
    )
}

/// The notice a group of `notify` fires becomes: a header line saying where it
/// came from, then the rendered blocks. The footer exists so the main loop does
/// not read a trigger as something the user said.
pub fn compose_notice(fires: &[Fire]) -> String {
    let ids = fires
        .iter()
        .map(|fire| fire.id.as_str())
        .collect::<Vec<_>>()
        .join(",");
    let at = fires
        .first()
        .map(|fire| iso_local(fire.fired_at))
        .unwrap_or_default();
    let blocks = fires
        .iter()
        .map(render_fire)
        .collect::<Vec<_>>()
        .join("\n\n---\n\n");
    format!(
        "[Trigger notice] id={ids} merged={merged} at={at}\n\n{blocks}\n\n\
         This is a trigger firing, not a message from the user. Pass it on as written.",
        merged = fires.len()
    )
}

/// Fill the action's template with this fire's evidence, summary and time.
fn render_fire(fire: &Fire) -> String {
    let template = match fire.action.kind {
        ActionKind::Notify => fire.action.text_template.as_deref().unwrap_or("{{detail}}"),
        _ => fire.action.task_template.as_deref().unwrap_or("{{detail}}"),
    };
    render_template(template, &fire.evidence, &fire.detail, fire.fired_at)
        .unwrap_or_else(|e| format!("(template error: {e})"))
}

/// The fires carried by a work item, for the ledger and the logs.
fn fires_of(work: &Work) -> Vec<&Fire> {
    match work {
        Work::Notice {
            fires, overflow, ..
        } => fires.iter().chain(overflow.iter()).collect(),
        Work::Run { fire } | Work::Book { fire, .. } => vec![fire],
        Work::Alert { .. } => Vec::new(),
    }
}

// ── The queue and its consumer ──

/// The bounded FIFO queue between the tick and the world.
///
/// Enqueueing never waits: a full queue means fires are arriving faster than
/// actions can run, and dropping the newest is the honest answer — a reminder
/// that lands ten minutes late is worse than one that never lands. Nothing is
/// rescheduled.
///
/// One consumer drains it, outside the tick, in the order fires were made. That
/// is what keeps the most expensive step of a fire — a model call — away from
/// the tick's hard deadline.
pub struct Dispatcher {
    tx: mpsc::Sender<Work>,
    /// The consumer's half. Taken once by [`Self::run`], or borrowed for the
    /// length of [`Self::drain`]; either way exactly one consumer holds it.
    rx: std::sync::Mutex<Option<mpsc::Receiver<Work>>>,
    runtime: Option<Arc<SubagentRuntime>>,
    bus: Arc<MessageBus>,
    history_path: PathBuf,
    history_lines: usize,
    max_merge: usize,
    tracer: Tracer,
    /// Fires dropped because the queue was full.
    dropped: AtomicUsize,
}

impl Dispatcher {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        runtime: Option<Arc<SubagentRuntime>>,
        bus: Arc<MessageBus>,
        history_path: PathBuf,
        history_lines: usize,
        max_queue: usize,
        max_merge: usize,
        tracer: Tracer,
    ) -> Arc<Self> {
        let (tx, rx) = mpsc::channel(max_queue.max(1));
        Arc::new(Self {
            tx,
            rx: std::sync::Mutex::new(Some(rx)),
            runtime,
            bus,
            history_path,
            history_lines: history_lines.max(1),
            max_merge: max_merge.max(1),
            tracer,
            dropped: AtomicUsize::new(0),
        })
    }

    /// Queue one tick's fires and return how many were accepted.
    pub fn enqueue_fires(&self, fires: Vec<Fire>) -> usize {
        self.enqueue_all(group_fires(fires, self.max_merge))
    }

    /// Queue one tick's alerts and return how many were accepted.
    pub fn enqueue_alerts(&self, alerts: Vec<Alert>) -> usize {
        self.enqueue_all(
            alerts
                .into_iter()
                .map(|alert| Work::Alert { alert })
                .collect(),
        )
    }

    fn enqueue_all(&self, works: Vec<Work>) -> usize {
        let mut accepted = 0;
        for work in works {
            if self.enqueue(work) {
                accepted += 1;
            }
        }
        accepted
    }

    fn enqueue(&self, work: Work) -> bool {
        match self.tx.try_send(work) {
            Ok(()) => true,
            Err(mpsc::error::TrySendError::Full(work)) => {
                self.note_dropped(&work, "the dispatch queue is full");
                false
            }
            Err(mpsc::error::TrySendError::Closed(work)) => {
                self.note_dropped(&work, "no dispatch consumer is running");
                false
            }
        }
    }

    /// A dropped fire still belongs in the ledger: `ok=false` is the only place
    /// a later reader can find out that the rule fired and said nothing.
    fn note_dropped(&self, work: &Work, reason: &str) {
        self.dropped.fetch_add(1, Ordering::Relaxed);
        crate::warn_log!(
            "[trigger] dropped fire(s) {}: {reason}",
            fires_of(work)
                .iter()
                .map(|fire| fire.id.as_str())
                .collect::<Vec<_>>()
                .join(",")
        );
        for fire in fires_of(work) {
            self.book(fire, 0, None, Some(reason.to_string()));
        }
    }

    /// Fires dropped so far because the queue had no room.
    pub fn dropped_count(&self) -> usize {
        self.dropped.load(Ordering::Relaxed)
    }

    /// Whether an alert has a reader at all — the same three levels a fire goes
    /// through: the action's own flag, then the agent definition's default, then
    /// whether there is a conversation to tell. A `record` rule answers no: a
    /// watcher that never speaks to anybody has no audience for its failures,
    /// and an alert it cannot route would be dropped by the queue anyway.
    pub fn is_alertable(&self, alert: &Alert) -> bool {
        let routable = alert
            .origin
            .as_ref()
            .and_then(|origin| DeliveryTarget::from_session_id(&origin.session_id()))
            .is_some();
        if !routable || alert.action.kind == ActionKind::Record {
            return false;
        }
        match alert.action.deliver {
            Some(deliver) => deliver,
            None => match alert.action.agent.as_deref() {
                Some(name) => self.runtime.as_ref().is_some_and(|runtime| {
                    runtime
                        .registry()
                        .get(name)
                        .is_some_and(|definition| definition.deliver)
                }),
                None => true,
            },
        }
    }

    /// Process everything queued, down to empty. Tests use this instead of
    /// sleeping on a spawned consumer; production calls [`Self::run`].
    pub async fn drain(&self) -> usize {
        let mut rx = self.take_rx();
        let mut handled = 0;
        while let Ok(work) = rx.try_recv() {
            self.handle(work).await;
            handled += 1;
        }
        self.put_rx(rx);
        handled
    }

    /// The single consumer, outside the tick, until the queue closes — which
    /// happens once every handle to the dispatcher is dropped.
    pub async fn run(self: Arc<Self>) {
        let Some(mut rx) = self.take_rx_opt() else {
            crate::warn_log!(
                "[trigger] a dispatch consumer is already running; not starting another"
            );
            return;
        };
        while let Some(work) = rx.recv().await {
            self.handle(work).await;
        }
    }

    fn take_rx(&self) -> mpsc::Receiver<Work> {
        self.take_rx_opt()
            .expect("the dispatch queue is already held by a consumer")
    }

    fn take_rx_opt(&self) -> Option<mpsc::Receiver<Work>> {
        self.rx
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .take()
    }

    fn put_rx(&self, rx: mpsc::Receiver<Work>) {
        *self
            .rx
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(rx);
    }

    async fn handle(&self, work: Work) {
        match work {
            Work::Book { fire, error } => self.book(&fire, 1, None, error),
            Work::Notice {
                dest,
                fires,
                overflow,
            } => {
                let merged = fires.len();
                let error = self.inject_notice(&dest, &fires).await;
                for fire in &fires {
                    self.book(fire, merged, None, error.clone());
                }
                for fire in &overflow {
                    self.book(
                        fire,
                        0,
                        None,
                        Some(format!(
                            "beyond the merge cap of {} for this destination and tick",
                            self.max_merge
                        )),
                    );
                }
            }
            Work::Run { fire } => self.run_agent(fire).await,
            Work::Alert { alert } => self.send_alert(&alert).await,
        }
    }

    /// Put a notice group into the originating conversation as one inbound
    /// input. Going in through the inbound path is what buys the main loop's
    /// full tool set, its per-session serial queue, outbound routing and
    /// `/stop`, without any of them being reimplemented here.
    async fn inject_notice(&self, dest: &str, fires: &[Fire]) -> Option<String> {
        if self.bus.inbound_tx().is_closed() {
            crate::warn_log!("[trigger] no inbound consumer; the notice for {dest} was dropped");
            return Some("no inbound consumer is running".to_string());
        }
        self.bus
            .publish_inbound(BusRequest::owner(
                dest,
                compose_notice(fires),
                TaskHook::new(dest),
            ))
            .await;
        None
    }

    async fn run_agent(&self, fire: Fire) {
        let Some(runtime) = self.runtime.clone() else {
            self.book(
                &fire,
                1,
                None,
                Some("no subagent runtime is wired".to_string()),
            );
            return;
        };
        let Some(agent) = fire
            .action
            .agent
            .as_deref()
            .map(str::trim)
            .filter(|agent| !agent.is_empty())
        else {
            self.book(&fire, 1, None, Some("action.agent is empty".to_string()));
            return;
        };
        let request = StartRequest {
            agent: agent.to_string(),
            task: render_fire(&fire),
            origin: fire.origin.clone(),
            // The action's own flag wins; the agent definition supplies the
            // default; a missing origin turns delivery into log-only, which the
            // runtime already decides.
            deliver: fire.action.deliver,
        };
        // A fire has no caller who could retry, so it waits for a free slot
        // instead of being answered with `busy`.
        match runtime.start(request, AcquireMode::WaitForSlot).await {
            Ok(run) => {
                if !run.delivers {
                    debug!(
                        "[trigger] rule {} ran agent {} with no delivery ({}: {})",
                        fire.id, run.agent, "result traced only", run.run_id
                    );
                }
                self.book(&fire, 1, Some(&run.run_id), None);
            }
            Err(reject) => self.book(&fire, 1, None, Some(describe_reject(&reject))),
        }
    }

    /// The one exception in this chain that bypasses the main loop: fixed text,
    /// nothing to summarize, no model call.
    async fn send_alert(&self, alert: &Alert) {
        let Some(dest) = crate::delivery::resolve_target(
            None,
            alert
                .origin
                .as_ref()
                .and_then(|origin| DeliveryTarget::from_session_id(&origin.session_id())),
            None,
        )
        .target()
        .map(|target| target.session_id())
        else {
            crate::warn_log!(
                "[trigger] alert for rule {} has no conversation to tell",
                alert.id
            );
            return;
        };
        if self.bus.outbound_tx().is_closed() {
            crate::warn_log!(
                "[trigger] no channel consumer; the alert for {} was dropped",
                alert.id
            );
            return;
        }
        self.bus
            .publish_outbound(BusResult {
                session_id: dest.clone(),
                run_id: format!("alert-{}", alert.id),
                content: alert_text(alert),
            })
            .await;
        crate::info!("[trigger] alert for rule {} sent to {dest}", alert.id);
    }

    /// Book a fire: one root span and one history line. The span is a root of
    /// its own, correlated by `trigger_id`, never a child of the tick — a run
    /// outlives the tick that queued it, and nesting that would produce a child
    /// span ending after its parent. A ledger that cannot be written is logged
    /// and skipped: it must not stop a fire.
    fn book(&self, fire: &Fire, merged: usize, run_id: Option<&str>, error: Option<String>) {
        let ok = error.is_none();
        let span = self.tracer.start_span("trigger.fire", None);
        span.set_attribute("trigger_id", json!(fire.id));
        span.set_attribute("trigger_name", json!(fire.name));
        span.set_attribute("action_kind", json!(fire.action.kind.as_str()));
        if let Some(agent) = &fire.action.agent {
            span.set_attribute("agent", json!(agent));
        }
        if let Some(run_id) = run_id {
            span.set_attribute("run_id", json!(run_id));
        }
        span.set_attribute("ok", json!(ok));
        span.set_attribute("merged", json!(merged));
        if let Some(error) = &error {
            span.set_status(SpanStatus::Error(error.clone()));
            crate::warn_log!("[trigger] rule {} could not act: {error}", fire.id);
        }
        let row = json!({
            "trigger_id": fire.id,
            "fired_at": fire.fired_at,
            "action_kind": fire.action.kind.as_str(),
            "ok": ok,
            "merged": merged,
            "detail": fire.detail,
        });
        if let Err(e) = append_history(&self.history_path, &row, self.history_lines) {
            crate::warn_log!("[trigger] history write failed: {e}");
        }
    }
}

fn describe_reject(reject: &StartReject) -> String {
    match reject {
        StartReject::UnknownAgent { available } => format!(
            "agent is no longer defined (known: {})",
            if available.is_empty() {
                "none".to_string()
            } else {
                available.join(", ")
            }
        ),
        StartReject::MissingTools { missing } => {
            format!("agent declares unavailable tools: {}", missing.join(", "))
        }
        StartReject::Busy { current, max } => {
            format!("concurrency gate is full ({current}/{max})")
        }
        StartReject::Internal(e) => e.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::io::Write as _;

    #[test]
    fn test_history_rows_append_in_order() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join(HISTORY_FILE);
        for n in 0..3 {
            append_history(&path, &json!({"trigger_id": "trg-a", "merged": n}), 100).unwrap();
        }
        let rows = read_history(&path).unwrap();
        assert_eq!(rows.len(), 3);
        assert_eq!(rows[2]["merged"], 2);
    }

    #[test]
    fn test_history_keeps_only_the_newest_generation() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join(HISTORY_FILE);
        for n in 0..12 {
            append_history(&path, &json!({"n": n}), 5).unwrap();
        }
        let rows = read_history(&path).unwrap();
        assert_eq!(rows.len(), 5, "the file must not outgrow its budget");
        assert_eq!(rows[0]["n"], 7);
        assert_eq!(rows[4]["n"], 11);
    }

    #[test]
    fn test_torn_line_is_skipped_not_fatal() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join(HISTORY_FILE);
        append_history(&path, &json!({"n": 1}), 100).unwrap();
        std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap()
            .write_all(b"{\"n\": 2, broken\n")
            .unwrap();
        let rows = read_history(&path).unwrap();
        assert_eq!(rows.len(), 1);
    }

    #[test]
    fn test_missing_history_reads_as_empty() {
        let tmp = tempfile::tempdir().unwrap();
        let rows = read_history(&tmp.path().join("none.jsonl")).unwrap();
        assert!(rows.is_empty());
    }

    #[test]
    fn test_history_carries_no_evidence_body() {
        // The ledger shape is a contract: ids, times, outcome, a summary line.
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join(HISTORY_FILE);
        append_history(
            &path,
            &json!({
                "trigger_id": "trg-a",
                "fired_at": 1,
                "action_kind": "record",
                "ok": true,
                "merged": 1,
                "detail": "two items still open",
            }),
            100,
        )
        .unwrap();
        let raw = std::fs::read_to_string(&path).unwrap();
        assert!(!raw.contains("evidence"), "{raw}");
    }

    // ── The queue and its consumer ──

    use crate::agent::{Origin, SubagentRuntime, ToolCatalog, default_tool_set};
    use crate::config::AgentConfig;
    use crate::memory::{MemoryService, SharedMemory};
    use crate::message_bus::MessageBusReceivers;
    use crate::provider::MockProvider;
    use crate::session::SessionManager;
    use crate::trigger::subscription::meta::TriggerAction;
    use std::time::Duration;

    const AT: i64 = 1_700_000_000_000;

    fn fire(id: &str, action: TriggerAction, origin: Option<Origin>) -> Fire {
        let kind = action.kind;
        Fire {
            id: id.to_string(),
            name: format!("rule {id}"),
            origin,
            action,
            detail: "two items still open".to_string(),
            evidence: vec![json!({"source_id": "src-0001", "text": "buy milk", "at": AT})],
            fired_at: AT,
            // The judgment layer sets this exactly this way; a fire that could
            // never reach anybody is booked, not queued for delivery.
            delivers: kind != ActionKind::Record,
        }
    }

    fn notifying(template: &str) -> TriggerAction {
        TriggerAction {
            kind: ActionKind::Notify,
            text_template: Some(template.to_string()),
            ..Default::default()
        }
    }

    fn origin() -> Origin {
        Origin::new("webui", "chat-1")
    }

    struct Harness {
        _tmp: tempfile::TempDir,
        dispatch: Arc<Dispatcher>,
        consumers: MessageBusReceivers,
    }

    impl Harness {
        /// A dispatcher with no subagent runtime: enough for everything but an
        /// `agent` action.
        fn new(max_queue: usize, max_merge: usize) -> Self {
            let tmp = tempfile::tempdir().unwrap();
            let (bus, consumers) = MessageBus::new();
            let dispatch = Dispatcher::new(
                None,
                bus,
                tmp.path().join(HISTORY_FILE),
                100,
                max_queue,
                max_merge,
                Tracer::new(vec![]),
            );
            Self {
                _tmp: tmp,
                dispatch,
                consumers,
            }
        }

        /// A dispatcher wired to a real subagent runtime over the same bus, so
        /// an `agent` action can be followed all the way to an inbound result.
        fn with_runtime(max_concurrent: usize) -> Self {
            let tmp = tempfile::tempdir().unwrap();
            let workspace = tmp.path().join("workspace");
            std::fs::create_dir_all(workspace.join("agents")).unwrap();
            std::fs::write(
                workspace.join("agents").join("echo.md"),
                "---\nname: echo\ndescription: test agent\ndeliver: true\ntimeout_secs: 30\n---\n\nYou are a test agent.\n",
            )
            .unwrap();
            let (bus, consumers) = MessageBus::new();
            let session_manager = Arc::new(tokio::sync::Mutex::new(
                SessionManager::new(tmp.path().join("sessions")).unwrap(),
            ));
            let memory: SharedMemory =
                Arc::new(tokio::sync::Mutex::new(MemoryService::new(&workspace)));
            let mut catalog = ToolCatalog::new();
            catalog.register_builtin("file_reader", &workspace);
            let runtime = Arc::new(SubagentRuntime::new(
                Arc::new(catalog),
                workspace,
                Arc::new(MockProvider::new()),
                session_manager,
                memory,
                AgentConfig::default(),
                default_tool_set(&[]),
                512,
                max_concurrent,
                None,
                bus.clone(),
            ));
            let dispatch = Dispatcher::new(
                Some(runtime),
                bus,
                tmp.path().join(HISTORY_FILE),
                100,
                20,
                3,
                Tracer::new(vec![]),
            );
            Self {
                _tmp: tmp,
                dispatch,
                consumers,
            }
        }

        fn rows(&self) -> Vec<Value> {
            read_history(&self._tmp.path().join(HISTORY_FILE)).unwrap()
        }

        /// The one input the consumer handed to the main loop, waiting for none
        /// rather than for a timeout that a slow machine could beat.
        async fn inbound(&mut self) -> Option<crate::message_bus::BusRequest> {
            tokio::time::timeout(Duration::from_secs(5), self.consumers.inbound.recv())
                .await
                .ok()
                .flatten()
        }

        async fn outbound(&mut self) -> Option<BusResult> {
            tokio::time::timeout(Duration::from_secs(5), self.consumers.outbound.recv())
                .await
                .ok()
                .flatten()
        }
    }

    #[tokio::test]
    async fn test_a_full_queue_drops_the_newest_fire_and_books_it_as_failed() {
        let h = Harness::new(2, 3);
        assert_eq!(
            h.dispatch
                .enqueue_fires(vec![fire("trg-a", TriggerAction::record(), None)]),
            1
        );
        assert_eq!(
            h.dispatch
                .enqueue_fires(vec![fire("trg-b", TriggerAction::record(), None)]),
            1
        );
        // The queue is at capacity and nothing has been consumed: the newest
        // fire is dropped, and a late reminder is not queued up for later.
        assert_eq!(
            h.dispatch
                .enqueue_fires(vec![fire("trg-c", TriggerAction::record(), None)]),
            0
        );
        assert_eq!(h.dispatch.dropped_count(), 1);

        // The two accepted fires are still pending; the dropped one is already
        // in the ledger, marked as not having worked out.
        let rows = h.rows();
        assert_eq!(rows.len(), 1, "only the dropped fire is booked so far");
        assert_eq!(rows[0]["trigger_id"], json!("trg-c"));
        assert_eq!(rows[0]["ok"], json!(false));
        assert_eq!(h.dispatch.drain().await, 2, "and the queue kept its order");
    }

    #[tokio::test]
    async fn test_notify_fires_from_one_tick_and_one_origin_become_one_input() {
        let mut h = Harness::new(20, 3);
        let fires = vec![
            fire("trg-a", notifying("first: {{detail}}"), Some(origin())),
            fire("trg-b", notifying("second: {{evidence}}"), Some(origin())),
            fire("trg-c", notifying("third"), Some(origin())),
        ];
        assert_eq!(h.dispatch.enqueue_fires(fires), 1, "one merged item");
        assert_eq!(h.dispatch.drain().await, 1);

        let inbound = h.inbound().await.expect("one input, not three");
        assert_eq!(inbound.session_id, "webui:chat-1");
        assert!(inbound.content.contains("merged=3"), "{}", inbound.content);
        assert!(inbound.content.contains("first: two items"));
        assert!(inbound.content.contains("second:"));
        assert!(inbound.content.contains("- [src-0001 @"), "evidence line");
        assert!(h.consumers.inbound.try_recv().is_err(), "exactly one");

        let rows = h.rows();
        assert_eq!(rows.len(), 3, "every fire is booked on its own");
        assert!(rows.iter().all(|row| row["merged"] == json!(3)), "{rows:?}");
        assert!(rows.iter().all(|row| row["ok"] == json!(true)), "{rows:?}");
    }

    #[tokio::test]
    async fn test_fires_from_different_conversations_are_never_merged() {
        let mut h = Harness::new(20, 3);
        let fires = vec![
            fire("trg-a", notifying("one"), Some(origin())),
            fire(
                "trg-b",
                notifying("two"),
                Some(Origin::new("webui", "chat-2")),
            ),
        ];
        assert_eq!(h.dispatch.enqueue_fires(fires), 2);
        assert_eq!(h.dispatch.drain().await, 2);

        let first = h.inbound().await.unwrap();
        let second = h.inbound().await.unwrap();
        assert_eq!(first.session_id, "webui:chat-1");
        assert_eq!(
            second.session_id, "webui:chat-2",
            "one user's notice must not travel to another"
        );
    }

    #[tokio::test]
    async fn test_notices_beyond_the_merge_cap_are_dropped_and_booked() {
        let mut h = Harness::new(20, 2);
        let fires = vec![
            fire("trg-a", notifying("one"), Some(origin())),
            fire("trg-b", notifying("two"), Some(origin())),
            fire("trg-c", notifying("three"), Some(origin())),
        ];
        assert_eq!(h.dispatch.enqueue_fires(fires), 1);
        assert_eq!(h.dispatch.drain().await, 1);

        let inbound = h.inbound().await.unwrap();
        assert!(inbound.content.contains("merged=2"), "{}", inbound.content);
        assert!(!inbound.content.contains("three"), "the cap is a cap");

        let rows = h.rows();
        assert_eq!(rows.len(), 3);
        assert_eq!(rows[2]["trigger_id"], json!("trg-c"));
        assert_eq!(
            rows[2]["ok"],
            json!(false),
            "a dropped fire says so in the ledger: {rows:?}"
        );
    }

    #[tokio::test]
    async fn test_a_notify_with_nowhere_to_go_fails_loudly_and_goes_nowhere() {
        let mut h = Harness::new(20, 3);
        assert_eq!(
            h.dispatch
                .enqueue_fires(vec![fire("trg-a", notifying("hello"), None)]),
            1
        );
        assert_eq!(h.dispatch.drain().await, 1);

        assert!(
            h.consumers.inbound.try_recv().is_err(),
            "no fallback session may receive it"
        );
        assert!(
            h.consumers.outbound.try_recv().is_err(),
            "and it must not be pushed to a channel either"
        );
        let rows = h.rows();
        assert_eq!(rows.len(), 1, "the fire is still booked");
        assert_eq!(rows[0]["ok"], json!(false));
    }

    #[tokio::test]
    async fn test_an_explicit_destination_wins_over_the_origin() {
        let mut h = Harness::new(20, 3);
        let action = TriggerAction {
            channel: Some("cli".to_string()),
            to: Some("chat-9".to_string()),
            ..notifying("over there")
        };
        assert_eq!(
            h.dispatch
                .enqueue_fires(vec![fire("trg-a", action, Some(origin()))]),
            1
        );
        assert_eq!(h.dispatch.drain().await, 1);
        assert_eq!(h.inbound().await.unwrap().session_id, "cli:chat-9");
    }

    #[tokio::test]
    async fn test_a_record_fire_is_booked_and_sends_nothing() {
        let mut h = Harness::new(20, 3);
        assert_eq!(
            h.dispatch
                .enqueue_fires(vec![fire("trg-a", TriggerAction::record(), Some(origin()))]),
            1
        );
        assert_eq!(h.dispatch.drain().await, 1);

        assert!(h.consumers.inbound.try_recv().is_err());
        let rows = h.rows();
        assert_eq!(rows[0]["action_kind"], json!("record"));
        assert_eq!(rows[0]["ok"], json!(true), "booking is the whole job");
        let mut keys: Vec<&str> = rows[0]
            .as_object()
            .unwrap()
            .keys()
            .map(|k| k.as_str())
            .collect();
        keys.sort_unstable();
        assert_eq!(
            keys,
            vec![
                "action_kind",
                "detail",
                "fired_at",
                "merged",
                "ok",
                "trigger_id"
            ],
            "the ledger line has exactly the agreed fields, and no error text"
        );
    }

    #[tokio::test]
    async fn test_an_agent_fire_runs_and_the_result_comes_back_to_the_origin() {
        let mut h = Harness::with_runtime(3);
        let action = TriggerAction {
            agent: Some("echo".to_string()),
            task_template: Some("look into: {{detail}}".to_string()),
            ..Default::default()
        };
        assert_eq!(
            h.dispatch
                .enqueue_fires(vec![fire("trg-a", action, Some(origin()))]),
            1
        );
        assert_eq!(h.dispatch.drain().await, 1);

        let rows = h.rows();
        assert_eq!(rows[0]["action_kind"], json!("agent"));
        assert_eq!(rows[0]["ok"], json!(true), "{}", rows[0]);

        let inbound = h.inbound().await.expect("the run's result");
        assert_eq!(inbound.session_id, "webui:chat-1");
        assert!(
            inbound.content.starts_with("[Subagent result] agent=echo"),
            "{}",
            inbound.content
        );
        assert!(inbound.content.contains("Summarize this for the user"));
    }

    #[tokio::test]
    async fn test_an_agent_fire_waits_for_a_free_slot_instead_of_being_refused() {
        // A fire has no caller who could retry it, so `busy` is not an answer.
        let h = Harness::with_runtime(1);
        for n in 0..3 {
            let action = TriggerAction {
                agent: Some("echo".to_string()),
                task_template: Some(format!("task {n}")),
                ..Default::default()
            };
            assert_eq!(
                h.dispatch.enqueue_fires(vec![fire("trg-a", action, None)]),
                1
            );
        }
        assert_eq!(h.dispatch.drain().await, 3);

        let rows = h.rows();
        assert_eq!(rows.len(), 3);
        assert!(
            rows.iter().all(|row| row["ok"] == json!(true)),
            "waiting for a slot is not a failure: {rows:?}"
        );
    }

    #[tokio::test]
    async fn test_an_agent_fire_without_a_runtime_is_booked_as_failed() {
        let h = Harness::new(20, 3);
        let action = TriggerAction {
            agent: Some("echo".to_string()),
            task_template: Some("task".to_string()),
            ..Default::default()
        };
        h.dispatch
            .enqueue_fires(vec![fire("trg-a", action, Some(origin()))]);
        assert_eq!(h.dispatch.drain().await, 1);

        let rows = h.rows();
        assert_eq!(rows[0]["ok"], json!(false), "{}", rows[0]);
    }

    #[tokio::test]
    async fn test_a_fire_with_no_origin_still_runs_and_only_logs_its_result() {
        let mut h = Harness::with_runtime(3);
        let action = TriggerAction {
            agent: Some("echo".to_string()),
            task_template: Some("task".to_string()),
            ..Default::default()
        };
        h.dispatch.enqueue_fires(vec![fire("trg-a", action, None)]);
        assert_eq!(h.dispatch.drain().await, 1);
        assert_eq!(
            h.rows()[0]["ok"],
            json!(true),
            "it ran; there is just nobody to tell"
        );

        tokio::time::sleep(Duration::from_millis(300)).await;
        assert!(
            h.consumers.inbound.try_recv().is_err(),
            "no origin means no delivery, and no invented destination"
        );
    }

    #[tokio::test]
    async fn test_an_explicit_deliver_false_overrides_the_agent_default() {
        let mut h = Harness::with_runtime(3);
        let action = TriggerAction {
            agent: Some("echo".to_string()),
            task_template: Some("task".to_string()),
            deliver: Some(false),
            ..Default::default()
        };
        h.dispatch
            .enqueue_fires(vec![fire("trg-a", action, Some(origin()))]);
        assert_eq!(h.dispatch.drain().await, 1);

        tokio::time::sleep(Duration::from_millis(300)).await;
        assert!(
            h.consumers.inbound.try_recv().is_err(),
            "the rule said keep quiet, and the agent definition cannot overrule it"
        );
    }

    #[tokio::test]
    async fn test_an_alert_goes_straight_to_the_channel_and_not_through_a_model() {
        let mut h = Harness::new(20, 3);
        let alert = Alert {
            id: "trg-a".to_string(),
            name: "daily todo".to_string(),
            origin: Some(origin()),
            action: notifying("{{detail}}"),
            error: "judge threw: boom".to_string(),
            failures: 5,
        };
        assert_eq!(h.dispatch.enqueue_alerts(vec![alert]), 1);
        assert_eq!(h.dispatch.drain().await, 1);

        let outbound = h.outbound().await.expect("the alert");
        assert_eq!(outbound.session_id, "webui:chat-1");
        assert!(outbound.content.starts_with("[Trigger alert] id=trg-a"));
        assert!(outbound.content.contains("boom"));
        assert!(outbound.content.contains("until it recovers"));
        assert!(
            h.consumers.inbound.try_recv().is_err(),
            "an alert is not a result to be summarized"
        );
        assert!(h.rows().is_empty(), "an alert is not a fire");
    }

    #[tokio::test]
    async fn test_an_alert_with_no_conversation_to_tell_is_dropped_quietly() {
        let mut h = Harness::new(20, 3);
        let alert = Alert {
            id: "trg-a".to_string(),
            name: "orphan".to_string(),
            origin: None,
            action: notifying("{{detail}}"),
            error: "boom".to_string(),
            failures: 5,
        };
        h.dispatch.enqueue_alerts(vec![alert]);
        assert_eq!(h.dispatch.drain().await, 1);
        assert!(h.consumers.outbound.try_recv().is_err());
    }

    #[test]
    fn test_an_alert_is_only_raised_for_a_rule_somebody_would_hear_from() {
        let h = Harness::with_runtime(1);
        let alert = Alert {
            id: "trg-a".to_string(),
            name: "daily todo".to_string(),
            origin: Some(origin()),
            action: TriggerAction::record(),
            error: "boom".to_string(),
            failures: 5,
        };
        assert!(
            !h.dispatch.is_alertable(&alert),
            "a rule that never reaches a user has no audience for its failures"
        );
        let speaking = Alert {
            action: notifying("{{detail}}"),
            ..alert.clone()
        };
        assert!(h.dispatch.is_alertable(&speaking));
        let opted_out = Alert {
            action: TriggerAction {
                deliver: Some(false),
                ..notifying("{{detail}}")
            },
            ..alert
        };
        assert!(
            !h.dispatch.is_alertable(&opted_out),
            "the rule's own flag outranks the default"
        );
    }

    #[test]
    fn test_a_notice_reads_as_a_notice_and_not_as_the_user() {
        let fires = vec![
            fire("trg-a", notifying("alpha"), Some(origin())),
            fire("trg-b", notifying("beta"), Some(origin())),
        ];
        let text = compose_notice(&fires);
        assert!(text.starts_with("[Trigger notice] id=trg-a,trg-b merged=2 at="));
        assert!(text.contains("alpha\n\n---\n\nbeta"), "{text}");
        assert!(text.contains("not a message from the user"));

        // No evidence still reads as an explicit absence rather than a blank.
        let mut bare = fire("trg-c", notifying("{{evidence}}"), Some(origin()));
        bare.evidence = Vec::new();
        let text = compose_notice(&[bare]);
        assert!(text.contains("(no evidence)"), "{text}");
    }
}
