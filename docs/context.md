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
    memory_store: Arc<MemoryStore>,           // 长期记忆存储
}
```

## System Prompt 构建流程

`ContextBuilder.build()` 按以下顺序组装 system prompt：

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
   [Skill: <filename>] <content>

4. 通道注入内容（可选）
   来自 Channel.prepare_inject() 的返回值

↓ 用 "\n\n---\n\n" 拼接所有部分
↓
system_prompt

↓ 包装为 Message::System
↓
[Message::System { content: system_prompt }]
  + session 历史消息
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

扫描 `workspace/skills/` 目录下所有 `.md` 文件，每个文件以 `[Skill: <文件名>]` 为前缀加入 system prompt。未修改的模板文件（与嵌入模板一致）会被跳过，以节省 token。

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
