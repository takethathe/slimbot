# Provider

对应模块：`src/provider/mod.rs` + `src/provider/openai.rs`

## 概述

`Provider` 是 SlimBot 与 LLM API 交互的抽象层，定义了统一的接口并提供了 OpenAI 兼容的实现。

## Provider 接口

```rust
#[async_trait]
pub trait Provider: Send + Sync {
    async fn chat(
        &self,
        messages: &[&Message],
        tools: Option<&[ToolDefinition]>,
        recorder: Option<&dyn SpanRecorder>,
    ) -> Result<LLMResponse>;

    // 流式变体，默认实现降级为非流式（见下）
    async fn chat_stream(
        &self,
        messages: &[&Message],
        tools: Option<&[ToolDefinition]>,
        recorder: Option<&dyn SpanRecorder>,
        on_signal: &mut (dyn FnMut(StreamSignal) + Send),
    ) -> Result<LLMResponse> { /* 默认：调 chat()，整段内容作为单个 Content 信号发出 */ }
}
```

### 参数

| 参数 | 说明 |
|------|------|
| `messages` | 消息引用列表（system + 历史 + 用户消息），避免 clone 开销 |
| `tools` | 可选的工具定义列表，用于 OpenAI function calling |
| `recorder` | 可选的 span 记录器，用于把请求体、模型名、token usage 等写入 trace |
| `on_signal` | （仅 `chat_stream`）流式信号回调，随到达逐段交付：`StreamSignal::Content(String)` 为内容增量（参与渲染/编译），`StreamSignal::Activity` 为传输层存活心跳（无任何可展示内容但流是活的，例如 reasoning chunk、role 开场块、keep-alive） |

### `runtime_content` 处理

`Message::User` 可能携带 `runtime_content: Option<String>` 字段（由 `ContextBuilder.build_messages()` 设置）。Provider 序列化时将其 prepend 到 content 字符串前：

```rust
Message::User { content, runtime_content, .. } => {
    let mut json_content = content.to_openai_value();
    if let Some(ctx) = runtime_content {
        // prepend ctx to content
    }
    // ...
}
```

`runtime_content` 通过 `skip_serializing_if` 保证**永远不会写入 JSONL**。

### 返回值

```rust
pub struct LLMResponse {
    pub content: Option<String>,            // 文本回复
    pub tool_calls: Option<Vec<ToolCall>>,  // 工具调用列表
    pub finish_reason: FinishReason,        // 结束原因
    pub usage: Usage,                       // token 用量
}

pub struct Usage {
    pub prompt_tokens: u32,
    pub prompt_cache_hit_tokens: u32,
    pub completion_tokens: u32,
    pub total_tokens: u32,
}

pub enum FinishReason {
    Stop,       // 正常结束
    ToolCalls,  // 触发了工具调用
    Length,     // 超出 token 限制
    Error,      // API 错误
}
```

`usage` 是上下文压缩（consolidate 的 token-budget 触发）与 trace attribute（`gen_ai.usage.*`）的数据源。

## 流式：`chat_stream`

`chat_stream` 是 `chat` 的流式变体：每个流式信号经 `on_signal` 回调交付（`StreamSignal::Content` / `StreamSignal::Activity`，见参数表），返回的 `LLMResponse` 携带累积后的完整结果（含 usage）。信号区分的目的：消费方（如 CardService）可以把超时做成**无进展窗口**——任何信号到达即续命，而不必把思考模型的长推理阶段误杀为超时。

**默认降级**：trait 的默认实现直接调 `chat()`，把整段 content 作为**单个 `Content` 信号**发出。因此只实现 `chat` 的 provider（及测试替身如 `MockProvider`）无需任何改动即可满足 `chat_stream` 契约；调用方拿到的行为是「一次性内容」，不是失败。注意：降级 provider 在调用完成前不发任何信号，对无进展窗口而言它表现为总时长上限（不会因降级而提前死，也不会得到续命）。

**OpenAI 真流式**（`src/provider/openai.rs`）：

- 请求体带 `"stream": true` 与 `"stream_options": { "include_usage": true }`，token usage 从尾部的独立 usage chunk 读取。
- SSE 解析由 `SseLineReader` 完成：缓冲原始字节、按 `\n` 切行，**事件跨网络 chunk 不会被切碎**（UTF-8 续字节永不等于 `0x0A`，按字节切行不会损坏半字符）；只消费 `data:` 字段，空行/注释/`event:`/`id:` 忽略；流末尾未换行的尾巴在 `finish()` 时补交付。`data: [DONE]` 终止。
- **信号映射**（`handle_data` 三态）：合法 chunk 且 `delta.content` 非空 → `Content`；合法但无内容的 chunk（role 开场、`reasoning_content` 思考块、finish_reason、usage、`[DONE]`）→ `Activity`；坏行 → 不发信号。
- **坏行跳过**：流中某行 JSON 解析失败只跳过该 chunk，不中止整次生成（与非流式路径「整次调用失败」的语义不同）。
- **带 tools 回落非流式**：当前真流式实现只覆盖纯内容生成（卡片调用从不传 tools）；`chat_stream(tools=Some(..))` 回落到非流式 `chat()` + 单个 `Content` 信号，避免在此叠加增量 tool_call 拼接的复杂度。

## OpenAIProvider 实现

`OpenAIProvider` 是 Provider 接口的默认实现，兼容 OpenAI 的 `/v1/chat/completions` API。

**思考模式关闭开关**：provider 配置的 `disable_thinking: true` 会在请求体（非流式与流式都适用）注入 `"enable_thinking": false`（DashScope 兼容接口的扩展字段，qwen3 类思考模型据此跳过推理阶段）；未标记的 provider 不发该字段，行为逐字不变。卡片生成路径的 provider 由 `card_provider_config` 强制置 true：卡片是机械的转录→JSONL 转换，思考只会白白拖慢速度。

### 初始化

从 `ProviderConfig` 创建：

```rust
OpenAIProvider::new(provider_config)
```

`ProviderConfig` 包含：
- `api_url` — 完整 API 端点 URL
- `api_key` — 认证密钥
- `model` — 模型名称
- `temperature` — 采样温度
- `max_tokens` — 最大响应 token 数
- `prompt_cache_enabled` — 是否启用 prompt 缓存（默认 `true`）

### URL 解析逻辑

```
if api_url 不为空 → 使用 api_url
else if base_url 不为空:
    if base_url 以 "/chat/completions" 结尾 → 直接使用
    else if base_url 以 "/v1" 结尾 → base_url + "/chat/completions"
    else → base_url + "/v1/chat/completions"
else → "https://api.openai.com/v1/chat/completions"
```

### Prompt Cache

当 `prompt_cache_enabled` 为 `true`（默认）时，OpenAIProvider 会在最后一条 system message 的 content 字段上注入 `cache_control: {"type": "ephemeral"}`。这使得支持该格式的 provider（如 Anthropic 兼容接口）能够缓存该消息，减少重复请求的 token 消耗。

```json
{
  "role": "system",
  "content": [
    {"type": "text", "text": "...", "cache_control": {"type": "ephemeral"}}
  ]
}
```

设置为 `false` 时，所有消息以纯字符串 content 发送。

### 请求格式

```json
{
  "model": "gpt-4o",
  "messages": [...],
  "tools": [...],
  "temperature": 0.7,
  "max_tokens": 4096
}
```

### 响应解析

从 OpenAI 格式响应中提取：
- `choices[0].message.content` → `LLMResponse.content`
- `choices[0].message.tool_calls` → `LLMResponse.tool_calls`
- `choices[0].finish_reason` → `LLMResponse.finish_reason`
- `usage` → `LLMResponse.usage`（流式时来自 `include_usage` 的尾部 chunk）

### 认证

通过 `Authorization: Bearer {api_key}` 请求头传递 API Key。

## 扩展自定义 Provider

实现 `Provider` trait 即可接入任意 LLM API：

```rust
pub struct MyProvider { ... }

#[async_trait]
impl Provider for MyProvider {
    async fn chat(&self, messages: &[&Message], tools: Option<&[ToolDefinition]>, recorder: Option<&dyn SpanRecorder>) -> Result<LLMResponse> {
        // 调用你自己的 LLM API
        // 将响应转换为 LLMResponse 格式返回
    }
}
```

只需实现 `chat`；若上游 API 支持流式，再覆写 `chat_stream` 逐段交付信号（内容增量发 `Content`，无可展示内容但流存活时发 `Activity`，参考 `OpenAIProvider`）。

## Provider 类型

配置中的 `provider.type` 字段用于区分：

| 类型 | 说明 |
|------|------|
| `openai` | 默认，使用 OpenAI API |
| `custom` | 任意 OpenAI 兼容 API，需设置 `base_url` |

当前实现中，两种类型都使用同一个 `OpenAIProvider`，区别仅在于 URL 配置。
