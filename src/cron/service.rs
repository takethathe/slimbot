use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use super::types::*;
use crate::debug;

pub type CronJobCallback = Arc<
    dyn Fn(CronJob) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>>
        + Send
        + Sync,
>;

/// How late a one-shot (`At`) occurrence may be caught and still run.
///
/// The scheduler ticks about once a second, so a delay of more than a few
/// minutes means the tick that should have caught the occurrence never happened
/// (the process was down, or a job blocked the scheduler). Such a window is
/// treated as missed: the occurrence is recorded as `skipped` in the job's run
/// history and is **never** back-run (ADR 0018: downtime is acknowledged, not
/// paid for with a catch-up burst). Occurrences inside the grace window still
/// run late, which keeps a normal tick cadence free of false skips.
pub const ONE_SHOT_GRACE_MS: i64 = 5 * 60 * 1000;

pub struct CronService {
    store_path: PathBuf,
    jobs: Arc<Mutex<Vec<CronJob>>>,
    on_job: Mutex<Option<CronJobCallback>>,
    running: AtomicBool,
}

impl CronService {
    pub fn new(workspace_dir: &Path) -> Self {
        let cron_dir = workspace_dir.join("cron");
        std::fs::create_dir_all(&cron_dir).ok();
        Self {
            store_path: cron_dir.join("jobs.json"),
            jobs: Arc::new(Mutex::new(Vec::new())),
            on_job: Mutex::new(None),
            running: AtomicBool::new(false),
        }
    }

    pub fn set_on_job(&self, cb: CronJobCallback) {
        *self.on_job.lock().unwrap() = Some(cb);
    }

    pub fn load(&self) {
        if self.store_path.exists()
            && let Ok(content) = std::fs::read_to_string(&self.store_path)
            && let Ok(store) = serde_json::from_str::<CronStore>(&content)
        {
            let mut guard = self.jobs.lock().unwrap();
            *guard = store.jobs;
        }
    }

    pub fn save(&self) {
        let jobs = self.jobs.lock().unwrap().clone();
        let store = CronStore { version: 1, jobs };
        if let Ok(content) = serde_json::to_string_pretty(&store) {
            let _ = std::fs::write(&self.store_path, content);
        }
    }

    pub fn add_job(&self, job: CronJob) {
        let mut guard = self.jobs.lock().unwrap();
        let now = now_ms();
        let mut job = job;
        plan_next_run(&mut job, now);
        guard.push(job);
        drop(guard);
        self.save();
    }

    pub fn remove_job(&self, job_id: &str) -> bool {
        let mut guard = self.jobs.lock().unwrap();
        let before = guard.len();
        guard.retain(|j| j.id != job_id);
        let removed = guard.len() < before;
        drop(guard);
        if removed {
            self.save();
        }
        removed
    }

    pub fn list_jobs(&self) -> Vec<CronJob> {
        self.jobs.lock().unwrap().clone()
    }

    pub fn start(&self) {
        self.running.store(true, Ordering::Relaxed);
        self.recompute_next_runs();
        self.save();
    }

    pub fn stop(&self) {
        self.running.store(false, Ordering::Relaxed);
    }

    fn recompute_next_runs(&self) {
        let now = now_ms();
        let mut guard = self.jobs.lock().unwrap();
        for job in guard.iter_mut() {
            if job.enabled {
                // Coming up after downtime: a one-shot window that already
                // passed is recorded as skipped here instead of silently
                // disappearing; periodic schedules re-anchor on now, which is
                // what keeps a restart from firing a burst of missed rounds.
                plan_next_run(job, now);
            }
        }
    }

    pub async fn tick(&self) {
        if !self.running.load(Ordering::Relaxed) {
            return;
        }

        let now = now_ms();

        // Collect work outside the lock: one-shot occurrences whose window is
        // gone (skipped, never run) and jobs whose horizon has arrived.
        let (missed, due_jobs): (Vec<(i64, CronJob)>, Vec<CronJob>) = {
            let guard = self.jobs.lock().unwrap();
            let mut missed = Vec::new();
            let mut due = Vec::new();
            for job in guard.iter().filter(|j| j.enabled) {
                if let Some(horizon) = missed_one_shot(job, now) {
                    missed.push((horizon, job.clone()));
                } else if job.state.next_run_at_ms.is_some_and(|t| now >= t) {
                    due.push(job.clone());
                }
            }
            (missed, due)
        };

        if missed.is_empty() && due_jobs.is_empty() {
            return;
        }

        for (horizon, job) in missed {
            self.skip_job(&job.id, horizon, now);
        }
        for job in due_jobs {
            self.execute_job(&job).await;
        }
        self.save();
    }

    /// Record a missed one-shot occurrence and disable the job. Nothing ran, so
    /// `last_run_at_ms` is left untouched; the `skipped` entry in the run history
    /// is the audit trail, and the job stays on disk (disabled) to keep it.
    fn skip_job(&self, job_id: &str, horizon: i64, now: i64) {
        let mut guard = self.jobs.lock().unwrap();
        if let Some(j) = guard.iter_mut().find(|j| j.id == job_id) {
            debug!(
                "[cron] skipping missed one-shot '{}' ({}) scheduled for {}",
                j.name, j.id, horizon
            );
            record_skipped(j, horizon, now);
        }
    }

    async fn execute_job(&self, job: &CronJob) {
        let start = now_ms();
        debug!("[cron] executing job '{}' ({})", job.name, job.id);

        // Execute callback outside the lock to avoid deadlock
        let cb = { self.on_job.lock().unwrap().clone() };
        if let Some(cb) = cb {
            cb(job.clone()).await;
        }

        let end = now_ms();
        let mut guard = self.jobs.lock().unwrap();
        if let Some(j) = guard.iter_mut().find(|j| j.id == job.id) {
            j.state.last_run_at_ms = Some(start);
            j.state.last_status = Some("ok".to_string());
            j.state.last_error = None;
            j.state.run_history.push(CronRunRecord {
                run_at_ms: start,
                status: "ok".to_string(),
                duration_ms: end - start,
                error: None,
            });
            trim_history(&mut j.state.run_history);

            if matches!(&j.schedule, CronSchedule::At { .. }) || j.delete_after_run {
                j.enabled = false;
                j.state.next_run_at_ms = None;
                if j.delete_after_run {
                    let id = j.id.clone();
                    drop(guard);
                    self.remove_job(&id);
                    return;
                }
            } else {
                j.state.next_run_at_ms = compute_next_run(&j.schedule, now_ms());
            }
            j.updated_at_ms = now_ms();
        }
    }
}

/// The single moment a one-shot fires, or `None` for recurring schedules.
fn one_shot_horizon(job: &CronJob) -> Option<i64> {
    match &job.schedule {
        CronSchedule::At { at_ms } => Some(job.state.next_run_at_ms.unwrap_or(*at_ms)),
        _ => None,
    }
}

/// The horizon of an enabled one-shot whose window passed more than
/// [`ONE_SHOT_GRACE_MS`] ago, i.e. an occurrence that will never be run.
fn missed_one_shot(job: &CronJob, now_ms: i64) -> Option<i64> {
    if !job.enabled {
        return None;
    }
    let horizon = one_shot_horizon(job)?;
    (now_ms - horizon > ONE_SHOT_GRACE_MS).then_some(horizon)
}

/// Decide the job's next horizon: recurring schedules are computed forward from
/// now (missed rounds are never back-filled), while a one-shot keeps its single
/// moment so a slightly late tick still runs it. A one-shot past the grace window
/// is recorded as `skipped` and disabled instead.
fn plan_next_run(job: &mut CronJob, now_ms: i64) {
    let Some(horizon) = one_shot_horizon(job) else {
        job.state.next_run_at_ms = compute_next_run(&job.schedule, now_ms);
        return;
    };
    match missed_one_shot(job, now_ms) {
        Some(missed) => record_skipped(job, missed, now_ms),
        None => job.state.next_run_at_ms = Some(horizon),
    }
}

/// Record a `skipped` occurrence and disable the job. Nothing ran, so
/// `last_run_at_ms` stays untouched: the run history is the audit trail.
fn record_skipped(job: &mut CronJob, horizon: i64, now_ms: i64) {
    job.enabled = false;
    job.state.next_run_at_ms = None;
    job.state.last_status = Some("skipped".to_string());
    job.state.last_error = Some("missed one-shot window".to_string());
    job.state.run_history.push(CronRunRecord {
        run_at_ms: horizon,
        status: "skipped".to_string(),
        duration_ms: 0,
        error: Some(format!(
            "window missed by {}ms (grace {}ms)",
            now_ms - horizon,
            ONE_SHOT_GRACE_MS
        )),
    });
    trim_history(&mut job.state.run_history);
    job.updated_at_ms = now_ms;
}

/// Keep the run history bounded, dropping the oldest entries first.
fn trim_history(history: &mut Vec<CronRunRecord>) {
    if history.len() > MAX_RUN_HISTORY {
        let drain_to = history.len() - MAX_RUN_HISTORY;
        history.drain(..drain_to);
    }
}

/// Compute next run time in ms from now.
pub fn compute_next_run(schedule: &CronSchedule, now_ms: i64) -> Option<i64> {
    match schedule {
        CronSchedule::At { at_ms } => {
            if *at_ms > now_ms {
                Some(*at_ms)
            } else {
                None
            }
        }
        CronSchedule::Every { every_ms } => {
            if *every_ms <= 0 {
                None
            } else {
                Some(now_ms + *every_ms)
            }
        }
        CronSchedule::Cron { expr, .. } => compute_next_cron(expr, now_ms),
    }
}

fn compute_next_cron(expr: &str, now_ms: i64) -> Option<i64> {
    use cron::Schedule;
    use std::str::FromStr;
    use std::time::{Duration, SystemTime};

    // Scheduling is local time everywhere (ADR 0018): the expression is read on
    // the machine's clock, exactly as the trigger layer reads it. `tz` is
    // accepted for compatibility but never applied — no `chrono-tz` here.
    let schedule = Schedule::from_str(expr).ok()?;
    let now = SystemTime::UNIX_EPOCH.checked_add(Duration::from_millis(now_ms.max(0) as u64))?;
    // `after` iterates in the timezone of the anchor, so an expression is
    // resolved against the local wall clock.
    let anchor = chrono::DateTime::<chrono::Local>::from(now);
    schedule
        .after(&anchor)
        .next()
        .map(|dt| dt.timestamp_millis())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cron::types::now_ms;

    #[test]
    fn test_compute_next_run_at_future() {
        let future = now_ms() + 5000;
        let schedule = CronSchedule::at(future);
        let next = compute_next_run(&schedule, now_ms());
        assert_eq!(next, Some(future));
    }

    #[test]
    fn test_compute_next_run_at_past() {
        let past = now_ms() - 5000;
        let schedule = CronSchedule::at(past);
        let next = compute_next_run(&schedule, now_ms());
        assert_eq!(next, None);
    }

    #[test]
    fn test_compute_next_run_every() {
        let schedule = CronSchedule::every(60_000);
        let next = compute_next_run(&schedule, now_ms());
        assert!(next.is_some());
        assert!(next.unwrap() > now_ms());
    }

    #[test]
    fn test_compute_next_run_cron_expression() {
        // cron crate uses 6-field format (second minute hour day month weekday)
        let schedule = CronSchedule::cron("0 */5 * * * *".to_string(), None);
        let next = compute_next_run(&schedule, now_ms());
        assert!(next.is_some());
    }

    #[test]
    fn test_invalid_cron_expression() {
        let schedule = CronSchedule::cron("invalid".to_string(), None);
        let next = compute_next_run(&schedule, now_ms());
        assert_eq!(next, None);
    }

    #[test]
    fn test_cron_service_add_list_remove() {
        let tmp = tempfile::tempdir().unwrap();
        let svc = CronService::new(tmp.path());
        let job = CronJob {
            id: "test-1".to_string(),
            name: "test job".to_string(),
            enabled: true,
            schedule: CronSchedule::every(60_000),
            payload: CronPayload {
                kind: "agent_turn".to_string(),
                message: "test".to_string(),
                deliver: false,
                channel: None,
                to: None,
            },
            state: CronJobState::default(),
            created_at_ms: 0,
            updated_at_ms: 0,
            delete_after_run: false,
        };
        svc.add_job(job);
        assert_eq!(svc.list_jobs().len(), 1);
        assert!(svc.remove_job("test-1"));
        assert_eq!(svc.list_jobs().len(), 0);
        assert!(!svc.remove_job("nonexistent"));
    }

    #[test]
    fn test_cron_service_persistence() {
        let tmp = tempfile::tempdir().unwrap();
        {
            let svc = CronService::new(tmp.path());
            let job = CronJob {
                id: "persist-1".to_string(),
                name: "persist".to_string(),
                enabled: true,
                schedule: CronSchedule::every(30_000),
                payload: CronPayload::default(),
                state: CronJobState::default(),
                created_at_ms: 0,
                updated_at_ms: 0,
                delete_after_run: false,
            };
            svc.add_job(job);
        }
        // Reload
        let svc = CronService::new(tmp.path());
        svc.load();
        assert_eq!(svc.list_jobs().len(), 1);
    }

    #[test]
    fn test_cron_service_tick_only_when_running() {
        let tmp = tempfile::tempdir().unwrap();
        let svc = CronService::new(tmp.path());
        let job = CronJob {
            id: "tick-1".to_string(),
            name: "tick test".to_string(),
            enabled: true,
            schedule: CronSchedule::at(0), // past time, always due
            payload: CronPayload {
                kind: "test".to_string(),
                message: "tick".to_string(),
                deliver: false,
                channel: None,
                to: None,
            },
            state: CronJobState::default(),
            created_at_ms: 0,
            updated_at_ms: 0,
            delete_after_run: false,
        };
        svc.add_job(job);

        // Not running — tick should be a no-op
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            svc.tick().await;
        });

        // Job should still be in the list (wasn't executed/removed)
        assert_eq!(svc.list_jobs().len(), 1);
    }

    #[tokio::test]
    async fn test_cron_service_tick_executes_due_job() {
        let tmp = tempfile::tempdir().unwrap();
        let svc = CronService::new(tmp.path());

        let job = CronJob {
            id: "due-1".to_string(),
            name: "due job".to_string(),
            enabled: true,
            schedule: CronSchedule::every(1000),
            payload: CronPayload {
                kind: "test".to_string(),
                message: "execute me".to_string(),
                deliver: false,
                channel: None,
                to: None,
            },
            state: CronJobState::default(),
            created_at_ms: 0,
            updated_at_ms: 0,
            delete_after_run: false,
        };
        svc.add_job(job);
        svc.start();

        // start() calls recompute_next_runs which sets next_run_at_ms to a future time.
        // Reset it to make the job immediately due.
        {
            let mut guard = svc.jobs.lock().unwrap();
            if let Some(j) = guard.iter_mut().find(|j| j.id == "due-1") {
                j.state.next_run_at_ms = Some(0);
            }
        }

        svc.tick().await;

        // At-schedule job should be disabled after execution
        let jobs = svc.list_jobs();
        assert_eq!(jobs.len(), 1);
        // Every-schedule stays enabled; check that it was executed
        assert!(jobs[0].state.last_run_at_ms.is_some());
        assert_eq!(jobs[0].state.last_status, Some("ok".to_string()));
    }

    #[tokio::test]
    async fn test_cron_service_tick_skips_non_due_job() {
        let tmp = tempfile::tempdir().unwrap();
        let svc = CronService::new(tmp.path());

        let future_time = now_ms() + 60_000; // 1 minute from now
        let job = CronJob {
            id: "future-1".to_string(),
            name: "future job".to_string(),
            enabled: true,
            schedule: CronSchedule::at(future_time),
            payload: CronPayload::default(),
            state: CronJobState {
                next_run_at_ms: Some(future_time),
                ..Default::default()
            },
            created_at_ms: 0,
            updated_at_ms: 0,
            delete_after_run: false,
        };
        svc.add_job(job);
        svc.start();

        svc.tick().await;

        // Future job should NOT have been executed
        let jobs = svc.list_jobs();
        assert_eq!(jobs.len(), 1);
        assert!(jobs[0].state.last_run_at_ms.is_none());
    }

    #[tokio::test]
    async fn test_cron_service_tick_every_schedule_recomputes() {
        let tmp = tempfile::tempdir().unwrap();
        let svc = CronService::new(tmp.path());

        let job = CronJob {
            id: "every-1".to_string(),
            name: "every job".to_string(),
            enabled: true,
            schedule: CronSchedule::every(1000), // every 1 second
            payload: CronPayload::default(),
            state: CronJobState {
                next_run_at_ms: Some(0), // due now
                ..Default::default()
            },
            created_at_ms: 0,
            updated_at_ms: 0,
            delete_after_run: false,
        };
        svc.add_job(job);
        svc.start();

        let old_next = 0i64;
        svc.tick().await;

        let jobs = svc.list_jobs();
        assert_eq!(jobs.len(), 1);
        // Every-schedule should have recomputed next_run_at_ms
        assert!(jobs[0].state.next_run_at_ms.is_some());
        assert!(jobs[0].state.next_run_at_ms.unwrap() > old_next);
        assert!(jobs[0].enabled); // recurring job stays enabled
    }

    #[tokio::test]
    async fn test_cron_service_delete_after_run_removes_job() {
        let tmp = tempfile::tempdir().unwrap();
        let svc = CronService::new(tmp.path());

        let job = CronJob {
            id: "delete-1".to_string(),
            name: "one-shot".to_string(),
            enabled: true,
            schedule: CronSchedule::every(1000),
            payload: CronPayload::default(),
            state: CronJobState {
                next_run_at_ms: Some(0),
                ..Default::default()
            },
            created_at_ms: 0,
            updated_at_ms: 0,
            delete_after_run: true,
        };
        svc.add_job(job);

        // Don't call start() — it calls recompute_next_runs which would overwrite
        // next_run_at_ms to a future time. Just set running directly.
        svc.start();
        // Manually reset next_run_at_ms to make the job due
        {
            let mut guard = svc.jobs.lock().unwrap();
            if let Some(j) = guard.iter_mut().find(|j| j.id == "delete-1") {
                j.state.next_run_at_ms = Some(0);
            }
        }

        assert_eq!(svc.list_jobs().len(), 1);
        let jobs_before = svc.list_jobs();
        assert!(
            jobs_before[0].delete_after_run,
            "delete_after_run should be true"
        );
        svc.tick().await;

        // Job should have been removed
        assert_eq!(svc.list_jobs().len(), 0);
    }

    #[tokio::test]
    async fn test_cron_service_callback_is_invoked() {
        let tmp = tempfile::tempdir().unwrap();
        let svc = CronService::new(tmp.path());

        let called = std::sync::atomic::AtomicBool::new(false);
        let called_ref = std::sync::Arc::new(called);
        let called_for_callback = called_ref.clone();

        svc.set_on_job(Arc::new(move |_job| {
            let ref_clone = called_for_callback.clone();
            Box::pin(async move {
                ref_clone.store(true, std::sync::atomic::Ordering::Relaxed);
            })
        }));

        let job = CronJob {
            id: "cb-1".to_string(),
            name: "callback test".to_string(),
            enabled: true,
            schedule: CronSchedule::every(1000),
            payload: CronPayload::default(),
            state: CronJobState {
                next_run_at_ms: Some(0),
                ..Default::default()
            },
            created_at_ms: 0,
            updated_at_ms: 0,
            delete_after_run: false,
        };
        svc.add_job(job);
        svc.start();
        // start() calls recompute_next_runs which overwrites next_run_at_ms to a future time.
        // Reset it to make the job immediately due.
        {
            let mut guard = svc.jobs.lock().unwrap();
            if let Some(j) = guard.iter_mut().find(|j| j.id == "cb-1") {
                j.state.next_run_at_ms = Some(0);
            }
        }

        svc.tick().await;

        assert!(called_ref.load(std::sync::atomic::Ordering::Relaxed));
    }

    #[test]
    fn test_cron_service_recompute_next_runs() {
        let tmp = tempfile::tempdir().unwrap();
        let svc = CronService::new(tmp.path());

        let job = CronJob {
            id: "recompute-1".to_string(),
            name: "recompute test".to_string(),
            enabled: true,
            schedule: CronSchedule::every(5000),
            payload: CronPayload::default(),
            state: CronJobState::default(),
            created_at_ms: 0,
            updated_at_ms: 0,
            delete_after_run: false,
        };
        svc.add_job(job);

        // add_job() now computes next_run_at_ms
        let jobs = svc.list_jobs();
        assert!(jobs[0].state.next_run_at_ms.is_some());
        assert!(jobs[0].state.next_run_at_ms.unwrap() > now_ms());

        // After start, recompute_next_runs() may update it (same value for Every schedule)
        svc.start();
        let jobs = svc.list_jobs();
        assert!(jobs[0].state.next_run_at_ms.is_some());
        assert!(jobs[0].state.next_run_at_ms.unwrap() > now_ms());
    }

    #[test]
    fn test_cron_service_start_stop() {
        let tmp = tempfile::tempdir().unwrap();
        let svc = CronService::new(tmp.path());

        // Default is stopped
        assert!(!svc.running.load(std::sync::atomic::Ordering::Relaxed));

        svc.start();
        assert!(svc.running.load(std::sync::atomic::Ordering::Relaxed));

        svc.stop();
        assert!(!svc.running.load(std::sync::atomic::Ordering::Relaxed));
    }

    #[tokio::test]
    async fn test_cron_service_run_history_capped() {
        let tmp = tempfile::tempdir().unwrap();
        let svc = CronService::new(tmp.path());

        // Create a job that runs many times
        let job = CronJob {
            id: "history-1".to_string(),
            name: "history test".to_string(),
            enabled: true,
            schedule: CronSchedule::every(1000),
            payload: CronPayload::default(),
            state: CronJobState {
                next_run_at_ms: Some(0),
                ..Default::default()
            },
            created_at_ms: 0,
            updated_at_ms: 0,
            delete_after_run: false,
        };
        svc.add_job(job);
        svc.start();

        // Run more than MAX_RUN_HISTORY times
        for _ in 0..25 {
            // Reset next_run_at_ms to 0 so it's always due
            {
                let mut guard = svc.jobs.lock().unwrap();
                if let Some(j) = guard.iter_mut().find(|j| j.id == "history-1") {
                    j.state.next_run_at_ms = Some(0);
                }
            }
            svc.tick().await;
        }

        let jobs = svc.list_jobs();
        assert_eq!(jobs.len(), 1);
        assert!(jobs[0].state.run_history.len() <= MAX_RUN_HISTORY);
    }

    #[test]
    fn test_cron_service_load_corrupted_file() {
        let tmp = tempfile::tempdir().unwrap();
        let cron_dir = tmp.path().join("cron");
        std::fs::create_dir_all(&cron_dir).unwrap();
        std::fs::write(cron_dir.join("jobs.json"), "not valid json").unwrap();

        let svc = CronService::new(tmp.path());
        // Should not panic — should gracefully skip bad file
        svc.load();
        assert_eq!(svc.list_jobs().len(), 0);
    }

    #[test]
    fn test_compute_next_run_every_zero() {
        let schedule = CronSchedule::every(0);
        assert_eq!(compute_next_run(&schedule, now_ms()), None);
    }

    #[test]
    fn test_compute_next_run_every_negative() {
        let schedule = CronSchedule::every(-1000);
        assert_eq!(compute_next_run(&schedule, now_ms()), None);
    }

    #[tokio::test]
    async fn test_add_job_sets_next_run_at_ms() {
        let tmp = tempfile::tempdir().unwrap();
        let svc = CronService::new(tmp.path());

        let job = CronJob {
            id: "timing-1".to_string(),
            name: "timing test".to_string(),
            enabled: true,
            schedule: CronSchedule::every(1000),
            payload: CronPayload {
                kind: "test".to_string(),
                message: "execute".to_string(),
                deliver: false,
                channel: None,
                to: None,
            },
            state: CronJobState::default(),
            created_at_ms: 0,
            updated_at_ms: 0,
            delete_after_run: false,
        };
        svc.add_job(job);

        // next_run_at_ms should be set immediately after add_job, before start()
        let jobs = svc.list_jobs();
        assert!(
            jobs[0].state.next_run_at_ms.is_some(),
            "add_job must set next_run_at_ms so tick() can select the job"
        );
        assert!(jobs[0].state.next_run_at_ms.unwrap() > now_ms());
    }

    #[tokio::test]
    async fn test_tick_executes_job_added_via_add_job() {
        // End-to-end regression: a job added via add_job() (without manually
        // setting next_run_at_ms) must be picked up and executed by tick().
        let tmp = tempfile::tempdir().unwrap();
        let svc = CronService::new(tmp.path());

        let executed = std::sync::atomic::AtomicBool::new(false);
        let executed_ref = std::sync::Arc::new(executed);
        let executed_for_cb = executed_ref.clone();

        svc.set_on_job(Arc::new(move |job| {
            let ref_clone = executed_for_cb.clone();
            Box::pin(async move {
                ref_clone.store(true, std::sync::atomic::Ordering::Relaxed);
                drop(job);
            })
        }));

        let job = CronJob {
            id: "e2e-1".to_string(),
            name: "e2e timing test".to_string(),
            enabled: true,
            schedule: CronSchedule::every(60_000),
            payload: CronPayload {
                kind: "test".to_string(),
                message: "run".to_string(),
                deliver: false,
                channel: None,
                to: None,
            },
            state: CronJobState::default(),
            created_at_ms: 0,
            updated_at_ms: 0,
            delete_after_run: false,
        };
        svc.add_job(job);

        // Override next_run_at_ms to 0 so the job is immediately due,
        // AFTER start() since start() calls recompute_next_runs() which
        // would overwrite our manual value.
        svc.start();
        {
            let mut guard = svc.jobs.lock().unwrap();
            if let Some(j) = guard.iter_mut().find(|j| j.id == "e2e-1") {
                j.state.next_run_at_ms = Some(0);
            }
        }

        svc.tick().await;

        assert!(
            executed_ref.load(std::sync::atomic::Ordering::Relaxed),
            "tick() must execute a job that was added via add_job()"
        );
        let jobs = svc.list_jobs();
        assert_eq!(jobs.len(), 1);
        assert!(
            jobs[0].state.last_run_at_ms.is_some(),
            "job should have been executed"
        );
        assert_eq!(jobs[0].state.last_status, Some("ok".to_string()));
    }

    // ---- local-time scheduling + missed one-shots (ADR 0018) ----

    fn test_job(id: &str, schedule: CronSchedule) -> CronJob {
        CronJob {
            id: id.to_string(),
            name: format!("job {id}"),
            enabled: true,
            schedule,
            payload: CronPayload::default(),
            state: CronJobState::default(),
            created_at_ms: 0,
            updated_at_ms: 0,
            delete_after_run: false,
        }
    }

    fn counting_callback(counter: std::sync::Arc<std::sync::atomic::AtomicU32>) -> CronJobCallback {
        std::sync::Arc::new(move |_job| {
            let counter = counter.clone();
            Box::pin(async move {
                counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            })
        })
    }

    /// Local wall-clock `HH:MM:SS` of an epoch-ms instant.
    fn local_hms(ms: i64) -> String {
        chrono::DateTime::from_timestamp_millis(ms)
            .expect("valid epoch-ms")
            .with_timezone(&chrono::Local)
            .format("%H:%M:%S")
            .to_string()
    }

    #[test]
    fn test_compute_next_cron_resolves_local_wall_clock() {
        // "03:00:00 every day" must land on 03:00 of the machine's clock. A
        // UTC-resolved scheduler reads the expression against a different
        // clock, so the local time of the result differs whenever the host is
        // not at UTC+0.
        let now = now_ms();
        let next = compute_next_run(&CronSchedule::cron("0 0 3 * * *".to_string(), None), now)
            .expect("expression must parse");
        assert!(next > now, "{next} vs {now}");
        assert_eq!(local_hms(next), "03:00:00");
        assert!(
            next - now <= 48 * 3_600_000,
            "daily job must fire within a couple of days (DST gaps included), got {}ms",
            next - now
        );
    }

    #[test]
    fn test_cron_timezone_field_is_accepted_but_inert() {
        // `tz` is kept for config/JSON compatibility but never applied: the
        // machine's clock is the only reference (no chrono-tz in this crate).
        let with_tz = CronSchedule::cron(
            "0 0 3 * * *".to_string(),
            Some("America/New_York".to_string()),
        );
        let without_tz = CronSchedule::cron("0 0 3 * * *".to_string(), None);
        let now = now_ms();
        let next = compute_next_run(&with_tz, now).expect("expression must parse");
        assert_eq!(Some(next), compute_next_run(&without_tz, now));
        assert_eq!(local_hms(next), "03:00:00");
    }

    #[test]
    fn test_cron_expression_resolves_same_wall_time_across_paths() {
        // Behaviour lock for the UTC -> Local unification: the cron service and
        // the trigger layer must read one expression on one clock. Anchors cover
        // different days, mid-day phases and an exact second boundary.
        let anchors = [
            1_767_225_600_000i64,          // 2026-01-01T00:00:00Z
            1_767_225_600_000 + 3_599_000, // just before the next hour
            1_770_000_000_000,             // an exact minute boundary
            1_780_000_123_000,             // arbitrary mid-week instant
        ];
        let exprs = [
            "0 0 3 * * *",
            "0 */5 * * * *",
            "0 * * * * *",
            "30 0 9 * * Mon-Fri",
            "*/17 3 * * * *",
        ];
        for expr in exprs {
            let schedule = CronSchedule::cron(expr.to_string(), None);
            for anchor in anchors {
                assert_eq!(
                    compute_next_run(&schedule, anchor),
                    crate::trigger::subscription::judge::next_run_after(&schedule, anchor),
                    "expr '{expr}' must resolve identically at anchor {anchor}"
                );
            }
        }
    }

    #[tokio::test]
    async fn test_cron_service_add_job_schedules_local_wall_clock() {
        // The horizon persisted into jobs.json is a local-wall-clock moment.
        let tmp = tempfile::tempdir().unwrap();
        let svc = CronService::new(tmp.path());
        svc.add_job(test_job(
            "local-1",
            CronSchedule::cron("0 30 9 * * *".to_string(), None),
        ));

        let jobs = svc.list_jobs();
        let next = jobs[0]
            .state
            .next_run_at_ms
            .expect("a cron job must have a next run");
        assert_eq!(local_hms(next), "09:30:00");
    }

    #[tokio::test]
    async fn test_add_job_missed_one_shot_records_skipped_and_never_runs() {
        let tmp = tempfile::tempdir().unwrap();
        let svc = CronService::new(tmp.path());
        let fired = std::sync::Arc::new(std::sync::atomic::AtomicU32::new(0));
        svc.set_on_job(counting_callback(fired.clone()));

        let missed_at = now_ms() - ONE_SHOT_GRACE_MS - 60_000;
        svc.add_job(test_job("missed-1", CronSchedule::at(missed_at)));
        svc.start();
        svc.tick().await;

        assert_eq!(
            fired.load(std::sync::atomic::Ordering::SeqCst),
            0,
            "a one-shot whose window is gone must never be back-run"
        );
        let jobs = svc.list_jobs();
        assert_eq!(jobs[0].state.last_status.as_deref(), Some("skipped"));
        assert!(!jobs[0].enabled, "a skipped one-shot is disabled");
        assert_eq!(jobs[0].state.next_run_at_ms, None);
        assert!(
            jobs[0]
                .state
                .run_history
                .iter()
                .any(|r| r.status == "skipped"),
            "the skip must be visible in run history: {:?}",
            jobs[0].state.run_history
        );

        // A second tick changes nothing: one skip record, still no execution.
        svc.tick().await;
        let jobs = svc.list_jobs();
        assert_eq!(fired.load(std::sync::atomic::Ordering::SeqCst), 0);
        assert_eq!(
            jobs[0]
                .state
                .run_history
                .iter()
                .filter(|r| r.status == "skipped")
                .count(),
            1,
            "a skipped one-shot must not be re-recorded on every tick"
        );
    }

    #[tokio::test]
    async fn test_cron_tick_records_skipped_for_one_shot_missed_while_down() {
        // Downtime: the job persisted a horizon an hour in the past. Coming back
        // up records `skipped` once and never fires the callback.
        let tmp = tempfile::tempdir().unwrap();
        let cron_dir = tmp.path().join("cron");
        std::fs::create_dir_all(&cron_dir).unwrap();
        let missed_at = now_ms() - 3_600_000;
        let mut job = test_job("down-1", CronSchedule::at(missed_at));
        job.delete_after_run = true; // the cron tool marks one-shots this way
        job.state.next_run_at_ms = Some(missed_at);
        let store = CronStore {
            version: 1,
            jobs: vec![job],
        };
        std::fs::write(
            cron_dir.join("jobs.json"),
            serde_json::to_string(&store).unwrap(),
        )
        .unwrap();

        let svc = CronService::new(tmp.path());
        svc.load();
        let fired = std::sync::Arc::new(std::sync::atomic::AtomicU32::new(0));
        svc.set_on_job(counting_callback(fired.clone()));
        svc.start();
        svc.tick().await;

        assert_eq!(fired.load(std::sync::atomic::Ordering::SeqCst), 0);
        let jobs = svc.list_jobs();
        assert_eq!(
            jobs.len(),
            1,
            "the skipped job stays on disk (disabled) as the audit trail"
        );
        assert!(!jobs[0].enabled);
        assert_eq!(jobs[0].state.last_status.as_deref(), Some("skipped"));
        assert_eq!(jobs[0].state.next_run_at_ms, None);
        assert!(
            jobs[0].state.last_run_at_ms.is_none(),
            "nothing ran, so there is no last run"
        );
        assert_eq!(jobs[0].state.run_history.len(), 1);
        assert_eq!(jobs[0].state.run_history[0].status, "skipped");
        assert_eq!(jobs[0].state.run_history[0].run_at_ms, missed_at);
    }

    #[tokio::test]
    async fn test_cron_tick_runs_one_shot_inside_the_grace_window() {
        // Only windows missed by more than the grace are skipped: a tick that is
        // a little late still delivers the one-shot.
        let tmp = tempfile::tempdir().unwrap();
        let svc = CronService::new(tmp.path());
        let fired = std::sync::Arc::new(std::sync::atomic::AtomicU32::new(0));
        svc.set_on_job(counting_callback(fired.clone()));

        svc.add_job(test_job("late-1", CronSchedule::at(now_ms() - 1_000)));
        svc.start();
        svc.tick().await;

        assert_eq!(
            fired.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "a one-shot inside the grace window must still run"
        );
        let jobs = svc.list_jobs();
        assert_eq!(jobs[0].state.last_status.as_deref(), Some("ok"));
        assert!(!jobs[0].enabled, "a one-shot disables itself after running");
        assert_eq!(jobs[0].state.next_run_at_ms, None);
    }
}
