# Snip：上下文截断

对应模块：`src/snip.rs`，压力锚与派生限制见 `src/pressure.rs`。

Snip 在每次调用 LLM 之前运行，保证 prompt 不超出模型的 token 上限。它无状态、不调用 LLM，只按预算丢弃最旧的消息。

## 预算计算

```
budget = window_tokens − max_output_tokens − snip_safety_tokens
snip_safety_tokens = clamp(window_tokens × pressure.snip_safety_ratio, 128, 8192)
```

- `window_tokens` — 来自 `providers.<agent.provider>.context_window_tokens`（唯一绝对锚）
- `max_output_tokens` — 来自当前 provider 的 `max_tokens`
- 安全余量 — 窗口的 `snip_safety_ratio`（默认 3%），用于吸收 tokenizer 估算误差

`saturating_sub` 保证不会下溢：窗口不足以容纳输出预留时，预算为 `0`，Snip 原样返回（不做猜测式截断）。

## 消息选择

1. 保留全部 system 消息
2. 从尾部向前累加，保留能放进预算的最近消息
3. 保证第一条非 system 消息是 User（GLM 兼容）
4. 找不到 User 时，按 `pressure.min_keep_messages`（默认 4）兜底

## Token 估算

```
tokens = max(ceil(message_chars / char_per_token_ratio), 4)
```

`char_per_token_ratio` 取会话实测值（每次 LLM 调用后用 `total_chars / prompt_tokens` 更新并持久化）；冷启动默认 **3.0**（`pressure::DEFAULT_CHAR_PER_TOKEN_RATIO`）。

## 两阶段清理与可找回性

`PreChat` 阶段（`GovernanceHook`）依次执行：

| 函数 | 作用 |
|------|------|
| `snip::microcompact` | 把 `keep_recent_tool_results`（默认 10）之外的旧工具结果替换为占位符；内容短于 500 字符的不动 |
| `snip::apply_tool_result_budget` | 把超过 `single_tool_result_chars` 的工具结果按头尾截断 |
| `snip::snip_messages` | 按上面的预算裁剪消息条数 |

清理与找回成对出现：

- 占位符形如 `[{tool} result omitted · full output: tool-results/<tool_call_id>.txt]`
- 截断文本形如 `...\n... (truncated · full output: tool-results/<tool_call_id>.txt)`
- 清理时若发现该结果从未落盘（`AfterTool` 阶段没超阈值，或 `persist_tool_results = false`），**先补落盘**再写引用；落盘失败才退化为不带路径的 `(truncated)`
- `tool-results/<id>.txt` 全量内容由 Dream 成功收尾时的 TTL 清扫（`pressure.tool_results_ttl_days`，默认 7 天）回收，没有独立定时器

## 上下文豁免是工具自述

`Tool::context_exempt()`（默认 `false`）决定一条结果是否可被清理/截断。两个阶段读同一个自述，不存在外部白名单：

| 工具 | 自述 | 理由 |
|------|------|------|
| `file_reader` | `true` | 自身已按行区间限长，替换成占位符只会逼模型重读 |
| `memory_search` | `true` | 丢失命中只是重查一次，但截断会破坏事实 |
| `message` | `true` | 投递正文必须逐字保留 |
| `web_fetch`、`shell`、`grep` 等 | `false` | 结果可重取，按压力裁剪 |

未注册的工具（如 MCP）默认不豁免。

## 与 Consolidation 的关系

| 维度 | Snip | Consolidation |
|------|------|---------------|
| 时机 | 每次 LLM 调用前 | 每轮 ReAct 结束后 |
| 触发 | 估算 token 超预算 | provider 上报的 `prompt_tokens` 超触发线 |
| 动作 | 丢弃旧消息（仅内存） | 用 LLM 摘要旧消息 |
| 状态 | 无状态 | 有状态（JSONL + meta） |
| 成本 | 零（无 LLM 调用） | 每次一次 LLM 调用 |

同一轮中可以都发生：先 Snip 防止立即溢出，轮次执行完再按 `prompt_tokens` 判断是否摘要。

## 窗口翻倍的效果

预算、单条结果上限、落盘预览、摘要预算都按同一锚等比放大：`context_window_tokens` 从 32768 改成 65536，Snip 可保留的历史窗口与 Consolidation 的触发/目标 token 数都翻倍，无需逐项改配置（`test_snip_messages_doubling_window_doubles_kept_window`、`test_consolidate_limits_double_with_window`）。

## 示例

`context_window_tokens = 32768`、`max_tokens = 4096`、默认比例：

```
snip_safety_tokens = clamp(32768 × 0.03, 128, 8192) = 983
budget = 32768 − 4096 − 983 = 27689 tokens
```

单条工具结果上限 = `32768 × 0.25 × 3.0 = 24576` 字符；落盘预览 = `32768 × 0.04 × 3.0 = 3930` 字符。