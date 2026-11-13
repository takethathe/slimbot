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
│  │ WebUI    │  │ Feishu   │  │ ...      │  │
│  └──────────┘  └──────────┘  └──────────┘  │
│  每个通道在自己的任务 / 线程里读取输入          │
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

顶层编排组件，持有 Provider、ToolManager、SessionManager、MemoryService、Consolidator 的引用：

1. 从配置加载 Provider、ToolManager、SessionManager、MemoryService
2. 创建 `Arc<Consolidator>`（窗口锚取自 `providers.<agent.provider>.context_window_tokens`，占比取自 `pressure` 段）
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

Token 预算触发的会话摘要机制。作为 `Arc<Consolidator>` 在 AgentLoop 初始化时创建，传入 AgentRunner。ReAct 循环完成后若 prompt_tokens 超过安全预算，自动选取旧消息进行 LLM 摘要，结果通过 HistoryStore 追加到 `history.jsonl`，并更新会话的 consolidation 游标和 last_summary。

### ChannelManager

管理所有 I/O 通道，路由出站消息：

1. `ChannelManager::new()` 的工厂表是空的：gateway 装配时用 `register_webui_factory(startup)` 加 WebUI、`register_factory("feishu", ..)` 加飞书，各一次（CLI 不是 `Channel` 实现，由 `ShutdownHandle::run_main_thread_loop` 在主线程读 stdin）
2. 从配置的 `channels` 映射读取条目，通过工厂模式创建对应通道实例（`enabled:false` 的跳过）
3. 每个通道用 `start_with_scheduler(&io_scheduler)` 起自己的读取循环（返回 `IoHandle`，关停时一起 join）
4. `run().await` / `run_with_shutdown()` 监听 `outbound_rx` 与 `media_rx`，按 `channel_id` 路由到对应通道的 `write_output()`

### TriggerService + Dispatcher（gateway 常驻）

触发器是 cron 之上的一层，与 cron / heartbeat 并列跑自己的固定 60s tick（详见 [trigger.md](trigger.md)）：

```
每 60s：reload 定义 → freeze 快照 → 采集到期的源 → 判定到期的规则 → 命中入队
队列外：单个消费者按创建顺序执行 action（起子代理 / 注入 notify / 只记历史）
投递：publish_inbound(来源会话) → 主 loop 总结并回复；唯一例外是失败告警走 publish_outbound
```

把行动挪出 tick 是为了让 30s 硬超时不会掩断一次正在跑的子代理；三个并列的 root span（`trigger.tick` / `trigger.fire` / `subagent.run`）靠 id 关联而不设父子。

### SubagentRuntime（与主 loop 共用）

子代理是一份 `agents/<name>.md` 定义的一次性执行（详见 [agent.md](agent.md)）：自己的会话只在内存、按定义从 `ToolCatalog` 装配 `ToolManager`、全局并发闸。主 loop 的 `subagent` 工具与触发器的 `agent` action 共用同一个 runtime 实例，因此共用同一个闸与同一条回会话的路；`trigger` 与 `executor` 两个内置子代理走的都是这条路（执行器没有第二条执行路径，见 [ADR 0021](adr/0021-executor-parasitic-unattended-subagent.md)）。脚本执行（采集 / 判定 / 动作）走 `codemode` 沙箱：rquickjs + 专职线程 + 双层超时 + 宿主函数白名单（详见 [codemode.md](codemode.md)）。

### 单用户信任边界（身份层）

一个 gateway 服务且只服务一个 `owner`（`gateway.owner`，v1 永久单用户，见 ADR 0010）。身份层只做两件事：给记忆/审批/投递一个稳定的「谁的」，以及在入站口判定「谁说的」。

- **归属**：`TaskHook` 携带 `owner_id`；记忆不分 owner 分区，会话仍按 `{channel}:{chat_id}` 独立（「在哪」与「谁的」正交）。
- **作者信任**：入站 `BusRequest.author` 为 `Author::Owner`（本机 CLI / WebUI、owner 本人私聊、由 owner 配置启动的系统注入）或 `Author::Bystander`。`Bystander` 的入站消息在 `AgentLoop` 入站口被**直接拒绝**并回一条固定说明——它不能授权任何动作、不能写记忆；v1 无群聊，因此不需要「不可信但可读」的中间态。
- **WebUI 边界**：只监听 loopback（非 loopback 直接启动失败、无 override），无认证、无 token，本机即 owner；补偿控制是四条浏览器侧中间件（Host 白名单 / 同源 Origin / `application/json` / `Sec-Fetch-Site`）+ 零 CORS 头。本机任意进程仍可全权操作 WebUI，这一点显式接受。

### 出站投递（投递目标解析）

主动消息（cron / heartbeat / trigger / provider alert）都经**同一个解析入口**（`src/delivery.rs::resolve_target`）；调用点只声明来源类别（`DeliverySource`）与自己持有的事实，优先级与兜底资格在 `DeliverySource::candidate()` 一处求值：

```
                            own target   origin   owner primary
  cron (payload channel/to)     yes         —          yes
  heartbeat (override)         yes         —          yes
  trigger notify / alert       yes        yes         never
  provider alert                —          —        only leg
```

- 优先级与兜底资格按来源类别取舍：**cron / heartbeat** 是「显式目标 > Owner primary channel」（cron 的创建会话就是 payload 的 `channel`/`to`，属显式目标，没有独立 origin 腿）；**trigger** 是「显式 deliver-to > Origin session」，**永不到 owner**；**provider alert 只有 owner 一条腿**。来源类别是 seam 的显式输入，调用点不靠传 `None` 表达策略。
- `DeliveryTarget{channel, chat_id}` 两段式；任一为空即「不存在」，不会被就近兜底。
- `Owner primary channel` = owner 首次交互的会话，落在 `{data_dir}/owner-primary.json`（一条目标，不是投递账本），重启后仍可投递；未建立时降级 log-only。
- **trigger 永不兜底到 owner**：它恒有 origin，origin 不可达即 book-only（不误投）。
- **`deliver:false` 是唯一策略性静默**；「目标不存在」是降级兜底，两者在调用点区分。
- 无持久 outbox、无全局投递账本、重启不补投。

### Connector / Executor（外部接口层）

连接器 = codemode 可直接调用的、非基础原语、非内置工具的外部能力封装（同一连接器既读又动）；MCP server 是 v1 唯一实例（见 `GLOSSARY.md` `### Connector`）。它把「agent 对外部系统的能力」抽象成一个可寻址的服务单元，与 Channel（会话/传输面）正交，区别于宿主内置工具（file/shell/message）与 codemode 基础原语（时间、`ctx.http`）。

在此之上是两种 codemode 消费者，互为操作镜像：**采集器**（触发器源 `collect.js`，读侧、周期采集）与**执行器**（`act.js`，写侧、按需动作）。定位见 [ADR 0019](adr/0019-executor-as-codemode-operation-primitive.md)，形状与姿态见 [ADR 0021](adr/0021-executor-parasitic-unattended-subagent.md)：执行器**完全寄生一个 ephemeral 子代理**（`agents/executor.md`），`executor` 工具只注册进 `ToolCatalog`，因此主循环唯一能看到的是 `# Available Agents` 里的一行描述。act 入口与 agent 入口共用同一座连接器桥（`src/codemode/connector.rs`），差别只在传入哪份目录——采集器只见 server 声称只读的连接器工具，agent 见全量，act 见**全部可搜、写按 `allow_connectors` 才可调**（四张过滤表见 [codemode.md](codemode.md#连接器面一座桥四张过滤表)）。两个子代理共享的知识只有一份非-skill 文档 `references/connector.md`（`agents/*.md` 里一行硬指针按需读取，从不进主循环，见 [bootstrap.md](bootstrap.md#reference-文档不进-available-skills)）。

执行器动作带副作用、纳入审批（[ADR 0011](adr/0011-approval-by-turn-end-redemption.md)），并且因为跑在无人值守的临时会话里而**事实上 Allow-only**：命中需确认（`Ask`）的连接器写调用当场按拒绝处理、终止整段脚本、不写 Pending——「回合末兑现」要求「下一轮」，而这个会话跑完即删。需要 owner 交互确认的动作不是执行器场景，走主循环的工具路径（那里有真下一轮）。细节见 [executor.md](executor.md)，决策见 ADR 0021。

连接器的 v1 实例已落地（票 26）：`src/mcp/` 的 `McpService` 在 gateway/CLI 两侧由 `AgentLoop` 建池并启动，每 server 一个 supervisor（连接、工具缓存、退避重连）；它的工具**只存在于 codemode 脚本面**（`searchTools()` / `tools.mcp__<server>__<tool>()`），从不进入模型的 tool schema。细节见 [mcp.md](mcp.md)，决策见 [ADR 0016](adr/0016-mcp-through-codemode-only.md) 与 [ADR 0020](adr/0020-hand-rolled-minimal-mcp-client.md)。

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
| 触发器行动 | 在 tick 之外的单消费者队列执行 | 模型调用不能挤进 tick 的硬超时；队满丢弃而不顺延，迟到十分钟的提醒比不提醒更糟 |
| 子代理会话 | `mark_ephemeral` + 跑完 `drop_session` | 不留 JSONL、不进 consolidator，一次性任务不该污染长期上下文 |
| 外部内容 | 只以证据形式进入上下文，快照不落盘 | 见 [ADR 0009](adr/0009-no-raw-external-content.md) |
| 连接器工具 | 不进模型 tool schema，只在 codemode 脚本里发现与调用 | 一个外部 server 就带来 30+ 工具，会把模型注意力从内置能力上拉走；脚本还能一次编排多调用（ADR 0016） |
| 连接器离线 | 清空该 server 的工具缓存，调用立即回一条干净错误 | 发现面不给必然失败的调用；退避有上限且每 server 独立，一台连不上不会 wedge gateway |
| 执行器形状 | `executor` 工具只注册进 `ToolCatalog`，调用面是一个寄生子代理 | 主循环的 schema 与 prompt cache 不因对外动作而膨胀，执行路径只有一条（ADR 0021） |
| 执行器审批 | 无人值守 ⇒ `Ask` 即 Deny、不写 Pending | 「回合末兑现」要求下一轮，ephemeral 子代理没有下一轮（ADR 0011、0021） |

## 目录结构与模块关系

```
main.rs ── 入口：CLI 解析 → Logger 初始化 → 子命令分发
  │
  ├── setup.rs ── 配置规范化、目录创建、bootstrap 文件
  │     ├── config_defs.rs ── define_config! 字段定义与默认值
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
  │     │     ├── runner.rs ── ReAct 循环
  │     │     ├── mcp/ ── MCP 连接器池（两种 transport、工具缓存、退避重连；只经 codemode 脚本可见）
  │     │     └── executor/ ── 执行器定义 + `executor` 工具（只进 catalog，由执行器子代理装配）
  │     │
  │     ├── message_bus.rs ── 纯异步通道端点
  │     ├── memory/ ── category 化长期记忆、backend、历史记录
  │     ├── worker.rs ── WorkerPool 异步执行池
  │     └── io_scheduler.rs ── I/O 调度、阻塞读取
  │
  ├── gateway.rs ── Gateway 模式：cron / heartbeat / trigger 三个常驻 tick
  │     ├── trigger/ ── 触发器层（source 采集 → subscription 判定 → dispatch 投递）
  │     ├── agent/ ── 子代理定义 + ToolCatalog + SubagentRuntime
  │     ├── codemode/ ── 脚本沙箱（rquickjs + 专职线程 + 双层超时 + 宿主白名单）
  │     ├── cron/ ── 定时任务
  │     └── heartbeat/ ── 心跳服务
  │
  └── log.rs + macros.rs ── 日志系统、级别过滤、彩色输出
```

## Gateway 启动的两步：装配与运行

`run_gateway` 只有两件事：等 `ctrl_c`、按顺序关停。真正的接线在 `assemble_gateway`，它返回一个 `Gateway`——组件已建好并起跑，还没等信号：

```rust
let gateway = assemble_gateway(&paths).await?;   // 可构造、可断言
// ... 等待 ctrl_c ...
gateway.shutdown().await;                       // 关停：channel 广播 → card 取消 → heartbeat/cron → 10s 超时
```

- **装配**里是所有「谁拿到谁」的决定：Config 快照、MessageBus 的三个 receiver（inbound / outbound / media）、工具与 catalog、`AgentLoop`（含 tracer 与 connector 池）、Cron/Heartbeat/Trigger、ChannelManager 的工厂注册（Card 与 Feishu 各一次）、WebUI 启动快照。
- **WebUI 启动快照**（`WebuiStartup`）是一份纯数据：session manager、关停广播、AgentEvent 广播、trace 路径与启动时刻、config 路径、card service。它只经 `register_webui_factory` → `WebuiChannelFactory::create` 一条路径消费，通道与 `AppState` 都持有这份快照，所以新增一个 channel-side 参数只改它。
- **trace 同源**：`AgentLoop` 构造时解析一次 trace 文件路径，写侧（`FileBackend`）与读侧（viewer）共用；网关启动的截断只发生一次，CLI 从不截断（ADR 0022）。
- **config 路径不是配置值**：快照只带路径，Config editor 的 handler 每次自己读盘，所以「保存即写盘、重启才生效」在代码里是看得见的（ADR 0002）。

## main.rs 初始化流程

```rust
// 0. CLI 解析 + 日志初始化
let args = CliArgs::parse();
let log_level = LogLevel::from_u8(args.log).unwrap_or(LogLevel::Info);
crate::log::init(log_level, log_file_path.as_deref())?;

// 1. 配置快照 + MessageBus（纯通道，无后台任务）
let config = Arc::new(Config::load_resolved(..)?);
Config::install(config.as_ref())?;
let (message_bus, receivers) = MessageBus::new();

// 2. 创建 AgentLoop：inbound receiver 以值交进去
//    （CLI 不走 start_inbound，任务由 run_task 直接驱动；gateway 才跑 inbound 监听）
let agent_loop = AgentLoop::from_config(paths, message_bus, receivers.inbound, config).await?;

// 3. 启动 CLI agent session
crate::cli::run_agent_session(&agent_loop, session_id, query).await?;
```

SlimBot 使用子命令模式：`setup` 执行初始化，`agent` 启动 CLI 交互会话（上面的流程），`gateway` 走 `assemble_gateway` + `run_gateway` 两步常驻。
