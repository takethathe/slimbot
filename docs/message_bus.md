# MessageBus

对应模块：`src/message_bus.rs`

## 概述

`MessageBus` 是纯异步 mpsc 通道端点，**不启动任何后台任务**。它仅为通道与 AgentLoop 之间提供消息传递通道，具体的任务处理逻辑由 AgentLoop 负责。每条 mpsc receiver 按所有权移交唯一消费者（inbound → AgentLoop，outbound / media → ChannelManager），接收端无需锁。

## 结构

```rust
pub struct MessageBus {
    inbound_tx: mpsc::Sender<BusRequest>,
    outbound_tx: mpsc::Sender<BusResult>,
    media_tx: mpsc::Sender<MediaDelivery>,
}

pub struct MessageBusReceivers {
    pub inbound: mpsc::Receiver<BusRequest>,
    pub outbound: mpsc::Receiver<BusResult>,
    pub media: mpsc::Receiver<MediaDelivery>,
}

pub struct BusRequest {
    pub session_id: String,
    pub content: String,
    pub channel_inject: Option<String>,
    pub attachments: Vec<ContentBlock>,
    pub hook: TaskHook,
}

pub struct BusResult {
    pub session_id: String,
    pub run_id: String,   // 产出该结果的 Run；卡片的归属键
    pub content: String,
}
```

`media` 是独立的第三条通道（`MediaDelivery { session_id, media, caption }`），承载 `OutboundMedia::Image` / `OutboundMedia::File` 的本地路径：媒体按句柄走，字节由拥有该通道的一方自己上传，不过 bus（ADR 0017）。

## 构造

`new()` 返回共享 bus 与按所有权移交的三个 receiver：

```rust
let (message_bus, receivers) = MessageBus::new();
// receivers.inbound  → AgentLoop
// receivers.outbound → ChannelManager
// receivers.media    → ChannelManager（卡片路径旁的专用路由器）
```

bus 本身不再持有 receiver：三个接收端在 `new()` 时按值交回，由调用方沿构造链分发。`ChannelManager` 把 `outbound` / `media` 存进 `Mutex<Option<Receiver>>`，只在自己的 `run()` / `run_with_shutdown()` 里 `take()` 一次（`Mutex` 仅在启动时取一次，不跨 `await` 持有；重复 take 触发 `expect` panic，编译期 + 运行期双重保证单消费者契约）。`inbound` 则直接以值传给 `AgentLoop`。

## 端点方法

### `publish_inbound(req)` / `publish_outbound(res)` / `publish_media(delivery)` — async

推荐的发送侧封装。调用方无需触碰 `mpsc::Sender` 类型。内部忽略已关闭通道错误（所有消费者已 drop 时）。

```rust
bus.publish_inbound(request).await;
bus.publish_outbound(result).await;
bus.publish_media(delivery).await;
```

### `inbound_tx()` / `outbound_tx()` / `media_tx()` — 裸 sender

保留给确实需要 `'static` sender 的场景：
- `ShutdownHandle` 的 `try_send`（同步 stdin 线程不能用 async publish）
- `message_tool` 的 `set_send_callback` 闭包（需 `'static`）
- `IoScheduler` 内部持有 sender 做命令拦截（有状态调度器）

```rust
let tx = message_bus.inbound_tx();
tx.try_send(request)?; // 或 tx.send(req).await
```

### `MessageBusReceivers { inbound, outbound, media }` — 按所有权移交

`new()` 返回时按值移交。消费者启动时各取一次（`AgentLoop::from_config` 直接拿走 `inbound`；`ChannelManager` 的 `run` / `run_with_shutdown` 各 `take` 一次）。消费循环直接 `rx.recv().await`，无锁。

```rust
// 在 ChannelManager::run
let mut outbound_rx = self.take_outbound_rx();
while let Some(result) = outbound_rx.recv().await { ... }
```

## 通道容量

- `INBOUND_CAPACITY`: 32
- `OUTBOUND_CAPACITY`: 32（`outbound` 与 `media` 共用这个上限）

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
