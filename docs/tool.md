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
    /// The tool's non-configurable risk class (see below).
    fn risk_level(&self) -> RiskLevel;
    async fn execute(&self, args: serde_json::Value) -> Result<String>;
}
```

实现此 trait 即可接入任意工具。

### 风险级：`risk_level()`

每个工具**必须**声明自己的风险级（`slimbot::RiskLevel`），没有默认实现：漏声明是编译错误，而不是静默放宽。风险级是**工具的事实**，不可配置；配置只能决定同一风险级受到多强的管控（见 `docs/config.md` 的 `tools.approval`）。

| 风险级 | 含义 | 内置工具 |
|--------|------|----------|
| `ReadOnly` | 只读，不改动任何状态 | `file_reader`、`list_dir`、`grep`、`find_files`、`memory_search`、`memory_read` |
| `WorkspaceWrite` | 只写工作区内文件 | `file_writer`、`file_editor`、`make_dir`、`cron`、`memory_write`、`memory_edit`、`subagent`、`trigger_source`、`trigger`、`executor` |
| `Exec` | 执行任意命令 | `shell` |
| `NetworkEgress` | 内容离开本机到网络 | `web_fetch` |
| `ExternalSend` | 向外部的人/通道发送 | `message` |

约定：

- **MCP 工具等外部来源一律按 `ExternalSend` 声明**（票 08）：新装一个 server 就等于新增一批对外能力，必须落在最严的那一档。
- `ToolManager::risk_level(name)` 返回 `Option<RiskLevel>`：未注册的工具拿不到风险级，**也执行不了**（`execute` 报 `Tool not found`），因此审批层直接放行让它自己失败——不给 owner 展示"待确认一个不存在的工具"。
- 风险级只在 `ApprovalHook` 里被读取；工具自己不做任何审批判断（一个能力一个入口）。

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
| `codemode` | 沙箱 JS 程序（tools.* + searchTools，票 25） |

`tools` 数组只能表达 `create_tool()` 能构造的工具（仅需 workspace 目录）。其余工具在装配处直接 `register`：

| 工具 | 装配位置 | 说明 |
|------|----------|------|
| `memory_search` | `AgentLoop::from_parts` | 主循环只读记忆检索，CLI 与 gateway 恒可用，无法在配置中禁用 |
| `subagent` | `AgentLoop::from_parts`（仅主循环） | 启动子代理；保留工具，agent 定义不可声明 |
| `message` / `cron` | gateway 启动时 | 需要 channel 回调与 cron 运行时 |
| `trigger_source` / `trigger` | gateway 启动时，**只注册进 `ToolCatalog`** | 主循环不可见，仅当 agent 定义显式声明时才装配到子代理 |
| `executor` | `AgentLoop::from_parts`，**只注册进 `ToolCatalog`** | 主循环不可见；CLI 与 gateway 同源（否则 CLI 下 `agents/executor.md` 会缺工具），见 [executor.md](executor.md) |
| `memory_read` / `memory_write` / `memory_edit` | `DreamService::build_tools` | 只在 Dream 沙箱内注册，沙箱无文件工具 |

## ToolCatalog 与可见性

`ToolCatalog` 是工具工厂的进程级注册表（`src/agent/catalog.rs`）：子代理运行时按定义从 catalog 装配自己的 `ToolManager`，主循环的共享工具表与 prompt cache 因此不受影响。

- gateway 与 CLI 模式都在启动时 `register_builtin_set()` 镜像内置工具，`from_parts` 再补 `memory_search` / `executor`（后者只进 catalog）；gateway 另补 `message` / `cron` / `trigger_source` / `trigger`
- `STATIC_TOOL_NAMES`（16 个）用于在 catalog 尚未组装时（如构建 system prompt）校验工具名；`executor` 在这里而在主循环工具表之外，两处缺一都会让子代理缺工具
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

## 受保护文档：写工具的代码级拒写（ADR 0012）

主模型的记忆写侧是「单文件漏斗」：只用 `file_writer` / `file_editor` 改 `memory/MEMORY.md`。为防它静默改掉自己的人格或对主人的描述，这些文档由**代码**拒写（`src/tools/protected.rs`），不是提示词约束：

| 保护集 | 来源 | 理由 |
|--------|------|------|
| `AGENTS.md`、`USER.md`、`SOUL.md`、`TOOLS.md`、`agents/trigger.md`、`agents/executor.md`、`references/connector.md` | `bootstrap::protected_documents()`（从 bootstrap 列表派生后再去掉 `MEMORY.md`，不维护第二份硬编码清单） | 身份、工作指令与宿主知识，只有 owner 能改 |
| `skills/*/SKILL.md`、`memory/*.md`（`MEMORY.md` 除外） | 路径规则 | Dream 的产出区，由 Dream 写 |

命中即返回**工具结果文本**（不是 `Err`，不退回合、不 panic）：

```
REFUSED: `AGENTS.md` is a protected identity document -- edits to it are owner-only. Nothing was written.
Do not retry with another tool or path; record durable facts in `memory/MEMORY.md` and tell the owner what you wanted to change.
```

约定与边界：

- 判定在 `resolve_workspace_path` **之后**：`sub/../SOUL.md`、指向 `SOUL.md` 的符号链接都会落到同一个判定上；比较是 ASCII 大小写不敏感（macOS 默认文件系统不区分大小写，`soul.md` 不能当绕过）。
- 仅 `file_writer` / `file_editor` 接入。`make_dir` 只建目录（对已存在的文件会报错），改不了既有文档内容，因此不接。
- `shell` **是明写的缺口**：它能绕开任何工具级保护。保护的目标是「误改」，不是防范恶意模型；单 owner 下真正的边界仍然是审批与 trace。
- 运行文件不保护：`cron/jobs.json`、`sessions/*.jsonl`、`HEARTBEAT.md`、`memory/history.jsonl` 照常可写。
- Dream 不受影响：它的 `memory_write` / `memory_edit` 走 memory backend，不经过本模块。

## 内置工具

详见 [内置工具](tools.md)。
