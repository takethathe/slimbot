# SlimBot

基于 ReAct 循环、多通道、支持工具调用的 AI 机器人助手。本文档是项目术语表，不含实现细节。

## Language

### Core loop

**Run**:
一次 `AgentRunner::run()` 的执行 —— 一条用户消息到最终回复（含多轮 ReAct iteration）。对应一条 trace。
_Avoid_: turn, task, request

**Run id**:
一次 Run 的唯一标识，在请求进入消息总线时分配，贯穿该 Run 的生命周期事件、结果与卡片事件。
_Avoid_: task id, request id, turn id（代码历史字段名为 `task_id`，已统一为 `run_id`）

### Card (Generative UI)

**Card**:
WebUI 会话气泡下方渲染出的一块生成式 UI，是一次模型调用的产物，不是会话消息的一部分。
_Avoid_: widget, panel, block

**Card spec**:
卡片的扁平描述（`root` + `elements` + `state`），由模型输出的 JSONL patch 编译而成；它只含词表内的组件名，不含任何组件实现。
_Avoid_: UI JSON, layout tree

**Catalog**:
前端提供的卡片组件与动作词表（名称、props schema、描述），是 Card spec 的合法词汇边界。它归前端所有，服务端只消费其派生物。
_Avoid_: component list, schema（`schema` 一词在 json-render 中另指 spec 的结构定义）

**Catalog prompt**:
由 Catalog 生成的系统提示词，唯一用途是让模型产出合法的 Card spec。
_Avoid_: card prompt, system prompt（后者已指 agent 的上下文构建产物）

**Catalog registration**:
WebUI 客户端在连接后把 Catalog prompt 上报给服务端的行为，同时声明「本客户端能渲染卡片」。未注册的会话不产生卡片。
_Avoid_: catalog upload, schema sync

**Card generation**:
一次 Run 结束后发起的独立模型调用：以该 Run 的 Run transcript 为输入、Catalog prompt 为系统提示词，产出 Card spec。
_Avoid_: card render（渲染只发生在客户端）, genui call

**Run transcript**:
一次 Run 期间新增的会话消息（user / assistant / tool），经裁剪后作为 Card generation 的输入。
_Avoid_: history, context, messages

**Card sidecar**:
按 chat 存放 Card spec 的追加文件（`sessions/{chat_id}.cards.jsonl`），以该 Run 末条 assistant 消息的 timestamp 为锚。它刻意不进入会话 JSONL，避免 patch 文本污染 agent 上下文。
_Avoid_: card cache, card db

### Memory

**Memory category**:
记忆文档的语义种类（`soul` / `user` / `agent` / `tools` / `core` / `daily` / `skill` / `custom`），每个 category 对应一个文档。
_Avoid_: memory type, memory kind

**Memory backend**:
记忆文档的存储实现（当前 markdown，未来 sqlite 等），由 `MemoryBackend` trait 以「文档级」读写描述。
_Avoid_: memory store, memory driver

**MemoryService**:
统一的记忆 facade，组合可插拔 backend（category 文档）与 HistoryStore（交互历史），是记忆的对外入口。
_Avoid_: MemoryStore, MemoryManager

**History store**:
`history.jsonl` 追加日志 + Dream 游标的独立子系统，刻意不属于 backend trait。
_Avoid_: history backend, history log

**Memory entry**:
记忆文档中可独立检索的最小单元（一条事实、一条行为规则或一个工作流步骤）。它是文档的**派生视图**，不是独立存储的权威数据；权威始终是记忆文档本身。
_Avoid_: memory record, memory fact, memory item；与 History store 的 `HistoryEntry`（`history.jsonl` 的一行）混称

**Memory index**:
从记忆文档派生出的可检索视图，用于按查询取回相关的 Memory entry。它不持有权威数据，可从文档重建。
_Avoid_: vector store, memory db, cache

**Embedded resource**:
构建期经 `include!` 编进二进制的模板与静态文件（`src/embed.rs`、`resources/`）。
_Avoid_: embed（当意指向量 embedding 时，须写全 `embedding`）

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

**Trace file**:
span 记录被 append 到的 JSONL 文件（默认 `{workspace_dir}/traces.jsonl`）。默认在每次 Gateway 启动时截断（`tracing.reset_on_start`，默认 true），因此只保留当前进程运行期间的 trace；CLI 模式不截断。
_Avoid_: log file（与 `log.rs` 的日志文件无关）

**Trace viewer**:
WebUI 上专门查看 trace 的页面（`/traces`），按 trace 开始时间降序列出 trace，并可展开查看每个 span 的详情。
_Avoid_: trace UI（术语不用首字母缩写与界面层级概念混写）

### Configuration

**Config editor**:
WebUI 上可视化编辑 slimbot 配置的页面（`/config`），表单由 config schema 自动生成。
_Avoid_: settings page, 设置页

**Config schema**:
由 `define_config!` 宏生成的字段元数据集合（名称、类型、描述、约束、默认值、是否密钥），是 Config editor 表单的单一数据源。
_Avoid_: form spec, 表单定义

**Config field**:
config schema 中一个可编辑项，对应一份字段元数据。
_Avoid_: setting, option
