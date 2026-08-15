# SlimBot

基于 ReAct 循环、多通道、支持工具调用的 AI 机器人助手。本文档是项目术语表，不含实现细节。

## Language

### Core loop

**Run**:
一次 `AgentRunner::run()` 的执行 —— 一条用户消息到最终回复（含多轮 ReAct iteration）。对应一条 trace。
_Avoid_: turn, task, request

### Observability

**Trace**:
对一次「用户消息 → 最终回复」全过程的观测记录，含一个根 span 及其全部子 span，共享同一 trace_id。一次 session 可含多次 trace。
_Avoid_: session, request

**Span**:
一次带起止时间戳、可嵌套的具名操作记录（如一次 LLM 调用、一次工具执行），携带 trace_id / span_id / parent_span_id 与 attributes。
_Avoid_: event, log

**Trace context**:
一个 span 在 trace 树中的定位标识（trace_id + span_id + parent_span_id），仅进程内维护。
_Avoid_: context（代码中 `context` 已指 LLM 的 system-prompt / history 构建，二者无关）

**Backend**:
trace 数据的导出目标（file、OpenTelemetry 等）。多个 backend 可同时接收所有 span，每个 backend 独立、不可失败。
_Avoid_: exporter, sink

**AgentEvent**:
面向通道（WebUI SSE）实时展示的细粒度生命周期通知，与 span 独立共存、互不派生。
_Avoid_: event（裸用易与 span event 混淆）
