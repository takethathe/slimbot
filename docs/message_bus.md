# MessageBus

对应模块：`src/message_bus.rs`

## 概述

`MessageBus` 是纯异步 mpsc 通道端点，**不启动任何后台任务**。它仅为通道与 AgentLoop 之间提供消息传递通道，具体的任务处理逻辑由 AgentLoop 负责。

## 结构

```rust
pub struct MessageBus {
    inbound_tx: mpsc::Sender<BusRequest>,
    inbound_rx: Arc<tokio::sync::Mutex<mpsc::Receiver<BusRequest>>>,
    outbound_tx: mpsc::Sender<BusResult>,
    outbound_rx: Arc<tokio::sync::Mutex<mpsc::Receiver<BusResult>>>,
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

## 端点方法

### `inbound_tx()`

返回 `mpsc::Sender<BusRequest>`，通道使用此端点提交用户请求。

```rust
let tx = message_bus.inbound_tx();
tx.send(request).await?;
```

### `inbound_rx()`

返回 `Arc<Mutex<Receiver<BusRequest>>>`，AgentLoop 使用此端点监听入站请求。

```rust
let rx = message_bus.inbound_rx();
while let Some(request) = rx.lock().await.recv().await {
    // 处理请求
}
```

### `outbound_tx()`

返回 `mpsc::Sender<BusResult>`，AgentLoop 使用此端点发布处理结果。

```rust
let tx = message_bus.outbound_tx();
tx.send(result).await?;
```

### `outbound_rx()`

返回 `Arc<Mutex<Receiver<BusResult>>>`，ChannelManager 使用此端点监听出站结果并路由到对应通道。

```rust
let rx = message_bus.outbound_rx();
while let Some(result) = rx.lock().await.recv().await {
    // 根据 session_id 提取 channel_id，路由到对应通道
}
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

所有处理逻辑由 AgentLoop 的 `start_inbound()` 和 ChannelManager 的 `run()` 分别负责监听 inbound/outbound 端点。

## 与组件的关系

```
Channel ──inbound_tx──▶ MessageBus ──inbound_rx──▶ AgentLoop (处理)
                                                    │
                                         ──outbound_tx──▶ MessageBus
                                                              │
ChannelManager ◀──outbound_rx──◀◀◀◀◀◀◀◀◀◀◀◀◀◀◀◀◀◀◀◀◀◀◀◀◀◀◀◀◀◀│
```
