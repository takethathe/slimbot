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
| `grep` | 内容搜索 |
| `find_files` | 文件发现 |
| `web_fetch` | 网页抓取 |

`tools` 数组只能表达 `create_tool()` 能构造的工具（仅需 workspace 目录）。其余工具在装配处直接 `register`：

| 工具 | 装配位置 | 说明 |
|------|----------|------|
| `memory_search` | `AgentLoop::from_parts` | 主循环只读记忆检索，CLI 与 gateway 恒可用，无法在配置中禁用 |
| `subagent` | `AgentLoop::from_parts`（仅主循环） | 启动子代理；保留工具，agent 定义不可声明 |
| `message` / `cron` | gateway 启动时 | 需要 channel 回调与 cron 运行时 |
| `trigger_source` / `trigger` | gateway 启动时，**只注册进 `ToolCatalog`** | 主循环不可见，仅当 agent 定义显式声明时才装配到子代理 |
| `memory_read` / `memory_write` / `memory_edit` | `DreamService::build_tools` | 只在 Dream 沙箱内注册，沙箱无文件工具 |

## ToolCatalog 与可见性

`ToolCatalog` 是工具工厂的进程级注册表（`src/agent/catalog.rs`）：子代理运行时按定义从 catalog 装配自己的 `ToolManager`，主循环的共享工具表与 prompt cache 因此不受影响。

- gateway 与 CLI 模式都在启动时 `register_builtin_set()` 镜像内置工具，`from_parts` 再补 `memory_search`；gateway 另补 `message` / `cron` / `trigger_source` / `trigger`
- `STATIC_TOOL_NAMES`（13 个）用于在 catalog 尚未组装时（如构建 system prompt）校验工具名
- 保留工具：`FORBIDDEN_AGENT_TOOLS = ["cron", "subagent"]`，agent 声明它们会被整份拒绝（防止绕过触发器层与并发闸）
- `ToolCatalog::install_global()` 后，`agent::known_tool_names()` 与触发器工具可查询当前可见工具集

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
    max_chars: usize,      // pressure.single_tool_result_ratio 派生
    preview_chars: usize,  // pressure.persisted_preview_ratio 派生
) -> String
```

当工具结果超过 `max_chars` 时：
1. 将完整内容写入 `{workspace}/tool-results/{tool_call_id}.txt`（原子写入，已存在则不覆盖）
2. 返回引用字符串 + 预览（前 `preview_chars` 字符）
3. LLM 可通过引用路径读取完整内容

原来的 `agent.max_tool_result_chars` 同时控制"落盘阈值"和"截断长度"，现在拆成两个窗口占比（见 `docs/config.md` 的 `pressure` 段）。

### 头尾截断

通过 `utils::truncate_text_head_tail` 截断超长文本，保留头部和尾部，中间用省略号代替；长度上限同样由窗口派生。

### 上下文豁免：`context_exempt()`

```rust
fn context_exempt(&self) -> bool { false }  // 默认不豁免
```

工具自述自己是否可被上下文压力清理/截断，取代了中央白名单（`EXEMPT_TOOLS` / `COMPACTABLE_TOOLS`）。`ToolManager::is_context_exempt(name)` 查一次自述，`microcompact` 与 `apply_tool_result_budget` 两个阶段共用同一结果；未注册的工具默认不豁免。当前自述为 `true` 的有 `file_reader`、`memory_search`、`message`。

## 内置工具

详见 [内置工具](tools.md)。
