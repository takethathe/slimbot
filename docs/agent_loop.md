# AgentLoop

对应模块：`src/agent_loop.rs`

## 概述

`AgentLoop` 是 SlimBot 的顶层编排组件，负责初始化管理所有核心子系统的实例，包括 Provider、ToolManager、ToolCatalog、SessionManager、MemoryService、Consolidator、DreamService 和 SubagentRuntime。通过 `start_inbound()` 启动后台入站监听任务。

## 结构

```rust
pub struct AgentLoop {
    config: Arc<Config>,
    workspace_dir: PathBuf,
    provider: Arc<dyn Provider>,
    tool_manager: Arc<ToolManager>,
    session_manager: SharedSessionManager,
    memory_service: SharedMemory,  // Arc<tokio::sync::Mutex<MemoryService>>
    message_bus: Arc<MessageBus>,
    inbound_rx: Mutex<Option<mpsc::Receiver<BusRequest>>>,  // start_inbound 一次性消费
    consolidator: Arc<Consolidator>,
    shutdown_tx: broadcast::Sender<()>,  // 广播关闭入站循环与 I/O 调度器
    runner: AgentRunner,                 // 预构建、跨任务复用（cron/heartbeat 走此路径）
    tracer: Option<Arc<Tracer>>,
    dream_service: Arc<DreamService>,
    subagent_runtime: Arc<SubagentRuntime>,  // 与触发器 Dispatcher 共享同一实例
}
```

## 初始化流程

`AgentLoop::from_config(paths, message_bus, config)` 按以下顺序初始化：

1. **创建 Provider**：从 `config.providers` 中查找 agent 引用的 provider，创建 `OpenAIProvider`
2. **创建 ToolManager**：以 `workspace_dir` 为根目录，调用 `init_from_config()` 注册内置工具；gateway 模式改由 `from_config_with_tools()` 传入预装配的 ToolManager + ToolCatalog（见下）
3. **创建 SessionManager**：指向 session 目录，由 `Arc<Mutex<>>` 包装
4. **创建 MemoryService**：经 `MemoryService::with_backend`（按 `memory.backend` 配置选择 backend）指向 workspace 目录，由 `Arc<tokio::sync::Mutex<>>` 包装，调用 `init()`
5. **创建 Consolidator**：传入 provider、session_manager、memory_service、`context_window_tokens`、`max_completion_tokens`
6. **装配工具与运行时**（`from_parts`）：主循环注册 `memory_search`（同时镜像进 catalog）；创建 `SubagentRuntime`（全局并发闸 `subagent.max_concurrent`）并仅向主循环注册 `subagent` 工具；`ToolCatalog::install_global()` 供 agent 定义校验与 `known_tool_names()` 查询

## Gateway 装配

gateway 模式在创建 AgentLoop 前先注册额外工具：

- `message` / `cron`：同时注册进 ToolManager 和 ToolCatalog（子代理可声明）
- `trigger_source` / `trigger`：**只注册进 ToolCatalog**，主循环不可见
- 启动后通过 `agent_loop.subagent_runtime()` 把同一个 `SubagentRuntime` 交给触发器 `Dispatcher`，因此触发器 `agent` action 与 `subagent` 工具调用共用同一个并发闸和同一条回会话的路

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
  build SessionTask via SessionTaskBuilder (包含 consolidator)
  session_manager.submit_task(task)  // SessionRunner 保证顺序执行
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

每次调用时**新建**一个 `AgentRunner` 实例，传入 tool_manager、provider、session_manager、agent_config、workspace_dir、memory_service、consolidator（`Some`），然后执行 ReAct 循环。`ContextBuilder` 在 runner 的 `run()` 内部创建。返回 `AgentResult`（包含 `success`、`content` 字段）。

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
