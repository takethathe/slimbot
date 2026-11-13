# ReAct Runner

对应模块：`src/runner/`（`mod.rs` + `types.rs` + `traits.rs` + `registry.rs` + `hooks/`）

## 概述

`AgentRunner` 实现了 ReAct（Reasoning + Acting）循环的核心逻辑，是 SlimBot 与 LLM 交互的引擎。自 phase-hook 重构后，`AgentRunner::run()` 是一条分阶段的编排骨架（约 500 行），行为通过可插拔的 phase hook 实现。骨架自身只承担五件事：

1. **早返回** — `/stop` sentinel 与运行前取消走 `cancelled_result()`，不进入 hook 管线。
2. **Run 装配** — 从 `AgentRunner` 借出依赖集合填一个 `RunSpec`，配 `RunState`，进 `with_turn_context` scope。
3. **ReAct 循环** — 带 `timeout_seconds` 的 deadline 包裹 PreTurn → (PreChat → `provider.chat()` → AfterChat → PreTool → `tool_manager.execute()` → AfterTool)* → AfterTurn（始终执行）。
4. **循环自身的簿记** — `ToolCall` / `ToolResult` / `PostIteration` 事件、usage 累加、回合收尾（持久化或回滚 + 完成/失败事件）。这些是循环自己的记账而不是可变策略，所以内联在骨架里，不做成 hook。
5. **收尾** — `message_sent`、根 span 的 usage / 迭代数 / 状态映射。

## 架构

### 模块结构

```
src/runner/
├── mod.rs               # AgentResult / AgentRunnerBuilder / AgentRunner / run() 骨架
├── types.rs             # HookAction / ToolHookAction / HookPosition / RunSpec / RunState
├── test_support.rs      # RunFixture：唯一一份 Run 级测试夹具（cfg(test)）
├── traits.rs            # 6 个 per-phase hook trait
├── registry.rs          # HookRegistry（存储 + 执行 + 取消检查）
├── tests.rs             # 集成测试
└── hooks/               # 13 个 policy hook（10 个文件）
```

### Phase Hook 管线

六个 phase，每个 phase 可注册多个 hook，hook 按注册顺序串行执行：

| Phase | 触发时机 | 默认 Hook | 原逻辑 |
|---|---|---|---|
| PreTurn | 循环开始前 | `ApprovalTurnStartHook` | 审批回合开启：TTL 过期结算 |
| PreTurn | | `WriteUserMessageHook` | 写入用户消息 |
| PreTurn | | `NotifyRunningHook` | TaskState::Running + TaskStarted 事件 |
| PreChat | 每次 LLM 调用前 | `BuildContextHook` | ContextBuilder::build_messages() + PreIteration 事件 |
| PreChat | | `GovernanceHook` | drop_orphan + backfill + microcompact + snip（pin 住待兑现审批） |
| PreChat | | `ApprovalContextHook` | `merge_approval_rounds` 构建时变换 |
| AfterChat | 每次 LLM 调用后 | `HandleResponseHook` | 检查 tool_calls，无则标记 should_exit + AssistantMessage 事件 |
| AfterChat | | `PersistAssistantMsgHook` | 持久化 assistant 消息 + 更新 token ratio |
| PreTool | 每个 tool call 执行前 | `ApprovalHook` | 唯一审批决策点（Allow/Deny/Ask + 兑现） |
| AfterTool | 每个 tool call 执行后 | `ProcessToolResultHook` | ensure_nonempty + persist + truncate |
| AfterTool | | `PersistToolMessageHook` | 持久化 tool 消息 |
| AfterTurn | 循环退出后（无论成败） | `TriggerConsolidationHook` | 触发 consolidation（pin 住待兑现审批） |
| AfterTurn | | `ApprovalTurnEndHook` | 审批回合收尾：清理未兑现的携入记录 |

以下是骨架内联、不再作为 hook 注册的行为：

| 时机 | 行为 | 原 Hook（已内联） |
|---|---|---|
| 每个 tool call 前 | `ToolCall` 事件 | `FireToolCallEventHook` |
| 每个 tool call 后 | `ToolResult` 事件 | `FireToolResultEventHook` |
| 每次 LLM 调用后（AfterChat 之后） | 累计 token 用量 | `AccumulateUsageHook` |
| 所有 tool call 完成后 | `PostIteration` 事件 | `FirePostIterationEventHook` |
| 循环退出后（AfterTurn 之前） | 持久化/回滚 + TaskCompleted/TaskFailed 事件 | `FireTaskCompletedHook` |

### 流程控制

Hook 返回值控制执行流：

- `HookAction::Continue` — 继续下一个 hook
- `HookAction::Abort { reason }` — 中止整个 run，跳到 after_turn 阶段

PreTool hook 特有：

- `ToolHookAction::Execute` — 正常执行工具
- `ToolHookAction::Skip { result }` — 跳过执行，使用合成结果
- `ToolHookAction::Abort { reason }` — 中止整个 run

### 上下文对象

- **`RunSpec<'_>`**：一个 run 的 hook 视角。Run 级依赖（session / tool manager、config、pressure anchor、memory、consolidator、tracer、approvals、connectors、max_tokens）**借用** `AgentRunner` 上的那一份，经访问器（`spec.config()`、`spec.approvals()`…）读取，没有逐 turn 的复制；per-turn 输入来自 `RunTurn`（session_id、content、task_hook、channel/chat_id、runtime_ctx），`cancel_token` 与 `initial_message_count` 由 `run()` 在装配后补齐。装配点只有 `AgentRunnerBuilder` 一处；加一个 Run 级依赖 = builder + runner 字段 + 一行转发访问器，不存在第二份赋值。
- **`RunState`**（可变）：iterations、result、messages、tools、response、tool_results、turn_context、should_exit、rollback_to、consolidated、approval_marks

`RunState` 不再有通用的 `Any` 扩展字典：跨 phase 的数据只有 `approval_marks`（`tool_call_id → approval_id`）这一处，PreTool 的 `ApprovalHook` 写入、AfterTool 的 `PersistToolMessageHook` 读出以给引导结果打标。字段是具名的强类型字段而不是「字符串 key + `Any`」，字段名拼错或类型不符都是编译错误而不是静默 `None`；要加新的跨 phase 数据就加字段，不要退回字符串 key 的字典。

### Hook 注册

```rust
AgentRunner::builder()
    .session_manager(sm)
    .tool_manager(tm)
    .provider(provider)
    .config(agent_config)
    .workspace_dir(workspace)
    .memory_service(ms)
    .consolidator(Some(consolidator))  // 可选
    .max_tokens(4096)
    .with_defaults()                        // 注册全部默认 hook
    .add_pre_chat(my_hook, HookPosition::Append)          // 追加
    .add_pre_chat(my_hook, HookPosition::Replace(0))      // 替换索引 0
    .add_pre_chat(my_hook, HookPosition::Insert(0))       // 插入到索引 0 之前
    .build()
```

每个 phase 有独立的 `add_*` 方法：`add_pre_turn`、`add_pre_chat`、`add_after_chat`、`add_pre_tool`、`add_after_tool`、`add_after_turn`。

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
    pub provider_error: Option<ProviderError>,  // 本回合终止于 provider 失败时的分类结果
}
```

`provider_error` 携带 kind + provider 自己的 message（原始响应体永不进入）。产生端不再从类型擦除的错误里 `downcast` 猜 kind：`Provider::chat` 的错误类型就是 `ProviderError`（见 `docs/provider.md`），所以 provider 失败一定有 kind。它只在 provider 调用失败时是 `Some`；取消、hook abort、达到迭代上限属于别的失败类别，仍是 `None`（`classify_run_error` 按 message 区分它们）。

失败的回合怎么处置由 `AgentResult::provider_failure_route()` 一个函数给出，紧挨结果定义：

| kind | route | 调用方行为 |
|---|---|---|
| `AuthQuota` | `AlertOwner` | 经投递 seam 告警 owner，10 分钟窗口内合并重复 |
| `ContextLength` | `RelievePressure` | 跑一轮 `force_consolidate`，为下一轮腾窗口 |
| 其余（`Transient` / `ContentFilter` / `Other`） | `FailInPlace` | 除本回合失败外不做别的 |

决策在 `runner`，副作用在 `session`（`apply_provider_failure_route`）——前者可单独驱动断言，后者才需要 outbound bus 与 owner 主通道。

## run() 骨架

```rust
pub async fn run(&self, content, hook, session_id, _channel_inject, cancel_token, origin_channel, origin_chat_id) -> AgentResult
```

骨架职责（不通过 hook 实现的部分）：

1. `/stop` sentinel 检测 → `cancelled_result`
2. 运行前取消检查 → `cancelled_result`
3. 记录 `initial_message_count`（用于回滚）
4. 解析 turn context + 生成 runtime_ctx（每 turn 一次，缓存稳定）
5. 构建 `RunSpec`（借用 runner 的依赖集合，只填 per-turn 输入）+ `RunState`，进入 `with_turn_context` scope
6. `run_pre_turn()` → 进入循环：
   - 检查 `max_iterations`
   - `run_pre_chat()` → `provider.chat()`（内置）→ `run_after_chat()` → 累加 usage
   - `should_exit` 则退出循环
   - 对每个 tool_call：发 `ToolCall` 事件 → `run_pre_tool()` → `tool_manager.execute()`（内置）→ 发 `ToolResult` 事件 → `run_after_tool()`
   - 发 `PostIteration` 事件 → `iterations += 1` → 通知 Running
7. `close_turn()`：持久化或回滚 + `TaskCompleted`/`TaskFailed` 事件（**始终执行**）
8. `run_after_turn()`（**始终执行**，即使取消或出错）
9. `finalize()` 设置 `message_sent` 并返回

> **run 级 deadline**：步骤 6 整个（PreTurn + 循环）被包在 `tokio::time::timeout(agent.timeout_seconds)` 里，到期走取消路径（见下文「超时处理」）；步骤 7–8 在 timeout 之外，所以超时也一定收尾。

LLM 错误、取消（回滚到 initial_message_count）、hook abort 均在骨架层处理，通过 `state.fail()` / `state.rollback_to` 传递，由骨架的 `close_turn()` 统一执行持久化（或回滚）+ 完成/失败事件。取消的「错误 → 状态」映射只写一处（`fail_cancelled()`），但三种控制流出口（`false` / `break` / `break 'outer`）各自保留。

> **取消检查点**：`HookRegistry` 的 `run_*` 在每个 hook 前检查 token。内联进来的四个行为（`ToolCall` / `ToolResult` / `PostIteration` 事件与 usage 累加）原先都排在某个 phase 的首位或队尾，因此骨架在发这些事件前用 `cancel_here()` 复刻同一个检查点 —— 取消时不发出未兑现的事件。

> **PreTurn abort**：PreTurn hook 返回 `Abort`/错误/取消时，整个循环被跳过（不调用 provider），直接进入 AfterTurn。
>
> **边界**：`/stop` sentinel 与**运行前**取消（token 在 `run()` 调用前已取消）在 `RunSpec`/`RunState` 建立之前由 `cancelled_result()` 早返回，不经过 hook 管线 —— 此时任务从未开始、无状态可清理，因此 `after_turn` 不执行。运行中取消/错误则始终执行 `after_turn`。

### 取消检查

`HookRegistry` 的每个 `run_*` 方法在调用每个 hook 前检查 `cancel_token`，已取消则跳过后续 hook 返回 `HookRunError::Cancelled`。`run_after_turn` 不检查取消，始终执行（单个 hook 出错仅记录警告，不影响其余 hook）。

## 审批生命周期（Approval）

工具风险闸门只有一个决策核心：`src/runner/gate.rs` 的 `decide()`（风险级 → 管控档 → 决策 + 兑现/否决/审计）。顶层由 `ApprovalHook`（PreTool）调用它，codemode 脚本里的嵌套 `tools.*` 调用由 `RunToolExec` 走同一个决策核心（票 25，见 `docs/codemode.md`）。三层职责严格分离——`RiskLevel`（工具的事实）→ control level（配置的策略）→ gate（执行）。完整设计见 `docs/adr/0011-approval-by-turn-end-redemption.md`。

### 三种决策

| 决策 | 行为 | 回合 |
|------|------|------|
| `Allow` | 直接执行，不写任何记录 | 继续 |
| `Deny` | 返回固定拒绝文案 + "不要换写法重试"的引导，run 继续 | 继续（由模型向用户解释） |
| `Ask` | 中止本次调用，写入一条 pending 记录，注入引导 tool result | **结束**，以纯文本待确认清单作为 run 结果 |

`Ask` 不阻塞回合：没有 `ask_user` / `confirm` 工具，也不存在挂起的 run 状态。清单只是一段文本（无按钮），模型下一轮重放同一个调用即兑现。

### 兑现与作废

- **兑现**：下一轮模型发出 `(tool, args_hash)` 都相同的调用 → 放行执行。只认 **carried** 记录（本轮开始时已遗留的）：**本轮内刚提出的申请不能靠本轮重放兑现**，否则顶层一批两个相同 tool_call、或脚本里连调两次同 args 就等于自己批准自己（兑现与下面的否决用同一个 carried 口径）。执行前重新校验 risk 与 args_hash（fail-closed）。
- **否决**：同工具但 args 不同 → 删除记录 + 固定拒绝文案（一次批准不能授权另一个操作）。
- **滚动**：回合结束时清理的是"回合开始时已存在"的记录；**本回合内新产生的 `Ask` 滚动到下一轮**，否则确认轮里冒出的新动作永远等不到兑现。
- **TTL**：默认 1800 秒（可配，上限 24 小时）。过期的记录在下一回合**开始时**作废，且不再出现在上下文里。
- **重启**：pending 集合只在内存里（`ApprovalStore`），gateway 重启即清空，天然 fail-closed。
- **无人值守**：`TurnKind` 为 `Cron` / `Heartbeat` / `Trigger` / `Subagent` / `System`（Dream）时，`Ask` 一律按 `Deny` 处理且**不写记录**——没有"下一轮"可以兑现它。

### 会话历史的构建时变换

`merge_approval_rounds`（`ApprovalContextHook`，PreChat）是纯函数式的构建时变换，**不重写 JSONL**（保持 append-only）：

- 已兑现：保留第一轮 `assistant(tool_calls=[A])`，把确认轮的真实结果改挂到 A，删掉确认轮的 `assistant(tool_calls=[B])` 与 `user("确认")`——模型看到一轮完整跑完。逐条粒度，不是整轮粒度。
- 未兑现：引导 tool result **改写为执行失败文案**而不是删除（否则留下没有结果的 `tool_call`，模型会当成被截断的调用而重试）。于是每个 `tool_call` 在历史里永远有一个真结果。
- 幂等：重复执行结果一致，崩溃/重启安全。

### 裁剪保护

pending 记录存在期间，携带它的那个回合被 **pin 在历史尾部**：`GovernanceHook` 在 microcompact/snip 之前把它切出来、之后原样接回；`TriggerConsolidationHook` 通过 `maybe_consolidate_protecting` 把摘要边界收在该回合之前（整段被 pin 时本轮跳过摘要）；`ContextLength` 兜底的 `force_consolidate` 走同一份快照。记录消失后回到普通裁剪范围。

四个保护点（`GovernanceHook` / `TriggerConsolidationHook` / `ApprovalContextHook` / `session.rs` 的 `RelievePressure`）都经 `approval::LiveApprovals::collect` 取同一份 live id 快照，live 判定统一走 `is_live_pin`；裁剪与摘要另共用 `user_boundary_before` 作为 User 边界尺子：`protect_start` 取全历史**最早**的 User 边界，`clamp_boundary_for_pins` 取 chunk 内**首个** pin 且不越过 floor（整段被 pin 时返回 `None` 跳过本轮）。改 pin 语义只需改 `src/approval/mod.rs` 一处（ADR 0011）。

### 审计与补偿

- 决策（`proposed` / `redeemed` / `expired` / `cleared` / `denied_by_policy`）写 **trace 的 `approval` span**，不写 session JSONL——对话历史会被裁剪，审计放在那里会跟着消失。
- 兑现执行**前**先广播 `AgentEvent::ApprovalRedeemed`，owner 可在此期间 `/stop`；`gate.rs::redeem` 先重校验 `risk` / `args_hash`，通过后才 `take` 消费记录并广播，`yield_now` 后再查取消，命中则整个 run `Abort`（重校验失败也会 `take` 掉记录并拒绝；`ApprovalHook` 只把 `GateDecision` 翻成 `ToolHookAction`）。

### 端到端测试边界

`tests/approval_integration.rs` 只经公开面驱动整条链路：自建 `Provider`（按脚本出队响应，并记录每次收到的消息）、`Tool`（统计执行次数）、`TaskHook` + `ApprovalStore` + `AgentRunner::new`，断言直接读 `{session_id}.jsonl` 的原始文本。`src/` 里的 `RunnerHarness`（`runner/hooks/approval.rs` 测试模块内的私有夹具）是钩子级夹具，层级不同（它要能直接碰 `RunState` 和 hook trait），两者不复用。

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
| 超出 `agent.timeout_seconds` | `Failed`（回滚） | `"Task cancelled: run deadline of Ns exceeded."` |
| 取消 | `Failed`（回滚） | `"Task cancelled."` |

## 并发安全

所有 session 操作通过 `MutexGuard` 保护：
- 消息追加、状态更新、持久化操作均在 Mutex 锁内执行
- `TaskHook.notify_status_change()` 使用 `try_send`，不会阻塞
- `TurnContext` 通过 task-local 隔离，并发 run 互不干扰

## 超时处理（run 级挂钟 deadline）

`AgentConfig.timeout_seconds`（默认 120s，钳制 1–600）被真正执行：`run()` 把 **PreTurn + ReAct 循环**整体包进 `tokio::time::timeout(...)`。

- 到期即**复用 `/stop` 取消路径**：取消本次 run 的 `CancellationToken`，`state.fail("Task cancelled: run deadline of Ns exceeded.")`，`rollback_to = initial_message_count`，然后 AfterTurn 照常跑（回滚会话、发 `TaskFailed`、必要时 consolidate）。
- **只包 body**：收尾（`close_turn()` / `TriggerConsolidationHook`）在 timeout 之外，所以超时也一定收尾，不会留下半截会话。
- **run 级 token 是派生的 child token**：run 仍能观察到调用方的 `/stop` 取消（child 跟随 parent），但 deadline 取消的只是本次 run，不会误伤同一会话排队的其它任务。
- `timeout_seconds = 0` 视为不设 deadline（配置钳制后不会出现，仅作为防御）。
- 部分进度不丢：`state` 由 `run()` 持有（timeout 只丢弃 body 的 future），已累计的 iteration / usage 仍会记到 root span；`error.type` 沿用取消分类（`cancelled`）。
- 子代理运行走同一个 `run()`，所以也受 `agent.timeout_seconds` 约束；agent 定义里的 `timeout_secs`（默认 600）仍是外层硬上限，两者取先到者。

`max_iterations` 仍是另一道内置保护：它与 deadline 互补（一个限制轮数、一个限制挂钟）。

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
| `ToolCall` | 工具执行前（骨架内联） | session_id, name, args |
| `ToolResult` | 工具执行后（骨架内联） | session_id, name, output |
| `PostIteration` | 工具循环结束后（骨架内联） | session_id, iteration |
| `TaskCompleted` | 循环成功完成（骨架 `close_turn()`） | session_id, result |
| `TaskFailed` | 循环失败（骨架 `close_turn()`） | session_id, error |
| `ApprovalRedeemed` | 审批兑现、工具真正执行前（`gate.rs::redeem`） | session_id, approval_id, tool, args, risk |

事件通过 `broadcast::Sender<AgentEvent>` 广播到 `ChannelManager`，各 Channel 的 `write_event()` 方法负责将其送达客户端（WebUI 中通过 SSE 的 `agent_event` 事件类型）。
