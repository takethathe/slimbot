# Trigger 测试方案

对应模块：`src/trigger/`（source / subscription / dispatch）、`src/codemode/`；端到端用例：`tests/trigger_integration.rs`。
测试决策的由来见 `.scratch/personal-agent-triggers/spec.md` 的 `## Testing Decisions`；本文是可执行清单。

## 结论

1. **存量不重写**。`src/trigger/` + `src/codemode/` 已有 229 条层内测试（163 + 66）（含真写盘、真跑 rquickjs 的模块内 Harness），全部保留，矩阵里标 `locked`。
2. **只补三类空白**：跨模块接缝（库外视角）、真实外部源的一条链、以及必须靠真装配才能验证的行为（重启续跑、tick 预算、采集并发、主循环的负向锁）。
3. **新增 12 条端到端 + 1 条层内补测**。端到端全部走真目录、真 rquickjs、真 `Dispatcher`、真 `MessageBus`；只有三处是假的：provider、MCP server、keychain。
4. **矩阵每行落到测试名，或落到带理由的 `accepted gap`**。

## 覆盖矩阵

`locked` = 既有测试已锁定，不新增；其余为本次新增。

| 组 | 行为点 | 测试 |
|----|--------|------|
| 源 | 定义形状 / id 生成 / 目录名优先 | `locked`：`trigger::source::meta`（9 条） |
| 源 | 热加载：改定义保快照、删目录即消失 | **新**：`trigger::source::tests::test_reload_keeps_the_snapshot_and_forgets_a_deleted_source` |
| 源 | collect 入口能力面（http / fs / secret / log / time / 只读连接器） | `locked`：`trigger::source::host`（13 条） |
| 源 | 归一化：append 去重、snapshot 替换、`max_items`、TTL、4KB+64KB 截断 | `locked`：`trigger::source::normalize`（9 条） |
| 源 | 冻结一致性（条件读 tick 起点的世界） | `locked`：`test_frame_describes_the_tick_start_not_its_end` |
| 源 | 到期按 tick 序号取模，不读时钟 | `locked`：`test_due_follows_the_tick_counter_not_the_clock` |
| 源 | 失败局部化、3 次降频、成功恢复、挂死不拖垮 tick | `locked`：3 条 |
| 源 | 采集并发上限 `max_concurrent_collect` | **新**：`test_a_slow_collect_set_never_exceeds_the_configured_concurrency` |
| 规则 | judge 入口能力面（只有 store / log / time） | `locked`：`trigger::subscription::host`（6 条） |
| 规则 | 返回校验与预算（6→5 条、单条 200 字、4KB 证据、8KB 模板） | `locked`：`trigger::subscription::evidence`（9 条） |
| 规则 | 边沿：rise / steady / fall、冷却内推进不补发、必须落回才再升 | `locked`：5 条 + **新**端到端 `test_a_rising_edge_is_announced_once_and_rearms_after_it_falls` |
| 规则 | 调度闸门：cron / every / at、本地时间、到期后重算、重启不补、一次性自停用 | `locked`：6 条 + **新**端到端 `test_a_restart_resumes_the_state_and_never_back_fires` |
| 规则 | ready 门槛（源未采样即跳过、不动状态） | `locked`：`test_an_unsampled_source_skips_the_judgment` |
| 规则 | `state.json` 原子写 / 损坏重置 / 版本不符 / `fired_keys` / `store` 上限 | `locked`：`trigger::subscription::state`（8 条） |
| 规则 | 失败不自动停用：3 次降频、5 次告警恰一次 | `locked`：2 条 |
| 规则 | 工具语义：create 双条件、占位符校验、agent 已落盘、`test` 不写状态 | `locked`：`trigger::subscription::tool`（17 条） |
| 命中 | 有界队列丢最新并落 `ok=false` | **新**：`test_a_full_queue_drops_the_newest_fire_and_books_it_failed` |
| 命中 | 三种 action 恒等 + `deliver` 三级求值 | `locked`：`test_the_action_kinds_stay_the_three_the_engine_executes` 等 4 条 |
| 命中 | notify 同 tick 同目的地合并、超 `max_merge` 丢弃落账 | `locked`：2 条 |
| 命中 | agent fire → `SubagentRuntime(WaitForSlot)` → 结果回 origin | **新**：`test_an_agent_fire_runs_a_subagent_and_reports_back_to_the_origin` |
| 命中 | executor fire 全路径（成功 / in-band 失败 / 无 origin / evidence 不扩天花板） | `locked`：4 条 |
| 命中 | Alert 只由判定失败产生、直发渠道、无人可告静默 | `locked`：3 条 |
| 命中 | history 六字段、滚动、torn line、无 evidence 正文 | `locked`：5 条 |
| 命中 | tick 超预算：空 outcome、span Error、不入队、不记账 | **新**：`test_a_tick_that_exceeds_its_budget_books_nothing_and_says_so` |
| 命中 | 三个并列 root span 的属性面 | **新**：`test_a_rising_edge_...`（`trigger.fire`）、`test_an_agent_fire_...`（`subagent.run`）、`test_a_slow_collect_set_...`（`trigger.tick` 的 `max_concurrent`） |
| 处置 | 真实源一条链：渠道 → 归一 → 判定 → 通知落 origin | **新**：`test_a_collect_script_reading_a_real_channel_ends_in_a_notice_at_the_origin` |
| 边界 | 采集到的内容不落盘（ADR 0009） | **新**：`test_collected_content_never_lands_on_the_disk` |
| 边界 | 密钥不进投递文本与账本 | **新**：`test_a_notice_and_its_evidence_never_carry_a_secret` |
| 连接器面 | 采集入口只看到只读半、写半结构性缺失、基础原语仍在 | **新**：`test_the_collect_entry_sees_only_the_read_only_connector_half` |
| 可见性 | 主循环 schema 与默认工具集都没有 `trigger` / `trigger_source` / `executor` | **新**：`test_the_main_loop_schema_carries_none_of_the_trigger_tools` |
| 可见性 | 触发器只以 agent 存在，不存在 skill | **新**：`test_the_trigger_capability_ships_as_an_agent_and_not_a_skill` |

## 端到端用例（`tests/trigger_integration.rs`）

#1–#10 从 `trigger::assemble`（与 gateway 同一份装配）起手，断言面固定为三样：**投递到 origin 的入站文本 + `history.jsonl` 的一行 + 对应 span**；#11–#12 是注册面锁，只建 `ToolManager` 与 `run_setup`，不碰装配，也就没有账本与 span。

文件里另有第 13 个 `#[test] stdio_child_server`：它是 stdio 假 server 的子进程半边，正常 `cargo test` 跑时立即返回（与 `tests/mcp_integration.rs` 同一手法，两处各留一份约 60 行——跨测试 crate 共享支持模块是既有债，本次不动）。

| # | 用例 | 断言面 |
|---|------|--------|
| 1 | `test_a_collect_script_reading_a_real_channel_ends_in_a_notice_at_the_origin` | 入站头行 / 详情 / `- [src @ 时间]` / 「不是用户的话」尾行；账本 `ok=true`；快照内容与条目数 |
| 2 | `test_a_rising_edge_is_announced_once_and_rearms_after_it_falls` | 7 个 tick 的 fire 序列（0, 1, 0, 0, 0, 0, 1——首轮是采样轮）；`trigger.fire` 两条、属性 `trigger_id` / `action_kind` / `ok` |
| 3 | `test_an_agent_fire_runs_a_subagent_and_reports_back_to_the_origin` | `[Subagent result]` 头行带 `agent=` / `ok=` / 正文；`subagent.run` 与 `trigger.fire` 靠 `run_id` 关联 |
| 4 | `test_a_restart_resumes_the_state_and_never_back_fires` | 新装配不补触发、`prev` 与 `fire_count` 从 `state.json` 续上；过期一次性被停用而非补发 |
| 5 | `test_a_slow_collect_set_never_exceeds_the_configured_concurrency` | 4 个 120ms 采集器在 1 个许可下总耗时下界 + `trigger.tick` 的 `max_concurrent` / `sources_due` |
| 6 | `test_a_tick_that_exceeds_its_budget_books_nothing_and_says_so` | 用时被 1s 预算截断；`fired` / `queued` / `judged` 皆空；span 状态 Error；无 `history.jsonl` |
| 7 | `test_a_full_queue_drops_the_newest_fire_and_books_it_failed` | `max_queue=1` + 两个不同目的地（不让合并掩盖队列）；`dropped_count=1`、账本那行的 `trigger_id` 是后来者 `trg-2`、投递出去的是先来的那条 |
| 8 | `test_collected_content_never_lands_on_the_disk` | 全目录文本扫描：标记字符串（脚本内拼出）不在磁盘任何文件里 |
| 9 | `test_the_collect_entry_sees_only_the_read_only_connector_half` | 真 stdio fake server（一读一写）：`searchTools` 只回读半、`typeof ctx.tools.mcp__feed__post_note === "undefined"`、读调用真的走到、`fs`/`http` 仍在 |
| 10 | `test_a_notice_and_its_evidence_never_carry_a_secret` | `ctx.secret` 的值回显进证据后：投递文本与账本都不含明文，**且** `***` 确实出现在证据位（证明值真的流过去并被抹掉，而不是根本没到） |
| 11 | `test_the_main_loop_schema_carries_none_of_the_trigger_tools` | `ToolManager::init_from_config` 的 schema 与 `default_tool_set` 都不含三个工具名 |
| 12 | `test_the_trigger_capability_ships_as_an_agent_and_not_a_skill` | `setup` 铺出 `agents/trigger.md`，`skills/` 里没有触发器 |

## 测试纪律

1. **禁 sleep 等待**：tick 用 `tick()` 显式推进，消费用 `drain()`；唯一的等待助手是 `wait_until(probe)`，且必须带上界。
2. **禁真实 60s tick 与墙钟 cron**：语义级 cron 只留弱断言（「下一次落在 60s 内」），到期算法由可注入 `now` 的纯函数 `schedule_due` 覆盖。
3. **零出网**：provider 用 canned 回复，MCP 用本测试二进制再起的 stdio 子进程，keychain 用 `MapSource` / 配置直给。
4. **沙箱里的时间预算**：忙等只用 `while (ctx.now().epoch_ms - t0 < N) {}`（可复现）；并发用例只取下界（失败方向是「太快」，机器越慢越安全）。唯一的上界是 tick 预算用例的 `elapsed < 1400ms`：脚本要跑 1500ms，而预算 1s，判据是「被截断」而不是「够快」，1s 预算 + 400ms 调度余量给满了。
5. **接入方式**：`trigger::assemble` 是 gateway 与测试共用的唯一装配入口；装配若有漂移，端到端用例会先红。

## Accepted gaps

| 缺口 | 理由 | 兜底 |
|------|------|------|
| HTTP 采集链（`ctx.http.get` 打真端点） | `ctx.http` 复用 `web_fetch` 的 SSRF 守卫，**loopback 目标在连接前就被拒**，测试无法用本地桩；打真公网又违反「零出网」 | 用例 1 用 workspace 文件渠道表达同一条链（渠道 → 归一 → 判定 → 通知）；HTTP 链留在手工烟测 |
| 主循环把 `[Trigger notice]` 总结成人话 | `AgentLoop::from_config_with_tools` 按 config 自建 `OpenAIProvider`，测试注入不了 provider | 断言止于入站文本（头行 + 证据 + 「这是触发器命中」指令行）；出站总结由手工烟测覆盖 |
| 真 60s tick 节奏 | 1s 预算语义已由用例 6 覆盖，60s 等待在 CI 里必然 flaky | 手工烟测：`scripts/` 步骤 + `tmux` 起 gateway 看两轮 tick |

## 本次顺带修正

- `trigger.tick` span 增补 `max_concurrent` 属性（纯观测，配合用例 5）。
- `trigger::assemble` 从 gateway 抽出：账本路径、队列边界、子代理运行时**只拼一次**，gateway 与测试共用。
- `ctx.searchTools` 在 collect 入口同样是 **async**（返回 Promise，`await` 之后才是 JSON 字符串），原文档表格漏了 `await`，已在 `docs/codemode.md` 与 `docs/trigger.md` 修正。

## 验收

- 矩阵每行有测试名或带理由的 `accepted gap`；
- `cargo fmt -- --check`、`cargo clippy`、`cargo test` 三绿；
- `cargo test --test trigger_integration` 连跑 5 轮全绿；
- 文档同步：本文件 + `docs/index.md` + `docs/trigger.md` + `docs/TODO.md` + spec 的 `## Testing Decisions`。
