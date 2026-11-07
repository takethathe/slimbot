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

`call()` 定位 server 靠 callable 前缀而不是查缓存：离线的 server 缓存已经空了，而"那个连接器现在不在线"比"没有这个工具"对脚本更有用。

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

池由 `AgentLoop` 拥有并从 `config.mcp` 构造，`mcp.servers` 里没有 enabled 条目时**根本不建池**（`connectors()` 返回 `None`，脚本里 `mcp__*` 命名空间结构性缺失）。三条入口因此同源：

| 入口 | 有连接器 | 说明 |
|------|----------|------|
| `slimbot`（CLI agent session） | 是 | `AgentLoop::from_config` 一条路径 |
| `slimbot gateway` | 是 | 启动后额外记一行 `[gateway] MCP connectors: …` 作为"这台机器打算去够哪些东西"的开机账 |
| 触发器 fire 的会话 | 是 | dispatch 投进 inbound，与 CLI 走同一个 `SessionTaskBuilder` |
| `subagent` 工具起的子代理 | **否** | `SubagentRuntime` 自建 runner，未挂 `connectors`（v1 有意不做，见 `docs/TODO.md`） |

## 测试

`cargo test --lib mcp::`（协议、两种 transport、池与退避、命名与路由、风险映射、离线理由收敛），`cargo test --test mcp_integration`（GitHub 式读链路端到端 + secret 链 + 无敏感日志 + 拒绝式 server），`cargo test --test gateway_integration`（配置里有/没有 server 时池的有无）。
