# ReAct Runner

对应模块：`src/runner.rs`

## 概述

`AgentRunner` 实现了 ReAct（Reasoning + Acting）循环的核心逻辑，是 SlimBot 与 LLM 交互的引擎。

## 结构

```rust
pub struct AgentRunner {
    context_builder: ContextBuilder,             // 上下文构建器
    tool_manager: Arc<ToolManager>,              // 工具管理器
    provider: Arc<dyn Provider>,                 // LLM Provider
    session_manager: SharedSessionManager,       // 会话管理器
    config: AgentConfig,                         // Agent 配置
}
```

## `run` 方法流程

```rust
pub async fn run(
    &self,
    task: &mut SessionTask,
    session_id: &str,
    channel_inject: Option<String>,
) -> Result<String>
```

### 步骤 1：写入用户消息

将 `task.content` 作为 `Message::User` 追加到 session 的消息列表中。

### 步骤 2：更新任务状态为 Running

设置 `TaskState::Running { current_iteration: 0 }`，通过 `TaskHook.notify_status_change()` 通知通道。

### 步骤 3：进入 ReAct 循环

```
循环开始
  │
  ├─ 检查是否超出 max_iterations
  │   └── 是 → TaskState::Failed → 持久化 → 返回错误
  │
  ├─ ContextBuilder.build() → system prompt + 历史 + 工具定义
  │
  ├─ Provider.chat() → 调用 LLM API
  │
  ├─ 检查响应是否包含 tool_calls
  │   │
  │   ├── 无 tool_calls → 完成
  │   │   ├── 写入 Assistant 消息
  │   │   ├── TaskState::Completed → 通知通道
  │   │   ├── 持久化
  │   │   └── 返回文本
  │   │
  │   └── 有 tool_calls → 执行工具
  │       ├── 对每个 tool_call：
  │       │   ├── ToolManager.execute() → 执行
  │       │   ├── TaskHook.process_tool_result() → 处理结果
  │       │   ├── 写入 Assistant(tool_calls) 消息
  │       │   └── 写入 Tool 消息
  │       ├── iterations += 1
  │       ├── TaskState::Running { current_iteration } → 通知
  │       └── 继续下一轮
```

## 消息写入模式

每次 ReAct 循环迭代产生两条消息：
1. `Message::Assistant { content: None, tool_calls: Some(...) }` — 模型的工具调用请求
2. `Message::Tool { content: ..., tool_call_id: ... }` — 工具执行结果

循环结束时产生一条消息：
- `Message::Assistant { content: Some(text), tool_calls: None }` — 最终回复

## 结束条件

循环在以下情况结束：

| 条件 | 状态 | 返回值 |
|------|------|--------|
| 超出 `max_iterations` | `Failed` | `Err("Reached max iterations N")` |
| 模型返回无 tool_calls | `Completed` | `Ok(text)` |

## 并发安全

所有 session 操作通过 `MutexGuard` 保护：
- 消息追加、状态更新、持久化操作均在 Mutex 锁内执行
- `TaskHook.notify_status_change()` 使用 `try_send`，不会阻塞

## 超时处理

`AgentConfig.timeout_seconds` 字段定义了每轮 Agent 的超时时间，但实际的超时控制应在 `AgentRunner.run()` 的调用层（通过 `tokio::time::timeout`）实现。当前实现依赖 `max_iterations` 作为内置的保护机制。
