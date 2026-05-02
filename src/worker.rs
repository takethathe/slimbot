use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, OnceLock, RwLock};
use std::time::Instant;

use tokio::sync::{mpsc, Notify};

/// Type-erased async future returning ().
pub type BoxFuture = Pin<Box<dyn Future<Output = ()> + Send>>;

/// Type-erased async future returning T.
pub type BoxFutureResult<T> = Pin<Box<dyn Future<Output = T> + Send>>;

/// A single-threaded async executor that processes tasks in FIFO order.
pub struct Worker {
    queue_tx: mpsc::UnboundedSender<Box<dyn FnOnce() -> BoxFuture + Send>>,
    shutdown: Arc<Notify>,
    id: usize,
    queue_len: Arc<AtomicUsize>,
}

static WORKER_COUNTER: AtomicUsize = AtomicUsize::new(0);

impl Worker {
    /// Create a new Worker and spawn its internal message loop.
    pub fn new() -> Self {
        let (queue_tx, mut queue_rx) = mpsc::unbounded_channel::<Box<dyn FnOnce() -> BoxFuture + Send>>();
        let shutdown = Arc::new(Notify::new());
        let shutdown_clone = shutdown.clone();
        let id = WORKER_COUNTER.fetch_add(1, Ordering::Relaxed);
        let queue_len = Arc::new(AtomicUsize::new(0));

        tokio::spawn({
            let queue_len = queue_len.clone();
            async move {
                loop {
                    tokio::select! {
                        biased;
                        _ = shutdown_clone.notified() => {
                            while let Ok(cb) = queue_rx.try_recv() {
                                queue_len.fetch_sub(1, Ordering::SeqCst);
                                cb().await;
                            }
                            break;
                        }
                        Some(cb) = queue_rx.recv() => {
                            queue_len.fetch_sub(1, Ordering::SeqCst);
                            cb().await;
                        }
                    }
                }
            }
        });

        Self {
            queue_tx,
            shutdown,
            id,
            queue_len,
        }
    }

    /// Submit a closure to be executed in FIFO order.
    pub fn submit(&self, f: Box<dyn FnOnce() -> BoxFuture + Send>) {
        self.queue_len.fetch_add(1, Ordering::SeqCst);
        let _ = self.queue_tx.send(f);
    }

    /// Explicitly shut down this worker. Drains remaining queued tasks before exiting.
    pub fn close(self) {
        self.shutdown.notify_waiters();
    }

    /// Number of items in this worker's queue.
    pub fn queue_len(&self) -> usize {
        self.queue_len.load(Ordering::Relaxed)
    }

    pub fn id(&self) -> usize {
        self.id
    }
}

impl Clone for Worker {
    fn clone(&self) -> Self {
        Self {
            queue_tx: self.queue_tx.clone(),
            shutdown: self.shutdown.clone(),
            id: self.id,
            queue_len: self.queue_len.clone(),
        }
    }
}

/// Dynamic pool of Workers with a global singleton.
/// Manages 1..=max_workers workers, auto-scaling based on load.
pub struct WorkerPool {
    max_workers: usize,
    workers: RwLock<Vec<WorkerEntry>>,
}

struct WorkerEntry {
    worker: Worker,
    last_active: Instant,
}

static GLOBAL_POOL: OnceLock<WorkerPool> = OnceLock::new();

impl WorkerPool {
    /// Initialize the global singleton. Must be called once before `global()`.
    pub fn init_global(max_workers: usize) {
        let _ = GLOBAL_POOL.set(WorkerPool::new(max_workers));
    }

    /// Get the global WorkerPool instance. Panics if `init_global` was not called.
    pub fn global() -> &'static WorkerPool {
        GLOBAL_POOL
            .get()
            .expect("WorkerPool::global() called before init_global()")
    }

    fn new(max_workers: usize) -> Self {
        let pool = Self {
            max_workers,
            workers: RwLock::new(Vec::new()),
        };

        // Spawn idle reaper
        let pool_ref: &'static WorkerPool = unsafe { &*(&pool as *const _) };
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(std::time::Duration::from_secs(30)).await;
                pool_ref.reap_idle_workers();
            }
        });

        pool
    }

    /// Submit a closure. The pool finds an idle worker or creates a new one
    /// (up to max_workers). If all workers are busy, queues to the least busy one.
    pub fn submit(&self, f: Box<dyn FnOnce() -> BoxFuture + Send>) {
        // Try to find an idle worker
        {
            let guard = self.workers.read().unwrap();
            for entry in guard.iter() {
                if entry.worker.queue_len() == 0 {
                    entry.worker.submit(f);
                    return;
                }
            }
        }

        // No idle worker — try to create a new one
        {
            let mut guard = self.workers.write().unwrap();
            if guard.len() < self.max_workers {
                let worker = Worker::new();
                worker.submit(f);
                guard.push(WorkerEntry {
                    worker,
                    last_active: Instant::now(),
                });
                return;
            }
        }

        // At capacity — submit to worker with the smallest queue
        {
            let guard = self.workers.read().unwrap();
            if let Some(entry) = guard.iter().min_by_key(|e| e.worker.queue_len()) {
                entry.worker.submit(f);
            }
        }
    }

    fn reap_idle_workers(&self) {
        let mut guard = self.workers.write().unwrap();
        guard.retain(|entry| {
            // Keep workers that have pending messages or were active recently
            if entry.worker.queue_len() > 0 {
                return true;
            }
            let idle_duration = entry.last_active.elapsed();
            idle_duration < std::time::Duration::from_secs(120)
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;

    #[tokio::test]
    async fn test_worker_executes_in_order() {
        let order = Arc::new(std::sync::Mutex::new(Vec::new()));
        let worker = Worker::new();

        for i in 0..5 {
            let order = order.clone();
            worker.submit(Box::new(move || {
                let order = order.clone();
                Box::pin(async move {
                    order.lock().unwrap().push(i);
                })
            }));
        }

        // Give the worker time to process
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        worker.close();

        let result = order.lock().unwrap().clone();
        assert_eq!(result, vec![0, 1, 2, 3, 4]);
    }

    #[tokio::test]
    async fn test_worker_close_drains_queue() {
        let counter = Arc::new(AtomicUsize::new(0));

        let worker = Worker::new();
        for _ in 0..3 {
            let counter = counter.clone();
            worker.submit(Box::new(move || {
                let counter = counter.clone();
                Box::pin(async move {
                    counter.fetch_add(1, Ordering::SeqCst);
                })
            }));
        }

        worker.close();
        // After close, all tasks should have been drained
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        assert_eq!(counter.load(Ordering::SeqCst), 3);
    }

    #[tokio::test]
    async fn test_pool_scales_under_load() {
        let pool = WorkerPool::new(4);

        let counter = Arc::new(AtomicUsize::new(0));
        for _ in 0..4 {
            let counter = counter.clone();
            pool.submit(Box::new(move || {
                let counter = counter.clone();
                Box::pin(async move {
                    counter.fetch_add(1, Ordering::SeqCst);
                })
            }));
        }

        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        let workers_count = pool.workers.read().unwrap().len();
        assert_eq!(workers_count, 4); // Should have created 4 workers
        assert_eq!(counter.load(Ordering::SeqCst), 4);
    }

    #[tokio::test]
    async fn test_pool_respects_max_workers() {
        let pool = WorkerPool::new(2);

        // Submit 4 tasks — only 2 workers should be created
        for _ in 0..4 {
            pool.submit(Box::new(|| {
                Box::pin(async { tokio::time::sleep(std::time::Duration::from_millis(20)).await })
            }));
        }

        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        let workers_count = pool.workers.read().unwrap().len();
        assert_eq!(workers_count, 2); // Capped at max_workers
    }

    #[tokio::test]
    async fn test_global_pool_init_and_access() {
        let _ = WorkerPool::init_global(64);
        let pool = WorkerPool::global();
        assert_eq!(pool.max_workers, 64);
    }
}
