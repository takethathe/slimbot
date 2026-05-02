# Worker Pool

对应模块：`src/worker.rs`

## 概述

`Worker` 是一个单线程异步执行器，按 FIFO 顺序处理任务。`WorkerPool` 是动态工作池，根据负载自动扩展和缩减 Worker 数量。

## Worker

```rust
pub struct Worker {
    queue_tx: mpsc::UnboundedSender<Box<dyn FnOnce() -> BoxFuture + Send>>,
    shutdown: Arc<Notify>,
    id: usize,
    queue_len: Arc<AtomicUsize>,
}
```

### 特点

- 内部使用 `tokio::spawn` 启动消息循环
- 任务按 FIFO 顺序执行
- `submit()` 接受 `Box<dyn FnOnce() -> BoxFuture + Send>` 闭包
- `close()` 优雅关闭：排空队列中剩余任务后退出

## WorkerPool

```rust
pub struct WorkerPool {
    max_workers: usize,
    workers: RwLock<Vec<WorkerEntry>>,
}
```

### 全局单例

```rust
WorkerPool::init_global(64);  // 启动时调用
let pool = WorkerPool::global();  // 后续使用
```

### 调度策略

`pool.submit(closure)` 的行为：

1. **查找空闲 Worker**：`queue_len() == 0` 的 Worker
2. **创建新 Worker**：如果没有空闲且总数 < `max_workers`
3. **排队到最闲 Worker**：已达上限时，选择 `queue_len` 最小的

### 空闲回收

后台定时任务（每 30 秒）回收空闲超过 120 秒且无待处理任务的 Worker。

## 使用场景

WorkerPool 为需要顺序执行且可能并发的异步任务提供执行层。ReAct 循环中的任务通过此池调度，保证同一会话内的操作按顺序执行。
