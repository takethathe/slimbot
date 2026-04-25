# SlimBot

一个使用 Rust 开发的轻量级 AI 聊天机器人，基于 ReAct（推理 + 行动）模式，兼容 OpenAI 的 LLM API。支持工具调用和多通道 I/O（CLI、WebUI 等）。

## 特性

- **ReAct 循环** — Agent 按周期运行：构建上下文 → 调用 LLM → 执行工具 → 重复直到得出最终答案。
- **工具调用** — 可扩展的 `Tool` trait 工具系统，支持 OpenAI function calling 格式转换。
- **多通道** — 通过 `Channel` / `ChannelFactory` trait 实现可插拔通道架构，内置 CLI 通道。
- **会话持久化** — 基于 JSONL 的会话存储，自动加载和保存。
- **工作区驱动的上下文** — System prompt 由工作区文件（`agent.md`、`user.md`、`soul.md`、`tools.md`）和可选的技能文件（`skills/*.md`）组装而成。
- **完全可配置** — LLM 提供商、Agent 行为、通道和工具均通过单一 `config.json` 驱动。

## 快速开始

### 前置条件

- Rust 2024 edition 工具链（`rustc`、`cargo`）

### 构建

```bash
cargo build
```

### 运行

```bash
# 指定配置文件路径
cargo run -- /path/to/config.json

# 不传参数则使用默认路径 ~/.slimbot/config.json
cargo run
```

### 配置文件

创建 `~/.slimbot/config.json`：

```json
{
  "data_dir": "/home/user/.slimbot",
  "provider": {
    "api_url": "https://api.openai.com/v1/chat/completions",
    "api_key": "sk-...",
    "model": "gpt-4o",
    "temperature": 0.7,
    "max_tokens": 4096
  },
  "agent": {
    "max_iterations": 40,
    "timeout_seconds": 120
  },
  "tools": [
    { "name": "shell", "enabled": true }
  ],
  "channels": [
    {
      "type": "cli",
      "enabled": true,
      "config": { "prompt": "> " }
    }
  ]
}
```

### 工作区

工作区目录（`~/.slimbot/workspace/`）中的文件用于塑造 Agent 的行为：

| 文件 | 用途 |
|---|---|
| `agent.md` | Agent 行为定义 |
| `user.md` | 用户画像 |
| `soul.md` | Agent 人格 |
| `tools.md` | 工具使用指南 |
| `skills/*.md` | 可选技能文件 |

会话数据存储在 `~/.slimbot/workspace/sessions/{session_id}.jsonl`。

## 架构

```
AgentLoop ── 初始化 Provider、ToolManager、SessionManager
   │
   ▼
AgentRunner ── ReAct 循环
   ├── ContextBuilder.build() → system prompt + 历史消息 + 工具定义
   ├── Provider.chat()        → LLM API 调用
   ├── ToolManager.execute()  → 工具执行
   └── SessionManager.persist() → JSONL 持久化
   │
   ▼
MessageBus ── 接收 BusRequest → 封装 SessionTask → 入队/出队 → 调用 AgentRunner
   │
   ▼
ChannelManager ── 从 config.json 按 type 创建通道 → 轮询输入 → 与 MessageBus 交互
```

### 关键设计决策

- **共享访问**：`SharedSessionManager = Arc<Mutex<SessionManager>>`，所有模块通过共享 Mutex 访问。
- **状态事件**：`SessionTask` 携带 `TaskHook`，通过 `tokio::sync::mpsc` 向通道发送状态变更。
- **持久化**：每轮 AgentRunner 循环结束后全量写入 JSONL（追加模式，非覆盖）。
- **Session ID 格式**：`{channel_id}:{chat_id}`
- **循环终止条件**：超出 `max_iterations` 或模型返回无 tool_calls 时结束。

## 项目结构

```
slimbot/
├── Cargo.toml
├── .gitignore
└── src/
    ├── main.rs         # 入口：加载配置 → 初始化 AgentLoop → MessageBus → ChannelManager
    ├── config.rs       # 配置结构体：JSON 加载、ProviderConfig、AgentConfig、ChannelEntry
    ├── tool.rs         # Tool trait + ToolManager：工具注册与 OpenAI function calling 转换
    ├── provider/       # LLM 提供商抽象
    │   ├── mod.rs      # Provider trait、ChatResponse、FinishReason
    │   └── openai.rs   # OpenAIProvider 实现
    ├── session.rs      # Session + SessionManager：FIFO 队列、消息管理、JSONL 持久化
    ├── context.rs      # ContextBuilder：从工作区文件 + 技能 + 历史组装 system prompt
    ├── runner.rs       # AgentRunner：ReAct 循环核心
    ├── agent_loop.rs   # AgentLoop：顶层编排
    ├── message_bus.rs  # MessageBus：请求路由与结果传递
    └── channel/        # I/O 通道抽象
        ├── mod.rs      # Channel/ChannelFactory trait、ChannelManager
        └── cli.rs      # CliChannel & CliChannelFactory
```

## 开发

```bash
cargo check    # 快速检查编译
cargo build    # 完整构建
cargo test     # 运行测试
```

## License

MIT
