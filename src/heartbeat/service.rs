use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use crate::debug;

/// Callback type for heartbeat execution.
pub type HeartbeatExecuteCb = Arc<
    dyn Fn(String) -> std::pin::Pin<Box<dyn std::future::Future<Output = String> + Send>>
        + Send
        + Sync,
>;
pub type HeartbeatNotifyCb = Arc<
    dyn Fn(String) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>>
        + Send
        + Sync,
>;

pub struct HeartbeatService {
    workspace_dir: PathBuf,
    interval_s: u64,
    enabled: bool,
    running: AtomicBool,
    on_execute: Option<HeartbeatExecuteCb>,
    on_notify: Option<HeartbeatNotifyCb>,
}

impl HeartbeatService {
    pub fn new(workspace_dir: PathBuf, interval_s: u64, enabled: bool) -> Self {
        Self {
            workspace_dir,
            interval_s,
            enabled,
            running: AtomicBool::new(false),
            on_execute: None,
            on_notify: None,
        }
    }

    pub fn set_on_execute(&mut self, cb: HeartbeatExecuteCb) {
        self.on_execute = Some(cb);
    }

    pub fn set_on_notify(&mut self, cb: HeartbeatNotifyCb) {
        self.on_notify = Some(cb);
    }

    pub fn heartbeat_file(&self) -> PathBuf {
        self.workspace_dir.join("HEARTBEAT.md")
    }

    pub fn read_heartbeat_content(&self) -> Option<String> {
        let path = self.heartbeat_file();
        if path.exists() {
            std::fs::read_to_string(&path)
                .ok()
                .filter(|s| !s.trim().is_empty())
        } else {
            None
        }
    }

    pub fn start(&self) {
        if !self.enabled {
            debug!("[heartbeat] disabled");
            return;
        }
        self.running.store(true, Ordering::Relaxed);
    }

    pub fn stop(&self) {
        self.running.store(false, Ordering::Relaxed);
    }

    pub fn is_enabled(&self) -> bool {
        self.enabled
    }

    /// Execute a single heartbeat tick. Called by the gateway loop on interval.
    pub async fn tick(&self) {
        if !self.running.load(Ordering::Relaxed) {
            return;
        }

        let content = match self.read_heartbeat_content() {
            Some(c) => c,
            None => {
                debug!("[heartbeat] HEARTBEAT.md missing or empty");
                return;
            }
        };

        if let Some(ref cb) = self.on_execute {
            let result = cb(content).await;
            if !result.is_empty() {
                if let Some(ref notify) = self.on_notify {
                    notify(result).await;
                }
            }
        }
    }

    /// Sleep duration for the heartbeat interval.
    pub fn sleep_duration(&self) -> std::time::Duration {
        std::time::Duration::from_secs(self.interval_s)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_heartbeat_disabled() {
        let tmp = tempfile::tempdir().unwrap();
        let svc = HeartbeatService::new(tmp.path().to_path_buf(), 60, false);
        assert!(!svc.is_enabled());
        svc.start();
        assert!(!svc.running.load(Ordering::Relaxed));
    }

    #[test]
    fn test_heartbeat_missing_file() {
        let tmp = tempfile::tempdir().unwrap();
        let svc = HeartbeatService::new(tmp.path().to_path_buf(), 60, true);
        svc.start();
        assert!(svc.is_enabled());
        assert!(svc.read_heartbeat_content().is_none());
    }

    #[test]
    fn test_heartbeat_file_read() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("HEARTBEAT.md"), "task list").unwrap();
        let svc = HeartbeatService::new(tmp.path().to_path_buf(), 60, true);
        assert_eq!(svc.read_heartbeat_content(), Some("task list".to_string()));
    }

    #[test]
    fn test_heartbeat_empty_file() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("HEARTBEAT.md"), "   ").unwrap();
        let svc = HeartbeatService::new(tmp.path().to_path_buf(), 60, true);
        assert!(svc.read_heartbeat_content().is_none());
    }

    #[test]
    fn test_heartbeat_sleep_duration() {
        let tmp = tempfile::tempdir().unwrap();
        let svc = HeartbeatService::new(tmp.path().to_path_buf(), 120, true);
        assert_eq!(svc.sleep_duration(), std::time::Duration::from_secs(120));
    }

    #[test]
    fn test_heartbeat_start_when_disabled() {
        let tmp = tempfile::tempdir().unwrap();
        let svc = HeartbeatService::new(tmp.path().to_path_buf(), 60, false);
        svc.start();
        // start() should not set running when disabled
        assert!(!svc.running.load(Ordering::Relaxed));
    }

    #[test]
    fn test_heartbeat_start_stop() {
        let tmp = tempfile::tempdir().unwrap();
        let svc = HeartbeatService::new(tmp.path().to_path_buf(), 60, true);
        assert!(!svc.running.load(Ordering::Relaxed));
        svc.start();
        assert!(svc.running.load(Ordering::Relaxed));
        svc.stop();
        assert!(!svc.running.load(Ordering::Relaxed));
    }

    #[test]
    fn test_heartbeat_tick_when_not_running() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("HEARTBEAT.md"), "do something").unwrap();
        let svc = HeartbeatService::new(tmp.path().to_path_buf(), 60, true);
        // Not started — tick should be a no-op
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            svc.tick().await;
        });
        // No panic, no callback called
    }

    #[test]
    fn test_heartbeat_tick_missing_file() {
        let tmp = tempfile::tempdir().unwrap();
        let svc = HeartbeatService::new(tmp.path().to_path_buf(), 60, true);
        svc.start();
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            svc.tick().await;
        });
        // Should not panic when file is missing
    }

    #[tokio::test]
    async fn test_heartbeat_tick_executes_callback() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("HEARTBEAT.md"), "check status").unwrap();
        let mut svc = HeartbeatService::new(tmp.path().to_path_buf(), 60, true);

        let executed = std::sync::Arc::new(tokio::sync::Mutex::new(String::new()));
        let exec_clone = executed.clone();

        svc.set_on_execute(Arc::new(move |content| {
            let c = exec_clone.clone();
            Box::pin(async move {
                *c.lock().await = content;
                "result".to_string()
            })
        }));

        let notified = std::sync::Arc::new(tokio::sync::Mutex::new(String::new()));
        let notif_clone = notified.clone();

        svc.set_on_notify(Arc::new(move |result| {
            let n = notif_clone.clone();
            Box::pin(async move {
                *n.lock().await = result;
            })
        }));

        svc.start();
        svc.tick().await;

        assert_eq!(*executed.lock().await, "check status");
        assert_eq!(*notified.lock().await, "result");
    }

    #[tokio::test]
    async fn test_heartbeat_tick_empty_result_skips_notify() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("HEARTBEAT.md"), "content").unwrap();
        let mut svc = HeartbeatService::new(tmp.path().to_path_buf(), 60, true);

        let notified = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let notif_clone = notified.clone();

        svc.set_on_execute(Arc::new(move |_content| {
            Box::pin(async move {
                "".to_string() // empty result
            })
        }));

        svc.set_on_notify(Arc::new(move |_result| {
            let n = notif_clone.clone();
            Box::pin(async move {
                n.store(true, std::sync::atomic::Ordering::Relaxed);
            })
        }));

        svc.start();
        svc.tick().await;

        // Notify should NOT have been called
        assert!(!notified.load(std::sync::atomic::Ordering::Relaxed));
    }

    #[tokio::test]
    async fn test_heartbeat_tick_no_callback_set() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("HEARTBEAT.md"), "content").unwrap();
        let svc = HeartbeatService::new(tmp.path().to_path_buf(), 60, true);
        svc.start();
        // Should not panic when on_execute is None
        svc.tick().await;
    }
}
