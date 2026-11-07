# Codemode 脚本沙箱

对应模块：`src/codemode/`（`sandbox.rs`、`host.rs`、`http.rs`、`agent.rs`、`act/`（`mod.rs` 原语与 HTTP hooks、`connectors.rs` 连接器写侧、`budget.rs` 对外调用预算）、`connector.rs`、`redact.rs`、`error.rs`）+ 模型入口工具 `src/tools/codemode.rs`

## 概述

模型写的代码要能无人值守地跑，而且不能给这台机器一个 shell。四种脚本共用一个求值器：数据源的 `collect(ctx)`、触发器条件的 `judge(ctx)`、模型/子代理提交的 agent 程序 `main(ctx)`（票 25），与执行器的动作脚本 `act(ctx)`（票 12）。选型与实测数据见 [ADR 0008](adr/0008-codemode-runtime-choice.md)（引擎 = `rquickjs`，中断 203ms 生效，8MB 上限 2ms 报错，二进制净增 772KB）。

每次执行得到：

- **一个专职 OS 线程**，随脚本一起结束（结果通过 channel 回传，没有线程会活过它的那次运行）；线程内自带 current-thread tokio runtime + `LocalSet` + 全新 QuickJS VM（建一个约 0.1ms，因此没有池化的必要）；
- **一个裸的全局作用域**：没有模块、没有定时器、没有 `process`、没有 `console`，`queueMicrotask` 被显式删掉。`eval` 与 `Function` 故意保留——VM 内的动态代码不是逃逸，静态扫描脚本永远不能当安全边界；
- **两个独立 deadline**：watchdog 线程翻下 VM 的 interrupt flag，让 `while (true) {}` 在指令中间被掐断（runtime 自己的定时器在这种脚本里根本不会触发，所以 watchdog 必须是普通线程）；外层 `tokio::time::timeout` 兜住永不 settle 的宿主调用；
- **硬性内存上限与输出上限**，以及一条秘密抹除规则（见下）。

## 入口与预算

| 入口 | `Limits` | 超时 | 内存 | 输出 |
|------|----------|------|------|------|
| `collect` | `Limits::collect()` | 10s（源 `timeout_s` 可覆盖） | 64MB | 64KB |
| `judge` | `Limits::judge()` | 500ms | 16MB | 8KB（触发层再收 4KB 证据预算） |
| `main`（agent） | `Limits::agent()` | 30s | 64MB | 64KB |
| `act`（执行器） | `Limits::act()` | 30s（执行器 meta `timeout_s` 可覆盖） | 64MB | 16KB（固定预算，见下） |

调用方式：`execute_async(request, host, env)`（生产路径，异步）与 `execute(...)`（同步，目前只有测试与「无 runtime 可交」的调用者用）。脚本必须定义入口函数，否则报 `missing_entry`。

## ctx：数据 + 白名单函数

`RunRequest::with_ctx(value)` 传纯数据（快照视图、上次的 store 值…）。宿主函数装在 `__host` 上，包装层做一次 `Object.assign({}, JSON.parse(__ctx_json), __host)`，所以脚本里数据和函数长在同一层：`ctx.sources` 与 `ctx.log(...)` 并列。

`HostBindings::install(ctx, env)` 是白名单的唯一入口——每个执行装一次，底层装的是 `__host.fs_stat` 这类 JSON 原语，再由一段 JS shim 包装成脚本看到的形状（`__host.fs = { stat, read, list }`、`__host.http = { get, post }`、`__host.store = { get, set, keys }`）。实现在 `src/trigger/source/host.rs`、`src/trigger/subscription/host.rs`、`src/codemode/agent.rs`、`src/codemode/act/mod.rs`，共享件（时间、`log`、`secret`、只读 http、`tools` / `searchTools` 桥）在 `src/codemode/host.rs`、`src/codemode/http.rs` 与 `src/codemode/connector.rs`：

| 装给谁 | 脚本可见的函数 |
|--------|----------------|
| collect | `ctx.http.get/post(url, opts?)`、`ctx.fs.stat/read/list(path)`、`ctx.secret(name)`、`ctx.log(msg)`、`ctx.now()`、`ctx.boot_at()`、`ctx.uptime_s()`；**进程有连接器池时**再加只读连接器面 `ctx.tools.mcp__*(args?)` 与 `ctx.searchTools(query, limit?)`（见[连接器面](#连接器面一座桥四张过滤表)） |
| judge | `ctx.store.get/set/keys`、`ctx.log(msg)`、`ctx.now()`、`ctx.boot_at()`、`ctx.uptime_s()` |
| agent（`main`） | `ctx.tools.<name>(args?)`、`ctx.searchTools(query, limit?)`、`ctx.log(msg)`、`ctx.now()`、`ctx.boot_at()`、`ctx.uptime_s()` |
| act（执行器） | `ctx.http.get/post(url, opts?)`、`ctx.secret(name)`、`ctx.log(msg)`、`ctx.now()`、`ctx.boot_at()`、`ctx.uptime_s()`；**池存在时**再加连接器面 `ctx.tools.mcp__*(args?)` 与 `ctx.searchTools(query, limit?)`（读全开、写按 `allow_connectors`）—— **没有** fs / shell / store / 任何内置工具名 |
| 测试基线 | `NoHost`（一个函数都不装） |

判定侧**没有** http / fs / secret / subagent：条件只读已经采好的世界，永远不派活。时间三件是共享 helper `install_time`：`now()` 返回 `{epoch_ms, iso, hour, minute, weekday, date}`，`boot_at()` 是进程首次被问到时的 epoch 毫秒，`uptime_s()` 是**本次执行**开始以来的秒数；全部本地时间（`chrono::Local`），不提供时区配置。`install_log` 把 `log(msg)` 装成先脱敏再入 sink 的形式。

## agent 入口（票 25）

实现在 `src/codemode/agent.rs`（`AgentHost`）+ `src/tools/codemode.rs`（模型可见的 `codemode` 工具，默认工具集的第 10 个）。模型/子代理提交一段 `async function main(ctx)`，脚本的**唯一**额外能力是运行装配的工具面：

- `await ctx.tools.<name>(args)`：调本次运行可调用目录里的工具；目录里没有的名字在命名空间里**结构性缺失**（`typeof ctx.tools.nope === "undefined"`），能力清单仍是唯一安全闸；
- `await ctx.searchTools(query, limit?)`：对同一份可调用目录（name+description）做 BM25-lite 排序检索（复用票 15 `KeywordIndex` 的分词与打分核心：拉丁词 + CJK bigram、名字命中 ×2 权重），返回 JSON 字符串 `[{name, description}]`；未命中的词不产生结果，空查询无结果；
- 两份目录不同（票 26）：发给模型的只有 `ToolManager`；脚本可搜可调的是 `ToolManager` + **在线** MCP 连接器的工具（`RunToolExec::callable_definitions()`）。连接器工具因此只能被脚本发现与调用，永远不出现在模型的 `tools` 里（[ADR 0016](adr/0016-mcp-through-codemode-only.md)）；server 掉线时它的工具从两份面上同时消失，回来时又出现——同一个 `ctx` 在两次执行里可能长得不一样。连接器工具的 description 是 server 自述的不可信文本，进入时带 `[connector <server>]` 前缀；它们的输出统一包在 `<codemode_output>` 里。详见 [mcp.md](mcp.md)。
- 其余只有时间三件与 `log`。**没有** fs / http / secret / store：要碰外部世界只能走 `tools.*`，由各工具自己的风险等级把关。

每次 `tools.*` 调用走与顶层 tool_call 相同的审批闸（ADR 0011）：决策核心从 `ApprovalHook` 抽出到 `src/runner/gate.rs`（`GateContext` + `decide()`），钩子与嵌套调用共用同一 `decide`，`Allow/Deny/Ask`、redemption、veto、审计 span 逐字一致。运行期通路：runner 把 `RunToolExec`（内含 `GateContext`）挂进 task-local 的 `TurnContext`，`codemode` 工具在 `execute()` 里取到它、`AgentHost` 把每个工具装成异步宿主函数。唯一的形态差别：脚本里**没有中途弹窗**——`Deny` 的拒绝文本、`Ask` 的 NOT EXECUTED 指引文本作为调用的** resolved 值**返回给脚本（顶层对应的是把 tool 结果写成同样的文本再结束轮次）；attended 轮里嵌套 `Ask` 照常写入 pending 记录，脚本跑完后 `codemode` 工具把 owner 可见的 pending 清单附在工具结果尾部，下一轮模型重放同一脚本、同一 args 哈希按 redemption 放行。无人值守轮（subagent/cron/trigger）里嵌套 `Ask` 与顶层一样 fail closed 直接拒绝，不写 pending。

三条相关约束：

- **本轮提出的申请不能被本轮重放兑现**。`ApprovalStore::matching` 只认 carried（本轮开始时遗留）的记录，和 veto 判定 `mismatching` 同一个口径；否则脚本里连续两次 `tools.send(同 args)`（或顶层一批两个相同 tool_call）就等于自己批准自己。票 25 之前这条只在顶层理论可达（Ask 立刻结束轮次），有了嵌套调用面就变成真实绕过路径，因此一并收紧。
- **嵌套调用会重建轮次作用域**。宿主函数跑在沙箱专属线程上，runner 的 task-local 到不了；`AgentHost` 把 `TurnContext` 带进去，每次调用用 `with_turn_context` 包住执行，所以 `message` / `cron` / `trigger` 这类按当前轮 origin 解析投递目标的工具，在脚本里看到的 channel/chat_id 与顶层一致（`sent_in_turn` 也照样标记在同一个 Arc 上）。
- 嵌套调用没有自己的 `execute_tool` span，只有外层 `codemode` 那一个；脚本内部的调用序列靠 `ctx.log` 与返回值自述。

## 连接器面：一座桥，四张过滤表

本节讲宿主侧的实现与取舍。两个子代理读的同一份契约是 `references/connector.md`（bootstrap 到 workspace，指针在 `agents/trigger.md` 与 `agents/executor.md`；非 skill，见 [bootstrap.md](bootstrap.md#reference-文档不进-available-skills)）——那份讲脚本作者看到的规则，本节讲规则在哪儿实现。

`ctx.tools.<name>(args?)` 与 `ctx.searchTools(query, limit?)` 只有**一份实现**（`src/codemode/connector.rs`），由每个入口传入自己的 catalog 子集与调用路径。复制出第二份 install 就等于造第二个安全边界，因此入口之间的差别只能是「传入哪一份目录」：

| 入口 | `ctx.tools.*` / `searchTools()` 的目录 | 每次调用的去处 |
|------|------------------------------------------|----------------|
| collect（采集器） | **只有** server 声称 `readOnlyHint` 且未声称 `destructiveHint` 的连接器工具（`McpService::readonly_definitions()`）；没有池就连这一面都不装 | `McpService::call()`，**不过审批闸**（理由见下） |
| agent（`main`） | 本次运行的工具 + 全部在线连接器的工具（`RunToolExec::callable_definitions()`） | 审批闸 → 工具 / `McpService::call()` |
| act（执行器） | 池里**全部**工具（读与写都可搜）；调用时按 `allow_connectors` 判天花板，列表外的**写**装配成 tripwire（票 15） | 审批闸（票 15） |
| judge | 不装 | — |

`tools` 在 install 时按目录快照装配，因此目录里没有的名字是**结构性缺失**（`typeof ctx.tools.nope === "undefined"`，调用即 `TypeError`）；`searchTools` 每次查询重读目录，所以连接器掉线后立刻搜不到（ADR 0016）。采集器侧还多一道**调用时**复核：装好的函数在真正发出前再问一次 `is_read_only_claimed()`，中途改了声称或掉线的工具调用不了。

### 采集面的只读性（票 17）

采集入口是**加法**：`ctx.http` / `ctx.fs` / `ctx.secret` / `ctx.log` 一个不少，RSS、明文端点、本地文件继续走基础原语；某个外部系统有 owner 配好的 MCP server 时，才多出一条连接器读。写半对采集器**不可见**：同一 server 的写工具既不装进 `ctx.tools`，也不出现在 `searchTools()` 的结果里。

判定口径是**能力面**，不是策略面：过滤器用 `readOnlyHint && !destructiveHint`，两个 hint 缺省时是 `false` / `true`（fail-closed），所以未声明只读的工具一律不算只读。`McpService::risk_for()` 一个字没改，连接器调用仍是 `ExternalSend`——声称只决定「这个入口能不能看见它」，永不改变审批档位（[mcp.md](mcp.md#风险映射fail-closed)；这条偏离与它的残余风险见 [ADR 0021](adr/0021-executor-parasitic-unattended-subagent.md)）。

**采集器不接审批闸**，三条理由：

1. 接上也不会更安全，只会更没用：默认配置下 `ExternalSend` 在无人值守路径一律 Deny，采集器就一个连接器读都拿不到，票 17 等于没做；
2. 不扩权：采集器今天已经有无人值守、无审批的任意 `ctx.http.get/post`（`SourceLayer` 连 `ApprovalStore` 都不持有），而连接器读被限制在 owner 亲自在 `mcp.servers[]` 里配过的 server 上，严格窄于现有权限；
3. 信任已经发生过：把 server 写进 `mcp.servers[]` 就是一次显式信任决定；claim 只决定该 server 的哪些工具对采集器可见。

**残余风险（如实记下）**：一个**撒谎的 server**（把写工具标成 `readOnlyHint`）能让它的写工具进入采集面的只读视图，被无人值守地调用；同一个谎言在 act 侧还能绕过 `allow_connectors` 天花板。**绕不过的是审批闸——那条调用在 agent / act 入口仍是 `ExternalSend`**，无人值守即 Deny/中止，除非 owner 已在 `tools.approval.tools` 里逐个放行该 callable。**采集侧没有审批闸**（`SourceLayer` 不持有 `ApprovalStore`），采集面能兜的只有「无 fs 写、无 shell、动域只在 owner 配过的 server 上」——撒谎的 server 在采集器里确实是无人值守、无审批、无人拦。裁定依据见 `.scratch/executor-connector/notes/decision-connector-readonly-signal.md`。

## act 入口

实现在 `src/codemode/act/mod.rs`（`ActHost` 的原语部分；连接器写侧在 `act/connectors.rs`、对外调用预算在 `act/budget.rs`），给执行器的动作脚本 `async function act(ctx)`（`executor run` 调用，入参挂 `ctx.args`）。这是**动域**：它替主人去外部系统做事，所以能力面比 agent 入口更窄，只装四类基础原语——时间三件、`log`、`secret`、`ctx.http.get/post`——加上连接器面（`ctx.tools` / `ctx.searchTools`，票 15）。前几类是共享 helper（`install_time` / `install_log` / `install_secret`），`ctx.http` 是 **collect 入口同一份** `src/codemode/http.rs`（只 `http`/`https`、私网目标在连接前就拒、解析后钉住 IP、重定向最多 5 跳、单次 body 收 2MB），连接器面是 **collect / agent 同一份** `src/codemode/connector.rs` 桥——入口之间只差「传哪份目录、调用去哪」，没有第二套实现。

**明确不装**的东西与理由：

| 不装 | 理由 |
|------|------|
| shell | 动域只对外；本地命令不是执行器要的东西 |
| `ctx.fs` 读 / 写 | 执行器 meta 没有路径声明字段；本地数据由调用它的 subagent 用自己的 file 工具读好后经 `ctx.args` 传入 |
| `ctx.http.put/delete/patch` | raw http 写没有固有风险级、审批闸闸不住，是绕审批的后门。所有对外变更只能是已过 ApprovalHook 的连接器工具调用（ADR 0011、0019） |
| 任何内置工具名（如 `file_writer`、`shell`、`message`、`memory_search`） | act 的 `ctx.tools` **只服务连接器**：无论本次运行的 `ToolManager` 里有什么，`ctx.tools` 的键只有连接器名。执行器要读本地文件，就由调用它的 subagent 用自己的 file 工具读好再经 `ctx.args` 传入 |

### act 的连接器面：allow-only + 两闸正交（票 15）

act 的 `ctx.tools` 目录是池里的**全部**工具（`McpService::definitions()`，读与写都进 `searchTools()`），装卸在 `ActHost::with_connectors`。调用路径与 agent 入口完全一致——`RunToolExec::call_gated`，判词是 `Ran` / `Refused` / `Pending` / `Cancelled` 枚举（不是文本前缀）；连接器一律 `ExternalSend`，所以每一次写都真的走到 ApprovalHook。

**天花板只砍写，且按 server 名**：`allow_connectors` 之外的工具在目录里仍然可搜（脚本得知道它存在），但 invoke 先算 `server_of(callable)`——不在列表就置 `AbortHandle::stop()` 并让沙箱的 interrupt 终止整段脚本。`try/catch` 接不住，因为抛的是 uncatchable error。

**「不装进 callable」的落地形态是 tripwire**：名字在 `ctx.tools` 上存在，但指向一个只做「停下」的函数——它不派发、不计数成功、不产生副作用。之所以不干脆让名字缺失：缺名调用抛的是普通 `TypeError`，脚本一个 `try/catch` 就能把它当瞬时故障吞掉继续跑；spec C 要求的是「调用即宿主错误 → 终止整脚本」，只有 tripwire + uncatchable abort 同时满足「不可调用」和「不可续行」。`allow_connectors` 是**能力**（这个执行器被创建出来碰哪些 server），审批是**策略**（这一次调用运不运行）：

- 改 `allow_connectors` **不改变**任何工具的风险级或审批档位：把 server 加进列表不会让它的写免审批，逐工具 `Allow` 也抬不开天花板（测试双向钉死）。
- **读不看列表，但仍过审批闸**：`is_read_only_claimed()` 为真的工具不受 `allow_connectors` 约束（空列表下也不会被天花板打断），与 collect 入口同一判定。但「不受天花板限」只说的是**能力面**：读也是连接器调用、一律 `ExternalSend`，每一次都要过 `call_gated`。而 `LevelControls::default().external_send = ask`（`src/approval/mod.rs`），无人值守的 act 里 `Ask` 即 Deny 并**中止整段脚本**——**默认配置下连读都跑不通**，要真读得由 owner 把 `external_send` 放到 `allow`（或逐工具 override）；`test_act_entry_reads_are_callable_without_allow_connectors` 自己也是先给那个工具 `Allow` 才跑通的。
- **天花板在 install 时一次算定，不在调用时复核**：`blocked_writes()` 在装配 `ctx.tools` 那一刻按目录快照算出「哪些 callable 是天花板外的写」，之后 server 重列目录或改 `readOnlyHint` 都不回溯——理由与 `install_tool_bridge` 按快照装函数相同：脚本已经握住的函数名不该中途被重分类。collect 侧恰好相反，`call_read_only()` **每次调用**复核 `is_read_only_claimed()`（见 [采集面的只读性](#采集面的只读性票-17)）。两个入口对同一问题给了两个答案，这是**有意的**：collect 不接审批闸，只能靠调用时复核兜住谎报/掉线；act 的最后一道是审批闸，不改档案就抬不开它。

**Allow-only**：执行器跑在无人值守的临时子代理里（`TurnKind::Subagent`，跑完即删），没有下一轮兑现审批。于是 `Ask` 与 `Deny` 都是「拒绝 + 终止整脚本」，且**不写 Pending**——写了没有下一轮来兑现，只会留下永久死记录。`run` 报 in-band `{ok:false, failure:"approval_refused"}`，`summary` 写明原因；agent 入口「文本拒绝、脚本继续」的行为刻意保留（它**有**下一轮），分裂只发生在 act 侧，共享的 `decide()` 未改（ADR 0011）。

能力表是唯一安全闸，本模块**不做**任何脚本文本扫描。形状由探针测试正反双向钉住（`src/codemode/act/mod.rs` 的 tests；连接器面在 `act/connectors.rs`）：正例断言 `Object.keys(ctx)` 恰好等于允许集、且集合里每个名字都被真正调用过（时间有值、`secret` 拿到真值且回显被脱敏、`log` 真的到 sink、`http.get/post` 真的走到共享 SSRF 守卫）；反例逐个尝试 shell / fs 读 / fs 写 / `ctx.http` 写动词 / 代表性内置工具名，全部必须抛错，并复核 `eval` 也拿不到 `require` / `process` / `fetch`。连接器面单独钉：`Object.keys(ctx.tools)` 恰是池里的连接器名（同时塞进 `ToolManager` 的 `file_reader` / `file_writer` / `shell` / `message` / `memory_search` 一个都不出现，也搜不到）。**池为空时两个入口的安装语义相反，这是有意的**：act 仍装上 `ctx.tools` 与 `ctx.searchTools`，只是目录为空（`act/connectors.rs` 的 `install` 注释自陈「an empty namespace, not a missing one」），能力面的形状不随进程配置漂移——`test_act_entry_capability_surface_is_exact` 断言的就是这一点（无池时`ctx.tools` 是个空对象、`ctx.searchTools()` 回 `[]`，两个名字都在）；具体的 `ctx.tools.mcp__<server>__<tool>` 仍然是不存在的名字，按上文的「结构性缺失」抛 `TypeError`。collect 则相反：无池时 `CollectHost::install_connectors` 直接 `Ok(())`（`src/trigger/source/host.rs`），**什么都不装**，`Object.keys(ctx)` 与票 17 之前逐字一致。

`16KB` 结果上限是**固定安全预算**：evidence 量级是 5 条 × 4KB = 20KB，取同量级但更紧，也是 collect 上限（64KB）的四分之一——它是给 subagent 消化的中间产物，不是数据集。这个数字**不随 provider 窗口漂移**，也不该搬进 `pressure.rs`（ADR 0013 约束的是窗口派生的容量）。执行器 meta 的 `timeout_s` 只能覆盖超时，改不了输出闸。

结果形状 `{ok, summary, evidence?}` 的校验不在沙箱（它只保证返回是可解析 JSON 且在输出闸内），而是紧接 `execute_async` 之后由调用方做：字段规则、evidence 的条数/单条字数/单条字节三道裁剪、`summary` 的 500 字闸与失败分类都在 `src/executor/result.rs`，预算常量直接复用 trigger 的 `EVIDENCE_MAX_ITEMS` / `EVIDENCE_TEXT_MAX_CHARS`（见 [executor.md](executor.md#run一次按需动作)）。

**对外调用数闸与对外失败溯源**（票 13）。`ActHost` 在 `src/codemode/http.rs` 的宿主函数层挂了两个钩子（`HttpCallHooks`），因此**脚本直接调 `ctx.http_get` / `ctx.http_post` 也绕不过去**：

- `before`：每次对外**尝试**先计数（`MAX_EXTERNAL_CALLS = 20`，连接器的**每一次**调用——读与写都算，不只是写——与 `ctx.http` 合计同一个计数器：act 的连接器 invoke 在 `call_gated` 之前走同一个 `CallBudget::charge`（`src/codemode/act/budget.rs`），不是第二份预算）。超出上限时把 `AbortHandle` 置为 `stop(reason)`，错误文本之外还靠沙箱的 interrupt 机制**终止整脚本**——脚本 `try/catch` 吞不掉，因为 interrupt 抛的是 uncatchable error。计的是尝试而非成功：被 SSRF 守卫挡下也算，否则重试就能绕过预算。
- `after`：把「没按期望答复」的对外调用记进 `ActHost`（`answered HTTP 4xx/5xx` 或传输失败，带 method + URL + 原因），供 `executor run` 把随后的脚本异常归为**外部失败**而不是脚本 bug。

`AbortHandle`（`src/codemode/sandbox.rs`）是「宿主函数当场停脚本」的唯一手段：`RunRequest::with_abort(handle)` 把开关交给 VM，interrupt handler 同时读超时 flag 与 abort flag；abort 的判定**早于** `classify`（否则会被报成 `timeout`），并且 settle 成功分支也要再查一次（一个「一次宿主调用后直接 return」的脚本可能来不及被 interrupt）。`AbortKind::{Stopped, Refused}` 让调用方能区分「宿主闸终止」与「策略拒绝」，第一次写入者胜——拒绝不会被随后的预算计数覆盖。

## 秘密抹除

`Redactor` 由 `triggers.secrets` 的值构造，装在 `HostEnv` 里；脚本的返回值、`ctx.log` 的行、错误消息三类出口都过一遍替换（`***`）。这是尽力而为的子串替换，挡的是无意回显，不挡铁了心要 base64 的脚本——真正的边界是白名单里根本没有能让它跑出去的东西。空值被丢弃：空 needle 会把每个字符串都变成 `***`。

## 错误分类

`ScriptError{kind, message}`，kind 共八种：`syntax` / `exception` / `timeout` / `memory` / `output` / `missing_entry` / `aborted` / `internal`。VM 抛出的异常由 `classify()` 归到前三类之一；脚本线程 panic 记 `internal`，**永不把宿主进程带下去**。`aborted` 是宿主函数通过 `AbortHandle` 主动停的（原因在 `message` 里），它不参与 `classify()` 的推断，而是在 classify 之前按 abort 开关判定。上层把它渲染成一行文本进 warning / history，模型看到的是一条可以据此改脚本的错误，而不是一次神秘的沉默。
