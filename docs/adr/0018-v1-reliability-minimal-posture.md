# v1 可靠性走最小姿态：复用既有机件、时间基准统一到本地、不建预算硬上限

slimbot 是 7×24 单用户本机服务，失败模式必须可预期。事实核查发现多个「缺口」其实机制已在：message_bus 已是 bounded mpsc（背压）、每 run 的 token 已进 trace span、OTel trace backend 已可经 config 接 collector、CancellationToken 取消路径已现成。真正的空白只有：超时是死配置（`agent.timeout_seconds` 从不执行）、cron 主服务按 UTC 而触发器层 / `at` 解析按本地（同一表达式两种时刻）、无任何补跑与成本上限、provider 所有失败塌成一行通用错误。决策是 **v1 只做最小可靠闭环、其余推迟**：超时给整个 `run()` 套一条挂钟 deadline 且复用取消路径（`timeout_seconds` 就此有了真身）；调度**全库统一 `chrono::Local`、不引 `chrono-tz`**（`tz` 字段收下但不生效）；停机**只补记 `skipped`、绝不补跑**；瞬时错有限退避重试、**不做 fallback provider**；**不设成本硬上限**（额度交给 provider 自身提示）；把 provider 错误**按 `code` 分类、把配额/鉴权类提示原样投递给 owner，但绝不落完整响应体**；**不建 `/health` / OTel metrics**。取舍是：用现成件换最快到「可预期」，放弃预算门控、错过补跑与主动健康探测这些需要外部消费者才有价值的重机件。

**Considered Options**

- **完整可靠性套件**：run + per-step 双层超时、`chrono-tz` 命名字时区、错过合并补跑、fallback provider 降级、每日 token 账本 + 硬上限门控、`/health` 探活 + OTel metrics。优点是对「服务化托管」准备充分；缺点是 v1 无外部编排器 / 多时区 / 计费消费者，全属投机建设，且给单二进制加一堆无人调用的表面。
- **纯现状最小（连分类都不做）**：只把死掉的 `timeout_seconds` 接上，其余全推。优点改动最小；缺点是 provider「你额度用完了 / 密钥失效」被吞成 `API request failed`，owner 无从下手——违背「失败可预期」。
- **选定的最小姿态**：复用取消路径 / bounded 队列 / trace / 触发器告警合并这些既有机件补齐可预期性，统一本地时间这一烙进持久化数据的基础决策一次做对，其余（预算上限、补跑、健康端点、metrics、fallback）明确推迟。

**Consequences**

- 时间基准是不可逆约束：cron 表达式解析结果写进 `jobs.json` 的 `next_run_at_ms`，改 UTC↔Local 会即时改变存量任务触发时刻；故本票一次性统一到 Local 并接受 `tz` 字段从此不生效（保留不删以免反序列化失败）。
- 「不补跑」意味着长时间停机后一次性任务（`At`）视为作废、只留 `skipped` 痕迹；周期任务相位对齐到重启后的下一次。这是刻意的产品选择，不是遗漏。
- 「不设成本硬上限」把额度可见性完全押在 provider 的错误提示质量上，因此 provider 错误分类与投递是本票可靠性承诺的关键路径：瞬时错走重试、鉴权/配额类**绝不重试**且把原话递到 owner，重复硬错复用触发器冷却合并（五次失败提醒一次）。
- 隐私护栏与票 09/10 同源：provider 响应体可能回显请求正文，故只提取结构化 `code`/`message`，响应体永不落 session / 日志。
- 观测出口 = trace（含已存在的 OTel backend，config 指向 collector 即可）；`/health`、OTel metrics、队列/outbox 持久化均出 v1，服务化安装 / 打包（另票）若引入外部编排器再重开。
- `context_length_exceeded` 归 Context pressure（票 15）的降级路径，不作硬失败处理。
