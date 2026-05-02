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
    workspace_dir: PathBuf,                      // 工作区目录（用于持久化工具结果）
    channel_inject: Option<String>,              // 通道注入内容
}

pub struct AgentRunnerBuilder { ... }  // 构建器模式
```

### `AgentRunnerBuilder`

使用构建器模式创建 `AgentRunner`：

```rust
AgentRunner::builder()
    .session_manager(sm)
    .tool_manager(tm)
    .provider(provider)
    .config(agent_config)
    .workspace_dir(workspace)
    .memory_store(ms)
    .channel_inject(None)
    .build()
```

### `AgentResult`

```rust
pub struct AgentResult {
    pub success: bool,
    pub content: String,
    pub total_tokens: u32,
    pub prompt_tokens: u32,
    pub prompt_cache_hit_tokens: u32,
    pub completion_tokens: u32,
    pub iterations: u32,
}
```

## 工具结果处理链

每次工具执行后，结果依次经过以下处理：

1. **错误格式化**：若工具执行失败，返回 `Error: ...\n\n[Analyze the error above...]`，不中断循环
2. **空结果防护**：空字符串替换为 `(tool_name completed with no output)`
3. **持久化**（可选）：超长结果写入 `{workspace_dir}/tool-results/{tool_call_id}.txt`，返回引用 + 预览
4. **头尾截断**：保留头部 2000 字符 + 尾部 2000 字符

## `run` 方法流程

```rust
pub async fn run(
    &self,
    content: String,
    hook: TaskHook,
    session_id: &str,
) -> AgentResult
```

### 步骤 1：写入用户消息

将 `content` 作为 `Message::User` 追加到 session 的消息列表中。

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
  ├─ 历史治理预处理
  │   ├── drop_orphan_tool_results() → 丢弃无对应 tool_call 的 Tool 消息
  │   └── backfill_missing_tool_results() → 为缺失的 tool_call 插入合成错误
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
  │       │   ├── 失败 → format_tool_error() → 继续循环
  │       │   ├── 空结果 → ensure_nonempty_tool_result()
  │       │   ├── 超长 → persist_tool_result() → 写磁盘 + 引用
  │       │   ├── 截断 → truncate_text_head_tail() → 头+尾保留
  │       │   └── 写入 Tool { content, tool_call_id, name } 消息
  │       ├── iterations += 1
  │       ├── TaskState::Running { current_iteration } → 通知
  │       └── 继续下一轮
```

## 消息写入模式

每次 ReAct 循环迭代产生两条消息：
1. `Message::Assistant { content: Some/None, tool_calls: Some(...) }` — 模型的工具调用请求（content 可能保留模型的原始文本）
2. `Message::Tool { content, tool_call_id, name }` — 工具执行结果

循环结束时产生一条消息：
- `Message::Assistant { content: Some(text), tool_calls: None }` — 最终回复

## 结束条件

循环在以下情况结束：

| 条件 | 状态 | 返回值 |
|------|------|--------|
| 超出 `max_iterations` | `Failed` | `Err("Reached max iterations N")` |
| 模型返回无 tool_calls | `Completed` | `Ok(text)` |
| 工具执行失败 | 继续循环 | LLM 收到格式化错误后自行修正 |

## AgentConfig 新增字段

| 字段 | 类型 | 默认值 | 说明 |
|------|------|--------|------|
| `max_tool_result_chars` | `u32` | 8000 | 工具结果最大字符数（超过则截断） |
| `persist_tool_results` | `bool` | true | 是否将超长工具结果持久化到磁盘 |

## 并发安全

所有 session 操作通过 `MutexGuard` 保护：
- 消息追加、状态更新、持久化操作均在 Mutex 锁内执行
- `TaskHook.notify_status_change()` 使用 `try_send`，不会阻塞

## 超时处理

`AgentConfig.timeout_seconds` 字段定义了每轮 Agent 的超时时间，但实际的超时控制应在 `AgentRunner.run()` 的调用层（通过 `tokio::time::timeout`）实现。当前实现依赖 `max_iterations` 作为内置的保护机制。
