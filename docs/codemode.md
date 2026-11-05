# Codemode 脚本沙箱

对应模块：`src/codemode/`（`sandbox.rs`、`host.rs`、`agent.rs`、`redact.rs`、`error.rs`）+ 模型入口工具 `src/tools/codemode.rs`

## 概述

模型写的代码要能无人值守地跑，而且不能给这台机器一个 shell。三种脚本共用一个求值器：数据源的 `collect(ctx)`、触发器条件的 `judge(ctx)`，与模型/子代理提交的 agent 程序 `main(ctx)`（票 25）。选型与实测数据见 [ADR 0008](adr/0008-codemode-runtime-choice.md)（引擎 = `rquickjs`，中断 203ms 生效，8MB 上限 2ms 报错，二进制净增 772KB）。

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

调用方式：`execute_async(request, host, env)`（生产路径，异步）与 `execute(...)`（同步，目前只有测试与「无 runtime 可交」的调用者用）。脚本必须定义入口函数，否则报 `missing_entry`。

## ctx：数据 + 白名单函数

`RunRequest::with_ctx(value)` 传纯数据（快照视图、上次的 store 值…）。宿主函数装在 `__host` 上，包装层做一次 `Object.assign({}, JSON.parse(__ctx_json), __host)`，所以脚本里数据和函数长在同一层：`ctx.sources` 与 `ctx.log(...)` 并列。

`HostBindings::install(ctx, env)` 是白名单的唯一入口——每个执行装一次，底层装的是 `__host.fs_stat` 这类 JSON 原语，再由一段 JS shim 包装成脚本看到的形状（`__host.fs = { stat, read, list }`、`__host.http = { get }`、`__host.store = { get, set, keys }`）。实现在 `src/trigger/source/host.rs` 与 `src/trigger/subscription/host.rs`：

| 装给谁 | 脚本可见的函数 |
|--------|----------------|
| collect | `ctx.http.get/post(url, opts?)`、`ctx.fs.stat/read/list(path)`、`ctx.secret(name)`、`ctx.log(msg)`、`ctx.now()`、`ctx.boot_at()`、`ctx.uptime_s()` |
| judge | `ctx.store.get/set/keys`、`ctx.log(msg)`、`ctx.now()`、`ctx.boot_at()`、`ctx.uptime_s()` |
| agent（`main`） | `ctx.tools.<name>(args?)`、`ctx.searchTools(query, limit?)`、`ctx.log(msg)`、`ctx.now()`、`ctx.boot_at()`、`ctx.uptime_s()` |
| 测试基线 | `NoHost`（一个函数都不装） |

判定侧**没有** http / fs / secret / subagent：条件只读已经采好的世界，永远不派活。时间三件是共享 helper `install_time`：`now()` 返回 `{epoch_ms, iso, hour, minute, weekday, date}`，`boot_at()` 是进程首次被问到时的 epoch 毫秒，`uptime_s()` 是**本次执行**开始以来的秒数；全部本地时间（`chrono::Local`），不提供时区配置。`install_log` 把 `log(msg)` 装成先脱敏再入 sink 的形式。

## agent 入口（票 25）

实现在 `src/codemode/agent.rs`（`AgentHost`）+ `src/tools/codemode.rs`（模型可见的 `codemode` 工具，默认工具集的第 10 个）。模型/子代理提交一段 `async function main(ctx)`，脚本的**唯一**额外能力是运行装配的工具面：

- `await ctx.tools.<name>(args)`：调本次运行 `ToolManager` 装配的工具；目录里没有的名字在命名空间里**结构性缺失**（`typeof ctx.tools.nope === "undefined"`），能力清单仍是唯一安全闸；
- `await ctx.searchTools(query, limit?)`：对本运行工具目录（name+description）做 BM25-lite 排序检索（复用票 15 `KeywordIndex` 的分词与打分核心：拉丁词 + CJK bigram、名字命中 ×2 权重），返回 JSON 字符串 `[{name, description}]`；未命中的词不产生结果，空查询无结果；
- 其余只有时间三件与 `log`。**没有** fs / http / secret / store：要碰外部世界只能走 `tools.*`，由各工具自己的风险等级把关。

每次 `tools.*` 调用走与顶层 tool_call 相同的审批闸（ADR 0011）：决策核心从 `ApprovalHook` 抽出到 `src/runner/gate.rs`（`GateContext` + `decide()`），钩子与嵌套调用共用同一 `decide`，`Allow/Deny/Ask`、redemption、veto、审计 span 逐字一致。运行期通路：runner 把 `RunToolExec`（内含 `GateContext`）挂进 task-local 的 `TurnContext`，`codemode` 工具在 `execute()` 里取到它、`AgentHost` 把每个工具装成异步宿主函数。唯一的形态差别：脚本里**没有中途弹窗**——`Deny` 的拒绝文本、`Ask` 的 NOT EXECUTED 指引文本作为调用的** resolved 值**返回给脚本（顶层对应的是把 tool 结果写成同样的文本再结束轮次）；attended 轮里嵌套 `Ask` 照常写入 pending 记录，脚本跑完后 `codemode` 工具把 owner 可见的 pending 清单附在工具结果尾部，下一轮模型重放同一脚本、同一 args 哈希按 redemption 放行。无人值守轮（subagent/cron/trigger）里嵌套 `Ask` 与顶层一样 fail closed 直接拒绝，不写 pending。

三条相关约束：

- **本轮提出的申请不能被本轮重放兑现**。`ApprovalStore::matching` 只认 carried（本轮开始时遗留）的记录，和 veto 判定 `mismatching` 同一个口径；否则脚本里连续两次 `tools.send(同 args)`（或顶层一批两个相同 tool_call）就等于自己批准自己。票 25 之前这条只在顶层理论可达（Ask 立刻结束轮次），有了嵌套调用面就变成真实绕过路径，因此一并收紧。
- **嵌套调用会重建轮次作用域**。宿主函数跑在沙箱专属线程上，runner 的 task-local 到不了；`AgentHost` 把 `TurnContext` 带进去，每次调用用 `with_turn_context` 包住执行，所以 `message` / `cron` / `trigger` 这类按当前轮 origin 解析投递目标的工具，在脚本里看到的 channel/chat_id 与顶层一致（`sent_in_turn` 也照样标记在同一个 Arc 上）。
- 嵌套调用没有自己的 `execute_tool` span，只有外层 `codemode` 那一个；脚本内部的调用序列靠 `ctx.log` 与返回值自述。

## 秘密抹除

`Redactor` 由 `triggers.secrets` 的值构造，装在 `HostEnv` 里；脚本的返回值、`ctx.log` 的行、错误消息三类出口都过一遍替换（`***`）。这是尽力而为的子串替换，挡的是无意回显，不挡铁了心要 base64 的脚本——真正的边界是白名单里根本没有能让它跑出去的东西。空值被丢弃：空 needle 会把每个字符串都变成 `***`。

## 错误分类

`ScriptError{kind, message}`，kind 共七种：`syntax` / `exception` / `timeout` / `memory` / `output` / `missing_entry` / `internal`。VM 抛出的异常由 `classify()` 归到前三类之一；脚本线程 panic 记 `internal`，**永不把宿主进程带下去**。上层把它渲染成一行文本进 warning / history，模型看到的是一条可以据此改脚本的错误，而不是一次神秘的沉默。
