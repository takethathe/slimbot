# Trigger 触发器层

对应模块：`src/trigger/`（`mod.rs`、`source/`、`subscription/`、`dispatch.rs`、`tool.rs`），依赖 `src/agent/`（子代理运行时）与 `src/codemode/`（脚本沙箱）。

## 概述

触发器是「一条持久规则：当某条件成立时，做某事并告知某人」。它是 **cron 之上的一层，而不是 cron 的超集**（见 [ADR 0007](adr/0007-trigger-layer-above-cron.md)）：自己有固定的 60s tick，不写 `cron/jobs.json`，也不迁移任何既有定时任务。分层的好处是 `every_n_ticks` 有确定含义——cron 循环在两轮之间会睡自己的剩余时间并累积误差，一个纯计数器不会。

一个 tick 只做五件事：**热加载定义 → 冻结快照 → 采集到期的源 → 判定到期的规则 → 把命中丢进队列**。对命中采取行动（跑子代理、给用户发消息）刻意不在 tick 内，见 [dispatch](#执行与投递dispatch) 一节。

三个子层各司其职：

| 子层 | 模块 | 职责 |
|------|------|------|
| 数据源 | `src/trigger/source/` | 采集、归一化、内存快照、健康度 |
| 订阅规则 | `src/trigger/subscription/` | 定义、判定脚本、边沿状态机、试跑 |
| 执行投递 | `src/trigger/dispatch.rs` | 队列、action、投递、fire 账本 |

## 目录布局

```
workspace/
├── agents/trigger.md              # 内置 trigger agent 定义（setup 铺出）
└── triggers/
    ├── sources/<src-xxxx>/        # meta.json + collect.js
    ├── triggers/<trg-xxxx>/       # meta.json + condition.js + state.json
    └── history.jsonl              # fire 账本，宿主独占写
```

文件是唯一真相：定义由工具写入，每 tick 按 mtime 热加载；`state.json` 由宿主独占写，脚本改不了。快照**不落盘**——外部内容在磁盘上没有路径，这是 [ADR 0009](adr/0009-no-raw-external-content.md) 那条边界的实现方式。

## 数据源（source）

一个源 = 一个目录 + 一个 `collect(ctx)` 脚本。脚本只有一条返回路径：`{items: [...]}`，源的 `mode` 决定这些条目怎么并入窗口——条件脚本因此不需要区分源是天气、邮箱还是文件清单。上限一律**截断而非失败**：单条 4KB（先裁最长的字符串，裁不动才丢弃）、单次 64KB，被砍掉的数量作为 warning 由 tick 记日志。

- **mode**：`snapshot` 每轮整窗替换；`append` 按 fingerprint（脚本自带字段或整条哈希）去重追加，超出 `max_items`（默认 200）丢最旧。
- **cadence**：`every_n_ticks` 用 tick 序号取模决定本轮是否采集；同 tick 内并发上限 `max_concurrent_collect`（默认 4）。
- **冻结 / 提交**：tick 开始时 `freeze()` 出一份只读 `TickFrame`，结束时 `commit()`，所以同一 tick 里所有条件读到的是同一个瞬间，不存在「读到半新半旧」的窗口。
- **快照 TTL**：`DEFAULT_TTL` 24h，过期条目从视图消失但仍在 runtime 里，直到被 `max_items` 挤出。
- **失败是局部的**：单源超时、抛错、磁盘上脚本坏了，只记在这个源身上，其它源照常提交；连续 3 次失败降到每 5 个 tick 采一次，成功即恢复；脚本改坏时保留上一版可用产物。
- **凭证**：只来自 `triggers.secrets`，脚本用 `ctx.secret(name)` 取；`Redactor` 会把值从返回值、日志与错误信息里抹掉。

宿主函数白名单（详见 [codemode.md](codemode.md)）：`ctx.http.get` / `post`、`ctx.fs.stat` / `read` / `list`、`ctx.secret(name)`、`ctx.log(msg)`、`ctx.now()` / `boot_at()` / `uptime_s()`。**没有** shell，也没有 sleep——文件变更用轮询脚本表达。`ctx.http.get/post` 复用 `web_fetch` 的 SSRF 校验（`is_private_ip` / `resolve_and_pin`），fs 调用默认锁在 workspace 内，绝对路径要求源在 `allow_paths` 里声明过。

## 订阅规则（subscription）

一个规则 = `meta.json`（定义）+ `condition.js`（模型写的判定）+ `state.json`（宿主写的成绩）。

`judge(ctx)` 返回 `{fire, evidence?, detail?}`，`ctx` 只有本轮冻结的 `sources` 视图、`fired_keys`、`fire_count`、`last_fired_at`、`now_ms`，加上 `ctx.store.get/set/keys`、`ctx.log` 与时间三件——**不能调工具，也不能派子代理**（[codemode.md](codemode.md) 的沙箱宿主里没有 http / fs / secret）。预算：500ms / 16MB / 4KB 载荷。

判定语义：

- **边沿为主**：条件持续为真只算一次命中；`prev` 只在真正命中后翻真，冷却期内再命中也只会推进 `prev` 而不补发。没有 `condition.js` 的规则（纯日程提醒）每次到期都算一次新的上升沿。
- **ready 门槛**：规则声明了源但那些源还没采到过数据 → 跳过，不动状态。冷启动因此不可能「一启动就炸一串提醒」。
- **schedule 只是闸门**：`meta.schedule` 复用 `CronSchedule` 的表达（cron / every / at），到期与否自己算，不进 `jobs.json`。一次性 `At` 跑完即 `enabled=false`，保留定义。
- **本地时间**：全部用 `chrono::Local`；重启后周期类从当前时间重算，已过期的一次性规则被停用，**一律不补触发**。
- **证据上限**：最多 5 条、单条 200 字、证据加摘要 4KB，超出截断不报错；`fired_keys` 上限 1000 条按时间淘汰（7 天 TTL），`ctx.store` 16KB 超限写入失败并记日志。
- **失败不自动停用**：连续 3 次降频到每 5 tick，连续 5 次置告警位（投递见 dispatch）。

## 执行与投递（dispatch）

命中变成 `Fire` 交给 `Dispatcher`：一个 `max_queue`（默认 20）的有界 FIFO，**满则丢最新并记一条 `ok=false` 的账**——迟到十分钟的提醒比不提醒更糟，所以不顺延、不重试。单个消费者在 tick 之外按创建顺序执行，把最贵的模型调用挪出 30s 硬超时。消费者另外暴露一个同步 `drain()`，测试靠它拿到确定性，而不是 sleep 等待。

四种工作项：

| Action | 行为 |
|--------|------|
| `agent` | 起一个子代理 run（并发满时 `WaitForSlot` 等空位，而不是回 `busy`），结果由运行时投回来源会话 |
| `notify` | 把渲染好的文本作为**一条**入站输入注入来源会话 |
| `record` | 只写 history，永不投递 |
| `Alert` | 连续失败的固定文本告警，直发渠道 |

**deliver 三级求值**（`agent`）：`action.deliver` 显式声明 → agent 定义的 `deliver`（默认 true）→ 有没有可路由的 origin；都没有则不投递、只记日志。`notify` 的目的地：`action.channel` + `to` → origin → 没有就丢弃并告警，**绝不落到某个默认会话**。

投递走来源会话的**入站路径**（`MessageBus::publish_inbound` + `TaskHook`），于是白拿主 loop 的完整工具集、per-session 串行队列、出站路由和 `/stop` 语义。唯一例外是失败告警：它是运维信号不是结果总结，内容固定、无可总结，所以直接 `publish_outbound`，不经主 loop、不调模型。

合并只发生在 `notify`，且只限同一 tick、同一目的地（上限 `max_merge`，默认 3；超出部分丢弃并落 `ok=false`）。`agent` 的结果**不合并**——它们在不同时刻完成，把先完成的按住等慢的那个是缺陷不是特性。

结果文案固定为「结构化头行 + 正文 + 总结指令行」：`[Subagent result] agent=… run_id=… ok=… elapsed=…s`；失败时同构但 `ok=false`、正文换成错误摘要、指令行换成「说明原因并给出下一步」。`notify` 的头行是 `[Trigger notice] id=a,b merged=N at=…`，尾行明确告诉主 loop「这是触发器命中，不是用户的话」。

模板占位符：`{{evidence}}`（每行 `- [src @ RFC3339] text`，无证据给 `(no evidence)`）、`{{detail}}`、`{{now}}`，渲染结果 8KB 封顶。

## history.jsonl

append-only，每行只有六个字段：`{trigger_id, fired_at, action_kind, ok, merged, detail}`——**没有证据正文**。滚动到 `history_lines`（默认 5000）只留一代；写失败只记日志，不阻断触发。

## 工具与入口

两个多 action 工具**不注册进主 loop 的默认工具集**，只有声明它们的 agent 能看见：

| 工具 | actions | 说明 |
|------|---------|------|
| `trigger_source` | `list` / `create` / `update` / `delete` | `list` 只回 `shape`（字段名、类型、条数、观测时间），**不回显真实值且绝不触发采集** |
| `trigger` | `list` / `create` / `update` / `delete` / `test` | `create` 要求 `schedule` 与 `script` 至少有一个；`test` 用当前冻结快照立即跑一次，不 fire、不投递、不写状态 |

用户侧入口是内置 agent `agents/trigger.md`（system prompt 里以 `# Available Agents` 一行出现）。**一个能力一个入口**：触发器只以 agent 存在，不注册 skill——两者并存时模型会走不用读文档的那条路。

## 配置

`config.json` 的 `triggers` 段（详见 [config.md](config.md)）：`enabled`、`tick_interval_s`(60)、`tick_timeout_s`(30)、`max_concurrent_collect`(4)、`max_queue`(20)、`max_merge`(3)、`history_lines`(5000)、`secrets`（`name` + 掩码 `value` 的列表）。子代理并发上限在 `subagent.max_concurrent`(3)，因为子代理是主 loop 与触发器共用的能力。

## Trace

三个**并列**的 root span，靠 `trigger_id` / `run_id` 关联，彼此不设父子——异步 run 会活过发起它的 tick，挂成 child 会产生子 span 晚于父 span 结束的畸形 trace：

- `trigger.tick`：`sources_due` / `sources_ok` / `sources_failed` / `triggers_judged` / `fired` / `elapsed_ms`；空 tick 不记，否则 60s 一条会把 trace 变成噪音墙
- `trigger.fire`：`trigger_id` / `trigger_name` / `action_kind` / `agent` / `run_id` / `ok` / `merged`
- `subagent.run`：`agent` / `run_id` / `ok` / `elapsed_ms`（见 [agent.md](agent.md)）

## Gateway 装配

`cargo run -- gateway` 起三个并列的后台循环：cron（1s 轮询）、heartbeat、trigger tick（60s interval），外加 dispatcher 的单消费者任务。`TriggerService` 与两个工具共享同一份 layer 实例，所以 agent 写的规则就是下一个 tick 判定的规则。
