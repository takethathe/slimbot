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
    memory_store: SharedMemoryStore,          // 长期记忆存储（Arc<tokio::sync::Mutex<MemoryStore>>）
}
```

## System Prompt 构建流程

`ContextBuilder.build(session_id, channel_inject, session_summary)` 按以下顺序组装 system prompt：

```
system_parts = []

1. 固定简介
   "You are SlimBot, an AI assistant. You can call tools to help the user complete tasks."

2. 工作区文件（依次读取）
   agent.md  → [agent.md] <content>
   user.md   → [user.md] <content>
   soul.md   → [soul.md] <content>
   tools.md  → [tools.md] <content>

   规则：文件必须存在、非空才会被加入

3. 技能文件（skills/ 目录下的所有 *.md）
   区分 always skills（完整加载）和可选 skills（仅显示名称列表）

4. 长期记忆（MEMORY.md）
   ## Long-term Memory
   <MEMORY.md content>

4.5. 会话摘要（可选，来自 Consolidator 的 last_summary）
   [Resumed Session] <summary bullet points>
   代替已驱逐的消息，提供会话上下文

5. 近期历史（history.jsonl 最近 50 条）
   # Recent History
   - [timestamp] content
   - ...

6. 通道注入内容（可选）
   来自 Channel.prepare_inject() 的返回值

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

## 工作区文件加载

固定加载以下 4 个文件（按顺序）：

| 文件 | 用途 |
|------|------|
| `agent.md` | Agent 行为定义 |
| `user.md` | 用户画像 |
| `soul.md` | Agent 人格 |
| `tools.md` | 工具使用指南 |

每个文件必须存在且非空才会被加入 system prompt。不存在的文件会被静默跳过。

## 技能文件加载

扫描 `workspace/skills/` 目录下所有 `.md` 文件：

- **Always skills**（`always: true` YAML frontmatter）：加载完整内容
- **可选 skills**（`always: false`）：仅显示名称和描述列表，由 Agent 按需读取
- 无 frontmatter 的遗留文件：视为 always skill，完整加载

## 会话摘要注入

当 `session_summary` 参数为 `Some` 且非空时，在系统 prompt 中注入 `[Resumed Session]` 段落。该摘要来自 `Consolidator` 的 `last_summary` 字段，代替已被驱逐的旧消息，提供跨轮次的上下文。

## 工具定义

通过 `ToolManager.to_openai_functions()` 获取所有已注册工具的定义，返回为 `Vec<ToolDefinition>`，包含 `name`、`description`、`parameters`（JSON Schema）。

## 输出

`build()` 返回 `RunContext`：

```rust
pub struct RunContext {
    pub messages: Vec<Message>,     // system + 历史消息
    pub tools: Option<Vec<ToolDefinition>>,  // 工具定义
}
```

## 注意事项

- 工作区文件在每次 `build()` 时**从磁盘实时读取**，修改文件后无需重启即可生效
- 技能文件同样实时读取，支持动态加载/卸载技能
- System prompt 可能很长，需要注意模型的上下文窗口限制
