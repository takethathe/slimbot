//! End-to-end tests for the delivery seam's combination behaviour.
//!
//! The real `Dispatcher` turns real `Fire`s into work over a real `MessageBus`;
//! nothing here re-implements a caller's decision. Assertions read the receivers
//! and the on-disk history, so a change in "who gets told" cannot pass unnoticed.
//! See `docs/adr/0018` for the best-effort, no-backfill posture.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde_json::{Value, json};
use tokio::sync::mpsc;

use slimbot::agent::Origin;
use slimbot::delivery::{
    DeliverySource, DeliveryTarget, OwnerPrimaryStore, Resolution, cron_target, heartbeat_target,
    resolve_target,
};
use slimbot::trigger::dispatch::{Dispatcher, HISTORY_FILE, read_history};
use slimbot::trigger::subscription::Fire;
use slimbot::trigger::subscription::meta::{ActionKind, TriggerAction};
use slimbot::{MessageBus, MessageBusReceivers, Tracer, TurnKind};

const FIRED_AT: i64 = 1_700_000_000;

/// One process worth of delivery wiring over a temp data dir.
struct Harness {
    _tmp: Arc<tempfile::TempDir>,
    data_dir: PathBuf,
    history: PathBuf,
    _bus: Arc<MessageBus>,
    receivers: MessageBusReceivers,
    dispatch: Arc<Dispatcher>,
}

impl Harness {
    /// Brand new wiring over the same files: what a restart looks like. Only
    /// what was written to disk carries over; the queue and the bus do not.
    fn wire(tmp: Arc<tempfile::TempDir>, max_queue: usize) -> Self {
        let (bus, receivers) = MessageBus::new();
        let history = tmp.path().join("triggers").join(HISTORY_FILE);
        let dispatch = Dispatcher::new(
            None,
            bus.clone(),
            history.clone(),
            100,
            max_queue,
            4,
            Tracer::new(Vec::new()),
        );
        Self {
            _tmp: tmp.clone(),
            data_dir: tmp.path().to_path_buf(),
            history,
            _bus: bus,
            receivers,
            dispatch,
        }
    }

    fn new() -> Self {
        Self::wire(Arc::new(tempfile::tempdir().unwrap()), 16)
    }

    fn rows(&self) -> Vec<Value> {
        read_history(&self.history).unwrap()
    }
}

fn notify(text: &str) -> TriggerAction {
    TriggerAction {
        kind: ActionKind::Notify,
        text_template: Some(text.to_string()),
        ..Default::default()
    }
}

fn fire(id: &str, delivers: bool, origin: Option<Origin>, action: TriggerAction) -> Fire {
    Fire {
        id: id.to_string(),
        name: id.to_string(),
        origin,
        action,
        detail: format!("{id} detail"),
        evidence: vec![json!({"title": id})],
        fired_at: FIRED_AT,
        delivers,
    }
}

fn owner_target() -> DeliveryTarget {
    DeliveryTarget::new("webui", "main").expect("test owner target")
}

fn row_for<'a>(rows: &'a [Value], id: &str) -> &'a Value {
    rows.iter()
        .find(|row| row["trigger_id"] == id)
        .unwrap_or_else(|| panic!("no history row for {id}: {rows:?}"))
}

fn assert_empty<T>(receiver: &mut mpsc::Receiver<T>) {
    assert!(
        matches!(receiver.try_recv(), Err(mpsc::error::TryRecvError::Empty)),
        "a message went out when nothing should have been delivered"
    );
}

/// Every file under the data dir, concatenated: a boundary assertion ("no
/// undelivered message was persisted") rather than a shape assertion.
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

/// Structure locks scan code, not prose: a comment that quotes a marker must
/// neither keep a deleted guard green nor make a live one look duplicated.
fn code_only(src: &str) -> String {
    src.lines()
        .filter(|line| !line.trim_start().starts_with("//"))
        .collect::<Vec<_>>()
        .join("\n")
}

/// The block opened by a `match` arm: everything from the arm's first `{` up to
/// its brace-balanced `}`. Counted rather than "stop at the first `}`" so a
/// nested block inside the arm cannot end the slice early and hide a later
/// `publish_outbound`.
fn arm_block(arm: &str) -> String {
    let mut depth = 0usize;
    let mut opened = false;
    for (idx, ch) in arm.char_indices() {
        match ch {
            '{' => {
                depth += 1;
                opened = true;
            }
            '}' => {
                depth = depth.saturating_sub(1);
                if opened && depth == 0 {
                    return arm[..=idx].to_string();
                }
            }
            _ => {}
        }
    }
    arm.to_string()
}

/// The three outcomes in one tick: a policy silence, a delivered notice and an
/// unreachable origin. They must not collapse into "nothing was sent".
#[tokio::test]
async fn test_delivery_combination_of_silent_delivered_and_nowhere_stays_distinct() {
    let mut h = Harness::new();
    let reachable = Some(Origin::new("webui", "chat-1"));
    let unreachable = Some(Origin::new("webui", ""));

    let accepted = h.dispatch.enqueue_fires(vec![
        // Silent delivery: the rule chose not to speak, even though it could.
        fire("trg-silent", false, reachable.clone(), notify("{{detail}}")),
        // Delivered: the notice reaches the conversation the rule was made in.
        fire("trg-delivered", true, reachable, notify("{{detail}}")),
        // Nowhere: an origin with an empty half is not deliverable.
        fire("trg-nowhere", true, unreachable, notify("{{detail}}")),
    ]);
    assert_eq!(accepted, 3, "all three fires must enter the queue");
    assert_eq!(h.dispatch.drain().await, 3);

    let notice = h
        .receivers
        .inbound
        .try_recv()
        .expect("the deliverable fire must inject one notice");
    assert_eq!(notice.session_id, "webui:chat-1");
    assert!(notice.content.contains("trg-delivered"));
    assert!(notice.content.contains("trg-delivered detail"));
    assert_empty(&mut h.receivers.inbound);
    assert_empty(&mut h.receivers.outbound);

    let rows = h.rows();
    assert_eq!(rows.len(), 3, "each fire is booked exactly once: {rows:?}");
    assert_eq!(row_for(&rows, "trg-silent")["ok"], true);
    assert_eq!(row_for(&rows, "trg-delivered")["ok"], true);
    assert_eq!(row_for(&rows, "trg-nowhere")["ok"], false);
    // A policy silence is a successful act; a missing destination is a failed
    // one. Sharing one "silent" branch would flatten this difference.
    assert_ne!(
        row_for(&rows, "trg-silent")["ok"],
        row_for(&rows, "trg-nowhere")["ok"]
    );
}

/// An explicit target wins over the origin. An established owner primary does
/// not become a fallback leg for a trigger whose origin is unreachable.
#[tokio::test]
async fn test_delivery_explicit_target_with_owner_primary_established_wins_over_origin() {
    let mut h = Harness::new();
    OwnerPrimaryStore::load(&h.data_dir).record_first(&owner_target());

    let explicit = TriggerAction {
        channel: Some("feishu".to_string()),
        to: Some("oc_explicit".to_string()),
        ..notify("{{detail}}")
    };
    let unreachable = Some(Origin::new("webui", ""));
    assert_eq!(
        h.dispatch.enqueue_fires(vec![
            fire(
                "trg-explicit",
                true,
                Some(Origin::new("webui", "chat-1")),
                explicit,
            ),
            fire("trg-unreachable", true, unreachable, notify("{{detail}}")),
        ]),
        2
    );
    assert_eq!(h.dispatch.drain().await, 2);

    let notice = h
        .receivers
        .inbound
        .try_recv()
        .expect("the explicit target must be injected");
    assert_eq!(notice.session_id, "feishu:oc_explicit");
    assert_empty(&mut h.receivers.inbound);
    assert_empty(&mut h.receivers.outbound);

    let rows = h.rows();
    assert_eq!(row_for(&rows, "trg-explicit")["ok"], true);
    assert_eq!(row_for(&rows, "trg-unreachable")["ok"], false);

    // The owner primary file still holds one destination, not a ledger of what
    // was or was not delivered.
    let body = std::fs::read_to_string(h.data_dir.join(OwnerPrimaryStore::FILE_NAME)).unwrap();
    let stored: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(stored["channel"], "webui");
    assert_eq!(stored["chat_id"], "main");
    assert_eq!(stored.as_object().unwrap().len(), 2);
}

/// A restart recovers *who* the owner is, never *what* was left undelivered.
///
/// Overflowing a one-slot queue leaves a real `ok=false` history line on disk:
/// that is a genuine undelivered record a backfill could later pick up. A
/// restart must not re-queue it, resend it, or rewrite its verdict.
#[tokio::test]
async fn test_delivery_restart_after_undelivered_fires_backfills_nothing() {
    let tmp = Arc::new(tempfile::tempdir().unwrap());
    let before = Harness::wire(tmp.clone(), 1);
    OwnerPrimaryStore::load(&before.data_dir).record_first(&owner_target());

    // Accepted into the queue, never drained: it dies with the process and is
    // never written to disk at all.
    assert_eq!(
        before.dispatch.enqueue_fires(vec![fire(
            "trg-in-flight",
            true,
            Some(Origin::new("webui", "chat-1")),
            notify("{{detail}}"),
        )]),
        1
    );
    // The next fire overflows the queue. `note_dropped` books it as failed, so
    // the on-disk record of an undelivered message is real, not imagined.
    assert_eq!(
        before.dispatch.enqueue_fires(vec![fire(
            "trg-undelivered",
            true,
            Some(Origin::new("webui", "chat-1")),
            notify("{{detail}}"),
        )]),
        0
    );
    assert_eq!(before.dispatch.dropped_count(), 1);
    let rows_before = before.rows();
    assert_eq!(rows_before.len(), 1);
    assert_eq!(rows_before[0]["trigger_id"], "trg-undelivered");
    assert_eq!(rows_before[0]["ok"], false);
    drop(before);

    let mut after = Harness::wire(tmp, 16);
    assert_eq!(
        after.dispatch.drain().await,
        0,
        "a restart must not re-queue the fires it died holding"
    );
    assert_empty(&mut after.receivers.inbound);
    assert_empty(&mut after.receivers.outbound);
    assert_eq!(
        after.rows(),
        rows_before,
        "the undelivered record must be neither resent nor rewritten"
    );
    assert!(
        !all_text_on_disk(&after.data_dir).contains("trg-in-flight"),
        "an in-flight message must not be persisted for a later backfill"
    );

    // The one thing that does survive is the owner's address.
    assert_eq!(
        OwnerPrimaryStore::load(&after.data_dir).current(),
        Some(owner_target())
    );
    assert_eq!(
        cron_target(None, None, Some(owner_target())),
        Resolution::Deliver(owner_target()),
        "a new proactive message after the restart still has an audience"
    );
}

/// Without an attended turn no source can invent an audience: cron, heartbeat
/// and the provider alert all answer `Nowhere` while the owner is unknown.
#[test]
fn test_delivery_without_attended_turn_all_sources_resolve_nowhere() {
    let tmp = tempfile::tempdir().unwrap();
    let store = OwnerPrimaryStore::load(tmp.path());
    assert_eq!(store.current(), None);
    assert!(
        [
            TurnKind::Cron,
            TurnKind::Heartbeat,
            TurnKind::Trigger,
            TurnKind::Subagent,
            TurnKind::System
        ]
        .iter()
        .all(|kind| !kind.is_attended()),
        "only a user turn can establish the owner primary channel"
    );
    assert!(TurnKind::User.is_attended());

    let owner = store.current();
    assert_eq!(cron_target(None, None, owner.clone()), Resolution::Nowhere);
    assert_eq!(
        heartbeat_target(None, None, owner.clone()),
        Resolution::Nowhere
    );
    assert_eq!(
        resolve_target(DeliverySource::OwnerAlert, owner.clone()),
        Resolution::Nowhere
    );
    assert_eq!(
        resolve_target(
            DeliverySource::Trigger {
                channel: None,
                to: None,
                origin: Some(("webui", "chat-1")),
            },
            owner,
        ),
        Resolution::Deliver(DeliveryTarget::new("webui", "chat-1").unwrap()),
        "a trigger with a reachable origin still delivers without an owner"
    );
}

/// The five kinds of silence stay at their callers. A `debug!` line is only a
/// log, never a control-flow gate, and the seam has no unified silence flag.
#[test]
fn test_delivery_silent_flags_stay_at_each_caller() {
    let gateway = code_only(include_str!("../src/gateway.rs"));
    assert_eq!(
        gateway
            .matches("if job_clone.payload.deliver && !result.message_sent")
            .count(),
        1,
        "cron's own deliver flag and the messageSent gate stay together"
    );
    assert_eq!(
        code_only(include_str!("../src/agent/runtime.rs"))
            .matches("if !plan.deliver")
            .count(),
        1,
        "the subagent decides its own silence"
    );
    assert_eq!(
        code_only(include_str!("../src/trigger/dispatch.rs"))
            .matches("if !fire.delivers")
            .count(),
        1,
        "the trigger dispatcher honours the judgment's answer"
    );
    let delivery = code_only(include_str!("../src/delivery.rs"));
    assert!(
        !delivery.contains("silent: bool") && !delivery.contains("silent_flag"),
        "the seam must not grow a unified silence flag"
    );
    let debug_macro = include_str!("../src/macros.rs")
        .split("macro_rules! debug")
        .nth(1)
        .expect("the debug macro")
        .split("#[macro_export]")
        .next()
        .unwrap();
    assert!(
        !debug_macro.contains("return")
            && !debug_macro.contains("continue")
            && !debug_macro.contains("break"),
        "a debug log must not decide whether a result is delivered"
    );
}

/// `message_sent` keeps one writer and two readers; the owner primary is
/// established at exactly one attended-only gate. Ticket 06's lock is the
/// baseline; this adds the gate that an unattended turn must not pass.
#[test]
fn test_delivery_message_sent_and_owner_primary_gates_keep_their_shape() {
    let runner = code_only(include_str!("../src/runner/mod.rs"));
    assert_eq!(runner.matches("message_sent =").count(), 1);
    let agent_loop = code_only(include_str!("../src/agent_loop.rs"));
    assert_eq!(agent_loop.matches("result.message_sent").count(), 1);
    let gateway = code_only(include_str!("../src/gateway.rs"));
    assert_eq!(gateway.matches("!result.message_sent").count(), 1);
    assert_eq!(
        agent_loop.matches("record_first(").count(),
        1,
        "one place may establish the owner primary channel"
    );
    assert_eq!(
        agent_loop.matches("is_attended()").count(),
        1,
        "that place is gated on an attended turn"
    );
    assert_eq!(
        code_only(include_str!("../src/owner_alert.rs"))
            .matches("if !dedup.try_alert(&provider_alert_key(err), ALERT_DEDUP_WINDOW)")
            .count(),
        1,
        "the owner alert still merges a streak on its own window"
    );
}

/// `Nowhere` degrades to a log line at each caller. No branch publishes an
/// outbound result, writes a pending-delivery file, or schedules a retry.
#[test]
fn test_delivery_nowhere_branches_only_log_and_never_publish() {
    let gateway = code_only(include_str!("../src/gateway.rs"));
    let arms: Vec<String> = gateway
        .split("Resolution::Nowhere =>")
        .skip(1)
        .map(arm_block)
        .collect();
    assert_eq!(arms.len(), 2, "cron and heartbeat each own one Nowhere arm");
    for arm in &arms {
        assert!(arm.contains("warn_log!"), "a Nowhere arm must log: {arm}");
        assert!(
            !arm.contains("publish_outbound") && !arm.contains("write_file_atomic"),
            "a Nowhere arm must not send or persist: {arm}"
        );
    }
    assert_eq!(
        gateway.matches("publish_outbound").count(),
        2,
        "only the two Deliver arms publish"
    );
    assert_eq!(
        gateway.matches("warn_log!").count(),
        2,
        "no other gateway branch degrades to log-only"
    );
    assert_eq!(
        code_only(include_str!("../src/trigger/dispatch.rs"))
            .matches("publish_outbound")
            .count(),
        1,
        "only the trigger alert's Deliver path publishes"
    );
    assert_eq!(
        code_only(include_str!("../src/owner_alert.rs"))
            .matches(".send(BusResult")
            .count(),
        1,
        "only the resolved owner leg sends"
    );
    assert_eq!(
        code_only(include_str!("../src/delivery.rs"))
            .matches("write_file_atomic(")
            .count(),
        1,
        "the owner-primary address is the only thing the delivery layer writes; \
         there is no outbox or pending-delivery ledger"
    );
}
