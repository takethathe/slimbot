# Subagent 子代理运行时

对应模块：`src/agent/`（`definition.rs`、`catalog.rs`、`runtime.rs`、`tool.rs`）

## 概述

子代理是「一份定义 + 一次一发的执行」：定义写在 `agents/<name>.md`，运行由主 loop 的 `subagent` 工具或触发器的 `agent` action 发起。它跑在**自己的会话**里，工具集按定义组装，跑完即销毁——与发起方只交换一段结果文本。设计取舍见 [ADR 0006](adr/0006-subagent-runtime-shape.md)：不给主 Runner 动态加工具，而是每次都造一个独立 `AgentRunner`，因此工具集在构造期就绑定死了。

## 定义：`agents/<name>.md`

frontmatter 只声明**增量**，正文原样作为 system prompt：

| 字段 | 默认 | 说明 |
|------|------|------|
| `name` | 文件名 | 必须与文件 stem 一致，也是 `subagent` 工具的 `agent` 参数 |
| `description` | — | 一行，渲染进 system prompt 的 `# Available Agents` |
| `tools` | 空 | 追加到默认集的工具名 |
| `deliver` | `true` | 结果是否投回来源会话 |
| `timeout_secs` | `600` | 单次运行硬超时 |

**默认工具集** = `config.tools`（未配置则十个内置工具）+ `message` + `memory_search`，再减去保留工具。`cron` 与 `subagent` 是保留的（`FORBIDDEN_AGENT_TOOLS`）：前者会让任务绕过触发器层偷偷建定时器，后者会让并发上限自己乘自己。声明不存在的工具或保留工具 → **整个定义被拒绝**，而不是降级成半残的 agent：一个中途缺工具的 agent 看起来像模型不会用工具。

## ToolCatalog

进程级的 `name → 工厂` 注册表，把「有哪些工具」与「某次 run 用哪些工具」分开：主 loop 装配一次，子代理按定义每次装配自己的 `ToolManager`。`known_tool_names()` 供定义校验。`trigger_source` / `trigger` **只注册在 catalog 里**，不进主 loop 的默认集——只有声明它们的 agent 看得见，这就是「触发器机制不在主 loop 出现」这条需求的实现方式；`subagent` 反过来，由主 loop 的 `ToolManager` 直接注册，agent 定义永远拿不到它。

## 运行：SubagentRuntime

`start(StartRequest, AcquireMode)` 立即返回 `StartedRun{run_id, agent, delivers}`，运行在后台任务里继续：

1. 取一个并发许可（`subagent.max_concurrent`，默认 3）；
2. 建会话 `subagent:<agent>:<run_id>`，标记 `mark_ephemeral`、写入 system override；
3. 用 `AgentRunner` 跑完整个 ReAct 循环；
4. **任何退出路径都 `drop_session()`**——没有 JSONL，没有 consolidator，不留痕；
5. 结果按 `deliver` 投回来源会话。

`AcquireMode` 区分两种调用方：

- `RejectWhenFull`：`subagent` 工具用，闸满直接回 `busy`，因为模型可以自己稍后重试；
- `WaitForSlot`：触发器 fire 用，没有调用方能够重试，所以排队等空位。

启动前的拒绝还有 `UnknownAgent`（附当前已知名字）与 `MissingTools`。

## 投递

结果走**入站路径**（`MessageBus::publish_inbound` + `TaskHook::new(origin)`），因此主 loop 的完整工具集、per-session 串行队列、出站路由与 `/stop` 语义照常生效——触发器与子代理都不必重新实现这些。文案由 `compose_result()` 固定：

```
[Subagent result] agent=<name> run_id=<id> ok=<bool> elapsed=<n>s

<正文或错误摘要>

<总结指令：成功=「向用户总结做了什么、发现了什么、需要什么决定」；失败=「说明哪里失败并给出下一步」>
```

`deliver` 三级求值：调用方显式声明（触发 action）→ 定义的 `deliver` → 有没有可路由的 origin。三级都拿不到「要发给谁」时，不投递、只记日志——宁可静默也不发到某个错误的会话。

## Trace

每次运行一个独立 root span `subagent.run`（`agent` / `run_id` / `ok` / `elapsed_ms`），**不挂在发起方的 span 下**：run 是异步的，会活过发起它的那个 tick 或那次工具调用。

## 提示词注入

`ContextBuilder` 在 system prompt 里渲染 `# Available Agents`（一行一个名字与描述）。与 `# Available Skills` 的分工是一条硬约定：**一个能力一个入口**——agent 是「可被启动的执行体」，skill 是「可被加载的知识」，同一能力同时占两个位置时模型会走不用读文档的那条路（直接调 agent），文档就形同虚设。
