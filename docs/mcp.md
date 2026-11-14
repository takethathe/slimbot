# MCP 连接器

对应模块：`src/mcp/`（`mod.rs`、`rpc.rs`、`transport.rs`、`service.rs`），消费方在 `src/codemode/agent.rs`、`src/runner/gate.rs`、`src/context.rs`

## 概述

一个 MCP server 就是一个**连接器**（v1 唯一的连接器种类）：owner 在 `mcp.servers[]` 里声明，gateway 启动时拉起连接，codemode 脚本发现并调用它的工具。两条硬决策定义了这块代码的形状：[ADR 0016](adr/0016-mcp-through-codemode-only.md)（连接器工具**绝不进模型 schema**，只在脚本里可见）与 [ADR 0020](adr/0020-hand-rolled-minimal-mcp-client.md)（客户端自研最小实现，只说 `initialize` / `tools/list` / `tools/call`）。

调用链一跳不少地走完：`AgentLoop` 建池并 start → runner 把池挂进 `GateContext` → `codemode` 工具的 `AgentHost` 把每个在线工具装成异步宿主函数 → 脚本 `await ctx.tools.mcp__github__get_pull({...})` → 审批闸定风险级 → `McpService::call()` → transport 出一帧 JSON-RPC。

## 三层

| 层 | 文件 | 职责 | 不知道什么 |
|----|------|------|------------|
| 协议 | `rpc.rs` | JSON-RPC 帧、handshake、`tools/list` 分页（`nextCursor`，最多 32 页）、`tools/call` 结果扁平化 | 连接怎么开、什么时候重试 |
| 传输 | `transport.rs` | stdio 子进程（newline-delimited，ambient reader task + oneshot 关联）与 streamable-HTTP（一请求一 POST，应答是 JSON 或一帧 SSE；server 回的 `mcp-session-id` 与协商版本沿用为后续请求头） | server 生死、工具语义 |
| 服务 | `service.rs` | `McpService`：每 server 一个 supervisor task、工具缓存、退避重连、状态视图、风险映射 | 脚本长什么样 |

两个注入点决定可测性：`McpTransport`（一帧怎么变成答案）与 `Connector`（一条连接怎么开）。单元测试给内存假 peer，集成测试走真进程与真 socket。

## 配置与凭证

字段面在 [config.md](config.md#mcp--mcp-连接器)。两个要点：

- `env` 与 `headers` 的值可以写 `${secret:token}` 这类占位符，缺 `slimbot.` 前缀时按所属 server 展开成 `slimbot.mcp.<server>.<name>`，走 keychain → env 的解析链（`Config::load_resolved`，见 [config.md](config.md#密钥与凭证keychain--env--配置文件)）。键查不到时占位符**原样留下**：握手失败的理由里带着它，比静默拿着空凭证启动更好诊断。
- stdio 子进程**不继承** gateway 的环境：`env_clear()` 后只放回 `PATH`、`HOME`（`npx` 类 launcher 需要）和配置里点名的变量。

## 命名与两个目录面

工具的可调用名是 `mcp__<server>__<tool>`：server 名与 tool 名都过 `js_safe_ident`（非 `[A-Za-z0-9_]` 一律变 `_`，因为它是 JS 属性访问的键），前缀由 `server_prefix` 生成。两个名字只差标点的工具会撞成同一个可调用名，所以按**基名**计数，撞第二次起追加 `_2`、`_3`。

一个 server 名可能正好是另一个的前缀（`github` 与 `github__enterprise`），所以 `McpService::call()` 定位所属 server 时取**最长匹配前缀**，不是配置里第一个匹配。

同一个运行有两个目录面，区别就是 ADR 0016（`src/runner/gate.rs`）：

- `tool_definitions()` — 发给模型的那一份，**只有** `ToolManager` 里注册的工具；
- `callable_definitions()` — 脚本能搜能调的那一份，`ToolManager` + 每个**在线** server 的工具，注册的工具排在前面并按名字优先（连接器工具不可能遮蔽内置工具）。

池自己还提供一个**只读子集**：`readonly_definitions()`（以及逐名的 `is_read_only_claimed()`）只收 server 声称 `readOnlyHint` 且未声称 `destructiveHint` 的工具。它是**能力面**过滤器，只给采集器用——同一个池对 agent / act 入口仍是全量目录，而两个入口里每一次连接器调用照旧按 `ExternalSend` 过审批。缺省方向仍是 fail-closed（`readOnlyHint` 缺省 `false`、`destructiveHint` 缺省 `true`），所以未声明的工具不算只读。这条「按声称筛能力面、不改风险级」的口径是对 spec 字面要求的刻意偏离，理由与残余风险见 [ADR 0021](adr/0021-executor-parasitic-unattended-subagent.md) 与 [codemode.md](codemode.md#采集面的只读性票-17)。

`call()` 定位 server 靠 callable 前缀而不是查缓存：离线的 server 缓存已经空了，而"那个连接器现在不在线"比"没有这个工具"对脚本更有用。

## 服务端推送（notification）

server 可以不请自来地推一帧（无 `id`、有 `method`）。接收点是一个**默认空实现**的 transport 能力：

```rust
pub type NotificationSink = Arc<dyn Fn(&str, Value) + Send + Sync>;

fn set_notification_sink(&self, sink: NotificationSink) {}
```

`NotificationSink` 收的是「帧的 method + 它的 `params`」（server 没带 params 时是 `{}`）。默认实现丢弃，所以没有带外推送通道的 transport（脚本化的假 peer、只回答案的流）不必改写这个方法。`McpService` 在 `connect_and_cache()` 里、握手**之后**把它装到新 transport 上（`initialize` 本身就可能触发 `notifications/tools/list_changed`，装早了才能收得到）；一条连接服务一台 server，所以这个 sink 只带**该 server 自己的**订阅，写的是全池共享的那一个缓冲。

两种 transport 的差别是**能力差别，不是缺口**：

| transport | 推送什么时候能收到 |
|-----------|--------------------|
| stdio | 实时。ambient reader task 一直在读子进程 stdout，任何一帧推送当场交给 sink |
| http | 只在**某次调用等待应答期间**。这里是一请求一 POST，没有常驻推送通道：server 穿插在那次应答 SSE 流里的帧会被交给 sink，两次调用之间推的东西永远不会被读到 |

所以 `type = "notification"` 的虚拟源在 HTTP 连接器上**不会自己更新**：它只在有工具调用正好在等应答时顺带刷新。要让推送真的实时，用 stdio。

缓冲是 last-value，**键为订阅的 `(server, uri)`**：只有 `mcp.servers[].triggers[]` 里 `type = "notification"` 且 `uri` 精确等于该 method 的订阅才会写入；未声明的 method、以及别的 server 推的同名 uri，一律静默丢弃。槽与订阅声明一一对应，所以条目数恒等于声明的订阅数（一条订阅一个槽，后来的覆盖先前的），天然有界；两个 server 各自声明、甚至声明同一个 uri，也各占一个槽，互不覆盖。另一条入口 `ingest_notification(server, method, params)` 走同一段判定，供池外的调用方与测试注入。

读法只有一个：`notification_snapshot() -> BTreeMap<String, (Value, i64)>`（`source_id → (params, received_at_ms)`）。缓冲区按 `(server, uri)` 存，`source_id` 是在读快照时按配置查出来的——它是 `ctx.sources` 的键，也是这份 map 的键。配置里改掉某个订阅的 `source_id` 不会丢掉它已经收到的推送。两个订阅声明同一个 `source_id` 会在这张 map 上撞车，所以那种配置在 `Config::validate_mcp()` 就被拒绝（见 [config.md](config.md#mcp--mcp-连接器)），而不是在这里择一。它是**只读**的——没有任何路径能往槽里写，除了上面那两条受配置约束的入口。消费方是 trigger 层的虚拟源：每次 `freeze()` 把每条订阅变成 `{items: [{params, received_at}], count: 1, observed_at}`，见 [trigger.md](trigger.md#虚拟源mcp-订阅)。它**不返回** `trigger::source::SourceView`，因为 `trigger` 依赖 `mcp` 而不反过来；转换在 trigger 侧做。`received_at` 用本地时钟毫秒（`crate::cron::now_ms()`），与 source 层判断新鲜度用的是同一个钟。池不存在、槽为空或已过源的 TTL 时，读到的就是一条 `observed_at: None` 的空视图——源的**存在**不变，只是未采样。外部内容只作证据（ADR 0009）：它只活在内存快照里，从不落盘。

## 配置声明订阅与虚拟源注入

`mcp.servers[].triggers[]` 是**源声明**，不是工具白名单（工具面仍只由 ADR 0016 决定）。每条订阅在 trigger 层的 source 里对应一个源，键是 `source_id`，条件脚本用 `ctx.sources[source_id]` 读它。形状（字段全部见 [config.md](config.md#mcp--mcp-连接器)）：

```json
{
  "name": "ci",
  "transport": "stdio",
  "command": "npx",
  "args": ["-y", "some-ci-mcp"],
  "triggers": [
    { "type": "notification", "uri": "notifications/builds", "source_id": "ci-builds",
      "description": "Latest build status pushed by the CI server" },
    { "type": "tool", "tool": "list_runs", "args": {"limit": 5}, "source_id": "ci-runs",
      "description": "The five most recent pipeline runs" }
  ]
}
```

| `type` | 必填 | 数据怎么来 | 在 tick 里的位置 |
|--------|------|------------|------------------|
| `notification` | `uri` | server 主动推，池保持 last-value | 不采集：`freeze()` 直接读 `notification_snapshot()` |
| `tool` | `tool`（`mcp__<server>__` 前缀由 host 补） | 到期 tick 里 host 调 `McpService::call()` | 采集段的并发批次，与磁盘采集器混排、共享 `max_concurrent_collect` 与超时 |

注入是**接线层的一次对账**，不是 MCP 池的职责：`trigger::assemble` 把 `&config.mcp.servers` 交给 `SourceLayer::attach_mcp_servers()`，它在 `reload()` 里按 `mcp::server_prefix` / `js_safe_ident` 把 `tool` 解析成可调用名，然后合成 `Origin::Tool` / `Origin::Notification` 的 runtime。所以：

- 改订阅不用重启：下一次 `reload()`（下一 tick）就生效；删掉订阅，虚拟源同一 tick 消失。
- 订阅与磁盘上的 `triggers/sources/<id>/` 撞 id 时订阅赢，并记一行 `warn`。
- 两条订阅声明同一个 `source_id` 不是「谁赢」的问题，而是配置错误：`Config::validate_mcp()` 在加载时拒绝，报错点名 id 与涉及的 server（gateway 不会带着歧义起来）。
- `args` 缺省时按 `null` 下发，池归一成 `{}`；返回值整段成为 `source_id` 下唯一一条 item（与 `ctx.tools.*` 的 `<codemode_output>` 包装无关——虚拟源不经过脚本，也就没有这层信封）。
- 虚拟源**只能读**：`trigger_source update` / `delete` 命中它们时直接拒绝（`updated: false` / `deleted: false`，`error` 指向 `mcp.servers[].triggers`），既不写盘也不改内存。
- `notification` 源的 `observed_at` 是那次推送的 `received_at`；池里没有、或推送已超过源的 TTL，视图就是 `observed_at: None` 的空视图。

## 生命周期

```
start() ──► 每个 enabled server 一个 supervisor task（立即返回，不等任何连接）
              │
              ├─ Connecting{attempt} ──► connect + handshake + tools/list
              │        失败 ↓                    成功 ↓
              │   Offline{reason,next_retry_s}   Online（缓存工具 + 记工具数）
              │        退避 min(2x, backoff_max)     │
              │        后重新 Connecting              ├─ 每 probe_interval 一次 tools/list 当活体探测
              │                                      │     （探测本身带 connect_timeout 预算）
              │                                      │     失败/静默 → Offline，成功 → 继续 Online
              └─ Disabled（配置里关了，永不尝试）     └─ shutdown() → 并发且有预算地 close + abort task
```

四条保证都来自这张图：`start()` 不阻塞，所以一台连不上的 server 不会拖住 gateway 启动；**每一处等待都有预算**（一次 connect、一次探测、一次 close），退避有上限，所以不会 wedge；离线即清空该 server 的工具缓存，所以 `searchTools()` 结构上给不出必然失败的调用；一台 server 的死活只影响它自己的状态行。

## 不可信文本

server 写的每一个字节都按数据对待，绝不按指令对待：

| 入口 | 处理方式 |
|------|----------|
| `tools/list` 的 `description` | 进 `ToolDefinition` 时加前缀 `[connector <server>]`；换行压平；只出现在 `searchTools()` 的返回值里，**不进 system prompt**（`connectors_section` 只报 server 名、状态、工具数） |
| `tools/call` 的结果 | 包进 `<codemode_output server="…" tool="…">`，模型看到的是脚本的输出而不是 server 的原文；非文本块（image/resource 等）只留 `[<type> content: <mime>]` 占位，`structuredContent` 序列化后附在文本之后；wrapper 属性值里的 server/tool 名过 `escape_attr`，正文里的 `</codemode_output` 闭合序列一并中性化为 `&lt;/codemode_output`，所以一个含 `"` 或闭合标签的工具名或正文都跳不出信封 |
| `isError: true` | 保留 wrapper，并在前面加一行 `MCP tool \`server\`/\`tool\` reported an error:`；它不是 JSON-RPC 错误，所以不映射成 `McpError`，脚本自己判断 |
| JSON-RPC 错误帧（`error.message` / `error.data`） | **只发给调用它的脚本**（那是它的工具结果，`not found` 这类理由必须可读）；进状态视图、日志、system prompt 的那一份由 `offline_reason()` 生成，只保留 error code 这个类别，绝不含 server 原文 |
| arguments | 必须是 object；`Null` 变 `{}`，其它包成 `{"value": …}`（裸标量发出去 server 也只会看到 `{}`） |

## 风险映射（fail-closed）

连接器工具走票 03/21 的同一个审批闸，风险级由 `McpService::risk_for` 决定，规则只有一条：**server 的自述不参与定级**。

这条规则在三个入口上一致：agent 脚本、act 脚本、以及被 `subagent` 起的子代理。执行器的 `allow_connectors` 是**能力**上限（哪些 server 的写允许被这个执行器碰），不是定级输入——把 server 加进列表不会把它的写变成 `Allow`，把 callable 设成 `Allow` 也不会让它越过天花板。

| 来源 | 结果 | 默认档 |
|------|------|--------|
| 任何连接器工具（无论 annotation 写什么） | `ExternalSend` | ask |

理由：`readOnlyHint` / `destructiveHint` 是 server 对自己的描述，把它接进风险级就等于让外部服务给自己提权——一个声称只读、实际会写的工具会直接跳过 owner。所以这两条 hint 只作为**声称**出现在可调用名旁边的文本里（`… (server claims read-only)`），供脚本和 owner 阅读。

缺省值仍是有方向的（`readOnlyHint` 缺省 `false`，`destructiveHint` 缺省 **`true`**），因为 `searchTools()` 的文本与将来可能的策略都要用到。真被审批卡住的常用读工具，逃生口是 owner 在 `tools.approval.tools` 里按可调用名逐个放行（ADR 0011），不放宽整档。

## 日志与隐私

状态迁移是仅有的两类 MCP 日志：上线 `info!`（server 名 + 工具数），离线 `warn_log!`（server 名 + `offline_reason` + 下次重试秒数）。四条规则由 transport 与 service 一起守：

- transport reason 只报**端点或进程**，不报凭证：HTTP 侧用 `safe_endpoint()`（scheme + host + port + path，query 剥掉，因为 token 常挂在 query 上；端口留着，否则两台 loopback 连接器的错误无法区分），且永不回显 response body；
- 离线理由经 `offline_reason()` 收敛：JSON-RPC 错误只留 code，整串压成一行并截断到 200 字符 —— 前置代理把 `Authorization` 回显进 `error.data`、或 server 在错误里塞一行 `# Connectors` 伪装成 prompt 段落，都出不去；
- server 的 stderr **只计字节数**，内容不被捕获、不进日志——server 启动时把自己的 token 打出来不是我们该广播的事；
- 凭证只在开连接那一刻物化，不进缓存、不进状态、不进日志；解析链只记「键 + 来源」，值以 `Secret` 包装，`Debug`/`Display` 恒为 `***`。

`tests/mcp_integration.rs` 把这几条变成断言：一条 URL query 里带 token 的 HTTP 链路、一个把 token 打到 stderr 的真实子进程、一个把 Authorization 回显在 body 里的 401，最后要求 token 既不出现在 `status.label()` 里，也不出现在日志文件里。

## 装配位置

池由 `AgentLoop` 拥有并从 `config.mcp` 构造，`mcp.servers` 里没有 enabled 条目时**根本不建池**（`connectors()` 返回 `None`，脚本里 `mcp__*` 命名空间结构性缺失）。所有入口同源：

| 入口 | 有连接器 | 说明 |
|------|----------|------|
| `slimbot`（CLI agent session） | 是 | `AgentLoop::from_config` 一条路径 |
| `slimbot gateway` | 是 | 启动后额外记一行 `[gateway] MCP connectors: …` 作为"这台机器打算去够哪些东西"的开机账 |
| 执行器 act 脚本（`executor run`） | **写半边，按 `allow_connectors`** | 复用同一座 `codemode/connector.rs` 桥；目录是全量工具，调用前按 `server_of()` 判天花板，列表外的写即终止整段脚本。每次调用过同一个审批闸（一律 `ExternalSend`），见 [codemode.md](codemode.md#act-的连接器面allow-only--两闸正交票-15) |
| 触发器 fire 的会话 | 是 | dispatch 投进 inbound，与 CLI 走同一个 `SessionTaskBuilder` |
| `subagent` 工具起的子代理 | 是 | `SubagentRuntime` 把同一个 `Arc<McpService>` 交给它自建的 runner（票 15 打通：执行器子代理的 act 脚本要靠它写外部对象）。没有池时行为与从前一致：`mcp__*` 结构性缺失 |
| 触发器采集器（`collect.js`） | **只读半边** | tick 与 `AgentLoop` 同进程，池经 `SourceLayer::attach_connectors()` 交到采集器；采集面只装 `readonly_definitions()`，见 [codemode.md](codemode.md#采集面的只读性票-17) |

## 测试

`cargo test --lib mcp::`（协议、两种 transport、池与退避、命名与路由、风险映射、离线理由收敛、`server_of()` 取最长前缀、notification 的投递/last-value/未声明丢弃/本地时间戳），`cargo test --lib codemode::act`（act 侧：读写判据、`allow_connectors` 双向、Ask/Deny 中止且不留 Pending、共用调用预算），`cargo test --lib executor::tool`（写成功带 receipt、无人值守拒因、evidence 逐条 4KB），`cargo test --lib agent::runtime`（子代理继承池、拒因回到 origin），`cargo test --test mcp_integration`（GitHub 式读链路端到端 + secret 链 + 无敏感日志 + 拒绝式 server），`cargo test --test gateway_integration`（配置里有/没有 server 时池的有无）。
