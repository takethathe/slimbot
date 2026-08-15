# ReAct Runner

对应模块：`src/runner/`（`mod.rs` + `types.rs` + `traits.rs` + `registry.rs` + `hooks/`）

## 概述

`AgentRunner` 实现了 ReAct（Reasoning + Acting）循环的核心逻辑，是 SlimBot 与 LLM 交互的引擎。自 phase-hook 重构后，`AgentRunner::run()` 是一个约 80 行的骨架，所有行为通过可插拔的 phase hook 实现。

## 架构

### 模块结构

```
src/runner/
├── mod.rs               # AgentResult / AgentRunnerBuilder / AgentRunner / run() 骨架
├── types.rs             # HookAction / ToolHookAction / HookPosition / RunSpec / RunState
├── traits.rs            # 7 个 per-phase hook trait
├── registry.rs          # HookRegistry（存储 + 执行 + 取消检查）
├── tests.rs             # 集成测试
└── hooks/               # 默认 hook 实现（每个 hook 一个文件）
```

### Phase Hook 管线

七个 phase，每个 phase 可注册多个 hook，hook 按注册顺序串行执行：

| Phase | 触发时机 | 默认 Hook | 原逻辑 |
|---|---|---|---|
| PreTurn | 循环开始前 | `WriteUserMessageHook` | 写入用户消息 |
| PreTurn | | `NotifyRunningHook` | TaskState::Running + TaskStarted 事件 |
| PreChat | 每次 LLM 调用前 | `BuildContextHook` | ContextBuilder::build_messages() + PreIteration 事件 |
| PreChat | | `GovernanceHook` | drop_orphan + backfill + microcompact + snip |
| AfterChat | 每次 LLM 调用后 | `HandleResponseHook` | 检查 tool_calls，无则标记 should_exit + AssistantMessage 事件 |
| AfterChat | | `PersistAssistantMsgHook` | 持久化 assistant 消息 + 更新 token ratio |
| AfterChat | | `AccumulateUsageHook` | 累计 token 用量 |
| PreTool | 每个 tool call 执行前 | `FireToolCallEventHook` | ToolCall 事件 |
| AfterTool | 每个 tool call 执行后 | `FireToolResultEventHook` | ToolResult 事件 |
| AfterTool | | `ProcessToolResultHook` | ensure_nonempty + persist + truncate |
| AfterTool | | `PersistToolMessageHook` | 持久化 tool 消息 |
| PostIteration | 所有 tool call 完成后 | `FirePostIterationEventHook` | PostIteration 事件 |
| AfterTurn | 循环退出后（无论成败） | `FireTaskCompletedHook` | 持久化/回滚 + TaskCompleted/TaskFailed 事件 |
| AfterTurn | | `TriggerConsolidationHook` | 触发 consolidation |

### 流程控制

Hook 返回值控制执行流：

- `HookAction::Continue` — 继续下一个 hook
- `HookAction::Abort { reason }` — 中止整个 run，跳到 after_turn 阶段

PreTool hook 特有：

- `ToolHookAction::Execute` — 正常执行工具
- `ToolHookAction::Skip { result }` — 跳过执行，使用合成结果
- `ToolHookAction::Abort { reason }` — 中止整个 run

### 上下文对象

- **`RunSpec`**（不可变）：session_id、content、task_hook、max_iterations、max_tokens、cancel_token、workspace_dir、session_manager、tool_manager、config、memory_store、consolidator、initial_message_count、channel/chat_id、runtime_ctx
- **`RunState`**（可变）：iterations、result、messages、tools、response、tool_results、turn_context、should_exit、rollback_to、extensions

`RunState` 提供类型安全的扩展存储：`set_extension::<T>(key, value)` / `get_extension::<T>(key)`，用于跨 phase 传递自定义数据。

### Hook 注册

```rust
AgentRunner::builder()
    .session_manager(sm)
    .tool_manager(tm)
    .provider(provider)
    .config(agent_config)
    .workspace_dir(workspace)
    .memory_store(ms)
    .consolidator(Some(consolidator))  // 可选
    .max_tokens(4096)
    .with_defaults()                        // 注册全部默认 hook
    .add_pre_chat(my_hook, HookPosition::Append)          // 追加
    .add_pre_chat(my_hook, HookPosition::Replace(0))      // 替换索引 0
    .add_pre_chat(my_hook, HookPosition::Insert(0))       // 插入到索引 0 之前
    .build()
```

每个 phase 有独立的 `add_*` 方法：`add_pre_turn`、`add_pre_chat`、`add_after_chat`、`add_pre_tool`、`add_after_tool`、`add_post_iteration`、`add_after_turn`。

`HookPosition` 越界时 `Replace` 退化为 `Append`，`Insert` 退化为插入到末尾。

> **注意**：`build()` 在 `hooks` 完全为空时自动注册默认 hook（保证未调用 `with_defaults()` 的现有调用方行为不变）。一旦通过 `add_*` 添加了任何自定义 hook 而未调用 `with_defaults()`，则只有自定义 hook 生效 —— 自定义 pipeline 需显式调用 `with_defaults()`（或完全自行注册）才能获得所需行为。

`AgentRunner::new(...)` 保持原有签名，内部自动使用 `with_defaults()`，现有调用方（AgentLoop、dream、session）无需修改。

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
    pub message_sent: bool,  // message 工具已投递输出时由调用方抑制最终回复
}
```

## run() 骨架

```rust
pub async fn run(&self, content, hook, session_id, _channel_inject, cancel_token, origin_channel, origin_chat_id) -> AgentResult
```

骨架职责（不通过 hook 实现的部分）：

1. `/stop` sentinel 检测 → `cancelled_result`
2. 运行前取消检查 → `cancelled_result`
3. 记录 `initial_message_count`（用于回滚）
4. 解析 turn context + 生成 runtime_ctx（每 turn 一次，缓存稳定）
5. 构建 `RunSpec` + `RunState`，进入 `with_turn_context` scope
6. `run_pre_turn()` → 进入循环：
   - 检查 `max_iterations`
   - `run_pre_chat()` → `provider.chat()`（内置）→ `run_after_chat()`
   - `should_exit` 则退出循环
   - 对每个 tool_call：`run_pre_tool()` → `tool_manager.execute()`（内置）→ `run_after_tool()`
   - `run_post_iteration()` → `iterations += 1` → 通知 Running
7. 循环退出后 `run_after_turn()`（**始终执行**，即使取消或出错）
8. `finalize()` 设置 `message_sent` 并返回

LLM 错误、取消（回滚到 initial_message_count）、hook abort 均在骨架层处理，通过 `state.fail()` / `state.rollback_to` 传递，由 AfterTurn 的 `FireTaskCompletedHook` 统一执行持久化（或回滚）+ 完成/失败事件。

> **PreTurn abort**：PreTurn hook 返回 `Abort`/错误/取消时，整个循环被跳过（不调用 provider），直接进入 AfterTurn。
>
> **边界**：`/stop` sentinel 与**运行前**取消（token 在 `run()` 调用前已取消）在 `RunSpec`/`RunState` 建立之前由 `cancelled_result()` 早返回，不经过 hook 管线 —— 此时任务从未开始、无状态可清理，因此 `after_turn` 不执行。运行中取消/错误则始终执行 `after_turn`。

### 取消检查

`HookRegistry` 的每个 `run_*` 方法在调用每个 hook 前检查 `cancel_token`，已取消则跳过后续 hook 返回 `HookRunError::Cancelled`。`run_after_turn` 不检查取消，始终执行（单个 hook 出错仅记录警告，不影响其余 hook）。

## 工具结果处理链

每次工具执行后，结果依次经过以下处理（`ProcessToolResultHook`）：

1. **错误格式化**：若工具执行失败，返回 `Error: ...\n\n[Analyze the error above...]`，不中断循环
2. **空结果防护**：空字符串替换为 `(tool_name completed with no output)`
3. **持久化**（可选）：超长结果写入 `{workspace_dir}/tool-results/{tool_call_id}.txt`，返回引用 + 预览
4. **头尾截断**：保留头部 2000 字符 + 尾部 2000 字符

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
| 超出 `max_iterations` | `Failed` | `"Reached max iterations N"` |
| 模型返回无 tool_calls | `Completed` | 回复文本 |
| hook 返回 `Abort` | `Failed` | abort reason |
| LLM 调用错误 | `Failed`（回滚） | `"LLM API request failed - ..."` |
| 取消 | `Failed`（回滚） | `"Task cancelled."` |

## 并发安全

所有 session 操作通过 `MutexGuard` 保护：
- 消息追加、状态更新、持久化操作均在 Mutex 锁内执行
- `TaskHook.notify_status_change()` 使用 `try_send`，不会阻塞
- `TurnContext` 通过 task-local 隔离，并发 run 互不干扰

## 超时处理

`AgentConfig.timeout_seconds` 字段定义了每轮 Agent 的超时时间，但实际的超时控制应在 `AgentRunner.run()` 的调用层（通过 `tokio::time::timeout`）实现。当前实现依赖 `max_iterations` 作为内置的保护机制。

## Consolidation 集成

当 `consolidator` 字段为 `Some(Arc<Consolidator>)` 时，ReAct 循环完成后（AfterTurn 阶段）由 `TriggerConsolidationHook` 调用 `consolidator.maybe_consolidate(session_id, prompt_tokens)`。若 prompt_tokens 超过安全预算，Consolidator 会自动选取旧消息进行摘要，摘要追加到 `history.jsonl` 并更新会话的 consolidation 游标。

传入 `None` 时跳过摘要检查。

## AgentEvent 事件流

ReAct 循环在关键节点通过 `TaskHook.fire_event()` 发出 `AgentEvent`，供前端实时展示：

| 事件 | 触发时机 | 数据 |
|------|---------|------|
| `TaskStarted` | 循环开始前（NotifyRunningHook） | session_id |
| `PreIteration` | 每轮迭代开始（BuildContextHook） | session_id, iteration |
| `AssistantMessage` | LLM 返回后（HandleResponseHook） | session_id, content |
| `ToolCall` | 工具执行前（FireToolCallEventHook） | session_id, name, args |
| `ToolResult` | 工具执行后（FireToolResultEventHook） | session_id, name, output |
| `PostIteration` | 工具循环结束后（FirePostIterationEventHook） | session_id, iteration |
| `TaskCompleted` | 循环成功完成（FireTaskCompletedHook） | session_id, result |
| `TaskFailed` | 循环失败（FireTaskCompletedHook） | session_id, error |

事件通过 `broadcast::Sender<AgentEvent>` 广播到 `ChannelManager`，各 Channel 的 `write_event()` 方法负责将其送达客户端（WebUI 中通过 SSE 的 `agent_event` 事件类型）。
