# MessageBus

对应模块：`src/message_bus.rs`

## 概述

`MessageBus` 是通道与 Agent 之间的桥梁，负责将通道传来的用户请求包装为任务，交由 AgentRunner 执行后返回结果。

## 结构

```rust
pub struct MessageBus {
    agent_loop: Arc<AgentLoop>,    // Agent 循环的共享引用
}

pub struct BusRequest {
    pub session_id: String,         // 目标 Session ID
    pub content: String,            // 用户输入内容
    pub channel_inject: Option<String>,  // 通道注入的额外信息
    pub hook: TaskHook,             // 状态通知钩子
}

pub struct BusResult {
    pub session_id: String,         // 所属 Session ID
    pub task_id: String,            // 任务唯一 ID
    pub content: String,            // Agent 回复内容
}
```

## `send` 方法流程

```rust
pub async fn send(&self, request: BusRequest) -> Result<BusResult>
```

执行以下步骤：

### 1. 确保 Session 存在

调用 `ensure_session()` 确保 `request.session_id` 对应的 Session 已创建。如果 Session 不存在，会自动创建并尝试从 JSONL 加载历史。

### 2. 封装为 SessionTask

生成 UUID 作为 task_id，将 `BusRequest` 包装为：

```rust
SessionTask {
    id: task_id,
    content: request.content,
    hook: request.hook,
    state: TaskState::Pending,
}
```

### 3. 入队

将 SessionTask 加入对应 Session 的 `task_queue`（FIFO）。

### 4. 出队

立即从队列中取出该任务。（当前实现为同步入队后立即出队，预留了异步队列能力。）

### 5. 执行

调用 `AgentLoop.run_task()` 执行 ReAct 循环，等待结果返回。

### 6. 返回结果

```rust
BusResult {
    session_id: request.session_id,
    task_id: task_id,
    content: result,    // AgentRunner 返回的最终文本
}
```

## 设计说明

当前 MessageBus 的实现是**同步**的：入队 → 出队 → 执行在一个 `send()` 调用内完成。这种设计简化了流程，但保留了队列结构，未来可以扩展为：

- 真正的异步任务队列，支持多任务排队
- 任务优先级
- 任务取消和超时控制

## 与 ChannelManager 的关系

```
ChannelManager
  → 每个通道在 tokio::spawn 中轮询
  → 读取输入后构建 BusRequest
  → 在 tokio::spawn 中调用 MessageBus.send()
  → 获取 BusResult 后输出到通道
```

每个通道的 I/O 循环都在独立的 tokio 任务中运行，通过 `tokio::spawn` 并发执行 MessageBus 调用。
