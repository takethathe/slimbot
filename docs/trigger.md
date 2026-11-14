# Trigger 触发器层

对应模块：`src/trigger/`（`mod.rs`、`source/`、`subscription/`、`dispatch.rs`、`tool.rs`），依赖 `src/agent/`（子代理运行时）与 `src/codemode/`（脚本沙箱）。

## 概述

触发器是「一条持久规则：当某条件成立时，做某事并告知某人」。它是 **cron 之上的一层，而不是 cron 的超集**（见 [ADR 0007](adr/0007-trigger-layer-above-cron.md)）：自己有固定的 60s tick，不写 `cron/jobs.json`，也不迁移任何既有定时任务。分层的好处是 `every_n_ticks` 有确定含义——cron 循环在两轮之间会睡自己的剩余时间并累积误差，一个纯计数器不会。

一个 tick 只做五件事：**热加载定义 → 冻结快照 → 采集到期的源 → 判定到期的规则 → 把命中丢进队列**。热加载之后、采集之前还会把「本 tick 启用的规则读了哪些源」推给 source 层（引用计数，见[引用计数](#引用计数)）；对命中采取行动（跑子代理、给用户发消息）刻意不在 tick 内，见 [dispatch](#执行与投递dispatch) 一节。

三个子层各司其职：

| 子层 | 模块 | 职责 |
|------|------|------|
| 数据源 | `src/trigger/source/` | 采集、归一化、内存快照、健康度；虚拟源（MCP 订阅）的合成与冻结；引用计数过滤 |
| 订阅规则 | `src/trigger/subscription/` | 定义、判定脚本、边沿状态机、试跑；`referenced_sources()` 是引用集合的唯一来源 |
| 执行投递 | `src/trigger/dispatch.rs` | 队列、action、投递、fire 账本 |

## 目录布局

```
workspace/
├── agents/trigger.md              # 内置 trigger agent 定义（setup 铺出）
├── agents/executor.md             # 内置 executor agent 定义，fire 一个执行器就是起它
└── triggers/
    ├── sources/<src-xxxx>/        # meta.json + collect.js
    ├── triggers/<trg-xxxx>/       # meta.json + condition.js + state.json
    └── history.jsonl              # fire 账本，宿主独占写
```

文件是唯一真相：定义由工具写入，每 tick 按 mtime 热加载；`state.json` 由宿主独占写，脚本改不了。快照**不落盘**——外部内容在磁盘上没有路径，这是 [ADR 0009](adr/0009-no-raw-external-content.md) 那条边界的实现方式。

## 数据源（source）

一个源默认 = 一个目录 + 一个 `collect(ctx)` 脚本（`origin = disk`）；另外两类**虚拟源**没有目录也没有脚本，由 `mcp.servers[].triggers[]` 声明而来（见[虚拟源](#虚拟源mcp-订阅)）。脚本只有一条返回路径：`{items: [...]}`，源的 `mode` 决定这些条目怎么并入窗口——条件脚本因此不需要区分源是天气、邮箱还是文件清单。上限一律**截断而非失败**：单条 4KB（先裁最长的字符串，裁不动才丢弃）、单次 64KB，被砍掉的数量作为 warning 由 tick 记日志。

- **mode**：`snapshot` 每轮整窗替换；`append` 按 fingerprint（脚本自带字段或整条哈希）去重追加，超出 `max_items`（默认 200）丢最旧。
- **cadence**：`every_n_ticks` 用 tick 序号取模决定本轮是否采集；同 tick 内并发上限 `max_concurrent_collect`（默认 4）。
- **冻结 / 提交**：tick 开始时 `freeze()` 出一份只读 `TickFrame`，结束时 `commit()`，所以同一 tick 里所有条件读到的是同一个瞬间，不存在「读到半新半旧」的窗口。
- **快照 TTL**：`DEFAULT_TTL` 24h，过期条目从视图消失但仍在 runtime 里，直到被 `max_items` 挤出。
- **失败是局部的**：单源超时、抛错、磁盘上脚本坏了，只记在这个源身上，其它源照常提交；连续 3 次失败降到每 5 个 tick 采一次，成功即恢复；脚本改坏时保留上一版可用产物。
- **凭证**：只来自 `triggers.secrets`，脚本用 `ctx.secret(name)` 取；`Redactor` 会把值从返回值、日志与错误信息里抹掉。

宿主函数白名单（详见 [codemode.md](codemode.md)）：`ctx.http.get` / `post`、`ctx.fs.stat` / `read` / `list`、`ctx.secret(name)`、`ctx.log(msg)`、`ctx.now()` / `boot_at()` / `uptime_s()`。**没有** shell，也没有 sleep——文件变更用轮询脚本表达。

进程里有连接器池时，采集器还多一条**连接器读**：`ctx.tools.mcp__<server>__<tool>(args?)` 与 `await ctx.searchTools(query, limit?)`，目录只含 server 声称只读（`readOnlyHint` 且非 `destructiveHint`）的工具——同一 server 的写工具既不装也搜不到，采集面因此是 act 写半的**读半镜像**。这是**加法**：基础原语一个不少，写面不存在，采集器**不接审批闸**（采集本来就无人值守、无审批，连接器读被限制在 owner 在 `mcp.servers[]` 里配过的 server 上，严格窄于既有 `ctx.http` 权限）。过滤口径、残余风险与理由见 [codemode.md](codemode.md#采集面的只读性票-17) 与 [mcp.md](mcp.md#风险映射fail-closed)。`ctx.http.get/post` 复用 `web_fetch` 的 SSRF 校验（`is_private_ip` / `resolve_and_pin`），fs 调用默认锁在 workspace 内，绝对路径要求源在 `allow_paths` 里声明过。

### 虚拟源（MCP 订阅）

`mcp.servers[].triggers[]` 的每条订阅同时是一个源，**没有目录、没有 `collect.js`**：`source_id` 就是条件脚本里 `ctx.sources[...]` 的键。两类：

| 订阅 `type` | 数据从哪来 | 何时进 frame |
|-------------|------------|--------------|
| `notification` | `McpService::notification_snapshot()`（server 推的 last-value 缓冲） | 每次 `freeze()` 直接读池，不经过 tick 的采集段 |
| `tool` | 到期 tick 里调 `McpService::call("mcp__<server>__<tool>", args)` | `collect()` 的并发段，与磁盘采集器同一批 |

- **来源标记**：runtime 带 `Origin::{Disk, Notification, Tool}`。`reload()` 按 config 重新合成虚拟项、保留旧的窗口与失败计数，config 删掉订阅即删项；磁盘目录与订阅撞 id 时订阅赢（并记一行 `warn`）。
- **notify 只冻结不采集**：池里有推就是 `{params, received_at}` 一条；池里没有（无池 / 没推过 / 超过 TTL）就是一条 `observed_at: None` 的空视图——源的**存在**不变，只是未采样，所以命名它的规则被 ready 门槛挡住而不是报「源不存在」。
- **tool 每次一个条目**：调用结果整段（字符串或结构化 JSON 序列化后）作为 `source_id` 名下的**唯一**一条 item，没有字段级拆分；`args` 缺省时调用传 `null`，由池归一成 `{}`。
- **tool 失败与采集器同构**：server 离线、调用报错、没有连接器池，都记一条 `TickReport.failed`（原因形如 `mcp__ci__list_runs: transport error: ...` 或 `no connector pool is attached; cannot call ...`），进健康度与降频。
- **配置校验**：每条订阅必须写 `source_id`；`type=notification` 必须写 `uri`，`type=tool` 必须写 `tool`，否则 `validate` 直接拒绝启动（形状校验跳过 disabled 的 server）。`source_id` 是帧里的键，一个 id 只能指一个源：**同一个 `source_id` 被两条订阅声明**（跨 server 或同一 server 内）由 `Config::validate_mcp()` 拒绝，报错点名撞名的 id 与涉及的 server，并在运行时不再「择一」——这条校验不看 `enabled`。
- **不可编辑**：虚拟源没有磁盘形态，只能用 `trigger_source list` 读；`update` / `delete` 命中虚拟源时被明确拒绝（`updated: false` / `deleted: false`，`error` 为 `not updated: source '<id>' is a virtual source declared in mcp.servers[].triggers; edit the MCP config instead`），既不写盘也不改内存。改它只能改 `mcp.servers[].triggers[]` 再等下一次 `reload()`（下一 tick）。
- **`trigger_source list`** 多一个 `origin` 字段（`disk` / `notification` / `tool`），仍然只报 shape、不回显值。

### 引用计数

只有**至少一个 `enabled` 规则**在 `meta.sources` 里点名的源才会被采集：`TriggerService::tick()` 在 `sources.reload()` 与 `rules.reload()` 之后把 `rules.referenced_sources()` 推给 source 层，`due()` / `due_sources()` / `freeze()` 都按这个集合过滤。规则停用或删除，它独占的源同 tick 就不再跑，但**定义仍在**（`list` 照常列）。

- 从未推送过集合（`None`）＝不过滤，全部照常采集。这是兼容性契约：单独构造 `SourceLayer` 的调用方（测试、装配前的窗口）行为与之前完全一致；**显式推空集合**才是「没有源被引用」。
- 未被引用的源**不出现在 frame 里**，所以规则点名一个它本不该读的源时拿到的是「sources not found」而不是旧数据。
- 引用集合只由**规则的 `enabled` 与 `meta.sources`** 推出（`SubscriptionLayer::referenced_sources()`，停用的规则不贡献）；虚拟源也照此过滤——没被引用的 `tool` 订阅不调用，`notification` 订阅不进 frame。
- `set_enabled`（`trigger update` 的 `enabled`、以及一次性 `At` 规则跑完自动停用）**会把新的 `enabled` 写回 `meta.json`**：`reload()` 每 tick 重读定义，只改内存的开关会在下一 tick 被翻回去。写盘失败只记 `warn`，内存里的状态照常生效到下次 reload。

### 活跃 source 上限

`triggers.max_active_sources`（默认 8，`0` = 不限，上限 100）限制的是**被计数**的源数量。上限按**被 enabled 规则引用的 source** 计；在第一次 tick 推送引用集合之前，所有 source 都计入（此时无法区分谁被引用，保守地把全部算进上限）。它只管 `trigger_source create`，`update` / `delete` 不受影响：

- 上限**先查、后试跑**：已满时 `create` 直接返回 `created: false`，不跑脚本、不消耗 id。
- 拒绝响应是一份**可比较的清单**，形状固定：

```json
{"created": false,
 "error": "not created: 8 source(s) are active and `triggers.max_active_sources` is 8. Compare the sources below …",
 "count": 8,
 "max_active_sources": 8,
 "sources": [{"id": "src-…", "name": "…", "description": "…",
              "referenced": true, "origin": "disk", "status": "ready"}]}
```

`referenced` 通常恒为真（清单只含被计数的源），保留它是为了让调用方不必自己推断；删掉重复的源或改掉读它的规则，下一次 `create` 就能过。首次 tick 之前那一段是唯一的例外：那时清单里是所有源，包括还没有规则引用的孤儿——它们此刻正在占额度，**delete 掉它照样能腾出位置**。

### create / update 内嵌试跑

`trigger_source create` / `update` 在**写盘之前**先把采集器跑一次，用的是 tick 的同一条实现（`trial_collect`，`collect_one` 也走它）：同一份 `CollectHost`、同一份 `Limits::collect().with_timeout(meta.timeout_s)`、同一份 secret 与连接器池。所以「建的时候能过、下一个 tick 却跑不动」不会因为两套沙箱漂移而发生。

- **create**：带 `script` 才试；不带 `script` 直接落盘成半成品（缺 `collect.js`），留给 `update` 补。
- **update**：试跑的是**合并后的 meta + 本次的脚本**（本次没带脚本就用盘上已有的那份），所以只改 `timeout_s` / `allow_paths` 也会被验一次——把超时收紧到脚本跑不完，在这一步就该被拒。没有任何脚本的源无可试，保持半成品。
- **失败不落盘**：`create` 回 `created: false`、`update` 回 `updated: false` 并保留旧版本，两者的 `error` 都以 `not created:` / `not updated:` 开头并带 `Cause:`，同一段原因另有一份结构化的 `test`。
- **响应只报条数与耗时**，**绝不回显采集到的值**（外部内容不进工具结果，ADR 0009）：

```json
{"test": {"ok": true,  "items": 12, "elapsed_ms": 340, "warnings": ["…"]}}
{"test": {"ok": false, "error": "…", "elapsed_ms": 8}}
```

`warnings` 是截断/丢弃计数一类的归一化告警，只在非空时出现。试跑**不写快照**：源的状态仍是 `pending`，要等第一个 tick 采到才有数据；`append` 模式在试跑时对着空窗口去重，所以报的是「脚本本身产出多少条」。

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

### fire 一个执行器 = 一条普通 `agent` 规则

「周期性地动」不需要新机件：它就是一条 `kind=agent` 的规则，`agent` 取 `executor`（[执行器](executor.md#周期性地动绑在触发器上)）。

```json
{"on_fire": {"kind": "agent", "agent": "executor",
             "task_template": "对 exec-1a2b 执行：{{evidence}}"}}
```

`action_from_args` 与 `ActionKind` 里**没有**第四种 kind：加一个 `executor{id,args}` 会让 dispatch 引擎绕过模型直跑 `act.js`，等于把「执行器执行 = 起子代理、由模型驱动」这条单一执行路径分岔。回归锁：`test_the_action_kinds_stay_the_three_the_engine_executes`。

路径与普通 `agent` 规则完全同一：`run_agent` → `SubagentRuntime::start(WaitForSlot)` → `agents/executor.md` 起的 ephemeral 子代理 → 子模型按 task 调 `executor run{id,args}` → 结果以 `[Subagent result]` 回 origin。因此**没有执行器专用的投递逻辑**：origin 携带、不可达时 book-only、`deliver` 三级求值全部继承。

**args 从 evidence 取值不建注入机件**：子模型读渲染后的 `{{evidence}}` 文本，自己组 `executor run` 的 `id` 与 `args`。占位符只渲文本，撑不起结构化模板，这正是 spec I 不采 `executor` kind 的理由。

**外部 evidence 注入 args = 提示注入的动作版，靠四道限界兜**（实现在别处，本层不重复实现）：

| 限界 | 挡什么 | 落在哪 |
|------|--------|--------|
| `allow_connectors` | 只能写到列表里的 server，之外的调用**终止整段脚本** | [executor.md](executor.md#allow_connectors写的天花板不是审批的替代) · `codemode/act/connectors.rs` |
| Allow-only | 无人值守子代理里 `Ask`/`Deny` 都立即拒绝，注入最多诱导一个本就允许的动作 | [executor.md](executor.md#失败一律-in-band按根因分类) · `runner/gate.rs` |
| `args_schema` | 不合 schema 的 args 在脚本启动前就被拒 | [executor.md](executor.md#meta-形状) · `executor/args.rs` |
| evidence 裁剪 + provenance | 每条 ≤200 字、合计 4KB，进 task 后再 8KB 封顶，且带 `src @ 时间` | 上文的 `clamp_verdict` / `render_template` |

**失败反馈是 in-band 的，不是 dispatch alert**：绑定错误的执行器、外部 4xx、审批被拒，全部由子模型写进 `[Subagent result]` 的正文回 origin。`run_agent` 失败只做两件事——`book(ok=false)` 写一行 history 与一行 `warn` 日志；**不存在「action 执行失败告警」这条路径**，也不许新增（alert 只由判定失败产生，语义是「同一规则连续失败退网」）。回归锁：`test_an_executor_fire_whose_write_needs_approval_reports_in_band_and_raises_no_alert`。

**接受噪声**：绑了 Ask 级执行器的规则每次 fire 都会给 origin 发一条「需审批、已拒、未执行」。这是正确反馈（无人值守永远兑现不了它），v1 **不做抑制/去重/静默**。

## history.jsonl

append-only，每行只有六个字段：`{trigger_id, fired_at, action_kind, ok, merged, detail}`——**没有证据正文**。滚动到 `history_lines`（默认 5000）只留一代；写失败只记日志，不阻断触发。

## 工具与入口

多 action 工具**不注册进主 loop 的默认工具集**，只有声明它们的 agent 能看见：

| 工具 | actions | 说明 |
|------|---------|------|
| `trigger_source` | `list` / `create` / `update` / `delete` | `list` 只回 `shape`（字段名、类型、条数、观测时间）与 `origin`，**不回显真实值且绝不触发采集**；`create` / `update` 写盘前先试跑（见 [create / update 内嵌试跑](#create--update-内嵌试跑)），`create` 还受 `max_active_sources` 限制 |
| `trigger` | `list` / `create` / `update` / `delete` / `test` | `create` 要求 `schedule` 与 `script` 至少有一个；`test` 用当前冻结快照立即跑一次，不 fire、不投递、不写状态；`update` 的 `enabled` 会写回 `meta.json` |
| `executor` | `list` / `create` / `update` / `delete` / `run` | 只在 `executor` 子代理内可见，同属 catalog-only；`run` 与四道闸见 [executor.md](executor.md#executor-工具) |

`trigger create` / `update` 会校验 `on_fire.agent` 指向的定义已落盘，所以写「fire 一个执行器」的规则前 `agents/executor.md` 必须已经存在（`setup` 铺出；老部署要重跑 `setup`）。

用户侧入口是内置 agent `agents/trigger.md`（system prompt 里以 `# Available Agents` 一行出现）。**一个能力一个入口**：触发器只以 agent 存在，不注册 skill——两者并存时模型会走不用读文档的那条路。它的 `tools:` 白名单是 `trigger_source, trigger, web_fetch`：前两个是唯一能改定义的入口，`web_fetch` 只用来在写采集器**之前**查一个陌生 API 的真实字段（一次性查证，不属于源，也不在 tick 里跑）。

## 配置

`config.json` 的 `triggers` 段（详见 [config.md](config.md)）：`enabled`、`tick_interval_s`(60)、`tick_timeout_s`(30)、`max_concurrent_collect`(4)、`max_queue`(20)、`max_merge`(3)、`history_lines`(5000)、`max_active_sources`(8，0 = 不限)、`secrets`（`name` + 掩码 `value` 的列表）以及 `triggers.secrets[]`。虚拟源不在这一节，在 `mcp.servers[].triggers[]`（见 [config.md](config.md#mcp--mcp-连接器)）。子代理并发上限在 `subagent.max_concurrent`(3)，因为子代理是主 loop 与触发器共用的能力。

## Trace

三个**并列**的 root span，靠 `trigger_id` / `run_id` 关联，彼此不设父子——异步 run 会活过发起它的 tick，挂成 child 会产生子 span 晚于父 span 结束的畸形 trace：

- `trigger.tick`：`tick_index` / `max_concurrent` / `sources_due` / `sources_ok` / `sources_failed` / `triggers_judged` / `fired` / `elapsed_ms`；空 tick 不记，否则 60s 一条会把 trace 变成噪音墙
- `trigger.fire`：`trigger_id` / `trigger_name` / `action_kind` / `agent` / `run_id` / `ok` / `merged`
- `subagent.run`：`agent` / `run_id` / `ok` / `elapsed_ms`（见 [agent.md](agent.md)）

## Gateway 装配

`cargo run -- gateway` 起三个并列的后台循环：cron（1s 轮询）、heartbeat、trigger tick（60s interval），外加 dispatcher 的单消费者任务。`TriggerService` 与两个工具共享同一份 layer 实例，所以 agent 写的规则就是下一个 tick 判定的规则。

装配本身收在 `trigger::assemble`（layers + config + workspace + tracer + 可选子代理运行时 + bus → `Wiring{service, dispatch}`）：账本路径、队列边界、共享运行时**只拼一次**，gateway 与端到端测试调的是同一份。

## 测试

层内测试在各自模块的 `#[cfg(test)] mod tests`（真写盘、真跑 rquickjs 的 Harness 已在层内）；跨模块的端到端用例在 `tests/trigger_integration.rs`：前十条走 `trigger::assemble` 与 gateway 同一份装配（断言面 = 投递文本 + `history.jsonl` + span），后两条是注册面锁（主循环 schema 无触发器工具、能力只以 agent 存在）。覆盖矩阵、测试纪律与 accepted gaps 见 [触发器测试方案](trigger-test-plan.md)。
