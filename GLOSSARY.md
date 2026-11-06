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

### Identity & trust

**Owner**:
slimbot 的唯一人类主体（v1 永久单用户）。一个 gateway 只有一个 owner；会话、记忆、审批与投递最终都归属于 owner。
_Avoid_: user（多用户语境下才用）, account

**Owner id**:
owner 的稳定标识（v1 固定为单例值），把会话、任务、审计与投递记录关联到同一个主体。
_Avoid_: user id, account id

**TurnKind**:
一次 Run 的发起来源（User / Cron / Heartbeat / Trigger / Subagent），用于区分「谁在驱动这次运行」，不改变 Run 本身的定义。
_Avoid_: task type, run type

**Session**:
一个通道内的对话上下文（如 WebUI 的一个 chat、Telegram 的一个私聊或群组），归 owner 所有；同一 owner 可拥有多个通道会话，记忆跨会话共享。
_Avoid_: chat, conversation（`chat_id` 仍指通道内标识）

**Risk level**:
工具自带的能力分类（只读 / 工作区写入 / 命令执行 / 网络外出 / 对外发送），是工具的事实声明，不随配置改变。
_Avoid_: permission, capability tier

**Control level**:
一个风险级在运行时受到的管控强度——普通（直接执行）/ 中级（需 owner 确认）/ 高级（直接拒绝），由配置决定。
_Avoid_: risk level（那是工具的事实，不可配）

**Pending approval**:
一次 Run 中因中级管控而挂起、等待 owner 在**下一轮**确认的一个具体动作（工具 + 参数），仅在紧邻的下一轮可兑现，过期、重启或无人值守时作废。
_Avoid_: ask user, confirm tool, confirmation dialog（v1 明确不存在阻塞式追问/确认工具，也不存在等待输入的挂起 Run）

**Approval redemption**:
在确认轮里重新发起已记录的同一个动作、并通过参数一致性校验后真正执行的过程。
_Avoid_: auto approve, retry

### Trigger

**Data source**:
被定期观察的一个外部或本地信息渠道。
_Avoid_: feed, connector, watcher

**Agent**:
一份可被委托执行任务的独立定义：角色、可用能力、交付偏好。
_Avoid_: agent profile, bot, persona

**Subagent**:
由 agent 发起的一次独立执行，有自己的会话，与发起方隔离。
_Avoid_: worker, child agent

**Trigger**:
一条持久规则：「当某条件成立时，做某事并告知某人」。
_Avoid_: subscription, rule, watcher

**Condition**:
trigger 的判定逻辑：读当下的世界，回答「命中了吗」。
_Avoid_: filter, predicate

**Snapshot**:
某一时刻所有 data source 的只读视图。
_Avoid_: cache, state

**Fire**:
一次命中：trigger 从规则变成动作的那一刻。
_Avoid_: hit, match, emit

**Evidence**:
命中时被挑选出来、允许进入模型上下文的少量事实片段。
_Avoid_: sample, payload, excerpt

### Delivery

**Delivery**:
把一条消息送回某个 channel 的行为。主动消息（cron / heartbeat / trigger / agent 主动 `message`）都走同一投递模型，不因来源不同而分叉。
_Avoid_: notify, push, send（泛化动词；`notify` 在代码里特指 trigger 的入站动作）

**Delivery target**:
一次投递的落点。解析优先级：**显式 deliver-to（创建任务时指定）> 创建该任务的 Origin session > Owner primary channel（最后兜底）**。显式目标一旦给出即胜过默认 origin。
_Avoid_: recipient, destination（代码层用 `DeliveryTarget{channel, chat_id}`）

**Origin session**:
创建/设定某任务时所处的那个会话——任务的隐式投递目标默认就是它。heartbeat 由配置启动、不立于任何会话，没有 origin。
_Avoid_: source session, origin channel（严格说 origin 是会话，不只 channel）

**Owner primary channel**:
owner 首次交互所建立的那个会话通道，作为任务无显式目标且无 origin 时的最后兜底落点。未建立时不投递（降级 log-only）。
_Avoid_: default channel（v1 不存在运行期另设的全局默认目标）, home channel

**Silent delivery**:
任务显式声明 `deliver:false`，故意不把结果投给任何通道。它是唯一的策略性静默条件，与“目标尚不存在”的兜底降级区分。
_Avoid_: quiet hours（v1 不做）, mute

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

**Promotion**:
把一条 Memory entry 从「落地区文档」升到其规范归属文档（如从 core 升到 skill / user）的整理动作，由 Dream 在记忆整理时执行，不是写入时发生的判断。
_Avoid_: memory routing（那是 Dream 提示词里的分类规则，不是晋升本身）, migrate

**Protected document**:
主模型不得改写、只能由 Dream 改的记忆文档（人格与身份描述、技能文档等）。
_Avoid_: read-only doc（它不是只读，主模型仍可读）, locked file

**Memory snapshot**:
Dream 实际改写某篇记忆文档前，把旧内容按运行时间戳归档的一份副本。它既是回滚的载体（粒度 = 这篇文档回到这次 Dream 之前），也是条目年龄的推导源。快照目录不进入检索语料。
_Avoid_: backup（泛指易与整体数据备份混称）, git commit（回滚刻意不依赖 git）

**Entry age**:
一条 Memory entry 的正文首次出现在 Memory snapshot 序列中的那一次，到当前之间的跨度。它是系统侧的推导值，只作为 Dream 整理时的侧信道提示，不进文档、不进检索结果。标题行的变动不改变它。
_Avoid_: observed_date（v1 不写日期）, 时间戳（那是 history 行的系统时间，不是条目年龄）

### Context pressure

**Context pressure**:
可用上下文容量接近上限时，「该扔掉什么」的判定状态及其动作集合。
_Avoid_: token budget（budget 一词归花费配额独占）, overflow, context full

**Derivation anchor**:
唯一按绝对值配置的容量参数（模型窗口的 token 数）；其余长度类限制均由它按百分比派生。
_Avoid_: max tokens（那指单次响应输出上限）, 全局预算

**Tool-result exemption**:
工具自述的属性：其结果既不被长度截断也不被残余清理。不随配置改变，与 Risk level 同类。
_Avoid_: whitelist, compactable list（那是旧的按工具名硬编码的中心名单）

**Residual cleanup**:
把已过期的旧工具结果在上下文中压成一句引用的动作，引用里带已保存全文的位置，因而「被扔掉」不等于「不可找回」。
_Avoid_: microcompact（代码符号名）, drop（不带可找回线索的整条丢弃只属于上下文裁剪，不是本动作）

### MCP

**MCP server**:
一份配置声明的外部工具提供者（stdio 进程或 HTTP 端点），提供一组可被 codemode 脚本调用的工具。它不是模型 `tools` schema 中的一项。一个 MCP server 就是一个连接器（见 `### Connector`）：“MCP server”指该连接器的具体交付形式、“Connector”指 codemode 可直接调用的外部能力封装这一角色。
_Avoid_: plugin, extension

**McpService**:
Gateway 级服务，管理所有已配置 MCP server 的连接池、工具列表缓存、健康检查与退避重连。codemode 的 tool bridge 通过它路由调用。它由 `AgentLoop` 从 `config.mcp` 建池并启动（CLI 与 gateway 同源），每一台 server 一个 supervisor。状态视图（名字 / 状态 / 缓存工具数）是对外可观测的全部事实，恒不含凭证。
_Avoid_: MCP client（泛指协议实现）, MCP manager

**Connector tool (callable)**:
一个连接器工具在脚本里的地址：`mcp__<server>__<tool>`（下发时非 `[A-Za-z0-9_]` 字符被替换为 `_`，同前缀撞名时后缀 `_2`、`_3`）。`mcp__` 前缀同时是路由标记：以它开头的名字永不去 `ToolManager` 里找。它是脚本面的名字，不是模型面的名字。

**Codemode (agent entry)**:
模型在 ReAct 循环中可调用的一个工具：模型写 JS 源码，宿主在 QuickJS VM 里执行，脚本通过 `tools.*()` / `searchTools()` 调用工具，结果返回模型。区别于 trigger 路径的 `collect` / `judge` 入口。
_Avoid_: code interpreter, script runner, sandbox（后者指执行环境而非模型可调用的工具本身）

**searchTools**:
codemode 脚本内的全局函数，用 BM25 排序检索本次运行的**可调用目录**（本次运行装配的内置工具 + 在线连接器的工具，返回 `[{name, description}]`）。内置工具在模型 schema 里已可见，因此它存在的理由是连接器工具：那一批只经脚本发现、永不进入模型 `tools` 的东西。离线 server 的工具不在可调用目录里。
_Avoid_: tool_search（那是 pi 的模型级工具，本决策不用它）, memory_search（那是记忆检索）

### Connector

**Connector**:
一段 **codemode 可直接调用的、非基础原语、非内置工具**的外部能力封装；同一个连接器既能“获取信息”也能“执行动作”。定义特征是从 codemode 内可被调用，不是“凡与外部相连”——MCP server 是连接器的一个特例（v1 的实例）。
_Avoid_: Channel/通道（与连接器正交，是会话/传输面）, 基础原语（时间/网络这类 codemode 内置原语，不算连接器）, 内置工具（`file_editor`/`shell`/`message`/`web_fetch` 等，不算连接器）, 执行器（那是消费连接器写侧的动作脚本）, json-render 卡片（属 Channel 侧展示，不是连接器）

**Executor (执行器)**:
与采集器（触发器源 `collect.js`）**操作镜像**的 codemode 动作脚本：消费连接器的写侧，对外部资源执行一次真实动作。由 subagent 创建与调用、**按需执行**（无常驻 tick；周期性的“动”由触发器 fire 驱动）。其动作带副作用，纳入审批（见 Risk level / Pending approval）；无人值守一律按 Deny。
_Avoid_: Connector（连接器是它调用的能力后端，不是脚本本身）, 采集器（那是只读侧）, tool

### IM Channel

**Feishu Channel**:
slimbot 的首个 IM 通道（v1）。以企业自建应用 + WebSocket 长连接接入飞书；一个 `feishu` Channel 实例内部按会话管理多个 chat。
_Avoid_: Telegram, 邮件通道（均为后续通道）

**Owner open_id**:
飞书通道中 owner 的通道内身份（`ou_…`），纯配置声明，把入站消息映射到 owner；非该身份的私聊静默忽略。
_Avoid_: Owner id（那是全局主体标识）, chat_id（那是会话标识）

**Interactive card (Feishu)**:
飞书消息体系里的富交互卡片，承载 agent 逐步刷新直至定稿的流式输出。它与本项目的 Card（WebUI 生成式 UI）是两回事，不可简称 Card 混称。
_Avoid_: Card（在本项目已专指 WebUI 生成式 UI）, typing indicator

### Reliability

**Run deadline**:
一次 Run 的挂钟上限；到点按「取消」语义收尾（与主动停止同一条路径），区别于崩溃或半截会话。
_Avoid_: provider timeout（那是单次调用的上限）, hang

**Transient failure**:
上游（模型服务 / 网络）可预期会自行恢复的失败（限流、临时过载、连接超时），允许有界重试。
_Avoid_: hard failure, error（裸用不分可重试性）

**Hard failure**:
不会因重试而改变的失败（密钥失效、额度耗尽、计费问题）：绝不重试，把上游原话作为可操作提示送达 owner。
_Avoid_: transient failure, fatal（不指进程崩溃）

**Skipped run**:
任务因停机错过触发窗口而留下的显式「未做」痕迹；刻意不补跑，也不静默丢弃。
_Avoid_: missed run（口语，但不强调留痕）, failed run（没执行不等于执行失败）

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
