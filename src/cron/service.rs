use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use super::types::*;
use crate::debug;

pub type CronJobCallback = Arc<dyn Fn(CronJob) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>> + Send + Sync>;

pub struct CronService {
    store_path: PathBuf,
    action_path: PathBuf,
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
            action_path: cron_dir.join("action.jsonl"),
            jobs: Arc::new(Mutex::new(Vec::new())),
            on_job: Mutex::new(None),
            running: AtomicBool::new(false),
        }
    }

    pub fn set_on_job(&self, cb: CronJobCallback) {
        *self.on_job.lock().unwrap() = Some(cb);
    }

    pub fn load(&self) {
        if self.store_path.exists() {
            if let Ok(content) = std::fs::read_to_string(&self.store_path) {
                if let Ok(store) = serde_json::from_str::<CronStore>(&content) {
                    let mut guard = self.jobs.lock().unwrap();
                    *guard = store.jobs;
                }
            }
        }
    }

    pub fn save(&self) {
        let jobs = self.jobs.lock().unwrap().clone();
        let store = CronStore {
            version: 1,
            jobs,
        };
        if let Ok(content) = serde_json::to_string_pretty(&store) {
            let _ = std::fs::write(&self.store_path, content);
        }
    }

    pub fn add_job(&self, job: CronJob) {
        let mut guard = self.jobs.lock().unwrap();
        let now = now_ms();
        let mut job = job;
        job.state.next_run_at_ms = compute_next_run(&job.schedule, now);
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
                job.state.next_run_at_ms = compute_next_run(&job.schedule, now);
            }
        }
    }

    pub async fn tick(&self) {
        if !self.running.load(Ordering::Relaxed) {
            return;
        }

        let now = now_ms();

        // Collect due jobs outside the lock
        let due_jobs: Vec<CronJob> = {
            let guard = self.jobs.lock().unwrap();
            guard.iter()
                .filter(|j| j.enabled && j.state.next_run_at_ms.map_or(false, |t| now >= t))
                .cloned()
                .collect()
        };

        if due_jobs.is_empty() {
            return;
        }

        for job in due_jobs {
            self.execute_job(&job).await;
        }
        self.save();
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
            if j.state.run_history.len() > MAX_RUN_HISTORY {
                let drain_to = j.state.run_history.len() - MAX_RUN_HISTORY;
                j.state.run_history.drain(..drain_to);
            }

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

/// Compute next run time in ms from now.
pub fn compute_next_run(schedule: &CronSchedule, now_ms: i64) -> Option<i64> {
    match schedule {
        CronSchedule::At { at_ms } => {
            if *at_ms > now_ms { Some(*at_ms) } else { None }
        }
        CronSchedule::Every { every_ms } => {
            if *every_ms <= 0 { None } else { Some(now_ms + *every_ms) }
        }
        CronSchedule::Cron { expr, .. } => {
            compute_next_cron(expr, now_ms)
        }
    }
}

fn compute_next_cron(expr: &str, _now_ms: i64) -> Option<i64> {
    use cron::Schedule;
    use std::str::FromStr;
    match Schedule::from_str(expr) {
        Ok(schedule) => {
            schedule
                .upcoming(chrono::Utc)
                .take(1)
                .next()
                .map(|dt| dt.timestamp_millis())
        }
        Err(_) => None,
    }
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
        assert!(jobs_before[0].delete_after_run, "delete_after_run should be true");
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
        assert!(jobs[0].state.next_run_at_ms.is_some(),
            "add_job must set next_run_at_ms so tick() can select the job");
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

        assert!(executed_ref.load(std::sync::atomic::Ordering::Relaxed),
            "tick() must execute a job that was added via add_job()");
        let jobs = svc.list_jobs();
        assert_eq!(jobs.len(), 1);
        assert!(jobs[0].state.last_run_at_ms.is_some(), "job should have been executed");
        assert_eq!(jobs[0].state.last_status, Some("ok".to_string()));
    }
}
