# MessageBus

对应模块：`src/message_bus.rs`

## 概述

`MessageBus` 是纯异步 mpsc 通道端点，**不启动任何后台任务**。它仅为通道与 AgentLoop 之间提供消息传递通道，具体的任务处理逻辑由 AgentLoop 负责。每个 mpsc receiver 按所有权移交唯一消费者（inbound → AgentLoop，outbound → ChannelManager），接收端无需锁。

## 结构

```rust
pub struct MessageBus {
    inbound_tx: mpsc::Sender<BusRequest>,
    outbound_tx: mpsc::Sender<BusResult>,
}

pub struct MessageBusReceivers {
    pub inbound: mpsc::Receiver<BusRequest>,
    pub outbound: mpsc::Receiver<BusResult>,
}

pub struct BusRequest {
    pub session_id: String,
    pub content: String,
    pub channel_inject: Option<String>,
    pub hook: TaskHook,
}

pub struct BusResult {
    pub session_id: String,
    pub task_id: String,
    pub content: String,
}
```

## 构造

`new()` 返回共享 bus 与按所有权移交的两个 receiver：

```rust
let (message_bus, receivers) = MessageBus::new();
// receivers.inbound  → AgentLoop
// receivers.outbound → ChannelManager
```

每个 receiver 在 `Option<Receiver>` 中按 `Mutex` 一-shot 取用（`Mutex` 仅在启动时取一次，不跨 `await` 持有，无 per-recv 锁开销）。重复 take 触发 `expect` panic，编译器 + 运行期双重保证单消费者契约。

## 端点方法

### `publish_inbound(req)` / `publish_outbound(res)` — async

推荐的发送侧封装。调用方无需触碰 `mpsc::Sender` 类型。内部忽略已关闭通道错误（所有消费者已 drop 时）。

```rust
bus.publish_inbound(request).await;
bus.publish_outbound(result).await;
```

### `inbound_tx()` / `outbound_tx()` — 裸 sender

保留给确实需要 `'static` sender 的场景：
- `ShutdownHandle` 的 `try_send`（同步 stdin 线程不能用 async publish）
- `message_tool` 的 `set_send_callback` 闭包（需 `'static`）
- `IoScheduler` 内部持有 sender 做命令拦截（有状态调度器）

```rust
let tx = message_bus.inbound_tx();
tx.try_send(request)?; // 或 tx.send(req).await
```

### `MessageBusReceivers { inbound, outbound }` — 按所有权移交

`new()` 返回时按值移交。消费者启动时各取一次（`start_inbound` / `run` / `run_with_shutdown`）。消费循环直接 `rx.recv().await`，无锁。

```rust
// 在 AgentLoop::start_inbound
let mut inbound_rx = self.inbound_rx.lock().unwrap().take()
    .expect("inbound_rx already consumed");
while let Some(req) = inbound_rx.recv().await { ... }
```

## 通道容量

- `INBOUND_CAPACITY`: 32
- `OUTBOUND_CAPACITY`: 32

通道容量足够大以吸收突发请求。如果 inbound 满了，通道的 I/O 循环会在 `send` 上阻塞，暂停读取用户输入直到空间释放。

## 设计原则

MessageBus 是**纯通道**，不包含任何业务逻辑：
- 不创建 session
- 不执行 AgentRunner
- 不启动后台任务

所有处理逻辑由 AgentLoop 的 `start_inbound()` 和 ChannelManager 的 `run()` 分别监听 inbound/outbound 端点。

## 与组件的关系

```
Channel ──publish_inbound / inbound_tx──▶ MessageBus ──owned inbound_rx──▶ AgentLoop (处理)
                                                                                 │
                                                                    ──publish_outbound / outbound_tx──▶ MessageBus
                                                                                                              │
ChannelManager ◀──owned outbound_rx──◀◀◀◀◀◀◀◀◀◀◀◀◀◀◀◀◀◀◀◀◀◀◀◀◀◀◀◀◀◀◀◀◀◀◀◀◀◀◀◀◀◀◀◀│
```
