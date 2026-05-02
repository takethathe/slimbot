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
│ 对应 Channel     │    │  3. enqueue → dequeue            │
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

顶层编排组件，持有 Provider、ToolManager、SessionManager 的引用：

1. 从配置加载 Provider、ToolManager、SessionManager
2. 调用 `start_inbound()` 启动后台监听任务
3. 该任务监听 `inbound_rx`，对每条请求执行 ReAct 循环，发布结果到 `outbound_tx`

### AgentRunner

ReAct 循环的核心。每次处理任务时新建一个 Runner 实例：

1. 将用户消息写入 Session
2. 构建上下文（system prompt + 历史消息 + 工具定义）
3. 调用 Provider 获取模型响应
4. 如果响应包含 tool_calls，执行工具并追加消息，进入下一轮
5. 如果响应不包含 tool_calls，返回最终文本
6. 循环结束后将 Session 全量持久化为 JSONL

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
  → enqueue → dequeue → AgentRunner.run()
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
| 消息持久化 | 全量写入 JSONL（非追加） | 简单可靠，避免部分写入导致的数据损坏 |
| 通道并发 | 每个通道独立 `spawn_blocking` | 阻塞 I/O 不阻塞 tokio 运行时 |
| Session ID | `{channel_id}:{chat_id}` | 通过通道标识和聊天标识唯一确定会话 |
| 任务通知 | `TaskHook` + `tokio::sync::mpsc` | 异步非阻塞，避免工具执行时阻塞通道 |
| MessageBus | 纯通道，无后台任务 | 职责单一，AgentLoop 自己负责处理逻辑 |
| 工厂注册 | ChannelManager 内部自动注册 | 简化 main.rs，不需要手动注入工厂 |

## 目录结构与模块关系

```
main.rs ── 入口，初始化 AgentLoop + MessageBus + ChannelManager
  │
  ├── agent_loop.rs ── 顶层编排 + inbound 监听任务
  │     ├── config.rs + config_scheme.rs ── 配置加载、验证、规范化
  │     ├── provider/openai.rs ── LLM API 调用
  │     ├── tool.rs ── ToolManager + 工具注册
  │     ├── session.rs ── Session + SessionManager
  │     └── runner.rs ── ReAct 循环
  │           └── context.rs ── 上下文构建
  │
  ├── message_bus.rs ── 纯异步通道端点
  │
  └── channel/mod.rs + cli.rs ── 通道管理 + CLI 实现
```

## main.rs 初始化流程

```rust
// 1. 创建 MessageBus（纯通道，无后台任务）
let message_bus = Arc::new(MessageBus::new());

// 2. 创建 AgentLoop，启动 inbound 监听
let agent_loop = AgentLoop::from_config(&paths, message_bus.clone()).await?;
agent_loop.start_inbound();

// 3. 创建 ChannelManager（自动注册内置工厂）
let mut channel_manager = ChannelManager::new(message_bus);
channel_manager.init_from_config(&config.channels).await?;

// 4. 主线程阻塞，等待出站消息（通道全部关闭后退出）
channel_manager.run().await;
```
