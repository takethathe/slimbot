# CLAUDE.md

本文档在 Claude Code (claude.ai/code) 操作本仓库时提供指导。

## 项目概述

**slimbot** 是一个使用 Rust 开发的 AI 机器人助手，基于 ReAct 循环与 OpenAI 兼容的 LLM API 交互，支持工具调用和多通道（CLI、WebUI 等）。

## 构建和运行

- **构建：** `cargo build`
- **运行：** `cargo run -- setup` （初始化配置、目录和 bootstrap 文件），然后 `cargo run -- <config.json路径>` （默认 `~/.slimbot/config.json`）
- **Gateway 模式：** `cargo run -- gateway` （启动 cron、heartbeat 和 enabled channels，包括 WebUI）
- **快速检查：** `cargo check --message-format=json 2>/dev/null | jq -c 'select(.reason == "compiler-message") | {level: .message.level, location: (.message.spans[0] | "\(.file_name):\(.line_start)"), message: .message.message}'` （JSON 输出 + jq 压缩，仅保留诊断信息，减少 token 消耗）
- **Rust Edition：** 2024

## 目录结构

```
slimbot/
├── Cargo.toml          # 包清单
├── .gitignore          # 排除 /target
├── docs/
│   ├── config.md       # 配置指南
│   ├── tools.md        # 内置工具说明
│   └── logging.md      # 日志系统说明
└── src/
    ├── main.rs         # Entry: load config → init Logger → AgentLoop → MessageBus → ChannelManager
    ├── log.rs          # Logging: LogLevel enum, global singleton logger, init/log/should_log
    ├── macros.rs       # Logging macros: debug!, info!, warn!, error!, fatal!
    ├── cli.rs          # CLI: clap argument parsing, run_agent_session
    ├── config.rs       # Config: config.json read/write, AgentConfig/ProviderConfig/ChannelConfig/GatewayConfig
    ├── config_scheme.rs # ConfigScheme: default values, normalization, validation
    ├── bootstrap.rs    # Bootstrap templates: AGENTS.md, USER.md, SOUL.md, TOOLS.md
    ├── setup.rs        # Setup command: config normalization + directory + bootstrap creation
    ├── tool.rs         # Tool trait + ToolManager: tool registration & OpenAI function calling conversion
    ├── tools/          # Built-in tool implementations
    │   ├── mod.rs      # Factory function + resolve_data_path() path validation
    │   ├── shell.rs    # Shell command execution
    │   ├── file_reader.rs  # File reading
    │   ├── file_writer.rs  # File writing
    │   ├── file_editor.rs  # Search-and-replace editing
    │   ├── list_dir.rs     # Directory listing
    │   ├── make_dir.rs     # Directory creation
    │   ├── message.rs      # Message delivery to channels
    │   └── cron.rs         # Cron job management (add/list/remove)
    ├── gateway.rs      # Gateway mode: cron + heartbeat + channels + ctrl+c
    ├── cron/           # Cron service: scheduling, persistence, tick execution
    │   ├── mod.rs
    │   ├── types.rs    # CronSchedule, CronJob, CronPayload, CronStore
    │   └── service.rs  # CronService: CRUD, load/save, tick, compute_next_run
    ├── heartbeat/      # Heartbeat service
    │   ├── mod.rs
    │   └── service.rs  # HeartbeatService: periodic HEARTBEAT.md reading + LLM execution
    ├── provider/       # Provider trait + OpenAIProvider: LLM API request/response
    │   ├── mod.rs      # Provider trait, ChatResponse, FinishReason
    │   └── openai.rs   # OpenAIProvider implementation
    ├── session.rs      # Session + SessionManager + SharedSessionManager: FIFO task queue, message management, JSONL persistence
    ├── context.rs      # ContextBuilder: system prompt construction (intro → workspace files → skills → history → channel inject)
    ├── runner.rs       # AgentRunner: ReAct loop core
    ├── agent_loop.rs   # AgentLoop: top-level orchestration, initializes all components
    ├── message_bus.rs  # MessageBus: user request & result delivery
    └── channel/        # Channel trait + CliChannel + WebuiChannel + ChannelManager + ChannelFactory
        ├── mod.rs      # Channel/ChannelFactory traits, ChannelManager
        ├── cli.rs      # CliChannel & CliChannelFactory
        └── webui.rs    # WebuiChannel: axum HTTP server with SSE, embedded index.html
```

## 架构

### CLI 模式

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

### Gateway 模式

```
run_gateway()
   ├── CronService ── 定时任务调度，JSON 持久化，tick 驱动
   │   └── on_job → AgentLoop.run_task()
   ├── HeartbeatService ── 定期读取 HEARTBEAT.md，LLM 判断执行
   │   └── on_execute → AgentLoop.run_task()
   ├── AgentLoop ── 预注册 message + cron 工具的 ToolManager
   │   ├── message tool ── 通过 MessageBus 发送结果到指定 channel:chat_id
   │   └── cron tool ── 通过 CronService 管理定时任务
   └── ChannelManager ── 启动 enabled channels (CLI + WebUI)
       ├── CLI channel ── stdin/stdout
       └── WebUI channel ── axum HTTP server (SSE + REST API)
```

## 数据目录

```
~/.slimbot/                         # data_dir (默认，运行时数据)
└── workspace/                      # workspace_dir (默认，工作区)
    ├── AGENTS.md                   # Agent 行为定义（由 setup 生成）
    ├── USER.md                     # 用户画像（由 setup 生成）
    ├── SOUL.md                     # Agent 人格（由 setup 生成）
    ├── TOOLS.md                    # 工具使用指南（由 setup 生成）
    ├── HEARTBEAT.md                # Heartbeat 任务定义（gateway 模式）
    ├── skills/                     # 可选 skill 文件 (*.md)
    ├── cron/                       # Cron 持久化目录（gateway 模式）
    │   └── jobs.json               # 定时任务 JSON 存储
    └── sessions/
        └── {session_id}.jsonl      # 会话消息持久化
```

`data_dir` 和 `workspace_dir` 是两个独立配置项。`workspace_dir` 默认值为 `{data_dir}/workspace`。

运行 `cargo run -- setup` 时自动创建上述目录和 4 个 bootstrap 文件。若文件未修改（与模板一致），`context.rs` 加载时会跳过，节省 token。

## 关键设计决策

- `SharedSessionManager` = `Arc<Mutex<SessionManager>>`，所有模块通过共享 Mutex 访问
- `SessionTask` 绑定 `TaskHook`，通过 `tokio::sync::mpsc` 异步通知 Channel 状态变更
- 消息持久化：每轮 AgentRunner 结束后全量写入 JSONL（非追加）
- Session ID 格式：`{channel_id}:{chat_id}`
- 循环结束条件：超出 `max_iterations` 或模型返回无 tool_calls

## 开发规范

### 工作流程

所有开发任务遵循 **规划 → 实现 → 测试 → 更新文档** 四步流程：

1. **确认需求**：收到用户需求后，先确认理解是否正确，再开始后续工作
2. **规划**：明确目标和实现方案，确认后再开始编码
3. **实现**：按照设计方案编写代码，包括功能代码和对应的测试代码
4. **测试**：使用 `cargo check --message-format=json`（配合 jq 压缩输出）/ `cargo build` / `cargo test` 验证代码正确性，确保新增测试通过
5. **更新文档**：在 `docs/` 目录下补充或更新相关文档

### 文档管理

- **README 文档**：放在项目根目录
- **其它所有文档**（设计文档、API 文档、模块说明等）：统一放在 `docs/` 目录下
- **TODO 管理**：新增功能计划或代码中的 TODO 必须以简洁的语言记录到 `docs/TODO.md` 中。工作流程的"更新文档"步骤需包含：添加/完成 TODO 记录，以及编写相关技术文档

### Git 提交

- 使用 git 进行代码版本管理
- **提交前必须进行 code review**，确认代码质量和变更合理性
- **commit message 必须使用英文**
- **绝对禁止在 commit message 中添加 `Co-Authored-By: Claude` 或任何将 Claude / AI 列为贡献者的内容。这是硬性规则，无例外。**

### 代码注释

- **所有代码注释必须使用英文**

### 测试

- 功能实现时应同步编写对应的测试代码
- 测试代码放在对应模块的 `#[cfg(test)]` 块或 `tests/` 目录
- 核心逻辑、公共 API、边界条件必须有测试覆盖
