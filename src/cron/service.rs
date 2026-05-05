use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use super::types::*;
use crate::debug;

pub type CronJobCallback = Arc<dyn Fn(CronJob) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>> + Send + Sync>;

pub struct CronService {
    store_path: PathBuf,
    action_path: PathBuf,
    jobs: Arc<std::sync::Mutex<Vec<CronJob>>>,
    on_job: Option<CronJobCallback>,
    running: AtomicBool,
}

impl CronService {
    pub fn new(workspace_dir: &Path) -> Self {
        let cron_dir = workspace_dir.join("cron");
        std::fs::create_dir_all(&cron_dir).ok();
        Self {
            store_path: cron_dir.join("jobs.json"),
            action_path: cron_dir.join("action.jsonl"),
            jobs: Arc::new(std::sync::Mutex::new(Vec::new())),
            on_job: None,
            running: AtomicBool::new(false),
        }
    }

    pub fn set_on_job(&mut self, cb: CronJobCallback) {
        self.on_job = Some(cb);
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

        for job in due_jobs {
            self.execute_job(&job).await;
        }
        self.save();
    }

    async fn execute_job(&self, job: &CronJob) {
        let start = now_ms();
        debug!("[cron] executing job '{}' ({})", job.name, job.id);

        // Execute callback outside the lock to avoid deadlock
        if let Some(ref cb) = self.on_job {
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
            let now = chrono::Utc::now();
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
}
