# AgentLoop

对应模块：`src/agent_loop.rs`

## 概述

`AgentLoop` 是 SlimBot 的顶层编排组件，负责初始化管理所有核心子系统的实例，并提供 `run_task` 入口供 MessageBus 调用。

## 结构

```rust
pub struct AgentLoop {
    config: Config,                                    // 配置
    provider: Arc<dyn Provider>,                       // LLM Provider
    tool_manager: Arc<ToolManager>,                    // 工具管理器
    session_manager: SharedSessionManager,             // 会话管理器
}
```

## 初始化流程

`AgentLoop::from_config(config_path)` 按以下顺序初始化：

1. **加载配置**：`Config::load(config_path)` 解析并验证配置文件
2. **创建 Provider**：从 `config.providers` 中查找 agent 引用的 provider，创建 `OpenAIProvider`
3. **创建 ToolManager**：以 `workspace_dir` 为根目录，调用 `init_from_config()` 注册内置工具
4. **创建 SessionManager**：指向 `config.session_dir()`，由 `Arc<Mutex<>>` 包装为 `SharedSessionManager`

## 公共方法

### `run_task`

```rust
pub async fn run_task(
    &self,
    session_id: &str,
    task: &mut SessionTask,
    channel_inject: Option<String>,
) -> Result<String>
```

每次调用时**新建**一个 `AgentRunner` 实例，传入：
- `ContextBuilder`：从当前 session 构建上下文
- `ToolManager`：共享引用
- `Provider`：共享引用
- `SessionManager`：共享引用
- `AgentConfig`：从配置克隆

然后调用 `runner.run()` 执行 ReAct 循环。

### `session_manager`

返回 `SharedSessionManager` 的克隆，供 MessageBus 等模块访问会话状态。

### `config`

返回配置的可变引用。

### `register_tool`

预留方法，当前为空实现。在生产环境中应支持动态注册工具。

## 生命周期

```
main.rs
  → AgentLoop::from_config() 初始化一次
  → Arc::new(agent_loop) 包装为共享引用
  → MessageBus::new(agent_loop) 持有引用
  → 每次 BusRequest 触发 run_task()
```

AgentLoop 在整个应用程序生命周期中只创建一次，通过 `Arc` 在多个通道间共享。
