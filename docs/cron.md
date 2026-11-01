# 定时任务（cron）

对应模块：`src/cron/`（`service.rs` + `types.rs`）

## 概述

`CronService` 是 Gateway 模式下的常驻调度器，把「什么时候」与「做什么」分开：`CronJob` 只描述日程与一个 payload，到点由 Gateway 注册的回调（`set_on_job`）把 payload 变成一次 Agent run、一段维护动作（Dream）或一条投递。

## 数据

- 持久化：`{workspace}/cron/jobs.json`（`CronStore{version, jobs}`，`save()` 全量重写）
- `CronJob`：`id` / `name` / `enabled` / `schedule` / `payload` / `state` / `delete_after_run`
- `CronJobState`：`next_run_at_ms`、`last_run_at_ms`、`last_status`、`last_error`、`run_history`
- `run_history` 最多保留 `MAX_RUN_HISTORY = 20` 条，每条 `CronRunRecord{run_at_ms, status, duration_ms, error}`，超出按最旧丢弃

三种日程（`CronSchedule`）：`At{at_ms}`（一次性）、`Every{every_ms}`（间隔）、`Cron{expr,tz}`（表达式）。

## 时间基准：本地时间

调度**全库统一 `chrono::Local`**（ADR 0018）：`Cron{expr}` 按本机时钟解析，与触发器的 `next_run_after`（`src/trigger/subscription/judge.rs`）读同一把表——同一表达式在同一锚点给出同一结果，由 `test_cron_expression_resolves_same_wall_time_across_paths` 锁定。

- **不引入 `chrono-tz`**：机器时钟是唯一参考。
- **`tz` 字段接受但不生效**：`CronSchedule::Cron{tz}` 保留以兼容存量 `jobs.json` 与工具入参，不参与计算（`test_cron_timezone_field_is_accepted_but_inert`）。
- 表达式沿用 `cron` crate 的 6 段格式（`秒 分 时 日 月 周`），用 `Schedule::after(local_anchor)` 取下一次，严格晚于当前时刻。

> UTC → Local 是一次性、不可逆的基准变更：存量任务的 `next_run_at_ms` 是按旧基准写入的，切换后会即时改变其触发时刻。这是 ADR 0018 明确接受的代价。

## tick 语义

Gateway 每 1s 调一次 `tick()`（`config.gateway.cron.enabled` 为假时不启动；`stop()` 后 tick 是空操作）。

1. 取 `now`，锁内挑出两类作业：**到点**（`enabled` 且 `next_run_at_ms <= now`）与**已错过的一次性窗口**（见下节）。
2. 到点作业逐个执行回调（回调在锁外，避免重入死锁）。
3. 每次执行写一条 `status = "ok"` 的 run 记录；一次性（`At`）或 `delete_after_run` 的作业执行后 `enabled=false`、`next_run_at_ms=None`，`delete_after_run` 的直接从 `jobs.json` 删除；周期作业的 `next_run_at_ms` 从**执行后的 now** 重算。

**周期作业没有补跑**：`next_run_at_ms` 总是向前重算，`start()` 也会把所有启用作业的相位锚到启动时刻，因此长时间停机后只会从「现在」重新对齐，不会把错过的轮次一次性补出来。

## 错过的一次性窗口 = `skipped`

一次性作业只有一个时刻。宽限窗口 `ONE_SHOT_GRACE_MS = 5 分钟`：

- 到期后 5 分钟内被 tick 捞到 → 照常执行（tick 迟一点不算错过）。
- 超过宽限（进程停机，或某个作业把调度器堵了很久）→ 这次发生记为**错过**：`last_status = "skipped"`、`last_error = "missed one-shot window"`，`run_history` 追加一条 `status = "skipped"`（`run_at_ms` = 原定时刻），作业 `enabled=false`、`next_run_at_ms=None`。
- **绝不补跑**：skipped 只记一次，后续 tick 不再触发；作业留在 `jobs.json` 里（禁用态）作为审计痕迹，**不**因 `delete_after_run` 被删掉。
- 三个入口共用同一套判定：`add_job()`（创建即已过期）、`start()`（停机后重启）、`tick()`（运行中被堵）。什么都没跑，所以 `last_run_at_ms` 保持为空。

`Every` / `Cron` 错过的轮次不写 `skipped`：周期作业的语义是「从现在起按相位重复」，错过的轮次直接作废（同样不补跑）。

## 与触发器层的关系

触发器的 `schedule` 也复用 `CronSchedule` 表达，但**自算到期**、只写自己的 `state.json`，不进 `jobs.json`（见 [trigger.md](trigger.md)）。两层共享的约定是：本地时间、过期的一次性日程不补触发、过期的一次性作业/规则停用但保留定义。本层的改动不会触及触发器的到期判定（`docs/TODO.md` 票 12）。

## 相关测试

| 行为 | 测试 |
|---|---|
| cron 表达式按本地墙钟解析 | `cron::service::tests::test_compute_next_cron_resolves_local_wall_clock` |
| `tz` 接受但不生效 | `cron::service::tests::test_cron_timezone_field_is_accepted_but_inert` |
| cron 与触发器两路同值 | `cron::service::tests::test_cron_expression_resolves_same_wall_time_across_paths` |
| `add_job` 写下的 `next_run_at_ms` 是本地墙钟 | `cron::service::tests::test_cron_service_add_job_schedules_local_wall_clock` |
| 创建即过期 → 记 `skipped`、不执行 | `cron::service::tests::test_add_job_missed_one_shot_records_skipped_and_never_runs` |
| 停机错过 → 记 `skipped`、不补跑 | `cron::service::tests::test_cron_tick_records_skipped_for_one_shot_missed_while_down` |
| 宽限内晚到的一次性仍执行 | `cron::service::tests::test_cron_tick_runs_one_shot_inside_the_grace_window` |