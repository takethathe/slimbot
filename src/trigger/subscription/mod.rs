//! Trigger definitions and judgment: the rule half of the trigger layer.
//!
//! One rule is a directory holding what the model wrote (`meta.json` plus
//! `condition.js`) and what only the host writes (`state.json`). A tick freezes
//! the source snapshots, asks each due rule whether it fires, and feeds the
//! answer into an edge state machine. Acting on a fire is the dispatch layer's
//! job and happens outside the tick.

pub mod evidence;
pub mod host;
pub mod judge;
pub mod meta;
pub mod state;
pub mod tool;

use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use serde_json::{Value, json};

use crate::agent::Origin;
use crate::codemode::{
    ENTRY_JUDGE, HostEnv, Limits, Redactor, RunRequest, ScriptError, execute_async,
};
use crate::cron::CronSchedule;
use crate::debug;
use crate::warn_log;

use crate::trigger::source::{Failure, TickFrame, meta::now_ms};
use evidence::{Judgment, clamp_verdict};
use judge::{Edge, cadence_ticks, rise_is_due};
use meta::{ActionKind, LoadedTrigger, TriggerAction, TriggerMeta};
use state::TriggerState;

/// Consecutive judgment failures after which a rule is judged more slowly.
pub const DEGRADED_AFTER_FAILURES: u32 = 3;
/// Retry cadence for a degraded rule, in ticks.
pub const DEGRADED_EVERY_N_TICKS: u64 = 5;
/// Consecutive failures that raise exactly one alert (`alerted` dedupes it).
pub const ALERT_AFTER_FAILURES: u32 = 5;

/// A fire that needs acting on. The tick only produces these; execution and
/// delivery happen outside it.
#[derive(Debug, Clone)]
pub struct Fire {
    pub id: String,
    pub name: String,
    pub origin: Option<Origin>,
    /// Kept whole so dispatch never has to re-read disk for what to do.
    pub action: TriggerAction,
    pub detail: String,
    pub evidence: Vec<Value>,
    pub fired_at: i64,
    /// `false` for `record` rules: the fire is booked and nothing is sent.
    pub delivers: bool,
}

/// An operations signal, not a result summary: fixed text, nothing to
/// summarize, and the one delivery in the whole chain that skips the main loop.
#[derive(Debug, Clone)]
pub struct Alert {
    pub id: String,
    pub name: String,
    pub origin: Option<Origin>,
    /// Kept so dispatch can answer "would anybody hear about this fire at all?"
    /// without going back to disk.
    pub action: TriggerAction,
    pub error: String,
    pub failures: u32,
}

/// What one judgment round did.
#[derive(Debug, Default)]
pub struct JudgeReport {
    pub judged: usize,
    pub fired: Vec<Fire>,
    pub failures: Vec<Failure>,
    pub warnings: Vec<String>,
    pub alerts: Vec<Alert>,
}

/// A rule copied out of the lock, fixed for the whole judgment. No lock is ever
/// held across a script run.
#[derive(Debug, Clone)]
struct Candidate {
    meta: TriggerMeta,
    script: Option<String>,
    state: TriggerState,
    problem: Option<String>,
}

struct Runtime {
    meta: TriggerMeta,
    script: Option<String>,
    state: TriggerState,
    /// A disk-level problem (missing meta, unparsable definition).
    problem: Option<String>,
}

impl Runtime {
    fn new(loaded: LoadedTrigger) -> Self {
        let (mut state, reset_note) = TriggerState::load(&loaded.dir);
        let note = loaded.problem.or(reset_note);
        if let Some(note) = &note {
            warn_log!("[trigger] rule {}: {note}", loaded.meta.id);
        }
        // A restart must not fire a periodic rule whose slot passed while the
        // process was down, so the horizon is rebuilt from now.
        rebuild_horizon(&loaded.meta, &mut state, &loaded.dir);
        Self {
            meta: loaded.meta,
            script: loaded.script,
            state,
            problem: note,
        }
    }

    /// Judgment cadence: every tick, or slowed while the script keeps failing.
    fn every_n_ticks(&self) -> u64 {
        cadence_ticks(
            DEGRADED_AFTER_FAILURES,
            DEGRADED_EVERY_N_TICKS,
            self.state.consecutive_failures,
        )
    }

    fn candidate(&self) -> Candidate {
        Candidate {
            meta: self.meta.clone(),
            script: self.script.clone(),
            state: self.state.clone(),
            problem: self.problem.clone(),
        }
    }
}

/// Health row for `trigger list`.
#[derive(Debug, Clone)]
pub struct TriggerStatus {
    pub meta: TriggerMeta,
    pub status: &'static str,
    pub state: TriggerState,
    pub problem: Option<String>,
}

/// The rule registry plus the judgment driver.
pub struct SubscriptionLayer {
    root: PathBuf,
    inner: Mutex<BTreeMap<String, Runtime>>,
}

impl SubscriptionLayer {
    pub fn new(root: PathBuf) -> Arc<Self> {
        let layer = Arc::new(Self {
            root,
            inner: Mutex::new(BTreeMap::new()),
        });
        layer.reload();
        layer
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Re-read the definitions directory. Added, edited and deleted rules all
    /// take effect on the next tick, because the write-test-fix loop is the
    /// normal workflow.
    pub fn reload(&self) {
        let loaded = meta::load_all(&self.root);
        let now = now_ms();
        let mut inner = self.lock();
        let mut seen = Vec::with_capacity(loaded.len());
        for source in loaded {
            let id = source.meta.id.clone();
            seen.push(id.clone());
            let mut runtime = match inner.remove(&id) {
                Some(mut kept) => {
                    // Runtime state is the host's own; only disk content is
                    // refreshed. A rule that was broken and is now readable
                    // stops being broken.
                    kept.meta = source.meta.clone();
                    kept.script = source.script.clone();
                    kept.problem = source.problem.clone();
                    kept
                }
                None => Runtime::new(source.clone()),
            };
            // A one-shot whose moment passed while nobody was watching is
            // retired, not back-fired: the reminder was about an instant that
            // no longer exists. The definition stays on disk.
            if runtime.meta.enabled && runtime.meta.once_expired(now) {
                runtime.meta.enabled = false;
                debug!(
                    "[trigger] rule {} is a one-shot whose time has passed; disabled",
                    runtime.meta.id
                );
                if let Err(e) = meta::save_meta(&self.root, &runtime.meta) {
                    warn_log!("[trigger] cannot persist disabled rule {}: {}", id, e);
                }
            }
            inner.insert(id, runtime);
        }
        inner.retain(|id, _| seen.contains(id));
    }

    /// Ids judged at this tick (failure cadence only; the schedule gate and the
    /// ready gate are evaluated per rule).
    pub fn due(&self, tick_index: u64) -> Vec<String> {
        let inner = self.lock();
        inner
            .values()
            .filter(|runtime| runtime.meta.enabled)
            .filter(|runtime| rise_is_due(tick_index, runtime.every_n_ticks()))
            .map(|runtime| runtime.meta.id.clone())
            .collect()
    }

    /// Run one judgment round over the enabled, due rules.
    pub async fn judge(
        &self,
        tick_index: u64,
        frame: &TickFrame,
        secrets: &Arc<BTreeMap<String, String>>,
    ) -> JudgeReport {
        let candidates = self.candidates(tick_index);
        let mut report = JudgeReport::default();
        let now = now_ms();

        for candidate in candidates {
            let id = candidate.meta.id.clone();

            // -- schedule gate: closed means "do not judge, do not write" --
            if let Some(schedule) = &candidate.meta.schedule {
                let gate = judge::schedule_due(schedule, candidate.state.next_run_at, now);
                if candidate.state.next_run_at != gate.next_run_at {
                    self.set_next_run(&id, gate.next_run_at);
                }
                if !gate.is_due {
                    continue;
                }
            }

            // -- declared sources must exist --
            let missing: Vec<String> = candidate
                .meta
                .sources
                .iter()
                .filter(|sid| frame.view(sid).is_none())
                .cloned()
                .collect();
            if !missing.is_empty() {
                let error = format!(
                    "sources not found: {} (create them or drop them from this rule)",
                    missing.join(", ")
                );
                // A dangling reference is a definition problem, not a bad round:
                // it is what makes the rule read as `degraded` until fixed.
                self.record_failure(&id, &error);
                self.set_problem(&id, &error);
                report.failures.push(Failure {
                    id: id.clone(),
                    error: error.clone(),
                });
                self.raise_alert(&id, &candidate, &error, &mut report);
                continue;
            }

            // -- ready gate: an empty snapshot is not a considered answer --
            let unready: Vec<String> = candidate
                .meta
                .sources
                .iter()
                .filter(|sid| frame.view(sid).and_then(|v| v.observed_at).is_none())
                .cloned()
                .collect();
            if !unready.is_empty() {
                debug!(
                    "[trigger] rule {} skipped: sources not sampled yet ({})",
                    id,
                    unready.join(", ")
                );
                continue;
            }

            // A scheduled rule with no condition is a pure time reminder: the
            // moment itself is the fire.
            let outcome = match candidate.script.clone() {
                Some(script) => {
                    let ctx = self.build_ctx(&candidate, frame, now);
                    run_judge(script, ctx, &candidate.state.store, secrets).await
                }
                None if candidate.meta.schedule.is_some() => Ok((
                    Judgment {
                        fire: true,
                        detail: "scheduled time reached".to_string(),
                        ..Default::default()
                    },
                    BTreeMap::new(),
                )),
                None => Err("condition.js is missing".to_string()),
            };
            report.judged += 1;

            match outcome {
                Ok((judgment, writes)) => {
                    if let Some(warning) = &judgment.truncated {
                        report.warnings.push(format!("{id}: {warning}"));
                    }
                    self.commit_store(&id, writes);
                    self.apply_verdict(&id, judgment, &candidate, now, &mut report);
                }
                Err(error) => {
                    self.record_failure(&id, &error);
                    report.failures.push(Failure {
                        id: id.clone(),
                        error: error.clone(),
                    });
                    self.raise_alert(&id, &candidate, &error, &mut report);
                }
            }
        }
        report
    }

    /// `test`: judge once against the current snapshot, writing nothing.
    pub async fn test(
        &self,
        id: &str,
        frame: &TickFrame,
        secrets: &Arc<BTreeMap<String, String>>,
    ) -> Value {
        let Some(candidate) = self.candidate_by_id(id) else {
            return json!({"error": format!("no trigger with id '{id}'")});
        };
        let absent: Vec<String> = candidate
            .meta
            .sources
            .iter()
            .filter(|sid| frame.view(sid).is_none())
            .cloned()
            .collect();
        if !absent.is_empty() {
            return json!({
                "ready": false,
                "missing": absent,
                "message": "those source ids do not exist; create them or fix the reference",
            });
        }
        let unready: Vec<String> = candidate
            .meta
            .sources
            .iter()
            .filter(|sid| frame.view(sid).and_then(|v| v.observed_at).is_none())
            .cloned()
            .collect();
        if !unready.is_empty() {
            return json!({
                "ready": false,
                "missing": unready,
                "message": "those sources have not been collected yet; wait for the next tick",
            });
        }

        let started = std::time::Instant::now();
        let now = now_ms();
        let outcome = match candidate.script.clone() {
            Some(script) => {
                let ctx = self.build_ctx(&candidate, frame, now);
                run_judge(script, ctx, &candidate.state.store, secrets).await
            }
            None if candidate.meta.schedule.is_some() => Ok((
                Judgment {
                    fire: true,
                    detail: "scheduled time reached".to_string(),
                    ..Default::default()
                },
                BTreeMap::new(),
            )),
            None => Err("condition.js is missing".to_string()),
        };
        let elapsed_ms = started.elapsed().as_millis() as u64;

        match outcome {
            Ok((judgment, _writes)) => {
                let edge = judge::decide_edge(
                    effective_prev(&candidate),
                    judgment.fire,
                    now,
                    candidate.state.last_fired_at,
                    candidate.meta.cooldown_ms.max(0),
                );
                json!({
                    "ready": true,
                    "fire": judgment.fire,
                    "would_fire": edge == Edge::Fire,
                    "why": match edge {
                        Edge::Fire => "new rise, outside cooldown",
                        Edge::CooldownSuppressed => "new rise but inside cooldown",
                        Edge::NoRise => "condition was already true last round",
                        Edge::StillFalse => "condition is false",
                    },
                    "prev": effective_prev(&candidate),
                    "cooldown_remaining_ms": judge::cooldown_remaining(
                        now,
                        candidate.state.last_fired_at,
                        candidate.meta.cooldown_ms,
                    ),
                    "detail": judgment.detail,
                    "evidence": judgment.evidence,
                    "truncated": judgment.truncated,
                    "elapsed_ms": elapsed_ms,
                    "note": "a test judges the current snapshot; nothing fired and nothing was written",
                })
            }
            Err(error) => json!({"ready": true, "error": error, "elapsed_ms": elapsed_ms}),
        }
    }

    /// Health for `trigger list`.
    pub fn status(&self) -> Vec<TriggerStatus> {
        let inner = self.lock();
        inner
            .values()
            .map(|runtime| TriggerStatus {
                status: if !runtime.meta.enabled {
                    "disabled"
                } else if runtime.problem.is_some()
                    || runtime.state.consecutive_failures >= DEGRADED_AFTER_FAILURES
                {
                    "degraded"
                } else {
                    "ready"
                },
                meta: runtime.meta.clone(),
                state: runtime.state.clone(),
                problem: runtime.problem.clone(),
            })
            .collect()
    }

    pub fn get(&self, id: &str) -> Option<TriggerMeta> {
        self.lock().get(id).map(|r| r.meta.clone())
    }

    /// Every source at least one *enabled* rule reads.
    ///
    /// This is what keeps the source layer honest: a collector whose rule was
    /// paused or deleted stops running, so a source costs nothing until
    /// something actually looks at it. A disabled rule is excluded on purpose —
    /// it is not asking for data, and including it would let a paused rule pin
    /// a source forever.
    pub fn referenced_sources(&self) -> HashSet<String> {
        self.lock()
            .values()
            .filter(|runtime| runtime.meta.enabled)
            .flat_map(|runtime| runtime.meta.sources.iter().cloned())
            .filter(|id| !id.trim().is_empty())
            .collect()
    }

    pub fn script(&self, id: &str) -> Option<String> {
        self.lock().get(id).and_then(|r| r.script.clone())
    }

    pub fn exists(&self, id: &str) -> bool {
        self.lock().contains_key(id)
    }

    /// A rule copied out of the lock, for the read-only paths (`test`, and the
    /// judgment round itself). Private because it carries the host's state; the
    /// tool-facing view of a rule is [`SubscriptionLayer::status`].
    fn candidate_by_id(&self, id: &str) -> Option<Candidate> {
        self.lock().get(id).map(|r| r.candidate())
    }

    /// When this rule expects its next occurrence, for the tool's replies.
    pub fn next_run_at(&self, id: &str) -> Option<i64> {
        self.candidate_by_id(id).and_then(|c| c.state.next_run_at)
    }

    /// Insert or replace a definition in memory, keeping whatever runtime state
    /// the host already had for that id.
    pub fn put(&self, meta: TriggerMeta, script: Option<String>) {
        let dir = self.root.join(&meta.id);
        let (mut state, reset_note) = TriggerState::load(&dir);
        rebuild_horizon(&meta, &mut state, &dir);
        let incomplete = script.is_none() && meta.schedule.is_none();
        // A state file the host could not read is worth reporting even when the
        // definition itself is fine: the edge starts over, and silence about it
        // would look like a rule that forgot it had already fired.
        let problem = if incomplete {
            Some("condition.js is missing and no schedule was set".to_string())
        } else {
            reset_note
        };
        if let Some(problem) = &problem {
            warn_log!("[trigger] rule {}: {problem}", meta.id);
        }
        let runtime = Runtime {
            meta,
            script,
            state,
            problem,
        };
        self.lock().insert(runtime.meta.id.clone(), runtime);
    }

    pub fn forget(&self, id: &str) {
        self.lock().remove(id);
    }

    /// Flip `enabled`. Turning a rule back on resets the edge so a `true` left
    /// over from before the pause cannot fire the instant it is re-enabled —
    /// while cooldown, fired keys and the private store all stay put.
    pub fn set_enabled(&self, id: &str, enabled: bool) -> Option<TriggerMeta> {
        let mut inner = self.lock();
        let runtime = inner.get_mut(id)?;
        let was_enabled = runtime.meta.enabled;
        runtime.meta.enabled = enabled;
        if !was_enabled && enabled {
            runtime.state.prev = false;
        }
        runtime.meta.updated_at = now_ms();
        // The definition is on disk, and `reload` re-reads it every tick: a
        // toggle that only touched memory would be undone on the next tick.
        if let Err(e) = meta::save_meta(&self.root, &runtime.meta) {
            warn_log!("[trigger] cannot persist enabled flag for {id}: {e}");
        }
        self.persist_runtime(id, runtime);
        Some(runtime.meta.clone())
    }

    // -- internals --

    fn candidates(&self, tick_index: u64) -> Vec<Candidate> {
        let inner = self.lock();
        inner
            .values()
            .filter(|runtime| runtime.meta.enabled)
            .filter(|runtime| rise_is_due(tick_index, runtime.every_n_ticks()))
            .map(|runtime| runtime.candidate())
            .collect()
    }

    fn build_ctx(&self, candidate: &Candidate, frame: &TickFrame, now: i64) -> Value {
        let mut views = serde_json::Map::new();
        for id in &candidate.meta.sources {
            if let Some(view) = frame.view(id) {
                views.insert(id.clone(), view.to_json());
            }
        }
        json!({
            "sources": Value::Object(views),
            "fired_keys": candidate.state.fired_key_list(),
            "fire_count": candidate.state.fire_count,
            "last_fired_at": candidate.state.last_fired_at,
            "now_ms": now,
        })
    }

    /// Feed one verdict into the state machine and emit a fire when the rise is
    /// real and the cooldown allows it.
    fn apply_verdict(
        &self,
        id: &str,
        judgment: Judgment,
        candidate: &Candidate,
        now: i64,
        report: &mut JudgeReport,
    ) {
        // A clean round clears the failure streak and its alert latch.
        self.record_success(id);

        let edge = judge::decide_edge(
            effective_prev(candidate),
            judgment.fire,
            now,
            candidate.state.last_fired_at,
            candidate.meta.cooldown_ms.max(0),
        );
        let fired = edge == Edge::Fire;
        self.write_edge(id, &judgment, fired, now);

        if matches!(edge, Edge::CooldownSuppressed) {
            debug!(
                "[trigger] rule {} fire inside its cooldown; edge advanced, no fire",
                id
            );
        }
        if !fired {
            return;
        }

        if candidate.meta.is_once() {
            self.disable(id);
        }

        report.fired.push(Fire {
            id: id.to_string(),
            name: candidate.meta.name.clone(),
            origin: candidate.meta.origin.clone(),
            action: candidate.meta.action.clone(),
            detail: judgment.detail,
            evidence: judgment.evidence,
            fired_at: now,
            delivers: candidate.meta.action.kind != ActionKind::Record,
        });
    }

    fn set_next_run(&self, id: &str, next_run_at: Option<i64>) {
        let mut inner = self.lock();
        if let Some(runtime) = inner.get_mut(id) {
            runtime.state.next_run_at = next_run_at;
            self.persist_runtime(id, runtime);
        }
    }

    fn write_edge(&self, id: &str, judgment: &Judgment, fired: bool, now: i64) {
        let mut inner = self.lock();
        if let Some(runtime) = inner.get_mut(id) {
            runtime.state.prev = judgment.fire;
            if fired {
                runtime.state.last_fired_at = Some(now);
                runtime.state.fire_count += 1;
                runtime.state.touch_fired_keys(&judgment.keys, now);
            }
            self.persist_runtime(id, runtime);
        }
    }

    fn commit_store(&self, id: &str, writes: BTreeMap<String, Value>) {
        if writes.is_empty() {
            return;
        }
        let mut inner = self.lock();
        if let Some(runtime) = inner.get_mut(id) {
            for (key, value) in writes {
                runtime.state.store.insert(key, value);
            }
            self.persist_runtime(id, runtime);
        }
    }

    fn record_failure(&self, id: &str, error: &str) {
        let mut inner = self.lock();
        if let Some(runtime) = inner.get_mut(id) {
            runtime.state.consecutive_failures += 1;
            runtime.state.last_error = Some(error.to_string());
            if runtime.state.consecutive_failures >= DEGRADED_AFTER_FAILURES {
                debug!(
                    "[trigger] rule {} failed {} times in a row; judging every {} ticks",
                    runtime.meta.id, runtime.state.consecutive_failures, DEGRADED_EVERY_N_TICKS
                );
            } else {
                warn_log!(
                    "[trigger] rule {} judgment failed: {}",
                    runtime.meta.id,
                    error
                );
            }
            self.persist_runtime(id, runtime);
        }
    }

    fn record_success(&self, id: &str) {
        let mut inner = self.lock();
        if let Some(runtime) = inner.get_mut(id) {
            if runtime.state.consecutive_failures == 0
                && runtime.state.last_error.is_none()
                && runtime.problem.is_none()
            {
                return;
            }
            runtime.state.consecutive_failures = 0;
            runtime.state.last_error = None;
            runtime.state.alerted = false;
            runtime.problem = None;
            self.persist_runtime(id, runtime);
        }
    }

    fn set_problem(&self, id: &str, problem: &str) {
        let mut inner = self.lock();
        if let Some(runtime) = inner.get_mut(id) {
            runtime.problem = Some(problem.to_string());
        }
    }

    /// The alert latch: after [`ALERT_AFTER_FAILURES`] failures in a row, one
    /// signal, and only for a rule the user would otherwise hear about. Putting
    /// the alert in the report keeps delivery out of the tick — the dispatch
    /// layer decides how a fixed-text warning reaches a conversation.
    fn raise_alert(&self, id: &str, candidate: &Candidate, error: &str, report: &mut JudgeReport) {
        let failures = self.failure_count(id);
        if failures < ALERT_AFTER_FAILURES || self.is_alerted(id) {
            return;
        }
        if !candidate.meta.delivers_to_user() {
            return;
        }
        self.mark_alerted(id, true);
        report.alerts.push(Alert {
            id: id.to_string(),
            name: candidate.meta.name.clone(),
            origin: candidate.meta.origin.clone(),
            action: candidate.meta.action.clone(),
            error: error.to_string(),
            failures,
        });
    }

    fn failure_count(&self, id: &str) -> u32 {
        self.lock()
            .get(id)
            .map(|r| r.state.consecutive_failures)
            .unwrap_or(0)
    }

    fn is_alerted(&self, id: &str) -> bool {
        self.lock().get(id).is_some_and(|r| r.state.alerted)
    }

    fn mark_alerted(&self, id: &str, alerted: bool) {
        let mut inner = self.lock();
        if let Some(runtime) = inner.get_mut(id) {
            runtime.state.alerted = alerted;
            self.persist_runtime(id, runtime);
        }
    }

    fn disable(&self, id: &str) {
        let mut inner = self.lock();
        if let Some(runtime) = inner.get_mut(id) {
            runtime.meta.enabled = false;
            self.persist_runtime(id, runtime);
            if let Err(e) = meta::save_meta(&self.root, &runtime.meta) {
                warn_log!("[trigger] cannot disable rule {}: {}", id, e);
            }
        }
    }

    /// Write `state.json` for a rule the lock already holds. The path is derived
    /// from the id, which is also the directory name.
    fn persist_runtime(&self, id: &str, runtime: &Runtime) {
        let path = self.root.join(id).join(meta::STATE_FILE);
        if let Err(e) = runtime.state.save(&path) {
            warn_log!("[trigger] cannot write state for {}: {}", id, e);
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, BTreeMap<String, Runtime>> {
        match self.inner.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        }
    }
}

/// The horizon a periodic rule should carry, rebuilt from `now` whenever the
/// host (re)reads a definition, and written down at once: a rule with no stored
/// `next_run_at` is a rule whose `list` output promises nothing.
///
/// A one-shot keeps its absolute moment — that moment is the definition.
fn rebuild_horizon(meta: &TriggerMeta, state: &mut TriggerState, dir: &Path) {
    let Some(schedule) = &meta.schedule else {
        return;
    };
    if matches!(schedule, CronSchedule::At { .. }) {
        return;
    }
    state.next_run_at = judge::next_run_after(schedule, now_ms());
    if let Err(e) = state.save(&dir.join(meta::STATE_FILE)) {
        warn_log!("[trigger] cannot persist the horizon for {}: {e}", meta.id);
    }
}

/// The edge value a verdict is compared against.
///
/// A rule with no condition script has no persistent state to rise out of — its
/// schedule gate closing and opening *is* its edge — so every due occurrence
/// reads as a fresh rise and a daily reminder repeats instead of firing once.
/// A rule that *does* carry a script keeps the documented edge semantics: a
/// condition that stays true is announced once, and the gate never flushes it
/// to false in the hours when nothing is due.
fn effective_prev(candidate: &Candidate) -> bool {
    if candidate.script.is_none() {
        false
    } else {
        candidate.state.prev
    }
}

async fn run_judge(
    script: String,
    ctx: Value,
    store: &BTreeMap<String, Value>,
    secrets: &Arc<BTreeMap<String, String>>,
) -> Result<(Judgment, BTreeMap<String, Value>), String> {
    let redactor = Redactor::new(secrets.values().cloned());
    let env = HostEnv::new(redactor);
    let host = Arc::new(host::JudgeHost::new(store.clone()));
    let request = RunRequest::new(script, ENTRY_JUDGE)
        .with_ctx(ctx)
        .with_limits(Limits::judge());
    let raw = execute_async(request, host.clone(), env)
        .await
        .map_err(|e: ScriptError| e.to_string())?;
    let writes = host.writes();
    let judgment = clamp_verdict(&raw)?;
    Ok((judgment, writes))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::trigger::source::SourceLayer;
    use crate::trigger::source::{DEFAULT_TTL, meta as source_file};
    use std::time::Duration;

    const COLLECT_TWO: &str =
        "function collect() { return {items: [{title: 'a'}, {title: 'b'}]}; }";
    const ALWAYS_TRUE: &str = "function judge() { return {fire: true, detail: 'fire', \
                            evidence: [{source_id: 'src-0001', text: 'two open'}]}; }";
    const ALWAYS_FALSE: &str = "function judge() { return {fire: false}; }";
    const BROKEN: &str = "function judge() { throw new Error('boom'); }";
    const REMEMBER: &str = "function judge(ctx) { ctx.store.set('cursor', 'kept'); \
                        return {fire: true, detail: 'fire', keys: ['k-1']}; }";

    fn rule(id: &str, sources: &[&str]) -> TriggerMeta {
        let mut meta = TriggerMeta::new(id.to_string(), "test rule");
        meta.sources = sources.iter().map(|s| s.to_string()).collect();
        meta
    }

    fn notifying(id: &str) -> TriggerMeta {
        let mut meta = rule(id, &[]);
        meta.action = TriggerAction {
            kind: ActionKind::Notify,
            text_template: Some("{{detail}}".to_string()),
            ..Default::default()
        };
        meta
    }

    struct Harness {
        _tmp: tempfile::TempDir,
        sources: Arc<SourceLayer>,
        rules: Arc<SubscriptionLayer>,
        secrets: Arc<BTreeMap<String, String>>,
    }

    impl Harness {
        fn new() -> Self {
            let tmp = tempfile::tempdir().unwrap();
            let workspace = tmp.path().to_path_buf();
            let secrets: Arc<BTreeMap<String, String>> = Arc::new(BTreeMap::new());
            let sources = Arc::new(SourceLayer::new(
                workspace.join("triggers").join("sources"),
                workspace.clone(),
                secrets.clone(),
                Duration::from_secs(60),
                DEFAULT_TTL,
            ));
            let rules = SubscriptionLayer::new(workspace.join("triggers").join("triggers"));
            Self {
                _tmp: tmp,
                sources,
                rules,
                secrets,
            }
        }

        fn source(&self, id: &str) {
            let root = self.sources.root().to_path_buf();
            let mut meta = source_file::SourceMeta::new(id, id);
            meta.timeout_s = 1;
            source_file::save_meta(&root, &meta).unwrap();
            source_file::save_script(&root, id, COLLECT_TWO).unwrap();
            self.sources.reload();
        }

        fn rule(&self, meta: TriggerMeta, script: Option<&str>) {
            let root = self.rules.root().to_path_buf();
            meta::save_meta(&root, &meta).unwrap();
            if let Some(script) = script {
                meta::save_script(&root, &meta.id, script).unwrap();
            }
            self.rules.reload();
        }

        /// Editing a script on disk is the normal repair loop, so the layer must
        /// pick it up without a restart.
        fn edit_script(&self, id: &str, script: &str) {
            meta::save_script(self.rules.root(), id, script).unwrap();
            self.rules.reload();
        }

        /// A round on a cold process: the frame is frozen before anything has
        /// been collected, so every source reads as un-sampled.
        async fn cold_round(&self, tick: u64) -> JudgeReport {
            let report = self.sources.collect(tick, 2).await;
            self.rules.judge(tick, &report.frame, &self.secrets).await
        }

        /// A round with data in the snapshot: collect, then judge the frame that
        /// now carries it.
        async fn ready_round(&self, tick: u64) -> JudgeReport {
            self.sources.collect(tick, 2).await;
            let frame = self.sources.freeze();
            self.rules.judge(tick, &frame, &self.secrets).await
        }

        async fn frame(&self) -> TickFrame {
            self.sources.collect(1, 2).await;
            self.sources.freeze()
        }

        fn state(&self, id: &str) -> TriggerState {
            TriggerState::load(&self.rules.root().join(id)).0
        }

        fn status(&self, id: &str) -> TriggerStatus {
            self.rules
                .status()
                .into_iter()
                .find(|s| s.meta.id == id)
                .expect("rule must be listed")
        }
    }

    #[tokio::test]
    async fn test_set_enabled_survives_the_next_reload() {
        // `enabled` is the one field the tool edits in memory; a reload must not
        // undo it, or pausing a rule would last exactly until the next tick.
        let h = Harness::new();
        h.rule(rule("trg-a", &["src-0001"]), None);

        h.rules.set_enabled("trg-a", false).unwrap();
        h.rules.reload();
        assert!(
            !h.rules.get("trg-a").expect("rule must exist").enabled,
            "a paused rule must stay paused across a reload"
        );
        assert!(h.rules.referenced_sources().is_empty());
    }

    #[tokio::test]
    async fn test_referenced_sources_are_the_union_of_enabled_rules_only() {
        // A disabled rule is not asking for anything: it must not keep its
        // exclusive source alive while it is paused.
        let h = Harness::new();
        h.rule(rule("trg-a", &["src-0001", "src-0002"]), None);
        let mut paused = rule("trg-b", &["src-0003"]);
        paused.enabled = false;
        h.rule(paused, None);
        h.rule(rule("trg-c", &["src-0002"]), None);

        let referenced = h.rules.referenced_sources();
        let mut ids: Vec<&str> = referenced.iter().map(String::as_str).collect();
        ids.sort();
        assert_eq!(ids, vec!["src-0001", "src-0002"]);

        // Re-enabling the rule puts its source back in the same reload.
        h.rules.set_enabled("trg-b", true).unwrap();
        assert!(h.rules.referenced_sources().contains("src-0003"));
    }

    #[tokio::test]
    async fn test_a_rise_fires_once_while_the_condition_stays_true() {
        let h = Harness::new();
        h.source("src-0001");
        h.rule(rule("trg-a", &["src-0001"]), Some(ALWAYS_TRUE));

        let first = h.ready_round(1).await;
        assert_eq!(first.judged, 1);
        assert_eq!(first.fired.len(), 1, "a new rise must fire");
        assert_eq!(first.fired[0].detail, "fire");
        assert_eq!(first.fired[0].evidence.len(), 1);

        let state = h.state("trg-a");
        assert!(state.prev, "the edge must be persisted as true");
        assert_eq!(state.fire_count, 1);
        assert!(state.last_fired_at.is_some());

        // Same world, same verdict: `true -> true` is not a rise.
        let second = h.ready_round(2).await;
        assert!(
            second.fired.is_empty(),
            "a steady condition must not repeat"
        );
        assert_eq!(h.state("trg-a").fire_count, 1);
    }

    #[tokio::test]
    async fn test_the_edge_must_fall_before_it_can_fire_again() {
        let h = Harness::new();
        h.source("src-0001");
        h.rule(rule("trg-a", &["src-0001"]), Some(ALWAYS_TRUE));
        assert_eq!(h.ready_round(1).await.fired.len(), 1);

        h.edit_script("trg-a", ALWAYS_FALSE);
        let fell = h.ready_round(2).await;
        assert!(fell.fired.is_empty());
        assert!(
            !h.state("trg-a").prev,
            "a false verdict must reset the edge"
        );

        h.edit_script("trg-a", ALWAYS_TRUE);
        let rose = h.ready_round(3).await;
        assert_eq!(rose.fired.len(), 1, "a fresh rise after a fall must fire");
        assert_eq!(h.state("trg-a").fire_count, 2);
    }

    #[tokio::test]
    async fn test_a_fire_inside_cooldown_advances_the_edge_instead_of_firing() {
        let h = Harness::new();
        h.source("src-0001");
        let mut meta = rule("trg-a", &["src-0001"]);
        meta.cooldown_ms = 24 * 3600 * 1000;
        h.rule(meta, Some(ALWAYS_TRUE));

        assert_eq!(h.ready_round(1).await.fired.len(), 1);
        // Restarting the rule clears `prev` but keeps the cooldown, so the next
        // rise lands inside it: suppressed, yet still advancing the edge.
        h.rules.set_enabled("trg-a", false);
        h.rules.set_enabled("trg-a", true);
        assert!(!h.state("trg-a").prev);

        let suppressed = h.ready_round(2).await;
        assert!(suppressed.fired.is_empty(), "a cooldown fire must not fire");
        assert!(
            h.state("trg-a").prev,
            "the suppressed fire must still advance the edge"
        );
        assert_eq!(h.state("trg-a").fire_count, 1);

        // Once the cooldown expires, the still-true condition is not a rise.
        let mut state = h.state("trg-a");
        state.last_fired_at = Some(0);
        state
            .save(&h.rules.root().join("trg-a").join(meta::STATE_FILE))
            .unwrap();
        let meta = h.rules.get("trg-a").expect("rule must still exist");
        // `put` re-reads the state file, which is how the host picks the edit up.
        h.rules.put(meta, Some(ALWAYS_TRUE.to_string()));
        assert!(
            h.ready_round(3).await.fired.is_empty(),
            "an expired cooldown must not hand out a back-fill"
        );
        assert_eq!(h.state("trg-a").fire_count, 1);
    }

    #[tokio::test]
    async fn test_re_enabling_resets_the_edge_but_keeps_everything_else() {
        let h = Harness::new();
        h.source("src-0001");
        h.rule(rule("trg-a", &["src-0001"]), Some(REMEMBER));

        assert_eq!(h.ready_round(1).await.fired.len(), 1);
        let before = h.state("trg-a");
        assert!(before.prev && before.has_fired_key("k-1"));
        assert_eq!(before.store["cursor"], json!("kept"));

        h.rules.set_enabled("trg-a", false);
        h.rules.set_enabled("trg-a", true);
        let after = h.state("trg-a");
        assert!(!after.prev, "re-enable must not inherit a leftover rise");
        assert!(after.has_fired_key("k-1"), "the fired ledger must survive");
        assert_eq!(
            after.store["cursor"],
            json!("kept"),
            "the script store must survive"
        );
        assert_eq!(
            after.last_fired_at, before.last_fired_at,
            "cooldown must survive"
        );
    }

    #[tokio::test]
    async fn test_a_closed_schedule_gate_judges_nothing_and_writes_no_edge() {
        let h = Harness::new();
        h.source("src-0001");
        let mut meta = rule("trg-a", &["src-0001"]);
        meta.schedule = Some(CronSchedule::every(60_000));
        h.rule(meta, Some(ALWAYS_TRUE));

        let round = h.ready_round(1).await;
        assert_eq!(round.judged, 0, "a closed gate must not run the script");
        assert!(round.fired.is_empty());
        let state = h.state("trg-a");
        assert!(!state.prev);
        assert_eq!(state.fire_count, 0);
        assert!(
            state.next_run_at.is_some(),
            "the horizon must be remembered"
        );
    }

    #[tokio::test]
    async fn test_the_gate_never_flushes_the_edge_between_occurrences() {
        let h = Harness::new();
        h.source("src-0001");
        let mut meta = rule("trg-a", &["src-0001"]);
        // Every second, so a test can watch two occurrences pass.
        meta.schedule = Some(CronSchedule::cron("* * * * * *".to_string(), None));
        h.rule(meta, Some(ALWAYS_TRUE));

        // First occurrence: closed, because the horizon was built from now.
        assert_eq!(h.ready_round(1).await.judged, 0);
        tokio::time::sleep(Duration::from_millis(1_100)).await;
        let opened = h.ready_round(2).await;
        assert_eq!(opened.fired.len(), 1, "the first open gate must fire");

        // The second occurrence arrives with the condition still true and the
        // edge still true: writing `false` while the gate was closed would have
        // turned "tell me once" into "tell me every day".
        tokio::time::sleep(Duration::from_millis(1_100)).await;
        let again = h.ready_round(3).await;
        assert_eq!(again.judged, 1, "an open gate must run the script");
        assert!(
            again.fired.is_empty(),
            "a closed gate must not have reset the edge"
        );
        assert_eq!(h.state("trg-a").fire_count, 1);
    }

    #[tokio::test]
    async fn test_an_unsampled_source_skips_the_judgment() {
        let h = Harness::new();
        h.source("src-0001");
        h.rule(rule("trg-a", &["src-0001"]), Some(ALWAYS_TRUE));

        // The ready gate: the source exists but has never been collected.
        let round = h.cold_round(1).await;
        assert_eq!(round.judged, 0);
        assert!(round.fired.is_empty());
        assert!(round.failures.is_empty(), "waiting is not a failure");
        let state = h.state("trg-a");
        assert!(!state.prev);
        assert_eq!(
            state.consecutive_failures, 0,
            "the ready gate must not penalise"
        );

        // One tick later the data is there and the same rule fires.
        assert_eq!(h.ready_round(2).await.fired.len(), 1);
    }

    #[tokio::test]
    async fn test_a_missing_source_id_degrades_the_rule_and_names_the_id() {
        let h = Harness::new();
        h.rule(rule("trg-a", &["src-9999"]), Some(ALWAYS_TRUE));

        let round = h.ready_round(1).await;
        assert_eq!(round.judged, 0);
        assert_eq!(round.failures.len(), 1);
        assert!(
            round.failures[0].error.contains("src-9999"),
            "the error must say which id is missing: {}",
            round.failures[0].error
        );
        let status = h.status("trg-a");
        assert_eq!(status.status, "degraded");
        assert!(!h.state("trg-a").prev, "the edge must stay untouched");
    }

    #[tokio::test]
    async fn test_a_scheduled_rule_without_script_fires_at_every_occurrence() {
        let h = Harness::new();
        let mut meta = rule("trg-a", &[]);
        // One millisecond, purely so a test can see two occurrences.
        meta.schedule = Some(CronSchedule::every(1));
        h.rule(meta, None);
        tokio::time::sleep(Duration::from_millis(5)).await;

        let first = h.rules.judge(1, &h.frame().await, &h.secrets).await;
        assert_eq!(first.fired.len(), 1, "a pure reminder must fire");
        tokio::time::sleep(Duration::from_millis(5)).await;
        let second = h.rules.judge(2, &h.frame().await, &h.secrets).await;
        assert_eq!(
            second.fired.len(),
            1,
            "a time-only rule has no condition to stay true: the gate is its edge"
        );
        assert_eq!(h.state("trg-a").fire_count, 2);
    }

    #[tokio::test]
    async fn test_a_broken_script_is_booked_and_stays_enabled() {
        let h = Harness::new();
        h.rule(rule("trg-a", &[]), Some(BROKEN));

        let round = h.rules.judge(1, &h.frame().await, &h.secrets).await;
        assert_eq!(round.failures.len(), 1);
        assert!(
            h.rules.get("trg-a").unwrap().enabled,
            "failure must not disable"
        );
        let state = h.state("trg-a");
        assert_eq!(state.consecutive_failures, 1);
        assert!(state.last_error.unwrap().contains("boom"));
    }

    #[tokio::test]
    async fn test_five_failures_degrade_then_alert_exactly_once() {
        let h = Harness::new();
        h.rule(notifying("trg-a"), Some(BROKEN));

        // Three straight failures, then the cadence slows to every fifth tick.
        for tick in 1..=3 {
            let round = h.rules.judge(tick, &h.frame().await, &h.secrets).await;
            assert_eq!(round.failures.len(), 1, "tick {tick} must be judged");
        }
        assert!(
            h.rules
                .judge(4, &h.frame().await, &h.secrets)
                .await
                .failures
                .is_empty(),
            "a degraded rule must be judged less often"
        );
        assert_eq!(
            h.rules
                .judge(5, &h.frame().await, &h.secrets)
                .await
                .failures
                .len(),
            1
        );

        let fifth = h.rules.judge(10, &h.frame().await, &h.secrets).await;
        assert_eq!(fifth.alerts.len(), 1, "the fifth failure must alert");
        assert!(h.state("trg-a").alerted);

        // A sixth failure is still a failure, but nobody is told twice.
        let sixth = h.rules.judge(15, &h.frame().await, &h.secrets).await;
        assert_eq!(sixth.failures.len(), 1);
        assert!(sixth.alerts.is_empty(), "the alert must latch");

        // A clean round clears the latch, so a later break can alert again.
        h.edit_script("trg-a", ALWAYS_TRUE);
        let recovered = h.rules.judge(20, &h.frame().await, &h.secrets).await;
        assert_eq!(recovered.fired.len(), 1);
        let state = h.state("trg-a");
        assert_eq!(state.consecutive_failures, 0);
        assert!(!state.alerted, "recovery must re-arm the alert");
    }

    #[tokio::test]
    async fn test_a_silent_rule_never_alerts() {
        let h = Harness::new();
        h.rule(rule("trg-a", &[]), Some(BROKEN));
        for tick in [1u64, 2, 3, 5, 10] {
            h.rules.judge(tick, &h.frame().await, &h.secrets).await;
        }
        let report = h.rules.judge(15, &h.frame().await, &h.secrets).await;
        assert_eq!(report.failures.len(), 1);
        assert!(
            report.alerts.is_empty(),
            "a record rule has nobody to alert"
        );
        assert!(!h.state("trg-a").alerted);
    }

    #[tokio::test]
    async fn test_a_damaged_state_file_is_reset_with_a_warning_not_a_guess() {
        let h = Harness::new();
        let meta = rule("trg-a", &["src-0001"]);
        let dir = h.rules.root().join("trg-a");
        meta::save_meta(h.rules.root(), &meta).unwrap();
        meta::save_script(h.rules.root(), "trg-a", ALWAYS_TRUE).unwrap();
        std::fs::write(dir.join(meta::STATE_FILE), b"{\"prev\": true, torn").unwrap();

        let reopened = SubscriptionLayer::new(h.rules.root().to_path_buf());
        let candidate = reopened.candidate_by_id("trg-a").expect("rule must load");
        let problem = candidate.problem.expect("the reset must be reported");
        assert!(problem.contains("state.json"), "{problem}");
        assert!(!candidate.state.prev, "a torn edge must not be trusted");
        assert_eq!(candidate.state.fire_count, 0);
    }

    #[tokio::test]
    async fn test_a_fresh_layer_rebuilds_a_periodic_horizon_instead_of_back_firing() {
        let h = Harness::new();
        let mut meta = rule("trg-a", &[]);
        meta.schedule = Some(CronSchedule::every(60_000));
        let dir = h.rules.root().join("trg-a");
        meta::save_meta(h.rules.root(), &meta).unwrap();
        // A horizon from before the restart: already long past.
        let state = TriggerState {
            next_run_at: Some(1),
            ..Default::default()
        };
        state.save(&dir.join(meta::STATE_FILE)).unwrap();

        let reopened = SubscriptionLayer::new(h.rules.root().to_path_buf());
        let candidate = reopened.candidate_by_id("trg-a").unwrap();
        assert!(
            candidate.state.next_run_at.unwrap() > now_ms(),
            "a restart must not queue the missed occurrence"
        );
        // And the first round after the restart judges nothing, so nothing fires.
        let round = reopened.judge(1, &h.sources.freeze(), &h.secrets).await;
        assert_eq!(round.judged, 0);
        assert!(round.fired.is_empty());
    }

    #[tokio::test]
    async fn test_an_expired_one_shot_is_kept_but_disabled() {
        let h = Harness::new();
        let mut meta = rule("trg-a", &[]);
        meta.schedule = Some(CronSchedule::at(now_ms() - 60_000));
        meta::save_meta(h.rules.root(), &meta).unwrap();
        meta::save_script(h.rules.root(), "trg-a", ALWAYS_TRUE).unwrap();

        let reopened = SubscriptionLayer::new(h.rules.root().to_path_buf());
        assert!(
            reopened.get("trg-a").is_some(),
            "the definition must survive"
        );
        assert!(
            !reopened.get("trg-a").unwrap().enabled,
            "a missed moment is retired"
        );
        let on_disk = meta::load_all(h.rules.root())
            .into_iter()
            .find(|loaded| loaded.meta.id == "trg-a")
            .unwrap();
        assert!(
            !on_disk.meta.enabled,
            "and the retirement must be persisted"
        );
    }

    #[tokio::test]
    async fn test_a_one_shot_fires_once_and_disables_itself() {
        let h = Harness::new();
        let mut meta = rule("trg-a", &[]);
        meta.schedule = Some(CronSchedule::at(now_ms() + 20));
        h.rule(meta, Some(ALWAYS_TRUE));
        tokio::time::sleep(Duration::from_millis(40)).await;

        let fired = h.rules.judge(1, &h.frame().await, &h.secrets).await;
        assert_eq!(fired.fired.len(), 1);
        assert!(
            !h.rules.get("trg-a").unwrap().enabled,
            "a one-shot retires itself"
        );

        let later = h.rules.judge(2, &h.frame().await, &h.secrets).await;
        assert!(later.fired.is_empty(), "a retired rule cannot fire again");
    }

    #[tokio::test]
    async fn test_a_record_fire_is_booked_but_silent() {
        let h = Harness::new();
        h.source("src-0001");
        h.rule(rule("trg-a", &["src-0001"]), Some(ALWAYS_TRUE));
        let round = h.ready_round(1).await;
        assert_eq!(round.fired.len(), 1);
        assert!(
            !round.fired[0].delivers,
            "a record rule must never reach a conversation"
        );
    }

    #[tokio::test]
    async fn test_the_test_action_judges_without_firing_or_writing() {
        let h = Harness::new();
        h.source("src-0001");
        h.rule(rule("trg-a", &["src-0001"]), Some(REMEMBER));
        let frame = h.frame().await;

        let report = h.rules.test("trg-a", &frame, &h.secrets).await;
        assert_eq!(report["ready"], json!(true));
        assert_eq!(report["fire"], json!(true));
        assert_eq!(report["would_fire"], json!(true));
        assert_eq!(report["detail"], json!("fire"));
        assert!(
            report["elapsed_ms"].as_u64().is_some(),
            "a test must say what it cost: {report}"
        );

        // Nothing was written: no fire, no edge, no store, no counter.
        let state = h.state("trg-a");
        assert!(!state.prev);
        assert_eq!(state.fire_count, 0);
        assert!(state.last_fired_at.is_none());
        assert!(
            state.store.is_empty(),
            "a test may not persist a store write"
        );
        assert!(state.fired_keys.is_empty());

        // Running it twice says the same thing, because nothing advanced.
        let again = h.rules.test("trg-a", &frame, &h.secrets).await;
        assert_eq!(again["would_fire"], json!(true));
        assert_eq!(again["prev"], json!(false));
    }

    #[tokio::test]
    async fn test_the_test_action_says_which_source_is_missing() {
        let h = Harness::new();
        let frame = h.frame().await;

        let report = h.rules.test("trg-nope", &frame, &h.secrets).await;
        assert!(report["error"].is_string(), "{report}");

        h.rule(rule("trg-a", &["src-9999"]), Some(ALWAYS_TRUE));
        let missing = h.rules.test("trg-a", &frame, &h.secrets).await;
        assert_eq!(missing["ready"], json!(false));
        assert_eq!(missing["missing"], json!(["src-9999"]));

        h.source("src-0001");
        h.rule(rule("trg-b", &["src-0001"]), Some(ALWAYS_TRUE));
        // A frame taken before any collection: the source is known, just not
        // sampled yet, which is a different answer from "no such id".
        let unsampled = h.rules.test("trg-b", &h.sources.freeze(), &h.secrets).await;
        assert_eq!(unsampled["ready"], json!(false));
        assert!(
            unsampled["message"].as_str().unwrap().contains("next tick"),
            "{unsampled}"
        );
    }
}
