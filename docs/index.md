# SlimBot 文档索引

## 入门

| 文档 | 说明 |
|------|------|
| [配置指南](config.md) | `config.json` 结构、验证规则、多 Provider 配置 |
| [内置工具](tools.md) | 10 个内置工具（shell、文件读写编辑、目录、grep/find_files、web_fetch、codemode），加 `memory_search`、`subagent` 与 Dream 专用记忆工具；触发器工具（`trigger` / `trigger_source`）与 `executor` 只对声明它们的 agent 可见 |
| [日志系统](logging.md) | 日志级别、CLI 参数、输出格式 |

## 核心模块

| 文档 | 对应模块 | 说明 |
|------|----------|------|
| [架构概览](architecture.md) | 整体 | 系统架构、数据流、组件交互 |
| [Agent 循环](agent_loop.md) | `agent_loop.rs` | 顶层编排、组件初始化 |
| [ReAct 执行器](runner.md) | `runner.rs` | ReAct 循环核心：迭代控制、工具调用流程 |
| [审批生命周期](runner.md#审批生命周期approval) | `approval/`, `runner/hooks/approval.rs` | 风险级 → 管控档 → `ApprovalHook`：Deny 续跑、Ask 结束回合、下一轮按 args 哈希兑现、裁剪保护、审计入 trace（ADR 0011） |
| [上下文构建器](context.md) | `context.rs` | System Prompt 构建流程 |
| [会话管理](session.md) | `session.rs` | 会话生命周期、任务队列、JSONL 持久化 |
| [定时任务](cron.md) | `cron/` | 本地时间调度的 jobs.json、tick 语义、错过的一次性窗口只记 `skipped` |
| [会话压缩](consolidate.md) | `consolidate.rs` | Token 预算触发摘要、消息驱逐、上下文注入 |
| [上下文压力](snip.md) | `pressure.rs`, `snip.rs` | 单一窗口锚派生全部长度限制、两阶段截断、引用带落盘路径（ADR 0013） |
| [消息总线](message_bus.md) | `message_bus.rs` | 请求分发、任务封装、结果路由 |
| [Provider](provider.md) | `provider/` | Provider 接口、OpenAI 兼容 API 集成 |
| [通道](channel.md) | `channel/` | 通道抽象、工厂模式、并发 I/O 循环 |
| [飞书通道](feishu.md) | `channel/feishu.rs` | 首个 IM 通道：单条 WS 长连（gateway 独占）、`owner_open_id` 绑 owner、p2p 文本往返 + 流式卡片原位更新、图片/文件收发（入站只按 file-key 拉、防 SSRF）、隐私护栏（ADR 0017） |
| [触发器层](trigger.md) | `trigger/` | 固定 tick、数据源采集与快照、边沿判定、执行投递与 fire 账本 |
| [触发器测试方案](trigger-test-plan.md) | `trigger/`, `tests/trigger_integration.rs` | 覆盖矩阵、12 条端到端用例、确定性纪律与 accepted gaps |
| [执行器](executor.md) | `executor/` | 存放的 `act.js` 动作脚本：目录形状与 meta、入参校验、`scan-on-call` 读盘（无常驻层）、`executor` 工具的增删改查与 `run`（数值闸 + 结果契约 + 失败分类）；调用面是寄生的 `executor` 子代理（`agents/executor.md`），主循环零污染，act 侧 v1 Allow-only（ADR 0021） |
| [子代理运行时](agent.md) | `agent/` | agent 定义与工具增量、一次性 run、并发闸、结果投递 |
| [脚本沙箱](codemode.md) | `codemode/` | rquickjs 专职线程、双层超时、宿主函数白名单、凭证抹除 |
| [MCP 连接器](mcp.md) | `mcp/` | 自研最小客户端（stdio + streamable-HTTP）、gateway 级连接池与退避重连；工具只进 codemode 的 `searchTools()` / `tools.mcp__*`，**不进模型 schema**（ADR 0016、0020） |

## 基础设施模块

| 文档 | 对应模块 | 说明 |
|------|----------|------|
| [日志系统](logging.md) | `log.rs`, `macros.rs` | 全局单例日志、级别过滤、彩色终端输出 |
| [路径管理](path.md) | `path.rs` | 路径解析、默认值、沙箱验证、波浪号展开 |
| [工具系统](tool.md) | `tool.rs`, `tools/` | Tool trait、ToolManager、工具结果处理 |
| [记忆模块](memory.md) | `memory/` | category 化长期记忆、可插拔 backend、历史与游标 |
| [卡片生成](channels.md#卡片下发card) | `card/` | Run 后独立流式调用、Catalog 注册、spec 编译、侧车回放、护栏（机制文档在通道文档「卡片下发」节） |
| [工作池](worker.md) | `worker.rs` | 异步 Worker 与动态 WorkerPool |
| [I/O 调度器](io_scheduler.md) | `io_scheduler.rs` | 阻塞读取循环、stdin 处理 |
| [配置编辑 schema](config.md#webui-配置编辑器) | `config_schema.rs` | schema 序列化 + path-patch 引擎（WebUI 配置编辑器后端） |
| [Bootstrap 模板](bootstrap.md) | `bootstrap.rs`, `embed.rs` | 嵌入文件、模板加载、技能文件 |
| [工具函数](utils.md) | `utils/mod.rs` | 文本截断、原子写入、持久化引用 |
| [Trace](trace.md) | `trace/` | span 埋点、多 backend 导出（file / OpenTelemetry）；新建配置默认开启，覆盖所有模型调用（含 consolidation 与卡片回复），本地用 `/traces` 看（ADR 0022） |

## 快速链接

- [CLAUDE.md](../CLAUDE.md) — 开发规范与检索入口（构建/门禁、代码地图、硬约束）
- [README](../README.md) — 项目概览、构建与运行说明
