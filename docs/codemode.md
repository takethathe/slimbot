# Codemode 脚本沙箱

对应模块：`src/codemode/`（`sandbox.rs`、`host.rs`、`redact.rs`、`error.rs`）

## 概述

模型写的代码要能无人值守地跑，而且不能给这台机器一个 shell。两种脚本共用一个求值器：数据源的 `collect(ctx)` 与触发器条件的 `judge(ctx)`。选型与实测数据见 [ADR 0008](adr/0008-codemode-runtime-choice.md)（引擎 = `rquickjs`，中断 203ms 生效，8MB 上限 2ms 报错，二进制净增 772KB）。

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

调用方式：`execute_async(request, host, env)`（生产路径，异步）与 `execute(...)`（同步，目前只有测试与「无 runtime 可交」的调用者用）。脚本必须定义入口函数，否则报 `missing_entry`。

## ctx：数据 + 白名单函数

`RunRequest::with_ctx(value)` 传纯数据（快照视图、上次的 store 值…）。宿主函数装在 `__host` 上，包装层做一次 `Object.assign({}, JSON.parse(__ctx_json), __host)`，所以脚本里数据和函数长在同一层：`ctx.sources` 与 `ctx.log(...)` 并列。

`HostBindings::install(ctx, env)` 是白名单的唯一入口——每个执行装一次，底层装的是 `__host.fs_stat` 这类 JSON 原语，再由一段 JS shim 包装成脚本看到的形状（`__host.fs = { stat, read, list }`、`__host.http = { get }`、`__host.store = { get, set, keys }`）。实现在 `src/trigger/source/host.rs` 与 `src/trigger/subscription/host.rs`：

| 装给谁 | 脚本可见的函数 |
|--------|----------------|
| collect | `ctx.http.get/post(url, opts?)`、`ctx.fs.stat/read/list(path)`、`ctx.secret(name)`、`ctx.log(msg)`、`ctx.now()`、`ctx.boot_at()`、`ctx.uptime_s()` |
| judge | `ctx.store.get/set/keys`、`ctx.log(msg)`、`ctx.now()`、`ctx.boot_at()`、`ctx.uptime_s()` |
| 测试基线 | `NoHost`（一个函数都不装） |

判定侧**没有** http / fs / secret / subagent：条件只读已经采好的世界，永远不派活。时间三件是共享 helper `install_time`：`now()` 返回 `{epoch_ms, iso, hour, minute, weekday, date}`，`boot_at()` 是进程首次被问到时的 epoch 毫秒，`uptime_s()` 是**本次执行**开始以来的秒数；全部本地时间（`chrono::Local`），不提供时区配置。`install_log` 把 `log(msg)` 装成先脱敏再入 sink 的形式。

## 秘密抹除

`Redactor` 由 `triggers.secrets` 的值构造，装在 `HostEnv` 里；脚本的返回值、`ctx.log` 的行、错误消息三类出口都过一遍替换（`***`）。这是尽力而为的子串替换，挡的是无意回显，不挡铁了心要 base64 的脚本——真正的边界是白名单里根本没有能让它跑出去的东西。空值被丢弃：空 needle 会把每个字符串都变成 `***`。

## 错误分类

`ScriptError{kind, message}`，kind 共七种：`syntax` / `exception` / `timeout` / `memory` / `output` / `missing_entry` / `internal`。VM 抛出的异常由 `classify()` 归到前三类之一；脚本线程 panic 记 `internal`，**永不把宿主进程带下去**。上层把它渲染成一行文本进 warning / history，模型看到的是一条可以据此改脚本的错误，而不是一次神秘的沉默。
