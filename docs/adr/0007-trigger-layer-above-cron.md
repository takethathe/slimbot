# 触发器是 cron 之上的一层，而不是 cron 的超集

cron 表达的是「到点跑一段固定文本」，触发器表达的是「条件成立时做某事」。看起来触发器只是 cron 多了个条件，于是很容易得出「把它做成 cron 的超集、顺手迁掉 `jobs.json`」的结论。我们不这么做：超集的直接后果是 `CronService` 必须认识快照与脚本，`jobs.json` 会变成两种语义混住的容器，而 Dream job 与用户 job 的迁移要证明语义等价——恰恰不等价。cron 逾期会补跑一次，触发器的定案是**从 `now` 重算、不补触发**（停机 8 小时后补发 8 条「该喝水了」比不补更糟）。沿用 cron 的存储就等于把这条有意分歧悄悄改回去，而且只在停机时暴露。所以 `jobs.json` 继续只装 cron job，触发器一条也不进去。

触发器也不复用 cron 的调度器，只借用它的**表达**：`meta.schedule` 与 `CronSchedule` 同构（`{at_ms}` / `{every_ms}` / `{expr}`），由 trigger 服务自己 `compute_next_run()` 算到期，不注册 `CronService`。并且 `schedule` 只是**闸门**：到期后 `condition.js` 照常跑，纯时间提醒才可以只写 `schedule` + `action`。这样「每天 9 点检查一次」与「条件成立时提醒」是同一条管线上的两档，而不是两套机制。

tick 独立：`tokio::time::interval(60s)` 与 cron（1s）、heartbeat（30m）并列，不串在 cron 的 sleep 循环后面。串行的坏处是双重漂移——cron 自己用 `sleep` 累加，会把每轮耗时累进周期；再把它当触发器的时钟，`every_n_ticks` 这种按 tick 序号取模的调度就会移位。用 `interval` 让 tick 序号稳定，也让三套周期互不阻塞。

`schedule` 未到期时**不跑 judge、也不动边沿状态**。这一条不是实现细节而是语义正确性：若把闸门当 `false` 写进状态，「每天 9 点、条件持续为真」会在其余 23 小时被刷成 `false`，9 点时又变成 `false→true`，于是「提醒一次」变成了「每天都提醒」。想要每天都提醒，应当写 `cooldown_ms ≈ 24h` 或拆成两条规则，而不是靠闸门的副作用。

时区只认系统本地时间：`Cron{expr}` 用 `chrono::Local` 解析，`At` 在创建时换算成 `epoch_ms`，不引入 `chrono-tz` 也不在 config 里开时区。cron 那边忽略 `tz` 字段的既有行为不改。

两套「定时」共存靠文案划界，不靠代码合并：`cron` 工具的描述改为「底层调度器 / 系统任务（如 Dream）」，触发器场景一律提示走 `trigger` agent；`trigger_list` 不展示 cron job。v1 不做迁移——让 trigger 吸收 `/cron` 的 add/list 是另一次 effort。
