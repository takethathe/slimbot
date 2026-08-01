# Tool System

对应模块：`src/tool.rs` + `src/tools/`

## 概述

SlimBot 的工具系统通过 `Tool` trait 定义统一接口，`ToolManager` 管理工具注册、配置和 OpenAI function calling 格式转换。

## Tool Trait

```rust
#[async_trait]
pub trait Tool: Send + Sync {
    fn name(&self) -> &str;
    fn description(&self) -> &str;
    fn parameters(&self) -> serde_json::Value;
    async fn execute(&self, args: serde_json::Value) -> Result<String>;
}
```

实现此 trait 即可接入任意工具。

## TurnContext（per-turn 状态隔离）

`TurnContext` 聚合 per-turn 状态（origin channel/chat_id + `sent_in_turn`），并通过 task-local 隔离，使并发 run（cron/heartbeat 与用户消息共享同一 `Arc<ToolManager>`）各自看到独立状态。

```rust
pub struct TurnContext {
    channel: String,
    chat_id: String,
    sent_in_turn: AtomicBool,
}
```

- `with_turn_context(ctx, fut).await` —— 打开 turn 作用域，`ctx` 在 `fut` 执行期间可被工具读取，future 结束时自动清除（RAII）。
- `current_turn() -> Option<Arc<TurnContext>>` —— 工具在 `execute` 内读取当前 turn context；不在作用域内返回 `None`（如直接单测调用工具）。
- `current_turn_target() -> (String, String)` —— 解析当前 turn 的 origin channel/chat_id；无作用域时返回空串。
- `TurnContext::mark_sent_if_target(channel, chat_id)` —— 仅当目标命中 origin 时标记 `sent_in_turn`（跨 channel 发送不会抑制最终回复）。

`AgentRunner::run()` 在入口构建 `Arc<TurnContext>` 并用 `with_turn_context` 包住整个 ReAct 循环；`MessageTool`/`CronTool` 通过 `current_turn()`/`current_turn_target()` 取值，不再自持 context 副本。作用域外（或空 origin）调用工具时，message 返回 "No target channel/chat specified"、cron 返回 "no session context (channel/chat_id)"。

`TurnContext`/`current_turn()`/`current_turn_target()`/`with_turn_context()` 已从 crate 根 re-export（`slimbot::TurnContext` 等），外部自定义工具可在 `execute` 内读取 turn context。

## ToolManager

```rust
pub struct ToolManager {
    tools: HashMap<String, Box<dyn Tool>>,
    workspace_dir: PathBuf,
}
```

### `init_from_config`

从配置的 `tools` 数组注册工具。如果未配置，默认启用所有内置工具：

| 工具 | 说明 |
|------|------|
| `shell` | Shell 命令执行 |
| `file_reader` | 文件读取 |
| `file_writer` | 文件写入 |
| `file_editor` | 搜索替换编辑 |
| `list_dir` | 目录列表 |
| `make_dir` | 目录创建 |

### `to_openai_functions`

返回所有已注册工具的定义，转换为 OpenAI function calling 格式：

```rust
pub struct ToolDefinition {
    pub name: String,
    pub description: String,
    pub parameters: serde_json::Value,  // JSON Schema
}
```

### `execute`

按名称查找并执行工具。

## 工具结果处理

### 空结果防护

```rust
pub fn ensure_nonempty_tool_result(tool_name: &str, content: &str) -> String
```

空字符串替换为 `(tool_name completed with no output)`，避免 LLM 收到空响应后困惑。

### 错误格式化

```rust
pub fn format_tool_error(error_msg: &str) -> String
```

格式化为 `Error: ...\n\n[Analyze the error above and try a different approach.]`，不中断 ReAct 循环。

### 超长结果持久化

```rust
pub fn persist_tool_result(
    workspace_dir: &Path,
    tool_call_id: &str,
    content: &str,
    max_chars: usize,
) -> String
```

当工具结果超过 `max_chars` 时：
1. 将完整内容写入 `{workspace}/tool-results/{tool_call_id}.txt`（原子写入）
2. 返回引用字符串 + 预览（前 1200 字符）
3. LLM 可通过引用路径读取完整内容

### 头尾截断

通过 `utils::truncate_text_head_tail` 截断超长文本，保留头部和尾部各 2000 字符，中间用省略号代替。

## 内置工具

详见 [内置工具](tools.md)。
