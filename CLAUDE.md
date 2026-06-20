# CLAUDE.md

slimbot 是基于 ReAct 循环的 AI 机器人助手，使用 Rust 开发，支持工具调用和多通道（CLI、WebUI）。

## 构建和运行

- **构建：** `cargo build`
- **运行：** `cargo run -- setup` （初始化配置、目录和 bootstrap 文件），然后 `cargo run -- <config.json路径>` （默认 `~/.slimbot/config.json`）
- **Gateway 模式：** `cargo run -- gateway` （启动 cron、heartbeat 和 enabled channels，包括 WebUI）
- **快速检查：** `cargo check --message-format=json 2>/dev/null | jq -c 'select(.reason == "compiler-message") | {level: .message.level, location: (.message.spans[0] | "\(.file_name):\(.line_start)"), message: .message.message}'` （JSON 输出 + jq 压缩，仅保留诊断信息，减少 token 消耗）
- **代码格式化：** `cargo fmt` / `cargo fmt -- --check` （提交前必须先运行）
- **静态分析：** `cargo clippy` （lint 检查，提交前确保无新 warning）
- **测试：** `cargo test` （提交前确保所有测试通过）
- **Rust Edition：** 2024

## 目录结构

```
slimbot/
├── src/
│   ├── main.rs          # Entry
│   ├── cli.rs           # CLI 参数解析
│   ├── config.rs        # 配置读写
│   ├── bootstrap.rs     # workspace 模板文件
│   ├── setup.rs         # setup 命令
│   ├── agent_loop.rs    # 顶层协调
│   ├── runner.rs        # ReAct 循环核心
│   ├── session.rs       # 会话管理 + JSONL 持久化
│   ├── context.rs       # system prompt 构建
│   ├── message_bus.rs   # 消息总线
│   ├── tool.rs          # Tool trait + ToolManager
│   ├── tools/           # 内置工具实现
│   ├── channel/         # CLI + WebUI 通道
│   ├── gateway.rs       # Gateway 模式入口
│   ├── cron/            # 定时任务
│   ├── heartbeat/       # 心跳服务
│   ├── provider/        # LLM API 封装
│   ├── consolidate.rs   # 会话摘要压缩
│   ├── memory.rs        # 记忆存储
│   └── snip.rs          # 上下文截断
└── docs/                # 文档目录
```

## 架构

**CLI 模式：** `ChannelManager` 轮询输入 → `MessageBus` 封装任务 → `AgentRunner` 执行 ReAct 循环 → 结果回传通道。

**Gateway 模式：** 额外启动 `CronService`（定时任务）和 `HeartbeatService`（心跳检测），触发 `AgentLoop.run_task()`。WebUI 通过 SSE 广播 `AgentEvent`。

**核心数据流：** `ContextBuilder` 构建消息 → `Provider.chat()` 调用 LLM → `ToolManager.execute()` 执行工具 → `SessionManager.persist()` 持久化 JSONL。

## 数据目录

`~/.slimbot/workspace/` 包含：`AGENTS.md`、`USER.md`、`SOUL.md`、`TOOLS.md`（setup 生成）、`HEARTBEAT.md`、`skills/`、`cron/jobs.json`、`sessions/{id}.jsonl`。

`data_dir` 和 `workspace_dir` 独立配置，后者默认 `{data_dir}/workspace`。未修改的 bootstrap 文件会被 `context.rs` 跳过。

## 关键设计决策

- 消息持久化：默认 append-only JSONL，`consolidated_lines > 0` 时全量重写
- Session 双列表：`history: Arc<[Message]>`（已持久化）+ `current_turn: Vec<Message>`（本轮缓冲）
- Provider 接口：`Provider::chat(&[&Message])` 引用传递，避免 clone
- Session ID 格式：`{channel_id}:{chat_id}`
- 循环结束：超出 `max_iterations` 或模型返回无 tool_calls

## 开发规范

### 工作流程

所有开发任务遵循 **规划 → 测试 → 实现 → 验证** 四步流程：

1. **确认需求**：收到用户需求后，先确认理解是否正确，再开始后续工作
2. **规划（Plan）**：明确目标，分解为可测试的行为点，确认后再开始编码
3. **测试先行（Test First）**：为每个行为点编写测试用例，描述输入/输出/边界条件，此时测试应失败
4. **实现（Implement）**：编写最小代码使测试通过，同步更新文档
5. **验证（Verify）**：运行 `cargo check`、`cargo build`、`cargo test` 确保所有测试通过，更新文档和 TODO

### TDD 实践

- **测试命名**：`test_<功能>_<场景>_<预期结果>`，如 `test_parse_config_missing_field_returns_default`
- **测试粒度**：每个公共函数/方法至少一个单元测试，复杂逻辑覆盖边界条件
- **测试位置**：`#[cfg(test)] mod tests` 在对应模块内，集成测试放 `tests/` 目录
- **运行测试**：`cargo test <模块名>` 运行特定模块，`cargo test` 运行全部

### 文档管理

- **README 文档**：放在项目根目录
- **其它所有文档**（设计文档、API 文档、模块说明等）：统一放在 `docs/` 目录下
- **TODO 管理**：新增功能计划或代码中的 TODO 必须以简洁的语言记录到 `docs/TODO.md` 中。工作流程的"验证"步骤需包含：添加/完成 TODO 记录，以及编写相关技术文档

### Git 提交

- 使用 git 进行代码版本管理
- **提交前必须进行 code review**，确认代码质量和变更合理性
- **commit message 必须使用英文**
- **绝对禁止在 commit message 中添加 `Co-Authored-By: Claude` 或任何将 Claude / AI 列为贡献者的内容。这是硬性规则，无例外。**

### 代码注释

- **所有代码注释必须使用英文**
