# AgentLoop

对应模块：`src/agent_loop.rs`

## 概述

`AgentLoop` 是 SlimBot 的顶层编排组件，负责初始化管理所有核心子系统的实例，并通过 `start_inbound()` 启动后台入站监听任务。

## 结构

```rust
pub struct AgentLoop {
    config: Arc<Config>,
    workspace_dir: PathBuf,
    provider: Arc<dyn Provider>,
    tool_manager: Arc<ToolManager>,
    session_manager: SharedSessionManager,
    memory_store: Arc<MemoryStore>,
    message_bus: Arc<MessageBus>,
}
```

## 初始化流程

`AgentLoop::from_config(paths, message_bus, config)` 按以下顺序初始化：

1. **创建 Provider**：从 `config.providers` 中查找 agent 引用的 provider，创建 `OpenAIProvider`
2. **创建 ToolManager**：以 `workspace_dir` 为根目录，调用 `init_from_config()` 注册内置工具
3. **创建 SessionManager**：指向 session 目录，由 `Arc<Mutex<>>` 包装
4. **创建 MemoryStore**：指向 workspace 目录，调用 `init()`

## 公共方法

### `start_inbound`

```rust
pub fn start_inbound(&self)
```

启动一个 `tokio::spawn` 后台任务，监听 `inbound_rx` 并处理每条入站请求：

```
loop:
  request = inbound_rx.recv()
  ensure_session(session_id)
  enqueue → dequeue SessionTask
  AgentRunner.run(task, session_id)  // ReAct 循环
  outbound_tx.send(BusResult)
```

此方法是非阻塞的，返回后后台任务持续运行。

### `run_task`

```rust
pub async fn run_task(
    &self,
    session_id: &str,
    content: String,
    hook: TaskHook,
    channel_inject: Option<String>,
) -> AgentResult
```

每次调用时**新建**一个 `AgentRunner` 实例，传入 ContextBuilder、ToolManager、Provider、SessionManager、AgentConfig，然后执行 ReAct 循环。返回 `AgentResult`（包含 `success`、`content` 字段）。

### `session_manager`

返回 `SharedSessionManager` 的克隆，供外部模块访问会话状态。

### `config`

返回 `&Config` 的不可变引用。

### `register_tool`

预留方法，当前为空实现。在生产环境中应支持动态注册工具。

## 生命周期

```
main.rs
  → MessageBus::new()
  → AgentLoop::from_config(&paths, message_bus)
  → agent_loop.start_inbound()  // 启动后台监听
  → ChannelManager::new(message_bus)
  → channel_manager.run().await  // 主线程阻塞
```

AgentLoop 创建一次后通过 `Arc` 在多个组件间共享。`start_inbound()` 启动的后台任务与 `ChannelManager.run()` 的出站路由循环并发运行。
