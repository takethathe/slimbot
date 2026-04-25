# CLAUDE.md

本文档在 Claude Code (claude.ai/code) 操作本仓库时提供指导。

## 项目概述

**slimbot** 是一个使用 Rust 开发的 AI 机器人助手，基于 ReAct 循环与 OpenAI 兼容的 LLM API 交互，支持工具调用和多通道（CLI、WebUI 等）。

## 构建和运行

- **构建：** `cargo build`
- **运行：** `cargo run -- <config.json路径>` （默认 `~/.slimbot/config.json`）
- **快速检查：** `cargo check`
- **Rust Edition：** 2024

## 目录结构

```
slimbot/
├── Cargo.toml          # 包清单
├── .gitignore          # 排除 /target
└── src/
    ├── main.rs         # Entry: load config → init AgentLoop → MessageBus → ChannelManager
    ├── config.rs       # Config: config.json read/write, AgentConfig/ProviderConfig/ChannelEntry
    ├── tool.rs         # Tool trait + ToolManager: tool registration & OpenAI function calling conversion
    ├── provider/       # Provider trait + OpenAIProvider: LLM API request/response
    │   ├── mod.rs      # Provider trait, ChatResponse, FinishReason
    │   └── openai.rs   # OpenAIProvider implementation
    ├── session.rs      # Session + SessionManager + SharedSessionManager: FIFO task queue, message management, JSONL persistence
    ├── context.rs      # ContextBuilder: system prompt construction (intro → workspace files → skills → history → channel inject)
    ├── runner.rs       # AgentRunner: ReAct loop core
    ├── agent_loop.rs   # AgentLoop: top-level orchestration, initializes all components
    ├── message_bus.rs  # MessageBus: user request & result delivery
    └── channel/        # Channel trait + CliChannel + ChannelManager + ChannelFactory
        ├── mod.rs      # Channel/ChannelFactory traits, ChannelManager
        └── cli.rs      # CliChannel & CliChannelFactory
```

## 架构

```
AgentLoop ── 初始化 Provider、ToolManager、SessionManager
   │
   ▼
AgentRunner ── ReAct 循环
   ├── ContextBuilder.build() → system prompt + 历史消息 + 工具定义
   ├── Provider.chat() → LLM API 调用
   ├── ToolManager.execute() → 工具执行
   └── SessionManager.persist() → JSONL 持久化
   │
   ▼
MessageBus ── 接收 BusRequest → 封装 SessionTask → 入队/出队 → 调用 AgentRunner
   │
   ▼
ChannelManager ── 从 config.json 按 type 创建通道 → 轮询输入 → 与 MessageBus 交互
```

## 数据目录

```
~/.slimbot/
├── config.json                     # 配置文件
└── workspace/
    ├── agent.md                    # Agent 行为定义
    ├── user.md                     # 用户画像
    ├── soul.md                     # Agent 人格
    ├── tools.md                    # 工具使用指南
    ├── skills/                     # 可选 skill 文件 (*.md)
    └── sessions/
        └── {session_id}.jsonl      # 会话消息持久化
```

## 关键设计决策

- `SharedSessionManager` = `Arc<Mutex<SessionManager>>`，所有模块通过共享 Mutex 访问
- `SessionTask` 绑定 `TaskHook`，通过 `tokio::sync::mpsc` 异步通知 Channel 状态变更
- 消息持久化：每轮 AgentRunner 结束后全量写入 JSONL（非追加）
- Session ID 格式：`{channel_id}:{chat_id}`
- 循环结束条件：超出 `max_iterations` 或模型返回无 tool_calls

## 开发规范

### 工作流程

所有开发任务遵循 **规划 → 实现 → 测试 → 更新文档** 四步流程：

1. **规划**：明确目标和实现方案，确认后再开始编码
2. **实现**：按照设计方案编写代码，包括功能代码和对应的测试代码
3. **测试**：使用 `cargo check` / `cargo build` / `cargo test` 验证代码正确性，确保新增测试通过
4. **更新文档**：在 `docs/` 目录下补充或更新相关文档

### 文档管理

- **README 文档**：放在项目根目录
- **其它所有文档**（设计文档、API 文档、模块说明等）：统一放在 `docs/` 目录下

### Git 提交

- 使用 git 进行代码版本管理
- **提交前必须进行 code review**，确认代码质量和变更合理性
- **commit message 必须使用英文**

### 代码注释

- **所有代码注释必须使用英文**

### 测试

- 功能实现时应同步编写对应的测试代码
- 测试代码放在对应模块的 `#[cfg(test)]` 块或 `tests/` 目录
- 核心逻辑、公共 API、边界条件必须有测试覆盖
