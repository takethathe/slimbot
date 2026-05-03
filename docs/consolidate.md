# Consolidator

对应模块：`src/consolidate.rs`

## 概述

`Consolidator` 实现基于 token 预算触发的会话摘要机制。当 LLM 返回的 `prompt_tokens` 超过安全预算时，自动选取旧消息块（按用户轮次边界对齐），调用 LLM 进行摘要，将摘要通过 `MemoryStore::append_history()` 追加到 `history.jsonl`，并更新会话的 consolidation 游标。

## 核心类型

```rust
pub struct Consolidator {
    provider: Arc<dyn Provider>,
    session_manager: SharedSessionManager,
    memory_store: SharedMemoryStore,
    context_window_tokens: u32,
    max_completion_tokens: u32,
}
```

构造函数：`new(provider, session_manager, memory_store, context_window_tokens, max_completion_tokens)`

## 预算计算

```
budget    = context_window_tokens - max_completion_tokens - SAFETY_BUFFER (512)
target    = budget / 2
tokens_to_remove = prompt_tokens - target  (当 prompt_tokens > budget 时)
```

当 `prompt_tokens > budget` 时触发摘要，目标是将 token 数降至 `target`。

## Token 估算

```rust
pub fn estimate_message_tokens(msg: &Message, ratio: f64) -> u32
```

- 使用 `session.char_per_token_ratio`（`total_chars / prompt_tokens`）估算，`chars / ratio`
- 默认值为 `4.0`（4 字符/token），`Session::update_token_ratio()` 在每次 LLM 调用后更新该值，持久化到 `.meta.json`

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

`archive()` 方法将选中消息格式化为 `"[ROLE] content"` 文本，调用 LLM 的 `chat()` 方法获取摘要。系统提示要求提取：

- 用户事实（个人信息、偏好、观点）
- 决策（选择、结论）
- 解决方案（通过试错发现的非明显方法）
- 事件（计划、截止日期）
- 偏好（沟通风格、工具偏好）

优先级：用户修正和偏好 > 解决方案 > 决策 > 事件 > 环境事实

跳过：可从源码推导的代码模式、git 历史、已有记忆中的内容。

摘要通过 `MemoryStore::append_history()` 追加，使用统一的游标管理。

## 数据流

```
ReAct turn 完成
  │
  ├─ response.usage.prompt_tokens
  │
  ├─ maybe_consolidate(session_id, prompt_tokens)
  │   ├─ 检查 budget: prompt_tokens <= budget? → 返回
  │   ├─ consolidate_one_round()
  │   │   ├─ 读取 SessionData（messages, ratio, cursor）
  │   │   ├─ pick_consolidation_boundary() → 找到 end_idx
  │   │   ├─ cap_consolidation_boundary() → 限制 chunk 大小
  │   │   ├─ archive() → LLM 摘要
  │   │   │   ├─ 格式化消息为 "[ROLE] content" 文本
  │   │   │   ├─ provider.chat([system_prompt, formatted_messages])
  │   │   │   ├─ 摘要通过 MemoryStore::append_history() 追加到 history.jsonl
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
| `char_per_token_ratio` | `f64` (默认 4.0) | 平均每 token 的字符数，用于精确 token 估算 |
| `last_summary` | `Option<String>` | 最后一次摘要的文本，注入到 system prompt 的 `[Resumed Session]` 段落 |

## 上下文注入

`ContextBuilder.build()` 在 system prompt 中注入 `[Resumed Session]` 段落（在 memory 之后、recent history 之前），代替已驱逐的消息：

```
[Resumed Session] - User prefers dark mode
- Decided to use SQLite over Postgres
- Project deadline is 2024-06-01
```

## 配置

| 配置项 | 默认值 | 说明 |
|--------|--------|------|
| `agent.context_window_tokens` | 8192 | LLM 上下文窗口大小（token） |
| 硬编码 `max_completion_tokens` | 4096 | 最大输出 token 数 |
| 内部常量 `SAFETY_BUFFER` | 512 | 估算误差安全余量 |
| 内部常量 `MAX_CHUNK_MESSAGES` | 60 | 单次摘要最多消息数 |

## 集成方式

`Consolidator` 在 `AgentLoop` 初始化时创建为 `Arc<Consolidator>`，传入 `AgentRunner`。Runner 在每次 ReAct turn 结束后，若配置了 consolidator 则调用 `maybe_consolidate()`。不传入 consolidator 时（`None`）跳过摘要检查。

## 设计考量

- 每次 ReAct turn 完成后触发一次摘要检查（非循环重试），下一轮 LLM 调用时重新测量
- 摘要通过 `MemoryStore` 统一管理，使用游标机制追加到 `history.jsonl`
- 失败时静默跳过（`.ok().flatten()`），不中断主循环
