# Consolidator

对应模块：`src/consolidate.rs`

## 概述

`Consolidator` 实现基于 token 预算触发的会话摘要机制。当 LLM 返回的 `prompt_tokens` 超过安全预算时，自动选取旧消息块（按用户轮次边界对齐），调用 LLM 进行摘要，将摘要通过 `HistoryStore::append_history()` 追加到 `history.jsonl`，并更新会话的 consolidation 游标。

## 核心类型

```rust
pub struct Consolidator {
    provider: Arc<dyn Provider>,
    session_manager: SharedSessionManager,
    memory_service: SharedMemory,
    anchor: PressureAnchor,   // 窗口锚 + 占比表
    cancel_token: CancellationToken,
}
```

构造函数：`new(provider, session_manager, memory_service, context_window_tokens)`；`with_anchor(anchor)` 用配置里的 provider 窗口 + `pressure` 占比覆盖它（`AgentLoop` 就是这么接的）。

## 预算计算

全部派生自同一个窗口锚（`pressure::PressureLimits`）：

```
trigger = window × pressure.consolidate_trigger_ratio   (默认 0.50)
target  = trigger × pressure.consolidate_target_ratio   (默认 0.60)
summary_max_chars = window × pressure.summary_ratio × char_per_token_ratio
```

`prompt_tokens > trigger` 时触发摘要，目标是把 token 数降到 `target`；`trigger == 0`（窗口未知）时直接跳过。窗口翻倍，触发线与目标线同时翻倍——不需要改任何字段。

## Token 估算

```rust
pub fn estimate_message_tokens(msg: &Message, ratio: f64) -> u32
```

- 使用 `session.char_per_token_ratio`（`total_chars / prompt_tokens`）估算，`chars / ratio`
- 冷启动默认值为 `3.0`（中英混排的保守值，`pressure::DEFAULT_CHAR_PER_TOKEN_RATIO`），`Session::update_token_ratio()` 在每次 LLM 调用后用实测值更新，持久化到 `.meta.json`

## 消息选择策略

| 方法 | 说明 |
|------|------|
| `pick_consolidation_boundary()` | 从 `last_consolidated_id` 之后开始扫描，找到满足 `tokens_to_remove` 的第一个用户轮次边界 |
| `cap_consolidation_boundary()` | 限制单次最多处理 `MAX_CHUNK_MESSAGES`（60）条消息，回退到用户消息边界 |

### 用户轮次边界对齐

只驱逐到 `Message::User` 消息为止的消息块，确保语义完整性：

```
User msg → Assistant reply → Tool result → User msg (boundary)
[========= evictable chunk =========] ^ stop here
```

## 摘要生成

`archive()` 方法将选中消息格式化为 `"[ROLE] content"` 文本，调用 LLM 的 `chat()` 方法获取摘要。系统提示使用 SNIP 标准筛选重要事实：

- **Signal**：该事实被遗忘后用户是否需要重复说明？
- **Novel**：不是本对话段中已有事实的复述
- **Important**：能防止重复工作或捕捉偏好/规则
- **Persistent**：2 周后仍然相关

每条事实附带记忆属性标记，格式为 `- [mark] fact content`：

| 标记 | 说明 |
|------|------|
| `[permanent]` | 核心偏好、个人特质、习惯 — 永不过时 |
| `[durable]` | 技术发现、项目知识、配置细节 — 数月有效 |
| `[ephemeral]` | 活跃任务状态、临时决策 — 数周内可能变化 |
| `[correction]` | 对之前记忆的修正 — 说明变更内容 |
| `[skip]` | 不满足 SNIP 标准、对话填充、可从源码推导的代码模式 |

优先级：用户修正和偏好 > 解决方案 > 决策 > 事件 > 环境事实

不因"可能已存在于长期记忆"而标记为 `[skip]`；Dream 会处理下游去重。

摘要长度由窗口派生（`summary_ratio`，默认 2% → 32768 窗口约 1965 字符），通过三层保障（prompt 里写明字符预算 → 最多 2 次 LLM 压缩重试 → 强制 UTF-8 安全截断）。不再有硬编码的 512 字节上限。

摘要通过 `HistoryStore::append_history()` 追加，使用统一的游标管理。

## 数据流

```
ReAct turn 完成
  │
  ├─ response.usage.prompt_tokens
  │
  ├─ maybe_consolidate(session_id, prompt_tokens)
  │   ├─ 检查 trigger: prompt_tokens <= window × trigger_ratio? → 返回
  │   ├─ consolidate_one_round()
  │   │   ├─ 读取 SessionData（messages, ratio, cursor）
  │   │   ├─ pick_consolidation_boundary() → 找到 end_idx
  │   │   ├─ cap_consolidation_boundary() → 限制 chunk 大小
  │   │   ├─ archive() → LLM 摘要
  │   │   │   ├─ 格式化消息为 "[ROLE] content" 文本
  │   │   │   ├─ provider.chat([system_prompt, formatted_messages])
  │   │   │   ├─ 摘要通过 HistoryStore::append_history() 追加到 history.jsonl
  │   │   │   └─ 返回摘要文本
  │   │   ├─ update_consolidation_cursor(session_id, end_msg_id)
  │   │   │   └─ 移除 id <= cursor 的消息，更新 last_persisted_idx
  │   │   ├─ set_last_summary(session_id, summary)
  │   │   └─ save_session_meta(session_id)
  │   └─ 返回
```

## SessionMeta 持久化

| 字段 | 类型 | 说明 |
|------|------|------|
| `last_consolidated_id` | `usize` | 已摘要消息的最大 ID，加载时跳过这些消息 |
| `next_message_id` | `usize` | 下一个自增消息 ID，保证 ID 单调递增 |
| `char_per_token_ratio` | `f64` (默认 3.0) | 平均每 token 的字符数，用于精确 token 估算 |
| `last_summary` | `Option<String>` | 最后一次摘要的文本，注入到 system prompt 的 `[Resumed Session]` 段落 |

## 上下文注入

`ContextBuilder.build_messages()` 在 `current_turn` 的第一个 `Message::User` 上设置 `runtime_content` 字段（包含 `[Resumed Session]` 摘要），仅在 Provider 序列化时处理，不污染持久化消息：

```
[Resumed Session] - User prefers dark mode
- Decided to use SQLite over Postgres
- Project deadline is 2024-06-01
```

## 配置

| 配置项 | 默认值 | 说明 |
|--------|--------|------|
| `providers.<agent.provider>.context_window_tokens` | 32768 | 唯一绝对锚（token） |
| `pressure.consolidate_trigger_ratio` | 0.50 | 触发线 = 窗口 × 该比例 |
| `pressure.consolidate_target_ratio` | 0.60 | 目标线 = 触发线 × 该比例 |
| `pressure.summary_ratio` | 0.02 | 摘要字符预算 = 窗口 × 该比例 × 字符/token 比 |
| 内部常量 `MAX_CHUNK_MESSAGES` | 60 | 单次摘要最多消息数（常量，不随窗口放大） |

## 集成方式

`Consolidator` 在 `AgentLoop` 初始化时创建为 `Arc<Consolidator>`，传入 `AgentRunner`。Runner 在每次 ReAct turn 结束后，若配置了 consolidator 则调用 `maybe_consolidate()`。不传入 consolidator 时（`None`）跳过摘要检查。

## 设计考量

- 每次 ReAct turn 完成后触发一次摘要检查（非循环重试），下一轮 LLM 调用时重新测量
- 摘要通过 `HistoryStore` 统一管理，使用游标机制追加到 `history.jsonl`
- 失败时静默跳过（`.ok().flatten()`），不中断主循环
