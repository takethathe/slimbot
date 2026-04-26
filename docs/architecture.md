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
│    并发 I/O 循环，每个通道独立运行              │
└────────────┬────────────────────────────────┘
             │ BusRequest
             ▼
┌─────────────────────────────────────────────┐
│              MessageBus                      │
│  1. 确保 Session 存在                          │
│  2. 封装为 SessionTask + TaskHook             │
│  3. 入队 → 出队 → 调用 AgentRunner             │
│  4. 返回 BusResult                             │
└────────────┬────────────────────────────────┘
             │
             ▼
┌─────────────────────────────────────────────┐
│              AgentLoop                       │
│  初始化并持有：Provider、ToolManager、          │
│  SessionManager 的引用                        │
└────────────┬────────────────────────────────┘
             │ 每次 run_task 创建
             ▼
┌─────────────────────────────────────────────┐
│              AgentRunner                     │
│  ┌──────────────────────────────────────┐   │
│  │            ReAct 循环                 │   │
│  │                                      │   │
│  │  ContextBuilder.build()              │   │
│  │    → 构建 system prompt + 历史消息     │   │
│  │         + 工具定义                     │   │
│  │                                      │   │
│  │  Provider.chat()                     │   │
│  │    → 调用 LLM API                     │   │
│  │                                      │   │
│  │  有 tool_calls?                       │   │
│  │    ├── 否 → 返回文本，循环结束           │   │
│  │    └── 是 → ToolManager.execute()     │   │
│  │           → 工具执行 → 继续下一轮        │   │
│  │                                      │   │
│  │  结束条件：超出 max_iterations          │   │
│  │            或模型返回无 tool_calls      │   │
│  └──────────────────────────────────────┘   │
│                                              │
│  SessionManager.persist() → JSONL 持久化     │
└──────────────────────────────────────────────┘
```

## 核心组件

### AgentLoop

顶层编排组件，负责初始化所有子系统：

1. 加载配置文件，获取 Provider 配置
2. 创建 `OpenAIProvider` 实例
3. 创建 `ToolManager`，从配置注册内置工具
4. 创建 `SessionManager`，指向 session 目录

### AgentRunner

ReAct 循环的核心。每次 `run_task` 时新建一个 Runner 实例：

1. 将用户消息写入 Session
2. 构建上下文（system prompt + 历史消息 + 工具定义）
3. 调用 Provider 获取模型响应
4. 如果响应包含 tool_calls，执行工具并追加消息，进入下一轮
5. 如果响应不包含 tool_calls，返回最终文本
6. 循环结束后将 Session 全量持久化为 JSONL

### MessageBus

消息总线，将通道的输入转化为 Agent 任务：

1. 接收 `BusRequest`（包含 session_id、内容、通道注入信息、TaskHook）
2. 确保对应 Session 存在
3. 生成 task_id，封装为 `SessionTask`
4. 入队 → 立即出队 → 调用 `AgentRunner.run()`
5. 返回 `BusResult`

### ChannelManager

管理所有 I/O 通道，驱动并发轮询：

1. 从配置读取通道列表，通过工厂模式创建对应通道实例
2. 每个通道在独立的 `tokio::spawn` 中运行 I/O 循环
3. 读取用户输入 → 通过 MessageBus 发送 → 输出结果
4. 通过 `TaskHook` 监听任务状态变更，输出中间状态

## 数据流

### 用户请求流程

```
用户输入 → Channel.read_input()
         → Channel.prepare_inject()
         → BusRequest → MessageBus.send()
         → SessionTask 入队/出队
         → AgentRunner.run()
         → ReAct 循环（多次 LLM 调用 + 工具执行）
         → BusResult → Channel.write_output()
         → 用户看到输出
```

### 任务状态通知

```
AgentRunner 状态变更 → TaskHook.notify_status()
                     → tokio::mpsc channel
                     → ChannelManager 后台任务
                     → Channel.write_status()
```

## 关键设计决策

| 决策 | 方案 | 原因 |
|------|------|------|
| Session 访问 | `Arc<Mutex<SessionManager>>` 共享 Mutex | 所有模块通过共享引用访问，保证线程安全 |
| 消息持久化 | 全量写入 JSONL（非追加） | 简单可靠，避免部分写入导致的数据损坏 |
| 通道并发 | 每个通道独立 `tokio::spawn` | 通道之间互不阻塞，支持多用户并发 |
| Session ID | `{channel_id}:{chat_id}` | 通过通道标识和聊天标识唯一确定会话 |
| 任务通知 | `TaskHook` + `tokio::sync::mpsc` | 异步非阻塞，避免工具执行时阻塞通道 |

## 目录结构与模块关系

```
main.rs ── 入口，初始化 AgentLoop → MessageBus → ChannelManager
  │
  ├── agent_loop.rs ── 顶层编排
  │     ├── config.rs + config_scheme.rs ── 配置加载、验证、规范化
  │     ├── provider/openai.rs ── LLM API 调用
  │     ├── tool.rs ── ToolManager + 工具注册
  │     ├── session.rs ── Session + SessionManager
  │     └── runner.rs ── ReAct 循环
  │           └── context.rs ── 上下文构建
  │
  ├── message_bus.rs ── 消息总线
  │
  └── channel/mod.rs + cli.rs ── 通道管理 + CLI 实现
```
