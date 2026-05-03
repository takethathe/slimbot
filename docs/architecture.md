# 系统架构概览

本文档描述 SlimBot 的整体架构、数据流和组件交互。

## 架构图

```
用户输入
  │
  ▼
┌─────────────────────────────────────────────┐
│              ChannelManager                  │
│  ┌──────────┐  ┌──────────┐  ┌──────────┐  │
│  │ CliChannel│  │ WebChannel│  │ ...     │  │
│  └──────────┘  └──────────┘  └──────────┘  │
│  每个通道独立 spawn_blocking 读取输入         │
└────────────┬────────────────────────────────┘
             │ inbound_tx (mpsc)
             ▼
┌─────────────────────────────────────────────┐
│              MessageBus                      │
│  纯异步通道，不启动任何后台任务                │
│  inbound_tx / inbound_rx                     │
│  outbound_tx / outbound_rx                   │
└────────┬──────────────────────────┬──────────┘
         │ outbound_rx              │ inbound_rx (AgentLoop 监听)
         ▼                          ▼
┌──────────────────┐    ┌─────────────────────────────────┐
│ ChannelManager   │    │         AgentLoop                │
│ .run().await     │    │  .start_inbound()               │
│ (主线程阻塞)      │    │                                 │
│                  │    │  1. drain inbound_rx             │
│ 路由出站消息到    │◄───┤  2. ensure_session               │
│ 对应 Channel     │    │  3. SessionTaskBuilder → submit  │
│ write_output()   │    │  4. AgentRunner.run()            │
│                  │    │  5. publish outbound_tx           │
└──────────────────┘    └─────────────────────────────────┘
```

## 核心组件

### MessageBus

纯异步 mpsc 通道端点，**不启动任何后台任务**。提供四端点：

- `inbound_tx` — 通道提交用户请求到此
- `inbound_rx` — AgentLoop 监听此端点
- `outbound_tx` — AgentLoop 发布处理结果到此
- `outbound_rx` — ChannelManager 监听此端点

### AgentLoop

顶层编排组件，持有 Provider、ToolManager、SessionManager、MemoryStore、Consolidator 的引用：

1. 从配置加载 Provider、ToolManager、SessionManager、MemoryStore
2. 创建 `Arc<Consolidator>`（基于 context_window_tokens 配置）
3. 调用 `start_inbound()` 启动后台监听任务
4. 该任务通过 `SessionTaskBuilder` 构建任务（附带 Consolidator 的 Arc 引用），提交到 SessionRunner 顺序执行

### AgentRunner

ReAct 循环的核心。每次处理任务时新建一个 Runner 实例：

1. 将用户消息写入 Session
2. 在 `run()` 内部创建 `ContextBuilder`，获取 last_summary
3. 构建上下文（system prompt + 历史消息 + 工具定义 + 会话摘要）
4. 调用 Provider 获取模型响应
5. 如果响应包含 tool_calls，执行工具并追加消息，进入下一轮
6. 如果响应不包含 tool_calls，更新 token ratio、持久化、触发 Consolidation、返回最终文本
7. 循环结束后以追加模式写入 JSONL，并更新 meta 文件

### SessionRunner

每个 session 独立的执行协调器。通过 `AtomicBool` 和独立任务队列保证同一 session 的任务顺序执行，空闲时自动拉取下一个任务。

### Consolidator

Token 预算触发的会话摘要机制。作为 `Arc<Consolidator>` 在 AgentLoop 初始化时创建，传入 AgentRunner。ReAct 循环完成后若 prompt_tokens 超过安全预算，自动选取旧消息进行 LLM 摘要，结果通过 MemoryStore 追加到 `history.jsonl`，并更新会话的 consolidation 游标和 last_summary。

### ChannelManager

管理所有 I/O 通道，路由出站消息：

1. **自动注册**内置工厂（CLI 等），无需在 main.rs 手动注册
2. 从配置读取通道列表，通过工厂模式创建对应通道实例
3. 每个通道调用 `channel.start(inbound_tx)` 启动内部读取循环
4. `run().await` 在主线程上监听 `outbound_rx`，按 `channel_id` 路由到对应通道的 `write_output()`

## 数据流

### Inbound（用户输入）

```
用户输入 → Channel.read_input() (spawn_blocking)
         → inbound_tx.send(BusRequest)
```

### Agent 处理

```
AgentLoop.start_inbound() 后台任务:
  → inbound_rx.recv()
  → ensure_session()
  → SessionTaskBuilder 构建任务（附带 Consolidator）
  → session_manager.submit_task(task)
  → SessionRunner 顺序执行
  → AgentRunner.run()
  → ReAct 循环（多次 LLM 调用 + 工具执行）
  → outbound_tx.send(BusResult)
```

### Outbound（结果路由）

```
ChannelManager.run().await 主线程:
  → outbound_rx.recv()
  → 从 session_id 提取 channel_id
  → channels.get_mut(channel_id).send_output(result)
  → Channel.write_output(result) → 用户看到输出
```

### 任务状态通知

```
AgentRunner 状态变更 → TaskHook.notify_status()
                     → tokio::mpsc channel
                     → Channel 内部状态监听任务
                     → Channel.write_status()
```

## 关键设计决策

| 决策 | 方案 | 原因 |
|------|------|------|
| Session 访问 | `Arc<Mutex<SessionManager>>` 共享 Mutex | 所有模块通过共享引用访问，保证线程安全 |
| 消息持久化 | 追加写入 JSONL + 独立 meta 文件 | 简单可靠，增量写入避免全量覆盖 |
| 通道并发 | 每个通道独立 `spawn_blocking` | 阻塞 I/O 不阻塞 tokio 运行时 |
| Session ID | `{channel_id}:{chat_id}` | 通过通道标识和聊天标识唯一确定会话 |
| 任务通知 | `TaskHook` + `tokio::sync::mpsc` | 异步非阻塞，避免工具执行时阻塞通道 |
| MessageBus | 纯通道，无后台任务 | 职责单一，AgentLoop 自己负责处理逻辑 |
| 工厂注册 | ChannelManager 内部自动注册 | 简化 main.rs，不需要手动注入工厂 |
| Consolidator | 可选注入 `Arc<Consolidator>` | 不传入时跳过摘要，传入时自动触发 |

## 目录结构与模块关系

```
main.rs ── 入口：CLI 解析 → Logger 初始化 → 子命令分发
  │
  ├── setup.rs ── 配置规范化、目录创建、bootstrap 文件
  │     ├── config_scheme.rs ── 默认值、规范化、URL 派生
  │     └── bootstrap.rs + embed.rs ── 嵌入模板文件
  │
  ├── cli.rs ── clap 参数解析、CLI agent session
  │
  ├── agent (子命令) ── CLI agent 会话
  │     ├── path.rs ── 路径管理、默认值、沙箱验证
  │     ├── config.rs ── 配置加载、验证
  │     ├── agent_loop.rs ── 顶层编排 + inbound 监听任务
  │     │     ├── provider/openai.rs ── LLM API 调用
  │     │     ├── tool.rs ── ToolManager + 工具注册
  │     │     ├── session.rs ── Session + SessionManager + SessionRunner
  │     │     ├── consolidate.rs ── Consolidator（token 预算触发摘要）
  │     │     ├── context.rs ── 上下文构建
  │     │     └── runner.rs ── ReAct 循环
  │     │
  │     ├── message_bus.rs ── 纯异步通道端点
  │     ├── memory.rs ── 长期记忆、历史记录
  │     ├── worker.rs ── WorkerPool 异步执行池
  │     └── io_scheduler.rs ── I/O 调度、阻塞读取
  │
  └── log.rs + macros.rs ── 日志系统、级别过滤、彩色输出
```

## main.rs 初始化流程

```rust
// 0. CLI 解析 + 日志初始化
let args = CliArgs::parse();
let log_level = LogLevel::from_u8(args.log).unwrap_or(LogLevel::Info);
crate::log::init(log_level, log_file_path.as_deref())?;

// 1. 创建 MessageBus（纯通道，无后台任务）
let message_bus = Arc::new(MessageBus::new());

// 2. 创建 AgentLoop，启动 inbound 监听
let agent_loop = AgentLoop::from_config(&paths, message_bus.clone(), config.clone()).await?;
agent_loop.start_inbound();

// 3. 启动 CLI agent session
crate::cli::run_agent_session(&agent_loop, session_id).await;
```

SlimBot 使用子命令模式：`setup` 执行初始化，`agent` 启动 CLI 交互会话。
