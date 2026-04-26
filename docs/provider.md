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
        messages: &[Message],
        tools: Option<&[ToolDefinition]>,
    ) -> Result<ChatResponse>;
}
```

### 参数

| 参数 | 说明 |
|------|------|
| `messages` | 消息列表（system + 历史 + 用户消息） |
| `tools` | 可选的工具定义列表，用于 OpenAI function calling |

### 返回值

```rust
pub struct ChatResponse {
    pub content: Option<String>,                          // 文本回复
    pub tool_calls: Option<Vec<ToolCall>>,               // 工具调用列表
    pub finish_reason: FinishReason,                     // 结束原因
}

pub enum FinishReason {
    Stop,       // 正常结束
    ToolCalls,  // 触发了工具调用
    Length,     // 超出 token 限制
    Error,      // API 错误
}
```

## OpenAIProvider 实现

`OpenAIProvider` 是 Provider 接口的默认实现，兼容 OpenAI 的 `/v1/chat/completions` API。

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

### URL 解析逻辑

```
if api_url 不为空 → 使用 api_url
else if base_url 不为空 → base_url + "/v1/chat/completions"
else → "https://api.openai.com/v1/chat/completions"
```

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
- `choices[0].message.content` → `ChatResponse.content`
- `choices[0].message.tool_calls` → `ChatResponse.tool_calls`
- `choices[0].finish_reason` → `ChatResponse.finish_reason`

### 认证

通过 `Authorization: Bearer {api_key}` 请求头传递 API Key。

## 扩展自定义 Provider

实现 `Provider` trait 即可接入任意 LLM API：

```rust
pub struct MyProvider { ... }

#[async_trait]
impl Provider for MyProvider {
    async fn chat(&self, messages: &[Message], tools: Option<&[ToolDefinition]>) -> Result<ChatResponse> {
        // 调用你自己的 LLM API
        // 将响应转换为 ChatResponse 格式返回
    }
}
```

## Provider 类型

配置中的 `provider.type` 字段用于区分：

| 类型 | 说明 |
|------|------|
| `openai` | 默认，使用 OpenAI API |
| `custom` | 任意 OpenAI 兼容 API，需设置 `base_url` |

当前实现中，两种类型都使用同一个 `OpenAIProvider`，区别仅在于 URL 配置。
