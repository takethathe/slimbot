# ContextBuilder

对应模块：`src/context.rs`

## 概述

`ContextBuilder` 负责构建发送给 LLM 的完整上下文，包括 system prompt 和历史消息。

## 结构

```rust
pub struct ContextBuilder {
    session_manager: SharedSessionManager,    // 会话管理器
    tool_manager: Arc<ToolManager>,           // 工具管理器
    workspace_dir: PathBuf,                   // 工作区目录
    memory_service: SharedMemory,             // 记忆 facade（Arc<tokio::sync::Mutex<MemoryService>>）
    pressure: PressureAnchor,                 // 长度预算的唯一来源（ADR 0013），构造时必填
    connectors: Option<Arc<McpService>>,      // 网关的 MCP 连接池（有 server 配置时才有）
}
```

## System Prompt 构建流程

`ContextBuilder.build_messages(session_id, channel, chat_id, session_summary)` 按以下顺序组装 system prompt：

```
system_parts = []

1. 固定简介（来自嵌入的 identity.md）
   替换 <<runtime>> / <<workspace_path>> / <<platform_policy>> / <<channel_format_hint>>

2. 记忆文档（按 category 依次读取，经 get_effective）
   Agent → [AGENTS.md] <content>
   Soul  → [SOUL.md]  <content>
   Tools → [TOOLS.md] <content>
   User  → [USER.md]  <content>

   规则：缺失 / 空 / 与模板一致的文档返回 None，静默跳过

3. 技能（经 memory backend 枚举，见下）
   区分 always skills（完整加载）和可选 skills（仅显示名称列表）

3b. 可用代理（agents/<name>.md，见下）
   # Available Agents —— 每个代理一行名称+描述；无可用代理时整段不注入

4. 长期记忆（category Core）
   ## Long-term Memory
   <MEMORY.md content>

5. 近期历史（history.jsonl 最近 50 条）
   # Recent History
   - [timestamp] content
   - ...
   预算 = min(provider 窗口 × pressure.recent_history_ratio, 绝对封顶)，由
   `PressureAnchor::limits(会话实测 char_per_token_ratio)` 派生（ADR 0013 的
   第 7 个消费者）；`ContextBuilder::new` 必须传入 anchor，漏传即编译失败。
   固定 50 条与 `... (truncated)` 后缀不变，窗口为 0 时不截断。

6. 会话摘要（可选，来自 Consolidator 的 last_summary）
   [Archived Context Summary]
   <summary>

↓ 用 "\n\n---\n\n" 拼接所有部分
↓
system_prompt

↓ 包装为 Message::System
↓
[Message::System { content: system_prompt }]
  + session 历史消息（未驱逐的部分）
↓
RunContext { messages, tools }
```

## 记忆文档加载

按固定顺序经 `MemoryService::get_effective(category, None)` 读取 4 个 category：

| Category | 标签 | 用途 |
|----------|------|------|
| `Agent` | `[AGENTS.md]` | Agent 工作指令 |
| `Soul` | `[SOUL.md]` | Agent 行为规则与人格 |
| `Tools` | `[TOOLS.md]` | 工具使用指南 |
| `User` | `[USER.md]` | 用户画像 |

`get_effective` 对缺失、空、或与 bootstrap 模板一致的文档返回 `None`，因此未修改的模板不会占用 system prompt token。

## 技能加载

技能经 memory backend 枚举（`list_skill_names()` + `get(Skill, name)`），而非直接遍历文件系统：

- **Always skills**（`always: true` YAML frontmatter）：加载完整内容
- **可选 skills**（`always: false`）：仅显示名称、描述和可加载路径（`skills/<name>/SKILL.md`）列表，由 Agent 按需读取
- 无 frontmatter 的文件：视为 always skill，完整加载

## 代理定义加载

`load_agents_into_parts()` 经 `agent::load_workspace_registry()` 枚举 `workspace_dir/agents/*.md`，生成 `# Available Agents` 段落（与技能段落同级、同样式）：

- 每个通过校验的代理渲染为 `- **name** — description` 一行，正文不进入 system prompt（正文只在子代理运行时作为其专属 system prompt 注入）
- 定义按进程级 `ToolCatalog` 校验：声明了不存在工具的代理被拒绝，不进入列表，仅 debug 日志记录原因
- 段落内含使用指引：子代理在自身会话中执行、通过 `subagent` 工具启动、调用立即返回

技能与代理的分工：skill 是主代理自己按需读取的操作知识（`skills/<name>/SKILL.md`）；agent 是一次性外包执行单元（`agents/<name>.md`，自带工具集与并发闸），详见 [agent.md](agent.md)。

## 会话摘要注入

当 `session_summary` 参数为 `Some` 且非空时，在系统 prompt 注入 `[Archived Context Summary]` 段落。该摘要来自 `Consolidator` 的 `last_summary` 字段，代替已被驱逐的旧消息，提供跨轮次的上下文。

## 工具定义

通过 `ToolManager.to_openai_functions()` 获取所有已注册工具的定义，返回为 `Vec<ToolDefinition>`，包含 `name`、`description`、`parameters`（JSON Schema）。

## 输出

`build_messages()` 返回 `RunContext`：

```rust
pub struct RunContext {
    pub history: Arc<[Message]>,            // 不可变历史消息（零拷贝共享）
    pub current_turn: Vec<Message>,         // 本轮新增消息（含 runtime_content）
    pub system_message: Message,            // 构建的 system prompt
    pub tools: Option<Vec<ToolDefinition>>, // 工具定义
}
```

Runner 在每次 LLM 调用前从 `RunContext` 构建消息引用列表：
```rust
let messages: Vec<&Message> = ctx.history.iter()
    .chain(ctx.current_turn.iter())
    .collect();
provider.chat(&messages, ctx.tools.as_deref()).await;
```

### Runtime content 生命周期

`build_messages()` 在 `current_turn` 的第一个 `Message::User` 上设置 `runtime_content = Some(ctx)`。Provider 序列化时将其 prepend 到 content 字符串。`persist()` 时 strip 掉，**不会写入 JSONL**。

## 注意事项

- 工作区文件在每次 `build()` 时**从磁盘实时读取**，修改文件后无需重启即可生效
- 技能文件同样实时读取，支持动态加载/卸载技能；代理定义同样每轮重新枚举
- 子代理运行时通过 `SessionManager::system_override()` 注入会话级 system prompt（代理正文即 prompt），此时不使用组合式 system prompt
- System prompt 可能很长，需要注意模型的上下文窗口限制
